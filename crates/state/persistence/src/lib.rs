/*!
 * @file TurnJournal
 * @description Append-only JSONL journal of turn records for resume.
 *
 * Responsibilities:
 * - Persist one record per turn as a single JSON line.
 * - Reload records in order, skipping corrupt lines explicitly.
 * - Serve the tail for quick resume previews.
 *
 * This module must not depend on: any other workspace crate. History
 * entries travel as plain model/user text pairs owned by the caller.
 */

//! Persistence as dumb bytes: structure lives with the caller.

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
    pub fn append_turn(&self, record: &TurnRecord) -> Result<(), JournalError> {
        use std::io::Write;
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
        writeln!(file, "{}", serde_json::to_string(&line).unwrap_or_default())?;
        Ok(())
    }

    /// Reload every parseable record in order.
    pub fn load_all(&self) -> Result<Vec<TurnRecord>, JournalError> {
        let text = match std::fs::read_to_string(&self.path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(JournalError::Io(e)),
        };
        let mut records = Vec::new();
        for line in text.lines() {
            if line.trim().is_empty() {
                continue;
            }
            let value: serde_json::Value = match serde_json::from_str(line) {
                Ok(value) => value,
                Err(_) => continue,
            };
            records.push(TurnRecord {
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
            });
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
    fn corrupt_lines_skip_without_losing_good_records() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("turns.jsonl");
        std::fs::write(&path, "{not json\n").unwrap();
        let journal = JsonlJournal::new(path);
        journal.append_turn(&record("r1")).unwrap();
        let all = journal.load_all().unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].run_id, "r1");
    }
}
