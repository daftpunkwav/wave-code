# crates/runtime/scheduler/ — scheduling primitives with explicit clocks and durable cron entries

English | [中文](README.zh.md)

| Item | Role |
|---|---|
| `Cargo.toml` | Crate manifest; no workspace dependencies (serde/tokio only) |
| `src/lib.rs` | `TaskQueue` (priority levels with FIFO stability), `DelayQueue` (deadline holds), `CronField`/`CronSpec`/`CronDaemon` (five-field cron parsing and edge-triggered matching), `ConcurrencyLimit` (semaphore gate) |
| `src/durable.rs` | `Scheduler` / `ScheduleEntry`: cron entries persisted to `<home>/.wavecode/schedule.json`, reloaded on construct with fire-once missed-fire catch-up; running work reads back as interrupted, never resumed |

Time enters as parameters (epoch seconds, civil fields), never as
hidden reads, so every schedule is deterministic under test. Cron
fields that parse but can never match a real moment are rejected at
parse time — accepting them would arm a schedule that silently never
fires. Persistence deliberately stores no running jobs: OS processes
cannot survive a restart, so in-flight work from a dead process is
reported as interrupted.
