# crates/safety/secrets/ — secret storage with redaction

English | [中文](README.zh.md)

| Item | Role |
|---|---|
| `Cargo.toml` | crate manifest; no dependencies |
| `src/lib.rs` | `SecretsStore` (`from_env`, `insert`, `get`, `names`), `redact` with the `REDACTED` placeholder |

Values live behind explicit lookups — environment reads happen only in
the `from_env` constructor, never during recording or redaction.
`redact` replaces every known value with `***`, longest first so
overlapping values mask fully; it is best-effort hygiene for logs, not
a security boundary, since unknown values cannot be masked by
definition. Missing environment variables are skipped so optional
credentials do not fail composition, and empty values are never stored,
so they cannot redact everything.
