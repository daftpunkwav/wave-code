# Subsystem: the core loop

The RunLoop lives in `crates/runtime/runner/src/lib.rs` and owns one turn end to end: sample → decide → dispatch → recover. It depends only on trait seams and DTOs; concrete capabilities are injected by the composition root (`crates/operations/bootstrap`). Changing this file requires updating `docs/architecture.md`.

## Seams

`RunLoop<E, P, H, M, A, T, C>` is generic over seven traits, all in the same file:

| Seam | Role | Production adapter |
| --- | --- | --- |
| `ModelGateway` | One sample; `sample_streaming` feeds deltas; `PromptTooLong` signals compaction | `wavecode-llm` adapter |
| `ToolExecutor` | Executes one call; exposes `is_read_only` / `is_destructive` / `available_tools` | `ToolAdapter` (`crates/operations/bootstrap/src/tool_adapter.rs`) |
| `PolicyDecider` | Allow / Ask / Deny per call before execution | `PolicyAdapter` over `wavecode-sandbox` (`crates/operations/bootstrap/src/policy_adapter.rs`) |
| `HookGateway` | Runs lifecycle hooks; only blocking points may veto | `hook_adapter.rs` |
| `ApprovalSource` | Parks a wait until a user decision or timeout | `gate_adapter.rs` over `safety-gate` |
| `PlanTracker` | Unfinished plan items + reminder text (plan nudges) | plan tools |
| `GoalTracker` | Whether the durable objective is still open + reminder text (goal continuations) | `goal_adapter.rs` over the goal store (`NoGoal` by default) |
| `Compactor` | Summarizes history on trigger | `ContextCompactor` (`crates/operations/bootstrap/src/compactor.rs`) |

Actors never name these eight types; they drive a `TurnDriver` (blanket-impl'd for every `RunLoop` and `Arc<T>`), which also exposes `set_permission_mode`, `set_model`, `end_session`, and the shared `inbox_handle`. Per-run interrupt overrides live on the loop's `RunInterrupts` registry (`RunLoop::run_interrupts()`), not on the driver trait: the child task service registers one handle per child, so a `task_stop` bridges into exactly that turn.

## Turn lifecycle (`run_turn`)

1. Session-owned turns (no run-scoped interrupt registration under the turn's run id) run `approvals.clear_stale()` + `interrupt.reset()`; child turns skip both, so a child start can never wipe a parent approval parked mid-wait or swallow a user interrupt racing its start.
2. **Admission**: the `PromptSubmit` hook runs *before* `TurnStarted`. A blocked input never enters history and is never sampled; the turn ends with `Error` + `TurnCompleted` and `StopReason::Completed`.
3. Push the user entry, emit `TurnStarted`, enter the round loop.
4. **Checkpoint 1 — loop head**: a triggered interrupt settles usage, emits `TurnCompleted { interrupted: true }`, and returns `StopReason::Interrupted` without sampling.
5. Drain `NextTurn` steering from the inbox as user history.
6. **Round ceiling**: `state.rounds_exhausted(max_tool_rounds × (round_rearms + 1))` emits a `Warning`, settles the turn, and returns `StopReason::MaxToolRounds` — a hard stop, never an error, but distinguishable from natural completion. Default `DEFAULT_MAX_TOOL_ROUNDS = 256` — wavecode targets super-long-horizon work; the `max_tool_rounds` config key overrides it per session, and the warning names the key. **An open session goal re-arms the ceiling** instead of stopping (`MAX_GOAL_REARMS = 7`, so a turn spans 8 ceilings = an effective bound of 2048 rounds); each re-arm injects the goal reminder with the budget line and counts against the bound, and a blocked / paused / completed or empty objective re-arms nothing.
7. **Budget line** (`state-store` levels): `Warn` emits a once-per-turn warning; `AutoCompact`/`Blocking` run `do_compact` once per turn (blocking failure aborts the turn; auto failure downgrades to a warning).
8. Drain `NextStep` steering + direct injections *after* the budget line, then sample.
9. `PromptTooLong` → reactive compaction (`CompactTrigger::Reactive`). A successful sample resets the counter; `MAX_REACTIVE_COMPACTS = 3` consecutive failures melt the turn with an error. Transport/timeout errors fail the turn after settle.
10. Emit `AgentMessageComplete`, then:
    - no calls + `truncated` → push `CONTINUATION_PROMPT` and continue (`MAX_CONTINUATIONS = 2`);
    - unfinished plan items → nudge (`MAX_PLAN_NUDGES = 3`);
    - open session goal (`Active` with a non-empty objective) → continue with the goal's own status render plus the loop's live **budget line** (context used / tool round of the ceiling / continuation of cap), `MAX_GOAL_CONTINUATIONS = 8` per turn, then stop anyway. This is what removes the human "continue" from a long task; a `Blocked` / `Paused` / `Completed` goal never steers, because those are statements that work should stop. Child runs and sessions without a goal store use `NoGoal` (no continuation, as before);
    - `Stop` hook block → feed the reason back and continue (`MAX_STOP_BLOCKS = 3`, then proceed anyway);
    - otherwise break with `Completed`.
11. **Checkpoint 3 — pre-tool**: an interrupt here synthesizes `interrupted` results for every declared call, preserving call/result pairing without executing anything.
12. Dispatch (`execute_calls`, below), append results as a user entry, `bump_tool_round`, loop.

`settle` runs on every sampling exit; `TokenCount` is emitted only when a sample completed (carrying the session `context_window` and estimated `context_used` for frontend context meters), and `TurnCompleted` is emitted exactly once, last.

## Dispatch pipeline (`execute_calls`)

Fixed order: **all `ToolCallBegin` events upfront in declaration order; all `ToolCallEnd` events after, in declaration order.** Every declared call gets exactly one result slot, so pairing cannot break (a trailing fallback fills impossible gaps with an internal-error slot). Each `ToolCallEnd` carries a bounded output preview (`ToolCallPreview`, capped at 4 KiB, character-boundary safe) so frontends can render result heads without the full transcript body.

`TurnStarted` names the model the turn samples (`model: String`, empty when a driver has none), because `/model` can switch models mid-session and per-turn attribution is what makes the metrics table answer "is this tool weak, or is this model bad at it".

Each `ToolCallEnd` also carries the pipeline exit that produced it (`outcome: ToolOutcome`) and the body's wall-clock cost (`duration_ms`). `is_error` says a call did not succeed; `outcome` says who stopped it — the tool itself, policy, a hook, the run's tool surface, the user at the approval prompt, or an interrupt. A non-`Executed` outcome always reports `duration_ms: 0`, so a refusal can never be read as a slow tool. `operations-observe` folds these into per-tool counters (`Metrics::tools`); the exit taxonomy is closed (`ToolOutcome` covers every branch in `execute_calls`), so adding a new exit means adding a variant, and the metrics split follows without further wiring.

Per call: duplicate call-id admission (first occurrence runs; duplicates get error slots without executing — paired lookups would otherwise consume a slot twice) → per-run allowlist (`RunAllowlist`, fork-scoped `allowed-tools`; denial is a business error, and restricted runs never even see denied tools in `available_tools`) → `PreToolUse` hook (block ⇒ error slot, never reaches policy) → `PolicyDecider`:

- `Allow` → read-only (`is_read_only && !is_destructive`) calls join a concurrent batch capped at 8 in flight (`buffer_unordered`); everything else runs serially.
- `Deny` → error result with the reason.
- `Ask` → `ApprovalRequested` event, then a serial park on `ApprovalSource`. `AllowOnce`/`AllowAlways` execute; `Deny` returns the reason; `Interrupted` fills an interrupted slot. The serial loop re-checks the interrupt per item; remaining items fill interrupted slots instead of aborting the batch.

`PostToolUse` fires only for executed calls and never blocks. A deliberate divergence from the legacy fast path: policy sees *every* call, including read-only ones — the old deny-bypass is removed, only parallelism is kept.

## Steering inbox

`InboxHandle` is a shared, cloneable handle with three queues: `NextTurn` (applied at the loop head), `NextStep` (applied right before the next sample), and `inject` (same point as `NextStep`). Steered messages ride normal history as user entries, so budgets, compaction, and pairing treat them like any input; empty texts are dropped (providers reject empty user messages). `cancel(keep_next_turn)` drops pending items at interrupt time. Frontends steer through the actor's client, which holds the loop's `inbox_handle`.

## Event ordering contract

Deltas (`AgentMessageDelta` / `AgentThinkingDelta`) strictly precede their sample's `AgentMessageComplete`; `ToolCallBegin` all precede all `ToolCallEnd` within a batch; `CompactStarted`/`CompactCompleted` bracket compaction (with `PreCompact`/`PostCompact` hooks around them); `TurnCompleted` is exactly once and last. Replay and frontends rely on this — see `docs/subsystems/sessions-state.md`.
