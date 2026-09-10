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
