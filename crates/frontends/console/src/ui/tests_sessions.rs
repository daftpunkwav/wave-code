//! Session lifecycle: journaling, resume, fork, clear, and the
//! /btw side session.

use super::*;
use test_support::{NullStatus, TestLink};
use tui_engine::width::strip_ansi;

use super::tests_common::*;

#[test]
fn clear_command_empties_transcript() {
    let mut ui = ui();
    ui.submit("hello assistant");
    ui.handle_wire_event(&EventMsg::TurnCompleted { interrupted: false });
    assert!(!ui.state.busy());
    assert!(!ui.transcript.is_empty());
    ui.user_submit("/clear");
    // The clear resets to a fresh welcome card, not a bare screen.
    assert_eq!(ui.transcript.len(), 1);
    assert!(ui.state.dialogue.is_empty());
}

/// A running turn blocks the session picker: switching sessions would
/// silently drop the turn and the queued messages.
#[test]
fn sessions_picker_refuses_to_open_while_busy() {
    let mut ui = ui();
    ui.submit("running turn");
    assert!(ui.state.busy());
    ui.user_submit("/sessions");
    assert!(ui.dialog.is_none(), "picker stays closed");
}

#[test]
fn completed_turns_journal_under_the_session_id() {
    let dir = tempfile::tempdir().unwrap();
    let mut ui = ui();
    ui.state.home = Some(dir.path().to_path_buf());
    ui.submit("hello assistant");
    ui.handle_wire_event(&EventMsg::TurnCompleted { interrupted: false });
    let sessions = state_persistence::sessions::list_sessions(dir.path());
    assert_eq!(sessions.len(), 1, "one session recorded");
    assert_eq!(sessions[0].id, ui.state.session_id);
    let history =
        state_persistence::sessions::load_session_history(dir.path(), &sessions[0].id).unwrap();
    assert!(history.contains(&(false, "hello assistant".to_string())));
}

#[tokio::test]
async fn sessions_picker_resumes_through_the_factory() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().to_path_buf();
    state_persistence::sessions::record_turn(
        &home,
        "seeded-session-id",
        "/test",
        "earlier question",
        &[
            (false, "earlier question".to_string()),
            (true, "earlier answer".to_string()),
        ],
        "Completed",
        &|t: &str| t.to_string(),
    )
    .unwrap();
    let mut ui = ui();
    ui.state.home = Some(home);
    ui.state.cwd = PathBuf::from("/test");
    let launched = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let launched_clone = launched.clone();
    ui.set_factory(std::sync::Arc::new(move |spec: &LaunchSpec| {
        launched_clone.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        assert_eq!(spec.session_id.as_deref(), Some("seeded-session-id"));
        let ctx = UiContext {
            model_name: "test-model".to_string(),
            provider_id: String::new(),
            thinking_effort: None,
            thinking_levels: Vec::new(),
            cwd: PathBuf::from("/test"),
            permission_mode: "auto".to_string(),
            skill_names: Vec::new(),
            mcp_servers: Vec::new(),
            memory_files: Vec::new(),
            status: Arc::new(NullStatus),
            session_id: spec.session_id.clone().unwrap_or_default(),
            session_title: None,
            model_entries: Vec::new(),
            home: None,
            update_notice: None,
            redactor: None,
        };
        Ok(SessionLaunch {
            link: Box::new(TestLink::new()),
            ctx,
            history: vec![
                (false, "earlier question".to_string()),
                (true, "earlier answer".to_string()),
            ],
        })
    }));
    // Open the picker (cwd-scoped to /test, where the seed lives),
    // then resume the highlighted entry.
    ui.user_submit("/sessions");
    assert!(ui.dialog.is_some(), "picker opens over the index");
    ui.handle_key(KeyEvent::plain(Key::Enter));
    assert!(ui.dialog.is_none());
    // The run loop drains the pending launch on its next iteration.
    ui.take_pending_launch();
    assert_eq!(
        launched.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "factory assembled the resume"
    );
    assert_eq!(ui.state.session_id, "seeded-session-id");
    assert_eq!(ui.state.dialogue.len(), 2, "seed history replayed");
}

#[test]
fn resume_without_home_or_factory_is_rejected() {
    let mut ui = ui();
    // No factory: rejected before the home check.
    ui.request_resume("some-id");
    assert!(ui.pending_launch.is_none());
    // With a factory but no home, resume must not read a relative
    // journal path; home: None disables resume by contract.
    ui.set_factory(std::sync::Arc::new(|_spec: &LaunchSpec| {
        panic!("factory must not be consulted without a home")
    }));
    ui.request_resume("some-id");
    assert!(ui.pending_launch.is_none());
    let frame = ui.frame(80, 24);
    let joined: String = frame
        .iter()
        .map(|l| strip_ansi(l))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        joined.contains("resume is unavailable without a home directory"),
        "{joined}"
    );
}

#[test]
fn fork_writes_journal_history_with_model_side_flags() {
    let dir = tempfile::tempdir().unwrap();
    let mut ui = ui();
    ui.state.home = Some(dir.path().to_path_buf());
    ui.submit("user question");
    ui.handle_wire_event(&EventMsg::AgentMessageDelta {
        text: "an answer".to_string(),
    });
    ui.handle_wire_event(&EventMsg::TurnCompleted { interrupted: false });
    ui.user_submit("/fork");
    // The journal flags the model side, so resuming the fork must
    // replay the user question as user and the answer as assistant.
    let fork = state_persistence::sessions::list_sessions(dir.path())
        .into_iter()
        .find(|meta| meta.title.starts_with("Fork:"))
        .expect("fork recorded in the index");
    let history = state_persistence::sessions::load_session_history(dir.path(), &fork.id).unwrap();
    assert_eq!(
        history,
        vec![
            (false, "user question".to_string()),
            (true, "an answer".to_string()),
        ]
    );
}

/// A session link that replays scripted events (drives the btw
/// pump) and records submissions.
struct ScriptBtwLink {
    submitted: Arc<std::sync::Mutex<Vec<Op>>>,
    events: Arc<std::sync::Mutex<std::collections::VecDeque<Event>>>,
}

#[async_trait::async_trait]
impl SessionLink for ScriptBtwLink {
    async fn submit(&self, submission: Submission) -> Result<(), SubmitError> {
        self.submitted
            .lock()
            .expect("test lock")
            .push(submission.op);
        Ok(())
    }

    async fn next_event(&mut self) -> Option<Event> {
        loop {
            let next = self.events.lock().expect("test lock").pop_front();
            if let Some(event) = next {
                return Some(event);
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    fn steer(&self, _text: &str, _target: SteerTarget) -> bool {
        true
    }
}

#[tokio::test]
async fn btw_streams_answers_through_the_side_session() {
    let submitted = Arc::new(std::sync::Mutex::new(Vec::new()));
    let script = Arc::new(std::sync::Mutex::new(std::collections::VecDeque::from(
        vec![
            Event {
                id: "t1".to_string(),
                msg: EventMsg::AgentMessageDelta {
                    text: "the answer is ".to_string(),
                },
            },
            Event {
                id: "t1".to_string(),
                msg: EventMsg::AgentMessageDelta {
                    text: "42".to_string(),
                },
            },
            Event {
                id: "t1".to_string(),
                msg: EventMsg::AgentMessageComplete {
                    text: String::new(),
                },
            },
            Event {
                id: "t1".to_string(),
                msg: EventMsg::TurnCompleted { interrupted: false },
            },
        ],
    )));
    let mut ui = ui();
    ui.state.cwd = PathBuf::from("/test");
    let submitted_clone = submitted.clone();
    let script_clone = script.clone();
    ui.set_factory(std::sync::Arc::new(move |spec: &LaunchSpec| {
        assert!(spec.read_only, "btw launches are read-only");
        assert_eq!(spec.model_override.as_deref(), Some("test-model"));
        Ok(SessionLaunch {
            link: Box::new(ScriptBtwLink {
                submitted: submitted_clone.clone(),
                events: script_clone.clone(),
            }),
            ctx: UiContext {
                model_name: "test-model".to_string(),
                provider_id: String::new(),
                thinking_effort: None,
                thinking_levels: Vec::new(),
                cwd: PathBuf::from("/test"),
                permission_mode: "plan".to_string(),
                skill_names: Vec::new(),
                mcp_servers: Vec::new(),
                memory_files: Vec::new(),
                status: Arc::new(NullStatus),
                session_id: String::new(),
                session_title: None,
                model_entries: Vec::new(),
                home: None,
                update_notice: None,
                redactor: None,
            },
            history: Vec::new(),
        })
    }));
    ui.user_submit("/btw what is the answer?");
    assert!(ui.btw.is_some(), "panel opens");
    // The question crossed to the side session (the pump task
    // submits asynchronously; wait for it).
    for _ in 0..100 {
        let landed = submitted.lock().unwrap().iter().any(
            |op| matches!(op, Op::UserInput { text, .. } if text.contains("what is the answer")),
        );
        if landed {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        submitted.lock().unwrap().iter().any(
            |op| matches!(op, Op::UserInput { text, .. } if text.contains("what is the answer"))
        ),
        "question must reach the side session"
    );
    // Drain the pump until the answer settles.
    for _ in 0..100 {
        ui.poll_btw();
        if !ui.btw_running {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(!ui.btw_running, "side turn completed");
    assert!(
        ui.btw_log
            .contains(&(false, "the answer is 42".to_string())),
        "answer landed: {:?}",
        ui.btw_log
    );
    // Follow-up rides the same side session (one more UserInput,
    // no new factory launch).
    ui.user_submit("/btw and now what?");
    for _ in 0..100 {
        let landed =
            submitted.lock().unwrap().iter().any(
                |op| matches!(op, Op::UserInput { text, .. } if text.contains("and now what")),
            );
        if landed {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        submitted
            .lock()
            .unwrap()
            .iter()
            .any(|op| matches!(op, Op::UserInput { text, .. } if text.contains("and now what"))),
        "follow-up must reach the same side session"
    );
    // Esc closes the panel and cancels the side session.
    ui.handle_key(KeyEvent::plain(Key::Esc));
    assert!(ui.btw.is_none(), "panel closed");
    assert!(ui.btw_log.is_empty());
    // The frame no longer shows the panel.
    let frame = ui.frame(80, 24);
    let joined: String = frame
        .iter()
        .map(|l| strip_ansi(l))
        .collect::<Vec<_>>()
        .join(
            "
",
        );
    assert!(!joined.contains("btw —"), "panel gone: {joined}");
}

#[test]
fn btw_without_args_opens_the_question_prompt() {
    let mut ui = ui();
    ui.user_submit("/btw");
    assert!(
        matches!(ui.dialog, Some(Dialog::Prompt(_))),
        "the prompt opens"
    );
    // The typed question routes into the side-question path; this
    // surface has no factory, so the ask lands on the unavailable note.
    for c in "what?".chars() {
        ui.handle_key(KeyEvent::plain(Key::Char(c)));
    }
    ui.handle_key(KeyEvent::plain(Key::Enter));
    assert!(ui.dialog.is_none(), "prompt closes on submit");
    let frame = ui.frame(80, 24);
    let joined: String = frame
        .iter()
        .map(|l| strip_ansi(l))
        .collect::<Vec<_>>()
        .join(
            "
",
        );
    assert!(
        joined.contains("side questions are unavailable"),
        "{joined}"
    );
    // Esc dismisses without asking.
    ui.user_submit("/btw");
    ui.handle_key(KeyEvent::plain(Key::Esc));
    assert!(ui.dialog.is_none(), "esc dismisses");
}

/// A link whose session is already over: `next_event` ends
/// immediately, driving the pump to report `Ended`.
struct DeadLink;

#[async_trait::async_trait]
impl SessionLink for DeadLink {
    async fn submit(&self, _submission: Submission) -> Result<(), SubmitError> {
        Ok(())
    }

    async fn next_event(&mut self) -> Option<Event> {
        None
    }

    fn steer(&self, _text: &str, _target: SteerTarget) -> bool {
        false
    }
}

#[tokio::test]
async fn btw_panel_survives_a_dead_side_session_and_relaunches() {
    let mut ui = ui();
    let launched = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let launched_clone = launched.clone();
    ui.set_factory(Arc::new(move |_spec: &LaunchSpec| {
        launched_clone.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(SessionLaunch {
            link: Box::new(DeadLink),
            ctx: UiContext {
                model_name: "test-model".to_string(),
                provider_id: String::new(),
                thinking_effort: None,
                thinking_levels: Vec::new(),
                cwd: PathBuf::from("/test"),
                permission_mode: "plan".to_string(),
                skill_names: Vec::new(),
                mcp_servers: Vec::new(),
                memory_files: Vec::new(),
                status: Arc::new(NullStatus),
                session_id: String::new(),
                session_title: None,
                model_entries: Vec::new(),
                home: None,
                update_notice: None,
                redactor: None,
            },
            history: Vec::new(),
        })
    }));
    ui.user_submit("/btw first question");
    // The pump sees the dead link and reports Ended; the tick loop
    // drains it and must drop the job instead of stranding the
    // panel in streaming forever.
    for _ in 0..100 {
        ui.poll_btw();
        if ui.btw.is_none() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(ui.btw.is_none(), "dead side session drops the job");
    assert!(!ui.btw_log.is_empty(), "the question stays visible");
    assert!(!ui.btw_running, "the panel must not stick in streaming");
    // A follow-up relaunches through the factory instead of asking
    // a dead pump (which would silently drop the question).
    ui.user_submit("/btw second question");
    assert_eq!(
        launched.load(std::sync::atomic::Ordering::SeqCst),
        2,
        "factory relaunched the side session"
    );
    assert!(ui.btw.is_some(), "a fresh job backs the reopened panel");
}

#[test]
fn new_session_without_factory_soft_clears() {
    let mut ui = ui();
    ui.submit("hello");
    ui.handle_wire_event(&EventMsg::TurnCompleted { interrupted: false });
    ui.user_submit("/new");
    // No factory: the transcript resets (welcome + status note) but
    // the session identity stays.
    let frame = ui.frame(80, 24);
    let joined: String = frame
        .iter()
        .map(|l| strip_ansi(l))
        .collect::<Vec<_>>()
        .join(
            "
",
        );
    assert!(joined.contains("screen cleared"), "{joined}");
    assert!(ui.state.dialogue.is_empty());
}

#[test]
fn clear_command_is_rejected_while_busy() {
    let mut ui = ui();
    ui.submit("hello assistant");
    assert!(ui.state.busy());
    ui.user_submit("/clear");
    let frame = ui.frame(80, 24);
    let joined: String = frame
        .iter()
        .map(|l| strip_ansi(l))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        joined.contains("hello assistant"),
        "user message must remain while busy"
    );
    assert!(
        joined.contains("cannot clear screen"),
        "busy hint must be shown"
    );
}

#[test]
fn clear_command_drops_pending_streaming_draft() {
    let mut ui = ui();
    ui.submit("hello assistant");
    ui.handle_wire_event(&EventMsg::TurnCompleted { interrupted: false });
    ui.streaming.push_assistant("stale draft");
    ui.user_submit("/clear");
    // The clear resets to a fresh welcome card, not a bare screen.
    assert_eq!(ui.transcript.len(), 1);
    assert!(
        ui.streaming.is_empty(),
        "streaming drafts must not survive a clear"
    );
    let frame = ui.frame(80, 24);
    let joined: String = frame
        .iter()
        .map(|l| strip_ansi(l))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        !joined.contains("stale draft"),
        "cleared draft must not render"
    );
}

#[tokio::test]
async fn clear_command_is_rejected_while_shell_runs() {
    let sleep = if cfg!(windows) {
        "ping -n 30 127.0.0.1"
    } else {
        "sleep 30"
    };
    let mut ui = ui();
    ui.user_submit(&format!("!{sleep}"));
    assert!(ui.shell.is_some(), "shell job started");
    ui.user_submit("/clear");
    assert!(
        !ui.transcript.is_empty(),
        "refused clear must keep the transcript"
    );
    assert!(
        ui.shell.is_some(),
        "refused clear must not drop the running shell"
    );
    assert!(ui.shell_card.is_some(), "live card index must stay valid");
    let frame = ui.frame(80, 24);
    let joined: String = frame
        .iter()
        .map(|l| strip_ansi(l))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        joined.contains("(esc to cancel)"),
        "live shell card must survive a refused clear"
    );
    assert!(
        joined.contains("cannot clear screen"),
        "shell hint must be shown"
    );
    ui.handle_key(KeyEvent::plain(Key::Esc));
    for _ in 0..200 {
        ui.poll_shell();
        if ui.shell.is_none() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    assert!(ui.shell.is_none(), "shell cancelled during cleanup");
}
