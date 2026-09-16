//! The console UI orchestrator: owns the frame assembly, input
//! dispatch, and the session event pump.
//!
//! Frame layout (inline mode, native scrollback): transcript lines,
//! live thinking block, live assistant draft, activity pane, queue
//! pane, the editor box, then the two footer rows — all inside a
//! one-column gutter. Operations destined for the session are queued
//! on an outbox the run loop flushes; steering crosses the actor
//! inbox directly.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use crossterm::event::{Event as CEvent, EventStream, KeyEventKind};
use futures::StreamExt;
use operations_actor::{ActorClient, StatusQueries, SteerTarget, SubmitError};
use std::collections::HashMap;
use tui_engine::component::Component;
use tui_engine::editor::{Editor, EditorAction, EditorStyle};
use tui_engine::keys::{Key, KeyEvent};
use tui_engine::markdown::PlainHighlighter;
use tui_engine::screen::Screen;
use tui_engine::terminal::{self, TerminalGuard};
use uuid::Uuid;
use wavecode_wire::{Event, EventMsg, Op, Submission, WireDecision};

use crate::chrome::footer as footer_chrome;
use crate::chrome::{ActivityPane, TIP_ROTATE_INTERVAL, TransientHint};
use crate::complete::{ConsoleProvider, FileInventory};
use crate::controllers::StreamingController;
use crate::dialogs::{Answer, ApprovalDialog, Dialog, QuestionDialog};
use crate::history;
use crate::messages::tool_call::{ToolCall, ToolState};
use crate::messages::{AssistantMessage, ExpandedFlag, StatusLine, Thinking, UserMessage};
use crate::panes;
use crate::slash;
use crate::state::{AppState, StreamingPhase};
use crate::theme::{self, Token};
use crate::transcript::Transcript;
use crate::welcome::Welcome;

/// One-space chrome gutter: transcript, editor, and footer share it.
const GUTTER: usize = 1;
/// Double-press window for the Ctrl+C exit confirmation.
pub const EXIT_CONFIRM_WINDOW: Duration = Duration::from_millis(1500);

/// The session seam the UI drives. Implemented by [`ActorClient`] in
/// production; tests substitute recorded links.
#[async_trait]
pub trait SessionLink: Send + Sync {
    /// Submit one operation to the session.
    async fn submit(&self, submission: Submission) -> Result<(), SubmitError>;
    /// Await the next session event; `None` when the session ended.
    async fn next_event(&mut self) -> Option<Event>;
    /// Steer a running turn with user text.
    fn steer(&self, text: &str, target: SteerTarget) -> bool;
}

#[async_trait]
impl SessionLink for ActorClient {
    async fn submit(&self, submission: Submission) -> Result<(), SubmitError> {
        ActorClient::submit(self, submission).await
    }

    async fn next_event(&mut self) -> Option<Event> {
        ActorClient::next_event(self).await
    }

    fn steer(&self, text: &str, target: SteerTarget) -> bool {
        ActorClient::steer(self, text, target)
    }
}

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
    link: Box<dyn SessionLink>,
    editor: Editor,
    transcript: Transcript,
    screen: Screen,
    streaming: StreamingController,
    /// True when the current message arrived via deltas (its completion
    /// then carries no new text).
    streaming_flushed_assistant: bool,
    activity: ActivityPane,
    expanded: ExpandedFlag,
    /// Open tool call cards by call id (transcript entry index).
    open_calls: HashMap<String, usize>,
    /// Modal dialog (approvals, questions) when present.
    dialog: Option<Dialog>,
    /// History persistence path when a home directory is known.
    history_path: Option<PathBuf>,
    outbox: Vec<Op>,
    exit_armed_at: Option<Instant>,
    tip_index: usize,
    tip_rotated_at: Instant,
    version: String,
}

impl ConsoleUi {
    /// Build the UI; seeds the transcript with the welcome card.
    pub fn new(link: Box<dyn SessionLink>, ctx: &UiContext, version: impl Into<String>) -> Self {
        let state = AppState::new(
            ctx.model_name.clone(),
            ctx.cwd.clone(),
            ctx.permission_mode.clone(),
            ctx.mcp_servers.clone(),
        );
        let mut ui = Self {
            state,
            status: ctx.status.clone(),
            link,
            editor: {
                let mut editor = Editor::new(editor_style());
                let mut command_names: Vec<String> =
                    slash::COMMANDS.iter().map(|s| s.to_string()).collect();
                command_names.extend(ctx.skill_names.iter().cloned());
                editor.set_provider(Box::new(ConsoleProvider::new(
                    &command_names,
                    FileInventory::scan(&ctx.cwd),
                )));
                if let Some(path) = home_history_path() {
                    editor.load_history(history::load(&path));
                }
                editor
            },
            transcript: Transcript::new(),
            screen: Screen::new(),
            streaming: StreamingController::new(),
            streaming_flushed_assistant: false,
            activity: ActivityPane::new(),
            expanded: ExpandedFlag::new(),
            open_calls: HashMap::new(),
            dialog: None,
            history_path: home_history_path(),
            outbox: Vec::new(),
            exit_armed_at: None,
            tip_index: 0,
            tip_rotated_at: Instant::now(),
            version: version.into(),
        };
        let info = welcome_info_of(&ui.state, &ui.version);
        ui.transcript.push_new_turn(Box::new(Welcome::new(info)));
        ui
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
        self.activity.set_phase(StreamingPhase::Waiting);
    }

    /// Queue one operation for the run loop to submit.
    pub fn enqueue(&mut self, op: Op) {
        self.outbox.push(op);
    }

    /// Drain the pending operations into the session (run loop calls).
    pub async fn flush_outbox(&mut self) {
        for op in std::mem::take(&mut self.outbox) {
            let submission = Submission {
                id: Uuid::new_v4().to_string(),
                op,
            };
            if let Err(error) = self.link.submit(submission).await {
                self.push_status(&format!("submission failed: {error}"), true);
            }
        }
    }

    /// Await the next session event.
    pub async fn next_event(&mut self) -> Option<Event> {
        self.link.next_event().await
    }

    /// Steer the running turn with the queued message or editor text.
    /// Returns true when steering landed.
    pub fn steer(&mut self) -> bool {
        if !self.state.busy() {
            self.push_status("nothing is running to steer", false);
            return false;
        }
        let source = if let Some(queued) = self.state.queued.first().cloned() {
            Some((queued, true))
        } else if !self.editor.is_empty() {
            Some((self.editor.text(), false))
        } else {
            None
        };
        let Some((text, from_queue)) = source else {
            self.push_status("type a message or queue one to steer", false);
            return false;
        };
        let landed = self.link.steer(&text, SteerTarget::NextStep);
        if landed {
            if from_queue {
                self.state.queued.remove(0);
            } else {
                self.editor.clear();
            }
            self.push_status("steering the running turn", false);
        } else {
            self.push_status("steering is unavailable for this session", true);
        }
        landed
    }

    /// Push one user message.
    pub fn push_user_message(&mut self, text: &str) {
        self.transcript
            .push_new_turn(Box::new(UserMessage::new(text)));
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

    /// Fold the live thinking block into a finalized transcript entry.
    fn finalize_thinking(&mut self) {
        if !self.streaming.thinking.is_empty() {
            let text = std::mem::take(&mut self.streaming.thinking);
            self.transcript
                .push(Box::new(Thinking::finalized(text, self.expanded.clone())));
        }
    }

    /// Flush the streamed assistant draft into a transcript message.
    fn flush_assistant_draft(&mut self) {
        if !self.streaming.assistant.is_empty() {
            let draft = self.streaming.take_assistant();
            self.push_assistant_message(&draft);
        }
    }

    /// Session status queries (slash commands call these on demand).
    pub fn status(&self) -> &Arc<dyn StatusQueries> {
        &self.status
    }

    /// Pending outbox length (tests).
    pub fn take_outbox_len(&mut self) -> usize {
        self.outbox.len()
    }

    /// Borrow the pending outbox (tests).
    pub fn pending_ops(&self) -> &Vec<Op> {
        &self.outbox
    }

    /// Handle one wire event; returns true when the frame changed.
    pub fn handle_wire_event(&mut self, msg: &EventMsg) -> bool {
        match msg {
            EventMsg::TurnStarted => {
                self.state.phase = StreamingPhase::Waiting;
                self.activity.set_phase(StreamingPhase::Waiting);
                true
            }
            EventMsg::AgentThinkingDelta { text } => {
                self.streaming.push_thinking(text);
                self.state.phase = StreamingPhase::Thinking;
                self.activity.set_phase(StreamingPhase::Thinking);
                true
            }
            EventMsg::AgentMessageDelta { text } => {
                self.finalize_thinking();
                self.streaming.push_assistant(text);
                self.streaming_flushed_assistant = true;
                self.state.phase = StreamingPhase::Composing;
                self.activity.set_phase(StreamingPhase::Composing);
                true
            }
            EventMsg::AgentMessageComplete { text } => {
                self.finalize_thinking();
                // Deltas accumulated the full body; only fall back to the
                // completion text when nothing streamed.
                self.flush_assistant_draft();
                if !self.streaming_flushed_assistant && !text.is_empty() {
                    self.push_assistant_message(text);
                }
                self.streaming_flushed_assistant = false;
                true
            }
            EventMsg::ToolCallBegin {
                call_id,
                name,
                input,
            } => {
                self.finalize_thinking();
                self.flush_assistant_draft();
                self.state.phase = StreamingPhase::Tool;
                self.activity.set_phase(StreamingPhase::Tool);
                let card = ToolCall::running(name, input, self.expanded.clone());
                self.transcript.push(Box::new(card));
                let index = self.transcript.last_index();
                if let Some(index) = index {
                    self.open_calls.insert(call_id.clone(), index);
                }
                true
            }
            EventMsg::ToolCallEnd {
                call_id,
                is_error,
                output,
            } => {
                self.state.phase = StreamingPhase::Composing;
                self.activity.set_phase(StreamingPhase::Composing);
                if let Some(index) = self.open_calls.remove(call_id)
                    && let Some(entry) = self.transcript.get_mut(index)
                {
                    let state = if *is_error {
                        ToolState::Failed
                    } else {
                        ToolState::Done
                    };
                    if let Some(card) = as_tool_call(entry.component.as_mut()) {
                        let preview = output.as_ref().map(|preview| {
                            (
                                tui_engine::sanitize::sanitize_terminal(&preview.text).into_owned(),
                                preview.truncated,
                            )
                        });
                        card.finish(*is_error, preview);
                    }
                } else if *is_error {
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
                self.push_status("context compacted", false);
                let _ = summary_tokens;
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
                    self.activity.set_phase(StreamingPhase::Idle);
                }
                true
            }
            EventMsg::TurnCompleted { interrupted } => {
                self.finalize_thinking();
                self.flush_assistant_draft();
                self.streaming.clear();
                self.state.phase = StreamingPhase::Idle;
                self.activity.set_phase(StreamingPhase::Idle);
                if *interrupted {
                    self.push_status("interrupted", false);
                }
                // Trim old turns now that the frame is stable; no calls
                // span turns, so the open-call index resets with it.
                self.transcript.trim();
                self.open_calls.clear();
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
        // A modal dialog owns the keyboard while open.
        if self.dialog.is_some() {
            if event.is_ctrl_c() {
                self.dialog = None;
                self.push_status("dismissed", false);
                return Flow::Continue;
            }
            let answer = self.dialog.as_mut().and_then(|d| d.handle_key(event));
            if let Some(answer) = answer {
                match answer {
                    Answer::Approval { call_id, decision } => {
                        self.enqueue(Op::ExecApproval { call_id, decision });
                    }
                    Answer::Question { call_id, answer } => {
                        self.enqueue(Op::QuestionAnswer { call_id, answer });
                    }
                }
                self.dialog = None;
            }
            return Flow::Continue;
        }
        match self.editor.handle_key(event) {
            EditorAction::Submit(text) => {
                self.exit_armed_at = None;
                self.user_submit(&text)
            }
            EditorAction::Handled => Flow::Continue,
            EditorAction::Passthrough => self.handle_passthrough_key(event),
        }
    }

    /// Submit handling for editor text: slash dispatch first, then plain
    /// user input (which persists to history).
    fn user_submit(&mut self, text: &str) -> Flow {
        if let Some(invocation) = slash::parse(text) {
            return match slash::dispatch(&invocation, &self.state, self.status.as_ref()) {
                slash::Effect::Ops(ops) => {
                    if invocation.name == "theme" {
                        self.apply_theme(&invocation.args);
                    } else if invocation.name == "help" {
                        for line in slash::help_lines() {
                            self.push_status(&line, false);
                        }
                    } else if invocation.name == "model" && invocation.args.is_empty() {
                        self.push_status(&format!("model: {}", self.state.model_name), false);
                    } else if invocation.name == "permissions" && invocation.args.is_empty() {
                        self.push_status(
                            &format!("permission mode: {}", self.state.permission_mode),
                            false,
                        );
                    } else if invocation.name == "memory" {
                        let text = self
                            .status
                            .plan_status()
                            .unwrap_or_else(|| "no memory index available".to_string());
                        self.push_status(&text, false);
                    } else if invocation.name == "snapshots" {
                        let labels = self.status.snapshot_labels();
                        self.push_status(
                            &if labels.is_empty() {
                                "no snapshots".to_string()
                            } else {
                                format!("{}", labels.join(", "))
                            },
                            false,
                        );
                    } else if matches!(invocation.name.as_str(), "goal" | "status") {
                        let text = self
                            .status
                            .goal_status()
                            .unwrap_or_else(|| "no durable goal set".to_string());
                        self.push_status(&text, false);
                    }
                    for op in ops {
                        self.enqueue(op);
                    }
                    Flow::Continue
                }
                slash::Effect::Exit => Flow::Exit,
                slash::Effect::Fallthrough => {
                    self.send_user_input(text);
                    Flow::Continue
                }
            };
        }
        self.send_user_input(text);
        Flow::Continue
    }

    /// Apply a `/theme light|dark` switch locally.
    fn apply_theme(&mut self, args: &str) {
        match args.trim() {
            "light" => theme::set(theme::Theme::light()),
            "dark" => theme::set(theme::Theme::dark()),
            other => {
                self.push_status("usage: /theme light|dark", false);
                let _ = other;
                return;
            }
        }
        self.push_status(&format!("theme switched ({args})"), false);
    }

    /// Plain user input: persist history, render, and dispatch a turn.
    fn send_user_input(&mut self, text: &str) {
        if let Some(path) = &self.history_path {
            history::append(path, text);
        }
        self.submit(text);
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
            (Key::Char('o'), m) if m.ctrl => {
                self.expanded.toggle();
                Flow::Continue
            }
            (Key::Char('s'), m) if m.ctrl => {
                self.steer();
                Flow::Continue
            }
            (Key::Esc, _) if self.state.busy() => {
                self.enqueue(Op::Interrupt);
                Flow::Continue
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
        if self.tip_rotated_at.elapsed() >= TIP_ROTATE_INTERVAL {
            self.tip_index = self.tip_index.wrapping_add(1);
            self.tip_rotated_at = Instant::now();
        }
        let tip = footer_chrome::TIPS[self.tip_index % footer_chrome::TIPS.len()];
        let hint = if self.exit_armed() {
            TransientHint::ExitConfirm
        } else {
            TransientHint::None
        };
        vec![
            footer_chrome::row1(&self.state, Some(tip), columns),
            footer_chrome::row2(&self.state, &hint, columns),
        ]
    }

    /// Assemble the full frame at (columns, rows).
    pub fn frame(&mut self, columns: usize, rows: usize) -> Vec<String> {
        let inner = columns.saturating_sub(GUTTER * 2);
        let mut lines = Vec::new();
        lines.extend(self.transcript.render(inner));
        // Live thinking block (moves to the transcript when finalized).
        if !self.streaming.thinking.is_empty() {
            let mut block = Thinking::live(self.expanded.clone());
            block.push(&self.streaming.thinking.clone());
            lines.extend(Component::render(&mut block, inner));
        }
        // Live assistant draft.
        if !self.streaming.assistant.is_empty() {
            let mut draft =
                AssistantMessage::new(self.streaming.assistant.clone(), Box::new(PlainHighlighter));
            lines.extend(Component::render(&mut draft, inner));
        }
        lines.extend(self.activity.render(inner));
        lines.extend(panes::render_queue(&self.state.queued, inner));
        lines.extend(self.editor.render_box(inner, rows));
        lines.extend(self.footer(inner));
        let pad = " ".repeat(GUTTER);
        lines
            .into_iter()
            .map(|line| format!("{pad}{line}"))
            .collect()
    }

    /// True when an animation tick must repaint (busy phases animate).
    pub fn needs_tick_render(&self) -> bool {
        self.state.busy() || self.exit_armed()
    }

    /// Render one frame to the terminal.
    pub fn render(&mut self, out: &mut impl std::io::Write, columns: usize, rows: usize) {
        let frame = self.frame(columns, rows);
        let mut buffer: Vec<u8> = Vec::with_capacity(16 * 1024);
        self.screen.draw(&mut buffer, &frame, columns, rows);
        let _ = out.write_all(&buffer);
        let _ = out.flush();
    }
}

/// The themed editor style (rebuild on theme switches).
fn editor_style() -> EditorStyle {
    let theme = theme::current();
    EditorStyle {
        border: theme.style(Token::Border),
        prompt: theme.style(Token::TextDim),
        slash_command: theme.style(Token::Primary).bold(),
        hint: theme.style(Token::TextDim),
        paste_marker: theme.style(Token::TextDim),
        popup: tui_engine::select_list::SelectListStyle {
            selected: theme.style(Token::Primary),
            label: theme.style(Token::Text),
            description: theme.style(Token::TextDim),
        },
    }
}

/// The history file for the console surface, when HOME is known.
fn home_history_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"))?;
    if home.is_empty() {
        return None;
    }
    Some(history::history_path(Path::new(&home), "console"))
}

/// Downcast a transcript component to its tool card, updating state.
/// (Engine `Component` exposes an `as_any` seam for this.)
fn as_tool_call(component: &mut dyn tui_engine::Component) -> Option<&mut ToolCall> {
    component.as_any_mut().downcast_mut::<ToolCall>()
}

/// Welcome info derived from state (construction-order helper).
fn welcome_info_of(state: &AppState, version: &str) -> crate::welcome::WelcomeInfo {
    crate::welcome::WelcomeInfo {
        version: version.to_string(),
        model: state.model_name.clone(),
        mode: crate::ui::permission_mode_label(&state.permission_mode).to_string(),
        cwd: state.cwd.to_string_lossy().to_string(),
        mcp_servers: state.mcp_servers.clone(),
    }
}

/// Shorten a cwd for the footer: home → `~`, keep the last `keep`
/// segments with a `~/` prefix when trimmed.
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
pub async fn run(client: ActorClient, ctx: UiContext) -> anyhow::Result<()> {
    let mut guard =
        TerminalGuard::enter().map_err(|e| anyhow::anyhow!("terminal init failed: {e}"))?;
    let _ = guard.keyboard_enhanced();
    theme::set(theme::detect::resolve(None));

    let (mut columns, mut rows) = terminal::size().unwrap_or((80, 24));
    let mut ui = ConsoleUi::new(Box::new(client), &ctx, env!("CARGO_PKG_VERSION"));
    ui.render(&mut std::io::stdout().lock(), columns, rows);

    let mut events = EventStream::new();
    let mut tick = tokio::time::interval(Duration::from_millis(100));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut flow = Flow::Continue;

    while flow == Flow::Continue {
        let stdout = std::io::stdout();
        ui.flush_outbox().await;
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
            event = ui.next_event() => {
                match event {
                    Some(event) => {
                        ui.handle_wire_event(&event.msg);
                        ui.render(&mut stdout.lock(), columns, rows);
                    }
                    None => break,
                }
            }
            _ = tick.tick() => {
                // Spinner animation and streaming flush cadence.
                if ui.needs_tick_render() {
                    ui.render(&mut stdout.lock(), columns, rows);
                }
            }
        }
    }
    guard.leave();
    Ok(())
}

#[cfg(test)]
pub(crate) mod test_support {
    //! Test seams: a no-op status view and a recording session link.
    use super::*;

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

    /// Records submissions and steering; yields no events.
    #[derive(Default)]
    pub struct TestLink {
        pub submitted: std::sync::Mutex<Vec<Op>>,
        pub steered: std::sync::Mutex<Vec<(String, SteerTarget)>>,
    }

    impl TestLink {
        pub fn new() -> Self {
            Self::default()
        }
    }

    #[async_trait]
    impl SessionLink for TestLink {
        async fn submit(&self, submission: Submission) -> Result<(), SubmitError> {
            self.submitted
                .lock()
                .expect("test lock")
                .push(submission.op);
            Ok(())
        }

        async fn next_event(&mut self) -> Option<Event> {
            std::future::pending().await
        }

        fn steer(&self, text: &str, target: SteerTarget) -> bool {
            self.steered
                .lock()
                .expect("test lock")
                .push((text.to_string(), target));
            true
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::{self, Token};
    use std::path::Path;
    use test_support::{NullStatus, TestLink};
    use tui_engine::keys::Mods;
    use tui_engine::width::strip_ansi;

    fn ui() -> ConsoleUi {
        theme::set(theme::Theme::dark());
        ConsoleUi::new(
            Box::new(TestLink::new()),
            &UiContext {
                model_name: "test-model".to_string(),
                cwd: PathBuf::from("/home/user/work/proj/sub"),
                permission_mode: "guarded".to_string(),
                skill_names: Vec::new(),
                mcp_servers: vec!["fs".to_string()],
                status: Arc::new(NullStatus),
            },
            "0.1.0",
        )
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
        assert!(joined.contains("WaveCode"), "brand: {joined}");
        assert!(joined.contains("1 servers"), "mcp count: {joined}");
        assert!(joined.contains("Directory:"), "{joined}");
    }

    #[test]
    fn submit_enqueues_and_renders_user_message() {
        let mut ui = ui();
        ui.submit("hello");
        assert!(ui.state.busy(), "turn in flight");
        assert_eq!(ui.take_outbox_len(), 1, "user_input queued on the outbox");
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
        assert!(joined.contains("❯ second"), "queue pane: {joined}");
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
        assert!(joined.contains("thinking…"), "live header: {joined}");
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
}
