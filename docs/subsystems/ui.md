# Console UI subsystem

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

## Wave identity

Basic wave kinds map one-to-one onto activity categories; the same
glyph always means the same kind of surface:

| Wave | Category | Surfaces |
| --- | --- | --- |
| square `⊓` | user input (keyed-in pulses) | editor prompt, user bullet, queue pointer |
| sine `∿` | assistant speech | assistant bullet, composing spinner |
| triangle `△` | thinking | thinking spinner + bullet |
| saw `◿` | machine work | tool/shell running dot, waiting/tool spinner |

Result markers (`●` done, `✗` failed, `○` pending) stay neutral — they
report outcome, not activity kind.

## Frame model

One frame is the full logical line array:

```
transcript (welcome, user/assistant messages, thinking blocks, tool cards, shell cards, status)
todo panel               (todowrite mirror: ● in-progress / ✓ done / ○ pending)
queue pane               (queued user messages + steer hint)
editor box               (rounded frame, `⊓⊔` prompt, autocomplete popup below)
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
  through as user input (skills). `/usage` renders a severity-colored
  context bar plus the cumulative token split accumulated from
  `TokenCount` samples. `/copy` puts the last assistant message on the
  clipboard via OSC 52; `/export [path]` writes the full untrimmed
  user/assistant dialogue to markdown. Mode and model commands update
  the local chrome immediately (there is no mode-changed wire event).
- `/compact [instruction]` compresses the context now, optionally
  steering the summary (`CompactTrigger::Manual` carries the focus to
  the model summarizer). The transcript shows a live compaction card
  (saw-wave pulse, elapsed seconds) that settles into
  `● compacted: context <before>, summary <N> tokens`, where `before`
  is the most recent sample's context usage. A failed compaction emits
  no completion event, so the card settles into
  `● compaction failed (…)` on the error (idle `/compact`) or the turn
  end (in-turn auto compaction) that proves it dead.
- `/undo [n]` drops the last n conversation turns (default 1,
  idle-only): the wire `Rewind` op truncates the actor's conversation,
  the `HistoryRewound` event trims dialogue and transcript, and the
  truncated dialogue is journaled as the newest snapshot so resume
  replays the rewound conversation. Conversation-level only — file
  changes the dropped turns already made are not undone. Double-Esc
  (600 ms window, idle only) opens a rewind picker over the most
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
  is text-level by design: tool blocks are not replayed.
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
  `reasoning_effort`) plus the configured default model. Enter saves the
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
  letters — above the dim-label info grid (model / dir + ⎇ branch /
  mode + mcp + version). Below 56 columns the wordmark falls back to
  the spaced `W A V E C O D E` line. Resizing stirs the trace: it
  slides right with an ease-out tail for 1.4 s, then settles (a tick
  drives the frames; `Welcome::is_rippling` reports the state). A
  `#[ignore]` snapshot test (`welcome_snapshot`) prints it for
  eyeballing.
- Every semantic glyph comes from `chrome/symbols.rs` (single source):
  `⊓⊔` square wave for user input, `∿` sine for assistant speech, `△`
  triangle for thinking, `●`/`✓`/`○`/`✗` neutral results, `⎇` branch.
  Motion is a single-cell pulse instead of moving blocks: the editor
  prompt phase-flips `⊓⊔`↔`⊔⊓` every 400 ms while a turn runs (idle
  is static; shell mode keeps `!`), the streaming assistant draft
  breathes through the sine amplitude frames before settling on `∿`,
  a running tool or shell card pulses the saw amplitude, queued
  messages flip square-wave phases.

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

The default dark palette is the **synthwave** identity: neon cyan as
the primary accent, pink for user input, green success, salmon
warning, red-pink error, on a deep purple-dark ground (see
`theme/colors.rs`). 20 semantic tokens, dark/light palettes (hex values
locked by test),
`light|dark|deepwave|auto` resolution (OSC 11 probe on Unix — a bounded `poll`,
never a blocking reader thread, since byte-reads on the console input
would race crossterm's event reader and steal keystrokes; Windows skips
the probe entirely → `COLORFGBG` → the default), and a global theme installed
once at startup. Components request tokens, never
raw colors; the engine works on `Color`/`Style` values. Color depth is
also resolved once at startup (`COLORTERM` truecolor, `TERM`
256-color, otherwise the 16 classic ANSI colors) and every paint
degrades through the same role mapping.

The previous **deepwave / sonar** identity stays selectable
(`/theme deepwave` or `"base": "deepwave"` in a custom theme): teal
primary, the four waveform categories across an ocean spectrum.

One theme selection drives both the chrome roles and the syntax
highlighting: each built-in theme pairs with a syntect theme
(synthwave → the bundled `synthwave-84` tmTheme, deepwave →
`base16-ocean.dark`, light → `base16-ocean.light`; see
`theme/syntax.rs`). The highlighter (`console/src/highlight.rs`) reads
only foreground colors and font styles from the syntect theme —
backgrounds never render, so code blocks always sit on the terminal's
own background.

Custom themes live in `~/.wavecode/themes/<name>.json`: a `base`
(`dark`/`light`/`deepwave`) plus any subset of the 20 tokens as `#rrggbb`
overrides, and an optional `syntax_theme` alias (`synthwave-84`,
`ocean-dark`, `ocean-light`) overriding the base's syntax pairing.
Unknown token names, malformed colors, and unknown aliases are rejected (a
typo must not silently render as the base), path escapes never reach
the filesystem, and `/theme <name>` applies one live — the editor and
popup styles are rebuilt from the new palette, as for the built-ins.

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
the bottom. The literal ``` markers never render. The frame width
follows the terminal (capped at 80 columns); the highlighted text
itself is not re-wrapped in the fence style, since the inner ANSI
resets would cancel it.

### Reference study (codex, opencode, claude-code, kimi-code)

What was adopted, and what was rejected:

- **codex (ratatui)** — closest analog. Adopted: syntax themes as a
  registry of names resolved against the two-face bundle with
  runtime-swappable selection (codex keeps a `THEME` global plus a
  revision counter; wave pairs the syntax theme with the chrome theme
  instead, which satisfies the same goal with less machinery). Adopted:
  the terminal background decides the default look. Rejected: no
  framing at all around code blocks — with sparse highlights a block
  has no visible extent; wave draws the dim frame.
- **opencode (charmbracelet)** — the reference for
  palette-drives-syntax: its `generateSyntax(theme)` derives every
  syntax scope color from the theme's own `syntax*` values, exactly
  the pairing wave implements with `SyntaxTheme`. Adopted its
  synthwave84 color values as the on-disk reference for the neon
  palette. Rejected: one-line `marginTop` between message parts —
  wave needs in-message rhythm too, so separation lives in the
  renderer.
- **kimi-code (pi-tui)** — a streaming markdown lexer like wave's.
  Adopted: "add spacing unless a space token follows" is the same
  invariant wave's ensure-one-blank implements without lookahead.
  Rejected: rendering the raw ``` fence line as a dim border row — the
  fence markers themselves were the complaint; wave frames with rules
  instead.
- **claude-code (Ink)** — desktop/web surface, not a terminal
  markdown renderer; nothing applicable beyond confirming the common
  convention of blank-line-separated blocks.

## Deliberate non-goals this generation

Background tasks and a plan approval flow need wire or
backend support that does not exist yet. Cross-provider live model
switches need session re-assembly by design (base URL and credentials
are baked into the provider client), so the picker persists a default
for the next launch instead.
