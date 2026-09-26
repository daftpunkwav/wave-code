# crates/operations/bootstrap/ — composition root wiring concrete capabilities behind the run-loop seams

English | [中文](README.zh.md)

| Item | Role |
|---|---|
| `Cargo.toml` | Crate manifest; the only place that names concrete capability crates (`wavecode-tools`, `wavecode-sandbox`, `wavecode-hooks`, `wavecode-llm`, `wavecode-memory`, `wavecode-skills`, `wavecode-mcp`, `wavecode-context`) |
| `src/lib.rs` | Crate root; re-exports the adapters and the session assembly surface |
| `src/session.rs` | `assemble_session` / `SessionHandle`: full-session composition — config, provider, permissions (`Permissions`, `load_permissions`, `confinement_status`), adapters, run loop, actor, client |
| `src/tool_adapter.rs` | `ToolAdapter`: `runtime_runner::ToolExecutor` over the `wavecode-tools` registry |
| `src/policy_adapter.rs` | `PolicyAdapter`: `PolicyDecider` over `wavecode-sandbox`, with registry-backed tool attributes |
| `src/hook_adapter.rs` | `HookAdapter`: `HookGateway` over `wavecode-hooks` |
| `src/model_adapter.rs` | `ModelAdapter`: `ModelGateway` over `wavecode-llm`, folding provider streams into response blocks |
| `src/gate_adapter.rs` | `GateApprovalSource` + `HeadlessDeny`: parks approval/question waits on `safety-gate` with timeout |
| `src/compactor.rs` | `ContextCompactor`: `Compactor` over `wavecode-context`, appending journal and task-list footers |
| `src/evicting_gateway.rs` | `EvictingGateway`: gateway decorator running the shared cache-preserving tool-result eviction pass |
| `src/prune_adapter.rs` | `PruningExecutor`: executor decorator spilling oversized tool outputs to the side-store |
| `src/rate_limit.rs` | `RateLimitedModel`: token-bucket throttle in front of a chat model |
| `src/composite.rs` | `CompositeExecutor`: merges registry tools with late-registered native tools |
| `src/native.rs` | `NativeExecutor` / `NativeTool`: in-process tools as plain handlers |
| `src/plan_adapter.rs` | `TodoPlanTracker`: `PlanTracker` over the legacy todo store |
| `src/goal_adapter.rs` | `GoalTrackerAdapter`: `GoalTracker` over the session goal store (read-only from the loop) |
| `src/child_service.rs` | `TurnChildService`: runs full turns as child tasks behind the `action-tasks` seam |
| `src/memory_finish.rs` | `MemoryFinisher` / `SessionMemory`: model-distilled session-end memory extraction |
| `src/history_journal.rs` | `JournalSink`: mirrors history mutations into the write-ahead journal, rebuilds resumable history |
| `src/grants_sink.rs` | `GrantSink`: persists human "always allow" decisions as durable grant records |
| `src/metrics_tap.rs` | `MetricsTap`: `EventTap` folding a session's events into `operations-observe` ledger samples |
| `src/status_queries.rs` | `SessionStatus`: implements `operations_actor::StatusQueries` over the plan/goal/snapshot stores |
| `src/snapshot_tools.rs` | Snapshot (read-only) and restore (approval-gated) tools over the checkpoint store |
| `src/plugin_inventory.rs` | `PluginSummary`: plugin-pack discovery mapped to frontend display rows |
| `src/environment.rs` | `describe`: the system prompt's factual host/cwd/date paragraph |
| `tests/workspace_layers.rs` | Mechanical layer check reading the real dependency graph via `cargo metadata` |
| `tests/cache_prefix_stability.rs` | Regression guards for the provider prompt-cache prefix under eviction |

This crate is the top of the dependency DAG: runtime, state, action,
safety, and capability crates never depend on it. Each adapter maps one
`runtime-runner` trait onto one concrete crate, keeping policy in the
legacy crates and mapping here, so neither side names the other.
Assembly fails hard on missing config, provider, or credentials, and
warns-and-continues on soft degradation (memory, skills, hooks,
catalog).
