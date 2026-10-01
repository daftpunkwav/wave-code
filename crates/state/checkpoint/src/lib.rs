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
    // Opening the directory handle is structurally unavailable on some
    // platforms (Windows), so that skip stays silent; a failure *after*
    // the handle is open is a real durability signal and must surface.
    if let Ok(dir) = std::fs::File::open(root) {
        if let Err(error) = dir.sync_all() {
            tracing::warn!(
                "checkpoint directory fsync failed; the rename may not survive a crash: {error}"
            );
        }
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
/// Equal mtimes (saves landing within one timestamp tick) order by the
/// label's trailing number, so `turn-10` stays newer than `turn-2`.
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
    labeled.sort_by(|a, b| {
        (a.0, label_order_key(&a.1)).cmp(&(b.0, label_order_key(&b.1)))
    });
    labeled.into_iter().map(|(_, label)| label).collect()
}

/// Sort key for one label: the part before a trailing digit run, that run
/// as a number, then the label itself. Plain string order would place
/// `turn-10` between `turn-1` and `turn-2`, naming the wrong checkpoint
/// as the newest whenever two saves share one mtime tick. The final whole
/// label component keeps the order total for stems the numeric tail does
/// not distinguish. Labels reaching this function are validated ASCII
/// (`[A-Za-z0-9_-]`), so the byte split is always a char boundary.
fn label_order_key(label: &str) -> (&str, u64, &str) {
    let split = label
        .bytes()
        .rposition(|b| !b.is_ascii_digit())
        .map_or(0, |i| i + 1);
    let (prefix, tail) = label.split_at(split);
    (prefix, tail.parse().unwrap_or(u64::MAX), label)
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

// File-content snapshots live in their own module; the re-export keeps
// every crate-facing path (`state_checkpoint::SnapshotStore`, ...) unchanged.
mod snapshot;

pub use snapshot::{
    FILES_DIR, MANIFEST_FILE, MAX_FILE_BYTES, MAX_FILE_COUNT, MAX_TOTAL_BYTES, SNAPSHOTS_DIR,
    SnapshotCaps, SnapshotCreateReport, SnapshotError, SnapshotInfo, SnapshotRestoreReport,
    SnapshotResult, SnapshotStore, default_snapshot_store_root, snapshot_default_root,
    snapshot_home_dir, snapshot_store_root_for_session, validate_snapshot_label,
};

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

    /// Same-tick mtimes must not let string order invert the numeric
    /// sequence: with one shared timestamp, plain label order reads
    /// `turn-10` as older than `turn-2` and resume would name the wrong
    /// checkpoint as newest. The trailing number breaks the tie.
    #[test]
    fn equal_mtimes_order_labels_numerically() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("checkpoints");
        for label in ["turn-2", "turn-10", "turn-1"] {
            durable_save(&root, label, "x").unwrap();
        }
        // Force one shared mtime: real saves can land within one
        // filesystem timestamp tick.
        let shared = std::time::SystemTime::now();
        for entry in std::fs::read_dir(&root).unwrap().flatten() {
            std::fs::File::options()
                .write(true)
                .open(entry.path())
                .unwrap()
                .set_modified(shared)
                .unwrap();
        }
        assert_eq!(
            resume_checkpoint(&root),
            vec![
                "turn-1".to_owned(),
                "turn-2".to_owned(),
                "turn-10".to_owned()
            ]
        );
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
