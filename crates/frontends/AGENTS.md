# frontends/ agent rules

The crate map lives in [README.md](README.md). Further rules:
[engine/AGENTS.md](engine/AGENTS.md),
[console/AGENTS.md](console/AGENTS.md),
[harness/AGENTS.md](harness/AGENTS.md).

## Boundaries

- Internal edges match `FRONTEND_ALLOWLIST` in
  `operations/bootstrap/tests/workspace_layers.rs` and each crate's
  `dependency_matrix_locked` test.
- `tui-engine` has zero workspace dependencies.
- `console-ui` reaches the session through `wavecode-wire` and
  `operations-actor`, and renders through `tui-engine`.
- `harness-cli` is the crate that assembles the `wavecode` binary
  through `operations-bootstrap`.
- A new internal dependency updates the allowlist and the matrix
  test in the same change.
