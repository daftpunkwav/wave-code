# frontends/engine/ agent rules

The render model lives in [README.md](README.md).

## Rendering

- Zero workspace dependencies. `dependency_matrix_locked` in
  `src/lib.rs` allowlists `crossterm`, `unicode-width`,
  `unicode-segmentation`, `pulldown-cmark`, and `libc`. `libc` is the
  unix target dependency.
- A component returns one ANSI string per terminal row for a given
  width.
- `screen` diffs row arrays and rewrites the changed range. It
  preserves native scrollback and does not enter the alternate
  screen.
- Themes, sessions, and the wire protocol stay in `console-ui`.
- Integration tests under `tests/` use the public API only.
