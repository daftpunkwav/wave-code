# transport/ agent rules

The crate map lives in [README.md](README.md).

## Boundaries

- Transport speaks the external server's dialect. Translation into
  harness tools happens in `capabilities/mcp` and the composition
  root.
- `transport-mcp` is vocabulary-tier. It depends on no workspace
  crate. Its external dependencies are `thiserror`, `tokio`,
  `serde_json`, `reqwest`, and `tracing`.
- Do not depend on `wavecode-protocol`, runtime, or a capability
  crate.
- The in-workspace production consumer is `capabilities/mcp`.
  Test-only code may enable the `test-support` feature.
