//! Path escape guard: resolve model-provided paths under `ToolCtx::cwd`.
//!
//! All safety checks run on the canonicalized real path (on Windows canonicalize
//! adds a `\\?\` prefix, so both sides must be in the same form before comparing);
//! what is returned is a lexically normalized path anchored at the (non-canonicalized)
//! cwd, convenient for callers to display and compare.
//!
//! TOCTOU assumption: symlinks on the path may be swapped between the check and the actual
//! use (read/write), and this module cannot guard that race. The window is
//! accepted; stronger measures such as fd-anchored (openat-style) access are
//! not implemented.
//!
//! Threat model for that acceptance: this guard's boundary is the model's
//! own path input, which must not escape `cwd`. Winning the race requires
//! an actor that can run concurrent local code against the same files in
//! the check-to-use window — e.g. a process the model itself started. Such
//! an actor already acts with the user's own privileges and is governed by
//! the approval policy and job lifetime controls, not by path resolution,
//! so closing the window would not move the trust boundary; it only adds
//! handle-anchored IO complexity across every file tool. Revisit if the
//! agent ever gains a confinement model where the racing actor is weaker
//! than the user.

use std::path::{Component, Path, PathBuf};

use crate::{Result, ToolsError};

/// Resolve a user-provided path under ctx.cwd; escapes (.. breakout, absolute paths to another drive/directory) return PathEscape.
pub(crate) fn resolve(ctx: &crate::ToolCtx, path: &str) -> Result<PathBuf> {
    if path.is_empty() {
        return Err(ToolsError::InvalidInput {
            message: "path must not be empty".to_owned(),
        });
    }
    // On join, an absolute path replaces cwd wholesale while a relative path lands under cwd; normalize lexically first to drop `.` / `..`.
    let joined = ctx.cwd.join(path);
    let normalized = normalize_lexically(&joined);
    let cwd_canon = ctx.cwd.canonicalize().map_err(ToolsError::Io)?;

    // Use symlink_metadata for existence: a symlink itself counts as "existing", and a dangling symlink falls into
    // the exists-branch and errors at canonicalize, so writes never follow a dangling link outside cwd.
    match std::fs::symlink_metadata(&normalized) {
        Ok(_) => {
            // Exists: the real path after resolving symlinks must stay under the real path of cwd.
            let canon = normalized.canonicalize().map_err(ToolsError::Io)?;
            if canon.starts_with(&cwd_canon) {
                Ok(normalized)
            } else {
                Err(escape(path))
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // Missing (the write_file case): anchor at the nearest existing ancestor, append the remaining
            // segments after canonicalizing, then re-check the prefix; on success return the lexically normalized path (starting with the non-canonicalized cwd).
            let anchor = normalized
                .ancestors()
                .find(|a| a.symlink_metadata().is_ok())
                .ok_or_else(|| escape(path))?;
            let rest = normalized
                .strip_prefix(anchor)
                .expect("anchor must be a prefix of normalized");
            let anchor_canon = anchor.canonicalize().map_err(ToolsError::Io)?;
            let candidate = normalize_lexically(&anchor_canon.join(rest));
            if candidate.starts_with(&cwd_canon) {
                Ok(normalized)
            } else {
                Err(escape(path))
            }
        }
        Err(e) => Err(ToolsError::Io(e)),
    }
}

/// Build a PathEscape error, recording the user's raw input for debugging.
fn escape(path: &str) -> ToolsError {
    ToolsError::PathEscape {
        path: path.to_owned(),
    }
}

/// Purely lexical normalization: drop `.`; pop the parent normal directory for `..` when possible,
/// and keep unpoppable `..` at the root as-is -- a later starts_with check rejects such results.
fn normalize_lexically(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::CurDir => {}
            Component::Normal(seg) => out.push(seg),
            Component::ParentDir => {
                let can_pop = matches!(out.components().next_back(), Some(Component::Normal(_)));
                if can_pop {
                    out.pop();
                } else {
                    out.push("..");
                }
            }
            // Prefix / RootDir are kept as-is.
            other => out.push(other.as_os_str()),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn rejects_escape() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = crate::ToolCtx {
            cwd: dir.path().to_path_buf(),
            deny_env: Vec::new(),
        };
        assert!(
            super::resolve(&ctx, "inside/ok.txt")
                .unwrap()
                .ends_with("ok.txt")
        );
        assert!(super::resolve(&ctx, "../escape.txt").is_err());
        assert!(super::resolve(&ctx, "../../escape.txt").is_err());
        // Absolute path to another drive/directory: pick a path that almost surely exists on each platform yet never lives under the tempdir.
        #[cfg(windows)]
        assert!(super::resolve(&ctx, "C:/Windows/evil.txt").is_err());
        #[cfg(unix)]
        assert!(super::resolve(&ctx, "/etc/passwd").is_err());
    }

    #[test]
    fn resolves_nonexistent_file_under_cwd() {
        // write_file case: the target is missing, but anchoring at an existing ancestor keeps it inside cwd.
        let dir = tempfile::tempdir().unwrap();
        let ctx = crate::ToolCtx {
            cwd: dir.path().to_path_buf(),
            deny_env: Vec::new(),
        };
        let p = super::resolve(&ctx, "newdir/newfile.txt").unwrap();
        assert!(p.starts_with(dir.path()));
    }

    /// symlink/junction pointing outside cwd: read_file / write_file must both reject with zero pollution outside.
    /// When creation fails (permissions or platform policy), print a notice and skip instead of failing.
    #[tokio::test]
    async fn symlink_escape_is_rejected() {
        use crate::Tool;

        let outside = tempfile::tempdir().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let ctx = crate::ToolCtx {
            cwd: dir.path().to_path_buf(),
            deny_env: Vec::new(),
        };
        let link = dir.path().join("link");

        // On Windows use a junction (mklink /J is a cmd builtin, no elevation needed inside a tempdir).
        #[cfg(windows)]
        let made = std::process::Command::new("cmd")
            .args(["/c", "mklink", "/J"])
            .arg(&link)
            .arg(outside.path())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        #[cfg(unix)]
        let made = std::os::unix::fs::symlink(outside.path(), link.as_path()).is_ok();
        #[cfg(not(any(windows, unix)))]
        let made = false;
        if !made || link.symlink_metadata().is_err() {
            eprintln!(
                "cannot create symlink/junction (permissions or platform policy), skipping symlink escape test"
            );
            return;
        }

        let w = crate::fs::WriteFile::default()
            .execute(
                serde_json::json!({"path":"link/evil.txt","content":"x"}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(w.is_error);
        let r = crate::fs::ReadFile::default()
            .execute(serde_json::json!({"path":"link/evil.txt"}), &ctx)
            .await
            .unwrap();
        assert!(r.is_error);
        // Zero pollution of the outside directory.
        assert!(!outside.path().join("evil.txt").exists());
    }

    /// Sibling-directory prefix confusion: a plain string starts_with would mistake abd as inside abc;
    /// component-level comparison must reject it.
    #[test]
    fn rejects_sibling_prefix_confusion() {
        let root = tempfile::tempdir().unwrap();
        let cwd = root.path().join("abc");
        std::fs::create_dir(&cwd).unwrap();
        std::fs::create_dir(root.path().join("abd")).unwrap();
        let ctx = crate::ToolCtx {
            cwd,
            deny_env: Vec::new(),
        };
        assert!(super::resolve(&ctx, "../abd/x.txt").is_err());
    }
}
