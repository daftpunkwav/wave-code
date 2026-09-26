# crates/runtime/ — execution orchestration: run loop, child tasks, plugins, prompts, schedules

English | [中文](README.zh.md)

| Item | Role |
|---|---|
| `child/` | `runtime-child` — tracked background child tasks with structural depth isolation |
| `plugin/` | `runtime-plugin` — manifest-ordered plugin registry with type-erased service injection |
| `prompt/` | `runtime-prompt` — pure system-prompt layout from named slots |
| `runner/` | `runtime-runner` — the run loop state machine plus the anticorruption trait seams every capability wires behind |
| `scheduler/` | `runtime-scheduler` — priority/delay queues, cron matching, and durable cron entries |

This tier owns orchestration and passive mechanisms, and its
`Cargo.toml` edges show it: `runner` depends only on `state-store`,
`wavecode-wire`, and `infrastructure-base`, while `child`, `prompt`,
`plugin`, and `scheduler` depend on no workspace crate at all (or only
`infrastructure-base`). No capability crate — tools, sandbox, hooks,
models — appears here: concrete capabilities enter exclusively behind
`runner`'s trait seams, wired by the composition root in
`operations-bootstrap`.
