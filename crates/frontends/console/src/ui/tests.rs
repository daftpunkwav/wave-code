use super::*;
use crate::theme;
use std::path::Path;
use test_support::{NullStatus, TestLink};
use tui_engine::keys::Mods;
use tui_engine::width::strip_ansi;

/// Warning events route onto the amber Warning role, never the dim
/// body tone (loop notices: round limits, repeat breakers).
#[tokio::test]
async fn wire_warnings_render_with_the_warning_palette() {
    let mut ui = ui();
    ui.handle_wire_event(&EventMsg::Warning {
        message: "tool round limit reached (256); stopping this turn".to_string(),
    });
    let index = ui.transcript.last_index().expect("status pushed");
    let entry = ui.transcript.get_mut(index).expect("entry");
    let rendered = entry.component.render(80);
    let joined: String = rendered
        .iter()
        .map(|l| strip_ansi(l))
        .collect::<Vec<_>>()
        .join(
            "
",
        );
    assert!(
        joined.contains("warning: tool round limit reached (256)"),
        "{joined}"
    );
    // Warning amber #D29922, distinct from the dim body.
    let raw = rendered.join(
        "
",
    );
    assert!(raw.contains("[38;2;210;153;34m"), "{raw:?}");
}

fn ui() -> ConsoleUi {
    theme::set(theme::Theme::dark());
    let mut ui = ConsoleUi::new(
        Box::new(TestLink::new()),
        &UiContext {
            model_name: "test-model".to_string(),
            provider_id: String::new(),
            thinking_effort: None,
            thinking_levels: Vec::new(),
            cwd: PathBuf::from("/home/user/work/proj/sub"),
            permission_mode: "auto".to_string(),
            skill_names: Vec::new(),
            mcp_servers: vec!["fs".to_string()],
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
    // Picker and settings tests mutate settings; keep them off the
    // real user file.
    ui.settings = crate::settings::SharedSettings::without_persistence(Default::default());
    ui
}

#[test]
fn welcome_card_lists_brand_and_mcp() {
    let mut ui = ui();
    let frame = ui.frame(80, 24);
    let joined: String = frame
        .iter()
        .map(|l| strip_ansi(l))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(joined.contains(crate::welcome::LOGO[0]), "brand: {joined}");
    assert!(joined.contains("mcp 1"), "mcp count: {joined}");
    assert!(joined.contains("dir"), "{joined}");
}

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

/// Degenerate viewports (1-column terminals, single-row screens) must
/// render without panics and survive the full write pipeline: the
/// frame assembles, and the diff renderer truncates over-wide lines at
/// write time (Screen::write_line, tested in tui-engine) instead of
/// underflowing on `columns - 2` style arithmetic.
#[test]
fn degenerate_viewport_sizes_render_without_panics() {
    let mut screen = tui_engine::screen::Screen::new();
    screen.set_margin(crate::ui::GUTTER);
    for (columns, rows) in [(1usize, 1usize), (2, 1), (1, 30), (2, 30), (0, 0)] {
        let mut ui = ui();
        // Content on both sides of the frame: a submitted user turn and
        // a wide-character draft exercise transcript + editor + footer.
        ui.submit("user content to render");
        ui.editor.insert_text("editor draft 你好");
        let frame = ui.frame(columns, rows);
        let segment: tui_engine::component::Segment = Arc::new(frame);
        let mut out: Vec<u8> = Vec::new();
        screen.draw(&mut out, &[segment], columns, rows);
        assert!(!out.is_empty(), "frame({columns},{rows}) wrote nothing");
    }
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
fn turn_finished_notification_uses_the_composed_sequence() {
    let mut ui = ui();
    ui.handle_wire_event(&EventMsg::TurnCompleted { interrupted: false });
    // The composed notification (not the bare OSC 9) is what carries
    // WAVECODE_NOTIFY_STYLE and the tmux passthrough.
    assert_eq!(
        ui.take_pending_sequence().as_deref(),
        Some(notify::notification("turn finished").as_str())
    );
}

#[test]
fn failed_idle_compaction_settles_the_card() {
    let mut ui = ui();
    ui.handle_wire_event(&EventMsg::CompactStarted {
        trigger: "manual".to_string(),
    });
    assert!(ui.compaction_running(), "card live while compacting");
    // The actor reports a failed manual compaction as a recoverable
    // error with no turn attached; the card must settle anyway.
    ui.handle_wire_event(&EventMsg::Error {
        message: "compact failed".to_string(),
        recoverable: true,
        code: Some("compact.failed".to_string()),
    });
    assert!(!ui.compaction_running(), "error settles the idle card");
    let frame = ui.frame(80, 24);
    let joined: String = frame
        .iter()
        .map(|l| strip_ansi(l))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(joined.contains("compaction failed (manual)"), "{joined}");
}

#[test]
fn turn_end_settles_a_dangling_compaction_card() {
    let mut ui = ui();
    ui.handle_wire_event(&EventMsg::TurnStarted {
        model: "m".to_string(),
    });
    ui.handle_wire_event(&EventMsg::CompactStarted {
        trigger: "auto".to_string(),
    });
    assert!(ui.compaction_running());
    // No CompactCompleted arrives: the in-turn compaction failed and
    // the turn ended without it.
    ui.handle_wire_event(&EventMsg::TurnCompleted { interrupted: false });
    assert!(!ui.compaction_running(), "turn end settles the card");
    let frame = ui.frame(80, 24);
    let joined: String = frame
        .iter()
        .map(|l| strip_ansi(l))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(joined.contains("compaction failed (auto)"), "{joined}");
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
fn dead_turn_dismisses_a_parked_approval_modal() {
    let mut ui = ui();
    ui.handle_wire_event(&EventMsg::ApprovalRequested {
        call_id: "c1".to_string(),
        kind: wavecode_wire::ApprovalKind::Exec,
        detail: "run this".to_string(),
    });
    assert!(ui.dialog.is_some(), "approval open");
    // The turn ends (interrupt); the parked gate is gone.
    ui.handle_wire_event(&EventMsg::TurnCompleted { interrupted: true });
    assert!(ui.dialog.is_none(), "stale modal dismissed");
    let frame = ui.frame(80, 24);
    let joined = frame
        .iter()
        .map(|l| strip_ansi(l))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(joined.contains("request dismissed"), "{joined}");
    // User dialogs (settings) are never touched by turn events.
    ui.user_submit("/settings");
    assert!(ui.dialog.is_some(), "settings open");
    ui.handle_wire_event(&EventMsg::TurnCompleted { interrupted: false });
    assert!(ui.dialog.is_some(), "settings survive the turn end");
    ui.dialog = None;
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
fn turn_finished_notification_respects_focus() {
    let mut ui = ui();
    // Default (no focus event seen yet): notify as before — terminals
    // without focus reporting must not silently lose notifications.
    ui.handle_wire_event(&EventMsg::TurnCompleted { interrupted: false });
    assert!(
        ui.take_pending_sequence().is_some(),
        "unfocused default notifies"
    );
    // Focused terminal: the user is watching, no ping.
    ui.terminal_focused = true;
    ui.handle_wire_event(&EventMsg::TurnCompleted { interrupted: false });
    assert!(
        ui.take_pending_sequence().is_none(),
        "focused terminals must not be pinged"
    );
    // Focus lost again: notifications resume.
    ui.terminal_focused = false;
    ui.handle_wire_event(&EventMsg::TurnCompleted { interrupted: false });
    assert!(
        ui.take_pending_sequence().is_some(),
        "focus lost resumes pings"
    );
}

/// A wire warning that already carries the `warning:` marker must
/// not stack it twice on render.
#[test]
fn wire_warning_prefix_never_stacks() {
    let mut ui = ui();
    ui.handle_wire_event(&EventMsg::Warning {
        message: "warning: already marked".to_string(),
    });
    let index = ui.transcript.last_index().expect("status pushed");
    let entry = ui.transcript.get_mut(index).expect("entry");
    let rendered = entry.component.render(80);
    let joined = strip_ansi(&rendered.join(
        "
",
    ));
    assert!(joined.contains("warning: already marked"), "{joined}");
    assert!(
        !joined.contains("warning: warning:"),
        "no double marker: {joined}"
    );
}

#[test]
fn thinking_streaming_finalizes_into_transcript() {
    let mut ui = ui();
    ui.handle_wire_event(&EventMsg::AgentThinkingDelta {
        text: "pondering".to_string(),
    });
    let frame = ui.frame(80, 24);
    let joined: String = frame
        .iter()
        .map(|l| strip_ansi(l))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(joined.contains("Thinking for"), "live header: {joined}");
    assert!(joined.contains("pondering"), "live tail: {joined}");
    // Assistant output finalizes the thinking block.
    ui.handle_wire_event(&EventMsg::AgentMessageDelta {
        text: "answer".to_string(),
    });
    ui.handle_wire_event(&EventMsg::AgentMessageComplete {
        text: "answer".to_string(),
    });
    let frame = ui.frame(80, 24);
    let joined: String = frame
        .iter()
        .map(|l| strip_ansi(l))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(joined.contains("pondering"), "finalized block persists");
    assert!(joined.contains("answer"), "assistant text: {joined}");
    assert!(!joined.contains("thinking…"), "live header gone: {joined}");
}

#[test]
fn streaming_draft_persists_until_the_text_changes() {
    let mut ui = ui();
    ui.handle_wire_event(&EventMsg::AgentMessageDelta {
        text: "first".to_string(),
    });
    let frame = ui.frame(80, 24);
    let draft = ui.streaming_draft.as_ref().expect("draft built");
    assert_eq!(draft.text(), "first", "draft mirrors the buffer");
    // An unchanged buffer keeps the same draft instance across
    // frames: the markdown cache stays warm.
    let before: *const AssistantMessage = ui.streaming_draft.as_ref().unwrap();
    let _ = ui.frame(80, 24);
    assert!(
        std::ptr::eq(before, ui.streaming_draft.as_ref().unwrap()),
        "unchanged frames reuse the draft"
    );
    // A delta lands in the buffer: the next frame rebuilds to match.
    ui.handle_wire_event(&EventMsg::AgentMessageDelta {
        text: " second".to_string(),
    });
    let _ = ui.frame(80, 24);
    let draft = ui.streaming_draft.as_ref().expect("draft still live");
    assert_eq!(draft.text(), "first second", "rebuilt on the delta");
    assert!(!frame.is_empty(), "frame rendered");
    // Completion drains the buffer and retires the draft.
    ui.handle_wire_event(&EventMsg::AgentMessageComplete {
        text: "first second".to_string(),
    });
    let _ = ui.frame(80, 24);
    assert!(
        ui.streaming_draft.is_none(),
        "draft dropped once streaming ends"
    );
}

/// The live thinking block persists across frames: a rebuilt
/// instance would restart its header clock and spinner on every
/// flush (the same regression class as the streamed draft).
#[test]
fn streamed_thinking_block_persists_across_frames() {
    let mut ui = ui();
    ui.handle_wire_event(&EventMsg::AgentThinkingDelta {
        text: "deep thought".to_string(),
    });
    let _ = ui.frame(80, 24);
    let before: *const Thinking = ui.streaming_thinking.as_ref().unwrap();
    ui.handle_wire_event(&EventMsg::AgentThinkingDelta {
        text: " more".to_string(),
    });
    let _ = ui.frame(80, 24);
    assert!(
        std::ptr::eq(before, ui.streaming_thinking.as_ref().unwrap()),
        "same block instance across frames"
    );
    // Completion folds the persisted block into the transcript and
    // retires it.
    ui.handle_wire_event(&EventMsg::AgentMessageDelta {
        text: "answer".to_string(),
    });
    assert!(ui.streaming_thinking.is_none(), "block retired");
}

/// A fatal error ends the turn even when no TurnCompleted follows
/// behind it: the live blocks settle into the transcript and the
/// buffers drain, so no spinner spins forever and stale text cannot
/// bleed into the next turn's deltas.
#[test]
fn fatal_error_settles_the_streaming_blocks() {
    let mut ui = ui();
    ui.handle_wire_event(&EventMsg::AgentThinkingDelta {
        text: "deep thought".to_string(),
    });
    ui.handle_wire_event(&EventMsg::AgentMessageDelta {
        text: "partial answer".to_string(),
    });
    let _ = ui.frame(80, 24);
    assert!(ui.streaming_draft.is_some(), "draft live while streaming");
    ui.handle_wire_event(&EventMsg::Error {
        message: "connection lost".to_string(),
        recoverable: false,
        code: None,
    });
    // Like TurnCompleted, the buffers drain at once; the draft
    // instance retires on the next frame.
    let _ = ui.frame(80, 24);
    assert!(
        ui.streaming_draft.is_none(),
        "draft retired on the fatal error"
    );
    assert!(ui.streaming.is_empty(), "buffers drained");
    assert!(!ui.state.busy(), "phase back to idle");
    // The partial answer survives next to the error status line.
    let frame = ui.frame(80, 24);
    let joined: String = frame
        .iter()
        .map(|l| strip_ansi(l))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        joined.contains("partial answer"),
        "partial text kept: {joined}"
    );
    assert!(joined.contains("error: connection lost"), "{joined}");
    // The next turn starts from empty buffers.
    ui.handle_wire_event(&EventMsg::AgentMessageDelta {
        text: "next".to_string(),
    });
    let _ = ui.frame(80, 24);
    let draft = ui.streaming_draft.as_ref().expect("fresh draft");
    assert_eq!(draft.text(), "next", "no stale text");
}

#[test]
fn theme_switch_invalidates_the_cached_streaming_draft() {
    let mut ui = ui();
    ui.handle_wire_event(&EventMsg::AgentMessageDelta {
        text: "draft".to_string(),
    });
    let _ = ui.frame(80, 24);
    assert!(ui.streaming_draft.is_some(), "draft built while streaming");
    // The cached lines bake in the old palette: a theme switch must
    // drop the draft so the next frame re-renders under the new one
    // instead of stitching old-palette lines into the frame.
    ui.apply_theme("light");
    assert!(
        ui.streaming_draft.is_none(),
        "theme switch drops the cached draft"
    );
    let _ = ui.frame(80, 24);
    let draft = ui
        .streaming_draft
        .as_ref()
        .expect("draft rebuilt after the switch");
    assert_eq!(draft.text(), "draft", "rebuilt draft mirrors the buffer");
}

#[test]
fn completion_without_deltas_still_renders() {
    let mut ui = ui();
    ui.handle_wire_event(&EventMsg::AgentMessageComplete {
        text: "direct answer".to_string(),
    });
    let frame = ui.frame(80, 24);
    let joined: String = frame
        .iter()
        .map(|l| strip_ansi(l))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(joined.contains("direct answer"), "{joined}");
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
fn plan_command_updates_local_mode() {
    let mut ui = ui();
    let flow = ui.user_submit("/plan");
    assert_eq!(flow, Flow::Continue);
    assert_eq!(ui.state.permission_mode, "plan");
    assert!(
        matches!(
            ui.pending_ops().last(),
            Some(Op::SetPermissionMode { mode }) if mode == "plan"
        ),
        "wire op enqueued: {:?}",
        ui.pending_ops()
    );
}

#[test]
fn model_command_updates_local_model() {
    let mut ui = ui();
    ui.user_submit("/model fast-model");
    assert_eq!(ui.state.model_name, "fast-model");
    assert!(matches!(
        ui.pending_ops().last(),
        Some(Op::SetModel { name }) if name == "fast-model"
    ));
}

/// `/model <alias>` resolves through the picker entries exactly like
/// the picker's live switch: the wire model name rides the op, the
/// entry's default effort applies, and exactly one switch op is sent
/// (the old path double-sent one op per dispatch layer).
#[test]
fn model_command_resolves_an_alias_like_the_picker() {
    let mut ui = ui();
    ui.state.provider_id = "deepseek".to_string();
    ui.model_entries = vec![ModelEntryView {
        label: "GLM-5.3".to_string(),
        provider: "deepseek".to_string(),
        model: "glm-5.3".to_string(),
        effort: Some("max".to_string()),
    }];
    ui.user_submit("/model GLM-5.3");
    assert_eq!(ui.state.model_name, "GLM-5.3");
    assert_eq!(ui.state.thinking_effort.as_deref(), Some("max"));
    let ops = ui.pending_ops();
    assert_eq!(
        ops.iter()
            .filter(|op| matches!(op, Op::SetModel { .. }))
            .count(),
        1,
        "one switch op, not one per dispatch path: {ops:?}"
    );
    assert!(
        ops.iter()
            .any(|op| matches!(op, Op::SetModel { name } if name == "glm-5.3")),
        "the wire model name rides the op: {ops:?}"
    );
    assert!(
        ops.iter()
            .any(|op| matches!(op, Op::SetThinking { effort } if effort == "max")),
        "the entry's default effort applies: {ops:?}"
    );
}

/// A cross-provider alias refuses the live switch like the picker
/// does instead of silently keeping the old provider.
#[test]
fn model_command_refuses_cross_provider_switch_like_the_picker() {
    let mut ui = ui_with_models();
    ui.user_submit("/model MiniMax-M3");
    assert!(
        !ui.pending_ops()
            .iter()
            .any(|op| matches!(op, Op::SetModel { .. })),
        "cross-provider live switch refused: {:?}",
        ui.pending_ops()
    );
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
    assert!(ui.steer());
    assert!(ui.editor.is_empty(), "editor cleared after steering");
}

#[test]
fn ctrl_s_without_running_turn_reports() {
    let mut ui = ui();
    ui.editor.insert_text("hello");
    assert!(!ui.steer(), "idle turn cannot steer");
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
fn wire_text_is_sanitized_before_rendering() {
    let mut ui = ui();
    ui.handle_wire_event(&EventMsg::AgentMessageDelta {
        text: "ok\x1b[31mred".to_string(),
    });
    ui.handle_wire_event(&EventMsg::AgentMessageComplete {
        text: "done".to_string(),
    });
    let frame = ui.frame(80, 24);
    let raw: String = frame.join("\n");
    // The UI entry points strip injected SGR before rendering; the
    // only escapes left in the frame are the theme's own 38;2
    // sequences, never the model's basic-color injection.
    assert!(
        !raw.contains("\x1b[31m"),
        "injected SGR must be stripped: {raw:?}"
    );
    let joined: String = frame
        .iter()
        .map(|l| strip_ansi(l))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(joined.contains("okred"), "text still renders: {joined}");
}

#[test]
fn esc_while_busy_interrupts() {
    let mut ui = ui();
    ui.submit("go");
    ui.handle_key(KeyEvent::plain(Key::Esc));
    assert!(matches!(ui.pending_ops().last(), Some(Op::Interrupt)));
}

#[test]
fn footer_shows_context_meter() {
    let mut ui = ui();
    ui.handle_wire_event(&EventMsg::TokenCount {
        input_tokens: 84_000,
        output_tokens: 0,
        cache_read_tokens: 0,
        cache_creation_tokens: 0,
        context_window: Some(200_000),
        context_used: Some(84_000),
    });
    let footer = ui.footer(80);
    let right = strip_ansi(&footer[1]);
    assert!(
        right.trim_end().ends_with("context: 42% (82.0k/195k)"),
        "footer line 2: {right:?}"
    );
}

/// A drained release notice replaces the rotating tip in the
/// footer's right-hand slot and then sticks.
#[test]
fn update_notice_takes_the_footer_tip_slot() {
    let mut ui = ui();
    let slot = Arc::new(std::sync::Mutex::new(None));
    ui.update_slot = Some(slot.clone());
    assert!(!ui.poll_update_notice(), "empty slot must not repaint");
    *slot.lock().expect("test slot lock") = Some("update available: v9.9.9".to_string());
    assert!(ui.poll_update_notice(), "notice arrival must repaint");
    assert!(
        !ui.poll_update_notice(),
        "a shown notice must not re-trigger every tick"
    );
    let footer = ui.footer(80);
    let top = strip_ansi(&footer[0]);
    assert!(
        top.contains("update available: v9.9.9"),
        "footer line 1: {top:?}"
    );
}

#[test]
fn usage_command_renders_panel_from_accumulated_samples() {
    let mut ui = ui();
    ui.handle_wire_event(&EventMsg::TokenCount {
        input_tokens: 6_000,
        output_tokens: 1_500,
        cache_read_tokens: 50_000,
        cache_creation_tokens: 1_000,
        context_window: Some(200_000),
        context_used: Some(60_000),
    });
    ui.handle_wire_event(&EventMsg::TokenCount {
        input_tokens: 6_000,
        output_tokens: 1_500,
        cache_read_tokens: 50_000,
        cache_creation_tokens: 1_000,
        context_window: Some(200_000),
        context_used: Some(84_000),
    });
    ui.user_submit("/usage");
    let frame = ui.frame(80, 24);
    let joined: String = frame
        .iter()
        .map(|l| strip_ansi(l))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(joined.contains("● usage"), "{joined}");
    assert!(joined.contains("42% (82.0k/195k)"), "{joined}");
    assert!(joined.contains("read 97.7k"), "{joined}");
    assert!(
        joined.contains("total") && joined.contains("14.6k"),
        "{joined}"
    );
}

#[test]
fn version_command_prints_version() {
    let mut ui = ui();
    ui.user_submit("/version");
    let frame = ui.frame(80, 24);
    let joined: String = frame
        .iter()
        .map(|l| strip_ansi(l))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(joined.contains("WaveCode v0.1.0"), "{joined}");
}

#[test]
fn interrupt_acknowledges_in_transcript() {
    let mut ui = ui();
    ui.submit("go");
    ui.handle_key(KeyEvent::plain(Key::Esc));
    let frame = ui.frame(80, 24);
    let joined: String = frame
        .iter()
        .map(|l| strip_ansi(l))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(joined.contains("interrupting the turn"), "{joined}");
}

#[test]
fn cwd_shortening_keeps_tail_segments() {
    // No HOME mutation here: shorten_cwd emits "~/{tail}" whenever it
    // truncates, so these shapes assert the same under any home, and
    // mutating the process-global env would race concurrent tests.
    assert_eq!(
        shorten_cwd(Path::new("/home/user/work/proj/sub"), 3),
        "~/work/proj/sub"
    );
    assert_eq!(
        shorten_cwd(Path::new("/home/user/work/proj/sub"), 2),
        "~/proj/sub"
    );
    assert_eq!(shorten_cwd(Path::new("/opt"), 3), "/opt");
}

#[test]
fn frame_lines_fit_width() {
    let mut ui = ui();
    ui.submit("a very long user message that certainly wraps inside the eighty column budget of this test frame");
    ui.handle_wire_event(&EventMsg::AgentMessageDelta {
        text: "streamed reply ".repeat(20).trim_end().to_string(),
    });
    let frame = ui.frame(80, 24);
    for line in &frame {
        assert!(
            tui_engine::width::width(line) <= 80,
            "line exceeds width: {line:?}"
        );
    }
}

#[test]
fn todowrite_updates_panel() {
    let mut ui = ui();
    ui.handle_wire_event(&EventMsg::ToolCallBegin {
        call_id: "t1".to_string(),
        name: "todowrite".to_string(),
        input: serde_json::json!({
            "todos": [
                {"content": "probe\x1b[2Jclean", "status": "in_progress"},
                {"content": "second", "status": "pending"}
            ]
        }),
    });
    assert_eq!(ui.state.todos.len(), 2);
    let frame = ui.frame(80, 24);
    let joined: String = frame
        .iter()
        .map(|l| strip_ansi(l))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(joined.contains("Todo"), "panel header: {joined}");
    assert!(joined.contains("probeclean"), "sanitized content: {joined}");
    assert!(joined.contains("second"), "{joined}");
}

#[test]
fn todowrite_shape_mismatch_keeps_old_list() {
    let mut ui = ui();
    ui.handle_wire_event(&EventMsg::ToolCallBegin {
        call_id: "t1".to_string(),
        name: "todowrite".to_string(),
        input: serde_json::json!({
            "todos": [{"content": "ok", "status": "pending"}]
        }),
    });
    ui.handle_wire_event(&EventMsg::ToolCallBegin {
        call_id: "t2".to_string(),
        name: "todowrite".to_string(),
        input: serde_json::json!({
            "todos": [{"content": "bad", "status": "sideways"}]
        }),
    });
    assert_eq!(ui.state.todos.len(), 1);
    assert_eq!(ui.state.todos[0].content, "ok");
}

#[test]
fn empty_todowrite_hides_panel() {
    let mut ui = ui();
    ui.handle_wire_event(&EventMsg::ToolCallBegin {
        call_id: "t1".to_string(),
        name: "todowrite".to_string(),
        input: serde_json::json!({
            "todos": [{"content": "only", "status": "pending"}]
        }),
    });
    ui.handle_wire_event(&EventMsg::ToolCallBegin {
        call_id: "t2".to_string(),
        name: "todowrite".to_string(),
        input: serde_json::json!({"todos": []}),
    });
    assert!(ui.state.todos.is_empty());
    let frame = ui.frame(80, 24);
    let joined: String = frame
        .iter()
        .map(|l| strip_ansi(l))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(!joined.contains("Todo"), "panel hidden: {joined}");
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

struct FailingWriter;
impl std::io::Write for FailingWriter {
    fn write(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
        Err(std::io::Error::new(
            std::io::ErrorKind::BrokenPipe,
            "closed",
        ))
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Err(std::io::Error::new(
            std::io::ErrorKind::BrokenPipe,
            "closed",
        ))
    }
}

#[test]
fn render_propagates_write_error() {
    let mut ui = ui();
    let mut writer = FailingWriter;
    let res = ui.render(&mut writer, 80, 24);
    assert!(res.is_err());
    assert_eq!(res.unwrap_err().kind(), std::io::ErrorKind::BrokenPipe);
}

struct FailingLink;
#[async_trait]
impl SessionLink for FailingLink {
    async fn submit(&self, _submission: Submission) -> Result<(), SubmitError> {
        Err(SubmitError::ActorExited)
    }
    async fn next_event(&mut self) -> Option<Event> {
        None
    }
    fn steer(&self, _text: &str, _target: SteerTarget) -> bool {
        false
    }
}

#[tokio::test]
async fn flush_outbox_propagates_link_error() {
    let mut ui = ConsoleUi::new(
        Box::new(FailingLink),
        &UiContext {
            model_name: "test-model".to_string(),
            provider_id: String::new(),
            thinking_effort: None,
            thinking_levels: Vec::new(),
            cwd: PathBuf::from("/test"),
            permission_mode: "auto".to_string(),
            skill_names: Vec::new(),
            mcp_servers: Vec::new(),
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
    ui.enqueue(Op::Interrupt);
    let res = ui.flush_outbox().await;
    assert_eq!(res, Err(SubmitError::ActorExited));
    assert_eq!(
        ui.outbox.len(),
        1,
        "unsent operations must be preserved in outbox"
    );
}

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

fn picker_entries() -> Vec<crate::dialogs::ModelEntryView> {
    vec![
        crate::dialogs::ModelEntryView {
            label: "deepseek-chat".to_string(),
            provider: "deepseek".to_string(),
            model: "deepseek-chat".to_string(),
            effort: None,
        },
        crate::dialogs::ModelEntryView {
            label: "deepseek-reasoner".to_string(),
            provider: "deepseek".to_string(),
            model: "deepseek-reasoner".to_string(),
            effort: None,
        },
        crate::dialogs::ModelEntryView {
            label: "MiniMax-M3".to_string(),
            provider: "minimax".to_string(),
            model: "MiniMax-M3".to_string(),
            effort: Some("high".to_string()),
        },
    ]
}

fn ui_with_models() -> ConsoleUi {
    let mut ui = ui();
    ui.model_entries = picker_entries();
    ui.state.model_name = "deepseek-chat".to_string();
    ui.state.provider_id = "deepseek".to_string();
    ui.thinking_levels = vec!["off".to_string(), "low".to_string(), "high".to_string()];
    ui
}

#[test]
fn model_no_args_opens_picker_and_alt_s_switches_live() {
    let mut ui = ui_with_models();
    ui.user_submit("/model");
    assert!(ui.dialog.is_some(), "picker opens");
    let frame = ui.frame(80, 24);
    let joined: String = frame
        .iter()
        .map(|l| strip_ansi(l))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(joined.contains("Select a model"), "{joined}");
    // Move down to the same-provider sibling, then Alt+S: live
    // switch happens on the wire.
    ui.handle_key(KeyEvent::plain(Key::Down));
    ui.handle_key(KeyEvent::new(
        Key::Char('s'),
        tui_engine::keys::Mods {
            ctrl: false,
            alt: true,
            shift: false,
        },
    ));
    ui.handle_key(KeyEvent::plain(Key::Enter));
    let ops = ui.pending_ops().clone();
    assert!(
        ops.iter().any(|op| matches!(op, Op::SetModel { .. })),
        "same-provider selection switches live: {ops:?}"
    );
}

/// Bare `/provider add` opens the step-at-a-time model wizard;
/// `/model` is a switching command and points at /provider for its
/// retired subcommands.
#[test]
fn provider_add_bare_opens_the_wizard() {
    let mut ui = ui();
    ui.user_submit("/provider add");
    assert!(
        matches!(ui.dialog, Some(Dialog::ModelForm(_))),
        "wizard dialog opens"
    );
    ui.handle_key(KeyEvent::plain(Key::Esc));
    assert!(ui.dialog.is_none(), "esc dismisses");
    // The positional form still parses.
    ui.user_submit("/provider add x anthropic p https://h m");
    assert!(ui.dialog.is_none(), "positional add does not open a dialog");
    // /model keeps its switching focus and routes the old subcommands
    // to /provider with a status hint.
    ui.user_submit("/model add");
    assert!(ui.dialog.is_none(), "/model add opens nothing");
    let frame = ui.frame(80, 24);
    let joined: String = frame
        .iter()
        .map(|l| strip_ansi(l))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        joined.contains("/provider"),
        "migration hint shown: {joined}"
    );
}

#[test]
fn model_picker_enter_persists_default_and_cross_provider_skips_live_switch() {
    let mut ui = ui_with_models();
    ui.user_submit("/model");
    // Move to MiniMax-M3 (different provider), Enter persists.
    ui.handle_key(KeyEvent::plain(Key::Down));
    ui.handle_key(KeyEvent::plain(Key::Down));
    ui.handle_key(KeyEvent::plain(Key::Enter));
    let ops = ui.pending_ops().clone();
    assert!(
        !ops.iter().any(|op| matches!(op, Op::SetModel { .. })),
        "cross-provider selection must not switch live: {ops:?}"
    );
    // The picked model's effort must not leak onto the model that
    // is still live (it keeps sampling until the restart).
    assert!(
        !ops.iter().any(|op| matches!(op, Op::SetThinking { .. })),
        "cross-provider effort must not go live: {ops:?}"
    );
    assert_eq!(ui.state.thinking_effort, None);
    let view = ui.settings.get();
    assert_eq!(view.default_model.as_deref(), Some("MiniMax-M3"));
    assert_eq!(view.default_provider.as_deref(), Some("minimax"));
    assert_eq!(view.default_effort.as_deref(), Some("high"));
    let frame = ui.frame(80, 24);
    let joined: String = frame
        .iter()
        .map(|l| strip_ansi(l))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        joined.contains("restart to apply"),
        "cross-provider default explains the restart: {joined}"
    );
}

#[test]
fn cross_provider_session_only_switch_is_rejected_with_guidance() {
    let mut ui = ui_with_models();
    ui.user_submit("/model");
    // Alt+S on the cross-provider entry: nothing can go live and
    // session-only would persist nothing, so it must be rejected
    // with a pointer at Enter instead of a "switched" claim.
    ui.handle_key(KeyEvent::plain(Key::Down));
    ui.handle_key(KeyEvent::plain(Key::Down));
    ui.handle_key(KeyEvent::new(
        Key::Char('s'),
        tui_engine::keys::Mods {
            ctrl: false,
            alt: true,
            shift: false,
        },
    ));
    assert!(
        ui.pending_ops().is_empty(),
        "nothing may go live cross-provider: {:?}",
        ui.pending_ops()
    );
    assert_eq!(
        ui.settings.get().default_model,
        None,
        "session-only must not persist"
    );
    let frame = ui.frame(80, 24);
    let joined: String = frame
        .iter()
        .map(|l| strip_ansi(l))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        joined.contains("use Enter to save"),
        "guidance points at saving a default: {joined}"
    );
}

#[test]
fn model_picker_thinking_selection_enqueues_set_thinking() {
    let mut ui = ui_with_models();
    ui.user_submit("/model");
    // Right moves the draft from the seed (off) to "low".
    ui.handle_key(KeyEvent::plain(Key::Right));
    ui.handle_key(KeyEvent::plain(Key::Enter));
    let ops = ui.pending_ops().clone();
    assert!(
        ops.iter()
            .any(|op| matches!(op, Op::SetThinking { effort } if effort == "low")),
        "thinking draft reaches the wire: {ops:?}"
    );
    assert_eq!(ui.state.thinking_effort.as_deref(), Some("low"));
}

#[test]
fn permissions_no_args_opens_picker_and_enter_applies() {
    let mut ui = ui();
    ui.user_submit("/permissions");
    assert!(ui.dialog.is_some(), "mode picker opens");
    ui.handle_key(KeyEvent::plain(Key::Char('1')));
    assert!(ui.dialog.is_none());
    assert_eq!(ui.state.permission_mode, "plan");
    assert!(
        ui.pending_ops()
            .iter()
            .any(|op| matches!(op, Op::SetPermissionMode { mode } if mode == "plan")),
    );
}

/// Bare `/permissions` must not rotate the mode behind the picker: a
/// dismissal has to leave the mode exactly as it was.
#[test]
fn permissions_bare_leaves_mode_until_picked() {
    let mut ui = ui();
    let before = ui.state.permission_mode.clone();
    ui.user_submit("/permissions");
    assert!(ui.dialog.is_some(), "mode picker opens");
    ui.handle_key(KeyEvent::plain(Key::Esc));
    assert!(ui.dialog.is_none());
    assert_eq!(ui.state.permission_mode, before, "dismissal is a no-op");
    assert!(
        !ui.pending_ops()
            .iter()
            .any(|op| matches!(op, Op::SetPermissionMode { .. })),
        "no mode op queued by the bare form",
    );
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
fn help_opens_panel_dialog_instead_of_status_lines() {
    let mut ui = ui();
    ui.user_submit("/help");
    assert!(matches!(ui.dialog, Some(Dialog::Help(_))));
    ui.handle_key(KeyEvent::plain(Key::Esc));
    assert!(ui.dialog.is_none());
}

#[test]
fn direct_mode_shortcuts_dispatch() {
    let mut ui = ui();
    ui.user_submit("/wave");
    assert_eq!(ui.state.permission_mode, "wave");
    ui.user_submit("/auto");
    assert_eq!(ui.state.permission_mode, "auto");
}

#[test]
fn effort_command_sets_level() {
    let mut ui = ui();
    ui.user_submit("/effort high");
    assert_eq!(ui.state.thinking_effort.as_deref(), Some("high"));
    assert!(
        ui.pending_ops()
            .iter()
            .any(|op| matches!(op, Op::SetThinking { effort } if effort == "high")),
    );
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
        assert!(spec.readonly, "btw launches are read-only");
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

#[test]
fn theme_no_args_opens_picker_and_enter_applies() {
    let mut ui = ui();
    ui.user_submit("/theme");
    assert!(matches!(ui.dialog, Some(Dialog::Theme(_))), "picker opens");
    let frame = ui.frame(80, 24);
    let joined: String = frame
        .iter()
        .map(|l| strip_ansi(l))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(joined.contains("Select a theme"), "{joined}");
    assert!(joined.contains("deepwave"), "{joined}");
    // Enter on the highlighted row (auto, seeded at startup) applies.
    ui.handle_key(KeyEvent::plain(Key::Enter));
    assert!(ui.dialog.is_none());
    assert_eq!(ui.theme_name, "auto");
    // Quick-select digit 3 applies deepwave.
    ui.user_submit("/theme");
    ui.handle_key(KeyEvent::plain(Key::Char('3')));
    assert!(ui.dialog.is_none());
    assert_eq!(ui.theme_name, "deepwave");
    let frame = ui.frame(80, 24);
    let joined: String = frame
        .iter()
        .map(|l| strip_ansi(l))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(joined.contains("theme switched (deepwave)"), "{joined}");
    // Restore the default: the applied deepwave install is a process
    // global, and leaving it behind races every later dark assertion.
    theme::set(theme::Theme::dark());
}

#[test]
fn effort_no_args_opens_picker_and_digit_applies() {
    let mut ui = ui_with_models();
    ui.user_submit("/effort");
    assert!(matches!(ui.dialog, Some(Dialog::Effort(_))), "picker opens");
    // Digit 2 = "low" (off seeds first).
    ui.handle_key(KeyEvent::plain(Key::Char('2')));
    assert!(ui.dialog.is_none());
    assert_eq!(ui.state.thinking_effort.as_deref(), Some("low"));
    assert!(
        ui.pending_ops()
            .iter()
            .any(|op| matches!(op, Op::SetThinking { effort } if effort == "low")),
    );
    // Selecting "off" clears the level (digit 1 = the seeded "off").
    ui.user_submit("/effort");
    ui.handle_key(KeyEvent::plain(Key::Char('1')));
    assert_eq!(ui.state.thinking_effort, None);
    assert!(
        ui.pending_ops()
            .iter()
            .any(|op| matches!(op, Op::SetThinking { effort } if effort == "off")),
    );
}

#[test]
fn compact_no_args_opens_prompt_and_text_steers() {
    let mut ui = ui();
    ui.user_submit("/compact");
    assert!(matches!(ui.dialog, Some(Dialog::Prompt(_))));
    // Empty submit compacts without steering.
    ui.handle_key(KeyEvent::plain(Key::Enter));
    assert!(ui.dialog.is_none());
    assert_eq!(
        ui.pending_ops().last(),
        Some(&Op::Compact { instruction: None }),
        "empty submit compacts now"
    );
    // A typed instruction steers the summary.
    ui.user_submit("/compact");
    for c in "keep the api decisions".chars() {
        ui.handle_key(KeyEvent::plain(Key::Char(c)));
    }
    ui.handle_key(KeyEvent::plain(Key::Enter));
    assert_eq!(
        ui.pending_ops().last(),
        Some(&Op::Compact {
            instruction: Some("keep the api decisions".to_string())
        }),
        "typed text rides the op"
    );
}

#[test]
fn title_no_args_opens_prompt_and_enter_renames() {
    let dir = tempfile::tempdir().unwrap();
    let mut ui = ui();
    ui.state.home = Some(dir.path().to_path_buf());
    // A recorded session gives the rename something to land on.
    ui.submit("hello assistant");
    ui.handle_wire_event(&EventMsg::TurnCompleted { interrupted: false });
    ui.user_submit("/title");
    assert!(matches!(ui.dialog, Some(Dialog::Prompt(_))));
    for c in "the real topic".chars() {
        ui.handle_key(KeyEvent::plain(Key::Char(c)));
    }
    ui.handle_key(KeyEvent::plain(Key::Enter));
    assert!(ui.dialog.is_none());
    assert_eq!(
        ui.state.session_title.as_deref(),
        Some("the real topic"),
        "the prompt renames the session"
    );
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

/// `/settings` exposes the full tunable surface; cycling a row flips
/// the persisted value.
#[test]
fn settings_panel_covers_the_tunables_and_cycles_them() {
    let mut ui = ui();
    ui.user_submit("/settings");
    assert!(matches!(ui.dialog, Some(Dialog::Settings(_))));
    let frame = ui.frame(90, 30);
    let joined: String = frame
        .iter()
        .map(|l| strip_ansi(l))
        .collect::<Vec<_>>()
        .join("\n");
    for label in [
        "render user input as markdown",
        "thinking starts expanded",
        "stream the assistant draft",
        "context meter in footer",
        "rotate footer tips",
        "confirm before exit",
        "input history limit",
    ] {
        assert!(joined.contains(label), "missing row {label:?}: {joined}");
    }
    // The first row toggles markdown rendering and survives a cycle.
    let before = ui.settings.get().render_user_markdown;
    ui.handle_key(KeyEvent::plain(Key::Enter));
    assert_eq!(ui.settings.get().render_user_markdown, !before);
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
    ui.shutdown_session_within(std::time::Duration::from_millis(50)).await;
    let submitted = link.submitted.lock().expect("test lock");
    assert!(
        submitted
            .iter()
            .any(|op| matches!(op, Op::Shutdown)),
        "shutdown op missing: {submitted:?}"
    );
}
