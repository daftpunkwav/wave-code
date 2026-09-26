# crates/state/ — per-session state and its persistence

English | [中文](README.zh.md)

| Item | Role |
|---|---|
| `artifact/` | `state-artifact` — versioned registry of run-produced artifacts |
| `checkpoint/` | `state-checkpoint` — labeled checkpoints with rollback plus file-content snapshots |
| `goal/` | `state-goal` — durable per-session goal machine and the model-invokable `goal` tool |
| `persistence/` | `state-persistence` — turn journal, history write-ahead log, grant table, session registry, legacy import |
| `plan/` | `state-plan` — reviewed plan-mode state machine and the model-invokable `plan` tool |
| `store/` | `state-store` — in-memory conversation history with frozen snapshots and context budget levels |

Except for `store` — which holds history in memory and delegates
durability to its `HistorySink` — every crate persists under
`<home>/.wavecode/` (`goals/`, `plans/`, `snapshots/`, `sessions/`,
`grants.jsonl`), saves atomically by staging a temp file and renaming,
and reads a missing file as fresh or empty state. Dependency edges stay
narrow: `artifact` needs nothing, `checkpoint` uses `wavecode-config`
only for the home directory, `persistence` uses `wavecode-llm` only for
legacy message shapes, `store` uses `infrastructure-base` for timestamp
formatting, and only `goal` and `plan` reach outside the layer — for the
shared `Tool` trait in `wavecode-tools`. None of them depend on runtime,
action, safety, operations, transport, or any orchestration crate.
