/*!
 * @file DurableScheduler
 * @description Persisted cron entries surviving process restarts.
 *
 * Responsibilities:
 * - Persist schedule entries to `<home>/.wavecode/schedule.json`.
 * - Reload entries on construct; missed fires are NOT replayed.
 * - Refuse writes over an unreadable store so user schedules survive.
 * - Report running work as interrupted, never as resumed.
 *
 * This module must not depend on: any other workspace crate besides the
 * scheduler primitives in the parent module.
 */

//! Durable schedules: cron entries that survive restarts.
//!
//! Entries persist as one JSON file under `<home>/.wavecode/`. Loading an
//! existing file restores every entry; **missed fires during downtime are
//! not replayed** — each entry simply fires at its next cron occurrence
//! (the same default as cron and systemd timers). A store that cannot be
//! read or parsed marks the scheduler [`Scheduler::degraded`]: adds and
//! removals then fail instead of overwriting a schedule the host could
//! not see.
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
    /// The store could not be read, so writes are refused: an add or
    /// remove must never overwrite a schedule the host could not see.
    #[error("schedule store unreadable; remove the file to start fresh")]
    Degraded,
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
    /// The store exists but could not be read or parsed, so writes are
    /// refused rather than risk overwriting schedules nobody has seen.
    degraded: bool,
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
            degraded: false,
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
    /// A missing file loads empty and clean; an unreadable or corrupt file
    /// loads empty but [`Scheduler::degraded`] — the scheduler then refuses
    /// writes so a schedule the host could not see is never overwritten.
    pub fn load_or_default(home: &Path) -> Self {
        let file = Self::schedule_file_for_home(home);
        let text = match std::fs::read_to_string(&file) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Self {
                    file: Some(file),
                    ..Self::default()
                };
            }
            Err(_) => {
                return Self {
                    file: Some(file),
                    degraded: true,
                    // A file on disk means a previous process may have held
                    // running jobs; processes cannot survive the restart, so
                    // they read as interrupted, never resumed.
                    interrupted: true,
                    ..Self::default()
                };
            }
        };
        match serde_json::from_str::<ScheduleFile>(&text) {
            Ok(stored) => Self {
                file: Some(file),
                entries: stored.entries,
                next_id: stored.next_id.max(1),
                degraded: false,
                interrupted: true,
            },
            Err(_) => Self {
                file: Some(file),
                degraded: true,
                interrupted: true,
                ..Self::default()
            },
        }
    }

    /// Live entries in insertion order.
    pub fn entries(&self) -> &[ScheduleEntry] {
        &self.entries
    }

    /// True when the store exists but could not be read or parsed: writes
    /// are refused so the user's schedule on disk is never overwritten by
    /// a view of it that failed to load.
    pub fn degraded(&self) -> bool {
        self.degraded
    }

    /// True when a previous store existed, so its in-flight work died with
    /// the old process instead of resuming here.
    pub fn interrupted(&self) -> bool {
        self.interrupted
    }

    /// Add one entry, validating the cron expression, and persist. The
    /// write lands before the in-memory commit: a failed persist leaves
    /// the scheduler exactly as it was.
    pub fn add(&mut self, cron: &str, input: &str) -> Result<ScheduleEntry, SchedulePersistError> {
        if CronSpec::parse(cron).is_err() {
            return Err(SchedulePersistError::InvalidCron(cron.to_string()));
        }
        let entry = ScheduleEntry {
            id: format!("sched-{}", self.next_id),
            cron: cron.to_string(),
            input: input.to_string(),
        };
        let mut entries = self.entries.clone();
        entries.push(entry.clone());
        self.persist_snapshot(&entries, self.next_id + 1)?;
        self.next_id += 1;
        self.entries = entries;
        Ok(entry)
    }

    /// Remove one entry by id, persisting; false for unknown ids. The
    /// write lands before the in-memory commit, so a failed persist keeps
    /// the entry.
    pub fn remove(&mut self, id: &str) -> Result<bool, SchedulePersistError> {
        let mut entries = self.entries.clone();
        let before = entries.len();
        entries.retain(|entry| entry.id != id);
        if entries.len() == before {
            return Ok(false);
        }
        self.persist_snapshot(&entries, self.next_id)?;
        self.entries = entries;
        Ok(true)
    }

    /// Write the store atomically: a unique sibling staging file plus
    /// rename, so a crash never leaves a half-written schedule behind and
    /// concurrent writers never clobber each other's staging file.
    /// Degraded schedulers refuse to write at all. Memory-only schedulers
    /// (no file) skip the write.
    fn persist_snapshot(
        &self,
        entries: &[ScheduleEntry],
        next_id: u64,
    ) -> Result<(), SchedulePersistError> {
        let Some(path) = self.file.as_deref() else {
            return Ok(());
        };
        if self.degraded {
            return Err(SchedulePersistError::Degraded);
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let stored = ScheduleFile {
            entries: entries.to_vec(),
            next_id,
        };
        let text = serde_json::to_string_pretty(&stored)
            .map_err(|e| SchedulePersistError::Corrupt(e.to_string()))?;
        // Owner-only: schedule entries embed user-authored prompts and the
        // store lives under `<home>/.wavecode`.
        infrastructure_base::atomic_write_private(path, text.as_bytes())?;
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
        let mut scheduler = Scheduler::load_or_default(dir.path());
        assert!(!scheduler.interrupted());
        assert!(!scheduler.degraded());
        assert!(scheduler.entries().is_empty());

        let first = scheduler.add("0 9 * * *", "morning sync").expect("add");
        let second = scheduler.add("* * * * *", "every minute").expect("add");
        assert_eq!(first.id, "sched-1");
        assert_eq!(second.id, "sched-2");

        let mut reloaded = Scheduler::load_or_default(dir.path());
        assert!(reloaded.interrupted());
        assert_eq!(reloaded.entries(), scheduler.entries());

        // Ids stay monotonic across the restart, never reused.
        let third = reloaded.add("30 12 * * *", "noon").expect("add");
        assert_eq!(third.id, "sched-3");
    }

    /// The schedule store lands owner-only: entries embed user-authored
    /// prompts and the file lives under `<home>/.wavecode`.
    #[test]
    #[cfg(unix)]
    fn schedule_store_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = home();
        let mut scheduler = Scheduler::load_or_default(dir.path());
        scheduler.add("0 9 * * *", "morning sync").expect("add");
        let path = Scheduler::schedule_file_for_home(dir.path());
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "store file stays owner-only: {mode:o}");
    }

    /// Missed fires during downtime are NOT replayed: a restart restores
    /// the entries and nothing more, and the next cron occurrence fires.
    #[test]
    fn restart_replays_no_missed_fires() {
        let dir = home();
        let mut scheduler = Scheduler::load_or_default(dir.path());
        scheduler.add("0 9 * * *", "a").expect("add");
        scheduler.add("0 10 * * *", "b").expect("add");

        let reloaded = Scheduler::load_or_default(dir.path());
        assert_eq!(reloaded.entries().len(), 2);
        assert!(
            reloaded.interrupted(),
            "in-flight work reads as interrupted"
        );
        // There is no catch-up backlog to drain by construction.
        assert!(!reloaded.degraded());
    }

    #[test]
    fn invalid_cron_rejects_without_persisting() {
        let dir = home();
        let mut scheduler = Scheduler::load_or_default(dir.path());
        assert!(matches!(
            scheduler.add("not a cron", "x"),
            Err(SchedulePersistError::InvalidCron(_))
        ));
        assert!(scheduler.entries().is_empty());
        let reloaded = Scheduler::load_or_default(dir.path());
        assert!(reloaded.entries().is_empty());
    }

    #[test]
    fn remove_persists_and_reports_unknown_ids() {
        let dir = home();
        let mut scheduler = Scheduler::load_or_default(dir.path());
        let entry = scheduler.add("0 9 * * *", "a").expect("add");
        assert!(!scheduler.remove("sched-999").expect("remove unknown"));
        assert!(scheduler.remove(&entry.id).expect("remove known"));
        let reloaded = Scheduler::load_or_default(dir.path());
        assert!(reloaded.entries().is_empty());
    }

    #[test]
    fn corrupt_store_loads_degraded_and_refuses_writes() {
        let dir = home();
        let path = Scheduler::schedule_file_for_home(dir.path());
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        std::fs::write(&path, "{not json").expect("write corrupt");
        let mut scheduler = Scheduler::load_or_default(dir.path());
        assert!(scheduler.entries().is_empty());
        assert!(scheduler.interrupted());
        assert!(scheduler.degraded());
        // Writes are refused: a schedule the host could not see must never
        // be overwritten by a view of it that failed to load.
        assert!(matches!(
            scheduler.add("0 9 * * *", "a"),
            Err(SchedulePersistError::Degraded)
        ));
        assert!(scheduler.entries().is_empty(), "nothing was committed");
        let reloaded = Scheduler::load_or_default(dir.path());
        assert!(reloaded.degraded(), "the corrupt file is untouched");
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
