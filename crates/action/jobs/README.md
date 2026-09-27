# crates/action/jobs/ — background shell jobs with wait/cancel/notice semantics

English | [中文](README.zh.md)

| Item | Role |
|---|---|
| `Cargo.toml` | Crate manifest; depends on `infrastructure-base` (shell resolution), `runtime-child` (the shared completion channel), and `wavecode-tools` (for the tools module) |
| `src/lib.rs` | `JobService`: spawns shell commands as tracked jobs with a per-owner cap (`MAX_JOBS_PER_OWNER`), bounded output logs, `wait` that snapshots without killing, and `cancel` that kills the whole process tree; every terminal path files a notice on the `ChildRuntime` queue — except a still-`foreground` run (the shell tool waits inline, so the result is delivered directly; `mark_notified` re-arms the notice when the run is promoted) |
| `src/tools.rs` | The model-invokable `job_spawn` / `job_wait` / `job_cancel` / `job_output` tools over the service, plus `ForegroundRuns` — the shell tool's `RunHandoff` seam that turns a foreground timeout into a promotion |

A job is long shell work that stops blocking the turn: `job_spawn`
returns a `job-N` id immediately and the model polls from there.
Completion notices reuse the exact channel the parent loop already
drains for child tasks, so there is no second notification path. The
service knows nothing about policy, drivers, or sessions — the tools
module is the single capability edge that renders it model-invokable.
