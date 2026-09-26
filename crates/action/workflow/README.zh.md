# crates/action/workflow/ — 校验式 DAG 执行、Ralph 循环与持久化调度

[English](README.md) | 中文

| 条目 | 职责 |
|---|---|
| `Cargo.toml` | crate 清单；依赖 `action-tasks`、`runtime-scheduler`（vocabulary 层的调度器）与 `wavecode-tools`（供 tools 模块使用） |
| `src/lib.rs` | `WorkflowSpec` 校验（未知依赖与依赖环都是错误）与逐波执行、有界 fan-out（`DEFAULT_FANOUT_PARALLEL`、`MAX_FANOUT_PARALLEL`）；Ralph 循环每轮用同一不可变目标重新派生一个全新子任务，直到子任务报告 `RALPH_DONE` 或轮数耗尽 |
| `src/tools.rs` | 可被模型调用的 `workflow_run` / `ralph_run` / `schedule` 工具：经任务服务跑 DAG、驱动 Ralph 循环、在共享 `Scheduler` 上管理持久化 cron 条目 |

引擎只具名 `action-tasks` seam 与被动的调度器——绝不具名 state、
operations、transport 或 frontend；tools 模块提供唯一的能力边
（`wavecode-tools`），使引擎可被模型调用。任何一步失败都会让整个
运行失败并指名该 step id，且没有部分重试。
