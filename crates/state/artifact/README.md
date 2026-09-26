# crates/state/artifact/ — versioned registry of run-produced artifacts

English | [中文](README.zh.md)

| Item | Role |
|---|---|
| `Cargo.toml` | crate manifest; no dependencies |
| `src/lib.rs` | `ArtifactKind`, `Artifact` (`<name>@<version>` id, kind, digest), `ArtifactStore` with `publish` / `get` / `latest` / `list` |

`publish` auto-increments the version per name, so re-publishing never
destroys history; `latest` resolves the floating pointer and `get` pins
one versioned id. Payloads stay with their producers — the store tracks
only identity, kind, and the producer-supplied integrity digest (opaque
format), and it depends on no other workspace crate.
