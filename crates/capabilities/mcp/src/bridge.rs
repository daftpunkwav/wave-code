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
use std::time::{Duration, Instant};

use crate::{
    McpClient, McpError, McpPromptDef, McpPromptMessage, McpResourceContent, McpResourceDef,
    McpToolDef, McpToolOutput, try_tool_name,
};
use wavecode_tools::{Result, Tool, ToolCtx, ToolOutput};

/// Protocol version offered at `initialize`.
///
/// Keep in sync with the serve side's `operations_gateway::mcp_serve::
/// MCP_PROTOCOL_VERSION`: the two directions of one product must speak
/// the same MCP dialect. Duplicated on purpose — the client bridge
/// names no gateway types.
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
/// list/call drivers below are written once). The `Send + Sync` bound
/// lets [`ResilientMcpClient`] hold the live connection as
/// `Arc<dyn RpcClient>`.
#[async_trait::async_trait]
trait RpcClient: Send + Sync {
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

/// Capability-gated drivers shared by all three [`McpClient`] impls below:
/// each takes the caller's capability snapshot so the Stdio/Http clients
/// gate on their live handshake and [`ResilientMcpClient`] gates on its
/// `initial_caps` snapshot at the call site.

/// `prompts/list` walk behind the prompts capability gate.
async fn list_prompts_via<C>(
    client: &C,
    caps: ServerCaps,
) -> std::result::Result<Vec<McpPromptDef>, McpError>
where
    C: RpcClient + ?Sized,
{
    if !caps.prompts {
        return Ok(vec![]);
    }
    list_paged(client, "prompts/list", parse_prompts_list).await
}

/// `prompts/get` round trip behind the prompts capability gate.
async fn get_prompt_via<C>(
    client: &C,
    caps: ServerCaps,
    name: &str,
    arguments: HashMap<String, String>,
) -> std::result::Result<Vec<McpPromptMessage>, McpError>
where
    C: RpcClient + ?Sized,
{
    if !caps.prompts {
        return Ok(vec![]);
    }
    let payload = client
        .rpc(
            "prompts/get",
            serde_json::json!({"name": name, "arguments": arguments}),
        )
        .await?;
    parse_prompt_messages(&payload)
}

/// `resources/list` walk behind the resources capability gate.
async fn list_resources_via<C>(
    client: &C,
    caps: ServerCaps,
) -> std::result::Result<Vec<McpResourceDef>, McpError>
where
    C: RpcClient + ?Sized,
{
    if !caps.resources {
        return Ok(vec![]);
    }
    list_paged(client, "resources/list", parse_resources_list).await
}

/// `resources/read` round trip behind the resources capability gate.
async fn read_resource_via<C>(
    client: &C,
    caps: ServerCaps,
    uri: &str,
) -> std::result::Result<Vec<McpResourceContent>, McpError>
where
    C: RpcClient + ?Sized,
{
    if !caps.resources {
        return Ok(vec![]);
    }
    let payload = client
        .rpc("resources/read", serde_json::json!({"uri": uri}))
        .await?;
    parse_resource_contents(&payload)
}

/// Split one list-method page into its item array plus the next cursor.
///
/// `key` is the result field (`tools` / `resources` / `prompts`) and `what`
/// names it in the error message. A non-object payload or a non-array `key`
/// field is a protocol error; a missing or empty cursor reads as `None`.
fn parse_list_page<'a>(
    payload: &'a serde_json::Value,
    key: &str,
    what: &str,
) -> std::result::Result<(&'a Vec<serde_json::Value>, Option<String>), McpError> {
    let malformed = || McpError::Protocol(format!("malformed {what}/list result"));
    let result = payload.as_object().ok_or_else(malformed)?;
    let items = result
        .get(key)
        .and_then(|v| v.as_array())
        .ok_or_else(malformed)?;
    let cursor = result
        .get("nextCursor")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());
    Ok((items, cursor))
}

/// Parse one `tools/list` page into tool definitions plus a next cursor.
///
/// Items without a usable name are skipped (a server naming slip must not
/// poison the whole page); a non-object payload is a protocol error.
fn parse_tools_list(payload: &serde_json::Value) -> ListPage<McpToolDef> {
    let (tools, cursor) = parse_list_page(payload, "tools", "tools")?;
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
fn parse_resources_list(payload: &serde_json::Value) -> ListPage<McpResourceDef> {
    let (resources, cursor) = parse_list_page(payload, "resources", "resources")?;
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
fn parse_prompts_list(payload: &serde_json::Value) -> ListPage<McpPromptDef> {
    let (prompts, cursor) = parse_list_page(payload, "prompts", "prompts")?;
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
                        Some(crate::McpPromptArgument {
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
            // Well-formed server notifications (no id) skip like mismatched
            // frames; a genuinely malformed frame still fails the exchange.
            let Some(response) = transport
                .recv_message()
                .await
                .map_err(|e| transport_error("recv", e))?
            else {
                continue;
            };
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
        list_prompts_via(self, self.caps()).await
    }

    async fn get_prompt(
        &self,
        name: &str,
        arguments: HashMap<String, String>,
    ) -> std::result::Result<Vec<McpPromptMessage>, McpError> {
        get_prompt_via(self, self.caps(), name, arguments).await
    }

    async fn list_resources(&self) -> std::result::Result<Vec<McpResourceDef>, McpError> {
        list_resources_via(self, self.caps()).await
    }

    async fn read_resource(
        &self,
        uri: &str,
    ) -> std::result::Result<Vec<McpResourceContent>, McpError> {
        read_resource_via(self, self.caps(), uri).await
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
        list_prompts_via(self, self.caps()).await
    }

    async fn get_prompt(
        &self,
        name: &str,
        arguments: HashMap<String, String>,
    ) -> std::result::Result<Vec<McpPromptMessage>, McpError> {
        get_prompt_via(self, self.caps(), name, arguments).await
    }

    async fn list_resources(&self) -> std::result::Result<Vec<McpResourceDef>, McpError> {
        list_resources_via(self, self.caps()).await
    }

    async fn read_resource(
        &self,
        uri: &str,
    ) -> std::result::Result<Vec<McpResourceContent>, McpError> {
        read_resource_via(self, self.caps(), uri).await
    }
}

// ---- Demand-driven reconnection ----

/// How to rebuild a dropped connection to one server.
#[derive(Clone)]
enum ServerSpec {
    Stdio {
        name: String,
        command: String,
        args: Vec<String>,
        env: HashMap<String, String>,
    },
    Http {
        name: String,
        url: String,
        headers: HashMap<String, String>,
        oauth: Option<transport_mcp::http::OAuthClientCredentials>,
    },
    #[cfg(test)]
    Test(
        std::sync::Arc<dyn Fn() -> std::result::Result<Arc<dyn RpcClient>, McpError> + Send + Sync>,
    ),
}

impl ServerSpec {
    /// Open a fresh connection (spawn + handshake). The client surfaces
    /// as its request/response half: healing only needs to re-drive RPCs.
    async fn connect(&self) -> std::result::Result<(Arc<dyn RpcClient>, ServerCaps), McpError> {
        match self {
            Self::Stdio {
                name,
                command,
                args,
                env,
            } => {
                let client = StdioMcpClient::connect(name, command, args.clone(), env).await?;
                let caps = client.caps();
                Ok((Arc::new(client) as Arc<dyn RpcClient>, caps))
            }
            Self::Http {
                name: _,
                url,
                headers,
                oauth,
            } => {
                let client = HttpMcpClient::connect(url, headers.clone(), oauth.clone()).await?;
                let caps = client.caps();
                Ok((Arc::new(client) as Arc<dyn RpcClient>, caps))
            }
            #[cfg(test)]
            Self::Test(connect) => Ok((connect()?, ServerCaps::default())),
        }
    }

    /// Server name for error messages.
    fn name(&self) -> &str {
        match self {
            Self::Stdio { name, .. } | Self::Http { name, .. } => name,
            #[cfg(test)]
            Self::Test(_) => "test",
        }
    }
}

/// Cooldown base for the reconnect backoff: the first heal is immediate,
/// each consecutive transport failure doubles the wait (capped at 8s).
const RECONNECT_COOLDOWN_BASE_MS: u64 = 250;
const RECONNECT_COOLDOWN_MAX_SHIFTS: u32 = 5;

/// Exponential cooldown: `250ms << min(streak, 5)`.
fn reconnect_cooldown(streak: u32) -> Duration {
    Duration::from_millis(
        RECONNECT_COOLDOWN_BASE_MS
            .saturating_mul(1u64 << streak.min(RECONNECT_COOLDOWN_MAX_SHIFTS)),
    )
}

/// Consecutive-failure bookkeeping behind the reconnect cooldown.
#[derive(Default)]
struct HealState {
    streak: u32,
    last_heal: Option<Instant>,
}

/// [`McpClient`] decorator healing dropped connections on demand.
///
/// A transport failure triggers one reconnect (spawn/handshake) under the
/// client lock, gated by the reconnect cooldown below. Idempotent calls
/// retry on the fresh connection; `tools/call` never replays (see
/// [`ResilientMcpClient::once_replaying`]). Concurrent callers serialize
/// on the lock, so one healer serves them all — no background polling,
/// and a call that arrives while healing is under way simply waits for
/// the fresh connection. Protocol errors (bad frames, JSON-RPC errors)
/// never heal: the server is reachable and the problem is not the
/// connection. Capability gates use the first handshake's snapshot
/// (`connect`).
struct ResilientMcpClient {
    live: tokio::sync::Mutex<Arc<dyn RpcClient>>,
    spec: ServerSpec,
    initial_caps: ServerCaps,
    heal_state: tokio::sync::Mutex<HealState>,
}

impl ResilientMcpClient {
    /// Open the first connection; returns the wrapper plus that
    /// handshake's capability snapshot for bridge construction.
    async fn connect(spec: ServerSpec) -> std::result::Result<(Self, ServerCaps), McpError> {
        let (client, caps) = spec.connect().await?;
        Ok((
            Self {
                live: tokio::sync::Mutex::new(client),
                spec,
                initial_caps: caps,
                heal_state: tokio::sync::Mutex::new(HealState::default()),
            },
            caps,
        ))
    }

    /// Run `op` on the live client; on a transport failure heal once and —
    /// when `replay` allows — retry on the fresh connection.
    ///
    /// `replay` encodes idempotency: list/read calls can be safely re-issued,
    /// but a `tools/call` may have executed server-side before the response
    /// was lost, so replaying it could run a write twice. For those the
    /// connection is still healed (later calls get the fresh client) and the
    /// transport error surfaces unchanged — the same invariant
    /// `llm/src/retry.rs` applies to mid-stream tears.
    async fn once_replaying<F, Fut, T>(
        &self,
        op: F,
        replay: bool,
        method: &str,
    ) -> std::result::Result<T, McpError>
    where
        F: Fn(Arc<dyn RpcClient>) -> Fut,
        Fut: std::future::Future<Output = std::result::Result<T, McpError>>,
    {
        let client = self.live.lock().await.clone();
        match op(client).await {
            Ok(value) => {
                *self.heal_state.lock().await = HealState::default();
                Ok(value)
            }
            Err(McpError::Transport(reason)) => {
                // Back the heal off: a dead server must not cost a
                // full spawn + `initialize` handshake on every call.
                {
                    let mut state = self.heal_state.lock().await;
                    let cooldown = reconnect_cooldown(state.streak);
                    if state.last_heal.is_some_and(|t| t.elapsed() < cooldown) {
                        return Err(McpError::Transport(format!(
                            "reconnect to MCP server {} is cooling down for another {cooldown:?}: {reason}",
                            self.spec.name()
                        )));
                    }
                    state.streak = state.streak.saturating_add(1);
                    state.last_heal = Some(Instant::now());
                }
                let mut guard = self.live.lock().await;
                let fresh = self.spec.connect().await.map_err(|e| {
                    McpError::Transport(format!(
                        "reconnect to MCP server {} failed: {e} (initial failure: {reason})",
                        self.spec.name()
                    ))
                })?;
                *guard = fresh.0;
                if replay {
                    op(guard.clone()).await
                } else {
                    Err(McpError::Transport(format!(
                        "transport to MCP server {} failed during {method}; the call was \
                         not replayed because the server may have already executed it: \
                         {reason}",
                        self.spec.name()
                    )))
                }
            }
            Err(other) => Err(other),
        }
    }
}

#[async_trait::async_trait]
impl RpcClient for ResilientMcpClient {
    async fn rpc(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> std::result::Result<serde_json::Value, McpError> {
        let replay = method != "tools/call";
        self.once_replaying(
            |client| {
                let params = params.clone();
                async move { client.rpc(method, params).await }
            },
            replay,
            method,
        )
        .await
    }
}

#[async_trait::async_trait]
impl McpClient for ResilientMcpClient {
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
        list_prompts_via(self, self.initial_caps).await
    }

    async fn get_prompt(
        &self,
        name: &str,
        arguments: HashMap<String, String>,
    ) -> std::result::Result<Vec<McpPromptMessage>, McpError> {
        get_prompt_via(self, self.initial_caps, name, arguments).await
    }

    async fn list_resources(&self) -> std::result::Result<Vec<McpResourceDef>, McpError> {
        list_resources_via(self, self.initial_caps).await
    }

    async fn read_resource(
        &self,
        uri: &str,
    ) -> std::result::Result<Vec<McpResourceContent>, McpError> {
        read_resource_via(self, self.initial_caps, uri).await
    }
}

/// One MCP server tool as a registry tool.
pub struct McpToolBridge {
    name: String,
    description: String,
    input_schema: serde_json::Value,
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

    /// Never trusts the server's `readOnlyHint`: the declaration is
    /// third-party input, and a lying server could mark writes read-only
    /// to slip past the approval gate (`read_only && !destructive` auto-
    /// allows in the sandbox). Unknown effects keep the approval path.
    fn is_read_only(&self) -> bool {
        false
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
/// conversion stays on the core side); the prompt
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

/// Drive one connect under the whole-connect timeout and format the status
/// line plus optional degradation warning both connect paths share.
async fn connect_within_budget(
    connect: impl std::future::Future<Output = std::result::Result<usize, McpError>>,
    name: &str,
    summary: &str,
) -> (String, Option<String>) {
    match tokio::time::timeout(
        std::time::Duration::from_secs(MCP_CONNECT_TIMEOUT_SECS),
        connect,
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

/// Connect one server; returns its status line plus an optional warning.
async fn connect_one(
    name: &str,
    raw: &wavecode_config::McpServerRaw,
    registry: &Arc<wavecode_tools::Registry>,
) -> (String, Option<String>) {
    // Name validity, the either-or rule, and endpoint validation all live
    // in one place (`McpServerConfig::from_raw`) so this connect path and
    // doctor report the same verdicts.
    let config = match crate::McpServerConfig::from_raw(name, raw) {
        Ok(config) => config,
        Err(reason) => return (format!("{name} — skipped ({reason})"), Some(reason)),
    };
    match config {
        crate::McpServerConfig::Http {
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
            connect_within_budget(
                connect_http(name, &url, headers, oauth, registry),
                name,
                &summary,
            )
            .await
        }
        crate::McpServerConfig::Stdio { command, args, env } => {
            let summary = crate::McpServerConfig::Stdio {
                command: command.clone(),
                args: args.clone(),
                env: HashMap::new(),
            }
            .summary();
            connect_within_budget(
                connect_stdio(name, &command, args, &env, registry),
                name,
                &summary,
            )
            .await
        }
    }
}

/// Register a connected client's tools plus capability-gated discovery
/// bridges; returns the number of registered registry tools.
///
/// Discovery tools (`read_resource` / `get_prompt`) only register when the
/// server advertised the capability; a listing that fails despite the
/// advertisement skips that one tool quietly (same scoped degradation as a
/// single malformed tool item). A discovery bridge never overwrites a tool
/// the server itself listed under the same name — the registry replaces on
/// register, and the server's explicit tool is the one the model was shown.
async fn bridge_server(
    name: &str,
    client: Arc<dyn McpClient>,
    caps: ServerCaps,
    registry: &Arc<wavecode_tools::Registry>,
) -> std::result::Result<usize, McpError> {
    let mut count = 0;
    let mut bridged: std::collections::HashSet<String> = std::collections::HashSet::new();
    for def in &client.list_tools().await? {
        // Empty tool names never reach the registry; the handshake told
        // us the server speaks, so one bad item skips quietly.
        if let Some(bridge) = McpToolBridge::new(name, def, client.clone()) {
            bridged.insert(bridge.name().to_owned());
            registry.register(Arc::new(bridge));
            count += 1;
        }
    }
    if caps.resources
        && let Ok(resources) = client.list_resources().await
        && let Some(bridge) = McpResourceBridge::new(name, &resources, client.clone())
        && bridged.insert(bridge.name().to_owned())
    {
        registry.register(Arc::new(bridge));
        count += 1;
    }
    if caps.prompts
        && let Ok(prompts) = client.list_prompts().await
        && let Some(bridge) = McpPromptBridge::new(name, &prompts, client.clone())
        && bridged.insert(bridge.name().to_owned())
    {
        registry.register(Arc::new(bridge));
        count += 1;
    }
    Ok(count)
}

/// Handshake, list, and bridge one stdio server; returns bridged count.
/// The connection is demand-healed: a dropped child respawns on the
/// next tool call.
async fn connect_stdio(
    name: &str,
    command: &str,
    args: Vec<String>,
    env: &HashMap<String, String>,
    registry: &Arc<wavecode_tools::Registry>,
) -> std::result::Result<usize, McpError> {
    let spec = ServerSpec::Stdio {
        name: name.to_string(),
        command: command.to_string(),
        args,
        env: env.clone(),
    };
    let (client, caps) = ResilientMcpClient::connect(spec).await?;
    bridge_server(name, Arc::new(client), caps, registry).await
}

/// Handshake, list, and bridge one HTTP server; returns bridged count.
/// The connection is demand-healed: a session the server dropped
/// re-establishes on the next tool call.
async fn connect_http(
    name: &str,
    url: &str,
    headers: HashMap<String, String>,
    oauth: Option<transport_mcp::http::OAuthClientCredentials>,
    registry: &Arc<wavecode_tools::Registry>,
) -> std::result::Result<usize, McpError> {
    let spec = ServerSpec::Http {
        name: name.to_string(),
        url: url.to_string(),
        headers,
        oauth,
    };
    let (client, caps) = ResilientMcpClient::connect(spec).await?;
    bridge_server(name, Arc::new(client), caps, registry).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Scripted transport: pops queued rpc outcomes; each construction
    /// bumps `connects` so tests can observe the healing count.
    struct ScriptedRpc {
        outcomes: std::sync::Mutex<VecDeque<std::result::Result<serde_json::Value, McpError>>>,
    }

    #[async_trait::async_trait]
    impl RpcClient for ScriptedRpc {
        async fn rpc(
            &self,
            _method: &str,
            _params: serde_json::Value,
        ) -> std::result::Result<serde_json::Value, McpError> {
            self.outcomes
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .pop_front()
                .unwrap_or_else(|| Ok(serde_json::json!({})))
        }
    }

    /// One scripted connection: the nth connect (0 = initial) gets the
    /// outcomes its factory slot returns, then empty results.
    fn scripted_connect(
        make_outcomes: std::sync::Arc<
            dyn Fn(usize) -> Vec<std::result::Result<serde_json::Value, McpError>> + Send + Sync,
        >,
        connects: Arc<AtomicUsize>,
    ) -> ServerSpec {
        ServerSpec::Test(std::sync::Arc::new(move || {
            let outcomes = make_outcomes(connects.fetch_add(1, Ordering::SeqCst));
            Ok(Arc::new(ScriptedRpc {
                outcomes: std::sync::Mutex::new(VecDeque::from(outcomes)),
            }) as Arc<dyn RpcClient>)
        }))
    }

    #[tokio::test]
    async fn transport_failure_heals_once_and_retries_idempotent_calls() {
        let connects = Arc::new(AtomicUsize::new(0));
        // First connection fails the call with a transport error; the
        // heal reconnects (connects: 2) and the retry answers.
        let make = |n: usize| {
            if n == 0 {
                vec![Err(McpError::Transport("child died".to_string()))]
            } else {
                vec![Ok(serde_json::json!({"fresh": true}))]
            }
        };
        let (client, _) = ResilientMcpClient::connect(scripted_connect(
            std::sync::Arc::new(make),
            connects.clone(),
        ))
        .await
        .unwrap();
        // tools/list is idempotent, so the healed retry may replay it.
        let answer = client
            .rpc("tools/list", serde_json::json!({}))
            .await
            .unwrap();
        assert_eq!(answer["fresh"], true);
        assert_eq!(
            connects.load(Ordering::SeqCst),
            2,
            "one heal reconnects once"
        );
    }

    /// Consecutive transport failures back the heal off: inside the
    /// cooldown window the second call refuses to reconnect; once the
    /// window passes, the heal happens again.
    #[tokio::test]
    async fn reconnect_backs_off_while_the_cooldown_runs() {
        let connects = Arc::new(AtomicUsize::new(0));
        // Every connection fails its first three calls, so a call that
        // lands on the same connection keeps the failure streak alive;
        // the third connection finally answers.
        let make = move |n: usize| {
            if n <= 1 {
                vec![
                    Err(McpError::Transport("down".to_string())),
                    Err(McpError::Transport("down".to_string())),
                    Err(McpError::Transport("down".to_string())),
                ]
            } else {
                vec![]
            }
        };
        let (client, _) = ResilientMcpClient::connect(scripted_connect(
            std::sync::Arc::new(make),
            connects.clone(),
        ))
        .await
        .unwrap();
        // First call: immediate heal (connects: 2), whose retry also fails.
        assert!(
            client
                .rpc("tools/list", serde_json::json!({}))
                .await
                .is_err()
        );
        assert_eq!(connects.load(Ordering::SeqCst), 2);
        // Second call inside the cooldown: refused without a reconnect.
        let error = client
            .rpc("tools/list", serde_json::json!({}))
            .await
            .unwrap_err();
        assert!(
            matches!(&error, McpError::Transport(reason) if reason.contains("cooling down")),
            "the cooldown must gate the heal: {error}"
        );
        assert_eq!(
            connects.load(Ordering::SeqCst),
            2,
            "no reconnect inside the cooldown window"
        );
        // Once the window passes the heal happens again (connects: 3) and
        // the fresh connection answers.
        tokio::time::sleep(Duration::from_millis(600)).await;
        let answer = client
            .rpc("tools/list", serde_json::json!({}))
            .await
            .unwrap();
        assert_eq!(answer, serde_json::json!({}));
        assert_eq!(
            connects.load(Ordering::SeqCst),
            3,
            "the expired cooldown allows the heal"
        );
    }

    /// Cooldown doubles per consecutive failure and caps at the shift
    /// bound (250ms base, 8s ceiling).
    #[test]
    fn reconnect_cooldown_doubles_and_caps() {
        assert_eq!(reconnect_cooldown(0), Duration::from_millis(250));
        assert_eq!(reconnect_cooldown(1), Duration::from_millis(500));
        assert_eq!(reconnect_cooldown(2), Duration::from_millis(1_000));
        assert_eq!(
            reconnect_cooldown(50),
            Duration::from_millis(250 * (1 << RECONNECT_COOLDOWN_MAX_SHIFTS))
        );
    }

    /// `tools/call` is never replayed after a transport failure: the server
    /// may have already executed the tool, so a replay could run a write
    /// twice. The connection still heals for later calls.
    #[tokio::test]
    async fn tools_call_is_not_replayed_after_transport_failure() {
        let connects = Arc::new(AtomicUsize::new(0));
        let make = |n: usize| {
            if n == 0 {
                vec![Err(McpError::Transport("child died".to_string()))]
            } else {
                vec![Ok(serde_json::json!({"fresh": true}))]
            }
        };
        let (client, _) = ResilientMcpClient::connect(scripted_connect(
            std::sync::Arc::new(make),
            connects.clone(),
        ))
        .await
        .unwrap();
        let error = client
            .rpc("tools/call", serde_json::json!({}))
            .await
            .unwrap_err();
        assert!(
            matches!(&error, McpError::Transport(reason) if reason.contains("not replayed")),
            "the transport error must surface without a replay: {error}"
        );
        assert_eq!(
            connects.load(Ordering::SeqCst),
            2,
            "the heal still reconnects for later calls"
        );
    }

    #[tokio::test]
    async fn protocol_failure_surfaces_without_healing() {
        let connects = Arc::new(AtomicUsize::new(0));
        let make = |_n: usize| vec![Err(McpError::Protocol("bad frame".to_string()))];
        let (client, _) = ResilientMcpClient::connect(scripted_connect(
            std::sync::Arc::new(make),
            connects.clone(),
        ))
        .await
        .unwrap();
        let error = client
            .rpc("tools/call", serde_json::json!({}))
            .await
            .unwrap_err();
        assert!(matches!(error, McpError::Protocol(_)));
        assert_eq!(
            connects.load(Ordering::SeqCst),
            1,
            "protocol errors never heal"
        );
    }

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
    fn bridge_rejects_invalid_servers_and_distrusts_read_only_hint() {
        let client: Arc<dyn McpClient> = Arc::new(FakeClient::new(vec![]));
        assert!(McpToolBridge::new("a__b", &def("go"), client.clone()).is_none());
        let mut ro = def("scan");
        ro.description = None;
        ro.read_only_hint = true;
        let bridge = McpToolBridge::new("srv", &ro, client).expect("valid name");
        // The server's own readOnlyHint must not flip the bridge to
        // read-only: a lying server could otherwise bypass the approval
        // gate for writes.
        assert!(!bridge.is_read_only());
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
            arguments: vec![crate::McpPromptArgument {
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

    /// A server may list a tool named exactly like a discovery bridge
    /// (`read_resource` / `get_prompt`). The registry replaces on register,
    /// so a later discovery bridge would silently overwrite the server's own
    /// tool; the explicit tool must win and the discovery surface stays out.
    #[tokio::test]
    async fn discovery_bridges_never_shadow_same_named_tools() {
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
        let client = Arc::new(FakeClient {
            caps: ServerCaps {
                resources: true,
                prompts: true,
            },
            resources,
            prompts,
            ..FakeClient::new(vec![def("read_resource"), def("get_prompt")])
        });
        let registry = Arc::new(wavecode_tools::Registry::builtin());
        let count = bridge_server("srv", client.clone(), client.caps, &registry)
            .await
            .unwrap();
        // Two explicit tools only; neither discovery bridge registered.
        assert_eq!(count, 2);
        let out = registry
            .get("mcp__srv__read_resource")
            .expect("the server's own read_resource tool stays bridged")
            .execute(serde_json::json!({}), &ctx())
            .await
            .unwrap();
        assert!(
            out.content.contains("ran read_resource"),
            "the explicit tool answers, not the resource bridge: {}",
            out.content
        );
        let out = registry
            .get("mcp__srv__get_prompt")
            .expect("the server's own get_prompt tool stays bridged")
            .execute(serde_json::json!({}), &ctx())
            .await
            .unwrap();
        assert!(
            out.content.contains("ran get_prompt"),
            "the explicit tool answers, not the prompt bridge: {}",
            out.content
        );
    }

    /// Handler shape the HTTP tests reason in: JSON-RPC method plus the
    /// `mcp-session-id` request header, answered with a status, extra
    /// headers, and a UTF-8 body.
    type StubHandler =
        Arc<dyn Fn(&str, Option<&str>) -> (u16, Vec<(String, String)>, String) + Send + Sync>;

    /// Serve one HTTP stub over the transport crate's shared test server:
    /// the shared socket plumbing records requests; this adapter keeps the
    /// method/session handler shape the tests below are written in.
    async fn spawn_rpc_stub(handler: StubHandler) -> String {
        let server = transport_mcp::test_support::StubServer::spawn(Arc::new(move |request| {
            let session = request.headers.get("mcp-session-id").map(String::as_str);
            let (status, extra, body) = handler(&request.rpc_method(), session);
            // Echo the request's own id into any JSON-RPC body: the client
            // correlates responses by id, so a handler's hardcoded id never
            // matches once the client's id counter has moved past it.
            let request_id = serde_json::from_slice::<serde_json::Value>(&request.body)
                .ok()
                .and_then(|value| value.get("id").cloned());
            let mut body = body;
            if let (Some(request_id), Ok(mut value)) =
                (request_id, serde_json::from_str::<serde_json::Value>(&body))
                && let Some(object) = value.as_object_mut()
                && object.contains_key("id")
            {
                object.insert("id".to_string(), request_id);
                body = value.to_string();
            }
            (status, extra, body.into_bytes())
        }))
        .await;
        server.url("/mcp")
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
