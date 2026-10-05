//! Local git repository introspection for the footer badge.
//!
//! Reads `.git/HEAD` directly (no subprocess): cheap enough to refresh
//! on turn starts. Only the branch name (or short detached sha) is
//! reported; dirty state would need a walk or `git status` and is out
//! of scope for the badge.

use std::path::Path;

/// Branch name for the repository containing `cwd`, walking up parent
/// directories. Detached HEADs report the first 7 sha characters;
/// `None` when no repository is found.
pub fn branch(cwd: &Path) -> Option<String> {
    let git_dir = find_git_dir(cwd)?;
    let head = std::fs::read_to_string(git_dir.join("HEAD")).ok()?;
    let head = head.trim();
    if let Some(reference) = head.strip_prefix("ref: refs/heads/") {
        if reference.is_empty() {
            return None;
        }
        return Some(reference.to_string());
    }
    // Detached: HEAD holds the raw commit sha.
    let sha: String = head
        .chars()
        .filter(|c| c.is_ascii_hexdigit())
        .take(7)
        .collect();
    if sha.len() == 7 { Some(sha) } else { None }
}

/// The `.git` directory governing `cwd`. Handles both the plain
/// directory form and the `gitdir: …` file form (worktrees, submodule
/// checkouts).
fn find_git_dir(cwd: &Path) -> Option<std::path::PathBuf> {
    let mut current = Some(cwd);
    while let Some(dir) = current {
        let candidate = dir.join(".git");
        if candidate.is_dir() {
            return Some(candidate);
        }
        if candidate.is_file() {
            let content = std::fs::read_to_string(&candidate).ok()?;
            let target = content.trim().strip_prefix("gitdir:")?.trim();
            let path = Path::new(target);
            return Some(if path.is_absolute() {
                path.to_path_buf()
            } else {
                dir.join(path)
            });
        }
        current = dir.parent();
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_repo(name: &str) -> std::path::PathBuf {
        // nosemgrep: rust.lang.security.temp-dir.temp-dir
        let dir = std::env::temp_dir().join(format!("wc-gitinfo-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        dir
    }

    #[test]
    fn reads_branch_from_head() {
        let dir = temp_repo("branch");
        std::fs::write(dir.join(".git").join("HEAD"), "ref: refs/heads/feature-x\n").unwrap();
        assert_eq!(branch(&dir), Some("feature-x".to_string()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn walks_up_to_parent_repo() {
        let dir = temp_repo("nested");
        std::fs::write(dir.join(".git").join("HEAD"), "ref: refs/heads/main\n").unwrap();
        let nested = dir.join("a").join("b");
        std::fs::create_dir_all(&nested).unwrap();
        assert_eq!(branch(&nested), Some("main".to_string()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn detached_head_reports_short_sha() {
        let dir = temp_repo("detached");
        std::fs::write(
            dir.join(".git").join("HEAD"),
            "af06786111111111111111111111111111111111\n",
        )
        .unwrap();
        assert_eq!(branch(&dir), Some("af06786".to_string()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn gitdir_file_form_resolves() {
        // A plain working directory whose `.git` is a FILE pointing at
        // the real repository (worktree / submodule checkout form).
        // nosemgrep: rust.lang.security.temp-dir.temp-dir
        let dir = std::env::temp_dir().join(format!("wc-gitinfo-wt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let real = temp_repo("real");
        std::fs::write(real.join(".git").join("HEAD"), "ref: refs/heads/wt\n").unwrap();
        std::fs::write(
            dir.join(".git"),
            format!("gitdir: {}\n", real.join(".git").display()),
        )
        .unwrap();
        assert_eq!(branch(&dir), Some("wt".to_string()));
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&real);
    }

    #[test]
    fn no_repo_returns_none() {
        // nosemgrep: rust.lang.security.temp-dir.temp-dir
        let dir = std::env::temp_dir().join(format!("wc-gitinfo-none-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(branch(&dir), None);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
