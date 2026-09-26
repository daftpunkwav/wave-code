# crates/operations/ — session lifecycle: actor, composition root, RPC gateways, and observability

English | [中文](README.zh.md)

| Item | Role |
|---|---|
| `actor/` | `operations-actor` — the serial session driver and its in-process client handle, plus the `SessionSurface` contract |
| `bootstrap/` | `operations-bootstrap` — the composition root wiring concrete capabilities behind the `runtime-runner` trait seams |
| `eval/` | `operations-eval` — scripted benchmarks, recorded-session replay, and task-level scoring |
| `gateway/` | `operations-gateway` — RPC serving skins: ACP over stdio, the loopback REST + SSE app server, and the MCP tool server |
| `observe/` | `operations-observe` — the read-only metrics fold over wire events and the append-only turn ledger |
| `simulate/` | `operations-simulate` — dry-run rendering of planned model actions |

The dependency edges in this tier point one way: `gateway` consumes
sessions only through `actor`'s `SessionSurface` trait, `bootstrap`
assembles sessions and is the only crate here allowed to name concrete
capability crates (it is the top of the workspace DAG, depended on by
nothing in this tier), and `actor` drives turns through the generic
`runtime-runner::TurnDriver` seam. `observe` and `simulate` are pure
observers — the former depends on nothing but the wire protocol, the
latter on nothing but the runner seam types — so neither can affect
execution.
