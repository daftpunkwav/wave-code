# Changelog

All notable changes to `wavecode` are documented here. The format
follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/); the
project does not cut releases on a fixed cadence, so entries
accumulate under `Unreleased` until a `v*` tag publishes them.

## [Unreleased]

### Added
- Session system: `--session` resume, `--continue`, `/sessions`,
  `/fork`, `/title`, `/new`, text-level turn journal for replay.
- Tool surface: background tasks, goal tracking, todo panels,
  `ask_user`, file snapshots with rewind, memory, skills, plan mode.
- Security: deny-first path sandbox with OS-level confinement on all
  three platforms, permission modes (`plan` / `auto` / `wave`),
  persisted "always allow" grants with a `grants` audit command,
  sensitive-file prompts, SSRF-guarded web fetch.
- Headless: `exec --json` streaming wire events with a stdin
  approval channel, ACP mode negotiation, app server (REST + SSE),
  TypeScript SDK.
- Console UI: wave-themed rendering, syntax highlighting, window
  title and tab progress, kill-ring/yank-pop, paste-burst protection,
  external status-line command (Claude Code contract), custom JSON
  themes, `/undo` rewind picker, `/editor` (Ctrl+G), `/reload`,
  `/btw` side questions, and a model catalog (`~/.wavecode/models.json`)
  edited through `/model list|add|set|remove` beside the picker.
- Markdown rendering: fenced mermaid blocks draw box-drawing diagrams
  (`graph`/`flowchart`, `stateDiagram`, `sequenceDiagram`,
  `classDiagram`, `gantt`, `pie`; Ctrl+M returns the source view),
  ```diff fences color additions/removals/hunks, `$…$` math converts
  to unicode, `==highlight==` folds onto bold, `^sup^`/`~sub~` map to
  superscript/subscript glyphs, and `<details>`/`<summary>`/`<kbd>`
  reduce to plain terminal rows.
- Theming: `/theme light|dark|deepwave|auto` plus custom theme files
  with per-token color overrides and a `syntax_theme` alias; one
  selection colors both the chrome and code blocks (the bundled
  SynthWave '84 tmTheme, or base16-ocean dark/light), with
  256-color / 16-color degradation for plainer terminals.
- Long-horizon loop: `max_tool_rounds` config key; persisted tool
  results carry a wall-clock stamp so resumed sessions can reason
  about recency; a `/model` switch onto a smaller-window model
  compacts before the next sample instead of failing hard.
- Reliability: write-ahead history journal, checkpoint manifests,
  bounded LLM/MCP transports (no hang-forever paths), panic
  isolation for tool children, MCP reconnect/heal.
- Engineering: `doctor` diagnostics, `update` release check,
  `eval` task harness, offline metrics reporting, binary release
  pipeline with sha256-verified archives.

### Changed
- The default dark theme is the synthwave identity (neon cyan
  primary, amber user input on a subtle highlight band, a desaturated
  neutral gray for line chrome, deep purple-dark ground); the previous
  deepwave identity stays selectable via `/theme deepwave`.
- Fenced code frames hug their content instead of spanning the
  terminal (still capped at 80 columns on wide terminals).
- `max_tool_rounds` default raised from 32 to 256, and goal
  continuations per turn from 5 to 8, for super-long-horizon turns.
- Warnings and loop notices render in the theme's warning color
  instead of dim body text, and the round-limit warning names the
  `max_tool_rounds` config key.
- Edit tool cards and edit settings render as a single unified diff
  column (the `diff layout` setting is gone).

### Fixed
- Web fetch refuses redirect hops onto link-local hosts at every hop,
  so a public server cannot 302 the tool into cloud metadata or
  local-link addresses.
- MCP response bodies are read with a 32 MiB cap, and captured shell
  and script output stops at a per-stream cap, so a chatty server or
  child process cannot grow memory without bound.
- Markdown rendering: exactly one blank line between blocks, fenced
  code framed with dim rules instead of literal backtick markers
  (language tags sanitized), inline code spans without visible
  backticks, CJK-adjacent emphasis parses, and standalone `**bold**`
  pseudo-heading lines start their own block.
- Tables align borders on display width: ragged rows, CJK cells, and
  wide glyphs can no longer pull the grid out of alignment.
- The terminal cursor stays in the input editor only, and the input
  region stays pinned to the screen bottom while output streams.
- Journal tear recovery: torn writes and multi-byte UTF-8 cuts no
  longer read as an empty history.
- Windows: snapshot manifests cannot escape the restore root through
  backslash or drive-prefix path semantics; pty sessions no longer
  leak shell processes on setup failure.
- Correctness: permission-mode typos are rejected instead of
  silently widening to `auto`; oversized `--image` inputs are
  rejected before loading into memory.

### Removed
- The `ls` tool (redundant with `shell` / `glob` / `read`).
- The `pty_shell` tool and the `portable-pty` dependency.
