//! Two read-only search tools: `grep` (regex content search) and `glob` (path pattern matching).
//!
//! Path confinement and failure semantics match fs_tools: root paths/patterns first pass lexical
//! validation via [`crate::path_guard`] to stay under `ToolCtx::cwd`; traversal expands via the `glob`
//! crate (sync API), and each hit path is re-checked after canonicalize to still be inside cwd's real
//! path, guarding against symlink/junction escapes.
//! Traversal and file reads are blocking IO, wrapped in `spawn_blocking` off the executor thread
//! (SPEC §19.3); business failures (invalid regex, path escape, missing path, ...) return
//! `Ok(is_error=true)` to the model, and `Err` is only for implementation-level failures.

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::{Result, Tool, ToolCtx, ToolOutput, ToolsError, err_output, req_str};

/// grep match-line cap: stop scanning past it and mark truncation.
const MAX_MATCHES: usize = 500;
/// grep per-line content byte cap (truncation falls back to a char boundary, matching read_file style).
const MAX_LINE_BYTES: usize = 2000;
/// grep total output byte cap (aligned with read_file's 50 KB).
const MAX_OUTPUT_BYTES: usize = 50 * 1024;
/// grep per-file size cap (aligned with read_file's 4 MB input guard).
const MAX_FILE_BYTES: u64 = 4 * 1024 * 1024;
/// glob returned-path cap.
const MAX_PATHS: usize = 1000;

/// Build a success output.
fn ok_output(content: impl Into<String>) -> ToolOutput {
    ToolOutput {
        content: content.into(),
        is_error: false,
    }
}

/// Parse and validate a path: escapes/invalid input become business-failure output for the model, while io failures still propagate as `Err`.
fn resolve_path(ctx: &ToolCtx, path: &str) -> Result<std::result::Result<PathBuf, ToolOutput>> {
    match crate::path_guard::resolve(ctx, path) {
        Ok(p) => Ok(Ok(p)),
        Err(e @ (ToolsError::InvalidInput { .. } | ToolsError::PathEscape { .. })) => {
            Ok(Err(err_output(e.to_string())))
        }
        Err(e) => Err(e),
    }
}

/// Validate glob-style patterns (the glob tool's pattern and grep's glob filter) against escaping the root:
/// reject absolute paths and `..` components -- patterns expand lexically, so `..` could walk traversal past cwd.
fn validate_pattern(pattern: &str) -> std::result::Result<(), ToolOutput> {
    if pattern.is_empty() {
        return Err(err_output("glob pattern must not be empty"));
    }
    let p = Path::new(pattern);
    if p.is_absolute()
        || p.components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err(err_output(format!(
            "glob pattern escapes the working directory: {pattern}"
        )));
    }
    if let Err(e) = ::glob::Pattern::new(pattern) {
        return Err(err_output(format!("invalid glob pattern '{pattern}': {e}")));
    }
    Ok(())
}

/// Directory prefix for glob-crate pattern strings: in glob pattern syntax `\` is an escape character (Windows path
/// separators would be eaten), so normalize to `/` -- the Windows file APIs accept forward slashes, and matching
/// proceeds per path component regardless of separator shape. Glob metacharacters in directory names
/// (`[ ] { } * ?`) are escaped per pattern syntax -- without escaping, a directory like `D:\proj[1]\app`
/// would parse as a character class and always expand to nothing (filtered out by the under_cwd re-check, i.e. "never any result").
fn glob_prefix(dir: &Path) -> String {
    let unified = dir.to_string_lossy().replace('\\', "/");
    ::glob::Pattern::escape(&unified)
}

/// Re-check hit paths: after canonicalize they must stay under cwd's real path (glob traversal follows
/// junctions/symlinks pointing outside, which lexical validation cannot stop, so per-path re-checks backstop it).
fn under_cwd(path: &Path, cwd_canon: &Path) -> bool {
    path.canonicalize()
        .map(|c| c.starts_with(cwd_canon))
        .unwrap_or(false)
}

/// Display path relative to cwd, with separators normalized to `/` (stable output shape across platforms).
fn rel_display(path: &Path, cwd: &Path) -> String {
    path.strip_prefix(cwd)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

/// Regex content search (read-only).
mod glob;
mod grep;

pub use glob::Glob;
pub use grep::Grep;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Tool;

    fn ctx() -> (tempfile::TempDir, ToolCtx) {
        let dir = tempfile::tempdir().unwrap();
        let c = ToolCtx {
            cwd: dir.path().to_path_buf(),
            deny_env: Vec::new(),
        };
        (dir, c)
    }

    #[tokio::test]
    async fn grep_matches_with_line_numbers_and_footer() {
        let (_d, c) = ctx();
        std::fs::create_dir(c.cwd.join("src")).unwrap();
        std::fs::write(c.cwd.join("src/a.rs"), "fn main() {}\nlet x = 1;\n").unwrap();
        std::fs::write(c.cwd.join("b.txt"), "nothing here\n").unwrap();
        let out = Grep
            .execute(serde_json::json!({"pattern":"fn main"}), &c)
            .await
            .unwrap();
        assert!(!out.is_error);
        assert!(out.content.contains("src/a.rs:1:fn main() {}"));
        assert!(out.content.contains("[1 matches in 1 files]"));
    }

    #[tokio::test]
    async fn grep_glob_filter_limits_file_set() {
        let (_d, c) = ctx();
        std::fs::write(c.cwd.join("a.rs"), "target\n").unwrap();
        std::fs::write(c.cwd.join("b.txt"), "target\n").unwrap();
        let out = Grep
            .execute(serde_json::json!({"pattern":"target","glob":"*.rs"}), &c)
            .await
            .unwrap();
        assert!(!out.is_error);
        assert!(out.content.contains("a.rs:1:target"));
        assert!(!out.content.contains("b.txt"));
    }

    #[tokio::test]
    async fn grep_single_file_path() {
        let (_d, c) = ctx();
        std::fs::write(c.cwd.join("a.txt"), "hit\nmiss\n").unwrap();
        let out = Grep
            .execute(serde_json::json!({"pattern":"hit","path":"a.txt"}), &c)
            .await
            .unwrap();
        assert!(!out.is_error);
        assert!(out.content.contains("a.txt:1:hit"));
    }

    #[tokio::test]
    async fn grep_path_escape_rejected() {
        let (_d, c) = ctx();
        let out = Grep
            .execute(serde_json::json!({"pattern":"x","path":"../outside"}), &c)
            .await
            .unwrap();
        assert!(out.is_error);
        // The glob filter must not escape either.
        let out = Grep
            .execute(serde_json::json!({"pattern":"x","glob":"../**/*.rs"}), &c)
            .await
            .unwrap();
        assert!(out.is_error);
    }

    #[tokio::test]
    async fn grep_invalid_regex_is_error_output() {
        let (_d, c) = ctx();
        let out = Grep
            .execute(serde_json::json!({"pattern":"("}), &c)
            .await
            .unwrap();
        assert!(out.is_error);
    }

    #[tokio::test]
    async fn grep_rejects_empty_pattern() {
        // Empty regex matches every line: reject it instead of burning
        // the match cap on a full-tree dump.
        let (_d, c) = ctx();
        std::fs::write(c.cwd.join("a.txt"), "hello\n").unwrap();
        let out = Grep
            .execute(serde_json::json!({"pattern":""}), &c)
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(out.content.contains("must not be empty"));
    }

    #[tokio::test]
    async fn grep_rejects_non_string_optional_params() {
        // Present-but-wrong-typed optionals must fail loudly: silently
        // defaulting would search a wider scope than the caller asked for.
        let (_d, c) = ctx();
        std::fs::write(c.cwd.join("a.txt"), "hit\n").unwrap();
        for bad in [
            serde_json::json!({"pattern":"hit","path":42}),
            serde_json::json!({"pattern":"hit","path":true}),
            serde_json::json!({"pattern":"hit","glob":42}),
            serde_json::json!({"pattern":"hit","glob":true}),
            serde_json::json!({"pattern":"hit","glob":["*.rs"]}),
        ] {
            let out = Grep.execute(bad.clone(), &c).await.unwrap();
            assert!(out.is_error, "input {bad} should be rejected");
        }
        // Explicit null keeps the defaults (JSON callers).
        let out = Grep
            .execute(
                serde_json::json!({"pattern":"hit","path":null,"glob":null}),
                &c,
            )
            .await
            .unwrap();
        assert!(!out.is_error);
        assert!(out.content.contains("a.txt:1:hit"));
    }

    #[tokio::test]
    async fn grep_no_matches() {
        let (_d, c) = ctx();
        std::fs::write(c.cwd.join("a.txt"), "hello\n").unwrap();
        let out = Grep
            .execute(serde_json::json!({"pattern":"zzz"}), &c)
            .await
            .unwrap();
        assert!(!out.is_error);
        assert_eq!(out.content, "no matches");
    }

    #[tokio::test]
    async fn grep_truncates_at_match_cap() {
        let (_d, c) = ctx();
        let text: String = (0..MAX_MATCHES + 50).map(|i| format!("hit{i}\n")).collect();
        std::fs::write(c.cwd.join("many.txt"), text).unwrap();
        let out = Grep
            .execute(serde_json::json!({"pattern":"hit"}), &c)
            .await
            .unwrap();
        assert!(!out.is_error);
        assert!(
            out.content
                .contains(&format!("stopped at {MAX_MATCHES} matches"))
        );
        let body = out.content.lines().take(MAX_MATCHES).count();
        assert_eq!(body, MAX_MATCHES);
    }

    #[tokio::test]
    async fn grep_truncates_long_lines_at_char_boundary() {
        let (_d, c) = ctx();
        // Multi-byte chars ('€', 3 bytes each) straddling the 2000-byte boundary: truncation must not split a char.
        let line = format!("hit{}", "€".repeat(1000));
        std::fs::write(c.cwd.join("long.txt"), line).unwrap();
        let out = Grep
            .execute(serde_json::json!({"pattern":"hit"}), &c)
            .await
            .unwrap();
        assert!(!out.is_error);
        assert!(out.content.contains("[truncated]"));
        assert!(!out.content.contains('\u{FFFD}'));
    }

    #[tokio::test]
    async fn glob_returns_sorted_relative_paths() {
        let (_d, c) = ctx();
        std::fs::create_dir_all(c.cwd.join("src/sub")).unwrap();
        std::fs::write(c.cwd.join("src/b.rs"), "x").unwrap();
        std::fs::write(c.cwd.join("src/sub/a.rs"), "x").unwrap();
        std::fs::write(c.cwd.join("src/c.txt"), "x").unwrap();
        let out = Glob
            .execute(serde_json::json!({"pattern":"src/**/*.rs"}), &c)
            .await
            .unwrap();
        assert!(!out.is_error);
        let lines: Vec<&str> = out.content.lines().collect();
        assert_eq!(lines, vec!["src/b.rs", "src/sub/a.rs"]);
    }

    #[tokio::test]
    async fn glob_no_matches() {
        let (_d, c) = ctx();
        let out = Glob
            .execute(serde_json::json!({"pattern":"**/*.neverexist"}), &c)
            .await
            .unwrap();
        assert!(!out.is_error);
        assert_eq!(out.content, "no matches");
    }

    #[tokio::test]
    async fn glob_rejects_escape_patterns() {
        let (_d, c) = ctx();
        for bad in ["../**/*.rs", "../*.txt", "src/../../x"] {
            let out = Glob
                .execute(serde_json::json!({"pattern": bad}), &c)
                .await
                .unwrap();
            assert!(out.is_error, "pattern {bad} should be rejected");
        }
        // Absolute-path pattern: one that is absolute on each platform.
        #[cfg(windows)]
        let abs = "C:/Windows/*.ini";
        #[cfg(unix)]
        let abs = "/etc/*.conf";
        let out = Glob
            .execute(serde_json::json!({"pattern": abs}), &c)
            .await
            .unwrap();
        assert!(out.is_error);
    }

    #[tokio::test]
    async fn glob_truncates_over_path_cap() {
        let (_d, c) = ctx();
        for i in 0..MAX_PATHS + 5 {
            std::fs::write(c.cwd.join(format!("f{i:04}.txt")), "x").unwrap();
        }
        let out = Glob
            .execute(serde_json::json!({"pattern":"*.txt"}), &c)
            .await
            .unwrap();
        assert!(!out.is_error);
        assert!(out.content.ends_with("[truncated: 5 more paths]"));
        let body = out
            .content
            .strip_suffix("\n[truncated: 5 more paths]")
            .unwrap();
        assert_eq!(body.lines().count(), MAX_PATHS);
    }

    /// Regression (cwd metachar escaping): when the directory name contains glob metacharacters, the prefix must be
    /// escaped per pattern syntax -- otherwise `[1]` parses as a character class and expands to nothing.
    #[tokio::test]
    async fn glob_works_when_cwd_contains_glob_metachars() {
        let outer = tempfile::tempdir().unwrap();
        let dir = outer.path().join("proj[1]_x");
        tokio::fs::create_dir_all(&dir).await.unwrap();
        tokio::fs::write(dir.join("a.rs"), "fn main() {}")
            .await
            .unwrap();
        let ctx = ToolCtx {
            cwd: dir.clone(),
            deny_env: Vec::new(),
        };
        let glob = crate::Registry::builtin().get("glob").unwrap();
        let out = glob
            .execute(serde_json::json!({"pattern": "*.rs"}), &ctx)
            .await
            .unwrap();
        assert!(!out.is_error);
        assert!(
            out.content.contains("a.rs"),
            "should match files under a directory with metacharacters: {}",
            out.content
        );
    }

    #[tokio::test]
    async fn registry_contains_grep_and_glob() {
        let reg = crate::Registry::builtin();
        assert!(reg.get("grep").unwrap().is_read_only());
        assert!(reg.get("glob").unwrap().is_read_only());
    }
}
