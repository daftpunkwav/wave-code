# Subsystem: tools

The tool framework lives in `crates/capabilities/tools` (package `wavecode-tools`): the `Tool` trait, the `Registry`, path confinement, and the built-in tool set. Execution orchestration (hooks, policy, approval) is *not* here — the RunLoop drives it; see `docs/subsystems/core-loop.md`.

## The `Tool` trait (`crates/capabilities/tools/src/lib.rs`)

```rust
fn name(&self) -> &str;              // runtime names allowed (MCP tools), not &'static str
fn description(&self) -> &str;       // consumed by the model
fn input_schema(&self) -> Value;     // JSON Schema for sampling requests
fn is_read_only(&self) -> bool;      // read-only tools may run in parallel
fn is_destructive(&self) -> bool;    // default false; destructive tools need the policy path
async fn validate(&self, input) -> Result<()>;  // semantic pre-check, default pass-through
async fn execute(&self, input, ctx: &ToolCtx) -> Result<ToolOutput>;
```

The business-error convention is load-bearing: **business failures return `Ok(ToolOutput { is_error: true, content })` and must never panic**; `Err` is reserved for implementation faults (io errors). `ToolAdapter` (bootstrap) prefixes implementation faults with `tool fault:` so transcripts can tell them apart from business failures, and turns unknown tool names into error results too. `ToolCtx { cwd, deny_env }` carries the working directory and the env-strip list.

## Registry

`Registry` is a `Mutex<HashMap<String, Arc<dyn Tool>>>` with `register(&self)` — interior mutability is deliberate, so tools registered after assembly (skill tool, MCP tools, child tools) reach executors, policy, and model adapters through already-shared `Arc`s without rebuilds. Helpers: `name_subset` (skill `allowed-tools` forks; unknown names silently skipped), `read_only_subset` (explore subagents), `specs()` (sorted by name for stable output).

## Path confinement (`src/path_guard.rs`)

Every filesystem tool resolves user paths through `path_guard::resolve(ctx, path)`:

- Lexical normalization first (drop `.`, pop `..`), then containment checks on **canonicalized** real paths on both sides (Windows `\\?\` prefixes must match).
- Existing paths: resolved canonical path must start with the canonicalized `cwd` — this rejects symlink/junction escapes.
- Missing paths (the `write` case): anchor at the nearest existing ancestor, canonicalize, re-check the prefix.
- Component-level prefix comparison rejects sibling confusion (`../abd` inside `../abc`).
- Documented limit: the TOCTOU window (symlink swapped between check and use) is accepted by the M1 threat model; fd-anchored access is future work.

## Execution pipeline stages

For each declared call, the runner (`crates/runtime/runner/src/lib.rs::execute_calls`) applies, in order:

1. **Duplicate-id admission** — first occurrence wins.
2. **Per-run allowlist** (`RunAllowlist`) — fork-scoped `allowed-tools`; denial is a business error before anything else runs.
3. **`PreToolUse` hook** — may block; blocked calls never reach policy or approval.
4. **Policy** — `PolicyAdapter` reads `is_read_only`/`is_destructive` from the registry (never from names) and asks `wavecode-sandbox` for a verdict (deny rules → allow rules → mode default).
5. **Approval** (on `Ask`) — parked on the gate with timeout-deny.
6. **Execute** — read-only, non-destructive calls run concurrently, capped at 8 in flight (`buffer_unordered`); the rest serially with a per-item interrupt check.
7. **`PostToolUse`** — observe only, never blocks.

Note on `validate()`: the trait carries it as a pre-execution semantic check beyond JSON Schema ("stage 0"), but no orchestration call site invokes it today — it is surface for future wiring. Tools must not rely on it for safety; containment and policy do not depend on it.

## Built-in tools

`Registry::builtin()` registers (name — source):

- `read`, `write`, `edit` — `src/fs/mod.rs` (+ `fs/read.rs`, `fs/write.rs`, `fs/edit.rs`); writes are atomic (temp+rename) with size caps and exact-match uniqueness checks for edits.
- `grep`, `glob` — `src/search/`; sync traversal wrapped in `spawn_blocking`.
- `shell` — `src/shell_tool.rs`; spawns through `sanitize_env` (strips `deny_env` names plus sensitive-shape variables like `*_KEY`, `*_PAT`, `AWS_SECRET_ACCESS_KEY`) and the OS sandbox backend.
- `pty_shell` — `src/pty.rs`.
- `python`, `node` — `src/script.rs` (not read-only).
- `lsp_symbols`, `lsp_definition`, `lsp_hover`, `lsp_references` — `src/lsp.rs`; navigation tools are read-only.
- `web_fetch` — `src/web_fetch.rs`; `web_search` — `src/websearch.rs` (DuckDuckGo backend); both read-only.
- `view`, `present` — `src/fs/image.rs`, `src/fs/present.rs`; `spill` — `src/spill_tool.rs` (reads the context spill store).

`todowrite` (`src/todo_tool.rs`) is registered separately via `with_todo_write` so the tool and the session-level `TodoStore` share one `Arc`. `lsp_diagnostics` (`src/lsp.rs`) is not in `builtin()`; session assembly registers the LSP tools with live providers in `crates/operations/bootstrap/src/session.rs` (late registration on the shared registry). `task`, `task_output`, `task_stop` (`src/task_tools.rs`, `src/agent_task_tool.rs`), `ask_user` (`src/ask_user_tool.rs`) ship in this crate; the `skill` tool lives in `crates/capabilities/skills/src/tool.rs`, `memory_write` in `crates/capabilities/memory/src/tool.rs`, `goal` in `crates/state/goal/src/tool.rs`, `plan` in `crates/state/plan/src/tool.rs`, the `job_*` family in `crates/action/jobs/src/tools.rs`, and `workflow_run` / `ralph_run` / `schedule` in `crates/action/workflow/src/tools.rs` — session assembly registers all of them the same way, from those homes.
