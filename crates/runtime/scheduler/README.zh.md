# crates/runtime/scheduler/ — 显式时钟的调度原语与持久化 cron 条目

[English](README.md) | 中文

| 条目 | 职责 |
|---|---|
| `Cargo.toml` | crate 清单；无 workspace 依赖（仅 serde/tokio） |
| `src/lib.rs` | `TaskQueue`（优先级分层、层内 FIFO）、`DelayQueue`（按截止时间暂存）、`CronField`/`CronSpec`/`CronDaemon`（五段 cron 解析与边沿触发匹配）、`ConcurrencyLimit`（信号量闸门） |
| `src/durable.rs` | `Scheduler` / `ScheduleEntry`：cron 条目持久化到 `<home>/.wavecode/schedule.json`，构造时重载并对错过的窗口补触发一次；运行中的工作读回为 interrupted，绝不报成 resumed |

时间以参数形式进入（epoch 秒、civil 字段），绝无隐藏读取，因此每个
调度在测试下都是确定性的。能解析但永远匹配不到真实时刻的 cron
字段在解析期就被拒绝——接受它们等于武装一条静默永不触发的调度。
持久化有意不保存运行中的任务：OS 进程无法在重启后存活，因此上一个
进程的在途工作会被上报为 interrupted。
