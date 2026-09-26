# crates/infrastructure/ — runtime mechanisms with centralized limits

English | [中文](README.zh.md)

| Item | Role |
|---|---|
| `base/` | `infrastructure-base` — OS runtime primitives: channel capacities, timeouts, truncation budgets, the cooperative `InterruptHandle`, shared calendar-date and shell resolution |
| `ratelimit/` | `infrastructure-ratelimit` — `TokenBucket` rate limiting as pure arithmetic over caller-supplied time |

These crates hold mechanisms, not policy: scattered magic numbers are
collected here with their rationale, so every upper layer reads one
constant instead of re-inventing its own. Both depend on no workspace
crate (`base` declares none at all; `ratelimit` only `thiserror`), and
both are consumed from above — `base` by `action/jobs`,
`capabilities/hooks`, `capabilities/tools`, `operations/actor`,
`operations/bootstrap`, `operations/gateway`, `runtime/child`,
`runtime/runner`, and `state/store`; `ratelimit` by
`operations/bootstrap`. Nothing in this directory depends back on any
upper layer.
