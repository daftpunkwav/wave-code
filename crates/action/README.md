# crates/action/ — model-invokable capability seams: tasks, jobs, workflow, browser, retrieval

English | [中文](README.zh.md)

| Item | Role |
|---|---|
| `browser/` | `action-browser` — browser automation behind an async tab seam, with a scripted fake |
| `jobs/` | `action-jobs` — background shell jobs (spawn/wait/cancel/output) plus the `job_*` tools |
| `retrieval/` | `action-retrieval` — lexical term-overlap retrieval over chunked documents |
| `tasks/` | `action-tasks` — the capability-neutral task lifecycle seam and its test fakes |
| `workflow/` | `action-workflow` — validated DAG execution, Ralph loops, durable schedules, and the `workflow_run`/`ralph_run`/`schedule` tools |

The layer contract is visible in the manifests: `browser`, `retrieval`,
and `tasks` have no workspace dependencies at all — they define seams
and vocabulary only. Execution layers implement them (the composition
root maps `tasks` onto the child runtime), and where a crate becomes
model-invokable it adds exactly one capability edge on `wavecode-tools`
(`jobs/tools.rs`, `workflow/tools.rs`). Downward edges stay within
passive mechanisms: `workflow` may name the vocabulary-tier
`runtime-scheduler` and `jobs` the `runtime-child` completion channel,
but no action crate ever names state, operations, or a frontend.
