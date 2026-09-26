# crates/frontends/console/src/ — console-ui source file map

English | [中文](README.zh.md)

| File | Role |
|---|---|
| `lib.rs` | Crate root: module wiring, re-exports, and the dependency-matrix test that locks the five internal dependencies |
| `ui.rs` | The UI orchestrator: frame assembly, key/paste/resize dispatch, and the session event pump; terminal recovery on panic and BrokenPipe |
| `dialogs.rs` | Modal dialogs — tool approvals and structured questions; a dialog owns all key input while open |
| `slash.rs` | Slash command registry, parsing, and dispatch effects; unknown tokens fall through as plain input |
| `complete.rs` | Completion providers for slash commands and `@` file mentions (bounded workspace inventory) |
| `diff.rs` | Clustered LCS diff rendering for file edits: `+N -M` header, gutter grid, streaming-safe |
| `highlight.rs` | syntect syntax highlighting behind the engine's `SyntaxHighlighter` seam; the SynthWave '84 tmTheme is bundled |
| `history.rs` | Persistent input history: append-only JSONL per frontend surface |
| `settings.rs` | Persistent UI preferences (`~/.wavecode/console-settings.json`) shared through one `Arc<Mutex<UiSettings>>` |
| `state.rs` | `AppState` shared by chrome components: session identity, streaming phase, context usage, queued input, todos |
| `transcript.rs` | Turn-based transcript buffer with windowed trimming (keeps the newest 15 turns, hysteresis 5) |
| `welcome.rs` | The welcome card: figlet wordmark over the braille oscilloscope trace, with a resize ripple |
| `git_info.rs` | Branch badge: reads `.git/HEAD` directly, no subprocess |
| `chrome/` | Persistent frame furniture: footer rows, external status line, notifications, tab progress, symbols, todo panel |
| `controllers/` | Glue between session events and UI state: streaming coalescing, the `/btw` side session, `!` shell jobs |
| `messages/` | Transcript message components: user/assistant/status, thinking, tool calls, compaction, shell, usage |
| `panes/` | Framed regions between the transcript and the editor: the queued-message pane |
| `theme/` | Theme system: semantic tokens, `themes/*.json` data files, resolution (see its README) |
| `ui/` | Test-only modules of `ui.rs`: behavior tests, `/model` catalog tests, and the ANSI showcase dump |

Frame assembly, input dispatch, and the session event pump live in
`ui.rs`; everything else is either a component it composes
(`messages/`, `chrome/`, `panes/`, dialogs, transcript) or supporting
state (settings, history, theme). Rendering crosses only `tui-engine`
types; session interaction crosses only `wavecode-wire` events/ops and
`operations-actor` clients.
