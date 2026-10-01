# crates/ agent rules

The layer map lives in [docs/architecture.md](../docs/architecture.md).
Each group's README is the crate map. This file is the working rules
for every crate.

## Layout

- Workspace members are `crates/<group>/<crate>` only.
- Package names may differ from directory names. The capability stack
  keeps the `wavecode-*` package names.
- A new crate is added to `TIER_MEMBERS` in
  `operations/bootstrap/tests/workspace_layers.rs` in the same change.
  The tier table lists every workspace member.

## Dependencies

- Normal dependencies point downward. Tier order, bottom-up:
  infrastructure, vocabulary, foundation, capabilities, state, safety,
  action, runtime, operations, frontends.
- Vocabulary-tier crates, including those whose directory sits in
  another group: `action-tasks`, `runtime-child`,
  `runtime-scheduler`, `transport-mcp`, `wavecode-protocol`, and
  `wavecode-wire`.
- `runtime-runner`'s workspace dependencies are `state-store`,
  `wavecode-wire`, and `infrastructure-base`.
- `operations-bootstrap` is the composition root. It is the crate that
  adapts concrete capabilities onto runner trait seams.
- These crates may also name `wavecode-tools`: `state-goal`,
  `state-plan`, `action-jobs`, `action-workflow`, `wavecode-skills`,
  `wavecode-memory`, and `operations-gateway` (MCP serve uses its
  registry types).
  `wavecode-tools` may name `action-tasks` and `wavecode-context`.
- Only frontends take a normal dependency on `operations-bootstrap`.
  Gateway tests may use it as a dev-dependency.
- Frontend internal edges match `FRONTEND_ALLOWLIST` in
  `workspace_layers.rs`. A new edge updates that allowlist and the
  crate's `dependency_matrix_locked` test in the same change.
- A failing `workspace_layers_hold` means the dependency change is
  wrong. Do not weaken the test.

## Policy attributes

- Allow and deny decisions read `is_read_only`, `is_destructive`, and
  `ToolKind`.
- The only tool-name branch in `Sandbox::decide` is
  `is_user_question`, matching `ask_user`. Do not add another.
- Unregistered tool names stay serial and destructive.
- `ToolKind::Other` is the default.

## Code

- Crate and module entry points keep the file header current:
  `@file`, `@description`, responsibilities, and the layers it must
  not depend on.
- Comments and git-tracked docs are English.
- When an English doc has a paired `.zh.md`, update both in the same
  change.
- New behavior includes a test in the same change. A bug fix includes
  a regression test that fails before the fix.
- Tests pass without network access, model API keys, or a specific
  OS feature. Live-provider paths stay behind `LIVE=1` and pass when
  it is unset.
- A tool's business failure is `Ok(ToolOutput { is_error: true, .. })`.
  `Err` is an implementation fault. Tool code does not panic on bad
  input.
- Invalid config and a requested confinement backend that is
  unavailable fail closed.
- Plugins, skill packs, hooks, and agent-definition files that fail
  to load warn with a reason and are skipped.
- Do not land commented-out code, debug prints, or placeholder TODOs.

## Wiring and the loop

- Cite a crate as a product feature only when it is reachable from
  the `wavecode` binary. Unwired crates are listed under "Wiring
  status" and "Reserved, unwired by intent" in
  `docs/architecture.md`. Wiring one updates that table in the same
  change.
- A change to the run loop in `runtime/runner` updates
  `docs/architecture.md`, `docs/architecture.zh.md`,
  `docs/subsystems/core-loop.md`, and
  `docs/subsystems/core-loop.zh.md` in the same change.

## Checks

From the repository root, before a pull request:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
```

Clippy denies warnings. Fix the cause. Do not widen exceptions.
