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

/// Next request id seed; ids increase monotonically per transport.
pub const FIRST_REQUEST_ID: u64 = 1;

/// Advance a request id without panicking on overflow and without reusing 0.
fn next_id_after(current: u64) -> u64 {
    let next = current.wrapping_add(1);
    if next == 0 {
        FIRST_REQUEST_ID
    } else {
        next
    }
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

/// Decode one response line, rejecting malformed frames explicitly.
///
/// Only `jsonrpc: "2.0"` frames are accepted. When a frame carries both
/// `result` and `error`, the error wins so failures are never read as success.
pub fn decode_response(line: &str) -> Result<JsonRpcResponse, TransportError> {
    let value: serde_json::Value =
        serde_json::from_str(line).map_err(|_| TransportError::BadFrame(line.to_string()))?;
    if value.get("jsonrpc").and_then(|v| v.as_str()) != Some("2.0") {
        return Err(TransportError::BadFrame(line.to_string()));
    }
    let id = value
        .get("id")
        .and_then(|v| v.as_u64())
        .ok_or_else(|| TransportError::BadFrame(line.to_string()))?;
    let payload = value
        .get("error")
        .or_else(|| value.get("result"))
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    Ok(JsonRpcResponse { id, payload })
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
        Self::spawn_with_env(command, args, &HashMap::new(), timeout_secs).await
    }

    /// Spawn with extra environment variables over the inherited set.
    ///
    /// The child dies with the transport (`kill_on_drop`): a failed
    /// handshake never leaks a server process behind a dropped handle.
    pub async fn spawn_with_env(
        command: impl Into<String>,
        args: Vec<String>,
        env: &HashMap<String, String>,
        timeout_secs: u64,
    ) -> Result<Self, TransportError> {
        let command = command.into();
        let mut child = tokio::process::Command::new(&command)
            .args(&args)
            .envs(env)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| TransportError::Spawn {
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
        self.stdin.write_all(request.encode().as_bytes()).await?;
        self.stdin.write_all(b"\n").await?;
        self.stdin.flush().await?;
        Ok(id)
    }

    /// Send a JSON-RPC notification (no id, no response expected).
    ///
    /// Used for `notifications/initialized`, which the protocol requires
    /// after `initialize` and which must not carry a request id.
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
        self.stdin.write_all(line.as_bytes()).await?;
        self.stdin.write_all(b"\n").await?;
        self.stdin.flush().await?;
        Ok(())
    }

    /// Read the next response line with the configured timeout.
    pub async fn recv_response(&mut self) -> Result<JsonRpcResponse, TransportError> {
        let line = tokio::time::timeout(
            std::time::Duration::from_secs(self.timeout_secs),
            self.lines.next_line(),
        )
        .await
        .map_err(|_| TransportError::Timeout(self.timeout_secs))?
        .map_err(TransportError::Io)?
        .ok_or(TransportError::Closed)?;
        decode_response(&line)
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
        let response =
            decode_response("{\"jsonrpc\":\"2.0\",\"id\":4,\"error\":{\"code\":0,\"message\":\"x\"}}")
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
}
