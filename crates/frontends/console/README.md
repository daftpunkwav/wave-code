# crates/frontends/console/ — the interactive terminal console UI (console-ui)

English | [中文](README.zh.md)

| Item | Role |
|---|---|
| `Cargo.toml` | Crate manifest; the boundary comment records the locked dependency set |
| `src/` | The crate source: UI orchestrator, chrome, dialogs, and the theme system (see its README) |
| `assets/` | Data shipped with the binary: `synthwave-84.tmTheme`, the bundled syntax-highlighting theme |
| `tests/` | Integration-test target directory (currently empty; coverage lives in `src/` unit tests) |

The console consumes the session exclusively through the wire protocol
(`wavecode-wire`) and the actor client (`operations-actor`), and
delegates all rendering to `tui-engine`. Its internal dependencies are
locked to exactly five — `tui-engine`, `wavecode-wire`,
`wavecode-config`, `operations-actor`, `state-persistence` — by the
`dependency_matrix_locked` test in `src/lib.rs`, which checks every key
of every dependency table against the whitelist. It must not depend on
the runtime, action, safety, or transport crates, or the composition
root.
