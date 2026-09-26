# crates/state/plan/ — 受评审的 plan 模式状态机

[English](README.md) | 中文

| 条目 | 职责 |
|---|---|
| `Cargo.toml` | crate 清单;依赖 `async-trait`、`serde`、`serde_json`、`thiserror`、`tokio`、`wavecode-tools` |
| `src/lib.rs` | `PlanState` 状态机(`Draft` -> `Proposed` -> `Approved` -> `Executing` -> `Done`/`Abandoned`)、`PlanError`、`<home>/.wavecode/plans/` 下的原子 JSON 持久化 |
| `src/tool.rs` | `PlanTool`——可被模型调用的 `plan` 工具(`propose`/`approve`/`feedback`/`status`)、共享句柄 `PlanStore`、`render_status` |

状态迁移有序且被强制:乱序迁移会作为业务错误报出并指明期望状态,`feedback` 把提案退回 `Draft` 并把反馈追加到计划文本,终态 `Done` 与 `Abandoned` 无法通过任何迁移离开。工具只改 plan 状态,从不改仓库。持久化与 goal crate 采用同样的闸门:先写临时文件再 rename 的原子保存,文件缺失按全新 `Draft` 读取,没有 home 目录时状态仅存于内存。
