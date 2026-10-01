# operations/ agent rules

The crate map lives in [README.md](README.md). Further rules:
[bootstrap/AGENTS.md](bootstrap/AGENTS.md),
[gateway/AGENTS.md](gateway/AGENTS.md).

## Boundaries

- `operations-actor` drives turns through
  `runtime-runner::TurnDriver`. It does not name concrete tools,
  policy, hooks, or models. Control operations (interrupt, approval,
  shutdown) are routed immediately. A full pending queue rejects
  instead of parking an interrupt behind it.
- The gateway consumes sessions through
  `operations_actor::SessionSurface`.
- `operations-observe`'s only workspace dependency is
  `wavecode-wire`. Its external dependencies are `serde` and
  `serde_json`.
- `operations-simulate`'s production dependency is `runtime-runner`.
  `serde_json` is a dev-dependency.
- `operations-observe` and `operations-simulate` do not affect
  execution.
- `operations-simulate` is unwired. Do not cite it as a product
  feature. Wiring it updates `docs/architecture.md`.
- `operations-eval` owns scoring. Replay checks ordering, pairing,
  and settle-once before scoring. Malformed JSONL fails with the
  line number.
- Eval fixtures and scripted models stay offline.
