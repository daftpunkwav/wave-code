# Console UI subsystem

English | [中文](ui.zh.md)

The interactive console is two crates over the wire + actor seam:

- `frontends/engine` (`tui-engine`) — a pure rendering library with zero
  internal dependencies. Components produce one ANSI string per terminal
  row for a given width; the screen layer diffs those arrays between
  frames and rewrites only the changed range (inline main-screen mode,
  native scrollback preserved, CSI 2026 synchronized output).
- `frontends/console` (`console-ui`) — the themed application layer. It
  crosses to the session exclusively through `wavecode-wire` and
  `operations-actor` (locked by `dependency_matrix_locked`), and never
  names capabilities or the composition root.

## Glyph identity

The glyph family maps one-to-one onto activity categories. User input
is the one non-wave mark (a keyed-in chevron); animated work rides the
loader waveforms:

| Glyph | Category | Surfaces |
| --- | --- | --- |
| chevron `❯` | user input (keyed-in strokes) | editor prompt, user bullet, queue pointer |
| triangle sweep `▁▃▅▇▅▃` | thinking (live) | the streaming thinking header spinner |
| saw ramp `▁▃▅▇` | machine work | tool/shell running dots, the compaction card pulse |
| flower `✻` | thinking (finalized) | the frozen `Thought for Ns` header |

Result markers (`●` done, `✗` failed, `○` pending, `✓` checked) stay
neutral — they report outcome, not activity kind. The assistant message
bullet is the neutral `●` dot: it blinks on a fixed cadence while the
draft streams and settles steady once the message is complete.

## Frame model

One frame is the full logical line array:

```
transcript (welcome, user/assistant messages, thinking blocks, tool cards, shell cards, status)
todo panel               (todowrite mirror: ● in-progress / ✓ done / ○ pending)
queue pane               (queued user messages + steer hint)
editor box               (rounded frame, `❯` prompt, autocomplete popup below)
footer row 1             (▍mode model cwd ⎇ branch · rotating tip)
footer row 2             (transient exit hint ... context: N% (used/max))
```

Everything sits inside a one-column gutter so transcript and chrome share
a left edge. As the transcript grows, older lines scroll into native
scrollback and are never rewritten; the diff renderer tracks the
rewrite-eligible base and clamps cursor movement at the bottom margin.
Two screen-layer invariants keep the frame stable while it streams:

- **Pinned tail**: the frame's trailing lines (editor box, autocomplete
  popup, footer) are re-anchored to the physical bottom rows with
  absolute positioning on every frame that wrote anything, so
  streaming or scrollback churn above can never drag the input
  off-screen. Frames that still fit the screen skip the pin (the
  in-place diff already rewrites changed rows); identical frames write
  nothing at all.
- **Cursor discipline**: the hardware cursor is hidden for the whole
  frame and shown again only at the input editor's caret (an embedded
  marker the screen layer resolves to a cell), so no transcript, footer
  or streaming row can ever park a blinking cursor. Terminal modes
  restore on exit (and on panic), including the cursor.

### Segmented frames and the write-time gutter

`Component::render` returns a `Segment` — one reference-counted line
array (`Arc<Vec<String>>`). A frame is a list of segments, one per
component; nothing is flattened into a flat `Vec<String>` on the way
to the screen. The diff renderer keeps the previous frame as segments
too, which buys two things:

- **Refcount store**: storing the previous frame is a refcount bump per
  segment, not a per-line deep copy.
- **Pointer-equality skip**: a segment whose allocation is identical to
  the previous frame's same-range segment is identical by
  construction — the diff skips its whole range without comparing a
  single line. Cache-hit components (settled messages, welcome card)
  hand back the same allocation every frame, so idle regions cost one
  refcount comparison; rebuilt segments (streaming draft, status lines)
  fall back to per-line comparison with flat-compare semantics.

The frame gutter is applied at write time, not in the frame:
`Screen::set_margin` indents every row as the renderer writes it and
shrinks the truncation budget accordingly, so stored lines stay
unpadded and no frame ever pays a per-line padding copy. Cache
invalidation (theme switches, the mermaid toggle) walks the transcript
with `invalidate_all` and drops the streaming draft, so cached
segments never carry a stale palette or mode.

## Wire extensions the UI consumes

- `ToolCallEnd.output` — bounded head of the tool result
  (`ToolCallPreview`, 4 KiB, character-boundary safe). Cards render the
  collapsed outcome (≤3 lines) and expand on Ctrl+O.
- `TokenCount.context_window` / `context_used` — the footer context
  meter (`context: 42% (82.0k/195k)`, 1024-based units, ceil percent).

Both fields are `Option` + `skip_serializing_if`; older senders and
receivers remain compatible.

## Event → component mapping

| Event | UI effect |
| --- | --- |
| `TurnStarted` | phase → waiting |
| `AgentThinkingDelta` | live thinking block (spinner + last 2 lines, dim italic) |
| `AgentMessageDelta` | thinking finalizes; assistant draft streams (delta batches mark the draft dirty; the frame repaints when the 50 ms flush interval elapses, or on the 100 ms tick) |
| `AgentMessageComplete` | draft (or completion text) lands as a markdown message |
| `ToolCallBegin` / `ToolCallEnd` | tool card open/close (state dot, verb, key arg, outcome); `edit` cards lead with a clustered LCS diff preview (2 000-line budget per side, summary line beyond; failures show the apply error under the diff); `todowrite` inputs mirror into the todo panel |
| `ApprovalRequested` | approval dialog (1/2 quick-select, Enter, Esc = deny, Ctrl+C = deny) |
| `QuestionRequested` | question dialog (numbered options + free text) |
| `TokenCount` | footer context meter + cumulative usage (shown by `/usage`) |
| `Compact*` / `Plan*` / `Goal*` | status lines |
| `Warning` / `Error` | status lines (error-colored) |
| `TurnCompleted` | drafts fold, transcript trims (15 turns + 5 hysteresis), queued message dispatches |

## Input

- Multi-line editor with grapheme-correct movement, CJK-aware wrapping,
  kill ring, undo, input history (Up/Down with draft restore), and
  persistent JSONL history under `~/.wavecode/input-history/console.jsonl`.
- Bracketed paste with large-paste collapse (`[paste #N +L lines]`
  markers expand atomically on submit).
- Slash commands (`/help /new /clear /sessions /resume /fork /title
  /model /effort /permissions /auto /wave /plan /init /mcp /settings
  /theme /usage /version /status /memory /snapshots /goal /compact
  /undo /editor /copy /export /exit`) with fuzzy completion; unknown `/tokens` fall
  through as user input (skills). Every command that takes a parameter
  opens its interactive surface when invoked bare — `/theme` picks from
  the theme selector, `/effort` from the level list, `/title`, `/editor`,
  `/export`, and `/compact` open a prefilled free-text prompt, `/undo`
  opens the rewind picker, and `/btw` asks for the question — while
  instant actions (`/clear`, `/new`, `/fork`, `/auto`, `/copy`, …) and
  info-only commands (`/usage`, `/status`, `/mcp`, …) stay direct.
  `/usage` renders a severity-colored
  context bar plus the cumulative token split accumulated from
  `TokenCount` samples. `/copy` puts the last assistant message on the
  clipboard via OSC 52; `/export` writes the full untrimmed
  user/assistant dialogue to markdown (a path is prompted for; passing
  one directly skips the prompt). Mode and model commands update
  the local chrome immediately (there is no mode-changed wire event).
- `/compact` compresses the context now; the prompt accepts an optional
  instruction steering the summary (`CompactTrigger::Manual` carries the
  focus to
  the model summarizer). The transcript shows a live compaction card
  (saw-wave pulse, elapsed seconds) that settles into
  `● compacted: context <before>, summary <N> tokens`, where `before`
  is the most recent sample's context usage. A failed compaction emits
  no completion event, so the card settles into
  `● compaction failed (…)` on the error (idle `/compact`) or the turn
  end (in-turn auto compaction) that proves it dead.
- `/undo` opens the rewind picker over recent turns (or `/undo <n>`
  drops the last n directly, default 1,
  idle-only): the wire `Rewind` op truncates the actor's conversation,
  the `HistoryRewound` event trims dialogue and transcript, and the
  truncated dialogue is journaled as the newest snapshot so resume
  replays the rewound conversation. Conversation-level only — file
  changes the dropped turns already made are not undone. Double-Esc
  (600 ms window, idle only) opens the same rewind picker over the most
  recent user turns (newest first, up to 8 rows); picking a row feeds
  the same `/undo` path, so the busy/shell guards apply unchanged.
- Sessions: every interactive launch journals completed turns (text
  snapshots) under `~/.wavecode/sessions/<uuid>.jsonl` with a shared
  `index.json` (title, cwd, timestamps, turn count). `/sessions` (alias
  `/resume`) opens a picker (type-to-search, Ctrl+A toggles cwd/all
  scoping) and resumes in place: the harness re-assembles a session
  seeded with the recorded history and the transcript replays it.
  `wavecode --session <id>` and `wavecode --continue` (-c, latest
  session for the current directory) do the same from the CLI. `/fork`
  snapshots the current dialogue into a resumable copy and stays in the
  original (prints the `--session` command); `/title <title>` renames;
  `/new` starts a fresh session (new context and journal) through the
  same re-assembly path; `/clear` remains a soft screen reset. Resume
  is block-level whenever the session's write-ahead history journal
  exists (tool calls, tool results, thinking and images come back);
  sessions written before the journal existed fall back to the text
  snapshot. See `docs/subsystems/sessions-state.md`.
- `/btw <question>` asks a side question in a read-only side session
  (plan mode, seeded with the main dialogue via the same session
  factory): answers stream into a panel above the editor and never
  enter the main conversation or journal. Follow-ups (`/btw <question>`
  while the panel is open) ride the same side session; Esc closes the
  panel and shuts it down. The main session stays interactive the whole
  time.
- `/model` opens the model picker: provider tabs (Tab/Shift+Tab),
  type-to-search, ↑/↓ navigation, a `← current` marker, and an `N more`
  collapse. Entries come from the config `[models]` table
  (`[models.<alias>]` with `provider`/`model`/optional
  `reasoning_effort`) plus the model catalog `~/.wavecode/models.json`
  (its models merge into `[models]` with synthesized `catalog:<provider>`
  providers; a config.toml provider with the same id wins) and the
  configured default model. Enter saves the
  choice as the default (`default_model`/`default_provider` in
  `~/.wavecode/console-settings.json`; CLI `--model` still wins;
  a cross-provider default applies on the next launch — live switches
  stay same-provider only). Alt+S applies the choice to the session
  only, same-provider too: a cross-provider pick cannot go live, so
  Alt+S there is rejected with a pointer at Enter. The Thinking row
  (OpenAI-compatible providers; budget-driven Anthropic thinking hides
  it) switches off/low/medium/high with ←/→ and dispatches
  `SetThinking` live (same-provider picks only, so a picked model's
  effort never leaks onto the model still running). `/effort <level>`
  sets the level directly; `/model <name>` keeps direct name switching.
  The catalog itself is edited through `/model` subcommands:
  `/model list` prints every spec one status line each, `/model add
  <alias> <kind> <provider> <base_url> <model> [context] [max_output]`
  inserts one (the command never takes a credential; `api_key_env` or
  an inline `api_key` can be set by editing the file, which stays
  owner-only on Unix),
  `/model set <alias> <context|output|thinking|input> <value>` patches
  one field, and `/model remove <alias>` drops it. The catalog is
  optional: a missing file is an empty catalog, while a malformed one
  reports and leaves the subcommand cancelled (plain `/model <name>`
  switching never touches the file, so a broken catalog cannot take
  live switching down too).
- `/permissions` opens a mode picker (plan/auto/wave with
  descriptions); `/plan`, `/auto`, `/wave` apply directly.
- `/help` opens a scrollable panel (keybindings plus every command with
  a description; ↑/↓ line scroll, PgUp/PgDn page, Esc/q closes).
- `/init` sends a fixed analysis prompt so the agent writes AGENTS.md
  for the repository. `/mcp` lists configured MCP servers. `/status`
  prints the aggregate summary (session id/title, model + provider +
  thinking, mode, cwd, branch, context, usage, mcp, version).
- `!cmd` shell mode: a leading `!` runs the command locally under the
  platform shell (`cmd /C` / `sh -c`) with live output in a transcript
  card (dim tail, `(esc to cancel)`, exit-code row on failure). The
  editor tints its border shell-violet with a `! shell mode` label and
  paints the `!cmd` token while the buffer starts with `!`. Esc and
  Ctrl+C cancel the running command first (Windows kills the whole
  `cmd` tree via `taskkill /T`; elsewhere `start_kill`). Shell commands
  never reach the session, run concurrently with a busy turn, and only
  one runs at a time.
- Shift+Tab cycles the permission mode (plan → auto → wave) and
  re-tints the editor border (plan = primary, wave = warning).
- Permission modes: `plan` is read-only (non-read-only tools are
  denied outright, no prompts), `auto` lets edits through and asks
  only for command execution and destructive tools, `wave` allows
  everything. The wave denylist (`wave_denylist` in
  `~/.wavecode/console-settings.json`, `Bash(pattern)` rule syntax or
  bare commands) is enforced as sandbox deny rules in every mode: a
  banned command is refused without a prompt.
- `/settings` opens an interactive panel (Up/Down to move, Left/Right
  or Enter to cycle, changes persist to
  `~/.wavecode/console-settings.json` and apply to live components):
  user-input markdown rendering on/off, tool-call verbosity
  (names/summary/full), edit rendering (tool only/diff), and the
  wave-denylist entry count.
- Submitted user input renders as markdown in the transcript by
  default (the editor never renders); the setting turns it off.
- Tables in assistant and user markdown render as box-drawing grids
  with bold headers and even column shrink to fit the width; ragged
  rows pad onto one shared display-width grid, so CJK cells and
  checkmark glyphs cannot pull the borders out of alignment.
- `@` file mentions with a bounded workspace inventory (2 000 entries,
  vendored/hidden directories skipped).
- Ctrl+C cascade: interrupt when busy, otherwise arm the double-press
  exit (1 500 ms window, footer hint); Ctrl+D on an empty editor arms
  the same cascade. Ctrl+O toggles expansion, Ctrl+T toggles the todo
  panel, Ctrl+S steers the running turn (queued message or editor
  text), Esc interrupts while busy (both interrupts acknowledge with a
  status line), and Alt+B/Alt+F jump one word back/forward. Esc on an
  empty shell-mode prompt leaves shell mode. Ctrl+G hands the draft to
  an external editor: the
  terminal leaves raw mode, the editor runs on a temp file under the
  platform shell (`/editor <cmd>` setting, else `$VISUAL`/`$EDITOR`),
  and the saved text replaces the draft; an empty save keeps the
  original.
- Compaction shows a live card that settles into
  `● compacted (trigger): context <before>, summary <N> tokens` — or,
  when the compactor fails (a manual `/compact` reports a recoverable
  error with no turn attached), into
  `● compaction failed (trigger); context unchanged`. The console
  settles a still-running card on turn end or on the failure error, so
  the pulse and its animation ticks can never run forever.
- File-write approval payloads carry the affected lines (`-` old,
  `+` new, from the sandbox's `ask_detail`) and the approval dialog
  paints them with the diff colors — the user approves visible
  content, not a bare path. Character and line budgets bound the
  payload at the sandbox, the wire truncation, and the dialog.
- A wire-driven modal (approval or question) never outlives its turn:
  on `TurnCompleted` — and on non-recoverable errors — the dialog is
  dismissed with a status line, because the parked gate died with the
  turn and answering would only produce a "late approval" warning.
  User-opened dialogs (settings, pickers) are never touched by turn
  events.
- Turn-completion notifications: one OSC 9 desktop notification per
  finished turn unless interrupted or a queued follow-up continues the
  session (`WAVECODE_NOTIFY=0` disables). `WAVECODE_NOTIFY_STYLE`
  picks the delivery: `osc9` (default), `bell` (bare BEL ring), or
  `both`; under tmux the OSC 9 payload rides a DCS passthrough.
- Terminal chrome sequences: the window title (OSC 0,
  `WaveCode · model · session title`) tracks model, `/title`, and
  session swaps; a running turn reports indeterminate tab progress
  (OSC 9;4 state 2, re-emitted once a second, cleared on turn end and
  at exit). Sequences ride the single pending-sequence slot and never
  clobber a queued notification.
- Fenced code blocks in assistant markdown are syntax highlighted
  (syntect with the two-face extra syntax set — TypeScript, TOML, ...
  — on the pure-Rust fancy-regex backend) behind the engine's
  `SyntaxHighlighter` seam; the syntax theme follows the active chrome
  theme's pairing (synthwave → the bundled synthwave-84 tmTheme,
  deepwave → base16-ocean.dark, light → base16-ocean.light). Oversized
  blocks (>30 KB) and unknown languages fall back to plain lines.
- The footer shows the workspace git branch (⎇ badge), read directly
  from `.git/HEAD` (parent walk, worktree `gitdir:` file form, short
  sha when detached) at construction and on every turn start — no
  subprocess.
- The welcome card is frameless and centered: the `slant` figlet
  wordmark (`WAVECODE`, generated with pyfiglet) fading primary→accent
  top to bottom, sitting directly on the braille oscilloscope trace —
  a 1-pixel amplitude-modulated sine line that flows out of the
  letters — one blank spacer, then the left-aligned info grid
  (model / dir + ⎇ branch / mode + mcp + version). Below 56 columns
  the wordmark falls back to the spaced `W A V E C O D E` line.
  Resizing stirs the trace: it slides right with an ease-out tail for
  1.4 s, then settles (a tick drives the frames; `Welcome::is_rippling`
  reports the state). A `#[ignore]` snapshot test (`welcome_snapshot`)
  prints it for eyeballing.
- Every semantic glyph comes from `chrome/symbols.rs` (single source):
  `❯` chevron for user input, `●`/`✓`/`○`/`✗` neutral results, `⎇`
  branch. Motion stays single-cell: the editor prompt pulses `❯`↔`›`
  every 400 ms while a turn runs (idle is static; shell mode keeps
  `!`), the streaming assistant draft's `●` bullet blinks on a fixed
  cadence before settling steady, a running tool or shell card pulses
  the saw ramp frames, queued messages flip the chevron pulse, and the
  live thinking header spins the triangle sweep before freezing into
  `✻ Thought for Ns`.

## Input sanitizing

`ConsoleUi::push_status` / `push_user_message` / `push_assistant_message`
and the delta handlers funnel every wire-sourced string through
`sanitize_terminal` before it enters a component; tool cards sanitize
name and argument summaries the same way. The engine adds a second
guard: markdown hyperlinks whose URL carries control characters degrade
to plain styled text (no OSC 8 emission). The trust boundary stays the
same as the rest of the harness: wire strings are hostile until
sanitized.

## Steering vocabulary note

`SteerTarget` originates in `runtime-runner` but is re-exported through
`operations-actor`; the console depends only on the actor crate, which
is the established seam (the actor already wraps the runner).

## Theming

A theme is pure data: one `theme.json` per theme. The built-ins ship
as bundled files (`theme/themes/dark|deepwave|light.json`, embedded via
`include_str!` and parsed once); user themes drop into
`~/.wavecode/themes/<name>.json` and become `/theme <name>` — see
`docs/themes.md` for the authoring guide. Theme code is decoupled, one
responsibility per file (`theme/tokens.rs` semantic contract,
`theme/file.rs` format + storage, `theme/builtin.rs` bundled data,
`theme/active.rs` the global + paint helpers, `theme/detect.rs`
resolution); no color values live in Rust.

All palettes share one minimal structure: a four-step neutral text ramp
plus true-gray chrome, **one accent hue per theme** (the dark theme
rides an azure, deepwave a teal, light a GitHub blue) that carries the
prompt, user input, inline code, and focus chrome, and one semantic
band (green/amber/red) reused by the diff pair and shell mode. Nothing
else is colored; code blocks stay colorful through their syntax themes.
The user-input row sits on a subtle lighter band. 23 semantic tokens,
hex values locked by test against the parsed data files,
`light|dark|deepwave|auto` resolution (OSC 11 probe on Unix — a bounded `poll`,
never a blocking reader thread, since byte-reads on the console input
would race crossterm's event reader and steal keystrokes; Windows skips
the probe entirely → `COLORFGBG` → the default), and a global theme installed
once at startup. Components request tokens, never
raw colors; the engine works on `Color`/`Style` values. Color depth is
also resolved once at startup (`COLORTERM` truecolor, `TERM`
256-color, otherwise the 16 classic ANSI colors) and every paint
degrades through the same role mapping.

Two palette contracts are locked by test so the themes never regress
into what the old lavender ramp was:

- **No violet grays**: on every neutral ramp token (text, dim, muted,
  border, neutral, diff gutter) green stays at or above red and the
  channel progression stays even — blue may run past green by no more
  than the green-over-red step — so grays read as grays and never
  purple. The dark palette is a true graphite ramp; deepwave's slate
  (an even, blue-leaning progression) remains allowed.
- **Contrast-locked ink**: every ramp step clears WCAG contrast against
  its own theme `background` token — body ≥ 7:1, dim ≥ 4.5:1, muted
  ≥ 3.5:1 — and the input band stays a quiet, visible step of that
  background.

The `background` token is also functional: the light theme applies its
paper + ink + cursor color to the terminal itself (OSC 11 background,
OSC 10 foreground, OSC 12 cursor — best-effort) so it stays readable
and usable on dark-terminal hosts — whose near-white default
foreground and cursor would otherwise vanish on the paper. Any theme
switch back to a dark theme — or terminal restore on exit and around
the external-editor round trip — resets all three to the terminal's
own colors (OSC 110/111/112). Terminals that ignore the sequences are
unaffected. As defense in depth, spans that would otherwise inherit
the terminal's default foreground — the editor draft body, the
autocomplete popup, markdown list markers, and editor scroll labels —
are painted from the theme, and the message renderers restyle their
markdown on every theme switch (a message rendered under `dark`
repaints under `light`, not the reverse order of operations).

The **deepwave** identity stays selectable (`/theme deepwave` or
`"base": "deepwave"` in a custom theme): the same minimal structure
with a teal accent on a slate ramp.

One theme selection drives both the chrome roles and the syntax
highlighting: each built-in theme pairs with a syntect theme
(synthwave → the bundled `synthwave-84` tmTheme, deepwave →
`base16-ocean.dark`, light → `base16-ocean.light`; see
`theme/syntax.rs`). The highlighter (`console/src/highlight.rs`) reads
only foreground colors and font styles from the syntect theme —
backgrounds never render, so code blocks always sit on the terminal's
own background.

User themes follow the same file format (a `base`
(`dark`/`light`/`deepwave`) plus any subset of the 23 tokens as
`#rrggbb` overrides, an optional `syntax_theme` alias, an optional
`description` shown in the picker, and an optional `dark` kind).
Unknown token names, malformed colors, unknown aliases, and unknown
fields are rejected (a typo must not silently render as the base),
path escapes never reach the filesystem, and `/theme <name>` applies
one live — the editor and popup styles are rebuilt from the new
palette, as for the built-ins, and the repaint clears the scrollback
so rows drawn under the previous palette cannot survive.

## Markdown rendering conventions

The renderer (`tui-engine/src/markdown.rs`) keeps one blank line
between blocks and never two: every block (heading, paragraph, list,
fence, quote, table, rule) ensures separation from whatever came
before, instead of pushing trailing blanks — so a list followed by a
heading can no longer cram together, and a mid-document heading that
is still empty during streaming never leaves a phantom blank line.
Tight list items stay tight; loose-list (blank-separated) items render
with their source separation.

Fenced code blocks render as a dim rounded frame — `╭─ lang ───` on
top (language tag only when the fence carries one, whitelisted to
language-name characters so fence info can never inject escape
sequences), a dim `│ ` bar before each highlighted line, `╰────` at
the bottom. The literal ``` markers never render. The frame hugs its
content (plus the tag) and caps at 80 columns on wide terminals; the
highlighted text itself is not re-wrapped in the fence style, since
the inner ANSI resets would cancel it.

### Fenced-block renderers and inline fixups

Fences consult pluggable renderers before the syntax highlighter (the
`FenceRenderer` seam; applications register more with
`Markdown::with_fence`):

- ```mermaid fences render as box-drawing diagrams (on by default;
  Ctrl+M in the console toggles the source view). One shared graph
  engine (nodes + styled edges + subgraph frames, layered band or
  column layouts) carries `graph`/`flowchart` (TD/TB/LR, subgraph
  groups, node shapes collapse onto boxes), `stateDiagram(-v2)` through
  a flowchart adapter, `classDiagram`, `erDiagram`,
  `requirementDiagram`, and the `C4*` family. Kinds with their own
  geometry: `sequenceDiagram` lanes, `gitGraph` branch timeline,
  `mindmap`/`timeline` trees, and the chart rows of `pie`, `journey`,
  `quadrantChart`, and `xychart-beta`. Anything unsupported
  (`block-beta`, `sankey-beta`), or wider than the columns budget,
  returns nothing and the fence falls back to the plain source view.
- ```diff / ```patch fences ride dedicated line styles: additions
  green, removals red, file headers and `@@` hunks in the metadata
  tone, context plain.

Inline preprocessing (all fenced-code aware, applied before parsing):
`$…$` / `$$…$$` math converts to unicode (`E = mc²`; block math lays
matrix environments out over aligned lines), `==highlight==` folds
onto bold, `^sup^` / `~sub~` map onto superscript/subscript code
points when every glyph in the span has one, `<details>`/`</details>`
rows vanish while `<summary>X</summary>` becomes a bold `▸ X` marker
row, and `<kbd>C</kbd>` becomes inline code. Spans that cannot map —
or tags outside the handled set — pass through unchanged.

## Deliberate non-goals this generation

Background tasks and a plan approval flow need wire or
backend support that does not exist yet. Cross-provider live model
switches need session re-assembly by design (base URL and credentials
are baked into the provider client), so the picker persists a default
for the next launch instead.
