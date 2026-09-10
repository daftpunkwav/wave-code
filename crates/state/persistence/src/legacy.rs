/*!
 * @file LegacyRollout
 * @description Tolerant reader for legacy engine rollout journals.
 *
 * Responsibilities:
 * - List legacy thread journals with summaries for resume menus.
 * - Load message histories as plain text pairs for new conversations.
 * - Skip corrupt lines explicitly instead of failing whole files.
 *
 * This module must not depend on: the legacy engine crate. It reads the
 * frozen JSONL layout directly (one record per line, `kind` tagged) so
 * old sessions resume after the engine is deleted.
 */

//! Legacy import: frozen format knowledge, tolerant parsing.
//!
//! Format (one JSON object per line): `{"kind":"message","seq":N,
//! "message":{...}}` or `{"kind":"compaction","seq":N,"messages":[...]}`.
//! Compaction records reset history like the legacy replay did; corrupt
//! lines stop the scan with earlier records kept.

use std::path::{Path, PathBuf};

use wavecode_llm::{ContentBlock, Message, Role};

/// Threads directory name under the user home.
pub const THREADS_DIR: &str = "threads";

/// One legacy thread summary for resume menus.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LegacyThread {
    /// Thread id (journal file stem).
    pub thread_id: String,
    /// Message records seen.
    pub message_count: usize,
    /// Compaction records seen.
    pub compaction_count: usize,
    /// First user text for previews, if any user message exists.
    pub first_user_text: Option<String>,
    /// Journal modification time for recency sorting.
    pub modified: std::time::SystemTime,
}

/// Legacy import failures.
#[derive(Debug, thiserror::Error)]
pub enum LegacyError {
    /// Filesystem failure listing or reading journals.
    #[error("legacy journal IO failed: {0}")]
    Io(#[from] std::io::Error),
    /// Thread id rejected by the path-escape whitelist.
    #[error("invalid thread id: {0:?}")]
    InvalidId(String),
}

/// Legacy record mirror: trigger shapes stay unparsed on purpose so
/// unknown trigger values degrade to skipped lines, not hard failures.
///
/// Sequence numbers ride along for format fidelity (the importer folds
/// in file order); they are intentionally unread.
#[derive(Debug, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[allow(dead_code)]
enum LegacyRecord {
    /// One history message.
    Message {
        /// Record sequence number.
        seq: u64,
        /// Message body.
        message: Message,
    },
    /// Post-compaction full history replacing everything before it.
    Compaction {
        /// Record sequence number.
        seq: u64,
        /// Full replacement history.
        messages: Vec<Message>,
    },
}

/// Thread id whitelist: non-empty, bounded, ASCII alphanumerics plus
/// `-` and `_`, blocking `../` escapes from CLI arguments.
pub fn is_valid_thread_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Production threads root: `~/.wavecode/threads/`.
pub fn default_root(home: &Path) -> PathBuf {
    home.join(".wavecode").join(THREADS_DIR)
}

/// Journal path for one thread id, rejecting escapes.
fn journal_path(root: &Path, thread_id: &str) -> Result<PathBuf, LegacyError> {
    if !is_valid_thread_id(thread_id) {
        return Err(LegacyError::InvalidId(thread_id.to_string()));
    }
    Ok(root.join(format!("{thread_id}.jsonl")))
}

/// Render one legacy message as plain text for new conversations.
///
/// Text blocks pass through; tool calls collapse to one bracketed line
/// each so pairing structure survives without the old block schema.
fn message_text(message: &Message) -> (bool, String) {
    let from_model = matches!(message.role, Role::Assistant);
    let mut parts = Vec::new();
    for block in &message.content {
        match block {
            ContentBlock::Text { text } => parts.push(text.clone()),
            ContentBlock::ToolUse { name, .. } => parts.push(format!("[tool:{name}]")),
            ContentBlock::ToolResult {
                content, is_error, ..
            } => {
                if *is_error {
                    parts.push(format!("[error:{content}]"));
                } else {
                    parts.push(content.clone());
                }
            }
        }
    }
    (from_model, parts.join("\n"))
}

/// Load one thread history as (from_model, text) pairs.
///
/// Compaction records reset accumulated history; corrupt lines end the
/// scan with earlier records kept; missing files read as empty.
pub fn load_history(root: &Path, thread_id: &str) -> Result<Vec<(bool, String)>, LegacyError> {
    let path = journal_path(root, thread_id)?;
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(LegacyError::Io(e)),
    };
    let mut history = Vec::new();
    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let record: LegacyRecord = match serde_json::from_str(line) {
            Ok(record) => record,
            Err(_) => break,
        };
        match record {
            LegacyRecord::Message { message, .. } => {
                let (from_model, text) = message_text(&message);
                if !text.trim().is_empty() {
                    history.push((from_model, text));
                }
            }
            LegacyRecord::Compaction { messages, .. } => {
                history.clear();
                for message in &messages {
                    let (from_model, text) = message_text(message);
                    if !text.trim().is_empty() {
                        history.push((from_model, text));
                    }
                }
            }
        }
    }
    Ok(history)
}

/// List legacy threads newest-first with one history scan each.
pub fn list_threads(root: &Path) -> Result<Vec<LegacyThread>, LegacyError> {
    let entries = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(LegacyError::Io(e)),
    };
    let mut threads = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        if !is_valid_thread_id(stem) {
            continue;
        }
        let modified = entry
            .metadata()
            .and_then(|m| m.modified())
            .unwrap_or(std::time::UNIX_EPOCH);
        let history = load_history(root, stem).unwrap_or_default();
        let mut message_count = 0usize;
        let mut compaction_count = 0usize;
        let mut first_user_text = None;
        // Re-scan raw records for faithful counts (load_history folds
        // compactions away by design).
        if let Ok(text) = std::fs::read_to_string(&path) {
            for line in text.lines() {
                if line.trim().is_empty() {
                    continue;
                }
                match serde_json::from_str::<LegacyRecord>(line) {
                    Ok(LegacyRecord::Message { .. }) => message_count += 1,
                    Ok(LegacyRecord::Compaction { .. }) => compaction_count += 1,
                    Err(_) => break,
                }
            }
        }
        for (from_model, text) in &history {
            if !from_model {
                first_user_text = Some(text.clone());
                break;
            }
        }
        threads.push(LegacyThread {
            thread_id: stem.to_string(),
            message_count,
            compaction_count,
            first_user_text,
            modified,
        });
    }
    threads.sort_by_key(|t| std::cmp::Reverse(t.modified));
    Ok(threads)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn journal(dir: &Path, id: &str, lines: &[&str]) {
        std::fs::write(dir.join(format!("{id}.jsonl")), lines.join("\n")).unwrap();
    }

    fn fixture() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        journal(
            dir.path(),
            "abc-1",
            &[
                r#"{"kind":"message","seq":1,"message":{"role":"user","content":[{"type":"text","text":"hello"}]}}"#,
                r#"{"kind":"message","seq":2,"message":{"role":"assistant","content":[{"type":"text","text":"hi there"}]}}"#,
                r#"{"kind":"compaction","seq":3,"trigger":"auto","summary_tokens":10,"messages":[{"role":"user","content":[{"type":"text","text":"summary"}]}]}"#,
                r#"{"kind":"message","seq":4,"message":{"role":"user","content":[{"type":"text","text":"after"}]}}"#,
                "{broken tail",
            ],
        );
        dir
    }

    #[test]
    fn compaction_resets_and_corrupt_tails_stop() {
        let dir = fixture();
        let history = load_history(dir.path(), "abc-1").unwrap();
        // Compaction at seq 3 resets; corrupt tail ends the scan.
        assert_eq!(
            history,
            vec![(false, "summary".to_string()), (false, "after".to_string()),]
        );
    }

    #[test]
    fn threads_list_newest_first_with_counts() {
        let dir = fixture();
        journal(
            dir.path(),
            "old-9",
            &[
                r#"{"kind":"message","seq":1,"message":{"role":"user","content":[{"type":"text","text":"first"}]}}"#,
            ],
        );
        let threads = list_threads(dir.path()).unwrap();
        assert_eq!(threads.len(), 2);
        let abc = threads.iter().find(|t| t.thread_id == "abc-1").unwrap();
        assert_eq!(abc.message_count, 3);
        assert_eq!(abc.compaction_count, 1);
        assert_eq!(abc.first_user_text.as_deref(), Some("summary"));
    }

    #[test]
    fn ids_reject_path_escapes_and_missing_reads_empty() {
        let dir = fixture();
        assert!(matches!(
            load_history(dir.path(), "../evil").unwrap_err(),
            LegacyError::InvalidId(_)
        ));
        assert!(load_history(dir.path(), "nope").unwrap().is_empty());
        assert!(
            list_threads(&dir.path().join("missing"))
                .unwrap()
                .is_empty()
        );
    }
}
