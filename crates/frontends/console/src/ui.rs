/*!
 * @file ConsoleUi
 * @description Terminal UI orchestrator driving layout assembly, input event loop, and session wire bridge.
 *
 * Responsibilities:
 * - Render inline frames using differential screen algorithms to preserve terminal scrollback.
 * - Map terminal key events, paste actions, and resizes to UI and session actions.
 * - Pump session events and display live thinking, tool execution cards, and assistant drafts.
 * - Safely recover terminal state on abnormal exits or BrokenPipe failures.
 *
 * This module must not depend on: LLM clients or tool implementations.
 * It reads local input history and scans the working directory for
 * completion candidates, but never executes tools or touches the network.
 */

//! The console UI orchestrator: owns the frame assembly, input
//! dispatch, and the session event pump.
//!
//! Frame layout (inline mode, native scrollback): transcript lines,
//! live thinking block, live assistant draft, queue
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
use tui_engine::screen::Screen;
use tui_engine::terminal::{self, TerminalGuard};
use uuid::Uuid;
use wavecode_wire::{Event, EventMsg, Op, Submission};

use crate::chrome::footer as footer_chrome;
use crate::chrome::notify;
use crate::chrome::title;
use crate::chrome::{TIP_ROTATE_INTERVAL, TransientHint, render_todos};
use crate::complete::{ConsoleProvider, FileInventory};
use crate::controllers::StreamingController;
use crate::controllers::btw::BtwJob;
use crate::controllers::shell::{ShellEvent, ShellJob};
use crate::dialogs::{Answer, ApprovalDialog, Dialog, ModelEntryView, QuestionDialog, SessionRow};
use crate::history;
use crate::messages::shell::ShellCard;
use crate::messages::compaction::CompactionCard;
use crate::messages::tool_call::ToolCall;
use crate::messages::usage::UsagePanel;
use crate::messages::{AssistantMessage, ExpandedFlag, StatusLine, Thinking, UserMessage};
use crate::panes;
use crate::slash;
use crate::state::{AppState, StreamingPhase, TodoEntry, TodoStatus};
use crate::theme::{self, Token};
use crate::transcript::Transcript;
use crate::welcome::Welcome;

/// One-space chrome gutter: transcript, editor, and footer share it.
const GUTTER: usize = 1;
/// Double-press window for the Ctrl+C exit confirmation.
pub const EXIT_CONFIRM_WINDOW: Duration = Duration::from_millis(1500);
/// Double-press window for Esc-Esc opening the rewind picker.
pub const DOUBLE_ESC_WINDOW: Duration = Duration::from_millis(600);

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
    /// Provider id the model samples through.
    pub provider_id: String,
    /// Current reasoning-effort level (`None` = unset/unsupported).
    pub thinking_effort: Option<String>,
    /// Thinking levels the picker offers; empty hides the row.
    pub thinking_levels: Vec<String>,
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
    /// Session id (journal identity; rotates on `/new`).
    pub session_id: String,
    /// Session title carried over a resume, when known.
    pub session_title: Option<String>,
    /// Model catalog for the `/model` picker (config `[models]` table
    /// plus the configured default, converted by the harness).
    pub model_entries: Vec<ModelEntryView>,
    /// Home directory for the session journal; `None` disables
    /// journaling, resume, fork, and title persistence.
    pub home: Option<PathBuf>,
}

/// What the UI asks the factory to launch: a fresh session (`history`
/// empty and no id), a resumed one (id + text history), or a read-only
/// side session (`/btw`).
pub struct LaunchSpec {
    /// Session id to resume; `None` starts a fresh session.
    pub session_id: Option<String>,
    /// Seed history as (from_model, text) pairs.
    pub history: Vec<(bool, String)>,
    /// True assembles a read-only side session (`/btw`): approvals and
    /// destructive work are impossible by mode, never by trust.
    pub readonly: bool,
    /// Model the launch should sample with (e.g. the live `/model`
    /// choice); `None` falls back to the launch-time defaults.
    pub model_override: Option<String>,
}

/// A launched session: live link plus the facts the UI keeps.
pub struct SessionLaunch {
    /// Live session link (actor client in production).
    pub link: Box<dyn SessionLink>,
    /// Session facts for the replacement UI state.
    pub ctx: UiContext,
    /// Seed history to replay into the transcript.
    pub history: Vec<(bool, String)>,
}

/// Assembles sessions on demand (resume, `/new`); implemented by the
/// harness so the UI never depends on the composition root.
pub type SessionFactory = dyn Fn(&LaunchSpec) -> Result<SessionLaunch, String> + Send + Sync;

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
    expanded: ExpandedFlag,
    /// Open tool call cards by call id (transcript entry index).
    open_calls: HashMap<String, usize>,
    /// Modal dialog (approvals, questions) when present.
    dialog: Option<Dialog>,
    /// Running local shell command (`!` mode), when present.
    shell: Option<ShellJob>,
    /// Transcript index of the live shell card.
    shell_card: Option<usize>,
    /// Transcript index of the live compaction card.
    compaction_card: Option<usize>,
    /// Context usage when the running compaction started.
    compaction_before: Option<u64>,
    /// True while the editor chrome is tinted for shell mode.
    shell_chrome: bool,
    /// History persistence path when a home directory is known.
    history_path: Option<PathBuf>,
    outbox: Vec<Op>,
    exit_armed_at: Option<Instant>,
    tip_index: usize,
    tip_rotated_at: Instant,
    /// Session start, the phase reference for editor animations.
    started_at: Instant,
    /// Shared UI preferences; `/settings` mutates them live.
    settings: crate::settings::SharedSettings,
    /// Terminal sequence queued by UI logic, flushed straight to stdout
    /// by the run loop (notifications, clipboard writes).
    pending_sequence: Option<String>,
    /// Window title last sent to the terminal (`None` = not yet sent).
    chrome_title: Option<String>,
    /// True while tab progress is being reported as running.
    chrome_progress_on: bool,
    /// Last time the busy progress was (re-)emitted.
    chrome_progress_at: Instant,
    /// Terminal focus state from focus-reporting events. Starts `false`
    /// so terminals without focus reporting keep the always-notify
    /// behavior; once a FocusGained arrives, turn-finished notifications
    /// pause while the user is looking at the terminal.
    terminal_focused: bool,
    /// Session launch requested by a dialog (`/new`, `/sessions`),
    /// drained by the run loop through the session factory.
    pending_launch: Option<LaunchSpec>,
    /// On-demand session factory (resume, `/new`); `None` in tests and
    /// when the harness cannot re-assemble.
    factory: Option<std::sync::Arc<SessionFactory>>,
    /// Model catalog for the `/model` picker.
    model_entries: Vec<ModelEntryView>,
    /// Thinking levels the picker offers (empty hides the row).
    thinking_levels: Vec<String>,
    /// Active `/btw` side session, when present.
    btw: Option<BtwJob>,
    /// `/btw` question/answer pairs (question first).
    btw_log: Vec<(bool, String)>,
    /// Streaming tail of the current btw answer.
    btw_buffer: String,
    /// True while the btw side turn is running.
    btw_running: bool,
    /// Pending Ctrl+G request: the draft handed to the external editor,
    /// drained by the run loop (raw-mode suspend happens there).
    pending_external_edit: Option<String>,
    /// Last idle Esc press, for the double-Esc rewind picker.
    last_esc_at: Option<Instant>,
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
                editor.set_provider(Box::new(
                    ConsoleProvider::new(
                        &command_names,
                        FileInventory::scan(&ctx.cwd),
                    )
                    .with_models(
                        ctx.model_entries
                            .iter()
                            .map(|entry| (entry.label.clone(), entry.provider.clone()))
                            .collect(),
                    )
                    .with_themes(
                        ctx.home
                            .as_deref()
                            .map(crate::theme::custom::list)
                            .unwrap_or_default(),
                    ),
                ));
                if let Some(path) = home_history_path() {
                    editor.load_history(history::load(&path));
                }
                editor
            },
            transcript: Transcript::new(),
            screen: Screen::new(),
            streaming: StreamingController::new(),
            streaming_flushed_assistant: false,
            expanded: ExpandedFlag::new(),
            open_calls: HashMap::new(),
            dialog: None,
            shell: None,
            shell_card: None,
            compaction_card: None,
            compaction_before: None,
            shell_chrome: false,
            history_path: home_history_path(),
            outbox: Vec::new(),
            exit_armed_at: None,
            tip_index: 0,
            tip_rotated_at: Instant::now(),
            started_at: Instant::now(),
            settings: crate::settings::SharedSettings::load(),
            pending_sequence: None,
            chrome_title: None,
            chrome_progress_on: false,
            chrome_progress_at: Instant::now(),
            terminal_focused: false,
            pending_launch: None,
            factory: None,
            model_entries: ctx.model_entries.clone(),
            thinking_levels: ctx.thinking_levels.clone(),
            btw: None,
            btw_log: Vec::new(),
            btw_buffer: String::new(),
            btw_running: false,
            pending_external_edit: None,
            last_esc_at: None,
            version: version.into(),
        };
        ui.state.provider_id = ctx.provider_id.clone();
        ui.state.thinking_effort = ctx.thinking_effort.clone();
        ui.state.session_id = ctx.session_id.clone();
        ui.state.session_title = ctx.session_title.clone();
        ui.state.home = ctx.home.clone();
        ui.state.git_branch = crate::gitinfo::branch(&ui.state.cwd);
        let info = welcome_info_of(&ui.state, &ui.version);
        ui.transcript.push_new_turn(Box::new(Welcome::new(info)));
        ui
    }

    /// Attach the session factory (run loop calls once after build).
    pub fn set_factory(&mut self, factory: std::sync::Arc<SessionFactory>) {
        self.factory = Some(factory);
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

    /// Run a local `!` shell command, rendering its output live in the
    /// transcript. Shell commands run client-side and never reach the
    /// session; only one runs at a time.
    fn run_shell(&mut self, command: &str) {
        let command = command.trim();
        if command.is_empty() {
            self.push_status("usage: !<command>", false);
            return;
        }
        if self.shell.is_some() {
            self.push_status("a shell command is already running (esc cancels it)", true);
            return;
        }
        match ShellJob::spawn(command) {
            Ok(job) => {
                let card = ShellCard::running(command, self.expanded.clone());
                self.transcript.push(Box::new(card));
                self.shell_card = self.transcript.last_index();
                self.shell = Some(job);
            }
            Err(error) => {
                self.push_status(&format!("shell command failed to start: {error}"), true)
            }
        }
    }

    /// Drain pending shell output into the live card; returns true when
    /// the frame changed (run loop tick calls this).
    pub fn poll_shell(&mut self) -> bool {
        let Some(job) = self.shell.as_mut() else {
            return false;
        };
        let mut events = Vec::new();
        while let Some(event) = job.try_recv() {
            events.push(event);
        }
        let mut done: Option<Option<i32>> = None;
        let mut changed = !events.is_empty();
        for event in events {
            match event {
                ShellEvent::Out(line) => self.push_shell_output(&line, false),
                ShellEvent::Err(line) => self.push_shell_output(&line, true),
                ShellEvent::Done(code) => done = Some(code),
            }
        }
        if let Some(code) = done {
            if let Some(card) = self.shell_card_mut() {
                card.finish(code);
            }
            self.shell = None;
            self.shell_card = None;
            changed = true;
        }
        changed
    }

    fn shell_card_mut(&mut self) -> Option<&mut ShellCard> {
        let index = self.shell_card?;
        let entry = self.transcript.get_mut(index)?;
        entry.component.as_any_mut().downcast_mut::<ShellCard>()
    }

    /// Borrow the live compaction card for in-place finalization.
    fn compaction_card_mut(&mut self) -> Option<&mut CompactionCard> {
        let index = self.compaction_card?;
        let entry = self.transcript.get_mut(index)?;
        entry
            .component
            .as_any_mut()
            .downcast_mut::<CompactionCard>()
    }

    /// True while a compaction card is still live (its pulse needs
    /// animation ticks).
    fn compaction_running(&self) -> bool {
        self.compaction_card.is_some()
    }

    fn push_shell_output(&mut self, line: &str, stderr: bool) {
        if let Some(card) = self.shell_card_mut() {
            card.push_output(line, stderr);
        }
    }

    /// Kill the running shell command; the card finalizes via the
    /// `Done(None)` event the job reports after its pipes drain.
    fn cancel_shell(&mut self) {
        if let Some(job) = &self.shell {
            job.cancel();
        }
        self.push_status("cancelling shell command", false);
    }

    /// Queue one operation for the run loop to submit.
    pub fn enqueue(&mut self, op: Op) {
        self.outbox.push(op);
    }

    /// Route a dialog answer onto the outbox.
    fn submit_answer(&mut self, answer: Option<Answer>) {
        match answer {
            Some(Answer::Dismissed) => {
                self.dialog = None;
            }
            Some(Answer::Approval { call_id, decision }) => {
                self.enqueue(Op::ExecApproval { call_id, decision });
            }
            Some(Answer::Question { call_id, answer }) => {
                self.enqueue(Op::QuestionAnswer { call_id, answer });
            }
            Some(Answer::ModelSelected {
                label,
                provider,
                model,
                effort,
                session_only,
            }) => {
                self.apply_model_selection(label, provider, model, effort, session_only);
            }
            Some(Answer::PermissionSelected { mode }) => {
                self.apply_permission_mode(&mode);
                self.enqueue(Op::SetPermissionMode { mode });
            }
            Some(Answer::ResumeSession { id }) => {
                self.request_resume(&id);
            }
            Some(Answer::RewindTurns { turns }) => {
                self.rewind_turns_command(&turns.to_string());
            }
            None => {}
        }
    }

    /// Apply a picker selection: live switch within the same provider,
    /// persistence for the default (Enter), and a restart hint when the
    /// provider differs (cross-provider needs re-assembly).
    fn apply_model_selection(
        &mut self,
        label: String,
        provider: String,
        model: String,
        effort: Option<String>,
        session_only: bool,
    ) {
        if label == self.state.model_name
            && provider == self.state.provider_id
            && effort == self.state.thinking_effort
        {
            self.push_status("already using this model", false);
            return;
        }
        let same_provider = provider == self.state.provider_id;
        if same_provider {
            self.state.model_name = label.clone();
            self.enqueue(Op::SetModel {
                name: model.clone(),
            });
            // Effort belongs to the selected model; without the live
            // model switch it must not leak onto the current one.
            if let Some(level) = &effort
                && self.state.thinking_effort.as_deref() != Some(level.as_str())
            {
                self.state.thinking_effort = Some(level.clone());
                self.enqueue(Op::SetThinking {
                    effort: level.clone(),
                });
            }
        }
        if session_only {
            if same_provider {
                self.push_status(
                    &format!("switched to model {label} (this session only)"),
                    false,
                );
            } else {
                // A different provider cannot go live mid-session, and
                // session-only would persist nothing; point at Enter.
                self.push_status(
                    "switching provider needs a restart — use Enter to save it as the default",
                    true,
                );
            }
            return;
        }
        // Persist the default so the next launch picks it up (the
        // harness applies settings.default_model before config).
        self.settings.update(|view| {
            view.default_model = Some(model);
            view.default_provider = Some(provider.clone());
            view.default_effort = effort;
        });
        let scope = if same_provider {
            format!("{label} as default")
        } else {
            format!("{label} ({provider}) as default; restart to apply")
        };
        self.push_status(&format!("saved {scope}"), false);
    }

    /// Queue a resume through the session factory.
    fn request_resume(&mut self, id: &str) {
        if self.factory.is_none() {
            self.push_status("resume is unavailable in this surface", true);
            return;
        }
        let Some(home) = self.state.home.clone() else {
            self.push_status("resume is unavailable without a home directory", true);
            return;
        };
        match state_persistence::sessions::load_session_history(&home, id) {
            Ok(history) => {
                self.pending_launch = Some(LaunchSpec {
                    session_id: Some(id.to_string()),
                    history,
                    readonly: false,
                    model_override: Some(self.state.model_name.clone()),
                });
            }
            Err(error) => self.push_status(&format!("resume failed: {error}"), true),
        }
    }

    /// Drain the pending operations into the session (run loop calls).
    ///
    /// Preserves unsent operations in `self.outbox` if a submission fails,
    /// ensuring state resilience and preventing silent loss of user operations.
    pub async fn flush_outbox(&mut self) -> Result<(), SubmitError> {
        let pending = std::mem::take(&mut self.outbox);
        for (i, op) in pending.iter().enumerate() {
            let submission = Submission {
                id: Uuid::new_v4().to_string(),
                op: op.clone(),
            };
            if let Err(error) = self.link.submit(submission).await {
                self.push_status(&format!("submission failed: {error}"), true);
                let mut unsent = pending[i..].to_vec();
                unsent.append(&mut self.outbox);
                self.outbox = unsent;
                return Err(error);
            }
        }
        Ok(())
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

    /// Push one user message (markdown per the user-render setting).
    pub fn push_user_message(&mut self, text: &str) {
        self.state.push_dialogue(true, text);
        let markdown = self.settings.get().render_user_markdown;
        self.transcript
            .push_new_turn(Box::new(UserMessage::new(text, markdown)));
    }

    /// Push one assistant markdown message.
    pub fn push_assistant_message(&mut self, text: &str) {
        self.state.push_dialogue(false, text);
        self.transcript.push(Box::new(AssistantMessage::new(
            text,
            crate::highlight::highlighter(),
        )));
    }

    /// Push one status line. Everything entering the transcript funnels
    /// through here or its siblings, so this is the sanitize chokepoint
    /// for wire-sourced strings (warnings, errors, tool names, notes).
    pub fn push_status(&mut self, text: &str, is_error: bool) {
        let text = tui_engine::sanitize::sanitize_terminal(text);
        if is_error {
            self.transcript
                .push(Box::new(StatusLine::error(text.as_ref())));
        } else {
            self.transcript
                .push(Box::new(StatusLine::new(text.as_ref())));
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
    pub fn outbox_len(&self) -> usize {
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
                // Cheap .git/HEAD read: catches checkout/branch switches.
                self.state.git_branch = crate::gitinfo::branch(&self.state.cwd);
                true
            }
            EventMsg::AgentThinkingDelta { text } => {
                let text = tui_engine::sanitize::sanitize_terminal(text);
                self.streaming.push_thinking(&text);
                self.state.phase = StreamingPhase::Thinking;
                true
            }
            EventMsg::AgentMessageDelta { text } => {
                let text = tui_engine::sanitize::sanitize_terminal(text);
                self.finalize_thinking();
                self.streaming.push_assistant(&text);
                self.streaming_flushed_assistant = true;
                self.state.phase = StreamingPhase::Composing;
                true
            }
            EventMsg::AgentMessageComplete { text } => {
                self.finalize_thinking();
                // Deltas accumulated the full body; only fall back to the
                // completion text when nothing streamed.
                self.flush_assistant_draft();
                if !self.streaming_flushed_assistant && !text.is_empty() {
                    let text = tui_engine::sanitize::sanitize_terminal(text);
                    self.push_assistant_message(&text);
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
                if name == "todowrite"
                    && let Some(todos) = parse_todos(input)
                {
                    // The todo list mirrors into the panel; the call card
                    // still renders its own success row.
                    self.state.todos = todos;
                }
                let card =
                    ToolCall::running(name, input, self.expanded.clone(), self.settings.clone());
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
                if let Some(index) = self.open_calls.remove(call_id)
                    && let Some(entry) = self.transcript.get_mut(index)
                {
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
                input_tokens,
                output_tokens,
                cache_read_tokens,
                cache_creation_tokens,
                context_used,
                context_window,
            } => {
                self.state.usage.input += input_tokens;
                self.state.usage.output += output_tokens;
                self.state.usage.cache_read += cache_read_tokens;
                self.state.usage.cache_creation += cache_creation_tokens;
                self.state.context_used = *context_used;
                self.state.context_window = *context_window;
                true
            }
            EventMsg::CompactStarted { trigger } => {
                let trigger = tui_engine::sanitize::sanitize_terminal(trigger);
                let card = CompactionCard::running(&trigger);
                self.transcript.push(Box::new(card));
                self.compaction_card = self.transcript.last_index();
                // The most recent sample is the "before" figure the
                // finished card reports.
                self.compaction_before = self.state.context_used;
                true
            }
            EventMsg::CompactCompleted { summary_tokens } => {
                let before = self.compaction_before;
                if let Some(card) = self.compaction_card_mut() {
                    card.finish(*summary_tokens, before);
                }
                self.compaction_card = None;
                self.compaction_before = None;
                true
            }
            EventMsg::HistoryRewound { turns } => {
                self.apply_rewound(*turns);
                true
            }
            EventMsg::PlanProposed { text } => {
                let text = tui_engine::sanitize::sanitize_terminal(text);
                self.push_assistant_message(&text);
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
                    // Same reasoning as TurnCompleted: the gates are
                    // gone, so a parked modal must not linger.
                    if matches!(
                        self.dialog,
                        Some(Dialog::Approval(_) | Dialog::Question(_))
                    ) {
                        self.dialog = None;
                        self.push_status("request dismissed (turn failed)", false);
                    }
                }
                // An idle compaction failure surfaces as a recoverable
                // error with no turn attached (manual `/compact`); with
                // no turn to settle the card, the error is the signal
                // the compaction died.
                if self.compaction_card.is_some() && !self.state.busy() {
                    if let Some(card) = self.compaction_card_mut() {
                        card.fail();
                    }
                    self.compaction_card = None;
                    self.compaction_before = None;
                }
                true
            }
            EventMsg::TurnCompleted { interrupted } => {
                self.finalize_thinking();
                self.flush_assistant_draft();
                self.streaming.clear();
                self.state.phase = StreamingPhase::Idle;
                // Parked gates died with the turn: a stale approval or
                // question modal could only answer into "late approval"
                // warnings, so it goes with the turn.
                if matches!(
                    self.dialog,
                    Some(Dialog::Approval(_) | Dialog::Question(_))
                ) {
                    self.dialog = None;
                    self.push_status("request dismissed (turn ended)", false);
                }
                if *interrupted {
                    self.push_status("interrupted", false);
                }
                // A compaction card still live at turn end means its
                // compaction died without emitting CompactCompleted
                // (in-turn auto/reactive/blocking failures): settle it,
                // or the pulse and the animation ticks would run forever.
                if self.compaction_card.is_some() {
                    if let Some(card) = self.compaction_card_mut() {
                        card.fail();
                    }
                    self.compaction_card = None;
                    self.compaction_before = None;
                }
                // Journal the completed turn (text snapshot) so
                // /sessions + resume can replay it later.
                self.journal_turn(*interrupted);
                // Desktop attention ping for finished turns, but only when
                // the user is not looking at the terminal (focus reporting
                // events track that); queued follow-ups keep the session
                // visibly active.
                let queued_next = !self.state.queued.is_empty();
                if !*interrupted && !queued_next && notify::enabled() && !self.terminal_focused {
                    // Composed (style + tmux passthrough), not the bare
                    // OSC 9: delivery must follow WAVECODE_NOTIFY_STYLE.
                    self.pending_sequence = Some(notify::notification("turn finished"));
                }
                // Trim old turns now that the frame is stable; no calls
                // span turns, so the open-call index resets with it.
                let dropped = self.transcript.trim();
                self.open_calls.clear();
                // Live-card indices shift with the dropped entries.
                if let Some(index) = self.shell_card.as_mut() {
                    *index = index.saturating_sub(dropped);
                }
                if let Some(index) = self.compaction_card.as_mut() {
                    *index = index.saturating_sub(dropped);
                }
                // Dequeue a queued message as the next turn.
                if queued_next {
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
                let detail = tui_engine::sanitize::sanitize_terminal(detail);
                self.dialog = Some(Dialog::Approval(ApprovalDialog::new(
                    call_id.clone(),
                    *kind,
                    &detail,
                )));
                true
            }
            EventMsg::QuestionRequested {
                call_id,
                question,
                options,
            } => {
                let question = tui_engine::sanitize::sanitize_terminal(question);
                let options: Vec<String> = options
                    .iter()
                    .map(|option| tui_engine::sanitize::sanitize_terminal(option).into_owned())
                    .collect();
                self.dialog = Some(Dialog::Question(QuestionDialog::new(
                    call_id.clone(),
                    &question,
                    options,
                )));
                true
            }
        }
    }

    /// Handle one key event; returns the flow decision.
    pub fn handle_key(&mut self, event: KeyEvent) -> Flow {
        // A modal dialog owns the keyboard while open. Ctrl+C dismisses
        // WITH the denial answer: dropping the dialog silently would
        // leave the session parked forever.
        if self.dialog.is_some() {
            if event.is_ctrl_c() {
                let answer = self.dialog.as_ref().map(Dialog::dismiss);
                self.dialog = None;
                self.submit_answer(answer);
                self.push_status("dismissed", false);
                return Flow::Continue;
            }
            let answer = self.dialog.as_mut().and_then(|d| d.handle_key(event));
            if let Some(answer) = answer {
                self.dialog = None;
                self.submit_answer(Some(answer));
            }
            return Flow::Continue;
        }
        // Shift+Tab cycles the permission mode globally:
        // plan → auto → wave → plan.
        if event.key == Key::Tab && event.mods.shift {
            let next = slash::cycle_mode(&self.state.permission_mode);
            self.apply_permission_mode(&next);
            self.enqueue(Op::SetPermissionMode { mode: next });
            return Flow::Continue;
        }
        match self.editor.handle_key(event) {
            EditorAction::Submit(text) => {
                self.exit_armed_at = None;
                let flow = self.user_submit(&text);
                self.refresh_editor_chrome();
                flow
            }
            EditorAction::Handled => {
                self.refresh_editor_chrome();
                Flow::Continue
            }
            EditorAction::Passthrough => self.handle_passthrough_key(event),
        }
    }

    /// Re-tint the editor border for the current buffer/mode state:
    /// shell violet with a `! shell mode` label while the buffer starts
    /// with `!`, otherwise the permission-mode color.
    fn refresh_editor_chrome(&mut self) {
        let shell = self.editor.text().starts_with('!');
        if shell == self.shell_chrome {
            return;
        }
        let theme = theme::current();
        if shell {
            self.editor.set_border_style(theme.style(Token::ShellMode));
            self.editor.set_label(Some("! shell mode".to_string()));
        } else {
            self.editor
                .set_border_style(mode_border_style(&self.state.permission_mode));
            self.editor.set_label(None);
        }
        self.shell_chrome = shell;
    }

    /// Apply a permission-mode change to local state and chrome (the
    /// caller enqueues the wire op; there is no mode-changed event).
    fn apply_permission_mode(&mut self, mode: &str) {
        self.state.permission_mode = mode.to_string();
        self.editor.set_border_style(mode_border_style(mode));
        self.push_status(&format!("permission mode: {mode}"), false);
    }

    /// Best-effort journal write for one completed turn: text snapshot
    /// of the dialogue under the current session id. No home or no
    /// session id (tests, headless surfaces) skips silently.
    fn journal_turn(&mut self, interrupted: bool) {
        let Some(home) = self.state.home.clone() else {
            return;
        };
        let session_id = self.state.session_id.clone();
        if session_id.is_empty() {
            return;
        }
        let input = self
            .state
            .dialogue
            .iter()
            .rev()
            .find(|entry| entry.from_user)
            .map(|entry| entry.text.clone())
            .unwrap_or_default();
        let history: Vec<(bool, String)> = self
            .state
            .dialogue
            .iter()
            // Dialogue flags users; the journal flags the model side.
            .map(|entry| (!entry.from_user, entry.text.clone()))
            .collect();
        let cwd = self.state.cwd.to_string_lossy().to_string();
        let outcome = if interrupted {
            "Interrupted"
        } else {
            "Completed"
        };
        let _ = state_persistence::sessions::record_turn(
            &home,
            &session_id,
            &cwd,
            &input,
            &history,
            outcome,
        );
    }

    /// Drain a pending session launch through the factory (run loop
    /// calls once per iteration). Returns true when a session was
    /// swapped in (the caller must repaint). Failures land as status
    /// lines.
    fn take_pending_launch(&mut self) -> bool {
        let Some(spec) = self.pending_launch.take() else {
            return false;
        };
        let Some(factory) = &self.factory else {
            return false;
        };
        match factory(&spec) {
            Ok(launch) => {
                self.replace_session(launch);
                true
            }
            Err(error) => {
                self.push_status(&format!("session launch failed: {error}"), true);
                false
            }
        }
    }

    /// Swap in a freshly launched session: replace link and state,
    /// replay the seed history, and start a clean transcript.
    fn replace_session(&mut self, launch: SessionLaunch) {
        let ctx = launch.ctx;
        self.link = launch.link;
        self.status = ctx.status.clone();
        let mut state = AppState::new(
            ctx.model_name.clone(),
            ctx.cwd.clone(),
            ctx.permission_mode.clone(),
            ctx.mcp_servers.clone(),
        );
        state.provider_id = ctx.provider_id.clone();
        state.thinking_effort = ctx.thinking_effort.clone();
        state.session_id = ctx.session_id.clone();
        state.session_title = ctx.session_title.clone();
        state.home = ctx.home.clone();
        state.git_branch = crate::gitinfo::branch(&state.cwd);
        self.state = state;
        self.model_entries = ctx.model_entries.clone();
        self.thinking_levels = ctx.thinking_levels.clone();
        self.outbox.clear();
        self.open_calls.clear();
        self.reset_transcript();
        for (from_model, text) in &launch.history {
            if *from_model {
                self.push_assistant_message(text);
            } else {
                self.push_user_message(text);
            }
        }
        self.screen.invalidate();
        self.push_status(
            &format!(
                "session {} ({})",
                if ctx.session_title.is_some() {
                    "restored"
                } else {
                    "started"
                },
                short_id(&self.state.session_id)
            ),
            false,
        );
    }

    /// Submit handling for editor text: `!` shell commands first, then
    /// slash dispatch, then plain user input (which persists to
    /// history).
    fn user_submit(&mut self, text: &str) -> Flow {
        if let Some(command) = text.strip_prefix('!') {
            if let Some(path) = &self.history_path {
                history::append(path, text);
            }
            self.run_shell(command);
            return Flow::Continue;
        }
        if let Some(invocation) = slash::parse(text) {
            return match slash::dispatch(&invocation, &self.state, self.status.as_ref()) {
                slash::Effect::Ops(ops) => {
                    if invocation.name == "clear" {
                        self.clear_screen();
                    } else if invocation.name == "new" {
                        self.start_new_session();
                    } else if invocation.name == "theme" {
                        self.apply_theme(&invocation.args);
                    } else if invocation.name == "copy" {
                        self.copy_last_assistant();
                    } else if invocation.name == "export" {
                        self.export_markdown(&invocation.args);
                    } else if invocation.name == "settings" {
                        let dialog = crate::dialogs::SettingsDialog::new(self.settings.clone());
                        self.dialog = Some(Dialog::Settings(dialog));
                    } else if invocation.name == "help" {
                        self.dialog = Some(Dialog::Help(crate::dialogs::HelpPanel::new(
                            slash::help_lines(),
                        )));
                    } else if invocation.name == "model" && invocation.args.is_empty() {
                        self.dialog = Some(Dialog::Model(crate::dialogs::ModelPickerDialog::new(
                            self.model_entries.clone(),
                            self.state.model_name.clone(),
                            self.state.thinking_effort.clone(),
                            self.thinking_levels.clone(),
                        )));
                    } else if invocation.name == "permissions" && invocation.args.is_empty() {
                        self.dialog = Some(Dialog::Permissions(
                            crate::dialogs::PermissionPickerDialog::new(
                                &self.state.permission_mode,
                            ),
                        ));
                    } else if matches!(invocation.name.as_str(), "sessions" | "resume")
                        && invocation.args.is_empty()
                    {
                        self.open_session_picker();
                    } else if invocation.name == "fork" {
                        self.fork_session();
                    } else if invocation.name == "title" {
                        self.set_session_title(&invocation.args);
                    } else if invocation.name == "undo" {
                        self.rewind_turns_command(&invocation.args);
                    } else if invocation.name == "editor" {
                        self.set_editor_command(&invocation.args);
                    } else if invocation.name == "init" {
                        self.init_project();
                    } else if invocation.name == "mcp" {
                        self.list_mcp_servers();
                    } else if invocation.name == "effort" && invocation.args.is_empty() {
                        match &self.state.thinking_effort {
                            Some(level) => self.push_status(&format!("thinking: {level}"), false),
                            None => self.push_status("thinking: off (unset)", false),
                        }
                    } else if invocation.name == "btw" {
                        self.handle_btw(&invocation.args);
                    } else if invocation.name == "usage" {
                        let panel = UsagePanel::new(
                            self.state.context_used,
                            self.state.context_window,
                            self.state.usage,
                        );
                        self.transcript.push(Box::new(panel));
                    } else if invocation.name == "version" {
                        self.push_status(&format!("WaveCode v{}", self.version), false);
                    } else if invocation.name == "model" {
                        // Non-empty args already dispatched to Op::SetModel.
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
                                labels.join(", ")
                            },
                            false,
                        );
                    } else if invocation.name == "goal" {
                        let text = self
                            .status
                            .goal_status()
                            .unwrap_or_else(|| "no durable goal set".to_string());
                        self.push_status(&text, false);
                    } else if invocation.name == "status" {
                        self.push_session_status();
                    }
                    for op in ops {
                        // Mode/model changes have no wire echo; the
                        // requesting UI is the source of truth for its
                        // own chrome.
                        match &op {
                            Op::SetPermissionMode { mode } => {
                                self.apply_permission_mode(mode);
                            }
                            Op::SetModel { name } => {
                                self.state.model_name = name.clone();
                                self.push_status(&format!("model: {name}"), false);
                            }
                            Op::SetThinking { effort } => {
                                let level = if effort.eq_ignore_ascii_case("off") {
                                    None
                                } else {
                                    Some(effort.clone())
                                };
                                self.state.thinking_effort = level;
                                self.push_status(&format!("thinking: {effort}"), false);
                            }
                            _ => {}
                        }
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

    /// Soft clear: transcript and dialogue reset, context (the actor's
    /// conversation) is kept.
    fn clear_screen(&mut self) {
        if self.state.busy() {
            self.push_status(
                "cannot clear screen while a turn is running (press Esc to interrupt)",
                false,
            );
        } else if self.shell.is_some() {
            self.push_status(
                "cannot clear screen while a shell command is running (press Esc to cancel)",
                false,
            );
        } else {
            self.reset_transcript();
        }
    }

    /// Reset transcript-side state; keeps the link, usage, and session
    /// identity (a fresh welcome card lands on top).
    fn reset_transcript(&mut self) {
        self.transcript.clear();
        self.streaming.clear();
        self.open_calls.clear();
        self.shell_card = None;
        self.compaction_card = None;
        self.compaction_before = None;
        self.state.dialogue.clear();
        let info = welcome_info_of(&self.state, &self.version);
        self.transcript.push_new_turn(Box::new(Welcome::new(info)));
        self.screen.invalidate();
    }

    /// `/new`: rotate to a fresh session through the factory (new
    /// context and journal); without a factory, degrade to a soft clear.
    fn start_new_session(&mut self) {
        if self.state.busy() {
            self.push_status("cannot start a new session while a turn runs", false);
            return;
        }
        if self.factory.is_some() {
            self.pending_launch = Some(LaunchSpec {
                session_id: None,
                history: Vec::new(),
                readonly: false,
                model_override: None,
            });
        } else {
            self.reset_transcript();
            self.push_status("screen cleared (no session factory; context kept)", false);
        }
    }

    /// `/sessions`: list recorded sessions in a picker.
    fn open_session_picker(&mut self) {
        let Some(home) = self.state.home.clone() else {
            self.push_status("sessions are unavailable without a home directory", true);
            return;
        };
        let metas = state_persistence::sessions::list_sessions(&home);
        let rows: Vec<SessionRow> = metas
            .into_iter()
            .map(|meta| SessionRow {
                title: if meta.title.is_empty() {
                    meta.id.clone()
                } else {
                    meta.title
                },
                id: meta.id,
                cwd: meta.cwd,
                age: relative_age(meta.updated_at),
                turns: meta.turns,
            })
            .collect();
        if rows.is_empty() {
            self.push_status("no recorded sessions yet", false);
            return;
        }
        let current_cwd = self.state.cwd.to_string_lossy().to_string();
        self.dialog = Some(Dialog::Sessions(crate::dialogs::SessionPickerDialog::new(
            rows,
            &current_cwd,
        )));
    }

    /// `/fork`: snapshot the dialogue into a resumable copy and stay in
    /// this session (fork tools and context carry over as text).
    fn fork_session(&mut self) {
        let Some(home) = self.state.home.clone() else {
            self.push_status("forking is unavailable without a home directory", true);
            return;
        };
        if self.state.dialogue.is_empty() {
            self.push_status("nothing to fork yet — send a message first", false);
            return;
        }
        if self.state.busy() {
            self.push_status("cannot fork while a turn is running", false);
            return;
        }
        let history: Vec<(bool, String)> = self
            .state
            .dialogue
            .iter()
            // Dialogue flags users; the journal flags the model side.
            .map(|entry| (!entry.from_user, entry.text.clone()))
            .collect();
        let source_title = self
            .state
            .session_title
            .clone()
            .unwrap_or_else(|| self.state.session_id.clone());
        let fork_id = Uuid::new_v4().to_string();
        let cwd = self.state.cwd.to_string_lossy().to_string();
        match state_persistence::sessions::fork_session(
            &home,
            &fork_id,
            &format!("Fork: {source_title}"),
            &cwd,
            &history,
        ) {
            Ok(meta) => {
                self.push_status(
                    &format!(
                        "session forked ({}). still in the original session;",
                        short_id(&meta.id)
                    ),
                    false,
                );
                self.push_status(
                    &format!("open the fork with: wavecode --session {}", meta.id),
                    false,
                );
            }
            Err(error) => self.push_status(&format!("fork failed: {error}"), true),
        }
    }

    /// `/title <title>`: rename the session; no args shows the current.
    fn set_session_title(&mut self, args: &str) {
        let title = args.trim();
        if title.is_empty() {
            match &self.state.session_title {
                Some(current) => {
                    self.push_status(
                        &format!("session: {current} ({})", short_id(&self.state.session_id)),
                        false,
                    );
                }
                None => self.push_status(
                    &format!(
                        "no title set — session {}",
                        short_id(&self.state.session_id)
                    ),
                    false,
                ),
            }
            return;
        }
        let Some(home) = self.state.home.clone() else {
            self.push_status("titles are unavailable without a home directory", true);
            return;
        };
        match state_persistence::sessions::set_title(&home, &self.state.session_id, title) {
            Ok(Some(_)) => {
                self.state.session_title = Some(title.to_string());
                self.push_status(&format!("session renamed: {title}"), false);
            }
            Ok(None) => {
                self.push_status("usage: /title <title>", false);
            }
            Err(error) => self.push_status(&format!("rename failed: {error}"), true),
        }
    }

    /// Double-Esc: list recent turns for a conversation rewind.
    fn open_undo_picker(&mut self) {
        let picker = crate::dialogs::UndoPickerDialog::new(&self.state.dialogue);
        if picker.is_empty() {
            self.push_status("nothing to rewind yet", false);
            return;
        }
        self.dialog = Some(Dialog::Undo(picker));
    }

    /// `/undo [n]`: drop the last n conversation turns (default 1).
    /// The agent stops seeing the dropped turns; file changes they
    /// already made stay. Idle-only, and the landed event trims the
    /// transcript (`apply_rewound`).
    fn rewind_turns_command(&mut self, args: &str) {
        let trimmed = args.trim();
        let turns = if trimmed.is_empty() {
            1
        } else {
            match trimmed.parse::<u32>() {
                Ok(turns) => turns,
                Err(_) => {
                    self.push_status("usage: /undo [n] — drop the last n turns (default 1)", true);
                    return;
                }
            }
        };
        if self.state.busy() {
            self.push_status(
                "cannot rewind while a turn is running (press Esc to interrupt)",
                false,
            );
            return;
        }
        if self.shell.is_some() {
            self.push_status(
                "cannot rewind while a shell command is running (press Esc to cancel)",
                false,
            );
            return;
        }
        self.enqueue(Op::Rewind { turns });
    }

    /// Apply a landed rewind: trim dialogue and transcript, journal the
    /// truncated snapshot, and state what the rewind does not do.
    fn apply_rewound(&mut self, turns: u32) {
        if turns == 0 {
            self.push_status("nothing to rewind", false);
            return;
        }
        let removed = self.state.rewind_dialogue(turns);
        if removed == 0 {
            // The actor dropped turns this view never held (a session
            // resumed by CLI flag seeds the conversation without
            // replaying it here). Journaling the untouched dialogue
            // would append a snapshot with less history than the
            // newest record already has, so the journal stands.
            self.push_status("rewound on the session; nothing to trim here", false);
            return;
        }
        let dropped_entries = self.transcript.rewind_turns(removed);
        // Dropped tool cards free their open-call indices.
        self.open_calls.clear();
        self.journal_rewind(removed as u32);
        self.push_status(
            &format!(
                "rewound {removed} turn(s) ({dropped_entries} transcript entries); \
                 file changes already made are not undone"
            ),
            false,
        );
    }

    /// Append the truncated dialogue as the newest journal snapshot so
    /// a later resume replays the rewound conversation.
    fn journal_rewind(&mut self, turns_removed: u32) {
        let Some(home) = self.state.home.clone() else {
            return;
        };
        let history: Vec<(bool, String)> = self
            .state
            .dialogue
            .iter()
            // Dialogue flags users; the journal flags the model side.
            .map(|entry| (!entry.from_user, entry.text.clone()))
            .collect();
        if let Err(error) = state_persistence::sessions::record_rewind(
            &home,
            &self.state.session_id,
            &self.state.cwd.to_string_lossy(),
            &history,
            turns_removed,
        ) {
            self.push_status(&format!("rewind journaling failed: {error}"), true);
        }
    }

    /// `/editor <cmd>`: set the external editor for Ctrl+G; no args
    /// shows the current resolution.
    fn set_editor_command(&mut self, args: &str) {
        let command = args.trim();
        if command.is_empty() {
            match self.resolve_editor_command() {
                Some(current) => self.push_status(&format!("editor: {current}"), false),
                None => self.push_status("no editor configured — usage: /editor <cmd>", false),
            }
            return;
        }
        self.settings
            .update(|view| view.editor_command = Some(command.to_string()));
        self.push_status(&format!("editor set: {command} (ctrl+g to use)"), false);
    }

    /// Ctrl+G: hand the current draft to the external editor (the run
    /// loop performs the raw-mode suspend/resume round trip).
    fn request_external_edit(&mut self) {
        if self.state.busy() {
            self.push_status("cannot open the editor while a turn is running", false);
            return;
        }
        if self.pending_external_edit.is_none() {
            self.pending_external_edit = Some(self.editor.text());
        }
    }

    /// Take the pending external-edit request: the draft to edit.
    fn take_external_edit(&mut self) -> Option<String> {
        self.pending_external_edit.take()
    }

    /// Load the externally edited text into the draft buffer.
    fn apply_external_edit(&mut self, text: &str) {
        self.editor.set_text(text);
        self.push_status("draft loaded from the external editor", false);
    }

    /// The external editor command: the `/editor` setting wins, then
    /// `$VISUAL`, then `$EDITOR`.
    fn resolve_editor_command(&self) -> Option<String> {
        let visual = std::env::var("VISUAL").ok();
        let editor = std::env::var("EDITOR").ok();
        pick_editor_command(
            self.settings.get().editor_command.as_deref(),
            visual.as_deref(),
            editor.as_deref(),
        )
        .map(str::to_string)
    }

    /// Hand the draft to the external editor: leave raw mode, run the
    /// command on a temp file, restore the terminal, load the result.
    /// Only a terminal-restore failure is fatal (`Err`); editor
    /// failures keep the original draft and surface as status lines.
    pub async fn run_external_editor(&mut self, guard: &mut TerminalGuard) -> anyhow::Result<()> {
        let Some(draft) = self.take_external_edit() else {
            return Ok(());
        };
        let Some(command) = self.resolve_editor_command() else {
            self.editor.set_text(&draft);
            self.push_status("no editor configured — set $EDITOR or /editor <cmd>", true);
            return Ok(());
        };
        let temp = std::env::temp_dir().join(format!("wavecode-edit-{}.md", Uuid::new_v4()));
        if let Err(error) = std::fs::write(&temp, &draft) {
            self.editor.set_text(&draft);
            self.push_status(&format!("editor temp file failed: {error}"), true);
            return Ok(());
        }
        guard.leave();
        let outcome = edit_with_command(&command, &temp).await;
        *guard = TerminalGuard::enter()
            .map_err(|e| anyhow::anyhow!("terminal restore failed: {e}"))?;
        let _ = guard.keyboard_enhanced();
        let _ = std::fs::remove_file(&temp);
        match outcome {
            Ok(text) if text.is_empty() => {
                self.editor.set_text(&draft);
                self.push_status("editor saved nothing; draft kept", false);
            }
            Ok(text) => self.apply_external_edit(&text),
            Err(error) => {
                self.editor.set_text(&draft);
                self.push_status(&format!("editor failed: {error}"), true);
            }
        }
        self.screen.invalidate();
        Ok(())
    }

    /// `/init`: send a fixed analysis prompt so the agent writes
    /// AGENTS.md for this repository.
    fn init_project(&mut self) {
        const INIT_PROMPT: &str = "Analyze this repository and create (or update) an AGENTS.md \
file at the repository root. Cover: the project's purpose in one or two sentences, the \
directory layout a newcomer needs, build/test/lint commands that actually work here, and \
any conventions the code follows. Keep it under 60 lines and state only what you can \
verify from the repository.";
        self.push_status("initializing: the agent will write AGENTS.md", false);
        self.send_user_input(INIT_PROMPT);
    }

    /// `/btw [question]`: ask a side question in a read-only session
    /// seeded with the main dialogue; the answer streams into a panel
    /// and never enters the main conversation. An open panel asks
    /// follow-ups on the same side session.
    fn handle_btw(&mut self, args: &str) {
        let question = args.trim();
        if question.is_empty() {
            self.push_status("usage: /btw <question> (esc closes the panel)", false);
            return;
        }
        if let Some(job) = &self.btw {
            job.ask(question);
            self.btw_log.push((true, question.to_string()));
            self.btw_running = true;
            return;
        }
        if self.factory.is_none() {
            self.push_status("side questions are unavailable in this surface", true);
            return;
        }
        let history: Vec<(bool, String)> = self
            .state
            .dialogue
            .iter()
            .map(|entry| (!entry.from_user, entry.text.clone()))
            .collect();
        let spec = LaunchSpec {
            session_id: None,
            history,
            readonly: true,
            model_override: Some(self.state.model_name.clone()),
        };
        match self.factory.as_ref().map(|factory| factory(&spec)) {
            Some(Ok(launch)) => {
                self.btw_log = vec![(true, question.to_string())];
                self.btw_buffer.clear();
                self.btw_running = true;
                self.btw = Some(BtwJob::spawn(launch.link, question));
            }
            Some(Err(error)) => self.push_status(&format!("side session failed: {error}"), true),
            None => self.push_status("side questions are unavailable in this surface", true),
        }
    }

    /// Close the btw panel: cancels a running answer and shuts the side
    /// session down (the pump drains in the background).
    fn close_btw(&mut self) {
        if let Some(job) = &self.btw {
            job.cancel();
        }
        self.btw = None;
        self.btw_log.clear();
        self.btw_buffer.clear();
        self.btw_running = false;
    }

    /// Drain the btw pump into the panel state; returns true when the
    /// frame changed (the run-loop tick calls this, like the shell).
    pub fn poll_btw(&mut self) -> bool {
        let Some(job) = self.btw.as_mut() else {
            return false;
        };
        let mut changed = false;
        let mut ended = false;
        while let Some(event) = job.try_recv() {
            match event {
                crate::controllers::btw::BtwEvent::Delta(text) => {
                    let text = tui_engine::sanitize::sanitize_terminal(&text);
                    self.btw_buffer.push_str(&text);
                    changed = true;
                }
                crate::controllers::btw::BtwEvent::Done { text, interrupted } => {
                    if !text.is_empty() {
                        let text = tui_engine::sanitize::sanitize_terminal(&text);
                        self.btw_log.push((false, text.into_owned()));
                    } else if interrupted {
                        self.btw_log.push((false, "(cancelled)".to_string()));
                    }
                    self.btw_buffer.clear();
                    self.btw_running = false;
                    changed = true;
                }
                crate::controllers::btw::BtwEvent::Ended => {
                    ended = true;
                    changed = true;
                }
            }
        }
        if ended {
            // The side session is gone; drop the job so the next /btw
            // relaunches instead of asking a dead pump (which would
            // strand the panel in streaming forever). The log stays
            // visible until the panel is closed or reopened.
            self.btw = None;
            self.btw_running = false;
            self.btw_buffer.clear();
        }
        changed
    }

    /// True while the btw panel occupies the frame: a live job, or the
    /// leftover log of a side session that already ended.
    fn btw_open(&self) -> bool {
        self.btw.is_some() || !self.btw_log.is_empty()
    }

    /// `/mcp`: list configured MCP servers.
    fn list_mcp_servers(&mut self) {
        if self.state.mcp_servers.is_empty() {
            self.push_status("(no MCP servers configured)", false);
            return;
        }
        for server in self.state.mcp_servers.clone() {
            self.push_status(&format!("- {server}"), false);
        }
    }

    /// `/status`: the aggregate session summary.
    fn push_session_status(&mut self) {
        let state = self.state.clone();
        let model_line = match (&state.thinking_effort, state.provider_id.as_str()) {
            (Some(level), provider) if !provider.is_empty() => {
                format!(
                    "model: {} @ {provider} · thinking: {level}",
                    state.model_name
                )
            }
            (Some(level), _) => format!("model: {} · thinking: {level}", state.model_name),
            (None, provider) if !provider.is_empty() => {
                format!("model: {} @ {provider}", state.model_name)
            }
            (None, _) => format!("model: {}", state.model_name),
        };
        let session_line = match &state.session_title {
            Some(title) => {
                format!("session: {title} ({})", short_id(&state.session_id))
            }
            None => format!("session: {}", short_id(&state.session_id)),
        };
        let context_line = match (state.context_used, state.context_window) {
            (Some(used), Some(window)) => format!(
                "context: {}% ({}/{})",
                crate::state::context_percent(used, window),
                crate::state::format_tokens(used),
                crate::state::format_tokens(window)
            ),
            _ => "context: n/a".to_string(),
        };
        let usage_line = format!(
            "tokens: {} in / {} out / {} total",
            crate::state::format_tokens(state.usage.input),
            crate::state::format_tokens(state.usage.output),
            crate::state::format_tokens(state.usage.total())
        );
        for line in [
            session_line,
            model_line,
            format!("mode: {}", permission_mode_label(&state.permission_mode)),
            format!("cwd: {}", state.cwd.display()),
        ]
        .into_iter()
        .chain(
            state
                .git_branch
                .iter()
                .map(|branch| format!("branch: {branch}")),
        )
        .chain([context_line, usage_line])
        .chain([format!(
            "mcp: {} server(s) · WaveCode v{}",
            state.mcp_servers.len(),
            self.version
        )]) {
            self.push_status(&line, false);
        }
    }

    /// Apply a `/theme light|dark|auto` switch locally.
    fn apply_theme(&mut self, args: &str) {
        let name = args.trim();
        match name {
            "light" => theme::set(theme::Theme::light()),
            "dark" => theme::set(theme::Theme::dark()),
            // Re-query the terminal background (OSC 11) and pick dark
            // or light from the answer.
            "auto" => theme::set(theme::detect::resolve(None)),
            "" => {
                let custom = self
                    .state
                    .home
                    .as_deref()
                    .map(theme::custom::list)
                    .unwrap_or_default();
                if custom.is_empty() {
                    self.push_status("usage: /theme light|dark|auto (or a custom theme name)", false);
                } else {
                    self.push_status(
                        &format!("usage: /theme light|dark|auto|<{}>", custom.join("|")),
                        false,
                    );
                }
                return;
            }
            custom => {
                // Everything else names a file in ~/.wavecode/themes/.
                let Some(home) = self.state.home.clone() else {
                    self.push_status("custom themes need a home directory", true);
                    return;
                };
                match theme::custom::load(&home, custom) {
                    Ok(resolved) => theme::set(resolved),
                    Err(error) => {
                        self.push_status(&format!("theme {custom:?} failed: {error}"), true);
                        return;
                    }
                }
            }
        }
        // Chrome built at construction carries colors by value: the
        // editor (and its popup) must be rebuilt for the new palette.
        self.editor.set_style(editor_style());
        self.screen.invalidate();
        self.push_status(&format!("theme switched ({name})"), false);
    }

    /// `/copy`: put the last assistant message on the clipboard via
    /// OSC 52 (the terminal owns the system clipboard).
    fn copy_last_assistant(&mut self) {
        use base64::Engine as _;
        match self.state.last_assistant() {
            Some(text) => {
                let encoded = base64::engine::general_purpose::STANDARD.encode(text.as_bytes());
                let seq = format!("\x1b]52;c;{encoded}\x07");
                self.pending_sequence = Some(seq);
                let preview: String = text.chars().take(40).collect();
                self.push_status(&format!("copied to clipboard: {preview}…"), false);
            }
            None => self.push_status("no assistant message to copy", false),
        }
    }

    /// `/export [path]`: write the full user/assistant dialogue to a
    /// markdown file (cwd by default, timestamped name).
    fn export_markdown(&mut self, args: &str) {
        let target = match args.trim() {
            "" => {
                let stamp = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                self.state.cwd.join(format!("wavecode-export-{stamp}.md"))
            }
            arg => PathBuf::from(arg),
        };
        let markdown = self.state.export_markdown();
        match std::fs::write(&target, markdown) {
            Ok(()) => self.push_status(
                &format!(
                    "exported {} messages to {}",
                    self.state.dialogue.len(),
                    target.display()
                ),
                false,
            ),
            Err(error) => {
                self.push_status(&format!("export failed: {error}"), true);
            }
        }
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
            (Key::Esc, _) if self.shell.is_some() => {
                self.cancel_shell();
                Flow::Continue
            }
            (Key::Esc, _) if self.btw_open() => {
                self.close_btw();
                Flow::Continue
            }
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
            (Key::Char('t'), m) if m.ctrl => {
                self.state.todo_expanded = !self.state.todo_expanded;
                Flow::Continue
            }
            (Key::Char('s'), m) if m.ctrl => {
                self.steer();
                Flow::Continue
            }
            (Key::Char('g'), m) if m.ctrl => {
                self.request_external_edit();
                Flow::Continue
            }
            (Key::Esc, _) if self.state.busy() => {
                self.enqueue(Op::Interrupt);
                self.push_status("interrupting the turn", false);
                Flow::Continue
            }
            (Key::Esc, _) if self.shell_chrome && self.editor.text() == "!" => {
                // Esc on an empty shell-mode prompt exits the mode: the
                // `!` is the buffer itself, so clearing it restores the
                // normal prompt.
                self.editor.clear();
                self.refresh_editor_chrome();
                Flow::Continue
            }
            (Key::Esc, _) => {
                // Double-Esc opens the rewind picker; a lone Esc stays
                // a no-op.
                if self.last_esc_at.is_some_and(|at| at.elapsed() <= DOUBLE_ESC_WINDOW) {
                    self.last_esc_at = None;
                    self.open_undo_picker();
                } else {
                    self.last_esc_at = Some(Instant::now());
                }
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
        // Ctrl+C cancels the running shell command before anything else.
        if self.shell.is_some() {
            self.cancel_shell();
            return Flow::Continue;
        }
        if self.exit_armed() {
            return Flow::Exit;
        }
        if self.state.busy() {
            self.enqueue(Op::Interrupt);
            self.push_status("interrupting the turn", false);
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
        self.update_chrome();
        self.animate_editor_prompt();
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
            let mut draft = AssistantMessage::streaming(
                self.streaming.assistant.clone(),
                crate::highlight::highlighter(),
            );
            lines.extend(Component::render(&mut draft, inner));
        }
        lines.extend(render_todos(
            &self.state.todos,
            self.state.todo_expanded,
            inner,
        ));
        lines.extend(panes::render_queue(
            &self.state.queued,
            inner,
            Instant::now(),
        ));
        // Modal dialogs render as an inline panel above the editor: they
        // own the keyboard, so the frame must show them.
        if let Some(dialog) = &mut self.dialog {
            lines.extend(dialog.render(inner));
        }
        // The /btw side-question panel streams answers above the editor.
        if self.btw_open() {
            lines.extend(self.render_btw_panel(inner));
        }
        lines.extend(self.editor.render_box(inner, rows));
        lines.extend(self.footer(inner));
        let pad = " ".repeat(GUTTER);
        lines
            .into_iter()
            .map(|line| format!("{pad}{line}"))
            .collect()
    }

    /// Sync the window title and tab progress with state; queues raw
    /// sequences the run loop flushes next iteration. A frame calls
    /// this, so changes land with the next repaint. The single
    /// pending-sequence slot is never clobbered: when occupied (a
    /// notification is waiting), the update defers to the next frame.
    fn update_chrome(&mut self) {
        let wanted = title::session_title(
            &self.state.model_name,
            self.state.session_title.as_deref(),
            &self.state.session_id,
        );
        if self.chrome_title.as_deref() != Some(wanted.as_str()) && self.pending_sequence.is_none()
        {
            self.pending_sequence = Some(title::set_title(&wanted));
            self.chrome_title = Some(wanted);
        }
        let busy = self.state.busy();
        if busy
            && self.chrome_progress_on
            && self.chrome_progress_at.elapsed() >= title::PROGRESS_KEEPALIVE
            && self.pending_sequence.is_none()
        {
            // Terminals may clear the progress state on their own; the
            // keepalive re-arms it for as long as the turn runs.
            self.chrome_progress_at = Instant::now();
            self.pending_sequence = Some(title::progress_start());
            return;
        }
        if busy != self.chrome_progress_on && self.pending_sequence.is_none() {
            self.pending_sequence = Some(if busy {
                title::progress_start()
            } else {
                title::progress_clear()
            });
            self.chrome_progress_on = busy;
            self.chrome_progress_at = Instant::now();
        }
    }

    /// Flip the editor's square-wave prompt while a turn runs: high↔low
    /// phase every 400 ms (a calm pulse); idle restores the resting
    /// cycle. Shell mode keeps its `!` marker regardless.
    fn animate_editor_prompt(&mut self) {
        if self.shell_chrome {
            self.editor.set_prompt("!");
            return;
        }
        let ticking = self.state.busy() && (self.started_at.elapsed().as_millis() / 400) % 2 == 1;
        self.editor.set_prompt(if ticking { "⊔⊓" } else { "⊓⊔" });
    }

    /// Take the queued terminal sequence, if any.
    pub fn take_pending_sequence(&mut self) -> Option<String> {
        self.pending_sequence.take()
    }

    /// True when an animation tick must repaint (busy phases animate,
    /// shell output arrives asynchronously, the welcome ripple flows).
    pub fn needs_tick_render(&mut self) -> bool {
        self.state.busy()
            || self.exit_armed()
            || self.shell.is_some()
            || self.compaction_running()
            || self.btw_running
            || self.welcome_rippling()
    }

    /// True while the welcome wave is still flowing after a resize.
    fn welcome_rippling(&mut self) -> bool {
        self.transcript
            .get_mut(0)
            .and_then(|entry| {
                entry
                    .component
                    .as_any_mut()
                    .downcast_ref::<crate::welcome::Welcome>()
            })
            .is_some_and(|welcome| welcome.is_rippling())
    }

    /// Whether a wire event batch may paint immediately: during heavy
    /// streaming the 50 ms flush cadence throttles redraws (the tick
    /// repaints anyway while busy).
    pub fn render_due(&self) -> bool {
        !self.streaming.is_dirty() || self.streaming.due(Instant::now())
    }

    /// Record that a render happened (restarts the flush cadence).
    pub fn note_rendered(&mut self) {
        self.streaming.flushed(Instant::now());
    }

    /// Render one frame to the terminal.
    pub fn render(
        &mut self,
        out: &mut impl std::io::Write,
        columns: usize,
        rows: usize,
    ) -> std::io::Result<()> {
        let frame = self.frame(columns, rows);
        let mut buffer: Vec<u8> = Vec::with_capacity(16 * 1024);
        self.screen.draw(&mut buffer, &frame, columns, rows);
        out.write_all(&buffer)?;
        out.flush()
    }
}

/// Run `command` on `path` as a foreground external editor, returning
/// the file contents afterwards. The command runs under the platform
/// shell so multi-word editors (`code -w`) work as configured.
async fn edit_with_command(command: &str, path: &std::path::Path) -> anyhow::Result<String> {
    let script = format!("{command} \"{}\"", path.display());
    let mut cmd = tokio::process::Command::new(crate::controllers::shell::platform_shell());
    if cfg!(windows) {
        cmd.arg("/C");
    } else {
        cmd.arg("-c");
    }
    cmd.arg(&script);
    let status = cmd
        .status()
        .await
        .map_err(|e| anyhow::anyhow!("spawn failed: {e}"))?;
    if !status.success() {
        anyhow::bail!("editor exited with {status}");
    }
    let text = std::fs::read_to_string(path).unwrap_or_default();
    // Editors append a final newline; it is layout, not content.
    Ok(text.strip_suffix('\n').unwrap_or(&text).to_string())
}

/// Resolve the external editor: the `/editor` setting wins, then
/// `$VISUAL`, then `$EDITOR`; blank values never win.
fn pick_editor_command<'a>(
    setting: Option<&'a str>,
    visual: Option<&'a str>,
    editor: Option<&'a str>,
) -> Option<&'a str> {
    [setting, visual, editor]
        .into_iter()
        .flatten()
        .map(str::trim)
        .find(|command| !command.is_empty())
}

/// The editor border color for a permission mode (plan accent, auto
/// warning, otherwise the default border).
fn mode_border_style(mode: &str) -> tui_engine::color::Style {
    let theme = theme::current();
    match mode {
        "plan" => theme.style(Token::Primary),
        "auto" => theme.style(Token::Warning),
        _ => theme.style(Token::Border),
    }
}

/// The themed editor style (rebuild on theme switches).
fn editor_style() -> EditorStyle {
    let theme = theme::current();
    EditorStyle {
        border: theme.style(Token::Border),
        // The square-wave prompt carries the brand color.
        prompt: theme.style(Token::Primary).bold(),
        slash_command: theme.style(Token::Primary).bold(),
        shell_command: theme.style(Token::ShellMode),
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

/// Parse a `todowrite` tool input into panel entries: `{"todos": [{"content",
/// "status"}]}`. `None` on any shape mismatch leaves the current list in
/// place; content is sanitized here (wire-sourced text).
fn parse_todos(input: &serde_json::Value) -> Option<Vec<TodoEntry>> {
    let items = input.get("todos")?.as_array()?;
    let mut todos = Vec::with_capacity(items.len());
    for item in items {
        let content = item.get("content")?.as_str()?;
        let status = match item.get("status")?.as_str()? {
            "pending" => TodoStatus::Pending,
            "in_progress" => TodoStatus::InProgress,
            "completed" => TodoStatus::Completed,
            _ => return None,
        };
        let content = tui_engine::sanitize::sanitize_terminal(content);
        todos.push(TodoEntry {
            content: content.into_owned(),
            status,
        });
    }
    Some(todos)
}

/// Welcome info derived from state (construction-order helper).
fn welcome_info_of(state: &AppState, version: &str) -> crate::welcome::WelcomeInfo {
    crate::welcome::WelcomeInfo {
        version: version.to_string(),
        model: state.model_name.clone(),
        mode: crate::ui::permission_mode_label(&state.permission_mode).to_string(),
        cwd: state.cwd.to_string_lossy().to_string(),
        mcp_servers: state.mcp_servers.clone(),
        branch: state.git_branch.clone(),
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
        "wave" => "Wave Mode",
        _ => "Auto Mode",
    }
}

/// Short display form of a session id: first 8 characters.
pub fn short_id(id: &str) -> String {
    id.chars().take(8).collect()
}

/// Visible panel rows for an open `/btw` session.
const BTW_PANEL_ROWS: usize = 10;

impl ConsoleUi {
    /// Render the `/btw` side-question panel: the question/answer tail
    /// plus the streaming buffer, above the editor.
    fn render_btw_panel(&self, columns: usize) -> Vec<String> {
        use crate::chrome::symbols;
        let theme = theme::current();
        let mut body = Vec::new();
        let status = if self.btw_running {
            "streaming"
        } else {
            "idle · type /btw <question> to ask"
        };
        body.push(theme.bold(
            Token::BorderFocus,
            &format!("{} btw — {status}", symbols::SINE_WAVE),
        ));
        // Flatten the log into lines, then keep the tail.
        let mut lines: Vec<(bool, String)> = Vec::new();
        for (from_user, text) in &self.btw_log {
            let prefix = if *from_user { "? " } else { "" };
            for line in text.lines() {
                let marker = if *from_user {
                    prefix.to_string()
                } else {
                    "  ".to_string()
                };
                lines.push((*from_user, format!("{marker}{line}")));
            }
        }
        if !self.btw_buffer.is_empty() {
            for line in self.btw_buffer.lines() {
                lines.push((false, format!("  {line}")));
            }
        }
        let skip = lines.len().saturating_sub(BTW_PANEL_ROWS);
        for (from_user, line) in lines.into_iter().skip(skip) {
            if from_user {
                body.push(theme.bold(Token::Accent, &line));
            } else {
                body.push(theme.paint(Token::Text, &line));
            }
        }
        body.push(theme.paint(
            Token::TextDim,
            "esc closes the panel; the main session is unaffected",
        ));
        tui_engine::border::frame(body, columns, theme.style(Token::BorderFocus), None)
    }
}

/// Relative age label for a session's last activity.
fn relative_age(updated_at: u64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let seconds = now.saturating_sub(updated_at);
    match seconds {
        0..=59 => "just now".to_string(),
        60..=3599 => format!("{}m ago", seconds / 60),
        3600..=86_399 => format!("{}h ago", seconds / 3600),
        _ => format!("{}d ago", seconds / 86_400),
    }
}

/// Run the console UI until exit.
///
/// Handles key events, bracketed pastes, resizes, and session events;
/// renders through the differential screen. Ctrl+C exits via the
/// double-press cascade (interrupting the turn first when busy).
pub async fn run(client: ActorClient, ctx: UiContext) -> anyhow::Result<()> {
    run_with_factory(client, ctx, None).await
}

/// Run the console UI with an on-demand session factory, enabling
/// `/sessions` resume and `/new` re-assembly.
pub async fn run_with_factory(
    client: ActorClient,
    ctx: UiContext,
    factory: Option<std::sync::Arc<SessionFactory>>,
) -> anyhow::Result<()> {
    let mut guard =
        TerminalGuard::enter().map_err(|e| anyhow::anyhow!("terminal init failed: {e}"))?;
    let _ = guard.keyboard_enhanced();
    theme::set(theme::detect::resolve(None));

    let (mut columns, mut rows) = terminal::size().unwrap_or((80, 24));
    let mut ui = ConsoleUi::new(Box::new(client), &ctx, env!("CARGO_PKG_VERSION"));
    if let Some(factory) = factory {
        ui.set_factory(factory);
    }
    if let Err(e) = ui.render(&mut std::io::stdout().lock(), columns, rows) {
        guard.leave();
        return Err(anyhow::anyhow!("initial render failed: {e}"));
    }

    let mut events = EventStream::new();
    let mut tick = tokio::time::interval(Duration::from_millis(100));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut flow = Flow::Continue;
    // Abnormal-termination reason, reported AFTER the terminal is
    // restored: printing in raw mode would ladder across the last frame.
    let mut abort_reason: Option<String> = None;

    while flow == Flow::Continue && abort_reason.is_none() {
        // Dialog-requested launches (resume, /new) re-assemble the
        // session before anything else runs.
        if ui.take_pending_launch()
            && let Err(e) = ui.render(&mut std::io::stdout().lock(), columns, rows)
        {
            abort_reason = Some(format!("terminal render error: {e}"));
        }
        let stdout = std::io::stdout();
        if let Err(error) = ui.flush_outbox().await {
            abort_reason = Some(format!("session submission failed: {error}"));
            break;
        }
        // Terminal sequences queued by handlers go straight out;
        // they are invisible and never disturb the diff renderer.
        if let Some(seq) = ui.take_pending_sequence() {
            notify::emit_raw(&seq);
        }
        // Ctrl+G: the draft goes to an external editor while the
        // terminal is restored to cooked mode for the duration.
        if ui.pending_external_edit.is_some()
            && let Err(e) = ui.run_external_editor(&mut guard).await
        {
            abort_reason = Some(format!("terminal restore failed: {e}"));
            break;
        }
        tokio::select! {
            maybe_event = events.next() => match maybe_event {
                Some(Ok(CEvent::Key(key))) => {
                    if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
                        flow = ui.handle_key(KeyEvent::from(key));
                        if let Err(e) = ui.render(&mut stdout.lock(), columns, rows) {
                            abort_reason = Some(format!("terminal render error: {e}"));
                        }
                    }
                }
                Some(Ok(CEvent::Paste(text))) => {
                    ui.editor.insert_paste(&text);
                    if let Err(e) = ui.render(&mut stdout.lock(), columns, rows) {
                        abort_reason = Some(format!("terminal render error: {e}"));
                    }
                }
                Some(Ok(CEvent::Resize(w, h))) => {
                    columns = w as usize;
                    rows = h as usize;
                    ui.screen.invalidate();
                    if let Err(e) = ui.render(&mut stdout.lock(), columns, rows) {
                        abort_reason = Some(format!("terminal render error: {e}"));
                    }
                }
                Some(Ok(CEvent::FocusGained)) => {
                    ui.terminal_focused = true;
                }
                Some(Ok(CEvent::FocusLost)) => {
                    ui.terminal_focused = false;
                }
                Some(Ok(_)) => {}
                Some(Err(error)) => {
                    abort_reason = Some(format!("terminal event error: {error}"));
                }
                None => {
                    abort_reason = Some("terminal event source ended".to_string());
                }
            },
            event = ui.next_event() => {
                match event {
                    Some(event) => {
                        if ui.handle_wire_event(&event.msg) && ui.render_due() {
                            ui.note_rendered();
                            if let Err(e) = ui.render(&mut stdout.lock(), columns, rows) {
                                abort_reason = Some(format!("terminal render error: {e}"));
                            }
                        }
                    }
                    None => {
                        abort_reason = Some("session ended unexpectedly".to_string());
                    }
                }
            }
            _ = tick.tick() => {
                // Spinner animation, streaming flush cadence, the live
                // shell card, and the btw panel.
                let shell_changed = ui.poll_shell();
                let btw_changed = ui.poll_btw();
                if ((shell_changed || btw_changed) || ui.needs_tick_render())
                    && let Err(e) = ui.render(&mut stdout.lock(), columns, rows)
                {
                    abort_reason = Some(format!("terminal render error: {e}"));
                }
            }
        }
    }
    // Stop the tab progress even when the final frame never flushed;
    // the title stays so the pane keeps naming its session.
    notify::emit_raw(&title::progress_clear());
    guard.leave();
    match abort_reason {
        Some(reason) => Err(anyhow::anyhow!(reason)),
        None => Ok(()),
    }
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
    use crate::theme;
    use std::path::Path;
    use test_support::{NullStatus, TestLink};
    use tui_engine::keys::Mods;
    use tui_engine::width::strip_ansi;

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
            joined.contains("⊓⊔ second") || joined.contains("⊔⊓ second"),
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
        let flow = ui.user_submit("/undo");
        assert_eq!(flow, Flow::Continue);
        assert_eq!(
            ui.pending_ops().last(),
            Some(&Op::Rewind { turns: 1 }),
            "bare /undo rewinds one turn"
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
        });
        assert!(!ui.compaction_running(), "error settles the idle card");
        let frame = ui.frame(80, 24);
        let joined: String = frame
            .iter()
            .map(|l| strip_ansi(l))
            .collect::<Vec<_>>()
            .join("
");
        assert!(joined.contains("compaction failed (manual)"), "{joined}");
    }

    #[test]
    fn turn_end_settles_a_dangling_compaction_card() {
        let mut ui = ui();
        ui.handle_wire_event(&EventMsg::TurnStarted);
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
            .join("
");
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
        assert_eq!(pick_editor_command(Some("  "), None, Some("vi")), Some("vi"));
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
        let history =
            state_persistence::sessions::load_session_history(dir.path(), &fork.id).unwrap();
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
                |op| matches!(op, Op::UserInput { text } if text.contains("what is the answer")),
            );
            if landed {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            submitted.lock().unwrap().iter().any(
                |op| matches!(op, Op::UserInput { text } if text.contains("what is the answer"))
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
                    |op| matches!(op, Op::UserInput { text } if text.contains("and now what")),
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
                .any(|op| matches!(op, Op::UserInput { text } if text.contains("and now what"))),
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
    fn btw_without_args_shows_usage() {
        let mut ui = ui();
        ui.user_submit("/btw");
        let frame = ui.frame(80, 24);
        let joined: String = frame
            .iter()
            .map(|l| strip_ansi(l))
            .collect::<Vec<_>>()
            .join(
                "
",
            );
        assert!(joined.contains("usage: /btw"), "{joined}");
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
}
