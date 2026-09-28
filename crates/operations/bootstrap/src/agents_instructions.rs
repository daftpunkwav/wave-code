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

/// Character cap for one nested `AGENTS.md` injection (~4k tokens), with
/// the overflow replaced by an explicit truncation marker so the model
/// knows the text was cut rather than silently losing its tail.
pub const MAX_NESTED_INSTRUCTION_CHARS: usize = 16_000;

fn truncate_instructions(content: String) -> String {
    if content.chars().count() <= MAX_NESTED_INSTRUCTION_CHARS {
        return content;
    }
    let mut head: String = content.chars().take(MAX_NESTED_INSTRUCTION_CHARS).collect();
    head.push_str(&format!(
        "

[truncated: this AGENTS.md exceeds {MAX_NESTED_INSTRUCTION_CHARS} characters;          the remainder was cut at injection time]"
    ));
    head
}

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

    /// Offer the nearest not-yet-injected `AGENTS.md` for `dir`. Files
    /// past [`MAX_NESTED_INSTRUCTION_CHARS`] are offered truncated: the
    /// rules' head (conventions, boundaries) is what matters, and an
    /// oversized injection must not be able to blow the budget in one
    /// loop head.
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
                    self.requests
                        .offer(directory, truncate_instructions(content));
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

    /// A compaction replaced the whole history, so every injected
    /// instruction left it: clear the seen-set and let the rules come
    /// back the next time their directory is touched. Directories the
    /// model never revisits stay out of context on purpose.
    fn note_compacted(&self) {
        self.injected
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
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

    /// After a compaction clears the seen-set, a directory whose rules
    /// were already injected can be offered again.
    #[tokio::test]
    async fn a_compaction_lets_instructions_come_back() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("repo");
        write(
            &root.join(".git/HEAD"),
            "x
",
        );
        write(&root.join("crates/app/AGENTS.md"), "APP RULES");
        write(&root.join("crates/app/src/lib.rs"), "fn main() {}");

        let requests = Arc::new(DirectoryInstructions::new());
        let executor = AgentsInstructionsExecutor::new(
            PassthroughExecutor,
            Some(root.clone()),
            requests.clone(),
        );
        executor
            .execute(call_at(
                &root.join("crates/app/src/lib.rs").to_string_lossy(),
            ))
            .await;
        assert_eq!(requests.take_all().len(), 1);

        executor.note_compacted();
        executor
            .execute(call_at(
                &root.join("crates/app/src/lib.rs").to_string_lossy(),
            ))
            .await;
        let drained = requests.take_all();
        assert_eq!(
            drained,
            vec![(root.join("crates/app"), "APP RULES".to_string())],
            "the rule re-offers after compaction: {drained:?}"
        );
    }

    /// An oversized AGENTS.md injects truncated, with an explicit marker
    /// instead of a silently lost tail.
    #[tokio::test]
    async fn oversized_instructions_are_truncated_with_a_marker() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("repo");
        write(
            &root.join(".git/HEAD"),
            "x
",
        );
        write(
            &root.join("big/AGENTS.md"),
            &"x".repeat(MAX_NESTED_INSTRUCTION_CHARS + 500),
        );
        write(&root.join("big/src/lib.rs"), "fn main() {}");

        let requests = Arc::new(DirectoryInstructions::new());
        let executor =
            AgentsInstructionsExecutor::new(PassthroughExecutor, Some(root), requests.clone());
        executor
            .execute(call_at(
                &dir.path().join("repo/big/src/lib.rs").to_string_lossy(),
            ))
            .await;
        let drained = requests.take_all();
        assert_eq!(drained.len(), 1);
        let content = &drained[0].1;
        assert!(
            content.contains("[truncated: this AGENTS.md exceeds")
                && content.ends_with("the remainder was cut at injection time]"),
            "{content}"
        );
        assert!(
            content.chars().count() < MAX_NESTED_INSTRUCTION_CHARS + 200,
            "head plus the marker, nothing more: {}",
            content.chars().count()
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
