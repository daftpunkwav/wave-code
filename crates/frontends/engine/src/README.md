# crates/frontends/engine/src/ — tui-engine source file map

English | [中文](README.zh.md)

| File | Role |
|---|---|
| `lib.rs` | Crate root: render-model documentation, module wiring, re-exports, and the no-internal-dependency test |
| `component.rs` | The component model: `Component`/`Container`/`Focusable`, and `Segment` — reference-counted line arrays shared across frames |
| `screen.rs` | The inline differential screen renderer: minimal rewrites, native scrollback, synchronized-output markers, pinned input tail, write-time margin |
| `editor.rs` | The multi-line input editor: grapheme-correct editing, CJK-aware wrapping, history, kill ring + undo, bracketed paste, autocomplete popup, submit contract |
| `autocomplete.rs` | Completion plumbing: the `CompletionProvider` trait, trigger detection, and the popup |
| `select_list.rs` | The popup select list used by slash commands, file mentions, pickers, and dialogs |
| `fuzzy.rs` | Fuzzy subsequence scoring for completion ranking |
| `markdown.rs` | Markdown-to-ANSI rendering with the `SyntaxHighlighter` and `FenceRenderer` seams (the mermaid renderer plugs in here) |
| `mermaid.rs` | Terminal rendering of fenced mermaid blocks: a common graph IR + band layout, with dedicated layouts for sequence/gitGraph/mindmap/chart kinds |
| `math.rs` | Minimal TeX-to-Unicode conversion for math segments (superscripts, fractions, roots) |
| `width.rs` | ANSI-aware visible-width measurement, truncation, and word wrapping on grapheme boundaries |
| `color.rs` | `Color`/`Style` and SGR emission, with `ColorDepth` degradation (truecolor / 256 / 16) |
| `keys.rs` | The normalized key model (`Key`, `KeyEvent`, `Mods`) so component code never parses terminal sequences |
| `terminal.rs` | Raw mode, bracketed paste, focus reporting, Kitty keyboard protocol, OSC 11 background probe, color-scheme sync |
| `border.rs` | Rounded-box drawing shared by the editor frame, welcome card, and dialogs |
| `loader.rs` | Spinner frames per activity kind (sine = composing, triangle = thinking, saw = machine work) |
| `text.rs` | Static pre-styled text components |
| `typing_burst.rs` | Paste detection for terminals without bracketed paste (Enter rewritten to Shift+Enter during a burst) |
| `sanitize.rs` | Strips C0/C1 control characters and ESC sequences from model/tool-sourced strings before they reach the terminal |
| `markdown/` | `#[cfg(test)]` tests for `markdown.rs` |
| `screen/` | `#[cfg(test)]` tests for `screen.rs` |

Components speak ANSI strings and engine-owned `Color` values only; the
screen layer diffs their line arrays between frames. Extension seams
(`SyntaxHighlighter`, `FenceRenderer`, `CompletionProvider`) let the
application plug in highlighting, diagrams, and completion sources
without this crate growing dependencies.
