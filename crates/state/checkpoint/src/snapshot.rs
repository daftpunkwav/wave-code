//! File-content snapshots: labeled captures of the working tree with
//! rewind support, stored outside the working directory under a
//! home-scoped root (mirrors the memories `~/.wavecode/memories` layout).
//!
//! Layout under the store root (default `<home>/.wavecode/snapshots`):
//! `<root>/<label>/manifest.json` plus `<root>/<label>/files/<rel-path>`.
//! The manifest is plain JSON so slash commands can render summaries
//! without depending on this module.
//!
//! Snapshots here use only std + tokio + serde_json: no git, no network,
//! and no other workspace crate (same isolation rule as the `CheckpointStore`
//! in the crate root).

use std::path::{Path, PathBuf};

use tokio::io::AsyncReadExt as _;

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

/// Default store root under a home directory: `<home>/.wavecode/snapshots`.
/// Pure path derivation; see [`effective_snapshot_store_root`] for the
/// root that production actually uses when nothing is configured.
pub fn default_snapshot_root(home: &Path) -> PathBuf {
    home.join(".wavecode").join(SNAPSHOTS_DIR)
}

/// Effective store root when no explicit root is configured: the
/// home-scoped default, falling back to the system temp dir when the home
/// directory cannot be resolved (explicit, never silent cwd writes).
pub fn effective_snapshot_store_root() -> PathBuf {
    snapshot_home_dir()
        .map(|h| default_snapshot_root(&h))
        .unwrap_or_else(|| std::env::temp_dir().join(format!("wavecode-{SNAPSHOTS_DIR}")))
}

/// Snapshot root for a session: derived from the session's memory store
/// root (`<home>/.wavecode/memories` -> `<home>/.wavecode/snapshots`) so
/// injected (test) roots stay hermetic; falls back to
/// [`effective_snapshot_store_root`] when no memory root is configured.
pub fn snapshot_store_root_for_session(memory_store_root: Option<&Path>) -> PathBuf {
    memory_store_root
        .and_then(|p| p.parent().map(|parent| parent.join(SNAPSHOTS_DIR)))
        .unwrap_or_else(effective_snapshot_store_root)
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
/// What one capture walk observed: the collected files plus the skip and
/// cap counters the manifest and report surface. Crate-internal — the
/// public view is [`SnapshotCreateReport`].
#[derive(Debug, Default)]
struct SnapshotWalk {
    /// Collected files as `(relative path, bytes)`.
    collected: Vec<(String, Vec<u8>)>,
    /// Stored bytes total.
    total_bytes: u64,
    /// Files skipped as binary (extension or null-byte sniff).
    skipped_binary: usize,
    /// Files skipped for exceeding the per-file cap.
    skipped_large: usize,
    /// Files skipped for other reasons (symlinks, unreadable names).
    skipped_other: usize,
    /// Caps that triggered, e.g. "per-file cap 524288 bytes".
    caps_hit: Vec<String>,
}

impl SnapshotWalk {
    /// Record a per-file cap hit once; repeat hits only bump the counter.
    fn note_per_file_cap(&mut self, caps: &SnapshotCaps) {
        self.skipped_large += 1;
        if !self.caps_hit.iter().any(|c| c.starts_with("per-file cap")) {
            self.caps_hit
                .push(format!("per-file cap {} bytes", caps.max_file_bytes));
        }
    }
}

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
    /// uses [`effective_snapshot_store_root`] or
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
        default_snapshot_root(home)
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
        let walk = self.walk_snapshot_files(cwd, caps).await?;

        // Stage then rename so a failed capture never leaves a half label.
        let staging = self.root.join(format!(".staging-{label}"));
        let _ = tokio::fs::remove_dir_all(&staging).await;
        tokio::fs::create_dir_all(staging.join(FILES_DIR)).await?;
        for (rel, bytes) in &walk.collected {
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
            "file_count": walk.collected.len(),
            "total_bytes": walk.total_bytes,
            "skipped_binary": walk.skipped_binary,
            "skipped_large": walk.skipped_large,
            "skipped_other": walk.skipped_other,
            "caps_hit": walk.caps_hit,
            "files": walk.collected.iter().map(|(rel, bytes)| serde_json::json!({
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
        // Swap, never gap: the old snapshot moves aside before the staging
        // directory takes the label, so a crash between the two renames —
        // or a Windows rename-over-existing-directory failure — can never
        // leave the label without a recovery point.
        let final_dir = self.snapshot_dir(label);
        // The previous snapshot, parked aside while the swap runs: it is
        // restored if the staging rename fails, and discarded otherwise.
        let previous = self.root.join(format!("{label}.old"));
        let _ = tokio::fs::remove_dir_all(&previous).await;
        if tokio::fs::rename(&final_dir, &previous).await.is_ok() {
            if let Err(e) = tokio::fs::rename(&staging, &final_dir).await {
                // Keep the old snapshot: restore it and surface the failure.
                let _ = tokio::fs::rename(&previous, &final_dir).await;
                let _ = tokio::fs::remove_dir_all(&previous).await;
                return Err(e.into());
            }
            let _ = tokio::fs::remove_dir_all(&previous).await;
        } else {
            // First save under this label: nothing to protect.
            tokio::fs::rename(&staging, &final_dir).await?;
        }

        Ok(SnapshotCreateReport {
            label: label.to_owned(),
            file_count: walk.collected.len(),
            total_bytes: walk.total_bytes,
            skipped_binary: walk.skipped_binary,
            skipped_large: walk.skipped_large,
            skipped_other: walk.skipped_other,
            caps_hit: walk.caps_hit,
        })
    }

    /// Iterative depth-first walk of `cwd` (sorted per directory for
    /// determinism) collecting snapshot-eligible files under `caps`.
    async fn walk_snapshot_files(
        &self,
        cwd: &Path,
        caps: SnapshotCaps,
    ) -> SnapshotResult<SnapshotWalk> {
        let mut walk = SnapshotWalk::default();

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
                    walk.skipped_other += 1;
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
                    walk.skipped_other += 1;
                    continue;
                }
                let Some(rel) = rel_string(&entry.path(), cwd) else {
                    walk.skipped_other += 1;
                    continue;
                };
                if walk.collected.len() >= caps.max_files {
                    walk.caps_hit
                        .push(format!("file count cap {}", caps.max_files));
                    break 'walk;
                }
                let len = entry.metadata().await?.len();
                if len > caps.max_file_bytes {
                    walk.note_per_file_cap(&caps);
                    continue;
                }
                if walk.total_bytes.saturating_add(len) > caps.max_total_bytes {
                    walk.caps_hit
                        .push(format!("total size cap {} bytes", caps.max_total_bytes));
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
                    walk.skipped_binary += 1;
                    continue;
                }
                // Bounded read: the cap must constrain the allocation, not
                // just the verdict — a file that grows between the stat and
                // the read (an active log, a sparse file) streams through a
                // `take` window instead of being materialized in full.
                let file = tokio::fs::File::open(entry.path()).await?;
                let mut limited = file.take(caps.max_file_bytes.saturating_add(1));
                let mut bytes = Vec::new();
                limited.read_to_end(&mut bytes).await?;
                if bytes.len() as u64 > caps.max_file_bytes {
                    walk.note_per_file_cap(&caps);
                    continue;
                }
                if bytes.iter().take(SNIFF_BYTES).any(|b| *b == 0) {
                    walk.skipped_binary += 1;
                    continue;
                }
                walk.total_bytes = walk.total_bytes.saturating_add(bytes.len() as u64);
                walk.collected.push((rel, bytes));
            }
        }
        Ok(walk)
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
            default_snapshot_root(&home),
            home.join(".wavecode").join("snapshots")
        );
    }

    // Home variables are process-global state; tests mutating them run
    // mutually exclusive and restore the originals before asserting.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// With the home unresolvable (neither USERPROFILE nor HOME set — the
    /// USERPROFILE-first order matches [`wavecode_config::home_dir`]), the
    /// store root falls back explicitly to the system temp dir. The
    /// contract is "explicit fallback, never a silent relative path": a
    /// cwd- or relative-looking fallback would scatter snapshot stores
    /// into whatever directory the user happened to launch from.
    #[test]
    fn unresolved_home_falls_back_to_temp_dir_not_a_relative_path() {
        let _guard = ENV_LOCK.lock().unwrap();
        let saved_userprofile = std::env::var_os("USERPROFILE");
        let saved_home = std::env::var_os("HOME");
        unsafe {
            std::env::remove_var("USERPROFILE");
            std::env::remove_var("HOME");
        }
        let fallback = effective_snapshot_store_root();
        // Restore before asserting so a failure cannot leak the mutation.
        unsafe {
            if let Some(value) = saved_userprofile {
                std::env::set_var("USERPROFILE", value);
            }
            if let Some(value) = saved_home {
                std::env::set_var("HOME", value);
            }
        }
        assert_eq!(
            fallback,
            std::env::temp_dir().join(format!("wavecode-{SNAPSHOTS_DIR}")),
            "the unresolvable-home fallback must be the documented temp-dir root"
        );
        assert!(
            fallback.is_absolute(),
            "the fallback must stay an explicit absolute path, never cwd-relative: {fallback:?}"
        );
    }
}
