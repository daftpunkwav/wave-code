/*!
 * @file McpServe
 * @description MCP server mode: expose a tool executor over stdio.
 *
 * Responsibilities:
 * - Serve NDJSON JSON-RPC over stdin/stdout (initialize, tools/list,
 *   tools/call, ping, notifications).
 * - Execute tool calls through a caller-supplied `ToolExecutor`.
 * - Map protocol failures to JSON-RPC error codes; shut down on EOF.
 *
 * This module must not depend on: drivers, actors, or sessions. The
 * registry and its executor are caller-built (the composition root); this
 * is a thin serving skin over them.
 */

//! MCP server mode: WaveCode tools over stdio.
//!
//! Transport framing matches `transport-mcp` (plain NDJSON lines, one
//! JSON-RPC message each). The loop handles `initialize` (replying
//! protocol version `2024-11-05` plus `wavecode` server info),
//! `notifications/initialized` (no reply), `tools/list` (from the
//! registry specs), `tools/call` (dispatched through the caller's
//! [`ToolExecutor`]), `ping`, and unknown notifications (ignored).
//! Shutdown happens on EOF (or stdio close); there is no sentinel
//! method.

use std::sync::Arc;

use runtime_runner::{ToolCall, ToolExecutor};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt};

/// Adapter fault marker: the composition root's executor converts
/// implementation faults into error results prefixed this way (so
/// transcripts stay distinguishable from business failures); the serve
/// loop maps that documented prefix back to a protocol-level internal
/// error.
const FAULT_PREFIX: &str = "tool fault:";

/// Protocol version advertised (and expected) at `initialize`.
pub const MCP_PROTOCOL_VERSION: &str = "2024-11-05";
/// Server name reported in `serverInfo`.
pub const SERVER_NAME: &str = "wavecode";

/// JSON-RPC error codes served by this loop.
const PARSE_ERROR: i32 = -32700;
const INVALID_REQUEST: i32 = -32600;
const METHOD_NOT_FOUND: i32 = -32601;
const INVALID_PARAMS: i32 = -32602;
const INTERNAL_ERROR: i32 = -32603;

/// Serve the registry over the process stdio streams until EOF.
///
/// Executes tool calls through the caller-supplied `executor` and pumps
/// stdin-to-stdout; returns when stdin closes.
pub async fn run_stdio_server<E: ToolExecutor>(
    registry: Arc<wavecode_tools::Registry>,
    executor: E,
) -> std::io::Result<()> {
    let reader = tokio::io::BufReader::new(tokio::io::stdin());
    let writer = tokio::io::stdout();
    serve_loop(&registry, &executor, reader, writer).await
}

/// Serve loop over explicit streams (the duplex-testable core behind
/// [`run_stdio_server`]).
async fn serve_loop<R, W, E: ToolExecutor>(
    registry: &wavecode_tools::Registry,
    executor: &E,
    reader: R,
    writer: W,
) -> std::io::Result<()>
where
    R: AsyncBufRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut reader = reader;
    let mut writer = writer;
    let mut line = String::new();
    loop {
        line.clear();
        let read = reader.read_line(&mut line).await?;
        if read == 0 {
            // EOF (or stdio close): clean shutdown, no sentinel needed.
            return Ok(());
        }
        if line.trim().is_empty() {
            continue;
        }
        if let Some(response) = handle_line(&line, registry, executor).await {
            writer.write_all(response.to_string().as_bytes()).await?;
            writer.write_all(b"\n").await?;
            writer.flush().await?;
        }
    }
}

/// Handle one NDJSON line; `None` means notification (no reply).
async fn handle_line<E: ToolExecutor>(
    line: &str,
    registry: &wavecode_tools::Registry,
    executor: &E,
) -> Option<serde_json::Value> {
    let message: serde_json::Value = match serde_json::from_str(line) {
        Ok(value) => value,
        Err(_) => {
            return Some(error_response(
                serde_json::Value::Null,
                PARSE_ERROR,
                "parse error: request line is not valid JSON",
            ));
        }
    };
    let object = match message.as_object() {
        Some(object) => object,
        None => {
            // Batches (arrays) and scalar frames are valid JSON but never
            // valid requests here: no id can carry a reply.
            return Some(error_response(
                serde_json::Value::Null,
                INVALID_REQUEST,
                "invalid request: expected a JSON-RPC object",
            ));
        }
    };
    if object.get("jsonrpc").and_then(|v| v.as_str()) != Some("2.0") {
        return Some(error_response(
            id_or_null(object),
            INVALID_REQUEST,
            "invalid request: jsonrpc must be \"2.0\"",
        ));
    }
    let method = match object.get("method").and_then(|v| v.as_str()) {
        Some(method) => method,
        None => {
            return Some(error_response(
                id_or_null(object),
                INVALID_REQUEST,
                "invalid request: missing method",
            ));
        }
    };
    let id = id_or_null(object);
    if id.is_null() {
        // Notifications never reply: `notifications/initialized` is the
        // expected one, anything else is ignored just as quietly.
        return None;
    }
    let params = object
        .get("params")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    match method {
        "initialize" => Some(success_response(id, initialize_result())),
        "ping" => Some(success_response(id, serde_json::json!({}))),
        "tools/list" => Some(success_response(id, tools_list(registry))),
        "tools/call" => Some(call_tool(id, &params, executor).await),
        _ => Some(error_response(
            id,
            METHOD_NOT_FOUND,
            format!("method not found: {method}"),
        )),
    }
}

/// Request id echoed back, or null when absent (notifications/errors).
fn id_or_null(object: &serde_json::Map<String, serde_json::Value>) -> serde_json::Value {
    object.get("id").cloned().unwrap_or(serde_json::Value::Null)
}

/// Success envelope for one request id.
fn success_response(id: serde_json::Value, result: serde_json::Value) -> serde_json::Value {
    serde_json::json!({"jsonrpc": "2.0", "id": id, "result": result})
}

/// Error envelope for one request id (null when no id can be echoed).
fn error_response(
    id: serde_json::Value,
    code: i32,
    message: impl Into<String>,
) -> serde_json::Value {
    serde_json::json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message.into()}})
}

/// `initialize` result: negotiated version plus server identity.
fn initialize_result() -> serde_json::Value {
    serde_json::json!({
        "protocolVersion": MCP_PROTOCOL_VERSION,
        "capabilities": {"tools": {}},
        "serverInfo": {"name": SERVER_NAME, "version": env!("CARGO_PKG_VERSION")},
    })
}

/// `tools/list` result from the registry specs (sorted by name there).
fn tools_list(registry: &wavecode_tools::Registry) -> serde_json::Value {
    let tools: Vec<serde_json::Value> = registry
        .specs()
        .into_iter()
        .map(|spec| {
            serde_json::json!({
                "name": spec.name,
                "description": spec.description,
                "inputSchema": spec.input_schema,
            })
        })
        .collect();
    serde_json::json!({"tools": tools})
}

/// `tools/call`: validate params, dispatch through the executor, and map
/// the outcome onto an MCP result (or a protocol error for faults).
async fn call_tool<E: ToolExecutor>(
    id: serde_json::Value,
    params: &serde_json::Value,
    executor: &E,
) -> serde_json::Value {
    let object = match params.as_object() {
        Some(object) => object,
        None => {
            return error_response(
                id,
                INVALID_PARAMS,
                "invalid params: tools/call params must be an object",
            );
        }
    };
    let Some(name) = object.get("name").and_then(|v| v.as_str()) else {
        return error_response(
            id,
            INVALID_PARAMS,
            "invalid params: tools/call requires a string name",
        );
    };
    let arguments = match object.get("arguments") {
        None | Some(serde_json::Value::Null) => serde_json::json!({}),
        Some(args) if args.is_object() => args.clone(),
        Some(_) => {
            return error_response(
                id,
                INVALID_PARAMS,
                "invalid params: tools/call arguments must be an object",
            );
        }
    };
    let outcome = executor
        .execute(ToolCall {
            call_id: format!("mcp-{id}"),
            name: name.to_string(),
            input: arguments,
        })
        .await;
    if let Some(detail) = outcome.content.strip_prefix(FAULT_PREFIX) {
        // Implementation faults are server-side failures, not tool
        // results: surface them as internal errors (see FAULT_PREFIX).
        return error_response(id, INTERNAL_ERROR, format!("tool fault:{detail}"));
    }
    // Unknown tools and business failures ride as results with
    // `isError`, matching the run-loop adapter semantics MCP clients
    // already handle.
    success_response(
        id,
        serde_json::json!({
            "content": [{"type": "text", "text": outcome.content}],
            "isError": outcome.is_error,
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use operations_bootstrap::ToolAdapter;
    use wavecode_tools::{Result as ToolResultAlias, Tool, ToolCtx, ToolOutput};

    struct EchoTool;

    #[async_trait::async_trait]
    impl Tool for EchoTool {
        fn name(&self) -> &str {
            "echo_tool"
        }
        fn description(&self) -> &str {
            "test echo tool"
        }
        fn input_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }
        fn is_read_only(&self) -> bool {
            true
        }
        async fn execute(
            &self,
            input: serde_json::Value,
            _ctx: &ToolCtx,
        ) -> ToolResultAlias<ToolOutput> {
            Ok(ToolOutput {
                content: input.to_string(),
                is_error: false,
            })
        }
    }

    struct FaultyTool;

    #[async_trait::async_trait]
    impl Tool for FaultyTool {
        fn name(&self) -> &str {
            "faulty_tool"
        }
        fn description(&self) -> &str {
            "test faulty tool"
        }
        fn input_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }
        fn is_read_only(&self) -> bool {
            true
        }
        async fn execute(
            &self,
            _input: serde_json::Value,
            _ctx: &ToolCtx,
        ) -> ToolResultAlias<ToolOutput> {
            Err(std::io::Error::other("disk gone").into())
        }
    }

    fn test_registry() -> Arc<wavecode_tools::Registry> {
        let registry = wavecode_tools::Registry::builtin();
        registry.register(Arc::new(EchoTool));
        registry.register(Arc::new(FaultyTool));
        Arc::new(registry)
    }

    fn test_ctx() -> ToolCtx {
        ToolCtx {
            cwd: std::path::PathBuf::from("/tmp"),
            deny_env: Vec::new(),
        }
    }

    /// One live serve loop over a duplex pair: the client half writes
    /// request lines and reads response lines.
    struct TestClient {
        write: tokio::io::DuplexStream,
        read: tokio::io::Lines<tokio::io::BufReader<tokio::io::DuplexStream>>,
    }

    impl TestClient {
        async fn start() -> (Self, tokio::task::JoinHandle<std::io::Result<()>>) {
            let registry = test_registry();
            let adapter = ToolAdapter::new(registry.clone(), test_ctx());
            let (client_write, server_read) = tokio::io::duplex(64 * 1024);
            let (server_write, client_read) = tokio::io::duplex(64 * 1024);
            let handle = tokio::spawn(async move {
                serve_loop(
                    &registry,
                    &adapter,
                    tokio::io::BufReader::new(server_read),
                    server_write,
                )
                .await
            });
            let client = Self {
                write: client_write,
                read: tokio::io::BufReader::new(client_read).lines(),
            };
            (client, handle)
        }

        /// Send one line and read the next response object.
        async fn round_trip(&mut self, request: &str) -> serde_json::Value {
            self.write.write_all(request.as_bytes()).await.unwrap();
            self.write.write_all(b"\n").await.unwrap();
            let line = self.read.next_line().await.unwrap().unwrap();
            serde_json::from_str(&line).unwrap()
        }

        /// Send one line with no reply expected.
        async fn notify(&mut self, request: &str) {
            self.write.write_all(request.as_bytes()).await.unwrap();
            self.write.write_all(b"\n").await.unwrap();
            self.write.flush().await.unwrap();
        }
    }

    #[tokio::test]
    async fn initialize_replies_with_version_and_server_info() {
        let (mut client, _server) = TestClient::start().await;
        let response = client
            .round_trip(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#)
            .await;
        assert_eq!(response["id"], 1);
        assert_eq!(response["result"]["protocolVersion"], MCP_PROTOCOL_VERSION);
        assert_eq!(response["result"]["serverInfo"]["name"], SERVER_NAME);
        assert!(response.get("error").is_none());
    }

    #[tokio::test]
    async fn initialized_notification_gets_no_reply() {
        let (mut client, _server) = TestClient::start().await;
        // A notification must not produce a line: the next response read
        // belongs to the ping that follows it.
        client
            .notify(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#)
            .await;
        client
            .notify(r#"{"jsonrpc":"2.0","method":"noise/unknown"}"#)
            .await;
        let response = client
            .round_trip(r#"{"jsonrpc":"2.0","id":2,"method":"ping"}"#)
            .await;
        assert_eq!(response["id"], 2);
        assert_eq!(response["result"], serde_json::json!({}));
    }

    #[tokio::test]
    async fn tools_list_advertises_registry_specs() {
        let (mut client, _server) = TestClient::start().await;
        let response = client
            .round_trip(r#"{"jsonrpc":"2.0","id":3,"method":"tools/list"}"#)
            .await;
        let tools = response["result"]["tools"].as_array().unwrap();
        let echo = tools.iter().find(|t| t["name"] == "echo_tool").unwrap();
        assert_eq!(echo["description"], "test echo tool");
        assert_eq!(echo["inputSchema"], serde_json::json!({"type": "object"}));
    }

    #[tokio::test]
    async fn tools_call_executes_and_returns_content() {
        let (mut client, _server) = TestClient::start().await;
        let response = client
            .round_trip(
                r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"echo_tool","arguments":{"x":1}}}"#,
            )
            .await;
        assert_eq!(response["id"], 4);
        assert_eq!(response["result"]["isError"], false);
        assert!(
            response["result"]["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("\"x\":1")
        );
    }

    #[tokio::test]
    async fn unknown_tool_call_returns_error_result() {
        let (mut client, _server) = TestClient::start().await;
        let response = client
            .round_trip(
                r#"{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"nope"}}"#,
            )
            .await;
        assert_eq!(response["result"]["isError"], true);
        assert!(
            response["result"]["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("unknown tool")
        );
    }

    #[tokio::test]
    async fn malformed_line_yields_parse_error() {
        let (mut client, _server) = TestClient::start().await;
        let response = client.round_trip("this is not json").await;
        assert_eq!(response["error"]["code"], PARSE_ERROR);
        assert!(response["id"].is_null());
    }

    #[tokio::test]
    async fn non_object_frame_yields_invalid_request() {
        let (mut client, _server) = TestClient::start().await;
        let response = client.round_trip("[1,2,3]").await;
        assert_eq!(response["error"]["code"], INVALID_REQUEST);
    }

    #[tokio::test]
    async fn unknown_method_yields_method_not_found() {
        let (mut client, _server) = TestClient::start().await;
        let response = client
            .round_trip(r#"{"jsonrpc":"2.0","id":6,"method":"tools/fly"}"#)
            .await;
        assert_eq!(response["id"], 6);
        assert_eq!(response["error"]["code"], METHOD_NOT_FOUND);
    }

    #[tokio::test]
    async fn call_with_bad_params_yields_invalid_params() {
        let (mut client, _server) = TestClient::start().await;
        let missing_name = client
            .round_trip(
                r#"{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"arguments":{}}}"#,
            )
            .await;
        assert_eq!(missing_name["error"]["code"], INVALID_PARAMS);
        let bad_arguments = client
            .round_trip(
                r#"{"jsonrpc":"2.0","id":8,"method":"tools/call","params":{"name":"echo_tool","arguments":[1]}}"#,
            )
            .await;
        assert_eq!(bad_arguments["error"]["code"], INVALID_PARAMS);
    }

    #[tokio::test]
    async fn faulty_tool_yields_internal_error() {
        let (mut client, _server) = TestClient::start().await;
        let response = client
            .round_trip(
                r#"{"jsonrpc":"2.0","id":9,"method":"tools/call","params":{"name":"faulty_tool"}}"#,
            )
            .await;
        assert_eq!(response["id"], 9);
        assert_eq!(response["error"]["code"], INTERNAL_ERROR);
    }

    #[tokio::test]
    async fn eof_shuts_down_cleanly() {
        let (client, server) = TestClient::start().await;
        drop(client.write);
        server.await.unwrap().unwrap();
        let _ = client.read;
    }
}
