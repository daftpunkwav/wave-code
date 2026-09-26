# crates/transport/ — 连接外部服务器自有方言的适配层

[English](README.md) | 中文

| 条目 | 职责 |
|---|---|
| `mcp/` | `transport-mcp`——经由子进程 stdio 管道与 streamable HTTP 的 MCP JSON-RPC 交换 |

Transport crate 说的是服务器的方言，而不是 harness 的词汇：MCP
server 交换的是它自己的 JSON-RPC 帧，翻译成 harness 工具这件事发生在
组装根（composition root），不在这里。相应地，本目录下的 crate 不依赖
任何 workspace 内 crate——`transport-mcp` 只用 `thiserror`、`tokio`、
`serde_json` 与 `reqwest`——因此 transport 可以在不牵连仓库其余部分的
情况下独立测试与复用。当前 workspace 内的消费者是
`capabilities/mcp`，其 dev-dependencies 还使用了 `test-support`
feature。
