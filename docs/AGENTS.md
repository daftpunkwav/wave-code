# docs/ agent rules

The index lives in [architecture.md](architecture.md) and
[development.md](development.md).

## Language and tense

- English is the canonical text.
- When an English doc has a paired `.zh.md`, update both in the same
  change. The English file links the Chinese file, and the Chinese
  file links back.
- Prose describes current behavior.

## What stays aligned

- Dependency rules and the wiring table in `architecture.md` match
  `crates/operations/bootstrap/tests/workspace_layers.rs` and the
  crate graph.
- A subsystem or cookbook page names the owning crate paths.
- `docs-local/` is never committed.
