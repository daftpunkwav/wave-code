//! Edit tool: exact-string file edits with atomic write and uniqueness checks.

use super::*;

pub struct EditFile {
    ledger: FileLedger,
}

impl EditFile {
    pub(crate) fn new(ledger: FileLedger) -> Self {
        Self { ledger }
    }
}

impl Default for EditFile {
    fn default() -> Self {
        Self::new(FileLedger::new())
    }
}

#[async_trait::async_trait]
impl Tool for EditFile {
    fn name(&self) -> &str {
        "edit"
    }

    fn kind(&self) -> wavecode_protocol::ToolKind {
        wavecode_protocol::ToolKind::FileEdit
    }

    fn description(&self) -> &str {
        "Replace an exact string in a file inside the working directory. \
         old_string must match exactly one location; include more context to make it unique."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Path of the file to edit, relative to the working directory"
                },
                "old_string": {
                    "type": "string",
                    "description": "Exact text to replace; must occur exactly once in the file"
                },
                "new_string": {
                    "type": "string",
                    "description": "Replacement text"
                }
            },
            "required": ["path", "old_string", "new_string"]
        })
    }

    fn is_read_only(&self) -> bool {
        false
    }

    async fn execute(&self, input: Value, ctx: &ToolCtx) -> Result<ToolOutput> {
        let path = match req_str(&input, "path") {
            Ok(p) => p,
            Err(out) => return Ok(out),
        };
        let old_string = match req_str(&input, "old_string") {
            Ok(s) => s,
            Err(out) => return Ok(out),
        };
        let new_string = match req_str(&input, "new_string") {
            Ok(s) => s,
            Err(out) => return Ok(out),
        };
        if old_string.is_empty() {
            return Ok(err_output("old_string must not be empty"));
        }
        let path = match resolve_path(ctx, path)? {
            Ok(p) => p,
            Err(out) => return Ok(out),
        };
        // Input guard: symmetric with read, files over 4 MB are not read wholesale
        // (edit needs full-text matching; large files should use a chunked rewrite strategy).
        let meta = match tokio::fs::metadata(&path).await {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(err_output(format!("file not found: {}", path.display())));
            }
            Err(e) => return Err(e.into()),
        };
        if meta.is_dir() {
            // Business-level branch mirroring read: a directory path returns a
            // self-correctable error message instead of an implementation-level Err
            // (reading a directory yields a non-NotFound io error).
            return Ok(err_output(format!(
                "path is a directory, cannot edit: {}",
                path.display()
            )));
        }
        if meta.len() > MAX_READ_BYTES {
            return Ok(err_output(format!(
                "file too large ({} bytes), refusing to edit: {}",
                meta.len(),
                path.display()
            )));
        }
        // Freshness guard: an edit decided against the session's last view
        // must not apply onto an outside change that landed after that view
        // (see [`FileLedger`]).
        if let Some(out) = self.ledger.check_fresh(&path).await {
            return Ok(out);
        }
        let bytes = match tokio::fs::read(&path).await {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(err_output(format!("file not found: {}", path.display())));
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
        match text.matches(old_string).count() {
            0 => {
                // Fuzzy fallback: when the exact string is absent, try a
                // whitespace-insensitive whole-line block match. This
                // recovers the common model mistake of drifting
                // indentation while staying bounded and honest — the
                // result names the fallback that fired.
                if meta.len() > FUZZY_MAX_BYTES {
                    return Ok(err_output(format!(
                        "old_string not found in {} (file too large for the fuzzy fallback)",
                        path.display()
                    )));
                }
                match fuzzy_line_replace(&text, old_string, new_string) {
                    FuzzyOutcome::Replaced { updated } => {
                        super::atomic_write(&path, &updated).await?;
                        self.ledger.refresh(&path).await;
                        Ok(ok_output(format!(
                            "replaced 1 occurrence in {} (fuzzy line match: whitespace-insensitive; retry with exact text to pin the match)",
                            path.display()
                        )))
                    }
                    FuzzyOutcome::Ambiguous { matches } => Ok(err_output(format!(
                        "old_string is not unique in {} even whitespace-insensitively: {matches} candidate blocks",
                        path.display()
                    ))),
                    FuzzyOutcome::NearMiss { line, similarity } => Ok(err_output(format!(
                        "old_string not found in {}; closest line-block near line {line} ({similarity}% of lines match after trimming whitespace) - read the exact text there and retry",
                        path.display()
                    ))),
                    FuzzyOutcome::NotFound => Ok(err_output(format!(
                        "old_string not found in {}",
                        path.display()
                    ))),
                }
            }
            1 => {
                let updated = text.replacen(old_string, new_string, 1);
                // Atomic write: the original file stays intact when the read succeeds but the write fails (temp+rename).
                super::atomic_write(&path, &updated).await?;
                self.ledger.refresh(&path).await;
                Ok(ok_output(format!(
                    "replaced 1 occurrence in {}",
                    path.display()
                )))
            }
            n => Ok(err_output(format!(
                "old_string is not unique in {}: {n} matches",
                path.display()
            ))),
        }
    }
}

/// Max file bytes for the fuzzy fallback: the scan is O(file_lines x
/// old_lines) with per-line trims, bounded to keep worst-case cost sane.
const FUZZY_MAX_BYTES: u64 = 1024 * 1024;
/// Minimum fraction of trimmed-equal lines for a near-miss hint to name a
/// candidate block (below this, the candidate is noise, not guidance).
const NEAR_MISS_MIN_RATIO: f64 = 0.7;

/// Outcome of the fuzzy line-block fallback.
#[derive(Debug)]
enum FuzzyOutcome {
    /// Exactly one whitespace-insensitive block matched; carries the fully
    /// updated file text.
    Replaced {
        /// File content with the matched window replaced by `new_string`.
        updated: String,
    },
    /// Two or more blocks match whitespace-insensitively: ambiguous, the
    /// exact-match error stands.
    Ambiguous {
        /// Number of candidate blocks.
        matches: usize,
    },
    /// No exact match, but one block clears [`NEAR_MISS_MIN_RATIO`]: name
    /// it so the model can re-read the real text and retry precisely.
    NearMiss {
        /// 1-based line number where the candidate block starts.
        line: usize,
        /// Percentage (0-100) of lines equal after trimming.
        similarity: u8,
    },
    /// Nothing close enough to hint at.
    NotFound,
}

/// Whitespace-insensitive whole-line block replacement.
///
/// `old_string` must span whole lines: the file is split into lines
/// (endings kept), a window of old_string's line count matches when every
/// line pair is equal after `trim()`, and the replacement keeps the
/// window's line-ending style (a trailing-newline-less window at EOF stays
/// newline-less). Mid-line snippets never fuzzy-match - they fall through
/// to [`FuzzyOutcome::NotFound`] so the exact error stays truthful.
fn fuzzy_line_replace(text: &str, old_string: &str, new_string: &str) -> FuzzyOutcome {
    let file_lines: Vec<&str> = text.split_inclusive('\n').collect();
    let old_lines: Vec<&str> = old_string
        .split_inclusive('\n')
        .map(|l| l.strip_suffix('\n').unwrap_or(l))
        .collect();
    let trimmed_old: Vec<&str> = old_lines.iter().map(|l| l.trim()).collect();

    if old_lines.is_empty() || file_lines.len() < old_lines.len() {
        return FuzzyOutcome::NotFound;
    }
    let windows = file_lines.len() - old_lines.len() + 1;
    let equal_counts: Vec<usize> = (0..windows)
        .map(|start| {
            (0..old_lines.len())
                .filter(|&i| file_lines[start + i].trim() == trimmed_old[i])
                .count()
        })
        .collect();
    let full: Vec<usize> = equal_counts
        .iter()
        .enumerate()
        .filter(|(_, eq)| **eq == old_lines.len())
        .map(|(start, _)| start)
        .collect();
    match full.len() {
        1 => {
            let start = full[0];
            // Ending style: borrow the window's first line's ending (or the
            // preceding line's when the window starts the file); a window
            // ending at EOF without a newline stays newline-less.
            let ending = match file_lines[start].strip_suffix('\n') {
                Some(first) if first.ends_with('\r') => "\r\n",
                Some(_) => "\n",
                None if start == 0 => "",
                None => match file_lines[start - 1].strip_suffix('\n') {
                    Some(prev) if prev.ends_with('\r') => "\r\n",
                    Some(_) => "\n",
                    None => "",
                },
            };
            let window_ended_with_newline = file_lines[start + old_lines.len() - 1].ends_with('\n');
            let body = new_string
                .strip_suffix('\n')
                .unwrap_or(new_string)
                .trim_end_matches('\r');
            let mut replacement = body
                .split('\n')
                .map(|l| l.trim_end_matches('\r'))
                .collect::<Vec<_>>()
                .join(ending);
            if window_ended_with_newline {
                replacement.push_str(ending);
            }
            let offset: usize = file_lines[..start].iter().map(|l| l.len()).sum();
            let window_len: usize = file_lines[start..start + old_lines.len()]
                .iter()
                .map(|l| l.len())
                .sum();
            let mut updated = String::with_capacity(text.len() + replacement.len());
            updated.push_str(&text[..offset]);
            updated.push_str(&replacement);
            updated.push_str(&text[offset + window_len..]);
            FuzzyOutcome::Replaced { updated }
        }
        n if n > 1 => FuzzyOutcome::Ambiguous { matches: n },
        _ => {
            // Near-miss hint: the best window by equal-line count, only
            // when it clears the ratio (below it, the candidate is noise).
            let (best_start, best_equal) = equal_counts
                .iter()
                .enumerate()
                .max_by_key(|(_, eq)| **eq)
                .map(|(start, &eq)| (start, eq))
                .unwrap_or((0, 0));
            let ratio = best_equal as f64 / old_lines.len() as f64;
            if ratio >= NEAR_MISS_MIN_RATIO {
                FuzzyOutcome::NearMiss {
                    line: best_start + 1,
                    similarity: (ratio * 100.0) as u8,
                }
            } else {
                FuzzyOutcome::NotFound
            }
        }
    }
}

#[cfg(test)]
mod fuzzy_tests {
    use super::*;

    #[test]
    fn fuzzy_recovers_indentation_drift() {
        let text = "fn main() {\n    let a = 1;\n    if a > 0 {\n        go();\n    }\n}\n";
        // Model supplies the block with drifted indentation.
        let old = "if a > 0 {\n    go();\n}";
        let out = fuzzy_line_replace(text, old, "if a > 0 {\n        go();\n    }");
        match out {
            FuzzyOutcome::Replaced { updated } => {
                assert!(
                    updated.contains("        go();\n    }\n}"),
                    "window replaced, rest intact: {updated}"
                );
                assert!(updated.starts_with("fn main() {"), "prefix untouched");
            }
            other => panic!("expected Replaced, got {other:?}"),
        }
    }

    #[test]
    fn fuzzy_two_candidate_blocks_are_ambiguous() {
        let text = "alpha\n  body\nbeta\n  body\n";
        let out = fuzzy_line_replace(text, "body", "x");
        assert!(
            matches!(out, FuzzyOutcome::Ambiguous { matches: 2 }),
            "{out:?}"
        );
    }

    #[test]
    fn fuzzy_near_miss_names_the_closest_line() {
        let text = "x1\nL1\nL2\nL3\nL4\nx2\n";
        // 3 of 4 window lines match after trimming (75% >= 70%).
        let old = "L1\nL2\nL3\nL4x";
        let out = fuzzy_line_replace(text, old, "x");
        match out {
            FuzzyOutcome::NearMiss { line, similarity } => {
                assert_eq!(line, 2);
                assert_eq!(similarity, 75);
            }
            other => panic!("expected NearMiss, got {other:?}"),
        }
    }

    #[test]
    fn fuzzy_mid_line_snippets_never_match() {
        let text = "hello world\n";
        let out = fuzzy_line_replace(text, "hello", "x");
        assert!(matches!(out, FuzzyOutcome::NotFound), "{out:?}");
    }

    #[test]
    fn fuzzy_preserves_crlf_style_of_the_window() {
        let text = "a\r\nif x {\r\n    go();\r\n}\r\nb\r\n";
        let old = "if x {\n  go();\n}";
        let out = fuzzy_line_replace(text, old, "if x {\n    go();\n}");
        match out {
            FuzzyOutcome::Replaced { updated } => {
                assert!(
                    updated.contains("if x {\r\n    go();\r\n}\r\n"),
                    "{updated}"
                );
                assert!(
                    updated.starts_with("a\r\n"),
                    "untouched lines keep their endings"
                );
                assert_eq!(
                    updated.matches("\r\n").count(),
                    5,
                    "no ending churn: {updated}"
                );
            }
            other => panic!("expected Replaced, got {other:?}"),
        }
    }

    #[test]
    fn fuzzy_eof_window_stays_newline_less() {
        let text = "head\nold body";
        // Extra surrounding whitespace only: the fallback is
        // whitespace-insensitive, never case-insensitive.
        let out = fuzzy_line_replace(text, " old body ", "new body");
        match out {
            FuzzyOutcome::Replaced { updated } => {
                assert_eq!(updated, "head\nnew body");
            }
            other => panic!("expected Replaced, got {other:?}"),
        }
    }

    /// End-to-end: a 0-match exact search now succeeds through the fuzzy
    /// path and the output says so, keeping the fallback honest.
    #[tokio::test]
    async fn execute_falls_back_to_fuzzy_and_names_it() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("code.rs");
        std::fs::write(&file, "fn a() {\n    one();\n}\n").unwrap();
        let ctx = ToolCtx {
            cwd: dir.path().to_path_buf(),
            deny_env: Vec::new(),
        };
        let out = EditFile::default()
            .execute(
                serde_json::json!({
                    "path": "code.rs",
                    "old_string": "fn a() {\n  one();\n}",
                    "new_string": "fn a() {\n    one();\n    two();\n}"
                }),
                &ctx,
            )
            .await
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("fuzzy line match"), "{}", out.content);
        let written = std::fs::read_to_string(&file).unwrap();
        assert!(written.contains("    two();\n"), "{written}");
    }
}
