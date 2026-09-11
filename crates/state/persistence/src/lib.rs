/*!
 * @file TurnJournal
 * @description Append-only JSONL journal of turn records for resume.
 *
 * Responsibilities:
 * - Persist one record per turn as a single JSON line.
 * - Reload records in order, skipping corrupt lines explicitly.
 * - Count skipped lines so strict callers can refuse partial loads.
 * - Serve the tail for quick resume previews.
 *
 * This module must not depend on: drivers, tools, or sessions. History
 * entries travel as plain model/user text pairs owned by the caller;
 * the legacy import submodule additionally reads frozen provider
 * message shapes.
 */

//! Persistence as dumb bytes: structure lives with the caller.

/// Legacy engine journal import for session resume.
pub mod legacy;

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
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        if needs_header {
            let header = serde_json::json!({"format": JOURNAL_FORMAT_VERSION});
            writeln!(
                file,
                "{}",
                serde_json::to_string(&header).unwrap_or_default()
            )?;
        }
        writeln!(file, "{}", serde_json::to_string(&line).unwrap_or_default())?;
        Ok(())
    }

    /// On-disk format version of this journal.
    ///
    /// Returns [`JOURNAL_FORMAT_VERSION`] for new journals (missing or
    /// empty files), the header's version when the first line is a format
    /// header, and [`JOURNAL_FORMAT_V0`] for headerless v0 journals
    /// (including ones whose first line is corrupt: they predate headers).
    pub fn format_version(&self) -> Result<u32, JournalError> {
        let text = match std::fs::read_to_string(&self.path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(JOURNAL_FORMAT_VERSION);
            }
            Err(e) => return Err(JournalError::Io(e)),
        };
        for line in text.lines() {
            if line.trim().is_empty() {
                continue;
            }
            let value: serde_json::Value = match serde_json::from_str(line) {
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
            return Ok(JOURNAL_FORMAT_V0);
        }
        Ok(JOURNAL_FORMAT_VERSION)
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
    /// the count lets callers decide whether partial is acceptable.
    pub fn load_reported(&self) -> Result<(Vec<TurnRecord>, usize), JournalError> {
        let text = match std::fs::read_to_string(&self.path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok((Vec::new(), 0)),
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
    pub fn last_n(&self, n: usize) -> Result<Vec<TurnRecord>, JournalError> {
        let all = self.load_all()?;
        let skip = all.len().saturating_sub(n);
        Ok(all.into_iter().skip(skip).collect())
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
