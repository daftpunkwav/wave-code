/*!
 * @file SessionRegistry
 * @description Session identity, index, and turn journaling for resume.
 *
 * Responsibilities:
 * - Assign and validate session ids; keep journals under one directory.
 * - Maintain a small index file backing the session picker.
 * - Record completed turns (text-level snapshots) for resume.
 * - Support rename and fork without touching journal internals.
 *
 * This module must not depend on: drivers, tools, or sessions. History
 * travels as plain (from_model, text) pairs owned by the caller; tool
 * blocks are not represented (text-level resume by design).
 */

//! New-stack session registry: identity + index + per-turn snapshots.

use std::path::{Path, PathBuf};

use crate::JsonlJournal;

/// Sessions root directory name under `~/.wavecode/`.
const SESSIONS_DIR: &str = "sessions";

/// Index file name inside the sessions directory.
const INDEX_FILE: &str = "index.json";
/// Subdirectory of the sessions root holding per-child-task journals.
const CHILDREN_DIR: &str = "children";

/// One resumable session as listed by the picker.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SessionMeta {
    /// Session id (uuid-shaped, also the journal file stem).
    pub id: String,
    /// Display title; defaults to the first user message head.
    pub title: String,
    /// Working directory the session ran in (for cwd scoping).
    pub cwd: String,
    /// Unix seconds of the first recorded turn.
    pub created_at: u64,
    /// Unix seconds of the last recorded turn.
    pub updated_at: u64,
    /// Recorded turn count.
    pub turns: u32,
}

/// Registry failures: IO problems or invalid session ids.
#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    /// Filesystem failure.
    #[error("session IO failed: {0}")]
    Io(#[from] std::io::Error),
    /// JSON (de)serialization failure on the index file.
    #[error("session index failed: {0}")]
    Json(#[from] serde_json::Error),
    /// Journal append/load failure.
    #[error("session journal failed: {0}")]
    Journal(#[from] crate::JournalError),
    /// A session id failed validation (path-escape guard).
    #[error("invalid session id: {0}")]
    InvalidId(String),
}

/// Sessions root: `~/.wavecode/sessions/`.
pub fn sessions_dir(home: &Path) -> PathBuf {
    home.join(".wavecode").join(SESSIONS_DIR)
}

/// Index file path inside the sessions root.
pub fn index_path(home: &Path) -> PathBuf {
    sessions_dir(home).join(INDEX_FILE)
}

/// Journal path for one session id, rejecting path escapes.
fn journal_path(home: &Path, id: &str) -> Result<PathBuf, SessionError> {
    if !is_valid_session_id(id) {
        return Err(SessionError::InvalidId(id.to_string()));
    }
    Ok(sessions_dir(home).join(format!("{id}.jsonl")))
}

/// Journal file one session's turns are appended to, for callers that want
/// to *name* it (a prompt note pointing at the on-disk record) rather than
/// read or write it. `None` for an invalid id, so path escapes stay rejected
/// at one place.
pub fn session_journal_file(home: &Path, id: &str) -> Option<PathBuf> {
    journal_path(home, id).ok()
}

/// History journal file for one session: `<id>.history.jsonl` beside the turn
/// journal, sharing its id validation (path-escape guard). The turn journal
/// records what happened per turn for the picker and the text-level fallback;
/// this file is the block-level write-ahead log that resume samples from.
pub fn session_history_file(home: &Path, id: &str) -> Option<PathBuf> {
    Some(journal_path(home, id).ok()?.with_extension("history.jsonl"))
}

/// Journal file for one child task of a session:
/// `.../sessions/children/<parent>/<child>.jsonl`, keeping a session's own
/// log and its subagents' logs together without mixing them.
pub fn child_journal_file(home: &Path, parent: &str, child: &str) -> Option<PathBuf> {
    if !is_valid_session_id(parent) || !is_valid_session_id(child) {
        return None;
    }
    Some(child_journal_dir(home, parent).join(format!("{child}.jsonl")))
}

/// Directory holding one session's child-task journals.
pub fn child_journal_dir(home: &Path, parent: &str) -> PathBuf {
    sessions_dir(home).join(CHILDREN_DIR).join(parent)
}

/// Append one completed child task's turn to its own journal. Best-effort by
/// contract (callers ignore failures): a subagent's log must never fail the
/// run that spawned it.
///
/// Every persisted text rides `redact` first: the journal is the resume
/// surface, so callers pass their credential mask (see `safety/secrets`)
/// to keep secrets from surviving into readable history. Pass a
/// `|text| text.to_string()` closure in tests.
pub fn record_child_turn(
    home: &Path,
    parent: &str,
    child: &str,
    input: &str,
    history: &[(bool, String)],
    outcome: &str,
    redact: &dyn Fn(&str) -> String,
) -> Result<PathBuf, SessionError> {
    let path = child_journal_file(home, parent, child)
        .ok_or_else(|| SessionError::InvalidId(child.to_string()))?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    JsonlJournal::new(path.clone()).append_turn(&crate::TurnRecord {
        run_id: child.to_string(),
        input: redact(input),
        history: history
            .iter()
            .map(|(from_model, text)| (*from_model, redact(text)))
            .collect(),
        outcome: redact(outcome),
    })?;
    Ok(path)
}

/// Session id whitelist: non-empty, bounded, ASCII alphanumerics plus
/// `-` and `_`, blocking `../` escapes from CLI arguments and picker
/// payloads alike.
pub fn is_valid_session_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Current unix seconds; 0 when the clock is before the epoch (tests).
fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Derive a default title from the first user message: first line,
/// bounded, whitespace-collapsed.
fn default_title(history: &[(bool, String)]) -> String {
    let first_user = history
        .iter()
        .find(|(from_model, _)| !*from_model)
        .map(|(_, text)| text.as_str())
        .unwrap_or("");
    let head: String = first_user
        .lines()
        .next()
        .unwrap_or("")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(80)
        .collect();
    if head.is_empty() {
        "(untitled)".to_string()
    } else {
        head
    }
}

/// Load the whole index; missing or broken files yield an empty list.
pub fn list_sessions(home: &Path) -> Vec<SessionMeta> {
    let text = match std::fs::read_to_string(index_path(home)) {
        Ok(text) => text,
        Err(_) => return Vec::new(),
    };
    let mut sessions: Vec<SessionMeta> = serde_json::from_str(&text).unwrap_or(Vec::new());
    sessions.sort_by_key(|meta| std::cmp::Reverse(meta.updated_at));
    sessions
}

/// Upsert one index entry and persist the index file.
fn upsert_index(home: &Path, meta: SessionMeta) -> Result<(), SessionError> {
    let dir = sessions_dir(home);
    std::fs::create_dir_all(&dir)?;
    let mut sessions = {
        let text = std::fs::read_to_string(index_path(home)).unwrap_or_default();
        serde_json::from_str::<Vec<SessionMeta>>(&text).unwrap_or_default()
    };
    match sessions.iter_mut().find(|entry| entry.id == meta.id) {
        Some(entry) => *entry = meta,
        None => sessions.push(meta),
    }
    let text = serde_json::to_string(&sessions)?;
    // Write-then-rename keeps a crash mid-write from truncating the
    // index: a truncated index reads as empty, and the next write would
    // then drop every other session's entry for good.
    let tmp = index_path(home).with_extension("json.tmp");
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, index_path(home))?;
    Ok(())
}

/// Record one completed turn: append the journal snapshot and upsert the
/// index entry (creating it on the first turn).
///
/// `input` is the user text that started the turn (may be empty); the
/// snapshot keeps the whole (from_model, text) dialogue so the latest
/// record alone suffices for resume.
pub fn record_turn(
    home: &Path,
    id: &str,
    cwd: &str,
    input: &str,
    history: &[(bool, String)],
    outcome: &str,
    redact: &dyn Fn(&str) -> String,
) -> Result<SessionMeta, SessionError> {
    let path = journal_path(home, id)?;
    std::fs::create_dir_all(sessions_dir(home))?;
    let journal = JsonlJournal::new(path);
    journal.append_turn(&crate::TurnRecord {
        run_id: id.to_string(),
        input: redact(input),
        history: history
            .iter()
            .map(|(from_model, text)| (*from_model, redact(text)))
            .collect(),
        outcome: redact(outcome),
    })?;
    let now = now_secs();
    let existing = list_sessions(home).into_iter().find(|entry| entry.id == id);
    let meta = match existing {
        Some(mut meta) => {
            meta.updated_at = now;
            meta.turns += 1;
            meta
        }
        None => SessionMeta {
            id: id.to_string(),
            title: default_title(history),
            cwd: cwd.to_string(),
            created_at: now,
            updated_at: now,
            turns: 1,
        },
    };
    upsert_index(home, meta.clone())?;
    Ok(meta)
}

/// Record a rewind: append the truncated dialogue as the newest
/// journal snapshot (resume replays the latest record alone) and pull
/// the index turn count back by `turns_removed`.
pub fn record_rewind(
    home: &Path,
    id: &str,
    cwd: &str,
    history: &[(bool, String)],
    turns_removed: u32,
    redact: &dyn Fn(&str) -> String,
) -> Result<SessionMeta, SessionError> {
    let path = journal_path(home, id)?;
    std::fs::create_dir_all(sessions_dir(home))?;
    let journal = JsonlJournal::new(path);
    journal.append_turn(&crate::TurnRecord {
        run_id: id.to_string(),
        input: String::new(),
        history: history
            .iter()
            .map(|(from_model, text)| (*from_model, redact(text)))
            .collect(),
        outcome: redact("Rewound"),
    })?;
    let now = now_secs();
    let existing = list_sessions(home).into_iter().find(|entry| entry.id == id);
    let meta = match existing {
        Some(mut meta) => {
            meta.updated_at = now;
            meta.turns = meta.turns.saturating_sub(turns_removed);
            meta
        }
        None => SessionMeta {
            id: id.to_string(),
            title: default_title(history),
            cwd: cwd.to_string(),
            created_at: now,
            updated_at: now,
            turns: 0,
        },
    };
    upsert_index(home, meta.clone())?;
    Ok(meta)
}

/// Rename a session; returns the updated meta (`None` when unknown).
pub fn set_title(home: &Path, id: &str, title: &str) -> Result<Option<SessionMeta>, SessionError> {
    let title = title.trim();
    if title.is_empty() {
        return Ok(None);
    }
    let mut sessions = {
        let text = std::fs::read_to_string(index_path(home)).unwrap_or_default();
        serde_json::from_str::<Vec<SessionMeta>>(&text).unwrap_or_default()
    };
    let mut updated = None;
    for entry in &mut sessions {
        if entry.id == id {
            // Bounded titles keep the picker rows readable.
            entry.title = title.chars().take(200).collect();
            updated = Some(entry.clone());
        }
    }
    if let Some(meta) = &updated {
        upsert_index(home, meta.clone())?;
    }
    Ok(updated)
}

/// Fork a session: seed a fresh journal with the caller's snapshot and
/// register it under a new id. The caller stays responsible for the fork
/// id (typically a fresh uuid); history may be empty for a pristine fork.
///
/// Every persisted text rides `redact` first — same contract as
/// [`record_turn`] / [`record_child_turn`]: the journal is a readable
/// resume surface, so the fork must never preserve what the turn journal
/// would have masked.
pub fn fork_session(
    home: &Path,
    id: &str,
    title: &str,
    cwd: &str,
    history: &[(bool, String)],
    redact: &dyn Fn(&str) -> String,
) -> Result<SessionMeta, SessionError> {
    let path = journal_path(home, id)?;
    std::fs::create_dir_all(sessions_dir(home))?;
    let journal = JsonlJournal::new(path);
    journal.append_turn(&crate::TurnRecord {
        run_id: id.to_string(),
        input: String::new(),
        history: history
            .iter()
            .map(|(from_model, text)| (*from_model, redact(text)))
            .collect(),
        outcome: redact("Forked"),
    })?;
    let now = now_secs();
    let meta = SessionMeta {
        id: id.to_string(),
        title: title.to_string(),
        cwd: cwd.to_string(),
        created_at: now,
        updated_at: now,
        turns: 0,
    };
    upsert_index(home, meta.clone())?;
    Ok(meta)
}

/// Load one session's resume history: the latest journal snapshot.
///
/// Missing journals read as empty; corrupt lines skip (partial history
/// still resumes) — recovery paths prefer best-effort over refusal.
pub fn load_session_history(home: &Path, id: &str) -> Result<Vec<(bool, String)>, SessionError> {
    let path = journal_path(home, id)?;
    let journal = JsonlJournal::new(path);
    let records = journal.load_all()?;
    Ok(records
        .last()
        .map(|record| record.history.clone())
        .unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn history() -> Vec<(bool, String)> {
        vec![(false, "hello".to_string()), (true, "hi there".to_string())]
    }

    #[test]
    fn recorded_text_rides_the_redaction_gate() {
        let dir = tempfile::tempdir().unwrap();
        let mask = |text: &str| text.replace("sk-secret-1", "***");
        record_turn(
            dir.path(),
            "s-redact",
            "/tmp",
            "my key is sk-secret-1",
            &[(false, "echo sk-secret-1".to_string())],
            "Completed",
            &mask,
        )
        .unwrap();
        let journal = JsonlJournal::new(journal_path(dir.path(), "s-redact").unwrap());
        let records = journal.load_all().unwrap();
        assert_eq!(records.len(), 1);
        let record = &records[0];
        assert_eq!(record.input, "my key is ***");
        assert_eq!(record.history[0].1, "echo ***");
        assert_eq!(record.outcome, "Completed");
        let joined = format!(
            "{} {} {}",
            record.input,
            record
                .history
                .iter()
                .map(|(_, t)| t.as_str())
                .collect::<Vec<_>>()
                .join(" "),
            record.outcome
        );
        assert!(
            !joined.contains("sk-secret-1"),
            "the raw value must not survive anywhere in the journal"
        );
    }

    #[test]
    fn turn_recording_builds_and_updates_the_index() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        let meta = record_turn(
            home,
            "s-1",
            "/tmp",
            "hello",
            &history(),
            "Completed",
            &|t: &str| t.to_string(),
        )
        .unwrap();
        assert_eq!(meta.title, "hello");
        assert_eq!(meta.turns, 1);
        let second = record_turn(
            home,
            "s-1",
            "/tmp",
            "more",
            &history(),
            "Completed",
            &|t: &str| t.to_string(),
        )
        .unwrap();
        assert_eq!(second.turns, 2);
        // Titles stick to the first user message, later turns keep them.
        assert_eq!(second.title, "hello");
        let listed = list_sessions(home);
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, "s-1");
        // Resume replays the latest snapshot.
        assert_eq!(load_session_history(home, "s-1").unwrap(), history());
    }

    #[test]
    fn id_validation_blocks_path_escapes() {
        assert!(is_valid_session_id("9f0c2e1a-1111-2222-3333-444455556666"));
        assert!(!is_valid_session_id("../evil"));
        assert!(!is_valid_session_id(""));
        assert!(journal_path(Path::new("/tmp"), "../evil").is_err());
        // Missing journals read as empty history, never as errors.
        assert_eq!(
            load_session_history(Path::new("/nonexistent"), "s-9").unwrap(),
            Vec::new()
        );
    }

    #[test]
    fn rename_and_fork_create_independent_entries() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        record_turn(
            home,
            "src",
            "/tmp",
            "hello",
            &history(),
            "Completed",
            &|t: &str| t.to_string(),
        )
        .unwrap();
        let renamed = set_title(home, "src", "renamed session").unwrap().unwrap();
        assert_eq!(renamed.title, "renamed session");
        // Empty titles are rejected, not silently applied.
        assert!(set_title(home, "src", "  ").unwrap().is_none());

        let fork = fork_session(
            home,
            "fork-1",
            "Fork: renamed session",
            "/tmp",
            &history(),
            &|t: &str| t.to_string(),
        )
        .unwrap();
        assert_eq!(fork.title, "Fork: renamed session");
        assert_eq!(load_session_history(home, "fork-1").unwrap(), history());
        let listed = list_sessions(home);
        assert_eq!(listed.len(), 2);
        // Forking is independent: the source keeps its own journal.
        assert_eq!(
            list_sessions(home).iter().filter(|m| m.id == "src").count(),
            1
        );
    }

    #[test]
    fn broken_index_files_degrade_to_empty() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        std::fs::create_dir_all(sessions_dir(home)).unwrap();
        std::fs::write(index_path(home), "{not json").unwrap();
        assert!(list_sessions(home).is_empty());
        // Recording heals the index by upserting over the broken file.
        record_turn(
            home,
            "s-2",
            "/tmp",
            "hi",
            &history(),
            "Completed",
            &|t: &str| t.to_string(),
        )
        .unwrap();
        assert_eq!(list_sessions(home).len(), 1);
    }

    /// The fork's seeded snapshot rides the redaction gate like every other
    /// journal write: a secret visible in the source dialogue must not
    /// survive into the fork's readable history.
    #[test]
    fn forked_history_rides_the_redaction_gate() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        let source = vec![
            (false, "my key is sk-secret-1".to_string()),
            (true, "noted".to_string()),
        ];
        let mask = |text: &str| text.replace("sk-secret-1", "***");
        fork_session(home, "fork-redact", "Fork", "/tmp", &source, &mask).unwrap();
        let history = load_session_history(home, "fork-redact").unwrap();
        let joined = history
            .iter()
            .map(|(_, t)| t.as_str())
            .collect::<Vec<_>>()
            .join(" ");
        assert!(
            !joined.contains("sk-secret-1"),
            "the raw value must not survive the fork: {joined}"
        );
        assert_eq!(history[0].1, "my key is ***");
    }

    #[test]
    fn rewind_appends_a_truncated_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        record_turn(
            home,
            "s-1",
            "/tmp",
            "one",
            &history(),
            "Completed",
            &|t: &str| t.to_string(),
        )
        .unwrap();
        let two = vec![
            (false, "one".to_string()),
            (true, "hi there".to_string()),
            (false, "two".to_string()),
            (true, "more".to_string()),
        ];
        record_turn(home, "s-1", "/tmp", "two", &two, "Completed", &|t: &str| {
            t.to_string()
        })
        .unwrap();
        let rewound = vec![(false, "one".to_string()), (true, "hi there".to_string())];
        let meta =
            record_rewind(home, "s-1", "/tmp", &rewound, 1, &|t: &str| t.to_string()).unwrap();
        // Resume replays the rewound dialogue, not the dropped turn.
        assert_eq!(load_session_history(home, "s-1").unwrap(), rewound);
        // The index turn count follows the rewind.
        assert_eq!(meta.turns, 1);
        let listed = list_sessions(home);
        assert_eq!(listed[0].turns, 1);
    }
}
