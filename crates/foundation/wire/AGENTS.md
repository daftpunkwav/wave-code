# foundation/wire/ agent rules

The type map lives in [README.md](README.md).

## Contract

- `Submission`, `Op`, `Event`, and `EventMsg` are defined here. Do not
  add a second copy in `wavecode-protocol` or a frontend.
- Adding, removing, or renaming a variant or a serde tag updates
  `wire_tags_are_locked` and the round-trip tests in `src/lib.rs`,
  plus `sdk/typescript/src/types.ts`, in the same change.
- `ApprovalKind` is the serde twin of
  `wavecode_protocol::ApprovalKind`. A tag change updates both and the
  byte-equal lock in `operations/bootstrap/src/lib.rs` in the same
  change.
- New optional fields use `serde(default)` and
  `skip_serializing_if`. Unset fields do not appear on the wire.
- System-reminder text goes through `wrap_system_reminder`.
- This crate depends only on `serde` and `serde_json`.
- It does not depend on runtime, state, action, safety, transport, or
  any orchestration crate.
