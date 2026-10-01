# runtime/runner/ agent rules

The state machine and seams live in [README.md](README.md).

## The loop

- Workspace dependencies are `state-store`, `wavecode-wire`, and
  `infrastructure-base`. External dependencies are `thiserror`,
  `async-trait`, `serde_json`, and `futures`.
- Capabilities cross this crate as traits: `ToolExecutor`,
  `PolicyDecider`, `HookGateway`, `ModelGateway`, `ApprovalSource`,
  `PlanTracker`, `GoalTracker`, `Compactor`, and `TurnDriver`. Do not
  import a concrete capability crate.
- `TurnDriver` is the seam session actors consume.
- The tool-round ceiling, reactive compaction limits, and the
  repeat-call breaker stop the turn.
- Every declared tool call gets exactly one result slot, including
  the interrupt path and the trailing internal-error fallback.
- Empty steering and injection texts are dropped before they enter
  history.
- Side effects use `RunContext::idempotency_key`.
- A change to the state machine updates `docs/architecture.md`,
  `docs/architecture.zh.md`, `docs/subsystems/core-loop.md`, and
  `docs/subsystems/core-loop.zh.md` in the same change.

## Benchmarks

- `tests/benchmarks.rs` is the executable form of `benchmarks/run.rs`.
  A change to rounds or bounds updates both.
- Timing multipliers stay in `benchmarks/baseline.json`.
- The live-provider smoke runs only behind `LIVE=1` and does not fail
  the suite.
