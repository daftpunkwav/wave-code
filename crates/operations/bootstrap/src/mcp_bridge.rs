/*!
 * @file McpBridge
 * @description Real stdio MCP client plus registry bridge tools.
 *
 * Responsibilities:
 * - Speak MCP over child-process stdio (initialize handshake,
 *   tools/list with pagination, tools/call).
 * - Bridge each listed tool as a `mcp__{server}__{tool}` registry tool.
 * - Connect configured servers with warn-and-continue degradation:
 *   an unreachable server skips its tools, never fails startup.
 *
 * This module must not depend on: drivers, actors, or sessions. Servers
 * are plain child processes; the registry only sees bridge tools.
 */

//! MCP stdio client and registry bridging.
//!
//! Only stdio servers connect for now; HTTP servers report their status
//! honestly instead of pretending. Interleaved server messages without a
//! matching response id are skipped (bounded), so stray notifications can
//! never be misread as call results.

use std::collections::HashMap;
use std::sync::Arc;

use wavecode_mcp::{McpClient, McpError, McpToolDef, McpToolOutput};
use wavecode_tools::{Result, Tool, ToolCtx, ToolOutput};

/// Protocol version offered at `initialize`.
pub const MCP_PROTOCOL_VERSION: &str = "2024-11-05";
/// Whole-connect budget per server: spawn, handshake, and tool listing.
pub const MCP_CONNECT_TIMEOUT_SECS: u64 = 60;
/// Per-read timeout for one server response line.
const MCP_RESPONSE_TIMEOUT_SECS: u64 = 30;
/// Cap on interleaved server messages skipped while awaiting a response.
const MAX_INTERLEAVED_SKIPS: usize = 32;
/// Cap on `tools/list` pagination pages.
const MAX_LIST_PAGES: usize = 16;

/// MCP `initialize` response must carry a `protocolVersion` string.
///
/// Unknown versions proceed (documented, not rejected): strict servers
/// answer the version they speak and still serve `tools/list`.
fn check_initialize(payload: &serde_json::Value) -> std::result::Result<(), McpError> {
    if payload
        .get("protocolVersion")
        .and_then(|v| v.as_str())
        .is_some_and(|v| !v.is_empty())
    {
        Ok(())
    } else {
        Err(McpError::Protocol(
            "initialize response lacks protocolVersion".to_string(),
        ))
    }
}

/// Parse one `tools/list` page into tool definitions plus a next cursor.
///
/// Items without a usable name are skipped (a server naming slip must not
/// poison the whole page); a non-object payload is a protocol error.
fn parse_tools_list(
    payload: &serde_json::Value,
) -> std::result::Result<(Vec<McpToolDef>, Option<String>), McpError> {
    let malformed = || McpError::Protocol("malformed tools/list result".to_string());
    let result = payload.as_object().ok_or_else(malformed)?;
    let tools = result
        .get("tools")
        .and_then(|v| v.as_array())
        .ok_or_else(malformed)?;
    let mut defs = Vec::with_capacity(tools.len());
    for item in tools {
        let Some(name) = item.get("name").and_then(|v| v.as_str()) else {
            continue;
        };
        if name.is_empty() {
            continue;
        }
        defs.push(McpToolDef {
            name: name.to_string(),
            description: item
                .get("description")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
            input_schema: item
                .get("inputSchema")
                .cloned()
                .unwrap_or_else(|| serde_json::json!({"type": "object"})),
            read_only_hint: item
                .get("annotations")
                .and_then(|v| v.get("readOnlyHint"))
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
        });
    }
    let cursor = result
        .get("nextCursor")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());
    Ok((defs, cursor))
}

/// Parse one `tools/call` payload into model-facing output.
///
/// A payload with `content` is a result (`isError` defaults to false);
/// a `{code, message}` shape without `content` is the JSON-RPC error the
/// transport decoder preserved verbatim. Anything else is malformed.
fn parse_tool_result(payload: &serde_json::Value) -> std::result::Result<McpToolOutput, McpError> {
    if let Some(content) = payload.get("content").and_then(|v| v.as_array()) {
        let mut parts = Vec::with_capacity(content.len());
        for block in content {
            if block.get("type").and_then(|v| v.as_str()) == Some("text") {
                parts.push(
                    block
                        .get("text")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default(),
                );
            } else {
                parts.push("[non-text content omitted]");
            }
        }
        return Ok(McpToolOutput {
            content: parts.join("\n"),
            is_error: payload
                .get("isError")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
        });
    }
    if let (Some(code), Some(message)) = (
        payload.get("code").and_then(|v| v.as_i64()),
        payload.get("message").and_then(|v| v.as_str()),
    ) {
        return Err(McpError::Protocol(format!(
            "server returned JSON-RPC error {code}: {message}"
        )));
    }
    Err(McpError::Protocol(
        "malformed tools/call result (no content)".to_string(),
    ))
}

fn transport_error(context: &str, error: transport_mcp::TransportError) -> McpError {
    // Protocol failures (bad frames, JSON-RPC errors, unsupported
    // interactive auth) stay protocol errors; the rest reads as transport.
    match error {
        transport_mcp::TransportError::Protocol(detail) => {
            McpError::Protocol(format!("{context}: {detail}"))
        }
        other => McpError::Transport(format!("{context}: {other}")),
    }
}

/// Real MCP client over a spawned stdio server process.
pub struct StdioMcpClient {
    transport: tokio::sync::Mutex<transport_mcp::ChildTransport>,
    server: String,
}

impl StdioMcpClient {
    /// Spawn the server and run the `initialize` handshake.
    pub async fn connect(
        server: &str,
        command: &str,
        args: Vec<String>,
        env: &HashMap<String, String>,
    ) -> std::result::Result<Self, McpError> {
        let transport = transport_mcp::ChildTransport::spawn_with_env(
            command,
            args,
            env,
            MCP_RESPONSE_TIMEOUT_SECS,
        )
        .await
        .map_err(|e| transport_error("spawn", e))?;
        let client = Self {
            transport: tokio::sync::Mutex::new(transport),
            server: server.to_string(),
        };
        let payload = client
            .request(
                "initialize",
                serde_json::json!({
                    "protocolVersion": MCP_PROTOCOL_VERSION,
                    "capabilities": {},
                    "clientInfo": {
                        "name": "wavecode",
                        "version": env!("CARGO_PKG_VERSION"),
                    },
                }),
            )
            .await?;
        check_initialize(&payload)?;
        client
            .notify("notifications/initialized", serde_json::json!({}))
            .await?;
        Ok(client)
    }

    /// One request/response exchange, skipping interleaved notifications.
    async fn request(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> std::result::Result<serde_json::Value, McpError> {
        let mut transport = self.transport.lock().await;
        let id = transport
            .send_request(method, params)
            .await
            .map_err(|e| transport_error("send", e))?;
        for _ in 0..MAX_INTERLEAVED_SKIPS {
            let response = transport
                .recv_response()
                .await
                .map_err(|e| transport_error("recv", e))?;
            if response.id == id {
                return Ok(response.payload);
            }
        }
        Err(McpError::Protocol(format!(
            "server {} interleaved too many messages before answering {method}",
            self.server
        )))
    }

    /// One fire-and-forget notification.
    async fn notify(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> std::result::Result<(), McpError> {
        self.transport
            .lock()
            .await
            .send_notification(method, params)
            .await
            .map_err(|e| transport_error("notify", e))
    }
}

#[async_trait::async_trait]
impl McpClient for StdioMcpClient {
    async fn list_tools(&self) -> std::result::Result<Vec<McpToolDef>, McpError> {
        let mut tools = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..MAX_LIST_PAGES {
            let mut params = serde_json::json!({});
            if let Some(next) = &cursor {
                params["cursor"] = serde_json::Value::String(next.clone());
            }
            let payload = self.request("tools/list", params).await?;
            let (mut defs, next) = parse_tools_list(&payload)?;
            tools.append(&mut defs);
            cursor = next;
            if cursor.is_none() {
                break;
            }
        }
        Ok(tools)
    }

    async fn call_tool(
        &self,
        name: &str,
        input: serde_json::Value,
    ) -> std::result::Result<McpToolOutput, McpError> {
        let payload = self
            .request(
                "tools/call",
                serde_json::json!({"name": name, "arguments": input}),
            )
            .await?;
        parse_tool_result(&payload)
    }
}

/// Real MCP client over a streamable-HTTP server endpoint.
pub struct HttpMcpClient {
    transport: tokio::sync::Mutex<transport_mcp::http::HttpMcp>,
}

impl HttpMcpClient {
    /// Build the transport and run the `initialize` handshake.
    pub async fn connect(
        url: &str,
        headers: HashMap<String, String>,
        oauth: Option<transport_mcp::http::OAuthClientCredentials>,
    ) -> std::result::Result<Self, McpError> {
        let transport = transport_mcp::http::HttpMcp::new(transport_mcp::http::HttpMcpConfig {
            endpoint: url.to_string(),
            headers,
            oauth,
        })
        .map_err(|e| transport_error("build", e))?;
        let client = Self {
            transport: tokio::sync::Mutex::new(transport),
        };
        let payload = client
            .request(
                "initialize",
                serde_json::json!({
                    "protocolVersion": MCP_PROTOCOL_VERSION,
                    "capabilities": {},
                    "clientInfo": {
                        "name": "wavecode",
                        "version": env!("CARGO_PKG_VERSION"),
                    },
                }),
            )
            .await?;
        check_initialize(&payload)?;
        client
            .notify("notifications/initialized", serde_json::json!({}))
            .await?;
        Ok(client)
    }

    /// One request/response exchange over the HTTP transport.
    async fn request(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> std::result::Result<serde_json::Value, McpError> {
        self.transport
            .lock()
            .await
            .rpc(method, params)
            .await
            .map_err(|e| transport_error("request", e))
    }

    /// One fire-and-forget notification.
    async fn notify(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> std::result::Result<(), McpError> {
        self.transport
            .lock()
            .await
            .notify(method, params)
            .await
            .map_err(|e| transport_error("notify", e))
    }
}

#[async_trait::async_trait]
impl McpClient for HttpMcpClient {
    async fn list_tools(&self) -> std::result::Result<Vec<McpToolDef>, McpError> {
        let mut tools = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..MAX_LIST_PAGES {
            let mut params = serde_json::json!({});
            if let Some(next) = &cursor {
                params["cursor"] = serde_json::Value::String(next.clone());
            }
            let payload = self.request("tools/list", params).await?;
            let (mut defs, next) = parse_tools_list(&payload)?;
            tools.append(&mut defs);
            cursor = next;
            if cursor.is_none() {
                break;
            }
        }
        Ok(tools)
    }

    async fn call_tool(
        &self,
        name: &str,
        input: serde_json::Value,
    ) -> std::result::Result<McpToolOutput, McpError> {
        let payload = self
            .request(
                "tools/call",
                serde_json::json!({"name": name, "arguments": input}),
            )
            .await?;
        parse_tool_result(&payload)
    }
}

/// One MCP server tool as a registry tool.
pub struct McpToolBridge {
    name: String,
    description: String,
    input_schema: serde_json::Value,
    read_only: bool,
    client: Arc<dyn McpClient>,
    tool: String,
}

impl McpToolBridge {
    /// Wrap one listed tool; `None` when the qualified name is invalid
    /// (unreachable after server-name validation, guarded anyway).
    pub fn new(server: &str, def: &McpToolDef, client: Arc<dyn McpClient>) -> Option<Self> {
        Some(Self {
            name: def.qualified_name(server)?,
            description: def
                .description
                .clone()
                .unwrap_or_else(|| format!("MCP tool {} from server {server}", def.name)),
            input_schema: def.input_schema.clone(),
            read_only: def.read_only_hint,
            client,
            tool: def.name.clone(),
        })
    }
}

#[async_trait::async_trait]
impl Tool for McpToolBridge {
    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn input_schema(&self) -> serde_json::Value {
        self.input_schema.clone()
    }

    /// Follows the server's `readOnlyHint`; unknown effects stay
    /// non-read-only and keep the approval path.
    fn is_read_only(&self) -> bool {
        self.read_only
    }

    async fn execute(&self, input: serde_json::Value, _ctx: &ToolCtx) -> Result<ToolOutput> {
        match self.client.call_tool(&self.tool, input).await {
            Ok(output) => Ok(ToolOutput {
                content: output.content,
                is_error: output.is_error,
            }),
            // Transport outage reads as a business error so the model can
            // report or retry instead of tripping an implementation fault.
            Err(error) => Ok(ToolOutput {
                content: format!("MCP tool {} failed: {error}", self.name),
                is_error: true,
            }),
        }
    }
}

/// Live-connect report: display lines plus startup warnings.
pub struct McpConnectReport {
    /// One status line per configured server, in config order.
    pub lines: Vec<String>,
    /// Degradation notes for startup display.
    pub warnings: Vec<String>,
}

/// Connect every configured server and bridge stdio tools into `registry`.
///
/// Failures degrade per server (warning plus `unavailable` line) and never
/// fail the whole report.
pub async fn connect_all(
    servers: &[(String, wavecode_config::McpServerRaw)],
    registry: &Arc<wavecode_tools::Registry>,
) -> McpConnectReport {
    let mut report = McpConnectReport {
        lines: Vec::with_capacity(servers.len()),
        warnings: Vec::new(),
    };
    for (name, raw) in servers {
        let (line, warning) = connect_one(name, raw, registry).await;
        report.lines.push(line);
        if let Some(warning) = warning {
            report.warnings.push(warning);
        }
    }
    report
}

/// Connect one server; returns its status line plus an optional warning.
async fn connect_one(
    name: &str,
    raw: &wavecode_config::McpServerRaw,
    registry: &Arc<wavecode_tools::Registry>,
) -> (String, Option<String>) {
    if !wavecode_mcp::is_valid_server_name(name) {
        let reason = format!("invalid MCP server name {name:?}: must be non-empty without `__`");
        return (format!("{name} — skipped ({reason})"), Some(reason));
    }
    let config = match (&raw.command, &raw.url) {
        (Some(_), Some(_)) => {
            let reason = format!("MCP server {name:?} sets both command and url; use one");
            return (format!("{name} — skipped ({reason})"), Some(reason));
        }
        (Some(command), None) => wavecode_mcp::McpServerConfig::Stdio {
            command: command.clone(),
            args: raw.args.clone(),
            env: raw.env.clone(),
        },
        (None, Some(url)) => wavecode_mcp::McpServerConfig::Http {
            url: url.clone(),
            headers: raw.headers.clone(),
            oauth_token_url: raw.oauth_token_url.clone(),
            oauth_client_id: raw.oauth_client_id.clone(),
            oauth_client_secret: raw.oauth_client_secret.clone(),
            oauth_scope: raw.oauth_scope.clone(),
        },
        (None, None) => {
            let reason = format!("MCP server {name:?} sets neither command nor url");
            return (format!("{name} — skipped ({reason})"), Some(reason));
        }
    };
    if let Err(reason) = config.validate() {
        return (format!("{name} — skipped ({reason})"), Some(reason));
    }
    match config {
        wavecode_mcp::McpServerConfig::Http {
            url,
            headers,
            oauth_token_url,
            oauth_client_id,
            oauth_client_secret,
            oauth_scope,
        } => {
            let oauth = match (oauth_token_url, oauth_client_id, oauth_client_secret) {
                (Some(token_url), Some(client_id), Some(client_secret)) => {
                    Some(transport_mcp::http::OAuthClientCredentials {
                        token_url,
                        client_id,
                        client_secret,
                        scope: oauth_scope,
                    })
                }
                _ => None,
            };
            let summary = format!("http: {url}");
            match tokio::time::timeout(
                std::time::Duration::from_secs(MCP_CONNECT_TIMEOUT_SECS),
                connect_http(name, &url, headers, oauth, registry),
            )
            .await
            {
                Ok(Ok(count)) => (
                    format!(
                        "{name} ({summary}) — connected ({count} tool{})",
                        if count == 1 { "" } else { "s" }
                    ),
                    None,
                ),
                Ok(Err(error)) => {
                    let reason = format!("MCP server {name:?} failed: {error}; tools skipped");
                    (
                        format!("{name} ({summary}) — unavailable ({error})"),
                        Some(reason),
                    )
                }
                Err(_) => {
                    let reason = format!(
                        "MCP server {name:?} connect timed out after {MCP_CONNECT_TIMEOUT_SECS}s; tools skipped"
                    );
                    (
                        format!("{name} ({summary}) — unavailable (connect timed out)"),
                        Some(reason),
                    )
                }
            }
        }
        wavecode_mcp::McpServerConfig::Stdio { command, args, env } => {
            let summary = wavecode_mcp::McpServerConfig::Stdio {
                command: command.clone(),
                args: args.clone(),
                env: HashMap::new(),
            }
            .summary();
            match tokio::time::timeout(
                std::time::Duration::from_secs(MCP_CONNECT_TIMEOUT_SECS),
                connect_stdio(name, &command, args, &env, registry),
            )
            .await
            {
                Ok(Ok(count)) => (
                    format!(
                        "{name} ({summary}) — connected ({count} tool{})",
                        if count == 1 { "" } else { "s" }
                    ),
                    None,
                ),
                Ok(Err(error)) => {
                    let reason = format!("MCP server {name:?} failed: {error}; tools skipped");
                    (
                        format!("{name} ({summary}) — unavailable ({error})"),
                        Some(reason),
                    )
                }
                Err(_) => {
                    let reason = format!(
                        "MCP server {name:?} connect timed out after {MCP_CONNECT_TIMEOUT_SECS}s; tools skipped"
                    );
                    (
                        format!("{name} ({summary}) — unavailable (connect timed out)"),
                        Some(reason),
                    )
                }
            }
        }
    }
}

/// Handshake, list, and bridge one stdio server; returns bridged count.
async fn connect_stdio(
    name: &str,
    command: &str,
    args: Vec<String>,
    env: &HashMap<String, String>,
    registry: &Arc<wavecode_tools::Registry>,
) -> std::result::Result<usize, McpError> {
    let client: Arc<dyn McpClient> =
        Arc::new(StdioMcpClient::connect(name, command, args, env).await?);
    let tools = client.list_tools().await?;
    let mut count = 0;
    for def in &tools {
        // Empty tool names never reach the registry; the handshake told
        // us the server speaks, so one bad item skips quietly.
        if let Some(bridge) = McpToolBridge::new(name, def, client.clone()) {
            registry.register(Arc::new(bridge));
            count += 1;
        }
    }
    Ok(count)
}

/// Handshake, list, and bridge one HTTP server; returns bridged count.
async fn connect_http(
    name: &str,
    url: &str,
    headers: HashMap<String, String>,
    oauth: Option<transport_mcp::http::OAuthClientCredentials>,
    registry: &Arc<wavecode_tools::Registry>,
) -> std::result::Result<usize, McpError> {
    let client: Arc<dyn McpClient> = Arc::new(HttpMcpClient::connect(url, headers, oauth).await?);
    let tools = client.list_tools().await?;
    let mut count = 0;
    for def in &tools {
        // Empty tool names never reach the registry; the handshake told
        // us the server speaks, so one bad item skips quietly.
        if let Some(bridge) = McpToolBridge::new(name, def, client.clone()) {
            registry.register(Arc::new(bridge));
            count += 1;
        }
    }
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FakeClient {
        tools: Vec<McpToolDef>,
        calls: std::sync::Mutex<Vec<(String, serde_json::Value)>>,
        fail_calls: bool,
    }

    impl FakeClient {
        fn new(tools: Vec<McpToolDef>) -> Self {
            Self {
                tools,
                calls: std::sync::Mutex::new(Vec::new()),
                fail_calls: false,
            }
        }
    }

    #[async_trait::async_trait]
    impl McpClient for FakeClient {
        async fn list_tools(&self) -> std::result::Result<Vec<McpToolDef>, McpError> {
            Ok(self.tools.clone())
        }

        async fn call_tool(
            &self,
            name: &str,
            input: serde_json::Value,
        ) -> std::result::Result<McpToolOutput, McpError> {
            self.calls.lock().unwrap().push((name.to_string(), input));
            if self.fail_calls {
                return Err(McpError::Transport("pipe broken".to_string()));
            }
            Ok(McpToolOutput {
                content: format!("ran {name}"),
                is_error: false,
            })
        }
    }

    fn def(name: &str) -> McpToolDef {
        McpToolDef {
            name: name.to_string(),
            description: Some(format!("does {name}")),
            input_schema: serde_json::json!({"type": "object"}),
            read_only_hint: false,
        }
    }

    fn ctx() -> ToolCtx {
        ToolCtx {
            cwd: std::path::PathBuf::from("/tmp"),
            deny_env: Vec::new(),
        }
    }

    #[test]
    fn initialize_requires_a_protocol_version() {
        assert!(check_initialize(&serde_json::json!({"protocolVersion": "2024-11-05"})).is_ok());
        assert!(check_initialize(&serde_json::json!({})).is_err());
        assert!(check_initialize(&serde_json::json!({"protocolVersion": ""})).is_err());
        assert!(check_initialize(&serde_json::json!([])).is_err());
    }

    #[test]
    fn tools_list_pages_parse_with_cursor() {
        let (defs, cursor) = parse_tools_list(&serde_json::json!({
            "tools": [
                {"name": "click", "description": "d", "inputSchema": {"type": "object"}},
                {"name": "", "description": "skipped"},
                {"description": "nameless skipped"},
                {"name": "scan", "annotations": {"readOnlyHint": true}},
            ],
            "nextCursor": "page2",
        }))
        .unwrap();
        assert_eq!(defs.len(), 2);
        assert_eq!(defs[0].name, "click");
        assert_eq!(defs[0].description.as_deref(), Some("d"));
        assert!(defs[1].read_only_hint);
        // Missing inputSchema defaults to a plain object schema.
        assert_eq!(defs[1].input_schema, serde_json::json!({"type": "object"}));
        assert_eq!(cursor.as_deref(), Some("page2"));
        let (defs, cursor) = parse_tools_list(&serde_json::json!({"tools": []})).unwrap();
        assert!(defs.is_empty());
        assert_eq!(cursor, None);
        assert!(parse_tools_list(&serde_json::json!({"tools": {}})).is_err());
        assert!(parse_tools_list(&serde_json::json!([])).is_err());
    }

    #[test]
    fn tool_results_flatten_and_detect_errors() {
        let out = parse_tool_result(&serde_json::json!({
            "content": [
                {"type": "text", "text": "hello"},
                {"type": "text", "text": "world"},
            ],
        }))
        .unwrap();
        assert_eq!(out.content, "hello\nworld");
        assert!(!out.is_error);
        let out = parse_tool_result(&serde_json::json!({
            "content": [{"type": "text", "text": "bad"}],
            "isError": true,
        }))
        .unwrap();
        assert!(out.is_error);
        let out = parse_tool_result(&serde_json::json!({
            "content": [{"type": "image", "data": "x"}],
        }))
        .unwrap();
        assert!(out.content.contains("non-text"));
        // JSON-RPC error shape (preserved verbatim by the decoder).
        assert!(
            parse_tool_result(&serde_json::json!({"code": -32601, "message": "nope"})).is_err()
        );
        assert!(parse_tool_result(&serde_json::json!({"unexpected": 1})).is_err());
    }

    #[tokio::test]
    async fn bridge_names_describes_and_forwards_calls() {
        let client = Arc::new(FakeClient::new(vec![def("click")]));
        let bridge =
            McpToolBridge::new("playwright", &def("click"), client.clone()).expect("valid name");
        assert_eq!(bridge.name(), "mcp__playwright__click");
        assert_eq!(bridge.description(), "does click");
        assert!(!bridge.is_read_only());
        let out = bridge
            .execute(serde_json::json!({"x": 1}), &ctx())
            .await
            .unwrap();
        assert!(!out.is_error);
        assert!(out.content.contains("ran click"));
        // Server-side raw name (no prefix) reaches the client.
        assert_eq!(client.calls.lock().unwrap()[0].0, "click".to_string());
    }

    #[tokio::test]
    async fn bridge_transport_failures_read_as_business_errors() {
        let client = Arc::new(FakeClient {
            tools: vec![],
            calls: std::sync::Mutex::new(Vec::new()),
            fail_calls: true,
        });
        let bridge = McpToolBridge::new("srv", &def("go"), client).expect("valid name");
        let out = bridge.execute(serde_json::json!({}), &ctx()).await.unwrap();
        assert!(out.is_error);
        assert!(out.content.contains("mcp__srv__go"));
    }

    #[test]
    fn bridge_rejects_invalid_servers_and_marks_read_only() {
        let client: Arc<dyn McpClient> = Arc::new(FakeClient::new(vec![]));
        assert!(McpToolBridge::new("a__b", &def("go"), client.clone()).is_none());
        let mut ro = def("scan");
        ro.description = None;
        ro.read_only_hint = true;
        let bridge = McpToolBridge::new("srv", &ro, client).expect("valid name");
        assert!(bridge.is_read_only());
        assert!(bridge.description().contains("srv"));
    }

    fn raw(command: Option<&str>, url: Option<&str>) -> wavecode_config::McpServerRaw {
        wavecode_config::McpServerRaw {
            command: command.map(|s| s.to_string()),
            args: Vec::new(),
            env: HashMap::new(),
            url: url.map(|s| s.to_string()),
            headers: HashMap::new(),
            oauth_token_url: None,
            oauth_client_id: None,
            oauth_client_secret: None,
            oauth_scope: None,
        }
    }

    #[tokio::test]
    async fn connect_all_skips_misconfigurations_without_spawning() {
        let registry = Arc::new(wavecode_tools::Registry::builtin());
        let report = connect_all(
            &[
                ("bad__name".to_string(), raw(Some("cmd"), None)),
                ("both".to_string(), raw(Some("cmd"), Some("http://x"))),
                ("neither".to_string(), raw(None, None)),
                (
                    "web".to_string(),
                    raw(None, Some("https://mcp.example.com")),
                ),
            ],
            &registry,
        )
        .await;
        assert_eq!(report.lines.len(), 4);
        assert!(
            report
                .lines
                .iter()
                .all(|l| l.contains("skipped") || l.contains("unavailable"))
        );
        assert!(report.warnings.len() == 4);
        assert!(registry.get("mcp__bad__name__x").is_none());
    }

    #[tokio::test]
    async fn connect_all_empty_stays_empty() {
        let registry = Arc::new(wavecode_tools::Registry::builtin());
        let report = connect_all(&[], &registry).await;
        assert!(report.lines.is_empty());
        assert!(report.warnings.is_empty());
    }
}
