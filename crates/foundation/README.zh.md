# crates/foundation/ — 所有上层依赖的零内部依赖叶子 crate

[English](README.md) | 中文

| 条目 | 职责 |
|---|---|
| `auth/` | `wavecode-auth`——按 provider 索引的凭据，以显式值传递，`Debug` 输出脱敏 |
| `config/` | `wavecode-config`——TOML 配置加载、provider/密钥解析、模型目录 |
| `llm/` | `wavecode-llm`——跨三种 wire 方言的统一多 provider 流式对话抽象 |
| `protocol/` | `wavecode-protocol`——共享词汇枚举（`PermissionMode`、`ApprovalKind`） |
| `wire/` | `wavecode-wire`——前端/后端 wire 类型（`Submission`、`Event`） |

Foundation crate 是 workspace 依赖图的叶子：它们均不声明任何指向
workspace 内 crate 的 path 依赖——`[dependencies]` 只引用根
`Cargo.toml` 钉住的外部 crate。所有上层（runtime、state、
capabilities、action、operations、frontends）都依赖 foundation
crate；这里没有任何反向依赖。配置解析后的语义校验（hook 事件点、
MCP 的 stdio/http 二选一、权限规则语法）刻意留给上层完成，使本层的
解析保持零内部依赖。
