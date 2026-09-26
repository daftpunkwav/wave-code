# crates/foundation/wire/ — frontend/backend wire protocol types

English | [中文](README.zh.md)

| Item | Role |
|---|---|
| `src/lib.rs` | `Submission`/`Op` (+ `UserImage`), `WireDecision`, `ApprovalKind`, `ToolOutcome`, `ToolCallPreview`, `Event`/`EventMsg`, and the `<system-reminder>` marker constants with `wrap_system_reminder` |

This is the single source of truth for frontend communication: every
inbound operation (`Op`) and outbound event (`EventMsg`) crossing
between frontends and the harness is defined here and nowhere else.
Variants are intentionally exhaustive — adding or renaming one is a
breaking change, and the locked wire-tag test makes every such rename
explicit. Newer optional fields ride with `serde(default)` +
`skip_serializing_if`, so events recorded before a field existed
deserialize unchanged and unset fields never appear on the wire. The
crate is pure data — its module contract forbids dependence on runtime,
state, action, safety, transport, or any orchestration layer — and
only depends on `serde`/`serde_json`; it is consumed by
`runtime/runner`, the frontends, and the operations crates.
