# crates/operations/simulate/ — read-only dry-run rendering of planned model actions

English | [中文](README.zh.md)

| Item | Role |
|---|---|
| `Cargo.toml` | Crate manifest; the only dependency is `runtime-runner` |
| `src/lib.rs` | `render_plan` / `PlanSummary`: turns sampled `SampleBlock`s into one dry-run line each — text as speech, tool blocks as invocations with raw input |

Simulation here is honest preview: it prints what WOULD run and never
executes anything. Result, image, and thinking blocks never appear in
a plan, and nothing touches executors, policy, or approvals — the
module shares only the block vocabulary with the run-loop seam.
