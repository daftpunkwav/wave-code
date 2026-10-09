# operations/bootstrap/ agent rules

The adapter map lives in [README.md](README.md).

## Composition root

- This crate adapts concrete capability crates onto `runtime-runner`
  traits. Each adapter maps one trait onto one concrete crate.
  Policy stays in the capability crate.
- The recorded exceptions that may also name `wavecode-tools` are
  listed in [docs/architecture.md](../../../docs/architecture.md)
  and in [../../AGENTS.md](../../AGENTS.md).
- Runtime, state, action, safety, and capability crates do not
  depend on this crate. Among production crates, only the frontends
  do. `operations-gateway` may use this crate as a dev-dependency.

## Assembly

- Missing config, provider, or credentials fail assembly.
- Memory, skills, hooks, and the skill catalog warn and continue
  when they degrade.
- `assemble_session` loads `ModelCatalog` from the supplied home,
  when present, and merges it into config before provider resolution.
  Catalog load failures warn and continue with the unmerged config.
- Session-scoped tools are registered in `session.rs` after their
  dependencies exist. Late registration stays visible through an
  already-shared `Arc`.
- `PolicyAdapter` reads tool attributes from the registry.
- Approval waits time out to deny. A dropped waiter is deny.
- `workspace_layers.rs` reads the real graph with `cargo metadata`.
  A failing `workspace_layers_hold` means the dependency change is
  wrong. Update `TIER_MEMBERS` or `FRONTEND_ALLOWLIST` only in the
  same change that adds the crate or the frontend edge.

## Nested AGENTS.md

- `agents_instructions.rs` walks from a touched file's directory up
  to, and not including, the project root.
- It offers each directory's `AGENTS.md` at most once per session.
  Compaction clears that seen set.
- A file past `MAX_NESTED_INSTRUCTION_CHARS` (16_000) is offered
  truncated, with the truncation marker appended.
- Discovery observes the tool call. It does not gate execution.
