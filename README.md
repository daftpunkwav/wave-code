# WaveCode

Headless-first AI coding agent in Rust: one `wavecode` binary serves single-turn
execution, an interactive REPL, an inline console UI (full-viewport frames on
the main screen, native scrollback preserved), and legacy session resume over
a shared agent core (multi-turn ReAct loop, tools, skills, memory, MCP).

> 🚧 Under active development; no stable release yet.

## Acknowledgments

WaveCode's console experience was shaped by studying several excellent
terminal coding agents — notably [Kimi Code CLI](https://github.com/MoonshotAI/kimi-code)
(MIT; its pi-tui frontend informed the differential-rendering console)
alongside codex, opencode, and Claude Code. All Rust code here is a
fresh implementation.

## Install

Prebuilt binaries (sha256-verified) ship with each `v*` release:

```bash
# macOS / Linux
curl -fsSL https://raw.githubusercontent.com/daftpunkwav/wave-code/main/scripts/install.sh | sh

# Windows (PowerShell)
irm https://raw.githubusercontent.com/daftpunkwav/wave-code/main/scripts/install.ps1 | iex
```

Or build from source:

```bash
cargo build --bin wavecode

wavecode exec "fix the failing test"   # single turn, exit code follows outcome
wavecode exec --json "summarize"       # JSONL events on stdout
wavecode repl                          # interactive multi-turn session
wavecode resume                        # list previous sessions / resume one
wavecode                               # inline console on a TTY, REPL otherwise
wavecode --plan                        # start in plan mode (-y for auto mode)
wavecode --debug                       # debug-level file logging (~/.wavecode/logs)
wavecode metrics                       # per-model, per-tool quality report
wavecode grants list                   # what "always allow" has been approving since
wavecode eval tasks                    # task-level suite: real turns, judged workspaces
wavecode doctor                        # validate local setup, no provider contact
wavecode update                        # check for a newer published release
wavecode update --install              # download + verify + swap in the newer release
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
# rpm_limit = 30               # optional local throttle (requests/minute)
# fallback_providers = ["backup-provider"]  # ordered failover: each provider
# retries transient errors (429/5xx, honoring Retry-After) before the next
# one takes over; auth and quota errors fail fast

# Wire dialect per provider, set with `type`:
#   "anthropic"         -> Anthropic Messages   (POST {base_url}/v1/messages)
#   "openai-compatible" -> OpenAI Chat Completions (POST {base_url}/chat/completions)
#   "openai-responses"  -> OpenAI Responses     (POST {base_url}/responses)
# Chat Completions is what most third-party gateways speak; Responses is the
# endpoint that serves models with no chat route (o1-pro, gpt-5-codex) and the
# recommended one for the newer reasoning families. All three stream, carry
# tools and images, and pair tool calls with their results across rounds.

# Optional: entries for the `/model` picker (alias -> provider + wire model).
[models.fast]
provider = "your-provider"
model = "your-model-fast"
# reasoning_effort = "low"   # OpenAI-compatible providers only

# The same picker also reads `~/.wavecode/models.json`, a standalone model
# catalog holding full provider specs (endpoint, API kind, context/output
# limits, thinking variants, modalities). Edit it in the console with
# `/model list` / `add` / `set` / `remove`, or by hand; its models merge
# into `[models]` at startup, and a config.toml provider with the same id
# wins over a catalog one.

# Optional: cheap model for routine side sessions. `/btw` answers sample
# through this [models] entry instead of the primary model; an explicit
# /model choice in the session still wins, and a dangling alias degrades
# to the primary with a warning (doctor reports it).
# secondary_model = "fast"

# Optional: tool-round ceiling per turn. Unset uses the runner default
# (256 — wavecode targets super-long-horizon work); an open session goal
# re-arms the ceiling up to 7 more times (8 ceilings per turn). `0`
# stops every turn before its first tool round.
# max_tool_rounds = 256

# Optional: permission rules, `Scope(pattern)` syntax. Allow entries skip
# approval; deny entries refuse in every mode (deny always wins). `*` matches
# any run of characters, `?` one. Read from this home file only — never from a
# repo-local config, because the working directory is the agent's to write in.
# [permissions]
# allow = ["Bash(cargo test *)", "File(docs/**)"]
# deny  = ["Bash(curl *)"]
```

## Surfaces

- `exec`: one prompt, one turn. Streams the answer to stdout; tool activity,
  approvals, and usage go to stderr. `--json` swaps to JSONL events on stdout
  with human rendering on stderr — a leading `{"meta":"session",…}` line
  carries the session id and its `wavecode --session` resume command, and
  the finished turn is journaled so the session can be resumed. Ctrl-C
  interrupts the turn (exit 130).
- `exec --image <path>`: attaches an image (PNG/JPEG/WebP/GIF, ≤5 MB) to
  the prompt for vision-capable models; repeatable. Providers without
  vision reject it with a visible error. Each request carries only the two
  newest images; older ones travel as `[image … omitted]` text placeholders
  so a screenshot-heavy session keeps its budget (the stored session keeps
  every image, and the placeholder names what was left out).
- `exec --approvals`: opts in to answering parked approvals from stdin.
  Off by default, so unattended runs keep the fail-closed deny. The JSON
  dialect answers a specific request with one stdin line:
  `<call_id> allow|always|deny[:reason]` (the call id comes from the
  `approval_requested` event). The text dialect prompts on stderr and
  takes `y` / `a` / `n`. stdin closing (EOF) denies everything still
  parked immediately — no run can hang waiting for an answer.
- `repl`: multi-turn session over one conversation. Slash commands: `/compact`
  (compress context now), `/memory` (show memory index), `/mcp` (list
  servers), `/permissions` (approval mode), `/quit` (end session),
  `/help`. `/skill-name args` invokes a user-invocable skill by name.
- Console: same session behind a full-viewport inline interface (native
  scrollback preserved) — approval prompts render inline
  (file-write approvals show the affected lines as a colored diff), `/mcp`
  status rows, `/btw` side questions (read-only, answers stream into a panel
  without touching the conversation), and dialogs for `/model` (provider tabs,
  search, session-only Alt+S, a thinking-level row on OpenAI-compatible
  providers, plus `/model list|add|set|remove` editing the
  `~/.wavecode/models.json` catalog), `/permissions`, `/theme` (built-ins
  plus custom themes), and
  `/help` (scrollable keybinding + command reference).
  Session commands: `/sessions` (alias `/resume`) resumes a recorded session
  in place, `/fork` snapshots a resumable copy, `/title` renames, `/new`
  starts a fresh session, `/init` asks the agent to write AGENTS.md,
  `/status` summarizes the session, `/undo [n]` (or double-Esc) rewinds the
  conversation by whole turns, `/compact [instruction]` compresses context
  with optional steering, `/export [path]` and `/copy` take the dialogue out,
  `/usage` shows token split, `/editor <cmd>` sets the Ctrl+G external
  editor. Completed turns journal under `~/.wavecode/sessions/`;
  `wavecode --session <id>` and `wavecode --continue` resume from the CLI.
  Resume is block-level — tool calls, tool results and images come back
  intact — because each session keeps a write-ahead history journal beside
  its turn snapshot; a tool whose result was lost to a crash is reported as
  unresolved rather than re-run. Sessions written before that journal existed
  resume from their text snapshot. When compaction replaces earlier turns,
  the summary ends with a `## Context Recovery` note naming that journal
  (and the live task list), so the agent can look up exact
  earlier output instead of guessing; each `task` subagent logs its own
  turns under `sessions/children/<parent>/`.
- `resume`: `wavecode resume` lists recent legacy sessions newest-first;
  `wavecode resume <thread-id>` imports its history as text and continues
  interactively. Tool calls import as `[tool:name]` / `[error:...]` markers
  (text import is the documented scope; replay never re-executes).
- `serve`: local HTTP app server (REST + SSE) over live sessions. Binds
  `127.0.0.1` only, requires a per-run bearer token (printed on startup,
  `--token` overrides). `POST /sessions` assembles a parking-enabled
  session; `GET /sessions/{id}/events` streams wire events as SSE;
  `POST /sessions/{id}/prompt` submits a turn; parked approvals and
  questions are answered via `POST .../approvals/{call_id}` and
  `.../questions/{call_id}`; `POST /shutdown` stops the server.
- `metrics`: aggregates the local metrics ledger (`~/.wavecode/metrics/`)
  into a per-model, per-tool table — executed calls with their success rate,
  refusals and denials kept in separate columns, prompt-cache read share,
  turns, approvals. Offline: it reads local files and never contacts a
  provider. `--session <id>` narrows to one session; `--json` emits the
  merged totals for scripts.
- `grants`: lists the "always allow" decisions previous sessions persisted
  (`~/.wavecode/grants.jsonl`) with the index `wavecode grants remove <i>`
  takes; `grants clear` revokes all of them. Every action here only tightens
  authority — a revoked grant goes back to asking. Exit 1 when a revoke could
  not be carried out or the index is not in the table.
- `doctor`: validates local setup without contacting any provider — config
  parse, provider api key resolution (never printed), `[models]` entries,
  permission rules (invalid entries, and allows a deny rule shadows), the OS
  confinement backend, console settings, custom themes, and session records.
  Exit 1 when any check fails.
- `eval tasks`: runs the task-level suite under `benchmarks/tasks/` (run it
  from the repo root). Each task copies its fixture into a throwaway work
  root, drives one real `exec` turn there, then judges the workspace with
  assertions — a check command run by argv, or a file that must match,
  contain, stop containing, exist, or stay byte-for-byte as it was. A task
  passes only if the turn ended cleanly *and* every assertion holds, so a
  crashed step cannot pass on files something else left behind. Costs real
  tokens: this is a nightly measurement, not a per-PR gate. `--filter`,
  `--tag`, `--json`, `--out <path>` and `--agent-bin <path>` (compare two
  builds) narrow a run; `--permission-mode wave` is what lets an unattended
  suite act at all. Exit 1 unless every selected task passes.

Diagnostics: every surface logs to a daily rolling file under
`~/.wavecode/logs/` (14 days retained), never to stdout/stderr, so the
`exec --json` stream stays clean. Levels: `--debug` wins over the
`WAVECODE_LOG` env var, which wins over the `warn` default (`WAVECODE_LOG`
takes full env-filter syntax — `wavecode=debug` for this crate's debug
logs; a bare word filters by target and would silence everything). A panic in a
live UI restores terminal modes before the report prints.

- `update`: compares the running version against the newest GitHub
  release (10s probe). Prints the release page when newer, "up to date"
  otherwise, "no published release yet" before the first tag. A failed
  probe exits 1 so scripts never mistake it for "no update". The TUI
  probes once at startup and shows `update available: <tag>` in the
  footer when a newer release exists.

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

Rules sit under the modes: `deny` entries refuse in every mode, `allow`
entries skip the prompt (with `[permissions]` in the home config, see the
template above). Two bounds are worth knowing when you write one: a wildcard
allow never exempts a compound Bash command — `git status && curl …` still
asks, because `*` spans command separators — and one entry with a bad syntax
costs only itself, reported as a startup warning rather than dropping the
table it sat in.

Answering **always allow** stores that exact command or path in
`~/.wavecode/grants.jsonl`, so the next session starts exempt to the same
call instead of asking you again. Stored grants are literals on purpose: an
entry carrying `*` or `?` would re-parse as a wildcard and approve more than
the human did, so those stay in-memory for the session that approved them,
with a warning in that session's log. Revoke with `wavecode grants list` /
`grants remove <i>` / `grants clear`.

Approval is intent, not containment. OS confinement of shell spawns is
opt-in via `WAVECODE_SANDBOX_OS=1`; when enabled, the first available backend
wins (bwrap → Landlock → seatbelt → Windows job object) and a platform with
none fails closed instead of running unconfined. Note what that means per
platform: Linux and macOS backends bound the filesystem (cwd + temp writable,
no network for shell spawns), while the Windows job object bounds only process
lifetime and count — no filesystem or network boundary, so an approved command
there runs with your own account's privileges. `wavecode doctor` prints which
backend is live and how far it reaches.

## Themes

The default dark look is the **synthwave** identity (neon cyan primary, amber
user input, deep purple-dark ground); the previous teal **deepwave** identity
stays selectable. One selection colors both the interface chrome and code
blocks: synthwave pairs with the bundled SynthWave '84 syntax theme, deepwave
with `base16-ocean.dark`, light with `base16-ocean.light`. Color depth is
probed once at startup — truecolor by default, nearest-256 or the 16 classic
ANSI colors on plainer terminals.

`/theme light|dark|deepwave|auto` switches live (`auto` re-probes the
terminal background); `/theme <name>` loads a custom theme from
`~/.wavecode/themes/<name>.json`:

```json
{
  "base": "dark",
  "syntax_theme": "ocean-dark",
  "colors": { "primary": "#ff0000" }
}
```

`base` is `dark`, `light` or `deepwave`; `colors` overrides any subset of the
20 semantic tokens with `#rrggbb` values; `syntax_theme` is an optional alias
(`synthwave-84`, `ocean-dark`, `ocean-light`). Unknown token names, malformed
colors and unknown aliases are rejected at load — a typo never silently
renders as the base.

## Long-running work

- **Goals keep the turn going.** An objective recorded through the `goal` tool
  (`/goal` shows its status; per home, CAS-versioned) steers the loop twice
  over: when the model stops making tool calls while the goal is still
  `Active`, the turn continues — up to 8 times — and when the turn hits its
  tool-round ceiling with the goal still open, the ceiling re-arms — 8
  ceilings per turn (2048 rounds at the default), matching the cap the goal
  driver sets on itself. Every
  one of those nudges states what is left of the context, round, re-arm and
  continuation budgets, so the model can wrap up instead of opening work it
  cannot finish. Marking the goal `completed`, `blocked` or `paused` stops all
  steering at once; those are statements that work should stop, and the loop
  never writes goal state itself.
- **History is durable per mutation.** Every conversation change appends a
  synced record to `~/.wavecode/sessions/<id>.history.jsonl`, so a crash costs
  at most the step that was in flight, a resumed session keeps its tool calls
  and results rather than only prose, and a tool whose result never landed is
  reported as unresolved instead of silently re-run.

## Skills, memory, MCP

- Skills: Markdown files with frontmatter discovered from home and project
  directories. Inline skills expand into the turn; fork skills run as
  background child tasks honoring their `allowed-tools` surface, with
  completion re-injected as notifications. `task_output` / `task_stop` let the
  model poll and stop them.
- Memory: per-turn transcript distillation appends `[category]` entries to a
  home-scoped store; the next session reads them back as its index.
- MCP: stdio servers configured under `[mcp_servers.<name>]` (`command` plus
  optional `args`/`env`) and streamable-HTTP servers (`url`, optional OAuth
  client credentials) connect at startup with `initialize` + `tools/list`
  and bridge each tool as `mcp__<server>__<tool>` (honoring
  `annotations.readOnlyHint`; capability-gated `read_resource` /
  `get_prompt` discovery tools bridge too). Unreachable servers degrade to
  warnings, never failed startups. Prompts-to-skills conversion is future
  work.

## SDK

[`@wavecode/sdk`](sdk/typescript) drives the agent from JavaScript:
`execSession({ prompt })` spawns `wavecode exec --json`, exposes the typed
JSONL event stream as an async iterator, and — with `approvals: true` — lets
your code answer approval requests (`session.answerApproval(callId, "allow")`).
Zero runtime dependencies; the wire types in `src/types.ts` mirror the Rust
`EventMsg` one-to-one. Tests run against a real binary (see the SDK README).

## Develop

```bash
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo fmt --check
```

Layout: the workspace is a flat crate DAG under `crates/<group>/<crate>`,
dependencies pointing downward only — `infrastructure` (leaf primitives),
`foundation` (wire types, config, llm, protocol vocabulary, auth) and
`capabilities` (tools, skills, memory, mcp, sandbox, hooks, context) form
the capability stack, `state` (store, persistence, checkpoint, goal, plan)
and `safety` (gate, guardrail, audit, secrets) hold durable data and
policy, `action` (tasks, workflow, jobs, browser, retrieval) and `runtime`
(run loop, child turns, scheduler, prompt assembly, plugin seam) execute,
`operations` (actor, bootstrap composition root, gateway, eval) assembles,
`transport` carries the MCP byte framing, and `frontends` hosts the
`wavecode` binary and the console UI. The TypeScript SDK lives in
`sdk/typescript`. See [docs/architecture.md](docs/architecture.md).
Not every crate in that DAG is reachable from the shipped binary: the
architecture doc's [Wiring status](docs/architecture.md#wiring-status) table
lists the unwired ones, so a crate existing on disk is not a feature claim.

Documentation: [architecture overview](docs/architecture.md),
[development guide](docs/development.md),
[contributing guide](CONTRIBUTING.md), and [AGENTS.md](AGENTS.md) for coding
agents.

Commits follow Conventional Commits (`feat:`/`fix:`/`docs:`/`refactor:`/...,
imperative subject, one change per commit).

## License

MIT, see [LICENSE](LICENSE).
