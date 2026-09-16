//! TUI application state machine: protocol events and keyboard input
//! → UI state + outbound [`Op`].
//!
//! All transition logic stays side-effect free (no terminal, no client)
//! so tests drive it directly; rendering semantics mirror the legacy
//! human renderer: tool rows `▸`/`✗`, compaction `⟳`/`✓`, approval `⚠`,
//! todo lists `☐▸✓` with migration marks, interrupt `(interrupted)`.
//! Deltas enter the buffer sanitized and render as markdown once on
//! complete (a plain-text preview trails the stream while flowing).

use wavecode_wire::Op;

/// Spinner frames for in-turn waiting (status bar, advanced per 100ms tick).
pub const SPINNER: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// Message stream item cap: item line sets are the largest sustained TUI
/// allocation, so long sessions must stay memory-bounded.
/// Overflow drops the oldest items (see [`App::push_item`]).
const MAX_ITEMS: usize = 512;

/// Theme colors (same family as CLI rendering).
mod types;

/// Behavior submodules: protocol events / keyboard paste / slash
/// commands / approval popup / interactive question popup.
mod approval;
mod event;
mod input;
mod question;
mod slash;

use self::types::{ApprovalPopup, Item, QuestionPopup, err};
pub use self::types::{PermissionMode, TuiContext};
pub(crate) use self::types::{accent, dim, warn};

/// TUI application state (the single source of truth for events/keys).
pub struct App {
    ctx: TuiContext,
    /// Committed message stream items.
    pub items: Vec<Item>,
    /// Streaming assistant buffer (deltas accumulate, submitted on
    /// complete / before tool rows).
    msg_buf: String,
    /// Streaming thinking buffer (deltas accumulate, rendered as one dim
    /// item when the next content event arrives).
    think_buf: String,
    /// Whether a turn is running.
    pub in_turn: bool,
    /// Input box text and cursor (character index).
    pub input: String,
    pub cursor: usize,
    /// Message stream scroll (row offset; converges to bottom in ui
    /// while following the tail).
    pub scroll: usize,
    pub follow_tail: bool,
    /// Approval popup (all keys route to it while open).
    pub approval: Option<ApprovalPopup>,
    /// Interactive question popup (all keys route to it while open).
    pub question: Option<QuestionPopup>,
    /// Slash popup manually dismissed via Esc (reset on input change).
    slash_dismissed: bool,
    slash_selected: usize,
    /// Latest TokenCount (status bar input/output).
    pub tokens: Option<(u64, u64)>,
    /// Latest TokenCount cache counters (read, creation); `None` or zero
    /// values mean the provider reports no cache accounting.
    pub cache_tokens: Option<(u64, u64)>,
    /// Current permission mode (synced locally after `/permissions`).
    pub permission_mode: PermissionMode,
    /// Spinner phase.
    pub spinner: usize,
    /// Queued outbound protocol Ops (run loop sends them via client).
    outbox: Vec<Op>,
    quit: bool,
    /// Last rendered todo_write list (used to diff status migration).
    last_todos: Vec<(String, String)>,
    /// Model label shown in the status bar: the locally requested
    /// `/model` override once set, the session model otherwise.
    model_label: Option<String>,
}

impl App {
    pub fn new(ctx: TuiContext) -> Self {
        let permission_mode = ctx.permission_mode;
        let mut app = Self {
            ctx,
            items: Vec::new(),
            msg_buf: String::new(),
            think_buf: String::new(),
            in_turn: false,
            input: String::new(),
            cursor: 0,
            scroll: 0,
            follow_tail: true,
            approval: None,
            question: None,
            slash_dismissed: false,
            slash_selected: 0,
            tokens: None,
            cache_tokens: None,
            permission_mode,
            spinner: 0,
            outbox: Vec::new(),
            quit: false,
            last_todos: Vec::new(),
            model_label: None,
        };
        app.push_item(Item::plain(
            "WaveCode TUI — Enter submit · / completion · Esc interrupt · Ctrl-C quit".into(),
            dim(),
        ));
        app
    }

    /// Push one item with a cap: the single entry point for committed
    /// message stream items. Overflow drops the oldest items and
    /// converges the scroll offset by the dropped row count, so
    /// non-following views do not jump (row estimates ignore wrapping;
    /// follow-tail frames recompute every frame anyway).
    pub(super) fn push_item(&mut self, item: Item) {
        self.items.push(item);
        if self.items.len() > MAX_ITEMS {
            let dropped = self.items.remove(0);
            self.scroll = self.scroll.saturating_sub(dropped.lines.len() + 1);
        }
    }

    pub fn model_name(&self) -> &str {
        self.model_label.as_deref().unwrap_or(&self.ctx.model_name)
    }

    pub fn cwd(&self) -> &std::path::Path {
        &self.ctx.cwd
    }

    /// Streaming buffer content (ui appends a plain-text preview at the
    /// stream tail).
    pub fn streaming_buffer(&self) -> &str {
        &self.msg_buf
    }

    pub fn is_quit(&self) -> bool {
        self.quit
    }

    /// Drain all queued outbound Ops.
    pub fn take_ops(&mut self) -> Vec<Op> {
        std::mem::take(&mut self.outbox)
    }

    /// 100ms tick: advance the spinner phase only while in a turn.
    pub fn tick(&mut self) {
        if self.in_turn {
            self.spinner = (self.spinner + 1) % SPINNER.len();
        }
    }

    /// Event stream ended early (actor exited): red notice, then quit.
    pub fn actor_died(&mut self) {
        self.push_item(Item::plain(
            "Session terminated (agent engine exited unexpectedly)".into(),
            err(),
        ));
        self.quit = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use std::path::PathBuf;
    use std::sync::Arc;
    use wavecode_wire::{ApprovalKind, Event, EventMsg, WireDecision};

    /// Deterministic status-query stub: slash state tests no longer hit
    /// the real home directory.
    #[derive(Default)]
    struct StubStatus {
        plan: Option<String>,
        goal: Option<String>,
        labels: Vec<String>,
    }

    impl operations_actor::StatusQueries for StubStatus {
        fn plan_status(&self) -> Option<String> {
            self.plan.clone()
        }

        fn goal_status(&self) -> Option<String> {
            self.goal.clone()
        }

        fn snapshot_labels(&self) -> Vec<String> {
            self.labels.clone()
        }

        fn snapshot_summary(&self, label: &str) -> Option<String> {
            self.labels
                .contains(&label.to_string())
                .then(|| format!("Snapshot '{label}': 1 file (10 bytes)"))
        }
    }

    fn ctx() -> TuiContext {
        TuiContext {
            model_name: "m".into(),
            cwd: PathBuf::from("/tmp/x"),
            permission_mode: PermissionMode::Guarded,
            skill_names: vec!["commit".into()],
            mcp_server_lines: vec![],
            memory_index: String::new(),
            status_queries: Arc::new(StubStatus::default()),
        }
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn type_str(app: &mut App, s: &str) {
        for c in s.chars() {
            app.handle_key(key(KeyCode::Char(c)));
        }
    }

    fn ev(msg: EventMsg) -> Event {
        Event {
            id: "s-1".into(),
            msg,
        }
    }

    /// Typing → Enter: the user line enters the stream, Op::UserInput
    /// dequeues, and the input box clears.
    #[test]
    fn typing_and_enter_submits_user_input() {
        let mut app = App::new(ctx());
        type_str(&mut app, "hello");
        app.handle_key(key(KeyCode::Enter));
        let ops = app.take_ops();
        assert!(
            matches!(&ops[..], [Op::UserInput { text }] if text == "hello"),
            "should emit UserInput: {ops:?}"
        );
        assert!(app.input.is_empty() && app.cursor == 0);
        let user = app
            .items
            .iter()
            .flat_map(|i| &i.lines)
            .flat_map(|l| &l.spans)
            .any(|s| s.content.contains("> hello"));
        assert!(user, "user line should enter the stream");
    }

    /// Esc priority: open popup closes first; otherwise interrupt
    /// in-turn; idle does nothing.
    #[test]
    fn esc_priority_slash_then_interrupt() {
        let mut app = App::new(ctx());
        // In-turn: Esc → Interrupt
        app.handle_event(&ev(EventMsg::TurnStarted));
        app.handle_key(key(KeyCode::Esc));
        assert!(matches!(&app.take_ops()[..], [Op::Interrupt]));
        // Idle: Esc does nothing
        app.handle_event(&ev(EventMsg::TurnCompleted { interrupted: false }));
        app.handle_key(key(KeyCode::Esc));
        assert!(app.take_ops().is_empty());
        // Popup open: Esc only dismisses, never interrupts
        app.handle_event(&ev(EventMsg::TurnStarted));
        type_str(&mut app, "/c");
        assert!(app.slash_visible());
        app.handle_key(key(KeyCode::Esc));
        assert!(!app.slash_visible(), "Esc should dismiss the popup");
        assert!(app.take_ops().is_empty(), "dismissing must not interrupt");
    }

    /// Approval flow: event opens the popup; y → AllowOnce; n enters
    /// reason mode, Enter after typing denies with the reason.
    #[test]
    fn approval_flow_allow_and_deny_with_reason() {
        let mut app = App::new(ctx());
        let req = |id: &str| {
            ev(EventMsg::ApprovalRequested {
                call_id: id.into(),
                kind: ApprovalKind::Exec,
                detail: "d".into(),
            })
        };
        // y allows
        app.handle_event(&req("c1"));
        assert!(app.approval.is_some());
        app.handle_key(key(KeyCode::Char('y')));
        let ops = app.take_ops();
        assert!(
            matches!(&ops[..], [Op::ExecApproval { call_id, decision: WireDecision::AllowOnce }] if call_id == "c1"),
            "y should allow: {ops:?}"
        );
        assert!(app.approval.is_none());
        // n → reason mode → Enter denies with the reason
        app.handle_event(&req("c2"));
        app.handle_key(key(KeyCode::Char('n')));
        assert!(app.approval.as_ref().is_some_and(|p| p.reason_mode));
        type_str(&mut app, "dangerous");
        app.handle_key(key(KeyCode::Enter));
        let ops = app.take_ops();
        assert!(
            matches!(&ops[..], [Op::ExecApproval { call_id, decision: WireDecision::Deny { reason } }] if call_id == "c2" && reason == "dangerous"),
            "n + reason should deny: {ops:?}"
        );
        // Esc denies directly (empty reason, no parked hang)
        app.handle_event(&req("c3"));
        app.handle_key(key(KeyCode::Esc));
        let ops = app.take_ops();
        assert!(
            matches!(&ops[..], [Op::ExecApproval { decision: WireDecision::Deny { reason }, .. }] if reason.is_empty()),
            "Esc should deny with an empty reason: {ops:?}"
        );
    }

    /// follow_tail semantics: only content events (new items / grown
    /// buffers) resume following; content-free events such as TokenCount
    /// or TurnStarted must not yank the user back — high-frequency
    /// in-turn deltas would otherwise rip readers out of history.
    #[test]
    fn follow_tail_follows_only_content_events() {
        let mut app = App::new(ctx());
        app.handle_key(key(KeyCode::PageUp));
        assert!(!app.follow_tail, "paging away should leave follow mode");
        app.handle_event(&ev(EventMsg::TokenCount {
            input_tokens: 1,
            output_tokens: 2,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
            context_window: None,
            context_used: None,
        }));
        assert!(!app.follow_tail, "TokenCount must not pull back to bottom");
        app.handle_event(&ev(EventMsg::TurnStarted));
        assert!(
            !app.follow_tail,
            "TurnStarted only clears the buffer, not content"
        );
        app.handle_event(&ev(EventMsg::AgentMessageDelta { text: "hi".into() }));
        assert!(app.follow_tail, "growing delta buffer should resume follow");
        app.handle_key(key(KeyCode::PageUp));
        assert!(!app.follow_tail);
        app.handle_event(&ev(EventMsg::Warning {
            message: "w".into(),
        }));
        assert!(app.follow_tail, "warning items should resume follow");
    }

    /// Item cap: oldest items drop, length stays bounded (long sessions
    /// stay in memory). The banner and earliest warnings squeeze out;
    /// first-survivor, last-dropped, and newest boundaries are exact.
    #[test]
    fn items_cap_drops_oldest() {
        let mut app = App::new(ctx());
        for i in 0..(MAX_ITEMS * 2) {
            app.handle_event(&ev(EventMsg::Warning {
                message: format!("w{i}"),
            }));
        }
        assert_eq!(app.items.len(), MAX_ITEMS);
        // Banner + 1024 warnings total 1025 items; after dropping 513
        // the first survivor is exactly w512 (item text is the raw
        // message, so boundaries assert precisely).
        let text = |i: usize| -> String {
            app.items[i]
                .lines
                .iter()
                .flat_map(|l| &l.spans)
                .map(|s| s.content.as_ref())
                .collect()
        };
        assert_eq!(text(0), "w512");
        assert_eq!(text(MAX_ITEMS - 1), format!("w{}", MAX_ITEMS * 2 - 1));
    }

    /// Cap coverage: every committed item path funnels through
    /// push_item — CompactStarted rows and /mcp server rows must drop
    /// the oldest items too, keeping long sessions memory-bounded.
    #[test]
    fn items_cap_covers_compact_and_mcp_paths() {
        let mut app = App::new(ctx());
        for i in 0..(MAX_ITEMS + 10) {
            app.handle_event(&ev(EventMsg::CompactStarted {
                trigger: format!("t{i}"),
            }));
        }
        assert_eq!(app.items.len(), MAX_ITEMS);
        let mut app = App::new(TuiContext {
            mcp_server_lines: (0..(MAX_ITEMS + 10))
                .map(|i| format!("srv{i} — ok"))
                .collect(),
            ..ctx()
        });
        type_str(&mut app, "/mcp");
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.items.len(), MAX_ITEMS);
    }

    /// Paste routing: same routing as keys while a popup is open — pastes
    /// into the reason in reason mode, ignored otherwise (the main input
    /// sits hidden behind the popup, so silent writes would be invisible);
    /// pastes into the main input once the popup closes.
    #[test]
    fn paste_routes_to_approval_popup_when_open() {
        let mut app = App::new(ctx());
        app.paste("main input");
        assert_eq!(app.input, "main input");
        app.handle_event(&ev(EventMsg::ApprovalRequested {
            call_id: "c1".into(),
            kind: ApprovalKind::Exec,
            detail: "d".into(),
        }));
        app.paste("xyz");
        assert_eq!(
            app.input, "main input",
            "pastes must be ignored outside reason mode"
        );
        app.handle_key(key(KeyCode::Char('n')));
        app.paste("very dangerous\nsecond line");
        let popup = app.approval.as_ref().unwrap();
        assert_eq!(popup.reason, "very dangerous\nsecond line");
    }

    /// Slash completion state machine: prefix filtering, Up/Down
    /// wraparound, Tab completion, Enter completing first when the input
    /// differs from the candidate and submitting when it matches.
    #[test]
    fn slash_completion_state_machine() {
        let mut app = App::new(ctx());
        type_str(&mut app, "/c");
        assert_eq!(
            app.slash_candidates(),
            vec!["/compact".to_string(), "/commit".to_string()]
        );
        // Down moves the selection with wraparound
        app.handle_key(key(KeyCode::Down));
        assert_eq!(app.slash_selected(), 1);
        app.handle_key(key(KeyCode::Down));
        assert_eq!(app.slash_selected(), 0);
        app.handle_key(key(KeyCode::Up));
        assert_eq!(app.slash_selected(), 1);
        // Tab completes the selected item
        app.handle_key(key(KeyCode::Tab));
        assert_eq!(app.input, "/commit");
        // Input exactly matches a candidate: Enter submits (known skills
        // route through the model as a guided turn, no warning row).
        app.handle_key(key(KeyCode::Enter));
        let ops = app.take_ops();
        assert!(
            matches!(&ops[..], [Op::UserInput { text }] if text.contains("/commit") || text.contains("'commit'")),
            "skill slash should submit a guided turn: {ops:?}"
        );
        let has_warn = app
            .items
            .iter()
            .flat_map(|i| &i.lines)
            .flat_map(|l| &l.spans)
            .any(|s| s.content.contains("Unknown command"));
        assert!(!has_warn, "skill routing no longer warns");
    }

    /// Enter with the popup open and a prefix input completes first
    /// instead of submitting.
    #[test]
    fn enter_with_popup_completes_before_submit() {
        let mut app = App::new(ctx());
        type_str(&mut app, "/c");
        app.handle_key(key(KeyCode::Enter));
        assert!(app.take_ops().is_empty(), "should complete, not submit");
        assert_eq!(app.input, "/compact");
        app.handle_key(key(KeyCode::Enter));
        assert!(
            matches!(&app.take_ops()[..], [Op::Compact]),
            "second Enter submits after completion"
        );
    }

    /// /permissions: three-mode cycle with Op and local state in sync.
    #[test]
    fn permissions_cycles_three_modes() {
        let mut app = App::new(ctx());
        // ctx() starts at guarded, so the cycle begins at auto.
        let expect = ["auto", "plan", "guarded"];
        for want in expect {
            type_str(&mut app, "/permissions");
            app.handle_key(key(KeyCode::Enter));
            let ops = app.take_ops();
            assert!(
                matches!(&ops[..], [Op::SetPermissionMode { mode }] if mode == want),
                "cycle step: {ops:?}"
            );
            assert_eq!(app.permission_mode.as_str(), want);
        }
    }

    /// Unknown slash: yellow notice row enters the stream, no Op.
    #[test]
    fn unknown_slash_warns_without_op() {
        let mut app = App::new(ctx());
        type_str(&mut app, "/nope");
        app.handle_key(key(KeyCode::Enter));
        assert!(app.take_ops().is_empty());
        let has_warn = app
            .items
            .iter()
            .flat_map(|i| &i.lines)
            .flat_map(|l| &l.spans)
            .any(|s| s.content.contains("Unknown command: /nope"));
        assert!(has_warn);
    }

    /// /quit: set the quit flag (the run loop finalizes Shutdown outside).
    #[test]
    fn quit_command_sets_flag() {
        let mut app = App::new(ctx());
        type_str(&mut app, "/quit");
        app.handle_key(key(KeyCode::Enter));
        assert!(app.is_quit());
    }

    /// /mcp: locally render server status rows (no Op); hint when empty.
    #[test]
    fn mcp_lists_configured_servers_locally() {
        let mut app = App::new(TuiContext {
            mcp_server_lines: vec![
                "playwright (stdio: npx @playwright/mcp@latest) — connected (3 tools)".into(),
            ],
            ..ctx()
        });
        type_str(&mut app, "/mcp");
        app.handle_key(key(KeyCode::Enter));
        assert!(app.take_ops().is_empty(), "/mcp renders locally");
        let has_line = app
            .items
            .iter()
            .flat_map(|i| &i.lines)
            .flat_map(|l| &l.spans)
            .any(|s| s.content.contains("playwright") && s.content.contains("connected"));
        assert!(has_line);
        // Unconfigured: hint row.
        let mut app = App::new(ctx());
        type_str(&mut app, "/mcp");
        app.handle_key(key(KeyCode::Enter));
        let has_hint = app
            .items
            .iter()
            .flat_map(|i| &i.lines)
            .flat_map(|l| &l.spans)
            .any(|s| s.content.contains("No MCP servers configured"));
        assert!(has_hint);
    }

    /// /plan: locally render reviewed-plan status (no Op); the row text
    /// comes from the injected status queries, and an empty query
    /// renders the empty-state hint.
    #[test]
    fn plan_shows_status_locally() {
        let mut app = App::new(ctx());
        type_str(&mut app, "/plan");
        app.handle_key(key(KeyCode::Enter));
        assert!(app.take_ops().is_empty(), "/plan renders locally");
        let has_hint = app
            .items
            .iter()
            .flat_map(|i| &i.lines)
            .flat_map(|l| &l.spans)
            .any(|s| s.content.contains("no reviewed plan"));
        assert!(has_hint);
        // Completion offers the new builtin.
        let mut app = App::new(ctx());
        type_str(&mut app, "/p");
        assert!(app.slash_candidates().contains(&"/plan".to_string()));

        let mut app = App::new(TuiContext {
            status_queries: Arc::new(StubStatus {
                plan: Some("plan status: proposed (updated round 1)\nplan:\ndo things".into()),
                ..StubStatus::default()
            }),
            ..ctx()
        });
        type_str(&mut app, "/plan");
        app.handle_key(key(KeyCode::Enter));
        let has_status = app
            .items
            .iter()
            .flat_map(|i| &i.lines)
            .flat_map(|l| &l.spans)
            .any(|s| {
                s.content
                    .contains("plan status: proposed (updated round 1)")
            });
        assert!(has_status);
    }

    /// /goal: locally render durable-goal status (no Op); hint when no
    /// goal exists yet, query text when one does.
    #[test]
    fn goal_shows_status_locally() {
        let mut app = App::new(ctx());
        type_str(&mut app, "/goal");
        app.handle_key(key(KeyCode::Enter));
        assert!(app.take_ops().is_empty(), "/goal renders locally");
        let has_hint = app
            .items
            .iter()
            .flat_map(|i| &i.lines)
            .flat_map(|l| &l.spans)
            .any(|s| s.content.contains("no durable goal"));
        assert!(has_hint);
        // Completion offers the new builtin.
        let mut app = App::new(ctx());
        type_str(&mut app, "/g");
        assert!(app.slash_candidates().contains(&"/goal".to_string()));

        let mut app = App::new(TuiContext {
            status_queries: Arc::new(StubStatus {
                goal: Some("goal status: active (version 1, round 2)\ngoal:\nship it".into()),
                ..StubStatus::default()
            }),
            ..ctx()
        });
        type_str(&mut app, "/goal");
        app.handle_key(key(KeyCode::Enter));
        let has_status = app
            .items
            .iter()
            .flat_map(|i| &i.lines)
            .flat_map(|l| &l.spans)
            .any(|s| {
                s.content
                    .contains("goal status: active (version 1, round 2)")
            });
        assert!(has_status);
    }

    /// /snapshots: locally list labels (no Op); empty stores hint at the
    /// snapshot tool instead of failing.
    #[test]
    fn snapshots_lists_labels_locally() {
        let mut app = App::new(ctx());
        type_str(&mut app, "/snapshots");
        app.handle_key(key(KeyCode::Enter));
        assert!(app.take_ops().is_empty(), "/snapshots renders locally");
        // User line plus at least one response row (hint or labels).
        assert!(app.items.len() >= 2);
        // Completion offers the new builtin.
        let mut app = App::new(ctx());
        type_str(&mut app, "/s");
        assert!(
            app.slash_candidates().contains(&"/snapshots".to_string()),
            "candidates: {:?}",
            app.slash_candidates()
        );
    }

    /// /rewind: usage without a label, an unknown-label row for
    /// well-formed but missing labels, and the query summary for known
    /// labels (no Op ever).
    #[test]
    fn rewind_shows_usage_and_label_errors_locally() {
        let mut app = App::new(ctx());
        type_str(&mut app, "/rewind");
        app.handle_key(key(KeyCode::Enter));
        assert!(app.take_ops().is_empty(), "/rewind renders locally");
        let has_usage = app
            .items
            .iter()
            .flat_map(|i| &i.lines)
            .flat_map(|l| &l.spans)
            .any(|s| s.content.contains("usage: /rewind <label>"));
        assert!(has_usage);

        let mut app = App::new(ctx());
        type_str(&mut app, "/rewind a/b");
        app.handle_key(key(KeyCode::Enter));
        assert!(app.take_ops().is_empty());
        let has_unknown = app
            .items
            .iter()
            .flat_map(|i| &i.lines)
            .flat_map(|l| &l.spans)
            .any(|s| s.content.contains("unknown snapshot"));
        assert!(has_unknown);

        // Known label: the injected summary renders verbatim.
        let mut app = App::new(TuiContext {
            status_queries: Arc::new(StubStatus {
                labels: vec!["snap-1".to_string()],
                ..StubStatus::default()
            }),
            ..ctx()
        });
        type_str(&mut app, "/rewind snap-1");
        app.handle_key(key(KeyCode::Enter));
        assert!(app.take_ops().is_empty());
        let has_summary = app
            .items
            .iter()
            .flat_map(|i| &i.lines)
            .flat_map(|l| &l.spans)
            .any(|s| s.content.contains("Snapshot 'snap-1': 1 file"));
        assert!(has_summary);
    }

    /// Event flow: buffered deltas render as markdown on complete;
    /// interrupt leftovers still render with the interrupted marker;
    /// TokenCount feeds the status bar.
    #[test]
    fn event_flow_markdown_and_interrupt() {
        let mut app = App::new(ctx());
        use wavecode_wire::EventMsg as M;
        app.handle_event(&ev(M::TurnStarted));
        assert!(app.in_turn);
        app.handle_event(&ev(M::AgentMessageDelta {
            text: "**ok**".into(),
        }));
        assert!(app.items.len() == 1, "deltas must not enter items directly");
        app.handle_event(&ev(M::TokenCount {
            input_tokens: 5,
            output_tokens: 100,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
            context_window: None,
            context_used: None,
        }));
        app.handle_event(&ev(M::TurnCompleted { interrupted: true }));
        assert!(!app.in_turn);
        assert_eq!(app.tokens, Some((5, 100)));
        let text: String = app
            .items
            .iter()
            .flat_map(|i| &i.lines)
            .flat_map(|l| &l.spans)
            .map(|s| s.content.as_ref())
            .collect();
        assert!(text.contains("ok"), "interrupted leftovers render: {text}");
        assert!(!text.contains("**"), "markdown markers render away: {text}");
        assert!(text.contains("(interrupted)"), "interrupt marker: {text}");
    }

    /// todowrite rows: status symbols with migration marks (legacy CLI
    /// semantics).
    #[test]
    fn todowrite_renders_status_migration() {
        let mut app = App::new(ctx());
        let todo = |content: &str, status: &str| {
            ev(EventMsg::ToolCallBegin {
                call_id: "c".into(),
                name: "todowrite".into(),
                input: serde_json::json!({"todos": [{"content": content, "status": status}]}),
            })
        };
        app.handle_event(&todo("design", "in_progress"));
        app.handle_event(&todo("design", "completed"));
        let last = app.items.last().unwrap();
        let text: String = last
            .lines
            .iter()
            .flat_map(|l| &l.spans)
            .map(|s| s.content.as_ref())
            .collect();
        assert!(text.contains("✓ design"), "completion mark: {text}");
        assert!(
            text.contains("(in_progress -> completed)"),
            "migration mark: {text}"
        );
    }

    /// Cursor editing: multibyte insert / delete / moves never split UTF-8.
    #[test]
    fn cursor_editing_multibyte_safe() {
        let mut app = App::new(ctx());
        type_str(&mut app, "éc");
        app.handle_key(key(KeyCode::Left));
        app.handle_key(key(KeyCode::Char('b')));
        assert_eq!(app.input, "ébc");
        assert_eq!(app.cursor, 2);
        app.handle_key(key(KeyCode::Backspace));
        assert_eq!(app.input, "éc");
        app.handle_key(key(KeyCode::Home));
        app.handle_key(key(KeyCode::Delete));
        assert_eq!(app.input, "c");
    }
}
