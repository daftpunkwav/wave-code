# crates/operations/observe/ — metrics fold over wire events plus the append-only turn ledger

English | [中文](README.zh.md)

| Item | Role |
|---|---|
| `Cargo.toml` | Crate manifest; the only workspace dependency is `wavecode-wire` |
| `src/lib.rs` | `Metrics`: cumulative counters folded from every wire event variant exactly once, with per-tool splits and token/cache accounting; snapshots without stopping the fold |
| `src/ledger.rs` | `Ledger` / `TurnSample`: append-once JSONL persistence under the home metrics dir, tolerant and counting malformed lines on read |

Recording is a read-only fold: dashboards, cost guards, and
evaluations all read `Metrics`, and none of them can perturb
execution. The ledger only moves samples between memory and disk —
the fold that produces a `TurnSample` lives in the event tap, which
stamps the model per turn because the wire carries no model identity
on every event.
