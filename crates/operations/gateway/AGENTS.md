# operations/gateway/ agent rules

The server map lives in [README.md](README.md).

## Serving skins

- Workspace production dependencies are `operations-actor`,
  `wavecode-wire`, `runtime-runner`, `wavecode-tools`,
  `safety-gate`, and `infrastructure-base`. External production
  dependencies are `async-stream`, `axum`, `futures`, `tokio`, and
  `serde_json`.
- Sessions are consumed through `SessionSurface`. The caller injects
  assembly. This crate does not construct the session executor.
- `operations-bootstrap`, `wavecode-mcp`, `wavecode-config`, and
  `wavecode-llm` are dev-dependencies.
- MCP serve may name `wavecode-tools` registry types. Tool execution
  goes through the caller-supplied `ToolExecutor`.
- Stdio servers speak NDJSON JSON-RPC.
- The app server binds loopback only. Every route except `/healthz`
  requires the bearer token. The Host-header rebinding guard stays.
- `test_stubs.rs` is `cfg(test)` only and does not depend on a server
  in this crate.
