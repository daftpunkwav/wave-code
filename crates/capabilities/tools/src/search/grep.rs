//! Search tools (split from search_tools.rs in phase 3).

use super::*;

pub struct Grep;

#[async_trait::async_trait]
impl Tool for Grep {
    fn name(&self) -> &str {
        "grep"
    }

    fn description(&self) -> &str {
        "Search file contents with a regular expression inside the working directory. \
         Output is one 'path:line:content' per match (paths relative to the working directory), \
         followed by a match/file count footer. Capped at 500 matches / 50KB; matching lines \
         are truncated at 2000 bytes. Binary, unreadable and over-4MB files are skipped."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "Regular expression (regex crate syntax) to search for"
                },
                "path": {
                    "type": "string",
                    "description": "File or directory to search, relative to the working directory (default: the working directory itself)"
                },
                "glob": {
                    "type": "string",
                    "description": "Optional filename filter applied when searching a directory, e.g. \"*.rs\" or \"src/*.toml\" (use / as separator)"
                }
            },
            "required": ["pattern"]
        })
    }

    fn is_read_only(&self) -> bool {
        true
    }

    async fn execute(&self, input: Value, ctx: &ToolCtx) -> Result<ToolOutput> {
        let pattern = match req_str(&input, "pattern") {
            Ok(p) => p,
            Err(out) => return Ok(out),
        };
        // Empty regex matches every line: almost always a model mistake
        // (and would burn the match cap). Reject it like glob rejects
        // empty patterns in `validate_pattern`.
        if pattern.is_empty() {
            return Ok(err_output("pattern must not be empty"));
        }
        let regex = match regex::Regex::new(pattern) {
            Ok(r) => r,
            Err(e) => return Ok(err_output(format!("invalid regex pattern: {e}"))),
        };
        // Optional params are strictly typed: a present-but-non-string
        // value is a caller bug, not "use the default" (silently ignoring
        // it would search a wider scope than the caller asked for).
        // Explicit null keeps the default for JSON callers.
        let path = match input.get("path") {
            None | Some(Value::Null) => ".",
            Some(Value::String(s)) => s.as_str(),
            Some(_) => return Ok(err_output("invalid parameter 'path' (string required)")),
        };
        let root = match resolve_path(ctx, path)? {
            Ok(p) => p,
            Err(out) => return Ok(out),
        };
        let filter = match input.get("glob") {
            None | Some(Value::Null) => None,
            Some(Value::String(f)) => {
                if let Err(out) = validate_pattern(f) {
                    return Ok(out);
                }
                Some(f.to_owned())
            }
            Some(_) => return Ok(err_output("invalid parameter 'glob' (string required)")),
        };
        let cwd = ctx.cwd.clone();
        // Traversal + file reads + regex scanning are blocking IO, moved off the executor thread;
        // JoinError only fires on a bug (panic) in this module, converted to business output without interrupting the turn.
        match tokio::task::spawn_blocking(move || {
            grep_search(&regex, &root, filter.as_deref(), &cwd)
        })
        .await
        {
            Ok(content) => Ok(content),
            Err(e) => Ok(err_output(format!("grep failed: {e}"))),
        }
    }
}

/// grep's blocking core: expand candidate files, then scan file by file. Returns the full output (including the stats footer).
fn grep_search(regex: &regex::Regex, root: &Path, filter: Option<&str>, cwd: &Path) -> ToolOutput {
    let cwd_canon = match cwd.canonicalize() {
        Ok(c) => c,
        Err(e) => return err_output(format!("failed to canonicalize working directory: {e}")),
    };
    // Candidate file list: when the root is a file scan only it; when it is a directory expand via the glob pattern `root/**/{filter}`
    // (`**` matches any depth, including the root directory's own level). Sorted for stable output.
    let files = if root.is_file() {
        vec![root.to_path_buf()]
    } else if root.is_dir() {
        let pattern = format!("{}/**/{}", glob_prefix(root), filter.unwrap_or("*"));
        let mut files: Vec<PathBuf> = match ::glob::glob(&pattern) {
            Ok(paths) => paths
                .filter_map(std::result::Result::ok)
                .filter(|p| p.is_file())
                .collect(),
            Err(e) => return err_output(format!("invalid glob pattern: {e}")),
        };
        files.sort();
        files
    } else {
        return err_output(format!("path not found: {}", root.display()));
    };

    let mut lines: Vec<String> = Vec::new();
    let mut matched_files = 0usize;
    let mut hit_cap = false;
    'files: for file in &files {
        // Re-check each path: skip junction/symlink targets pointing outside cwd.
        if !under_cwd(file, &cwd_canon) {
            continue;
        }
        // Input guard aligned with read_file: files over 4 MB are not read wholesale (skipped, not counted as matches).
        let meta = match std::fs::metadata(file) {
            Ok(m) => m,
            Err(_) => continue,
        };
        if meta.len() > MAX_FILE_BYTES {
            continue;
        }
        // Silently skip read failures / non-UTF-8 (binary) files: grep semantics search text.
        let Ok(bytes) = std::fs::read(file) else {
            continue;
        };
        let Ok(text) = String::from_utf8(bytes) else {
            continue;
        };
        let rel = rel_display(file, cwd);
        let mut file_had_match = false;
        for (idx, line) in text.lines().enumerate() {
            if !regex.is_match(line) {
                continue;
            }
            file_had_match = true;
            let mut line = line.to_owned();
            if line.len() > MAX_LINE_BYTES {
                let mut cut = MAX_LINE_BYTES;
                while !line.is_char_boundary(cut) {
                    cut -= 1;
                }
                line.truncate(cut);
                line.push_str("[truncated]");
            }
            lines.push(format!("{rel}:{}:{line}", idx + 1));
            if lines.len() >= MAX_MATCHES {
                hit_cap = true;
                // The current file already matched before breaking at the cap: count it before exiting,
                // otherwise the stats footer undercounts by one file.
                if file_had_match {
                    matched_files += 1;
                }
                break 'files;
            }
        }
        if file_had_match {
            matched_files += 1;
        }
    }

    if lines.is_empty() {
        return ok_output("no matches");
    }
    let mut content = lines.join("\n");
    let mut byte_truncated = false;
    if content.len() > MAX_OUTPUT_BYTES {
        let mut cut = MAX_OUTPUT_BYTES;
        while !content.is_char_boundary(cut) {
            cut -= 1;
        }
        content.truncate(cut);
        byte_truncated = true;
    }
    if byte_truncated {
        content.push_str("\n[truncated]");
    }
    content.push_str(&format!(
        "\n[{} matches in {} files{}]",
        lines.len(),
        matched_files,
        if hit_cap {
            format!(", stopped at {MAX_MATCHES} matches")
        } else {
            String::new()
        }
    ));
    ok_output(content)
}
