/*!
 * @file TurnDurability
 * @description Persist-then-act checkpoints for turn-driving paths.
 *
 * Responsibilities:
 * - Save an in-memory plus fsynced file checkpoint before turns act.
 * - Short-circuit with zero IO when the durability policy is off.
 * - Surface durable labels for resume-from-checkpoint on startup.
 *
 * This module must not depend on: tools, policy, hooks, models, memory,
 * skills, frontends, or any concrete capability implementation.
 */

//! Persist-then-act: every turn-driving path checkpoints before it acts.
//!
//! Ordering is the whole point: `store.save` plus the fsynced file write
//! land before the driver broadcasts or executes anything, so a crash
//! mid-turn resumes from the pre-turn snapshot, never from a half-act.
//! A disabled policy flag short-circuits with zero IO.

use std::path::{Path, PathBuf};

pub use state_checkpoint::resume_checkpoint;
use state_checkpoint::{CheckpointError, CheckpointPolicy, CheckpointStore};

/// Sink seam for the in-memory half of persist-then-act.
///
/// [`CheckpointStore`] implements this directly; tests inject a fake
/// recorder to assert that the save precedes the faked act.
pub trait CheckpointSink {
    /// Save one snapshot, returning its sequence number.
    fn save_checkpoint(&mut self, label: &str, data: &str) -> u64;
}

impl CheckpointSink for CheckpointStore {
    fn save_checkpoint(&mut self, label: &str, data: &str) -> u64 {
        self.save(label, data)
    }
}

/// Durability configuration for [`crate::SessionActor::spawn_with_durability`].
#[derive(Debug)]
pub struct DurabilityConfig {
    /// In-memory snapshots, newest-wins rollback.
    pub store: CheckpointStore,
    /// Directory holding the fsynced `<label>.json` copies.
    pub root: PathBuf,
    /// Which turn-driving hook points checkpoint.
    pub policy: CheckpointPolicy,
}

/// Durability configuration carried by the session actor.
///
/// `None` (the plain `spawn` path) means no checkpointing at all;
/// `Some` checkpoints at the policy's hook points with `turn-N` labels.
#[derive(Debug)]
pub(crate) struct TurnDurability {
    /// In-memory snapshots, newest-wins rollback.
    pub store: CheckpointStore,
    /// Directory holding the fsynced `<label>.json` copies.
    pub root: PathBuf,
    /// Which turn-driving hook points checkpoint.
    pub policy: CheckpointPolicy,
}

impl From<DurabilityConfig> for TurnDurability {
    fn from(config: DurabilityConfig) -> Self {
        Self {
            store: config.store,
            root: config.root,
            policy: config.policy,
        }
    }
}

/// Persist one checkpoint: in-memory save first, fsynced file second.
///
/// Returns `None` without touching the sink or the filesystem when
/// `enabled` is false (policy off short-circuits with zero IO).
pub fn persist_checkpoint<S: CheckpointSink>(
    sink: &mut S,
    root: &Path,
    enabled: bool,
    label: &str,
    snapshot: &str,
) -> Option<Result<(), CheckpointError>> {
    if !enabled {
        return None;
    }
    sink.save_checkpoint(label, snapshot);
    Some(state_checkpoint::durable_save(root, label, snapshot))
}

/// Persist-then-act: the durable write lands BEFORE the act closure runs.
///
/// Returns the act's output plus the durable outcome (`None` when the
/// policy flag was off). Durable failures are reported, never thrown
/// away: the caller decides whether the turn still proceeds.
pub fn persist_then_act<S, F, R>(
    sink: &mut S,
    root: &Path,
    enabled: bool,
    label: &str,
    snapshot: &str,
    act: F,
) -> (R, Option<Result<(), CheckpointError>>)
where
    S: CheckpointSink,
    F: FnOnce() -> R,
{
    let durable = persist_checkpoint(sink, root, enabled, label, snapshot);
    (act(), durable)
}

/// Label for the Nth turn checkpoint (1-based): `turn-N`.
pub fn turn_label(seq: u64) -> String {
    format!("turn-{seq}")
}

/// Checkpoint one turn-driving hook point, warning loudly on failure.
///
/// No durability configured (`None`) or a disabled `enabled` flag means
/// no contact with the store or the filesystem. Durable failures emit a
/// warning through `on_warn` and let the turn proceed: the in-memory
/// entry stays, so the failure is loud but never silent data loss.
pub(crate) fn checkpoint_turn(
    durability: &mut Option<TurnDurability>,
    enabled: bool,
    label: &str,
    snapshot: &str,
    on_warn: &dyn Fn(String),
) {
    let Some(dur) = durability.as_mut() else {
        return;
    };
    if let Some(Err(cause)) = persist_checkpoint(&mut dur.store, &dur.root, enabled, label, snapshot)
    {
        on_warn(format!(
            "durable checkpoint {label} failed ({cause}); turn proceeds without a new recovery point"
        ));
    }
}

/// Render a conversation as one `role: text` line per entry.
///
/// The snapshot captures pre-turn state: the actor calls this before the
/// driver pushes the new input, so resume restores the turn boundary.
pub fn render_snapshot(conv: &state_store::Conversation) -> String {
    conv.snapshot()
        .iter()
        .map(|entry| {
            let role = match entry.role {
                state_store::Role::User => "user",
                state_store::Role::Assistant => "assistant",
            };
            format!("{role}: {}", entry.text)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    /// Fake sink sharing an order log with the faked act.
    struct FakeSink {
        log: Rc<RefCell<Vec<String>>>,
    }

    impl CheckpointSink for FakeSink {
        fn save_checkpoint(&mut self, label: &str, data: &str) -> u64 {
            self.log
                .borrow_mut()
                .push(format!("save:{label}:{data}"));
            self.log.borrow().len() as u64
        }
    }

    #[test]
    fn save_precedes_the_faked_act() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("checkpoints");
        let log: Rc<RefCell<Vec<String>>> = Rc::default();
        let mut sink = FakeSink { log: log.clone() };
        let act_log = log.clone();
        let (acted, durable) = persist_then_act(
            &mut sink,
            &root,
            true,
            "turn-1",
            "snap",
            || {
                act_log.borrow_mut().push("act".to_string());
                42
            },
        );
        assert_eq!(acted, 42);
        assert!(durable.expect("enabled policy checkpoints").is_ok());
        assert_eq!(
            log.borrow().clone(),
            vec!["save:turn-1:snap".to_string(), "act".to_string()],
            "the durable write lands before the act"
        );
        // The file copy survives for resume.
        assert_eq!(
            state_checkpoint::durable_load(&root, "turn-1")
                .unwrap()
                .as_deref(),
            Some("snap")
        );
    }

    #[test]
    fn disabled_policy_short_circuits_with_zero_io() {
        let dir = tempfile::tempdir().unwrap();
        // A root that does not exist yet: any IO would create it.
        let root = dir.path().join("never-created");
        let log: Rc<RefCell<Vec<String>>> = Rc::default();
        let mut sink = FakeSink { log: log.clone() };
        let (acted, durable) = persist_then_act(
            &mut sink,
            &root,
            false,
            "turn-1",
            "snap",
            || "done",
        );
        assert_eq!(acted, "done");
        assert!(durable.is_none());
        assert!(log.borrow().is_empty(), "the sink is never contacted");
        assert!(
            !root.exists(),
            "disabled policy performs zero IO, not even mkdir"
        );
    }

    #[test]
    fn resume_lists_newest_last() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("checkpoints");
        assert!(resume_checkpoint(&root).is_empty());
        state_checkpoint::durable_save(&root, "turn-1", "a").unwrap();
        state_checkpoint::durable_save(&root, "turn-2", "b").unwrap();
        let labels = resume_checkpoint(&root);
        assert_eq!(labels, vec!["turn-1".to_owned(), "turn-2".to_owned()]);
        assert_eq!(labels.last().map(String::as_str), Some("turn-2"));
    }

    #[test]
    fn turn_labels_and_snapshots_render() {
        assert_eq!(turn_label(1), "turn-1");
        let mut conv = state_store::Conversation::new();
        conv.push(state_store::Role::User, "hi".to_string());
        conv.push(state_store::Role::Assistant, "hello".to_string());
        assert_eq!(render_snapshot(&conv), "user: hi\nassistant: hello");
    }
}
