# crates/foundation/protocol/ — shared permission-mode and approval-kind vocabulary

English | [中文](README.zh.md)

| Item | Role |
|---|---|
| `src/lib.rs` | `PermissionMode` (`plan` / `auto` / `wave` wire strings, legacy-alias `parse`, `as_str`) and `ApprovalKind` (`exec` / `write`) |

This crate holds the small enums every layer must name identically —
config's `permission_mode` strings, the sandbox's decision logic, and
frontend display all read `PermissionMode` from here so the modes
cannot drift apart. `PermissionMode::parse` keeps legacy config names
loading (`guarded`/`default`/`acceptEdits` map to `Auto`,
`bypassPermissions`/`yolo` map to `Wave`). It deliberately contains no
request or event types: the live `Submission`/`Event` surface has
exactly one home, `wavecode-wire`, and a second copy must not
reappear here. The wire tag spellings are locked by tests, and the
only dependencies are `serde`/`serde_json`; in-workspace consumers
today are `capabilities/sandbox` and `operations/bootstrap`.
