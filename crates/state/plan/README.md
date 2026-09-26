# crates/state/plan/ — reviewed plan-mode state machine

English | [中文](README.zh.md)

| Item | Role |
|---|---|
| `Cargo.toml` | crate manifest; `async-trait`, `serde`, `serde_json`, `thiserror`, `tokio`, `wavecode-tools` |
| `src/lib.rs` | `PlanState` machine (`Draft` -> `Proposed` -> `Approved` -> `Executing` -> `Done`/`Abandoned`), `PlanError`, atomic JSON persistence under `<home>/.wavecode/plans/` |
| `src/tool.rs` | `PlanTool` — the model-invokable `plan` tool (`propose`/`approve`/`feedback`/`status`), the shared `PlanStore` handle, `render_status` |

Transitions are ordered and enforced: an out-of-order move is a
business error naming the expected state, `feedback` returns a proposal
to `Draft` while appending the notes, and the terminal `Done` and
`Abandoned` states leave through no transition. The tool mutates plan
state, never the repository. Persistence follows the same gates as the
goal crate: atomic temp-file-then-rename saves, a missing file reads as
a fresh `Draft`, and without a home directory the state stays
memory-only.
