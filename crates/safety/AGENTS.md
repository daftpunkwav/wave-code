# safety/ agent rules

The crate map lives in [README.md](README.md).

## Boundaries

- This layer decides policy. OS confinement lives in
  `capabilities/sandbox`.
- `PermissionMode` and `ApprovalKind` come from `wavecode-protocol`
  or `wavecode-wire`. Do not add a definition here. The
  `ApprovalKind` alias in `gate/` stays deprecated.
- `audit/`, `guardrail/`, and `secrets/` depend on no workspace
  crate. `gate/` depends only on `tokio` and `thiserror`.
- `safety-audit` and `safety-guardrail` are unwired. The live
  approval path is `safety-gate` plus `wavecode-sandbox`. Do not
  splice the unwired crates into that path. Adopting one updates
  `docs/architecture.md` in the same change.
- `SecretsStore`'s `Debug` prints key names and not values.
- `redact` replaces known values with `REDACTED` (`"***"`), longest
  value first.

## Gate

- One call id has one parked waiter. The first `decide` takes the
  slot. A late decision returns false and is dropped.
- A dropped or expired waiter resolves as deny.
- `cancel` frees the id so it can park again.
- Poisoned mutexes are recovered by taking the inner guard.
