# WaveCode

Headless-first AI coding agent in Rust: one `wavecode` binary serves single-turn
execution, an interactive REPL, a fullscreen TUI, and legacy session resume over
a shared agent core (multi-turn ReAct loop, tools, skills, memory, MCP).

> 🚧 Under active development; no stable release yet.

## Quick start

```bash
cargo build --bin wavecode

wavecode exec "fix the failing test"   # single turn, exit code follows outcome
wavecode exec --json "summarize"       # JSONL events on stdout
wavecode repl                          # interactive multi-turn session
wavecode resume                        # list previous sessions / resume one
wavecode                               # fullscreen TUI on a TTY, REPL otherwise
```

On first run without configuration, WaveCode prints a creation guide with a
config template. Create `~/.wavecode/config.toml`:

```toml
model = "your-model"
model_provider = "your-provider"

[model_providers.your-provider]
type = "anthropic"
base_url = "https://api.example.com/anthropic"
env_key = "YOUR_API_KEY_ENV"  # key read from this env var (wins over inline api_key)

# Optional: entries for the `/model` picker (alias -> provider + wire model).
[models.fast]
provider = "your-provider"
model = "your-model-fast"
# reasoning_effort = "low"   # OpenAI-compatible providers only
```

## Surfaces

- `exec`: one prompt, one turn. Streams the answer to stdout; tool activity,
  approvals, and usage go to stderr. `--json` swaps to JSONL events on stdout
  with human rendering on stderr. Ctrl-C interrupts the turn (exit 130).
- `repl`: multi-turn session over one conversation. Slash commands: `/compact`
  (compress context now), `/memory` (show memory index), `/mcp` (list
  servers), `/permissions` (approval mode), `/quit` (end session),
  `/help`. `/skill-name args` invokes a user-invocable skill by name.
- TUI: same session behind a fullscreen interface — inline approval prompts,
  `/mcp` status rows, `/btw` side questions (read-only, answers stream into a
  panel without touching the conversation), and dialogs for `/model` (provider
  tabs, search, session-only Alt+S, a thinking-level row on OpenAI-compatible
  providers),
  `/permissions`, and `/help` (scrollable keybinding + command reference).
  Session commands: `/sessions` (alias `/resume`) resumes a recorded session
  in place, `/fork` snapshots a resumable copy, `/title` renames, `/new`
  starts a fresh session, `/init` asks the agent to write AGENTS.md,
  `/status` summarizes the session. Completed turns journal under
  `~/.wavecode/sessions/`; `wavecode --session <id>` and `wavecode --continue`
  resume from the CLI (text-level: tool blocks are not replayed).
- `resume`: `wavecode resume` lists recent legacy sessions newest-first;
  `wavecode resume <thread-id>` imports its history as text and continues
  interactively. Tool calls import as `[tool:name]` / `[error:...]` markers
  (text import is the documented scope; replay never re-executes).
- `doctor`: validates local setup without contacting any provider — config
  parse, provider api key resolution (never printed), `[models]` entries,
  console settings, custom themes, and session records. Exit 1 when any
  check fails.

## Permissions

Three modes: `plan` (read-only exploration; the model is nudged to propose a
plan and may also simply answer), `auto` (asks only for command execution and
destructive tools), `wave` (fully automatic; deny rules still apply). Sources
in precedence order:

1. `--permission-mode <mode>` CLI flag (global, wins over config),
2. `permission_mode` in config,
3. built-in `auto`.

Legacy names still parse (`guarded`/`default`/`acceptEdits` → `auto`,
`bypassPermissions`/`yolo` → `wave`) with a startup warning. Unknown values
warn and fall back to `auto`. Shift+Tab (or `/permissions` in the REPL)
cycles the mode for the running session.

## Skills, memory, MCP

- Skills: Markdown files with frontmatter discovered from home and project
  directories. Inline skills expand into the turn; fork skills run as
  background child tasks honoring their `allowed-tools` surface, with
  completion re-injected as notifications. `task_output` / `task_stop` let the
  model poll and stop them.
- Memory: per-turn transcript distillation appends `[category]` entries to a
  home-scoped store; the next session reads them back as its index.
- MCP: stdio servers configured under `[mcp_servers.<name>]` (`command` plus
  optional `args`/`env`) connect at startup with `initialize` + `tools/list`
  and bridge each tool as `mcp__<server>__<tool>` (honoring
  `annotations.readOnlyHint`). Unreachable servers degrade to warnings, never
  failed startups. HTTP servers report `unavailable (http transport not
  implemented)`; prompts-to-skills conversion is future work.

## Develop

```bash
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo fmt --check
```

Layout: the workspace is a flat crate DAG under `crates/<group>/<crate>`,
dependencies pointing downward only — `infrastructure` (primitives),
`foundation` (config, llm, protocol, auth) and `capabilities` (tools, skills,
memory, mcp, ...) form the capability stack, `state` (conversation,
persistence, trajectory) and `safety` (policy, approvals, sandbox) hold
durable data and policy, `action` (tool registry, tasks, workflows) and
`runtime` (run loop, child turns, scheduler) execute, `operations` (wire
protocol, actor, bootstrap composition root, gateway) assembles, and
`frontends` hosts the `wavecode` binary and TUI. `apps/` holds thin
Web/Desktop/SDK shells. See [docs/architecture.md](docs/architecture.md).

Documentation: [architecture overview](docs/architecture.md),
[development guide](docs/development.md),
[contributing guide](CONTRIBUTING.md), and [AGENTS.md](AGENTS.md) for coding
agents.

Commits follow Conventional Commits (`feat:`/`fix:`/`docs:`/`refactor:`/...,
imperative subject, one change per commit).

## License

MIT, see [LICENSE](LICENSE).
