# crates/safety/audit/ — append-only audit trail

English | [中文](README.zh.md)

| Item | Role |
|---|---|
| `Cargo.toml` | crate manifest; no dependencies |
| `src/lib.rs` | `AuditVerdict` (Allow/Ask/Deny/Error), `AuditEvent` (seq, actor, action, target, verdict), `AuditLog` with `append` / `by_actor` / `all` |

Every entry gets a monotonic sequence number and the log keeps emission
order, so `by_actor` gives incident review a stable filtered view of
who did what to which tool, and what policy said. The trail is
memory-only, and the crate depends on no other workspace crate.
