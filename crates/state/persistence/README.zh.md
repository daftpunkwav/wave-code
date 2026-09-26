# crates/state/persistence/ — turn journal、history log、grants 与 sessions

[English](README.md) | 中文

| 条目 | 职责 |
|---|---|
| `Cargo.toml` | crate 清单;依赖 `thiserror`、`serde`、`serde_json`、`wavecode-llm`(legacy 消息形状) |
| `src/lib.rs` | `JsonlJournal`——append-only 的 turn journal(`TurnRecord`),`{"format":1}` 头行与读取时的 v0 迁移、撕裂尾部修复、`last_n` 预览 |
| `src/history.rs` | `HistoryJournal`——逐条 fsync 的 history 写前日志;`HistoryRead` 区分撕裂尾部与日志中段损坏 |
| `src/grants.rs` | 持久化的 always-allow 授权表(`~/.wavecode/grants.jsonl`);仅接受字面规则、去重、按序号撤销、整体清空 |
| `src/legacy.rs` | 对 `~/.wavecode/threads/` 下 legacy 引擎 rollout journal 的容错读取;把消息渲染为纯文本对 |
| `src/sessions.rs` | session registry:`SessionMeta` 索引(`index.json`)、`record_turn`/`record_rewind`、标题与 fork、子任务 journal、id 白名单 |

持久化只搬运字节,结构留在调用方——历史以纯 `(from_model, text)` 对传递,journal 记录以已编码的 JSON 传递。写入是 append-only 且逐条 fsync,崩溃至多撕裂最后一行;读取方要么修复它(turn journal),要么把它归类(`HistoryRead.torn_tail`)而不是直接失败,日志中段的损坏行会被计数——严格的调用方用 `load_all_checked` 拒绝部分历史。凡成为文件名的 id 都要通过阻止路径穿越的白名单。
