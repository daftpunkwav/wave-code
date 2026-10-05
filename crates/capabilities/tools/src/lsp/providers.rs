//! Lazy per-extension language-server registry: pooled clients spawn once,
//! initialize once, and are reused across calls; a failed request drops the
//! pooled client so the next call respawns fresh.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde_json::Value;

use crate::{Result, ToolOutput, err_output, lock};

use super::DiagnosticsStore;
use super::client::{LspClient, path_to_uri};
use super::transport::AnyTransport;

/// Normalize a file extension key: strip a leading dot, lowercase (`RS`
/// and `.rs` both become `rs`).
fn normalize_ext(ext: &str) -> String {
    ext.strip_prefix('.').unwrap_or(ext).to_lowercase()
}

/// Extension of a model-provided path (lexical; empty when none).
pub(super) fn extension_of(path: &str) -> String {
    Path::new(path)
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_lowercase()
}

/// Lazy per-provider language-server registry, keyed by file extension.
///
/// Servers spawn once per extension on first use (initialized against the
/// workspace root) and are reused across calls; a failed request drops the
/// pooled client so the next call respawns fresh. Dropping the registry reaps
/// every child via `kill_on_drop` (polite `shutdown` first with
/// [`LspProviders::shutdown_all`]).
/// One pooled language-server connection (shared across calls for an extension).
type PooledClient = Arc<tokio::sync::Mutex<Option<LspClient<AnyTransport>>>>;

/// Target of one LSP request (bundles the pooled-call params).
pub(super) struct RequestTarget<'a> {
    pub(super) uri: &'a str,
    pub(super) line: u32,
    pub(super) character: u32,
}

/// Lazy per-provider language-server registry, keyed by file extension.
///
/// Servers spawn once per extension on first use (initialized against the
/// workspace root) and are reused across calls; a failed request drops the
/// pooled client so the next call respawns fresh. Dropping the registry reaps
/// every child via `kill_on_drop` (polite `shutdown` first with
/// [`LspProviders::shutdown_all`]).
pub struct LspProviders {
    /// Workspace root: server working directory and `initialize` rootUri.
    /// Required at construction (no per-call cwd guessing).
    root: PathBuf,
    root_uri: String,
    commands: std::sync::Mutex<HashMap<String, String>>,
    pooled: std::sync::Mutex<HashMap<String, PooledClient>>,
    /// Shared diagnostics sink installed on every client this registry holds:
    /// pushes observed during any call land here and survive across calls
    /// (read back by the `lsp_diagnostics` tool via [`Self::diagnostics_text`]).
    pub(super) diagnostics: Arc<std::sync::Mutex<DiagnosticsStore>>,
    /// Environment names stripped from every pooled server spawn (the same
    /// list the shell tool receives, so a registered server inherits the
    /// same scrubbed environment a model-driven command would).
    deny_env: Vec<String>,
    spawns: AtomicU64,
}

impl LspProviders {
    /// Build for `workspace_root` (must be the workspace directory; used as
    /// the server working directory and the `initialize` rootUri). `deny_env`
    /// is stripped from every pooled server spawn (see `ChildLsp::spawn`).
    pub fn new(workspace_root: PathBuf, deny_env: Vec<String>) -> Self {
        let root_uri = path_to_uri(&workspace_root);
        Self {
            root: workspace_root,
            root_uri,
            commands: std::sync::Mutex::new(HashMap::new()),
            pooled: std::sync::Mutex::new(HashMap::new()),
            diagnostics: Arc::new(std::sync::Mutex::new(DiagnosticsStore::default())),
            deny_env,
            spawns: AtomicU64::new(0),
        }
    }

    /// Register `server_command` for a file extension (`rs`, `.py`, ...).
    /// Re-registering an extension replaces the command and drops the pooled
    /// client (the old server is reaped on drop).
    pub fn register(&self, extension: &str, server_command: String) {
        let key = normalize_ext(extension);
        lock(&self.commands).insert(key.clone(), server_command);
        lock(&self.pooled).remove(&key);
    }

    /// Resolve the registered command for a path's extension, if any.
    pub fn command_for_path(&self, path: &str) -> Option<String> {
        lock(&self.commands).get(&extension_of(path)).cloned()
    }

    /// Real server spawns so far (failed spawns do not count).
    pub fn spawn_count(&self) -> u64 {
        self.spawns.load(Ordering::Relaxed)
    }

    /// Test-only: install an already-connected client for an extension
    /// (backed by an in-memory fake; no real spawn, counter untouched). The
    /// injected client gets the shared diagnostics sink too, matching what
    /// pooled spawns get.
    #[cfg(test)]
    pub fn insert_ready(&self, extension: &str, client: LspClient<AnyTransport>) {
        let client = client.with_diagnostics_sink(Arc::clone(&self.diagnostics));
        lock(&self.pooled).insert(
            normalize_ext(extension),
            Arc::new(tokio::sync::Mutex::new(Some(client))),
        );
    }

    /// Polite teardown of every pooled server (best-effort; children are
    /// reaped by `kill_on_drop` regardless).
    pub async fn shutdown_all(&self) {
        let handles: Vec<_> = lock(&self.pooled).values().cloned().collect();
        for handle in handles {
            let mut guard = handle.lock().await;
            if let Some(mut client) = guard.take() {
                let _ = client.shutdown(Duration::from_millis(1_000)).await;
            }
        }
    }

    /// Render the diagnostics store's contents (see
    /// `DiagnosticsStore::render`) — the read side of the sink that pooled
    /// clients feed (the `lsp_diagnostics` tool).
    pub fn diagnostics_text(&self, uri_filter: Option<&str>) -> String {
        lock(&self.diagnostics).render(uri_filter)
    }

    /// Run one request against the pooled server for `ext` (spawn +
    /// initialize lazily on first use). `Err` is a business-failure output.
    pub(super) async fn pooled_call(
        &self,
        ext: &str,
        target: RequestTarget<'_>,
        timeout: Duration,
        method: &str,
        call: impl AsyncFnOnce(&mut LspClient<AnyTransport>, &str, u32, u32, Duration) -> Result<Value>,
    ) -> std::result::Result<Value, ToolOutput> {
        let command = lock(&self.commands).get(ext).cloned().ok_or_else(|| {
            err_output(format!(
                "no language server registered for extension '{ext}': provide server_command explicitly or register one"
            ))
        })?;
        let handle = {
            let mut pooled = lock(&self.pooled);
            pooled
                .entry(ext.to_owned())
                .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(None)))
                .clone()
        };
        let mut guard = handle.lock().await;
        if guard.is_none() {
            let transport =
                AnyTransport::spawn(&command, &self.root, &self.deny_env).map_err(|e| {
                    err_output(format!("failed to spawn language server '{command}': {e}"))
                })?;
            let mut client =
                LspClient::new(transport).with_diagnostics_sink(Arc::clone(&self.diagnostics));
            if let Err(e) = client.initialize(&self.root_uri, timeout).await {
                return Err(err_output(format!("LSP initialize failed: {e}")));
            }
            self.spawns.fetch_add(1, Ordering::Relaxed);
            *guard = Some(client);
        }
        let client = guard.as_mut().expect("pooled client just installed");
        match call(client, target.uri, target.line, target.character, timeout).await {
            Ok(v) => Ok(v),
            Err(e) => {
                // The server may have died mid-request: drop the client so the
                // next call respawns fresh instead of reusing a wedged pipe.
                *guard = None;
                Err(err_output(format!("LSP {method} failed: {e}")))
            }
        }
    }
}
