# crates/transport/mcp/ — 经 stdio 与 streamable HTTP 的 MCP JSON-RPC 传输

[English](README.md) | 中文

| 条目 | 职责 |
|---|---|
| `src/lib.rs` | NDJSON JSON-RPC 分帧（`JsonRpcRequest`/`JsonRpcResponse`/`decode_response`）、`TransportError`、`ChildTransport`（spawn、带超时上限的收发、`kill_on_drop`）、单调递增的请求 id 序列 |
| `src/http.rs` | Streamable-HTTP 传输：POST 携带 SSE 响应的 JSON-RPC、`mcp-session-id` 会话保持、静态请求头透传、带令牌缓存的 OAuth client-credentials bearer 认证、404 → 可重新初始化的 `SessionExpired` |
| `src/test_support.rs` | crate 内置的 HTTP/1.1 stub 服务器，在 loopback 上应答脚本化的响应；仅在 `cfg(test)` 或 `test-support` feature 下编译，生产构建永不包含 |

分帧加关联的核心逻辑经内存管道做双工测试；进程管理只负责 spawn 与
kill，而 `kill_on_drop` 保证句柄被丢弃后不会残留泄漏的 server 进程。
所有读写都受超时约束，卡死的服务器会以超时浮出，而不是让调用方（进
而整轮对话）永远挂起。按其模块契约，本 crate 不得依赖任何 workspace
协议 crate——MCP server 说自己的 JSON-RPC 方言，翻译成 harness 工具
发生在组装根（composition root）。`decode_response` 只接受
`jsonrpc: "2.0"` 帧，且当 `result` 与 `error` 同时出现时以 `error`
为准，失败绝不会被误读为成功。
