//! wavecode-tools: tool framework and built-in tool set.
//!
//! Shape: the [`Tool`] trait, the [`Registry`] registry, built-in file tools
//! (`read` / `write` / `edit` / `view` / `present`), search tools
//! (`grep` / `glob`), command tools (`shell` / `python` / `node`),
//! LSP tools (`lsp_symbols` / `lsp_definition` / `lsp_hover` / `lsp_references`),
//! web tools (`web_fetch` / `web_search`), the spill reader (`spill`), and the
//! session task-list tool (`todowrite`, deepagents-style planning), the
//! child-task delegation tools (`task`, `task_output`, `task_stop`,
//! `task_continue`, fed by the `action-tasks` seam), and the `ask_user`
//! question surface (the sandbox routes its valid calls to the question
//! flow by tool name, so the tool body stays pure validation). File and
//! search tools confine all paths under [`ToolCtx::cwd`] via `path_guard`,
//! guarding against `..` escapes and absolute-path breakouts.
//! Every built-in tool's execute is truly async (`tokio::fs` / `tokio::process`;
//! grep/glob directory traversal is a sync API wrapped in `spawn_blocking` internally).
//! The execution pipeline (allowlist checks, hooks, permission approval) is orchestrated by the run loop (`runtime-runner`).

mod agent_task_tool;
mod ask_user_tool;
mod fs;
mod html;
mod lsp;
mod path_guard;
mod script;
mod search;
mod shell_tool;
mod spill_tool;
mod task_tools;
mod todo_tool;
mod web_fetch;
mod web_search;

pub use shell_tool::{RunHandoff, RunSnapshot, shell_with_handoff};

pub use agent_task_tool::{AgentDef, TaskTool, discover_agent_defs};
pub use ask_user_tool::{AskUserTool, MAX_QUESTION_OPTIONS};
pub use fs::{Present, PresentStore, ReadImage};
pub use lsp::{
    DocumentSymbols, FindReferences, GotoDefinition, Hover, LspDiagnostics, LspProviders,
};
pub use spill_tool::SpillRead;
pub use task_tools::{TaskContinueTool, TaskOutputTool, TaskStopTool};
pub use todo_tool::{TodoItem, TodoStatus, TodoStore, TodoWrite, format_todos};
pub use web_search::{DuckDuckGoBackend, SearchBackend, SearchResult, WebSearch};

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

/// Required string field helper for tool inputs: missing or blank
/// becomes a business error the model can self-correct from. Shared by
/// the model tools that live next to their engines (job, workflow) so
/// the error wording stays identical everywhere.
pub fn required_str<'a>(
    input: &'a serde_json::Value,
    field: &str,
) -> std::result::Result<&'a str, ToolOutput> {
    match input.get(field).and_then(|value| value.as_str()) {
        Some(value) if !value.trim().is_empty() => Ok(value),
        _ => Err(ToolOutput {
            content: format!("missing required field: {field}"),
            is_error: true,
        }),
    }
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

/// True when an env var name looks like a secret carrier (case-insensitive).
///
/// Fail-closed shapes: `_SECRET` / `_TOKEN` / `_PASSW` / `_PRIVATE` segments,
/// `_KEY` / `_PAT` suffixes (AWS_SECRET_ACCESS_KEY, *_PRIVATE_KEY, GITHUB_PAT,
/// bare API_KEY), and the bare names SECRET / TOKEN / PASSWORD / PRIVATE /
/// KEY / PAT. A pure suffix list misses real shapes (`AWS_SECRET_ACCESS_KEY`
/// ends in `_KEY`, not `_API_KEY`), so match segments and suffixes instead.
/// Anything else passes so normal configuration stays visible to children.
///
/// The canonical copy lives here so every process-spawning surface (shell,
/// pty, background jobs) scrubs the same shapes; point new spawn paths at
/// this function instead of growing private copies.
pub fn is_sensitive_env_name(name: &str) -> bool {
    const MARKERS: [&str; 4] = ["_SECRET", "_TOKEN", "_PASSW", "_PRIVATE"];
    const SUFFIXES: [&str; 2] = ["_KEY", "_PAT"];
    const BARE: [&str; 6] = ["SECRET", "TOKEN", "PASSWORD", "PRIVATE", "KEY", "PAT"];
    let upper = name.to_uppercase();
    MARKERS.iter().any(|m| upper.contains(m))
        || SUFFIXES.iter().any(|s| upper.ends_with(s))
        || BARE.contains(&upper.as_str())
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

/// Content prefix marking a `ToolOutput` whose failure is an *implementation
/// fault* (io error, spawn failure, ...) rather than a business failure.
///
/// Executor adapters ride faults as error results so the model can see the
/// reason; this marker keeps them distinguishable from business failures in
/// transcripts and lets the MCP serve surface map them back to a
/// protocol-level internal error. Canonical single definition: the producer
/// (composition-root executor adapters) and the consumer (`operations-
/// gateway`'s MCP serve) must agree on the exact bytes.
///
/// Cross-crate contract, both halves normative: the producer stamps the
/// prefix onto **error results only** (`is_error = true`), and the
/// consumer maps a result to a protocol internal error only when
/// `is_error` *and* the prefix both hold — a successful output opening
/// with the same bytes is never a fault. Residual, accepted: a business
/// failure whose reason coincidentally opens with the prefix still reads
/// as a protocol-level internal error on the serve surface.
pub const TOOL_FAULT_PREFIX: &str = "tool fault:";

/// Build a business-failure output: the reason is fed back to the model for self-correction.
///
/// Canonical crate-wide copy (like [`is_sensitive_env_name`]): the failure-semantics contract,
/// message wording included, is maintained in one place instead of a per-module private copy.
pub(crate) fn err_output(reason: impl Into<String>) -> ToolOutput {
    ToolOutput {
        content: reason.into(),
        is_error: true,
    }
}

/// Build a success output: plain content with `is_error` cleared.
///
/// Canonical crate-wide copy (like [`err_output`]): one definition instead
/// of a per-module private copy of the `ToolOutput` shape.
pub(crate) fn ok_output(content: impl Into<String>) -> ToolOutput {
    ToolOutput {
        content: content.into(),
        is_error: false,
    }
}

/// Extract a required string parameter; missing or mistyped params yield a business-failure output.
pub(crate) fn req_str<'a>(
    input: &'a serde_json::Value,
    key: &str,
) -> std::result::Result<&'a str, ToolOutput> {
    input
        .get(key)
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| {
            err_output(format!(
                "missing or invalid parameter '{key}' (string required)"
            ))
        })
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
    /// Declarative capability class the policy layer keys on; the default
    /// is the cautious [`wavecode_protocol::ToolKind::Other`], which keeps the approval path.
    /// Builtin tools classify themselves so the sandbox never matches on
    /// names (architecture rule 4).
    fn kind(&self) -> wavecode_protocol::ToolKind {
        wavecode_protocol::ToolKind::Other
    }
    /// Destructive tools (deleting or irreversibly overwriting state, etc.) require approval by default. Non-destructive by default.
    fn is_destructive(&self) -> bool {
        false
    }
    /// Pre-execution semantic check (validation beyond JSON Schema). A
    /// provided method: the default passes everything through, and no
    /// workspace call site invokes it today — tools must not rely on it
    /// for safety; containment and policy do not depend on it.
    async fn validate(&self, _input: &serde_json::Value) -> Result<()> {
        Ok(())
    }
    /// Execute. `Err` is only for implementation-level failures (io errors, etc.); business failures return
    /// `Ok(ToolOutput { is_error: true, .. })` and must never panic.
    ///
    /// # Environment responsibility (implementation-side contract)
    ///
    /// An implementation that spawns child processes **owns the child's
    /// environment**: it must strip [`ToolCtx::deny_env`] itself (the
    /// assembly layer cannot reach into a tool's spawn) and should route
    /// the scrub through the shared helpers so every spawn path matches —
    /// [`is_sensitive_env_name`] for the shape fallback and
    /// `shell_tool::sanitize_env` (crate-private) for the combined strip.
    /// All model-facing built-in spawns comply (`shell`, `python`, `node`,
    /// the job handoff, and the LSP tools — a `server_command` can arrive
    /// as model input, so its process is scrubbed like any other). Only
    /// operator-configured spawn paths outside this crate's tools (hooks,
    /// the interactive PTY shell) keep the full environment by trust level.
    ///
    /// All built-in tools are truly async: file tools use `tokio::fs`, shell uses
    /// `tokio::process`; grep/glob directory traversal is the `glob` crate's sync API,
    /// wrapped in `spawn_blocking` inside each tool.
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

/// Shared handle for a name-based tool allowlist (skills `allowed-tools`):
/// with `Some(set)` only tools in the set may execute, `None` allows all.
///
/// Same shape as [`TodoStore`] -- a per-session shared handle, not owned by
/// [`Registry`] (a pure tool index) or [`ToolCtx`] (a plain data snapshot rebuilt every turn).
/// The `allowed-tools` enforcement that actually runs today lives in the run
/// loop's per-run allowlist (`runtime_runner::RunAllowlist`) and
/// [`Registry::name_subset`]; no pipeline stage reads this handle.
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

/// Serializes tests that touch environment variables: `set_var` /
/// `remove_var` mutate process-global state, so parallel unit tests reading
/// the same names (e.g. the sandbox routing check) must run mutually
/// exclusive to stay deterministic. The OS-sandbox watcher itself needs no
/// such exclusion anymore: it pairs only pids explicitly committed via
/// `commit_confined_spawn`, so a child spawned by an unrelated test is never
/// confined and a confined child is never left unpaired. One lock for the
/// whole crate: separate per-module mutexes would not exclude each other.
#[cfg(test)]
pub(crate) static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

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
    /// Register the built-in tools (file / search / command / LSP / web /
    /// spill), **excluding** `todowrite`.
    ///
    /// `todowrite` must be injected by the caller via [`Registry::with_todo_write`], sharing the same handle
    /// as the [`TodoStore`] in the session config.
    ///
    /// Tests-and-hermetic-registries convenience: this builds its own
    /// spill store on the default root. Production assembly must call
    /// [`Registry::builtin_with_spill_store`] with the session's single
    /// shared store instead — separate instances over one root would race
    /// their manifest read-modify-write cycles and drop entries.
    pub fn builtin() -> Self {
        Self::builtin_with_spill_store(std::sync::Arc::new(wavecode_context::SpillStore::new(
            wavecode_context::default_spill_store_root(),
        )))
    }

    /// [`Registry::builtin`] against an injected spill store: the
    /// composition root builds the session's one shared `SpillStore` and
    /// hands the same `Arc` to the shell tool (via
    /// [`shell_tool::shell_with_handoff`] re-registration) and the pruning
    /// executor, so every spill writer shares one manifest ledger. The
    /// registry stays a pure registry — it never picks a store root.
    pub fn builtin_with_spill_store(spill: std::sync::Arc<wavecode_context::SpillStore>) -> Self {
        let reg = Self {
            tools: Mutex::new(HashMap::new()),
        };
        // One freshness ledger across the file trio: `read` records what
        // the session saw, `write`/`edit` refuse to run over an outside
        // change that landed after that view.
        let ledger = fs::FileLedger::new();
        reg.register(Arc::new(fs::ReadFile::new(ledger.clone())));
        reg.register(Arc::new(fs::WriteFile::new(ledger.clone())));
        reg.register(Arc::new(fs::EditFile::new(ledger)));
        reg.register(Arc::new(search::Grep));
        reg.register(Arc::new(search::Glob));
        // The shell tool spills the full text of truncated outputs into
        // the injected store — the same one the `spill` tool reads back
        // (below) and the context prune prunes through. No handoff here:
        // session assembly re-registers this entry with the job service
        // wired, so timeouts promote instead of kill.
        reg.register(Arc::new(shell_tool::Shell::new(Some(spill.clone()), None)));
        reg.register(Arc::new(script::PythonTool));
        reg.register(Arc::new(script::NodeTool));
        reg.register(Arc::new(lsp::DocumentSymbols::new()));
        reg.register(Arc::new(lsp::GotoDefinition::new()));
        reg.register(Arc::new(lsp::Hover::new()));
        reg.register(Arc::new(lsp::FindReferences::new()));
        reg.register(Arc::new(web_fetch::WebFetch));
        reg.register(Arc::new(web_search::WebSearch::new()));
        reg.register(Arc::new(fs::ReadImage));
        // Present records into a registry-scoped store (first version: no
        // steering consumer needs the handle, unlike todowrite).
        reg.register(Arc::new(fs::Present::new(fs::PresentStore::default())));
        reg.register(Arc::new(spill_tool::SpillRead::new(spill)));
        reg
    }

    /// Register `todowrite`, sharing state with the session-level [`TodoStore`].
    pub fn with_todo_write(self, todos: TodoStore) -> Self {
        self.register(Arc::new(TodoWrite::new(todos)));
        self
    }

    /// Full built-in set (including `todowrite`) with its companion [`TodoStore`] -- session assembly should
    /// store the returned store in the session config so tools and steering share one source.
    pub fn builtin_with_todos() -> (Self, TodoStore) {
        Self::builtin_with_spill_store_and_todos(std::sync::Arc::new(
            wavecode_context::SpillStore::new(wavecode_context::default_spill_store_root()),
        ))
    }

    /// [`Registry::builtin_with_spill_store`] plus the `todowrite` pair:
    /// the production session-assembly entry (one shared spill store, one
    /// shared todo store).
    pub fn builtin_with_spill_store_and_todos(
        spill: std::sync::Arc<wavecode_context::SpillStore>,
    ) -> (Self, TodoStore) {
        let todos = TodoStore::default();
        (
            Self::builtin_with_spill_store(spill).with_todo_write(todos.clone()),
            todos,
        )
    }

    /// Derive a by-name allowlist subset registry (the `allowed-tools` tool surface of a skill fork):
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

    /// Derive a read-only subset registry (tool surface for explore-type subagents): keep only
    /// `is_read_only()` tools (`todowrite` is not read-only and never enters the subset).
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
    ///
    /// **Replace semantics** (declared contract): a tool whose name is
    /// already registered is silently replaced — re-registration is the
    /// sanctioned way session assembly upgrades a late-wired entry (the
    /// shell tool re-registers with the job service wired), so this is
    /// insert-or-replace, never an error. Register nothing you cannot
    /// afford to have swapped.
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

    /// The allowlist is unrestricted by default; after set() only listed tools run; clearing restores access.
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

    /// name_subset filters by name; unknown names are silently skipped; read/write tools are kept per the list.
    #[test]
    fn name_subset_filters_by_name() {
        let (reg, _todos) = Registry::builtin_with_todos();
        let sub = reg.name_subset(&["read".to_owned(), "grep".to_owned(), "nope".to_owned()]);
        assert!(sub.get("read").is_some());
        assert!(sub.get("grep").is_some());
        assert!(sub.get("write").is_none());
        assert!(sub.get("shell").is_none());
        assert!(
            sub.get("todowrite").is_none(),
            "unlisted tools are unavailable"
        );
        // specs output is stable (sorted by name).
        let names: Vec<String> = sub.specs().into_iter().map(|s| s.name).collect();
        assert_eq!(names, vec!["grep", "read"]);
    }

    /// Session-assembly contract: `name_subset` (skill-fork tool-surface derivation) keeps the same
    /// `todowrite` instance -- state written through the subset tool must be visible to the session config's
    /// [`TodoStore`] (same Arc handle); otherwise todowrite, steering, and
    /// context injection would each write/read their own copy and silently diverge.
    #[tokio::test]
    async fn todowrite_via_name_subset_shares_session_store() {
        let (reg, todos) = Registry::builtin_with_todos();
        let sub = reg.name_subset(&["todowrite".to_owned()]);
        let tool = sub
            .get("todowrite")
            .expect("listed todowrite should be kept");
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
        assert_eq!(
            todos.snapshot().len(),
            1,
            "subset tool and session store must share one handle"
        );
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
    /// scripts can write (not read-only), LSP navigation, web_fetch, web_search,
    /// image, spill, and present tools are read-only.
    #[test]
    fn builtin_registers_script_lsp_and_web_fetch() {
        let reg = Registry::builtin();
        for name in [
            "python",
            "node",
            "lsp_symbols",
            "lsp_definition",
            "lsp_hover",
            "lsp_references",
            "web_fetch",
            "web_search",
            "view",
            "spill",
            "present",
        ] {
            assert!(reg.get(name).is_some(), "{name} must be registered");
        }
        assert!(!reg.get("python").unwrap().is_read_only());
        assert!(!reg.get("node").unwrap().is_read_only());
        for name in [
            "lsp_symbols",
            "lsp_definition",
            "lsp_hover",
            "lsp_references",
            "web_fetch",
            "web_search",
            "view",
            "spill",
            "present",
        ] {
            assert!(
                reg.get(name).unwrap().is_read_only(),
                "{name} must be read-only"
            );
        }
        // Read-only explore subset picks up the navigation/fetch tools.
        let explore = reg.read_only_subset();
        for name in [
            "lsp_symbols",
            "lsp_definition",
            "lsp_hover",
            "lsp_references",
            "web_fetch",
            "web_search",
            "view",
            "spill",
            "present",
        ] {
            assert!(
                explore.get(name).is_some(),
                "{name} must be in the explore subset"
            );
        }
        assert!(explore.get("python").is_none());
    }
}
