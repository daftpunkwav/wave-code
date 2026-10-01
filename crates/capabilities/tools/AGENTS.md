# capabilities/tools/ agent rules

The tool map lives in [README.md](README.md). The sequence for a new
tool lives in
[docs/cookbook/adding-a-tool.md](../../../docs/cookbook/adding-a-tool.md).

## Tool contract

- Implement `Tool`. `name` is globally unique.
- `is_read_only` and `is_destructive` match the real effect. A
  writing tool is not read-only. `is_destructive` stays false unless
  the tool is destructive.
- A business failure is `Ok(ToolOutput { is_error: true, .. })` with a
  reason the model can correct from. `Err` is an implementation fault
  and is later surfaced with the `tool fault:` prefix. Do not panic.
- Execution is async. Blocking traversal uses `spawn_blocking`.
- Filesystem paths go through `path_guard::resolve` and stay under
  `ToolCtx::cwd`. Checks compare canonicalized paths.
- Child processes use the shared shell resolution and environment
  scrubbing.
- Mutating file tools write a temp file and rename it.
- `write` and `edit` refuse a file whose on-disk fingerprint differs
  from the session's last `read`.
- Schema validation, hooks, and approval are not implemented here.
- Workspace dependencies are `infrastructure-base`, `action-tasks`,
  `wavecode-protocol`, `wavecode-sandbox`, `wavecode-llm`, and
  `wavecode-context`.

## Registration

- A tool with no session state is appended to `Registry::builtin()`.
- A tool that needs session state is registered in
  `operations/bootstrap/src/session.rs` after that state exists.
- Late registration stays visible through an already-shared
  `Arc<Registry>`.
- `Registry::specs()` stays sorted by name.

## Tests

- Register the tool and lock its read-only classification.
- Cover the business-error path.
- A path-taking tool includes a `path_guard` escape test.
