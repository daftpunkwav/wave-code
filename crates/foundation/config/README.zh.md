# crates/foundation/config/ — TOML 配置加载与 provider 解析

[English](README.md) | 中文

| 条目 | 职责 |
|---|---|
| `src/lib.rs` | `Config`（顶层 TOML：model、providers、permission mode、hooks、MCP servers、models 目录、`max_tool_rounds`）、`ModelEntry`、`home_dir()`、`load`/`load_from`、`resolve_provider`/`resolve_named_provider` |
| `src/provider.rs` | `ProviderKind`（anthropic / OpenAI 兼容 / OpenAI Responses，含解析别名）、`ProviderConfig`（`Debug` 脱敏、fallback 链、reasoning effort）、`DEFAULT_CONTEXT_WINDOW`/`DEFAULT_MAX_OUTPUT_TOKENS` |
| `src/hooks.rs` | `HookRule`/`HookRuleSet`（单表与表数组两种形态）及 `ConfigError` |
| `src/mcp.rs` | `McpServerRaw`——原始 `[mcp_servers.<name>]` 条目：stdio（`command`/`args`/`env`）与 http（`url`/`headers`）字段，外加可选的 OAuth client-credentials 字段 |
| `src/model_catalog.rs` | `ModelCatalog`/`ModelSpec`/`ApiKind`/`ReasoningSpec`/`ModalitiesSpec`——`~/.wavecode/models.json` 目录，经 `ModelCatalog::merge_into` 合入 `Config` |
| `src/permissions.rs` | `PermissionsConfig`——原始的 `[permissions]` allow/deny 字符串条目 |

本层只做解析，不持有语义。条目的有效性由上层校验——hook 事件点与
MCP 二选一在组装期检查，权限规则语法则归 sandbox crate 所有（它是
判定结论的唯一权威）。API key 的解析优先取 `env_key` 指向的环境
变量，其次内联 `api_key`，空值或纯空白一律视为未设置；主目录先取
`USERPROFILE` 再取 `HOME`，加载绝不回退到相对路径。`[permissions]`
表只从用户级文件读取，这是刻意设计：agent 自己就能写仓库内的配置
文件，扩权只能来自人工编辑的 home 配置。依赖全部为外部 crate
（`serde`、`serde_json`、`toml`、`thiserror`）；依赖方包括
`capabilities/context`、`capabilities/mcp`、frontends、
`operations/actor`、`operations/bootstrap`、`operations/gateway` 与
`state/checkpoint`。
