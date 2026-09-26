# crates/frontends/console/src/theme/ — the theme system: semantic tokens, data files, resolution

English | [中文](README.zh.md)

| File | Role |
|---|---|
| `mod.rs` | Module wiring plus `apply_terminal_scheme`: OSC 10/11/12 sync so the light theme stays readable on dark-terminal hosts |
| `tokens.rs` | The semantic color contract: the `Token` roles and the resolved `Palette`; carries no color values |
| `file.rs` | The `theme.json` format: parsing, validation, and storage of user themes (`~/.wavecode/themes/<name>.json`); unknown fields, color names, and syntax aliases are rejected |
| `builtin.rs` | The bundled themes, embedded at compile time from `themes/*.json` |
| `active.rs` | The global active `Theme` (`set`/`current`) and the paint helpers every component renders through |
| `detect.rs` | Resolution order: config choice → `NO_COLOR`/CI environment → OSC 11 terminal background probe |
| `syntax.rs` | `SyntaxTheme` aliases mapping a chrome theme onto the code-highlighting palette |
| `themes/` | The bundled theme data files: `dark.json`, `deepwave.json`, `light.json` |

Colors live in data, never in code: components request a semantic
`Token` and the active palette resolves it (23 tokens; a theme may
override any subset). One selection drives both the interface chrome and
the syntax theme coloring code blocks.
