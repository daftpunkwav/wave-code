/*!
 * @file AcpServer
 * @description ACP (Agent Client Protocol) server over stdio.
 *
 * Responsibilities:
 * - Serve JSON-RPC 2.0 initialize/session-new/prompt/cancel over stdin/stdout.
 * - Assemble one headless session per session/new and drive single turns.
 * - Map wire events onto session/update notifications and stop reasons.
 *
 * This module must not depend on: frontends, TUI, or interactive prompts.
 */

//! ACP server: scripted controllers drive headless sessions over stdio.
//!
//! Transport framing matches `mcp_serve` (plain NDJSON lines, one
//! JSON-RPC message each). The subset served here is `initialize`
//! (replying [`ACP_PROTOCOL_VERSION`] plus agent capabilities),
//! `session/new` (assembling one headless session per id),
//! `session/prompt` (one turn, streaming `session/update`
//! notifications), and `session/cancel` (interrupting the in-flight
//! turn; the spec delivers it as an id-less notification, and an
//! id-bearing request form is answered for scripted controllers).
//! Unknown methods fail with `-32601`; other notifications (no id)
//! are ignored; stdin EOF ends the server cleanly.
//!
//! Like `exec`, every session assembles from config, so this surface
//! needs provider credentials; assembly failures fail `session/new`
//! instead of the process. `mcpServers` in `session/new` is accepted
//! and ignored: MCP servers come from the config file. Sessions are
//! consumed through the [`SessionSurface`] contract, so the caller
//! supplies the assembly seam (the composition root's
//! `assemble_session` in production, a stub-model assembly in tests).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use operations_actor::{AssembleOptions, DEFAULT_IDENTITY, SessionError, SessionSurface};
use tokio::io::{AsyncBufRead, AsyncWrite, AsyncWriteExt};
use tokio::sync::{Mutex, mpsc};
use wavecode_wire::{EventMsg, Op, Submission};

use crate::jsonrpc::{
    INVALID_REQUEST, IncomingLine, error_response, id_or_null, read_line_capped, success_response,
};

/// Protocol version advertised at `initialize` (subset pin, not negotiated).
const ACP_PROTOCOL_VERSION: &str = "0.1.0";

/// JSON-RPC error codes served by this loop (`INVALID_REQUEST` is the
/// shared one from [`crate::jsonrpc`]).
const PARSE_ERROR: i32 = -32700;
const METHOD_NOT_FOUND: i32 = -32601;
const INVALID_PARAMS: i32 = -32602;
const INTERNAL_ERROR: i32 = -32603;

/// Server inputs; sessions inherit these unless `session/new` overrides them.
#[derive(Debug, Clone)]
pub struct AcpServerOptions {
    /// Config file path; `None` loads the user-level config.
    pub config_path: Option<PathBuf>,
    /// `--model` override winning over the configured model.
    pub model_override: Option<String>,
    /// `--permission-mode` override winning over the configured mode.
    pub permission_override: Option<String>,
    /// Working directory for tools when `session/new` omits `cwd`.
    pub cwd: PathBuf,
    /// Home directory; `None` degrades memory without failing.
    pub home: Option<PathBuf>,
}

/// Serve ACP over the process stdio streams until EOF.
///
/// Assembles one headless session per `session/new` from `options`
/// through the caller's `assemble` seam; returns when stdin closes.
pub async fn run_stdio_server<F, S>(options: AcpServerOptions, assemble: F) -> std::io::Result<()>
where
    F: Fn(AssembleOptions) -> Result<S, SessionError>,
    S: SessionSurface + 'static,
{
    let reader = tokio::io::BufReader::new(tokio::io::stdin());
    let writer = tokio::io::stdout();
    serve_loop(reader, writer, options, assemble).await
}

/// One live session behind the serve loop.
struct SessionEntry {
    /// Jobs for the session task owning the actor client.
    jobs: mpsc::UnboundedSender<SessionJob>,
    /// Request id of the in-flight prompt, if any.
    in_flight: Option<serde_json::Value>,
}

/// Work for a session task.
enum SessionJob {
    /// Run one turn; the reply is deferred until the turn ends.
    Prompt {
        /// JSON-RPC id to answer when the turn ends.
        req_id: serde_json::Value,
        /// Joined prompt text.
        text: String,
    },
    /// Interrupt the in-flight turn, if any.
    Cancel,
    /// Switch the session permission mode (pre-validated wire name).
    SetMode {
        /// Mode wire name (`plan` / `auto` / `wave`).
        mode: String,
    },
}

/// The mode table every ACP client sees: the three wire names with
/// controller-facing descriptions, plus the session's current pick.
fn acp_modes(current: &str) -> serde_json::Value {
    serde_json::json!({
        "currentModeId": current,
        "availableModes": [
            {
                "id": "plan",
                "name": "Plan",
                "description": "Read-only exploration; the agent proposes instead of acting",
            },
            {
                "id": "auto",
                "name": "Auto",
                "description": "Asks only for command execution and destructive tools",
            },
            {
                "id": "wave",
                "name": "Wave",
                "description": "Fully automatic; deny rules still apply",
            },
        ],
    })
}

/// Turn outcome reported back to the serve loop.
struct TaskMsg {
    /// Session the turn ran in.
    session_id: String,
    /// JSON-RPC id to answer.
    req_id: serde_json::Value,
    /// How the turn ended.
    outcome: PromptOutcome,
}

/// How a prompted turn ended.
enum PromptOutcome {
    /// Turn reached `TurnCompleted`; carries the ACP stop reason.
    Stop(String),
    /// Turn failed at the harness level; carries the error text.
    Failed(String),
}

/// Serve loop over explicit streams (the duplex-testable core behind
/// [`run_stdio_server`]).
///
/// `assemble` builds one session per `session/new`; production passes
/// the composition root's `assemble_session`, tests a stub-model seam.
async fn serve_loop<R, W, F, S>(
    reader: R,
    writer: W,
    base: AcpServerOptions,
    assemble: F,
) -> std::io::Result<()>
where
    R: AsyncBufRead + Unpin,
    W: AsyncWrite + Unpin + Send + 'static,
    F: Fn(AssembleOptions) -> Result<S, SessionError>,
    S: SessionSurface + 'static,
{
    let writer = Arc::new(Mutex::new(writer));
    // Unbounded on purpose: producers emit one `TaskMsg` per finished
    // prompt, and this loop drains the receiver on every iteration, so
    // the queue holds at most a handful of completions. A bounded channel
    // would let one wedged session task stall the whole reader loop.
    let (done_tx, mut done_rx) = mpsc::unbounded_channel::<TaskMsg>();
    let mut sessions: HashMap<String, SessionEntry> = HashMap::new();
    let mut next_session: u64 = 1;
    let mut reader = reader;
    loop {
        tokio::select! {
            incoming = read_line_capped(&mut reader) => {
                let line = match incoming? {
                    IncomingLine::Eof => {
                        // EOF (or stdio close): clean shutdown, no sentinel needed.
                        break;
                    }
                    IncomingLine::TooLong => {
                        write_error(
                            &writer,
                            &serde_json::Value::Null,
                            INVALID_REQUEST,
                            "invalid request: line exceeds the size cap",
                        )
                        .await;
                        continue;
                    }
                    IncomingLine::Line(line) => line,
                };
                if line.trim().is_empty() {
                    continue;
                }
                dispatch_line(
                    &line,
                    &mut next_session,
                    &mut sessions,
                    &base,
                    &assemble,
                    &writer,
                    &done_tx,
                )
                .await;
            }
            msg = done_rx.recv() => {
                let Some(msg) = msg else { break };
                handle_task_msg(msg, &mut sessions, &writer).await;
            }
        }
    }
    // Dropping the job senders tells session tasks to run their bounded
    // shutdown drains; the process exits once they finish.
    sessions.clear();
    Ok(())
}

/// Handle one NDJSON line; prompt replies arrive later via [`handle_task_msg`].
#[allow(clippy::too_many_arguments)]
async fn dispatch_line<W, F, S>(
    line: &str,
    next_session: &mut u64,
    sessions: &mut HashMap<String, SessionEntry>,
    base: &AcpServerOptions,
    assemble: &F,
    writer: &Arc<Mutex<W>>,
    done_tx: &mpsc::UnboundedSender<TaskMsg>,
) where
    W: AsyncWrite + Unpin + Send + 'static,
    F: Fn(AssembleOptions) -> Result<S, SessionError>,
    S: SessionSurface + 'static,
{
    let message: serde_json::Value = match serde_json::from_str(line) {
        Ok(value) => value,
        Err(_) => {
            write_error(
                writer,
                &serde_json::Value::Null,
                PARSE_ERROR,
                "parse error: request line is not valid JSON",
            )
            .await;
            return;
        }
    };
    let object = match message.as_object() {
        Some(object) => object,
        None => {
            write_error(
                writer,
                &serde_json::Value::Null,
                INVALID_REQUEST,
                "invalid request: expected a JSON-RPC object",
            )
            .await;
            return;
        }
    };
    if object.get("jsonrpc").and_then(|v| v.as_str()) != Some("2.0") {
        write_error(
            writer,
            &id_or_null(object),
            INVALID_REQUEST,
            "invalid request: jsonrpc must be \"2.0\"",
        )
        .await;
        return;
    }
    let method = match object.get("method").and_then(|v| v.as_str()) {
        Some(method) => method,
        None => {
            write_error(
                writer,
                &id_or_null(object),
                INVALID_REQUEST,
                "invalid request: missing method",
            )
            .await;
            return;
        }
    };
    let params = object
        .get("params")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    // Notifications never reply. The subset observes exactly one: the
    // spec sends `session/cancel` id-less (Session Cancel), so it takes
    // the same cancel path silently; any other id-less frame is ignored
    // just as quietly.
    let id = match object.get("id") {
        None | Some(serde_json::Value::Null) => {
            if method == "session/cancel" {
                queue_cancel(&params, sessions);
            }
            return;
        }
        Some(id) => id.clone(),
    };
    match method {
        "initialize" => {
            write_success(
                writer,
                &id,
                serde_json::json!({
                    "protocolVersion": ACP_PROTOCOL_VERSION,
                    "agentCapabilities": {
                        "promptCapabilities": { "image": false },
                        "mcpCapabilities": false,
                    },
                }),
            )
            .await;
        }
        "session/new" => {
            new_session(
                &id,
                &params,
                next_session,
                sessions,
                base,
                assemble,
                writer,
                done_tx,
            )
            .await;
        }
        "session/prompt" => {
            start_prompt(&id, &params, sessions, writer).await;
        }
        "session/cancel" => {
            cancel_prompt(&id, &params, sessions, writer).await;
        }
        "session/set_mode" => {
            set_mode(&id, &params, sessions, writer).await;
        }
        "session/release" => {
            release_session(&id, &params, sessions, writer).await;
        }
        _ => {
            write_error(
                writer,
                &id,
                METHOD_NOT_FOUND,
                format!("method not found: {method}"),
            )
            .await;
        }
    }
}

/// Assemble one headless session and answer with its id.
///
/// `cwd` overrides the server default; `mcpServers` is accepted and
/// ignored (servers come from the config file). Assembly failures
/// (missing config, provider, credentials) fail this request, never
/// the server: later requests with a fixed environment can retry.
#[allow(clippy::too_many_arguments)]
async fn new_session<W, F, S>(
    id: &serde_json::Value,
    params: &serde_json::Value,
    next_session: &mut u64,
    sessions: &mut HashMap<String, SessionEntry>,
    base: &AcpServerOptions,
    assemble: &F,
    writer: &Arc<Mutex<W>>,
    done_tx: &mpsc::UnboundedSender<TaskMsg>,
) where
    W: AsyncWrite + Unpin + Send + 'static,
    F: Fn(AssembleOptions) -> Result<S, SessionError>,
    S: SessionSurface + 'static,
{
    let cwd = params
        .get("cwd")
        .and_then(|v| v.as_str())
        .map(PathBuf::from)
        .unwrap_or_else(|| base.cwd.clone());
    // Mint the id before assembly: the compaction footers point at the
    // journal this id names.
    let session_id = format!("sess-{next_session}");
    *next_session += 1;
    let handle = match assemble(AssembleOptions {
        session_id: Some(session_id.clone()),
        config_path: base.config_path.clone(),
        model_override: base.model_override.clone(),
        provider_override: None,
        permission_override: base.permission_override.clone(),
        thinking_override: None,
        cwd,
        home: base.home.clone(),
        identity: DEFAULT_IDENTITY.to_string(),
        // Scripted controllers cannot answer approvals: deny openly
        // instead of parking on a gate nobody reads.
        headless: true,
        initial_history: Vec::new(),
        // ACP sessions start unconstrained; constraint rules arrive
        // through the protocol layer instead of the console settings.
        wave_denylist: Vec::new(),
    }) {
        Ok(handle) => handle,
        Err(e) => {
            write_error(
                writer,
                id,
                INTERNAL_ERROR,
                format!("session assembly failed: {e}"),
            )
            .await;
            return;
        }
    };
    let mode = handle.permission_mode().to_string();
    // Unbounded on purpose: the dispatch loop admits at most one prompt
    // per session (`start_prompt` rejects while `in_flight` is set), so
    // the queue never holds more than a couple of protocol frames
    // (cancel / set_mode alongside the running prompt). Bounding it would
    // only turn a slow session task into a hung JSON-RPC request.
    let (job_tx, job_rx) = mpsc::unbounded_channel();
    tokio::spawn(session_task(
        session_id.clone(),
        handle,
        job_rx,
        writer.clone(),
        done_tx.clone(),
    ));
    sessions.insert(
        session_id.clone(),
        SessionEntry {
            jobs: job_tx,
            in_flight: None,
        },
    );
    write_success(
        writer,
        id,
        serde_json::json!({
            "sessionId": session_id,
            "modes": acp_modes(&mode),
        }),
    )
    .await;
}
/// Queue one turn; the JSON-RPC reply is deferred until the turn ends.
///
/// Text blocks join with newlines; non-text blocks are skipped. A
/// second prompt on a busy session fails instead of queueing: ACP
/// controllers cancel first, so silent queueing would only hide bugs.
async fn start_prompt<W>(
    id: &serde_json::Value,
    params: &serde_json::Value,
    sessions: &mut HashMap<String, SessionEntry>,
    writer: &Arc<Mutex<W>>,
) where
    W: AsyncWrite + Unpin,
{
    let session_id = match params.get("sessionId").and_then(|v| v.as_str()) {
        Some(session_id) => session_id,
        None => {
            write_error(
                writer,
                id,
                INVALID_PARAMS,
                "invalid params: session/prompt requires a string sessionId",
            )
            .await;
            return;
        }
    };
    let Some(entry) = sessions.get_mut(session_id) else {
        write_error(
            writer,
            id,
            INVALID_PARAMS,
            format!("invalid params: unknown sessionId {session_id:?}"),
        )
        .await;
        return;
    };
    let Some(text) = join_prompt_text(params.get("prompt")) else {
        write_error(
            writer,
            id,
            INVALID_PARAMS,
            "invalid params: session/prompt prompt has no text content",
        )
        .await;
        return;
    };
    if entry.in_flight.is_some() {
        write_error(
            writer,
            id,
            INTERNAL_ERROR,
            "a prompt is already running for this session; cancel it first",
        )
        .await;
        return;
    }
    if entry
        .jobs
        .send(SessionJob::Prompt {
            req_id: id.clone(),
            text,
        })
        .is_err()
    {
        write_error(
            writer,
            id,
            INTERNAL_ERROR,
            "session ended before the prompt was delivered",
        )
        .await;
        return;
    }
    entry.in_flight = Some(id.clone());
}

/// Join text blocks of a `session/prompt` prompt; `None` when empty.
fn join_prompt_text(prompt: Option<&serde_json::Value>) -> Option<String> {
    let blocks = prompt?.as_array()?;
    let mut parts = Vec::new();
    for block in blocks {
        let object = block.as_object()?;
        if object.get("type").and_then(|v| v.as_str()) != Some("text") {
            continue;
        }
        parts.push(object.get("text").and_then(|v| v.as_str())?.to_string());
    }
    let text = parts.join("\n");
    if text.trim().is_empty() {
        None
    } else {
        Some(text)
    }
}

/// Queue the interrupt for the session `params` names; a silent no-op
/// when the params carry no string sessionId or name an unknown session
/// (the notification form has no reply channel to report either case).
fn queue_cancel(params: &serde_json::Value, sessions: &mut HashMap<String, SessionEntry>) {
    let Some(session_id) = params.get("sessionId").and_then(|v| v.as_str()) else {
        return;
    };
    if let Some(entry) = sessions.get(session_id) {
        let _ = entry.jobs.send(SessionJob::Cancel);
    }
}

/// Interrupt the in-flight turn; always succeeds for known sessions.
///
/// Cancelling an idle session is a no-op success: controllers cannot
/// know exactly when a turn ended, so strict errors would race every
/// legitimate cancel against completion. The spec delivers cancel as an
/// id-less notification (handled in [`dispatch_line`] via
/// [`queue_cancel`]); the id-bearing request form below keeps a reply
/// so a scripted controller still learns the outcome.
async fn cancel_prompt<W>(
    id: &serde_json::Value,
    params: &serde_json::Value,
    sessions: &mut HashMap<String, SessionEntry>,
    writer: &Arc<Mutex<W>>,
) where
    W: AsyncWrite + Unpin,
{
    let session_id = match params.get("sessionId").and_then(|v| v.as_str()) {
        Some(session_id) => session_id,
        None => {
            write_error(
                writer,
                id,
                INVALID_PARAMS,
                "invalid params: session/cancel requires a string sessionId",
            )
            .await;
            return;
        }
    };
    match sessions.get(session_id) {
        None => {
            write_error(
                writer,
                id,
                INVALID_PARAMS,
                format!("invalid params: unknown sessionId {session_id:?}"),
            )
            .await;
        }
        Some(entry) => {
            let _ = entry.jobs.send(SessionJob::Cancel);
            write_success(writer, id, serde_json::Value::Null).await;
        }
    }
}

/// Switch a session's permission mode (`session/set_mode`).
///
/// Only the three table ids are accepted: legacy wire names never cross
/// the protocol. The reply is immediate (the switch takes effect at the
/// next approval decision, not retroactively on a running turn); an
/// unknown session or mode id fails the request so controllers can
/// surface a typo instead of silently keeping the old mode.
async fn set_mode<W>(
    id: &serde_json::Value,
    params: &serde_json::Value,
    sessions: &mut HashMap<String, SessionEntry>,
    writer: &Arc<Mutex<W>>,
) where
    W: AsyncWrite + Unpin,
{
    let session_id = match params.get("sessionId").and_then(|v| v.as_str()) {
        Some(session_id) => session_id,
        None => {
            write_error(
                writer,
                id,
                INVALID_PARAMS,
                "invalid params: session/set_mode requires a string sessionId",
            )
            .await;
            return;
        }
    };
    let mode_id = match params.get("modeId").and_then(|v| v.as_str()) {
        Some(mode_id) => mode_id,
        None => {
            write_error(
                writer,
                id,
                INVALID_PARAMS,
                "invalid params: session/set_mode requires a string modeId",
            )
            .await;
            return;
        }
    };
    if !matches!(mode_id, "plan" | "auto" | "wave") {
        write_error(
            writer,
            id,
            INVALID_PARAMS,
            format!("invalid params: unknown modeId {mode_id:?}"),
        )
        .await;
        return;
    }
    match sessions.get_mut(session_id) {
        None => {
            write_error(
                writer,
                id,
                INVALID_PARAMS,
                format!("invalid params: unknown sessionId {session_id:?}"),
            )
            .await;
        }
        Some(entry) => {
            // No local mode copy is kept: the actor is the source of
            // truth (a queued switch can be rejected there), and ACP
            // has no query surface that would need the mirror.
            let _ = entry.jobs.send(SessionJob::SetMode {
                mode: mode_id.to_string(),
            });
            write_success(writer, id, serde_json::Value::Null).await;
        }
    }
}

/// Drop a session and free its task: the entry (and its job sender)
/// leaves the map, so the session task sees the channel close and runs
/// its bounded shutdown. Long-running hosts (editor integrations) call
/// this per closed conversation; without it every session would stay
/// parked in the map until process exit.
async fn release_session<W>(
    id: &serde_json::Value,
    params: &serde_json::Value,
    sessions: &mut HashMap<String, SessionEntry>,
    writer: &Arc<Mutex<W>>,
) where
    W: AsyncWrite + Unpin,
{
    let session_id = match params.get("sessionId").and_then(|v| v.as_str()) {
        Some(session_id) => session_id,
        None => {
            write_error(
                writer,
                id,
                INVALID_PARAMS,
                "invalid params: session/release requires a string sessionId",
            )
            .await;
            return;
        }
    };
    if sessions.remove(session_id).is_none() {
        write_error(
            writer,
            id,
            INVALID_PARAMS,
            format!("invalid params: unknown sessionId {session_id:?}"),
        )
        .await;
        return;
    }
    write_success(writer, id, serde_json::Value::Null).await;
}

/// Answer a deferred prompt once its turn ends.
async fn handle_task_msg<W>(
    msg: TaskMsg,
    sessions: &mut HashMap<String, SessionEntry>,
    writer: &Arc<Mutex<W>>,
) where
    W: AsyncWrite + Unpin,
{
    match sessions.get_mut(&msg.session_id) {
        Some(entry) if entry.in_flight.as_ref() == Some(&msg.req_id) => {
            entry.in_flight = None;
        }
        _ => {}
    }
    match msg.outcome {
        PromptOutcome::Stop(reason) => {
            write_success(
                writer,
                &msg.req_id,
                serde_json::json!({ "stopReason": reason }),
            )
            .await;
        }
        PromptOutcome::Failed(message) => {
            write_error(writer, &msg.req_id, INTERNAL_ERROR, message).await;
        }
    }
}

/// Own one session: run prompted turns serially and stream their events
/// as `session/update` notifications.
///
/// The JSON-RPC reply for a prompt leaves through the serve loop when
/// this reports [`TaskMsg`]; cancels set a flag so the stop reason
/// reads `cancelled` even when the actor already passed its last
/// interrupt checkpoint (the `exec` Ctrl-C contract).
async fn session_task<S, W>(
    session_id: String,
    mut session: S,
    mut jobs: mpsc::UnboundedReceiver<SessionJob>,
    writer: Arc<Mutex<W>>,
    done: mpsc::UnboundedSender<TaskMsg>,
) where
    W: AsyncWrite + Unpin + Send,
    S: SessionSurface,
{
    let mut submissions: u64 = 1;
    // Request id plus cancel flag of the running turn, if any.
    let mut in_flight: Option<(serde_json::Value, bool)> = None;
    loop {
        tokio::select! {
            job = jobs.recv() => {
                match job {
                    // All senders dropped (EOF shutdown): bounded drain so
                    // SessionEnd hooks speak instead of dying on drop.
                    None => {
                        shutdown_session(&mut session).await;
                        return;
                    }
                    Some(SessionJob::Cancel) => {
                        if let Some((_, cancelled)) = in_flight.as_mut() {
                            *cancelled = true;
                            let _ = session
                                .submit(Submission {
                                    id: format!("acp-{session_id}-cancel"),
                                    op: Op::Interrupt,
                                })
                                .await;
                        }
                    }
                    Some(SessionJob::SetMode { mode }) => {
                        // Pre-validated by set_mode; a rejection here can
                        // only mean a fixed gateway, which surfaces as a
                        // warning event instead of failing the protocol.
                        let _ = session
                            .submit(Submission {
                                id: format!("acp-{session_id}-mode"),
                                op: Op::SetPermissionMode { mode },
                            })
                            .await;
                    }
                    Some(SessionJob::Prompt { req_id, text }) => {
                        if in_flight.is_some() {
                            // The loop guards this; answering anyway keeps a
                            // buggy controller unblocked instead of hung.
                            let _ = done.send(TaskMsg {
                                session_id: session_id.clone(),
                                req_id,
                                outcome: PromptOutcome::Failed(
                                    "a prompt is already running for this session".to_string(),
                                ),
                            });
                            continue;
                        }
                        let submission = format!("acp-{session_id}-{submissions}");
                        submissions += 1;
                        if session
                            .submit(Submission {
                                id: submission,
                                op: Op::UserInput {
                                text,
                                images: Vec::new(),
                            },
                            })
                            .await
                            .is_err()
                        {
                            let _ = done.send(TaskMsg {
                                session_id: session_id.clone(),
                                req_id,
                                outcome: PromptOutcome::Failed(
                                    "session ended before the prompt was delivered".to_string(),
                                ),
                            });
                            return;
                        }
                        in_flight = Some((req_id, false));
                    }
                }
            }
            event = session.next_event(), if in_flight.is_some() => {
                let Some(event) = event else {
                    // Actor exited mid-turn without TurnCompleted.
                    if let Some((req_id, _)) = in_flight.take() {
                        let _ = done.send(TaskMsg {
                            session_id: session_id.clone(),
                            req_id,
                            outcome: PromptOutcome::Failed(
                                "session ended unexpectedly".to_string(),
                            ),
                        });
                    }
                    return;
                };
                if let Some(end) = handle_turn_event(&event.msg, &session_id, &writer).await {
                    let Some((req_id, cancelled)) = in_flight.take() else {
                        continue;
                    };
                    let outcome = match end {
                        TurnEnd::Done { interrupted } if cancelled || interrupted => {
                            PromptOutcome::Stop("cancelled".to_string())
                        }
                        TurnEnd::Done { .. } => PromptOutcome::Stop("end_turn".to_string()),
                        TurnEnd::Fatal(message) => PromptOutcome::Failed(message),
                    };
                    let _ = done.send(TaskMsg {
                        session_id: session_id.clone(),
                        req_id,
                        outcome,
                    });
                }
            }
        }
    }
}

/// Bounded shutdown drain after EOF: SessionEnd hooks speak instead of
/// dying on a dropped session; a hung hook cannot hold exit past the
/// deadline.
async fn shutdown_session<S: SessionSurface>(session: &mut S) {
    let _ = session
        .submit(Submission {
            id: "acp-shutdown".to_string(),
            op: Op::Shutdown,
        })
        .await;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    while tokio::time::timeout_at(deadline, session.next_event())
        .await
        .ok()
        .flatten()
        .is_some()
    {}
}

/// Terminal turn state: done (with the interrupt flag) or harness-failed.
enum TurnEnd {
    /// `TurnCompleted` arrived.
    Done {
        /// True when the run stopped at an interrupt checkpoint.
        interrupted: bool,
    },
    /// A fatal harness error ended the turn.
    Fatal(String),
}

/// Map one wire event onto notifications; `Some` ends the turn.
async fn handle_turn_event<W>(
    msg: &EventMsg,
    session_id: &str,
    writer: &Arc<Mutex<W>>,
) -> Option<TurnEnd>
where
    W: AsyncWrite + Unpin,
{
    match msg {
        EventMsg::AgentMessageDelta { text } => {
            if !text.is_empty() {
                write_notification(
                    writer,
                    "session/update",
                    serde_json::json!({
                        "sessionId": session_id,
                        "update": {
                            "sessionUpdate": "agent_message_chunk",
                            "content": { "type": "text", "text": text },
                        },
                    }),
                )
                .await;
            }
            None
        }
        EventMsg::AgentThinkingDelta { text } => {
            if !text.is_empty() {
                // ACP has a dedicated thought chunk kind; reasoning trace
                // maps there, never onto the message chunk.
                write_notification(
                    writer,
                    "session/update",
                    serde_json::json!({
                        "sessionId": session_id,
                        "update": {
                            "sessionUpdate": "agent_thought_chunk",
                            "content": { "type": "text", "text": text },
                        },
                    }),
                )
                .await;
            }
            None
        }
        EventMsg::ToolCallBegin { call_id, name, .. } => {
            write_notification(
                writer,
                "session/update",
                serde_json::json!({
                    "sessionId": session_id,
                    "update": {
                        "sessionUpdate": "tool_call",
                        "toolCallId": call_id,
                        "title": name,
                        "kind": "other",
                        "status": "in_progress",
                    },
                }),
            )
            .await;
            None
        }
        EventMsg::ToolCallEnd {
            call_id, is_error, ..
        } => {
            write_notification(
                writer,
                "session/update",
                serde_json::json!({
                    "sessionId": session_id,
                    "update": {
                        "sessionUpdate": "tool_call_update",
                        "toolCallId": call_id,
                        "status": if *is_error { "failed" } else { "completed" },
                    },
                }),
            )
            .await;
            None
        }
        EventMsg::TurnCompleted { interrupted } => Some(TurnEnd::Done {
            interrupted: *interrupted,
        }),
        EventMsg::Error {
            message,
            recoverable,
            ..
        } => {
            if *recoverable {
                None
            } else {
                Some(TurnEnd::Fatal(message.clone()))
            }
        }
        // Completions repeat already-streamed text; approvals die on the
        // headless gate while usage, compaction, and warnings stay inside
        // the turn with no ACP surface in this subset.
        _ => None,
    }
}

/// Notification envelope (no id, never answered).
fn notification(method: &str, params: serde_json::Value) -> serde_json::Value {
    serde_json::json!({"jsonrpc": "2.0", "method": method, "params": params})
}

/// Write one NDJSON frame; a closed reader ends writes silently.
async fn write_line<W>(writer: &Arc<Mutex<W>>, value: &serde_json::Value)
where
    W: AsyncWrite + Unpin,
{
    let mut line = value.to_string();
    line.push('\n');
    let mut guard = writer.lock().await;
    if guard.write_all(line.as_bytes()).await.is_err() {
        return;
    }
    let _ = guard.flush().await;
}

/// Answer a request successfully.
async fn write_success<W>(writer: &Arc<Mutex<W>>, id: &serde_json::Value, result: serde_json::Value)
where
    W: AsyncWrite + Unpin,
{
    write_line(writer, &success_response(id.clone(), result)).await;
}

/// Fail a request with a JSON-RPC error code.
async fn write_error<W>(
    writer: &Arc<Mutex<W>>,
    id: &serde_json::Value,
    code: i32,
    message: impl Into<String>,
) where
    W: AsyncWrite + Unpin,
{
    write_line(writer, &error_response(id.clone(), code, message)).await;
}

/// Emit a server-to-client notification.
async fn write_notification<W>(writer: &Arc<Mutex<W>>, method: &str, params: serde_json::Value)
where
    W: AsyncWrite + Unpin,
{
    write_line(writer, &notification(method, params)).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use tokio::io::AsyncBufReadExt as _;
    use tokio::io::DuplexStream;
    use wavecode_llm::{ChatModel, ChatRequest, EventStream, StreamEvent, Usage};

    use crate::test_stubs::{CONFIG, OneShotModel, done_script};
    use infrastructure_base::InterruptHandle;
    use operations_bootstrap::SessionHandle;
    use operations_bootstrap::session::{WithModel, assemble_session_with_model};

    /// Model blocked on a gate: prompt turns stay in flight until the
    /// test releases them, so cancel paths run deterministically.
    struct GateModel {
        gate: Arc<TurnGate>,
    }

    #[async_trait::async_trait]
    impl ChatModel for GateModel {
        async fn stream(&self, _req: ChatRequest) -> wavecode_llm::Result<EventStream> {
            self.gate.entered.notify_one();
            self.gate.release.notified().await;
            Ok(Box::pin(futures::stream::iter(
                done_script().into_iter().map(Ok),
            )))
        }
    }

    /// Barrier between the test and the gated model, both directions
    /// signal-safe (each `Notify` stores a permit, so wake order never
    /// loses a signal).
    struct TurnGate {
        /// Fired by the model once `stream` is entered: the turn is
        /// provably running and past the runner's turn-start interrupt
        /// reset, so a cancel from here on cannot be swallowed.
        entered: tokio::sync::Notify,
        /// Fired by the test to let the model finish the turn.
        release: tokio::sync::Notify,
    }

    /// Test-side controls for the gated model plus the assembled
    /// session's interrupt handle: together they turn the cancel tests'
    /// ordering hopes into observations.
    #[derive(Clone)]
    struct TurnControls {
        gate: Arc<TurnGate>,
        /// Filled by the test assembler with the session's interrupt
        /// handle; triggering it is the actor's last step when serving a
        /// queued cancel.
        interrupt: Arc<std::sync::Mutex<Option<InterruptHandle>>>,
    }

    impl TurnControls {
        fn new() -> Self {
            Self {
                gate: Arc::new(TurnGate {
                    entered: tokio::sync::Notify::new(),
                    release: tokio::sync::Notify::new(),
                }),
                interrupt: Arc::new(std::sync::Mutex::new(None)),
            }
        }

        /// Wait until the gated model entered its stream: the turn is
        /// in flight and its interrupt reset already ran.
        async fn wait_turn_started(&self) {
            self.gate.entered.notified().await;
        }

        /// Wait until the queued cancel reached the actor (the interrupt
        /// flag is the actor's own acknowledgement). Releasing the model
        /// only after this makes the `cancelled` stop reason independent
        /// of scheduling: the gateway's cancel flag is already set and no
        /// turn-end event can exist yet (the model is still parked).
        async fn wait_cancel_landed(&self) {
            let handle = self
                .interrupt
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
                .expect("session must be assembled before waiting for the cancel");
            while !handle.is_triggered() {
                tokio::task::yield_now().await;
            }
        }

        /// Let the gated model finish the turn.
        fn release_turn(&self) {
            self.gate.release.notify_one();
        }
    }

    /// Scripted write turn: text, a write tool call, completion.
    fn write_script() -> Vec<StreamEvent> {
        vec![
            StreamEvent::TextDelta {
                text: "working".to_string(),
            },
            StreamEvent::ToolUseBegin {
                id: "c1".to_string(),
                name: "write".to_string(),
            },
            StreamEvent::ToolUseInputDelta {
                partial_json: r#"{"path":"hello.txt","content":"wavecode-acp-ok"}"#.to_string(),
            },
            StreamEvent::BlockEnd,
            StreamEvent::MessageComplete {
                stop_reason: "end_turn".to_string(),
                usage: Usage {
                    input_tokens: 10,
                    output_tokens: 5,
                    ..Usage::default()
                },
            },
        ]
    }

    /// Test assembler: production-shaped sessions around stub models.
    ///
    /// Honors the requested config path exactly like production (so
    /// assembly failures map the same way) and pops one queued script
    /// per session; the gated model overrides scripts when controls
    /// are set, and the session's interrupt handle lands in the
    /// controls for the cancel tests to observe.
    struct TestAssemble {
        scripts: Arc<std::sync::Mutex<VecDeque<Vec<StreamEvent>>>>,
        fallback_config: PathBuf,
        controls: Option<TurnControls>,
    }

    impl TestAssemble {
        fn assemble(&self, options: AssembleOptions) -> Result<SessionHandle, SessionError> {
            let path = options
                .config_path
                .clone()
                .unwrap_or_else(|| self.fallback_config.clone());
            let config = wavecode_config::Config::load_from(&path).map_err(SessionError::Config)?;
            let model_name = options
                .model_override
                .clone()
                .unwrap_or_else(|| config.model.clone());
            let model: Arc<dyn ChatModel> = match &self.controls {
                Some(controls) => Arc::new(GateModel {
                    gate: controls.gate.clone(),
                }),
                None => {
                    let script = self
                        .scripts
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .pop_front()
                        .unwrap_or_else(done_script);
                    Arc::new(OneShotModel::new(script))
                }
            };
            let handle = assemble_session_with_model(WithModel {
                config,
                model,
                model_name,
                provider_id: "test".to_string(),
                thinking_effort: None,
                deny_env: Vec::new(),
                context_window: 200_000,
                max_output_tokens: 64,
                per_model_window: None,
                wave_denylist: Vec::new(),
                session_id: None,
                // Bypass approvals: scripted tools must execute instead of
                // dying on the headless deny gate (the same ground as the
                // session proof test). Approval-denied runs are orthogonal
                // to the notification mapping asserted here.
                permission_override: Some("bypassPermissions".to_string()),
                cwd: options.cwd,
                home: options.home,
                identity: options.identity,
                headless: true,
                initial_history: Vec::new(),
                warnings: Vec::new(),
            });
            if let Some(controls) = &self.controls {
                *controls.interrupt.lock().unwrap_or_else(|e| e.into_inner()) =
                    Some(handle.interrupt());
            }
            Ok(handle)
        }
    }

    /// Duplex-driven client speaking to a served loop in-process.
    struct Harness {
        to_server: DuplexStream,
        from_server: tokio::io::BufReader<DuplexStream>,
        next_id: u64,
        /// Kept alive: session working directories live under it.
        _tmp: tempfile::TempDir,
    }

    impl Harness {
        async fn send(&mut self, value: &serde_json::Value) {
            let mut line = value.to_string();
            line.push('\n');
            self.to_server.write_all(line.as_bytes()).await.unwrap();
        }

        async fn next_line(&mut self) -> serde_json::Value {
            let mut buf = String::new();
            let n = self.from_server.read_line(&mut buf).await.unwrap();
            assert!(n > 0, "server closed the stream");
            serde_json::from_str(&buf).unwrap()
        }

        /// Request/response with notifications skipped; dedicated tests
        /// assert on notifications via `next_line`/`run_prompt` instead.
        async fn request(&mut self, method: &str, params: serde_json::Value) -> serde_json::Value {
            let id = self.next_id;
            self.next_id += 1;
            self.send(&serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": method,
                "params": params,
            }))
            .await;
            loop {
                let line = self.next_line().await;
                if line.get("id") == Some(&serde_json::json!(id)) {
                    return line;
                }
            }
        }

        /// Prompt a session, collecting notifications until its reply.
        async fn run_prompt(
            &mut self,
            session: &str,
            text: &str,
        ) -> (Vec<serde_json::Value>, serde_json::Value) {
            let id = self.next_id;
            self.next_id += 1;
            self.send(&serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": "session/prompt",
                "params": {
                    "sessionId": session,
                    "prompt": [{ "type": "text", "text": text }],
                },
            }))
            .await;
            let mut notes = Vec::new();
            loop {
                let line = self.next_line().await;
                if line.get("id") == Some(&serde_json::json!(id)) {
                    return (notes, line);
                }
                notes.push(line);
            }
        }

        async fn new_session(&mut self, params: serde_json::Value) -> String {
            let reply = self.request("session/new", params).await;
            reply
                .pointer("/result/sessionId")
                .and_then(|v| v.as_str())
                .expect("session/new must return a sessionId")
                .to_string()
        }
    }

    async fn spawn_full(
        scripts: Vec<Vec<StreamEvent>>,
        controls: Option<TurnControls>,
        config_path: Option<PathBuf>,
    ) -> (Harness, tokio::task::JoinHandle<std::io::Result<()>>) {
        let tmp = tempfile::tempdir().unwrap();
        let fallback = tmp.path().join("config.toml");
        std::fs::write(&fallback, CONFIG).unwrap();
        let cwd = tmp.path().to_path_buf();
        let (client_w, server_r) = tokio::io::duplex(65536);
        let (server_w, client_r) = tokio::io::duplex(65536);
        let assemble = TestAssemble {
            scripts: Arc::new(std::sync::Mutex::new(scripts.into_iter().collect())),
            fallback_config: fallback,
            controls,
        };
        let base = AcpServerOptions {
            config_path,
            model_override: None,
            permission_override: None,
            cwd,
            home: None,
        };
        let server = tokio::spawn(async move {
            serve_loop(
                tokio::io::BufReader::new(server_r),
                server_w,
                base,
                move |options| assemble.assemble(options),
            )
            .await
        });
        (
            Harness {
                to_server: client_w,
                from_server: tokio::io::BufReader::new(client_r),
                next_id: 1,
                _tmp: tmp,
            },
            server,
        )
    }

    async fn spawn_server(
        scripts: Vec<Vec<StreamEvent>>,
    ) -> (Harness, tokio::task::JoinHandle<std::io::Result<()>>) {
        spawn_full(scripts, None, None).await
    }

    #[tokio::test]
    async fn initialize_replies_with_subset_capabilities() {
        let (mut h, _server) = spawn_server(vec![]).await;
        let reply = h
            .request(
                "initialize",
                serde_json::json!({ "protocolVersion": ACP_PROTOCOL_VERSION }),
            )
            .await;
        assert_eq!(
            reply
                .pointer("/result/protocolVersion")
                .and_then(|v| v.as_str()),
            Some(ACP_PROTOCOL_VERSION)
        );
        let caps = reply.pointer("/result/agentCapabilities").unwrap();
        assert_eq!(
            caps.pointer("/promptCapabilities/image"),
            Some(&serde_json::Value::Bool(false))
        );
        assert_eq!(
            caps.get("mcpCapabilities"),
            Some(&serde_json::Value::Bool(false))
        );
    }

    #[tokio::test]
    async fn session_new_response_carries_the_mode_table() {
        let (mut h, _server) = spawn_server(vec![done_script()]).await;
        let reply = h.request("session/new", serde_json::json!({})).await;
        let session = reply
            .pointer("/result/sessionId")
            .and_then(|v| v.as_str())
            .expect("session/new must return a sessionId")
            .to_string();
        // The table names the three wire modes and the assembly-resolved
        // current pick; controllers render it and key set_mode off it.
        let current = reply
            .pointer("/result/modes/currentModeId")
            .and_then(|v| v.as_str())
            .expect("modes.currentModeId")
            .to_string();
        assert_eq!(
            reply
                .pointer("/result/modes/availableModes")
                .and_then(|v| v.as_array())
                .map(|modes| {
                    modes
                        .iter()
                        .filter_map(|m| m.get("id").and_then(|v| v.as_str()))
                        .collect::<Vec<_>>()
                }),
            Some(vec!["plan", "auto", "wave"])
        );
        assert!(
            ["plan", "auto", "wave"].contains(&current.as_str()),
            "currentModeId {current:?} must be one of the table ids"
        );

        // A valid switch answers empty; the table id set is enforced.
        let switched = h
            .request(
                "session/set_mode",
                serde_json::json!({"sessionId": session, "modeId": "plan"}),
            )
            .await;
        assert!(switched.get("error").is_none(), "{switched}");
    }

    #[tokio::test]
    async fn set_mode_rejects_unknown_ids_and_sessions() {
        let (mut h, _server) = spawn_server(vec![done_script()]).await;
        let session = h.new_session(serde_json::json!({})).await;
        for params in [
            serde_json::json!({"sessionId": session, "modeId": "yolo"}),
            serde_json::json!({"sessionId": session, "modeId": ""}),
            serde_json::json!({"sessionId": "sess-999", "modeId": "auto"}),
        ] {
            let reply = h.request("session/set_mode", params).await;
            assert_eq!(
                reply.pointer("/error/code"),
                Some(&serde_json::json!(-32602)),
                "{reply}"
            );
        }
    }

    #[tokio::test]
    async fn prompt_streams_text_then_reports_end_turn() {
        let (mut h, _server) = spawn_server(vec![done_script()]).await;
        let session = h.new_session(serde_json::json!({})).await;
        let (notes, reply) = h.run_prompt(&session, "hello").await;
        let chunks: Vec<&str> = notes
            .iter()
            .filter(|n| {
                n.pointer("/params/update/sessionUpdate")
                    .and_then(|v| v.as_str())
                    == Some("agent_message_chunk")
            })
            .filter_map(|n| {
                n.pointer("/params/update/content/text")
                    .and_then(|v| v.as_str())
            })
            .collect();
        assert_eq!(chunks, vec!["done"]);
        assert_eq!(
            reply.pointer("/result/stopReason").and_then(|v| v.as_str()),
            Some("end_turn")
        );
    }

    #[tokio::test]
    async fn prompt_maps_tool_calls_to_updates_and_writes_files() {
        let (mut h, _server) = spawn_server(vec![write_script()]).await;
        let session = h.new_session(serde_json::json!({})).await;
        let (notes, reply) = h.run_prompt(&session, "write it").await;
        assert_eq!(
            reply.pointer("/result/stopReason").and_then(|v| v.as_str()),
            Some("end_turn")
        );
        let begin = notes
            .iter()
            .find(|n| {
                n.pointer("/params/update/sessionUpdate")
                    .and_then(|v| v.as_str())
                    == Some("tool_call")
            })
            .expect("a tool_call notification must stream");
        assert_eq!(
            begin
                .pointer("/params/update/toolCallId")
                .and_then(|v| v.as_str()),
            Some("c1")
        );
        assert_eq!(
            begin
                .pointer("/params/update/title")
                .and_then(|v| v.as_str()),
            Some("write")
        );
        let end = notes
            .iter()
            .find(|n| {
                n.pointer("/params/update/sessionUpdate")
                    .and_then(|v| v.as_str())
                    == Some("tool_call_update")
            })
            .expect("a tool_call_update notification must stream");
        assert_eq!(
            end.pointer("/params/update/status")
                .and_then(|v| v.as_str()),
            Some("completed")
        );
        // The tool really ran: content landed in the session cwd.
        let root = h._tmp.path().join("hello.txt");
        assert_eq!(std::fs::read_to_string(&root).unwrap(), "wavecode-acp-ok");
    }

    #[tokio::test]
    async fn unknown_methods_fail_with_method_not_found() {
        let (mut h, _server) = spawn_server(vec![]).await;
        let reply = h.request("session/frobnicate", serde_json::json!({})).await;
        assert_eq!(
            reply.pointer("/error/code"),
            Some(&serde_json::json!(METHOD_NOT_FOUND))
        );
    }

    #[tokio::test]
    async fn notifications_are_ignored() {
        let (mut h, _server) = spawn_server(vec![]).await;
        h.send(&serde_json::json!({"jsonrpc": "2.0", "method": "ping"}))
            .await;
        // No reply arrives for the notification; the next request still
        // gets the next line, proving nothing was emitted in between.
        let reply = h
            .request(
                "initialize",
                serde_json::json!({ "protocolVersion": ACP_PROTOCOL_VERSION }),
            )
            .await;
        assert!(reply.get("result").is_some());
        let idle = tokio::time::timeout(std::time::Duration::from_millis(50), h.next_line()).await;
        assert!(idle.is_err(), "notification produced a reply");
    }

    #[tokio::test]
    async fn unknown_sessions_fail_with_invalid_params() {
        let (mut h, _server) = spawn_server(vec![]).await;
        let prompt = h
            .request(
                "session/prompt",
                serde_json::json!({
                    "sessionId": "sess-999",
                    "prompt": [{ "type": "text", "text": "hi" }],
                }),
            )
            .await;
        assert_eq!(
            prompt.pointer("/error/code"),
            Some(&serde_json::json!(INVALID_PARAMS))
        );
        let cancel = h
            .request(
                "session/cancel",
                serde_json::json!({ "sessionId": "sess-999" }),
            )
            .await;
        assert_eq!(
            cancel.pointer("/error/code"),
            Some(&serde_json::json!(INVALID_PARAMS))
        );
    }

    #[tokio::test]
    async fn assembly_failures_fail_session_new_not_the_server() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("missing.toml");
        let (mut h, _server) = spawn_full(vec![], None, Some(missing)).await;
        let reply = h.request("session/new", serde_json::json!({})).await;
        assert_eq!(
            reply.pointer("/error/code"),
            Some(&serde_json::json!(INTERNAL_ERROR))
        );
        let message = reply
            .pointer("/error/message")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        assert!(
            message.contains("session assembly failed"),
            "unexpected message: {message}"
        );
        // The server still answers afterwards.
        let init = h
            .request(
                "initialize",
                serde_json::json!({ "protocolVersion": ACP_PROTOCOL_VERSION }),
            )
            .await;
        assert!(init.get("result").is_some());
    }

    #[tokio::test]
    async fn cancel_interrupts_the_in_flight_turn() {
        let controls = TurnControls::new();
        let (mut h, _server) = spawn_full(vec![], Some(controls.clone()), None).await;
        let session = h.new_session(serde_json::json!({})).await;
        // Send the prompt without waiting: the gated model holds the
        // turn open until released below.
        let id = h.next_id;
        h.next_id += 1;
        h.send(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "session/prompt",
            "params": {
                "sessionId": session,
                "prompt": [{ "type": "text", "text": "take your time" }],
            },
        }))
        .await;
        // The model entering its stream proves the turn is running; the
        // queued cancel must then reach the actor before the model is
        // freed, so the cancel can no longer lose the race.
        controls.wait_turn_started().await;
        let cancel = h
            .request(
                "session/cancel",
                serde_json::json!({ "sessionId": session }),
            )
            .await;
        assert_eq!(cancel.get("result"), Some(&serde_json::Value::Null));
        controls.wait_cancel_landed().await;
        controls.release_turn();
        let reply = loop {
            let line = h.next_line().await;
            if line.get("id") == Some(&serde_json::json!(id)) {
                break line;
            }
        };
        assert_eq!(
            reply.pointer("/result/stopReason").and_then(|v| v.as_str()),
            Some("cancelled")
        );
    }

    /// The spec delivers `session/cancel` as an id-less notification:
    /// it must interrupt the in-flight turn and produce no reply line.
    #[tokio::test]
    async fn cancel_notification_interrupts_without_replying() {
        let controls = TurnControls::new();
        let (mut h, _server) = spawn_full(vec![], Some(controls.clone()), None).await;
        let session = h.new_session(serde_json::json!({})).await;
        let id = h.next_id;
        h.next_id += 1;
        h.send(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "session/prompt",
            "params": {
                "sessionId": session,
                "prompt": [{ "type": "text", "text": "take your time" }],
            },
        }))
        .await;
        // The model entering its stream proves the turn is running; the
        // notification's cancel must then reach the actor before the
        // model is freed, so the cancel can no longer lose the race.
        controls.wait_turn_started().await;
        // The spec shape: no id, no response expected.
        h.send(&serde_json::json!({
            "jsonrpc": "2.0",
            "method": "session/cancel",
            "params": { "sessionId": session },
        }))
        .await;
        controls.wait_cancel_landed().await;
        controls.release_turn();
        let reply = loop {
            let line = h.next_line().await;
            if line.get("id") == Some(&serde_json::json!(id)) {
                break line;
            }
        };
        assert_eq!(
            reply.pointer("/result/stopReason").and_then(|v| v.as_str()),
            Some("cancelled")
        );
        // Nothing further was emitted: the notification itself drew no
        // reply, and the ended turn left the stream quiet.
        let idle = tokio::time::timeout(std::time::Duration::from_millis(50), h.next_line()).await;
        assert!(idle.is_err(), "cancel notification produced a reply");
    }

    /// An id-less cancel naming an unknown session stays silent instead
    /// of erroring: the notification form has no reply channel.
    #[tokio::test]
    async fn cancel_notification_for_unknown_session_stays_silent() {
        let (mut h, _server) = spawn_server(vec![]).await;
        h.send(&serde_json::json!({
            "jsonrpc": "2.0",
            "method": "session/cancel",
            "params": { "sessionId": "sess-999" },
        }))
        .await;
        // The next request still gets the next line, proving nothing was
        // emitted in between.
        let reply = h
            .request(
                "initialize",
                serde_json::json!({ "protocolVersion": ACP_PROTOCOL_VERSION }),
            )
            .await;
        assert!(reply.get("result").is_some());
    }

    #[tokio::test]
    async fn malformed_frames_fail_without_killing_the_server() {
        let (mut h, _server) = spawn_server(vec![]).await;
        h.send(&serde_json::json!([1, 2, 3])).await;
        let batch = h.next_line().await;
        assert_eq!(
            batch.pointer("/error/code"),
            Some(&serde_json::json!(INVALID_REQUEST))
        );
        // Raw non-JSON text is not valid NDJSON JSON-RPC either.
        use tokio::io::AsyncWriteExt as _;
        h.to_server.write_all(b"not json\n").await.unwrap();
        let parse = h.next_line().await;
        assert_eq!(
            parse.pointer("/error/code"),
            Some(&serde_json::json!(PARSE_ERROR))
        );
        let init = h
            .request(
                "initialize",
                serde_json::json!({ "protocolVersion": ACP_PROTOCOL_VERSION }),
            )
            .await;
        assert!(init.get("result").is_some());
    }
    /// `session/release` drops the session: the reply acknowledges, and
    /// later prompts on the released id fail as unknown sessions — a
    /// long-running host must be able to reclaim sessions.
    #[tokio::test]
    async fn release_drops_the_session_and_frees_the_id() {
        let (mut h, _server) = spawn_server(vec![]).await;
        let session = h.new_session(serde_json::json!({})).await;
        let reply = h
            .request("session/release", serde_json::json!({"sessionId": session}))
            .await;
        assert_eq!(reply.pointer("/result"), Some(&serde_json::Value::Null));
        let late = h
            .request(
                "session/prompt",
                serde_json::json!({"sessionId": session, "prompt": "hi"}),
            )
            .await;
        assert!(
            late.pointer("/error/message")
                .and_then(|m| m.as_str())
                .unwrap_or_default()
                .contains("unknown sessionId"),
            "the released session must be gone: {late:?}"
        );
    }
}
