/*!
 * @file RunLoop
 * @description Execution orchestration for a single agent run.
 *
 * Responsibilities:
 * - Own the run state machine (sample -> decide -> execute -> recover).
 * - Depend only on anticorruption traits, never on concrete capabilities.
 * - Enforce retry budgets and idempotency keys.
 *
 * This module must not depend on: tools, sandbox, hooks, memory, skills,
 * mcp, transport, or any concrete capability implementation.
 */

//! Run orchestration primitives and anticorruption trait seams.
//!
//! Concrete capabilities (tools, policy, hooks, models) are wired behind
//! the traits defined here by the composition root (bootstrap). This crate
//! only carries data transfer objects so the dependency graph stays acyclic.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};

use serde_json::Value;

use futures::StreamExt;
use infrastructure_base::InterruptHandle;
use state_store::{
    BudgetLevel, CONTEXT_OVERHEAD_TOKENS, CompactTrigger, Conversation, HistoryEntry, Role, Usage,
    check_budget, estimate_tokens,
};
use wavecode_wire::{ApprovalKind as WireApprovalKind, Event, EventMsg, ToolCallPreview};

/// Default ceiling for tool rounds inside one run.
///
/// Rationale: every tool round consumes model context and wall-clock time.
/// The composition root may override this per session; the loop itself must
/// always terminate, so the ceiling is a hard stop, not a hint.
pub const DEFAULT_MAX_TOOL_ROUNDS: u32 = 32;

/// Identifies one agent run end to end.
///
/// Both `run_id` and `submission_id` act as idempotency keys: retries of the
/// same submission must reuse them so downstream stores can deduplicate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunContext {
    /// Stable identifier of this run (one per user turn).
    pub run_id: String,
    /// Identifier of the inbound submission that started this run.
    pub submission_id: String,
    /// Raw user input text for this run.
    pub input: String,
    /// Inline images attached to the input (empty on text-only runs and
    /// on older senders; serde-compatible default).
    pub images: Vec<wavecode_wire::UserImage>,
}

impl RunContext {
    /// Build a namespaced idempotency key for run-scoped side effects.
    pub fn idempotency_key(&self, scope: &str) -> String {
        format!("{}:{}", self.run_id, scope)
    }
}

/// Terminal reason of a run, used for observability and recovery decisions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopReason {
    /// The model produced a final answer with no pending tool calls.
    Completed,
    /// An interrupt signal arrived and was honoured at a safe point.
    Interrupted,
    /// The tool-round ceiling was reached; the run stops without error.
    MaxToolRounds,
    /// The run failed; carries a human-readable cause.
    Error(String),
}

/// Mutable counters owned by the run loop.
///
/// Kept in this crate (not in the session store) because they describe the
/// execution of one run, not the persisted conversation.
#[derive(Debug, Clone, Default)]
pub(crate) struct TurnState {
    /// Cumulative output tokens across samples in this run.
    pub(crate) total_output_tokens: u64,
    /// Cumulative prompt-cache read tokens across samples in this run
    /// (0 when the provider reports no cache accounting).
    pub(crate) total_cache_read_tokens: u64,
    /// Cumulative prompt-cache write tokens across samples in this run
    /// (0 when the provider reports no cache accounting).
    pub(crate) total_cache_creation_tokens: u64,
    /// How many tool dispatch rounds have executed in this run.
    pub(crate) tool_rounds: u32,
}

impl TurnState {
    /// Create a zeroed state for a fresh run.
    pub fn new() -> Self {
        Self::default()
    }

    /// Accumulate output tokens reported by one sample.
    pub fn add_output(&mut self, tokens: u64) {
        self.total_output_tokens = self.total_output_tokens.saturating_add(tokens);
    }

    /// Record one finished tool dispatch round.
    pub fn bump_tool_round(&mut self) {
        self.tool_rounds = self.tool_rounds.saturating_add(1);
    }

    /// Check the tool-round ceiling; the loop must stop, not error, on hit.
    pub fn rounds_exhausted(&self, max: u32) -> bool {
        self.tool_rounds >= max
    }
}

/// Maximum reactive compactions per turn (default for
/// [`RunConfig::max_reactive_compacts`).
///
/// Counts consecutive prompt-too-long failures like the legacy loop: two
/// successful compactions are attempted and the third consecutive failure
/// melts the turn. A successful sample resets the count.
pub const MAX_REACTIVE_COMPACTS: u8 = 3;

/// One tool invocation requested by the model.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolCall {
    /// Stable identifier used to pair the call with its result.
    pub call_id: String,
    /// Tool name as registered in the capability registry.
    pub name: String,
    /// Validated JSON input for the tool.
    pub input: Value,
}

/// Outcome of one tool invocation.
///
/// Business failures travel as `Ok` with `is_error = true` so the model can
/// self-correct; `Err` is reserved for implementation faults.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolResult {
    /// Identifier pairing this result with its [`ToolCall`].
    pub call_id: String,
    /// Human-readable result payload.
    pub content: String,
    /// True when the tool ran but reported a business failure.
    pub is_error: bool,
}

/// Policy verdict for one tool call, decided before execution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PolicyVerdict {
    /// Execute immediately without asking.
    Allow,
    /// Ask the user; carries the approval kind and a bounded detail string.
    Ask { kind: AskKind, detail: String },
    /// Park an interactive question: the user's answer becomes the tool
    /// result and the tool body never executes.
    Question {
        /// The question text for display.
        question: String,
        /// Numbered answer options; empty when free text is expected.
        options: Vec<String>,
    },
    /// Refuse execution with a reason; reported back as an error result.
    Deny { reason: String },
}

/// Approval kind requested by an [`PolicyVerdict::Ask`] verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AskKind {
    /// Arbitrary command execution needs approval.
    Exec,
    /// File modification needs approval.
    Write,
}

/// Lifecycle point at which a hook may observe or block execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookPoint {
    /// Before a tool executes; may block.
    PreToolUse,
    /// After a tool executes; never blocks.
    PostToolUse,
    /// When user input arrives; may block.
    PromptSubmit,
    /// When the session starts; never blocks.
    SessionStart,
    /// When the session ends; never blocks.
    SessionEnd,
    /// When the model requests stop; may block.
    Stop,
    /// Before context compaction; never blocks.
    PreCompact,
    /// After context compaction; never blocks.
    PostCompact,
}

/// Outcome of running hooks at one lifecycle point.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookReport {
    /// False only when a blocking hook point vetoed execution.
    pub allow: bool,
    /// Human-readable summary, surfaced as a warning when non-empty.
    pub message: String,
    /// Context injected by prompt-type hooks (their capped stdout), folded
    /// into the conversation as harness guidance. Empty when none produced
    /// output; never blocks by itself.
    pub context: String,
}

/// Minimal tool reference advertised to the model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolRef {
    /// Tool name as registered in the capability registry.
    pub name: String,
    /// One-line description shown to the model.
    pub description: String,
}

/// One content block crossing the model seam.
///
/// Re-exported from the conversation store so history, sample requests,
/// and sample responses share one shape. Responses never contain
/// [`SampleBlock::ToolResult`]; results enter history through the loop.
pub use state_store::Block as SampleBlock;

/// Request handed to the model gateway for one sample.
#[derive(Debug, Clone, PartialEq)]
pub struct SampleRequest {
    /// System prompt text assembled by the prompt layer.
    pub system: String,
    /// Conversation snapshot for this sample, blocks included.
    pub messages: Vec<HistoryEntry>,
    /// Tools available in this sample.
    pub tools: Vec<ToolRef>,
}

/// Response of one model sample.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct SampleResponse {
    /// Ordered content blocks of the assistant message.
    pub blocks: Vec<SampleBlock>,
    /// Input tokens reported by the provider, if any.
    pub input_tokens: Option<u64>,
    /// Output tokens reported by the provider, if any.
    pub output_tokens: Option<u64>,
    /// Tokens served from the provider prompt cache (0 when the provider
    /// reports no cache accounting).
    pub cache_read_tokens: u64,
    /// Tokens written to the provider prompt cache by this sample (0 when
    /// the provider reports no cache accounting).
    pub cache_creation_tokens: u64,
    /// True when the provider stopped at its output limit.
    pub truncated: bool,
}

/// Sampling failure modes relevant to the run loop.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SampleError {
    /// The prompt exceeded the model context window; may trigger compaction.
    #[error("prompt exceeds context window")]
    PromptTooLong,
    /// Transport or provider failure; the loop fails the run after settle.
    #[error("model transport failed: {0}")]
    Transport(String),
    /// Sampling timed out; the loop fails the run after settle.
    #[error("model sampling timed out")]
    Timeout,
}

/// Executes tool calls; implemented by the capability layer adapter.
///
/// Attribute methods are the single source of dispatch truth: the loop
/// never re-derives execution semantics from tool names. The safe
/// defaults (serial, destructive) apply to unregistered names.
#[async_trait::async_trait]
pub trait ToolExecutor: Send + Sync {
    /// Execute one validated tool call and return its result.
    async fn execute(&self, call: ToolCall) -> ToolResult;

    /// True when the tool never mutates state and may run concurrently.
    fn is_read_only(&self, _tool: &str) -> bool {
        false
    }

    /// True when the tool can destroy user data and needs the policy path.
    fn is_destructive(&self, _tool: &str) -> bool {
        true
    }

    /// Tools advertised to the model; defaults to none.
    fn available_tools(&self) -> Vec<ToolRef> {
        Vec::new()
    }
}

/// Decides the policy verdict for one tool call before execution.
#[async_trait::async_trait]
pub trait PolicyDecider: Send + Sync {
    /// Return allow/ask/deny for the given call.
    async fn decide(&self, call: &ToolCall) -> PolicyVerdict;

    /// Record a session-level approval for `call` after the user chose
    /// "always allow". Default: nothing (policies without a rule store
    /// degrade the decision to a one-shot allow).
    fn remember_always(&self, _call: &ToolCall) {}

    /// Switch the permission mode by wire name; false rejects the name.
    ///
    /// Defaults to rejecting: adapters over fixed policies override this
    /// only when a live mode handle exists behind them.
    fn set_permission_mode(&self, _mode: &str) -> bool {
        false
    }
}

/// Runs lifecycle hooks at one hook point.
#[async_trait::async_trait]
pub trait HookGateway: Send + Sync {
    /// Run hooks for the point; only blocking points may veto.
    async fn run(&self, point: HookPoint, payload: &str) -> HookReport;

    /// Run hooks for tool-scoped points with full call context.
    ///
    /// The default implementation drops tool context and delegates to
    /// [`HookGateway::run`]; adapters over hook engines with matcher
    /// filtering must override this to preserve match semantics.
    async fn run_tool(
        &self,
        point: HookPoint,
        tool: &str,
        input: &Value,
        output: Option<&str>,
    ) -> HookReport {
        let _ = (tool, input, output);
        self.run(point, "").await
    }
}

/// Incremental model output during one sample.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SampleDelta {
    /// Assistant text fragment as it arrives.
    Text(String),
    /// Extended-thinking fragment as it arrives (display-only: thinking is
    /// per-turn reasoning and never enters the conversation history).
    Thinking(String),
}

/// Samples the model once; implemented by the provider adapter.
#[async_trait::async_trait]
pub trait ModelGateway: Send + Sync {
    /// Sample the model; `PromptTooLong` signals the loop to compact.
    async fn sample(&self, request: SampleRequest) -> Result<SampleResponse, SampleError>;

    /// Switch the wire model name for subsequent samples; false rejects
    /// the name. Adapters over fixed models keep the default (rejecting);
    /// adapters with a swappable name override this. Same-provider
    /// switching only: cross-provider changes need session re-assembly.
    fn set_model(&self, _name: &str) -> bool {
        false
    }

    /// Switch the reasoning-effort level for subsequent samples; false
    /// rejects the level. Adapters over gateways with a mutable effort
    /// override this; fixed gateways keep the default (rejecting).
    fn set_thinking(&self, _effort: &str) -> bool {
        false
    }

    /// Sample with per-delta callbacks for live frontends.
    ///
    /// The default implementation replays the final text as a single
    /// delta, so adapters without true streaming stay compatible while
    /// frontends keep working unchanged.
    async fn sample_streaming(
        &self,
        request: SampleRequest,
        on_delta: &(dyn Fn(SampleDelta) + Send + Sync),
    ) -> Result<SampleResponse, SampleError> {
        let response = self.sample(request).await?;
        for block in &response.blocks {
            if let SampleBlock::Text(text) = block {
                on_delta(SampleDelta::Text(text.clone()));
            }
        }
        Ok(response)
    }
}

/// Target selecting which checkpoint consumes a steered message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SteerTarget {
    /// Applied at the next loop head (the next tool-round iteration).
    NextTurn,
    /// Applied immediately before the next model sample.
    NextStep,
}

/// Mid-turn message inbox behind a shared, cloneable handle.
///
/// Steering and injection ride normal history as user messages, so budget
/// checks, compaction, and pairing logic treat them like any other user
/// input. Frontends reach the loop through [`TurnDriver::inbox_handle`]
/// (the actor stores that handle on its client); direct holders of the
/// loop use [`RunLoop::steer`] / [`RunLoop::inject`].
#[derive(Debug, Clone, Default)]
pub struct InboxHandle {
    inner: Arc<Mutex<InboxQueue>>,
}

#[derive(Debug, Default)]
struct InboxQueue {
    next_turn: VecDeque<String>,
    next_step: VecDeque<String>,
    inject: VecDeque<String>,
}

impl InboxHandle {
    /// Create an empty inbox.
    pub fn new() -> Self {
        Self::default()
    }

    /// Queue a steering message for `target`.
    pub fn steer(&self, text: String, target: SteerTarget) {
        let mut inbox = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        match target {
            SteerTarget::NextTurn => inbox.next_turn.push_back(text),
            SteerTarget::NextStep => inbox.next_step.push_back(text),
        }
    }

    /// Queue a user message for the upcoming sample.
    pub fn inject(&self, text: String) {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .inject
            .push_back(text);
    }

    /// Drop queued (not yet applied) items and return the dropped count.
    /// With `keep_next_turn`, items aimed at the next turn survive while
    /// current-turn items drop; otherwise everything pending is dropped.
    pub fn cancel(&self, keep_next_turn: bool) -> usize {
        let mut inbox = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let mut dropped = inbox.next_step.len() + inbox.inject.len();
        inbox.next_step.clear();
        inbox.inject.clear();
        if !keep_next_turn {
            dropped += inbox.next_turn.len();
            inbox.next_turn.clear();
        }
        dropped
    }

    /// Drain items aimed at the loop head.
    fn take_next_turn(&self) -> Vec<String> {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .next_turn
            .drain(..)
            .collect()
    }

    /// Drain items aimed at the next sample.
    fn take_next_step(&self) -> Vec<String> {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .next_step
            .drain(..)
            .collect()
    }

    /// Drain directly injected messages.
    fn take_inject(&self) -> Vec<String> {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .inject
            .drain(..)
            .collect()
    }
}

/*
 * @file RunLoop
 * @description Single-turn agent loop over anticorruption seams (part 2).
 *
 * Responsibilities:
 * - Drive sample, decide, dispatch, and recover for one turn.
 * - Emit wire events in a fixed order with usage settle guarantees.
 * - Enforce interrupt checkpoints, round ceilings, and retry budgets.
 *
 * This module must not depend on: concrete tools, policy, hooks, models,
 * or transport. It sees Conversation snapshots, gateway traits, and wire
 * data only.
 *
 * Deliberate divergence from the legacy fast path: every tool call goes
 * through the policy gate, including read-only calls. The legacy pipeline
 * bypassed policy for the read-only batch (a documented deny-bypass
 * risk); parallelism is preserved, only the bypass is removed.
 */

/// Prompt appended when output truncation needs a continuation sample.
pub const CONTINUATION_PROMPT: &str =
    "Output token limit reached. Continue exactly where you left off.";

/// Maximum model continuations per turn (default for
/// [`RunConfig::max_continuations`]).
pub const MAX_CONTINUATIONS: u8 = 2;
/// Maximum plan-steering reminders per turn (default for
/// [`RunConfig::max_plan_nudges`]).
pub const MAX_PLAN_NUDGES: u8 = 3;
/// Maximum Stop-hook blocks per turn before the loop proceeds anyway
/// (default for [`RunConfig::max_stop_blocks`]).
pub const MAX_STOP_BLOCKS: u8 = 3;
/// Byte budget of the per-call output preview carried on the wire with
/// [`EventMsg::ToolCallEnd`]. Full results stay in conversation history;
/// this bound keeps event frames small for slow transports.
const TOOL_OUTPUT_PREVIEW_BYTES: usize = 4096;

/// Static run configuration, frozen per session rather than per turn.
#[derive(Debug, Clone)]
pub struct RunConfig {
    /// Provider model identifier for sampling.
    pub model_name: String,
    /// Context window in tokens for budget checks.
    pub context_window: u64,
    /// Per-sample output cap handed to the provider.
    pub max_output_tokens: u32,
    /// Tool dispatch rounds per turn; reaching it stops with
    /// [`StopReason::MaxToolRounds`].
    pub max_tool_rounds: u32,
    /// Model continuations per turn on output truncation.
    pub max_continuations: u8,
    /// Plan-steering reminders per turn while todos stay unfinished.
    pub max_plan_nudges: u8,
    /// Stop-hook blocks per turn before the loop proceeds anyway.
    pub max_stop_blocks: u8,
    /// Reactive compactions per turn on overlong prompts.
    pub max_reactive_compacts: u8,
}

/// User decision delivered for one parked approval request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApprovalResolution {
    /// Approve this call only.
    AllowOnce,
    /// Approve this call; session persistence is the implementation's job.
    AllowAlways,
    /// Refuse with a reason surfaced back to the model.
    Deny {
        /// Human-readable refusal reason.
        reason: String,
    },
    /// The wait was interrupted; the call must not execute.
    Interrupted,
}

/// User answer delivered for one parked interactive question.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QuestionResolution {
    /// The user picked an option or typed an answer; an empty string
    /// means the question was dismissed without answering.
    Answered(String),
    /// Nobody could answer (headless driver or expired wait): the
    /// question fails as a business error instead of executing.
    Unavailable {
        /// Human-readable reason for the UI/model.
        reason: String,
    },
    /// The wait was interrupted; the call must not execute.
    Interrupted,
}

/// Parks approval requests and delivers user decisions.
///
/// Timeout handling belongs to the implementation: an expired wait must
/// resolve to [`ApprovalResolution::Deny`], never park forever. Late
/// decisions for consumed ids must be dropped, never stored.
#[async_trait::async_trait]
pub trait ApprovalSource: Send + Sync {
    /// Block until a decision arrives for `call_id` or the wait expires.
    async fn decide(&self, call_id: &str, kind: AskKind, detail: &str) -> ApprovalResolution;

    /// Block until the user answers the parked question for `call_id`, or
    /// the wait expires. Default: nobody can answer (headless drivers),
    /// so implementations only override this when they can park.
    async fn ask(
        &self,
        _call_id: &str,
        _question: &str,
        _options: &[String],
    ) -> QuestionResolution {
        QuestionResolution::Unavailable {
            reason: "non-interactive session: nobody can answer questions".to_string(),
        }
    }

    /// Drop stale waiters, e.g. decisions that arrived after a turn ended.
    fn clear_stale(&self);
}

/// Read-only view of plan state for steering reminders.
pub trait PlanTracker: Send + Sync {
    /// Number of unfinished plan items.
    fn unfinished(&self) -> usize;

    /// Reminder text pushed to the model when items are unfinished.
    fn reminder(&self) -> String;
}

/// Result of one context compaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Compacted {
    /// Summary text replacing the compacted history.
    pub summary: String,
    /// Token estimate of the summary message.
    pub summary_tokens: u64,
}

/// Compaction failure modes.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CompactError {
    /// The summarizer failed; carries a human-readable cause.
    #[error("compaction failed: {0}")]
    Failed(String),
}

/// Compacts history into a summary; implemented by the summarizer adapter.
#[async_trait::async_trait]
pub trait Compactor: Send + Sync {
    /// Summarize `history` for `trigger`, preserving pairing order.
    async fn compact(
        &self,
        history: Vec<HistoryEntry>,
        trigger: CompactTrigger,
    ) -> Result<Compacted, CompactError>;
}

/// The turn state machine, generic over all seams for testability.
/// Per-run tool allowlist for turns sharing one driver.
///
/// Child turns run on the session driver, so a fork-scoped `allowed-tools`
/// set cannot ride a per-driver registry. The child service registers one
/// entry per task id; runs without an entry keep the full tool surface.
#[derive(Clone, Default, Debug)]
pub struct RunAllowlist {
    inner: Arc<Mutex<HashMap<String, HashSet<String>>>>,
}

impl RunAllowlist {
    /// Restrict one run to the named tools.
    pub fn restrict(&self, run_id: &str, tools: HashSet<String>) {
        self.lock().insert(run_id.to_string(), tools);
    }

    /// True when the run may execute the tool (no entry means unrestricted).
    pub fn is_allowed(&self, run_id: &str, tool: &str) -> bool {
        self.lock().get(run_id).is_none_or(|set| set.contains(tool))
    }

    /// Drop one run's restriction (child teardown).
    pub fn release(&self, run_id: &str) {
        self.lock().remove(run_id);
    }

    /// Recover the lock after a poison; inserts are single map writes with
    /// no half-written invariant.
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, HashSet<String>>> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// Per-run interrupt overrides for turns sharing one driver.
///
/// A child turn registers its own handle under its run id before driving;
/// [`RunLoop::run_turn`] consumes the entry at turn start, so a `task_stop`
/// bridges into exactly that child turn instead of the session-wide flag.
/// Entries are single-use: `take` removes them, and a registration that is
/// never driven is released by the spawning service's teardown guard.
#[derive(Clone, Default)]
pub struct RunInterrupts {
    inner: Arc<Mutex<HashMap<String, InterruptHandle>>>,
}

impl RunInterrupts {
    /// Bind one run id to its own interrupt handle.
    pub fn register(&self, run_id: &str, handle: InterruptHandle) {
        self.lock().insert(run_id.to_string(), handle);
    }

    /// Consume one run's handle; `None` keeps the driver-wide flag.
    fn take(&self, run_id: &str) -> Option<InterruptHandle> {
        self.lock().remove(run_id)
    }

    /// Drop one run's handle (teardown when the turn never consumed it).
    pub fn release(&self, run_id: &str) {
        self.lock().remove(run_id);
    }

    /// Recover the lock after a poison; inserts are single map writes with
    /// no half-written invariant.
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, InterruptHandle>> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// Effective interrupt for one turn: the session-wide flag plus an
/// optional run-scoped override.
///
/// Child turns observe both their own stop signal and a user interrupt of
/// the whole session. Turn entry resets nothing for a child turn — its
/// fresh handle arrives untriggered from the spawning service — so a
/// child turn start can never swallow a user interrupt that races it.
struct TurnInterrupt {
    session: InterruptHandle,
    run: Option<InterruptHandle>,
}

impl TurnInterrupt {
    fn is_triggered(&self) -> bool {
        self.session.is_triggered()
            || self
                .run
                .as_ref()
                .is_some_and(|handle| handle.is_triggered())
    }
}

pub struct RunLoop<E, P, H, M, A, T, C> {
    executor: E,
    policy: P,
    hooks: H,
    model: M,
    approvals: A,
    plans: T,
    compactor: C,
    cfg: RunConfig,
    interrupt: InterruptHandle,
    run_allowlist: RunAllowlist,
    run_interrupts: RunInterrupts,
    inbox: InboxHandle,
}

impl<E, P, H, M, A, T, C> RunLoop<E, P, H, M, A, T, C>
where
    E: ToolExecutor,
    P: PolicyDecider,
    H: HookGateway,
    M: ModelGateway,
    A: ApprovalSource,
    T: PlanTracker,
    C: Compactor,
{
    /// Assemble a loop over seam implementations and static config.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        executor: E,
        policy: P,
        hooks: H,
        model: M,
        approvals: A,
        plans: T,
        compactor: C,
        cfg: RunConfig,
        interrupt: InterruptHandle,
    ) -> Self {
        Self {
            executor,
            policy,
            hooks,
            model,
            approvals,
            plans,
            compactor,
            cfg,
            interrupt,
            run_allowlist: RunAllowlist::default(),
            run_interrupts: RunInterrupts::default(),
            inbox: InboxHandle::new(),
        }
    }

    /// Queue a steering message for `target`; empty texts are dropped
    /// because providers reject empty user messages.
    pub fn steer(&self, text: String, target: SteerTarget) {
        if !text.is_empty() {
            self.inbox.steer(text, target);
        }
    }

    /// Queue a user message for the upcoming sample; empty texts dropped.
    pub fn inject(&self, text: String) {
        if !text.is_empty() {
            self.inbox.inject(text);
        }
    }

    /// Drop queued inbox items, returning the dropped count; with
    /// `keep_next_turn`, next-turn steering survives.
    pub fn cancel_inbox(&self, keep_next_turn: bool) -> usize {
        self.inbox.cancel(keep_next_turn)
    }

    /// Handle to this loop's per-run tool allowlist for child services.
    pub fn run_allowlist(&self) -> RunAllowlist {
        self.run_allowlist.clone()
    }

    /// Per-run interrupt registry; the child task service registers one
    /// handle per spawned child so stop signals stay scoped to that turn.
    pub fn run_interrupts(&self) -> RunInterrupts {
        self.run_interrupts.clone()
    }

    /// Shared inbox handle; the actor stores this on its client so
    /// frontends can steer or inject mid-turn without touching the loop.
    pub fn inbox_handle(&self) -> InboxHandle {
        self.inbox.clone()
    }

    /// Run one turn to a terminal [`StopReason`].
    ///
    /// Event order is fixed: TurnStarted once, AgentMessageComplete per
    /// sample, ToolCallBegin all upfront in declaration order, ToolCallEnd
    /// all after in declaration order, TokenCount on every settle with a
    /// completed sample, TurnCompleted exactly once as the last event.
    /// Usage settle runs on every sampling exit; with no completed sample
    /// it is a no-op that neither covers the carry nor emits TokenCount.
    pub async fn run_turn(
        &self,
        ctx: &RunContext,
        conv: &mut Conversation,
        input: TurnInput<'_>,
        system: &str,
        on_event: &(dyn Fn(Event) + Send + Sync),
    ) -> StopReason {
        let emit_msg = |msg: EventMsg| {
            on_event(Event {
                id: ctx.submission_id.clone(),
                msg,
            })
        };
        // A registered run-scoped handle marks a child turn: it observes
        // its own stop signal plus the session-wide flag, but never resets
        // or clears shared state — resetting the session flag here would
        // swallow a user interrupt racing the child start, and clearing
        // the gates would drop a parent approval parked mid-wait.
        let run_interrupt = self.run_interrupts.take(&ctx.run_id);
        let owns_session_state = run_interrupt.is_none();
        let interrupt = TurnInterrupt {
            session: self.interrupt.clone(),
            run: run_interrupt,
        };
        if owns_session_state {
            self.approvals.clear_stale();
            self.interrupt.reset();
        }

        // Admission runs before TurnStarted: blocked input never enters
        // history and is never sampled.
        let admission = self.hooks.run(HookPoint::PromptSubmit, input.text).await;
        if !admission.allow {
            emit_msg(EventMsg::Error {
                message: admission.message,
                recoverable: true,
                code: Some("hook.blocked".to_string()),
            });
            emit_msg(EventMsg::TurnCompleted { interrupted: false });
            return StopReason::Completed;
        }
        if !admission.message.is_empty() {
            emit_msg(EventMsg::Warning {
                message: admission.message,
            });
        }

        if input.images.is_empty() {
            conv.push(Role::User, input.text);
        } else {
            let mut blocks = vec![state_store::Block::Text(input.text.to_string())];
            for image in input.images {
                blocks.push(state_store::Block::Image {
                    id: image.id.clone(),
                    mime: image.mime.clone(),
                    base64: image.base64.clone(),
                });
            }
            conv.push_blocks(Role::User, blocks);
        }
        emit_msg(EventMsg::TurnStarted);

        let mut warned = false;
        let mut compacted = false;
        let mut continuations: u8 = 0;
        let mut nudges: u8 = 0;
        let mut stop_blocks: u8 = 0;
        let mut reactive_compacts: u8 = 0;
        let mut state = TurnState::new();
        let mut last_input: Option<u64> = None;
        let mut estimate_cache = EstimateCache::default();

        loop {
            // Checkpoint 1: loop head interrupt returns without sampling.
            if interrupt.is_triggered() {
                settle(
                    conv,
                    &last_input,
                    &state,
                    self.cfg.context_window,
                    &emit_msg,
                );
                emit_msg(EventMsg::TurnCompleted { interrupted: true });
                return StopReason::Interrupted;
            }

            // Mid-turn steering (NextTurn target): queued steer messages
            // land as user history at the loop head, ahead of the budget
            // line and the next sample.
            let mut applied = 0;
            for text in self.inbox.take_next_turn() {
                conv.push(Role::User, text);
                applied += 1;
            }

            // Round ceiling stops with a settled turn, never with an error.
            // Checked before the budget line so a zero ceiling melts the
            // very first iteration. The stop reason names the ceiling so
            // callers can tell it from a natural completion.
            if state.rounds_exhausted(self.cfg.max_tool_rounds) {
                emit_msg(EventMsg::Warning {
                    message: format!(
                        "tool round limit reached ({}); stopping this turn",
                        self.cfg.max_tool_rounds
                    ),
                });
                settle(
                    conv,
                    &last_input,
                    &state,
                    self.cfg.context_window,
                    &emit_msg,
                );
                emit_msg(EventMsg::TurnCompleted { interrupted: false });
                return StopReason::MaxToolRounds;
            }

            // Pre-turn budget check with per-turn once-only warning and
            // auto-compaction flags (locals, never carried across turns).
            let used = match last_input {
                Some(input_tokens) => input_tokens + state.total_output_tokens,
                None => {
                    let carry = conv.usage_carry();
                    if carry.input_tokens > 0 {
                        carry.input_tokens + carry.output_tokens
                    } else {
                        estimate_cache.estimate(conv) + CONTEXT_OVERHEAD_TOKENS
                    }
                }
            };
            let remaining = self.cfg.context_window.saturating_sub(used);
            match check_budget(remaining) {
                BudgetLevel::Ok => {}
                BudgetLevel::Warn => {
                    if !warned {
                        warned = true;
                        emit_msg(EventMsg::Warning {
                            message: format!(
                                "context near limit: {used}/{} tokens used",
                                self.cfg.context_window
                            ),
                        });
                    }
                }
                level @ (BudgetLevel::AutoCompact | BudgetLevel::Blocking) => {
                    if compacted {
                        if !warned {
                            warned = true;
                            emit_msg(EventMsg::Warning {
                                message: format!(
                                    "context still near limit after compaction: {used}/{} tokens",
                                    self.cfg.context_window
                                ),
                            });
                        }
                    } else {
                        compacted = true;
                        let trigger = if level == BudgetLevel::Blocking {
                            CompactTrigger::Blocking
                        } else {
                            CompactTrigger::Auto
                        };
                        let blocking = trigger == CompactTrigger::Blocking;
                        match self.do_compact(conv, trigger, &emit_msg).await {
                            Ok(()) => continue,
                            Err(cause) => {
                                // Blocking failures abort; automatic
                                // failures downgrade to a warning.
                                if blocking {
                                    settle(
                                        conv,
                                        &last_input,
                                        &state,
                                        self.cfg.context_window,
                                        &emit_msg,
                                    );
                                    emit_msg(EventMsg::Error {
                                        message: cause,
                                        recoverable: false,
                                        code: Some("compact.failed".to_string()),
                                    });
                                    emit_msg(EventMsg::TurnCompleted { interrupted: false });
                                    return StopReason::Error(
                                        "blocking compaction failed".to_string(),
                                    );
                                }
                                emit_msg(EventMsg::Warning {
                                    message: format!("auto compaction failed, continuing: {cause}"),
                                });
                            }
                        }
                    }
                }
            }

            // Pre-sample steering (NextStep target) plus direct injections:
            // both land as user history immediately before the next sample
            // so they steer the upcoming call, after the budget line.
            for text in self
                .inbox
                .take_next_step()
                .into_iter()
                .chain(self.inbox.take_inject())
            {
                conv.push(Role::User, text);
                applied += 1;
            }
            if applied > 0 {
                emit_msg(EventMsg::Warning {
                    message: format!("applied {applied} steered message(s)"),
                });
            }

            let request = SampleRequest {
                system: system.to_string(),
                messages: history_messages(conv),
                // Restricted runs never see denied tools: the model plans
                // within its surface instead of hitting refusals.
                tools: self
                    .executor
                    .available_tools()
                    .into_iter()
                    .filter(|tool| self.run_allowlist.is_allowed(&ctx.run_id, &tool.name))
                    .collect(),
            };
            // Live deltas stream to frontends ahead of the assembled
            // message; ordering (deltas before AgentMessageComplete) is
            // part of the event contract.
            let response = match self
                .model
                .sample_streaming(request, &|delta| match delta {
                    SampleDelta::Text(text) => {
                        emit_msg(EventMsg::AgentMessageDelta { text });
                    }
                    SampleDelta::Thinking(text) => {
                        emit_msg(EventMsg::AgentThinkingDelta { text });
                    }
                })
                .await
            {
                Err(SampleError::PromptTooLong) => {
                    reactive_compacts += 1;
                    if reactive_compacts >= self.cfg.max_reactive_compacts {
                        settle(
                            conv,
                            &last_input,
                            &state,
                            self.cfg.context_window,
                            &emit_msg,
                        );
                        emit_msg(EventMsg::Error {
                            message: format!(
                                "prompt exceeds context window after {} compactions",
                                self.cfg.max_reactive_compacts
                            ),
                            recoverable: false,
                            code: Some("context.overflow".to_string()),
                        });
                        emit_msg(EventMsg::TurnCompleted { interrupted: false });
                        return StopReason::Error("prompt too long".to_string());
                    }
                    if let Err(cause) = self
                        .do_compact(conv, CompactTrigger::Reactive, &emit_msg)
                        .await
                    {
                        settle(
                            conv,
                            &last_input,
                            &state,
                            self.cfg.context_window,
                            &emit_msg,
                        );
                        emit_msg(EventMsg::Error {
                            message: cause,
                            recoverable: false,
                            code: Some("compact.failed".to_string()),
                        });
                        emit_msg(EventMsg::TurnCompleted { interrupted: false });
                        return StopReason::Error("reactive compaction failed".to_string());
                    }
                    continue;
                }
                Err(other) => {
                    settle(
                        conv,
                        &last_input,
                        &state,
                        self.cfg.context_window,
                        &emit_msg,
                    );
                    let code = match &other {
                        SampleError::Timeout => "provider.timeout",
                        SampleError::PromptTooLong => "context.overflow",
                        SampleError::Transport(_) => "provider.error",
                    };
                    emit_msg(EventMsg::Error {
                        message: other.to_string(),
                        recoverable: false,
                        code: Some(code.to_string()),
                    });
                    emit_msg(EventMsg::TurnCompleted { interrupted: false });
                    return StopReason::Error(other.to_string());
                }
                Ok(response) => {
                    reactive_compacts = 0;
                    response
                }
            };
            last_input = response.input_tokens;
            if let Some(output) = response.output_tokens {
                state.add_output(output);
            }
            state.total_cache_read_tokens = state
                .total_cache_read_tokens
                .saturating_add(response.cache_read_tokens);
            state.total_cache_creation_tokens = state
                .total_cache_creation_tokens
                .saturating_add(response.cache_creation_tokens);

            // The assistant entry keeps every block (text and tool calls)
            // so providers see the request/result pairing on the next
            // sample instead of a flattened text rendering.
            let mut text = String::new();
            let mut blocks = Vec::new();
            let mut calls = Vec::new();
            for block in &response.blocks {
                match block {
                    SampleBlock::Text(fragment) => text.push_str(fragment),
                    SampleBlock::ToolUse {
                        call_id,
                        name,
                        input,
                    } => calls.push(ToolCall {
                        call_id: call_id.clone(),
                        name: name.clone(),
                        input: input.clone(),
                    }),
                    SampleBlock::ToolResult { .. } => {}
                    // Responses never carry images today; if one ever
                    // does, the block still belongs in history.
                    SampleBlock::Image { .. } => {}
                }
                if !matches!(block, SampleBlock::ToolResult { .. }) {
                    blocks.push(block.clone());
                }
            }
            if !blocks.is_empty() {
                conv.push_blocks(Role::Assistant, blocks);
            }
            emit_msg(EventMsg::AgentMessageComplete { text });

            if calls.is_empty() {
                if response.truncated && continuations < self.cfg.max_continuations {
                    continuations += 1;
                    emit_msg(EventMsg::Warning {
                        message: format!(
                            "output truncated at max_tokens; continuing ({continuations}/{})",
                            self.cfg.max_continuations
                        ),
                    });
                    conv.push(Role::User, CONTINUATION_PROMPT);
                    continue;
                }
                let unfinished = self.plans.unfinished();
                if unfinished > 0 && nudges < self.cfg.max_plan_nudges {
                    nudges += 1;
                    emit_msg(EventMsg::Warning {
                        message: format!(
                            "plan has {unfinished} unfinished items; reminding the model"
                        ),
                    });
                    conv.push(Role::User, self.plans.reminder());
                    continue;
                }
                let stop = self.hooks.run(HookPoint::Stop, "").await;
                if !stop.allow && stop_blocks < self.cfg.max_stop_blocks {
                    stop_blocks += 1;
                    let reason = if stop.message.is_empty() {
                        "(hook gave no reason)".to_string()
                    } else {
                        stop.message.clone()
                    };
                    emit_msg(EventMsg::Warning {
                        message: format!(
                            "Stop hook blocked turn completion ({stop_blocks}/{})",
                            self.cfg.max_stop_blocks
                        ),
                    });
                    conv.push(
                        Role::User,
                        format!("A Stop hook blocked turn completion:\n{reason}"),
                    );
                    continue;
                }
                if stop.allow && !stop.message.is_empty() {
                    emit_msg(EventMsg::Warning {
                        message: stop.message,
                    });
                }
                break;
            }

            // Checkpoint 3: pre-tool interrupt preserves pairing with
            // synthesized results instead of executing anything.
            if interrupt.is_triggered() {
                let results: Vec<ToolResult> = calls.iter().map(interrupted_result).collect();
                conv.push_blocks(Role::User, result_blocks(&results));
                settle(
                    conv,
                    &last_input,
                    &state,
                    self.cfg.context_window,
                    &emit_msg,
                );
                emit_msg(EventMsg::TurnCompleted { interrupted: true });
                return StopReason::Interrupted;
            }
            let (results, hook_contexts) = self
                .execute_calls(&ctx.run_id, &calls, &interrupt, &emit_msg)
                .await;
            conv.push_blocks(Role::User, result_blocks(&results));
            // Prompt-type hook contexts ride normal history as guidance,
            // kept separate from tool outputs by construction.
            if !hook_contexts.is_empty() {
                conv.push(Role::User, hook_contexts.join("\n\n"));
            }
            state.bump_tool_round();
        }

        settle(
            conv,
            &last_input,
            &state,
            self.cfg.context_window,
            &emit_msg,
        );
        emit_msg(EventMsg::TurnCompleted { interrupted: false });
        StopReason::Completed
    }

    /// Compact history behind CompactStarted/Completed events.
    async fn do_compact(
        &self,
        conv: &mut Conversation,
        trigger: CompactTrigger,
        emit: &(dyn Fn(EventMsg) + Send + Sync),
    ) -> Result<(), String> {
        let pre = self.hooks.run(HookPoint::PreCompact, "").await;
        if !pre.message.is_empty() {
            emit(EventMsg::Warning {
                message: pre.message,
            });
        }
        emit(EventMsg::CompactStarted {
            trigger: trigger_name(&trigger).to_string(),
        });
        let done = self
            .compactor
            .compact(history_messages(conv), trigger)
            .await
            .map_err(|e| e.to_string())?;
        conv.replace(vec![HistoryEntry {
            role: Role::User,
            blocks: vec![state_store::Block::Text(done.summary.clone())],
        }]);
        conv.settle(Usage {
            input_tokens: estimate_tokens(&done.summary) + CONTEXT_OVERHEAD_TOKENS,
            output_tokens: 0,
            ..Usage::default()
        });
        emit(EventMsg::CompactCompleted {
            summary_tokens: done.summary_tokens,
        });
        let post = self.hooks.run(HookPoint::PostCompact, "").await;
        if !post.message.is_empty() {
            emit(EventMsg::Warning {
                message: post.message,
            });
        }
        Ok(())
    }

    /// Run one hook point outside a turn (session lifecycle).
    ///
    /// Warnings surface as events; the allow verdict returns for drivers
    /// that gate behaviour on it.
    pub async fn run_hook_point(
        &self,
        point: HookPoint,
        payload: &str,
        emit: &(dyn Fn(EventMsg) + Send + Sync),
    ) -> bool {
        let report = self.hooks.run(point, payload).await;
        if !report.message.is_empty() {
            emit(EventMsg::Warning {
                message: report.message.clone(),
            });
        }
        report.allow
    }

    /// Compact the given conversation immediately (idle `/compact` path).
    pub async fn compact_now(
        &self,
        conv: &mut Conversation,
        trigger: CompactTrigger,
        emit: &(dyn Fn(EventMsg) + Send + Sync),
    ) -> Result<(), String> {
        self.do_compact(conv, trigger, emit).await
    }

    /// Dispatch one tool-use batch with policy, approvals, and hooks.
    ///
    /// All ToolCallBegin events go out upfront in declaration order; all
    /// ToolCallEnd events follow in declaration order. Every declared call
    /// gets exactly one result slot, so pairing can never break.
    ///
    /// Duplicate call ids (model misbehavior, never well-formed output)
    /// collapse at admission: the first occurrence runs the full pipeline
    /// while later ones fill error slots without executing, reaching
    /// policy, or touching the approval gate. Pairing results by id would
    /// otherwise consume one slot twice and panic on the second lookup.
    async fn execute_calls(
        &self,
        run_id: &str,
        calls: &[ToolCall],
        interrupt: &TurnInterrupt,
        emit: &(dyn Fn(EventMsg) + Send + Sync),
    ) -> (Vec<ToolResult>, Vec<String>) {
        // Prompt-type hook contexts from this dispatch batch (pre- and
        // post-tool); returned for the caller to fold into the
        // conversation, separated from tool outputs by construction.
        let mut hook_contexts: Vec<String> = Vec::new();
        for call in calls {
            emit(EventMsg::ToolCallBegin {
                call_id: call.call_id.clone(),
                name: call.name.clone(),
                input: call.input.clone(),
            });
        }

        let mut seen: HashSet<&str> = HashSet::with_capacity(calls.len());
        let mut dupes: HashMap<String, ToolResult> = HashMap::new();
        let mut unique: Vec<&ToolCall> = Vec::with_capacity(calls.len());
        for call in calls {
            if seen.insert(call.call_id.as_str()) {
                unique.push(call);
            } else if !dupes.contains_key(call.call_id.as_str()) {
                dupes.insert(
                    call.call_id.clone(),
                    ToolResult {
                        call_id: call.call_id.clone(),
                        content: format!(
                            "duplicate tool call id {:?}: first occurrence runs, this one is refused without executing",
                            call.call_id
                        ),
                        is_error: true,
                    },
                );
            }
        }

        // Pre-tool hooks run per call before policy; blocks never execute
        // and never reach the approval gate. Duplicates were already
        // collapsed above, so only first occurrences flow through here.
        let mut blocked: HashMap<String, ToolResult> = HashMap::new();
        let mut live: Vec<&ToolCall> = Vec::with_capacity(unique.len());
        for call in unique {
            // Fork-scoped `allowed-tools` enforcement: denied calls fail
            // as business errors so the model self-corrects, and never
            // reach hooks, policy, or the approval gate.
            if !self.run_allowlist.is_allowed(run_id, &call.name) {
                blocked.insert(
                    call.call_id.clone(),
                    ToolResult {
                        call_id: call.call_id.clone(),
                        content: format!("tool {:?} is not in this run's allowed tools", call.name),
                        is_error: true,
                    },
                );
                continue;
            }
            let report = self
                .hooks
                .run_tool(HookPoint::PreToolUse, &call.name, &call.input, None)
                .await;
            if !report.message.is_empty() {
                emit(EventMsg::Warning {
                    message: report.message.clone(),
                });
            }
            if !report.context.is_empty() {
                hook_contexts.push(format!(
                    "[hook:pre-tool-use {}] {}",
                    call.name, report.context
                ));
            }
            if report.allow {
                live.push(call);
            } else {
                blocked.insert(
                    call.call_id.clone(),
                    ToolResult {
                        call_id: call.call_id.clone(),
                        content: format!("blocked by PreToolUse hook: {}", report.message),
                        is_error: true,
                    },
                );
            }
        }

        let mut results: HashMap<String, ToolResult> = HashMap::new();
        let mut parallel: Vec<&ToolCall> = Vec::new();
        let mut serial: Vec<SerialCall<'_>> = Vec::new();
        for call in live {
            match self.policy.decide(call).await {
                PolicyVerdict::Allow => {
                    if self.executor.is_read_only(&call.name)
                        && !self.executor.is_destructive(&call.name)
                    {
                        parallel.push(call);
                    } else {
                        serial.push(SerialCall::Direct(call));
                    }
                }
                PolicyVerdict::Deny { reason } => {
                    results.insert(
                        call.call_id.clone(),
                        ToolResult {
                            call_id: call.call_id.clone(),
                            content: reason,
                            is_error: true,
                        },
                    );
                }
                PolicyVerdict::Ask { kind, detail } => {
                    emit(EventMsg::ApprovalRequested {
                        call_id: call.call_id.clone(),
                        kind: match kind {
                            AskKind::Exec => WireApprovalKind::Exec,
                            AskKind::Write => WireApprovalKind::Write,
                        },
                        detail: detail.clone(),
                    });
                    serial.push(SerialCall::Approval { call, kind, detail });
                }
                PolicyVerdict::Question { question, options } => {
                    emit(EventMsg::QuestionRequested {
                        call_id: call.call_id.clone(),
                        question: question.clone(),
                        options: options.clone(),
                    });
                    serial.push(SerialCall::Question {
                        call,
                        question,
                        options,
                    });
                }
            }
        }

        // Read-only allows run concurrently, but capped: a model-declared
        // flood of fetches or searches must not exhaust file descriptors
        // or trip remote rate limits with unbounded parallelism. Order
        // within the batch still follows declaration order via the result
        // map below (unordered completion, keyed insertion).
        const READ_ONLY_CONCURRENCY: usize = 8;
        // Own the calls up front so the queued futures carry no borrow of
        // the declaration slice (and rustc's closure-variance inference
        // stays out of the way).
        let owned: Vec<ToolCall> = parallel.into_iter().cloned().collect();
        let concurrent: Vec<(String, ToolCall, ToolResult)> = futures::stream::iter(owned)
            .map(|call| async move {
                let id = call.call_id.clone();
                let output = self.executor.execute(call.clone()).await;
                (id, call, output)
            })
            .buffer_unordered(READ_ONLY_CONCURRENCY)
            .collect()
            .await;
        for (id, call, output) in concurrent {
            if let Some(context) = self.post_tool(&call, &output, emit).await {
                hook_contexts.push(format!("[hook:post-tool-use {}] {}", call.name, context));
            }
            results.insert(id, result_of(&call, output));
        }

        // Mutations, approvals, and questions run serially with an
        // interrupt check per item; the rest fill with interrupted results
        // instead of breaking, so every declared call still gets its slot.
        for item in serial {
            let (call, approved) = match item {
                SerialCall::Direct(call) => (call, true),
                SerialCall::Approval { call, kind, detail } => {
                    match self.approvals.decide(&call.call_id, kind, &detail).await {
                        ApprovalResolution::AllowOnce => (call, true),
                        ApprovalResolution::AllowAlways => {
                            // Session-level "always allow": the policy records
                            // its rule so later identical calls skip the ask.
                            self.policy.remember_always(call);
                            (call, true)
                        }
                        ApprovalResolution::Deny { reason } => {
                            results.insert(
                                call.call_id.clone(),
                                ToolResult {
                                    call_id: call.call_id.clone(),
                                    content: reason,
                                    is_error: true,
                                },
                            );
                            continue;
                        }
                        ApprovalResolution::Interrupted => {
                            results.insert(call.call_id.clone(), interrupted_result(call));
                            continue;
                        }
                    }
                }
                SerialCall::Question {
                    call,
                    question,
                    options,
                } => {
                    // The answer IS the tool result: the tool body never
                    // executes, so there is nothing to approve or run.
                    let resolution = self.approvals.ask(&call.call_id, &question, &options).await;
                    match resolution {
                        QuestionResolution::Answered(text) => {
                            let content = if text.trim().is_empty() {
                                "the user dismissed the question without answering".to_string()
                            } else {
                                text
                            };
                            results.insert(
                                call.call_id.clone(),
                                ToolResult {
                                    call_id: call.call_id.clone(),
                                    content,
                                    is_error: false,
                                },
                            );
                            continue;
                        }
                        QuestionResolution::Unavailable { reason } => {
                            results.insert(
                                call.call_id.clone(),
                                ToolResult {
                                    call_id: call.call_id.clone(),
                                    content: reason,
                                    is_error: true,
                                },
                            );
                            continue;
                        }
                        QuestionResolution::Interrupted => {
                            results.insert(call.call_id.clone(), interrupted_result(call));
                            continue;
                        }
                    }
                }
            };
            if !approved {
                continue;
            }
            if interrupt.is_triggered() {
                results.insert(call.call_id.clone(), interrupted_result(call));
                continue;
            }
            let output = self.executor.execute(call.clone()).await;
            if let Some(context) = self.post_tool(call, &output, emit).await {
                hook_contexts.push(format!("[hook:post-tool-use {}] {}", call.name, context));
            }
            results.insert(call.call_id.clone(), result_of(call, output));
        }

        // Merge hook-blocked slots, then emit ends and results strictly in
        // declaration order. Duplicates reuse their shared refusal slot;
        // the trailing fallback keeps pairing total even against future
        // pipeline changes that might skip a slot.
        results.extend(blocked);
        let mut ordered = Vec::with_capacity(calls.len());
        for call in calls {
            let result = results
                .remove(&call.call_id)
                .or_else(|| dupes.get(&call.call_id).cloned())
                .unwrap_or_else(|| ToolResult {
                    call_id: call.call_id.clone(),
                    content: "internal error: missing tool result slot".to_string(),
                    is_error: true,
                });
            emit(EventMsg::ToolCallEnd {
                call_id: result.call_id.clone(),
                is_error: result.is_error,
                output: Some(ToolCallPreview::head(
                    &result.content,
                    TOOL_OUTPUT_PREVIEW_BYTES,
                )),
            });
            ordered.push(result);
        }
        (ordered, hook_contexts)
    }

    /// Post-tool hooks fire only for executed calls and never block; a
    /// prompt-type hook's context is returned to the caller.
    async fn post_tool(
        &self,
        call: &ToolCall,
        output: &ToolResult,
        emit: &(dyn Fn(EventMsg) + Send + Sync),
    ) -> Option<String> {
        let report = self
            .hooks
            .run_tool(
                HookPoint::PostToolUse,
                &call.name,
                &call.input,
                Some(&output.content),
            )
            .await;
        if !report.message.is_empty() {
            emit(EventMsg::Warning {
                message: report.message,
            });
        }
        (!report.context.is_empty()).then_some(report.context)
    }
}

/// Turn-driving seam consumed by session actors.
///
/// The blanket implementation below wires every [`RunLoop`] automatically;
/// actors stay generic over this trait instead of the seven concrete seam
/// types, so transport never names capabilities.
/// The user input driving one turn: text plus optional inline images.
/// Images ride into conversation history as `Block::Image` and reach
/// vision-capable providers as image content parts; providers without
/// vision reject them at request translation.
pub struct TurnInput<'a> {
    pub text: &'a str,
    pub images: &'a [wavecode_wire::UserImage],
}

impl<'a> TurnInput<'a> {
    /// Text-only input (the common case).
    pub fn text(text: &'a str) -> Self {
        Self { text, images: &[] }
    }
}

#[async_trait::async_trait]
pub trait TurnDriver: Send + Sync {
    /// Drive one turn, emitting wire events through `on_event`.
    async fn drive_turn(
        &self,
        ctx: &RunContext,
        conv: &mut Conversation,
        input: TurnInput<'_>,
        system: &str,
        on_event: &(dyn Fn(Event) + Send + Sync),
    ) -> StopReason;

    /// Compact the conversation immediately (idle `/compact` path).
    async fn drive_compact(
        &self,
        conv: &mut Conversation,
        trigger: CompactTrigger,
        on_event: &(dyn Fn(Event) + Send + Sync),
    ) -> Result<(), String>;

    /// Run one hook point outside a turn (session lifecycle).
    async fn drive_hook(
        &self,
        point: HookPoint,
        payload: &str,
        on_event: &(dyn Fn(Event) + Send + Sync),
    ) -> bool;

    /// Switch the permission mode by wire name; false rejects the name.
    fn set_permission_mode(&self, _mode: &str) -> bool {
        false
    }

    /// Switch the sampling model by wire name; false rejects the name.
    ///
    /// Defaults to rejecting: the gateway behind the loop decides whether
    /// the name is acceptable (same-provider switches only).
    fn set_model(&self, _name: &str) -> bool {
        false
    }

    /// Switch the reasoning-effort level by wire value; false rejects it.
    ///
    /// Defaults to rejecting: the gateway behind the loop decides whether
    /// the level applies (mutable-effort gateways only).
    fn set_thinking(&self, _effort: &str) -> bool {
        false
    }

    /// Session teardown hook: the actor calls this once with the final
    /// transcript (one `role: text` line per entry) before returning from
    /// Shutdown or client disconnect. The default is a no-op; composition
    /// roots override it for best-effort end-of-session work (memory
    /// extraction) that must never block exit or fail the shutdown.
    async fn end_session(&self, _transcript: &[String]) {}

    /// Shared mid-turn inbox of the driven loop, if it exposes one. The
    /// actor stores this handle on its client so frontends can steer or
    /// inject without owning the driver loop.
    fn inbox_handle(&self) -> Option<InboxHandle> {
        None
    }
}

#[async_trait::async_trait]
impl<E, P, H, M, A, T, C> TurnDriver for RunLoop<E, P, H, M, A, T, C>
where
    E: ToolExecutor,
    P: PolicyDecider,
    H: HookGateway,
    M: ModelGateway,
    A: ApprovalSource,
    T: PlanTracker,
    C: Compactor,
{
    async fn drive_turn(
        &self,
        ctx: &RunContext,
        conv: &mut Conversation,
        input: TurnInput<'_>,
        system: &str,
        on_event: &(dyn Fn(Event) + Send + Sync),
    ) -> StopReason {
        self.run_turn(ctx, conv, input, system, on_event).await
    }

    async fn drive_compact(
        &self,
        conv: &mut Conversation,
        trigger: CompactTrigger,
        on_event: &(dyn Fn(Event) + Send + Sync),
    ) -> Result<(), String> {
        // The blanket impl stamps an empty correlation id; session actors
        // re-stamp with the serving submission id before forwarding.
        self.compact_now(conv, trigger, &|msg| {
            on_event(Event {
                id: String::new(),
                msg,
            })
        })
        .await
    }

    async fn drive_hook(
        &self,
        point: HookPoint,
        payload: &str,
        on_event: &(dyn Fn(Event) + Send + Sync),
    ) -> bool {
        self.run_hook_point(point, payload, &|msg| {
            on_event(Event {
                id: String::new(),
                msg,
            })
        })
        .await
    }

    fn set_permission_mode(&self, mode: &str) -> bool {
        self.policy.set_permission_mode(mode)
    }

    fn set_model(&self, name: &str) -> bool {
        self.model.set_model(name)
    }

    fn set_thinking(&self, effort: &str) -> bool {
        self.model.set_thinking(effort)
    }

    fn inbox_handle(&self) -> Option<InboxHandle> {
        Some(self.inbox.clone())
    }
}

/// Forwarding implementation so shared sources erase to trait objects.
#[async_trait::async_trait]
impl<T> ApprovalSource for std::sync::Arc<T>
where
    T: ApprovalSource,
{
    async fn decide(&self, call_id: &str, kind: AskKind, detail: &str) -> ApprovalResolution {
        self.as_ref().decide(call_id, kind, detail).await
    }

    fn clear_stale(&self) {
        self.as_ref().clear_stale();
    }
}
///
/// Composition roots hold `Arc<dyn TurnDriver>`; this blanket forward
/// keeps every concrete driver working unchanged behind the pointer.
#[async_trait::async_trait]
impl<T> TurnDriver for std::sync::Arc<T>
where
    T: TurnDriver,
{
    async fn drive_turn(
        &self,
        ctx: &RunContext,
        conv: &mut Conversation,
        input: TurnInput<'_>,
        system: &str,
        on_event: &(dyn Fn(Event) + Send + Sync),
    ) -> StopReason {
        self.as_ref()
            .drive_turn(ctx, conv, input, system, on_event)
            .await
    }

    async fn drive_compact(
        &self,
        conv: &mut Conversation,
        trigger: CompactTrigger,
        on_event: &(dyn Fn(Event) + Send + Sync),
    ) -> Result<(), String> {
        self.as_ref().drive_compact(conv, trigger, on_event).await
    }

    async fn drive_hook(
        &self,
        point: HookPoint,
        payload: &str,
        on_event: &(dyn Fn(Event) + Send + Sync),
    ) -> bool {
        self.as_ref().drive_hook(point, payload, on_event).await
    }

    fn set_permission_mode(&self, mode: &str) -> bool {
        self.as_ref().set_permission_mode(mode)
    }

    fn set_model(&self, name: &str) -> bool {
        self.as_ref().set_model(name)
    }

    fn set_thinking(&self, effort: &str) -> bool {
        self.as_ref().set_thinking(effort)
    }

    async fn end_session(&self, transcript: &[String]) {
        self.as_ref().end_session(transcript).await
    }

    fn inbox_handle(&self) -> Option<InboxHandle> {
        self.as_ref().inbox_handle()
    }
}

/// Serial dispatch item: direct execution, parked approval, or parked
/// question.
enum SerialCall<'a> {
    /// Policy allowed; execute after the interrupt check.
    Direct(&'a ToolCall),
    /// Policy asked; resolve through the approval source first.
    Approval {
        /// The parked call.
        call: &'a ToolCall,
        /// Approval kind for the decision prompt.
        kind: AskKind,
        /// Bounded detail string for display.
        detail: String,
    },
    /// Policy parked an interactive question; the user's answer becomes
    /// the tool result without executing the tool body.
    Question {
        /// The parked call.
        call: &'a ToolCall,
        /// The question text for display.
        question: String,
        /// Numbered answer options; empty when free text is expected.
        options: Vec<String>,
    },
}

/// Build the stored result for one executed call.
fn result_of(call: &ToolCall, output: ToolResult) -> ToolResult {
    ToolResult {
        call_id: call.call_id.clone(),
        content: output.content,
        is_error: output.is_error,
    }
}

/// Synthesize an interrupted result that preserves call pairing.
fn interrupted_result(call: &ToolCall) -> ToolResult {
    ToolResult {
        call_id: call.call_id.clone(),
        content: "interrupted by user".to_string(),
        is_error: true,
    }
}

/// Render tool results as history blocks, keeping ids for pairing.
fn result_blocks(results: &[ToolResult]) -> Vec<SampleBlock> {
    results
        .iter()
        .map(|r| SampleBlock::ToolResult {
            call_id: r.call_id.clone(),
            content: r.content.clone(),
            is_error: r.is_error,
        })
        .collect()
}

/// Settle usage after sampling: no completed sample means no-op, so the
/// carry is never covered and no TokenCount is emitted. `context_window`
/// rides along so frontends can render a context meter without config.
fn settle(
    conv: &mut Conversation,
    last_input: &Option<u64>,
    state: &TurnState,
    context_window: u64,
    emit: &(dyn Fn(EventMsg) + Send + Sync),
) {
    let Some(input) = last_input else {
        return;
    };
    conv.settle(Usage {
        input_tokens: *input,
        output_tokens: state.total_output_tokens,
        cache_read_tokens: state.total_cache_read_tokens,
        cache_creation_tokens: state.total_cache_creation_tokens,
    });
    emit(EventMsg::TokenCount {
        input_tokens: *input,
        output_tokens: state.total_output_tokens,
        cache_read_tokens: state.total_cache_read_tokens,
        cache_creation_tokens: state.total_cache_creation_tokens,
        context_window: Some(context_window),
        context_used: Some(input + state.total_output_tokens),
    });
}

/// Snapshot the conversation as model-facing messages.
fn history_messages(conv: &Conversation) -> Vec<HistoryEntry> {
    conv.snapshot().as_ref().clone()
}

/// Display name of one compaction trigger for event payloads.
fn trigger_name(trigger: &CompactTrigger) -> &'static str {
    match trigger {
        CompactTrigger::Auto => "auto",
        CompactTrigger::Blocking => "blocking",
        CompactTrigger::Reactive => "reactive",
        CompactTrigger::Manual { .. } => "manual",
    }
}

/// Incremental fallback token estimate for the run loop.
///
/// Re-flattening the whole history every tool round costs O(history)
/// per round (snapshot deep copy plus per-entry text builds); the cache
/// accounts each entry once and only scans growth. A shrink means the
/// history was replaced (compaction) or rewound — the estimate is a
/// fallback heuristic, so the one full re-scan after that is fine.
#[derive(Default)]
struct EstimateCache {
    entries: usize,
    tokens: u64,
}

impl EstimateCache {
    /// Token estimate of the full history, counting only new entries.
    fn estimate(&mut self, conv: &Conversation) -> u64 {
        conv.with_entries(|entries| {
            if entries.len() < self.entries {
                self.entries = 0;
                self.tokens = 0;
            }
            for entry in &entries[self.entries..] {
                self.tokens += estimate_tokens(&entry.text());
            }
            self.entries = entries.len();
            self.tokens
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Growth is accounted incrementally (each entry once); a shrink
    /// resets so the post-compaction history is re-estimated whole.
    #[test]
    fn estimate_cache_scans_only_growth_and_resets_on_shrink() {
        let mut conv = Conversation::new();
        let mut cache = EstimateCache::default();
        conv.push(Role::User, "hello");
        let after_one = cache.estimate(&conv);
        assert_eq!(after_one, estimate_tokens("hello"));
        // Re-estimating without growth returns the cached total.
        assert_eq!(cache.estimate(&conv), after_one);
        // Growth adds only the new entry.
        conv.push(Role::Assistant, "hi there");
        assert_eq!(
            cache.estimate(&conv),
            estimate_tokens("hello") + estimate_tokens("hi there")
        );
        // Compaction shrank the history: full re-scan of what remains.
        conv.replace(vec![HistoryEntry {
            role: Role::User,
            blocks: vec![state_store::Block::Text("summary only".to_string())],
        }]);
        assert_eq!(cache.estimate(&conv), estimate_tokens("summary only"));
    }

    #[test]
    fn turn_state_round_ceiling_is_a_stop_not_an_error() {
        let mut state = TurnState::new();
        for _ in 0..DEFAULT_MAX_TOOL_ROUNDS {
            state.bump_tool_round();
        }
        assert!(state.rounds_exhausted(DEFAULT_MAX_TOOL_ROUNDS));
    }

    #[test]
    fn idempotency_keys_are_namespaced_by_run() {
        let ctx = RunContext {
            run_id: "run-1".to_string(),
            submission_id: "sub-1".to_string(),
            input: "hi".to_string(),
            images: Vec::new(),
        };
        assert_eq!(ctx.idempotency_key("tools"), "run-1:tools");
        assert_ne!(
            ctx.idempotency_key("tools"),
            RunContext {
                run_id: "run-2".to_string(),
                ..ctx.clone()
            }
            .idempotency_key("tools")
        );
    }
}

/// Turn-level tests over scripted seams.
#[cfg(test)]
mod run_loop_tests {
    use super::*;
    use std::collections::{HashMap, VecDeque};
    use std::sync::Mutex;

    #[derive(Clone)]
    struct FakeExecutor {
        read_only: Vec<String>,
        results: HashMap<String, ToolResult>,
        executed: std::sync::Arc<Mutex<Vec<String>>>,
    }

    impl FakeExecutor {
        fn new(read_only: &[&str]) -> Self {
            Self {
                read_only: read_only.iter().map(|s| s.to_string()).collect(),
                results: HashMap::new(),
                executed: std::sync::Arc::new(Mutex::new(Vec::new())),
            }
        }

        fn lock_executed(&self) -> std::sync::MutexGuard<'_, Vec<String>> {
            self.executed.lock().unwrap_or_else(|e| e.into_inner())
        }
    }

    #[async_trait::async_trait]
    impl ToolExecutor for FakeExecutor {
        async fn execute(&self, call: ToolCall) -> ToolResult {
            self.lock_executed().push(call.name.clone());
            self.results
                .get(&call.call_id)
                .cloned()
                .unwrap_or(ToolResult {
                    call_id: call.call_id.clone(),
                    content: "ok".to_string(),
                    is_error: false,
                })
        }

        fn is_read_only(&self, tool: &str) -> bool {
            self.read_only.iter().any(|t| t == tool)
        }

        fn is_destructive(&self, _tool: &str) -> bool {
            false
        }

        fn available_tools(&self) -> Vec<ToolRef> {
            Vec::new()
        }
    }

    struct FakePolicy {
        verdicts: HashMap<String, PolicyVerdict>,
    }

    #[async_trait::async_trait]
    impl PolicyDecider for FakePolicy {
        async fn decide(&self, call: &ToolCall) -> PolicyVerdict {
            self.verdicts
                .get(&call.name)
                .cloned()
                .unwrap_or(PolicyVerdict::Allow)
        }
    }

    struct FakeHooks {
        submit_block: bool,
        stop_blocks_left: Mutex<u8>,
    }

    impl FakeHooks {
        fn lock_blocks(&self) -> std::sync::MutexGuard<'_, u8> {
            self.stop_blocks_left
                .lock()
                .unwrap_or_else(|e| e.into_inner())
        }
    }

    #[async_trait::async_trait]
    impl HookGateway for FakeHooks {
        async fn run(&self, point: HookPoint, _payload: &str) -> HookReport {
            match point {
                HookPoint::PromptSubmit if self.submit_block => HookReport {
                    allow: false,
                    message: "nope".to_string(),
                    context: String::new(),
                },
                HookPoint::Stop => {
                    let mut left = self.lock_blocks();
                    if *left > 0 {
                        *left -= 1;
                        return HookReport {
                            allow: false,
                            message: "stop it".to_string(),
                            context: String::new(),
                        };
                    }
                    HookReport {
                        allow: true,
                        message: String::new(),
                        context: String::new(),
                    }
                }
                _ => HookReport {
                    allow: true,
                    message: String::new(),
                    context: String::new(),
                },
            }
        }
    }

    enum ModelStep {
        Answer(SampleResponse),
        Fail(SampleError),
    }

    struct FakeModel {
        steps: Mutex<VecDeque<ModelStep>>,
        samples: Mutex<usize>,
    }

    #[async_trait::async_trait]
    impl ModelGateway for FakeModel {
        async fn sample(&self, _request: SampleRequest) -> Result<SampleResponse, SampleError> {
            *self.samples.lock().unwrap_or_else(|e| e.into_inner()) += 1;
            match self
                .steps
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .pop_front()
            {
                Some(ModelStep::Answer(response)) => Ok(response),
                Some(ModelStep::Fail(error)) => Err(error),
                None => Ok(SampleResponse {
                    blocks: Vec::new(),
                    input_tokens: Some(5),
                    output_tokens: Some(1),
                    truncated: false,
                    cache_read_tokens: 0,
                    cache_creation_tokens: 0,
                }),
            }
        }
    }

    struct FakeApprovals {
        resolutions: HashMap<String, ApprovalResolution>,
        answers: HashMap<String, QuestionResolution>,
    }

    #[async_trait::async_trait]
    impl ApprovalSource for FakeApprovals {
        async fn decide(&self, call_id: &str, _kind: AskKind, _detail: &str) -> ApprovalResolution {
            self.resolutions
                .get(call_id)
                .cloned()
                .unwrap_or(ApprovalResolution::Deny {
                    reason: "test deny".to_string(),
                })
        }

        async fn ask(
            &self,
            call_id: &str,
            _question: &str,
            _options: &[String],
        ) -> QuestionResolution {
            self.answers
                .get(call_id)
                .cloned()
                .unwrap_or(QuestionResolution::Unavailable {
                    reason: "test: nobody answered".to_string(),
                })
        }

        fn clear_stale(&self) {}
    }

    struct FakePlans {
        unfinished: usize,
    }

    impl PlanTracker for FakePlans {
        fn unfinished(&self) -> usize {
            self.unfinished
        }

        fn reminder(&self) -> String {
            "PLAN-REMINDER".to_string()
        }
    }

    struct FakeCompactor {
        fail: bool,
        calls: std::sync::Arc<Mutex<Vec<CompactTrigger>>>,
    }

    #[async_trait::async_trait]
    impl Compactor for FakeCompactor {
        async fn compact(
            &self,
            _history: Vec<HistoryEntry>,
            trigger: CompactTrigger,
        ) -> Result<Compacted, CompactError> {
            self.calls
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(trigger);
            if self.fail {
                return Err(CompactError::Failed("boom".to_string()));
            }
            Ok(Compacted {
                summary: "summary".to_string(),
                summary_tokens: 10,
            })
        }
    }

    struct Fixture {
        events: std::sync::Arc<Mutex<Vec<Event>>>,
        interrupt: InterruptHandle,
        ctx: RunContext,
    }

    impl Fixture {
        fn new() -> Self {
            Self {
                events: std::sync::Arc::new(Mutex::new(Vec::new())),
                interrupt: InterruptHandle::new(),
                ctx: RunContext {
                    run_id: "run-1".to_string(),
                    submission_id: "sub-1".to_string(),
                    input: "hi".to_string(),
                    images: Vec::new(),
                },
            }
        }

        fn lock_events(&self) -> std::sync::MutexGuard<'_, Vec<Event>> {
            self.events.lock().unwrap_or_else(|e| e.into_inner())
        }

        fn event_kinds(&self) -> Vec<String> {
            self.lock_events()
                .iter()
                .map(|e| {
                    serde_json::to_value(&e.msg)
                        .unwrap()
                        .get("type")
                        .unwrap()
                        .as_str()
                        .unwrap()
                        .to_string()
                })
                .collect()
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn build_loop<M: ModelGateway, P: PolicyDecider>(
        executor: FakeExecutor,
        policy: P,
        hooks: FakeHooks,
        model: M,
        approvals: FakeApprovals,
        plans: FakePlans,
        compactor: FakeCompactor,
        max_tool_rounds: u32,
        interrupt: InterruptHandle,
    ) -> RunLoop<FakeExecutor, P, FakeHooks, M, FakeApprovals, FakePlans, FakeCompactor> {
        RunLoop::new(
            executor,
            policy,
            hooks,
            model,
            approvals,
            plans,
            compactor,
            RunConfig {
                model_name: "test".to_string(),
                context_window: 200_000,
                max_output_tokens: 100,
                max_tool_rounds,
                max_continuations: MAX_CONTINUATIONS,
                max_plan_nudges: MAX_PLAN_NUDGES,
                max_stop_blocks: MAX_STOP_BLOCKS,
                max_reactive_compacts: MAX_REACTIVE_COMPACTS,
            },
            interrupt,
        )
    }

    fn text_response(text: &str) -> SampleResponse {
        SampleResponse {
            blocks: vec![SampleBlock::Text(text.to_string())],
            input_tokens: Some(10),
            output_tokens: Some(1),
            truncated: false,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
        }
    }

    fn default_parts() -> (
        FakeExecutor,
        FakePolicy,
        FakeHooks,
        FakeModel,
        FakeApprovals,
        FakePlans,
        FakeCompactor,
    ) {
        (
            FakeExecutor::new(&["read_file"]),
            FakePolicy {
                verdicts: HashMap::new(),
            },
            FakeHooks {
                submit_block: false,
                stop_blocks_left: Mutex::new(0),
            },
            FakeModel {
                steps: Mutex::new(VecDeque::new()),
                samples: Mutex::new(0),
            },
            FakeApprovals {
                resolutions: HashMap::new(),
                answers: HashMap::new(),
            },
            FakePlans { unfinished: 0 },
            FakeCompactor {
                fail: false,
                calls: std::sync::Arc::new(Mutex::new(Vec::new())),
            },
        )
    }

    #[tokio::test]
    async fn blocked_submit_never_samples_nor_starts() {
        let fx = Fixture::new();
        let (exec, policy, _, model, approvals, plans, compactor) = default_parts();
        let hooks = FakeHooks {
            submit_block: true,
            stop_blocks_left: Mutex::new(0),
        };
        let conv = &mut Conversation::new();
        let events = fx.events.clone();
        let outcome = build_loop(
            exec,
            policy,
            hooks,
            model,
            approvals,
            plans,
            compactor,
            8,
            fx.interrupt.clone(),
        )
        .run_turn(&fx.ctx, conv, TurnInput::text("hi"), "sys", &|e| {
            events.lock().unwrap_or_else(|e| e.into_inner()).push(e);
        })
        .await;
        assert_eq!(outcome, StopReason::Completed);
        assert!(conv.is_empty());
        let kinds = fx.event_kinds();
        assert!(!kinds.contains(&"turn_started".to_string()));
        assert!(kinds.contains(&"error".to_string()));
        assert_eq!(kinds.last().unwrap(), "turn_completed");
    }

    #[tokio::test]
    async fn text_turn_settles_usage_and_completes() {
        let fx = Fixture::new();
        let (exec, policy, hooks, model, approvals, plans, compactor) = default_parts();
        model
            .steps
            .lock()
            .unwrap()
            .push_back(ModelStep::Answer(text_response("hello")));
        let conv = &mut Conversation::new();
        let events = fx.events.clone();
        let outcome = build_loop(
            exec,
            policy,
            hooks,
            model,
            approvals,
            plans,
            compactor,
            8,
            fx.interrupt.clone(),
        )
        .run_turn(&fx.ctx, conv, TurnInput::text("hi"), "sys", &|e| {
            events.lock().unwrap_or_else(|e| e.into_inner()).push(e);
        })
        .await;
        assert_eq!(outcome, StopReason::Completed);
        assert_eq!(conv.len(), 2);
        let kinds = fx.event_kinds();
        assert_eq!(kinds.first().unwrap(), "turn_started");
        assert!(kinds.contains(&"agent_message_complete".to_string()));
        assert!(kinds.contains(&"token_count".to_string()));
        assert_eq!(kinds.last().unwrap(), "turn_completed");
        assert!(conv.usage_carry().input_tokens > 0);
        // Deltas stream ahead of the assembled message (default replay
        // emits the final text as one delta through the loop contract).
        let delta_pos = kinds
            .iter()
            .position(|k| k == "agent_message_delta")
            .unwrap();
        let complete_pos = kinds
            .iter()
            .position(|k| k == "agent_message_complete")
            .unwrap();
        assert!(delta_pos < complete_pos);
    }

    #[tokio::test]
    async fn approved_tool_executes_and_denied_tool_skips() {
        let fx = Fixture::new();
        let (exec, _, hooks, model, _, plans, compactor) = default_parts();
        let mut verdicts = HashMap::new();
        verdicts.insert(
            "shell".to_string(),
            PolicyVerdict::Ask {
                kind: AskKind::Exec,
                detail: "run ls".to_string(),
            },
        );
        verdicts.insert(
            "write_file".to_string(),
            PolicyVerdict::Deny {
                reason: "nope".to_string(),
            },
        );
        let policy = FakePolicy { verdicts };
        let mut resolutions = HashMap::new();
        resolutions.insert("c1".to_string(), ApprovalResolution::AllowOnce);
        let approvals = FakeApprovals {
            resolutions,
            answers: HashMap::new(),
        };
        model
            .steps
            .lock()
            .unwrap()
            .push_back(ModelStep::Answer(SampleResponse {
                blocks: vec![
                    SampleBlock::ToolUse {
                        call_id: "c1".to_string(),
                        name: "shell".to_string(),
                        input: serde_json::Value::Null,
                    },
                    SampleBlock::ToolUse {
                        call_id: "c2".to_string(),
                        name: "write_file".to_string(),
                        input: serde_json::Value::Null,
                    },
                ],
                input_tokens: Some(10),
                output_tokens: Some(1),
                truncated: false,
                cache_read_tokens: 0,
                cache_creation_tokens: 0,
            }));
        model
            .steps
            .lock()
            .unwrap()
            .push_back(ModelStep::Answer(text_response("done")));
        let conv = &mut Conversation::new();
        let events = fx.events.clone();
        let outcome = build_loop(
            exec,
            policy,
            hooks,
            model,
            approvals,
            plans,
            compactor,
            8,
            fx.interrupt.clone(),
        )
        .run_turn(&fx.ctx, conv, TurnInput::text("hi"), "sys", &|e| {
            events.lock().unwrap_or_else(|e| e.into_inner()).push(e);
        })
        .await;
        assert_eq!(outcome, StopReason::Completed);
        let kinds = fx.event_kinds();
        assert!(kinds.contains(&"approval_requested".to_string()));
        assert_eq!(kinds.iter().filter(|k| *k == "tool_call_begin").count(), 2);
        assert_eq!(kinds.iter().filter(|k| *k == "tool_call_end").count(), 2);
        // Denied tools report errors without executing; history keeps both.
        let history = conv
            .snapshot()
            .iter()
            .map(|e| e.text())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(history.contains("nope"));
        assert!(history.contains("[c2] error"));
    }

    /// Policy that asks for one tool and records every "always allow"
    /// callback (the session-rule seam under test).
    struct RecordingPolicy {
        verdicts: HashMap<String, PolicyVerdict>,
        always: Arc<std::sync::Mutex<Vec<String>>>,
    }

    #[async_trait::async_trait]
    impl PolicyDecider for RecordingPolicy {
        async fn decide(&self, call: &ToolCall) -> PolicyVerdict {
            self.verdicts
                .get(&call.name)
                .cloned()
                .unwrap_or(PolicyVerdict::Allow)
        }

        fn remember_always(&self, call: &ToolCall) {
            self.always
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(call.name.clone());
        }
    }

    #[tokio::test]
    async fn allow_always_resolution_records_a_session_rule() {
        let fx = Fixture::new();
        let (exec, _, hooks, model, _, plans, compactor) = default_parts();
        let mut verdicts = HashMap::new();
        verdicts.insert(
            "shell".to_string(),
            PolicyVerdict::Ask {
                kind: AskKind::Exec,
                detail: "run ls".to_string(),
            },
        );
        let always = Arc::new(std::sync::Mutex::new(Vec::new()));
        let policy = RecordingPolicy {
            verdicts,
            always: always.clone(),
        };
        let mut resolutions = HashMap::new();
        resolutions.insert("c1".to_string(), ApprovalResolution::AllowAlways);
        let approvals = FakeApprovals {
            resolutions,
            answers: HashMap::new(),
        };
        model
            .steps
            .lock()
            .unwrap()
            .push_back(ModelStep::Answer(SampleResponse {
                blocks: vec![SampleBlock::ToolUse {
                    call_id: "c1".to_string(),
                    name: "shell".to_string(),
                    input: serde_json::Value::Null,
                }],
                input_tokens: Some(10),
                output_tokens: Some(1),
                truncated: false,
                cache_read_tokens: 0,
                cache_creation_tokens: 0,
            }));
        model
            .steps
            .lock()
            .unwrap()
            .push_back(ModelStep::Answer(text_response("done")));
        let conv = &mut Conversation::new();
        let events = fx.events.clone();
        let outcome = build_loop(
            exec,
            policy,
            hooks,
            model,
            approvals,
            plans,
            compactor,
            8,
            fx.interrupt.clone(),
        )
        .run_turn(&fx.ctx, conv, TurnInput::text("hi"), "sys", &|e| {
            events.lock().unwrap_or_else(|e| e.into_inner()).push(e);
        })
        .await;
        assert_eq!(outcome, StopReason::Completed);
        assert_eq!(
            *always.lock().unwrap_or_else(|e| e.into_inner()),
            vec!["shell".to_string()],
            "an always-allow decision must reach the policy's rule store"
        );
        // The approved call still executes: the resolution is an allow.
        let kinds = fx.event_kinds();
        assert!(kinds.contains(&"tool_call_begin".to_string()));
    }

    #[tokio::test]
    async fn allowed_calls_run_read_only_first_then_mutations() {
        let fx = Fixture::new();
        let (exec, policy, hooks, model, approvals, plans, compactor) = default_parts();
        model
            .steps
            .lock()
            .unwrap()
            .push_back(ModelStep::Answer(SampleResponse {
                blocks: vec![
                    SampleBlock::ToolUse {
                        call_id: "m1".to_string(),
                        name: "write_file".to_string(),
                        input: serde_json::Value::Null,
                    },
                    SampleBlock::ToolUse {
                        call_id: "r1".to_string(),
                        name: "read_file".to_string(),
                        input: serde_json::Value::Null,
                    },
                    SampleBlock::ToolUse {
                        call_id: "r2".to_string(),
                        name: "read_file".to_string(),
                        input: serde_json::Value::Null,
                    },
                ],
                input_tokens: Some(10),
                output_tokens: Some(1),
                truncated: false,
                cache_read_tokens: 0,
                cache_creation_tokens: 0,
            }));
        model
            .steps
            .lock()
            .unwrap()
            .push_back(ModelStep::Answer(text_response("done")));
        let conv = &mut Conversation::new();
        let run = build_loop(
            exec,
            policy,
            hooks,
            model,
            approvals,
            plans,
            compactor,
            8,
            fx.interrupt.clone(),
        );
        let outcome = run
            .run_turn(&fx.ctx, conv, TurnInput::text("hi"), "sys", &|_| {})
            .await;
        assert_eq!(outcome, StopReason::Completed);
        // Read-only allows execute before mutations; results still come
        // back in declaration order.
        let executed = run.executor.lock_executed();
        assert_eq!(
            *executed,
            vec![
                "read_file".to_string(),
                "read_file".to_string(),
                "write_file".to_string()
            ]
        );
    }

    #[tokio::test]
    async fn restricted_runs_refuse_denied_tools_without_executing() {
        let fx = Fixture::new();
        let (exec, policy, hooks, model, approvals, plans, compactor) = default_parts();
        model
            .steps
            .lock()
            .unwrap()
            .push_back(ModelStep::Answer(SampleResponse {
                blocks: vec![SampleBlock::ToolUse {
                    call_id: "c1".to_string(),
                    name: "shell".to_string(),
                    input: serde_json::Value::Null,
                }],
                input_tokens: Some(10),
                output_tokens: Some(1),
                truncated: false,
                cache_read_tokens: 0,
                cache_creation_tokens: 0,
            }));
        model
            .steps
            .lock()
            .unwrap()
            .push_back(ModelStep::Answer(text_response("done")));
        let task_loop = build_loop(
            exec,
            policy,
            hooks,
            model,
            approvals,
            plans,
            compactor,
            8,
            fx.interrupt.clone(),
        );
        // Fork-scoped surface: this run may only read files.
        task_loop.run_allowlist().restrict(
            &fx.ctx.run_id,
            ["read_file".to_string()].into_iter().collect(),
        );
        let conv = &mut Conversation::new();
        let events = fx.events.clone();
        assert!(task_loop.executor.lock_executed().is_empty());
        let outcome = task_loop
            .run_turn(&fx.ctx, conv, TurnInput::text("hi"), "sys", &|e| {
                events.lock().unwrap_or_else(|e| e.into_inner()).push(e);
            })
            .await;
        assert_eq!(outcome, StopReason::Completed);
        // Denied, not executed: the executor never saw the call.
        assert!(task_loop.executor.lock_executed().is_empty());
        let history = conv
            .snapshot()
            .iter()
            .map(|e| e.text())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(history.contains("not in this run's allowed tools"));
    }

    #[tokio::test]
    async fn duplicate_call_ids_collapse_without_panicking() {
        let fx = Fixture::new();
        let (exec, policy, hooks, model, approvals, plans, compactor) = default_parts();
        model
            .steps
            .lock()
            .unwrap()
            .push_back(ModelStep::Answer(SampleResponse {
                blocks: vec![
                    SampleBlock::ToolUse {
                        call_id: "c1".to_string(),
                        name: "shell".to_string(),
                        input: serde_json::Value::Null,
                    },
                    SampleBlock::ToolUse {
                        call_id: "c1".to_string(),
                        name: "shell".to_string(),
                        input: serde_json::Value::Null,
                    },
                ],
                input_tokens: Some(10),
                output_tokens: Some(1),
                truncated: false,
                cache_read_tokens: 0,
                cache_creation_tokens: 0,
            }));
        model
            .steps
            .lock()
            .unwrap()
            .push_back(ModelStep::Answer(text_response("done")));
        let conv = &mut Conversation::new();
        let events = fx.events.clone();
        let outcome = build_loop(
            exec,
            policy,
            hooks,
            model,
            approvals,
            plans,
            compactor,
            8,
            fx.interrupt.clone(),
        )
        .run_turn(&fx.ctx, conv, TurnInput::text("hi"), "sys", &|e| {
            events.lock().unwrap_or_else(|e| e.into_inner()).push(e);
        })
        .await;
        // Previously this panicked pairing the second slot by id.
        assert_eq!(outcome, StopReason::Completed);
        let kinds = fx.event_kinds();
        assert_eq!(kinds.iter().filter(|k| *k == "tool_call_begin").count(), 2);
        assert_eq!(kinds.iter().filter(|k| *k == "tool_call_end").count(), 2);
        let history = conv
            .snapshot()
            .iter()
            .map(|e| e.text())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(history.contains("duplicate tool call id"));
    }

    #[tokio::test]
    async fn zero_round_ceiling_melts_without_token_count() {
        let fx = Fixture::new();
        let (exec, policy, hooks, model, approvals, plans, compactor) = default_parts();
        model
            .steps
            .lock()
            .unwrap()
            .push_back(ModelStep::Answer(text_response("hi")));
        let conv = &mut Conversation::new();
        let events = fx.events.clone();
        let outcome = build_loop(
            exec,
            policy,
            hooks,
            model,
            approvals,
            plans,
            compactor,
            0,
            fx.interrupt.clone(),
        )
        .run_turn(&fx.ctx, conv, TurnInput::text("hi"), "sys", &|e| {
            events.lock().unwrap_or_else(|e| e.into_inner()).push(e);
        })
        .await;
        // The ceiling reports the declared stop reason, not a plain
        // Completed (callers may distinguish it; the turn still settles
        // with TurnCompleted { interrupted: false }).
        assert_eq!(outcome, StopReason::MaxToolRounds);
        let kinds = fx.event_kinds();
        assert!(kinds.iter().any(|k| k == "warning"));
        // No completed sample means settle is a no-op: no token event.
        assert!(!kinds.contains(&"token_count".to_string()));
    }

    #[tokio::test]
    async fn continuations_cap_at_two() {
        let fx = Fixture::new();
        let (exec, policy, hooks, model, approvals, plans, compactor) = default_parts();
        for _ in 0..5 {
            model
                .steps
                .lock()
                .unwrap()
                .push_back(ModelStep::Answer(SampleResponse {
                    blocks: vec![SampleBlock::Text("more".to_string())],
                    input_tokens: Some(10),
                    output_tokens: Some(1),
                    truncated: true,
                    cache_read_tokens: 0,
                    cache_creation_tokens: 0,
                }));
        }
        let conv = &mut Conversation::new();
        let events = fx.events.clone();
        let outcome = build_loop(
            exec,
            policy,
            hooks,
            model,
            approvals,
            plans,
            compactor,
            8,
            fx.interrupt.clone(),
        )
        .run_turn(&fx.ctx, conv, TurnInput::text("hi"), "sys", &|e| {
            events.lock().unwrap_or_else(|e| e.into_inner()).push(e);
        })
        .await;
        assert_eq!(outcome, StopReason::Completed);
        let reminders = conv
            .snapshot()
            .iter()
            .filter(|e| e.text() == CONTINUATION_PROMPT)
            .count();
        assert_eq!(reminders, 2);
    }

    #[tokio::test]
    async fn continuation_ceiling_comes_from_config() {
        async fn reminders_with(max_continuations: u8) -> usize {
            let fx = Fixture::new();
            let (exec, policy, hooks, model, approvals, plans, compactor) = default_parts();
            for _ in 0..5 {
                model
                    .steps
                    .lock()
                    .unwrap()
                    .push_back(ModelStep::Answer(SampleResponse {
                        blocks: vec![SampleBlock::Text("more".to_string())],
                        input_tokens: Some(10),
                        output_tokens: Some(1),
                        truncated: true,
                        cache_read_tokens: 0,
                        cache_creation_tokens: 0,
                    }));
            }
            let conv = &mut Conversation::new();
            let events = fx.events.clone();
            let outcome = RunLoop::new(
                exec,
                policy,
                hooks,
                model,
                approvals,
                plans,
                compactor,
                RunConfig {
                    model_name: "test".to_string(),
                    context_window: 200_000,
                    max_output_tokens: 100,
                    max_tool_rounds: 8,
                    max_continuations,
                    max_plan_nudges: MAX_PLAN_NUDGES,
                    max_stop_blocks: MAX_STOP_BLOCKS,
                    max_reactive_compacts: MAX_REACTIVE_COMPACTS,
                },
                fx.interrupt.clone(),
            )
            .run_turn(&fx.ctx, conv, TurnInput::text("hi"), "sys", &|e| {
                events.lock().unwrap_or_else(|e| e.into_inner()).push(e);
            })
            .await;
            assert_eq!(outcome, StopReason::Completed);
            conv.snapshot()
                .iter()
                .filter(|e| e.text() == CONTINUATION_PROMPT)
                .count()
        }
        // Zero disables continuations; one allows exactly one.
        assert_eq!(reminders_with(0).await, 0);
        assert_eq!(reminders_with(1).await, 1);
    }

    #[tokio::test]
    async fn plan_nudges_cap_at_three() {
        let fx = Fixture::new();
        let (exec, policy, hooks, model, approvals, _, compactor) = default_parts();
        for _ in 0..6 {
            model
                .steps
                .lock()
                .unwrap()
                .push_back(ModelStep::Answer(text_response("working")));
        }
        let plans = FakePlans { unfinished: 2 };
        let conv = &mut Conversation::new();
        let events = fx.events.clone();
        let outcome = build_loop(
            exec,
            policy,
            hooks,
            model,
            approvals,
            plans,
            compactor,
            8,
            fx.interrupt.clone(),
        )
        .run_turn(&fx.ctx, conv, TurnInput::text("hi"), "sys", &|e| {
            events.lock().unwrap_or_else(|e| e.into_inner()).push(e);
        })
        .await;
        assert_eq!(outcome, StopReason::Completed);
        let reminders = conv
            .snapshot()
            .iter()
            .filter(|e| e.text() == "PLAN-REMINDER")
            .count();
        assert_eq!(reminders, 3);
    }

    #[tokio::test]
    async fn stop_hook_blocks_cap_at_three() {
        let fx = Fixture::new();
        let (exec, policy, _, model, approvals, plans, compactor) = default_parts();
        for _ in 0..6 {
            model
                .steps
                .lock()
                .unwrap()
                .push_back(ModelStep::Answer(text_response("done")));
        }
        let hooks = FakeHooks {
            submit_block: false,
            stop_blocks_left: Mutex::new(u8::MAX),
        };
        let conv = &mut Conversation::new();
        let events = fx.events.clone();
        let outcome = build_loop(
            exec,
            policy,
            hooks,
            model,
            approvals,
            plans,
            compactor,
            8,
            fx.interrupt.clone(),
        )
        .run_turn(&fx.ctx, conv, TurnInput::text("hi"), "sys", &|e| {
            events.lock().unwrap_or_else(|e| e.into_inner()).push(e);
        })
        .await;
        assert_eq!(outcome, StopReason::Completed);
        let blocks = conv
            .snapshot()
            .iter()
            .filter(|e| e.text().contains("Stop hook blocked turn completion"))
            .count();
        assert_eq!(blocks, 3);
    }

    #[tokio::test]
    async fn reactive_compact_retries_then_fails() {
        let fx = Fixture::new();
        let (exec, policy, hooks, model, approvals, plans, compactor) = default_parts();
        let compactions = compactor.calls.clone();
        for _ in 0..4 {
            model
                .steps
                .lock()
                .unwrap()
                .push_back(ModelStep::Fail(SampleError::PromptTooLong));
        }
        let conv = &mut Conversation::new();
        let events = fx.events.clone();
        let outcome = build_loop(
            exec,
            policy,
            hooks,
            model,
            approvals,
            plans,
            compactor,
            8,
            fx.interrupt.clone(),
        )
        .run_turn(&fx.ctx, conv, TurnInput::text("hi"), "sys", &|e| {
            events.lock().unwrap_or_else(|e| e.into_inner()).push(e);
        })
        .await;
        assert!(matches!(outcome, StopReason::Error(_)));
        let kinds = fx.event_kinds();
        assert!(kinds.contains(&"error".to_string()));
        assert_eq!(kinds.last().unwrap(), "turn_completed");
        // Two successful compactions, then the third prompt-too-long melts.
        assert_eq!(compactions.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn reactive_compact_recovery_continues() {
        let fx = Fixture::new();
        let (exec, policy, hooks, model, approvals, plans, compactor) = default_parts();
        model
            .steps
            .lock()
            .unwrap()
            .push_back(ModelStep::Fail(SampleError::PromptTooLong));
        model
            .steps
            .lock()
            .unwrap()
            .push_back(ModelStep::Answer(text_response("recovered")));
        let conv = &mut Conversation::new();
        let events = fx.events.clone();
        let outcome = build_loop(
            exec,
            policy,
            hooks,
            model,
            approvals,
            plans,
            compactor,
            8,
            fx.interrupt.clone(),
        )
        .run_turn(&fx.ctx, conv, TurnInput::text("hi"), "sys", &|e| {
            events.lock().unwrap_or_else(|e| e.into_inner()).push(e);
        })
        .await;
        assert_eq!(outcome, StopReason::Completed);
        let kinds = fx.event_kinds();
        assert!(kinds.contains(&"compact_started".to_string()));
        assert!(kinds.contains(&"compact_completed".to_string()));
    }

    #[tokio::test]
    async fn budget_warning_fires_once() {
        let fx = Fixture::new();
        let (exec, policy, hooks, model, approvals, plans, compactor) = default_parts();
        // Tiny window: every sample stays near the limit, but the warning
        // must fire exactly once per turn.
        for _ in 0..4 {
            model
                .steps
                .lock()
                .unwrap()
                .push_back(ModelStep::Answer(text_response("x")));
        }
        let conv = &mut Conversation::new();
        // Seed the carry so the first budget check lands in the Warn band
        // (remaining 19_100 of 25_100: warn, but above auto-compact).
        conv.settle(Usage {
            input_tokens: 6000,
            output_tokens: 0,
            ..Usage::default()
        });
        let events = fx.events.clone();
        let outcome = RunLoop::new(
            exec,
            policy,
            hooks,
            model,
            approvals,
            plans,
            compactor,
            RunConfig {
                model_name: "test".to_string(),
                context_window: 25_100,
                max_output_tokens: 100,
                max_tool_rounds: 8,
                max_continuations: MAX_CONTINUATIONS,
                max_plan_nudges: MAX_PLAN_NUDGES,
                max_stop_blocks: MAX_STOP_BLOCKS,
                max_reactive_compacts: MAX_REACTIVE_COMPACTS,
            },
            fx.interrupt.clone(),
        )
        .run_turn(&fx.ctx, conv, TurnInput::text("hi"), "sys", &|e| {
            events.lock().unwrap_or_else(|e| e.into_inner()).push(e);
        })
        .await;
        assert_eq!(outcome, StopReason::Completed);
        let warnings = fx
            .lock_events()
            .iter()
            .filter(|e| {
                matches!(&e.msg, EventMsg::Warning { message } if message.contains("near limit"))
            })
            .count();
        assert_eq!(warnings, 1);
    }

    /// Request-recording scripted model for inbox tests.
    type SeenLog = std::sync::Arc<Mutex<Vec<Vec<HistoryEntry>>>>;
    type ReleaseGate = std::sync::Arc<tokio::sync::Notify>;

    struct CaptureModel {
        seen: SeenLog,
        release: ReleaseGate,
        wait_first: bool,
    }

    fn capture_model(wait_first: bool) -> (CaptureModel, SeenLog, ReleaseGate) {
        let seen = std::sync::Arc::new(Mutex::new(Vec::new()));
        let release = std::sync::Arc::new(tokio::sync::Notify::new());
        (
            CaptureModel {
                seen: seen.clone(),
                release: release.clone(),
                wait_first,
            },
            seen,
            release,
        )
    }

    #[async_trait::async_trait]
    impl ModelGateway for CaptureModel {
        async fn sample(&self, request: SampleRequest) -> Result<SampleResponse, SampleError> {
            let count = {
                let mut seen = self.seen.lock().unwrap_or_else(|e| e.into_inner());
                seen.push(request.messages.clone());
                seen.len()
            };
            if self.wait_first && count == 1 {
                self.release.notified().await;
            }
            Ok(text_response("done"))
        }
    }

    fn seen_texts(seen: &SeenLog, index: usize) -> Vec<String> {
        seen.lock().unwrap_or_else(|e| e.into_inner())[index]
            .iter()
            .map(|entry| entry.text())
            .collect()
    }

    #[tokio::test]
    async fn inbox_targets_land_in_order_before_first_sample() {
        let fx = Fixture::new();
        let (exec, policy, hooks, _, approvals, plans, compactor) = default_parts();
        let (model, seen, _) = capture_model(false);
        let cycle = build_loop(
            exec,
            policy,
            hooks,
            model,
            approvals,
            plans,
            compactor,
            4,
            fx.interrupt.clone(),
        );
        cycle.steer("TURN".to_string(), SteerTarget::NextTurn);
        cycle.steer("STEP".to_string(), SteerTarget::NextStep);
        cycle.inject("INJECT".to_string());
        // Empty texts never queue (providers reject empty user messages).
        cycle.steer(String::new(), SteerTarget::NextStep);
        cycle.inject(String::new());
        let conv = &mut Conversation::new();
        let events = fx.events.clone();
        let outcome = cycle
            .run_turn(&fx.ctx, conv, TurnInput::text("hi"), "sys", &|e| {
                events.lock().unwrap_or_else(|e| e.into_inner()).push(e);
            })
            .await;
        assert_eq!(outcome, StopReason::Completed);
        // Loop-head (NextTurn) drains before pre-sample (NextStep, inject).
        assert_eq!(seen_texts(&seen, 0), vec!["hi", "TURN", "STEP", "INJECT"]);
        assert!(fx.event_kinds().contains(&"warning".to_string()));
    }

    #[tokio::test]
    async fn cancel_inbox_keep_flag_selects_survivors() {
        // keep=false drops everything pending.
        let fx = Fixture::new();
        let (exec, policy, hooks, _, approvals, plans, compactor) = default_parts();
        let (model, seen, _) = capture_model(false);
        let cycle = build_loop(
            exec,
            policy,
            hooks,
            model,
            approvals,
            plans,
            compactor,
            4,
            fx.interrupt.clone(),
        );
        cycle.steer("TURN".to_string(), SteerTarget::NextTurn);
        cycle.steer("STEP".to_string(), SteerTarget::NextStep);
        assert_eq!(cycle.cancel_inbox(false), 2);
        let conv = &mut Conversation::new();
        let events = fx.events.clone();
        cycle
            .run_turn(&fx.ctx, conv, TurnInput::text("hi"), "sys", &|e| {
                events.lock().unwrap_or_else(|e| e.into_inner()).push(e);
            })
            .await;
        assert_eq!(seen_texts(&seen, 0), vec!["hi"]);

        // keep=true preserves next-turn steering, drops current-turn items.
        let fx = Fixture::new();
        let (exec, policy, hooks, _, approvals, plans, compactor) = default_parts();
        let (model, seen, _) = capture_model(false);
        let cycle = build_loop(
            exec,
            policy,
            hooks,
            model,
            approvals,
            plans,
            compactor,
            4,
            fx.interrupt.clone(),
        );
        cycle.steer("TURN".to_string(), SteerTarget::NextTurn);
        cycle.steer("STEP".to_string(), SteerTarget::NextStep);
        cycle.inject("INJECT".to_string());
        assert_eq!(cycle.cancel_inbox(true), 2);
        let conv = &mut Conversation::new();
        let events = fx.events.clone();
        cycle
            .run_turn(&fx.ctx, conv, TurnInput::text("hi"), "sys", &|e| {
                events.lock().unwrap_or_else(|e| e.into_inner()).push(e);
            })
            .await;
        assert_eq!(seen_texts(&seen, 0), vec!["hi", "TURN"]);
    }

    #[tokio::test]
    async fn mid_turn_next_step_steer_reaches_second_sample() {
        let fx = Fixture::new();
        let (exec, policy, hooks, _, approvals, _, compactor) = default_parts();
        // One unfinished plan item forces reminder-driven re-samples, so a
        // mid-turn steer has a second sample to land in.
        let plans = FakePlans { unfinished: 1 };
        let (model, seen, release) = capture_model(true);
        let cycle = build_loop(
            exec,
            policy,
            hooks,
            model,
            approvals,
            plans,
            compactor,
            8,
            fx.interrupt.clone(),
        );
        let handle = cycle.inbox_handle();
        let seen_wait = seen.clone();
        let release_wait = release.clone();
        let steerer = tokio::spawn(async move {
            loop {
                let sampled = seen_wait.lock().unwrap_or_else(|e| e.into_inner()).len();
                if sampled >= 1 {
                    break;
                }
                tokio::task::yield_now().await;
            }
            handle.steer("FOLLOW-UP".to_string(), SteerTarget::NextStep);
            release_wait.notify_one();
        });
        let conv = &mut Conversation::new();
        let events = fx.events.clone();
        let outcome = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            cycle.run_turn(&fx.ctx, conv, TurnInput::text("hi"), "sys", &|e| {
                events.lock().unwrap_or_else(|e| e.into_inner()).push(e);
            }),
        )
        .await
        .expect("turn must not hang on inbox steering");
        steerer.await.unwrap();
        assert_eq!(outcome, StopReason::Completed);
        // The steer arrived after the first sample was taken ...
        assert_eq!(seen_texts(&seen, 0), vec!["hi"]);
        // ... and steered the immediately following sample.
        assert!(seen_texts(&seen, 1).contains(&"FOLLOW-UP".to_string()));
    }

    #[tokio::test]
    async fn questions_answer_without_executing() {
        let fx = Fixture::new();
        let (exec, _, hooks, model, _, plans, compactor) = default_parts();
        let mut verdicts = HashMap::new();
        verdicts.insert(
            "ask_user".to_string(),
            PolicyVerdict::Question {
                question: "proceed with the risky path?".to_string(),
                options: vec!["yes".to_string(), "no".to_string()],
            },
        );
        let policy = FakePolicy { verdicts };
        let mut answers = HashMap::new();
        answers.insert(
            "c1".to_string(),
            QuestionResolution::Answered("yes".to_string()),
        );
        let approvals = FakeApprovals {
            resolutions: HashMap::new(),
            answers,
        };
        model
            .steps
            .lock()
            .unwrap()
            .push_back(ModelStep::Answer(SampleResponse {
                blocks: vec![SampleBlock::ToolUse {
                    call_id: "c1".to_string(),
                    name: "ask_user".to_string(),
                    input: serde_json::Value::Null,
                }],
                input_tokens: Some(10),
                output_tokens: Some(1),
                truncated: false,
                cache_read_tokens: 0,
                cache_creation_tokens: 0,
            }));
        model
            .steps
            .lock()
            .unwrap()
            .push_back(ModelStep::Answer(text_response("done")));
        let conv = &mut Conversation::new();
        let events = fx.events.clone();
        let run = build_loop(
            exec,
            policy,
            hooks,
            model,
            approvals,
            plans,
            compactor,
            8,
            fx.interrupt.clone(),
        );
        let outcome = run
            .run_turn(&fx.ctx, conv, TurnInput::text("hi"), "sys", &|e| {
                events.lock().unwrap_or_else(|e| e.into_inner()).push(e);
            })
            .await;
        assert_eq!(outcome, StopReason::Completed);
        let kinds = fx.event_kinds();
        assert!(kinds.contains(&"question_requested".to_string()));
        // The answer is the result: the tool body never executes.
        assert!(
            !run.executor
                .lock_executed()
                .contains(&"ask_user".to_string())
        );
        let history = conv
            .snapshot()
            .iter()
            .map(|e| e.text())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(history.contains("yes"), "{history}");
    }

    #[tokio::test]
    async fn unanswered_questions_fail_openly() {
        let fx = Fixture::new();
        let (exec, _, hooks, model, _, plans, compactor) = default_parts();
        let mut verdicts = HashMap::new();
        verdicts.insert(
            "ask_user".to_string(),
            PolicyVerdict::Question {
                question: "proceed?".to_string(),
                options: Vec::new(),
            },
        );
        let policy = FakePolicy { verdicts };
        // No scripted answer: the default resolves Unavailable.
        let approvals = FakeApprovals {
            resolutions: HashMap::new(),
            answers: HashMap::new(),
        };
        model
            .steps
            .lock()
            .unwrap()
            .push_back(ModelStep::Answer(SampleResponse {
                blocks: vec![SampleBlock::ToolUse {
                    call_id: "c1".to_string(),
                    name: "ask_user".to_string(),
                    input: serde_json::Value::Null,
                }],
                input_tokens: Some(10),
                output_tokens: Some(1),
                truncated: false,
                cache_read_tokens: 0,
                cache_creation_tokens: 0,
            }));
        model
            .steps
            .lock()
            .unwrap()
            .push_back(ModelStep::Answer(text_response("done")));
        let conv = &mut Conversation::new();
        let run = build_loop(
            exec,
            policy,
            hooks,
            model,
            approvals,
            plans,
            compactor,
            8,
            fx.interrupt.clone(),
        );
        let outcome = run
            .run_turn(&fx.ctx, conv, TurnInput::text("hi"), "sys", &|_| {})
            .await;
        assert_eq!(outcome, StopReason::Completed);
        let history = conv
            .snapshot()
            .iter()
            .map(|e| e.text())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(history.contains("nobody answered"), "{history}");
        assert!(
            !run.executor
                .lock_executed()
                .contains(&"ask_user".to_string())
        );
    }

    /// Executor that parks inside one tool call until `release` fires, so
    /// a test can trigger a run-scoped interrupt mid-turn deterministically.
    struct ParkExecutor {
        release: InterruptHandle,
    }

    #[async_trait::async_trait]
    impl ToolExecutor for ParkExecutor {
        async fn execute(&self, call: ToolCall) -> ToolResult {
            for _ in 0..2000 {
                if self.release.is_triggered() {
                    return ToolResult {
                        call_id: call.call_id,
                        content: "parked work finished".to_string(),
                        is_error: false,
                    };
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
            ToolResult {
                call_id: call.call_id,
                content: "park executor timed out".to_string(),
                is_error: true,
            }
        }

        fn is_read_only(&self, _tool: &str) -> bool {
            true
        }
    }

    #[tokio::test]
    async fn run_scoped_stop_interrupts_the_child_turn_without_touching_the_session() {
        let fx = Fixture::new();
        let run_handle = InterruptHandle::new();
        let (_, policy, hooks, model, approvals, plans, compactor) = default_parts();
        model
            .steps
            .lock()
            .unwrap()
            .push_back(ModelStep::Answer(SampleResponse {
                blocks: vec![SampleBlock::ToolUse {
                    call_id: "c1".to_string(),
                    name: "read_file".to_string(),
                    input: serde_json::Value::Null,
                }],
                input_tokens: Some(10),
                output_tokens: Some(1),
                truncated: false,
                cache_read_tokens: 0,
                cache_creation_tokens: 0,
            }));
        let conv = &mut Conversation::new();
        let run = RunLoop::new(
            ParkExecutor {
                release: run_handle.clone(),
            },
            policy,
            hooks,
            model,
            approvals,
            plans,
            compactor,
            RunConfig {
                model_name: "test".to_string(),
                context_window: 200_000,
                max_output_tokens: 100,
                max_tool_rounds: 8,
                max_continuations: MAX_CONTINUATIONS,
                max_plan_nudges: MAX_PLAN_NUDGES,
                max_stop_blocks: MAX_STOP_BLOCKS,
                max_reactive_compacts: MAX_REACTIVE_COMPACTS,
            },
            fx.interrupt.clone(),
        );
        // Register on the loop's own registry — the same seam the child
        // service uses before driving a child turn.
        run.run_interrupts()
            .register(&fx.ctx.run_id, run_handle.clone());
        // The scoped stop fires while the child parks inside the tool call;
        // the post-execute checkpoint must end the turn as Interrupted.
        let trigger = run_handle.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            trigger.trigger();
        });
        let outcome = run
            .run_turn(&fx.ctx, conv, TurnInput::text("hi"), "sys", &|_| {})
            .await;
        assert_eq!(outcome, StopReason::Interrupted);
        // The whole point of the run-scoped handle: a child stop must not
        // look like (or become) a user interrupt of the session.
        assert!(
            !fx.interrupt.is_triggered(),
            "a scoped child stop must not trip the session-wide flag"
        );
    }

    #[tokio::test]
    async fn child_turns_observe_the_session_interrupt_without_resetting_it() {
        let fx = Fixture::new();
        let (exec, policy, hooks, model, approvals, plans, compactor) = default_parts();
        // No model steps: if the loop sampled past checkpoint 1 the empty
        // default response would complete the turn, so Interrupted proves
        // the child turn observed the session-wide flag.
        fx.interrupt.trigger();
        let conv = &mut Conversation::new();
        let run = build_loop(
            exec,
            policy,
            hooks,
            model,
            approvals,
            plans,
            compactor,
            8,
            fx.interrupt.clone(),
        );
        run.run_interrupts()
            .register(&fx.ctx.run_id, InterruptHandle::new());
        let outcome = run
            .run_turn(&fx.ctx, conv, TurnInput::text("hi"), "sys", &|_| {})
            .await;
        assert_eq!(outcome, StopReason::Interrupted);
        // The child turn's fresh handle starts untriggered and is never
        // reset at entry: the user's session-wide interrupt survives the
        // child turn start.
        assert!(
            fx.interrupt.is_triggered(),
            "a child turn start must not swallow a user interrupt"
        );
    }

    #[tokio::test]
    async fn round_ceiling_reports_max_tool_rounds() {
        let fx = Fixture::new();
        let (exec, policy, hooks, model, approvals, plans, compactor) = default_parts();
        let conv = &mut Conversation::new();
        // A zero ceiling melts the very first iteration: the loop
        // reports MaxToolRounds, not Completed.
        let outcome = build_loop(
            exec,
            policy,
            hooks,
            model,
            approvals,
            plans,
            compactor,
            0,
            fx.interrupt.clone(),
        )
        .run_turn(&fx.ctx, conv, TurnInput::text("hi"), "sys", &|_| {})
        .await;
        assert_eq!(outcome, StopReason::MaxToolRounds);
    }

    #[tokio::test]
    async fn interrupted_approval_resolution_fills_the_slot_without_executing() {
        let fx = Fixture::new();
        let (exec, _, hooks, model, _, plans, compactor) = default_parts();
        let mut verdicts = HashMap::new();
        verdicts.insert(
            "shell".to_string(),
            PolicyVerdict::Ask {
                kind: AskKind::Exec,
                detail: "run ls".to_string(),
            },
        );
        let mut resolutions = HashMap::new();
        // The gate reports the session interrupt firing mid-wait.
        resolutions.insert("c1".to_string(), ApprovalResolution::Interrupted);
        let approvals = FakeApprovals {
            resolutions,
            answers: HashMap::new(),
        };
        let policy = FakePolicy { verdicts };
        model
            .steps
            .lock()
            .unwrap()
            .push_back(ModelStep::Answer(SampleResponse {
                blocks: vec![SampleBlock::ToolUse {
                    call_id: "c1".to_string(),
                    name: "shell".to_string(),
                    input: serde_json::Value::Null,
                }],
                input_tokens: Some(10),
                output_tokens: Some(1),
                truncated: false,
                cache_read_tokens: 0,
                cache_creation_tokens: 0,
            }));
        let conv = &mut Conversation::new();
        let loop_ = build_loop(
            exec.clone(),
            policy,
            hooks,
            model,
            approvals,
            plans,
            compactor,
            8,
            fx.interrupt.clone(),
        );
        let outcome = loop_
            .run_turn(&fx.ctx, conv, TurnInput::text("hi"), "sys", &|_| {})
            .await;
        // The turn continues with the interrupted slot filled: pairing
        // holds, and the tool never ran.
        assert_eq!(outcome, StopReason::Completed);
        assert!(!exec.lock_executed().contains(&"shell".to_string()));
        let history = conv
            .snapshot()
            .iter()
            .map(|e| e.text())
            .collect::<Vec<_>>()
            .join(
                "
",
            );
        assert!(history.contains("interrupted by user"), "{history}");
    }

    #[tokio::test]
    async fn interrupted_question_resolution_fills_the_slot_without_executing() {
        let fx = Fixture::new();
        let (exec, _, hooks, model, _, plans, compactor) = default_parts();
        let mut verdicts = HashMap::new();
        verdicts.insert(
            "ask_user".to_string(),
            PolicyVerdict::Question {
                question: "pick one".to_string(),
                options: vec!["a".to_string()],
            },
        );
        let mut answers = HashMap::new();
        answers.insert("c1".to_string(), QuestionResolution::Interrupted);
        let approvals = FakeApprovals {
            resolutions: HashMap::new(),
            answers,
        };
        let policy = FakePolicy { verdicts };
        model
            .steps
            .lock()
            .unwrap()
            .push_back(ModelStep::Answer(SampleResponse {
                blocks: vec![SampleBlock::ToolUse {
                    call_id: "c1".to_string(),
                    name: "ask_user".to_string(),
                    input: serde_json::Value::Null,
                }],
                input_tokens: Some(10),
                output_tokens: Some(1),
                truncated: false,
                cache_read_tokens: 0,
                cache_creation_tokens: 0,
            }));
        let loop_ = build_loop(
            exec.clone(),
            policy,
            hooks,
            model,
            approvals,
            plans,
            compactor,
            8,
            fx.interrupt.clone(),
        );
        let conv = &mut Conversation::new();
        let outcome = loop_
            .run_turn(&fx.ctx, conv, TurnInput::text("hi"), "sys", &|_| {})
            .await;
        assert_eq!(outcome, StopReason::Completed);
        // The answer IS the tool result: an interrupted question never
        // reaches the executor either.
        assert!(!exec.lock_executed().contains(&"ask_user".to_string()));
        let history = conv
            .snapshot()
            .iter()
            .map(|e| e.text())
            .collect::<Vec<_>>()
            .join(
                "
",
            );
        assert!(history.contains("interrupted by user"), "{history}");
    }

    #[tokio::test]
    async fn round_ceiling_after_a_sample_settles_usage() {
        let fx = Fixture::new();
        let (exec, policy, hooks, model, approvals, plans, compactor) = default_parts();
        // One completed sample with a tool call: after the dispatch round
        // the ceiling (1) melts the loop, and settle must emit TokenCount
        // because a sample completed.
        model
            .steps
            .lock()
            .unwrap()
            .push_back(ModelStep::Answer(SampleResponse {
                blocks: vec![SampleBlock::ToolUse {
                    call_id: "c1".to_string(),
                    name: "read_file".to_string(),
                    input: serde_json::Value::Null,
                }],
                input_tokens: Some(10),
                output_tokens: Some(1),
                truncated: false,
                cache_read_tokens: 0,
                cache_creation_tokens: 0,
            }));
        let conv = &mut Conversation::new();
        let events = fx.events.clone();
        let outcome = build_loop(
            exec,
            policy,
            hooks,
            model,
            approvals,
            plans,
            compactor,
            1,
            fx.interrupt.clone(),
        )
        .run_turn(&fx.ctx, conv, TurnInput::text("hi"), "sys", &|e| {
            events.lock().unwrap_or_else(|e| e.into_inner()).push(e);
        })
        .await;
        assert_eq!(outcome, StopReason::MaxToolRounds);
        let kinds = fx.event_kinds();
        assert!(kinds.contains(&"token_count".to_string()));
        assert_eq!(kinds.last().unwrap(), "turn_completed");
    }

    /// Executor counting peak in-flight concurrency, for the capped
    /// read-only batch. Counters are shared so the test can read them
    /// after the loop consumes the executor.
    type SharedCounter = std::sync::Arc<std::sync::atomic::AtomicUsize>;

    struct CountingExecutor {
        in_flight: SharedCounter,
        max_in_flight: SharedCounter,
        executed: SharedCounter,
    }

    #[async_trait::async_trait]
    impl ToolExecutor for CountingExecutor {
        async fn execute(&self, call: ToolCall) -> ToolResult {
            let now = self
                .in_flight
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
                + 1;
            self.max_in_flight
                .fetch_max(now, std::sync::atomic::Ordering::SeqCst);
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            self.in_flight
                .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
            self.executed
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            ToolResult {
                call_id: call.call_id,
                content: "ok".to_string(),
                is_error: false,
            }
        }

        fn is_read_only(&self, _tool: &str) -> bool {
            true
        }
    }

    fn counter() -> SharedCounter {
        std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0))
    }

    #[tokio::test]
    async fn read_only_batches_run_capped_and_fully_execute() {
        let fx = Fixture::new();
        let (_, policy, hooks, model, approvals, plans, compactor) = default_parts();
        let (in_flight, max_in_flight, executed) = (counter(), counter(), counter());
        let executor = CountingExecutor {
            in_flight: in_flight.clone(),
            max_in_flight: max_in_flight.clone(),
            executed: executed.clone(),
        };
        let calls: Vec<SampleBlock> = (0..24)
            .map(|i| SampleBlock::ToolUse {
                call_id: format!("c{i}"),
                name: "read_file".to_string(),
                input: serde_json::Value::Null,
            })
            .collect();
        model
            .steps
            .lock()
            .unwrap()
            .push_back(ModelStep::Answer(SampleResponse {
                blocks: calls,
                input_tokens: Some(10),
                output_tokens: Some(1),
                truncated: false,
                cache_read_tokens: 0,
                cache_creation_tokens: 0,
            }));
        let conv = &mut Conversation::new();
        let outcome = RunLoop::new(
            executor,
            policy,
            hooks,
            model,
            approvals,
            plans,
            compactor,
            RunConfig {
                model_name: "test".to_string(),
                context_window: 200_000,
                max_output_tokens: 100,
                max_tool_rounds: 8,
                max_continuations: MAX_CONTINUATIONS,
                max_plan_nudges: MAX_PLAN_NUDGES,
                max_stop_blocks: MAX_STOP_BLOCKS,
                max_reactive_compacts: MAX_REACTIVE_COMPACTS,
            },
            fx.interrupt.clone(),
        )
        .run_turn(&fx.ctx, conv, TurnInput::text("hi"), "sys", &|_| {})
        .await;
        assert_eq!(outcome, StopReason::Completed);
        // All 24 executed despite the cap, and the in-flight peak stayed
        // within the batch limit.
        assert_eq!(executed.load(std::sync::atomic::Ordering::SeqCst), 24);
        let peak = max_in_flight.load(std::sync::atomic::Ordering::SeqCst);
        assert!(peak <= 8, "peak in-flight: {peak}");
    }

    /// Approval source recording `clear_stale` calls, to pin the gating:
    /// session-owned turns clear stale gate state, child turns must not.
    struct RecordingApprovals {
        clears: std::sync::atomic::AtomicUsize,
    }

    #[async_trait::async_trait]
    impl ApprovalSource for RecordingApprovals {
        async fn decide(
            &self,
            _call_id: &str,
            _kind: AskKind,
            _detail: &str,
        ) -> ApprovalResolution {
            ApprovalResolution::Deny {
                reason: "test deny".to_string(),
            }
        }

        async fn ask(
            &self,
            _call_id: &str,
            _question: &str,
            _options: &[String],
        ) -> QuestionResolution {
            QuestionResolution::Unavailable {
                reason: "test: nobody answered".to_string(),
            }
        }

        fn clear_stale(&self) {
            self.clears
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
    }

    #[tokio::test]
    async fn only_session_owned_turns_clear_stale_gate_state() {
        let fx = Fixture::new();
        let (exec, policy, hooks, model, _fake_approvals, plans, compactor) = default_parts();
        let approvals = RecordingApprovals {
            clears: std::sync::atomic::AtomicUsize::new(0),
        };
        let run = RunLoop::new(
            exec,
            policy,
            hooks,
            model,
            approvals,
            plans,
            compactor,
            RunConfig {
                model_name: "test".to_string(),
                context_window: 200_000,
                max_output_tokens: 100,
                max_tool_rounds: 8,
                max_continuations: MAX_CONTINUATIONS,
                max_plan_nudges: MAX_PLAN_NUDGES,
                max_stop_blocks: MAX_STOP_BLOCKS,
                max_reactive_compacts: MAX_REACTIVE_COMPACTS,
            },
            fx.interrupt.clone(),
        );
        // Session-owned turn: clears stale gate state like before.
        let conv = &mut Conversation::new();
        let outcome = run
            .run_turn(&fx.ctx, conv, TurnInput::text("hi"), "sys", &|_| {})
            .await;
        assert_eq!(outcome, StopReason::Completed);
        assert_eq!(
            run.approvals
                .clears
                .load(std::sync::atomic::Ordering::SeqCst),
            1
        );
        // Child turn (registered run handle): must not wipe a parent
        // approval parked mid-wait on the shared gates.
        run.run_interrupts()
            .register(&fx.ctx.run_id, InterruptHandle::new());
        let outcome = run
            .run_turn(&fx.ctx, conv, TurnInput::text("hi again"), "sys", &|_| {})
            .await;
        assert_eq!(outcome, StopReason::Completed);
        assert_eq!(
            run.approvals
                .clears
                .load(std::sync::atomic::Ordering::SeqCst),
            1,
            "a child turn start must not clear the shared gates"
        );
    }
}
