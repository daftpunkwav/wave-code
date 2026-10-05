//! Minimal LSP client session: JSON-RPC request/response correlation, the
//! `initialize`/`shutdown` handshake, and the per-method navigation
//! requests, plus the `file://` URI helpers shared by callers.

use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};

use crate::{Result, ToolsError, lock};

use super::DiagnosticsStore;
use super::LspTransport;

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
            // A message carrying a `method` is a server-to-client
            // request (e.g. `workspace/configuration`), not the response
            // to ours — the server picks its own ids, so the id check
            // alone would consume it as a response.
            if msg.get("method").is_some() {
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
        // Best-effort by contract: a failed shutdown request (server
        // error, transport hiccup) must not skip the `exit` notification,
        // or the server never terminates its session.
        let _ = self.request("shutdown", Value::Null, timeout).await;
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
pub(super) fn path_to_uri(path: &std::path::Path) -> String {
    let text = path.to_string_lossy().replace('\\', "/");
    if text.starts_with('/') {
        format!("file://{}", encode_uri_path(&text))
    } else {
        format!("file:///{}", encode_uri_path(&text))
    }
}
