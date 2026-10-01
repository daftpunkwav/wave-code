/*! @file Lsp
 * @description Minimal stdio LSP client plus code-navigation tools.
 *
 * Responsibilities:
 * - Frame/unframe Content-Length JSON-RPC messages over async stdio
 * - Speak initialize/shutdown plus symbol/definition/hover/references
 * - Record textDocument/publishDiagnostics pushes into a bounded store
 * - Expose five read-only tools: four spawn an explicit server command,
 *   lsp_diagnostics only reads the recorded diagnostics store
 *
 * This module must not depend on: UI-layer components, bundled servers.
 */

//! LSP navigation tools (`lsp_symbols` / `lsp_definition` / `lsp_hover` /
//! `lsp_references`): with an explicit `server_command` each call spawns the
//! caller-provided language server over stdio, runs `initialize`, issues one
//! request, then `shutdown`s. With a [`LspProviders`] registry the server for
//! the file extension is spawned lazily once, initialized once, and reused
//! across calls (shutdown on registry drop).
//! Servers push `textDocument/publishDiagnostics` while a call's requests are
//! in flight; registry-backed clients record those pushes into the registry's
//! shared bounded store, and `lsp_diagnostics` renders it (no server call).
//! No server is bundled: `server_command` names the binary explicitly
//! (e.g. `rust-analyzer`, `pyright-langserver --stdio`).
//!
//! Failure semantics follow the crate convention: business failures (bad
//! params, spawn failure, protocol errors) return `Ok(is_error=true)`;
//! `Err` is reserved for implementation faults.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};

use crate::{Result, Tool, ToolCtx, ToolOutput, ToolsError, err_output, lock, ok_output, req_str};

/// Default per-read timeout: 30 s.
const DEFAULT_TIMEOUT_MS: u64 = 30_000;
/// Timeout ceiling: 300 s, values above are clamped.
const MAX_TIMEOUT_MS: u64 = 300_000;

/// Cap on diagnostics kept per file: entries beyond the cap are dropped on
/// each push (a chatty server cannot grow memory without bound; each push
/// still fully replaces the file's previous list per LSP semantics).
const MAX_DIAGNOSTICS_PER_FILE: usize = 200;
/// Cap on tracked files: the oldest-inserted file is evicted FIFO when a new
/// file would push the count past the cap.
const MAX_DIAGNOSTIC_FILES: usize = 32;

/// Encode one JSON-RPC message with an LSP `Content-Length` header.
fn encode_message(body: &Value) -> Vec<u8> {
    let json = serde_json::to_vec(body).expect("JSON Value serializes infallibly");
    let mut out = format!("Content-Length: {}\r\n\r\n", json.len()).into_bytes();
    out.extend_from_slice(&json);
    out
}

/// Decode the first frame in `buf`: returns the value plus total bytes
/// consumed, or `None` when the buffer holds no complete frame yet.
/// Test-only: the live path streams via [`read_frame`]; this pure decoder
/// backs the frame unit tests.
#[cfg(test)]
fn parse_frame(buf: &[u8]) -> Option<(Value, usize)> {
    let head_end = find_header_end(buf)?;
    let head = std::str::from_utf8(&buf[..head_end]).ok()?;
    let mut length: Option<usize> = None;
    for line in head.split("\r\n") {
        let lower = line.to_lowercase();
        if let Some(rest) = lower.strip_prefix("content-length:") {
            length = rest.trim().parse().ok();
        }
    }
    let length = length?;
    let total = head_end + 4 + length;
    if buf.len() < total {
        return None;
    }
    let value = serde_json::from_slice(&buf[head_end + 4..total]).ok()?;
    Some((value, total))
}

/// Locate the `\r\n\r\n` header terminator (test-only helper).
#[cfg(test)]
fn find_header_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

/// Write one framed message.
async fn write_frame<W>(writer: &mut W, msg: &Value) -> std::io::Result<()>
where
    W: AsyncWriteExt + Unpin,
{
    writer.write_all(&encode_message(msg)).await?;
    writer.flush().await
}

/// Byte cap on a single header line: a malformed or hostile server must
/// not be able to grow the read buffer without bound while the body cap
/// (below) never triggers. LSP headers are tiny in practice; 64 KiB is
/// orders of magnitude above any legitimate `Content-Length` line.
const MAX_HEADER_LINE_BYTES: usize = 64 * 1024;

/// Read one framed message (whole read bounded by `timeout`).
async fn read_frame<R>(reader: &mut R, timeout: Duration) -> std::io::Result<Value>
where
    R: AsyncBufReadExt + Unpin,
{
    tokio::time::timeout(timeout, async {
        let mut content_length: Option<usize> = None;
        loop {
            let mut line = String::new();
            // The `take` bounds one header line's allocation: a server that
            // keeps sending bytes without a newline stops at the cap instead
            // of growing the string until the timeout fires.
            let mut limited = (&mut *reader).take(MAX_HEADER_LINE_BYTES as u64);
            let n = limited.read_line(&mut line).await?;
            drop(limited);
            if n == 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "LSP stream closed",
                ));
            }
            if n as usize == MAX_HEADER_LINE_BYTES && !line.ends_with('\n') {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("LSP header line exceeds the {MAX_HEADER_LINE_BYTES}-byte cap"),
                ));
            }
            let trimmed = line.trim();
            if trimmed.is_empty() {
                break;
            }
            if trimmed.to_lowercase().starts_with("content-length:") {
                content_length = trimmed["content-length:".len()..].trim().parse().ok();
            }
        }
        let length = content_length.ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "LSP frame missing Content-Length",
            )
        })?;
        // The length comes from the peer; refuse absurd declarations instead
        // of pre-allocating (same threat as the llm SSE buffer cap).
        const MAX_FRAME_BYTES: usize = 32 * 1024 * 1024;
        if length > MAX_FRAME_BYTES {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("LSP frame Content-Length {length} exceeds the {MAX_FRAME_BYTES}-byte cap"),
            ));
        }
        let mut body = vec![0u8; length];
        reader.read_exact(&mut body).await?;
        serde_json::from_slice(&body)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
    })
    .await
    .map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "LSP read timed out"))?
}

/// Byte transport for JSON-RPC messages.
#[async_trait::async_trait]
pub trait LspTransport: Send {
    /// Send one message.
    async fn send(&mut self, msg: &Value) -> std::io::Result<()>;
    /// Receive one message, bounded by `timeout`.
    async fn recv(&mut self, timeout: Duration) -> std::io::Result<Value>;
}

/// Stdio transport over a spawned language-server child process.
pub struct ChildLsp {
    /// Kept alive for the session; `kill_on_drop` reaps it on timeout/drop.
    #[allow(dead_code)]
    child: tokio::process::Child,
    stdin: tokio::process::ChildStdin,
    stdout: tokio::io::BufReader<tokio::process::ChildStdout>,
}

impl ChildLsp {
    /// Spawn `server_command` with `cwd` as working directory. The command
    /// is split on ASCII whitespace (program plus argv, no shell quoting);
    /// stderr is discarded so a chatty server cannot block on a full pipe.
    /// `deny_env` plus the shared sensitive-shape fallback are stripped
    /// from the child's environment (same scrub as the shell tool): a
    /// `server_command` is model-facing input here, so its process must
    /// not inherit secrets.
    pub fn spawn(
        server_command: &str,
        cwd: &std::path::Path,
        deny_env: &[String],
    ) -> std::io::Result<Self> {
        let mut parts = server_command.split_whitespace();
        let program = parts.next().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "empty server_command")
        })?;
        let mut cmd = tokio::process::Command::new(program);
        cmd.args(parts)
            .current_dir(cwd)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true);
        crate::shell_tool::strip_child_env(cmd.as_std_mut(), deny_env);
        let mut child = cmd.spawn()?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| std::io::Error::other("server stdin unavailable"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| std::io::Error::other("server stdout unavailable"))?;
        Ok(Self {
            child,
            stdin,
            stdout: tokio::io::BufReader::new(stdout),
        })
    }
}

#[async_trait::async_trait]
impl LspTransport for ChildLsp {
    async fn send(&mut self, msg: &Value) -> std::io::Result<()> {
        write_frame(&mut self.stdin, msg).await
    }

    async fn recv(&mut self, timeout: Duration) -> std::io::Result<Value> {
        read_frame(&mut self.stdout, timeout).await
    }
}

/// Boxed LSP transport: lets the provider registry hold one client type while
/// production spawns [`ChildLsp`] and tests inject in-memory fakes.
pub struct AnyTransport(Box<dyn LspTransport>);

impl AnyTransport {
    /// Spawn a real language server over stdio (environment scrubbed via
    /// `deny_env`, see [`ChildLsp::spawn`]).
    pub fn spawn(server_command: &str, cwd: &Path, deny_env: &[String]) -> std::io::Result<Self> {
        Ok(Self(Box::new(ChildLsp::spawn(
            server_command,
            cwd,
            deny_env,
        )?)))
    }
}

#[async_trait::async_trait]
impl LspTransport for AnyTransport {
    async fn send(&mut self, msg: &Value) -> std::io::Result<()> {
        self.0.send(msg).await
    }

    async fn recv(&mut self, timeout: Duration) -> std::io::Result<Value> {
        self.0.recv(timeout).await
    }
}

/// Normalize a file extension key: strip a leading dot, lowercase (`RS`
/// and `.rs` both become `rs`).
fn normalize_ext(ext: &str) -> String {
    ext.strip_prefix('.').unwrap_or(ext).to_lowercase()
}

/// Extension of a model-provided path (lexical; empty when none).
fn extension_of(path: &str) -> String {
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
struct RequestTarget<'a> {
    uri: &'a str,
    line: u32,
    character: u32,
}

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
    diagnostics: Arc<std::sync::Mutex<DiagnosticsStore>>,
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
    async fn pooled_call(
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

/// In-memory transport over a `tokio::io::duplex` stream (test-only:
/// lets tests drive [`LspClient`] against a fake in-process server).
#[cfg(test)]
pub struct DuplexLsp {
    stream: tokio::io::BufReader<tokio::io::DuplexStream>,
}

#[cfg(test)]
impl DuplexLsp {
    /// Wrap one end of a duplex pair; the other end feeds the fake server.
    pub fn new(stream: tokio::io::DuplexStream) -> Self {
        Self {
            stream: tokio::io::BufReader::new(stream),
        }
    }
}

#[cfg(test)]
#[async_trait::async_trait]
impl LspTransport for DuplexLsp {
    async fn send(&mut self, msg: &Value) -> std::io::Result<()> {
        write_frame(&mut self.stream, msg).await
    }

    async fn recv(&mut self, timeout: Duration) -> std::io::Result<Value> {
        read_frame(&mut self.stream, timeout).await
    }
}

/// One stored diagnostic: the flattened fields the text renderer needs
/// (positions kept 0-based exactly as LSP addresses them).
struct DiagnosticEntry {
    line: u32,
    character: u32,
    severity: u8,
    message: String,
}

impl DiagnosticEntry {
    /// Flatten one LSP `Diagnostic` object; entries without a usable range
    /// start or message are skipped (defensive: the schema is advisory).
    fn parse(d: &Value) -> Option<Self> {
        let start = d.get("range")?.get("start")?;
        Some(Self {
            line: start.get("line")?.as_u64()? as u32,
            character: start.get("character").and_then(Value::as_u64).unwrap_or(0) as u32,
            severity: d
                .get("severity")
                .and_then(Value::as_u64)
                .unwrap_or(0)
                .min(u8::MAX as u64) as u8,
            message: d.get("message")?.as_str()?.to_owned(),
        })
    }
}

/// Bounded store of server-pushed diagnostics, keyed by the document URI
/// exactly as the server addressed it. Each `textDocument/publishDiagnostics`
/// notification fully replaces its file's list (LSP push semantics); the
/// per-file list is capped at [`MAX_DIAGNOSTICS_PER_FILE`] and the number of
/// tracked files at [`MAX_DIAGNOSTIC_FILES`] (oldest-inserted file evicted
/// FIFO).
#[derive(Default)]
pub struct DiagnosticsStore {
    files: HashMap<String, Vec<DiagnosticEntry>>,
    /// Insertion order of tracked URIs (drives the FIFO eviction at the file
    /// cap; never holds duplicates — a URI is pushed while untracked exactly
    /// once, and eviction removes it from both sides).
    order: Vec<String>,
}

impl DiagnosticsStore {
    /// Record one push: `diagnostics` is the notification's array and
    /// replaces any previous list for `uri`. Malformed entries are dropped;
    /// the per-file cap keeps the first N entries.
    pub fn record(&mut self, uri: &str, diagnostics: &[Value]) {
        let entries: Vec<DiagnosticEntry> = diagnostics
            .iter()
            .take(MAX_DIAGNOSTICS_PER_FILE)
            .filter_map(DiagnosticEntry::parse)
            .collect();
        if !self.files.contains_key(uri) {
            while self.order.len() >= MAX_DIAGNOSTIC_FILES {
                let Some(oldest) = self.order.first() else {
                    break;
                };
                let oldest = oldest.clone();
                self.files.remove(&oldest);
                self.order.remove(0);
            }
            self.order.push(uri.to_owned());
        }
        self.files.insert(uri.to_owned(), entries);
    }

    /// Render stored diagnostics as compact text: one `line:col: severity:
    /// message` entry per diagnostic (1-based, matching common compiler
    /// output), grouped under each file URI. `uri_filter` restricts the
    /// output to one file; `None` renders every tracked file. Files whose
    /// latest push cleared their list are skipped.
    pub fn render(&self, uri_filter: Option<&str>) -> String {
        let mut out = String::new();
        match uri_filter {
            Some(uri) => {
                if let Some(entries) = self.files.get(uri) {
                    render_file(&mut out, uri, entries);
                }
            }
            None => {
                let mut uris: Vec<&String> = self.files.keys().collect();
                uris.sort(); // deterministic order
                for uri in uris {
                    render_file(&mut out, uri, &self.files[uri]);
                }
            }
        }
        if out.is_empty() {
            match uri_filter {
                Some(uri) => format!("no diagnostics recorded for {uri}"),
                None => "no diagnostics recorded".to_owned(),
            }
        } else {
            out.trim_end().to_owned()
        }
    }
}

/// Append one file's block: the URI, then one indented line per diagnostic
/// (messages are kept on one physical line by escaping embedded newlines).
fn render_file(out: &mut String, uri: &str, entries: &[DiagnosticEntry]) {
    if entries.is_empty() {
        return;
    }
    if !out.is_empty() {
        out.push('\n');
    }
    out.push_str(uri);
    out.push('\n');
    for e in entries {
        out.push_str(&format!(
            "  {}:{}: {}: {}\n",
            e.line + 1,
            e.character + 1,
            severity_label(e.severity),
            e.message.replace('\n', "\\n")
        ));
    }
}

/// LSP severity number to label (1 error / 2 warning / 3 information /
/// 4 hint; anything else renders as the generic "diagnostic").
fn severity_label(severity: u8) -> &'static str {
    match severity {
        1 => "error",
        2 => "warning",
        3 => "info",
        4 => "hint",
        _ => "diagnostic",
    }
}

/// Minimal LSP client: `initialize` (plus `initialized`), one request at a
/// time, then `shutdown`/`exit`. Request ids count up from 1 per client.
/// Server-pushed `textDocument/publishDiagnostics` notifications observed
/// while waiting for a response are recorded into an optional shared sink
/// (see [`LspClient::with_diagnostics_sink`]); all other notifications are
/// skipped as before.
pub struct LspClient<T> {
    transport: T,
    next_id: u64,
    /// Shared diagnostics sink (`None`: pushes are dropped, the historical
    /// behavior — most callers never need them).
    diagnostics: Option<Arc<std::sync::Mutex<DiagnosticsStore>>>,
}

impl<T: LspTransport> LspClient<T> {
    /// Wrap an already-connected transport.
    pub fn new(transport: T) -> Self {
        Self {
            transport,
            next_id: 1,
            diagnostics: None,
        }
    }

    /// Attach a shared diagnostics sink: registry-backed clients share the
    /// registry's store so pushes survive across pooled calls (per-call
    /// override sessions feed it too when a registry is configured).
    pub fn with_diagnostics_sink(mut self, sink: Arc<std::sync::Mutex<DiagnosticsStore>>) -> Self {
        self.diagnostics = Some(sink);
        self
    }

    /// Send a request and wait for the response with the matching id,
    /// skipping unrelated notifications. A JSON-RPC `error` response
    /// becomes an `InvalidInput` error.
    pub async fn request(
        &mut self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        self.transport
            .send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
            .await?;
        loop {
            let msg = self.transport.recv(timeout).await?;
            // Server push: record diagnostics (when a sink is attached) and
            // keep waiting for the response; other notifications are skipped
            // as before.
            if msg.get("method").and_then(Value::as_str) == Some("textDocument/publishDiagnostics")
            {
                if let (Some(sink), Some(params)) = (&self.diagnostics, msg.get("params")) {
                    let uri = params.get("uri").and_then(Value::as_str);
                    if let (Some(uri), Some(diags)) = (
                        uri.filter(|u| !u.is_empty()),
                        params.get("diagnostics").and_then(Value::as_array),
                    ) {
                        lock(sink).record(uri, diags);
                    }
                }
                continue;
            }
            if msg.get("id").and_then(Value::as_u64) != Some(id) {
                continue;
            }
            if let Some(err) = msg.get("error") {
                return Err(ToolsError::InvalidInput {
                    message: format!("LSP {method} failed: {err}"),
                });
            }
            return Ok(msg.get("result").cloned().unwrap_or(Value::Null));
        }
    }

    /// Send a notification (no response expected).
    pub async fn notify(&mut self, method: &str, params: Value) -> Result<()> {
        self.transport
            .send(&json!({"jsonrpc": "2.0", "method": method, "params": params}))
            .await?;
        Ok(())
    }

    /// Handshake: `initialize` with `rootUri`, then `initialized`.
    /// Returns the server's `result` (its capabilities).
    pub async fn initialize(&mut self, root_uri: &str, timeout: Duration) -> Result<Value> {
        let result = self
            .request(
                "initialize",
                json!({
                    "processId": std::process::id(),
                    "rootUri": root_uri,
                    "capabilities": {}
                }),
                timeout,
            )
            .await?;
        self.notify("initialized", json!({})).await?;
        Ok(result)
    }

    /// Polite teardown: `shutdown` request, then `exit` notification.
    /// Best-effort by convention; callers ignore the result.
    pub async fn shutdown(&mut self, timeout: Duration) -> Result<()> {
        let _ = self.request("shutdown", Value::Null, timeout).await?;
        let _ = self.notify("exit", json!({})).await;
        Ok(())
    }

    /// `textDocument/documentSymbol` for `uri`.
    pub async fn document_symbol(&mut self, uri: &str, timeout: Duration) -> Result<Value> {
        self.request(
            "textDocument/documentSymbol",
            json!({"textDocument": {"uri": uri}}),
            timeout,
        )
        .await
    }

    /// `textDocument/definition` at a position.
    pub async fn definition(
        &mut self,
        uri: &str,
        line: u32,
        character: u32,
        timeout: Duration,
    ) -> Result<Value> {
        self.request(
            "textDocument/definition",
            json!({
                "textDocument": {"uri": uri},
                "position": {"line": line, "character": character}
            }),
            timeout,
        )
        .await
    }

    /// `textDocument/hover` at a position.
    pub async fn hover(
        &mut self,
        uri: &str,
        line: u32,
        character: u32,
        timeout: Duration,
    ) -> Result<Value> {
        self.request(
            "textDocument/hover",
            json!({
                "textDocument": {"uri": uri},
                "position": {"line": line, "character": character}
            }),
            timeout,
        )
        .await
    }

    /// `textDocument/references` at a position (including the declaration).
    pub async fn references(
        &mut self,
        uri: &str,
        line: u32,
        character: u32,
        timeout: Duration,
    ) -> Result<Value> {
        self.request(
            "textDocument/references",
            json!({
                "textDocument": {"uri": uri},
                "position": {"line": line, "character": character},
                "context": {"includeDeclaration": true}
            }),
            timeout,
        )
        .await
    }
}

/// Percent-encode a URI path per RFC 3986: unreserved bytes (`A-Z a-z 0-9
/// - . _ ~`) pass through, `/` separates segments, and `:` is kept so
/// Windows drive letters stay the conventional `file:///C:/...` shape;
/// every other byte encodes as `%XX` over its UTF-8 bytes. Without this,
/// a path holding a space, `#`, `%`, or non-ASCII text addresses a
/// different resource on the server side (a raw `#` cuts the path off at
/// fragment parsing, a raw `%` poisons the percent-decode).
fn encode_uri_path(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' | b':' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// Convert a filesystem path to a `file://` URI: backslashes become
/// forward slashes and the path is percent-encoded (see
/// [`encode_uri_path`]). Producers and consumers here both go through
/// this function, so pooled requests and diagnostics filters address the
/// exact same URI strings the server observes.
fn path_to_uri(path: &std::path::Path) -> String {
    let text = path.to_string_lossy().replace('\\', "/");
    if text.starts_with('/') {
        format!("file://{}", encode_uri_path(&text))
    } else {
        format!("file:///{}", encode_uri_path(&text))
    }
}

/// Parse `timeout_ms` (default 30 s, clamped to 300 s). Zero is rejected:
/// a zero-length bound aborts the request before the server can answer.
fn resolve_timeout(input: &Value) -> std::result::Result<Duration, ToolOutput> {
    let ms = match input.get("timeout_ms") {
        None | Some(Value::Null) => DEFAULT_TIMEOUT_MS,
        Some(v) => match v.as_u64() {
            Some(0) => {
                return Err(err_output(
                    "invalid parameter 'timeout_ms' (positive integer required; omit it for the default)",
                ));
            }
            Some(n) => n.min(MAX_TIMEOUT_MS),
            None => {
                return Err(err_output(
                    "invalid parameter 'timeout_ms' (non-negative integer required)",
                ));
            }
        },
    };
    Ok(Duration::from_millis(ms))
}

/// Extract a required 0-based line/character offset (clamped to u32).
fn req_position(input: &Value, key: &str) -> std::result::Result<u32, ToolOutput> {
    match input.get(key).and_then(Value::as_u64) {
        Some(n) => Ok(n.min(u32::MAX as u64) as u32),
        None => Err(err_output(format!(
            "missing or invalid parameter '{key}' (non-negative integer required)"
        ))),
    }
}

/// Resolve `path` under `ctx.cwd` and convert to a `file://` URI.
/// Escape/invalid input becomes a business error; IO faults propagate.
fn resolve_uri(ctx: &ToolCtx, path: &str) -> Result<std::result::Result<String, ToolOutput>> {
    match crate::path_guard::resolve(ctx, path) {
        Ok(p) => Ok(Ok(path_to_uri(&p))),
        Err(e @ (ToolsError::InvalidInput { .. } | ToolsError::PathEscape { .. })) => {
            Ok(Err(err_output(e.to_string())))
        }
        Err(e) => Err(e),
    }
}

/// Parse the optional `server_command` override: missing/null means "resolve
/// from the registry"; present-but-mistyped is a business error.
fn opt_server_command(input: &Value) -> std::result::Result<Option<String>, ToolOutput> {
    match input.get("server_command") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        Some(_) => Err(err_output(
            "missing or invalid parameter 'server_command' (string required)",
        )),
    }
}

/// Shared flow: parse params, resolve the server (explicit `server_command`
/// override spawns per call; otherwise the registry's pooled server for the
/// file extension is used), run one request closure, pretty-print the result.
async fn run_lsp_call(
    input: &Value,
    ctx: &ToolCtx,
    providers: Option<&Arc<LspProviders>>,
    with_position: bool,
    method: &str,
    call: impl AsyncFnOnce(&mut LspClient<AnyTransport>, &str, u32, u32, Duration) -> Result<Value>,
) -> Result<ToolOutput> {
    let override_command = match opt_server_command(input) {
        Ok(c) => c,
        Err(out) => return Ok(out),
    };
    let path = match req_str(input, "path") {
        Ok(s) => s,
        Err(out) => return Ok(out),
    };
    let (line, character) = if with_position {
        let line = match req_position(input, "line") {
            Ok(n) => n,
            Err(out) => return Ok(out),
        };
        let character = match req_position(input, "character") {
            Ok(n) => n,
            Err(out) => return Ok(out),
        };
        (line, character)
    } else {
        (0, 0)
    };
    let timeout = match resolve_timeout(input) {
        Ok(t) => t,
        Err(out) => return Ok(out),
    };
    let uri = match resolve_uri(ctx, path)? {
        Ok(u) => u,
        Err(out) => return Ok(out),
    };
    if let Some(server_command) = override_command {
        // Explicit override (also the fallback when the extension is
        // unregistered): per-call spawn, handshake, one request, teardown.
        let transport = match AnyTransport::spawn(&server_command, &ctx.cwd, &ctx.deny_env) {
            Ok(transport) => transport,
            Err(e) => {
                return Ok(err_output(format!(
                    "failed to spawn language server '{server_command}': {e}"
                )));
            }
        };
        let mut client = LspClient::new(transport);
        // Registry-aware override sessions also feed the shared diagnostics
        // store, so pushes observed here survive the session teardown.
        if let Some(providers) = providers {
            client = client.with_diagnostics_sink(Arc::clone(&providers.diagnostics));
        }
        if let Err(e) = client.initialize(&path_to_uri(&ctx.cwd), timeout).await {
            return Ok(err_output(format!("LSP initialize failed: {e}")));
        }
        let result = match call(&mut client, &uri, line, character, timeout).await {
            Ok(v) => v,
            Err(e) => return Ok(err_output(format!("LSP {method} failed: {e}"))),
        };
        // Best-effort teardown: the child is reaped by kill_on_drop regardless.
        let _ = client.shutdown(timeout).await;
        return Ok(ok_output(
            serde_json::to_string_pretty(&result).unwrap_or_else(|_| result.to_string()),
        ));
    }
    let Some(providers) = providers else {
        return Ok(err_output(
            "missing parameter 'server_command' (no language server registry configured; provide server_command explicitly)",
        ));
    };
    let ext = extension_of(path);
    let result = match providers
        .pooled_call(
            &ext,
            RequestTarget {
                uri: &uri,
                line,
                character,
            },
            timeout,
            method,
            call,
        )
        .await
    {
        Ok(v) => v,
        Err(out) => return Ok(out),
    };
    Ok(ok_output(
        serde_json::to_string_pretty(&result).unwrap_or_else(|_| result.to_string()),
    ))
}

/// List document symbols (outline) for a file (read-only).
pub struct DocumentSymbols {
    providers: Option<Arc<LspProviders>>,
}

impl DocumentSymbols {
    /// Build without a registry (explicit `server_command` required per call).
    pub fn new() -> Self {
        Self { providers: None }
    }

    /// Build resolving default servers from `providers` (`server_command`
    /// becomes a per-call override).
    pub fn with_providers(providers: Arc<LspProviders>) -> Self {
        Self {
            providers: Some(providers),
        }
    }
}

impl Default for DocumentSymbols {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl Tool for DocumentSymbols {
    fn name(&self) -> &str {
        "lsp_symbols"
    }

    fn description(&self) -> &str {
        "List the symbol outline (functions, classes, etc.) of a file via a language \
         server. Omit server_command to use the registered provider for the file \
         extension, or provide it as an override (e.g. rust-analyzer, \
         pyright-langserver --stdio); no server is bundled. Path is relative to \
         the working directory."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "server_command": {
                    "type": "string",
                    "description": "Language server command to spawn (program plus whitespace-separated args)"
                },
                "path": {
                    "type": "string",
                    "description": "Path of the file, relative to the working directory"
                },
                "timeout_ms": {
                    "type": "integer",
                    "description": "Timeout per LSP round trip in milliseconds (default 30000, clamped to max 300000)"
                }
            },
            "required": ["path"]
        })
    }

    fn is_read_only(&self) -> bool {
        true
    }

    async fn execute(&self, input: Value, ctx: &ToolCtx) -> Result<ToolOutput> {
        run_lsp_call(
            &input,
            ctx,
            self.providers.as_ref(),
            false,
            "documentSymbol",
            async |c, uri, _, _, t| c.document_symbol(uri, t).await,
        )
        .await
    }
}

/// Jump to the definition of the symbol at a position (read-only).
pub struct GotoDefinition {
    providers: Option<Arc<LspProviders>>,
}

impl GotoDefinition {
    /// Build without a registry (explicit `server_command` required per call).
    pub fn new() -> Self {
        Self { providers: None }
    }

    /// Build resolving default servers from `providers` (`server_command`
    /// becomes a per-call override).
    pub fn with_providers(providers: Arc<LspProviders>) -> Self {
        Self {
            providers: Some(providers),
        }
    }
}

impl Default for GotoDefinition {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl Tool for GotoDefinition {
    fn name(&self) -> &str {
        "lsp_definition"
    }

    fn description(&self) -> &str {
        "Jump to the definition of the symbol at a 0-based line/character position \
         via a language server. Omit server_command to use the registered provider \
         for the file extension, or provide it as an override (e.g. rust-analyzer, \
         pyright-langserver --stdio); no server is bundled. Path is relative to \
         the working directory."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "server_command": {
                    "type": "string",
                    "description": "Language server command to spawn (program plus whitespace-separated args)"
                },
                "path": {
                    "type": "string",
                    "description": "Path of the file, relative to the working directory"
                },
                "line": {
                    "type": "integer",
                    "description": "0-based line number"
                },
                "character": {
                    "type": "integer",
                    "description": "0-based character offset on the line"
                },
                "timeout_ms": {
                    "type": "integer",
                    "description": "Timeout per LSP round trip in milliseconds (default 30000, clamped to max 300000)"
                }
            },
            "required": ["path", "line", "character"]
        })
    }

    fn is_read_only(&self) -> bool {
        true
    }

    async fn execute(&self, input: Value, ctx: &ToolCtx) -> Result<ToolOutput> {
        run_lsp_call(
            &input,
            ctx,
            self.providers.as_ref(),
            true,
            "definition",
            async |c, uri, line, ch, t| c.definition(uri, line, ch, t).await,
        )
        .await
    }
}

/// Hover documentation for the symbol at a position (read-only).
pub struct Hover {
    providers: Option<Arc<LspProviders>>,
}

impl Hover {
    /// Build without a registry (explicit `server_command` required per call).
    pub fn new() -> Self {
        Self { providers: None }
    }

    /// Build resolving default servers from `providers` (`server_command`
    /// becomes a per-call override).
    pub fn with_providers(providers: Arc<LspProviders>) -> Self {
        Self {
            providers: Some(providers),
        }
    }
}

impl Default for Hover {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl Tool for Hover {
    fn name(&self) -> &str {
        "lsp_hover"
    }

    fn description(&self) -> &str {
        "Show hover documentation for the symbol at a 0-based line/character position \
         via a language server. Omit server_command to use the registered provider \
         for the file extension, or provide it as an override (e.g. rust-analyzer, \
         pyright-langserver --stdio); no server is bundled. Path is relative to \
         the working directory."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "server_command": {
                    "type": "string",
                    "description": "Language server command to spawn (program plus whitespace-separated args)"
                },
                "path": {
                    "type": "string",
                    "description": "Path of the file, relative to the working directory"
                },
                "line": {
                    "type": "integer",
                    "description": "0-based line number"
                },
                "character": {
                    "type": "integer",
                    "description": "0-based character offset on the line"
                },
                "timeout_ms": {
                    "type": "integer",
                    "description": "Timeout per LSP round trip in milliseconds (default 30000, clamped to max 300000)"
                }
            },
            "required": ["path", "line", "character"]
        })
    }

    fn is_read_only(&self) -> bool {
        true
    }

    async fn execute(&self, input: Value, ctx: &ToolCtx) -> Result<ToolOutput> {
        run_lsp_call(
            &input,
            ctx,
            self.providers.as_ref(),
            true,
            "hover",
            async |c, uri, line, ch, t| c.hover(uri, line, ch, t).await,
        )
        .await
    }
}

/// Find all references to the symbol at a position (read-only).
pub struct FindReferences {
    providers: Option<Arc<LspProviders>>,
}

impl FindReferences {
    /// Build without a registry (explicit `server_command` required per call).
    pub fn new() -> Self {
        Self { providers: None }
    }

    /// Build resolving default servers from `providers` (`server_command`
    /// becomes a per-call override).
    pub fn with_providers(providers: Arc<LspProviders>) -> Self {
        Self {
            providers: Some(providers),
        }
    }
}

impl Default for FindReferences {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl Tool for FindReferences {
    fn name(&self) -> &str {
        "lsp_references"
    }

    fn description(&self) -> &str {
        "Find all references (including the declaration) to the symbol at a 0-based \
         line/character position via a language server. Omit server_command to use \
         the registered provider for the file extension, or provide it as an \
         override (e.g. rust-analyzer, pyright-langserver --stdio); no server is \
         bundled. Path is relative to the working directory."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "server_command": {
                    "type": "string",
                    "description": "Language server command to spawn (program plus whitespace-separated args)"
                },
                "path": {
                    "type": "string",
                    "description": "Path of the file, relative to the working directory"
                },
                "line": {
                    "type": "integer",
                    "description": "0-based line number"
                },
                "character": {
                    "type": "integer",
                    "description": "0-based character offset on the line"
                },
                "timeout_ms": {
                    "type": "integer",
                    "description": "Timeout per LSP round trip in milliseconds (default 30000, clamped to max 300000)"
                }
            },
            "required": ["path", "line", "character"]
        })
    }

    fn is_read_only(&self) -> bool {
        true
    }

    async fn execute(&self, input: Value, ctx: &ToolCtx) -> Result<ToolOutput> {
        run_lsp_call(
            &input,
            ctx,
            self.providers.as_ref(),
            true,
            "references",
            async |c, uri, line, ch, t| c.references(uri, line, ch, t).await,
        )
        .await
    }
}

/// Read back the diagnostics captured from server pushes (read-only).
///
/// Language servers push `textDocument/publishDiagnostics` while a tool
/// call's requests are in flight; registry-backed clients record those pushes
/// into the registry's shared bounded store (see [`LspProviders`]). This tool
/// renders the store — it never spawns a server or issues a request, so a
/// path filter only needs to resolve (same path guard as the other LSP
/// tools) to match the URIs earlier calls addressed.
pub struct LspDiagnostics {
    providers: Option<Arc<LspProviders>>,
}

impl LspDiagnostics {
    /// Build without a registry: there is no store to read, so every call is
    /// a business error (diagnostics only exist with registry-backed
    /// clients).
    pub fn new() -> Self {
        Self { providers: None }
    }

    /// Build reading the shared store of `providers`.
    pub fn with_providers(providers: Arc<LspProviders>) -> Self {
        Self {
            providers: Some(providers),
        }
    }
}

impl Default for LspDiagnostics {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl Tool for LspDiagnostics {
    fn name(&self) -> &str {
        "lsp_diagnostics"
    }

    fn description(&self) -> &str {
        "Return the diagnostics (errors, warnings, hints) that language servers \
         pushed for files during earlier language-server tool calls. Provide path \
         to see one file's diagnostics, or omit it to see every tracked file. \
         Entries render as line:col (1-based) with severity and message. \
         Read-only: reads the recorded diagnostics, never spawns a server."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Path of the file, relative to the working directory; omit to return all tracked files"
                }
            }
        })
    }

    fn is_read_only(&self) -> bool {
        true
    }

    async fn execute(&self, input: Value, ctx: &ToolCtx) -> Result<ToolOutput> {
        let Some(providers) = &self.providers else {
            return Ok(err_output(
                "no language server registry configured: lsp_diagnostics reads diagnostics recorded by registry-backed language-server tool calls",
            ));
        };
        // Optional path: absent/null renders every tracked file; a present
        // path resolves through the same guard as the other LSP tools so the
        // URI matches what earlier pooled calls addressed.
        let filter = match input.get("path") {
            None | Some(Value::Null) => None,
            Some(Value::String(path)) => match resolve_uri(ctx, path)? {
                Ok(uri) => Some(uri),
                Err(out) => return Ok(out),
            },
            Some(_) => {
                return Ok(err_output(
                    "missing or invalid parameter 'path' (string required)",
                ));
            }
        };
        Ok(ok_output(providers.diagnostics_text(filter.as_deref())))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `timeout_ms` resolution: default, ceiling clamp, and a rejected
    /// zero (it would abort the request before the server can answer).
    #[test]
    fn zero_timeout_is_rejected() {
        assert_eq!(
            resolve_timeout(&serde_json::json!({})).unwrap(),
            Duration::from_millis(DEFAULT_TIMEOUT_MS)
        );
        assert_eq!(
            resolve_timeout(&serde_json::json!({"timeout_ms": 999_999_999})).unwrap(),
            Duration::from_millis(MAX_TIMEOUT_MS)
        );
        assert!(resolve_timeout(&serde_json::json!({"timeout_ms": 0})).is_err());
        assert!(resolve_timeout(&serde_json::json!({"timeout_ms": -1})).is_err());
    }

    /// Fake language server: answers initialize/symbol requests, errors hover,
    /// quits on `exit`. Returns the client-side stream end.
    fn fake_server() -> tokio::io::DuplexStream {
        let (client_end, server_end) = tokio::io::duplex(64 * 1024);
        tokio::spawn(async move {
            let mut io = tokio::io::BufReader::new(server_end);
            let timeout = Duration::from_secs(10);
            loop {
                let msg = match read_frame(&mut io, timeout).await {
                    Ok(m) => m,
                    Err(_) => break,
                };
                let method = msg
                    .get("method")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned();
                if method == "exit" {
                    break;
                }
                let Some(id) = msg.get("id").cloned() else {
                    continue; // Notification (e.g. initialized): no reply.
                };
                let reply = match method.as_str() {
                    "initialize" => {
                        json!({"jsonrpc": "2.0", "id": id, "result": {"capabilities": {}}})
                    }
                    "textDocument/documentSymbol" => {
                        json!({"jsonrpc": "2.0", "id": id, "result": [{"name": "main", "kind": 12}]})
                    }
                    "shutdown" => json!({"jsonrpc": "2.0", "id": id, "result": null}),
                    _ => {
                        json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": "unknown method"}})
                    }
                };
                if write_frame(&mut io, &reply).await.is_err() {
                    break;
                }
            }
        });
        client_end
    }

    #[test]
    fn frame_encode_decode_roundtrip() {
        let msg = json!({"jsonrpc": "2.0", "id": 7, "method": "initialize", "params": {}});
        let bytes = encode_message(&msg);
        assert!(bytes.starts_with(b"Content-Length: "));
        let (back, consumed) = parse_frame(&bytes).expect("complete frame decodes");
        assert_eq!(back, msg);
        assert_eq!(consumed, bytes.len());
    }

    #[test]
    fn frame_decode_waits_for_complete_body() {
        let msg = json!({"jsonrpc": "2.0", "id": 1, "result": [1, 2, 3]});
        let bytes = encode_message(&msg);
        // Truncated body: no frame yet.
        assert!(parse_frame(&bytes[..bytes.len() - 2]).is_none());
        // Two back-to-back frames: first decodes, cursor points at the second.
        let mut two = bytes.clone();
        two.extend_from_slice(&bytes);
        let (first, consumed) = parse_frame(&two).expect("first frame decodes");
        assert_eq!(first, msg);
        let (second, _) = parse_frame(&two[consumed..]).expect("second frame decodes");
        assert_eq!(second, msg);
    }

    #[test]
    fn frame_decode_rejects_garbage() {
        assert!(parse_frame(b"not a frame").is_none());
        assert!(parse_frame(b"Content-Length: 5\r\n\r\n{bad").is_none());
    }

    /// A header line past the cap is a protocol error, not an unbounded
    /// buffer: a hostile or broken server cannot grow memory until the
    /// timeout by withholding the newline.
    #[tokio::test]
    async fn oversized_header_line_is_refused() {
        let (client_end, mut server_end) = tokio::io::duplex(64 * 1024);
        tokio::spawn(async move {
            // More than the cap in one line, no newline, then EOF.
            let line = vec![b'A'; MAX_HEADER_LINE_BYTES + 1];
            let _ = server_end.write_all(&line).await;
        });
        let mut reader = tokio::io::BufReader::new(client_end);
        let err = read_frame(&mut reader, Duration::from_secs(10))
            .await
            .expect_err("oversized header must fail");
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("header line"), "{err}");
    }

    #[tokio::test]
    async fn client_conversation_against_fake_server() {
        let mut client = LspClient::new(DuplexLsp::new(fake_server()));
        let timeout = Duration::from_secs(10);
        let init = client.initialize("file:///work", timeout).await.unwrap();
        assert_eq!(init, json!({"capabilities": {}}));
        let symbols = client
            .document_symbol("file:///work/a.py", timeout)
            .await
            .unwrap();
        assert_eq!(symbols, json!([{"name": "main", "kind": 12}]));
        client.shutdown(timeout).await.unwrap();
    }

    #[tokio::test]
    async fn client_surfaces_jsonrpc_error() {
        let mut client = LspClient::new(DuplexLsp::new(fake_server()));
        let timeout = Duration::from_secs(10);
        client.initialize("file:///work", timeout).await.unwrap();
        let err = client
            .hover("file:///work/a.py", 0, 0, timeout)
            .await
            .expect_err("unknown method must fail");
        assert!(err.to_string().contains("hover"));
        let _ = client.shutdown(timeout).await;
    }

    // -- diagnostics capture --

    /// Fake language server that pushes a `textDocument/publishDiagnostics`
    /// notification *before* answering `initialize` and every
    /// `documentSymbol` (the push-then-reply ordering is what forces the
    /// client to observe pushes while waiting for a response). The
    /// documentSymbol push addresses the requested document URI so tests can
    /// filter on it.
    fn diagnostics_server() -> tokio::io::DuplexStream {
        let (client_end, server_end) = tokio::io::duplex(64 * 1024);
        tokio::spawn(async move {
            let mut io = tokio::io::BufReader::new(server_end);
            let timeout = Duration::from_secs(10);
            loop {
                let msg = match read_frame(&mut io, timeout).await {
                    Ok(m) => m,
                    Err(_) => break,
                };
                let method = msg
                    .get("method")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned();
                if method == "exit" {
                    break;
                }
                let Some(id) = msg.get("id").cloned() else {
                    continue; // Notification (e.g. initialized): no reply.
                };
                let reply = match method.as_str() {
                    "initialize" => {
                        let _ = write_frame(
                            &mut io,
                            &json!({
                                "jsonrpc": "2.0",
                                "method": "textDocument/publishDiagnostics",
                                "params": {"uri": "file:///w/a.rs", "diagnostics": [
                                    {"range": {"start": {"line": 11, "character": 4}},
                                     "severity": 1, "message": "first push"}
                                ]}
                            }),
                        )
                        .await;
                        json!({"jsonrpc": "2.0", "id": id, "result": {"capabilities": {}}})
                    }
                    "textDocument/documentSymbol" => {
                        let uri = msg
                            .pointer("/params/textDocument/uri")
                            .and_then(Value::as_str)
                            .unwrap_or("file:///unknown")
                            .to_owned();
                        let _ = write_frame(
                            &mut io,
                            &json!({
                                "jsonrpc": "2.0",
                                "method": "textDocument/publishDiagnostics",
                                "params": {"uri": uri, "diagnostics": [
                                    {"range": {"start": {"line": 2, "character": 0}},
                                     "severity": 2, "message": "unused import"}
                                ]}
                            }),
                        )
                        .await;
                        json!({"jsonrpc": "2.0", "id": id, "result": []})
                    }
                    "shutdown" => json!({"jsonrpc": "2.0", "id": id, "result": null}),
                    _ => {
                        json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": "unknown method"}})
                    }
                };
                if write_frame(&mut io, &reply).await.is_err() {
                    break;
                }
            }
        });
        client_end
    }

    /// Store semantics: each push replaces its file's list, malformed entries
    /// are dropped, and both caps hold (per-file entries, tracked files with
    /// oldest-inserted FIFO eviction).
    #[test]
    fn diagnostics_store_replaces_and_caps() {
        let mut store = DiagnosticsStore::default();
        let diag = |line: u64, message: &str| json!({"range": {"start": {"line": line, "character": 0}}, "severity": 1, "message": message});
        store.record("file:///a.rs", &[diag(0, "one")]);
        store.record("file:///a.rs", &[diag(1, "two"), json!({"no": "range"})]);
        // Second push fully replaces the first; malformed entries are skipped.
        let text = store.render(Some("file:///a.rs"));
        assert!(text.contains("2:1: error: two"), "{text}");
        assert!(!text.contains("one"), "{text}");

        // Per-file cap: entries beyond the cap are dropped on each push.
        let flood: Vec<Value> = (0..MAX_DIAGNOSTICS_PER_FILE as u64 + 50)
            .map(|i| diag(i, "flood"))
            .collect();
        store.record("file:///b.rs", &flood);
        let text = store.render(Some("file:///b.rs"));
        assert_eq!(text.lines().count(), 1 + MAX_DIAGNOSTICS_PER_FILE);

        // File cap: a brand-new file at capacity evicts the oldest-inserted
        // one, so the tracked set stays at the cap and stays fresh.
        for i in 0..MAX_DIAGNOSTIC_FILES {
            store.record(&format!("file:///f{i}.rs"), &[diag(0, "x")]);
        }
        store.record("file:///new.rs", &[diag(0, "fresh")]);
        assert_eq!(store.files.len(), MAX_DIAGNOSTIC_FILES);
        assert_eq!(store.order.len(), MAX_DIAGNOSTIC_FILES);
        let text = store.render(None);
        assert!(text.contains("file:///new.rs"), "{text}");
        assert!(
            text.contains(&format!("file:///f{}.rs", MAX_DIAGNOSTIC_FILES - 1)),
            "{text}"
        );
        assert!(!text.contains("file:///f0.rs"), "oldest evicted: {text}");
    }

    /// Client records pushes into the attached sink while waiting for
    /// responses; a later push for the same URI replaces the stored list.
    #[tokio::test]
    async fn client_records_publish_diagnostics_pushes() {
        let store = Arc::new(std::sync::Mutex::new(DiagnosticsStore::default()));
        let mut client = LspClient::new(DuplexLsp::new(diagnostics_server()))
            .with_diagnostics_sink(store.clone());
        let timeout = Duration::from_secs(10);
        client.initialize("file:///w", timeout).await.unwrap();
        let text = lock(&store).render(Some("file:///w/a.rs"));
        assert!(text.contains("12:5: error: first push"), "{text}");
        client
            .document_symbol("file:///w/a.rs", timeout)
            .await
            .unwrap();
        let text = lock(&store).render(None);
        assert!(text.contains("3:1: warning: unused import"), "{text}");
        assert!(!text.contains("first push"), "push replaced: {text}");
        // Without a sink, pushes stay dropped (historical behavior).
        let mut sinkless = LspClient::new(DuplexLsp::new(diagnostics_server()));
        sinkless.initialize("file:///w", timeout).await.unwrap();
    }

    #[test]
    fn path_to_uri_shapes() {
        assert_eq!(
            path_to_uri(std::path::Path::new("/tmp/a.py")),
            "file:///tmp/a.py"
        );
    }

    /// Reserved and non-ASCII path characters percent-encode over UTF-8 so
    /// the URI survives round trips through servers that decode it; the
    /// Windows drive colon stays literal.
    #[test]
    fn path_to_uri_percent_encodes_reserved_and_non_ascii_bytes() {
        assert_eq!(
            path_to_uri(std::path::Path::new("/tmp/my file.py")),
            "file:///tmp/my%20file.py"
        );
        assert_eq!(
            path_to_uri(std::path::Path::new("/tmp/a#b%c.py")),
            "file:///tmp/a%23b%25c.py"
        );
        assert_eq!(
            path_to_uri(std::path::Path::new("/tmp/备注.rs")),
            "file:///tmp/%E5%A4%87%E6%B3%A8.rs"
        );
        assert_eq!(
            path_to_uri(std::path::Path::new("C:\\Users\\me\\a.py")),
            "file:///C:/Users/me/a.py"
        );
    }

    #[test]
    fn position_params_reject_missing_or_negative() {
        assert!(req_position(&json!({"line": 3}), "line").is_ok());
        assert!(req_position(&json!({}), "line").is_err());
        assert!(req_position(&json!({"line": -1}), "line").is_err());
        assert!(req_position(&json!({"line": "3"}), "line").is_err());
    }

    #[test]
    fn registry_resolves_commands_by_extension() {
        let dir = tempfile::tempdir().unwrap();
        let providers = LspProviders::new(dir.path().to_path_buf(), Vec::new());
        assert!(providers.command_for_path("a.rs").is_none());
        providers.register("rs", "rust-analyzer".to_owned());
        providers.register(".PY", "pyright-langserver --stdio".to_owned());
        assert_eq!(
            providers.command_for_path("src/main.rs"),
            Some("rust-analyzer".to_owned())
        );
        // Case-insensitive, dot-tolerant.
        assert_eq!(
            providers.command_for_path("a.py"),
            Some("pyright-langserver --stdio".to_owned())
        );
        assert_eq!(
            providers.command_for_path("A.PY"),
            Some("pyright-langserver --stdio".to_owned())
        );
        // Re-registering replaces the command.
        providers.register("rs", "other-server".to_owned());
        assert_eq!(
            providers.command_for_path("a.rs"),
            Some("other-server".to_owned())
        );
        // Extensionless files resolve to nothing.
        assert!(providers.command_for_path("Makefile").is_none());
        assert_eq!(providers.spawn_count(), 0);
    }

    #[tokio::test]
    async fn unregistered_extension_is_a_business_error_without_spawning() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ToolCtx {
            cwd: dir.path().to_path_buf(),
            deny_env: Vec::new(),
        };
        std::fs::write(dir.path().join("a.rs"), "fn main() {}\n").unwrap();
        let providers = Arc::new(LspProviders::new(dir.path().to_path_buf(), Vec::new()));
        let out = DocumentSymbols::with_providers(providers.clone())
            .execute(json!({"path": "a.rs"}), &ctx)
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(out.content.contains("no language server registered"));
        assert_eq!(providers.spawn_count(), 0);
    }

    #[tokio::test]
    async fn explicit_command_still_spawns_per_call_as_fallback() {
        // Unregistered extension + explicit (missing) binary: the per-call
        // spawn path runs and reports a business error, never touching the pool.
        let dir = tempfile::tempdir().unwrap();
        let ctx = ToolCtx {
            cwd: dir.path().to_path_buf(),
            deny_env: Vec::new(),
        };
        std::fs::write(dir.path().join("a.xyz"), "x\n").unwrap();
        let providers = Arc::new(LspProviders::new(dir.path().to_path_buf(), Vec::new()));
        let out = DocumentSymbols::with_providers(providers.clone())
            .execute(
                json!({"server_command": "wavecode-definitely-missing-server", "path": "a.xyz"}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(out.content.contains("failed to spawn"));
        assert_eq!(providers.spawn_count(), 0);
    }

    /// Counting fake: answers initialize + documentSymbol, counts symbol requests.
    fn counting_server(hits: Arc<std::sync::atomic::AtomicU64>) -> tokio::io::DuplexStream {
        let (client_end, server_end) = tokio::io::duplex(64 * 1024);
        tokio::spawn(async move {
            let mut io = tokio::io::BufReader::new(server_end);
            let timeout = Duration::from_secs(10);
            loop {
                let msg = match read_frame(&mut io, timeout).await {
                    Ok(m) => m,
                    Err(_) => break,
                };
                let method = msg
                    .get("method")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned();
                if method == "exit" {
                    break;
                }
                let Some(id) = msg.get("id").cloned() else {
                    continue;
                };
                let reply = match method.as_str() {
                    "initialize" => {
                        json!({"jsonrpc": "2.0", "id": id, "result": {"capabilities": {}}})
                    }
                    "textDocument/documentSymbol" => {
                        hits.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        json!({"jsonrpc": "2.0", "id": id, "result": [{"name": "main", "kind": 12}]})
                    }
                    "textDocument/definition" => {
                        hits.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        json!({"jsonrpc": "2.0", "id": id, "result": [{"uri": "file:///work/a.py"}]})
                    }
                    "textDocument/hover" => {
                        hits.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        json!({"jsonrpc": "2.0", "id": id, "result": {"contents": "doc"}})
                    }
                    "textDocument/references" => {
                        hits.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        json!({"jsonrpc": "2.0", "id": id, "result": []})
                    }
                    "shutdown" => json!({"jsonrpc": "2.0", "id": id, "result": null}),
                    _ => {
                        json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": "unknown method"}})
                    }
                };
                if write_frame(&mut io, &reply).await.is_err() {
                    break;
                }
            }
        });
        client_end
    }

    #[tokio::test]
    async fn pooled_client_is_reused_across_calls() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ToolCtx {
            cwd: dir.path().to_path_buf(),
            deny_env: Vec::new(),
        };
        std::fs::write(dir.path().join("a.py"), "x = 1\n").unwrap();
        let hits = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let providers = Arc::new(LspProviders::new(dir.path().to_path_buf(), Vec::new()));
        providers.register("py", "fake-server (injected)".to_owned());
        // Handshake the injected client, then install it as the pooled entry.
        let mut ready = LspClient::new(AnyTransport(Box::new(DuplexLsp::new(counting_server(
            hits.clone(),
        )))));
        ready
            .initialize("file:///work", Duration::from_secs(10))
            .await
            .unwrap();
        providers.insert_ready("py", ready);
        let symbols = DocumentSymbols::with_providers(providers.clone());
        for _ in 0..2 {
            let out = symbols
                .execute(json!({"path": "a.py"}), &ctx)
                .await
                .unwrap();
            assert!(!out.is_error, "pooled call failed: {}", out.content);
            assert!(out.content.contains("main"));
        }
        // Every navigation tool resolves the same pooled connection.
        let pos = json!({"path": "a.py", "line": 0, "character": 0});
        for (tool, marker) in [
            (
                Arc::new(GotoDefinition::with_providers(providers.clone())) as Arc<dyn Tool>,
                "a.py",
            ),
            (
                Arc::new(Hover::with_providers(providers.clone())) as Arc<dyn Tool>,
                "doc",
            ),
            (
                Arc::new(FindReferences::with_providers(providers.clone())) as Arc<dyn Tool>,
                "[]",
            ),
        ] {
            let out = tool.execute(pos.clone(), &ctx).await.unwrap();
            assert!(!out.is_error, "pooled call failed: {}", out.content);
            assert!(
                out.content.contains(marker),
                "unexpected body: {}",
                out.content
            );
        }
        // All calls rode one pooled connection (no real spawn, five requests).
        assert_eq!(providers.spawn_count(), 0);
        assert_eq!(hits.load(std::sync::atomic::Ordering::Relaxed), 5);
        providers.shutdown_all().await;
    }

    /// The diagnostics tool renders what pooled calls recorded: a navigation
    /// call rides the pooled client (whose pushes feed the shared store), the
    /// store render addresses the same resolved URI for a path filter, and a
    /// registry-less tool is a business error.
    #[tokio::test]
    async fn lsp_diagnostics_tool_reads_shared_store() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ToolCtx {
            cwd: dir.path().to_path_buf(),
            deny_env: Vec::new(),
        };
        std::fs::write(dir.path().join("a.py"), "x = 1\n").unwrap();
        let providers = Arc::new(LspProviders::new(dir.path().to_path_buf(), Vec::new()));
        providers.register("py", "fake-server (injected)".to_owned());
        let mut ready =
            LspClient::new(AnyTransport(Box::new(DuplexLsp::new(diagnostics_server()))));
        ready
            .initialize("file:///w", Duration::from_secs(10))
            .await
            .unwrap();
        providers.insert_ready("py", ready);
        // A navigation call records the push into the shared store.
        let out = DocumentSymbols::with_providers(providers.clone())
            .execute(json!({"path": "a.py"}), &ctx)
            .await
            .unwrap();
        assert!(!out.is_error, "pooled call failed: {}", out.content);
        // The tool renders the store without touching a server.
        let tool = LspDiagnostics::with_providers(providers.clone());
        let out = tool.execute(json!({}), &ctx).await.unwrap();
        assert!(!out.is_error);
        assert!(out.content.contains("unused import"), "{}", out.content);
        // A path filter resolves through the same guard as the other tools
        // and matches the URI the pooled call addressed.
        let out = tool.execute(json!({"path": "a.py"}), &ctx).await.unwrap();
        assert!(
            out.content.contains("3:1: warning: unused import"),
            "{}",
            out.content
        );
        // Escaping paths are rejected by the path guard.
        let out = tool
            .execute(json!({"path": "../evil.py"}), &ctx)
            .await
            .unwrap();
        assert!(out.is_error);
        // An untracked path is an honest empty answer, not an error.
        let out = tool
            .execute(json!({"path": "missing.py"}), &ctx)
            .await
            .unwrap();
        assert!(!out.is_error);
        assert!(out.content.contains("no diagnostics recorded"));
        // Registry-less builds have no store to read: business error.
        let out = LspDiagnostics::new()
            .execute(json!({}), &ctx)
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(out.content.contains("no language server registry"));
        providers.shutdown_all().await;
    }

    #[tokio::test]
    async fn tool_rejects_escape_and_missing_params() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ToolCtx {
            cwd: dir.path().to_path_buf(),
            deny_env: Vec::new(),
        };
        // Missing server_command never spawns.
        let out = DocumentSymbols::new()
            .execute(json!({"path": "a.py"}), &ctx)
            .await
            .unwrap();
        assert!(out.is_error);
        // Escaping path never spawns either.
        let out = GotoDefinition::new()
            .execute(
                json!({"server_command": "nope", "path": "../evil.py", "line": 0, "character": 0}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(out.content.contains("evil.py") || out.content.to_lowercase().contains("escape"));
        // Unknown binary is a business error, not a panic.
        std::fs::write(dir.path().join("a.py"), "x = 1\n").unwrap();
        let out = Hover::new()
            .execute(
                json!({"server_command": "wavecode-definitely-missing-server", "path": "a.py", "line": 0, "character": 0}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(out.is_error);
    }

    // -- child environment scrubbing --

    /// The LSP spawn path scrubs the child environment like the shell tool:
    /// a `server_command` can arrive as model input, so sensitive-shaped
    /// variables and `deny_env` names never reach the server process while
    /// normal variables stay visible. A scripted fake server echoes the
    /// three variables inside an LSP-framed response; skipped where the
    /// platform refuses the spawn or the temp path carries a space (the
    /// whitespace-split command cannot quote it).
    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn server_command_child_env_is_scrubbed() {
        let _guard = crate::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        #[cfg(windows)]
        if dir.path().to_string_lossy().contains(' ') {
            eprintln!("temp path contains a space; skipping the LSP env scrub test");
            return;
        }

        // Fake server: answers `initialize` (id 1), replies to the
        // navigation request (id 2) with the three variables embedded, and
        // answers the tool's `shutdown` (id 3) so the call never waits out
        // its timeout on a quiet pipe. Responses are written first, then
        // stdin is drained until the client closes it. Exiting immediately
        // closes that pipe, and a later write fails with EPIPE.
        #[cfg(windows)]
        let server_command = {
            let script = dir.path().join("lsp_env.ps1");
            std::fs::write(
                &script,
                concat!(
                    "$b1='{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"capabilities\":{}}}'\n",
                    "$b2='{\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{\"v\":\"' + $env:FOO_LSP_SECRET + '|' + $env:FOO_LSP_DENY + '|' + $env:FOO_LSP_NORMAL + '\"}}'\n",
                    "$b3='{\"jsonrpc\":\"2.0\",\"id\":3,\"result\":null}'\n",
                    "[Console]::Out.Write('Content-Length: ' + $b1.Length + \"`r`n`r`n\" + $b1)\n",
                    "[Console]::Out.Write('Content-Length: ' + $b2.Length + \"`r`n`r`n\" + $b2)\n",
                    "[Console]::Out.Write('Content-Length: ' + $b3.Length + \"`r`n`r`n\" + $b3)\n",
                    "[Console]::Out.Flush()\n",
                    "[Console]::In.ReadToEnd() | Out-Null\n",
                ),
            )
            .unwrap();
            format!(
                "powershell -NoProfile -ExecutionPolicy Bypass -File {}",
                script.display()
            )
        };
        #[cfg(unix)]
        let server_command = {
            let script = dir.path().join("lsp_env.sh");
            std::fs::write(
                &script,
                concat!(
                    "(\n",
                    "b1=\"{\\\"jsonrpc\\\":\\\"2.0\\\",\\\"id\\\":1,\\\"result\\\":{\\\"capabilities\\\":{}}}\"",
                    "\n",
                    "b2=\"{\\\"jsonrpc\\\":\\\"2.0\\\",\\\"id\\\":2,\\\"result\\\":{\\\"v\\\":\\\"$FOO_LSP_SECRET|$FOO_LSP_DENY|$FOO_LSP_NORMAL\\\"}}\"",
                    "\n",
                    "b3=\"{\\\"jsonrpc\\\":\\\"2.0\\\",\\\"id\\\":3,\\\"result\\\":null}\"",
                    "\n",
                    "printf 'Content-Length: %s\\r\\n\\r\\n%s' \"${#b1}\" \"$b1\"",
                    "\n",
                    "printf 'Content-Length: %s\\r\\n\\r\\n%s' \"${#b2}\" \"$b2\"",
                    "\n",
                    "printf 'Content-Length: %s\\r\\n\\r\\n%s' \"${#b3}\" \"$b3\"",
                    "\n",
                    ")\n",
                    "cat >/dev/null\n",
                ),
            )
            .unwrap();
            format!("sh {}", script.display())
        };

        unsafe {
            std::env::set_var("FOO_LSP_SECRET", "lsp-secret-value");
            std::env::set_var("FOO_LSP_DENY", "lsp-deny-value");
            std::env::set_var("FOO_LSP_NORMAL", "lsp-visible-value");
        }
        let ctx = ToolCtx {
            cwd: dir.path().to_path_buf(),
            deny_env: vec!["FOO_LSP_DENY".to_owned()],
        };
        // The navigation result carries the echoed variables. The default
        // timeout covers a cold PowerShell start on a loaded CI runner;
        // the fake server answers shutdown, so the happy path returns
        // as soon as the frames arrive.
        let out = DocumentSymbols::new()
            .execute(
                json!({"server_command": server_command, "path": "a.py"}),
                &ctx,
            )
            .await
            .unwrap();
        unsafe {
            std::env::remove_var("FOO_LSP_SECRET");
            std::env::remove_var("FOO_LSP_DENY");
            std::env::remove_var("FOO_LSP_NORMAL");
        }
        assert!(!out.is_error, "fake server call failed: {}", out.content);
        // Sensitive-shaped and deny-listed names are stripped: the values
        // never reach the server, so it echoes empty slots for them.
        assert!(
            !out.content.contains("lsp-secret-value"),
            "secret leaked to the LSP child: {}",
            out.content
        );
        assert!(
            !out.content.contains("lsp-deny-value"),
            "deny_env value leaked to the LSP child: {}",
            out.content
        );
        // No over-stripping: a normal variable stays visible.
        assert!(
            out.content.contains("lsp-visible-value"),
            "normal variable was over-stripped: {}",
            out.content
        );
    }
}
