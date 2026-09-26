# crates/runtime/child/ — 带结构化深度隔离的受管后台子任务

[English](README.md) | 中文

| 条目 | 职责 |
|---|---|
| `Cargo.toml` | crate 清单；唯一的 workspace 依赖是 `infrastructure-base`（中断原语） |
| `src/lib.rs` | `ChildRuntime`：后台任务的 spawn/query/stop——`ChildSpec`/`ChildTicket`、深度上限（`MAX_CHILD_DEPTH`）、lineage 链、`TaskResult` 结果，以及有界的完成通知队列 |

深度隔离是结构性的而非约定性的：工作工厂只拿到 `ChildTicket`
（身份加停止信号），拿不到 `ChildRuntime` 引用，因此子任务在内部
再派生孙任务是"无法构造出来"的——更深的代际只经由父侧 follow-up
spawn（depth 加一、parent 置位）产生，超过上限的 spawn 以显式
`Failed` 结果拒绝。panic 在任务边界被捕获并记为失败；完成通知流经
一个只携带数据的 `CompletionSink`，它不具备任何 spawn 能力。
