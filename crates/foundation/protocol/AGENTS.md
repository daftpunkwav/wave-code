# foundation/protocol/ agent rules

The enum map lives in [README.md](README.md).

## Vocabulary

- `PermissionMode`, `ToolKind`, and the mode cycle
  (`next_in_cycle`, `cycle_from_str`) are defined here. Other crates
  use these types. Do not mirror the enums or their wire strings.
- Wire strings are `plan`, `auto`, and `wave`.
- `PermissionMode::parse` keeps these legacy aliases: `guarded`,
  `default`, and `acceptEdits` map to `Auto`; `bypassPermissions` and
  `yolo` map to `Wave`.
- `ToolKind::Other` is the default.
- `ApprovalKind` here is the sandbox-side enum. `wavecode-wire` holds
  the serde twin. A tag change updates both enums and the byte-equal
  lock in `operations/bootstrap/src/lib.rs` in the same change.
- Do not add a third `ApprovalKind`. The alias in `safety-gate` stays
  deprecated.
- This crate does not hold `Submission`, `Op`, `Event`, or `EventMsg`.
- The production dependency is `serde`. `serde_json` is a
  dev-dependency. The crate depends on no workspace crate.
