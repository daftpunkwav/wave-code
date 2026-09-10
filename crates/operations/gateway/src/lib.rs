/*!
 * @file RpcGateway
 * @description NDJSON JSON-RPC 2.0 gateway over the session actor.
 *
 * Responsibilities:
 * - Serve session/submit, session/poll, and session/shutdown methods.
 * - Echo request ids and emit standard JSON-RPC error codes.
 * - Stay transport-shaped: any async line streams work as wires.
 *
 * This module must not depend on: concrete drivers, tools, or models.
 */

//! Gateway: remote control for the session actor over plain JSON-RPC.
//!
//! Event retrieval is poll-based by design: clients drain `session/poll`
//! at their own pace, so slow frontends apply their own backpressure
//! instead of stalling the turn loop.

use std::sync::Arc;

use operations_actor::ActorClient;
use operations_wire::{Op, Submission};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt};

/// Method names served by the gateway.
pub const METHOD_SUBMIT: &str = "session/submit";
/// Poll method name.
pub const METHOD_POLL: &str = "session/poll";
/// Shutdown method name.
pub const METHOD_SHUTDOWN: &str = "session/shutdown";

/// Default events returned per poll call.
pub const DEFAULT_POLL_MAX: usize = 32;

/// Gateway errors (transport-level; protocol errors ride as responses).
#[derive(Debug, thiserror::Error)]
pub enum GatewayError {
    /// Underlying IO failure on either stream.
    #[error("gateway IO failed: {0}")]
    Io(#[from] std::io::Error),
    /// A response failed to serialize (caller bug, not client input).
    #[error("response serialization failed: {0}")]
    Encode(String),
}

/// Gateway over one session actor client.
pub struct Gateway {
    client: tokio::sync::Mutex<ActorClient>,
}

impl Gateway {
    /// Wrap a live actor client.
    pub fn new(client: ActorClient) -> Self {
        Self {
            client: tokio::sync::Mutex::new(client),
        }
    }

    /// Serve requests until the input closes or shutdown is requested.
    pub async fn serve<R, W>(self: &Arc<Self>, reader: R, writer: W) -> Result<(), GatewayError>
    where
        R: AsyncBufRead + Unpin,
        W: AsyncWrite + Unpin,
    {
        let mut lines = tokio::io::BufReader::new(reader).lines();
        let mut writer = tokio::io::BufWriter::new(writer);
        loop {
            let line = match lines.next_line().await? {
                Some(line) => line,
                None => break,
            };
            if line.trim().is_empty() {
                continue;
            }
            let (response, exit) = self.handle_line(&line).await;
            let mut text = serde_json::to_string(&response)
                .map_err(|e| GatewayError::Encode(e.to_string()))?;
            text.push('\n');
            writer.write_all(text.as_bytes()).await?;
            writer.flush().await?;
            if exit {
                break;
            }
        }
        Ok(())
    }

    /// Handle one request line, returning the response and an exit flag.
    async fn handle_line(&self, line: &str) -> (serde_json::Value, bool) {
        let request: serde_json::Value = match serde_json::from_str(line) {
            Ok(request) => request,
            Err(_) => {
                return (
                    rpc_error(serde_json::Value::Null, -32700, "parse error"),
                    false,
                );
            }
        };
        let id = request
            .get("id")
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        let method = request.get("method").and_then(|m| m.as_str()).unwrap_or("");
        let params = request
            .get("params")
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        match method {
            METHOD_SUBMIT => {
                let submission: Result<Submission, _> = serde_json::from_value(params);
                match submission {
                    Ok(submission) => match self.client.lock().await.submit(submission).await {
                        Ok(()) => (rpc_ok(&id, serde_json::json!({"accepted": true})), false),
                        Err(e) => (rpc_error(id, -32000, &e.to_string()), false),
                    },
                    Err(_) => (rpc_error(id, -32602, "invalid submission params"), false),
                }
            }
            METHOD_POLL => {
                let max = params
                    .get("max")
                    .and_then(|m| m.as_u64())
                    .unwrap_or(DEFAULT_POLL_MAX as u64) as usize;
                let mut events = Vec::new();
                {
                    let mut client = self.client.lock().await;
                    while events.len() < max {
                        match client.try_poll() {
                            Some(event) => events.push(event),
                            None => break,
                        }
                    }
                }
                (rpc_ok(&id, serde_json::json!({"events": events})), false)
            }
            METHOD_SHUTDOWN => {
                let shutdown = Submission {
                    id: "gateway-shutdown".to_string(),
                    op: Op::Shutdown,
                };
                let _ = self.client.lock().await.submit(shutdown).await;
                (rpc_ok(&id, serde_json::json!({"ok": true})), true)
            }
            _ => (rpc_error(id, -32601, "unknown method"), false),
        }
    }
}

/// Build a success response echoing the request id.
fn rpc_ok(id: &serde_json::Value, result: serde_json::Value) -> serde_json::Value {
    serde_json::json!({"jsonrpc": "2.0", "id": id, "result": result})
}

/// Build an error response echoing the request id.
fn rpc_error(id: serde_json::Value, code: i64, message: &str) -> serde_json::Value {
    serde_json::json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

#[cfg(test)]
mod tests {
    use super::*;
    use operations_actor::SessionActor;
    use operations_wire::Event;
    use operations_wire::EventMsg;
    use runtime_child::ChildRuntime;
    use runtime_runner::{HookPoint, RunContext, StopReason, TurnDriver};
    use safety_gate::ApprovalGate;
    use state_store::{CompactTrigger, Conversation};
    use std::sync::Arc;
    use tokio::io::duplex;

    struct EchoDriver;

    #[async_trait::async_trait]
    impl TurnDriver for EchoDriver {
        async fn drive_turn(
            &self,
            _ctx: &RunContext,
            _conv: &mut Conversation,
            _input: &str,
            _system: &str,
            on_event: &(dyn Fn(Event) + Send + Sync),
        ) -> StopReason {
            on_event(Event {
                id: "s1".to_string(),
                msg: EventMsg::TurnStarted,
            });
            on_event(Event {
                id: "s1".to_string(),
                msg: EventMsg::TurnCompleted,
            });
            StopReason::Completed
        }

        async fn drive_compact(
            &self,
            _conv: &mut Conversation,
            _trigger: CompactTrigger,
            _on_event: &(dyn Fn(Event) + Send + Sync),
        ) -> Result<(), String> {
            Ok(())
        }

        async fn drive_hook(
            &self,
            _point: HookPoint,
            _payload: &str,
            _on_event: &(dyn Fn(Event) + Send + Sync),
        ) -> bool {
            true
        }
    }

    async fn gateway() -> (
        tokio::io::WriteHalf<tokio::io::DuplexStream>,
        tokio::io::BufReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>,
    ) {
        let client = SessionActor::spawn(
            EchoDriver,
            Conversation::new(),
            Arc::new(ChildRuntime::new()),
            Arc::new(ApprovalGate::new()),
            infrastructure_base::InterruptHandle::new(),
            "sys".to_string(),
        );
        let gateway = Arc::new(Gateway::new(client));
        let (client_io, server_io) = duplex(64 * 1024);
        let (server_read, server_write) = tokio::io::split(server_io);
        let server_read = tokio::io::BufReader::new(server_read);
        tokio::spawn(async move {
            let _ = gateway.serve(server_read, server_write).await;
        });
        let (read_half, write_half) = tokio::io::split(client_io);
        (write_half, tokio::io::BufReader::new(read_half))
    }

    async fn rpc(
        writer: &mut tokio::io::WriteHalf<tokio::io::DuplexStream>,
        reader: &mut tokio::io::BufReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>,
        id: u64,
        method: &str,
        params: serde_json::Value,
    ) -> serde_json::Value {
        let line = serde_json::json!({
            "jsonrpc": "2.0", "id": id, "method": method, "params": params,
        })
        .to_string()
            + "\n";
        writer.write_all(line.as_bytes()).await.unwrap();
        writer.flush().await.unwrap();
        let response = reader.lines().next_line().await.unwrap().unwrap();
        serde_json::from_str(&response).unwrap()
    }

    #[tokio::test]
    async fn submit_then_poll_returns_turn_events() {
        let (mut writer, mut reader) = gateway().await;
        let ack = rpc(
            &mut writer,
            &mut reader,
            1,
            METHOD_SUBMIT,
            serde_json::json!({"id": "s1", "type": "user_input", "text": "hi"}),
        )
        .await;
        assert_eq!(ack["result"]["accepted"], true);
        // Poll until the turn completes.
        let mut kinds = Vec::new();
        for _ in 0..20 {
            let polled = rpc(
                &mut writer,
                &mut reader,
                2,
                METHOD_POLL,
                serde_json::json!({}),
            )
            .await;
            for event in polled["result"]["events"].as_array().unwrap() {
                kinds.push(event["type"].as_str().unwrap().to_string());
            }
            if kinds.contains(&"turn_completed".to_string()) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert_eq!(kinds, vec!["turn_started", "turn_completed"]);
    }

    #[tokio::test]
    async fn unknown_methods_and_garbage_get_error_codes() {
        let (mut writer, mut reader) = gateway().await;
        let unknown = rpc(
            &mut writer,
            &mut reader,
            1,
            "nope/method",
            serde_json::json!({}),
        )
        .await;
        assert_eq!(unknown["error"]["code"], -32601);
        assert_eq!(unknown["id"], 1);
        writer.write_all(b"not json\n").await.unwrap();
        writer.flush().await.unwrap();
        let response = reader.lines().next_line().await.unwrap().unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&response).unwrap();
        assert_eq!(parsed["error"]["code"], -32700);
    }

    #[tokio::test]
    async fn shutdown_acks_and_ends_the_stream() {
        let (mut writer, mut reader) = gateway().await;
        let ack = rpc(
            &mut writer,
            &mut reader,
            9,
            METHOD_SHUTDOWN,
            serde_json::json!({}),
        )
        .await;
        assert_eq!(ack["result"]["ok"], true);
        // The server closed after responding: no further lines arrive.
        let end = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            reader.lines().next_line(),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(end.is_none());
    }
}
