# frontends/console/ agent rules

The UI map lives in [README.md](README.md) and
[docs/subsystems/ui.md](../../../docs/subsystems/ui.md).

## Boundaries

- Internal dependencies are exactly `infrastructure-base`,
  `tui-engine`, `wavecode-wire`, `wavecode-config`,
  `wavecode-protocol`, `operations-actor`, and `state-persistence`.
  `dependency_matrix_locked` in `src/lib.rs` checks every dependency
  table, including dev, build, and target sections.
- Do not depend on runtime, action, safety, transport, capability
  crates, or `operations-bootstrap`.
- Session events enter as wire events. Rendering goes through
  `tui-engine`.
- Glyph identity follows the table in `docs/subsystems/ui.md`. A new
  activity category updates that table and
  `docs/subsystems/ui.zh.md`.
