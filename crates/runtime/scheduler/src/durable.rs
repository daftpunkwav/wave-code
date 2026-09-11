/*!
 * @file DurableScheduler
 * @description Persisted cron entries surviving process restarts.
 *
 * Responsibilities:
 * - Persist schedule entries to `<home>/.wavecode/schedule.json`.
 * - Reload entries on construct with fire-once missed-fire catch-up.
 * - Report running work as interrupted, never as resumed.
 *
 * This module must not depend on: any other workspace crate besides the
 * scheduler primitives in the parent module.
 */

//! Durable schedules: cron entries that survive restarts.
//!
//! Entries persist as one JSON file under `<home>/.wavecode/`. Loading an
//! existing file restores every entry and marks each as one pending
//! immediate fire: while the process was away at least the missed window
//! is covered by firing once, then the normal cadence resumes. The host
//! drains [`Scheduler::take_pending_fires`] and surfaces the returned
//! count as a startup warning.
//!
//! Running jobs are NOT persisted on purpose: OS processes cannot survive
//! a restart, so any work in flight when the previous process died reads
//! back as interrupted ([`Scheduler::interrupted`]), never as resumed.

use std::path::{Path, PathBuf};

use crate::CronSpec;

/// One persisted cron entry.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ScheduleEntry {
    /// Stable id (`sched-N`).
    pub id: String,
    /// Five-field cron expression.
    pub cron: String,
    /// Task input the host fires on schedule.
    pub input: String,
}

/// On-disk shape: entries plus the monotonic id allocator.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct ScheduleFile {
    /// Persisted entries in insertion order.
    #[serde(default)]
    entries: Vec<ScheduleEntry>,
    /// Next id counter; ids look like `sched-{next_id}`.
    #[serde(default = "default_next_id")]
    next_id: u64,
}

/// Fresh stores start numbering at 1.
fn default_next_id() -> u64 {
    1
}

/// Durable schedule failures.
#[derive(Debug, thiserror::Error)]
pub enum SchedulePersistError {
    /// The cron expression does not parse.
    #[error("invalid cron expression: {0}")]
    InvalidCron(String),
    /// The store file is present but not valid JSON.
    #[error("schedule store corrupt: {0}")]
    Corrupt(String),
    /// Filesystem failure reading or writing the store.
    #[error("schedule store io: {0}")]
    Io(#[from] std::io::Error),
}

/// Cron entries persisted to `<home>/.wavecode/schedule.json`.
///
/// Memory-only when built without a file ([`Scheduler::default`]): adds
/// and removals work but nothing reaches disk.
#[derive(Debug)]
pub struct Scheduler {
    /// Store file; `None` disables persistence.
    file: Option<PathBuf>,
    /// Live entries in insertion order.
    entries: Vec<ScheduleEntry>,
    /// Monotonic id allocator.
    next_id: u64,
    /// Restored entry ids owed one immediate fire after a restart.
    pending_fire_ids: Vec<String>,
    /// True when a previous store existed, so its in-flight work died.
    interrupted: bool,
}

impl Default for Scheduler {
    /// Empty memory-only scheduler: no file, no entries, not interrupted.
    fn default() -> Self {
        Self {
            file: None,
            entries: Vec::new(),
            next_id: 1,
            pending_fire_ids: Vec::new(),
            interrupted: false,
        }
    }
}

impl Scheduler {
    /// Store file for a home directory: `<home>/.wavecode/schedule.json`.
    pub fn schedule_file_for_home(home: &Path) -> PathBuf {
        home.join(".wavecode").join("schedule.json")
    }

    /// Load the persisted schedule or start empty.
    ///
    /// Returns the scheduler plus the missed-fire catch-up count: one per
    /// restored entry, each owed a single immediate fire before the normal
    /// cadence resumes. A missing file loads empty with count zero and no
    /// interruption; an unreadable or corrupt file loads empty, counts
    /// zero, and still reports interrupted so the host warns that prior
    /// state was lost.
    pub fn load_or_default(home: &Path) -> (Self, u64) {
        let file = Self::schedule_file_for_home(home);
        let text = match std::fs::read_to_string(&file) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return (
                    Self {
                        file: Some(file),
                        ..Self::default()
                    },
                    0,
                );
            }
            Err(_) => {
                return (
                    Self {
                        file: Some(file),
                        interrupted: true,
                        ..Self::default()
                    },
                    0,
                );
            }
        };
        match serde_json::from_str::<ScheduleFile>(&text) {
            Ok(stored) => {
                let pending_fire_ids =
                    stored.entries.iter().map(|entry| entry.id.clone()).collect::<Vec<_>>();
                let catchup = pending_fire_ids.len() as u64;
                (
                    Self {
                        file: Some(file),
                        entries: stored.entries,
                        next_id: stored.next_id.max(1),
                        pending_fire_ids,
                        // A file on disk means a previous process may have
                        // held running jobs; processes cannot survive the
                        // restart, so they read as interrupted, never resumed.
                        interrupted: true,
                    },
                    catchup,
                )
            }
            Err(_) => (
                Self {
                    file: Some(file),
                    interrupted: true,
                    ..Self::default()
                },
                0,
            ),
        }
    }

    /// Live entries in insertion order.
    pub fn entries(&self) -> &[ScheduleEntry] {
        &self.entries
    }

    /// Restored entries still owed their one immediate post-restart fire.
    pub fn catchup_pending(&self) -> usize {
        self.pending_fire_ids.len()
    }

    /// True when a previous store existed, so its in-flight work died with
    /// the old process instead of resuming here.
    pub fn interrupted(&self) -> bool {
        self.interrupted
    }

    /// Drain entries owed one immediate fire; the host fires each once,
    /// then resumes the normal cadence.
    pub fn take_pending_fires(&mut self) -> Vec<ScheduleEntry> {
        let ids = std::mem::take(&mut self.pending_fire_ids);
        ids.into_iter()
            .filter_map(|id| self.entries.iter().find(|entry| entry.id == id).cloned())
            .collect()
    }

    /// Add one entry, validating the cron expression, and persist.
    pub fn add(&mut self, cron: &str, input: &str) -> Result<ScheduleEntry, SchedulePersistError> {
        if CronSpec::parse(cron).is_err() {
            return Err(SchedulePersistError::InvalidCron(cron.to_string()));
        }
        let entry = ScheduleEntry {
            id: format!("sched-{}", self.next_id),
            cron: cron.to_string(),
            input: input.to_string(),
        };
        self.next_id += 1;
        self.entries.push(entry.clone());
        self.persist()?;
        Ok(entry)
    }

    /// Remove one entry by id, persisting; false for unknown ids.
    pub fn remove(&mut self, id: &str) -> Result<bool, SchedulePersistError> {
        let before = self.entries.len();
        self.entries.retain(|entry| entry.id != id);
        self.pending_fire_ids.retain(|pending| pending != id);
        if self.entries.len() == before {
            return Ok(false);
        }
        self.persist()?;
        Ok(true)
    }

    /// Write the store atomically: sibling temp file plus rename, so a
    /// crash never leaves a half-written schedule behind. Memory-only
    /// schedulers (no file) skip the write.
    fn persist(&self) -> Result<(), SchedulePersistError> {
        let Some(path) = self.file.as_deref() else {
            return Ok(());
        };
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let stored = ScheduleFile {
            entries: self.entries.clone(),
            next_id: self.next_id,
        };
        let text = serde_json::to_string_pretty(&stored)
            .map_err(|e| SchedulePersistError::Corrupt(e.to_string()))?;
        let staging = path.with_extension("json.staging-tmp");
        std::fs::write(&staging, text)?;
        std::fs::rename(&staging, path)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn home() -> tempfile::TempDir {
        tempfile::tempdir().expect("tempdir")
    }

    #[test]
    fn persist_reload_round_trip_keeps_entries_and_ids() {
        let dir = home();
        let (mut scheduler, catchup) = Scheduler::load_or_default(dir.path());
        assert_eq!(catchup, 0);
        assert!(!scheduler.interrupted());
        assert!(scheduler.entries().is_empty());

        let first = scheduler.add("0 9 * * *", "morning sync").expect("add");
        let second = scheduler.add("* * * * *", "every minute").expect("add");
        assert_eq!(first.id, "sched-1");
        assert_eq!(second.id, "sched-2");

        let (reloaded, catchup) = Scheduler::load_or_default(dir.path());
        assert_eq!(catchup, 2);
        assert!(reloaded.interrupted());
        assert_eq!(reloaded.entries(), scheduler.entries());

        // Ids stay monotonic across the restart, never reused.
        let mut reloaded = reloaded;
        let third = reloaded.add("30 12 * * *", "noon").expect("add");
        assert_eq!(third.id, "sched-3");
    }

    #[test]
    fn missed_fire_catch_up_fires_each_entry_once() {
        let dir = home();
        let (mut scheduler, _) = Scheduler::load_or_default(dir.path());
        scheduler.add("0 9 * * *", "a").expect("add");
        scheduler.add("0 10 * * *", "b").expect("add");
        scheduler.add("0 11 * * *", "c").expect("add");

        let (mut reloaded, catchup) = Scheduler::load_or_default(dir.path());
        assert_eq!(catchup, 3);
        assert_eq!(reloaded.catchup_pending(), 3);
        let fires = reloaded.take_pending_fires();
        assert_eq!(fires.len(), 3);
        assert_eq!(reloaded.catchup_pending(), 0);
        // Draining is one-shot: the cadence resumes with no backlog.
        assert!(reloaded.take_pending_fires().is_empty());
    }

    #[test]
    fn invalid_cron_rejects_without_persisting() {
        let dir = home();
        let (mut scheduler, _) = Scheduler::load_or_default(dir.path());
        assert!(matches!(
            scheduler.add("not a cron", "x"),
            Err(SchedulePersistError::InvalidCron(_))
        ));
        assert!(scheduler.entries().is_empty());
        let (reloaded, _) = Scheduler::load_or_default(dir.path());
        assert!(reloaded.entries().is_empty());
    }

    #[test]
    fn remove_persists_and_reports_unknown_ids() {
        let dir = home();
        let (mut scheduler, _) = Scheduler::load_or_default(dir.path());
        let entry = scheduler.add("0 9 * * *", "a").expect("add");
        assert!(!scheduler.remove("sched-999").expect("remove unknown"));
        assert!(scheduler.remove(&entry.id).expect("remove known"));
        let (reloaded, _) = Scheduler::load_or_default(dir.path());
        assert!(reloaded.entries().is_empty());
    }

    #[test]
    fn corrupt_store_loads_empty_but_interrupted() {
        let dir = home();
        let path = Scheduler::schedule_file_for_home(dir.path());
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        std::fs::write(&path, "{not json").expect("write corrupt");
        let (scheduler, catchup) = Scheduler::load_or_default(dir.path());
        assert_eq!(catchup, 0);
        assert!(scheduler.entries().is_empty());
        assert!(scheduler.interrupted());
    }

    #[test]
    fn memory_only_scheduler_skips_disk() {
        let mut scheduler = Scheduler::default();
        let entry = scheduler.add("0 9 * * *", "a").expect("add");
        assert_eq!(entry.id, "sched-1");
        assert_eq!(scheduler.entries().len(), 1);
        assert!(!scheduler.interrupted());
    }
}
