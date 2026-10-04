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
- Context surface: instructions read `AGENTS.md` with `AGENTS.local.md`
  supplements at the global, project-root, and cwd tiers (legacy
  `WAVECODE.md`/`CLAUDE.md` are no longer read), and nested
  `AGENTS.md` files load on demand as file tools touch deeper
  directories. Every sample carries transient notes rebuilt from live
  state — serving model, live window, today's date, the `Context
  usage` line (marked when estimated), the repeat streak — and the
  model can request compaction via `compact_context`: the loop reviews
  each request (session grant cap, per-turn once, usage floor) and
  denials ride back as transient notes, so a nearly empty window never
  burns a summary. Compaction keeps a verbatim tail past the summary,
  the system prompt assembles under a character budget (drops are
  reported, never silent), and nested instruction files inject
  truncated at a fixed cap and re-offer after a compaction.
- Security: deny-first path sandbox with OS-level confinement on all
  three platforms, permission modes (`plan` / `auto` / `wave`),
  persisted "always allow" grants with a `grants` audit command,
  sensitive-file prompts, SSRF-guarded web fetch.
- Headless: `exec --json` streaming wire events with a stdin
  approval channel, ACP mode negotiation, app server (REST + SSE),
  TypeScript SDK.
- Console UI: wave-themed rendering, syntax highlighting, window
  title and tab progress, kill-ring/yank-pop, typing-burst paste protection,
  external status-line command (JSON snapshot contract), custom JSON
  themes, `/undo` rewind picker, `/editor` (Ctrl+G), `/reload`,
  `/btw` side questions, and a model catalog (`~/.wavecode/models.json`)
  edited through `/provider` beside the picker. In-console surfaces:
  `/provider` (a step-at-a-time guided wizard over the full catalog
  spec, able to stage several models on one provider), `/doctor`
  (one-pass config/credential/catalog/settings health check), `/agents`
  (the background-job table), `/hooks` (the configured hook table), and
  `/release-notes` (the newest changelog sections).
- Markdown rendering: fenced mermaid blocks draw box-drawing diagrams
  (`graph`/`flowchart` TD/TB/LR with subgraphs, `stateDiagram`,
  `sequenceDiagram`, `classDiagram`, `erDiagram`, `requirementDiagram`,
  `C4*`, `gitGraph`, `mindmap`, `timeline`, `gantt`, `pie`, `journey`,
  `quadrantChart`, `xychart-beta`; Ctrl+M returns the source view),
  ```diff fences color additions/removals/hunks, `$…$` math converts
  to unicode, `==highlight==` folds onto bold, `^sup^`/`~sub~` map to
  superscript/subscript glyphs, and `<details>`/`<summary>`/`<kbd>`
  reduce to plain terminal rows.
- Theming, data-driven: every theme is one `theme.json` — the
  built-ins (`dark`, `deepwave`, `light`) are bundled JSON files and
  user themes drop into `~/.wavecode/themes/` to become
  `/theme <name>`, VS Code-style (optional `base` inheritance, `dark`
  kind, `description` for the picker, `syntax_theme` alias; unknown
  keys rejected). `/theme` picks interactively; one selection colors
  both the chrome and code blocks, with 256-color / 16-color
  degradation for plainer terminals. Bare
  `/theme` (and every other parameterized command) opens an
  interactive picker; the light theme applies its paper background to
  the terminal (OSC 11 background + OSC 10 foreground + OSC 12 cursor,
  so unpainted spans and the caret stay visible) and dark themes
  restore the terminal's own colors on switch and exit; messages
  repaint under the new theme when it changes (a message rendered
  under dark no longer strands near-white text on the light paper);
  frame-shrinking transitions (dialog close, rewind) repaint the
  viewport so no stale copy of the input box survives.
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

### Security
- macOS Seatbelt sandbox profiles are written to a process-private,
  owner-only temp directory instead of a shared predictable one, and a
  failed profile write now refuses the confined spawn instead of
  letting `sandbox-exec` read whatever file was there. The previous
  layout let a local attacker who pre-created the directory swap the
  profile between write and read.

### Changed
- All three themes re-tune their neutrals: the dark theme's lavender
  (violet-cast) ramp becomes a true graphite ramp under one azure
  accent, the light theme's slate grays neutralize, and both lock
  WCAG contrast (body ≥ 7:1, dim ≥ 4.5:1, muted ≥ 3.5:1) and a
  no-violet-grays contract in tests; deepwave keeps its teal identity
  with a brighter muted step.
- Parameterized slash commands open their interactive surface when
  invoked bare: `/theme` and `/effort` pick from a selector, `/title`,
  `/editor`, `/export`, `/compact`, and `/btw` open a prefilled
  free-text prompt, and bare `/undo` opens the rewind picker instead
  of dropping a turn immediately (while busy it still refuses).
- The previous deepwave identity stays selectable via `/theme deepwave`.
- Fenced code frames hug their content instead of spanning the
  terminal (still capped at 80 columns on wide terminals).
- `max_tool_rounds` default raised from 32 to 256, and goal
  continuations per turn from 5 to 8, for super-long-horizon turns.
- Warnings and loop notices render in the theme's warning color
  instead of dim body text, and the round-limit warning names the
  `max_tool_rounds` config key.
- Edit tool cards and edit settings render as a single unified diff
  column (the `diff layout` setting is gone).
- Internal module organization: oversized source files split at real
  seams (LSP tools into `lsp/` transport/client/diagnostics/providers/
  tools, dialogs into per-dialog files, editor/actor/session assembly
  stage helpers, inline test modules moved to directory-form test
  files) so every file stays under the analyzer line budgets; no
  public API changes.
- Model catalog editing moved from `/model list|add|set|remove` to
  `/provider`; the old spellings report the move instead of doing
  nothing, bare `/provider` lists saved providers (picking one
  preseeds the wizard), and the `/settings` panel expands to ten
  tunables (thinking expansion, streaming draft, footer meter and
  tips, confirm-exit, history limit, wave denylist).

### Fixed
- Leaving the console TUI runs the SessionEnd hooks and the memory
  pass inside a bounded drain, matching `exec`; previously neither
  fired on TUI exit.
- The wave denylist is enforced on every session surface: it moved to
  `~/.wavecode/wave-denylist.json` (owned by the config layer, with a
  one-time migration from `console-settings.json`), and ACP and HTTP
  serve sessions — which silently ran without the configured rules —
  load the same store as the TUI, exec, and the REPL.
- `/memory` lists the instruction files the session actually assembled
  (AGENTS.md tiers, local supplements, and rules) instead of
  re-walking directories, so the view cannot drift from the injected
  context.
- `/title <name>` before the first completed turn reports the missing
  journal instead of printing a usage line, and settings-panel changes
  to tool/edit display refresh already-rendered tool cards; `/quit`
  completes like `/exit`.
- `/permissions <name>` parses the shared protocol vocabulary exactly:
  legacy aliases must be spelled as documented (`acceptEdits`,
  `bypassPermissions`); unknown names warn and keep the mode instead
  of matching case-insensitively.
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
- Console and engine: shell/tool child output decodes through the
  Windows console code page instead of assuming UTF-8; a frame
  shrink re-anchors the repaint base so no stale rows survive a
  viewport resize; settings rows cycle correctly in both directions
  and the provider wizard honors Esc; the welcome screen clamps to
  narrow terminals.
- The session event channel is bounded: a stalled frontend can no
  longer grow the queue without limit during heavy streaming
  (overflowing notices degrade to a warning, never a hang).

### Removed
- The `ls` tool (redundant with `shell` / `glob` / `read`).
- The `pty_shell` tool and the `portable-pty` dependency.
