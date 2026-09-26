# crates/state/store/ — conversation history with context budgets

English | [中文](README.zh.md)

| Item | Role |
|---|---|
| `Cargo.toml` | crate manifest; `infrastructure-base` (timestamp formatting), `serde`, `serde_json` |
| `src/lib.rs` | `Conversation` (append-only history, frozen `Arc` snapshots, `Usage` carry) behind the `HistorySink` durability seam; `Block`/`HistoryEntry` shapes; `check_budget` levels; `estimate_tokens`; `normalize_history`/`find_pairing_violations`/`close_open_calls` |

All mutations flow through one write entry (`push`/`push_blocks`/
`replace`), and the sink is notified on every committed change, so no
push can bypass a durability journal by forgetting a second write.
Budget levels are fixed thresholds on remaining tokens — warn at
20,000, auto-compact at 13,000, block sampling at 3,000 — and
`estimate_tokens` is only a fallback: authoritative provider usage
always wins. `Block` keeps tool use/result pairing and reasoning blocks
in history so providers see structured multi-round tool traffic, while
`Thinking` stays invisible to the prose view used by estimates and
compaction.
