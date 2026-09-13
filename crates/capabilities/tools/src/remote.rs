/*! @file RemoteShell
 *  @description Generic remote shell client over a minimal worker protocol.
 *
 *  Responsibilities:
 *  - POST commands to a remote worker speaking the JSON protocol below.
 *  - Enforce HTTPS except for loopback HTTP (tests and local workers).
 *  - Map worker responses, errors and timeouts to tool outputs.
 *
 *  Worker protocol (minimal, documented):
 *  - Request:  POST {endpoint} with `Content-Type: application/json`,
 *    optional `Authorization: Bearer <bearer>`, and body
 *    `{"cmd": "<command>", "cwd": "<dir>"?, "env_subset": {...}?}`.
 *  - Success: HTTP 200 with `{"stdout": "...", "stderr": "..."?, "exit_code": 0}`.
 *    `stdout` also accepts the aliases `output` and `content`.
 *  - Failure: any non-2xx status, unreachable host, timeout, or invalid
 *    JSON becomes a business error (`is_error: true`) — never `Err`.
 *
 *  Hosted E2B-style providers do NOT speak this protocol directly; they
 *  need an adapter service in front, which is not included here.
 *
 *  This module must not depend on: transport, frontend crates.
 */

use std::time::Duration;

use serde_json::{Value, json};

use crate::{Result, Tool, ToolCtx, ToolOutput};

/// Default timeout: 60 s.
const DEFAULT_TIMEOUT_MS: u64 = 60_000;
/// Timeout cap: 300 s, clamped to the cap when exceeded.
const MAX_TIMEOUT_MS: u64 = 300_000;
/// Error-body cap for non-2xx responses: 4 KB.
const MAX_ERROR_BYTES: usize = 4 * 1024;

fn err_output(reason: impl Into<String>) -> ToolOutput {
    ToolOutput {
        content: reason.into(),
        is_error: true,
    }
}

/// Whether an endpoint URL may be used: HTTPS anywhere, HTTP only for
/// loopback hosts (tests and local workers). Pure and hermetic.
pub fn endpoint_allowed(endpoint: &str) -> bool {
    let (scheme, rest) = match endpoint.split_once("://") {
        Some(pair) => pair,
        None => return false,
    };
    let authority = rest
        .split(['/', '?', '#'])
        .next()
        .unwrap_or("")
        .split('@')
        .next_back()
        .unwrap_or("");
    // Bracketed IPv6 keeps its colons (`[::1]:8080`); plain hosts split off
    // the port instead.
    let bare: String = if let Some(inside) = authority
        .strip_prefix('[')
        .and_then(|r| r.find(']').map(|i| &r[..i]))
    {
        format!("[{inside}]")
    } else {
        authority
            .split(':')
            .next()
            .unwrap_or("")
            .strip_suffix('.')
            .unwrap_or(authority.split(':').next().unwrap_or(""))
            .to_owned()
    };
    match scheme.to_lowercase().as_str() {
        "https" => !bare.is_empty(),
        "http" => {
            bare.eq_ignore_ascii_case("localhost")
                || bare == "127.0.0.1"
                || bare == "::1"
                || bare == "[::1]"
        }
        _ => false,
    }
}

/// Pick the worker's output text: `stdout`, with `output`/`content` aliases.
fn worker_stdout(body: &Value) -> String {
    for key in ["stdout", "output", "content"] {
        if let Some(text) = body.get(key).and_then(Value::as_str) {
            return text.to_owned();
        }
    }
    String::new()
}

/// Generic remote shell tool: `remote_shell {endpoint, bearer?, command,
/// timeout_ms?}`. A writing tool (remote side effects), never read-only.
pub struct RemoteShell;

#[async_trait::async_trait]
impl Tool for RemoteShell {
    fn name(&self) -> &str {
        "remote_shell"
    }

    fn description(&self) -> &str {
        "Run a shell command on a remote worker speaking the minimal JSON worker protocol \
         (POST {cmd, cwd?, env_subset?} -> {stdout, stderr?, exit_code}). `endpoint` must be \
         HTTPS, except HTTP for loopback workers (localhost / 127.0.0.1 / ::1). `bearer` sets \
         the Authorization header when given. `timeout_ms` bounds the round trip \
         (default 60000 ms, clamped to 300000 ms). Hosted providers (E2B-style) need an \
         adapter service speaking this protocol, which is not included."
    }

    fn input_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "endpoint": {
                    "type": "string",
                    "description": "Worker URL (HTTPS, or HTTP for loopback only)"
                },
                "bearer": {
                    "type": "string",
                    "description": "Bearer token for the Authorization header (optional)"
                },
                "command": {
                    "type": "string",
                    "description": "Shell command for the worker to execute"
                },
                "timeout_ms": {
                    "type": "integer",
                    "description": "Timeout in milliseconds (default 60000, clamped to max 300000)"
                }
            },
            "required": ["endpoint", "command"]
        })
    }

    fn is_read_only(&self) -> bool {
        false
    }

    async fn execute(&self, input: Value, ctx: &ToolCtx) -> Result<ToolOutput> {
        let endpoint = match input.get("endpoint").and_then(Value::as_str) {
            Some(e) if !e.is_empty() => e.to_owned(),
            _ => {
                return Ok(err_output(
                    "missing or invalid parameter 'endpoint' (non-empty string required)",
                ));
            }
        };
        let command = match input.get("command").and_then(Value::as_str) {
            Some(c) => c.to_owned(),
            None => {
                return Ok(err_output(
                    "missing or invalid parameter 'command' (string required)",
                ));
            }
        };
        let bearer = input
            .get("bearer")
            .and_then(Value::as_str)
            .map(|s| s.to_owned());
        let timeout_ms = match input.get("timeout_ms") {
            None => DEFAULT_TIMEOUT_MS,
            Some(v) => match v.as_u64() {
                Some(n) => n.min(MAX_TIMEOUT_MS),
                None => {
                    return Ok(err_output(
                        "invalid parameter 'timeout_ms' (non-negative integer required)",
                    ));
                }
            },
        };
        if !endpoint_allowed(&endpoint) {
            return Ok(err_output(
                "endpoint must be HTTPS (HTTP is allowed only for loopback workers)",
            ));
        }
        let client = match reqwest::Client::builder()
            .timeout(Duration::from_millis(timeout_ms))
            .build()
        {
            Ok(c) => c,
            Err(e) => return Ok(err_output(format!("http client failed: {e}"))),
        };
        let mut request = client.post(&endpoint).json(&json!({
            "cmd": command,
            "cwd": ctx.cwd.to_string_lossy(),
        }));
        if let Some(token) = bearer {
            request = request.bearer_auth(token);
        }
        let response = match request.send().await {
            Ok(r) => r,
            Err(e) => return Ok(err_output(format!("remote worker request failed: {e}"))),
        };
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            let clipped = body.chars().take(MAX_ERROR_BYTES).collect::<String>();
            return Ok(err_output(format!(
                "remote worker error (http {status}): {clipped}"
            )));
        }
        let body: Value = match response.json().await {
            Ok(v) => v,
            Err(e) => return Ok(err_output(format!("remote worker bad JSON: {e}"))),
        };
        let stdout = worker_stdout(&body);
        let stderr = body
            .get("stderr")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        let code = body.get("exit_code").and_then(Value::as_i64).unwrap_or(0) as i32;
        let mut content = format!("exit code: {code}");
        if !stdout.is_empty() {
            content.push_str(&format!("\n--- stdout ---\n{stdout}"));
        }
        if !stderr.is_empty() {
            content.push_str(&format!("\n--- stderr ---\n{stderr}"));
        }
        Ok(ToolOutput {
            content,
            is_error: code != 0,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Tool, ToolCtx};
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::time::Duration;

    fn ctx() -> (tempfile::TempDir, ToolCtx) {
        let dir = tempfile::tempdir().unwrap();
        let c = ToolCtx {
            cwd: dir.path().to_path_buf(),
            deny_env: Vec::new(),
        };
        (dir, c)
    }

    #[test]
    fn endpoint_policy_allows_https_and_loopback_http_only() {
        assert!(endpoint_allowed("https://worker.example.com/run"));
        assert!(endpoint_allowed("https://10.0.0.5:8443/run"));
        assert!(endpoint_allowed("http://localhost:8080/run"));
        assert!(endpoint_allowed("http://127.0.0.1:8080/run"));
        assert!(endpoint_allowed("http://[::1]:8080/run"));
        assert!(!endpoint_allowed("http://192.0.2.1/run"));
        assert!(!endpoint_allowed("http://worker.example.com/run"));
        assert!(!endpoint_allowed("ftp://worker.example.com/run"));
        assert!(!endpoint_allowed("not-a-url"));
    }

    #[test]
    fn stdout_aliases_cover_worker_shapes() {
        assert_eq!(worker_stdout(&json!({"stdout": "a"})), "a".to_owned());
        assert_eq!(worker_stdout(&json!({"output": "b"})), "b".to_owned());
        assert_eq!(worker_stdout(&json!({"content": "c"})), "c".to_owned());
        assert_eq!(worker_stdout(&json!({})), "".to_owned());
    }

    /// Hand-rolled hermetic stub: one connection, canned bytes, loopback only.
    fn stub_once(
        response: Vec<u8>,
        hold: Option<Duration>,
    ) -> (String, std::thread::JoinHandle<Vec<u8>>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("loopback listener");
        let endpoint = format!(
            "http://127.0.0.1:{}/run",
            listener.local_addr().unwrap().port()
        );
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("one connection");
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .expect("read timeout");
            // Read the request head plus body (Content-Length bounded).
            let mut raw = Vec::new();
            let mut buf = [0u8; 4096];
            let mut header_end = None;
            let mut want = 0usize;
            while header_end.is_none() || raw.len() < header_end.unwrap() + want {
                match stream.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        raw.extend_from_slice(&buf[..n]);
                        if header_end.is_none() {
                            if let Some(pos) = find_crlf2(&raw) {
                                header_end = Some(pos);
                                want = content_length(&raw[..pos]);
                                if want > 64 * 1024 {
                                    break;
                                }
                            } else if raw.len() > 16 * 1024 {
                                break;
                            }
                        }
                    }
                    Err(_) => break,
                }
            }
            if let Some(delay) = hold {
                std::thread::sleep(delay);
            }
            let _ = stream.write_all(&response);
            raw
        });
        (endpoint, handle)
    }

    fn find_crlf2(raw: &[u8]) -> Option<usize> {
        raw.windows(4).position(|w| w == b"\r\n\r\n").map(|p| p + 4)
    }

    fn content_length(head: &[u8]) -> usize {
        let text = String::from_utf8_lossy(head).to_lowercase();
        text.lines()
            .filter_map(|line| line.strip_prefix("content-length:"))
            .filter_map(|v| v.trim().parse::<usize>().ok())
            .next()
            .unwrap_or(0)
    }

    fn http_json(status: &str, body: &str) -> Vec<u8> {
        format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .into_bytes()
    }

    #[tokio::test]
    async fn stub_success_round_trip() {
        let body = r#"{"stdout":"hi-remote","exit_code":0}"#;
        let (endpoint, handle) = stub_once(http_json("200 OK", body), None);
        let (_d, c) = ctx();
        let out = RemoteShell
            .execute(json!({"endpoint": endpoint, "command": "echo hi"}), &c)
            .await
            .unwrap();
        let raw = handle.join().expect("stub thread");
        let request = String::from_utf8_lossy(&raw);
        assert!(
            request.contains("\"cmd\":\"echo hi\""),
            "worker protocol body: {request}"
        );
        assert!(!out.is_error, "stub success: {}", out.content);
        assert!(out.content.contains("hi-remote"));
    }

    #[tokio::test]
    async fn stub_bearer_reaches_worker() {
        let body = r#"{"stdout":"ok","exit_code":0}"#;
        // The stub answers 200 only when the bearer header is present.
        let listener = TcpListener::bind("127.0.0.1:0").expect("loopback listener");
        let endpoint = format!(
            "http://127.0.0.1:{}/run",
            listener.local_addr().unwrap().port()
        );
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("one connection");
            let mut buf = [0u8; 8192];
            let mut raw = Vec::new();
            while find_crlf2(&raw).is_none_or(|pos| raw.len() < pos + content_length(&raw[..pos])) {
                match stream.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => raw.extend_from_slice(&buf[..n]),
                    Err(_) => break,
                }
            }
            let authed = String::from_utf8_lossy(&raw).contains("Bearer secret-123");
            let (status, payload) = if authed {
                ("200 OK", body)
            } else {
                ("401 Unauthorized", r#"{"error":"nope"}"#)
            };
            let _ = stream.write_all(&http_json(status, payload));
        });
        let (_d, c) = ctx();
        let out = RemoteShell
            .execute(
                json!({"endpoint": endpoint, "command": "echo hi", "bearer": "secret-123"}),
                &c,
            )
            .await
            .unwrap();
        handle.join().expect("stub thread");
        assert!(!out.is_error, "authed request: {}", out.content);
    }

    #[tokio::test]
    async fn stub_error_status_is_business_error() {
        let (endpoint, handle) = stub_once(http_json("500 Internal Server Error", "boom"), None);
        let (_d, c) = ctx();
        let out = RemoteShell
            .execute(json!({"endpoint": endpoint, "command": "echo hi"}), &c)
            .await
            .unwrap();
        handle.join().expect("stub thread");
        assert!(out.is_error);
        assert!(
            out.content.contains("500"),
            "status surfaces: {}",
            out.content
        );
    }

    #[tokio::test]
    async fn stub_slow_worker_times_out() {
        let body = r#"{"stdout":"late","exit_code":0}"#;
        let (endpoint, handle) = stub_once(http_json("200 OK", body), Some(Duration::from_secs(2)));
        let (_d, c) = ctx();
        let out = RemoteShell
            .execute(
                json!({"endpoint": endpoint, "command": "echo hi", "timeout_ms": 400}),
                &c,
            )
            .await
            .unwrap();
        handle.join().expect("stub thread");
        assert!(out.is_error);
        assert!(
            out.content.contains("remote worker request failed"),
            "timeout surfaces as request failure: {}",
            out.content
        );
    }

    #[tokio::test]
    async fn non_loopback_http_refused_without_traffic() {
        let (_d, c) = ctx();
        let out = RemoteShell
            .execute(
                json!({"endpoint": "http://192.0.2.1/run", "command": "echo hi"}),
                &c,
            )
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(
            out.content.contains("HTTPS"),
            "policy message: {}",
            out.content
        );
    }
}
