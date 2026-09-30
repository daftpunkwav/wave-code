//! Built-in file tools: `read` / `write` / `edit` (+ `view` / `present`).
//! All paths are confined under `ToolCtx::cwd` via [`crate::path_guard::resolve`].
//! Failure semantics: business failures (missing file, non-unique match, missing/mistyped params, path escape)
//! return `Ok(is_error=true)` with the reason fed back to the model; `Err` is only for implementation-level io failures.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};

use crate::{Result, Tool, ToolCtx, ToolOutput, ToolsError, err_output, ok_output, req_str};

/// Per-session record of what the session last saw on disk for the files
/// its file tools touched. `read` records a fingerprint when it returns
/// content; `write` / `edit` compare the on-disk fingerprint against the
/// recorded one before mutating and refuse a mismatch, so an edit decided
/// against an outdated view cannot silently clobber an outside change
/// (formatter, git checkout, another process) that landed after the read.
/// Absent entries never block: a file the session never saw has no
/// baseline to defend. A fingerprint that cannot be taken (filesystem
/// without mtime support) simply opts that file out.
#[derive(Clone, Default)]
pub(crate) struct FileLedger(Arc<Mutex<HashMap<PathBuf, (std::time::SystemTime, u64)>>>);

impl FileLedger {
    /// Ledger shared by one session's `read` / `write` / `edit` trio.
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Record what the session just observed on disk.
    fn record(&self, path: &std::path::Path, meta: &std::fs::Metadata) {
        if let Ok(mtime) = meta.modified() {
            self.0
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(path.to_path_buf(), (mtime, meta.len()));
        }
    }

    /// Re-stat after this session's own write so the recorded fingerprint
    /// matches the file it left behind; stat failures skip recording.
    pub(crate) async fn refresh(&self, path: &std::path::Path) {
        if let Ok(meta) = tokio::fs::metadata(path).await {
            self.record(path, &meta);
        }
    }

    /// Business-failure output when the on-disk file drifted from the
    /// recorded fingerprint; `None` when the write may proceed (no recorded
    /// baseline, stat failure, or an unchanged file).
    pub(crate) async fn check_fresh(&self, path: &std::path::Path) -> Option<ToolOutput> {
        let expected = *self.0.lock().unwrap_or_else(|e| e.into_inner()).get(path)?;
        let meta = tokio::fs::metadata(path).await.ok()?;
        let current = match meta.modified() {
            Ok(mtime) => Some((mtime, meta.len())),
            Err(_) => None,
        };
        if current == Some(expected) {
            return None;
        }
        Some(err_output(format!(
            "{} changed on disk since it was last read; re-read it before writing",
            path.display()
        )))
    }
}

/// read output cap: 2000 lines / 50 KB.
const MAX_LINES: usize = 2000;
const MAX_BYTES: usize = 50 * 1024;
/// read hard input cap: files over 4 MB are rejected outright to avoid loading them fully into memory.
const MAX_READ_BYTES: u64 = 4 * 1024 * 1024;
/// write content cap: content over 10 MB is rejected outright.
const MAX_WRITE_BYTES: usize = 10 * 1024 * 1024;

/// Parse an optional non-negative integer parameter; present-but-negative/float/non-numeric values yield a business-failure output.
fn opt_usize(input: &Value, key: &str) -> std::result::Result<Option<usize>, ToolOutput> {
    match input.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v.as_u64().map(|n| Some(n as usize)).ok_or_else(|| {
            err_output(format!(
                "invalid parameter '{key}' (non-negative integer required)"
            ))
        }),
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

/// Atomic overwrite (shared by write / edit): write a temp file in the same directory first, then
/// rename over the target (same-volume rename is atomic; MOVEFILE_REPLACE_EXISTING on Windows).
/// Overwriting an existing file directly with `tokio::fs::write` would truncate the target into a half-written
/// file with no backup if the write fails midway; with temp+rename the target holds either the old or the new content.
///
/// Temp names carry the process id plus a per-process sequence number, so concurrent writes to the same directory
/// in one batch do not collide. On write or rename failure the temp file is cleaned up before the error
/// propagates (no litter, no swallowed errors).
pub(super) async fn atomic_write(path: &std::path::Path, content: &str) -> std::io::Result<()> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let file_name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".into());
    let tmp = path.with_file_name(format!(
        ".{file_name}.{}.{}.tmp",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    if let Err(e) = tokio::fs::write(&tmp, content).await {
        // Write failures (disk full / permissions): half-written .tmp files are removed as well.
        let _ = tokio::fs::remove_file(&tmp).await;
        return Err(e);
    }
    if let Err(e) = tokio::fs::rename(&tmp, path).await {
        let _ = tokio::fs::remove_file(&tmp).await;
        return Err(e);
    }
    Ok(())
}

mod edit;
mod image;
mod present;
mod read;
mod write;

pub use edit::EditFile;
pub use image::ReadImage;
pub use present::{Present, PresentStore};
pub use read::ReadFile;
pub use write::WriteFile;

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

    #[tokio::test]
    async fn write_then_read_roundtrip() {
        let (_d, c) = ctx();
        let out = WriteFile::default()
            .execute(
                serde_json::json!({"path":"sub/hello.txt","content":"hi"}),
                &c,
            )
            .await
            .unwrap();
        assert!(!out.is_error);
        let out = ReadFile::default()
            .execute(serde_json::json!({"path":"sub/hello.txt"}), &c)
            .await
            .unwrap();
        assert_eq!(out.content, "hi");
    }

    /// Atomic write: content is correct after overwrite, and no .tmp leftovers remain in the directory
    /// (on the temp+rename success path rename finalizes the file; on the failure path temp files are cleaned up).
    #[tokio::test]
    async fn write_overwrites_without_temp_leftovers() {
        let (_d, c) = ctx();
        WriteFile::default()
            .execute(serde_json::json!({"path":"a.txt","content":"v1"}), &c)
            .await
            .unwrap();
        WriteFile::default()
            .execute(
                serde_json::json!({"path":"a.txt","content":"v2-longer"}),
                &c,
            )
            .await
            .unwrap();
        EditFile::default()
            .execute(
                serde_json::json!({"path":"a.txt","old_string":"v2","new_string":"v3"}),
                &c,
            )
            .await
            .unwrap();
        let out = ReadFile::default()
            .execute(serde_json::json!({"path":"a.txt"}), &c)
            .await
            .unwrap();
        assert_eq!(out.content, "v3-longer");
        let leftovers: Vec<String> = std::fs::read_dir(c.cwd.join("."))
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".tmp"))
            .collect();
        assert!(
            leftovers.is_empty(),
            "no temp files should remain: {leftovers:?}"
        );
    }

    #[tokio::test]
    async fn read_missing_file_is_error_output_not_err() {
        let (_d, c) = ctx();
        let out = ReadFile::default()
            .execute(serde_json::json!({"path":"nope.txt"}), &c)
            .await
            .unwrap();
        assert!(out.is_error);
    }

    #[tokio::test]
    async fn edit_requires_unique_match() {
        let (_d, c) = ctx();
        WriteFile::default()
            .execute(
                serde_json::json!({"path":"a.txt","content":"foo bar foo"}),
                &c,
            )
            .await
            .unwrap();
        let dup = EditFile::default()
            .execute(
                serde_json::json!({"path":"a.txt","old_string":"foo","new_string":"x"}),
                &c,
            )
            .await
            .unwrap();
        assert!(dup.is_error);
        let ok = EditFile::default()
            .execute(
                serde_json::json!({"path":"a.txt","old_string":"bar foo","new_string":"baz"}),
                &c,
            )
            .await
            .unwrap();
        assert!(!ok.is_error);
        let out = ReadFile::default()
            .execute(serde_json::json!({"path":"a.txt"}), &c)
            .await
            .unwrap();
        assert_eq!(out.content, "foo baz");
    }

    #[tokio::test]
    async fn registry_specs_sorted_and_have_schema() {
        let reg = crate::Registry::builtin();
        let specs = reg.specs();
        // builtin excludes todowrite (injected by session assembly via with_todo_write).
        // Exact counts are not asserted: concurrent milestones register more
        // builtins (web_search, spill, image, present, ...); the stable
        // contract is sorted specs, object schemas, and per-tool presence.
        assert!(specs.len() >= 14);
        for name in ["read", "view", "present", "web_search", "spill"] {
            assert!(
                specs.iter().any(|s| s.name == name),
                "{name} must be registered"
            );
        }
        let names: Vec<_> = specs.iter().map(|s| s.name.as_str()).collect();
        let mut sorted = names.clone();
        sorted.sort();
        assert_eq!(names, sorted);
        assert!(specs.iter().all(|s| s.input_schema["type"] == "object"));
        assert!(reg.get("read").unwrap().is_read_only());
        assert!(!reg.get("write").unwrap().is_read_only());
        assert!(reg.get("todowrite").is_none());

        let (full, _todos) = crate::Registry::builtin_with_todos();
        assert_eq!(full.specs().len(), specs.len() + 1);
        assert!(full.get("todowrite").is_some());
    }

    #[tokio::test]
    async fn missing_param_is_error_output() {
        let (_d, c) = ctx();
        let out = WriteFile::default()
            .execute(serde_json::json!({"path":"x.txt"}), &c)
            .await
            .unwrap(); // missing content
        assert!(out.is_error);
    }

    #[tokio::test]
    async fn read_empty_file_with_offset_is_error_not_panic() {
        // Regression: an empty file with offset>0 used to trigger a usize underflow panic.
        let (_d, c) = ctx();
        WriteFile::default()
            .execute(serde_json::json!({"path":"empty.txt","content":""}), &c)
            .await
            .unwrap();
        let out = ReadFile::default()
            .execute(serde_json::json!({"path":"empty.txt","offset":5}), &c)
            .await
            .unwrap();
        assert!(out.is_error);
        // offset=0 on an empty file still returns an empty string normally.
        let out = ReadFile::default()
            .execute(serde_json::json!({"path":"empty.txt"}), &c)
            .await
            .unwrap();
        assert!(!out.is_error);
        assert_eq!(out.content, "");
    }

    #[tokio::test]
    async fn read_rejects_oversized_file() {
        let (_d, c) = ctx();
        let big = vec![b'x'; (MAX_READ_BYTES + 1) as usize];
        std::fs::write(c.cwd.join("big.txt"), big).unwrap();
        let out = ReadFile::default()
            .execute(serde_json::json!({"path":"big.txt"}), &c)
            .await
            .unwrap();
        assert!(out.is_error);
    }

    #[tokio::test]
    async fn read_dir_path_is_error() {
        let (_d, c) = ctx();
        std::fs::create_dir(c.cwd.join("sub")).unwrap();
        let out = ReadFile::default()
            .execute(serde_json::json!({"path":"sub"}), &c)
            .await
            .unwrap();
        assert!(out.is_error);
    }

    #[tokio::test]
    async fn invalid_offset_limit_is_error() {
        let (_d, c) = ctx();
        WriteFile::default()
            .execute(serde_json::json!({"path":"a.txt","content":"l1\nl2"}), &c)
            .await
            .unwrap();
        for bad in [
            serde_json::json!({"path":"a.txt","limit":0}),
            // Negative offsets are now legal (tail reads); a mistyped
            // string offset stays a business error.
            serde_json::json!({"path":"a.txt","offset":"-1"}),
            serde_json::json!({"path":"a.txt","limit":"100"}),
        ] {
            let out = ReadFile::default().execute(bad.clone(), &c).await.unwrap();
            assert!(out.is_error, "input {bad} should return is_error");
        }
    }

    #[tokio::test]
    async fn write_rejects_oversized_content() {
        let (_d, c) = ctx();
        let big = "x".repeat(MAX_WRITE_BYTES + 1);
        let out = WriteFile::default()
            .execute(serde_json::json!({"path":"big.txt","content":big}), &c)
            .await
            .unwrap();
        assert!(out.is_error);
        // Oversized content does not create the file.
        assert!(!c.cwd.join("big.txt").exists());
    }

    #[tokio::test]
    async fn edit_rejects_oversized_file() {
        let (_d, c) = ctx();
        let big = vec![b'x'; (MAX_READ_BYTES + 1) as usize];
        std::fs::write(c.cwd.join("big.txt"), big).unwrap();
        let out = EditFile::default()
            .execute(
                serde_json::json!({"path":"big.txt","old_string":"x","new_string":"y"}),
                &c,
            )
            .await
            .unwrap();
        assert!(out.is_error);
    }

    #[tokio::test]
    async fn read_truncates_at_line_cap() {
        // 3000 lines: the default limit clamps to 2000 lines with a [truncated] marker.
        let (_d, c) = ctx();
        let text: String = (0..3000).map(|i| format!("line{i}\n")).collect();
        std::fs::write(c.cwd.join("many.txt"), text).unwrap();
        let out = ReadFile::default()
            .execute(serde_json::json!({"path":"many.txt"}), &c)
            .await
            .unwrap();
        assert!(!out.is_error);
        assert!(out.content.ends_with("\n[truncated]"));
        let body = out.content.strip_suffix("\n[truncated]").unwrap();
        assert_eq!(body.lines().count(), MAX_LINES);
        assert!(body.starts_with("line0"));
        assert!(body.ends_with("line1999"));
    }

    #[tokio::test]
    async fn read_truncates_at_byte_cap_on_char_boundary() {
        // Multi-byte chars ('€', 3 bytes each) pushing past 50 KB: truncation lands on a char boundary, no mojibake.
        let (_d, c) = ctx();
        let text = "€".repeat(MAX_BYTES); // 3 * 50 KB bytes
        std::fs::write(c.cwd.join("euro.txt"), text).unwrap();
        let out = ReadFile::default()
            .execute(serde_json::json!({"path":"euro.txt"}), &c)
            .await
            .unwrap();
        assert!(!out.is_error);
        assert!(out.content.ends_with("\n[truncated]"));
        let body = out.content.strip_suffix("\n[truncated]").unwrap();
        assert!(body.len() <= MAX_BYTES);
        // String guarantees valid UTF-8; the cut point must be a char boundary ('€' intact, no U+FFFD).
        assert!(!body.contains('\u{FFFD}'));
        assert_eq!(body.len() % "€".len(), 0);
    }

    #[tokio::test]
    async fn read_offset_limit_pages_correctly() {
        // Slice within the offset/limit range: content matches line numbers exactly.
        let (_d, c) = ctx();
        let text: String = (0..100).map(|i| format!("line{i}\n")).collect();
        std::fs::write(c.cwd.join("page.txt"), text).unwrap();
        let out = ReadFile::default()
            .execute(
                serde_json::json!({"path":"page.txt","offset":10,"limit":5}),
                &c,
            )
            .await
            .unwrap();
        assert!(!out.is_error);
        // End of file not reached, so append the [truncated] marker per the rule.
        assert_eq!(
            out.content,
            "line10\nline11\nline12\nline13\nline14\n[truncated]"
        );
    }

    /// Staleness trio sharing one ledger, like the assembly registers them.
    fn fresh_trio() -> (ReadFile, WriteFile, EditFile) {
        let ledger = FileLedger::new();
        (
            ReadFile::new(ledger.clone()),
            WriteFile::new(ledger.clone()),
            EditFile::new(ledger),
        )
    }

    /// After the session read a file, an outside change (formatter, git
    /// checkout, another process) blocks edit until the model re-reads;
    /// the re-read re-records the baseline and the same edit then applies.
    #[tokio::test]
    async fn edit_refuses_after_outside_change_until_reread() {
        let (_d, c) = ctx();
        let (r, _w, e) = fresh_trio();
        std::fs::write(c.cwd.join("f.txt"), "alpha beta gamma\n").unwrap();
        r.execute(serde_json::json!({"path": "f.txt"}), &c)
            .await
            .unwrap();
        // Outside change: different length so the fingerprint differs even
        // on coarse-mtime filesystems.
        std::fs::write(c.cwd.join("f.txt"), "alpha beta GAMMA CHANGED\n").unwrap();
        let out = e
            .execute(
                serde_json::json!({"path": "f.txt", "old_string": "alpha", "new_string": "ALPHA"}),
                &c,
            )
            .await
            .unwrap();
        assert!(out.is_error, "{:?}", out.content);
        assert!(out.content.contains("changed on disk"), "{}", out.content);
        assert!(
            !out.content.contains("ALPHA"),
            "file untouched: {}",
            out.content
        );
        assert_eq!(
            std::fs::read_to_string(c.cwd.join("f.txt")).unwrap(),
            "alpha beta GAMMA CHANGED\n"
        );
        // Re-read re-records the baseline; the same edit now applies.
        r.execute(serde_json::json!({"path": "f.txt"}), &c)
            .await
            .unwrap();
        let out = e
            .execute(
                serde_json::json!({"path": "f.txt", "old_string": "GAMMA", "new_string": "gamma"}),
                &c,
            )
            .await
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
    }

    /// The guard covers whole-file overwrites too, and a write with no
    /// recorded baseline (file the session never read) still goes through.
    #[tokio::test]
    async fn write_refuses_after_outside_change_but_unseen_files_pass() {
        let (_d, c) = ctx();
        let (r, w, _e) = fresh_trio();
        // No baseline for a brand-new file: the write proceeds.
        let out = w
            .execute(serde_json::json!({"path": "new.txt", "content": "v1"}), &c)
            .await
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        // Baseline via read; outside change then blocks the overwrite.
        r.execute(serde_json::json!({"path": "new.txt"}), &c)
            .await
            .unwrap();
        std::fs::write(c.cwd.join("new.txt"), "v1 externally extended\n").unwrap();
        let out = w
            .execute(serde_json::json!({"path": "new.txt", "content": "v2"}), &c)
            .await
            .unwrap();
        assert!(out.is_error, "{:?}", out.content);
        assert!(out.content.contains("changed on disk"), "{}", out.content);
        assert_eq!(
            std::fs::read_to_string(c.cwd.join("new.txt")).unwrap(),
            "v1 externally extended\n"
        );
    }

    /// The session's own writes keep the ledger current: an edit following
    /// a write (or a second edit) never trips the guard.
    #[tokio::test]
    async fn own_writes_keep_the_ledger_current() {
        let (_d, c) = ctx();
        let (_r, w, e) = fresh_trio();
        w.execute(serde_json::json!({"path": "seq.txt", "content": "one"}), &c)
            .await
            .unwrap();
        let out = e
            .execute(
                serde_json::json!({"path": "seq.txt", "old_string": "one", "new_string": "two"}),
                &c,
            )
            .await
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        let out = e
            .execute(
                serde_json::json!({"path": "seq.txt", "old_string": "two", "new_string": "three"}),
                &c,
            )
            .await
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        assert_eq!(
            std::fs::read_to_string(c.cwd.join("seq.txt")).unwrap(),
            "three"
        );
    }
}
