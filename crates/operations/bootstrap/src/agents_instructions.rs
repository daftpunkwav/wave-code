/*!
 * @file AgentsInstructionsDiscovery
 * @description Per-directory `AGENTS.md` discovery wrapped around the executor.
 *
 * Responsibilities:
 * - Inspect each file-tool call's `path` input and walk from its directory
 *   up to the project root, queueing the nearest not-yet-loaded
 *   `AGENTS.md` per directory into the loop's instruction channel.
 * - Delegate execution untouched: discovery is an observer, never a gate.
 *
 * This module must not depend on: the run loop. It shares the
 * `DirectoryInstructions` slot; assembly wires both sides.
 */

//! On-demand nested-instruction loading: a session starts with the global,
//! project-root, and cwd tiers (see `wavecode-memory::collect`); this layer
//! adds the deeper directories the moment a tool touches them, so a
//! monorepo subdirectory's rules arrive before the model works in it and
//! never load when it does not.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use runtime_runner::{DirectoryInstructions, ToolCall, ToolExecutor, ToolRef, ToolResult};
use wavecode_memory::instructions::INSTRUCTION_FILE;

/// Discovery decorator over any [`ToolExecutor`].
pub struct AgentsInstructionsExecutor<E> {
    inner: E,
    /// Inclusive ceiling for the upward walk: the project root's tier is
    /// loaded at assembly, so discovery never re-offers it or anything
    /// above. `None` (non-repo cwd) walks to the filesystem root.
    root: Option<PathBuf>,
    /// Directories already offered this session (canonicalized when
    /// possible), so each `AGENTS.md` lands at most once.
    injected: Mutex<HashSet<PathBuf>>,
    requests: Arc<DirectoryInstructions>,
}

impl<E: ToolExecutor> AgentsInstructionsExecutor<E> {
    /// Wrap `inner`; `root` bounds the upward walk.
    pub fn new(inner: E, root: Option<PathBuf>, requests: Arc<DirectoryInstructions>) -> Self {
        Self {
            inner,
            root,
            injected: Mutex::new(HashSet::new()),
            requests,
        }
    }

    /// Directories between `dir` (inclusive) and `root` (exclusive) holding
    /// an `AGENTS.md`, nearest first.
    fn candidate_dirs(&self, dir: &Path) -> Vec<PathBuf> {
        let mut candidates = Vec::new();
        for ancestor in dir.ancestors() {
            // `ancestors` ends at the empty path, one past the filesystem
            // root; joining it would resolve against the process cwd and
            // re-offer the cwd tier assembly already loaded.
            if ancestor.as_os_str().is_empty() {
                break;
            }
            if let Some(root) = &self.root {
                if ancestor == root.as_path() {
                    break;
                }
                if !ancestor.starts_with(root) {
                    break;
                }
            }
            if ancestor.join(INSTRUCTION_FILE).is_file() {
                candidates.push(ancestor.to_path_buf());
            }
        }
        candidates
    }

    /// Offer the nearest not-yet-injected `AGENTS.md` for `dir`.
    fn discover(&self, dir: &Path) {
        for directory in self.candidate_dirs(dir) {
            let mut seen = self.injected.lock().unwrap_or_else(|e| e.into_inner());
            if seen.contains(&directory) {
                continue;
            }
            match std::fs::read_to_string(directory.join(INSTRUCTION_FILE)) {
                Ok(content) => {
                    seen.insert(directory.clone());
                    drop(seen);
                    self.requests.offer(directory, content);
                }
                // Unreadable mid-walk: mark visited and keep climbing, so a
                // racing delete cannot spin the discovery every call.
                Err(_) => {
                    seen.insert(directory);
                }
            }
        }
    }

    /// Test-only peek at the injected set.
    #[cfg(test)]
    fn lock_injected(&self) -> std::sync::MutexGuard<'_, HashSet<PathBuf>> {
        self.injected.lock().unwrap_or_else(|e| e.into_inner())
    }
}

#[async_trait::async_trait]
impl<E: ToolExecutor> ToolExecutor for AgentsInstructionsExecutor<E> {
    async fn execute(&self, call: ToolCall) -> ToolResult {
        // The file tools share one input convention: the target lives in
        // `path`, and discovery walks its containing directory. Grep's
        // optional search root rides the same field; it is a directory, so
        // the walk starts one level up — bounded, just conservative. Tools
        // without a path (shell, glob) pass through untouched.
        if let Some(path) = call
            .input
            .get("path")
            .and_then(serde_json::Value::as_str)
            .map(PathBuf::from)
            && let Some(dir) = path.parent()
        {
            self.discover(dir);
        }
        self.inner.execute(call).await
    }

    fn is_read_only(&self, name: &str) -> bool {
        self.inner.is_read_only(name)
    }

    fn is_destructive(&self, name: &str) -> bool {
        self.inner.is_destructive(name)
    }

    fn available_tools(&self) -> Vec<ToolRef> {
        self.inner.available_tools()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct PassthroughExecutor;

    #[async_trait::async_trait]
    impl ToolExecutor for PassthroughExecutor {
        async fn execute(&self, call: ToolCall) -> ToolResult {
            ToolResult {
                call_id: call.call_id,
                content: "ok".to_string(),
                is_error: false,
            }
        }

        fn available_tools(&self) -> Vec<ToolRef> {
            vec![ToolRef {
                name: "read_file".to_string(),
                description: "read".to_string(),
            }]
        }
    }

    fn call_at(path: &str) -> ToolCall {
        ToolCall {
            call_id: "c1".to_string(),
            name: "read_file".to_string(),
            input: serde_json::json!({"path": path}),
        }
    }

    fn write(path: &Path, content: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }

    #[tokio::test]
    async fn touching_a_nested_dir_offers_its_nearest_agents_md_once() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("repo");
        let nested = root.join("crates/app/src");
        write(&root.join(".git/HEAD"), "x\n");
        // Only the intermediate crate dir carries instructions.
        write(&root.join("crates/app/AGENTS.md"), "APP RULES");
        write(&nested.join("lib.rs"), "fn main() {}");

        let requests = Arc::new(DirectoryInstructions::new());
        let executor = AgentsInstructionsExecutor::new(
            PassthroughExecutor,
            Some(root.clone()),
            requests.clone(),
        );
        executor
            .execute(call_at(&nested.join("lib.rs").to_string_lossy()))
            .await;

        let drained = requests.take_all();
        assert_eq!(drained.len(), 1, "one directory offered: {drained:?}");
        assert_eq!(drained[0].0, root.join("crates/app"));
        assert_eq!(drained[0].1, "APP RULES");

        // A second touch in the same subtree offers nothing new.
        executor
            .execute(call_at(&nested.join("main.rs").to_string_lossy()))
            .await;
        assert!(requests.take_all().is_empty());
        assert_eq!(executor.lock_injected().len(), 1);
    }

    #[tokio::test]
    async fn the_project_root_tier_is_never_re_offered() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("repo");
        write(&root.join(".git/HEAD"), "x\n");
        write(&root.join("AGENTS.md"), "ROOT RULES");
        write(&root.join("src/lib.rs"), "fn main() {}");

        let requests = Arc::new(DirectoryInstructions::new());
        let executor = AgentsInstructionsExecutor::new(
            PassthroughExecutor,
            Some(root.clone()),
            requests.clone(),
        );
        executor
            .execute(call_at(&root.join("src/lib.rs").to_string_lossy()))
            .await;
        assert!(
            requests.take_all().is_empty(),
            "the root tier belongs to session assembly, not discovery"
        );
    }

    #[tokio::test]
    async fn calls_without_a_path_field_pass_through() {
        let requests = Arc::new(DirectoryInstructions::new());
        let executor = AgentsInstructionsExecutor::new(PassthroughExecutor, None, requests.clone());
        let outcome = executor
            .execute(ToolCall {
                call_id: "c9".to_string(),
                name: "shell".to_string(),
                input: serde_json::json!({"command": "ls"}),
            })
            .await;
        assert!(!outcome.is_error);
        assert!(requests.take_all().is_empty());
    }

    /// The upward walk stops at the filesystem root: the empty trailing
    /// ancestor of `Path::ancestors` is never a candidate, so nothing can
    /// resolve against the process cwd or re-offer the cwd tier.
    #[test]
    fn the_walk_stops_at_the_filesystem_root() {
        let requests = Arc::new(DirectoryInstructions::new());
        let executor = AgentsInstructionsExecutor::new(PassthroughExecutor, None, requests.clone());
        let candidates = executor.candidate_dirs(Path::new("/w/repo/src"));
        assert!(
            candidates.iter().all(|d| !d.as_os_str().is_empty()),
            "empty ancestor must not be a candidate: {candidates:?}"
        );
    }
}
