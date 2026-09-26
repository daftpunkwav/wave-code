# crates/state/goal/ — 持久的每会话 goal 服务

[English](README.md) | 中文

| 条目 | 职责 |
|---|---|
| `Cargo.toml` | crate 清单;依赖 `async-trait`、`serde`、`serde_json`、`thiserror`、`tokio`、`wavecode-tools` |
| `src/lib.rs` | `GoalState` 状态机(`set`/`update`/`pause`/`resume`/`block`/`complete`/`round_tick`,由 CAS `version` 守护)、`GoalError`、`<home>/.wavecode/goals/` 下的原子 JSON 持久化 |
| `src/tool.rs` | `GoalTool`——可被模型调用的 `goal` 工具(`set`/`update`/`status`/`tick`)、共享句柄 `GoalStore`、`render_status` |

crate 根是纯状态与持久化;`tool.rs` 是唯一的能力边缘,通过共享的 `Tool` trait 把这台状态机暴露给模型。变更受乐观并发守护:`update` 必须提供 `expected_version`,版本不匹配时同时报出两个版本号,模型可以重新读取后重试;轮次驱动以 `MAX_ROUND`(256)为上限,触顶后 goal 进入 blocked。运行循环只通过快照读取状态,从不写入——工具是唯一写入者——没有 home 目录时状态仅存于内存。
