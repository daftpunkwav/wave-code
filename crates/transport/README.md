# crates/transport/ — connectors to external servers' own dialects

English | [中文](README.zh.md)

| Item | Role |
|---|---|
| `mcp/` | `transport-mcp` — MCP JSON-RPC exchange over child-process stdio pipes and streamable HTTP |

Transport crates speak the servers' dialect, not the harness
vocabulary: MCP servers exchange their own JSON-RPC frames, and
translation into harness tools happens at the composition root, not
here. Correspondingly, the crates in this directory depend on no
workspace crate — `transport-mcp` uses only `thiserror`, `tokio`,
`serde_json`, and `reqwest` — so a transport can be tested and reused
without dragging in the rest of the tree. The in-workspace consumer
today is `capabilities/mcp`, which also uses the `test-support`
feature in its dev-dependencies.
