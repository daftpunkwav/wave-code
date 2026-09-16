//! The console UI orchestrator: owns the frame assembly, input
//! dispatch, and the session event pump.
//!
//! Frame layout (inline mode, native scrollback): transcript lines,
//! then the editor box, then the two footer rows. As the transcript
//! grows, earlier lines scroll into scrollback and are never rewritten.
//! Operations destined for the session are placed on an outbox that
//! the run loop drains and submits, keeping the UI logic synchronous
//! and testable.

use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossterm::event::{Event as CEvent, EventStream, KeyEventKind};
use futures::StreamExt;
use operations_actor::{ActorClient, StatusQueries};
use tui_engine::editor::{Editor, EditorAction, EditorStyle};
use tui_engine::keys::{Key, KeyEvent};
use tui_engine::markdown::PlainHighlighter;
use tui_engine::screen::Screen;
use tui_engine::terminal::{self, TerminalGuard};
use tui_engine::width;
use uuid::Uuid;
use wavecode_wire::{EventMsg, Op, Submission};

use crate::messages::{AssistantMessage, StatusLine, UserMessage};
use crate::state::{AppState, StreamingPhase, context_percent, format_tokens};
use crate::theme::{self, Token};
use crate::welcome::Welcome;

/// One-space chrome gutter: transcript, editor, and footer share it.
const GUTTER: usize = 1;
/// Double-press window for the Ctrl+C exit confirmation.
pub const EXIT_CONFIRM_WINDOW: Duration = Duration::from_millis(1500);

/// Session facts the UI cannot derive from the wire.
#[derive(Clone)]
pub struct UiContext {
    /// Model display name.
    pub model_name: String,
    /// Session working directory.
    pub cwd: PathBuf,
    /// Permission-mode wire name.
    pub permission_mode: String,
    /// Directly invokable skill names (slash completion candidates).
    pub skill_names: Vec<String>,
    /// Connected MCP server names.
    pub mcp_servers: Vec<String>,
    /// Session status queries (plan, goal, snapshots).
    pub status: Arc<dyn StatusQueries>,
}

/// What the run loop should do after handling one input batch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flow {
    /// Continue running.
    Continue,
    /// Tear down and exit.
    Exit,
}

/// The console UI: transcript + editor + footer over a session.
pub struct ConsoleUi {
    state: AppState,
    status: Arc<dyn StatusQueries>,
    editor: Editor,
    transcript: Vec<Box<dyn tui_engine::Component>>,
    screen: Screen,
    outbox: Vec<Op>,
    /// Text streamed for the in-flight assistant message.
    assistant_draft: String,
    /// Streamed thinking text (shown live, then folded in later phases).
    thinking_draft: String,
    exit_armed_at: Option<Instant>,
    version: String,
}

impl ConsoleUi {
    /// Build the UI; seeds the transcript with the welcome card.
    pub fn new(ctx: &UiContext, version: impl Into<String>) -> Self {
        let state = AppState::new(
            ctx.model_name.clone(),
            ctx.cwd.clone(),
            ctx.permission_mode.clone(),
            ctx.mcp_servers.clone(),
        );
        let mut ui = Self {
            state,
            status: ctx.status.clone(),
            editor: Editor::new(EditorStyle::default()),
            transcript: Vec::new(),
            screen: Screen::new(),
            outbox: Vec::new(),
            assistant_draft: String::new(),
            thinking_draft: String::new(),
            exit_armed_at: None,
            version: version.into(),
        };
        ui.push_welcome();
        ui
    }

    fn push_welcome(&mut self) {
        let info = crate::welcome::WelcomeInfo {
            version: self.version.clone(),
            model: self.state.model_name.clone(),
            mode: permission_mode_label(&self.state.permission_mode).to_string(),
            cwd: self.state.cwd.to_string_lossy().to_string(),
            mcp_servers: self.state.mcp_servers.clone(),
        };
        self.transcript.push(Box::new(Welcome::new(info)));
    }

    /// Push one user message.
    pub fn push_user_message(&mut self, text: &str) {
        self.transcript.push(Box::new(UserMessage::new(text)));
    }

    /// Push one assistant markdown message.
    pub fn push_assistant_message(&mut self, text: &str) {
        self.transcript.push(Box::new(AssistantMessage::new(
            text,
            Box::new(PlainHighlighter),
        )));
    }

    /// Push one status line.
    pub fn push_status(&mut self, text: &str, is_error: bool) {
        if is_error {
            self.transcript.push(Box::new(StatusLine::error(text)));
        } else {
            self.transcript.push(Box::new(StatusLine::new(text)));
        }
    }

    /// Submit user text: queued while busy, sent otherwise.
    pub fn submit(&mut self, text: &str) {
        let text = text.to_string();
        if self.state.busy() {
            self.state.queued.push(text);
            return;
        }
        self.push_user_message(&text);
        self.enqueue(Op::UserInput { text });
        self.state.phase = StreamingPhase::Waiting;
    }

    /// Queue one operation for the run loop to submit.
    pub fn enqueue(&mut self, op: Op) {
        self.outbox.push(op);
    }

    /// Drain the pending operations (the run loop submits these).
    pub fn take_ops(&mut self) -> Vec<Op> {
        std::mem::take(&mut self.outbox)
    }

    /// Handle one wire event; returns true when the frame changed.
    pub fn handle_wire_event(&mut self, msg: &EventMsg) -> bool {
        match msg {
            EventMsg::TurnStarted => {
                self.state.phase = StreamingPhase::Waiting;
                true
            }
            EventMsg::AgentThinkingDelta { text } => {
                self.thinking_draft.push_str(text);
                self.state.phase = StreamingPhase::Thinking;
                true
            }
            EventMsg::AgentMessageDelta { text } => {
                self.assistant_draft.push_str(text);
                self.state.phase = StreamingPhase::Composing;
                true
            }
            EventMsg::AgentMessageComplete { text } => {
                // Deltas already streamed the text; the completion carries
                // the full body for transcript rendering.
                if !self.assistant_draft.is_empty() {
                    let draft = std::mem::take(&mut self.assistant_draft);
                    self.push_assistant_message(&draft);
                } else if !text.is_empty() {
                    self.push_assistant_message(text);
                }
                self.thinking_draft.clear();
                true
            }
            EventMsg::ToolCallBegin { name, .. } => {
                self.state.phase = StreamingPhase::Tool;
                self.push_status(&format!("running {name}"), false);
                true
            }
            EventMsg::ToolCallEnd { is_error, .. } => {
                self.state.phase = StreamingPhase::Composing;
                if *is_error {
                    self.push_status("tool call failed", true);
                }
                true
            }
            EventMsg::TokenCount {
                context_used,
                context_window,
                ..
            } => {
                self.state.context_used = *context_used;
                self.state.context_window = *context_window;
                true
            }
            EventMsg::CompactStarted { trigger } => {
                self.push_status(&format!("compacting context ({trigger})"), false);
                true
            }
            EventMsg::CompactCompleted { summary_tokens } => {
                self.push_status(
                    &format!(
                        "context compacted ({} tokens)",
                        format_tokens(*summary_tokens)
                    ),
                    false,
                );
                true
            }
            EventMsg::PlanProposed { text } => {
                self.push_assistant_message(text);
                true
            }
            EventMsg::PlanApproved => {
                self.push_status("plan approved", false);
                true
            }
            EventMsg::GoalSet { objective } => {
                self.push_status(&format!("goal set: {objective}"), false);
                true
            }
            EventMsg::GoalCompleted => {
                self.push_status("goal completed", false);
                true
            }
            EventMsg::Warning { message } => {
                self.push_status(&format!("warning: {message}"), false);
                true
            }
            EventMsg::Error {
                message,
                recoverable,
            } => {
                self.push_status(&format!("error: {message}"), true);
                if !*recoverable {
                    self.state.phase = StreamingPhase::Idle;
                }
                true
            }
            EventMsg::TurnCompleted { interrupted } => {
                self.state.phase = StreamingPhase::Idle;
                if *interrupted {
                    self.push_status("interrupted", false);
                }
                self.assistant_draft.clear();
                self.thinking_draft.clear();
                // Dequeue a queued message as the next turn.
                if !self.state.queued.is_empty() {
                    let next = self.state.queued.remove(0);
                    self.submit(&next);
                }
                true
            }
            EventMsg::ApprovalRequested {
                call_id,
                kind,
                detail,
            } => {
                self.push_status(
                    &format!("approval requested for {call_id} ({kind:?}): {detail}"),
                    false,
                );
                true
            }
            EventMsg::QuestionRequested { .. } => false,
        }
    }

    /// Handle one key event; returns the flow decision.
    pub fn handle_key(&mut self, event: KeyEvent) -> Flow {
        match self.editor.handle_key(event) {
            EditorAction::Submit(text) => {
                self.exit_armed_at = None;
                self.submit(&text);
                Flow::Continue
            }
            EditorAction::Handled => Flow::Continue,
            EditorAction::Passthrough => self.handle_passthrough_key(event),
        }
    }

    fn handle_passthrough_key(&mut self, event: KeyEvent) -> Flow {
        match (event.key, event.mods) {
            (Key::Char('c'), m) if m.ctrl => self.handle_ctrl_c(),
            (Key::Char('d'), m) if m.ctrl && self.editor.is_empty() => {
                if self.exit_armed() {
                    Flow::Exit
                } else {
                    self.exit_armed_at = Some(Instant::now());
                    Flow::Continue
                }
            }
            (Key::Up, _) => {
                self.editor.history_previous();
                Flow::Continue
            }
            (Key::Down, _) => {
                self.editor.history_next();
                Flow::Continue
            }
            _ => Flow::Continue,
        }
    }

    fn handle_ctrl_c(&mut self) -> Flow {
        if self.exit_armed() {
            return Flow::Exit;
        }
        if self.state.busy() {
            self.enqueue(Op::Interrupt);
            self.exit_armed_at = None;
            return Flow::Continue;
        }
        // Idle: first press clears and arms exit; second press exits.
        self.exit_armed_at = Some(Instant::now());
        if !self.editor.is_empty() {
            self.editor.clear();
        }
        Flow::Continue
    }

    fn exit_armed(&self) -> bool {
        self.exit_armed_at
            .is_some_and(|at| at.elapsed() < EXIT_CONFIRM_WINDOW)
    }

    /// The two footer rows.
    pub fn footer(&mut self, columns: usize) -> Vec<String> {
        let theme = theme::current();
        let mut line1 = String::new();
        line1.push_str(&mode_badge(&self.state.permission_mode));
        line1.push_str("  ");
        line1.push_str(&theme.paint(Token::Text, &self.state.model_name));
        line1.push_str("  ");
        line1.push_str(&theme.paint(Token::TextDim, &shorten_cwd(&self.state.cwd, 3)));

        let mut line2 = String::new();
        if self.exit_armed() {
            line2.push_str(&theme.bold(Token::Warning, "Press ctrl+c again to exit"));
        }
        let mut right = String::new();
        if let (Some(used), Some(window)) = (self.state.context_used, self.state.context_window) {
            right = format!(
                "context: {}% ({}/{})",
                context_percent(used, window),
                format_tokens(used),
                format_tokens(window)
            );
            right = theme.paint(Token::Text, &right);
        }
        let left_width = width::width(&line2);
        let right_width = width::width(&right);
        let spacing = columns
            .saturating_sub(left_width + right_width)
            .max(if right.is_empty() { 0 } else { 1 });
        line2.push_str(&" ".repeat(spacing));
        line2.push_str(&right);
        vec![line1, line2]
    }

    /// Assemble the full frame at (columns, rows).
    pub fn frame(&mut self, columns: usize, rows: usize) -> Vec<String> {
        let inner = columns.saturating_sub(GUTTER * 2);
        let mut lines = Vec::new();
        let _ = &self.status;
        for component in &mut self.transcript {
            lines.extend(component.render(inner));
        }
        // In-flight draft renders as a live message (streaming).
        if !self.assistant_draft.is_empty() {
            let mut draft =
                AssistantMessage::new(self.assistant_draft.clone(), Box::new(PlainHighlighter));
            lines.extend(tui_engine::Component::render(&mut draft, inner));
        }
        lines.extend(self.editor.render_box(inner, rows));
        lines.extend(self.footer(inner));
        // Apply the chrome gutter as a uniform left indent.
        let pad = " ".repeat(GUTTER);
        lines
            .into_iter()
            .map(|line| format!("{pad}{line}"))
            .collect()
    }

    /// Render one frame to the terminal.
    pub fn render(&mut self, out: &mut impl Write, columns: usize, rows: usize) {
        let frame = self.frame(columns, rows);
        let mut buffer: Vec<u8> = Vec::with_capacity(16 * 1024);
        self.screen.draw(&mut buffer, &frame, columns, rows);
        let _ = out.write_all(&buffer);
        let _ = out.flush();
    }
}

/// The bracketed mode badge for the footer.
pub fn mode_badge(mode: &str) -> String {
    let theme = theme::current();
    let (label, token) = match mode {
        "plan" => ("[Plan Mode]", Token::Primary),
        "auto" => ("[Auto Approve]", Token::Warning),
        _ => ("[Ask When Needed]", Token::Text),
    };
    theme.bold(token, label)
}

/// Shorten a cwd for the footer: home → `~`, keep the last `keep`
/// segments with a `…/` prefix when trimmed.
pub fn shorten_cwd(path: &std::path::Path, keep: usize) -> String {
    let text = path.to_string_lossy().to_string();
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"));
    let mut display = text.clone();
    if let Some(home) = home
        && !home.is_empty()
        && let Some(rest) = text.strip_prefix(&home.to_string_lossy().to_string())
    {
        display = format!("~{}", rest.replace('\\', "/"));
    }
    let segments: Vec<&str> = display
        .split(['/', '\\'])
        .filter(|segment| !segment.is_empty() && *segment != "~")
        .collect();
    if segments.len() <= keep {
        return display;
    }
    let tail = segments[segments.len() - keep..].join("/");
    format!("~/{tail}")
}

/// Permission-mode display label used in the welcome card.
pub fn permission_mode_label(mode: &str) -> &'static str {
    match mode {
        "plan" => "Plan Mode",
        "auto" => "Auto Approve",
        _ => "Ask When Needed",
    }
}

/// Run the console UI until exit.
///
/// Handles key events, bracketed pastes, resizes, and session events;
/// renders through the differential screen. Ctrl+C exits via the
/// double-press cascade (interrupting the turn first when busy).
pub async fn run(mut client: ActorClient, ctx: UiContext) -> anyhow::Result<()> {
    let mut guard =
        TerminalGuard::enter().map_err(|e| anyhow::anyhow!("terminal init failed: {e}"))?;
    let _ = guard.keyboard_enhanced();
    theme::set(theme::detect::resolve(None));

    let (mut columns, mut rows) = terminal::size().unwrap_or((80, 24));
    let mut ui = ConsoleUi::new(&ctx, env!("CARGO_PKG_VERSION"));
    ui.render(&mut std::io::stdout().lock(), columns, rows);

    let mut events = EventStream::new();
    let mut tick = tokio::time::interval(Duration::from_millis(100));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut flow = Flow::Continue;

    while flow == Flow::Continue {
        let stdout = std::io::stdout();
        // Drain queued submissions before waiting for the next event.
        for op in ui.take_ops() {
            let submission = Submission {
                id: Uuid::new_v4().to_string(),
                op,
            };
            if let Err(error) = client.submit(submission).await {
                ui.push_status(&format!("submission failed: {error}"), true);
            }
        }
        tokio::select! {
            maybe_event = events.next() => match maybe_event {
                Some(Ok(CEvent::Key(key))) => {
                    if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
                        flow = ui.handle_key(KeyEvent::from(key));
                        ui.render(&mut stdout.lock(), columns, rows);
                    }
                }
                Some(Ok(CEvent::Paste(text))) => {
                    ui.editor.insert_paste(&text);
                    ui.render(&mut stdout.lock(), columns, rows);
                }
                Some(Ok(CEvent::Resize(w, h))) => {
                    columns = w as usize;
                    rows = h as usize;
                    ui.screen.invalidate();
                    ui.render(&mut stdout.lock(), columns, rows);
                }
                Some(Ok(_)) => {}
                Some(Err(_)) => {}
                None => break,
            },
            event = client.next_event() => {
                match event {
                    Some(event) => {
                        ui.handle_wire_event(&event.msg);
                        ui.render(&mut stdout.lock(), columns, rows);
                    }
                    None => break,
                }
            }
            _ = tick.tick() => {
                // Animation frames (spinners) land with the phase work in
                // later phases; render only when the exit hint expires.
                ui.render(&mut stdout.lock(), columns, rows);
            }
        }
    }
    guard.leave();
    Ok(())
}

#[cfg(test)]
pub(crate) mod test_support {
    //! A no-op status seam for tests.
    use operations_actor::StatusQueries;

    /// All queries empty.
    pub struct NullStatus;

    impl StatusQueries for NullStatus {
        fn plan_status(&self) -> Option<String> {
            None
        }
        fn goal_status(&self) -> Option<String> {
            None
        }
        fn snapshot_labels(&self) -> Vec<String> {
            Vec::new()
        }
        fn snapshot_summary(&self, _label: &str) -> Option<String> {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme;
    use std::path::Path;
    use tui_engine::keys::Mods;
    use tui_engine::width::strip_ansi;

    fn ctx() -> UiContext {
        UiContext {
            model_name: "test-model".to_string(),
            cwd: PathBuf::from("/home/user/work/proj/sub"),
            permission_mode: "guarded".to_string(),
            skill_names: Vec::new(),
            mcp_servers: vec!["fs".to_string()],
            status: Arc::new(test_support::NullStatus),
        }
    }

    #[test]
    fn mode_badges_match_display_names() {
        theme::set(theme::Theme::dark());
        assert_eq!(strip_ansi(&mode_badge("plan")), "[Plan Mode]");
        assert_eq!(strip_ansi(&mode_badge("auto")), "[Auto Approve]");
        assert_eq!(strip_ansi(&mode_badge("guarded")), "[Ask When Needed]");
        assert_eq!(strip_ansi(&mode_badge("unknown")), "[Ask When Needed]");
        // Auto carries the warning color (bold amber).
        assert!(mode_badge("auto").contains("\x1b[38;2;232;168;56;1m"));
    }

    #[test]
    fn cwd_shortening_keeps_tail_segments() {
        unsafe { std::env::set_var("HOME", "/home/user") };
        assert_eq!(
            shorten_cwd(Path::new("/home/user/work/proj/sub"), 3),
            "~/work/proj/sub"
        );
        assert_eq!(
            shorten_cwd(Path::new("/home/user/work/proj/sub"), 2),
            "~/proj/sub"
        );
        assert_eq!(shorten_cwd(Path::new("/opt"), 3), "/opt");
        unsafe { std::env::set_var("HOME", "") };
    }

    #[test]
    fn footer_shows_context_meter() {
        theme::set(theme::Theme::dark());
        let mut ui = ConsoleUi::new(&ctx(), "0.1.0");
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

    #[test]
    fn submit_enqueues_and_renders_user_message() {
        theme::set(theme::Theme::dark());
        let mut ui = ConsoleUi::new(&ctx(), "0.1.0");
        ui.submit("hello");
        assert!(ui.state.busy(), "turn in flight");
        assert_eq!(ui.take_ops().len(), 1, "user_input queued on the outbox");
        let frame = ui.frame(80, 24);
        let joined: String = frame
            .iter()
            .map(|l| strip_ansi(l))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(joined.contains("hello"), "user text in frame: {joined}");
    }

    #[test]
    fn busy_submit_queues_message() {
        theme::set(theme::Theme::dark());
        let mut ui = ConsoleUi::new(&ctx(), "0.1.0");
        ui.submit("first");
        ui.submit("second");
        assert_eq!(ui.state.queued, vec!["second".to_string()]);
    }

    #[test]
    fn turn_completion_dequeues_next_message() {
        theme::set(theme::Theme::dark());
        let mut ui = ConsoleUi::new(&ctx(), "0.1.0");
        ui.submit("first");
        ui.submit("second");
        ui.handle_wire_event(&EventMsg::TurnCompleted { interrupted: false });
        assert!(ui.state.busy(), "queued message starts the next turn");
        assert!(ui.state.queued.is_empty());
    }

    #[test]
    fn ctrl_c_cascade_arms_then_exits() {
        theme::set(theme::Theme::dark());
        let mut ui = ConsoleUi::new(&ctx(), "0.1.0");
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
        theme::set(theme::Theme::dark());
        let mut ui = ConsoleUi::new(&ctx(), "0.1.0");
        ui.submit("go");
        assert_eq!(
            ui.handle_key(KeyEvent::new(Key::Char('c'), Mods::CTRL)),
            Flow::Continue
        );
        assert!(!ui.exit_armed(), "interrupt does not arm exit");
        assert!(matches!(ui.take_ops().last(), Some(Op::Interrupt)));
    }

    #[test]
    fn assistant_streaming_lands_as_transcript_message() {
        theme::set(theme::Theme::dark());
        let mut ui = ConsoleUi::new(&ctx(), "0.1.0");
        ui.handle_wire_event(&EventMsg::AgentMessageDelta {
            text: "hel".to_string(),
        });
        ui.handle_wire_event(&EventMsg::AgentMessageDelta {
            text: "lo".to_string(),
        });
        ui.handle_wire_event(&EventMsg::AgentMessageComplete {
            text: "hello".to_string(),
        });
        let frame = ui.frame(80, 24);
        let joined: String = frame
            .iter()
            .map(|l| strip_ansi(l))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(joined.contains("hello"), "assistant body: {joined}");
    }

    #[test]
    fn welcome_card_lists_mcp_servers() {
        theme::set(theme::Theme::dark());
        let mut ui = ConsoleUi::new(&ctx(), "0.1.0");
        let frame = ui.frame(80, 24);
        let joined: String = frame
            .iter()
            .map(|l| strip_ansi(l))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(joined.contains("WaveCode"), "brand: {joined}");
        assert!(joined.contains("1 servers"), "mcp count: {joined}");
    }
}
