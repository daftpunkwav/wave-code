# crates/operations/eval/ — scripted benchmarks, recorded-session replay, and task scoring

English | [中文](README.zh.md)

| Item | Role |
|---|---|
| `Cargo.toml` | Crate manifest; depends only on `runtime-runner`, `state-store`, and `wavecode-wire` |
| `src/lib.rs` | `EvalCase`/`EvalResult`/`EvalReport` and `evaluate`: scripted cases run through the `TurnDriver` seam, expectations checked against final history |
| `src/replay.rs` | Recorded-session replay: validates JSONL event recordings against the wire contract (ordering, pairing, settle-once), then scores assistant text — no model, no tools |
| `src/task.rs` | Task-level benchmarks: `task.toml` manifests, `Assertion`s scored against a caller-supplied `World` (filesystem reads + command exits), suite reports as text or JSON |

Nothing here executes an agent: cases run through the generic
`TurnDriver` seam, replay is a pure read-only fold over recorded
events, and task scoring judges observations a caller hands back
through `World`. A failed turn fails a case even when the text
happens to match, and a task with no assertions judges nothing.
