# crates/frontends/harness/ — the `wavecode` binary: CLI entry and app-server surface (harness-cli)

English | [中文](README.zh.md)

| Item | Role |
|---|---|
| `Cargo.toml` | Manifest; declares the `wavecode` binary target |
| `src/main.rs` | The clap CLI: argument/subcommand parsing, session assembly, and every surface — headless `exec` (text or `--json` JSONL), `repl`, the inline console on a TTY (REPL otherwise), `resume`, `mcp serve`, `plugin list`, `acp`, `doctor`, `metrics`, `update`, `grants`, `eval tasks`, and `serve` (the local HTTP app-server over REST + SSE, run through `operations-gateway`) |
| `src/logging.rs` | Daily rolling file logging under `~/.wavecode/logs`; file-only, so protocol streams and the TUI stay clean |
| `src/task_eval.rs` | Task-level eval execution: fixture isolation, the agent step through the `exec` surface behind the `AgentStep` seam, a real-process `World`, text/JSON reports; scoring lives in `operations-eval` |
| `src/update.rs` | Release check and checksum-verified self-install against GitHub releases, keeping a `.bak` rollback copy |

The binary is a thin entry: it assembles sessions through
`operations-bootstrap`, drives them through the actor client, and hands
interactive rendering to `console-ui`. In headless `exec`, parked
approvals deny openly unless `--approvals` opts in, and every surface
logs to files only, never to stdout/stderr.
