# crates/action/workflow/ — validated DAG execution, Ralph loops, and durable schedules

English | [中文](README.zh.md)

| Item | Role |
|---|---|
| `Cargo.toml` | Crate manifest; depends on `action-tasks`, `runtime-scheduler` (the vocabulary-tier scheduler), and `wavecode-tools` (for the tools module) |
| `src/lib.rs` | `WorkflowSpec` validation (unknown deps and cycles are errors) and wave-by-wave execution with bounded fan-out (`DEFAULT_FANOUT_PARALLEL`, `MAX_FANOUT_PARALLEL`); the Ralph loop respawning a fresh child per round with the same immutable objective until `RALPH_DONE` or round exhaustion |
| `src/tools.rs` | The model-invokable `workflow_run` / `ralph_run` / `schedule` tools: DAG runs through the task service, Ralph loops, and durable cron entries on the shared `Scheduler` |

The engine names only the `action-tasks` seam plus the passive
scheduler — never state, operations, transport, or frontends; the
tools module adds the single capability edge (`wavecode-tools`) that
renders it model-invokable. Any step failure fails the whole run
naming the step id, and there is no partial retry.
