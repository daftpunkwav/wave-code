/*!
 * @file ProjectWorkspace
 * @description Project root discovery and bounded file inventory.
 *
 * Responsibilities:
 * - Locate the project root by walking up marker files.
 * - List project files with extension filters and skip rules.
 * - Assemble a byte-capped project context for prompts.
 *
 * This module must not depend on: any other workspace crate.
 */

//! Workspace: where the project lives and what it contains.
//!
//! Inventory is synchronous filesystem IO by design; drivers call it once
//! per session start, never inside the turn loop.

use std::path::{Path, PathBuf};

/// Marker files identifying a project root, in search priority order.
const ROOT_MARKERS: &[&str] = &[
    ".git",
    "Cargo.toml",
    "package.json",
    "WAVECODE.md",
    ".wavecode",
];

/// Directory names never descended into during inventory.
const SKIP_DIRS: &[&str] = &[".git", "target", "node_modules", ".venv", "dist", "build"];

/// Maximum bytes of file content carried into project context.
pub const MAX_CONTEXT_BYTES: u64 = 200_000;

/// Locate the project root by walking up from `start`.
pub fn discover_root(start: &Path) -> Option<PathBuf> {
    let mut dir = if start.is_file() {
        start.parent()?.to_path_buf()
    } else {
        start.to_path_buf()
    };
    loop {
        if ROOT_MARKERS.iter().any(|m| dir.join(m).exists()) {
            return Some(dir);
        }
        if !dir.pop() {
            return None;
        }
    }
}

/// List files under `root` with the given extensions (without dots).
///
/// Hidden directories, skip-listed build outputs, and files beyond `limit`
/// are excluded; traversal is depth-first in directory order.
pub fn list_files(root: &Path, extensions: &[&str], limit: usize) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(_) => continue,
        };
        let mut entries: Vec<_> = entries.filter_map(|e| e.ok()).collect();
        entries.sort_by_key(|e| e.path());
        for entry in entries {
            if out.len() >= limit {
                return out;
            }
            let path = entry.path();
            if path.is_dir() {
                // Never descend into symlinked directories: link cycles
                // (self-referential mounts, symlink forests inside
                // node_modules) would push the same directories forever.
                // Symlinked files still list; dangling links list nowhere.
                if entry.file_type().map(|t| t.is_symlink()).unwrap_or(false) {
                    continue;
                }
                let name = entry.file_name().to_string_lossy().into_owned();
                if name.starts_with('.') || SKIP_DIRS.contains(&name.as_str()) {
                    continue;
                }
                stack.push(path);
            } else if path.is_file() {
                let matches = extensions.is_empty()
                    || path
                        .extension()
                        .and_then(|e| e.to_str())
                        .is_some_and(|e| extensions.contains(&e));
                if matches {
                    out.push(path);
                }
            }
        }
    }
    out
}

/// Assembled project context for prompt injection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectContext {
    /// Discovered project root.
    pub root: PathBuf,
    /// Inventoried files within limits.
    pub files: Vec<PathBuf>,
    /// Sum of file sizes read.
    pub total_bytes: u64,
    /// True when files or bytes were cut by limits.
    pub truncated: bool,
}

/// Assemble bounded project context from a root directory.
pub fn assemble(root: &Path, extensions: &[&str], max_files: usize) -> ProjectContext {
    let files = list_files(root, extensions, max_files + 1);
    let truncated_files = files.len() > max_files;
    let mut kept = Vec::new();
    let mut total_bytes = 0u64;
    let mut truncated_bytes = false;
    for path in files.into_iter().take(max_files) {
        let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        if total_bytes + size > MAX_CONTEXT_BYTES {
            truncated_bytes = true;
            break;
        }
        total_bytes += size;
        kept.push(path);
    }
    ProjectContext {
        root: root.to_path_buf(),
        files: kept,
        total_bytes,
        truncated: truncated_files || truncated_bytes,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".git")).unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::create_dir_all(dir.path().join("target")).unwrap();
        std::fs::write(dir.path().join("src/main.rs"), "fn main() {}").unwrap();
        std::fs::write(dir.path().join("target/out.bin"), "x").unwrap();
        std::fs::write(dir.path().join("README.md"), "hi").unwrap();
        dir
    }

    #[test]
    fn root_discovery_walks_up_to_markers() {
        let dir = fixture();
        let nested = dir.path().join("src");
        assert_eq!(discover_root(&nested).unwrap(), dir.path());
    }

    #[test]
    fn inventory_skips_build_outputs_and_filters_extensions() {
        let dir = fixture();
        let rs = list_files(dir.path(), &["rs"], 100);
        assert_eq!(rs.len(), 1);
        assert!(rs[0].ends_with("main.rs"));
        let all = list_files(dir.path(), &[], 100);
        assert!(all.iter().all(|p| !p.to_string_lossy().contains("target")));
    }

    #[test]
    fn assembly_caps_files_and_reports_truncation() {
        let dir = fixture();
        let ctx = assemble(dir.path(), &[], 1);
        assert!(ctx.truncated);
        assert_eq!(ctx.files.len(), 1);
        let full = assemble(dir.path(), &[], 100);
        assert!(!full.truncated);
    }

    /// Symlink loops terminate: linked directories never descend.
    ///
    /// Unix-only: Windows symlink creation needs privileges the test
    /// runner cannot assume.
    #[cfg(unix)]
    #[test]
    fn symlink_cycles_terminate() {
        use std::os::unix::fs::symlink;
        let dir = fixture();
        // A directory linking to its own ancestor: descent would loop.
        symlink(dir.path(), dir.path().join("src/loop")).unwrap();
        let files = list_files(dir.path(), &[], 100);
        assert!(files.iter().all(|p| !p.to_string_lossy().contains("loop")));
    }
}
