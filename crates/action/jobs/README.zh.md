# crates/action/jobs/ — 具备 wait/cancel/notice 语义的后台 shell 作业

[English](README.md) | 中文

| 条目 | 职责 |
|---|---|
| `Cargo.toml` | crate 清单；依赖 `infrastructure-base`（shell 解析）、`runtime-child`（共享完成通道）与 `wavecode-tools`（供 tools 模块使用） |
| `src/lib.rs` | `JobService`：把 shell 命令作为受管作业派生，按 owner 限容（`MAX_JOBS_PER_OWNER`）、有界输出日志；`wait` 只做快照不杀进程，`cancel` 杀掉整棵进程树；每条终止路径都会向 `ChildRuntime` 队列投递通知——仍处 `foreground` 的运行除外（shell 工具内联等待、结果直接交付；运行被晋升时由 `mark_notified` 重新打开通知） |
| `src/tools.rs` | 建立在该服务之上的可被模型调用的 `job_spawn` / `job_wait` / `job_cancel` / `job_output` 工具，以及 `ForegroundRuns`——shell 工具的 `RunHandoff` 接缝，把前台超时变成晋升 |

作业是不再阻塞 turn 的长时间 shell 工作：`job_spawn` 立即返回
`job-N` id，之后由模型轮询。完成通知复用父 loop 为子任务准备的同一
条通道，因此不存在第二条通知路径。服务本身不了解 policy、driver 或
session——tools 模块是使它可被模型调用的唯一能力边。
