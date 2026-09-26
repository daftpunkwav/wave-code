# crates/capabilities/ — model-facing capability crates

English | [中文](README.zh.md)

| Item | Role |
|---|---|
| `context/` | Context-management pipeline: token accounting, three-level thresholds, compaction, cache-preserving tool-result eviction, system-reminder channel, spill side-store |
| `hooks/` | Lifecycle hooks: event points, `command`/`prompt` hook execution over the platform shell, blocking semantics |
| `mcp/` | Two-way Model Context Protocol support: client traits and data types, stdio/streamable-HTTP clients, registry bridge under `mcp__{server}__{tool}` |
| `memory/` | Instruction memory discovery, persistent memory store, extraction-output parsing, heuristic near-duplicate consolidation |
| `sandbox/` | Permission policy (modes, allow/deny rules, verdicts) plus optional OS confinement backends (bwrap / Landlock / seatbelt / Windows Job Objects) |
| `skills/` | SKILL.md discovery and parsing, catalog injection, plugin packs, and the model-invokable `skill` tool |
| `tools/` | The `Tool` trait, `Registry`, and the builtin tool set: file, search, shell, script, LSP, web, spill, todo, task delegation, `ask_user` |

Internal dependency edges run one way. `tools` sits on `sandbox` (policy
vocabulary) and `context` (spill store root); `mcp`, `memory`, and `skills`
each depend on `tools` for the shared `Tool` trait. At the bottom, `sandbox`
knows only `wavecode-protocol` and `context` only the foundation crates
(`wavecode-llm`, `wavecode-config`, `wavecode-wire`); the shared seams below
the layer are `infrastructure-base` (platform shell resolution),
`action-tasks` (child-task delegation), and `transport-mcp` (byte-level MCP
framing). None of these crates orchestrate: validation, hooks, and the
approval flow are driven core-side, which is why several module headers
state the matching boundary — no drivers, actors, or sessions here.
