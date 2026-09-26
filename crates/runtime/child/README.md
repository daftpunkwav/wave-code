# crates/runtime/child/ — tracked background child tasks with structural depth isolation

English | [中文](README.zh.md)

| Item | Role |
|---|---|
| `Cargo.toml` | Crate manifest; the only workspace dependency is `infrastructure-base` (interrupt primitives) |
| `src/lib.rs` | `ChildRuntime`: spawn/query/stop background tasks — `ChildSpec`/`ChildTicket`, depth cap (`MAX_CHILD_DEPTH`), lineage chains, `TaskResult` outcomes, and the bounded completion-notification queue |

Depth isolation is structural, not conventional: the work factory
receives only a `ChildTicket` (identity plus a stop signal), never a
`ChildRuntime` reference, so a child spawning its own grandchildren is
unconstructable — deeper generations exist only through parent-side
follow-up spawns that bump `depth` and set `parent`, refused past the
cap with an explicit `Failed` result. Panics are caught at the task
boundary and recorded as failures; completion notices flow through a
data-only `CompletionSink` that carries no spawn capability.
