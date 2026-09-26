# crates/operations/actor/ — 单个客户端句柄背后的会话串行驱动器

[English](README.md) | 中文

| 条目 | 职责 |
|---|---|
| `Cargo.toml` | crate 清单；依赖 `runtime-runner`、`runtime-child`、`state-store`、`state-checkpoint`、`safety-gate`、`wavecode-wire`、`wavecode-config`、`infrastructure-base` |
| `src/lib.rs` | crate 根；再导出 `SessionActor`、`ActorClient`、会话契约、持久化与 `StatusQueries` |
| `src/actor.rs` | `SessionActor`：串行 turn 驱动器——输入/compact 进入 pending 队列，控制操作立即路由，另有 rewind、生命周期钩子与可选的持久化 checkpoint |
| `src/client.rs` | `ActorClient`：进程内句柄——`submit`、事件流、只读 `EventTap`、inbox steering（`steer`/`inject`/`cancel_inbox`）、`SubmitError` |
| `src/contract.rs` | 与 gateway 共享的词汇：`AssembleOptions`、`SessionError`、`DEFAULT_IDENTITY`，以及 RPC 服务端消费的 `SessionSurface` trait |
| `src/durable.rs` | persist-then-act checkpoint：`DurabilityConfig`、`CheckpointSink`、`persist_checkpoint`、`resume_checkpoint`、`turn_label` |
| `src/status.rs` | `StatusQueries` trait：供前端 slash 命令按需读取的 plan/goal/snapshot 只读视图 |

actor 将用户 turn 串行化，同时立即路由控制操作（interrupt、审批、
shutdown）；队列满时显式拒绝，而不是让 interrupt 排在队伍后面被卡住。
它从不具名任何具体能力——一切都通过泛型 `TurnDriver` seam 加上
spawn 时注入的 gate 和 interrupt 句柄运行。gateway 只通过
`SessionSurface` 消费会话，因此这份契约才是本 crate 真正的公开接口。
