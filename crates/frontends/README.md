# crates/frontends/ — user-facing surfaces: the console UI, its rendering engine, and the `wavecode` CLI entry

English | [中文](README.zh.md)

| Crate | Role |
|---|---|
| `console/` | `console-ui` — the interactive terminal frontend: themed chrome, dialogs, slash commands, transcript rendering (see its README) |
| `engine/` | `tui-engine` — the inline terminal rendering library: components, editor, markdown/mermaid, differential screen (see its README) |
| `harness/` | `harness-cli` — the `wavecode` binary: headless exec, REPL, console launch, and the app-server surface (see its README) |

Dependency direction points downward only. `tui-engine` is a pure
rendering library with zero internal workspace dependencies, locked by a
test in its `lib.rs`. `console-ui` renders exclusively through
`tui-engine` and reaches the session only over the wire protocol
(`wavecode-wire`) and the actor client (`operations-actor`); its full
internal set is five crates (`tui-engine`, `wavecode-wire`,
`wavecode-config`, `operations-actor`, `state-persistence`), also locked
by a test. `harness-cli` assembles sessions through
`operations-bootstrap` and owns every command-line surface of the
`wavecode` binary, including the `serve` app-server.
