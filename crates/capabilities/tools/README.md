# crates/capabilities/tools/ — tool framework and builtin tool set

English | [中文](README.zh.md)

| Item | Role |
|---|---|
| `src/lib.rs` | The framework: `Tool` trait (read-only / destructive attributes, business-failure semantics), `Registry` (builtin set, `name_subset` / `read_only_subset`, late registration), `ToolCtx` (cwd + `deny_env`), `ToolOutput`, `ToolAllowlist` (the skills `allowed-tools` surface), `is_sensitive_env_name`, the `TOOL_FAULT_PREFIX` implementation-fault marker |
| `src/fs/` | File tools confined under `ToolCtx::cwd`: `read` (`read.rs`, 2000-line / 50 KB budgets, negative offset for tail reads, did-you-mean suggestions from sibling files on a miss), `write` (`write.rs`, atomic, 10 MB cap), `edit` (`edit.rs`, exact-match uniqueness check), `view` (`image.rs`, magic-byte-sniffed image attachments, 5 MB), `present` (`present.rs`, deliverables recorded into a shared `PresentStore`); shared caps and helpers in `mod.rs`, which also hosts the `FileLedger` — `read` records each file's (mtime, len) fingerprint and `write` / `edit` refuse to mutate a file whose on-disk fingerprint drifted from the session's last view, so an outside change (formatter, git checkout, another process) forces a re-read instead of being silently overwritten |
| `src/search/` | Read-only search: `grep` (`grep.rs`, regex content search, 500-match cap) and `glob` (`glob.rs`, path patterns, 1000-path cap); blocking traversal runs in `spawn_blocking` and hit paths are re-checked after canonicalize against symlink escapes (`mod.rs`) |
| `src/path_guard.rs` | The path escape guard: every check runs on the canonicalized real path (the symlink-swap TOCTOU window is documented), returning a lexically normalized path for display |
| `src/shell_tool.rs` | `shell`: cross-platform command execution through the shared `shell_invocation` resolution, sensitive-env scrubbing, 30 KB per-stream output cap (truncated streams of a completed run spill their full text to the `SpillStore` and name the `spill://` URI), 60 s default timeout (300 s ceiling) that kills the process and reports the output produced before the kill, optional OS confinement behind `WAVECODE_SANDBOX_OS` |
| `src/script.rs` | `python` / `node`: inline scripts run non-interactively with a discovered interpreter, cwd confinement, a clamped timeout, and shell-mirroring failure semantics |
| `src/lsp.rs` | A minimal stdio LSP client plus `lsp_symbols` / `lsp_definition` / `lsp_hover` / `lsp_references` (per-call `server_command`, or a lazily spawned registry-backed server reused across calls) and `lsp_diagnostics` (renders recorded `publishDiagnostics` pushes); no server is bundled |
| `src/web_fetch.rs` | `web_fetch`: http/https only, manual redirects (max 5 hops), streamed size cap (256 KB default / 1 MB hard) with a `[truncated]` marker, HTML rendered to Markdown (`raw=true` opts out) |
| `src/websearch.rs` | `web_search`: pluggable `SearchBackend`; the default `DuckDuckGoBackend` scrapes the keyless HTML endpoint, and parse failures surface as business errors |
| `src/html.rs` | Lenient single-pass HTML -> Markdown converter for `web_fetch`: structural markup becomes Markdown, scripts/styles are dropped, malformed input degrades to plain text and never fails |
| `src/spill_tool.rs` | `spill`: read-only readback of `spill://` URIs through the context crate's `SpillStore` (spills live outside `cwd`, so `read` cannot reach them) |
| `src/todo_tool.rs` | `todowrite`: session task list with full-rewrite semantics; the `TodoStore` handle lives in the session config and is injected at assembly |
| `src/agent_task_tool.rs` | `task`: free-form subagent delegation through the `action-tasks` seam, with named agent definitions from `.wavecode/agents/` resolving to tool-surface restrictions and an identity preamble |
| `src/task_tools.rs` | `task_output` / `task_stop` / `task_continue`: observe, stop, and continue child tasks; unknown ids stay business errors |
| `src/ask_user_tool.rs` | `ask_user`: the interactive question surface; valid calls are routed by the sandbox into the question flow, so the body only validates and reports openly when the gate was bypassed |

Two contracts hold across every tool: business failures return
`Ok(ToolOutput { is_error: true, .. })` with a reason the model can
self-correct from, while `Err` is reserved for implementation faults; and
all execution is truly async (`tokio::fs` / `tokio::process`, blocking
traversal wrapped in `spawn_blocking`). The crate is the seam hub of this
layer — `wavecode-mcp`, `wavecode-memory`, and `wavecode-skills` implement
its `Tool` trait — while it reaches down to `wavecode-sandbox` (policy
vocabulary), `wavecode-context` (spill store root), `infrastructure-base`
(shell resolution), and `action-tasks` (delegation). Schema validation,
hooks, and permission approval are orchestrated core-side, not here.
