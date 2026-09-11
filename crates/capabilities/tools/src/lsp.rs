/*! @file Lsp
 * @description Minimal stdio LSP client plus code-navigation tools.
 *
 * Responsibilities:
 * - Frame/unframe Content-Length JSON-RPC messages over async stdio
 * - Speak initialize/shutdown plus symbol/definition/hover/references
 * - Expose four read-only tools, each spawning an explicit server command
 *
 * This module must not depend on: UI-layer components, bundled servers.
 */

//! LSP navigation tools (`document_symbols` / `goto_definition` / `hover` /
//! `find_references`): with an explicit `server_command` each call spawns the
//! caller-provided language server over stdio, runs `initialize`, issues one
//! request, then `shutdown`s. With a [`LspProviders`] registry the server for
//! the file extension is spawned lazily once, initialized once, and reused
//! across calls (shutdown on registry drop).
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

use crate::{Result, Tool, ToolCtx, ToolOutput, ToolsError, lock};

/// Default per-read timeout: 30 s.
const DEFAULT_TIMEOUT_MS: u64 = 30_000;
/// Timeout ceiling: 300 s, values above are clamped.
const MAX_TIMEOUT_MS: u64 = 300_000;

/// Build a business-failure output so the model can self-correct.
fn err_output(reason: impl Into<String>) -> ToolOutput {
    ToolOutput {
        content: reason.into(),
        is_error: true,
    }
}

/// Build a success output.
fn ok_output(content: impl Into<String>) -> ToolOutput {
    ToolOutput {
        content: content.into(),
        is_error: false,
    }
}

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

/// Read one framed message (whole read bounded by `timeout`).
async fn read_frame<R>(reader: &mut R, timeout: Duration) -> std::io::Result<Value>
where
    R: AsyncBufReadExt + Unpin,
{
    tokio::time::timeout(timeout, async {
        let mut content_length: Option<usize> = None;
        loop {
            let mut line = String::new();
            let n = reader.read_line(&mut line).await?;
            if n == 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "LSP stream closed",
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
    pub fn spawn(server_command: &str, cwd: &std::path::Path) -> std::io::Result<Self> {
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
    /// Spawn a real language server over stdio.
    pub fn spawn(server_command: &str, cwd: &Path) -> std::io::Result<Self> {
        Ok(Self(Box::new(ChildLsp::spawn(server_command, cwd)?)))
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
    spawns: AtomicU64,
}

impl LspProviders {
    /// Build for `workspace_root` (must be the workspace directory; used as
    /// the server working directory and the `initialize` rootUri).
    pub fn new(workspace_root: PathBuf) -> Self {
        let root_uri = path_to_uri(&workspace_root);
        Self {
            root: workspace_root,
            root_uri,
            commands: std::sync::Mutex::new(HashMap::new()),
            pooled: std::sync::Mutex::new(HashMap::new()),
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
    /// (backed by an in-memory fake; no real spawn, counter untouched).
    #[cfg(test)]
    pub fn insert_ready(&self, extension: &str, client: LspClient<AnyTransport>) {
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
            let transport = AnyTransport::spawn(&command, &self.root).map_err(|e| {
                err_output(format!("failed to spawn language server '{command}': {e}"))
            })?;
            let mut client = LspClient::new(transport);
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

/// Minimal LSP client: `initialize` (plus `initialized`), one request at a
/// time, then `shutdown`/`exit`. Request ids count up from 1 per client.
pub struct LspClient<T> {
    transport: T,
    next_id: u64,
}

impl<T: LspTransport> LspClient<T> {
    /// Wrap an already-connected transport.
    pub fn new(transport: T) -> Self {
        Self {
            transport,
            next_id: 1,
        }
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

/// Convert a filesystem path to a `file://` URI (best-effort:
/// backslashes become forward slashes, no percent-encoding).
fn path_to_uri(path: &std::path::Path) -> String {
    let text = path.to_string_lossy().replace('\\', "/");
    if text.starts_with('/') {
        format!("file://{text}")
    } else {
        format!("file:///{text}")
    }
}

/// Parse `timeout_ms` (default 30 s, clamped to 300 s).
fn resolve_timeout(input: &Value) -> std::result::Result<Duration, ToolOutput> {
    let ms = match input.get("timeout_ms") {
        None | Some(Value::Null) => DEFAULT_TIMEOUT_MS,
        Some(v) => match v.as_u64() {
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

/// Extract a required string parameter.
fn req_str<'a>(input: &'a Value, key: &str) -> std::result::Result<&'a str, ToolOutput> {
    input.get(key).and_then(Value::as_str).ok_or_else(|| {
        err_output(format!(
            "missing or invalid parameter '{key}' (string required)"
        ))
    })
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
        let transport = match AnyTransport::spawn(&server_command, &ctx.cwd) {
            Ok(transport) => transport,
            Err(e) => {
                return Ok(err_output(format!(
                    "failed to spawn language server '{server_command}': {e}"
                )));
            }
        };
        let mut client = LspClient::new(transport);
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
        .pooled_call(&ext, RequestTarget { uri: &uri, line, character }, timeout, method, call)
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
        "document_symbols"
    }

    fn description(&self) -> &str {
        "List the symbol outline (functions, classes, etc.) of a file via a language \
         server. Omit server_command to use the registered provider for the file
         extension, or provide it as an override (e.g. rust-analyzer, \
         no server is bundled. Path is relative to the working directory."
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
        "goto_definition"
    }

    fn description(&self) -> &str {
        "Jump to the definition of the symbol at a 0-based line/character position \
         via a language server. Omit server_command to use the registered provider
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
        "hover"
    }

    fn description(&self) -> &str {
        "Show hover documentation for the symbol at a 0-based line/character position \
         via a language server. Omit server_command to use the registered provider
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
        run_lsp_call(&input, ctx, self.providers.as_ref(), true, "hover", async |c, uri, line, ch, t| {
            c.hover(uri, line, ch, t).await
        })
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
        "find_references"
    }

    fn description(&self) -> &str {
        "Find all references (including the declaration) to the symbol at a 0-based \
         line/character position via a language server. Omit server_command to use
         the registered provider for the file extension, or provide it as an
         override (e.g. \
         rust-analyzer, pyright-langserver --stdio); no server is bundled. Path is \
         relative to the working directory."
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

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn path_to_uri_shapes() {
        assert_eq!(
            path_to_uri(std::path::Path::new("/tmp/a.py")),
            "file:///tmp/a.py"
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
        let providers = LspProviders::new(dir.path().to_path_buf());
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
        let providers = Arc::new(LspProviders::new(dir.path().to_path_buf()));
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
        let providers = Arc::new(LspProviders::new(dir.path().to_path_buf()));
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
    fn counting_server(
        hits: Arc<std::sync::atomic::AtomicU64>,
    ) -> tokio::io::DuplexStream {
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
        let providers = Arc::new(LspProviders::new(dir.path().to_path_buf()));
        providers.register("py", "fake-server (injected)".to_owned());
        // Handshake the injected client, then install it as the pooled entry.
        let mut ready =
            LspClient::new(AnyTransport(Box::new(DuplexLsp::new(counting_server(
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
            assert!(out.content.contains(marker), "unexpected body: {}", out.content);
        }
        // All calls rode one pooled connection (no real spawn, five requests).
        assert_eq!(providers.spawn_count(), 0);
        assert_eq!(hits.load(std::sync::atomic::Ordering::Relaxed), 5);
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
}
