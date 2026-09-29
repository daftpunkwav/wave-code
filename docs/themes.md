# Theme authoring guide

English | [中文](themes.zh.md)

A WaveCode theme is **one JSON file — nothing else**. The bundled
themes live in
`crates/frontends/console/src/theme/themes/*.json`; your own themes go
into `~/.wavecode/themes/<name>.json` and become selectable as
`/theme <name>` (the file stem is the name) right next to the built-ins
— the same mechanism VS Code theme extensions use. `wavecode doctor`
validates every user theme file.

## Quick start

Drop this into `~/.wavecode/themes/ocean-night.json`:

```json
{
  "base": "deepwave",
  "description": "a deeper ocean",
  "colors": {
    "background": "#0E1A1F",
    "primary": "#38BDF8",
    "role_user": "#38BDF8"
  }
}
```

Restart (or `/reload`) and run `/theme ocean-night`. Colors you don't
override are inherited from `base`, so a theme can be three lines.

## The format

| Field | Kind | Meaning |
| --- | --- | --- |
| `colors` | object | Any subset of the 24 tokens below, `#rrggbb` values. Missing tokens inherit from `base`. |
| `base` | string | Built-in to start from: `dark`, `deepwave`, or `light` (default `dark`). Built-in files never use `base` — they are complete. |
| `dark` | bool | The theme's kind. Optional: an explicit `background` override decides by its luminance, otherwise the base's kind, otherwise `true`. |
| `syntax_theme` | string | Code-highlighting theme: `synthwave-84`, `ocean-dark`, or `ocean-light`. Defaults to the base's. |
| `description` | string | Free text shown next to the name in the `/theme` picker. |

Unknown fields, unknown color names, malformed colors, and unknown
aliases are rejected — a typo never silently renders as the base.

## The 24 color tokens

The UI never hardcodes colors; every surface requests one of these
semantic tokens (see `theme/tokens.rs`):

- **Accent hue** (one hue carries the chrome): `primary` (prompt, user
  input, focus, spinners), `accent` (a lighter step on dark themes, a
  darker one on light), `code_span`, `role_user`, `border_focus`.
- **Neutral text ramp**: `text` (body), `text_strong` (headings,
  emphasized), `text_dim` (thinking, hints), `text_muted` (tips, fence
  urls).
- **True-gray chrome**: `neutral` (frames, rules), `border`,
  `diff_gutter`, `diff_meta`.
- **Semantic band** (never decorative): `success` / `diff_added`,
  `warning` / `shell_mode`, `error` / `diff_removed`,
  `diff_added_strong`, `diff_removed_strong`, `wave` (the wave-mode badge).
- **Surfaces**: `background` (the terminal background the ink is tuned
  against; the light theme also applies it to the terminal itself),
  `input_bg` (the user input highlight band).

## Rules the built-ins follow

The bundled themes are locked by test; keep your own themes readable
by following the same contracts:

- Body text ≥ 7:1 contrast against `background`, dim ≥ 4.5:1, muted
  ≥ 3.5:1 (WCAG relative luminance).
- The neutral ramp carries no violet cast: grays read as grays.
- `role_user`, `code_span`, and `border_focus` ride `primary`; the
  diff pair reuses the semantic band — one accent hue, nothing else.

## File layout (for contributors)

Theme code is decoupled, one responsibility per file — colors are
data, never Rust:

| Module | Responsibility |
| --- | --- |
| `theme/tokens.rs` | The semantic contract: `Token`, `Palette`, luminance/contrast math. No colors. |
| `theme/file.rs` | The `theme.json` format: parse, validate, resolve, list/load user themes. |
| `theme/builtin.rs` | The bundled data files (`include_str!`), parsed once; the palette user `base`s resolve through. |
| `theme/active.rs` | The global active theme + paint helpers. |
| `theme/detect.rs` | Resolving the configured choice / terminal probing. |
| `theme/syntax.rs` | The syntax-highlighting aliases. |

Adding a bundled theme = adding one JSON file to `theme/themes/` and
the matching entries in `builtin.rs` (the `include_str!` constant, the
`IDS` list, the `builtins()` array, and its `[BuiltinTheme; N]`
lengths). Nothing else changes.
