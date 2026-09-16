/*!
 * @file McpBridge
 * @description Real stdio/streamable-HTTP MCP clients plus registry bridge tools.
 *
 * Responsibilities:
 * - Speak MCP over child-process stdio and over streamable HTTP
 *   (initialize handshake, capability detection, paged listings, calls).
 * - Bridge each listed tool as a `mcp__{server}__{tool}` registry tool.
 * - Bridge capability-gated discovery tools: `mcp__{server}__read_resource`
 *   and `mcp__{server}__get_prompt` when the server advertises the
 *   resources / prompts capabilities (the server's catalog is embedded in
 *   the tool description).
 * - Re-initialize once when a streamable-HTTP server expires the session
 *   (404), then retry the original request.
 * - Connect configured servers with warn-and-continue degradation:
 *   an unreachable server skips its tools, never fails startup.
 *
 * This module must not depend on: drivers, actors, or sessions. Servers
 * are plain child processes or HTTP endpoints; the registry only sees
 * bridge tools.
 */

//! MCP clients (stdio + streamable HTTP) and registry bridging.
//!
//! Both transports connect for real: handshake, capability-gated listings,
//! and calls go over the wire. Interleaved stdio server messages without a
//! matching response id are skipped (bounded), so stray notifications can
//! never be misread as call results.

use std::collections::HashMap;
use std::sync::Arc;

use wavecode_mcp::{
    McpClient, McpError, McpPromptDef, McpPromptMessage, McpResourceContent, McpResourceDef,
    McpToolDef, McpToolOutput, try_tool_name,
};
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

/// Server capabilities relevant to bridging, read from the `initialize`
/// response's `capabilities` block (presence of the sub-object means the
/// capability is supported, per the MCP spec).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct ServerCaps {
    /// Server advertised the `resources` capability.
    resources: bool,
    /// Server advertised the `prompts` capability.
    prompts: bool,
}

/// Read the advertised capabilities out of an `initialize` result.
fn server_caps(payload: &serde_json::Value) -> ServerCaps {
    let caps = payload.get("capabilities");
    ServerCaps {
        resources: caps.and_then(|c| c.get("resources")).is_some(),
        prompts: caps.and_then(|c| c.get("prompts")).is_some(),
    }
}

/// The request/response half both client transports share (each keeps its
/// own connection handling behind [`McpClient`]; this trait exists so the
/// list/call drivers below are written once).
#[async_trait::async_trait]
trait RpcClient {
    /// One request/response exchange.
    async fn rpc(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> std::result::Result<serde_json::Value, McpError>;
}

/// One parsed list page: items plus the next cursor, if the server paginates.
type ListPage<T> = std::result::Result<(Vec<T>, Option<String>), McpError>;

/// Walk a paginated MCP list method (`tools/list`, `resources/list`,
/// `prompts/list`) to its end, parsing each page with `parse` and bounding
/// the walk at [`MAX_LIST_PAGES`].
async fn list_paged<C, T>(
    client: &C,
    method: &str,
    parse: fn(&serde_json::Value) -> ListPage<T>,
) -> std::result::Result<Vec<T>, McpError>
where
    C: RpcClient + ?Sized,
{
    let mut items = Vec::new();
    let mut cursor: Option<String> = None;
    for _ in 0..MAX_LIST_PAGES {
        let mut params = serde_json::json!({});
        if let Some(next) = &cursor {
            params["cursor"] = serde_json::Value::String(next.clone());
        }
        let (mut page, next) = parse(&client.rpc(method, params).await?)?;
        items.append(&mut page);
        match next {
            Some(next) => cursor = Some(next),
            None => return Ok(items),
        }
    }
    Ok(items)
}

/// `tools/call` round trip shared by both transports.
async fn call_tool_via<C>(
    client: &C,
    name: &str,
    input: serde_json::Value,
) -> std::result::Result<McpToolOutput, McpError>
where
    C: RpcClient + ?Sized,
{
    let payload = client
        .rpc(
            "tools/call",
            serde_json::json!({"name": name, "arguments": input}),
        )
        .await?;
    parse_tool_result(&payload)
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

/// Parse one `resources/list` page into resource definitions plus a cursor.
///
/// Items without a usable URI are skipped; a missing `name` falls back to
/// the URI (the protocol makes it optional).
fn parse_resources_list(
    payload: &serde_json::Value,
) -> std::result::Result<(Vec<McpResourceDef>, Option<String>), McpError> {
    let malformed = || McpError::Protocol("malformed resources/list result".to_string());
    let result = payload.as_object().ok_or_else(malformed)?;
    let resources = result
        .get("resources")
        .and_then(|v| v.as_array())
        .ok_or_else(malformed)?;
    let mut defs = Vec::with_capacity(resources.len());
    for item in resources {
        let Some(uri) = item
            .get("uri")
            .and_then(|v| v.as_str())
            .filter(|u| !u.is_empty())
        else {
            continue;
        };
        defs.push(McpResourceDef {
            uri: uri.to_string(),
            name: item
                .get("name")
                .and_then(|v| v.as_str())
                .filter(|n| !n.is_empty())
                .unwrap_or(uri)
                .to_string(),
            description: item
                .get("description")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
            mime_type: item
                .get("mimeType")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
        });
    }
    let cursor = result
        .get("nextCursor")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());
    Ok((defs, cursor))
}

/// Parse a `resources/read` result into text contents.
///
/// `blob` (base64) entries surface as an omission note, matching the
/// non-text content-block handling of tool results; a missing `contents`
/// array is a protocol error.
fn parse_resource_contents(
    payload: &serde_json::Value,
) -> std::result::Result<Vec<McpResourceContent>, McpError> {
    let contents = payload
        .get("contents")
        .and_then(|v| v.as_array())
        .ok_or_else(|| McpError::Protocol("malformed resources/read result".to_string()))?;
    let mut out = Vec::with_capacity(contents.len());
    for entry in contents {
        let Some(uri) = entry
            .get("uri")
            .and_then(|v| v.as_str())
            .filter(|u| !u.is_empty())
        else {
            continue;
        };
        let mime_type = entry
            .get("mimeType")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let text = if let Some(text) = entry.get("text").and_then(|v| v.as_str()) {
            text.to_string()
        } else if entry.get("blob").is_some() {
            "[binary resource content omitted]".to_string()
        } else {
            continue;
        };
        out.push(McpResourceContent {
            uri: uri.to_string(),
            mime_type,
            text,
        });
    }
    Ok(out)
}

/// Parse one `prompts/list` page into prompt definitions plus a cursor.
fn parse_prompts_list(
    payload: &serde_json::Value,
) -> std::result::Result<(Vec<McpPromptDef>, Option<String>), McpError> {
    let malformed = || McpError::Protocol("malformed prompts/list result".to_string());
    let result = payload.as_object().ok_or_else(malformed)?;
    let prompts = result
        .get("prompts")
        .and_then(|v| v.as_array())
        .ok_or_else(malformed)?;
    let mut defs = Vec::with_capacity(prompts.len());
    for item in prompts {
        let Some(name) = item
            .get("name")
            .and_then(|v| v.as_str())
            .filter(|n| !n.is_empty())
        else {
            continue;
        };
        let arguments = item
            .get("arguments")
            .and_then(|v| v.as_array())
            .map(|args| {
                args.iter()
                    .filter_map(|arg| {
                        let name = arg.get("name").and_then(|v| v.as_str())?;
                        Some(wavecode_mcp::McpPromptArgument {
                            name: name.to_string(),
                            description: arg
                                .get("description")
                                .and_then(|v| v.as_str())
                                .map(|s| s.to_string()),
                            required: arg
                                .get("required")
                                .and_then(|v| v.as_bool())
                                .unwrap_or(false),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        defs.push(McpPromptDef {
            name: name.to_string(),
            description: item
                .get("description")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
            arguments,
        });
    }
    let cursor = result
        .get("nextCursor")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());
    Ok((defs, cursor))
}

/// Parse a `prompts/get` result into `(role, text)` messages.
///
/// Non-text content blocks surface as an omission note; items without a
/// usable role are skipped.
fn parse_prompt_messages(
    payload: &serde_json::Value,
) -> std::result::Result<Vec<McpPromptMessage>, McpError> {
    let messages = payload
        .get("messages")
        .and_then(|v| v.as_array())
        .ok_or_else(|| McpError::Protocol("malformed prompts/get result".to_string()))?;
    let mut out = Vec::with_capacity(messages.len());
    for message in messages {
        let Some(role) = message
            .get("role")
            .and_then(|v| v.as_str())
            .filter(|r| !r.is_empty())
        else {
            continue;
        };
        let content = message.get("content");
        let text = if let Some(text) = content.and_then(|c| c.get("text")).and_then(|v| v.as_str())
        {
            text.to_string()
        } else if content.is_some() {
            "[non-text content omitted]".to_string()
        } else {
            String::new()
        };
        out.push(McpPromptMessage {
            role: role.to_string(),
            text,
        });
    }
    Ok(out)
}

/// Real MCP client over a spawned stdio server process.
pub struct StdioMcpClient {
    transport: tokio::sync::Mutex<transport_mcp::ChildTransport>,
    server: String,
    caps: ServerCaps,
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
            caps: ServerCaps::default(),
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
        Ok(Self {
            caps: server_caps(&payload),
            ..client
        })
    }

    /// Capabilities the server advertised at `initialize`.
    fn caps(&self) -> ServerCaps {
        self.caps
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
impl RpcClient for StdioMcpClient {
    async fn rpc(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> std::result::Result<serde_json::Value, McpError> {
        self.request(method, params).await
    }
}

#[async_trait::async_trait]
impl McpClient for StdioMcpClient {
    async fn list_tools(&self) -> std::result::Result<Vec<McpToolDef>, McpError> {
        list_paged(self, "tools/list", parse_tools_list).await
    }

    async fn call_tool(
        &self,
        name: &str,
        input: serde_json::Value,
    ) -> std::result::Result<McpToolOutput, McpError> {
        call_tool_via(self, name, input).await
    }

    async fn list_prompts(&self) -> std::result::Result<Vec<McpPromptDef>, McpError> {
        if !self.caps.prompts {
            return Ok(vec![]);
        }
        list_paged(self, "prompts/list", parse_prompts_list).await
    }

    async fn get_prompt(
        &self,
        name: &str,
        arguments: HashMap<String, String>,
    ) -> std::result::Result<Vec<McpPromptMessage>, McpError> {
        if !self.caps.prompts {
            return Ok(vec![]);
        }
        let payload = self
            .rpc(
                "prompts/get",
                serde_json::json!({"name": name, "arguments": arguments}),
            )
            .await?;
        parse_prompt_messages(&payload)
    }

    async fn list_resources(&self) -> std::result::Result<Vec<McpResourceDef>, McpError> {
        if !self.caps.resources {
            return Ok(vec![]);
        }
        list_paged(self, "resources/list", parse_resources_list).await
    }

    async fn read_resource(
        &self,
        uri: &str,
    ) -> std::result::Result<Vec<McpResourceContent>, McpError> {
        if !self.caps.resources {
            return Ok(vec![]);
        }
        let payload = self
            .rpc("resources/read", serde_json::json!({"uri": uri}))
            .await?;
        parse_resource_contents(&payload)
    }
}

/// Real MCP client over a streamable-HTTP server endpoint.
pub struct HttpMcpClient {
    transport: tokio::sync::Mutex<transport_mcp::http::HttpMcp>,
    caps: ServerCaps,
}

/// Run the `initialize` handshake over an HTTP transport; returns the
/// server capabilities. Also used to rebuild the session after the server
/// expires it (404), so the exchange stays request-scoped and idempotent.
async fn http_handshake(
    transport: &transport_mcp::http::HttpMcp,
) -> std::result::Result<ServerCaps, McpError> {
    let payload = transport
        .rpc(
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
        .await
        .map_err(|e| transport_error("request", e))?;
    check_initialize(&payload)?;
    transport
        .notify("notifications/initialized", serde_json::json!({}))
        .await
        .map_err(|e| transport_error("notify", e))?;
    Ok(server_caps(&payload))
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
        let caps = http_handshake(&transport).await?;
        Ok(Self {
            transport: tokio::sync::Mutex::new(transport),
            caps,
        })
    }

    /// Capabilities the server advertised at `initialize`.
    fn caps(&self) -> ServerCaps {
        self.caps
    }

    /// One request/response exchange over the HTTP transport.
    ///
    /// A 404 mid-session means the server expired (or dropped) the session:
    /// the transport already cleared its session id, so re-initialize once
    /// and retry. A second expiry surfaces as an error instead of looping.
    async fn request(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> std::result::Result<serde_json::Value, McpError> {
        let transport = self.transport.lock().await;
        match transport.rpc(method, params.clone()).await {
            Err(transport_mcp::TransportError::SessionExpired) => {
                http_handshake(&transport).await?;
                transport
                    .rpc(method, params)
                    .await
                    .map_err(|e| transport_error("request", e))
            }
            other => other.map_err(|e| transport_error("request", e)),
        }
    }
}

#[async_trait::async_trait]
impl RpcClient for HttpMcpClient {
    async fn rpc(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> std::result::Result<serde_json::Value, McpError> {
        self.request(method, params).await
    }
}

#[async_trait::async_trait]
impl McpClient for HttpMcpClient {
    async fn list_tools(&self) -> std::result::Result<Vec<McpToolDef>, McpError> {
        list_paged(self, "tools/list", parse_tools_list).await
    }

    async fn call_tool(
        &self,
        name: &str,
        input: serde_json::Value,
    ) -> std::result::Result<McpToolOutput, McpError> {
        call_tool_via(self, name, input).await
    }

    async fn list_prompts(&self) -> std::result::Result<Vec<McpPromptDef>, McpError> {
        if !self.caps.prompts {
            return Ok(vec![]);
        }
        list_paged(self, "prompts/list", parse_prompts_list).await
    }

    async fn get_prompt(
        &self,
        name: &str,
        arguments: HashMap<String, String>,
    ) -> std::result::Result<Vec<McpPromptMessage>, McpError> {
        if !self.caps.prompts {
            return Ok(vec![]);
        }
        let payload = self
            .rpc(
                "prompts/get",
                serde_json::json!({"name": name, "arguments": arguments}),
            )
            .await?;
        parse_prompt_messages(&payload)
    }

    async fn list_resources(&self) -> std::result::Result<Vec<McpResourceDef>, McpError> {
        if !self.caps.resources {
            return Ok(vec![]);
        }
        list_paged(self, "resources/list", parse_resources_list).await
    }

    async fn read_resource(
        &self,
        uri: &str,
    ) -> std::result::Result<Vec<McpResourceContent>, McpError> {
        if !self.caps.resources {
            return Ok(vec![]);
        }
        let payload = self
            .rpc("resources/read", serde_json::json!({"uri": uri}))
            .await?;
        parse_resource_contents(&payload)
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

/// Description for the `read_resource` bridge: usage line plus the server's
/// resource catalog (one `- uri — name: description (mime)` per line).
fn resource_catalog(server: &str, resources: &[McpResourceDef]) -> String {
    let mut out = format!("Read a resource from MCP server {server} by URI.");
    if resources.is_empty() {
        out.push_str(" The server listed no resources; use a URI obtained from elsewhere.");
        return out;
    }
    out.push_str(" Available resources:");
    for resource in resources {
        out.push_str(&format!("\n- {} — {}", resource.uri, resource.name));
        if let Some(description) = &resource.description {
            out.push_str(&format!(": {description}"));
        }
        if let Some(mime_type) = &resource.mime_type {
            out.push_str(&format!(" ({mime_type})"));
        }
    }
    out
}

/// Description for the `get_prompt` bridge: usage line plus the server's
/// prompt catalog (one `- name: description [arguments: …]` per line).
fn prompt_catalog(server: &str, prompts: &[McpPromptDef]) -> String {
    let mut out = format!("Render a prompt from MCP server {server} by name.");
    if prompts.is_empty() {
        out.push_str(" The server listed no prompts.");
        return out;
    }
    out.push_str(" Available prompts:");
    for prompt in prompts {
        out.push_str(&format!("\n- {}", prompt.name));
        if let Some(description) = &prompt.description {
            out.push_str(&format!(": {description}"));
        }
        if !prompt.arguments.is_empty() {
            let names: Vec<String> = prompt
                .arguments
                .iter()
                .map(|arg| {
                    if arg.required {
                        format!("{} (required)", arg.name)
                    } else {
                        arg.name.clone()
                    }
                })
                .collect();
            out.push_str(&format!(" [arguments: {}]", names.join(", ")));
        }
    }
    out
}

/// `mcp__{server}__read_resource` bridge: reads one MCP resource by URI.
///
/// V1 discovery surface for the `resources` capability: the server's
/// catalog is embedded in the description, so the listing is visible to the
/// model without an extra registry tool.
pub struct McpResourceBridge {
    name: String,
    description: String,
    client: Arc<dyn McpClient>,
}

impl McpResourceBridge {
    /// Build from the server's listed resources; `None` when the qualified
    /// name is invalid (unreachable after server-name validation, guarded
    /// anyway).
    pub fn new(
        server: &str,
        resources: &[McpResourceDef],
        client: Arc<dyn McpClient>,
    ) -> Option<Self> {
        Some(Self {
            name: try_tool_name(server, "read_resource")?,
            description: resource_catalog(server, resources),
            client,
        })
    }
}

#[async_trait::async_trait]
impl Tool for McpResourceBridge {
    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "uri": {
                    "type": "string",
                    "description": "Resource URI, from the catalog in this tool's description",
                },
            },
            "required": ["uri"],
        })
    }

    /// Reads have no side effects on the server.
    fn is_read_only(&self) -> bool {
        true
    }

    async fn execute(&self, input: serde_json::Value, _ctx: &ToolCtx) -> Result<ToolOutput> {
        let Some(uri) = input.get("uri").and_then(|v| v.as_str()) else {
            return Ok(ToolOutput {
                content: "missing required string parameter `uri`".to_owned(),
                is_error: true,
            });
        };
        match self.client.read_resource(uri).await {
            Ok(contents) => {
                let text = contents
                    .iter()
                    .map(|content| content.text.as_str())
                    .collect::<Vec<_>>()
                    .join("\n");
                Ok(ToolOutput {
                    content: if text.is_empty() {
                        format!("resource {uri} returned no contents")
                    } else {
                        text
                    },
                    is_error: false,
                })
            }
            // Transport outage reads as a business error so the model can
            // report or retry instead of tripping an implementation fault.
            Err(error) => Ok(ToolOutput {
                content: format!("MCP resource {uri} failed: {error}"),
                is_error: true,
            }),
        }
    }
}

/// `mcp__{server}__get_prompt` bridge: renders one MCP prompt by name.
///
/// V1 discovery surface for the `prompts` capability (the inline-skill
/// conversion of SPEC section 10 stays on the core side); the prompt
/// catalog is embedded in the description.
pub struct McpPromptBridge {
    name: String,
    description: String,
    client: Arc<dyn McpClient>,
}

impl McpPromptBridge {
    /// Build from the server's listed prompts; `None` when the qualified
    /// name is invalid (unreachable after server-name validation, guarded
    /// anyway).
    pub fn new(server: &str, prompts: &[McpPromptDef], client: Arc<dyn McpClient>) -> Option<Self> {
        Some(Self {
            name: try_tool_name(server, "get_prompt")?,
            description: prompt_catalog(server, prompts),
            client,
        })
    }
}

#[async_trait::async_trait]
impl Tool for McpPromptBridge {
    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "Prompt name, from the catalog in this tool's description",
                },
                "arguments": {
                    "type": "object",
                    "description": "Prompt argument values keyed by argument name",
                    "additionalProperties": {"type": "string"},
                },
            },
            "required": ["name"],
        })
    }

    /// Rendering a prompt has no side effects on the server.
    fn is_read_only(&self) -> bool {
        true
    }

    async fn execute(&self, input: serde_json::Value, _ctx: &ToolCtx) -> Result<ToolOutput> {
        let Some(name) = input.get("name").and_then(|v| v.as_str()) else {
            return Ok(ToolOutput {
                content: "missing required string parameter `name`".to_owned(),
                is_error: true,
            });
        };
        // The protocol wants string values; non-string entries are dropped
        // rather than forwarded in a shape the server would reject.
        let arguments: HashMap<String, String> = input
            .get("arguments")
            .and_then(|v| v.as_object())
            .map(|object| {
                object
                    .iter()
                    .filter_map(|(key, value)| value.as_str().map(|s| (key.clone(), s.to_owned())))
                    .collect()
            })
            .unwrap_or_default();
        match self.client.get_prompt(name, arguments).await {
            Ok(messages) => {
                let text = messages
                    .iter()
                    .map(|message| format!("{}: {}", message.role, message.text))
                    .collect::<Vec<_>>()
                    .join("\n");
                Ok(ToolOutput {
                    content: if text.is_empty() {
                        format!("prompt {name} returned no messages")
                    } else {
                        text
                    },
                    is_error: false,
                })
            }
            Err(error) => Ok(ToolOutput {
                content: format!("MCP prompt {name} failed: {error}"),
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
    // Connect concurrently: an unreachable server burns its whole connect
    // timeout, and serial awaits would stack that per server on startup.
    // `join_all` keeps result order aligned with the config order, so the
    // report stays deterministic.
    let outcomes = futures::future::join_all(
        servers
            .iter()
            .map(|(name, raw)| connect_one(name, raw, registry)),
    )
    .await;
    for (line, warning) in outcomes {
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

/// Register a connected client's tools plus capability-gated discovery
/// bridges; returns the number of registered registry tools.
///
/// Discovery tools (`read_resource` / `get_prompt`) only register when the
/// server advertised the capability; a listing that fails despite the
/// advertisement skips that one tool quietly (same scoped degradation as a
/// single malformed tool item).
async fn bridge_server(
    name: &str,
    client: Arc<dyn McpClient>,
    caps: ServerCaps,
    registry: &Arc<wavecode_tools::Registry>,
) -> std::result::Result<usize, McpError> {
    let mut count = 0;
    for def in &client.list_tools().await? {
        // Empty tool names never reach the registry; the handshake told
        // us the server speaks, so one bad item skips quietly.
        if let Some(bridge) = McpToolBridge::new(name, def, client.clone()) {
            registry.register(Arc::new(bridge));
            count += 1;
        }
    }
    if caps.resources
        && let Ok(resources) = client.list_resources().await
        && let Some(bridge) = McpResourceBridge::new(name, &resources, client.clone())
    {
        registry.register(Arc::new(bridge));
        count += 1;
    }
    if caps.prompts
        && let Ok(prompts) = client.list_prompts().await
        && let Some(bridge) = McpPromptBridge::new(name, &prompts, client.clone())
    {
        registry.register(Arc::new(bridge));
        count += 1;
    }
    Ok(count)
}

/// Handshake, list, and bridge one stdio server; returns bridged count.
async fn connect_stdio(
    name: &str,
    command: &str,
    args: Vec<String>,
    env: &HashMap<String, String>,
    registry: &Arc<wavecode_tools::Registry>,
) -> std::result::Result<usize, McpError> {
    let client = StdioMcpClient::connect(name, command, args, env).await?;
    let caps = client.caps();
    bridge_server(name, Arc::new(client), caps, registry).await
}

/// Handshake, list, and bridge one HTTP server; returns bridged count.
async fn connect_http(
    name: &str,
    url: &str,
    headers: HashMap<String, String>,
    oauth: Option<transport_mcp::http::OAuthClientCredentials>,
    registry: &Arc<wavecode_tools::Registry>,
) -> std::result::Result<usize, McpError> {
    let client = HttpMcpClient::connect(url, headers, oauth).await?;
    let caps = client.caps();
    bridge_server(name, Arc::new(client), caps, registry).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};

    struct FakeClient {
        tools: Vec<McpToolDef>,
        resources: Vec<McpResourceDef>,
        prompts: Vec<McpPromptDef>,
        caps: ServerCaps,
        calls: std::sync::Mutex<Vec<(String, serde_json::Value)>>,
        fail_calls: bool,
    }

    impl FakeClient {
        fn new(tools: Vec<McpToolDef>) -> Self {
            Self {
                tools,
                resources: Vec::new(),
                prompts: Vec::new(),
                caps: ServerCaps::default(),
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

        async fn list_prompts(&self) -> std::result::Result<Vec<McpPromptDef>, McpError> {
            if !self.caps.prompts {
                return Ok(vec![]);
            }
            Ok(self.prompts.clone())
        }

        async fn get_prompt(
            &self,
            name: &str,
            arguments: HashMap<String, String>,
        ) -> std::result::Result<Vec<McpPromptMessage>, McpError> {
            if !self.caps.prompts {
                return Ok(vec![]);
            }
            self.calls
                .lock()
                .unwrap()
                .push(("prompts/get".to_string(), serde_json::json!(arguments)));
            Ok(vec![McpPromptMessage {
                role: "user".to_string(),
                text: format!("prompt {name} asks"),
            }])
        }

        async fn list_resources(&self) -> std::result::Result<Vec<McpResourceDef>, McpError> {
            if !self.caps.resources {
                return Ok(vec![]);
            }
            Ok(self.resources.clone())
        }

        async fn read_resource(
            &self,
            uri: &str,
        ) -> std::result::Result<Vec<McpResourceContent>, McpError> {
            if !self.caps.resources {
                return Ok(vec![]);
            }
            self.calls
                .lock()
                .unwrap()
                .push(("resources/read".to_string(), serde_json::json!(uri)));
            Ok(vec![McpResourceContent {
                uri: uri.to_string(),
                mime_type: Some("text/plain".to_string()),
                text: "hello".to_string(),
            }])
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
            fail_calls: true,
            ..FakeClient::new(vec![])
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
        // Concurrent connection keeps the report aligned with config order.
        assert!(report.lines[0].contains("bad__name"));
        assert!(report.lines[1].contains("both"));
        assert!(report.lines[2].contains("neither"));
        assert!(report.lines[3].contains("web"));
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

    #[test]
    fn server_caps_read_from_initialize_payload() {
        let caps = server_caps(&serde_json::json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {"resources": {}, "prompts": {"listChanged": true}},
        }));
        assert!(caps.resources && caps.prompts);
        let tools_only = server_caps(&serde_json::json!({"capabilities": {"tools": {}}}));
        assert!(!tools_only.resources && !tools_only.prompts);
        assert_eq!(server_caps(&serde_json::json!({})), ServerCaps::default());
    }

    #[test]
    fn resources_and_prompts_payloads_parse() {
        // resources/list: uri-less and empty-uri items skip; missing name
        // falls back to the uri; the cursor survives.
        let (defs, cursor) = parse_resources_list(&serde_json::json!({
            "resources": [
                {"uri": "file:///a.txt", "name": "a", "description": "first", "mimeType": "text/plain"},
                {"uri": "", "name": "skipped"},
                {"name": "no uri skipped"},
            ],
            "nextCursor": "r2",
        }))
        .unwrap();
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].uri, "file:///a.txt");
        assert_eq!(defs[0].mime_type.as_deref(), Some("text/plain"));
        assert_eq!(cursor.as_deref(), Some("r2"));
        let (defs, cursor) =
            parse_resources_list(&serde_json::json!({"resources": [{"uri": "mem://x"}]})).unwrap();
        assert_eq!(defs[0].name, "mem://x");
        assert_eq!(cursor, None);
        assert!(parse_resources_list(&serde_json::json!({"resources": {}})).is_err());
        assert!(parse_resources_list(&serde_json::json!([])).is_err());

        // resources/read: text and blob entries; a text-less entry skips.
        let contents = parse_resource_contents(&serde_json::json!({
            "contents": [
                {"uri": "file:///a.txt", "mimeType": "text/plain", "text": "hello"},
                {"uri": "file:///b.bin", "blob": "aGVsbG8="},
                {"uri": "file:///empty.json"},
            ],
        }))
        .unwrap();
        assert_eq!(contents.len(), 2);
        assert_eq!(contents[0].text, "hello");
        assert!(contents[1].text.contains("omitted"));
        assert!(parse_resource_contents(&serde_json::json!({})).is_err());

        // prompts/list: arguments parse with their required flags.
        let (defs, cursor) = parse_prompts_list(&serde_json::json!({
            "prompts": [
                {"name": "review", "description": "code review", "arguments": [
                    {"name": "path", "description": "file", "required": true},
                    {"name": "lang"},
                ]},
                {"description": "nameless skipped"},
                {"name": ""},
            ],
        }))
        .unwrap();
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].arguments.len(), 2);
        assert!(defs[0].arguments[0].required);
        assert!(!defs[0].arguments[1].required);
        assert_eq!(cursor, None);
        assert!(parse_prompts_list(&serde_json::json!([])).is_err());

        // prompts/get: role-bearing messages; non-text content degrades.
        let messages = parse_prompt_messages(&serde_json::json!({
            "messages": [
                {"role": "user", "content": {"type": "text", "text": "review this"}},
                {"role": "assistant", "content": {"type": "image", "data": "x"}},
                {"content": {"type": "text", "text": "no role skipped"}},
            ],
        }))
        .unwrap();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].role, "user");
        assert_eq!(messages[0].text, "review this");
        assert!(messages[1].text.contains("omitted"));
        assert!(parse_prompt_messages(&serde_json::json!({"messages": {}})).is_err());
    }

    #[test]
    fn discovery_catalogs_embed_listings() {
        let resources = vec![McpResourceDef {
            uri: "file:///a.txt".to_string(),
            name: "a".to_string(),
            description: Some("first".to_string()),
            mime_type: Some("text/plain".to_string()),
        }];
        let catalog = resource_catalog("srv", &resources);
        assert!(catalog.contains("srv") && catalog.contains("file:///a.txt — a"));
        assert!(catalog.contains("text/plain"));
        assert!(resource_catalog("srv", &[]).contains("no resources"));

        let prompts = vec![McpPromptDef {
            name: "review".to_string(),
            description: Some("code review".to_string()),
            arguments: vec![wavecode_mcp::McpPromptArgument {
                name: "path".to_string(),
                description: None,
                required: true,
            }],
        }];
        let catalog = prompt_catalog("srv", &prompts);
        assert!(catalog.contains("review") && catalog.contains("path (required)"));
        assert!(prompt_catalog("srv", &[]).contains("no prompts"));
    }

    #[tokio::test]
    async fn resource_bridge_reads_and_flags_bad_input() {
        let resources = vec![McpResourceDef {
            uri: "file:///a.txt".to_string(),
            name: "a".to_string(),
            description: None,
            mime_type: None,
        }];
        let client: Arc<dyn McpClient> = Arc::new(FakeClient {
            caps: ServerCaps {
                resources: true,
                prompts: false,
            },
            ..FakeClient::new(vec![])
        });
        let bridge = McpResourceBridge::new("srv", &resources, client).expect("valid name");
        assert_eq!(bridge.name(), "mcp__srv__read_resource");
        assert!(bridge.is_read_only());
        let out = bridge
            .execute(serde_json::json!({"uri": "file:///a.txt"}), &ctx())
            .await
            .unwrap();
        assert!(!out.is_error);
        assert_eq!(out.content, "hello");
        // A missing uri reads as a business error, not an implementation fault.
        let out = bridge.execute(serde_json::json!({}), &ctx()).await.unwrap();
        assert!(out.is_error && out.content.contains("uri"));
    }

    #[tokio::test]
    async fn prompt_bridge_renders_and_drops_non_string_arguments() {
        let prompts = vec![McpPromptDef {
            name: "review".to_string(),
            description: None,
            arguments: vec![],
        }];
        let client: Arc<dyn McpClient> = Arc::new(FakeClient {
            caps: ServerCaps {
                resources: false,
                prompts: true,
            },
            ..FakeClient::new(vec![])
        });
        let bridge = McpPromptBridge::new("srv", &prompts, client).expect("valid name");
        assert_eq!(bridge.name(), "mcp__srv__get_prompt");
        assert!(bridge.is_read_only());
        let out = bridge
            .execute(
                serde_json::json!({"name": "review", "arguments": {"path": "x.rs", "n": 1}}),
                &ctx(),
            )
            .await
            .unwrap();
        assert!(!out.is_error);
        assert!(out.content.contains("user: prompt review asks"));
        let out = bridge.execute(serde_json::json!({}), &ctx()).await.unwrap();
        assert!(out.is_error && out.content.contains("name"));
    }

    #[tokio::test]
    async fn bridge_server_registers_discovery_tools_only_when_capable() {
        let resources = vec![McpResourceDef {
            uri: "file:///a.txt".to_string(),
            name: "a".to_string(),
            description: None,
            mime_type: None,
        }];
        let prompts = vec![McpPromptDef {
            name: "review".to_string(),
            description: None,
            arguments: vec![],
        }];
        let capable = Arc::new(FakeClient {
            caps: ServerCaps {
                resources: true,
                prompts: true,
            },
            resources,
            prompts,
            ..FakeClient::new(vec![def("click")])
        });
        let registry = Arc::new(wavecode_tools::Registry::builtin());
        let count = bridge_server("srv", capable.clone(), capable.caps, &registry)
            .await
            .unwrap();
        // One tool plus the two discovery bridges.
        assert_eq!(count, 3);
        assert!(registry.get("mcp__srv__click").is_some());
        let reader = registry.get("mcp__srv__read_resource").expect("reader");
        assert!(reader.description().contains("file:///a.txt"));
        let out = reader
            .execute(serde_json::json!({"uri": "file:///a.txt"}), &ctx())
            .await
            .unwrap();
        assert_eq!(out.content, "hello");
        let getter = registry.get("mcp__srv__get_prompt").expect("getter");
        let out = getter
            .execute(serde_json::json!({"name": "review"}), &ctx())
            .await
            .unwrap();
        assert!(out.content.contains("user:"));

        // Without the capabilities only the tool itself registers.
        let plain_registry = Arc::new(wavecode_tools::Registry::builtin());
        let plain = Arc::new(FakeClient::new(vec![def("click")]));
        let count = bridge_server("srv", plain, ServerCaps::default(), &plain_registry)
            .await
            .unwrap();
        assert_eq!(count, 1);
        assert!(plain_registry.get("mcp__srv__read_resource").is_none());
        assert!(plain_registry.get("mcp__srv__get_prompt").is_none());
    }

    /// Minimal hand-rolled HTTP/1.1 stub over TCP (one connection per
    /// request): the handler maps the request's JSON-RPC method and
    /// `mcp-session-id` header to a status, extra headers, and body.
    /// Hermetic mirror of the stub style of the `transport-mcp` HTTP tests.
    type StubHandler =
        Arc<dyn Fn(&str, Option<&str>) -> (u16, Vec<(String, String)>, String) + Send + Sync>;

    async fn spawn_rpc_stub(handler: StubHandler) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((socket, _)) = listener.accept().await else {
                    return;
                };
                let handler = handler.clone();
                tokio::spawn(async move {
                    let (reader, mut writer) = socket.into_split();
                    let mut reader = tokio::io::BufReader::new(reader);
                    let mut line = String::new();
                    if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
                        return;
                    }
                    let mut headers = HashMap::new();
                    let mut content_length = 0usize;
                    loop {
                        line.clear();
                        if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
                            return;
                        }
                        let trimmed = line.trim_end();
                        if trimmed.is_empty() {
                            break;
                        }
                        if let Some((name, value)) = trimmed.split_once(':') {
                            let name = name.trim().to_ascii_lowercase();
                            if name == "content-length" {
                                content_length = value.trim().parse().unwrap_or(0);
                            }
                            headers.insert(name, value.trim().to_owned());
                        }
                    }
                    let mut body = vec![0u8; content_length];
                    if content_length > 0 && reader.read_exact(&mut body).await.is_err() {
                        return;
                    }
                    let method = serde_json::from_slice::<serde_json::Value>(&body)
                        .ok()
                        .and_then(|v| {
                            v.get("method")
                                .and_then(serde_json::Value::as_str)
                                .map(str::to_owned)
                        })
                        .unwrap_or_default();
                    let session = headers.get("mcp-session-id").cloned();
                    let (status, extra, resp_body) = handler(&method, session.as_deref());
                    let reason = match status {
                        200 => "OK",
                        202 => "Accepted",
                        404 => "Not Found",
                        _ => "OK",
                    };
                    let mut head = format!(
                        "HTTP/1.1 {status} {reason}\r\nContent-Length: {}\r\nConnection: close\r\n",
                        resp_body.len()
                    );
                    for (name, value) in &extra {
                        head.push_str(&format!("{name}: {value}\r\n"));
                    }
                    head.push_str("\r\n");
                    let _ = writer.write_all(head.as_bytes()).await;
                    let _ = writer.write_all(resp_body.as_bytes()).await;
                });
            }
        });
        format!("http://{addr}/mcp")
    }

    /// End-to-end session expiry over the local stub: the first
    /// post-handshake request 404s, the client re-initializes exactly once,
    /// and the retried listing succeeds with the fresh session.
    #[tokio::test]
    async fn http_client_reinitializes_once_on_session_404() {
        let initialize_hits = Arc::new(AtomicUsize::new(0));
        let counter = initialize_hits.clone();
        let handler: StubHandler = Arc::new(move |method, session| {
            let json_headers = vec![("content-type".to_owned(), "application/json".to_owned())];
            match method {
                "initialize" => {
                    let n = counter.fetch_add(1, Ordering::SeqCst) + 1;
                    let mut headers = json_headers;
                    headers.push(("mcp-session-id".to_owned(), format!("sess-{n}")));
                    let body = serde_json::json!({
                        "jsonrpc": "2.0", "id": 1,
                        "result": {
                            "protocolVersion": "2024-11-05",
                            "capabilities": {},
                            "serverInfo": {"name": "stub", "version": "0"},
                        },
                    })
                    .to_string();
                    (200, headers, body)
                }
                "notifications/initialized" => (202, vec![], String::new()),
                "tools/list" => {
                    if session == Some("sess-2") {
                        let body = serde_json::json!({
                            "jsonrpc": "2.0", "id": 2,
                            "result": {"tools": [
                                {"name": "click", "description": "clicks", "inputSchema": {"type": "object"}},
                            ]},
                        })
                        .to_string();
                        (200, json_headers, body)
                    } else {
                        (404, vec![], "session expired".to_owned())
                    }
                }
                _ => (400, vec![], "unknown method".to_owned()),
            }
        });
        let url = spawn_rpc_stub(handler).await;
        let registry = Arc::new(wavecode_tools::Registry::builtin());
        let count = connect_http("web", &url, HashMap::new(), None, &registry)
            .await
            .expect("connect succeeds after the re-initialize");
        assert_eq!(count, 1, "the retried tools/list bridges one tool");
        assert_eq!(
            initialize_hits.load(Ordering::SeqCst),
            2,
            "exactly one re-initialize after the 404"
        );
        assert!(registry.get("mcp__web__click").is_some());
    }
}
