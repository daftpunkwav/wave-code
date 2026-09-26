# crates/operations/observe/ — wire 事件之上的指标折叠与 append-only turn 台账

[English](README.md) | 中文

| 条目 | 职责 |
|---|---|
| `Cargo.toml` | crate 清单；唯一的 workspace 依赖是 `wavecode-wire` |
| `src/lib.rs` | `Metrics`：把每种 wire 事件变体恰好折叠一次的累计计数器，含按工具拆分与 token/缓存核算；可在不停止折叠的情况下取快照 |
| `src/ledger.rs` | `Ledger` / `TurnSample`：home metrics 目录下的 append-once JSONL 持久化，读取时容忍并统计损坏行 |

记录是只读折叠：仪表板、成本守卫与评估都读 `Metrics`，且没有任何
一方能扰动执行。ledger 只负责样本在内存与磁盘之间的搬运——产出
`TurnSample` 的折叠逻辑在事件 tap 中，模型名由 tap 逐 turn 盖章，
因为 wire 并非在每个事件上都携带模型标识。
