# crates/operations/gateway/ — 会话契约之上的 RPC 服务外壳

[English](README.md) | 中文

| 条目 | 职责 |
|---|---|
| `Cargo.toml` | crate 清单；会话只经 `operations-actor` 进入（组合根仅作为测试的 dev-dependency） |
| `src/lib.rs` | crate 根；声明各服务端与仅供测试的 stubs 模块 |
| `src/acp.rs` | ACP 服务端：stdio 上的 JSON-RPC 2.0——`initialize`、`session/new`（每个 id 组装一个 headless 会话）、`session/prompt` 流式 `session/update`、`session/cancel` |
| `src/app_server.rs` | 本地 app 服务端：loopback HTTP 上的 REST + SSE，带 bearer-token 认证与 Host 头防 DNS 重绑定；每会话一个 pump 任务喂广播通道 |
| `src/mcp_serve.rs` | MCP 服务端：stdio 上的 NDJSON JSON-RPC——`initialize`、`tools/list`、`tools/call`（经调用方提供的 `ToolExecutor`）、`ping` |
| `src/test_stubs.rs` | 仅 `cfg(test)` 编译的脚本化模型与最小 provider 配置，供各服务端测试共享 |

每个服务端都是薄薄一层服务外壳：会话经 `operations_actor::SessionSurface`
trait 消费，组装由调用方注入（生产环境传入 `assemble_session`），
因此服务端既不具名具体会话句柄，也不知道会话如何组装。两个 stdio
服务端的传输帧格式都是纯 NDJSON 行；app 服务端只绑定 loopback，
除 `/healthz` 外所有路由都要求 bearer token。
