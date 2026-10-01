# capabilities/ agent rules

The crate map lives in [README.md](README.md). Further rules:
[tools/AGENTS.md](tools/AGENTS.md),
[sandbox/AGENTS.md](sandbox/AGENTS.md),
[context/AGENTS.md](context/AGENTS.md),
[memory/AGENTS.md](memory/AGENTS.md).

## Boundaries

- These crates do not import drivers, actors, or sessions. Hooks,
  approval, and schema orchestration run in the run loop and in
  bootstrap.
- Internal edges run one way. `tools` depends on `sandbox` and
  `context`. `mcp`, `memory`, and `skills` depend on `tools` for the
  `Tool` trait.
- `sandbox` takes a normal dependency on `wavecode-protocol` only
  among workspace crates. `wavecode-tools` there is a dev-dependency
  for the classification lock test.
- `mcp` may depend on `transport-mcp` and `wavecode-config`.
  `skills` may depend on `action-tasks`. `hooks` may depend on
  `infrastructure-base`.
- `memory` and `skills` host their model tools in this layer.
  Bootstrap still registers every tool.
- MCP tool names use `mcp__{server}__{tool}`.
