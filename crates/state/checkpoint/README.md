# crates/state/checkpoint/ — labeled checkpoints and file-content snapshots

English | [中文](README.zh.md)

| Item | Role |
|---|---|
| `Cargo.toml` | crate manifest; `thiserror`, `serde_json`, `tokio`, `wavecode-config` (home directory) |
| `src/lib.rs` | `Checkpoint`/`CheckpointStore` (in-memory, newest-wins rollback), `CheckpointPolicy` with `durable_save`/`durable_load`/`list_resume_labels` (atomic, fsynced `<label>.json` files), `SnapshotStore` (working-tree captures under `<home>/.wavecode/snapshots` with caps and rewind) |

Snapshot payloads are opaque strings owned by the caller: the store
never interprets them, and what a restore means belongs to the driver.
Labels become file or directory names, so they must match
`[A-Za-z0-9_-]{1,64}`; captures are capped (512 KB per file, 1000
files, 64 MB total), skip binary content and `.git`/`target`/
`node_modules`/`.venv`, and live outside the working directory, so a
rewind never depends on git. `CheckpointPolicy` defaults fail-closed —
both hook points checkpoint — while a fully disabled policy
short-circuits with zero IO.
