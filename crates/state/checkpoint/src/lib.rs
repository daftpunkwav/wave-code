/*!
 * @file CheckpointStore
 * @description Labelled state snapshots with rollback support.
 *
 * Responsibilities:
 * - Save opaque state snapshots under human labels.
 * - Roll back to a label, discarding newer snapshots.
 * - List labels in save order for inspection.
 *
 * This module must not depend on: any other workspace crate. Snapshot
 * payloads are opaque strings owned by the caller (serialized state).
 */

//! Checkpoints: named recovery points with newest-wins rollback.
//!
//! The store never interprets payloads; recovery semantics (what restore
//! means for history, plans, and tasks) belong to the driver.

/// One saved snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Checkpoint {
    /// Monotonic save number starting at 1.
    pub seq: u64,
    /// Human label, e.g. a task or step id.
    pub label: String,
    /// Opaque payload owned by the caller.
    pub data: String,
}

/// Checkpoint errors.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CheckpointError {
    /// No checkpoint exists under the label.
    #[error("unknown checkpoint: {0}")]
    UnknownLabel(String),
    /// Label rejected: expected 1-64 chars of `[A-Za-z0-9_-]`.
    #[error("invalid checkpoint label: {0}")]
    InvalidLabel(String),
    /// Filesystem failure while writing or fsyncing the durable copy.
    #[error("checkpoint IO failed: {0}")]
    Io(String),
}

impl From<std::io::Error> for CheckpointError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e.to_string())
    }
}

/// Ordered checkpoint store with rollback.
#[derive(Debug, Default)]
pub struct CheckpointStore {
    checkpoints: Vec<Checkpoint>,
}

impl CheckpointStore {
    /// Create an empty store.
    pub fn new() -> Self {
        Self::default()
    }

    /// Save one snapshot, returning its sequence number.
    pub fn save(&mut self, label: impl Into<String>, data: impl Into<String>) -> u64 {
        let seq = self.checkpoints.len() as u64 + 1;
        self.checkpoints.push(Checkpoint {
            seq,
            label: label.into(),
            data: data.into(),
        });
        seq
    }

    /// Fetch the newest snapshot under a label.
    pub fn get(&self, label: &str) -> Option<&Checkpoint> {
        self.checkpoints.iter().rev().find(|c| c.label == label)
    }

    /// Roll back to a label, keeping its snapshot and discarding newer ones.
    pub fn rollback(&mut self, label: &str) -> Result<&Checkpoint, CheckpointError> {
        let pos = self
            .checkpoints
            .iter()
            .rposition(|c| c.label == label)
            .ok_or_else(|| CheckpointError::UnknownLabel(label.to_string()))?;
        self.checkpoints.truncate(pos + 1);
        Ok(&self.checkpoints[pos])
    }

    /// Labels in save order, including duplicates.
    pub fn labels(&self) -> Vec<&str> {
        self.checkpoints.iter().map(|c| c.label.as_str()).collect()
    }

    /// Number of stored snapshots.
    pub fn len(&self) -> usize {
        self.checkpoints.len()
    }

    /// True when nothing is stored.
    pub fn is_empty(&self) -> bool {
        self.checkpoints.is_empty()
    }
}

/// Durability policy for turn checkpoints.
///
/// Both flags default to true (fail-closed): when in doubt the actor
/// checkpoints rather than skipping. Setting a flag to false disables
/// that hook point, and a fully-disabled policy short-circuits with
/// zero IO (no file is created, read, or even probed).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CheckpointPolicy {
    /// Save a durable checkpoint before a model request starts a turn.
    pub checkpoint_before_model_request: bool,
    /// Save a durable checkpoint before a tool side effect executes.
    pub before_tool_side_effect: bool,
}

impl Default for CheckpointPolicy {
    fn default() -> Self {
        Self {
            checkpoint_before_model_request: true,
            before_tool_side_effect: true,
        }
    }
}

impl CheckpointPolicy {
    /// Policy with every hook point disabled: checkpoint helpers become
    /// no-ops that touch neither the store nor the filesystem.
    pub fn disabled() -> Self {
        Self {
            checkpoint_before_model_request: false,
            before_tool_side_effect: false,
        }
    }

    /// True when at least one hook point still checkpoints.
    pub fn anything_enabled(&self) -> bool {
        self.checkpoint_before_model_request || self.before_tool_side_effect
    }
}

/// Durable checkpoint file extension under the checkpoints root.
pub const CHECKPOINT_FILE_EXTENSION: &str = "json";

/// Validate a turn-checkpoint label before it touches the filesystem.
///
/// Labels become a single `<label>.json` file name, so the rule matches
/// snapshot labels: 1-64 chars of `[A-Za-z0-9_-]`, blocking `/` and `..`.
pub fn validate_checkpoint_label(label: &str) -> Result<(), CheckpointError> {
    validate_snapshot_label(label).map_err(CheckpointError::InvalidLabel)
}

/// Path of one durable checkpoint file, validating the label first.
fn checkpoint_file_path(
    root: &std::path::Path,
    label: &str,
) -> Result<std::path::PathBuf, CheckpointError> {
    validate_checkpoint_label(label)?;
    Ok(root.join(format!("{label}.{CHECKPOINT_FILE_EXTENSION}")))
}

/// Durably save one checkpoint payload under `root/<label>.json`.
///
/// The write is atomic (temp file plus rename) and fsynced before the
/// rename returns, so a crash never leaves a half-written label. Callers
/// save to the in-memory [`CheckpointStore`] first and call this second;
/// on error the in-memory entry stays, making failures loud but never
/// silent data loss.
pub fn durable_save(
    root: &std::path::Path,
    label: &str,
    data: &str,
) -> Result<(), CheckpointError> {
    let dest = checkpoint_file_path(root, label)?;
    std::fs::create_dir_all(root)?;
    let payload = serde_json::json!({"label": label, "data": data});
    let bytes = serde_json::to_string(&payload).map_err(|e| CheckpointError::Io(e.to_string()))?;
    // Stage beside the destination so rename stays on one filesystem.
    let staging = root.join(format!(".staging-{label}.{CHECKPOINT_FILE_EXTENSION}"));
    std::fs::write(&staging, &bytes)?;
    // Fsync the payload before it becomes visible under its label. The
    // handle needs write access: FlushFileBuffers fails on read-only
    // handles (Windows ERROR_ACCESS_DENIED).
    std::fs::OpenOptions::new()
        .write(true)
        .open(&staging)?
        .sync_all()?;
    std::fs::rename(&staging, &dest)?;
    // Best-effort directory fsync so the rename itself survives a crash.
    if let Ok(dir) = std::fs::File::open(root) {
        let _ = dir.sync_all();
    }
    Ok(())
}

/// Load one durable checkpoint payload, if present.
pub fn durable_load(
    root: &std::path::Path,
    label: &str,
) -> Result<Option<String>, CheckpointError> {
    let dest = checkpoint_file_path(root, label)?;
    let text = match std::fs::read_to_string(&dest) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let value: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| CheckpointError::Io(e.to_string()))?;
    Ok(value
        .get("data")
        .and_then(|v| v.as_str())
        .map(str::to_owned))
}

/// List durable checkpoint labels oldest-first (newest last).
///
/// Missing roots and unreadable entries read as empty/skipped, never as
/// errors: resume is best-effort discovery, and the caller decides what
/// a missing history means. Use `.last()` for the resume candidate.
pub fn resume_checkpoint(root: &std::path::Path) -> Vec<String> {
    let Ok(read_dir) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut labeled: Vec<(std::time::SystemTime, String)> = Vec::new();
    for entry in read_dir.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some(CHECKPOINT_FILE_EXTENSION) {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        if validate_checkpoint_label(stem).is_err() {
            continue;
        }
        // Dotfiles (staging temp files) never validate as labels, but skip
        // them explicitly so a crashed writer stays invisible.
        if stem.starts_with('.') || stem.starts_with(".staging-") {
            continue;
        }
        let modified = entry
            .metadata()
            .and_then(|m| m.modified())
            .unwrap_or(std::time::UNIX_EPOCH);
        labeled.push((modified, stem.to_owned()));
    }
    labeled.sort();
    labeled.into_iter().map(|(_, label)| label).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rollback_keeps_target_and_drops_newer() {
        let mut store = CheckpointStore::new();
        store.save("step-1", "a");
        store.save("step-2", "b");
        store.save("step-3", "c");
        let kept = store.rollback("step-2").unwrap();
        assert_eq!(kept.data, "b");
        assert_eq!(store.labels(), vec!["step-1", "step-2"]);
        assert!(store.get("step-3").is_none());
    }

    #[test]
    fn unknown_labels_fail_explicitly() {
        let mut store = CheckpointStore::new();
        assert_eq!(
            store.rollback("nope").unwrap_err(),
            CheckpointError::UnknownLabel("nope".to_string())
        );
    }
}

// File-content snapshots: labeled captures of the working tree with
// rewind support, stored outside the working directory under a
// home-scoped root (mirrors the memories `~/.wavecode/memories` layout).
//
// Layout under the store root (default `<home>/.wavecode/snapshots`):
// `<root>/<label>/manifest.json` plus `<root>/<label>/files/<rel-path>`.
// The manifest is plain JSON so slash commands can render summaries
// without depending on this module.

// Snapshots below this point use only std + tokio + serde_json: no git,
// no network, and no other workspace crate (same isolation rule as the
// [`CheckpointStore`] above).

use std::path::{Path, PathBuf};

/// Snapshot errors.
#[derive(Debug, thiserror::Error)]
pub enum SnapshotError {
    /// Invalid label, unknown label, or corrupt manifest.
    #[error("invalid input: {message}")]
    InvalidInput { message: String },
    /// Filesystem failure while capturing or rewinding.
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

/// Snapshot-specific result alias.
pub type SnapshotResult<T> = std::result::Result<T, SnapshotError>;

/// Store directory name under `<home>/.wavecode/` (mirrors the memories
/// `~/.wavecode/memories` layout).
pub const SNAPSHOTS_DIR: &str = "snapshots";
/// Manifest file name inside each snapshot directory.
pub const MANIFEST_FILE: &str = "manifest.json";
/// File payload directory inside each snapshot directory.
pub const FILES_DIR: &str = "files";

/// Capture caps (documented, enforced, and reported in every result):
/// per-file size cap (512 KB), file count cap (1000), total bytes cap (64 MB).
pub const MAX_FILE_BYTES: u64 = 512 * 1024;
/// See [`MAX_FILE_BYTES`].
pub const MAX_FILE_COUNT: usize = 1000;
/// See [`MAX_FILE_BYTES`].
pub const MAX_TOTAL_BYTES: u64 = 64 * 1024 * 1024;

/// Directories never descended into during capture (dependency trees,
/// build output, virtualenvs, and VCS metadata).
const SKIP_DIRS: &[&str] = &[".git", "target", "node_modules", ".venv"];

/// File extensions treated as binary without reading content (cheap
/// pre-filter; anything else is additionally null-byte sniffed).
const BINARY_EXTENSIONS: &[&str] = &[
    "exe", "dll", "so", "dylib", "o", "a", "lib", "class", "pyc", "pyo", "wasm", "node", "pdb",
    "bin", "dat", "db", "sqlite", "sqlite3", "png", "jpg", "jpeg", "gif", "webp", "ico", "bmp",
    "pdf", "zip", "tar", "gz", "bz2", "xz", "7z", "rar", "mp3", "mp4", "mov", "avi", "mkv", "woff",
    "woff2", "ttf", "otf", "eot",
];

/// Bytes of file head scanned for NUL when deciding binary vs text.
const SNIFF_BYTES: usize = 8192;

/// Summary lines list at most this many paths; longer lists are truncated
/// with an "and N more" tail.
const MAX_LISTED_PATHS: usize = 20;

/// Capture limits (defaults are the documented caps; tests inject smaller
/// values to exercise enforcement without writing tens of megabytes).
#[derive(Debug, Clone, Copy)]
pub struct SnapshotCaps {
    /// Per-file size cap in bytes.
    pub max_file_bytes: u64,
    /// Total file count cap.
    pub max_files: usize,
    /// Total captured bytes cap.
    pub max_total_bytes: u64,
}

impl Default for SnapshotCaps {
    fn default() -> Self {
        Self {
            max_file_bytes: MAX_FILE_BYTES,
            max_files: MAX_FILE_COUNT,
            max_total_bytes: MAX_TOTAL_BYTES,
        }
    }
}

/// User home directory, delegating to the shared [`wavecode_config::
/// home_dir`] definition (`USERPROFILE` first, `HOME` fallback).
pub fn snapshot_home_dir() -> Option<PathBuf> {
    wavecode_config::home_dir()
}

/// Default store root for production use: `<home>/.wavecode/snapshots`.
pub fn snapshot_default_root(home: &Path) -> PathBuf {
    home.join(".wavecode").join(SNAPSHOTS_DIR)
}

/// Effective store root when no explicit root is configured: the
/// home-scoped default, falling back to the system temp dir when the home
/// directory cannot be resolved (explicit, never silent cwd writes).
pub fn default_snapshot_store_root() -> PathBuf {
    snapshot_home_dir()
        .map(|h| snapshot_default_root(&h))
        .unwrap_or_else(|| std::env::temp_dir().join(format!("wavecode-{SNAPSHOTS_DIR}")))
}

/// Snapshot root for a session: derived from the session's memory store
/// root (`<home>/.wavecode/memories` -> `<home>/.wavecode/snapshots`) so
/// injected (test) roots stay hermetic; falls back to
/// [`default_snapshot_store_root`] when no memory root is configured.
pub fn snapshot_store_root_for_session(memory_store_root: Option<&Path>) -> PathBuf {
    memory_store_root
        .and_then(|p| p.parent().map(|parent| parent.join(SNAPSHOTS_DIR)))
        .unwrap_or_else(default_snapshot_store_root)
}

/// Validate a snapshot label: `[A-Za-z0-9_-]{1,64}`. Labels become a single
/// directory name, so anything else (including `/` and `..`) is rejected.
pub fn validate_snapshot_label(label: &str) -> std::result::Result<(), String> {
    if label.is_empty() || label.len() > 64 {
        return Err(format!(
            "invalid label {label:?}: expected 1-64 chars ([A-Za-z0-9_-])"
        ));
    }
    if label
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        Ok(())
    } else {
        Err(format!(
            "invalid label {label:?}: expected [A-Za-z0-9_-] only (no path separators)"
        ))
    }
}

/// Validate a manifest-relative path before writing during restore: no
/// absolute paths, no `..`, no empty segments (defense in depth against
/// hand-edited manifests).
///
/// Windows path semantics need two extra rejections there: a backslash is
/// a path separator even inside a single `/`-segment (so `..\..\x` passes
/// the split below as one benign-looking segment and the OS still resolves
/// the `..`s on open), and a drive prefix (`C:/x`) makes `PathBuf::push`
/// discard the base entirely. Both checks are scoped to Windows so legal
/// `:` / `\` characters in Unix filenames keep restoring.
fn validate_rel_path(raw: &str) -> bool {
    if raw.is_empty() || raw.starts_with('/') {
        return false;
    }
    if cfg!(windows) && (raw.contains('\\') || raw.contains(':')) {
        return false;
    }
    raw.split('/')
        .all(|seg| !seg.is_empty() && seg != "." && seg != "..")
}

/// Join a validated `/`-separated relative path onto a base directory.
fn join_rel(base: &Path, raw: &str) -> PathBuf {
    let mut path = base.to_path_buf();
    for seg in raw.split('/') {
        path.push(seg);
    }
    path
}

/// Format a `/`-separated relative path from a path stripped of the cwd prefix.
fn rel_string(path: &Path, cwd: &Path) -> Option<String> {
    path.strip_prefix(cwd).ok().map(|rel| {
        rel.components()
            .map(|c| c.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("/")
    })
}

/// Report of a snapshot capture (counts plus which caps were hit).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotCreateReport {
    /// Snapshot label.
    pub label: String,
    /// Stored text files.
    pub file_count: usize,
    /// Stored bytes total.
    pub total_bytes: u64,
    /// Files skipped as binary (extension or null-byte sniff).
    pub skipped_binary: usize,
    /// Files skipped for exceeding the per-file cap.
    pub skipped_large: usize,
    /// Files skipped for other reasons (symlinks, unreadable names).
    pub skipped_other: usize,
    /// Caps that triggered, e.g. "per-file cap 524288 bytes".
    pub caps_hit: Vec<String>,
}

impl SnapshotCreateReport {
    /// Human-readable one-block summary for tool and slash output.
    pub fn summary(&self) -> String {
        let caps = if self.caps_hit.is_empty() {
            "none".to_owned()
        } else {
            self.caps_hit.join(", ")
        };
        format!(
            "Snapshot '{}' captured: {} files ({} bytes). Skipped: {} binary, {} over per-file cap, {} other. Caps hit: {}.",
            self.label,
            self.file_count,
            self.total_bytes,
            self.skipped_binary,
            self.skipped_large,
            self.skipped_other,
            caps
        )
    }
}

/// Per-file restore outcome counts plus the affected paths.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SnapshotRestoreReport {
    /// Snapshot label.
    pub label: String,
    /// Paths written (created or overwritten).
    pub restored: Vec<String>,
    /// Files already identical to the snapshot (left untouched).
    pub unchanged: usize,
    /// Paths refused (changed after the snapshot, no force).
    pub skipped_modified: Vec<String>,
    /// Snapshot entries whose payload is missing on disk.
    pub missing: Vec<String>,
}

impl SnapshotRestoreReport {
    /// Human-readable summary; long path lists are truncated.
    pub fn summary(&self) -> String {
        let mut out = format!(
            "Snapshot '{}' rewind: {} files written, {} unchanged, {} skipped (changed after snapshot, re-run with force:true to overwrite), {} missing from snapshot.",
            self.label,
            self.restored.len(),
            self.unchanged,
            self.skipped_modified.len(),
            self.missing.len()
        );
        for (title, paths) in [
            ("written", &self.restored),
            ("skipped", &self.skipped_modified),
            ("missing", &self.missing),
        ] {
            if !paths.is_empty() {
                out.push_str(&format!("\n{title}: {}", format_snapshot_paths(paths)));
            }
        }
        out
    }
}

/// Truncate a path list for display.
fn format_snapshot_paths(paths: &[String]) -> String {
    if paths.len() <= MAX_LISTED_PATHS {
        paths.join(", ")
    } else {
        format!(
            "{}, and {} more",
            paths[..MAX_LISTED_PATHS].join(", "),
            paths.len() - MAX_LISTED_PATHS
        )
    }
}

/// Stored snapshot metadata for local display (`/snapshots`, `/rewind`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotInfo {
    /// Snapshot label.
    pub label: String,
    /// Creation time (unix seconds).
    pub created_at: u64,
    /// Stored file count.
    pub file_count: usize,
    /// Stored bytes total.
    pub total_bytes: u64,
    /// Caps that triggered during capture.
    pub caps_hit: Vec<String>,
    /// Stored relative paths (manifest order).
    pub files: Vec<String>,
}

impl SnapshotInfo {
    /// Short multi-line rendering for slash output.
    pub fn display(&self) -> String {
        let caps = if self.caps_hit.is_empty() {
            "none".to_owned()
        } else {
            self.caps_hit.join(", ")
        };
        let mut out = format!(
            "Snapshot '{}': {} files ({} bytes), captured at unix {}. Caps hit: {}.",
            self.label, self.file_count, self.total_bytes, self.created_at, caps
        );
        if !self.files.is_empty() {
            out.push_str(&format!("\nfiles: {}", format_snapshot_paths(&self.files)));
        }
        out
    }
}

/// File-content snapshot store: labeled captures live outside the working
/// directory under a home-scoped root, so rewind never depends on git.
#[derive(Debug, Clone)]
pub struct SnapshotStore {
    root: PathBuf,
}

impl SnapshotStore {
    /// Build with an explicit root (tests inject a tempdir; production
    /// uses [`default_snapshot_store_root`] or
    /// [`snapshot_store_root_for_session`]).
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    /// Store root.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Default root for a home directory: `<home>/.wavecode/snapshots`.
    pub fn default_root(home: &Path) -> PathBuf {
        snapshot_default_root(home)
    }

    /// Directory holding one snapshot's payload.
    fn snapshot_dir(&self, label: &str) -> PathBuf {
        self.root.join(label)
    }

    /// Capture the working tree into `label` (overwrites the label on
    /// re-capture). Walks `cwd` directly, so no git presence is required.
    pub async fn create(&self, cwd: &Path, label: &str) -> SnapshotResult<SnapshotCreateReport> {
        self.create_with_caps(cwd, label, SnapshotCaps::default())
            .await
    }

    /// [`SnapshotStore::create`] with injectable caps (cap enforcement
    /// tests use tiny caps; production always passes
    /// [`SnapshotCaps::default`]).
    pub async fn create_with_caps(
        &self,
        cwd: &Path,
        label: &str,
        caps: SnapshotCaps,
    ) -> SnapshotResult<SnapshotCreateReport> {
        validate_snapshot_label(label)
            .map_err(|message| SnapshotError::InvalidInput { message })?;
        let mut collected: Vec<(String, Vec<u8>)> = Vec::new();
        let mut total_bytes: u64 = 0;
        let mut skipped_binary = 0usize;
        let mut skipped_large = 0usize;
        let mut skipped_other = 0usize;
        let mut caps_hit: Vec<String> = Vec::new();

        // Iterative depth-first walk (sorted per directory for determinism).
        let mut stack = vec![cwd.to_path_buf()];
        'walk: while let Some(dir) = stack.pop() {
            let mut entries = Vec::new();
            let mut read_dir = tokio::fs::read_dir(&dir).await?;
            while let Some(entry) = read_dir.next_entry().await? {
                entries.push(entry);
            }
            entries.sort_by_key(|e| e.file_name());
            for entry in entries {
                // Never capture the store itself when it sits under cwd.
                if entry.path() == self.root {
                    continue;
                }
                let file_type = entry.file_type().await?;
                if file_type.is_symlink() {
                    skipped_other += 1;
                    continue;
                }
                if file_type.is_dir() {
                    if entry
                        .file_name()
                        .to_str()
                        .is_some_and(|n| SKIP_DIRS.contains(&n))
                    {
                        continue;
                    }
                    stack.push(entry.path());
                    continue;
                }
                if !file_type.is_file() {
                    skipped_other += 1;
                    continue;
                }
                let Some(rel) = rel_string(&entry.path(), cwd) else {
                    skipped_other += 1;
                    continue;
                };
                if collected.len() >= caps.max_files {
                    caps_hit.push(format!("file count cap {}", caps.max_files));
                    break 'walk;
                }
                let len = entry.metadata().await?.len();
                if len > caps.max_file_bytes {
                    skipped_large += 1;
                    if !caps_hit.iter().any(|c| c.starts_with("per-file cap")) {
                        caps_hit.push(format!("per-file cap {} bytes", caps.max_file_bytes));
                    }
                    continue;
                }
                if total_bytes.saturating_add(len) > caps.max_total_bytes {
                    caps_hit.push(format!("total size cap {} bytes", caps.max_total_bytes));
                    break 'walk;
                }
                if entry
                    .path()
                    .extension()
                    .and_then(|e| e.to_str())
                    .is_some_and(|ext| {
                        BINARY_EXTENSIONS
                            .iter()
                            .any(|b| b.eq_ignore_ascii_case(ext))
                    })
                {
                    skipped_binary += 1;
                    continue;
                }
                let bytes = tokio::fs::read(entry.path()).await?;
                if bytes.len() as u64 > caps.max_file_bytes {
                    skipped_large += 1;
                    if !caps_hit.iter().any(|c| c.starts_with("per-file cap")) {
                        caps_hit.push(format!("per-file cap {} bytes", caps.max_file_bytes));
                    }
                    continue;
                }
                if bytes.iter().take(SNIFF_BYTES).any(|b| *b == 0) {
                    skipped_binary += 1;
                    continue;
                }
                total_bytes = total_bytes.saturating_add(bytes.len() as u64);
                collected.push((rel, bytes));
            }
        }

        // Stage then rename so a failed capture never leaves a half label.
        let staging = self.root.join(format!(".staging-{label}"));
        let _ = tokio::fs::remove_dir_all(&staging).await;
        tokio::fs::create_dir_all(staging.join(FILES_DIR)).await?;
        for (rel, bytes) in &collected {
            let dest = join_rel(&staging.join(FILES_DIR), rel);
            if let Some(parent) = dest.parent() {
                tokio::fs::create_dir_all(parent).await?;
            }
            tokio::fs::write(&dest, bytes).await?;
        }
        let created_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let manifest = serde_json::json!({
            "label": label,
            "created_at": created_at,
            "file_count": collected.len(),
            "total_bytes": total_bytes,
            "skipped_binary": skipped_binary,
            "skipped_large": skipped_large,
            "skipped_other": skipped_other,
            "caps_hit": caps_hit,
            "files": collected.iter().map(|(rel, bytes)| serde_json::json!({
                "path": rel,
                "size": bytes.len(),
            })).collect::<Vec<_>>(),
        });
        tokio::fs::write(
            staging.join(MANIFEST_FILE),
            serde_json::to_string_pretty(&manifest).unwrap_or_else(|_| "{}".to_owned()),
        )
        .await?;
        tokio::fs::create_dir_all(&self.root).await?;
        let _ = tokio::fs::remove_dir_all(self.snapshot_dir(label)).await;
        tokio::fs::rename(&staging, self.snapshot_dir(label)).await?;

        Ok(SnapshotCreateReport {
            label: label.to_owned(),
            file_count: collected.len(),
            total_bytes,
            skipped_binary,
            skipped_large,
            skipped_other,
            caps_hit,
        })
    }

    /// Rewind `cwd` files from `label`. Existing files whose content differs
    /// from the snapshot are only overwritten with `force` (they were
    /// modified after the snapshot); missing files are recreated and
    /// identical files are left untouched. Files created after the snapshot
    /// (not in the manifest) are left alone.
    pub async fn restore(
        &self,
        cwd: &Path,
        label: &str,
        force: bool,
    ) -> SnapshotResult<SnapshotRestoreReport> {
        validate_snapshot_label(label)
            .map_err(|message| SnapshotError::InvalidInput { message })?;
        let dir = self.snapshot_dir(label);
        let manifest_text = match tokio::fs::read_to_string(dir.join(MANIFEST_FILE)).await {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(SnapshotError::InvalidInput {
                    message: format!("unknown snapshot label {label:?}"),
                });
            }
            Err(e) => return Err(e.into()),
        };
        let manifest: serde_json::Value =
            serde_json::from_str(&manifest_text).map_err(|_| SnapshotError::InvalidInput {
                message: format!("snapshot {label:?} manifest is corrupt"),
            })?;
        let entries = manifest
            .get("files")
            .and_then(serde_json::Value::as_array)
            .cloned()
            .unwrap_or_default();

        let mut report = SnapshotRestoreReport {
            label: label.to_owned(),
            ..Default::default()
        };
        for entry in entries {
            let Some(rel) = entry.get("path").and_then(serde_json::Value::as_str) else {
                continue;
            };
            if !validate_rel_path(rel) {
                report.missing.push(format!("{rel} (unsafe path, skipped)"));
                continue;
            }
            let stored = match tokio::fs::read(join_rel(&dir.join(FILES_DIR), rel)).await {
                Ok(bytes) => bytes,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    report.missing.push(rel.to_owned());
                    continue;
                }
                Err(e) => return Err(e.into()),
            };
            let dest = join_rel(cwd, rel);
            match tokio::fs::read(&dest).await {
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    if let Some(parent) = dest.parent() {
                        tokio::fs::create_dir_all(parent).await?;
                    }
                    tokio::fs::write(&dest, &stored).await?;
                    report.restored.push(rel.to_owned());
                }
                Err(e) => return Err(e.into()),
                Ok(current) => {
                    if current == stored {
                        report.unchanged += 1;
                        continue;
                    }
                    if !force {
                        report.skipped_modified.push(rel.to_owned());
                        continue;
                    }
                    if let Some(parent) = dest.parent() {
                        tokio::fs::create_dir_all(parent).await?;
                    }
                    tokio::fs::write(&dest, &stored).await?;
                    report.restored.push(rel.to_owned());
                }
            }
        }
        Ok(report)
    }

    /// Sorted snapshot labels (missing root reads as empty, never an error).
    pub fn list_labels(&self) -> Vec<String> {
        let mut labels = Vec::new();
        let Ok(read_dir) = std::fs::read_dir(&self.root) else {
            return labels;
        };
        for entry in read_dir.flatten() {
            if !entry.file_type().is_ok_and(|t| t.is_dir()) {
                continue;
            }
            if let Some(name) = entry.file_name().to_str()
                && !name.starts_with(".staging-")
                && validate_snapshot_label(name).is_ok()
            {
                labels.push(name.to_owned());
            }
        }
        labels.sort();
        labels
    }

    /// Delete a label; returns true when a snapshot was removed.
    pub fn drop_label(&self, label: &str) -> SnapshotResult<bool> {
        validate_snapshot_label(label)
            .map_err(|message| SnapshotError::InvalidInput { message })?;
        match std::fs::remove_dir_all(self.snapshot_dir(label)) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e.into()),
        }
    }

    /// Load stored metadata for local display (slash commands).
    pub fn load_info(&self, label: &str) -> SnapshotResult<SnapshotInfo> {
        validate_snapshot_label(label)
            .map_err(|message| SnapshotError::InvalidInput { message })?;
        let text =
            std::fs::read_to_string(self.snapshot_dir(label).join(MANIFEST_FILE)).map_err(|e| {
                if e.kind() == std::io::ErrorKind::NotFound {
                    SnapshotError::InvalidInput {
                        message: format!("unknown snapshot label {label:?}"),
                    }
                } else {
                    SnapshotError::Io(e)
                }
            })?;
        let manifest: serde_json::Value =
            serde_json::from_str(&text).map_err(|_| SnapshotError::InvalidInput {
                message: format!("snapshot {label:?} manifest is corrupt"),
            })?;
        Ok(SnapshotInfo {
            label: label.to_owned(),
            created_at: manifest
                .get("created_at")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0),
            file_count: manifest
                .get("file_count")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0) as usize,
            total_bytes: manifest
                .get("total_bytes")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0),
            caps_hit: manifest
                .get("caps_hit")
                .and_then(serde_json::Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(serde_json::Value::as_str)
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default(),
            files: manifest
                .get("files")
                .and_then(serde_json::Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|e| e.get("path").and_then(serde_json::Value::as_str))
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default(),
        })
    }
}

#[cfg(test)]
mod snapshot_tests {
    use super::*;

    fn write_file(path: &Path, content: &[u8]) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, content).unwrap();
    }

    /// Label rules: word chars and dashes only, bounded length, no traversal.
    #[test]
    fn label_validation() {
        assert!(validate_snapshot_label("before-refactor").is_ok());
        assert!(validate_snapshot_label("a1_-B2").is_ok());
        assert!(validate_snapshot_label("").is_err());
        assert!(validate_snapshot_label(&"x".repeat(65)).is_err());
        assert!(validate_snapshot_label("../evil").is_err());
        assert!(validate_snapshot_label("a/b").is_err());
        assert!(validate_snapshot_label("has space").is_err());
        assert!(
            validate_snapshot_label(".staging-x").is_err(),
            "dots are rejected"
        );
    }

    /// Manifest paths cannot escape the restore root through Windows path
    /// semantics: a backslash acts as a separator inside a `/`-segment (so
    /// `..\..\x` hides a traversal from the forward-slash split) and a drive
    /// prefix makes `PathBuf::push` discard the base. Both are rejected on
    /// Windows; on other platforms `:` / `\` are legal filename characters,
    /// so those exact strings stay accepted there. Forward-slash traversal
    /// and absolute paths are rejected everywhere.
    #[test]
    fn manifest_paths_reject_windows_separator_tricks() {
        for raw in ["..\\..\\evil.txt", "C:/evil.txt", "sub\\..\\..\\evil.txt"] {
            assert_eq!(
                validate_rel_path(raw),
                !cfg!(windows),
                "{raw}: must be rejected exactly where it is a path escape"
            );
        }
        for raw in [
            "../evil.txt",
            "a/../../evil.txt",
            "/abs/evil.txt",
            "",
            "a//b",
            "./x",
        ] {
            assert!(!validate_rel_path(raw), "{raw}: must be rejected");
        }
        assert!(validate_rel_path("a/b.txt"));
    }

    /// Round trip: capture, modify, restore refuses without force and
    /// rewinds with force; deleted files come back; identical files stay.
    #[tokio::test]
    async fn round_trip_with_modified_after_refusal() {
        let cwd = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let store = SnapshotStore::new(root.path().to_path_buf());
        write_file(&cwd.path().join("a.txt"), b"original");
        write_file(&cwd.path().join("sub/b.txt"), b"sub original");

        let report = store.create(cwd.path(), "v1").await.unwrap();
        assert_eq!(report.file_count, 2);
        assert!(report.caps_hit.is_empty());

        // Modify one file, delete the other, add an untracked new file.
        write_file(&cwd.path().join("a.txt"), b"modified");
        std::fs::remove_file(cwd.path().join("sub/b.txt")).unwrap();
        write_file(&cwd.path().join("new.txt"), b"after snapshot");

        let refused = store.restore(cwd.path(), "v1", false).await.unwrap();
        assert_eq!(refused.restored, vec!["sub/b.txt".to_owned()]);
        assert_eq!(refused.skipped_modified, vec!["a.txt".to_owned()]);
        assert_eq!(refused.unchanged, 0);
        assert_eq!(
            std::fs::read(cwd.path().join("a.txt")).unwrap(),
            b"modified",
            "refused file keeps its newer content"
        );
        assert_eq!(
            std::fs::read(cwd.path().join("new.txt")).unwrap(),
            b"after snapshot",
            "files created after the snapshot are left alone"
        );

        let forced = store.restore(cwd.path(), "v1", true).await.unwrap();
        assert!(forced.skipped_modified.is_empty());
        assert!(forced.restored.contains(&"a.txt".to_owned()));
        assert_eq!(
            std::fs::read(cwd.path().join("a.txt")).unwrap(),
            b"original"
        );
    }

    /// Tiny caps exercise every enforcement branch without heavy IO.
    #[tokio::test]
    async fn caps_enforced() {
        let cwd = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let store = SnapshotStore::new(root.path().to_path_buf());
        write_file(&cwd.path().join("small.txt"), b"12345");
        write_file(&cwd.path().join("big.txt"), b"0123456789abcdef");
        write_file(&cwd.path().join("third.txt"), b"z");
        let caps = SnapshotCaps {
            max_file_bytes: 8,
            max_files: 2,
            max_total_bytes: 5,
        };
        let report = store
            .create_with_caps(cwd.path(), "capped", caps)
            .await
            .unwrap();
        // Sorted walk: big.txt hits the per-file cap, small.txt fills the
        // total cap, third.txt never fits.
        assert_eq!(report.skipped_large, 1);
        assert_eq!(report.file_count, 1);
        assert_eq!(report.total_bytes, 5);
        assert_eq!(report.caps_hit.len(), 2);
        assert_eq!(
            store.load_info("capped").unwrap().files,
            vec!["small.txt".to_owned()]
        );
    }

    /// Documented default caps keep their specified values.
    #[test]
    fn default_caps_match_documentation() {
        assert_eq!(MAX_FILE_BYTES, 512 * 1024);
        assert_eq!(MAX_FILE_COUNT, 1000);
        assert_eq!(MAX_TOTAL_BYTES, 64 * 1024 * 1024);
        let caps = SnapshotCaps::default();
        assert_eq!(caps.max_file_bytes, MAX_FILE_BYTES);
        assert_eq!(caps.max_files, MAX_FILE_COUNT);
        assert_eq!(caps.max_total_bytes, MAX_TOTAL_BYTES);
    }

    /// Binaries (by extension and by null-byte sniff) and skipped
    /// directories never enter the snapshot.
    #[tokio::test]
    async fn binaries_and_skipped_dirs_excluded() {
        let cwd = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let store = SnapshotStore::new(root.path().to_path_buf());
        write_file(&cwd.path().join("keep.txt"), b"text");
        write_file(&cwd.path().join("img.png"), b"fakepng");
        write_file(&cwd.path().join("blob.dat"), b"has\0nul");
        write_file(&cwd.path().join("noext"), b"plain\0binary");
        write_file(&cwd.path().join("target/out.txt"), b"build");
        write_file(&cwd.path().join(".git/config"), b"git");
        write_file(&cwd.path().join("node_modules/x.js"), b"dep");

        let report = store.create(cwd.path(), "mixed").await.unwrap();
        assert_eq!(report.file_count, 1);
        assert_eq!(report.skipped_binary, 3);
        let info = store.load_info("mixed").unwrap();
        assert_eq!(info.files, vec!["keep.txt".to_owned()]);
        assert!(report.summary().contains("Caps hit: none"));
    }

    /// Labels list, drop, and unknown-label errors.
    #[tokio::test]
    async fn label_lifecycle_and_unknown_labels() {
        let cwd = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let store = SnapshotStore::new(root.path().to_path_buf());
        assert!(store.list_labels().is_empty());
        write_file(&cwd.path().join("a.txt"), b"a");
        store.create(cwd.path(), "one").await.unwrap();
        store.create(cwd.path(), "two").await.unwrap();
        assert_eq!(
            store.list_labels(),
            vec!["one".to_owned(), "two".to_owned()]
        );
        assert!(store.drop_label("one").unwrap());
        assert!(!store.drop_label("one").unwrap());
        assert_eq!(store.list_labels(), vec!["two".to_owned()]);

        let err = store
            .restore(cwd.path(), "missing", false)
            .await
            .unwrap_err();
        assert!(
            matches!(err, SnapshotError::InvalidInput { .. }),
            "unknown labels fail explicitly: {err}"
        );
        assert!(err.to_string().contains("unknown snapshot"));
    }

    /// Session roots derive from the memory root; the default root is
    /// home-scoped and never the cwd.
    #[test]
    fn session_roots_follow_memory_layout() {
        let home = PathBuf::from("/tmp/home-test");
        let memory_root = home.join(".wavecode").join("memories");
        assert_eq!(
            snapshot_store_root_for_session(Some(&memory_root)),
            home.join(".wavecode").join("snapshots")
        );
        assert_eq!(
            snapshot_default_root(&home),
            home.join(".wavecode").join("snapshots")
        );
    }
}

#[cfg(test)]
mod durability_tests {
    use super::*;

    #[test]
    fn policy_defaults_fail_closed() {
        let policy = CheckpointPolicy::default();
        assert!(policy.checkpoint_before_model_request);
        assert!(policy.before_tool_side_effect);
        assert!(policy.anything_enabled());
        let off = CheckpointPolicy::disabled();
        assert!(!off.checkpoint_before_model_request);
        assert!(!off.before_tool_side_effect);
        assert!(!off.anything_enabled());
    }

    #[test]
    fn durable_round_trip_with_atomic_files() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("checkpoints");
        // Missing roots read as empty, never as errors.
        assert!(resume_checkpoint(&dir).is_empty());
        durable_save(&dir, "turn-1", "state-one").unwrap();
        durable_save(&dir, "turn-2", "state-two").unwrap();
        assert_eq!(
            durable_load(&dir, "turn-1").unwrap().as_deref(),
            Some("state-one")
        );
        assert_eq!(durable_load(&dir, "missing").unwrap(), None);
        // Oldest first, newest last: the tail is the resume candidate.
        let labels = resume_checkpoint(&dir);
        assert_eq!(labels, vec!["turn-1".to_owned(), "turn-2".to_owned()]);
        // No staging temp files leak into the listing or the directory.
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with(".staging-"))
            .collect();
        assert!(leftovers.is_empty());
    }

    #[test]
    fn labels_reject_path_escapes() {
        let root = tempfile::tempdir().unwrap();
        assert!(matches!(
            durable_save(root.path(), "../evil", "x").unwrap_err(),
            CheckpointError::InvalidLabel(_)
        ));
        assert!(matches!(
            durable_save(root.path(), "a/b", "x").unwrap_err(),
            CheckpointError::InvalidLabel(_)
        ));
        assert!(validate_checkpoint_label("turn-12_ok").is_ok());
        // Rejected labels never touch the filesystem.
        assert!(resume_checkpoint(root.path()).is_empty());
    }

    #[test]
    fn store_api_stays_backward_compatible() {
        // Pre-existing behavior is unchanged by the additive variants.
        let mut store = CheckpointStore::new();
        assert_eq!(store.save("step-1", "a"), 1);
        assert_eq!(store.get("step-1").unwrap().data, "a");
        assert_eq!(store.labels(), vec!["step-1"]);
        assert_eq!(
            store.rollback("nope").unwrap_err(),
            CheckpointError::UnknownLabel("nope".to_string())
        );
    }
}
