# crates/capabilities/ — 面向模型的能力 crate 层

[English](README.md) | 中文

| 条目 | 职责 |
|---|---|
| `context/` | 上下文管理流水线：token 记账、三级阈值、压缩、保缓存的工具结果驱逐、system-reminder 注入通道、spill 侧存储 |
| `hooks/` | 生命周期钩子：事件点、经平台 shell 执行的 `command`/`prompt` 钩子、阻断语义 |
| `mcp/` | 双向 Model Context Protocol 支持：client trait 与数据类型、stdio/streamable-HTTP 客户端、`mcp__{server}__{tool}` 命名空间下的注册表桥接 |
| `memory/` | 指令记忆发现、持久记忆存储、抽取输出解析、启发式近重复合并 |
| `sandbox/` | 权限策略（模式、allow/deny 规则、verdict）加可选的 OS 级隔离后端（bwrap / Landlock / seatbelt / Windows Job Objects） |
| `skills/` | SKILL.md 发现与解析、目录（catalog）注入、插件包，以及模型可调用的 `skill` 工具 |
| `tools/` | `Tool` trait、`Registry` 与内建工具集：文件、搜索、shell、脚本、LSP、web、spill、todo、子任务委派、`ask_user` |

层内依赖边单向流动。`tools` 踩在 `sandbox`（策略词汇）和 `context`（spill
存储根）之上；`mcp`、`memory`、`skills` 各自依赖 `tools` 以获得共享的
`Tool` trait。最底层，`sandbox` 只认识 `wavecode-protocol`，`context` 只认识
foundation 层 crate（`wavecode-llm`、`wavecode-config`、`wavecode-wire`）；
层下的共享接缝是 `infrastructure-base`（平台 shell 解析）、`action-tasks`
（子任务委派）与 `transport-mcp`（MCP 字节级分帧）。这些 crate 都不做编排：
校验、钩子与审批流由 core 驱动——多个模块头因此写明了对应的边界约束：
这里不出现 drivers、actors 或 sessions。
