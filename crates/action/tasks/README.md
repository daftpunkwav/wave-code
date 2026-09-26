# crates/action/tasks/ — capability-neutral task lifecycle seam plus test fakes

English | [中文](README.zh.md)

| Item | Role |
|---|---|
| `Cargo.toml` | Crate manifest; no dependencies |
| `src/lib.rs` | The task vocabulary (`TaskRequest`, `TaskKind`, `TaskState`, `TaskOutcome`, `TaskInfo` with lineage) and the `TaskService` seam (spawn/query/stop plus an optional `continue_task` follow-up hook); ships `FakeTaskService` (immediate echo completion) and `ScriptedTaskService` (scripted outcomes) for tests |

The seam carries lifecycle only — depth and parent fields ride along
for nested-child accounting, but execution knowledge does not exist
here. Layer direction forbids action crates from naming runtime types,
so the composition root maps this vocabulary onto the child runtime;
implementors that never override `continue_task` keep compiling
untouched.
