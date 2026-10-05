//! Unit tests for the LSP module: frame encode/decode, client conversations
//! against in-memory fake servers, diagnostics capture and caps, URI
//! shaping, and the registry/tool flows (moved verbatim from the former
//! inline test module).

use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::AsyncWriteExt;

use super::diagnostics::{MAX_DIAGNOSTIC_FILES, MAX_DIAGNOSTICS_PER_FILE};
use super::transport::{
    DuplexLsp, MAX_HEADER_LINE_BYTES, encode_message, parse_frame, read_frame, write_frame,
};
use super::*;
use crate::{Tool, lock};

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
    let mut client =
        LspClient::new(DuplexLsp::new(diagnostics_server())).with_diagnostics_sink(store.clone());
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
    let mut ready = LspClient::new(AnyTransport(Box::new(DuplexLsp::new(diagnostics_server()))));
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

    // nosemgrep: rust.lang.security.unsafe-usage.unsafe-usage
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
    // nosemgrep: rust.lang.security.unsafe-usage.unsafe-usage
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
