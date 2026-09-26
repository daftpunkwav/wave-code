# crates/capabilities/mcp/ — 双向 Model Context Protocol 支持

[English](README.md) | 中文

| 条目 | 职责 |
|---|---|
| `src/lib.rs` | 接口边界：`mcp__{server}__{tool}` 命名辅助函数（`tool_name` / `try_tool_name` / `parse_tool_name`）、协议数据类型（`McpToolDef`、`McpToolOutput`、`McpPromptDef`、`McpResourceDef`、`McpResourceContent`、`McpError`）、`McpClient` trait（tools / resources / prompts 的列举、读取、调用）、`McpServerHandler` 与 `McpServerConfig` |
| `src/bridge.rs` | 真实客户端与注册表桥接：`StdioMcpClient`（子进程）与 `HttpMcpClient`（streamable HTTP；服务端使会话过期返回 404 时重初始化一次，然后重试原请求）、按能力开关的 `McpToolBridge` / `McpResourceBridge` / `McpPromptBridge`，以及 warn-and-continue 的 `connect_all`——连不上的服务器跳过其工具，绝不拖垮启动 |

本 crate 拥有 trait、数据类型、命名约定与配置类型；字节级分帧留在
`transport-mcp`，服务端一侧（把本 agent 自己的工具经 MCP 暴露出去）位于
`operations-gateway`，executor 的构造在组合根。被桥接的工具实现
`wavecode-tools` 的共享 `Tool` trait——这是桥接层取用的唯一一条能力层依赖边——
服务端给出的 `read_only_hint` 映射到该 trait；未知的副作用一律按写对待、
走审批。客户端说 `2024-11-05` 协议，分页与乱序消息跳过均有上限；交互式
browser/PKCE OAuth 不在范围内——只支持静态请求头或 client-credentials 授权。
