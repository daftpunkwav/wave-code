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

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use operations_actor::{AssembleOptions, DEFAULT_IDENTITY, SessionError, SessionSurface};
use tokio::sync::{Notify, broadcast, mpsc};
use wavecode_wire::{Event, EventMsg, Op, Submission};

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
    /// Bearer token every authenticated route requires.
    pub token: String,
    /// Bind port; 0 picks an ephemeral port (the bound port is returned).
    pub port: u16,
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
pub async fn serve<F, S>(options: ServeOptions, assemble: F) -> std::io::Result<ServerHandle>
where
    F: Fn(AssembleOptions) -> Result<S, SessionError> + Send + Sync + Clone + 'static,
    S: SessionSurface + 'static,
{
    // Box at the seam: the router and handlers stay non-generic.
    let assemble: Assemble =
        Arc::new(move |options| assemble(options).map(|session| Box::new(session) as _));
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], options.port));
    let listener = tokio::net::TcpListener::bind(addr).await?;
    let port = listener.local_addr()?.port();

    let shutdown = Arc::new(Notify::new());
    let state = AppState {
        sessions: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
        token: options.token.clone(),
        next_session: Arc::new(tokio::sync::Mutex::new(1)),
        shutdown: shutdown.clone(),
        assemble,
        base: options,
    };
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
            .is_some_and(|value| value == expected);
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
        wave_denylist: Vec::new(),
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
    pump(handle, events.clone(), command_rx);

    state.sessions.lock().await.insert(
        session_id.clone(),
        AppSession {
            commands,
            submissions: 0,
            events,
            approvals,
            questions,
            interrupt,
        },
    );
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
    let images: Vec<wavecode_wire::UserImage> = body
        .get("images")
        .and_then(serde_json::Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| serde_json::from_value(item.clone()).ok())
                .collect()
        })
        .unwrap_or_default();
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
async fn stream_events(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
) -> axum::response::Response {
    let sessions = state.sessions.lock().await;
    let Some(session) = sessions.get(&session_id) else {
        return (StatusCode::NOT_FOUND, "unknown session").into_response();
    };
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
/// events to subscribers and executing submission commands.
fn pump(
    mut session: Box<dyn SessionSurface>,
    events: broadcast::Sender<Event>,
    mut commands: mpsc::Receiver<SessionCommand>,
) {
    tokio::spawn(async move {
        loop {
            tokio::select! {
                event = session.next_event() => match event {
                    Some(event) => {
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
    let submission_id = {
        let mut sessions = state.sessions.lock().await;
        let Some(session) = sessions.get_mut(session_id) else {
            return (StatusCode::NOT_FOUND, "unknown session").into_response();
        };
        session.submissions += 1;
        format!("srv-{session_id}-{}", session.submissions)
    };
    // Send without holding the lock across await; the pump replies.
    let sessions = state.sessions.lock().await;
    let send_result = sessions.get(session_id).map(|session| {
        session.commands.try_send(SessionCommand::Submit {
            submission_id,
            op,
            reply: reply_tx,
        })
    });
    drop(sessions);
    let Some(send_result) = send_result else {
        return (StatusCode::NOT_FOUND, "unknown session").into_response();
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
mod tests {
    use super::*;
    use crate::test_stubs::{CONFIG, OneShotModel, done_script};
    use std::collections::VecDeque;

    /// Assembly seam for tests: config from a tempdir, scripted model.
    #[derive(Clone)]
    struct TestAssemble {
        scripts: Arc<std::sync::Mutex<VecDeque<Vec<wavecode_llm::StreamEvent>>>>,
        fallback_config: std::path::PathBuf,
    }

    impl TestAssemble {
        fn assemble(
            &self,
            options: AssembleOptions,
        ) -> Result<operations_bootstrap::SessionHandle, SessionError> {
            let path = options
                .config_path
                .clone()
                .unwrap_or_else(|| self.fallback_config.clone());
            let config = wavecode_config::Config::load_from(&path).map_err(SessionError::Config)?;
            let model_name = options
                .model_override
                .clone()
                .unwrap_or_else(|| config.model.clone());
            let script = self
                .scripts
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .pop_front()
                .unwrap_or_else(done_script);
            let model: Arc<dyn wavecode_llm::ChatModel> = Arc::new(OneShotModel::new(script));
            Ok(operations_bootstrap::session::assemble_session_with_model(
                operations_bootstrap::session::WithModel {
                    config,
                    model,
                    model_name,
                    provider_id: "test".to_string(),
                    thinking_effort: None,
                    deny_env: Vec::new(),
                    context_window: 200_000,
                    max_output_tokens: 64,
                    per_model_window: None,
                    headless: options.headless,
                    session_id: options.session_id,
                    permission_override: options.permission_override,
                    cwd: options.cwd,
                    home: options.home,
                    identity: options.identity.clone(),
                    initial_history: options.initial_history,
                    wave_denylist: options.wave_denylist,
                    warnings: Vec::new(),
                },
            ))
        }
    }

    /// Start a server on an ephemeral port with the given scripts.
    async fn start(
        scripts: Vec<Vec<wavecode_llm::StreamEvent>>,
    ) -> (String, u16, tokio::sync::oneshot::Sender<()>) {
        let tmp = tempfile::tempdir().unwrap();
        let config = tmp.path().join("config.toml");
        std::fs::write(&config, CONFIG).unwrap();
        let assemble = TestAssemble {
            scripts: Arc::new(std::sync::Mutex::new(scripts.into_iter().collect())),
            fallback_config: config,
        };
        let token = "test-token".to_string();
        let handle = serve(
            ServeOptions {
                config_path: None,
                model_override: None,
                cwd: tmp.path().to_path_buf(),
                home: None,
                token: token.clone(),
                port: 0,
            },
            move |options| assemble.assemble(options),
        )
        .await
        .unwrap();
        let port = handle.port;
        // Keep the tempdir alive until the caller drops its sender: the
        // task holds the dir and waits for the receiver to go away.
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        tokio::spawn(async move {
            let _tmp = tmp;
            let _ = rx.await;
        });
        (token, port, tx)
    }

    fn client(token: &str) -> reqwest::Client {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            reqwest::header::AUTHORIZATION,
            reqwest::header::HeaderValue::from_str(&format!("Bearer {token}")).unwrap(),
        );
        reqwest::Client::builder()
            .default_headers(headers)
            .build()
            .unwrap()
    }

    /// Open the events endpoint and await its headers; call before
    /// prompting so the subscription cannot miss early events.
    async fn open_events(
        client: &reqwest::Client,
        port: u16,
        session_id: &str,
    ) -> reqwest::Response {
        let response = client
            .get(format!(
                "http://127.0.0.1:{port}/sessions/{session_id}/events"
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        response
    }

    /// Collect `n` SSE data payloads from an open events response.
    async fn collect_events(response: reqwest::Response, n: usize) -> Vec<serde_json::Value> {
        use futures::StreamExt as _;
        let mut stream = response.bytes_stream();
        let mut buffer = String::new();
        let mut out = Vec::new();
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
        while out.len() < n {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                panic!("timed out collecting SSE events");
            }
            let chunk = match tokio::time::timeout(remaining, stream.next()).await {
                Ok(Some(Ok(bytes))) => bytes,
                // The stream ends at turn completion (or session close):
                // return whatever arrived.
                _ => break,
            };
            buffer.push_str(&String::from_utf8_lossy(&chunk));
            while let Some(pos) = buffer.find("\n\n") {
                let frame = buffer.drain(..pos + 2).collect::<String>();
                for line in frame.lines() {
                    if let Some(data) = line.strip_prefix("data: ") {
                        out.push(serde_json::from_str(data).unwrap());
                    }
                }
            }
        }
        out
    }

    #[tokio::test]
    async fn healthz_is_open_and_other_routes_require_the_token() {
        let (token, port, done) = start(vec![]).await;
        let anonymous = reqwest::Client::new();
        assert_eq!(
            anonymous
                .get(format!("http://127.0.0.1:{port}/healthz"))
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::OK
        );
        assert_eq!(
            anonymous
                .get(format!("http://127.0.0.1:{port}/sessions"))
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::UNAUTHORIZED
        );
        let authorized = client(&token);
        assert_eq!(
            authorized
                .get(format!("http://127.0.0.1:{port}/sessions"))
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::OK
        );
        let _ = done;
    }

    #[tokio::test]
    async fn non_loopback_host_headers_are_refused() {
        let (token, port, done) = start(vec![]).await;
        let http = client(&token);
        let rebound = http
            .get(format!("http://127.0.0.1:{port}/sessions"))
            .header("Host", "evil.example.com")
            .send()
            .await
            .unwrap();
        assert_eq!(rebound.status(), StatusCode::FORBIDDEN);
        let _ = done;
    }

    #[tokio::test]
    async fn session_lifecycle_streams_prompt_events_over_sse() {
        let script = vec![
            wavecode_llm::StreamEvent::TextDelta {
                text: "hello".to_string(),
            },
            wavecode_llm::StreamEvent::MessageComplete {
                stop_reason: "end_turn".to_string(),
                usage: wavecode_llm::Usage::default(),
            },
        ];
        let (token, port, done) = start(vec![script]).await;
        let http = client(&token);

        let created: serde_json::Value = http
            .post(format!("http://127.0.0.1:{port}/sessions"))
            .json(&serde_json::json!({}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        println!("created: {created}");
        let session_id = created["session_id"]
            .as_str()
            .unwrap_or_else(|| panic!("no session_id in {created}"))
            .to_string();
        assert_eq!(created["permission_mode"], "auto");

        // Subscribe BEFORE prompting so no event is missed: headers
        // must be back before the prompt can race the subscription.
        let response = open_events(&http, port, &session_id).await;
        let events_task = tokio::spawn(collect_events(response, 8));
        let prompt_status = http
            .post(format!(
                "http://127.0.0.1:{port}/sessions/{session_id}/prompt"
            ))
            .json(&serde_json::json!({"text": "say hello"}))
            .send()
            .await
            .unwrap();
        assert_eq!(prompt_status.status(), StatusCode::ACCEPTED);

        let events = events_task.await.unwrap();
        let types: Vec<&str> = events.iter().filter_map(|e| e["type"].as_str()).collect();
        assert_eq!(types.first(), Some(&"turn_started"), "events: {types:?}");
        assert!(
            types.contains(&"agent_message_delta") && types.contains(&"turn_completed"),
            "events: {types:?}"
        );
        assert_eq!(events[1]["text"], "hello");
        let _ = done;
    }

    #[tokio::test]
    async fn approval_requests_answer_over_http() {
        use wavecode_llm::StreamEvent;
        let script = vec![
            StreamEvent::TextDelta {
                text: "working".to_string(),
            },
            StreamEvent::ToolUseBegin {
                id: "c1".to_string(),
                name: "shell".to_string(),
            },
            StreamEvent::ToolUseInputDelta {
                partial_json: r#"{"command":"echo hi"}"#.to_string(),
            },
            StreamEvent::BlockEnd,
            StreamEvent::MessageComplete {
                stop_reason: "tool_use".to_string(),
                usage: wavecode_llm::Usage::default(),
            },
        ];
        let (token, port, done) = start(vec![script]).await;
        let http = client(&token);

        let created: serde_json::Value = http
            .post(format!("http://127.0.0.1:{port}/sessions"))
            .json(&serde_json::json!({}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let session_id = created["session_id"].as_str().unwrap().to_string();

        let response = open_events(&http, port, &session_id).await;
        // Exactly the five pre-park events: turn_started, the text delta,
        // its completion, tool_call_begin, approval_requested. The stream
        // stays open while the turn parks, so an over-large count here
        // would stall on the deadline instead of returning.
        let events_task = tokio::spawn(collect_events(response, 5));
        let status = http
            .post(format!(
                "http://127.0.0.1:{port}/sessions/{session_id}/prompt"
            ))
            .json(&serde_json::json!({"text": "run it"}))
            .send()
            .await
            .unwrap();
        assert_eq!(status.status(), StatusCode::ACCEPTED);

        let events = events_task.await.unwrap();
        // tool_call_begin, then the approval parks the turn.
        let approval = events
            .iter()
            .find(|e| e["type"] == "approval_requested")
            .expect("shell call must ask in auto mode");
        let call_id = approval["call_id"].as_str().unwrap().to_string();

        // Answer over HTTP; the parked tool resumes.
        let answered = http
            .post(format!(
                "http://127.0.0.1:{port}/sessions/{session_id}/approvals/{call_id}"
            ))
            .json(&serde_json::json!({"decision": "allow"}))
            .send()
            .await
            .unwrap();
        assert_eq!(answered.status(), StatusCode::OK);

        let follow = open_events(&http, port, &session_id).await;
        let rest = collect_events(follow, 1).await;
        let types: Vec<&str> = rest.iter().filter_map(|e| e["type"].as_str()).collect();
        assert!(
            types.contains(&"tool_call_end") || types.contains(&"turn_completed"),
            "expected the run to continue after the approval, got {types:?}"
        );
        let _ = done;
    }
    /// A full submission queue answers 429 with a Retry-After hint —
    /// the prompt is retryable once the pump drains. Only a dead pump
    /// (409) means the session is closing.
    #[tokio::test]
    async fn full_submission_queue_answers_429_with_retry_after() {
        let (commands, _pending) = tokio::sync::mpsc::channel(32);
        let (events, _) = tokio::sync::broadcast::channel(8);
        let session = AppSession {
            commands,
            submissions: 0,
            events,
            approvals: Arc::new(safety_gate::ApprovalGate::new()),
            questions: Arc::new(safety_gate::QuestionGate::new()),
            interrupt: infrastructure_base::InterruptHandle::new(),
        };
        let state = AppState {
            sessions: Arc::new(tokio::sync::Mutex::new(HashMap::from([(
                "s-full".to_string(),
                session,
            )]))),
            token: String::new(),
            base: ServeOptions {
                config_path: None,
                model_override: None,
                cwd: std::env::temp_dir(),
                home: None,
                port: 0,
                token: String::new(),
            },
            next_session: Arc::new(tokio::sync::Mutex::new(1)),
            shutdown: Arc::new(tokio::sync::Notify::new()),
            assemble: Arc::new(|_| -> Result<Box<dyn SessionSurface>, SessionError> {
                unreachable!("no assembly in this test")
            }),
        };
        // Fill the queue with no pump consuming: the queued replies are
        // dropped, so those submissions would hang — the point is that
        // the thirty-third arrives at a channel already at capacity.
        for i in 0..32 {
            let (reply, _rx) = tokio::sync::oneshot::channel();
            state
                .sessions
                .lock()
                .await
                .get("s-full")
                .unwrap()
                .commands
                .try_send(SessionCommand::Submit {
                    submission_id: format!("fill-{i}"),
                    op: Op::Interrupt,
                    reply,
                })
                .expect("filler must fit");
        }
        let response = submit(
            state,
            "s-full",
            Op::UserInput {
                text: "one too many".to_string(),
                images: Vec::new(),
            },
        )
        .await;
        assert_eq!(
            response.status(),
            StatusCode::TOO_MANY_REQUESTS,
            "backpressure is retryable, not a closing session"
        );
        assert!(response.headers().get("Retry-After").is_some());
    }

    /// A dead pump (closed command channel) answers 409: unlike a full
    /// queue (429) the session is closing and the prompt is not retryable,
    /// so no Retry-After hint is offered.
    #[tokio::test]
    async fn closed_session_answers_409_without_retry_after() {
        let (commands, receiver) = tokio::sync::mpsc::channel(32);
        drop(receiver);
        let (events, _) = tokio::sync::broadcast::channel(8);
        let session = AppSession {
            commands,
            submissions: 0,
            events,
            approvals: Arc::new(safety_gate::ApprovalGate::new()),
            questions: Arc::new(safety_gate::QuestionGate::new()),
            interrupt: infrastructure_base::InterruptHandle::new(),
        };
        let state = AppState {
            sessions: Arc::new(tokio::sync::Mutex::new(HashMap::from([(
                "s-closed".to_string(),
                session,
            )]))),
            token: String::new(),
            base: ServeOptions {
                config_path: None,
                model_override: None,
                cwd: std::env::temp_dir(),
                home: None,
                port: 0,
                token: String::new(),
            },
            next_session: Arc::new(tokio::sync::Mutex::new(1)),
            shutdown: Arc::new(tokio::sync::Notify::new()),
            assemble: Arc::new(|_| -> Result<Box<dyn SessionSurface>, SessionError> {
                unreachable!("no assembly in this test")
            }),
        };
        let response = submit(
            state,
            "s-closed",
            Op::UserInput {
                text: "arrives at a closed pump".to_string(),
                images: Vec::new(),
            },
        )
        .await;
        assert_eq!(
            response.status(),
            StatusCode::CONFLICT,
            "a closing session must not read as retryable backpressure"
        );
        assert!(response.headers().get("Retry-After").is_none());
    }
}
