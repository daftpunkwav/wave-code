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
use wavecode_tools::{Result, Tool, ToolCtx, ToolOutput, is_sensitive_env_name};

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
/// `initial_caps` snapshot at the call site. `prompts/list` walks behind
/// the prompts capability gate.
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

/// Names to strip from a spawned stdio server's inherited environment:
/// every sensitive-shaped variable of this process (see
/// [`is_sensitive_env_name`]). A server process is third-party code that
/// may phone home, so it gets the same scrub a model-driven shell command
/// gets; the transport applies the config `env` block after the strip, so
/// a server that deliberately needs a secret-shaped variable still
/// declares it through its config.
fn sensitive_env_strip() -> Vec<String> {
    std::env::vars_os()
        .map(|(key, _)| key.to_string_lossy().into_owned())
        .filter(|name| is_sensitive_env_name(name))
        .collect()
}

impl StdioMcpClient {
    /// Spawn the server and run the `initialize` handshake.
    pub async fn connect(
        server: &str,
        command: &str,
        args: Vec<String>,
        env: &HashMap<String, String>,
    ) -> std::result::Result<Self, McpError> {
        let strip_env = sensitive_env_strip();
        let transport = transport_mcp::ChildTransport::spawn_with_env(
            command,
            args,
            env,
            &strip_env,
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
/// conversion is not wired into the composition root yet); the prompt
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
    // Status output carries the host only (no userinfo/query in the url,
    // no env values in the command line).
    let summary = config.summary();
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
            connect_within_budget(
                connect_http(name, &url, headers, oauth, registry),
                name,
                &summary,
            )
            .await
        }
        crate::McpServerConfig::Stdio { command, args, env } => {
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
mod bridge_tests;
