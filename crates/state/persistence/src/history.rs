/*!
 * @file HistoryJournal
 * @description Write-ahead record log of conversation history mutations.
 *
 * Responsibilities:
 * - Append one JSON line per committed history mutation, durable per write.
 * - Read the log back as the trustworthy prefix, classifying where it stops.
 * - Own the file layout beside the session registry's other per-session file.
 *
 * This module must not depend on: the conversation types, drivers, or tools.
 * Records arrive as already-encoded JSON values — persistence moves bytes,
 * structure lives with the caller (same contract as the crate's turn
 * journal).
 */

use std::io::Write;
use std::path::{Path, PathBuf};

/// On-disk record layout version, carried by the header line.
pub const HISTORY_FORMAT_VERSION: u32 = 1;

/// A history journal bound to one file.
#[derive(Debug, Clone)]
pub struct HistoryJournal {
    path: PathBuf,
}

/// What a read pass recovered.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct HistoryRead {
    /// Records in append order, up to the first gap.
    pub records: Vec<serde_json::Value>,
    /// True when the final line was unreadable (an expected crash artifact).
    pub torn_tail: bool,
    /// True when an unreadable line sat *between* records: the caller must
    /// treat the history as truncated rather than assume it is complete.
    pub mid_gap: bool,
}

impl HistoryJournal {
    /// Journal at `path` (created with its header on first append).
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// Where records are stored, for status and doctor output.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Append one record and flush it to the storage device.
    ///
    /// Durability per record is the point: a record that reached memory but
    /// not disk is exactly the gap that makes a resumed session re-run a
    /// tool whose side effect already happened.
    pub fn append(&self, record: &serde_json::Value) -> std::io::Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let needs_header = std::fs::metadata(&self.path)
            .map(|m| m.len() == 0)
            .unwrap_or(true);
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        if needs_header {
            let header = serde_json::json!({"k": "header", "format": HISTORY_FORMAT_VERSION});
            writeln!(file, "{header}")?;
        }
        writeln!(file, "{record}")?;
        file.flush()?;
        file.sync_data()?;
        Ok(())
    }

    /// Read back the trustworthy prefix of the log.
    ///
    /// Parsing stops at the first unreadable line. That line is a `torn_tail`
    /// when it is also the last one (the only shape a crash can leave behind,
    /// since each record is synced before the next begins) and a `mid_gap`
    /// otherwise, which means damage inside the history rather than at its
    /// end. A missing file reads as empty.
    pub fn read(&self) -> HistoryRead {
        // Decode lossily instead of failing the whole file: a torn write can
        // cut a multi-byte UTF-8 character in half, and a strict decode would
        // read the entire journal as empty (no records, no torn_tail flag) —
        // silent total history loss on every future resume, since the file is
        // append-only and never rewritten. The replacement-char bytes fail
        // JSON parsing on that one line, which the loop below classifies as a
        // torn tail (last line) or a mid gap.
        let Ok(bytes) = std::fs::read(&self.path) else {
            return HistoryRead::default();
        };
        let text = String::from_utf8_lossy(&bytes);
        let lines: Vec<&str> = text
            .lines()
            .filter(|line| !line.trim().is_empty())
            .collect();
        let mut records = Vec::with_capacity(lines.len());
        let mut out = HistoryRead::default();
        for (index, line) in lines.iter().enumerate() {
            let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
                if index + 1 == lines.len() {
                    out.torn_tail = true;
                } else {
                    out.mid_gap = true;
                }
                break;
            };
            // The header is metadata, never a mutation record.
            if index == 0 && value.get("k").and_then(|k| k.as_str()) == Some("header") {
                continue;
            }
            records.push(value);
        }
        out.records = records;
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path_in(dir: &Path) -> PathBuf {
        dir.join("history.jsonl")
    }

    #[test]
    fn missing_journal_reads_as_empty() {
        let dir = tempfile::tempdir().unwrap();
        let journal = HistoryJournal::new(path_in(dir.path()));
        assert_eq!(journal.read(), HistoryRead::default());
    }

    /// The header is written once and never surfaces as a record.
    #[test]
    fn appends_are_ordered_and_the_header_hidden() {
        let dir = tempfile::tempdir().unwrap();
        let journal = HistoryJournal::new(path_in(dir.path()));
        journal
            .append(&serde_json::json!({"k": "append", "n": 1}))
            .unwrap();
        journal
            .append(&serde_json::json!({"k": "append", "n": 2}))
            .unwrap();
        let read = journal.read();
        assert_eq!(read.records.len(), 2);
        assert_eq!(read.records[1]["n"], 2);
        assert!(!read.torn_tail && !read.mid_gap);
        let raw = std::fs::read_to_string(journal.path()).unwrap();
        assert_eq!(raw.lines().count(), 3, "one header plus two records");
        assert!(raw.lines().next().unwrap().contains("\"format\""));
    }

    /// A crash tears the tail, and only the tail: the prefix stays usable
    /// and the caller learns the log did not end cleanly.
    #[test]
    fn torn_tail_stops_the_read_and_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        let journal = HistoryJournal::new(path_in(dir.path()));
        journal
            .append(&serde_json::json!({"k": "append", "n": 1}))
            .unwrap();
        std::fs::OpenOptions::new()
            .append(true)
            .open(journal.path())
            .and_then(|mut f| write!(f, "{{\"k\":\"appe"))
            .unwrap();
        let read = journal.read();
        assert_eq!(read.records.len(), 1);
        assert!(read.torn_tail);
        assert!(!read.mid_gap);
    }

    /// A tear inside a multi-byte character leaves an invalid UTF-8 tail: the
    /// journal must still yield its intact prefix with `torn_tail` set, never
    /// read as wholesale-empty (a strict decode would silently drop the whole
    /// history on every resume; CJK turn text makes this realistic).
    #[test]
    fn torn_multibyte_tail_keeps_the_prefix_reported() {
        let dir = tempfile::tempdir().unwrap();
        let journal = HistoryJournal::new(path_in(dir.path()));
        journal
            .append(&serde_json::json!({"k": "append", "seq": 0, "entry": {"role": "user", "blocks": [{"text": "你好"}]}}))
            .unwrap();
        // Simulate a torn final write whose tail is a cut 3-byte CJK char.
        let torn = "{\"k\":\"append\",\"seq\":1,\"entry\":{\"role\":\"user\",\"blocks\":[{\"text\":\"\u{4f60}".to_string();
        std::fs::OpenOptions::new()
            .append(true)
            .open(journal.path())
            .and_then(|mut f| {
                f.write_all(torn.as_bytes())
                    .and_then(|()| f.write_all(&[0xe4, 0xb8]))
            })
            .unwrap();
        // The file is not even decodable as UTF-8 before the read.
        assert!(std::fs::read_to_string(journal.path()).is_err());

        let read = journal.read();
        assert_eq!(read.records.len(), 1, "the intact prefix survives");
        assert_eq!(read.records[0]["seq"], 0);
        assert!(read.torn_tail, "the torn line is reported, not hidden");
        assert!(!read.mid_gap);
    }

    #[test]
    fn damage_between_records_is_flagged_as_a_gap() {
        let dir = tempfile::tempdir().unwrap();
        let journal = HistoryJournal::new(path_in(dir.path()));
        journal
            .append(&serde_json::json!({"k": "append", "n": 1}))
            .unwrap();
        let mut raw = std::fs::read_to_string(journal.path()).unwrap();
        raw.push_str("{corrupt\n");
        std::fs::write(journal.path(), &raw).unwrap();
        journal
            .append(&serde_json::json!({"k": "append", "n": 2}))
            .unwrap();
        let read = journal.read();
        assert_eq!(
            read.records.len(),
            1,
            "the suffix after damage is not trusted"
        );
        assert!(read.mid_gap);
        assert!(!read.torn_tail);
    }
}
