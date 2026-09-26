# crates/foundation/protocol/ — 共享的 permission mode 与 approval kind 词汇

[English](README.md) | 中文

| 条目 | 职责 |
|---|---|
| `src/lib.rs` | `PermissionMode`（`plan` / `auto` / `wave` wire 字符串、兼容旧名的 `parse`、`as_str`）与 `ApprovalKind`（`exec` / `write`） |

这个 crate 存放每层都必须叫同一个名字的小枚举——config 的
`permission_mode` 字符串、sandbox 的判定逻辑、前端展示都从这里读取
`PermissionMode`，模式名因此不会各自漂移。`PermissionMode::parse`
让旧配置名继续可加载（`guarded`/`default`/`acceptEdits` 映射到
`Auto`，`bypassPermissions`/`yolo` 映射到 `Wave`）。它刻意不包含任何
请求或事件类型：`Submission`/`Event` 这条活跃接口只有一个家，即
`wavecode-wire`，不允许在这里出现第二份拷贝。wire 标签拼写由测试
锁定，依赖只有 `serde`/`serde_json`；当前 workspace 内的消费者是
`capabilities/sandbox` 与 `operations/bootstrap`。
