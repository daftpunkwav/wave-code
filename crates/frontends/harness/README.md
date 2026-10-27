# crates/frontends/harness/ — the `wavecode` binary: CLI entry and app-server surface (harness-cli)

English | [中文](README.zh.md)

| Item | Role |
|---|---|
| `Cargo.toml` | Manifest; declares the `wavecode` binary target |
| `src/main.rs` | The clap CLI: argument/subcommand parsing, session assembly, and dispatch across every surface |
| `src/exec.rs` | The headless `exec` surface: the single-turn pipeline, text or `--json` JSONL event rendering, the stdin approval dialect, and the resumable `ExecSession` journal |
| `src/repl.rs` | The interactive REPL: slash-command grammar, the multi-turn input loop, and the legacy-thread `resume` surface |
| `src/tui.rs` | The fullscreen TUI assembly over a live harness session (resume seed, `/model` picker catalog, UI wiring) |
| `src/serve.rs` | Server-like surfaces: MCP stdio, ACP stdio, plugin inventory, and the local HTTP app-server (REST + SSE, run through `operations-gateway`) |
| `src/surfaces/` | Self-contained command surfaces, local files in and report out: `doctor`, `grants`, `metrics`, `update_cmd` |
| `src/logging.rs` | Daily rolling file logging under `~/.wavecode/logs`; file-only, so protocol streams and the TUI stay clean |
| `src/task_eval.rs` | Task-level eval execution: fixture isolation, the agent step through the `exec` surface behind the `AgentStep` seam, a real-process `World`, text/JSON reports; scoring lives in `operations-eval` |
| `src/update.rs` | Release check and checksum-verified self-install against GitHub releases, keeping a `.bak` rollback copy |

The binary is a thin entry: it assembles sessions through
`operations-bootstrap`, drives them through the actor client, and hands
interactive rendering to `console-ui`. In headless `exec`, parked
approvals deny openly unless `--approvals` opts in, and every surface
logs to files only, never to stdout/stderr.
