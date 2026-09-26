# crates/runtime/runner/ — the run loop state machine and its anticorruption trait seams

English | [中文](README.zh.md)

| Item | Role |
|---|---|
| `Cargo.toml` | Crate manifest; depends only on `state-store`, `wavecode-wire`, and `infrastructure-base` |
| `src/lib.rs` | `RunLoop<E, P, H, M, A, T, C>`: the sample → decide → execute → recover state machine, plus the DTO vocabulary (`RunContext`, `StopReason`, `ToolCall`/`ToolResult`, `PolicyVerdict`, `SampleRequest`/`SampleResponse`) and every anticorruption trait (`ToolExecutor`, `PolicyDecider`, `HookGateway`, `ModelGateway`, `ApprovalSource`, `PlanTracker`, `GoalTracker`, `Compactor`, `TurnDriver`) |
| `tests/benchmarks.rs` | Offline benchmarks pinning turn performance and shape against `benchmarks/baseline.json`; live-provider smoke runs only behind `LIVE=1` and never fails |

The loop is generic over seven seam traits plus the goal tracker and
compactor; concrete capabilities (tools, policy, hooks, models) are
wired behind these traits by the composition root, so this crate can
never import one. Termination is structural: a hard tool-round ceiling
(re-armed by an open session goal), reactive-compaction limits, and a
repeat-call breaker all stop the turn instead of erroring. `TurnDriver`
is the blanket-implementation seam session actors consume, keeping
transport and actors away from the individual capabilities.
