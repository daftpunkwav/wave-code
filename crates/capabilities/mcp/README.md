# crates/capabilities/mcp/ — two-way Model Context Protocol support

English | [中文](README.zh.md)

| Item | Role |
|---|---|
| `src/lib.rs` | The interface boundary: `mcp__{server}__{tool}` naming helpers (`tool_name` / `try_tool_name` / `parse_tool_name`), the protocol data types (`McpToolDef`, `McpToolOutput`, `McpPromptDef`, `McpResourceDef`, `McpResourceContent`, `McpError`), the `McpClient` trait (tools / resources / prompts listing, reads, calls), `McpServerHandler`, and `McpServerConfig` |
| `src/bridge.rs` | The real clients and the registry bridge: `StdioMcpClient` (child process) and `HttpMcpClient` (streamable HTTP; re-initializes once when the server expires the session, then retries the original request), capability-gated `McpToolBridge` / `McpResourceBridge` / `McpPromptBridge`, and `connect_all` with warn-and-continue — an unreachable server skips its tools, never fails startup |

The crate owns traits, data types, naming, and config; byte-level framing
stays in `transport-mcp`, and the serving side (exposing the agent's own
tools over MCP) lives in `operations-gateway`, with executor construction in
the composition root. Bridged tools implement the shared `Tool` trait from
`wavecode-tools` — the one capability-layer edge the bridge takes — and a
server's `read_only_hint` maps onto it; unknown side effects are always
treated as writes and go through approvals. The client speaks protocol
`2024-11-05` with bounded pagination and bounded interleaved-message
skipping; interactive browser/PKCE OAuth is out of scope — static headers or
the client-credentials grant only.
