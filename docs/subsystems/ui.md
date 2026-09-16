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

## Frame model

One frame is the full logical line array:

```
transcript (welcome, user/assistant messages, thinking blocks, tool cards, status)
activity pane            (phase spinner: moon for waiting/tool, braille for composing)
queue pane               (queued user messages + steer hint)
editor box               (rounded frame, `>` prompt, autocomplete popup below)
footer row 1             ([mode] model cwd | rotating tip)
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
| `ToolCallBegin` / `ToolCallEnd` | tool card open/close (state dot, verb, key arg, outcome); `edit_file` cards lead with a clustered LCS diff preview (2 000-line budget per side) |
| `ApprovalRequested` | approval dialog (1/2 quick-select, Enter, Esc = deny) |
| `QuestionRequested` | question dialog (numbered options + free text) |
| `TokenCount` | footer context meter |
| `Compact*` / `Plan*` / `Goal*` | status lines |
| `Warning` / `Error` | status lines (error-colored) |
| `TurnCompleted` | drafts fold, transcript trims (15 turns + 5 hysteresis), queued message dispatches |

## Input

- Multi-line editor with grapheme-correct movement, CJK-aware wrapping,
  kill ring, undo, input history (Up/Down with draft restore), and
  persistent JSONL history under `~/.wavecode/input-history/console.jsonl`.
- Bracketed paste with large-paste collapse (`[paste #N +L lines]`
  markers expand atomically on submit).
- Slash commands (`/help /compact /model /permissions /plan /theme
  /memory /snapshots /goal /status /exit`) with fuzzy completion;
  unknown `/tokens` fall through as user input (skills).
- `@` file mentions with a bounded workspace inventory (2 000 entries,
  vendored/hidden directories skipped).
- Ctrl+C cascade: interrupt when busy, otherwise arm the double-press
  exit (1 500 ms window, footer hint). Ctrl+O toggles expansion, Ctrl+S
  steers the running turn (queued message or editor text), Esc
  interrupts while busy.

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

Shell mode (`!`), background tasks, todo panels, plan approval
flow, and model pickers need wire or backend support that does not
exist yet; the approval dialog only offers decisions the backend
implements (`AllowOnce` / `Deny` — `AllowAlways` is reserved but
unwired).
