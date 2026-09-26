# crates/operations/gateway/ — RPC serving skins over the session contract

English | [中文](README.zh.md)

| Item | Role |
|---|---|
| `Cargo.toml` | Crate manifest; sessions enter through `operations-actor` only (the composition root is a dev-dependency for tests) |
| `src/lib.rs` | Crate root; declares the servers and the test-only stubs module |
| `src/acp.rs` | ACP server: JSON-RPC 2.0 over stdio — `initialize`, `session/new` (one headless session per id), `session/prompt` streaming `session/update`, `session/cancel` |
| `src/app_server.rs` | Local app server: REST + SSE over loopback HTTP with bearer-token auth and Host-header rebinding guard; one pump task per session feeds a broadcast channel |
| `src/mcp_serve.rs` | MCP server: NDJSON JSON-RPC over stdio — `initialize`, `tools/list`, `tools/call` through a caller-supplied `ToolExecutor`, `ping` |
| `src/test_stubs.rs` | `cfg(test)`-only scripted model and minimal provider config shared by the server tests |

Every server is a thin serving skin: sessions are consumed through the
`operations_actor::SessionSurface` trait and assembly is injected by
the caller (production passes `assemble_session`), so the servers
never name the concrete session handle or how a session was built.
Transport framing is plain NDJSON lines for the stdio servers; the
app server binds loopback only and requires a bearer token on every
route except `/healthz`.
