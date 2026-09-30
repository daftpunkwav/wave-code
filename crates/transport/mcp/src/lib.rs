/*!
 * @file McpTransport
 * @description JSON-RPC framing over child-process stdio pipes.
 *
 * Responsibilities:
 * - Encode requests and decode `jsonrpc: "2.0"` line-delimited responses.
 * - Correlate responses to requests by numeric id (wrapping, never zero).
 * - Manage child process stdio with timeouts on every read.
 *
 * This module must not depend on: any workspace protocol crate. MCP
 * servers speak their own JSON-RPC dialect; translation to harness
 * tools happens at the composition root.
 */

//! MCP transport: bytes on pipes, correlation in memory.
//!
//! The duplex-tested core (framing plus correlation) runs over any async
//! byte streams; process management only spawns and kills.

use std::collections::HashMap;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

/// Streamable-HTTP transport (JSON-RPC over POST with SSE responses).
pub mod http;

/// HTTP stub for this crate's tests and downstream test suites;
/// compiled under `test-support` (or this crate's own tests), never in
/// production builds.
#[cfg(any(test, feature = "test-support"))]
pub mod test_support;

/// Next request id seed; ids increase monotonically per transport.
pub const FIRST_REQUEST_ID: u64 = 1;

/// Advance a request id without panicking on overflow and without reusing 0.
fn next_id_after(current: u64) -> u64 {
    let next = current.wrapping_add(1);
    if next == 0 { FIRST_REQUEST_ID } else { next }
}

/// Read timeout for one response line in seconds.
pub const DEFAULT_RESPONSE_TIMEOUT_SECS: u64 = 30;

/// One outbound JSON-RPC request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JsonRpcRequest {
    /// Correlation id.
    pub id: u64,
    /// Method name, e.g. `tools/call`.
    pub method: String,
    /// Parameters payload.
    pub params: serde_json::Value,
}

impl JsonRpcRequest {
    /// Encode as one NDJSON line (no trailing newline included).
    pub fn encode(&self) -> String {
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": self.id,
            "method": self.method,
            "params": self.params,
        })
        .to_string()
    }
}

/// One inbound JSON-RPC response line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JsonRpcResponse {
    /// Correlation id echoed by the server.
    pub id: u64,
    /// Result payload or error object, verbatim.
    pub payload: serde_json::Value,
}

/// One inbound JSON-RPC line classified by shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JsonRpcMessage {
    /// A response (or server-initiated request) carrying a numeric id.
    Response(JsonRpcResponse),
    /// A server-initiated notification: no usable id, a method present.
    /// Never a response; callers skip it.
    Notification,
}

/// Decode one inbound line into its message shape, rejecting malformed
/// frames explicitly.
///
/// Only `jsonrpc: "2.0"` frames are accepted. A frame with a numeric id is
/// a response; a frame without a usable id that names a method is a
/// server-initiated notification (interleaved chatter must not abort an
/// in-flight exchange); anything else is malformed. When a frame carries
/// both `result` and `error`, the error wins so failures are never read as
/// success.
pub fn decode_message(line: &str) -> Result<JsonRpcMessage, TransportError> {
    let value: serde_json::Value =
        serde_json::from_str(line).map_err(|_| TransportError::BadFrame(line.to_string()))?;
    if value.get("jsonrpc").and_then(|v| v.as_str()) != Some("2.0") {
        return Err(TransportError::BadFrame(line.to_string()));
    }
    match value.get("id").and_then(|v| v.as_u64()) {
        Some(id) => {
            let payload = value
                .get("error")
                .or_else(|| value.get("result"))
                .cloned()
                .unwrap_or(serde_json::Value::Null);
            Ok(JsonRpcMessage::Response(JsonRpcResponse { id, payload }))
        }
        // No usable id — absent, null, or non-numeric — plus a method:
        // a server-initiated notification.
        None if value.get("method").and_then(|v| v.as_str()).is_some() => {
            Ok(JsonRpcMessage::Notification)
        }
        None => Err(TransportError::BadFrame(line.to_string())),
    }
}

/// Decode one response line, rejecting malformed frames explicitly.
///
/// Only `jsonrpc: "2.0"` frames are accepted. When a frame carries both
/// `result` and `error`, the error wins so failures are never read as success.
pub fn decode_response(line: &str) -> Result<JsonRpcResponse, TransportError> {
    match decode_message(line)? {
        JsonRpcMessage::Response(response) => Ok(response),
        // Historic contract for direct callers: a notification line is not
        // a response and reads as a bad frame. Exchange loops that must
        // tolerate interleaved notifications use [`decode_message`] (or
        // [`ChildTransport::recv_message`]) instead.
        JsonRpcMessage::Notification => Err(TransportError::BadFrame(line.to_string())),
    }
}

/// Transport failures.
#[derive(Debug, thiserror::Error)]
pub enum TransportError {
    /// Child process failed to spawn.
    #[error("failed to spawn MCP server {command}: {cause}")]
    Spawn {
        /// Command that failed to start.
        command: String,
        /// OS-level cause.
        cause: String,
    },
    /// A line is not a JSON-RPC response.
    #[error("malformed response frame: {0}")]
    BadFrame(String),
    /// No response arrived within the timeout.
    #[error("response timed out after {0}s")]
    Timeout(u64),
    /// Pipes closed mid-exchange.
    #[error("transport closed")]
    Closed,
    /// Underlying IO failure.
    #[error("transport IO failed: {0}")]
    Io(#[from] std::io::Error),
    /// HTTP-layer failure (request send, non-401 status, body read, ...).
    #[error("HTTP request failed: {0}")]
    Http(String),
    /// Protocol-layer failure (bad URL config, unparseable response,
    /// server-returned JSON-RPC error, unsupported interactive auth, ...).
    #[error("MCP protocol error: {0}")]
    Protocol(String),
    /// The streamable-HTTP server answered 404 for the established session
    /// (expired or terminated server-side): the session id was dropped and
    /// the caller re-initializes, then retries the request once.
    #[error("MCP session expired (server returned 404); re-initialize")]
    SessionExpired,
}

/// Apply the child environment for one stdio server spawn: inherit this
/// process' environment, remove every `strip_env` name, then overlay the
/// configured `env` entries (applied last, so a config entry deliberately
/// wins over the strip — a server that genuinely needs a secret-shaped
/// variable re-declares it through its config block).
pub fn apply_child_env(
    cmd: &mut std::process::Command,
    env: &HashMap<String, String>,
    strip_env: &[String],
) {
    for name in strip_env {
        cmd.env_remove(name);
    }
    for (key, value) in env {
        cmd.env(key, value);
    }
}

/// Child-process MCP transport over stdio pipes.
#[derive(Debug)]
pub struct ChildTransport {
    stdin: tokio::process::ChildStdin,
    lines: tokio::io::Lines<BufReader<tokio::process::ChildStdout>>,
    next_id: u64,
    timeout_secs: u64,
    command: String,
    child: tokio::process::Child,
}

impl ChildTransport {
    /// Spawn `command args` with piped stdio for JSON-RPC exchange.
    pub async fn spawn(
        command: impl Into<String>,
        args: Vec<String>,
        timeout_secs: u64,
    ) -> Result<Self, TransportError> {
        Self::spawn_with_env(command, args, &HashMap::new(), &[], timeout_secs).await
    }

    /// Spawn with extra environment variables over the inherited set.
    ///
    /// **Append, not replace**: `env` entries are added on top of this
    /// process' inherited environment (a duplicate key in `env` wins),
    /// mirroring `tokio::process::Command::envs` and the
    /// `McpServerConfig::Stdio.env` field semantics. `strip_env` names are
    /// removed from the child environment before `env` is applied, so a
    /// config `env` entry deliberately wins over the strip: callers pass
    /// the sensitive names they want out of the child (the bridge strips
    /// the sensitive-shaped parent variables) and re-declare any the
    /// server genuinely needs through its configured `env` block.
    ///
    /// The child dies with the transport (`kill_on_drop`): a failed
    /// handshake never leaks a server process behind a dropped handle.
    pub async fn spawn_with_env(
        command: impl Into<String>,
        args: Vec<String>,
        env: &HashMap<String, String>,
        strip_env: &[String],
        timeout_secs: u64,
    ) -> Result<Self, TransportError> {
        let command = command.into();
        let mut cmd = tokio::process::Command::new(&command);
        cmd.args(&args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true);
        apply_child_env(cmd.as_std_mut(), env, strip_env);
        let mut child = cmd.spawn().map_err(|e| TransportError::Spawn {
            command: command.clone(),
            cause: e.to_string(),
        })?;
        let stdin = child.stdin.take().expect("piped stdin just requested");
        let stdout = child.stdout.take().expect("piped stdout just requested");
        Ok(Self {
            stdin,
            lines: BufReader::new(stdout).lines(),
            next_id: FIRST_REQUEST_ID,
            timeout_secs,
            command,
            child,
        })
    }

    /// Command this transport was spawned with, for diagnostics.
    pub fn command(&self) -> &str {
        &self.command
    }

    /// Send one request, returning its correlation id.
    ///
    /// The write is bounded by the same timeout as the response read: a
    /// server that wedged without reading stdin fills the pipe buffer, and
    /// an unbounded `write_all` would hang the caller (and with it the whole
    /// turn) forever instead of surfacing a timeout the bridge can heal from.
    pub async fn send_request(
        &mut self,
        method: impl Into<String>,
        params: serde_json::Value,
    ) -> Result<u64, TransportError> {
        let id = self.next_id;
        self.next_id = next_id_after(id);
        let request = JsonRpcRequest {
            id,
            method: method.into(),
            params,
        };
        let line = request.encode();
        self.bounded_write(line.as_bytes()).await?;
        self.bounded_write(b"\n").await?;
        Ok(id)
    }

    /// One timeout-bounded write plus flush of `payload`.
    async fn bounded_write(&mut self, payload: &[u8]) -> Result<(), TransportError> {
        let write = async {
            self.stdin.write_all(payload).await?;
            self.stdin.flush().await?;
            Ok(())
        };
        tokio::time::timeout(std::time::Duration::from_secs(self.timeout_secs), write)
            .await
            .map_err(|_| TransportError::Timeout(self.timeout_secs))?
    }

    /// Send a JSON-RPC notification (no id, no response expected).
    ///
    /// Used for `notifications/initialized`, which the protocol requires
    /// after `initialize` and which must not carry a request id. Timeout
    /// semantics match [`ChildTransport::send_request`].
    pub async fn send_notification(
        &mut self,
        method: impl Into<String>,
        params: serde_json::Value,
    ) -> Result<(), TransportError> {
        let line = serde_json::json!({
            "jsonrpc": "2.0",
            "method": method.into(),
            "params": params,
        })
        .to_string();
        self.bounded_write(line.as_bytes()).await?;
        self.bounded_write(b"\n").await?;
        Ok(())
    }

    /// One timeout-bounded line read; EOF maps to [`TransportError::Closed`].
    async fn next_line_bounded(&mut self) -> Result<String, TransportError> {
        tokio::time::timeout(
            std::time::Duration::from_secs(self.timeout_secs),
            self.lines.next_line(),
        )
        .await
        .map_err(|_| TransportError::Timeout(self.timeout_secs))?
        .map_err(TransportError::Io)?
        .ok_or(TransportError::Closed)
    }

    /// Read the next response line with the configured timeout.
    pub async fn recv_response(&mut self) -> Result<JsonRpcResponse, TransportError> {
        let line = self.next_line_bounded().await?;
        decode_response(&line)
    }

    /// Read the next inbound line and classify it: `Ok(None)` is a
    /// server-initiated notification (the caller skips it and keeps
    /// waiting for its response), `Ok(Some)` a response-shaped frame,
    /// `Err` a transport failure or a malformed frame. Malformed frames
    /// still fail the exchange — only well-formed notifications skip.
    pub async fn recv_message(&mut self) -> Result<Option<JsonRpcResponse>, TransportError> {
        let line = self.next_line_bounded().await?;
        Ok(match decode_message(&line)? {
            JsonRpcMessage::Response(response) => Some(response),
            JsonRpcMessage::Notification => None,
        })
    }

    /// Kill the child process; safe to call after natural exits.
    pub async fn shutdown(mut self) -> std::io::Result<()> {
        self.child.kill().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_lines_round_trip_through_decode() {
        let request = JsonRpcRequest {
            id: 7,
            method: "tools/call".to_string(),
            params: serde_json::json!({"name": "x"}),
        };
        let line = request.encode();
        assert!(!line.contains('\n'));
        let response =
            decode_response("{\"jsonrpc\":\"2.0\",\"id\":7,\"result\":{\"ok\":true}}").unwrap();
        assert_eq!(response.id, 7);
        assert_eq!(response.payload, serde_json::json!({"ok": true}));
    }

    #[test]
    fn malformed_frames_fail_explicitly() {
        assert!(matches!(
            decode_response("not json"),
            Err(TransportError::BadFrame(_))
        ));
        assert!(matches!(
            decode_response("{\"jsonrpc\":\"2.0\"}"),
            Err(TransportError::BadFrame(_))
        ));
    }

    #[test]
    fn decode_rejects_non_2_0_frames() {
        assert!(matches!(
            decode_response("{\"jsonrpc\":\"1.0\",\"id\":1,\"result\":{}}"),
            Err(TransportError::BadFrame(_))
        ));
        assert!(matches!(
            decode_response("{\"id\":1,\"result\":{}}"),
            Err(TransportError::BadFrame(_))
        ));
    }

    #[test]
    fn decode_prefers_error_over_result() {
        let response = decode_response(
            "{\"jsonrpc\":\"2.0\",\"id\":3,\"result\":{\"ok\":true},\"error\":{\"code\":-1,\"message\":\"boom\"}}",
        )
        .unwrap();
        assert_eq!(response.id, 3);
        assert_eq!(
            response.payload,
            serde_json::json!({"code": -1, "message": "boom"})
        );
    }

    #[test]
    fn decode_accepts_error_frames_and_rejects_bad_shapes() {
        let response = decode_response(
            "{\"jsonrpc\":\"2.0\",\"id\":4,\"error\":{\"code\":0,\"message\":\"x\"}}",
        )
        .unwrap();
        assert_eq!(response.id, 4);
        assert_eq!(
            response.payload,
            serde_json::json!({"code": 0, "message": "x"})
        );
        assert!(matches!(
            decode_response("[1, 2, 3]"),
            Err(TransportError::BadFrame(_))
        ));
        assert!(matches!(
            decode_response("{\"jsonrpc\":\"2.0\",\"id\":\"7\",\"result\":{}}"),
            Err(TransportError::BadFrame(_))
        ));
    }

    #[test]
    fn request_ids_wrap_without_zero() {
        assert_eq!(next_id_after(1), 2);
        assert_eq!(next_id_after(u64::MAX), FIRST_REQUEST_ID);
        assert_ne!(next_id_after(u64::MAX), 0);
    }

    /// Message classification: id-bearing frames are responses, id-less
    /// frames naming a method are notifications (skippable chatter), and
    /// everything id-less without a method stays malformed.
    #[test]
    fn decode_message_classifies_notifications() {
        assert!(matches!(
            decode_message("{\"jsonrpc\":\"2.0\",\"method\":\"notifications/message\"}"),
            Ok(JsonRpcMessage::Notification)
        ));
        // A null id still reads as absent: some servers send it.
        assert!(matches!(
            decode_message("{\"jsonrpc\":\"2.0\",\"id\":null,\"method\":\"x\"}"),
            Ok(JsonRpcMessage::Notification)
        ));
        let decoded =
            decode_message("{\"jsonrpc\":\"2.0\",\"id\":7,\"result\":{\"ok\":true}}").unwrap();
        assert_eq!(
            decoded,
            JsonRpcMessage::Response(JsonRpcResponse {
                id: 7,
                payload: serde_json::json!({"ok": true}),
            })
        );
        // decode_response keeps its historic contract: notifications and
        // malformed frames are bad frames there.
        assert!(decode_response("{\"jsonrpc\":\"2.0\",\"method\":\"x\"}").is_err());
        assert!(decode_message("{\"jsonrpc\":\"2.0\"}").is_err());
        assert!(decode_message("not json").is_err());
        assert!(decode_message("{\"jsonrpc\":\"1.0\",\"method\":\"x\"}").is_err());
    }

    /// Over a real child: a well-formed notification line is skipped (not
    /// a BadFrame aborting the exchange) and the following response line
    /// answers. Skipped silently where the platform refuses the spawn or
    /// the temp path carries a space the shell commands cannot quote.
    #[tokio::test]
    async fn recv_message_skips_notifications_over_a_real_child() {
        let dir = std::env::temp_dir().join(format!("wavecode-mcp-recv-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("lines.txt");
        std::fs::write(
            &script,
            concat!(
                "{\"jsonrpc\":\"2.0\",\"method\":\"notifications/message\",\"params\":{}}\n",
                "{\"jsonrpc\":\"2.0\",\"id\":7,\"result\":{\"ok\":true}}\n",
            ),
        )
        .unwrap();
        #[cfg(windows)]
        if dir.to_string_lossy().contains(' ') {
            let _ = std::fs::remove_dir_all(&dir);
            eprintln!("temp path contains a space; skipping the recv_message child test");
            return;
        }
        let (program, args): (&str, Vec<String>) = if cfg!(windows) {
            (
                "cmd",
                vec!["/C".into(), format!("type {}", script.display())],
            )
        } else {
            (
                "sh",
                vec!["-c".into(), format!("cat '{}'", script.display())],
            )
        };
        let mut transport = match ChildTransport::spawn(program, args, 10).await {
            Ok(transport) => transport,
            Err(_) => {
                let _ = std::fs::remove_dir_all(&dir);
                eprintln!("child spawn refused; skipping the recv_message child test");
                return;
            }
        };
        // The notification is classified and skipped, never a BadFrame.
        assert!(matches!(transport.recv_message().await, Ok(None)));
        let response = transport
            .recv_message()
            .await
            .unwrap()
            .expect("the response line follows the notification");
        assert_eq!(response.id, 7);
        assert_eq!(response.payload["ok"], true);
        // Script output exhausted: a closed transport, not a hang.
        assert!(matches!(
            transport.recv_message().await,
            Err(TransportError::Closed)
        ));
        let _ = transport.shutdown().await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn spawn_failures_name_the_command() {
        let err = ChildTransport::spawn("wavecode-definitely-missing-binary-xyz", vec![], 1)
            .await
            .unwrap_err();
        assert!(matches!(err, TransportError::Spawn { .. }));
        assert!(
            err.to_string()
                .contains("wavecode-definitely-missing-binary-xyz")
        );
    }

    /// Serializes env mutation in this binary against itself.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// `apply_child_env` strips sensitive inherited variables while the
    /// configured `env` block (applied after the strip) survives — live
    /// over a real child so the spawn semantics are pinned, not assumed.
    #[test]
    fn apply_child_env_strips_inherited_and_keeps_config() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        unsafe {
            std::env::set_var("FOO_MCP_TOKEN", "mcp-token-value");
            std::env::set_var("FOO_MCP_SECRET", "mcp-parent-secret");
            std::env::set_var("FOO_MCP_KEEP", "mcp-keep-value");
        }
        let mut cmd = if cfg!(windows) {
            let mut c = std::process::Command::new("cmd");
            c.args([
                "/C",
                "echo %FOO_MCP_TOKEN% & echo %FOO_MCP_SECRET% & echo %FOO_MCP_KEEP%",
            ]);
            c
        } else {
            let mut c = std::process::Command::new("sh");
            c.args([
                "-c",
                "echo \"$FOO_MCP_TOKEN\"; echo \"$FOO_MCP_SECRET\"; echo \"$FOO_MCP_KEEP\"",
            ]);
            c
        };
        // The config block re-declares FOO_MCP_SECRET on purpose: config
        // wins over the strip (the server deliberately asked for it).
        let mut env = HashMap::new();
        env.insert("FOO_MCP_SECRET".to_owned(), "mcp-cfg-wins-value".to_owned());
        apply_child_env(&mut cmd, &env, &["FOO_MCP_TOKEN".to_owned()]);
        let out = cmd.output().expect("child runs");
        let text = String::from_utf8_lossy(&out.stdout);
        unsafe {
            std::env::remove_var("FOO_MCP_TOKEN");
            std::env::remove_var("FOO_MCP_SECRET");
            std::env::remove_var("FOO_MCP_KEEP");
        }
        // The inherited sensitive-shaped variable never reaches the child.
        assert!(
            !text.contains("mcp-token-value"),
            "sensitive variable leaked to the child: {text}"
        );
        // The config block wins over the strip.
        assert!(
            text.contains("mcp-cfg-wins-value"),
            "config env entry must survive the strip: {text}"
        );
        assert!(
            !text.contains("mcp-parent-secret"),
            "config override must replace, not append: {text}"
        );
        // Untouched inherited variables stay visible.
        assert!(
            text.contains("mcp-keep-value"),
            "normal inherited variable was over-stripped: {text}"
        );
    }

    /// End to end through [`ChildTransport::spawn_with_env`]: the strip list
    /// really reaches the spawned child. The child echoes the variables as
    /// non-JSON output, which surfaces as a `BadFrame` carrying the text —
    /// the keep value proves the echo pipeline sees values, so the missing
    /// token value is the strip working, not a vacuous pass.
    #[tokio::test]
    async fn spawn_with_env_strips_the_inherited_environment() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        unsafe {
            std::env::set_var("FOO_MCP_E2E_TOKEN", "e2e-token-value");
            std::env::set_var("FOO_MCP_E2E_KEEP", "e2e-keep-value");
        }
        let (program, args): (&str, Vec<String>) = if cfg!(windows) {
            (
                "cmd",
                vec![
                    "/C".into(),
                    "echo T=%FOO_MCP_E2E_TOKEN% K=%FOO_MCP_E2E_KEEP%".into(),
                ],
            )
        } else {
            (
                "sh",
                vec![
                    "-c".into(),
                    "echo T=$FOO_MCP_E2E_TOKEN K=$FOO_MCP_E2E_KEEP".into(),
                ],
            )
        };
        let mut transport = match ChildTransport::spawn_with_env(
            program,
            args,
            &HashMap::new(),
            &["FOO_MCP_E2E_TOKEN".to_owned()],
            10,
        )
        .await
        {
            Ok(transport) => transport,
            Err(_) => {
                unsafe {
                    std::env::remove_var("FOO_MCP_E2E_TOKEN");
                    std::env::remove_var("FOO_MCP_E2E_KEEP");
                }
                eprintln!("child spawn refused; skipping the spawn_with_env strip test");
                return;
            }
        };
        transport
            .send_request("ping", serde_json::Value::Null)
            .await
            .unwrap();
        let err = transport.recv_response().await.unwrap_err();
        unsafe {
            std::env::remove_var("FOO_MCP_E2E_TOKEN");
            std::env::remove_var("FOO_MCP_E2E_KEEP");
        }
        let TransportError::BadFrame(line) = err else {
            panic!("echo output must surface as a BadFrame: {err:?}");
        };
        assert!(
            !line.contains("e2e-token-value"),
            "stripped variable leaked into the child: {line}"
        );
        assert!(
            line.contains("e2e-keep-value"),
            "untouched variable must stay visible: {line}"
        );
    }

    #[tokio::test]
    async fn duplex_pair_exchanges_frames() {
        // Transport logic over memory pipes: no child process needed.
        let (client_read, server_write) = tokio::io::duplex(1024);
        let (server_read, mut client_write) = tokio::io::duplex(1024);
        let request = JsonRpcRequest {
            id: 1,
            method: "ping".to_string(),
            params: serde_json::Value::Null,
        };
        client_write
            .write_all(request.encode().as_bytes())
            .await
            .unwrap();
        client_write.write_all(b"\n").await.unwrap();
        let mut lines = BufReader::new(server_read).lines();
        let got = lines.next_line().await.unwrap().unwrap();
        assert_eq!(got, request.encode());
        let _ = client_read;
        let mut server_write = server_write;
        server_write
            .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}\n")
            .await
            .unwrap();
        drop(server_write);
    }

    /// Spawn a live child that never reads stdin and never writes stdout
    /// (`waitfor` on Windows, `sleep` elsewhere): it stays alive for the
    /// whole test without touching either pipe. `signal` must be unique per
    /// test — same-name waiters on one machine disturb each other. Dropped
    /// transports kill the child via `kill_on_drop`, so nothing leaks past
    /// the test.
    async fn spawn_wedged_server(signal: &str, timeout_secs: u64) -> ChildTransport {
        let (command, args): (&str, Vec<String>) = if cfg!(windows) {
            (
                "waitfor",
                vec!["/t".into(), "30".into(), signal.to_string()],
            )
        } else {
            ("sleep", vec!["30".into()])
        };
        ChildTransport::spawn(command, args, timeout_secs)
            .await
            .expect("wedge child spawns")
    }

    /// A server that stopped reading stdin wedges `write_all` once the pipe
    /// buffer fills: `send_request` must surface the bounded-write timeout
    /// instead of hanging the caller (and with it the whole turn) forever.
    #[tokio::test]
    async fn write_times_out_when_the_server_stops_reading_stdin() {
        let mut transport = spawn_wedged_server("WaveCodeNeverWrite", 1).await;
        // Far larger than any OS pipe buffer, so the unbounded write pends.
        let params = serde_json::json!({"pad": "x".repeat(4 * 1024 * 1024)});
        let started = std::time::Instant::now();
        let outcome = transport.send_request("tools/call", params).await;
        let elapsed = started.elapsed();
        assert!(
            matches!(outcome, Err(TransportError::Timeout(1))),
            "expected the bounded write to time out: {outcome:?}"
        );
        assert!(
            elapsed >= std::time::Duration::from_millis(900),
            "timeout fired before its bound: {elapsed:?}"
        );
    }

    /// A server that never answers wedges the response read: `recv_response`
    /// must surface its timeout instead of blocking the caller forever.
    #[tokio::test]
    async fn read_times_out_when_the_server_never_answers() {
        let mut transport = spawn_wedged_server("WaveCodeNeverRead", 1).await;
        // A small request fits every pipe buffer, so the write succeeds and
        // the timeout can only come from the read side.
        transport
            .send_request("ping", serde_json::Value::Null)
            .await
            .unwrap();
        let started = std::time::Instant::now();
        let outcome = transport.recv_response().await;
        let elapsed = started.elapsed();
        assert!(
            matches!(outcome, Err(TransportError::Timeout(1))),
            "expected the response read to time out: {outcome:?}"
        );
        assert!(
            elapsed >= std::time::Duration::from_millis(900),
            "timeout fired before its bound: {elapsed:?}"
        );
    }
}
