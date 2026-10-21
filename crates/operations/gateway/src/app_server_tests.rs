use super::*;
use crate::test_stubs::{CONFIG, OneShotModel, done_script};
use std::collections::VecDeque;

/// The bearer comparison: equal content matches, a difference at any
/// byte position (first or last) fails, and length mismatches fail.
#[test]
fn constant_time_eq_matches_whole_content_only() {
    assert!(constant_time_eq("Bearer t", "Bearer t"));
    assert!(constant_time_eq("", ""));
    assert!(!constant_time_eq("Bearer a", "Bearer b"));
    // Differing last byte must not pass from a shared prefix.
    assert!(!constant_time_eq("token-aa", "token-ab"));
    assert!(!constant_time_eq("Bearer t", "Bearer "));
    assert!(!constant_time_eq("Bearer t", "bearer t"));
}

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
        Ok(operations_bootstrap::session::assemble_session_after_model(
            operations_bootstrap::session::WithModel {
                config,
                model,
                model_name,
                provider_id: "test".to_string(),
                thinking_effort: None,
                deny_env: Vec::new(),
                context_window: 200_000,
                // nosemgrep: codacy.yaml.security.hard-coded-tokens
                max_output_tokens: 64,
                per_model_window: None,
                headless: options.headless,
                session_id: options.session_id,
                permission_override: options.permission_override,
                cwd: options.cwd,
                home: options.home,
                identity: options.identity.clone(),
                initial_history: options.initial_history,
                wave_denylist: options.wave_denylist.unwrap_or_default(),
                warnings: Vec::new(),
            },
        ))
    }
}

/// Start a server on an ephemeral port with the given scripts.
async fn start(
    scripts: Vec<Vec<wavecode_llm::StreamEvent>>,
) -> (String, u16, tokio::sync::oneshot::Sender<()>) {
    start_with(scripts, None, None).await
}

/// `start` with the reaper knobs injected: `session_idle_ttl` and
/// `max_sessions` (`None` = production defaults).
async fn start_with(
    scripts: Vec<Vec<wavecode_llm::StreamEvent>>,
    session_idle_ttl: Option<Duration>,
    max_sessions: Option<usize>,
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
            session_idle_ttl,
            max_sessions,
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

/// Loopback URL of the server under test. Every request in this file
/// is built from it: the app server binds the loopback interface only,
/// so no test traffic leaves the machine.
fn server_url(port: u16, path: &str) -> String {
    format!("http://127.0.0.1:{port}{path}")
}

/// Create one session and return its resource URL
/// (`http://127.0.0.1:<port>/sessions/<id>`); panics on anything but
/// `201`. Callers address the session through that URL instead of
/// carrying its id around.
async fn create_session_url(http: &reqwest::Client, port: u16) -> String {
    let response = http
        .post(server_url(port, "/sessions"))
        .json(&serde_json::json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    let created: serde_json::Value = response.json().await.unwrap();
    assert_eq!(created["permission_mode"], "auto");
    let path = format!(
        "/sessions/{}",
        created["session_id"]
            .as_str()
            .unwrap_or_else(|| panic!("no session_id in {created}"))
    );
    server_url(port, &path)
}

/// Open the events endpoint and await its headers; call before
/// prompting so the subscription cannot miss early events.
async fn open_events(http: &reqwest::Client, session_url: &str) -> reqwest::Response {
    let response = http
        .get(format!("{session_url}/events"))
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
            .get(server_url(port, "/healthz"))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
    assert_eq!(
        anonymous
            .get(server_url(port, "/sessions"))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED
    );
    let authorized = client(&token);
    assert_eq!(
        authorized
            .get(server_url(port, "/sessions"))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
    let _ = done;
}

/// Every frame on an SSE stream carries the wire `Event` shape — a
/// correlation `id` plus the payload — including the one frame the
/// server raises itself: a consumer that parses each `data:` payload as
/// a `wavecode_wire::Event` must not trip over the subscriber-lag
/// warning.
#[test]
fn lag_warning_frame_is_a_wire_event() {
    let frame = serde_json::to_value(lag_warning(3)).unwrap();
    let back: wavecode_wire::Event = serde_json::from_value(frame).unwrap();
    assert_eq!(back.id, SERVER_EVENT_ID);
    assert!(
        matches!(&back.msg, EventMsg::Warning { message } if message.contains("3 events skipped")),
        "{back:?}"
    );
}

/// The lag frame the *stream* sends is a wire event too.
///
/// The helper-level check above only proves the helper builds one; it
/// cannot fail when the stream stops sending it. The observable here is
/// the stream: a subscriber that falls behind the broadcast buffer
/// receives the warning frame, and every `data:` payload the client
/// reads — the server-raised one included — must parse as an `Event`.
#[tokio::test]
async fn the_lag_frame_on_the_stream_parses_as_a_wire_event() {
    let (commands, _pending) = tokio::sync::mpsc::channel(32);
    let state = state_with_session("s-lag", commands);
    // A tiny broadcast buffer: the events published below overflow it, so
    // the subscriber falls behind and the stream raises the lag frame.
    let (events, _receiver) = tokio::sync::broadcast::channel(2);
    let sender = events.clone();
    state
        .sessions
        .lock()
        .await
        .get_mut("s-lag")
        .expect("the session exists")
        .events = events;
    // The handler subscribes before it returns, so the events published
    // below land in the subscriber's buffer.
    let response = stream_events(State(state), Path("s-lag".to_string())).await;
    assert_eq!(response.status(), StatusCode::OK);
    for i in 0..4 {
        sender
            .send(Event {
                id: format!("e{i}"),
                msg: EventMsg::Warning {
                    message: "filler".to_string(),
                },
            })
            .expect("the broadcast accepts the event");
    }
    // The turn-completed event ends the stream, so the body can be
    // collected without waiting for the session to close.
    sender
        .send(Event {
            id: "e-done".to_string(),
            msg: EventMsg::TurnCompleted { interrupted: false },
        })
        .expect("the broadcast accepts the event");
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("the SSE body reads");
    let text = String::from_utf8_lossy(&body);
    let payloads: Vec<serde_json::Value> = text
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .map(|data| serde_json::from_str(data).expect("every frame is JSON"))
        .collect();
    assert!(!payloads.is_empty(), "the stream carried frames: {text}");
    for payload in &payloads {
        serde_json::from_value::<wavecode_wire::Event>(payload.clone())
            .expect("every frame on the stream is a wire event");
    }
    let server_frames: Vec<&serde_json::Value> = payloads
        .iter()
        .filter(|payload| payload["id"] == SERVER_EVENT_ID)
        .collect();
    assert_eq!(
        server_frames.len(),
        1,
        "the stream carries exactly one server-raised frame: {payloads:?}"
    );
    assert!(
        server_frames[0]["type"] == "warning",
        "the server-raised frame is the lag warning: {server_frames:?}"
    );
    assert!(
        server_frames[0]["message"]
            .as_str()
            .is_some_and(|message| message.contains("events skipped")),
        "the lag warning names the skipped count: {server_frames:?}"
    );
}

/// An empty bearer token would make the guard accept a bare
/// `Bearer ` header: the server refuses to start instead.
#[tokio::test]
async fn serve_rejects_an_empty_token() {
    let error = serve(
        ServeOptions {
            config_path: None,
            model_override: None,
            // nosemgrep: rust.lang.security.temp-dir.temp-dir
            cwd: std::env::temp_dir(),
            home: None,
            token: String::new(),
            port: 0,
            session_idle_ttl: None,
            max_sessions: None,
        },
        |_options| -> Result<operations_bootstrap::SessionHandle, SessionError> {
            unreachable!("no assembly before the token check")
        },
    )
    .await
    .err()
    .expect("empty token must fail the bind");
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
}

#[tokio::test]
async fn non_loopback_host_headers_are_refused() {
    let (token, port, done) = start(vec![]).await;
    let http = client(&token);
    let rebound = http
        .get(server_url(port, "/sessions"))
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

    let session_url = create_session_url(&http, port).await;

    // Subscribe BEFORE prompting so no event is missed: headers
    // must be back before the prompt can race the subscription.
    let response = open_events(&http, &session_url).await;
    let events_task = tokio::spawn(collect_events(response, 8));
    let prompt_status = http
        .post(format!("{session_url}/prompt"))
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

/// Image entries parse strictly: a malformed entry answers 400
/// (naming the required shape) instead of silently vanishing behind a
/// 202, while a well-formed entry is accepted and unknown extra
/// fields keep passing.
#[tokio::test]
async fn malformed_prompt_images_answer_400() {
    let (token, port, done) = start(vec![]).await;
    let http = client(&token);
    let session_url = create_session_url(&http, port).await;
    let post = |body: serde_json::Value| {
        let http = &http;
        let url = format!("{session_url}/prompt");
        async move { http.post(url).json(&body).send().await.unwrap() }
    };
    let bad = post(serde_json::json!({
        "text": "look at this",
        "images": [{"mime": "image/png"}],
    }))
    .await;
    assert_eq!(bad.status(), StatusCode::BAD_REQUEST);
    let text = bad.text().await.unwrap();
    assert!(
        text.contains("mime") && text.contains("base64"),
        "the error must name the image shape: {text}"
    );
    // A well-formed entry (plus unknown extra fields) still queues.
    let good = post(serde_json::json!({
        "text": "look at this",
        "images": [{
            "mime": "image/png",
            "base64": "aGk=",
            "futureField": true,
        }],
    }))
    .await;
    assert_eq!(good.status(), StatusCode::ACCEPTED);
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

    let session_url = create_session_url(&http, port).await;

    let response = open_events(&http, &session_url).await;
    // Exactly the five pre-park events: turn_started, the text delta,
    // its completion, tool_call_begin, approval_requested. The stream
    // stays open while the turn parks, so an over-large count here
    // would stall on the deadline instead of returning.
    let events_task = tokio::spawn(collect_events(response, 5));
    let status = http
        .post(format!("{session_url}/prompt"))
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

    // Subscribe before the decision. The events channel does not replay,
    // and a fast tool can finish before a subscriber that joins afterwards.
    let follow = open_events(&http, &session_url).await;
    let rest_task = tokio::spawn(collect_events(follow, 1));

    // Answer over HTTP; the parked tool resumes.
    let answered = http
        .post(format!("{session_url}/approvals/{call_id}"))
        .json(&serde_json::json!({"decision": "allow"}))
        .send()
        .await
        .unwrap();
    assert_eq!(answered.status(), StatusCode::OK);

    let rest = rest_task.await.unwrap();
    let types: Vec<&str> = rest.iter().filter_map(|e| e["type"].as_str()).collect();
    assert!(
        types.contains(&"tool_call_end") || types.contains(&"turn_completed"),
        "expected the run to continue after the approval, got {types:?}"
    );
    let _ = done;
}
/// Minimal server state for the submit-path tests: one session under
/// `id` whose pump channel is `commands`. Assembly is unreachable and
/// the serve options are inert, so each new `AppState` field lands here
/// once instead of once per test.
fn state_with_session(id: &str, commands: mpsc::Sender<SessionCommand>) -> AppState {
    let (events, _) = tokio::sync::broadcast::channel(8);
    let session = AppSession {
        commands,
        submissions: 0,
        events,
        approvals: Arc::new(safety_gate::ApprovalGate::new()),
        questions: Arc::new(safety_gate::QuestionGate::new()),
        interrupt: infrastructure_base::InterruptHandle::new(),
        last_active_millis: Arc::new(AtomicU64::new(0)),
    };
    AppState {
        sessions: Arc::new(tokio::sync::Mutex::new(HashMap::from([(
            id.to_string(),
            session,
        )]))),
        token: String::new(),
        base: ServeOptions {
            config_path: None,
            model_override: None,
            // nosemgrep: rust.lang.security.temp-dir.temp-dir
            cwd: std::env::temp_dir(),
            home: None,
            port: 0,
            token: String::new(),
            session_idle_ttl: None,
            max_sessions: None,
        },
        next_session: Arc::new(tokio::sync::Mutex::new(1)),
        shutdown: Arc::new(tokio::sync::Notify::new()),
        assemble: Arc::new(|_| -> Result<Box<dyn SessionSurface>, SessionError> {
            unreachable!("no assembly in this test")
        }),
        started: Instant::now(),
        idle_ttl: SESSION_IDLE_TTL,
        max_sessions: MAX_SESSIONS,
    }
}

/// A full submission queue answers 429 with a Retry-After hint —
/// the prompt is retryable once the pump drains. Only a dead pump
/// (409) means the session is closing.
#[tokio::test]
async fn full_submission_queue_answers_429_with_retry_after() {
    let (commands, _pending) = tokio::sync::mpsc::channel(32);
    let state = state_with_session("s-full", commands);
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
    let state = state_with_session("s-closed", commands);
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

/// The reap helper removes only the stale entries: a session whose
/// activity stamp is older than the TTL goes, a fresh one stays.
#[test]
fn take_idle_removes_only_stale_sessions() {
    // A server-start a minute in the past: stamp arithmetic then has
    // real distance to work with and no test needs to sleep.
    let started = Instant::now().checked_sub(Duration::from_secs(60)).unwrap();
    let mut sessions = HashMap::new();
    let make = || AppSession {
        commands: tokio::sync::mpsc::channel(1).0,
        submissions: 0,
        events: tokio::sync::broadcast::channel(1).0,
        approvals: Arc::new(safety_gate::ApprovalGate::new()),
        questions: Arc::new(safety_gate::QuestionGate::new()),
        interrupt: infrastructure_base::InterruptHandle::new(),
        last_active_millis: Arc::new(AtomicU64::new(0)),
    };
    let fresh = make();
    fresh.touch(started);
    // The stale one never recorded activity: its stamp is still the
    // server start, a minute ago — the crashed-client shape.
    let stale = make();
    sessions.insert("fresh".to_string(), fresh);
    sessions.insert("stale".to_string(), stale);

    let reaped = take_idle(&mut sessions, Duration::from_secs(5), started);
    assert_eq!(reaped, vec!["stale".to_string()]);
    assert!(sessions.contains_key("fresh"), "an active session survives");
    assert!(!sessions.contains_key("stale"));
}

/// A crashed client can never send DELETE: the reaper closes its
/// session once the idle TTL passes, and later client calls see the
/// session as gone (404) instead of talking to a leaked actor.
#[tokio::test]
async fn idle_sessions_are_reaped_after_the_ttl() {
    let ttl = Duration::from_millis(400);
    let (token, port, done) = start_with(vec![], Some(ttl), None).await;
    let http = client(&token);
    let session_url = create_session_url(&http, port).await;
    let listed: serde_json::Value = http
        .get(server_url(port, "/sessions"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let listed_sessions = listed["sessions"]
        .as_array()
        .expect("sessions must be an array");
    assert_eq!(listed_sessions.len(), 1);
    assert_eq!(
        listed_sessions[0].as_str(),
        session_url.rsplit('/').next(),
        "the created session is the only listed one"
    );

    // Past TTL + one reap tick the session is gone for clients.
    tokio::time::sleep(ttl + Duration::from_millis(400)).await;
    let listed: serde_json::Value = http
        .get(server_url(port, "/sessions"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        listed["sessions"],
        serde_json::json!([]),
        "the idle session must be reaped"
    );
    let prompted = http
        .post(format!("{session_url}/prompt"))
        .json(&serde_json::json!({"text": "anyone there?"}))
        .send()
        .await
        .unwrap();
    assert_eq!(prompted.status(), StatusCode::NOT_FOUND);
    let _ = done;
}

/// The misfire guard: a client that keeps using its session — here by
/// prompting faster than the TTL — is never reaped, while the same
/// session goes once the activity stops.
#[tokio::test]
async fn recently_active_sessions_survive_the_reaper() {
    let ttl = Duration::from_millis(400);
    let (token, port, done) = start_with(vec![], Some(ttl), None).await;
    let http = client(&token);
    let session_url = create_session_url(&http, port).await;
    // Each prompt re-touches the session; the window spans several
    // TTLs, so only the touch (not creation) keeps it alive.
    for _ in 0..4 {
        tokio::time::sleep(ttl / 3).await;
        let prompted = http
            .post(format!("{session_url}/prompt"))
            .json(&serde_json::json!({"text": "still here"}))
            .send()
            .await
            .unwrap();
        assert_eq!(
            prompted.status(),
            StatusCode::ACCEPTED,
            "an actively used session must never be reaped mid-use"
        );
    }
    let listed: serde_json::Value = http
        .get(server_url(port, "/sessions"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let listed_sessions = listed["sessions"]
        .as_array()
        .expect("sessions must be an array");
    assert_eq!(listed_sessions.len(), 1);
    assert_eq!(
        listed_sessions[0].as_str(),
        session_url.rsplit('/').next(),
        "activity across several TTLs keeps the session"
    );
    // Activity stops: the reap takes over.
    tokio::time::sleep(ttl + Duration::from_millis(400)).await;
    let listed: serde_json::Value = http
        .get(server_url(port, "/sessions"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(listed["sessions"], serde_json::json!([]));
    let _ = done;
}

/// Creations past the cap answer 503 instead of assembling; deleting
/// a session frees a slot again.
#[tokio::test]
async fn session_creation_past_the_cap_answers_503() {
    let (token, port, done) = start_with(vec![], None, Some(1)).await;
    let http = client(&token);
    let first = create_session_url(&http, port).await;
    let refused = http
        .post(server_url(port, "/sessions"))
        .json(&serde_json::json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(
        refused.status(),
        StatusCode::SERVICE_UNAVAILABLE,
        "the cap must refuse, not evict"
    );
    let deleted = http.delete(first.as_str()).send().await.unwrap();
    assert_eq!(deleted.status(), StatusCode::OK);
    let replacement = create_session_url(&http, port).await;
    assert_ne!(first, replacement);
    let _ = done;
}
