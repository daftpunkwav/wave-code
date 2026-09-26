# crates/foundation/auth/ — provider-scoped credentials as explicit values

English | [中文](README.zh.md)

| Item | Role |
|---|---|
| `src/lib.rs` | `Credential` (API-key/bearer, redacted `Debug`, `expose()`), `AuthStore` (provider-keyed lookup, `from_env`, `insert`/`remove`/`get`/`get_or`), `Scheme`, `AuthError`, `REDACTED` |

Lookup is always scoped by provider name, so two providers can never
share material by accident, and empty values are rejected at
construction so a misconfigured environment fails at composition time.
Raw secret material leaves this crate only through `Credential::expose`;
`Debug` prints only the scheme with `REDACTED` in place of the value,
so logs and `{:#?}` dumps of parent structs cannot leak keys.
`from_env` skips missing or empty environment variables instead of
failing, so optional providers do not break startup. Durable storage
(OS keyrings, vaults) is out of scope: callers push values in through
`AuthStore::insert` or `from_env`, and transcript masking belongs to
the secrets store those call sites feed. The crate declares zero
dependencies (not even external ones), and no other workspace crate
currently depends on it.
