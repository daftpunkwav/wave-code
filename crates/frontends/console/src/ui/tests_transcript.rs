//! Wire events, streaming, and transcript / panel rendering.

use super::*;
use std::path::Path;
use test_support::NullStatus;
use tui_engine::width::strip_ansi;

use super::tests_common::*;

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

/// A streamed message whose live draft hit the runaway cap must not stay
/// truncated in the transcript: the completed text is authoritative, so the
/// tail the draft dropped is restored when the message completes.
#[test]
fn capped_stream_draft_is_replaced_by_the_completion_text() {
    let mut ui = ui();
    let full = format!("{}FINAL-TAIL-MARKER", "lorem ipsum ".repeat(6000));
    ui.handle_wire_event(&EventMsg::AgentMessageDelta { text: full.clone() });
    assert!(
        ui.streaming.assistant_truncated(),
        "the live draft hit the runaway cap"
    );
    ui.handle_wire_event(&EventMsg::AgentMessageComplete { text: full.clone() });
    let index = ui.transcript.last_index().expect("assistant pushed");
    let entry = ui.transcript.get_mut(index).expect("entry");
    let rendered = entry.component.render(80);
    let joined = strip_ansi(&rendered.join("\n"));
    assert!(
        joined.contains("FINAL-TAIL-MARKER"),
        "the completion text must restore the dropped tail"
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
fn footer_shows_context_meter() {
    let mut ui = ui();
    ui.handle_wire_event(&EventMsg::TokenCount {
        // nosemgrep: codacy.yaml.security.hard-coded-tokens
        input_tokens: 84_000,
        // nosemgrep: codacy.yaml.security.hard-coded-tokens
        output_tokens: 0,
        // nosemgrep: codacy.yaml.security.hard-coded-tokens
        cache_read_tokens: 0,
        // nosemgrep: codacy.yaml.security.hard-coded-tokens
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
        // nosemgrep: codacy.yaml.security.hard-coded-tokens
        input_tokens: 6_000,
        // nosemgrep: codacy.yaml.security.hard-coded-tokens
        output_tokens: 1_500,
        // nosemgrep: codacy.yaml.security.hard-coded-tokens
        cache_read_tokens: 50_000,
        // nosemgrep: codacy.yaml.security.hard-coded-tokens
        cache_creation_tokens: 1_000,
        context_window: Some(200_000),
        context_used: Some(60_000),
    });
    ui.handle_wire_event(&EventMsg::TokenCount {
        // nosemgrep: codacy.yaml.security.hard-coded-tokens
        input_tokens: 6_000,
        // nosemgrep: codacy.yaml.security.hard-coded-tokens
        output_tokens: 1_500,
        // nosemgrep: codacy.yaml.security.hard-coded-tokens
        cache_read_tokens: 50_000,
        // nosemgrep: codacy.yaml.security.hard-coded-tokens
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
    ui.enqueue(Op::Interrupt);
    let res = ui.flush_outbox().await;
    assert_eq!(res, Err(SubmitError::ActorExited));
    assert_eq!(
        ui.outbox.len(),
        1,
        "unsent operations must be preserved in outbox"
    );
}
