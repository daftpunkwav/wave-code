# crates/state/store/ — 带上下文预算的会话历史

[English](README.md) | 中文

| 条目 | 职责 |
|---|---|
| `Cargo.toml` | crate 清单;依赖 `infrastructure-base`(时间戳格式化)、`serde`、`serde_json` |
| `src/lib.rs` | `Conversation`(append-only 历史、冻结的 `Arc` 快照、`Usage` 结转),建立在 `HistorySink` 持久化接缝之上;`Block`/`HistoryEntry` 数据形状;`check_budget` 分级;`estimate_tokens`;`normalize_history`/`find_pairing_violations`/`close_open_calls` |

所有变更都经过唯一的写入入口(`push`/`push_blocks`/`replace`),每条已提交的变更都会通知 sink,因此任何 push 都不可能因为"忘了第二次写入"而绕过持久化 journal。预算分级是剩余 token 的固定阈值——剩 20,000 时警告、剩 13,000 时自动压缩、剩 3,000 时阻塞采样——`estimate_tokens` 只是兜底:provider 上报的权威用量总是优先。`Block` 在历史中保留 tool use/result 配对与 reasoning 块,让 provider 看到结构化的多轮工具调用;而 `Thinking` 对估算与压缩所用的纯文本视图不可见。
