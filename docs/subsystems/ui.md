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
  /copy /export /exit`) with fuzzy completion; unknown `/tokens` fall
  through as user input (skills). `/usage` renders a severity-colored
  context bar plus the cumulative token split accumulated from
  `TokenCount` samples. `/copy` puts the last assistant message on the
  clipboard via OSC 52; `/export [path]` writes the full untrimmed
  user/assistant dialogue to markdown. Mode and model commands update
  the local chrome immediately (there is no mode-changed wire event).
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
  (names/summary/full), edit rendering (tool only/diff), diff layout
  (unified/split), and the wave-denylist entry count.
- Submitted user input renders as markdown in the transcript by
  default (the editor never renders); the setting turns it off.
- Tables in assistant and user markdown render as box-drawing grids
  with bold headers and even column shrink to fit the width.
- `@` file mentions with a bounded workspace inventory (2 000 entries,
  vendored/hidden directories skipped).
- Ctrl+C cascade: interrupt when busy, otherwise arm the double-press
  exit (1 500 ms window, footer hint); Ctrl+D on an empty editor arms
  the same cascade. Ctrl+O toggles expansion, Ctrl+T toggles the todo
  panel, Ctrl+S steers the running turn (queued message or editor
  text), Esc interrupts while busy (both interrupts acknowledge with a
  status line).
- Turn-completion notifications: one OSC 9 desktop notification per
  finished turn unless interrupted or a queued follow-up continues the
  session (`WAVECODE_NOTIFY=0` disables). `WAVECODE_NOTIFY_STYLE`
  picks the delivery: `osc9` (default), `bell` (bare BEL ring), or
  `both`; under tmux the OSC 9 payload rides a DCS passthrough.
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

19 semantic tokens, dark/light palettes (hex values locked by test),
`light|dark|auto` resolution (OSC 11 probe on Unix — a bounded `poll`,
never a blocking reader thread, since byte-reads on the console input
would race crossterm's event reader and steal keystrokes; Windows skips
the probe entirely → `COLORFGBG` → dark), and a global theme installed
once at startup. Components request tokens, never
raw colors; the engine works on `Color`/`Style` values.

## Deliberate non-goals this generation

Background tasks, plan approval flow, and syntax highlighting in code
blocks (`PlainHighlighter` stands in; the seam exists) need wire or
backend support that does not exist yet; the approval dialog only
offers decisions the backend implements (`AllowOnce` / `Deny` —
`AllowAlways` is reserved but unwired). Cross-provider live model
switches need session re-assembly by design (base URL and credentials
are baked into the provider client), so the picker persists a default
for the next launch instead.
