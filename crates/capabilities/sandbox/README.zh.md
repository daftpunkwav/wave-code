# crates/capabilities/sandbox/ — 权限策略与执行安全层

[English](README.md) | 中文

| 条目 | 职责 |
|---|---|
| `src/lib.rs` | 策略核心：`Rule` 解析与匹配（`Bash(...)` / `File(...)` 两种作用域，配置用通配规则、会话规则用精确匹配）、`wildcard_match`、`Verdict` 枚举（Allow / Ask / Deny）与 `Sandbox::decide`（deny 优先；敏感凭证路径一律 Ask）、审批详情的长度上限 |
| `src/bash.rs` | 基于 tree-sitter-bash AST 的命令切分，用于权限匹配——每条解析出的命令产出全文与去掉环境变量前缀的"裸"变体两段；解析失败时保守回退到字符串切分 |
| `src/os.rs` | 后端接缝：`ConfinementProfile`（可写/只读根、网络开关）、基于 `tokio::process::Command` 的 `SandboxBackend` trait、`EnforcementLevel`、Linux `LinuxLandlockBackend`、`UnavailableBackend`、`detect_backend` |
| `src/bwrap.rs` | Linux bubblewrap 后端：探测 `bwrap --version` 加一次 user-namespace 往返（缺失则 fail-closed），把 spawn 改写为 `bwrap` 隔离前缀；报告 Full 级强制 |
| `src/seatbelt.rs` | macOS `sandbox-exec` 后端：按 cwd 生成 deny-by-default 的 profile；Partial 级强制——网络访问不做按进程限制 |
| `src/windows.rs` | 经 Win32 Job Objects 的部分 Windows 后端：进程树生命周期控制（job 关闭即杀）加单次 spawn 的进程数上限；没有文件系统或网络边界，超出该范围的隔离请求 fail-closed |
| `src/chain.rs` | 探测链：`PROBE_ORDER`（bwrap -> landlock -> seatbelt -> job）、fail-closed 的 `first_available` 选择、携带可 grep 的 `SANDBOX_UNAVAILABLE` 标记的 `status_line` |

机制与策略分离：`Sandbox::decide` 只裁决心图（core 传入
`read_only`/`destructive`，因此本 crate 绝不反向依赖 `tools`），后端则对单次
spawn 强制执行其 `ConfinementProfile`——经 `WAVECODE_SANDBOX_OS` 按 spawn
选择性开启，开启而无可用后端时拒绝执行。文档化的威胁模型对限制写得很明白：
不提供容器、VM 或 syscall 过滤；无隔离时被 spawn 的进程以用户自身的 OS
权限运行。`PermissionMode`/`ApprovalKind` 来自 `wavecode-protocol`；生产中
`tools` 依赖本 crate，另有一条 dev 依赖测试锁定工具名分类词汇。
