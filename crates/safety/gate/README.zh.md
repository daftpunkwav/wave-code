# crates/safety/gate/ — 审批与提问闸门

[English](README.md) | 中文

| 条目 | 职责 |
|---|---|
| `Cargo.toml` | crate 清单;依赖 `thiserror`、`tokio`(oneshot channel) |
| `src/lib.rs` | `ApprovalGate`(`wait_for`/`decide`/`cancel`/`clear`)与 `QuestionGate`(自由文本回答)、`ApprovalDecision`(AllowOnce/AllowAlways/Deny)、`GateError`、已弃用的 `ApprovalKind` 别名 |

每个 call id 只允许一个停靠的等待者,且投递是一次性的:第一个决定取走槽位,迟到的决定返回 `false`,因此过期的 UI 点击绝不可能批准复用该 id 的后续调用。`cancel` 释放超时的等待,让对应的 call id 可以重新停靠;被撤销的等待者的 receiver 以 dropped 收场,绝不会被当作批准。互斥锁中毒时通过取回内部 guard 恢复,因为这些短临界区不会留下写了一半的不变量。本 crate 只做策略裁决——操作系统级隔离不在范围内,规范的权限类型位于 protocol crate,因此这里的 `ApprovalKind` 别名已弃用。
