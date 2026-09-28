//! Persistent input history: append-only JSONL per frontend surface.
//!
//! One `{"content": "…"}` object per line, loaded newest-last at
//! startup (the editor browses newest-first) and appended on submit.
//! The file is plaintext and append-only (like shell history): it can
//! grow across sessions and contains whatever the user submitted, so
//! treat it as sensitive-local data. `load` caps at [`MAX_ENTRIES`];
//! trimming on disk happens lazily through that read path.

use std::io::{BufRead as _, Write as _};
use std::path::PathBuf;

/// Maximum entries loaded and kept per file when no configured limit
/// applies.
pub const MAX_ENTRIES: usize = 100;

/// The history file for a named surface (e.g. `console`).
pub fn history_path(home: &std::path::Path, surface: &str) -> PathBuf {
    home.join(".wavecode")
        .join("input-history")
        .join(format!("{surface}.jsonl"))
}

/// Load history entries, oldest first, capped at `limit`: `None` falls
/// back to [`MAX_ENTRIES`], `Some(0)` loads nothing. Malformed lines are
/// skipped; a missing file yields an empty list.
pub fn load(path: &std::path::Path, limit: Option<usize>) -> Vec<String> {
    let limit = limit.unwrap_or(MAX_ENTRIES);
    let Ok(file) = std::fs::File::open(path) else {
        return Vec::new();
    };
    let reader = std::io::BufReader::new(file);
    let mut entries: Vec<String> = reader
        .lines()
        .map_while(Result::ok)
        .filter_map(|line| {
            let value: serde_json::Value = serde_json::from_str(&line).ok()?;
            value.get("content")?.as_str().map(|s| s.to_string())
        })
        .collect();
    if entries.len() > limit {
        entries = entries.split_off(entries.len() - limit);
    }
    entries
}

/// Append one entry (ignoring failures: history is best-effort).
pub fn append(path: &std::path::Path, entry: &str) {
    if entry.trim().is_empty() {
        return;
    }
    if let Some(parent) = path.parent()
        && let Err(_ /* best effort: history is optional */) = std::fs::create_dir_all(parent)
    {
        return;
    }
    // Owner-only on Unix: the file carries everything the user submitted
    // (the module doc's sensitive-local data), through the persistence
    // crate's shared private-file primitive (the console's dependency set
    // is locked, so the policy arrives as a re-export, not a new edge).
    if let Ok(mut file) = state_persistence::open_append_private(path) {
        let value = serde_json::json!({ "content": entry });
        let _ = writeln!(file, "{value}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_through_jsonl() {
        let temp =
            std::env::temp_dir().join(format!("wc-history-test-{}.jsonl", std::process::id()));
        let _ = std::fs::remove_file(&temp);
        append(&temp, "first prompt");
        append(&temp, "second prompt");
        append(&temp, "   ");
        let entries = load(&temp, None);
        assert_eq!(entries, vec!["first prompt", "second prompt"]);
        let _ = std::fs::remove_file(&temp);
    }

    #[test]
    fn missing_file_loads_empty() {
        let entries = load(std::path::Path::new("/nonexistent/history.jsonl"), None);
        assert!(entries.is_empty());
    }

    /// A configured limit trims the tail of the load; `None` falls back to
    /// the built-in cap and `Some(0)` loads nothing.
    #[test]
    fn load_honors_a_configured_limit() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("history.jsonl");
        for i in 0..10 {
            append(&path, &format!("entry-{i}"));
        }
        assert_eq!(load(&path, None).len(), 10);
        let entries = load(&path, Some(3));
        assert_eq!(entries, vec!["entry-7", "entry-8", "entry-9"]);
        // An explicit zero is expressible: load nothing.
        assert!(load(&path, Some(0)).is_empty());
    }

    #[test]
    fn malformed_lines_are_skipped() {
        let temp =
            std::env::temp_dir().join(format!("wc-history-bad-{}.jsonl", std::process::id()));
        std::fs::write(&temp, "{\"content\": \"good\"}\nnot json\n{\"other\": 1}\n").unwrap();
        let entries = load(&temp, None);
        assert_eq!(entries, vec!["good"]);
        let _ = std::fs::remove_file(&temp);
    }

    #[test]
    fn load_caps_entries() {
        let temp =
            std::env::temp_dir().join(format!("wc-history-cap-{}.jsonl", std::process::id()));
        let mut body = String::new();
        for i in 0..150 {
            body.push_str(&format!("{{\"content\": \"{i}\"}}\n"));
        }
        std::fs::write(&temp, body).unwrap();
        let entries = load(&temp, None);
        assert_eq!(entries.len(), MAX_ENTRIES);
        assert_eq!(entries.first().unwrap(), "50");
        let _ = std::fs::remove_file(&temp);
    }
}
