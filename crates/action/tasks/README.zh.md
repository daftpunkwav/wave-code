# crates/action/tasks/ — 能力中立的任务生命周期 seam 与测试假实现

[English](README.md) | 中文

| 条目 | 职责 |
|---|---|
| `Cargo.toml` | crate 清单；没有任何依赖 |
| `src/lib.rs` | 任务词汇（`TaskRequest`、`TaskKind`、`TaskState`、`TaskOutcome`、带 lineage 的 `TaskInfo`）与 `TaskService` seam（spawn/query/stop，外加可选的 `continue_task` follow-up 钩子）；为测试提供 `FakeTaskService`（spawn 即完成并回显）与 `ScriptedTaskService`（脚本化结果） |

这条 seam 只承载生命周期——depth 与 parent 字段随请求一起传递以支持
嵌套子任务核算，但这里不存在任何执行知识。分层方向禁止 action crate
具名 runtime 类型，因此由组合根把这套词汇映射到 child runtime；
未覆写 `continue_task` 的实现者无需任何改动即可继续编译。
