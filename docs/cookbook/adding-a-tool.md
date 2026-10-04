# Cookbook: adding a tool

English | [中文](adding-a-tool.zh.md)

Worked examples: `web_fetch` (`crates/capabilities/tools/src/web_fetch.rs`, registered in `Registry::builtin()`) and `lsp_diagnostics` (`crates/capabilities/tools/src/lsp/tools.rs`, re-exported from `src/lsp.rs`, late-registered in session assembly).

## 1. Implement the `Tool` trait

Add a module under `crates/capabilities/tools/src/` and implement `Tool` (`crates/capabilities/tools/src/lib.rs`):

```rust
#[async_trait::async_trait]
impl Tool for MyTool {
    fn name(&self) -> &str { "my_tool" }            // globally unique; &str, not &'static str
    fn description(&self) -> &str { "...English, model-facing..." }
    fn input_schema(&self) -> serde_json::Value { /* JSON Schema */ }
    fn is_read_only(&self) -> bool { true }          // drives parallelism + plan mode
    // fn is_destructive(&self) -> bool { false }    // default false; only set when real
    // fn validate(&self, input) -> Result<()>       // semantic pre-check; surface exists,
    //                                               // not yet invoked by orchestration

    async fn execute(&self, input: Value, ctx: &ToolCtx) -> Result<ToolOutput> {
        // Business failure:  Ok(ToolOutput { content: reason, is_error: true })
        // never panic, never Err. Err is for implementation faults only.
    }
}
```

Attribute truth matters: `is_read_only` puts the tool in the concurrent batch and the explore subset; `is_destructive = true` (or an unregistered name) forces the serial, approval-prone path. Do not mark a writing tool read-only to make it faster.

## 2. Register it

Pick one of the two registration points:

- **Static builtin**: append to `Registry::builtin()` in `crates/capabilities/tools/src/lib.rs`. Use this when the tool needs no session state.
- **Late registration**: register in `crates/operations/bootstrap/src/session.rs` after the pieces it needs exist — the LSP tools show the shape (`register_lsp_tools` registers the five tools around one shared `LspProviders` handle), as do the `skill` / `task` tools which need the child service built around the driver. The registry is interior-mutable, so late registration reaches every already-shared handle (executor, policy, model adapter) without a rebuild.

Tools needing a session-shared store follow the `todo_write` pattern: `with_todo_write`-style constructor sharing one `Arc` between the tool and the session config.

## 3. Policy and allowlist implications

- Policy never matches tool names. Verdicts come from your `is_read_only` / `is_destructive` attributes via `PolicyAdapter` (`crates/operations/bootstrap/src/policy_adapter.rs`) plus the sandbox's deny/allow rules and permission mode. Nothing to wire — but expect `shell`-style tools to draw `Ask` verdicts in `default` mode and hard `Deny` in `plan` mode unless read-only.
- `Registry::name_subset` (skill `allowed-tools` forks) and `read_only_subset` (explore subagents) pick your tool up by name automatically; a typo in a skill frontmatter simply leaves it unavailable.
- Filesystem access must go through `path_guard::resolve` (containment under `ToolCtx::cwd`); child processes must run through `sanitize_env` (env scrubbing) and honor the sandbox backend's confinement. See `docs/subsystems/safety.md`.

## 4. Tests to write

Mirror the existing tools (`web_fetch.rs` and `lsp/` are the reference set):

- **Registration and attributes**: extend the pattern of `builtin_registers_script_lsp_and_web_fetch` (`crates/capabilities/tools/src/lib.rs`) — the tool is present, and its `is_read_only` classification matches the read-only subset expectations.
- **Business errors are `Ok(..., is_error: true)`**: missing params, bad input, upstream "not found" — assert `is_error` and that content explains the failure (see the `web_fetch` failure-path tests around its `is_error: true` returns).
- **Faults stay distinguishable**: if `execute` can return `Err`, assert `ToolAdapter` surfaces it with the `tool fault:` prefix (`crates/operations/bootstrap/src/tool_adapter.rs` tests show the shape).
- **Containment**: any path-taking tool gets a `path_guard` escape test (sibling-prefix confusion and symlink-escape tests in `crates/capabilities/tools/src/path_guard.rs` are the templates).
- **Late registration**, if you chose it: assert the tool is reachable through a shared `Arc<Registry>` after registration (`late_registration_reaches_shared_handles` in `lib.rs`).

## 5. Check the pipeline end to end

`cargo test --workspace --locked` plus a manual turn: the tool should appear in `available_tools` (sorted by name via `Registry::specs`), respect the plan-mode read-only rule, and produce `ToolCallBegin`/`ToolCallEnd` pairs the replay fold can pair — see `docs/cookbook/recording-and-replaying.md` to record a session and confirm.
