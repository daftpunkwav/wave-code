/*!
 * @file AppServer
 * @description Local HTTP server exposing live sessions over REST + SSE.
 *
 * Responsibilities:
 * - Assemble one parking-enabled session per `POST /sessions`.
 * - Stream wire events to any number of SSE subscribers per session.
 * - Deliver approval/question decisions from HTTP handlers to the gates.
 * - Enforce bearer-token auth and loopback-only Host headers.
 *
 * Architecture: each session owns a pump task that holds the actor
 * client exclusively — it forwards wire events onto a broadcast channel
 * (SSE subscribers) and executes submissions arriving on a command
 * channel (HTTP handlers), so event consumption and submission never
 * race on the client.
 *
 * Session lifecycle: besides an explicit `DELETE /sessions/{id}`, a
 * background reaper closes sessions idle longer than [`SESSION_IDLE_TTL`]
 * (through the same removal path as the DELETE), because a crashed
 * client can never call DELETE and would otherwise leak its actor task,
 * journal, and MCP children forever. Every client-driven interaction —
 * submitting, deciding, answering, cancelling, subscribing, and every
 * forwarded turn event — resets the idle clock, so a session in active
 * use is never reaped. Creating beyond [`MAX_SESSIONS`] live sessions
 * fails with 503 (refusal, not eviction: evicting could kill a turn).
 *
 * Security posture: binds the loopback interface only; every route
 * except `/healthz` requires `Authorization: Bearer <token>`; the Host
 * header must name the loopback host (DNS-rebinding guard — an attacker
 * page cannot reach the server under its own hostname even if it
 * guesses the port). The token is generated per run and printed once on
 * startup.
 *
 * This module must not depend on: frontends (the binary drives it).
 */

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use operations_actor::{AssembleOptions, DEFAULT_IDENTITY, SessionError, SessionSurface};
use tokio::sync::{Notify, broadcast, mpsc};
use wavecode_wire::{Event, EventMsg, Op, Submission};

/// Idle TTL after which the reaper closes a served session.
///
/// The longest legitimate quiet stretch inside one running turn is bounded
/// far below this: an approval park resolves within the assembly's
/// `APPROVAL_TIMEOUT` (120s) and transport deadlines cap around 60s, so a
/// turn that is alive always produces activity (an event, a submit, a
/// decision) long before the TTL. A crashed client's session — the leak
/// this reaps — therefore closes within half an hour instead of never.
pub const SESSION_IDLE_TTL: Duration = Duration::from_secs(30 * 60);

/// Cap on concurrently live sessions; creations beyond it answer 503.
///
/// Refusal instead of eviction: evicting a session the reaper cannot
/// prove idle could kill a mid-turn client, and the local single-user
/// server never legitimately holds this many.
pub const MAX_SESSIONS: usize = 64;

/// Milliseconds from the server's monotonic start, for idle bookkeeping.
fn elapsed_millis(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// Server inputs; sessions inherit these at assembly.
#[derive(Debug, Clone)]
pub struct ServeOptions {
    /// Config file override passed through to assembly.
    pub config_path: Option<std::path::PathBuf>,
    /// Model override passed through to assembly.
    pub model_override: Option<String>,
    /// Working directory for sessions that omit one.
    pub cwd: std::path::PathBuf,
    /// Home directory for the session journal.
    pub home: Option<std::path::PathBuf>,
    /// Bearer token every authenticated route requires. Must be
    /// non-empty: an empty token would accept a bare `Bearer ` header
    /// from any local process, so [`serve`] rejects it up front.
    pub token: String,
    /// Bind port; 0 picks an ephemeral port (the bound port is returned).
    pub port: u16,
    /// Idle TTL the reaper enforces; `None` uses [`SESSION_IDLE_TTL`].
    /// Tests pass a short value to observe the reap quickly.
    pub session_idle_ttl: Option<Duration>,
    /// Cap on live sessions; `None` uses [`MAX_SESSIONS`]. Tests pass a
    /// small value to observe the 503 without 64 assemblies.
    pub max_sessions: Option<usize>,
}

/// Commands a session's pump task executes on the owned actor client.
enum SessionCommand {
    Submit {
        submission_id: String,
        op: Op,
        reply: tokio::sync::oneshot::Sender<Result<(), String>>,
    },
}

/// One live session: a command channel into the pump task plus the
/// gates HTTP handlers answer directly.
struct AppSession {
    commands: mpsc::Sender<SessionCommand>,
    /// Submission counter for unique submission ids.
    submissions: u64,
    /// Assembles wire events for every SSE subscriber.
    events: broadcast::Sender<Event>,
    approvals: Arc<safety_gate::ApprovalGate>,
    questions: Arc<safety_gate::QuestionGate>,
    interrupt: infrastructure_base::InterruptHandle,
    /// Milliseconds from the server start to the last client-driven
    /// activity (submit, decision, answer, cancel, subscribe, or a
    /// forwarded turn event). The reaper compares this with the TTL.
    last_active_millis: Arc<AtomicU64>,
}

impl AppSession {
    /// Record client-driven activity for the idle clock.
    fn touch(&self, started: Instant) {
        self.last_active_millis
            .store(elapsed_millis(started), Ordering::Relaxed);
    }

    /// How long since the last client-driven activity.
    fn idle_for(&self, started: Instant) -> Duration {
        Duration::from_millis(
            elapsed_millis(started).saturating_sub(self.last_active_millis.load(Ordering::Relaxed)),
        )
    }
}

type SharedSessions = Arc<tokio::sync::Mutex<HashMap<String, AppSession>>>;

/// The injection seam: builds one parking-enabled session. Sessions are
/// held behind [`SessionSurface`] so the server never names the concrete
/// handle; production passes the composition root's `assemble_session`.
type Assemble =
    Arc<dyn Fn(AssembleOptions) -> Result<Box<dyn SessionSurface>, SessionError> + Send + Sync>;

/// Shared server state behind the router.
#[derive(Clone)]
struct AppState {
    sessions: SharedSessions,
    token: String,
    base: ServeOptions,
    next_session: Arc<tokio::sync::Mutex<u64>>,
    shutdown: Arc<Notify>,
    assemble: Assemble,
    /// Monotonic server start the idle clock measures from.
    started: Instant,
    /// Effective idle TTL and session cap (option defaults resolved).
    idle_ttl: Duration,
    max_sessions: usize,
}

/// The server surface handed back to the caller (CLI or tests).
pub struct ServerHandle {
    /// The bound loopback port (useful when 0 was requested).
    pub port: u16,
    shutdown: Arc<Notify>,
}

impl ServerHandle {
    /// Trigger the graceful shutdown; `join` then reaps the server.
    pub fn shutdown(&self) {
        self.shutdown.notify_waiters();
    }
}

/// Bind and run the server on a background task.
///
/// `assemble` is the composition seam: production passes the composition
/// root's `assemble_session`; tests pass a scripted-model assembly.
/// Sessions assemble with parking enabled so approvals and questions
/// wait on the gates until the HTTP endpoints answer.
///
/// Fails with `InvalidInput` when `options.token` is empty: an empty
/// token would make the guard accept a bare `Bearer ` header, so the
/// server refuses to start without a real shared secret.
pub async fn serve<F, S>(options: ServeOptions, assemble: F) -> std::io::Result<ServerHandle>
where
    F: Fn(AssembleOptions) -> Result<S, SessionError> + Send + Sync + Clone + 'static,
    S: SessionSurface + 'static,
{
    if options.token.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "ServeOptions.token must not be empty: an empty token would authenticate any caller",
        ));
    }
    // Box at the seam: the router and handlers stay non-generic.
    let assemble: Assemble =
        Arc::new(move |options| assemble(options).map(|session| Box::new(session) as _));
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], options.port));
    let listener = tokio::net::TcpListener::bind(addr).await?;
    let port = listener.local_addr()?.port();

    let shutdown = Arc::new(Notify::new());
    let started = Instant::now();
    let idle_ttl = options.session_idle_ttl.unwrap_or(SESSION_IDLE_TTL);
    let max_sessions = options.max_sessions.unwrap_or(MAX_SESSIONS);
    let state = AppState {
        sessions: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
        token: options.token.clone(),
        next_session: Arc::new(tokio::sync::Mutex::new(1)),
        shutdown: shutdown.clone(),
        assemble,
        base: options,
        started,
        idle_ttl,
        max_sessions,
    };
    spawn_idle_reaper(state.clone(), shutdown.clone());
    let server_shutdown = shutdown.clone();

    let app = router(state);
    let server = axum::serve(listener, app).with_graceful_shutdown(async move {
        server_shutdown.notified().await;
    });
    tokio::spawn(async move {
        if let Err(e) = server.await {
            eprintln!("[serve] http server failed: {e}");
        }
    });
    Ok(ServerHandle { port, shutdown })
}

/// Spawn the background reaper: every `idle_ttl / 4` (clamped) it removes
/// sessions idle longer than the TTL. Removal drops the entry's command
/// sender, so the pump shuts the actor down — the same path an explicit
/// `DELETE /sessions/{id}` takes. The task exits on server shutdown; a
/// notification missed between iterations only costs one extra tick, and
/// the detached task dies with the runtime either way.
fn spawn_idle_reaper(state: AppState, shutdown: Arc<Notify>) {
    let reap_interval =
        (state.idle_ttl / 4).clamp(Duration::from_millis(50), Duration::from_secs(60));
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(reap_interval);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = interval.tick() => {}
                _ = shutdown.notified() => return,
            }
            let reaped = {
                let mut sessions = state.sessions.lock().await;
                take_idle(&mut sessions, state.idle_ttl, state.started)
            };
            for id in reaped {
                eprintln!("[serve] reaped session {id} idle past {:?}", state.idle_ttl);
            }
        }
    });
}

/// Remove and return the ids of sessions idle longer than `ttl`.
fn take_idle(
    sessions: &mut HashMap<String, AppSession>,
    ttl: Duration,
    started: Instant,
) -> Vec<String> {
    let stale: Vec<String> = sessions
        .iter()
        .filter(|(_, session)| session.idle_for(started) > ttl)
        .map(|(id, _)| id.clone())
        .collect();
    for id in &stale {
        sessions.remove(id);
    }
    stale
}

fn router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/sessions", post(create_session).get(list_sessions))
        .route("/sessions/{session_id}/prompt", post(prompt_session))
        .route("/sessions/{session_id}/events", get(stream_events))
        .route(
            "/sessions/{session_id}/approvals/{call_id}",
            post(answer_approval),
        )
        .route(
            "/sessions/{session_id}/questions/{call_id}",
            post(answer_question),
        )
        .route("/sessions/{session_id}/cancel", post(cancel_session))
        .route("/sessions/{session_id}", delete(drop_session))
        .route("/shutdown", post(shutdown))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            auth_and_host_guard,
        ))
        .with_state(state)
}

/// Auth + Host middleware: bearer token on every route except
/// `/healthz`, and the Host header must name the loopback host.
async fn auth_and_host_guard(
    State(state): State<AppState>,
    headers: HeaderMap,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    if request.uri().path() != "/healthz" {
        let expected = format!("Bearer {}", state.token);
        let authorized = headers
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|value| constant_time_eq(value, &expected));
        if !authorized {
            return (StatusCode::UNAUTHORIZED, "missing or invalid bearer token").into_response();
        }
    }
    if !host_allowed(
        headers
            .get(axum::http::header::HOST)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default(),
    ) {
        return (StatusCode::FORBIDDEN, "loopback hosts only").into_response();
    }
    next.run(request).await
}

/// Constant-time equality for the bearer comparison.
///
/// Defense in depth: the token is a per-run 122-bit UUID served on the
/// loopback interface with a Host-header guard, so a timing side channel
/// has no realistic attacker path — but string `==` short-circuits on the
/// first differing byte, and a comparison that does not is free. Only the
/// content compare is constant time: differing lengths return early, and
/// the `Bearer ` prefix plus the token's fixed shape make the length
/// public anyway.
fn constant_time_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |diff, (x, y)| diff | (x ^ y)) == 0
}

/// True when the Host header names the loopback interface (with any
/// port). Everything else is a rebinding attempt.
fn host_allowed(host: &str) -> bool {
    let hostname = host.rsplit_once(':').map_or(host, |(h, _)| h);
    matches!(hostname, "127.0.0.1" | "localhost" | "[::1]")
}

/// Parse an approval decision body: `{"decision":"allow"|"always"|"deny",
/// "reason"?: string}`.
fn parse_decision(value: &serde_json::Value) -> Option<safety_gate::ApprovalDecision> {
    match value.get("decision")?.as_str()? {
        "allow" => Some(safety_gate::ApprovalDecision::AllowOnce),
        "always" => Some(safety_gate::ApprovalDecision::AllowAlways),
        "deny" => Some(safety_gate::ApprovalDecision::Deny {
            reason: value
                .get("reason")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_string(),
        }),
        _ => None,
    }
}

async fn create_session(
    State(state): State<AppState>,
    Json(body): Json<serde_json::Value>,
) -> axum::response::Response {
    // Capacity first: a refused creation must not pay for an assembly.
    if state.sessions.lock().await.len() >= state.max_sessions {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({"error": format!(
                "session limit reached ({}, idle sessions are reaped); delete one first",
                state.max_sessions
            )})),
        )
            .into_response();
    }
    let cwd = body
        .get("cwd")
        .and_then(serde_json::Value::as_str)
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| state.base.cwd.clone());
    // Mint the id before assembly: the compaction footers point at the
    // journal this id names.
    let session_id = {
        let mut next = state.next_session.lock().await;
        let id = format!("sess-{}", *next);
        *next += 1;
        id
    };
    let handle = match (state.assemble)(AssembleOptions {
        session_id: Some(session_id.clone()),
        config_path: state.base.config_path.clone(),
        model_override: state.base.model_override.clone(),
        provider_override: None,
        permission_override: None,
        thinking_override: None,
        cwd,
        home: state.base.home.clone(),
        identity: DEFAULT_IDENTITY.to_string(),
        headless: false,
        initial_history: Vec::new(),
        // `None` loads the user's denylist store, so configured deny
        // rules hold on served sessions too.
        wave_denylist: None,
    }) {
        Ok(handle) => handle,
        Err(e) => {
            return (
                StatusCode::BAD_GATEWAY,
                Json(serde_json::json!({"error": format!("session assembly failed: {e}")})),
            )
                .into_response();
        }
    };

    let permission_mode = handle.permission_mode().to_string();
    // The gates are shared out before the pump takes the session: the
    // pump owns it exclusively from there on.
    let approvals = handle.approvals();
    let questions = handle.questions();
    let interrupt = handle.interrupt();
    let (events, _) = broadcast::channel(256);
    let (commands, command_rx) = mpsc::channel(32);
    let last_active_millis = Arc::new(AtomicU64::new(elapsed_millis(state.started)));
    pump(
        handle,
        events.clone(),
        command_rx,
        last_active_millis.clone(),
        state.started,
    );

    let entry = AppSession {
        commands,
        submissions: 0,
        events,
        approvals,
        questions,
        interrupt,
        last_active_millis,
    };
    let mut sessions = state.sessions.lock().await;
    // The pre-assembly check can be raced past by concurrent creations:
    // the insert re-checks under the lock. Losing the race drops `entry`,
    // which closes the command channel and makes the pump shut the freshly
    // assembled actor down — the DELETE path again.
    if sessions.len() >= state.max_sessions {
        drop(sessions);
        drop(entry);
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({"error": format!(
                "session limit reached ({}, idle sessions are reaped); delete one first",
                state.max_sessions
            )})),
        )
            .into_response();
    }
    sessions.insert(session_id.clone(), entry);
    (
        StatusCode::CREATED,
        Json(serde_json::json!({
            "session_id": session_id,
            "permission_mode": permission_mode,
        })),
    )
        .into_response()
}

async fn list_sessions(State(state): State<AppState>) -> axum::response::Response {
    let sessions = state.sessions.lock().await;
    let ids: Vec<&String> = sessions.keys().collect();
    Json(serde_json::json!({ "sessions": ids })).into_response()
}

async fn prompt_session(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    Json(body): Json<serde_json::Value>,
) -> axum::response::Response {
    let Some(text) = body.get("text").and_then(serde_json::Value::as_str) else {
        return (StatusCode::BAD_REQUEST, "`text` is required").into_response();
    };
    if text.is_empty() {
        return (StatusCode::BAD_REQUEST, "`text` must not be empty").into_response();
    }
    // Images parse strictly: a malformed entry answers 400 instead of
    // being silently dropped — a skipped image would otherwise leave the
    // client with a 202 and a model that never saw the attachment, with
    // nothing naming the loss. Unknown extra fields still pass (the wire
    // `UserImage` shape stays additively evolvable).
    let mut images = Vec::new();
    if let Some(items) = body.get("images").and_then(serde_json::Value::as_array) {
        for item in items {
            match serde_json::from_value::<wavecode_wire::UserImage>(item.clone()) {
                Ok(image) => images.push(image),
                Err(_) => {
                    return (
                        StatusCode::BAD_REQUEST,
                        "`images` entries must carry string `mime` and `base64` fields",
                    )
                        .into_response();
                }
            }
        }
    }
    submit(
        state,
        &session_id,
        Op::UserInput {
            text: text.to_string(),
            images,
        },
    )
    .await
}

async fn answer_approval(
    State(state): State<AppState>,
    Path((session_id, call_id)): Path<(String, String)>,
    Json(body): Json<serde_json::Value>,
) -> axum::response::Response {
    let Some(decision) = parse_decision(&body) else {
        return (
            StatusCode::BAD_REQUEST,
            "`decision` must be allow, always, or deny (with optional reason)".to_string(),
        )
            .into_response();
    };
    let sessions = state.sessions.lock().await;
    let Some(session) = sessions.get(&session_id) else {
        return (StatusCode::NOT_FOUND, "unknown session").into_response();
    };
    session.touch(state.started);
    if session.approvals.decide(&call_id, decision) {
        StatusCode::OK.into_response()
    } else {
        (StatusCode::NOT_FOUND, "no parked approval for that call id").into_response()
    }
}

async fn answer_question(
    State(state): State<AppState>,
    Path((session_id, call_id)): Path<(String, String)>,
    Json(body): Json<serde_json::Value>,
) -> axum::response::Response {
    let Some(answer) = body.get("answer").and_then(serde_json::Value::as_str) else {
        return (StatusCode::BAD_REQUEST, "`answer` is required").into_response();
    };
    let sessions = state.sessions.lock().await;
    let Some(session) = sessions.get(&session_id) else {
        return (StatusCode::NOT_FOUND, "unknown session").into_response();
    };
    session.touch(state.started);
    if session.questions.answer(&call_id, answer.to_string()) {
        StatusCode::OK.into_response()
    } else {
        (StatusCode::NOT_FOUND, "no parked question for that call id").into_response()
    }
}

async fn cancel_session(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
) -> axum::response::Response {
    let sessions = state.sessions.lock().await;
    let Some(session) = sessions.get(&session_id) else {
        return (StatusCode::NOT_FOUND, "unknown session").into_response();
    };
    session.touch(state.started);
    session.interrupt.trigger();
    StatusCode::OK.into_response()
}

async fn drop_session(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
) -> axum::response::Response {
    let removed = state.sessions.lock().await.remove(&session_id);
    if removed.is_some() {
        // Dropping the entry drops the pump's command sender and the
        // broadcast sender; the pump exits when the actor goes quiet.
        StatusCode::OK.into_response()
    } else {
        (StatusCode::NOT_FOUND, "unknown session").into_response()
    }
}

async fn shutdown(State(state): State<AppState>) -> axum::response::Response {
    state.shutdown.notify_waiters();
    StatusCode::OK.into_response()
}

/// SSE stream of one session's wire events.
///
/// Subscription lifecycle (the documented contract for SSE clients): the
/// stream carries the session's wire events from the subscribe point on
/// and **ends after the first `TurnCompleted` event** — one stream is one
/// turn. Clients re-subscribe for the next turn (events raised between
/// two subscriptions are not replayed; subscribe before prompting to
/// miss nothing, exactly like the tests below). A subscriber that falls
/// behind the broadcast buffer receives a synthetic `{"type": "warning",
/// "message": "..."}` frame naming the skipped count instead of the lost
/// events. Payloads are serialized `wavecode_wire::Event` JSON: the
/// snake_case `type` tags are locked by the wire crate's tag tests, and
/// field evolution there is additive (new fields are optional and
/// omitted when unset), so consumers may ignore unknown fields.
async fn stream_events(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
) -> axum::response::Response {
    let sessions = state.sessions.lock().await;
    let Some(session) = sessions.get(&session_id) else {
        return (StatusCode::NOT_FOUND, "unknown session").into_response();
    };
    // Subscribing is client liveness: the per-turn stream is opened right
    // before a prompt, so it must not race the reaper.
    session.touch(state.started);
    let mut receiver = session.events.subscribe();
    drop(sessions);
    let stream = async_stream::stream! {
        loop {
            match receiver.recv().await {
                Ok(event) => {
                    let payload = serde_json::to_string(&event).unwrap_or_default();
                    yield Ok(SseEvent::default().data(payload));
                    if matches!(event.msg, EventMsg::TurnCompleted { .. }) {
                        break;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(skipped)) => {
                    let payload = serde_json::json!({
                        "type": "warning",
                        "message": format!("SSE subscriber lagged; {skipped} events skipped"),
                    }).to_string();
                    yield Ok(SseEvent::default().data(payload));
                }
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    };
    // Box + pin: the generator borrows nothing but must outlive the
    // response, and `Sse` needs one concrete stream type.
    let stream: std::pin::Pin<
        Box<dyn futures::Stream<Item = Result<SseEvent, std::convert::Infallible>> + Send>,
    > = Box::pin(stream);
    Sse::new(stream)
        .keep_alive(KeepAlive::default())
        .into_response()
}

// ---- internals ----

/// Spawn a session's pump task: exclusive session owner, forwarding
/// events to subscribers and executing submission commands. Forwarded
/// events count as activity: a turn that is alive keeps producing them,
/// so the reaper can never take a running session.
fn pump(
    mut session: Box<dyn SessionSurface>,
    events: broadcast::Sender<Event>,
    mut commands: mpsc::Receiver<SessionCommand>,
    activity: Arc<AtomicU64>,
    started: Instant,
) {
    tokio::spawn(async move {
        loop {
            tokio::select! {
                event = session.next_event() => match event {
                    Some(event) => {
                        activity.store(elapsed_millis(started), Ordering::Relaxed);
                        let _ = events.send(event);
                    }
                    None => break,
                },
                command = commands.recv() => match command {
                    Some(SessionCommand::Submit { submission_id, op, reply }) => {
                        let result = session
                            .submit(Submission { id: submission_id, op })
                            .await
                            .map_err(|e| e.to_string());
                        let _ = reply.send(result);
                    }
                    None => {
                        // All handles dropped (session deleted): shut the
                        // actor down cleanly and end the pump.
                        let _ = session
                            .submit(Submission {
                                id: "server-shutdown".to_string(),
                                op: Op::Shutdown,
                            })
                            .await;
                        break;
                    }
                },
            }
        }
    });
}

/// Queue one op through the session's pump and wait for acceptance.
async fn submit(state: AppState, session_id: &str, op: Op) -> axum::response::Response {
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    // One lock hold covers touch, numbering, and enqueue (`try_send` is
    // synchronous); the lock drops before the only await, the pump reply.
    let send_result = {
        let mut sessions = state.sessions.lock().await;
        let Some(session) = sessions.get_mut(session_id) else {
            return (StatusCode::NOT_FOUND, "unknown session").into_response();
        };
        session.touch(state.started);
        session.submissions += 1;
        let submission_id = format!("srv-{session_id}-{}", session.submissions);
        session.commands.try_send(SessionCommand::Submit {
            submission_id,
            op,
            reply: reply_tx,
        })
    };
    match send_result {
        // Backpressure is not shutdown: a full queue means the prompt can
        // be retried once the pump drains, so answer 429 (not 409) and
        // keep the submission rejection visible instead of dropping it.
        Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
            let mut response = (
                StatusCode::TOO_MANY_REQUESTS,
                "session queue is full; retry shortly",
            )
                .into_response();
            response
                .headers_mut()
                .insert("Retry-After", axum::http::HeaderValue::from_static("1"));
            response
        }
        // The pump is gone: the session is closing.
        Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
            (StatusCode::CONFLICT, "session is closing").into_response()
        }
        Ok(()) => match reply_rx.await {
            Ok(Ok(())) => (
                StatusCode::ACCEPTED,
                Json(serde_json::json!({"queued": true})),
            )
                .into_response(),
            Ok(Err(e)) => (
                StatusCode::BAD_GATEWAY,
                Json(serde_json::json!({"error": e})),
            )
                .into_response(),
            Err(_) => (StatusCode::NOT_FOUND, "session closed").into_response(),
        },
    }
}

#[cfg(test)]
#[path = "app_server_tests.rs"]
mod tests;
