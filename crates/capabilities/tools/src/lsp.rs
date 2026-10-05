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

mod client;
mod diagnostics;
mod providers;
mod tools;
mod transport;

#[cfg(test)]
mod tests;

pub use client::LspClient;
pub use diagnostics::DiagnosticsStore;
pub use providers::LspProviders;
pub use tools::{DocumentSymbols, FindReferences, GotoDefinition, Hover, LspDiagnostics};
pub use transport::AnyTransport;
// `ChildLsp` is spawned only through `AnyTransport` internally; the
// re-export keeps the historical `lsp::ChildLsp` path resolving.
#[allow(unused_imports)]
pub use transport::ChildLsp;
pub use transport::LspTransport;

use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;

use crate::{Result, ToolCtx, ToolOutput, ToolsError, err_output, ok_output, req_str};

use client::path_to_uri;
use providers::{RequestTarget, extension_of};

/// Default per-read timeout: 30 s.
const DEFAULT_TIMEOUT_MS: u64 = 30_000;
/// Timeout ceiling: 300 s, values above are clamped.
const MAX_TIMEOUT_MS: u64 = 300_000;

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
async fn run_lsp_call<F>(
    input: &Value,
    ctx: &ToolCtx,
    providers: Option<&Arc<LspProviders>>,
    with_position: bool,
    method: &str,
    call: F,
) -> Result<ToolOutput>
where
    F: AsyncFnOnce(&mut LspClient<AnyTransport>, &str, u32, u32, Duration) -> Result<Value>,
{
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
