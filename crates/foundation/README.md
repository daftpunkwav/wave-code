# crates/foundation/ — dependency-free leaf crates every upper layer is built on

English | [中文](README.zh.md)

| Item | Role |
|---|---|
| `auth/` | `wavecode-auth` — provider-keyed credentials as explicit values, `Debug`-redacted |
| `config/` | `wavecode-config` — TOML config loading, provider/key resolution, model catalog |
| `llm/` | `wavecode-llm` — unified multi-provider streaming chat abstraction over three wire dialects |
| `protocol/` | `wavecode-protocol` — shared vocabulary enums (`PermissionMode`, `ApprovalKind`) |
| `wire/` | `wavecode-wire` — the frontend/backend wire types (`Submission`, `Event`) |

Foundation crates are the leaves of the workspace dependency graph: none
of them declares a path dependency on any workspace crate — their
`[dependencies]` reference only external crates pinned in the root
`Cargo.toml`. Every upper layer (runtime, state, capabilities, action,
operations, frontends) depends on foundation crates; nothing here
depends back. Semantic validation of parsed config (hook event points,
MCP stdio-vs-http either-or, permission rule syntax) is deliberately
left to upper layers so parsing here stays dependency-free.
