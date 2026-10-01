/*!
 * @file HistoryJournalBridge
 * @description Durables conversation history as a write-ahead block journal.
 *
 * Responsibilities:
 * - Mirror every committed history mutation into the session's journal.
 * - Rebuild a resumable history from the journal, closing lost tool calls.
 * - Detect gaps a crash or a failed write can leave, rather than hide them.
 *
 * This module must not depend on: the actor, drivers, or any frontend. It is
 * the adapter between the conversation store's mutation seam and the byte
 * journal in state-persistence.
 */

use std::sync::atomic::{AtomicU64, Ordering};

use state_persistence::history::HistoryJournal;
use state_store::{HistoryEntry, HistorySink};

/// Record kind whose absence would reopen a settled tool call.
const UNKNOWN_OUTCOME: &str = "wavecode lost this call's result while the session was down: the tool may \
     or may not have run, so do not assume either — verify before repeating it";

/// Mirrors history mutations into one session's journal.
#[derive(Debug)]
pub struct JournalSink {
    journal: HistoryJournal,
    /// Next sequence number. Records are numbered so a lost write shows up
    /// as a jump: without it, a missing record and an unwritten tail are
    /// indistinguishable, and a silent gap in the middle of the history is
    /// the one outcome that would make a resumed session re-run a tool.
    next_seq: AtomicU64,
}

impl JournalSink {
    /// A sink continuing one journal from `next_seq` (the sequence the last
    /// replayed record used plus one).
    pub fn new(journal: HistoryJournal, next_seq: u64) -> Self {
        Self {
            journal,
            next_seq: AtomicU64::new(next_seq),
        }
    }

    /// Encode and append one record; a failure costs durability only, never
    /// the running turn (history keeps advancing in memory, and the gap
    /// surfaces at the next replay through the sequence jump).
    fn write(&self, record: serde_json::Value) {
        let seq = self.next_seq.fetch_add(1, Ordering::Relaxed);
        let mut record = record;
        if let Some(object) = record.as_object_mut() {
            object.insert("seq".to_string(), serde_json::json!(seq));
        }
        if let Err(error) = self.journal.append(&record) {
            tracing::warn!("history journal write failed (seq {seq}): {error}");
        }
    }
}

impl HistorySink for JournalSink {
    fn appended(&self, entry: &HistoryEntry) {
        self.write(serde_json::json!({"k": "append", "entry": entry}));
    }

    fn replaced(&self, entries: &[HistoryEntry]) {
        self.write(serde_json::json!({"k": "replace", "entries": entries}));
    }
}

/// What a journal replay recovered.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Replayed {
    /// Restored history, with every lost tool call closed.
    pub entries: Vec<HistoryEntry>,
    /// Call ids whose result never reached the journal.
    pub lost_calls: Vec<String>,
    /// True when the log ended in a torn line (the expected crash shape).
    pub torn_tail: bool,
    /// True when records are missing between surviving ones.
    pub gapped: bool,
    /// Sequence the next record must use.
    pub next_seq: u64,
}

/// Fold a journal back into a history.
///
/// Append and replace records are applied in order; the result is then run
/// through [`state_store::close_open_calls`] so no assistant tool call is
/// left unanswered (providers reject an unmatched pair, and the model must
/// not be invited to repeat a call whose side effect may already exist).
pub fn replay_history(journal: &HistoryJournal) -> Replayed {
    let read = journal.read();
    let mut entries: Vec<HistoryEntry> = Vec::new();
    let mut expected_seq: u64 = 0;
    // A corrupt line between records already dropped everything after it.
    // That prefix is verified only when the caller treats the read as gapped.
    let mut gapped = read.mid_gap;
    let mut next_seq = 0u64;
    for record in &read.records {
        let seq = record.get("seq").and_then(serde_json::Value::as_u64);
        // Stop before applying a record whose sequence jumped. Applying it
        // and then closing open calls at the very end puts a synthetic
        // tool result after later messages, which providers reject.
        if seq != Some(expected_seq) {
            gapped = true;
            break;
        }
        match record.get("k").and_then(serde_json::Value::as_str) {
            Some("append") => match decode_entry(record.get("entry")) {
                Some(entry) => entries.push(entry),
                None => {
                    gapped = true;
                    break;
                }
            },
            Some("replace") => match decode_entries(record.get("entries")) {
                Some(replacement) => entries = replacement,
                None => {
                    gapped = true;
                    break;
                }
            },
            // An unknown kind predates this reader's format: stop rather
            // than rebuild a history from records it cannot interpret.
            _ => {
                gapped = true;
                break;
            }
        }
        expected_seq += 1;
        next_seq = expected_seq;
    }
    let (entries, lost_calls) = state_store::close_open_calls(&entries, UNKNOWN_OUTCOME);
    Replayed {
        entries,
        lost_calls,
        torn_tail: read.torn_tail,
        gapped,
        next_seq,
    }
}

fn decode_entry(value: Option<&serde_json::Value>) -> Option<HistoryEntry> {
    serde_json::from_value(value?.clone()).ok()
}

fn decode_entries(value: Option<&serde_json::Value>) -> Option<Vec<HistoryEntry>> {
    serde_json::from_value(value?.clone()).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use state_store::{Block, Conversation, Role};
    use std::sync::Arc;

    fn tool_use(call_id: &str) -> Block {
        Block::ToolUse {
            call_id: call_id.to_string(),
            name: "write".to_string(),
            input: serde_json::json!({"path": "a.txt"}),
        }
    }

    fn tool_result(call_id: &str) -> Block {
        Block::ToolResult {
            call_id: call_id.to_string(),
            content: "written".to_string(),
            is_error: false,
            produced_at: None,
        }
    }

    fn journal_in(dir: &std::path::Path) -> HistoryJournal {
        HistoryJournal::new(dir.join("s1.history.jsonl"))
    }

    /// Every conversation mutation reaches the journal through the store's
    /// own seam, so no append path can be forgotten.
    #[test]
    fn conversation_mutations_are_journaled() {
        let dir = tempfile::tempdir().unwrap();
        let journal = journal_in(dir.path());
        let mut conversation =
            Conversation::with_sink(Arc::new(JournalSink::new(journal.clone(), 0)));
        conversation.push(Role::User, "fix it");
        conversation.push_blocks(
            Role::Assistant,
            vec![Block::Text("on it".to_string()), tool_use("c1")],
        );
        conversation.push_blocks(Role::User, vec![tool_result("c1")]);

        let replayed = replay_history(&journal);
        assert_eq!(replayed.entries.len(), 3);
        assert_eq!(replayed.next_seq, 3);
        assert!(replayed.lost_calls.is_empty());
        // Blocks survive: the whole point versus the text-level journal.
        let assistant = &replayed.entries[1];
        assert_eq!(assistant.role, Role::Assistant);
        assert!(matches!(
            assistant.blocks[1],
            Block::ToolUse { ref call_id, .. } if call_id == "c1"
        ));
    }

    /// A crash after the model asked for a tool, before its result landed:
    /// the resumed history closes the call as unknown-outcome, so the next
    /// sample cannot silently re-issue a write that already happened.
    #[test]
    fn a_crash_mid_round_closes_the_open_call() {
        let dir = tempfile::tempdir().unwrap();
        let journal = journal_in(dir.path());
        let sink = Arc::new(JournalSink::new(journal.clone(), 0));
        let mut conversation = Conversation::with_sink(sink);
        conversation.push(Role::User, "write it");
        conversation.push_blocks(Role::Assistant, vec![tool_use("c1")]);
        // The process dies here: the result push never happens.

        let replayed = replay_history(&journal);
        assert_eq!(replayed.lost_calls, vec!["c1".to_string()]);
        let closing = replayed.entries.last().unwrap();
        assert_eq!(closing.role, Role::User);
        match &closing.blocks[0] {
            Block::ToolResult {
                call_id,
                content,
                is_error,
                ..
            } => {
                assert_eq!(call_id, "c1");
                assert!(is_error, "a lost outcome is never reported as success");
                assert!(content.contains("do not assume"), "{content}");
            }
            other => panic!("expected a closing tool result, got {other:?}"),
        }
        // A resumed session built from the repaired history needs no further
        // closing pass: nothing is left open.
        let mut resumed = Conversation::new();
        for entry in &replayed.entries {
            resumed.push_blocks(entry.role, entry.blocks.clone());
        }
        let still_open =
            resumed.with_entries(|entries| state_store::close_open_calls(entries, "unused").1);
        assert!(still_open.is_empty(), "repaired history reopened");
    }

    /// A replace (compaction, rewind) wins over what came before it, and the
    /// sequence keeps counting across the fold.
    #[test]
    fn replace_records_fold_the_history() {
        let dir = tempfile::tempdir().unwrap();
        let journal = journal_in(dir.path());
        let mut conversation =
            Conversation::with_sink(Arc::new(JournalSink::new(journal.clone(), 0)));
        conversation.push(Role::User, "one");
        conversation.push(Role::Assistant, "two");
        conversation.replace(vec![HistoryEntry {
            role: Role::User,
            blocks: vec![Block::Text("[compacted]".to_string())],
        }]);
        conversation.push(Role::Assistant, "three");

        let replayed = replay_history(&journal);
        assert_eq!(replayed.entries.len(), 2);
        assert_eq!(replayed.entries[0].text(), "[compacted]");
        assert_eq!(replayed.next_seq, 4);
    }

    /// Sequence continuity is the gap detector: a dropped record must be
    /// reported, never mistaken for a complete history.
    #[test]
    fn a_missing_record_is_reported_as_a_gap() {
        let dir = tempfile::tempdir().unwrap();
        let journal = journal_in(dir.path());
        journal
            .append(&serde_json::json!({"k":"append","seq":0,"entry":{"role":"user","blocks":[{"text":"a"}]}}))
            .unwrap();
        journal
            .append(&serde_json::json!({"k":"append","seq":2,"entry":{"role":"assistant","blocks":[{"text":"b"}]}}))
            .unwrap();
        let replayed = replay_history(&journal);
        assert!(replayed.gapped);
        assert_eq!(replayed.entries.len(), 1, "records after the hole stay out");
        assert_eq!(replayed.entries[0].text(), "a");
        assert_eq!(replayed.next_seq, 1, "numbering resumes at the hole");
    }

    /// A hole where a tool result should have been must not keep the later
    /// messages and then tack a synthetic result onto the end.
    #[test]
    fn a_gap_does_not_apply_records_after_the_hole() {
        let dir = tempfile::tempdir().unwrap();
        let journal = journal_in(dir.path());
        journal
            .append(&serde_json::json!({
                "k": "append",
                "seq": 0,
                "entry": {
                    "role": "assistant",
                    "blocks": [{"tool_use": {"call_id": "c1", "name": "shell", "input": {"command": "ls"}}}]
                }
            }))
            .unwrap();
        journal
            .append(&serde_json::json!({
                "k": "append",
                "seq": 2,
                "entry": {"role": "assistant", "blocks": [{"text": "later"}]}
            }))
            .unwrap();
        let replayed = replay_history(&journal);
        assert!(replayed.gapped);
        assert_eq!(replayed.lost_calls, vec!["c1".to_string()]);
        assert_eq!(
            replayed.entries.len(),
            2,
            "tool call plus the closing result"
        );
        assert!(
            matches!(
                replayed.entries.last().map(|entry| entry.blocks.last()),
                Some(Some(Block::ToolResult { call_id, .. })) if call_id == "c1"
            ),
            "the synthetic result stays adjacent to the open call"
        );
    }

    /// A journal written before tool results carried wall-clock stamps has
    /// records without the optional `produced_at` key; those must replay
    /// unchanged instead of failing the whole history.
    #[test]
    fn pre_stamp_records_replay_with_unstamped_results() {
        let dir = tempfile::tempdir().unwrap();
        let journal = journal_in(dir.path());
        journal
            .append(&serde_json::json!({
                "k": "append",
                "seq": 0,
                "entry": {
                    "role": "assistant",
                    "blocks": [{"tool_use": {"call_id": "c1", "name": "write", "input": {}}}]
                }
            }))
            .unwrap();
        journal
            .append(&serde_json::json!({
                "k": "append",
                "seq": 1,
                "entry": {
                    "role": "user",
                    "blocks": [{"tool_result": {"call_id": "c1", "content": "written", "is_error": false}}]
                }
            }))
            .unwrap();
        let replayed = replay_history(&journal);
        assert_eq!(replayed.entries.len(), 2);
        assert!(replayed.lost_calls.is_empty());
        assert!(!replayed.gapped);
        match &replayed.entries[1].blocks[0] {
            Block::ToolResult {
                call_id,
                produced_at,
                ..
            } => {
                assert_eq!(call_id, "c1");
                assert_eq!(*produced_at, None, "absent key loads as unstamped");
            }
            other => panic!("expected a tool result, got {other:?}"),
        }
    }

    #[test]
    fn an_empty_journal_replays_as_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let journal = journal_in(dir.path());
        let replayed = replay_history(&journal);
        assert!(replayed.entries.is_empty());
        assert_eq!(replayed.next_seq, 0);
        assert!(!replayed.gapped && !replayed.torn_tail);
    }
}
