# crates/transport/mcp/ — MCP JSON-RPC transport over stdio and streamable HTTP

English | [中文](README.zh.md)

| Item | Role |
|---|---|
| `src/lib.rs` | NDJSON JSON-RPC framing (`JsonRpcRequest`/`JsonRpcResponse`/`decode_response`), `TransportError`, `ChildTransport` (spawn, timeout-bounded send/recv, `kill_on_drop`), monotonic request-id sequencing |
| `src/http.rs` | Streamable-HTTP transport: JSON-RPC over POST with SSE responses, `mcp-session-id` persistence, static-header passthrough, OAuth client-credentials bearer auth with token caching, 404 → re-initializable `SessionExpired` |
| `src/test_support.rs` | In-crate HTTP/1.1 stub server serving scripted answers over loopback; compiled only under `cfg(test)` or the `test-support` feature, never in production builds |

The framing-plus-correlation core is duplex-tested over memory pipes;
process management only spawns and kills, and `kill_on_drop` means a
dropped handle never leaks a server behind it. Every write and read is
timeout-bounded, so a wedged server surfaces a timeout instead of
hanging the caller (and with it the whole turn) forever. Per its
module contract the crate must not depend on any workspace protocol
crate — MCP servers speak their own JSON-RPC dialect, and translation
to harness tools happens at the composition root. `decode_response`
accepts only `jsonrpc: "2.0"` frames and lets an `error` object win
over a `result` when both appear, so failures are never read as
success.
