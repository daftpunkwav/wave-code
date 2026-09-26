# crates/foundation/wire/ — 前端/后端 wire 协议类型

[English](README.md) | 中文

| 条目 | 职责 |
|---|---|
| `src/lib.rs` | `Submission`/`Op`（含 `UserImage`）、`WireDecision`、`ApprovalKind`、`ToolOutcome`、`ToolCallPreview`、`Event`/`EventMsg`，以及 `<system-reminder>` 标记常量与 `wrap_system_reminder` |

这里是前端通信的唯一事实来源：凡是在前端与 harness 之间往返的入站
操作（`Op`）与出站事件（`EventMsg`）都只在此定义。变体刻意做到穷尽
——新增或重命名都是破坏性变更，锁定的 wire-tag 测试让每一次重命名
都显式可见。较新的可选字段配合 `serde(default)` +
`skip_serializing_if`：该字段出现之前记录的事件照常反序列化，未设置
的字段绝不上 wire。本 crate 是纯数据——其模块契约禁止依赖 runtime、
state、action、safety、transport 及任何编排层——依赖只有
`serde`/`serde_json`；消费方包括 `runtime/runner`、frontends 与
operations 各 crate。
