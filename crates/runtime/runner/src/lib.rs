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

use serde_json::Value;

use infrastructure_base::InterruptHandle;
use operations_wire::{Event, EventMsg};
use state_store::{
    BudgetLevel, CONTEXT_OVERHEAD_TOKENS, CompactTrigger, Conversation, HistoryEntry, Role, Usage,
    check_budget, estimate_tokens,
};

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
pub struct TurnState {
    /// Input tokens of the most recent sample, if reported by usage.
    pub last_input_tokens: Option<u64>,
    /// Cumulative output tokens across samples in this run.
    pub total_output_tokens: u64,
    /// How many tool dispatch rounds have executed in this run.
    pub tool_rounds: u32,
    /// How many reactive compactions have run in this run.
    pub reactive_compacts: u8,
    /// How many plan-nudge reminders have been injected in this run.
    pub plan_nudges: u8,
    /// How many model continuations have been issued in this run.
    pub continuations: u8,
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

/// Maximum reactive compactions per turn.
///
/// Counts consecutive prompt-too-long failures like the legacy loop: two
/// successful compactions are attempted and the third consecutive failure
/// melts the turn. A successful sample resets the count.
pub const MAX_REACTIVE_COMPACTS: u8 = 3;

/// Decision produced by the planning/decision layer for one loop iteration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PolicyDecision {
    /// Proceed to the next sample or tool dispatch.
    Continue,
    /// Terminate the run with the given reason.
    Finish(StopReason),
    /// Inject a plan reminder and continue sampling.
    Steer(String),
    /// Compact context first, then retry the sample.
    CompactAndRetry,
    /// Abort the run with a human-readable cause.
    Fail(String),
}

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
}

/// Minimal message shape for a sample request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiteMessage {
    /// True for model messages, false for user messages.
    pub from_model: bool,
    /// Text payload of the message.
    pub text: String,
}

/// Minimal tool reference advertised to the model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolRef {
    /// Tool name as registered in the capability registry.
    pub name: String,
    /// One-line description shown to the model.
    pub description: String,
}

/// Request handed to the model gateway for one sample.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SampleRequest {
    /// System prompt text assembled by the prompt layer.
    pub system: String,
    /// Conversation snapshot for this sample.
    pub messages: Vec<LiteMessage>,
    /// Tools available in this sample.
    pub tools: Vec<ToolRef>,
}

/// One content block returned by the model gateway.
#[derive(Debug, Clone, PartialEq)]
pub enum SampleBlock {
    /// Plain assistant text.
    Text(String),
    /// A tool invocation request.
    ToolUse {
        /// Identifier pairing the request with its future result.
        call_id: String,
        /// Tool name as registered in the capability registry.
        name: String,
        /// Raw JSON input for the tool.
        input: Value,
    },
}

/// Response of one model sample.
#[derive(Debug, Clone, PartialEq)]
pub struct SampleResponse {
    /// Ordered content blocks of the assistant message.
    pub blocks: Vec<SampleBlock>,
    /// Input tokens reported by the provider, if any.
    pub input_tokens: Option<u64>,
    /// Output tokens reported by the provider, if any.
    pub output_tokens: Option<u64>,
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

/// Samples the model once; implemented by the provider adapter.
#[async_trait::async_trait]
pub trait ModelGateway: Send + Sync {
    /// Sample the model; `PromptTooLong` signals the loop to compact.
    async fn sample(&self, request: SampleRequest) -> Result<SampleResponse, SampleError>;
}

/// Dispatch tool calls with read-only parallelism and serial mutation.
///
/// Calls whose executor reports read-only and non-destructive run
/// concurrently via `join_all`; all other calls run serially afterwards,
/// one at a time, so mutations never interleave. Results are grouped by
/// class (read-only block first) and keep declaration order within each
/// class. Callers needing global declaration order must submit
/// single-class batches.
pub async fn dispatch_calls<E: ToolExecutor>(
    executor: &E,
    calls: Vec<ToolCall>,
) -> Vec<ToolResult> {
    let (read_only, mutating): (Vec<ToolCall>, Vec<ToolCall>) = calls
        .into_iter()
        .partition(|c| executor.is_read_only(&c.name) && !executor.is_destructive(&c.name));
    let mut out = Vec::with_capacity(read_only.len() + mutating.len());
    let concurrent =
        futures::future::join_all(read_only.into_iter().map(|call| executor.execute(call))).await;
    out.extend(concurrent);
    for call in mutating {
        out.push(executor.execute(call).await);
    }
    out
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

/// Maximum model continuations per turn.
pub const MAX_CONTINUATIONS: u8 = 2;
/// Maximum plan-steering reminders per turn.
pub const MAX_PLAN_NUDGES: u8 = 3;
/// Maximum Stop-hook blocks per turn before the loop proceeds anyway.
pub const MAX_STOP_BLOCKS: u8 = 3;

/// Static run configuration, frozen per session rather than per turn.
#[derive(Debug, Clone)]
pub struct RunConfig {
    /// Provider model identifier for sampling.
    pub model_name: String,
    /// Context window in tokens for budget checks.
    pub context_window: u64,
    /// Per-sample output cap handed to the provider.
    pub max_output_tokens: u32,
    /// Tool dispatch rounds per turn; reaching it stops with Completed.
    pub max_tool_rounds: u32,
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

/// Parks approval requests and delivers user decisions.
///
/// Timeout handling belongs to the implementation: an expired wait must
/// resolve to [`ApprovalResolution::Deny`], never park forever. Late
/// decisions for consumed ids must be dropped, never stored.
#[async_trait::async_trait]
pub trait ApprovalSource: Send + Sync {
    /// Block until a decision arrives for `call_id` or the wait expires.
    async fn decide(&self, call_id: &str, kind: AskKind, detail: &str) -> ApprovalResolution;

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
        history: Vec<LiteMessage>,
        trigger: CompactTrigger,
    ) -> Result<Compacted, CompactError>;
}

/// The turn state machine, generic over all seams for testability.
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
        }
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
        input: &str,
        system: &str,
        on_event: &(dyn Fn(Event) + Send + Sync),
    ) -> StopReason {
        let emit_msg = |msg: EventMsg| {
            on_event(Event {
                id: ctx.submission_id.clone(),
                msg,
            })
        };
        self.approvals.clear_stale();
        self.interrupt.reset();

        // Admission runs before TurnStarted: blocked input never enters
        // history and is never sampled.
        let admission = self.hooks.run(HookPoint::PromptSubmit, input).await;
        if !admission.allow {
            emit_msg(EventMsg::Error {
                message: admission.message,
                recoverable: true,
            });
            emit_msg(EventMsg::TurnCompleted);
            return StopReason::Completed;
        }
        if !admission.message.is_empty() {
            emit_msg(EventMsg::Warning {
                message: admission.message,
            });
        }

        conv.push(Role::User, input);
        emit_msg(EventMsg::TurnStarted);

        let mut warned = false;
        let mut compacted = false;
        let mut continuations: u8 = 0;
        let mut nudges: u8 = 0;
        let mut stop_blocks: u8 = 0;
        let mut reactive_compacts: u8 = 0;
        let mut state = TurnState::new();
        let mut last_input: Option<u64> = None;

        loop {
            // Checkpoint 1: loop head interrupt returns without sampling.
            if self.interrupt.is_triggered() {
                settle(conv, &last_input, &state, &emit_msg);
                emit_msg(EventMsg::TurnCompleted);
                return StopReason::Interrupted;
            }

            // Round ceiling stops with Completed, never with an error.
            // Checked before the budget line so a zero ceiling melts the
            // very first iteration.
            if state.rounds_exhausted(self.cfg.max_tool_rounds) {
                emit_msg(EventMsg::Warning {
                    message: format!(
                        "tool round limit reached ({}); stopping this turn",
                        self.cfg.max_tool_rounds
                    ),
                });
                break;
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
                        estimate_tokens(&flatten(conv)) + CONTEXT_OVERHEAD_TOKENS
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
                        match self.do_compact(conv, trigger, &emit_msg).await {
                            Ok(()) => continue,
                            Err(cause) => {
                                // Blocking failures abort; automatic
                                // failures downgrade to a warning.
                                if trigger == CompactTrigger::Blocking {
                                    settle(conv, &last_input, &state, &emit_msg);
                                    emit_msg(EventMsg::Error {
                                        message: cause,
                                        recoverable: false,
                                    });
                                    emit_msg(EventMsg::TurnCompleted);
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

            let request = SampleRequest {
                system: system.to_string(),
                messages: history_lite(conv),
                tools: self.executor.available_tools(),
            };
            let response = match self.model.sample(request).await {
                Err(SampleError::PromptTooLong) => {
                    reactive_compacts += 1;
                    if reactive_compacts >= MAX_REACTIVE_COMPACTS {
                        settle(conv, &last_input, &state, &emit_msg);
                        emit_msg(EventMsg::Error {
                            message: "prompt exceeds context window after 3 compactions"
                                .to_string(),
                            recoverable: false,
                        });
                        emit_msg(EventMsg::TurnCompleted);
                        return StopReason::Error("prompt too long".to_string());
                    }
                    if let Err(cause) = self
                        .do_compact(conv, CompactTrigger::Reactive, &emit_msg)
                        .await
                    {
                        settle(conv, &last_input, &state, &emit_msg);
                        emit_msg(EventMsg::Error {
                            message: cause,
                            recoverable: false,
                        });
                        emit_msg(EventMsg::TurnCompleted);
                        return StopReason::Error("reactive compaction failed".to_string());
                    }
                    continue;
                }
                Err(other) => {
                    settle(conv, &last_input, &state, &emit_msg);
                    emit_msg(EventMsg::Error {
                        message: other.to_string(),
                        recoverable: false,
                    });
                    emit_msg(EventMsg::TurnCompleted);
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

            let mut text = String::new();
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
                }
            }
            if !text.is_empty() {
                conv.push(Role::Assistant, text);
            }
            emit_msg(EventMsg::AgentMessageComplete);

            if calls.is_empty() {
                if response.truncated && continuations < MAX_CONTINUATIONS {
                    continuations += 1;
                    emit_msg(EventMsg::Warning {
                        message: format!(
                            "output truncated at max_tokens; continuing ({continuations}/2)"
                        ),
                    });
                    conv.push(Role::User, CONTINUATION_PROMPT);
                    continue;
                }
                let unfinished = self.plans.unfinished();
                if unfinished > 0 && nudges < MAX_PLAN_NUDGES {
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
                if !stop.allow && stop_blocks < MAX_STOP_BLOCKS {
                    stop_blocks += 1;
                    let reason = if stop.message.is_empty() {
                        "(hook gave no reason)".to_string()
                    } else {
                        stop.message.clone()
                    };
                    emit_msg(EventMsg::Warning {
                        message: format!("Stop hook blocked turn completion ({stop_blocks}/3)"),
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
            if self.interrupt.is_triggered() {
                let results: Vec<ToolResult> = calls.iter().map(interrupted_result).collect();
                conv.push(Role::User, format_tool_results(&results));
                settle(conv, &last_input, &state, &emit_msg);
                emit_msg(EventMsg::TurnCompleted);
                return StopReason::Interrupted;
            }
            let results = self.execute_calls(&calls, &emit_msg).await;
            conv.push(Role::User, format_tool_results(&results));
            state.bump_tool_round();
        }

        settle(conv, &last_input, &state, &emit_msg);
        emit_msg(EventMsg::TurnCompleted);
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
        emit(EventMsg::CompactStarted);
        let done = self
            .compactor
            .compact(history_lite(conv), trigger)
            .await
            .map_err(|e| e.to_string())?;
        conv.replace(vec![HistoryEntry {
            role: Role::User,
            text: done.summary.clone(),
        }]);
        conv.settle(Usage {
            input_tokens: estimate_tokens(&done.summary) + CONTEXT_OVERHEAD_TOKENS,
            output_tokens: 0,
        });
        emit(EventMsg::CompactCompleted);
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
    async fn execute_calls(
        &self,
        calls: &[ToolCall],
        emit: &(dyn Fn(EventMsg) + Send + Sync),
    ) -> Vec<ToolResult> {
        use std::collections::HashMap;

        for call in calls {
            emit(EventMsg::ToolCallBegin {
                call_id: call.call_id.clone(),
                name: call.name.clone(),
            });
        }

        // Pre-tool hooks run per call before policy; blocks never execute
        // and never reach the approval gate.
        let mut blocked: HashMap<String, ToolResult> = HashMap::new();
        let mut live: Vec<&ToolCall> = Vec::with_capacity(calls.len());
        for call in calls {
            let report = self
                .hooks
                .run_tool(HookPoint::PreToolUse, &call.name, &call.input, None)
                .await;
            if !report.message.is_empty() {
                emit(EventMsg::Warning {
                    message: report.message.clone(),
                });
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
                        detail: detail.clone(),
                    });
                    serial.push(SerialCall::Approval { call, kind, detail });
                }
            }
        }

        // Read-only allows run concurrently; order within the batch follows
        // declaration order via the result map below.
        let concurrent = futures::future::join_all(parallel.into_iter().map(|call| async move {
            let id = call.call_id.clone();
            let output = self.executor.execute(call.clone()).await;
            (id, call, output)
        }))
        .await;
        for (id, call, output) in concurrent {
            self.post_tool(call, &output, emit).await;
            results.insert(id, result_of(call, output));
        }

        // Mutations and approvals run serially with an interrupt check per
        // item; the rest fill with interrupted results instead of breaking,
        // so every declared call still gets its slot.
        for item in serial {
            let (call, approved) = match item {
                SerialCall::Direct(call) => (call, true),
                SerialCall::Approval { call, kind, detail } => {
                    match self.approvals.decide(&call.call_id, kind, &detail).await {
                        ApprovalResolution::AllowOnce | ApprovalResolution::AllowAlways => {
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
            };
            if !approved {
                continue;
            }
            if self.interrupt.is_triggered() {
                results.insert(call.call_id.clone(), interrupted_result(call));
                continue;
            }
            let output = self.executor.execute(call.clone()).await;
            self.post_tool(call, &output, emit).await;
            results.insert(call.call_id.clone(), result_of(call, output));
        }

        // Merge hook-blocked slots, then emit ends and results strictly in
        // declaration order.
        results.extend(blocked);
        let mut ordered = Vec::with_capacity(calls.len());
        for call in calls {
            let result = results
                .remove(&call.call_id)
                .expect("every call has a result");
            emit(EventMsg::ToolCallEnd {
                call_id: result.call_id.clone(),
                is_error: result.is_error,
            });
            ordered.push(result);
        }
        ordered
    }

    /// Post-tool hooks fire only for executed calls and never block.
    async fn post_tool(
        &self,
        call: &ToolCall,
        output: &ToolResult,
        emit: &(dyn Fn(EventMsg) + Send + Sync),
    ) {
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
    }
}

/// Turn-driving seam consumed by session actors.
///
/// The blanket implementation below wires every [`RunLoop`] automatically;
/// actors stay generic over this trait instead of the seven concrete seam
/// types, so transport never names capabilities.
#[async_trait::async_trait]
pub trait TurnDriver: Send + Sync {
    /// Drive one turn, emitting wire events through `on_event`.
    async fn drive_turn(
        &self,
        ctx: &RunContext,
        conv: &mut Conversation,
        input: &str,
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

    /// Interrupt handle observed by driven turns, if the driver exposes
    /// one. Composition roots bridge scoped stop signals into it.
    fn interrupt_handle(&self) -> Option<InterruptHandle> {
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
        input: &str,
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

    fn interrupt_handle(&self) -> Option<InterruptHandle> {
        Some(self.interrupt.clone())
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
        input: &str,
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

    fn interrupt_handle(&self) -> Option<InterruptHandle> {
        self.as_ref().interrupt_handle()
    }
}

/// Serial dispatch item: direct execution or parked approval.
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

/// Render tool results as history text, keeping ids for pairing audits.
fn format_tool_results(results: &[ToolResult]) -> String {
    results
        .iter()
        .map(|r| {
            format!(
                "[{}] {}: {}",
                r.call_id,
                if r.is_error { "error" } else { "ok" },
                r.content
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Settle usage after sampling: no completed sample means no-op, so the
/// carry is never covered and no TokenCount is emitted.
fn settle(
    conv: &mut Conversation,
    last_input: &Option<u64>,
    state: &TurnState,
    emit: &(dyn Fn(EventMsg) + Send + Sync),
) {
    let Some(input) = last_input else {
        return;
    };
    conv.settle(Usage {
        input_tokens: *input,
        output_tokens: state.total_output_tokens,
    });
    emit(EventMsg::TokenCount {
        input_tokens: *input,
        output_tokens: state.total_output_tokens,
    });
}

/// Snapshot the conversation as model-facing messages.
fn history_lite(conv: &Conversation) -> Vec<LiteMessage> {
    conv.snapshot()
        .iter()
        .map(|entry| LiteMessage {
            from_model: entry.role == Role::Assistant,
            text: entry.text.clone(),
        })
        .collect()
}

/// Flatten history text for fallback token estimates.
fn flatten(conv: &Conversation) -> String {
    conv.snapshot()
        .iter()
        .map(|entry| entry.text.as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn turn_state_round_ceiling_is_a_stop_not_an_error() {
        let mut state = TurnState::new();
        for _ in 0..DEFAULT_MAX_TOOL_ROUNDS {
            state.bump_tool_round();
        }
        assert!(state.rounds_exhausted(DEFAULT_MAX_TOOL_ROUNDS));
    }

    #[tokio::test]
    async fn dispatch_groups_read_only_first_and_keeps_order() {
        struct Echo;
        #[async_trait::async_trait]
        impl ToolExecutor for Echo {
            async fn execute(&self, call: ToolCall) -> ToolResult {
                ToolResult {
                    call_id: call.call_id.clone(),
                    content: call.name.clone(),
                    is_error: call.name == "boom",
                }
            }

            fn is_read_only(&self, tool: &str) -> bool {
                tool != "write_file"
            }

            fn is_destructive(&self, _tool: &str) -> bool {
                false
            }
        }

        fn call(id: &str, name: &str) -> ToolCall {
            ToolCall {
                call_id: id.to_string(),
                name: name.to_string(),
                input: serde_json::Value::Null,
            }
        }

        let results = dispatch_calls(
            &Echo,
            vec![
                call("m1", "write_file"),
                call("r1", "read_a"),
                call("r2", "boom"),
            ],
        )
        .await;
        let ids: Vec<_> = results.iter().map(|r| r.call_id.as_str()).collect();
        assert_eq!(ids, vec!["r1", "r2", "m1"]);
        // Business failures ride as Ok results so the model can self-correct.
        assert!(results.iter().find(|r| r.call_id == "r2").unwrap().is_error);
    }

    #[test]
    fn idempotency_keys_are_namespaced_by_run() {
        let ctx = RunContext {
            run_id: "run-1".to_string(),
            submission_id: "sub-1".to_string(),
            input: "hi".to_string(),
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

    struct FakeExecutor {
        read_only: Vec<String>,
        results: HashMap<String, ToolResult>,
        executed: Mutex<Vec<String>>,
    }

    impl FakeExecutor {
        fn new(read_only: &[&str]) -> Self {
            Self {
                read_only: read_only.iter().map(|s| s.to_string()).collect(),
                results: HashMap::new(),
                executed: Mutex::new(Vec::new()),
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
                },
                HookPoint::Stop => {
                    let mut left = self.lock_blocks();
                    if *left > 0 {
                        *left -= 1;
                        return HookReport {
                            allow: false,
                            message: "stop it".to_string(),
                        };
                    }
                    HookReport {
                        allow: true,
                        message: String::new(),
                    }
                }
                _ => HookReport {
                    allow: true,
                    message: String::new(),
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
                }),
            }
        }
    }

    struct FakeApprovals {
        resolutions: HashMap<String, ApprovalResolution>,
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
            _history: Vec<LiteMessage>,
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
    fn build_loop(
        executor: FakeExecutor,
        policy: FakePolicy,
        hooks: FakeHooks,
        model: FakeModel,
        approvals: FakeApprovals,
        plans: FakePlans,
        compactor: FakeCompactor,
        max_tool_rounds: u32,
        interrupt: InterruptHandle,
    ) -> RunLoop<
        FakeExecutor,
        FakePolicy,
        FakeHooks,
        FakeModel,
        FakeApprovals,
        FakePlans,
        FakeCompactor,
    > {
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
        .run_turn(&fx.ctx, conv, "hi", "sys", &|e| {
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
        .run_turn(&fx.ctx, conv, "hi", "sys", &|e| {
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
        let approvals = FakeApprovals { resolutions };
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
        .run_turn(&fx.ctx, conv, "hi", "sys", &|e| {
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
            .map(|e| e.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(history.contains("nope"));
        assert!(history.contains("[c2] error"));
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
        .run_turn(&fx.ctx, conv, "hi", "sys", &|e| {
            events.lock().unwrap_or_else(|e| e.into_inner()).push(e);
        })
        .await;
        assert_eq!(outcome, StopReason::Completed);
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
        .run_turn(&fx.ctx, conv, "hi", "sys", &|e| {
            events.lock().unwrap_or_else(|e| e.into_inner()).push(e);
        })
        .await;
        assert_eq!(outcome, StopReason::Completed);
        let reminders = conv
            .snapshot()
            .iter()
            .filter(|e| e.text == CONTINUATION_PROMPT)
            .count();
        assert_eq!(reminders, 2);
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
        .run_turn(&fx.ctx, conv, "hi", "sys", &|e| {
            events.lock().unwrap_or_else(|e| e.into_inner()).push(e);
        })
        .await;
        assert_eq!(outcome, StopReason::Completed);
        let reminders = conv
            .snapshot()
            .iter()
            .filter(|e| e.text == "PLAN-REMINDER")
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
        .run_turn(&fx.ctx, conv, "hi", "sys", &|e| {
            events.lock().unwrap_or_else(|e| e.into_inner()).push(e);
        })
        .await;
        assert_eq!(outcome, StopReason::Completed);
        let blocks = conv
            .snapshot()
            .iter()
            .filter(|e| e.text.contains("Stop hook blocked turn completion"))
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
        .run_turn(&fx.ctx, conv, "hi", "sys", &|e| {
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
        .run_turn(&fx.ctx, conv, "hi", "sys", &|e| {
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
            },
            fx.interrupt.clone(),
        )
        .run_turn(&fx.ctx, conv, "hi", "sys", &|e| {
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
}
