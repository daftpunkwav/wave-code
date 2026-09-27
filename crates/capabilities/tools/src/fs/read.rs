//! Read tool: file contents with line/byte budgets and truncation markers.

use super::*;

pub struct ReadFile {
    ledger: FileLedger,
}

impl ReadFile {
    pub(crate) fn new(ledger: FileLedger) -> Self {
        Self { ledger }
    }
}

impl Default for ReadFile {
    fn default() -> Self {
        Self::new(FileLedger::new())
    }
}

#[async_trait::async_trait]
impl Tool for ReadFile {
    fn name(&self) -> &str {
        "read"
    }

    fn description(&self) -> &str {
        "Read a text file inside the working directory. Output is capped at 2000 lines \
         / 50KB and marked [truncated] when cut. Use offset/limit to page through large files; \
         a negative offset counts lines back from the end (offset -50 = last 50 lines)."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Path of the file to read, relative to the working directory"
                },
                "offset": {
                    "type": "integer",
                    "description": "0-based line number to start reading from (default 0); \
                                    negative counts back from the end of the file (-50 = last 50 lines)"
                },
                "limit": {
                    "type": "integer",
                    "description": "Maximum number of lines to return (default 2000, hard cap 2000)"
                }
            },
            "required": ["path"]
        })
    }

    fn is_read_only(&self) -> bool {
        true
    }

    async fn execute(&self, input: Value, ctx: &ToolCtx) -> Result<ToolOutput> {
        let path = match req_str(&input, "path") {
            Ok(p) => p,
            Err(out) => return Ok(out),
        };
        let path = match resolve_path(ctx, path)? {
            Ok(p) => p,
            Err(out) => return Ok(out),
        };
        // Stat metadata first: directory/file mismatches and oversized files branch explicitly into business output without guessing ErrorKind.
        let meta = match tokio::fs::metadata(&path).await {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(err_output(missing_file_message(&path).await));
            }
            Err(e) => return Err(e.into()),
        };
        if meta.is_dir() {
            // There is no `ls` tool; `glob` or the shell lists directories.
            return Ok(err_output(format!(
                "path is a directory, use glob (pattern \"{}/*\") or the shell to list it: {}",
                path.file_name().and_then(|n| n.to_str()).unwrap_or("."),
                path.display()
            )));
        }
        // Input guard: files over 4 MB are not read wholesale; tell the model to page through them.
        if meta.len() > MAX_READ_BYTES {
            return Ok(err_output(format!(
                "file too large ({} bytes), use offset/limit to read parts: {}",
                meta.len(),
                path.display()
            )));
        }
        let bytes = match tokio::fs::read(&path).await {
            Ok(b) => b,
            // A TOCTOU window exists between metadata and read, so keep the NotFound branch.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(err_output(missing_file_message(&path).await));
            }
            Err(e) => return Err(e.into()),
        };
        let text = match String::from_utf8(bytes) {
            Ok(t) => t,
            Err(_) => {
                return Ok(err_output(format!(
                    "file is not valid UTF-8 (binary file?): {}",
                    path.display()
                )));
            }
        };

        // Negative offset counts back from the end (-50 = last 50 lines);
        // a positive or zero offset pages forward from the top.
        let raw_offset = match opt_offset(&input, "offset") {
            Ok(o) => o.unwrap_or(0),
            Err(out) => return Ok(out),
        };
        let tail = raw_offset < 0;
        let offset = raw_offset.unsigned_abs() as usize;
        let limit = match opt_usize(&input, "limit") {
            Ok(l) => l.unwrap_or(MAX_LINES),
            Err(out) => return Ok(out),
        };
        if limit == 0 {
            return Ok(err_output("invalid parameter 'limit' (must be >= 1)"));
        }
        let limit = limit.min(MAX_LINES);
        let total = text.lines().count();
        // Bounds guard: a forward offset past the file is a business failure
        // here, ruling out a downstream usize underflow; a tail offset past
        // the head simply clamps to the whole file.
        if !tail && offset > 0 && offset >= total {
            return Ok(err_output(format!(
                "offset {offset} is beyond end of file ({total} lines)"
            )));
        }
        let start = if tail {
            total.saturating_sub(offset)
        } else {
            offset
        };
        let end = start.saturating_add(limit).min(total);
        // Without slicing return the text as-is, preserving original newlines and the ending; with slicing rejoin lines with \n.
        let mut content = if !tail && start == 0 && end == total {
            text
        } else {
            text.lines()
                .skip(start)
                .take(end.saturating_sub(start))
                .collect::<Vec<_>>()
                .join("\n")
        };
        // A tail read deliberately shows the end: name the skipped head so
        // the model knows earlier lines exist and how to reach them.
        if tail && start > 0 {
            content = format!("[showing lines {}-{} of {}]\n{}", start + 1, end, total, content);
        }
        let mut truncated = !tail && end < total;
        if content.len() > MAX_BYTES {
            let mut cut = MAX_BYTES;
            while !content.is_char_boundary(cut) {
                cut -= 1;
            }
            content.truncate(cut);
            truncated = true;
        }
        if truncated {
            content.push_str("\n[truncated]");
        }
        // The content just returned becomes the session's baseline for
        // this file: later write/edit calls refuse to run over an outside
        // change that landed after this point.
        self.ledger.record(&path, &meta);
        Ok(ok_output(content))
    }
}

/// Parse the optional line offset: non-negative pages forward from the top,
/// negative counts back from the end. Present-but-non-integer values yield
/// a business-failure output (mirrors [`opt_usize`], signed).
fn opt_offset(input: &Value, key: &str) -> std::result::Result<Option<i64>, ToolOutput> {
    match input.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v.as_i64().map(Some).ok_or_else(|| {
            err_output(format!("invalid parameter '{key}' (integer required)"))
        }),
    }
}

/// Directory entries scanned before suggestion search gives up: a miss in
/// a huge directory costs one bounded listing, not a full walk.
const SUGGEST_SCAN_CAP: usize = 1000;
/// Suggestions shown per miss.
const SUGGESTION_COUNT: usize = 3;

/// NotFound message that names the closest sibling file names when the
/// directory holds any: a typo'd path is self-correctable only if the
/// model learns the real name. No siblings (or an unreadable directory)
/// degrade to the plain message.
async fn missing_file_message(path: &std::path::Path) -> String {
    let message = format!("file not found: {}", path.display());
    let (Some(parent), Some(file_name)) = (path.parent(), path.file_name())
        else {
            return message;
        };
    let Some(file_name) = file_name.to_str() else {
        return message;
    };
    let Ok(mut entries) = tokio::fs::read_dir(parent).await else {
        return message;
    };
    // A name is a candidate when its lowercase edit distance to the miss
    // fits a small budget derived from the miss's length; a length gap past
    // the budget can never pass, so candidates skip the DP outright.
    let budget = (file_name.len() / 3).clamp(1, 4);
    let lowered = file_name.to_lowercase();
    // The gap filter compares char counts, not byte counts: edit_distance
    // counts chars and a char-count gap lower-bounds it, while a byte gap
    // does not (multibyte names diverge) and would wrongly drop neighbors.
    let lowered_chars = lowered.chars().count();
    let mut candidates: Vec<(usize, String)> = Vec::new();
    let mut scanned = 0usize;
    while let Ok(Some(entry)) = entries.next_entry().await {
        scanned += 1;
        if scanned > SUGGEST_SCAN_CAP {
            break;
        }
        let entry_name = entry.file_name();
        let Some(name) = entry_name.to_str() else {
            continue;
        };
        if name == file_name {
            continue;
        }
        let lowered_name = name.to_lowercase();
        if lowered_name.chars().count().abs_diff(lowered_chars) > budget {
            continue;
        }
        let distance = edit_distance(&lowered, &lowered_name, budget);
        if distance <= budget {
            candidates.push((distance, name.to_string()));
        }
    }
    if candidates.is_empty() {
        return message;
    }
    candidates.sort();
    candidates.dedup();
    candidates.truncate(SUGGESTION_COUNT);
    format!(
        "{message}; did you mean: {}?",
        candidates
            .iter()
            .map(|(_, name)| name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    )
}

/// Bounded Levenshtein distance: aborts with `budget + 1` once the running
/// row minimum proves the names differ by more than `budget`, so the scan
/// stays O(name_len x budget) instead of a full DP per sibling.
fn edit_distance(a: &str, b: &str, budget: usize) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut previous: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.iter().enumerate() {
        let mut current = vec![i + 1];
        let mut row_min = current[0];
        for (j, cb) in b.iter().enumerate() {
            let substitute = previous[j] + usize::from(ca != cb);
            let value = (previous[j + 1] + 1).min(substitute);
            let value = value.min(current[j] + 1);
            current.push(value);
            row_min = row_min.min(value);
        }
        if row_min > budget {
            return budget + 1;
        }
        previous = current;
    }
    previous[b.len()]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Tool, ToolCtx};

    fn ctx() -> (tempfile::TempDir, ToolCtx) {
        let dir = tempfile::tempdir().unwrap();
        let c = ToolCtx {
            cwd: dir.path().to_path_buf(),
            deny_env: Vec::new(),
        };
        (dir, c)
    }

    fn hundred_lines() -> String {
        (1..=100).map(|i| format!("line{i}\n")).collect()
    }

    #[tokio::test]
    async fn negative_offset_reads_the_tail() {
        let (_d, c) = ctx();
        std::fs::write(c.cwd.join("log.txt"), hundred_lines()).unwrap();
        let out = ReadFile::default()
            .execute(serde_json::json!({"path": "log.txt", "offset": -3}), &c)
            .await
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.starts_with("[showing lines 98-100 of 100]\n"), "{}", out.content);
        assert!(out.content.contains("line100"));
        assert!(!out.content.contains("line1\n"), "head is skipped: {}", out.content);
    }

    #[tokio::test]
    async fn tail_offset_beyond_the_file_clamps_to_the_whole_file() {
        let (_d, c) = ctx();
        std::fs::write(c.cwd.join("log.txt"), hundred_lines()).unwrap();
        let out = ReadFile::default()
            .execute(serde_json::json!({"path": "log.txt", "offset": -100_000}), &c)
            .await
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("line1\n") && out.content.contains("line100"));
        assert!(!out.content.contains("[showing lines"), "no head was skipped");
    }

    #[tokio::test]
    async fn tail_offset_combines_with_limit() {
        let (_d, c) = ctx();
        std::fs::write(c.cwd.join("log.txt"), hundred_lines()).unwrap();
        let out = ReadFile::default()
            .execute(serde_json::json!({"path": "log.txt", "offset": -30, "limit": 2}), &c)
            .await
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.starts_with("[showing lines 71-72 of 100]\n"), "{}", out.content);
        assert!(out.content.contains("line72\n") || out.content.ends_with("line72"));
    }

    #[tokio::test]
    async fn forward_offset_behavior_is_unchanged() {
        let (_d, c) = ctx();
        std::fs::write(c.cwd.join("log.txt"), hundred_lines()).unwrap();
        let out = ReadFile::default()
            .execute(serde_json::json!({"path": "log.txt", "offset": 98}), &c)
            .await
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        assert_eq!(out.content, "line99\nline100");
        // The old failure mode still fails: a forward offset past the end.
        let out = ReadFile::default()
            .execute(serde_json::json!({"path": "log.txt", "offset": 100}), &c)
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(out.content.contains("beyond end of file"));
    }

    #[tokio::test]
    async fn missing_file_suggests_close_siblings() {
        let (_d, c) = ctx();
        std::fs::write(c.cwd.join("main.rs"), "fn main() {}\n").unwrap();
        std::fs::write(c.cwd.join("lib.rs"), "").unwrap();
        let out = ReadFile::default()
            .execute(serde_json::json!({"path": "main,rs"}), &c)
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(out.content.contains("did you mean"), "{}", out.content);
        assert!(out.content.contains("main.rs"), "{}", out.content);
        // A name nothing like the siblings stays suggestion-free.
        let out = ReadFile::default()
            .execute(serde_json::json!({"path": "zzzzzzzz.qqq"}), &c)
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(!out.content.contains("did you mean"), "{}", out.content);
        assert!(out.content.starts_with("file not found:"));
    }

    /// The gap filter must compare char counts, not byte counts: a sibling
    /// two CJK chars longer is 6 bytes longer (past the byte-derived budget)
    /// yet only 2 edits away, so a byte gap would wrongly drop the suggestion.
    #[tokio::test]
    async fn missing_file_suggests_multibyte_siblings() {
        let (_d, c) = ctx();
        std::fs::write(c.cwd.join("报告文档.txt"), "x").unwrap();
        let out = ReadFile::default()
            .execute(serde_json::json!({"path": "报告.txt"}), &c)
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(out.content.contains("did you mean"), "{}", out.content);
        assert!(out.content.contains("报告文档.txt"), "{}", out.content);
    }

    #[test]
    fn edit_distance_is_bounded_and_correct() {
        assert_eq!(edit_distance("main,rs", "main.rs", 4), 1);
        // kitten -> sitting takes 3 edits; past the budget the scan aborts
        // with budget + 1 instead of finishing the DP.
        assert_eq!(edit_distance("kitten", "sitting", 2), 3);
        assert_eq!(edit_distance("kitten", "sitting", 3), 3);
        assert_eq!(edit_distance("kitten", "kitten", 4), 0);
        assert_eq!(edit_distance("", "abc", 4), 3);
        assert_eq!(edit_distance("abc", "", 4), 3);
    }
}
