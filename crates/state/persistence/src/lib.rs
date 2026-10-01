/*!
 * @file TurnJournal
 * @description Append-only JSONL journal of turn records for resume.
 *
 * Responsibilities:
 * - Persist one record per turn as a single JSON line.
 * - Reload records in order, skipping corrupt lines explicitly.
 * - Count skipped lines so strict callers can refuse partial loads.
 * - Serve the tail for quick resume previews.
 * - Store the always-allow grant table (see the `grants` submodule).
 *
 * This module must not depend on: drivers, tools, or sessions. History
 * entries travel as plain model/user text pairs owned by the caller;
 * the legacy import submodule additionally reads frozen provider
 * message shapes.
 */

//! Persistence as dumb bytes: structure lives with the caller.

/// Owner-only file primitives for local session data (`0o600` on Unix,
/// no-op tightening elsewhere): re-exported so callers writing
/// session-local files (frontend input history) share the one policy
/// instead of restating the `cfg` dance per crate.
pub use infrastructure_base::{atomic_write_private, open_append_private, write_private};

/// Absolute path to Windows `taskkill.exe`.
///
/// Re-exported for crates whose dependency set cannot name
/// `infrastructure-base` directly (the console UI) and for the harness
/// binary, so a tree kill never resolves a bare `taskkill` from the
/// working directory. See [`infrastructure_base::taskkill_program`].
#[cfg(windows)]
pub use infrastructure_base::taskkill_program;

/// Legacy engine journal import for session resume.
pub mod legacy;

/// Write-ahead record log of conversation history mutations.
pub mod history;

/// Persisted always-allow grants shared by every session.
pub mod grants;

/// New-stack session registry: identity, index, and turn journaling.
pub mod sessions;

/// Current on-disk journal format version.
///
/// New journals start with a `{"format":1}` header line. Journals written
/// before versioning carry no header and read back as version 0.
pub const JOURNAL_FORMAT_VERSION: u32 = 1;

/// Version of a journal with no header line (pre-versioning layout).
pub const JOURNAL_FORMAT_V0: u32 = 0;

/// True when a parsed line is a format header rather than a turn record.
///
/// Headers carry a numeric `format` field and never carry `run_id`, so a
/// v0 record can never be mistaken for one.
fn is_header_value(value: &serde_json::Value) -> bool {
    value.get("format").and_then(|v| v.as_u64()).is_some() && value.get("run_id").is_none()
}

/// Migrate one v0 record value into the current [`TurnRecord`] shape.
///
/// Unknown or missing fields degrade to defaults so old journals keep
/// loading; use with [`JsonlJournal::format_version`] to detect whether a
/// file needs migration. Migration happens on read only: loaders never
/// rewrite the user's journal file.
pub fn migrate_record(value: &serde_json::Value) -> TurnRecord {
    TurnRecord {
        run_id: value
            .get("run_id")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string(),
        input: value
            .get("input")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string(),
        history: value
            .get("history")
            .and_then(|v| v.as_array())
            .map(|items| {
                items
                    .iter()
                    .map(|item| {
                        (
                            item.get("from_model")
                                .and_then(|v| v.as_bool())
                                .unwrap_or(false),
                            item.get("text")
                                .and_then(|v| v.as_str())
                                .unwrap_or_default()
                                .to_string(),
                        )
                    })
                    .collect()
            })
            .unwrap_or_default(),
        outcome: value
            .get("outcome")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string(),
    }
}

/// One persisted turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnRecord {
    /// Run identifier for correlation.
    pub run_id: String,
    /// User input that started the turn.
    pub input: String,
    /// Final history as (from_model, text) pairs.
    pub history: Vec<(bool, String)>,
    /// Terminal outcome name, e.g. `Completed`.
    pub outcome: String,
}

/// Journal failures.
#[derive(Debug, thiserror::Error)]
pub enum JournalError {
    /// Filesystem failure while reading or writing.
    #[error("journal IO failed: {0}")]
    Io(#[from] std::io::Error),
    /// A line is not valid JSON and was skipped (counted, not fatal).
    #[error("skipped {0} corrupt lines")]
    CorruptSkipped(usize),
}

/// Append-only JSONL turn journal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JsonlJournal {
    path: std::path::PathBuf,
}

impl JsonlJournal {
    /// Open (or create on first write) the journal at `path`.
    pub fn new(path: std::path::PathBuf) -> Self {
        Self { path }
    }

    /// Append one turn as a single JSON line.
    ///
    /// New (missing or empty) journals start with a
    /// `{"format":1}` header line. Existing headerless v0 journals are
    /// appended to as-is: migration happens on read, never by rewriting
    /// the user's file.
    pub fn append_turn(&self, record: &TurnRecord) -> Result<(), JournalError> {
        use std::io::Write;
        let needs_header = std::fs::metadata(&self.path)
            .map(|m| m.len() == 0)
            .unwrap_or(true);
        let line = serde_json::json!({
            "run_id": record.run_id,
            "input": record.input,
            "history": record.history.iter().map(|(from_model, text)| serde_json::json!({
                "from_model": from_model,
                "text": text,
            })).collect::<Vec<_>>(),
            "outcome": record.outcome,
        });
        let mut file = infrastructure_base::open_append_private(&self.path)?;
        if needs_header {
            let header = serde_json::json!({"format": JOURNAL_FORMAT_VERSION});
            writeln!(
                file,
                "{}",
                serde_json::to_string(&header).unwrap_or_default()
            )?;
        }
        writeln!(file, "{}", serde_json::to_string(&line).unwrap_or_default())?;
        // Durability per turn: a crash may lose at most the record being
        // written, never a "completed" one. The reader repairs the only
        // possible tear — a trailing partial line.
        file.flush()?;
        file.sync_data()?;
        Ok(())
    }

    /// On-disk format version of this journal.
    ///
    /// Returns [`JOURNAL_FORMAT_VERSION`] for new journals (missing or
    /// empty files), the header's version when the first line is a format
    /// header, and [`JOURNAL_FORMAT_V0`] for headerless v0 journals
    /// (including ones whose first line is corrupt: they predate headers).
    pub fn format_version(&self) -> Result<u32, JournalError> {
        // Only the first non-empty line decides the version: read that
        // line directly instead of materializing (and possibly
        // rewriting) the whole journal.
        let first = match self.first_non_empty_line() {
            Ok(Some(line)) => line,
            Ok(None) => return Ok(JOURNAL_FORMAT_VERSION),
            // A torn first line predates headers.
            Err(_) => return Ok(JOURNAL_FORMAT_V0),
        };
        let value: serde_json::Value = match serde_json::from_str(&first) {
            Ok(value) => value,
            Err(_) => return Ok(JOURNAL_FORMAT_V0),
        };
        if is_header_value(&value) {
            return Ok(value
                .get("format")
                .and_then(|v| v.as_u64())
                .and_then(|v| u32::try_from(v).ok())
                .unwrap_or(JOURNAL_FORMAT_V0));
        }
        Ok(JOURNAL_FORMAT_V0)
    }

    /// First non-empty line of the file, if any. `Ok(None)` covers a missing
    /// and an empty file; `Err` means a line could not be read (torn UTF-8).
    fn first_non_empty_line(&self) -> std::io::Result<Option<String>> {
        use std::io::{BufRead, BufReader};
        let file = match std::fs::File::open(&self.path) {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        };
        for line in BufReader::new(file).lines() {
            match line {
                Ok(line) if line.trim().is_empty() => continue,
                Ok(line) => return Ok(Some(line)),
                Err(e) => return Err(e),
            }
        }
        Ok(None)
    }

    /// Read the journal after repairing a torn final write, returning
    /// `Ok(None)` for a missing file.
    ///
    /// Repair covers both tear shapes: a tail cut inside a multi-byte
    /// UTF-8 character (truncates to the last valid boundary) and a
    /// trailing partial line (truncates to the last newline). Truncation
    /// persists so stricter consumers see a clean file. Mid-file damage
    /// is never rewritten here; the parser's skip-counting handles it.
    fn read_repaired(&self) -> std::result::Result<Option<String>, std::io::Error> {
        let bytes = match std::fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        };
        // A tear inside a multi-byte character leaves an invalid tail:
        // keep only the longest valid UTF-8 prefix.
        let mut text = match std::str::from_utf8(&bytes) {
            Ok(text) => text.to_owned(),
            Err(e) => {
                let cut = e.valid_up_to();
                let text = String::from_utf8_lossy(&bytes[..cut]).into_owned();
                std::fs::write(&self.path, text.as_bytes())?;
                text
            }
        };
        // A tear between lines leaves a partial trailing line.
        if !text.ends_with('\n') {
            let cut = text.rfind('\n').map(|i| i + 1).unwrap_or(0);
            text.truncate(cut);
            std::fs::write(&self.path, text.as_bytes())?;
        }
        Ok(Some(text))
    }

    /// Migrate one stored record value into the current shape.
    ///
    /// Associated-function alias of [`migrate_record`] for callers that
    /// prefer the journal namespace.
    pub fn migrate_record(value: &serde_json::Value) -> TurnRecord {
        migrate_record(value)
    }

    /// Reload every parseable record in order.
    ///
    /// Corrupt lines skip silently here; use [`JsonlJournal::load_reported`]
    /// to count them or [`JsonlJournal::load_all_checked`] to refuse them.
    pub fn load_all(&self) -> Result<Vec<TurnRecord>, JournalError> {
        self.load_reported().map(|(records, _)| records)
    }

    /// Reload every parseable record, reporting skipped corrupt lines.
    ///
    /// The journal is a recovery path, so partial history still returns;
    /// the count lets callers decide whether partial is acceptable. A
    /// crash mid-append leaves a trailing partial line (writes are
    /// fsynced per record, so a tear can only be the last one): it is
    /// truncated away persistently before parsing, keeping the file
    /// loadable by stricter consumers.
    pub fn load_reported(&self) -> Result<(Vec<TurnRecord>, usize), JournalError> {
        let text = match self.read_repaired() {
            Ok(Some(text)) => text,
            Ok(None) => return Ok((Vec::new(), 0)),
            Err(e) => return Err(JournalError::Io(e)),
        };
        let mut records = Vec::new();
        let mut skipped = 0;
        let mut first_line = true;
        for line in text.lines() {
            if line.trim().is_empty() {
                continue;
            }
            let value: serde_json::Value = match serde_json::from_str(line) {
                Ok(value) => value,
                Err(_) => {
                    skipped += 1;
                    first_line = false;
                    continue;
                }
            };
            // A leading format header is metadata, not a turn: skip it
            // without counting it as corrupt.
            if first_line && is_header_value(&value) {
                first_line = false;
                continue;
            }
            first_line = false;
            records.push(migrate_record(&value));
        }
        Ok((records, skipped))
    }

    /// Reload, failing explicitly when any line was corrupt.
    ///
    /// Gives the [`JournalError::CorruptSkipped`] variant its job: resume
    /// paths that must not silently continue from partial history use this
    /// instead of [`JsonlJournal::load_all`].
    pub fn load_all_checked(&self) -> Result<Vec<TurnRecord>, JournalError> {
        let (records, skipped) = self.load_reported()?;
        if skipped > 0 {
            return Err(JournalError::CorruptSkipped(skipped));
        }
        Ok(records)
    }

    /// Reload at most the last `n` records for resume previews.
    ///
    /// Streams the journal line by line, keeping only the newest `n`
    /// records in memory — the full history is never materialized just
    /// to preview a tail.
    pub fn last_n(&self, n: usize) -> Result<Vec<TurnRecord>, JournalError> {
        use std::io::BufRead;
        let file = match std::fs::File::open(&self.path) {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(JournalError::Io(e)),
        };
        let mut tail: std::collections::VecDeque<TurnRecord> =
            std::collections::VecDeque::with_capacity(n.min(1024));
        let mut first_line = true;
        for line in std::io::BufReader::new(file).lines() {
            // A torn final line carries no complete record.
            let Ok(line) = line else { break };
            if line.trim().is_empty() {
                continue;
            }
            let value: serde_json::Value = match serde_json::from_str(&line) {
                Ok(value) => value,
                Err(_) => {
                    first_line = false;
                    continue;
                }
            };
            if first_line && is_header_value(&value) {
                first_line = false;
                continue;
            }
            first_line = false;
            if n == 0 {
                continue;
            }
            if tail.len() == n {
                tail.pop_front();
            }
            tail.push_back(migrate_record(&value));
        }
        Ok(tail.into_iter().collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(id: &str) -> TurnRecord {
        TurnRecord {
            run_id: id.to_string(),
            input: format!("input {id}"),
            history: vec![(false, "hi".to_string()), (true, "hello".to_string())],
            outcome: "Completed".to_string(),
        }
    }

    #[test]
    fn appends_reload_in_order_and_tails_preview() {
        let dir = tempfile::tempdir().unwrap();
        let journal = JsonlJournal::new(dir.path().join("turns.jsonl"));
        // Missing files read as empty, never as errors.
        assert!(journal.load_all().unwrap().is_empty());
        journal.append_turn(&record("r1")).unwrap();
        journal.append_turn(&record("r2")).unwrap();
        let all = journal.load_all().unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[1], record("r2"));
        let tail = journal.last_n(1).unwrap();
        assert_eq!(tail.len(), 1);
        assert_eq!(tail[0].run_id, "r2");
    }

    /// A crash mid-append leaves a trailing partial line; loading must
    /// repair it persistently (the tear is always the last record) and
    /// return the intact prefix.
    #[test]
    fn torn_tail_is_truncated_on_load() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("turns.jsonl");
        let journal = JsonlJournal::new(path.clone());
        journal.append_turn(&record("r1")).unwrap();
        journal.append_turn(&record("r2")).unwrap();
        // Simulate the torn final write: partial JSON, no newline.
        use std::io::Write as _;
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        write!(file, "{{\"run_id\":\"r3\",\"inpu").unwrap();
        drop(file);

        let (records, skipped) = journal.load_reported().unwrap();
        assert_eq!(records, vec![record("r1"), record("r2")]);
        assert_eq!(skipped, 0, "the tear is repaired, not skipped");
        // The repair persists: the file is newline-terminated again and
        // a strict reload (which refuses corrupt lines) passes clean.
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.ends_with('\n'));
        assert_eq!(journal.load_all_checked().unwrap().len(), 2);
    }

    /// A tear inside a multi-byte character leaves an invalid UTF-8
    /// tail: the repair must truncate to the valid boundary instead of
    /// failing the whole load (CJK turn text makes this realistic).
    #[test]
    fn torn_multibyte_tail_is_truncated_on_load() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("turns.jsonl");
        let journal = JsonlJournal::new(path.clone());
        journal.append_turn(&record("r1")).unwrap();
        // Simulate a torn final write whose half-character tail is not
        // valid UTF-8 (a 3-byte CJK char cut after its first byte).
        use std::io::Write as _;
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        file.write_all("{\"run_id\":\"r3\",\"input\":\"你".as_bytes())
            .unwrap();
        file.write_all(&[0xe4, 0xb8]).unwrap(); // torn multibyte tail
        drop(file);
        // The file is not even readable as UTF-8 before the repair.
        assert!(std::fs::read_to_string(&path).is_err());

        let (records, skipped) = journal.load_reported().unwrap();
        assert_eq!(records, vec![record("r1")]);
        assert_eq!(skipped, 0);
        assert!(std::fs::read_to_string(&path).is_ok(), "repair persists");
        assert_eq!(journal.load_all_checked().unwrap().len(), 1);
    }

    #[test]
    fn new_journals_start_with_a_format_header() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("turns.jsonl");
        let journal = JsonlJournal::new(path.clone());
        // Missing files report the current version, never an error.
        assert_eq!(journal.format_version().unwrap(), JOURNAL_FORMAT_VERSION);
        journal.append_turn(&record("r1")).unwrap();
        journal.append_turn(&record("r2")).unwrap();
        let first = std::fs::read_to_string(&path)
            .unwrap()
            .lines()
            .next()
            .unwrap()
            .to_string();
        let header: serde_json::Value = serde_json::from_str(&first).unwrap();
        assert_eq!(header.get("format").and_then(|v| v.as_u64()), Some(1));
        assert_eq!(journal.format_version().unwrap(), 1);
        // The header is metadata: it never surfaces as a record.
        let all = journal.load_all().unwrap();
        assert_eq!(all, vec![record("r1"), record("r2")]);
    }

    #[test]
    fn headerless_v0_journals_read_without_rewrite() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("turns.jsonl");
        // Hand-written v0 layout: records only, no header line.
        let v0 = serde_json::to_string(&serde_json::json!({
            "run_id": "old-1",
            "input": "input old-1",
            "history": [{"from_model": false, "text": "hi"}],
            "outcome": "Completed",
        }))
        .unwrap();
        std::fs::write(&path, format!("{v0}\n")).unwrap();
        let before = std::fs::read_to_string(&path).unwrap();
        let journal = JsonlJournal::new(path.clone());
        assert_eq!(journal.format_version().unwrap(), 0);
        let all = journal.load_all().unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].run_id, "old-1");
        assert_eq!(all[0].history, vec![(false, "hi".to_string())]);
        // Migration happens on read: the file is untouched.
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
    }

    #[test]
    fn migrate_record_maps_v0_fields_with_defaults() {
        let value = serde_json::json!({
            "run_id": "m1",
            "input": "ask",
            "history": [
                {"from_model": true, "text": "answer"},
                {"unexpected": "shape"},
            ],
        });
        let migrated = migrate_record(&value);
        assert_eq!(migrated.run_id, "m1");
        assert_eq!(migrated.input, "ask");
        assert_eq!(
            migrated.history,
            vec![(true, "answer".to_string()), (false, String::new()),]
        );
        // Missing outcome degrades to empty, never to an error.
        assert_eq!(migrated.outcome, String::new());
        // The associated-function alias agrees with the free function.
        assert_eq!(JsonlJournal::migrate_record(&value), migrated);
    }

    #[test]
    fn header_plus_corrupt_lines_stay_tolerant() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("turns.jsonl");
        let journal = JsonlJournal::new(path.clone());
        journal.append_turn(&record("r1")).unwrap();
        // Splice a corrupt line between the header and the record tail.
        let mut text = std::fs::read_to_string(&path).unwrap();
        text.push_str("{not json\n");
        std::fs::write(&path, &text).unwrap();
        journal.append_turn(&record("r2")).unwrap();
        let (records, skipped) = journal.load_reported().unwrap();
        assert_eq!(skipped, 1);
        assert_eq!(records, vec![record("r1"), record("r2")]);
        assert_eq!(journal.format_version().unwrap(), 1);
        assert_eq!(
            journal.load_all_checked().unwrap_err().to_string(),
            "skipped 1 corrupt lines"
        );
    }

    #[test]
    fn corrupt_lines_skip_without_losing_good_records() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("turns.jsonl");
        std::fs::write(&path, "{not json\n").unwrap();
        let journal = JsonlJournal::new(path);
        journal.append_turn(&record("r1")).unwrap();
        let all = journal.load_all().unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].run_id, "r1");
        // The skip is counted, and strict loads refuse partial history.
        let (reported, skipped) = journal.load_reported().unwrap();
        assert_eq!(reported.len(), 1);
        assert_eq!(skipped, 1);
        assert_eq!(
            journal.load_all_checked().unwrap_err().to_string(),
            "skipped 1 corrupt lines"
        );
    }
}
