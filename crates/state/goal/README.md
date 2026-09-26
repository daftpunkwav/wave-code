# crates/state/goal/ — durable per-session goal service

English | [中文](README.zh.md)

| Item | Role |
|---|---|
| `Cargo.toml` | crate manifest; `async-trait`, `serde`, `serde_json`, `thiserror`, `tokio`, `wavecode-tools` |
| `src/lib.rs` | `GoalState` machine (`set`/`update`/`pause`/`resume`/`block`/`complete`/`round_tick` under a CAS `version`), `GoalError`, atomic JSON persistence under `<home>/.wavecode/goals/` |
| `src/tool.rs` | `GoalTool` — the model-invokable `goal` tool (`set`/`update`/`status`/`tick`), the shared `GoalStore` handle, `render_status` |

The crate root is pure state and persistence; `tool.rs` is the single
capability edge that renders the machine model-invokable through the
shared `Tool` trait. Mutations are guarded by optimistic concurrency:
`update` requires `expected_version`, and a mismatch names both
versions so the model can reload and retry; the round driver caps at
`MAX_ROUND` (256) and blocks the goal there. The run loop reads the
state through snapshots but never writes it — the tool is the single
writer — and without a home directory the state stays memory-only.
