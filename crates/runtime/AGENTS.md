# runtime/ agent rules

The crate map lives in [README.md](README.md). The run loop has
additional rules in [runner/AGENTS.md](runner/AGENTS.md).

## Boundaries

- No capability crate is named here. Concrete tools, sandbox, hooks,
  memory, skills, MCP, and models enter behind `runtime-runner` trait
  seams, wired in `operations-bootstrap`.
- `runtime-child` and `runtime-scheduler` are vocabulary-tier. Their
  only workspace dependency is `infrastructure-base`. They stay free
  of capability and session types.
- `runtime-prompt` and `runtime-plugin` depend on no workspace crate.
- `runtime-prompt` lays out named slots. Callers own the text that
  fills a slot.
- A plugin manifest that fails to load warns with a reason and is
  skipped. Assembly does not fail on it.
