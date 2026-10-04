//! Key and input handling: submission queue, editor staging,
//! undo, shell mode, and exit.

use super::*;
use crate::theme;
use test_support::{NullStatus, TestLink};
use tui_engine::keys::Mods;
use tui_engine::width::strip_ansi;

use super::tests_common::*;

#[test]
fn submit_enqueues_and_renders_user_message() {
    let mut ui = ui();
    ui.submit("hello");
    assert!(ui.state.busy(), "turn in flight");
    assert_eq!(ui.outbox_len(), 1, "user_input queued on the outbox");
    let frame = ui.frame(80, 24);
    let joined: String = frame
        .iter()
        .map(|l| strip_ansi(l))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(joined.contains("hello"), "user text in frame: {joined}");
}

#[test]
fn busy_submit_queues_message_and_pane_renders() {
    let mut ui = ui();
    ui.submit("first");
    ui.submit("second");
    assert_eq!(ui.state.queued, vec!["second".to_string()]);
    let frame = ui.frame(80, 24);
    let joined: String = frame
        .iter()
        .map(|l| strip_ansi(l))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        joined.contains("❯ second") || joined.contains("› second"),
        "queue pane: {joined}"
    );
    assert!(joined.contains("ctrl-s to steer"), "steer hint: {joined}");
}

#[test]
fn turn_completion_dequeues_next_message() {
    let mut ui = ui();
    ui.submit("first");
    ui.submit("second");
    ui.handle_wire_event(&EventMsg::TurnCompleted { interrupted: false });
    assert!(ui.state.busy(), "queued message starts the next turn");
    assert!(ui.state.queued.is_empty());
}

#[test]
fn undo_enqueues_rewind_and_rewound_trims_the_transcript() {
    let mut ui = ui();
    // Two completed turns in the books.
    ui.submit("one");
    ui.handle_wire_event(&EventMsg::TurnCompleted { interrupted: false });
    ui.handle_wire_event(&EventMsg::AgentMessageComplete {
        text: "answer one".to_string(),
    });
    ui.submit("two");
    ui.handle_wire_event(&EventMsg::TurnCompleted { interrupted: false });
    ui.handle_wire_event(&EventMsg::AgentMessageComplete {
        text: "answer two".to_string(),
    });
    // `/undo` while idle queues one Rewind op.
    let flow = ui.user_submit("/undo 1");
    assert_eq!(flow, Flow::Continue);
    assert_eq!(
        ui.pending_ops().last(),
        Some(&Op::Rewind { turns: 1 }),
        "/undo 1 rewinds one turn"
    );
    // Busy turns refuse the rewind.
    ui.submit("three");
    let before = ui.outbox_len();
    ui.user_submit("/undo");
    assert_eq!(ui.outbox_len(), before, "no rewind while busy");
    ui.handle_wire_event(&EventMsg::TurnCompleted { interrupted: false });
    // The landed event trims dialogue and transcript of one turn.
    ui.handle_wire_event(&EventMsg::HistoryRewound { turns: 1 });
    assert_eq!(ui.state.dialogue.len(), 4, "five entries minus one turn");
    let frame = ui.frame(80, 24);
    let joined: String = frame
        .iter()
        .map(|l| strip_ansi(l))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(joined.contains("answer two"), "older turns stay: {joined}");
    assert!(
        !joined.contains("three"),
        "rewound turn leaves the transcript: {joined}"
    );
    assert!(
        joined.contains("rewound 1 turn"),
        "status line confirms: {joined}"
    );
}

#[test]
fn undo_rejects_bad_counts_and_landed_zero() {
    let mut ui = ui();
    ui.user_submit("/undo two");
    assert!(
        ui.pending_ops().is_empty(),
        "non-numeric counts never reach the wire"
    );
    ui.handle_wire_event(&EventMsg::HistoryRewound { turns: 0 });
    let frame = ui.frame(80, 24);
    let joined = frame
        .iter()
        .map(|l| strip_ansi(l))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(joined.contains("nothing to rewind"), "{joined}");
}

#[test]
fn ctrl_g_stages_the_draft_and_apply_replaces_it() {
    let mut ui = ui();
    ui.editor.set_text("draft to improve");
    ui.handle_key(KeyEvent::new(Key::Char('g'), Mods::CTRL));
    assert_eq!(
        ui.take_external_edit().as_deref(),
        Some("draft to improve"),
        "ctrl+g stages the draft"
    );
    assert_eq!(
        ui.take_external_edit(),
        None,
        "the request is consumed once"
    );
    // The editor result replaces the draft buffer.
    ui.editor.set_text("draft to improve");
    ui.apply_external_edit("improved draft");
    assert_eq!(ui.editor.text(), "improved draft");
}

#[test]
fn ctrl_g_is_rejected_while_busy() {
    let mut ui = ui();
    ui.submit("running");
    ui.handle_key(KeyEvent::new(Key::Char('g'), Mods::CTRL));
    assert_eq!(ui.take_external_edit(), None, "busy blocks the editor");
}

#[test]
fn double_esc_opens_the_rewind_picker() {
    let mut ui = ui();
    ui.submit("first question");
    ui.handle_wire_event(&EventMsg::TurnCompleted { interrupted: false });
    ui.handle_wire_event(&EventMsg::AgentMessageComplete {
        text: "answer".to_string(),
    });
    // A lone Esc stays a no-op.
    ui.handle_key(KeyEvent::plain(Key::Esc));
    assert!(ui.dialog.is_none(), "single esc opens nothing");
    // The second Esc within the window opens the picker, newest
    // turn first.
    ui.handle_key(KeyEvent::plain(Key::Esc));
    assert!(matches!(ui.dialog, Some(Dialog::Undo(_))), "picker open");
    // Enter on the first row rewinds exactly one turn.
    let answer = ui
        .dialog
        .as_mut()
        .and_then(|d| d.handle_key(KeyEvent::plain(Key::Enter)));
    assert_eq!(answer, Some(Answer::RewindTurns { turns: 1 }));
    ui.dialog = None;
    ui.submit_answer(answer);
    assert_eq!(
        ui.pending_ops().last(),
        Some(&Op::Rewind { turns: 1 }),
        "picker feeds the same rewind path as /undo"
    );
}

#[test]
fn double_esc_with_no_history_reports_and_skips() {
    let mut ui = ui();
    ui.handle_key(KeyEvent::plain(Key::Esc));
    ui.handle_key(KeyEvent::plain(Key::Esc));
    assert!(ui.dialog.is_none(), "empty dialogue opens nothing");
}

#[test]
fn esc_exits_shell_mode_on_an_empty_prompt() {
    let mut ui = ui();
    ui.editor.set_text("!");
    ui.refresh_editor_chrome();
    assert!(ui.shell_chrome, "shell mode active");
    ui.handle_key(KeyEvent::plain(Key::Esc));
    assert!(!ui.shell_chrome, "esc leaves shell mode");
    assert_eq!(ui.editor.text(), "", "buffer cleared");
    // A populated shell buffer is not touched.
    ui.editor.set_text("!ls");
    ui.refresh_editor_chrome();
    ui.handle_key(KeyEvent::plain(Key::Esc));
    assert!(ui.shell_chrome, "non-empty shell buffer stays");
    assert_eq!(ui.editor.text(), "!ls");
}

#[test]
fn editor_command_resolution_prefers_the_setting() {
    // Pure resolution order: setting > $VISUAL > $EDITOR; blanks
    // never win.
    assert_eq!(
        pick_editor_command(Some("nvim"), Some("vim"), Some("vi")),
        Some("nvim")
    );
    assert_eq!(
        pick_editor_command(None, Some("vim"), Some("vi")),
        Some("vim")
    );
    assert_eq!(pick_editor_command(None, None, Some("vi")), Some("vi"));
    assert_eq!(pick_editor_command(None, None, None), None);
    assert_eq!(
        pick_editor_command(Some("  "), None, Some("vi")),
        Some("vi")
    );
}

#[test]
fn ctrl_c_cascade_arms_then_exits() {
    let mut ui = ui();
    assert_eq!(
        ui.handle_key(KeyEvent::new(Key::Char('c'), Mods::CTRL)),
        Flow::Continue
    );
    assert_eq!(
        ui.handle_key(KeyEvent::new(Key::Char('c'), Mods::CTRL)),
        Flow::Exit
    );
}

#[test]
fn ctrl_c_while_busy_interrupts_instead_of_exiting() {
    let mut ui = ui();
    ui.submit("go");
    assert_eq!(
        ui.handle_key(KeyEvent::new(Key::Char('c'), Mods::CTRL)),
        Flow::Continue
    );
    assert!(!ui.exit_armed(), "interrupt does not arm exit");
    assert!(matches!(ui.pending_ops().last(), Some(Op::Interrupt)));
}

#[test]
fn shift_tab_cycles_permission_mode() {
    let mut ui = ui();
    assert_eq!(ui.state.permission_mode, "auto");
    ui.handle_key(KeyEvent {
        key: Key::Tab,
        mods: Mods {
            shift: true,
            ctrl: false,
            alt: false,
        },
    });
    assert_eq!(ui.state.permission_mode, "wave");
    assert!(
        matches!(
            ui.pending_ops().last(),
            Some(Op::SetPermissionMode { mode }) if mode == "wave"
        ),
        "op enqueued"
    );
    ui.handle_key(KeyEvent {
        key: Key::Tab,
        mods: Mods {
            shift: true,
            ctrl: false,
            alt: false,
        },
    });
    assert_eq!(ui.state.permission_mode, "plan");
}

#[test]
fn ctrl_o_toggles_expansion() {
    let mut ui = ui();
    assert!(!ui.expanded.get());
    ui.handle_key(KeyEvent::new(Key::Char('o'), Mods::CTRL));
    assert!(ui.expanded.get());
    ui.handle_key(KeyEvent::new(Key::Char('o'), Mods::CTRL));
    assert!(!ui.expanded.get());
}

#[test]
fn ctrl_s_steers_with_editor_text() {
    let mut ui = ui();
    ui.submit("go");
    // Type text into the editor via direct insert.
    ui.editor.insert_text("change course");
    assert!(ui.steer_running_turn());
    assert!(ui.editor.is_empty(), "editor cleared after steering");
}

#[test]
fn ctrl_s_without_running_turn_reports() {
    let mut ui = ui();
    ui.editor.insert_text("hello");
    assert!(!ui.steer_running_turn(), "idle turn cannot steer");
    assert!(ui.editor.text() == "hello", "editor untouched");
}

#[test]
fn ctrl_c_in_dialog_denies_instead_of_dropping() {
    let mut ui = ui();
    ui.handle_wire_event(&EventMsg::ApprovalRequested {
        call_id: "c9".to_string(),
        kind: wavecode_wire::ApprovalKind::Exec,
        detail: "ls".to_string(),
    });
    assert!(ui.dialog.is_some(), "approval dialog open");
    ui.handle_key(KeyEvent::new(Key::Char('c'), Mods::CTRL));
    assert!(ui.dialog.is_none(), "dialog closed");
    match ui.pending_ops().last() {
        Some(Op::ExecApproval { call_id, decision }) => {
            assert_eq!(call_id, "c9");
            assert!(matches!(decision, wavecode_wire::WireDecision::Deny { .. }));
        }
        other => panic!("expected denial on the outbox: {other:?}"),
    }
}

#[test]
fn esc_while_busy_interrupts() {
    let mut ui = ui();
    ui.submit("go");
    ui.handle_key(KeyEvent::plain(Key::Esc));
    assert!(matches!(ui.pending_ops().last(), Some(Op::Interrupt)));
}

#[test]
fn ctrl_t_toggles_todo_expansion() {
    let mut ui = ui();
    assert!(!ui.state.todo_expanded);
    ui.handle_key(KeyEvent::new(Key::Char('t'), Mods::CTRL));
    assert!(ui.state.todo_expanded);
    ui.handle_key(KeyEvent::new(Key::Char('t'), Mods::CTRL));
    assert!(!ui.state.todo_expanded);
}

#[tokio::test]
async fn shell_command_runs_and_lands_in_transcript() {
    let mut ui = ui();
    let flow = ui.user_submit("!echo shell-ui-ok");
    assert_eq!(flow, Flow::Continue);
    assert!(
        ui.pending_ops().is_empty(),
        "shell commands never reach the session: {:?}",
        ui.pending_ops()
    );
    assert!(ui.shell.is_some(), "shell job running");
    // Drain until the job completes (bounded wait).
    for _ in 0..200 {
        ui.poll_shell();
        if ui.shell.is_none() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    assert!(ui.shell.is_none(), "shell job finished");
    let frame = ui.frame(80, 24);
    let joined: String = frame
        .iter()
        .map(|l| strip_ansi(l))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(joined.contains("$ echo shell-ui-ok"), "{joined}");
    assert!(joined.contains("shell-ui-ok"), "{joined}");
    assert!(
        !joined.contains("(esc to cancel)"),
        "card finalized: {joined}"
    );
}

#[test]
fn empty_bang_reports_usage() {
    let mut ui = ui();
    ui.user_submit("!");
    assert!(ui.shell.is_none());
    let frame = ui.frame(80, 24);
    let joined: String = frame
        .iter()
        .map(|l| strip_ansi(l))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(joined.contains("usage: !<command>"), "{joined}");
}

#[tokio::test]
async fn esc_cancels_running_shell() {
    let sleep = if cfg!(windows) {
        "ping -n 30 127.0.0.1"
    } else {
        "sleep 30"
    };
    let mut ui = ui();
    ui.user_submit(&format!("!{sleep}"));
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    ui.handle_key(KeyEvent::plain(Key::Esc));
    for _ in 0..200 {
        ui.poll_shell();
        if ui.shell.is_none() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    assert!(ui.shell.is_none(), "shell cancelled");
    let frame = ui.frame(80, 24);
    let joined: String = frame
        .iter()
        .map(|l| strip_ansi(l))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(joined.contains("terminated"), "{joined}");
}

#[test]
fn shell_buffer_tints_editor_chrome() {
    let mut ui = ui();
    ui.handle_key(KeyEvent::plain(Key::Char('!')));
    let frame = ui.frame(80, 24);
    let joined: String = frame
        .iter()
        .map(|l| strip_ansi(l))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(joined.contains("! shell mode"), "{joined}");
    ui.handle_key(KeyEvent::plain(Key::Backspace));
    let frame = ui.frame(80, 24);
    let joined: String = frame
        .iter()
        .map(|l| strip_ansi(l))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(!joined.contains("! shell mode"), "{joined}");
}

#[tokio::test]
async fn ctrl_c_cancels_shell_before_exiting() {
    let sleep = if cfg!(windows) {
        "ping -n 30 127.0.0.1"
    } else {
        "sleep 30"
    };
    let mut ui = ui();
    ui.user_submit(&format!("!{sleep}"));
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    // First Ctrl+C cancels the shell instead of arming exit.
    assert_eq!(
        ui.handle_key(KeyEvent::new(Key::Char('c'), Mods::CTRL)),
        Flow::Continue
    );
    assert!(!ui.exit_armed(), "shell cancel does not arm exit");
    for _ in 0..200 {
        ui.poll_shell();
        if ui.shell.is_none() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    assert!(ui.shell.is_none(), "shell cancelled by ctrl+c");
}

#[test]
fn bare_undo_opens_the_rewind_picker() {
    let mut ui = ui();
    ui.submit("one");
    ui.handle_wire_event(&EventMsg::TurnCompleted { interrupted: false });
    ui.handle_wire_event(&EventMsg::AgentMessageComplete {
        text: "answer one".to_string(),
    });
    ui.user_submit("/undo");
    assert!(matches!(ui.dialog, Some(Dialog::Undo(_))), "picker opens");
    let answer = ui
        .dialog
        .as_mut()
        .and_then(|d| d.handle_key(KeyEvent::plain(Key::Enter)));
    ui.dialog = None;
    ui.submit_answer(answer);
    assert_eq!(
        ui.pending_ops().last(),
        Some(&Op::Rewind { turns: 1 }),
        "picker Enter feeds the rewind path"
    );
}

/// With `confirm_exit` off, the first idle Ctrl+C exits instead of
/// arming the double-press window.
#[test]
fn confirm_exit_off_skips_the_double_press() {
    let mut ui = ui();
    ui.settings.update(|view| view.confirm_exit = false);
    let flow = ui.handle_key(KeyEvent::new(
        Key::Char('c'),
        Mods {
            ctrl: true,
            alt: false,
            shift: false,
        },
    ));
    assert_eq!(flow, Flow::Exit, "first ctrl+c exits");
}

/// UI exit submits `Op::Shutdown` so the actor's teardown (SessionEnd
/// hooks, memory pass) runs in a bounded drain instead of dying on
/// client drop — same contract the exec surface already honors.
#[tokio::test]
async fn exit_submits_shutdown_to_the_session_link() {
    theme::set(theme::Theme::dark());
    let link = std::sync::Arc::new(TestLink::new());
    let mut ui = ConsoleUi::new(
        Box::new(link.clone()),
        &UiContext {
            model_name: "test-model".to_string(),
            provider_id: String::new(),
            thinking_effort: None,
            thinking_levels: Vec::new(),
            cwd: PathBuf::from("/home/user/work/proj/sub"),
            permission_mode: "auto".to_string(),
            skill_names: Vec::new(),
            mcp_servers: Vec::new(),
            memory_files: Vec::new(),
            status: Arc::new(NullStatus),
            session_id: "session-0001".to_string(),
            session_title: None,
            model_entries: Vec::new(),
            home: None,
            update_notice: None,
            redactor: None,
        },
        "0.1.0",
    );
    // Shrunken window: TestLink yields no events, so the drain ends on
    // the deadline rather than a channel close.
    ui.shutdown_session_within(std::time::Duration::from_millis(50))
        .await;
    let submitted = link.submitted.lock().expect("test lock");
    assert!(
        submitted.iter().any(|op| matches!(op, Op::Shutdown)),
        "shutdown op missing: {submitted:?}"
    );
}
