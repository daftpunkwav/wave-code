//! wavecode-tools: tool framework and built-in tool set.
//!
//! Shape: the [`Tool`] trait, the [`Registry`] registry, built-in file tools
//! (`read_file` / `write_file` / `edit_file` / `list_dir`), search tools
//! (`grep` / `glob`), the `shell` tool, and the session task-list tool (`todo_write`,
//! P4 deepagents planning). File and search tools confine all paths
//! under [`ToolCtx::cwd`] via `path_guard`, guarding against `..` escapes and absolute-path breakouts.
//! Every built-in tool's execute is truly async (`tokio::fs` / `tokio::process`;
//! grep/glob directory traversal is a sync API wrapped in `spawn_blocking` internally). Later milestones
//! add web / browser tools; the execution pipeline (schema validation, hooks, permission approval) is orchestrated by core.

mod fs;
mod lsp;
mod path_guard;
mod script;
mod search;
mod shell_tool;
mod todo_tool;
mod webfetch;

pub use todo_tool::{TodoItem, TodoStatus, TodoStore, TodoWrite, format_todos};

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, RwLock};

/// Unified recovery policy for poisoned locks (single decision point for this crate): these locks guard
/// single-operation critical sections (one insert / assignment / read), and a panic while holding a lock
/// leaves no half-broken invariant behind -- so take the guard back and keep going on poisoning
/// (the panic already propagated on its original thread) instead of cascading into a secondary panic.
pub(crate) fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Read-side RwLock counterpart of [`lock`].
pub(crate) fn read<T>(l: &RwLock<T>) -> std::sync::RwLockReadGuard<'_, T> {
    l.read().unwrap_or_else(|e| e.into_inner())
}

/// Write-side RwLock counterpart of [`lock`].
pub(crate) fn write<T>(l: &RwLock<T>) -> std::sync::RwLockWriteGuard<'_, T> {
    l.write().unwrap_or_else(|e| e.into_inner())
}

/// Tool execution context.
#[derive(Debug, Clone)]
pub struct ToolCtx {
    /// Working directory (an absolute path by convention): all file-tool paths are confined under it.
    pub cwd: std::path::PathBuf,
    /// Environment variable names to strip before spawning child processes (explicit lists such as the
    /// provider's `env_key`, injected by the assembly layer; the shell tool additionally strips sensitive
    /// suffix patterns automatically, see `shell_tool::sanitize_env`).
    pub deny_env: Vec<String>,
}

/// Tool output.
///
/// `is_error = true` means a business failure (missing file, non-unique match, missing params, ...),
/// and `content` is the human-readable reason, fed back to the model for self-correction.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolOutput {
    pub content: String,
    pub is_error: bool,
}

/// Tool abstraction.
#[async_trait::async_trait]
pub trait Tool: Send + Sync {
    /// Tool name (injected into the model, globally unique). Returns `&str` rather than `&'static str`:
    /// M3 MCP dynamic tools need runtime names.
    fn name(&self) -> &str;
    /// Capability description (in English, consumed by the model).
    fn description(&self) -> &str;
    /// JSON Schema of the parameters (injected into sampling requests).
    fn input_schema(&self) -> serde_json::Value;
    /// Read-only tools may run in parallel; writing tools must run serially.
    fn is_read_only(&self) -> bool;
    /// Destructive tools (deleting or irreversibly overwriting state, etc.) require approval by default (SPEC §11.1;
    /// P2 sandbox/HITL wiring, P1 only lands the trait surface). Non-destructive by default.
    fn is_destructive(&self) -> bool {
        false
    }
    /// Pre-execution semantic check (validation beyond JSON Schema, one stage of the SPEC §11.1 execution pipeline;
    /// invoked by core orchestration from P2 on). The default implementation passes everything through.
    async fn validate(&self, _input: &serde_json::Value) -> Result<()> {
        Ok(())
    }
    /// Execute. `Err` is only for implementation-level failures (io errors, etc.); business failures return
    /// `Ok(ToolOutput { is_error: true, .. })` and must never panic.
    ///
    /// All built-in tools are truly async: file tools use `tokio::fs`, shell uses
    /// `tokio::process`; grep/glob directory traversal is the `glob` crate's sync API,
    /// wrapped in `spawn_blocking` inside each tool (SPEC §19.3).
    async fn execute(&self, input: serde_json::Value, ctx: &ToolCtx) -> Result<ToolOutput>;
}

/// Unified error type for the tools crate.
#[derive(Debug, thiserror::Error)]
pub enum ToolsError {
    /// IO-layer error.
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    /// Invalid input (e.g. an empty path).
    #[error("invalid input: {message}")]
    InvalidInput { message: String },
    /// Path escapes the working directory.
    #[error("path escapes working directory: {path}")]
    PathEscape { path: String },
}

/// Crate-wide Result alias.
pub type Result<T> = std::result::Result<T, ToolsError>;

/// Shared handle for the tool-surface allowlist (P7 skills `allowed-tools`, SPEC §8.2):
/// with `Some(set)` only tools in the set may execute, `None` allows all.
///
/// Same shape as [`TodoStore`] -- held by the **session config** (per-session), not by
/// [`Registry`] (a pure tool index) or [`ToolCtx`] (a plain data snapshot rebuilt every turn).
/// Checked by the core execution pipeline before PreToolUse / sandbox decisions; written when a skill tool activates.
/// Semantics are **turn-scoped**: core clears it at each turn entry (first-version tradeoff, see core::skills).
#[derive(Clone, Default)]
pub struct ToolAllowlist {
    inner: Arc<Mutex<Option<HashSet<String>>>>,
}

impl ToolAllowlist {
    /// Set the allowlist (None = lift the restriction).
    pub fn set(&self, names: Option<HashSet<String>>) {
        *lock(&self.inner) = names;
    }

    /// Whether a tool may execute (no allowlist -> true).
    pub fn is_allowed(&self, name: &str) -> bool {
        lock(&self.inner)
            .as_ref()
            .is_none_or(|set| set.contains(name))
    }
}

/// Tool registry: indexed by name, used by the execution pipeline for lookup and for building the request-side `ToolSpec` list.
///
/// Holds **no** session-level planning / skills state ([`TodoStore`] / [`ToolAllowlist`])
/// -- those live in the session config so the registry does not become a cross-concern change hub.
///
/// The map is interior-mutable: late tools (registered after the registry
/// is already shared, e.g. once child services exist) become visible to
/// executors, policy, and model adapters without rebuilding assembly.
pub struct Registry {
    tools: Mutex<HashMap<String, Arc<dyn Tool>>>,
}

impl Registry {
    /// Register the built-in tools (file / search / shell), **excluding** `todo_write`.
    ///
    /// `todo_write` must be injected by the caller via [`Registry::with_todo_write`], sharing the same handle
    /// as the [`TodoStore`] in the session config.
    pub fn builtin() -> Self {
        let reg = Self {
            tools: Mutex::new(HashMap::new()),
        };
        reg.register(Arc::new(fs::ReadFile));
        reg.register(Arc::new(fs::WriteFile));
        reg.register(Arc::new(fs::EditFile));
        reg.register(Arc::new(fs::ListDir));
        reg.register(Arc::new(search::Grep));
        reg.register(Arc::new(search::Glob));
        reg.register(Arc::new(shell_tool::Shell));
        reg.register(Arc::new(script::PythonTool));
        reg.register(Arc::new(script::NodeTool));
        reg.register(Arc::new(lsp::DocumentSymbols));
        reg.register(Arc::new(lsp::GotoDefinition));
        reg.register(Arc::new(lsp::Hover));
        reg.register(Arc::new(lsp::FindReferences));
        reg.register(Arc::new(webfetch::WebFetch));
        reg
    }

    /// Register `todo_write`, sharing state with the session-level [`TodoStore`].
    pub fn with_todo_write(self, todos: TodoStore) -> Self {
        self.register(Arc::new(TodoWrite::new(todos)));
        self
    }

    /// Full built-in set (including `todo_write`) with its companion [`TodoStore`] -- session assembly should
    /// store the returned store in the session config so tools and steering share one source.
    pub fn builtin_with_todos() -> (Self, TodoStore) {
        let todos = TodoStore::default();
        (Self::builtin().with_todo_write(todos.clone()), todos)
    }

    /// Derive a by-name allowlist subset registry (the `allowed-tools` tool surface of a P7 skill fork):
    /// keep only listed tools (unknown names are silently skipped -- the list comes from user frontmatter,
    /// so a typo only costs that tool's availability, scoped to that skill).
    pub fn name_subset(&self, names: &[String]) -> Self {
        let reg = Self {
            tools: Mutex::new(HashMap::new()),
        };
        for name in names {
            if let Some(tool) = lock(&self.tools).get(name) {
                reg.register(tool.clone());
            }
        }
        reg
    }

    /// Derive a read-only subset registry (tool surface for P5 explore-type subagents): keep only
    /// `is_read_only()` tools (`todo_write` is not read-only and never enters the subset).
    pub fn read_only_subset(&self) -> Self {
        let reg = Self {
            tools: Mutex::new(HashMap::new()),
        };
        for tool in lock(&self.tools).values() {
            if tool.is_read_only() {
                reg.register(tool.clone());
            }
        }
        reg
    }

    /// Register a tool (from M3 on, MCP and other dynamic tools register here too); takes `&self` to support
    /// late registration during late assembly (see the struct docs).
    pub fn register(&self, tool: Arc<dyn Tool>) {
        lock(&self.tools).insert(tool.name().to_owned(), tool);
    }

    /// `ToolSpec` for every tool, sorted by name for stable output.
    pub fn specs(&self) -> Vec<wavecode_llm::ToolSpec> {
        let guard = lock(&self.tools);
        let mut tools: Vec<&Arc<dyn Tool>> = guard.values().collect();
        tools.sort_by_key(|t| t.name());
        tools
            .iter()
            .map(|t| wavecode_llm::ToolSpec {
                name: t.name().to_owned(),
                description: t.description().to_owned(),
                input_schema: t.input_schema(),
            })
            .collect()
    }

    /// Look up a tool by name.
    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        lock(&self.tools).get(name).cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// P7: the allowlist is unrestricted by default; after set() only listed tools run; clearing restores access.
    #[test]
    fn allowlist_gating() {
        let allowlist = ToolAllowlist::default();
        assert!(allowlist.is_allowed("shell"));
        allowlist.set(Some(HashSet::from(["read_file".to_owned()])));
        assert!(allowlist.is_allowed("read_file"));
        assert!(!allowlist.is_allowed("shell"));
        allowlist.set(None);
        assert!(allowlist.is_allowed("shell"));
    }

    /// P7: name_subset filters by name; unknown names are silently skipped; read/write tools are kept per the list.
    #[test]
    fn name_subset_filters_by_name() {
        let (reg, _todos) = Registry::builtin_with_todos();
        let sub = reg.name_subset(&["read_file".to_owned(), "grep".to_owned(), "nope".to_owned()]);
        assert!(sub.get("read_file").is_some());
        assert!(sub.get("grep").is_some());
        assert!(sub.get("write_file").is_none());
        assert!(sub.get("shell").is_none());
        assert!(sub.get("todo_write").is_none(), "unlisted tools are unavailable");
        // specs output is stable (sorted by name).
        let names: Vec<String> = sub.specs().into_iter().map(|s| s.name).collect();
        assert_eq!(names, vec!["grep", "read_file"]);
    }

    /// Session-assembly contract: `name_subset` (skill-fork tool-surface derivation) keeps the same
    /// `todo_write` instance -- state written through the subset tool must be visible to the session config's
    /// [`TodoStore`] (same Arc handle); otherwise todo_write, steering, and
    /// context injection would each write/read their own copy and silently diverge.
    #[tokio::test]
    async fn todo_write_via_name_subset_shares_session_store() {
        let (reg, todos) = Registry::builtin_with_todos();
        let sub = reg.name_subset(&["todo_write".to_owned()]);
        let tool = sub.get("todo_write").expect("listed todo_write should be kept");
        let ctx = ToolCtx {
            cwd: std::env::temp_dir(),
            deny_env: Vec::new(),
        };
        let out = tool
            .execute(
                serde_json::json!({"todos": [{"content": "a", "status": "in_progress"}]}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(!out.is_error);
        assert_eq!(todos.snapshot().len(), 1, "subset tool and session store must share one handle");
    }

    /// Late registration reaches already-shared handles: tools appended
    /// after the registry is behind an `Arc` (skill tool, MCP tools) are
    /// visible to executors, policy, and model adapters without rebuilds.
    #[test]
    fn late_registration_reaches_shared_handles() {
        struct LateTool;
        #[async_trait::async_trait]
        impl Tool for LateTool {
            fn name(&self) -> &str {
                "late_tool"
            }
            fn description(&self) -> &str {
                "test-only late tool"
            }
            fn input_schema(&self) -> serde_json::Value {
                serde_json::json!({})
            }
            fn is_read_only(&self) -> bool {
                true
            }
            async fn execute(
                &self,
                _input: serde_json::Value,
                _ctx: &ToolCtx,
            ) -> Result<ToolOutput> {
                Ok(ToolOutput {
                    content: String::new(),
                    is_error: false,
                })
            }
        }
        let registry = Arc::new(Registry::builtin());
        assert!(registry.get("late_tool").is_none());
        let before = registry.specs().len();
        registry.register(Arc::new(LateTool));
        assert!(registry.get("late_tool").is_some());
        assert_eq!(registry.specs().len(), before + 1);
    }

    /// New tools are registered with the expected read-only surface:
    /// scripts can write (not read-only), LSP navigation and webfetch are read-only.
    #[test]
    fn builtin_registers_script_lsp_and_webfetch() {
        let reg = Registry::builtin();
        for name in [
            "run_python",
            "run_node",
            "document_symbols",
            "goto_definition",
            "hover",
            "find_references",
            "webfetch",
        ] {
            assert!(reg.get(name).is_some(), "{name} must be registered");
        }
        assert!(!reg.get("run_python").unwrap().is_read_only());
        assert!(!reg.get("run_node").unwrap().is_read_only());
        for name in [
            "document_symbols",
            "goto_definition",
            "hover",
            "find_references",
            "webfetch",
        ] {
            assert!(
                reg.get(name).unwrap().is_read_only(),
                "{name} must be read-only"
            );
        }
        // Read-only explore subset picks up the navigation/fetch tools.
        let explore = reg.read_only_subset();
        for name in [
            "document_symbols",
            "goto_definition",
            "hover",
            "find_references",
            "webfetch",
        ] {
            assert!(
                explore.get(name).is_some(),
                "{name} must be in the explore subset"
            );
        }
        assert!(explore.get("run_python").is_none());
    }
}
