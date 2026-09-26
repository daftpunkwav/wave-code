# crates/state/persistence/ — turn journal, history log, grants, sessions

English | [中文](README.zh.md)

| Item | Role |
|---|---|
| `Cargo.toml` | crate manifest; `thiserror`, `serde`, `serde_json`, `wavecode-llm` (legacy message shapes) |
| `src/lib.rs` | `JsonlJournal` — append-only turn journal (`TurnRecord`), `{"format":1}` header with read-time v0 migration, torn-tail repair, `last_n` previews |
| `src/history.rs` | `HistoryJournal` — per-record fsynced write-ahead log of history mutations; `HistoryRead` separates a torn tail from mid-log damage |
| `src/grants.rs` | persisted always-allow grant table (`~/.wavecode/grants.jsonl`); literal rules only, dedup, revoke by index, clear |
| `src/legacy.rs` | tolerant reader for legacy engine rollout journals under `~/.wavecode/threads/`; renders messages as plain text pairs |
| `src/sessions.rs` | session registry: `SessionMeta` index (`index.json`), `record_turn`/`record_rewind`, title/fork, child-task journals, id whitelist |

Persistence moves bytes; structure lives with the caller — history
travels as plain `(from_model, text)` pairs and journal records as
already-encoded JSON. Writes are append-only and fsynced per record, so
a crash can only tear the last line; readers repair it (turn journal)
or classify it (`HistoryRead.torn_tail`) instead of failing, and
corrupt mid-file lines are counted — `load_all_checked` refuses them
for strict callers. Every id that becomes a file name passes a
whitelist that blocks path traversal.
