# crates/frontends/engine/ — the zero-internal-dependency inline terminal rendering library (tui-engine)

English | [中文](README.zh.md)

| Item | Role |
|---|---|
| `Cargo.toml` | Manifest; the boundary comment records the zero-internal-dependency rule |
| `src/` | The library source: components, editor, markdown/mermaid/math renderers, and the differential screen (see its README) |
| `tests/` | Black-box integration tests over the public API only: `markdown_contract.rs` (render contract, caching, fence seam) and `mermaid_dispatch.rs` (diagram dispatch and `MermaidFences`) |

The engine is a pure rendering library with zero internal workspace
dependencies — external crates only (`crossterm`, `unicode-width`,
`unicode-segmentation`, `pulldown-cmark`, plus `libc` on unix), locked
by the `dependency_matrix_locked` test in `src/lib.rs`. The render
model: every component produces one ANSI string per terminal row for a
given width, and `screen` diffs those arrays between frames, rewriting
only what changed while preserving native scrollback (never the
alternate screen). Semantic theming and session wiring live in the
application layer above; the engine knows neither.
