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

use std::path::PathBuf;
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
use crate::chrome::symbols;
use crate::chrome::title;
use crate::chrome::{TIP_ROTATE_INTERVAL, TransientHint, render_todos};
use crate::complete::{ConsoleProvider, FileInventory};
use crate::controllers::StreamingController;
use crate::controllers::btw::BtwJob;
use crate::controllers::shell::{ShellEvent, ShellJob};
use crate::dialogs::{
    Answer, ApprovalDialog, Dialog, ModelEntryView, PromptPurpose, QuestionDialog, SessionRow,
};
use crate::history;
use crate::messages::compaction::CompactionCard;
use crate::messages::shell::ShellCard;
use crate::messages::tool_call::ToolCall;
use crate::messages::usage::UsagePanel;
use crate::messages::{AssistantMessage, ExpandedFlag, StatusLine, Thinking, UserMessage};
use crate::panes;
use crate::slash;
use crate::state::{AppState, GoalBadge, StreamingPhase, TodoEntry, TodoStatus};
use crate::theme::{self, Token};
use crate::transcript::Transcript;
use crate::welcome::Welcome;

/// One-space chrome gutter: transcript, editor, and footer share it.
const GUTTER: usize = 1;
/// Double-press window for the Ctrl+C exit confirmation.
pub const EXIT_CONFIRM_WINDOW: Duration = Duration::from_millis(1500);
/// Double-press window for Esc-Esc opening the rewind picker.
pub const DOUBLE_ESC_WINDOW: Duration = Duration::from_millis(600);
/// Bounded window for the exit-time shutdown drain: SessionEnd hooks
/// speak during it, and a hung hook cannot hold exit open past it.
pub const SHUTDOWN_DRAIN: Duration = Duration::from_secs(5);

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

/// Credential mask applied before any text reaches persistent storage
/// (turn journals, rewind snapshots). The harness injects it from its
/// provider secrets; `None` keeps bytes verbatim (tests). Kept as a
/// plain closure type so the console carries no secrets dependency.
pub type Redactor = std::sync::Arc<dyn Fn(&str) -> String + Send + Sync>;

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
    /// Instruction files assembled into the session's context
    /// (`AGENTS.md` tiers and rules), in concat order — the display
    /// source of truth for `/memory`, never a local rescan.
    pub memory_files: Vec<std::path::PathBuf>,
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
    /// Update-check slot: the harness polls the release API in the
    /// background and writes a one-line notice here when a newer
    /// release exists; the footer picks it up on a later tick. `None`
    /// skips the check (non-TUI surfaces, tests).
    pub update_notice: Option<Arc<std::sync::Mutex<Option<String>>>>,
    /// Credential mask for persisted text; `None` in tests.
    pub redactor: Option<Redactor>,
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
/// Provider-level seed for the model wizard (picked from the
/// provider list): the four fields every model on one provider shares.
pub struct ProviderPreset {
    pub provider: String,
    pub api: String,
    pub base_url: String,
    pub api_key_env: Option<String>,
}

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
    /// Update-check slot from the harness ([`UiContext::update_notice`]);
    /// polled on ticks so a late release probe still reaches the footer.
    update_slot: Option<Arc<std::sync::Mutex<Option<String>>>>,
    /// Session launch requested by a dialog (`/new`, `/sessions`),
    /// drained by the run loop through the session factory.
    pending_launch: Option<LaunchSpec>,
    /// On-demand session factory (resume, `/new`); `None` in tests and
    /// when the harness cannot re-assemble.
    factory: Option<std::sync::Arc<SessionFactory>>,
    /// Model catalog for the `/model` picker.
    model_entries: Vec<ModelEntryView>,
    /// Assembled instruction files for `/memory` (see [`UiContext`]).
    memory_files: Vec<std::path::PathBuf>,
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
    /// Persistent live assistant draft: rebuilt only when the streamed
    /// text changed, so frames without deltas (ticks, keystrokes) reuse
    /// the markdown render cache instead of re-parsing the whole buffer.
    streaming_draft: Option<AssistantMessage>,
    /// The persisted live thinking block: created once per turn so its
    /// header clock and spinner keep advancing across frames (the
    /// same pattern as the streamed assistant draft).
    streaming_thinking: Option<Thinking>,
    /// Pending Ctrl+G request: the draft handed to the external editor,
    /// drained by the run loop (raw-mode suspend happens there).
    pending_external_edit: Option<String>,
    /// Last idle Esc press, for the double-Esc rewind picker.
    last_esc_at: Option<Instant>,
    /// External status-line command runner (footer row 1 takeover).
    status_line: crate::chrome::statusline::StatusLine,
    /// Credential mask for persisted text (see [`UiContext::redactor`]).
    redactor: Option<Redactor>,
    /// Name of the live theme (`/theme` argument), re-applied by
    /// `/reload` so edited theme files show up.
    theme_name: String,
    version: String,
}

impl ConsoleUi {
    /// Build the UI; seeds the transcript with the welcome card.
    pub fn new(link: Box<dyn SessionLink>, ctx: &UiContext, version: impl Into<String>) -> Self {
        let settings = crate::settings::SharedSettings::load();
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
            status_line: crate::chrome::statusline::StatusLine::new(None),
            redactor: ctx.redactor.clone(),
            theme_name: "auto".to_string(),
            editor: {
                let mut editor = Editor::new(editor_style());
                // The engine default prompt is a fallback; the single
                // source for the glyph is chrome/symbols.rs.
                editor.set_prompt(symbols::USER_PROMPT);
                let mut command_names: Vec<String> =
                    slash::COMMANDS.iter().map(|s| s.to_string()).collect();
                command_names.extend(ctx.skill_names.iter().cloned());
                editor.set_provider(Box::new(
                    ConsoleProvider::new(&command_names, FileInventory::scan(&ctx.cwd))
                        .with_models(
                            ctx.model_entries
                                .iter()
                                .map(|entry| (entry.label.clone(), entry.provider.clone()))
                                .collect(),
                        )
                        .with_themes(
                            ctx.home
                                .as_deref()
                                .map(crate::theme::file::list)
                                .unwrap_or_default(),
                        ),
                ));
                if let Some(path) = home_history_path() {
                    // The editor keeps its own cap so later submits
                    // honor the configured limit, not a built-in 100.
                    let cap = settings.get().history_cap();
                    editor.set_history_cap(cap);
                    editor.load_history(history::load(&path, Some(cap)));
                }
                editor
            },
            transcript: Transcript::new(),
            screen: {
                let mut screen = Screen::new();
                // The frame gutter applies at write time: stored lines
                // stay unpadded, so frames carry no per-line pad copies.
                screen.set_margin(GUTTER);
                screen
            },
            streaming: StreamingController::new(),
            streaming_flushed_assistant: false,
            expanded: ExpandedFlag::with_initial(settings.get().thinking_expanded),
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
            settings,
            pending_sequence: None,
            chrome_title: None,
            chrome_progress_on: false,
            chrome_progress_at: Instant::now(),
            terminal_focused: false,
            update_slot: ctx.update_notice.clone(),
            pending_launch: None,
            factory: None,
            model_entries: ctx.model_entries.clone(),
            memory_files: ctx.memory_files.clone(),
            thinking_levels: ctx.thinking_levels.clone(),
            btw: None,
            btw_log: Vec::new(),
            btw_buffer: String::new(),
            btw_running: false,
            streaming_draft: None,
            streaming_thinking: None,
            pending_external_edit: None,
            last_esc_at: None,
            version: version.into(),
        };
        ui.state.provider_id = ctx.provider_id.clone();
        ui.state.thinking_effort = ctx.thinking_effort.clone();
        ui.state.session_id = ctx.session_id.clone();
        ui.state.session_title = ctx.session_title.clone();
        ui.state.home = ctx.home.clone();
        ui.state.git_branch = crate::git_info::branch(&ui.state.cwd);
        ui.status_line
            .set_command(ui.settings.get().status_line_command.clone());
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
        self.enqueue(Op::UserInput {
            text,
            images: Vec::new(),
        });
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

    /// Settle a compaction card that never received `CompactCompleted`
    /// (its compaction died): stop the pulse so animation ticks end.
    fn fail_dangling_compaction(&mut self) {
        if let Some(card) = self.compaction_card_mut() {
            card.fail();
        }
        self.compaction_card = None;
        self.compaction_before = None;
    }

    /// Drop a parked approval/question modal whose turn is gone: the
    /// gates died with the turn, so a late answer would only produce a
    /// "late approval" warning.
    fn dismiss_dead_gates(&mut self, reason: &str) {
        if matches!(self.dialog, Some(Dialog::Approval(_) | Dialog::Question(_))) {
            self.dialog = None;
            self.push_status(reason, false);
        }
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
                // A closed dialog may have flipped rendering settings the
                // finished-card cache cannot see (tool/edit display):
                // drop the caches so the next frame re-renders.
                self.transcript.invalidate_all();
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
                self.apply_model_selection(
                    label,
                    provider,
                    model,
                    effort,
                    session_only,
                    "use Enter to save it as the default",
                );
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
            Some(Answer::ThemeSelected { name }) => {
                self.apply_theme(&name);
            }
            Some(Answer::EffortSelected { level }) => {
                let effort = level.unwrap_or_else(|| "off".to_string());
                self.state.thinking_effort = (effort != "off").then(|| effort.clone());
                self.enqueue(Op::SetThinking { effort });
            }
            Some(Answer::Prompt { purpose, value }) => match purpose {
                PromptPurpose::SessionTitle => self.set_session_title(&value),
                PromptPurpose::EditorCommand => self.set_editor_command(&value),
                PromptPurpose::ExportPath => self.export_markdown(&value),
                PromptPurpose::CompactInstruction => {
                    self.enqueue(Op::Compact {
                        instruction: (!value.is_empty()).then_some(value),
                    });
                }
                PromptPurpose::BtwQuestion => self.handle_btw(&value),
            },
            Some(Answer::ModelForm { entries }) => {
                self.catalog_insert_form(entries);
            }
            Some(Answer::ProviderPicked { name }) => {
                let preset = name.and_then(|provider| self.provider_preset(&provider));
                self.dialog = Some(Dialog::ModelForm(Box::new(
                    crate::dialogs::ModelWizardDialog::new(preset),
                )));
            }
            None => {}
        }
    }

    /// Apply a picker selection: live switch within the same provider,
    /// persistence for the default (Enter), and a restart hint when the
    /// provider differs (cross-provider needs re-assembly). `restart_hint`
    /// names how to persist, which differs per entry point (picker Enter
    /// vs the /model command).
    fn apply_model_selection(
        &mut self,
        label: String,
        provider: String,
        model: String,
        effort: Option<String>,
        session_only: bool,
        restart_hint: &str,
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
                // session-only would persist nothing; point at the
                // caller's persist path.
                self.push_status(
                    &format!("switching provider needs a restart — {restart_hint}"),
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

    /// Graceful session teardown on UI exit: submit `Op::Shutdown` and
    /// drain events inside [`SHUTDOWN_DRAIN`], so SessionEnd hooks and
    /// the memory pass run in the actor instead of dying on client
    /// drop (Drop aborts the actor task).
    pub(crate) async fn shutdown_session(&mut self) {
        self.shutdown_session_within(SHUTDOWN_DRAIN).await;
    }

    /// [`Self::shutdown_session`] with an explicit drain window (tests
    /// shrink it; production uses the constant).
    async fn shutdown_session_within(&mut self, window: Duration) {
        let _ = self
            .link
            .submit(Submission {
                id: "tui-shutdown".to_string(),
                op: Op::Shutdown,
            })
            .await;
        let deadline = tokio::time::Instant::now() + window;
        while let Some(event) =
            tokio::time::timeout_at(deadline, self.next_event()).await.ok().flatten()
        {
            self.handle_wire_event(&event.msg);
        }
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

    /// Push one warning status line (amber: harness warnings and loop
    /// notices ride the Warning role, not the dim body tone).
    pub fn push_warning(&mut self, text: &str) {
        let text = tui_engine::sanitize::sanitize_terminal(text);
        self.transcript
            .push(Box::new(StatusLine::warning(text.as_ref())));
    }

    /// Fold the live thinking block into a finalized transcript entry.
    fn finalize_thinking(&mut self) {
        if !self.streaming.thinking.is_empty() {
            // Reuse the persisted live block when possible: finalize()
            // freezes the duration measured from its original start.
            let mut block = self
                .streaming_thinking
                .take()
                .unwrap_or_else(|| Thinking::live(self.expanded.clone()));
            block.set_text(std::mem::take(&mut self.streaming.thinking));
            block.finalize();
            self.transcript.push(Box::new(block));
        }
        self.streaming_thinking = None;
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
            EventMsg::TurnStarted { .. } => {
                self.state.phase = StreamingPhase::Waiting;
                // Cheap .git/HEAD read: catches checkout/branch switches.
                self.state.git_branch = crate::git_info::branch(&self.state.cwd);
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
                ..
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
                self.state.goal = Some(GoalBadge {
                    since: Instant::now(),
                });
                self.push_status(&format!("goal set: {objective}"), false);
                true
            }
            EventMsg::GoalCompleted => {
                self.state.goal = None;
                self.push_status("goal completed", false);
                true
            }
            EventMsg::Warning { message } => {
                // Wire messages usually arrive bare, but senders that
                // prefix already (or pass-through notices) must not
                // stack the marker twice.
                let prefixed = message.starts_with("warning:");
                self.push_warning(&if prefixed {
                    message.clone()
                } else {
                    format!("warning: {message}")
                });
                true
            }
            EventMsg::Error {
                message,
                recoverable,
                ..
            } => {
                self.push_status(&format!("error: {message}"), true);
                if !*recoverable {
                    // The turn is over even if no TurnCompleted follows
                    // (a producer dying between the two, a replayed
                    // stream): settle the live blocks like TurnCompleted
                    // does, or the spinner and the draft linger and the
                    // stale buffers bleed into the next turn's deltas.
                    self.finalize_thinking();
                    self.flush_assistant_draft();
                    self.streaming.clear();
                    self.streaming_flushed_assistant = false;
                    self.state.phase = StreamingPhase::Idle;
                    // Same reasoning as TurnCompleted: the gates are
                    // gone, so a parked modal must not linger.
                    self.dismiss_dead_gates("request dismissed (turn failed)");
                }
                // An idle compaction failure surfaces as a recoverable
                // error with no turn attached (manual `/compact`); with
                // no turn to settle the card, the error is the signal
                // the compaction died.
                if self.compaction_card.is_some() && !self.state.busy() {
                    self.fail_dangling_compaction();
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
                self.dismiss_dead_gates("request dismissed (turn ended)");
                if *interrupted {
                    self.push_status("interrupted", false);
                }
                // A compaction card still live at turn end means its
                // compaction died without emitting CompactCompleted
                // (in-turn auto/reactive/blocking failures): settle it,
                // or the pulse and the animation ticks would run forever.
                if self.compaction_card.is_some() {
                    self.fail_dangling_compaction();
                }
                // Journal the completed turn (text snapshot) so
                // /sessions + resume can replay it later.
                self.journal_turn(*interrupted);
                // Desktop attention ping for finished turns, but only when
                // the user is not looking at the terminal (focus reporting
                // events track that); queued follow-ups keep the session
                // visibly active.
                let queued_next = !self.state.queued.is_empty();
                let view = self.settings.get();
                if !*interrupted
                    && !queued_next
                    && notify::enabled_with(&view)
                    && !self.terminal_focused
                {
                    // Composed (style + tmux passthrough), not the bare
                    // OSC 9: delivery follows the configured style.
                    self.pending_sequence = Some(notify::notification_for("turn finished", &view));
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
        let fallback = |text: &str| text.to_string();
        let redact: &dyn Fn(&str) -> String = match &self.redactor {
            Some(gate) => gate.as_ref(),
            None => &fallback,
        };
        let _ = state_persistence::sessions::record_turn(
            &home,
            &session_id,
            &cwd,
            &input,
            &history,
            outcome,
            redact,
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
        state.git_branch = crate::git_info::branch(&state.cwd);
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
            // Bare forms of parameterized commands open their
            // interactive surface instead of guessing an argument;
            // instant actions (/clear, /new, /fork, …) and info-only
            // commands stay direct.
            if invocation.args.is_empty() {
                match invocation.name.as_str() {
                    "theme" => {
                        self.open_theme_picker();
                        return Flow::Continue;
                    }
                    "effort" => {
                        self.open_effort_picker();
                        return Flow::Continue;
                    }
                    "title" => {
                        self.open_prompt_dialog(
                            "Session title",
                            PromptPurpose::SessionTitle,
                            &self.state.session_title.clone().unwrap_or_default(),
                            "↵ renames the session · empty keeps it · esc cancels",
                        );
                        return Flow::Continue;
                    }
                    "editor" => {
                        let current = self.resolve_editor_command().unwrap_or_default();
                        self.open_prompt_dialog(
                            "External editor command",
                            PromptPurpose::EditorCommand,
                            &current,
                            "↵ saves the command (used by ctrl+g) · esc cancels",
                        );
                        return Flow::Continue;
                    }
                    "export" => {
                        let stamp = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_secs())
                            .unwrap_or(0);
                        self.open_prompt_dialog(
                            "Export path",
                            PromptPurpose::ExportPath,
                            &format!("wavecode-export-{stamp}.md"),
                            "↵ writes the dialogue to markdown · esc cancels",
                        );
                        return Flow::Continue;
                    }
                    "compact" => {
                        self.open_prompt_dialog(
                            "Compact the context",
                            PromptPurpose::CompactInstruction,
                            "",
                            "↵ compacts now · add text to steer the summary · esc cancels",
                        );
                        return Flow::Continue;
                    }
                    "btw" => {
                        self.open_prompt_dialog(
                            "Side question",
                            PromptPurpose::BtwQuestion,
                            "",
                            "↵ asks a read-only side session · esc cancels",
                        );
                        return Flow::Continue;
                    }
                    "undo" => {
                        // While a turn or shell runs the picker would
                        // only offer rows the rewind path refuses, so
                        // the busy guard answers directly.
                        if self.state.busy() || self.shell.is_some() {
                            self.rewind_turns_command("");
                        } else {
                            self.open_undo_picker();
                        }
                        return Flow::Continue;
                    }
                    _ => {}
                }
            }
            return match slash::dispatch(&invocation, &self.state) {
                slash::Effect::Ops(ops) => {
                    if invocation.name == "clear" {
                        self.clear_screen();
                    } else if invocation.name == "new" {
                        self.start_new_session();
                    } else if invocation.name == "theme" {
                        self.apply_theme(&invocation.args);
                    } else if invocation.name == "reload" {
                        self.reload_local_config();
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
                    } else if invocation.name == "model" {
                        self.handle_model_command(&invocation.args);
                    } else if invocation.name == "provider" {
                        self.handle_provider_command(&invocation.args);
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
                    } else if matches!(invocation.name.as_str(), "sessions" | "resume") {
                        // The picker is the only resume surface; a typed
                        // id silently matching nothing would look like a
                        // swallowed command.
                        self.push_status(
                            "/sessions takes no arguments — pick from the list",
                            false,
                        );
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
                    } else if invocation.name == "memory" {
                        self.show_memory_files();
                    } else if invocation.name == "agents" {
                        self.show_agents();
                    } else if invocation.name == "release-notes" {
                        self.dialog = Some(Dialog::Help(crate::dialogs::HelpPanel::new(
                            release_notes_lines(),
                        )));
                    } else if invocation.name == "doctor" {
                        self.run_doctor();
                    } else if invocation.name == "hooks" {
                        self.show_hooks();
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
        // Switching sessions replaces the whole UI state (outbox and
        // queue included): a running turn would be silently dropped,
        // so resume waits for the turn to end like /new and /fork do.
        if self.state.busy() {
            self.push_status(
                "cannot switch sessions while a turn runs (press Esc to interrupt)",
                false,
            );
            return;
        }
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
        // The fork journal rides the same credential mask as the turn
        // journal (see the record_turn call site).
        let fallback = |text: &str| text.to_string();
        let redact: &dyn Fn(&str) -> String = match &self.redactor {
            Some(gate) => gate.as_ref(),
            None => &fallback,
        };
        match state_persistence::sessions::fork_session(
            &home,
            &fork_id,
            &format!("Fork: {source_title}"),
            &cwd,
            &history,
            redact,
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
                // The session index entry appears with the first completed
                // turn; until then there is nothing to rename.
                self.push_status(
                    "no journal yet — /title works after the first turn completes",
                    false,
                );
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

    /// Bare `/theme`: pick among the built-ins plus custom themes, the
    /// live theme marked; Enter applies through the same path as
    /// `/theme <name>`.
    fn open_theme_picker(&mut self) {
        let custom = self
            .state
            .home
            .as_deref()
            .map(theme::file::describe)
            .unwrap_or_default();
        let current = self.theme_name.clone();
        self.dialog = Some(Dialog::Theme(crate::dialogs::ThemePickerDialog::new(
            &current, custom,
        )));
    }

    /// Bare `/effort`: pick a reasoning-effort level from the
    /// provider's levels (the standard ramp as fallback), the live
    /// level marked.
    fn open_effort_picker(&mut self) {
        let picker = crate::dialogs::EffortPickerDialog::new(
            self.state.thinking_effort.as_deref(),
            &self.thinking_levels,
        );
        self.dialog = Some(Dialog::Effort(picker));
    }

    /// Open a bare-command text prompt over prefilled text.
    fn open_prompt_dialog(
        &mut self,
        title: &str,
        purpose: PromptPurpose,
        initial: &str,
        hint: &'static str,
    ) {
        self.dialog = Some(Dialog::Prompt(crate::dialogs::PromptDialog::new(
            title, purpose, initial, hint,
        )));
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
        let fallback = |text: &str| text.to_string();
        let redact: &dyn Fn(&str) -> String = match &self.redactor {
            Some(gate) => gate.as_ref(),
            None => &fallback,
        };
        if let Err(error) = state_persistence::sessions::record_rewind(
            &home,
            &self.state.session_id,
            &self.state.cwd.to_string_lossy(),
            &history,
            turns_removed,
            redact,
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
        *guard =
            TerminalGuard::enter().map_err(|e| anyhow::anyhow!("terminal restore failed: {e}"))?;
        let _ = guard.keyboard_enhanced();
        // The guard round trip resets a recolored background; the
        // active theme's pairing must ride again.
        theme::apply_terminal_scheme();
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

    /// Apply a `/theme light|dark|deepwave|auto` switch locally.
    fn apply_theme(&mut self, args: &str) {
        let name = args.trim();
        self.theme_name = name.to_string();
        match name {
            "light" => theme::set(theme::Theme::light()),
            // "dark" tracks the default dark identity (synthwave);
            // "deepwave" pins the previous ocean identity.
            "dark" => theme::set(theme::Theme::dark()),
            "deepwave" => theme::set(theme::Theme::deepwave()),
            // Re-query the terminal background (OSC 11) and pick the
            // default dark or the light theme from the answer.
            "auto" => theme::set(theme::detect::resolve(None)),
            "" => {
                let custom = self
                    .state
                    .home
                    .as_deref()
                    .map(theme::file::list)
                    .unwrap_or_default();
                if custom.is_empty() {
                    self.push_status(
                        "usage: /theme light|dark|deepwave|auto (or a custom theme name)",
                        false,
                    );
                } else {
                    self.push_status(
                        &format!(
                            "usage: /theme light|dark|deepwave|auto|<{}>",
                            custom.join("|")
                        ),
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
                match theme::file::load(&home, custom) {
                    Ok(resolved) => theme::set(resolved),
                    Err(error) => {
                        self.push_status(&format!("theme {custom:?} failed: {error}"), true);
                        return;
                    }
                }
            }
        }
        // Chrome built at construction carries colors by value: the
        // editor (and its popup) must be rebuilt for the new palette,
        // and every transcript component drops its cached lines.
        self.editor.set_style(editor_style());
        // The generic style resets the border to Neutral; the mode or
        // shell accent re-applies so a theme switch does not gray out
        // the plan/auto/shell border color.
        if self.shell_chrome {
            let shell = theme::current().style(Token::ShellMode);
            self.editor.set_border_style(shell);
        } else {
            let border = mode_border_style(&self.state.permission_mode);
            self.editor.set_border_style(border);
        }
        self.transcript.invalidate_all();
        // The live draft bakes the old palette into its cached lines.
        self.streaming_draft = None;
        // The light theme pairs with a recolored terminal background;
        // dark themes hand the terminal its own background back.
        theme::apply_terminal_scheme();
        self.screen.invalidate();
        self.push_status(&format!("theme switched ({name})"), false);
    }

    /// Toggle mermaid fence rendering between diagrams and source, and
    /// drop every cached render so open blocks re-render in the new
    /// mode immediately.
    fn toggle_mermaid(&mut self) {
        let next = !tui_engine::mermaid::render_enabled();
        tui_engine::mermaid::set_render_enabled(next);
        self.transcript.invalidate_all();
        self.streaming_draft = None;
        self.screen.invalidate();
        self.push_status(
            if next {
                "mermaid: rendering diagrams (ctrl+m to show source)"
            } else {
                "mermaid: showing source (ctrl+m to render diagrams)"
            },
            false,
        );
    }

    /// `/reload`: re-read settings and the theme from disk, refresh
    /// the git branch, and re-arm the external status line.
    fn reload_local_config(&mut self) {
        self.settings.reload();
        let view = self.settings.get();
        self.status_line
            .set_command(view.status_line_command.clone());
        let theme_name = self.theme_name.clone();
        self.apply_theme(&theme_name);
        self.state.git_branch = crate::git_info::branch(&self.state.cwd);
        self.push_status("reloaded settings, theme, and git state", false);
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
            (Key::Char('m'), m) if m.ctrl => {
                self.toggle_mermaid();
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
                if self
                    .last_esc_at
                    .is_some_and(|at| at.elapsed() <= DOUBLE_ESC_WINDOW)
                {
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
        // The confirm gate is a setting: off means the first idle
        // Ctrl+C / Ctrl+D exits without the double-press window.
        !self.settings.get().confirm_exit
            || self
                .exit_armed_at
                .is_some_and(|at| at.elapsed() < EXIT_CONFIRM_WINDOW)
    }

    /// The two footer rows.
    pub fn footer(&mut self, columns: usize) -> Vec<String> {
        let rotate_tips = self.settings.get().rotate_tips;
        if rotate_tips && self.tip_rotated_at.elapsed() >= TIP_ROTATE_INTERVAL {
            self.tip_index = self.tip_index.wrapping_add(1);
            self.tip_rotated_at = Instant::now();
        }
        // An external status line owns row 1 outright when it has
        // produced output. Sanitized text rides the theme's dim tone:
        // unpainted, it would inherit the host foreground and drop off
        // the palette contract on a recolored light paper.
        if let Some(line) = self.state.status_line.clone() {
            let row1 = theme::current()
                .style(Token::TextDim)
                .paint(&tui_engine::width::truncate_to_width(&line, columns));
            let hint = if self.exit_armed() {
                TransientHint::ExitConfirm
            } else {
                TransientHint::None
            };
            return vec![
                row1,
                footer_chrome::row2(
                    &self.state,
                    &hint,
                    self.settings.get().show_context_footer,
                    columns,
                ),
            ];
        }
        // A release notice outranks the rotating tip: same right-hand
        // slot, so it inherits the width guard and right alignment.
        let tip = footer_chrome::TIPS[self.tip_index % footer_chrome::TIPS.len()];
        let right = self
            .state
            .update_notice
            .as_deref()
            .unwrap_or(if rotate_tips { tip } else { "" });
        let hint = if self.exit_armed() {
            TransientHint::ExitConfirm
        } else {
            TransientHint::None
        };
        vec![
            footer_chrome::row1(&self.state, Some(right), columns),
            footer_chrome::row2(
                &self.state,
                &hint,
                self.settings.get().show_context_footer,
                columns,
            ),
        ]
    }

    /// Tick work for the external status line: spawn when due, pick up
    /// a finished line. True when the footer needs a repaint.
    pub fn poll_statusline(&mut self) -> bool {
        // Sync before the guard: a disabled status line still has to
        // retire its last rendered output, or the final line parks in
        // the footer until the session is replaced.
        let line = self.status_line.current();
        if self.status_line.command().is_none() {
            if line.is_none() && self.state.status_line.is_some() {
                self.state.status_line = None;
                return true;
            }
            return false;
        }
        self.status_line.maybe_spawn(self.status_snapshot());
        if line != self.state.status_line {
            self.state.status_line = line;
            true
        } else {
            false
        }
    }

    /// The JSON session snapshot handed to the status-line command on
    /// stdin.
    fn status_snapshot(&self) -> serde_json::Value {
        serde_json::json!({
            "model": self.state.model_name,
            "provider": self.state.provider_id,
            "cwd": self.state.cwd.display().to_string(),
            "git_branch": self.state.git_branch,
            "permission_mode": self.state.permission_mode,
            "session_id": self.state.session_id,
            "session_title": self.state.session_title,
            "context_used": self.state.context_used,
            "context_window": self.state.context_window,
        })
    }

    /// Assemble the full frame at (columns, rows), flattened to one
    /// line array (tests and probes; the render path streams segments).
    pub fn frame(&mut self, columns: usize, rows: usize) -> Vec<String> {
        self.frame_with_tail(columns, rows)
            .0
            .into_iter()
            .flat_map(|segment| (*segment).clone())
            .collect()
    }

    /// Assemble the full frame plus the size of its pinned tail (the
    /// editor box, autocomplete popup, and footer rows), which the
    /// screen renderer re-anchors to the physical bottom rows on every
    /// writing frame. Segmented: cacheable components hand back their
    /// shared line arrays, so a frame costs one refcount bump per cache
    /// hit instead of a deep copy. The gutter margin is the screen's
    /// job (applied at write time), not a per-line transform here.
    fn frame_with_tail(
        &mut self,
        columns: usize,
        rows: usize,
    ) -> (Vec<tui_engine::component::Segment>, usize) {
        use tui_engine::component::Segment;
        let inner = columns.saturating_sub(GUTTER * 2);
        self.update_chrome();
        self.animate_editor_prompt();
        // Request phase: this turn's input lightens while the harness
        // waits for the model's first response byte, and returns to
        // full color once SSE deltas start flowing.
        let waiting = self.state.phase == crate::state::StreamingPhase::Waiting;
        if let Some(user) = self.transcript.newest_as_mut::<UserMessage>() {
            user.set_pending(waiting);
        }
        let mut lines: Vec<Segment> = Vec::new();
        lines.extend(self.transcript.render(inner));
        // Live thinking block (moves to the transcript when finalized).
        // The block persists across frames like the draft below: a
        // rebuilt instance would restart its header clock and spinner
        // on every flush.
        if !self.streaming.thinking.is_empty() {
            if self.streaming_thinking.is_none() {
                self.streaming_thinking = Some(Thinking::live(self.expanded.clone()));
            }
            if let Some(block) = self.streaming_thinking.as_mut() {
                block.set_text(self.streaming.thinking.clone());
                lines.push(Component::render(block, inner));
            }
        } else {
            self.streaming_thinking = None;
        }
        // Live assistant draft. The draft persists across frames and is
        // updated in place when its text no longer mirrors the
        // streaming buffer (a new delta or the draft byte cap):
        // `update_text` drops the caches but keeps the render clock, so
        // the bullet animation keeps breathing across flushes.
        if !self.streaming.assistant.is_empty() && self.settings.get().show_streaming_draft {
            if self
                .streaming_draft
                .as_ref()
                .is_none_or(|draft| draft.text() != self.streaming.assistant)
            {
                match self.streaming_draft.as_mut() {
                    Some(draft) => draft.update_text(self.streaming.assistant.clone()),
                    None => {
                        self.streaming_draft = Some(AssistantMessage::streaming(
                            self.streaming.assistant.clone(),
                            crate::highlight::highlighter(),
                        ));
                    }
                }
            }
            if let Some(draft) = self.streaming_draft.as_mut() {
                lines.push(Component::render(draft, inner));
            }
        } else {
            self.streaming_draft = None;
        }
        lines.push(Arc::new(render_todos(
            &self.state.todos,
            self.state.todo_expanded,
            inner,
        )));
        lines.push(Arc::new(panes::render_queue(
            &self.state.queued,
            inner,
            Instant::now(),
        )));
        // Modal dialogs render as an inline panel above the editor: they
        // own the keyboard, so the frame must show them.
        if let Some(dialog) = &mut self.dialog {
            lines.push(Arc::new(dialog.render(inner)));
        }
        // The /btw side-question panel streams answers above the editor.
        if self.btw_open() {
            lines.push(Arc::new(self.render_btw_panel(inner)));
        }
        // Everything from here on is the bottom-anchored input region.
        let tail_start: usize = lines.iter().map(|segment| segment.len()).sum();
        lines.push(Arc::new(self.editor.render_box(inner, rows)));
        lines.push(Arc::new(self.footer(inner)));
        let tail = lines.iter().map(|segment| segment.len()).sum::<usize>() - tail_start;
        (lines, tail)
    }

    /// `/model` entry point: bare (no args) opened the picker above; the
    /// catalog subcommands edit `~/.wavecode/models.json` (see
    /// [`Self::catalog_list`] and friends); any other word switches
    /// models through the picker semantics (see
    /// [`Self::switch_model_command`]).
    /// `/model` is a switching command only: bare opens the picker, a
    /// name switches. The catalog-editing subcommands live under
    /// `/provider` now; the old spellings point there instead of
    /// silently doing nothing.
    fn handle_model_command(&mut self, args: &str) {
        let parts: Vec<&str> = args.split_whitespace().collect();
        match parts.first().copied() {
            Some("add") | Some("list") | Some("set") | Some("remove") => self.push_status(
                "model configuration moved to /provider (add | list | set | remove)",
                false,
            ),
            _ => self.switch_model_command(args.trim()),
        }
    }

    /// `/memory`: the instruction files the bootstrap actually loaded
    /// into this session's context (`AGENTS.md` plus `AGENTS.local.md`
    /// tiers and `.wavecode/rules/*.md`), in concat order. The assembled
    /// list — not a local rescan — is the source of truth, so "what is
    /// the model reading" can never drift from what was injected. Each
    /// line names the file and its size.
    fn show_memory_files(&mut self) {
        if self.memory_files.is_empty() {
            self.push_status(
                "no instruction files in scope (/init writes an AGENTS.md for this repo)",
                false,
            );
            return;
        }
        for path in self.memory_files.clone() {
            let name = path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("?");
            let lines = std::fs::read_to_string(&path)
                .map(|text| text.lines().count())
                .unwrap_or(0);
            self.push_status(&format!("{name} · {} ({lines} lines)", path.display()), false);
        }
    }

    /// `/agents`: the background-job table — promoted shell commands
    /// running past their turn, with lifecycle state per row.
    fn show_agents(&mut self) {
        let rows = self.status.job_rows();
        if rows.is_empty() {
            self.push_status(
                "no background jobs (a shell command that outlives its timeout is promoted into one)",
                false,
            );
            return;
        }
        self.push_status(&format!("{} background job(s):", rows.len()), false);
        for row in rows {
            self.push_status(&row, false);
        }
    }

    /// `/doctor`: one-pass health report over the on-disk config
    /// surface — home directory, config.toml, the active provider's
    /// credential, models.json, console-settings.json, MCP servers —
    /// closing with the live session facts. Every line carries an
    /// `ok` / `warn` / `fail` marker so problems scan instantly.
    fn run_doctor(&mut self) {
        let Some(home) = self.state.home.clone() else {
            self.push_status("fail · no home directory (USERPROFILE/HOME unset)", true);
            return;
        };
        let dot = home.join(".wavecode");
        self.push_status(
            &format!(
                "{} · home {}",
                if dot.is_dir() { "ok  " } else { "warn" },
                dot.display()
            ),
            false,
        );
        let config_path = dot.join("config.toml");
        match wavecode_config::Config::load_from(&config_path) {
            Ok(config) => {
                self.push_status(
                    &format!(
                        "ok   · config.toml ({} provider(s), {} hook event(s))",
                        config.model_providers.len(),
                        config.hooks.len()
                    ),
                    false,
                );
                match config.model_providers.get(&self.state.provider_id) {
                    Some(provider) => match (&provider.env_key, &provider.api_key) {
                        (Some(env), _) if std::env::var_os(env).is_some_and(|v| !v.is_empty()) => {
                            self.push_status(&format!("ok   · credential via {env}"), false);
                        }
                        (Some(env), None) => {
                            self.push_status(
                                &format!("warn · key env {env} is not set in this shell"),
                                true,
                            );
                        }
                        (_, Some(_)) => {
                            self.push_status("ok   · inline api key in config", false);
                        }
                        _ => {
                            self.push_status("warn · no credential (env_key / api_key)", true);
                        }
                    },
                    None => self.push_status(
                        "info · current provider is not a config.toml entry (catalog-built?)",
                        false,
                    ),
                }
            }
            Err(wavecode_config::ConfigError::NotFound(_)) => {
                self.push_status("warn · config.toml absent (defaults apply)", false);
            }
            Err(error) => self.push_status(&format!("fail · config.toml: {error}"), true),
        }
        match wavecode_config::ModelCatalog::load(&home) {
            Ok(catalog) => self.push_status(
                &format!("ok   · models.json ({} model(s))", catalog.models.len()),
                false,
            ),
            Err(error) => self.push_status(&format!("warn · models.json: {error}"), true),
        }
        match crate::settings::UiSettings::path() {
            Some(path) => match std::fs::read_to_string(&path) {
                Ok(text) => match serde_json::from_str::<crate::settings::UiSettings>(&text) {
                    Ok(_) => self.push_status("ok   · console-settings.json", false),
                    Err(error) => {
                        self.push_status(&format!("warn · console-settings.json: {error}"), true)
                    }
                },
                Err(_) => self.push_status("info · console-settings.json not written yet", false),
            },
            None => self.push_status("warn · no home for console-settings.json", true),
        }
        self.push_status(
            &format!(
                "info · {} mcp server(s) · theme {}",
                self.state.mcp_servers.len(),
                self.theme_name
            ),
            false,
        );
        self.push_status(
            &format!(
                "info · model {} (provider {}) · effort {} · mode {}{}",
                self.state.model_name,
                self.state.provider_id,
                self.state.thinking_effort.as_deref().unwrap_or("off"),
                self.state.permission_mode,
                self.state
                    .git_branch
                    .as_deref()
                    .map(|branch| format!(" · branch {branch}"))
                    .unwrap_or_default()
            ),
            false,
        );
    }

    /// `/hooks`: the configured hook table, one line per rule — event,
    /// matcher, command, and the once/timeout modifiers.
    fn show_hooks(&mut self) {
        let Some(home) = self.state.home.clone() else {
            self.push_status("no home directory — no hooks configured", false);
            return;
        };
        let config_path = home.join(".wavecode").join("config.toml");
        match wavecode_config::Config::load_from(&config_path) {
            Ok(config) => {
                let mut total = 0;
                let mut events: Vec<_> = config.hooks.iter().collect();
                events.sort_by(|a, b| a.0.cmp(b.0));
                for (event, set) in events {
                    for rule in set.rules() {
                        total += 1;
                        let matcher = rule.matcher.as_deref().unwrap_or("*");
                        let once = if rule.once.unwrap_or(false) {
                            " · once"
                        } else {
                            ""
                        };
                        let timeout = rule
                            .timeout_ms
                            .map(|ms| format!(" · {ms}ms"))
                            .unwrap_or_default();
                        self.push_status(
                            &format!("{event} · {matcher} → {}{once}{timeout}", rule.command),
                            false,
                        );
                    }
                }
                if total == 0 {
                    self.push_status("no hook rules (config.toml [hooks.<EventPoint>])", false);
                }
            }
            Err(wavecode_config::ConfigError::NotFound(_)) => {
                self.push_status("no config.toml — no hooks configured", false);
            }
            Err(error) => self.push_status(&format!("hooks unavailable: {error}"), true),
        }
    }

    /// `/provider`: the guided catalog surface. Bare lists the existing
    /// providers (picking one preseeds the wizard) plus a new-provider
    /// row; `add` opens the wizard directly; `list` / `set` / `remove`
    /// edit the saved file.
    fn handle_provider_command(&mut self, args: &str) {
        let parts: Vec<&str> = args.split_whitespace().collect();
        match parts.first().copied() {
            None => {
                self.dialog = Some(Dialog::ProviderPick(Box::new(
                    crate::dialogs::ProviderPickerDialog::new(self.catalog_providers()),
                )));
            }
            Some("add") if parts.len() == 1 => {
                self.dialog = Some(Dialog::ModelForm(Box::new(
                    crate::dialogs::ModelWizardDialog::new(None),
                )));
            }
            Some("add") => self.catalog_add(&parts[1..]),
            Some("list") => self.catalog_list(),
            Some("remove") => self.catalog_remove(parts.get(1).copied()),
            Some("set") => self.catalog_set(&parts[1..]),
            other => {
                self.push_status(
                    &format!("unknown /provider subcommand {other:?} (add | list | set | remove)"),
                    true,
                );
            }
        }
    }

    /// Load the on-disk catalog for a CRUD subcommand: a missing home
    /// directory or a malformed models.json reports and cancels the
    /// subcommand. Plain `/model <name>` switching never touches the
    /// catalog, so a broken file cannot take live switching down too.
    fn load_catalog(&mut self) -> Option<(wavecode_config::ModelCatalog, PathBuf)> {
        let Some(home) = self.state.home.clone() else {
            self.push_status("model catalog needs a home directory", true);
            return None;
        };
        match wavecode_config::ModelCatalog::load(&home) {
            Ok(catalog) => Some((catalog, home)),
            Err(e) => {
                self.push_status(&format!("model catalog load failed: {e}"), true);
                None
            }
        }
    }

    /// `/model list`: every catalog spec, one status line each.
    fn catalog_list(&mut self) {
        let Some((catalog, _home)) = self.load_catalog() else {
            return;
        };
        if catalog.models.is_empty() {
            self.push_status("model catalog is empty (/provider add ...)", false);
            return;
        }
        for (alias, spec) in &catalog.models {
            let reasoning = spec.reasoning.variants.join("/");
            self.push_status(
                &format!(
                    "{alias} · {} ({}) ctx {} out {} thinking [{}] in [{}] out [{}]",
                    spec.model,
                    spec.base_url,
                    spec.context_window
                        .map(|v| v.to_string())
                        .unwrap_or_else(|| "default".into()),
                    spec.max_output
                        .map(|v| v.to_string())
                        .unwrap_or_else(|| "default".into()),
                    reasoning,
                    spec.modalities.input.join(","),
                    spec.modalities.output.join(","),
                ),
                false,
            );
        }
    }

    /// `/model remove <alias>`: drop the spec and save.
    fn catalog_remove(&mut self, alias: Option<&str>) {
        let Some(alias) = alias else {
            self.push_status("usage: /provider remove <alias>", true);
            return;
        };
        let Some((mut catalog, home)) = self.load_catalog() else {
            return;
        };
        if catalog.remove(alias).is_some() {
            match catalog.save(&home) {
                Ok(()) => self.push_status(&format!("removed {alias}"), false),
                Err(e) => self.push_status(&format!("catalog save failed: {e}"), true),
            }
        } else {
            self.push_status(&format!("no model named {alias}"), true);
        }
    }

    /// `/model set <alias> <context|output|thinking|input> <value>`:
    /// patch one spec field and save.
    fn catalog_set(&mut self, rest: &[&str]) {
        let (Some(alias), Some(field), Some(value)) = (rest.first(), rest.get(1), rest.get(2))
        else {
            self.push_status(
                "usage: /provider set <alias> <context|output|thinking|input> <value>",
                true,
            );
            return;
        };
        let (alias, field, value) = (*alias, *field, *value);
        let Some((mut catalog, home)) = self.load_catalog() else {
            return;
        };
        let Some(spec) = catalog.models.get_mut(alias) else {
            self.push_status(&format!("no model named {alias}"), true);
            return;
        };
        // Numeric fields parse before anything is written: an
        // unparseable value must report, not silently reset the field
        // to the default while claiming success.
        match field {
            "context" => match value.parse::<u64>() {
                Ok(parsed) => spec.context_window = Some(parsed),
                Err(_) => {
                    self.push_status(&format!("invalid context value {value:?} (a number)"), true);
                    return;
                }
            },
            "output" => match value.parse::<u32>() {
                Ok(parsed) => spec.max_output = Some(parsed),
                Err(_) => {
                    self.push_status(&format!("invalid output value {value:?} (a number)"), true);
                    return;
                }
            },
            "thinking" => {
                spec.reasoning.enabled = !value.eq_ignore_ascii_case("off");
                spec.reasoning.default =
                    (!value.eq_ignore_ascii_case("off")).then(|| value.to_string());
            }
            "input" => {
                spec.modalities.input = value
                    .split(',')
                    .map(str::trim)
                    .map(str::to_string)
                    .collect();
            }
            other => {
                self.push_status(&format!("unknown field {other}"), true);
                return;
            }
        }
        match catalog.save(&home) {
            Ok(()) => self.push_status(&format!("{alias} updated ({field} = {value})"), false),
            Err(e) => self.push_status(&format!("catalog save failed: {e}"), true),
        }
    }

    /// `/model add <alias> <kind> <provider> <base_url> <model>
    /// [context] [max_output]`: insert a spec and save.
    fn catalog_add(&mut self, rest: &[&str]) {
        if rest.len() < 5 {
            self.push_status(
                "usage: /provider add <alias> <anthropic-messages|openai-chat|openai-responses> \
                 <provider> <base_url> <model> [context] [max_output]",
                true,
            );
            return;
        }
        let kind = match rest[1].to_ascii_lowercase().as_str() {
            "anthropic-messages" | "anthropic" => wavecode_config::ApiKind::AnthropicMessages,
            "openai-chat" | "openai" => wavecode_config::ApiKind::OpenaiChat,
            "openai-responses" | "responses" => wavecode_config::ApiKind::OpenaiResponses,
            other => {
                self.push_status(&format!("unknown api kind {other}"), true);
                return;
            }
        };
        let spec = wavecode_config::ModelSpec {
            provider: rest[2].to_string(),
            model: rest[4].to_string(),
            kind,
            base_url: rest[3].to_string(),
            api_key_env: None,
            api_key: None,
            context_window: rest.get(5).and_then(|v| v.parse().ok()),
            max_output: rest.get(6).and_then(|v| v.parse().ok()),
            reasoning: wavecode_config::ReasoningSpec::default(),
            modalities: wavecode_config::ModalitiesSpec::default(),
        };
        let alias = rest[0].to_string();
        let Some((mut catalog, home)) = self.load_catalog() else {
            return;
        };
        catalog.insert(alias.clone(), spec);
        match catalog.save(&home) {
            Ok(()) => self.push_status(&format!("added {alias} (restart applies it)"), false),
            Err(e) => self.push_status(&format!("catalog save failed: {e}"), true),
        }
    }

    /// Persist the specs built by the `/provider` wizard: same insert
    /// and save as the positional command, but the wizard has already
    /// validated its own fields, so only the catalog I/O can fail. One
    /// pass can carry several models sharing one provider.
    fn catalog_insert_form(&mut self, entries: Vec<(String, wavecode_config::ModelSpec)>) {
        if entries.is_empty() {
            return;
        }
        let Some((mut catalog, home)) = self.load_catalog() else {
            return;
        };
        for (alias, spec) in &entries {
            if alias.is_empty() {
                self.push_status("model alias is required", true);
                return;
            }
            catalog.insert(alias.clone(), spec.clone());
        }
        let names: Vec<&str> = entries.iter().map(|(alias, _)| alias.as_str()).collect();
        match catalog.save(&home) {
            Ok(()) => self.push_status(
                &format!("added {} (restart applies it)", names.join(", ")),
                false,
            ),
            Err(e) => self.push_status(&format!("catalog save failed: {e}"), true),
        }
    }

    /// Distinct providers in the saved catalog with their model counts,
    /// alphabetically — the `/provider` opening list.
    fn catalog_providers(&self) -> Vec<(String, usize)> {
        let mut counts: std::collections::BTreeMap<String, usize> = Default::default();
        if let Some(home) = self.state.home.clone()
            && let Ok(catalog) = wavecode_config::ModelCatalog::load(&home)
        {
            for spec in catalog.models.values() {
                *counts.entry(spec.provider.clone()).or_default() += 1;
            }
        }
        counts.into_iter().collect()
    }

    /// The provider-level preset a wizard seeds from: the first spec
    /// under that provider donates its dialect, endpoint, and key env.
    fn provider_preset(&self, provider: &str) -> Option<ProviderPreset> {
        let home = self.state.home.clone()?;
        let catalog = wavecode_config::ModelCatalog::load(&home).ok()?;
        catalog
            .models
            .values()
            .find(|spec| spec.provider == provider)
            .map(|spec| ProviderPreset {
                provider: spec.provider.clone(),
                api: match spec.kind {
                    wavecode_config::ApiKind::AnthropicMessages => "anthropic-messages",
                    wavecode_config::ApiKind::OpenaiChat => "openai-chat",
                    wavecode_config::ApiKind::OpenaiResponses => "openai-responses",
                }
                .to_string(),
                base_url: spec.base_url.clone(),
                api_key_env: spec.api_key_env.clone(),
            })
    }

    /// `/model <name>`: switch through the picker semantics. A name
    /// matching a picker entry (a config or catalog alias) resolves to
    /// its wire model and default effort under the same-provider guard,
    /// exactly like the picker's live switch; an unknown name switches
    /// as typed (a wire model name on the current provider).
    fn switch_model_command(&mut self, name: &str) {
        if name.is_empty() {
            return; // bare /model opened the picker above
        }
        let entry = self
            .model_entries
            .iter()
            .find(|entry| entry.label == name)
            .cloned();
        match entry {
            Some(entry) => self.apply_model_selection(
                entry.label,
                entry.provider,
                entry.model,
                entry.effort,
                true,
                "save it as the default from the /model picker",
            ),
            None => {
                self.state.model_name = name.to_string();
                self.enqueue(Op::SetModel {
                    name: name.to_string(),
                });
                self.push_status(&format!("model: {name}"), false);
            }
        }
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

    /// Pulse the editor's chevron prompt while a turn runs: bold↔thin
    /// every 400 ms (a calm pulse); idle restores the resting chevron.
    /// Shell mode keeps its `!` marker regardless.
    fn animate_editor_prompt(&mut self) {
        if self.shell_chrome {
            self.editor.set_prompt("!");
            return;
        }
        let ticking = self.state.busy() && (self.started_at.elapsed().as_millis() / 400) % 2 == 1;
        self.editor.set_prompt(if ticking {
            symbols::USER_PROMPT_PULSE
        } else {
            symbols::USER_PROMPT
        });
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

    /// Drain the update-check slot into the footer state. The harness
    /// probe runs in the background, so the notice usually arrives long
    /// after the first frame. Returns true when a notice landed this
    /// tick (the caller repaints); once shown it stays until exit.
    pub fn poll_update_notice(&mut self) -> bool {
        if self.state.update_notice.is_some() {
            return false;
        }
        let Some(slot) = &self.update_slot else {
            return false;
        };
        // Poison recovery: the slot is a plain Option with no invariant
        // a panic could break; a poisoned guard must not take down the
        // render tick.
        match slot.lock().unwrap_or_else(|e| e.into_inner()).take() {
            Some(text) => {
                self.state.update_notice = Some(text);
                true
            }
            None => false,
        }
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
        let (frame, tail) = self.frame_with_tail(columns, rows);
        self.screen.set_pinned_tail(tail);
        let mut buffer: Vec<u8> = Vec::with_capacity(16 * 1024);
        self.screen.draw(&mut buffer, &frame, columns, rows);
        out.write_all(&buffer)?;
        out.flush()
    }
}

/// Run `command` on `path` as a foreground external editor, returning
/// the file contents afterwards. The command runs under the platform
/// shell so multi-word editors (`code -w`) work as configured.
///
/// Deliberately no timeout (audited): the editor is interactive and
/// user-owned — a word limit would kill a working session mid-edit. The
/// user controls the lifetime (close the editor, or kill a hung one from
/// another terminal; the CLI resumes on exit). While the editor runs the
/// session actor keeps streaming into the bounded event channel, whose
/// drop policy covers the gap.
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
        border: theme.style(Token::Neutral),
        // The draft body rides the text ramp: with the light theme's
        // recolored terminal, an unpainted draft would inherit the
        // host's default (near-white) foreground and vanish on paper.
        text: theme.style(Token::Text),
        // The input chrome rides the neutral gray band (the colored
        // surface is the sent message row, not the editor).
        prompt: theme.style(Token::Neutral).bold(),
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

/// The history file for the console surface, when the home directory is known.
fn home_history_path() -> Option<PathBuf> {
    let home = wavecode_config::home_dir()?;
    if home.as_os_str().is_empty() {
        return None;
    }
    Some(history::history_path(&home, "console"))
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
    let home = wavecode_config::home_dir();
    let mut display = text.clone();
    if let Some(home) = home
        && !home.as_os_str().is_empty()
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
            &format!("{} btw — {status}", symbols::DONE),
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

/// The CHANGELOG's two newest sections (embedded at compile time from
/// the workspace root), for the `/release-notes` panel.
pub(crate) fn release_notes_lines() -> Vec<String> {
    const CHANGELOG: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../../CHANGELOG.md"
    ));
    let mut out = Vec::new();
    let mut sections = 0usize;
    for line in CHANGELOG.lines() {
        if line.starts_with("## [") {
            sections += 1;
            if sections > 2 {
                break;
            }
        }
        if sections >= 1 {
            out.push(line.to_string());
        }
    }
    if out.is_empty() {
        out.push("(the changelog has no sections yet)".to_string());
    }
    out
}

/// Relative age label for a session's last activity.
fn relative_age(updated_at_secs: u64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let seconds = now.saturating_sub(updated_at_secs);
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
    // Color depth is a terminal property: resolve it once, before the
    // first paint (theme switches never change it).
    tui_engine::color::set_color_depth(theme::detect::color_depth());
    theme::set(theme::detect::resolve(None));
    // The light theme recolors the terminal background (OSC 11) so it
    // stays readable on dark-terminal hosts; dark resets to the host's
    // own background.
    theme::apply_terminal_scheme();

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
    // Paste-burst only matters when the terminal does not bracket
    // pastes itself: there, fast keystreams need the Enter rewrite.
    let mut typing_burst = tui_engine::typing_burst::TypingBurst::new(!guard.bracketed_paste());
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
                        let key = typing_burst.observe(KeyEvent::from(key), Instant::now());
                        flow = ui.handle_key(key);
                        if let Err(e) = ui.render(&mut stdout.lock(), columns, rows) {
                            abort_reason = Some(format!("terminal render error: {e}"));
                        }
                    }
                }
                Some(Ok(CEvent::Paste(text))) => {
                    // A modal owns the keyboard: pasting behind it would
                    // silently land in the covered editor.
                    if ui.dialog.is_none() {
                        ui.editor.insert_paste(&text);
                    }
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
                // shell card, the btw panel, the release notice, and
                // the external status line.
                let shell_changed = ui.poll_shell();
                let btw_changed = ui.poll_btw();
                let update_changed = ui.poll_update_notice();
                let statusline_changed = ui.poll_statusline();
                if ((shell_changed
                    || btw_changed
                    || update_changed
                    || statusline_changed)
                    || ui.needs_tick_render())
                    && let Err(e) = ui.render(&mut stdout.lock(), columns, rows)
                {
                    abort_reason = Some(format!("terminal render error: {e}"));
                }
            }
        }
    }
    // Graceful teardown: SessionEnd hooks and the memory pass run
    // during the bounded drain instead of dying on client drop (Drop
    // aborts the actor). One last render lands their output in the
    // final frame; a render failure cannot block exit.
    ui.shutdown_session().await;
    let _ = ui.render(&mut std::io::stdout().lock(), columns, rows);
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

    /// Shared-handle shape: tests keep a clone to read back what the UI
    /// submitted after the box moved into [`ConsoleUi`].
    #[async_trait]
    impl SessionLink for std::sync::Arc<TestLink> {
        async fn submit(&self, submission: Submission) -> Result<(), SubmitError> {
            self.as_ref().submit(submission).await
        }

        async fn next_event(&mut self) -> Option<Event> {
            // Same contract as the inner link: events never arrive.
            std::future::pending().await
        }

        fn steer(&self, text: &str, target: SteerTarget) -> bool {
            self.as_ref().steer(text, target)
        }
    }
}

#[cfg(test)]
mod showcase;

#[cfg(test)]
mod catalog_tests;

#[cfg(test)]
mod vt_repro;

#[cfg(test)]
mod tests;
