//! wavecode-mcp — two-way Model Context Protocol support.
//!
//! - Client: stdio and streamable-http transports, with external tools
//!   injected into the tool registry under the `mcp__{server}__{tool}`
//!   namespace (the client bridge lives in [`crate::bridge`]);
//! - Server: exposes WaveCode's own capabilities as an MCP server for other
//!   agents / IDEs to call.
//!
//! **Scope**: this crate owns the interface boundary — the [`McpClient`] /
//! [`McpServerHandler`] traits, the data types, and the naming convention /
//! [`McpServerConfig`] config type — plus the real client implementations
//! over `transport-mcp` (stdio child process, streamable-http with session
//! handling) and the bridge that injects MCP tools into the registry. The
//! byte-level framing stays in `transport-mcp`; the serving side of MCP
//! (exposing WaveCode's tools over stdio) is the serving skin in
//! `operations-gateway`, with executor construction in the composition
//! root. The client trait surface covers `tools/list`, `tools/call`,
//! `resources/list`, `resources/read`, `prompts/list`, and `prompts/get`;
//! interactive browser/PKCE OAuth stays out (static headers or the
//! client-credentials grant only).

use std::collections::HashMap;

use serde_json::Value;

/// Name prefix for MCP tools injected into the registry:
/// the full tool name is `mcp__{server}__{tool}`, which never collides with
/// builtin tools.
pub const MCP_TOOL_PREFIX: &str = "mcp__";

/// Separator between the server name and the tool name ([`parse_tool_name`]
/// splits on the first separator — a tool name containing `__` survives the
/// round trip, while a server name must not contain `__`, enforced by
/// [`McpServerConfig::from_raw`] at assembly time).
pub const NAME_SEPARATOR: &str = "__";

/// Assemble a registry tool name: `mcp__{server}__{tool}`.
pub fn tool_name(server: &str, tool: &str) -> String {
    format!("{MCP_TOOL_PREFIX}{server}{NAME_SEPARATOR}{tool}")
}

/// Check whether a server name is valid for registry names: non-empty and
/// free of `__` (a separator inside the server segment would break the
/// `tool_name` / `parse_tool_name` round-trip; enforced by the assembly
/// layer, exposed here for pre-validation).
pub fn is_valid_server_name(name: &str) -> bool {
    !name.is_empty() && !name.contains(NAME_SEPARATOR)
}

/// Validated registry-name assembly: returns `Some(mcp__{server}__{tool})`
/// when `server` is valid (see [`is_valid_server_name`]) and `tool` is
/// non-empty, else `None`. Validating counterpart of [`tool_name`], which is
/// kept as-is for compatibility and performs no checks.
pub fn try_tool_name(server: &str, tool: &str) -> Option<String> {
    if !is_valid_server_name(server) || tool.is_empty() {
        return None;
    }
    Some(tool_name(server, tool))
}

/// Split a registry tool name into `(server, tool)`; returns `None` for a
/// missing `mcp__` prefix, a missing separator, or an empty segment on either
/// side.
pub fn parse_tool_name(name: &str) -> Option<(&str, &str)> {
    let rest = name.strip_prefix(MCP_TOOL_PREFIX)?;
    let (server, tool) = rest.split_once(NAME_SEPARATOR)?;
    if server.is_empty() || tool.is_empty() {
        return None;
    }
    Some((server, tool))
}

/// A tool definition exposed by an MCP server (mirrors a protocol
/// `tools/list` result item).
#[derive(Debug, Clone, PartialEq)]
pub struct McpToolDef {
    /// Original server-side tool name (**without** the `mcp__` prefix;
    /// `call_tool` uses this name).
    pub name: String,
    /// Capability description (optional in the protocol; the bridge layer
    /// falls back to placeholder text when missing).
    pub description: Option<String>,
    /// Parameter JSON Schema (`inputSchema`, injected into sampling requests).
    pub input_schema: Value,
    /// Server-side `annotations.readOnlyHint`: bridged tools are marked
    /// read-only when true. Defaults to false (unknown side effects are
    /// always treated as writes and go through approvals).
    pub read_only_hint: bool,
}

impl McpToolDef {
    /// Registry-qualified name of this tool (`mcp__{server}__{name}`);
    /// returns `None` when `server` is invalid (see [`try_tool_name`]).
    pub fn qualified_name(&self, server: &str) -> Option<String> {
        try_tool_name(server, &self.name)
    }
}

/// An MCP tool call result (mirrors a protocol `tools/call` result).
///
/// The first version is flat text: the protocol's content-block array (text /
/// image / resource) becomes structured when the real transport lands, at
/// which point the bridge layer splices the blocks into `content`.
#[derive(Debug, Clone, PartialEq)]
pub struct McpToolOutput {
    /// Human-readable output (fed back to the model).
    pub content: String,
    /// Business-failure flag (protocol `isError`; distinct from
    /// transport/protocol-level failures, which travel as [`McpError`]).
    pub is_error: bool,
}

/// A parameter definition for an MCP prompt (an `arguments` item of protocol
/// `prompts/list`).
#[derive(Debug, Clone, PartialEq)]
pub struct McpPromptArgument {
    /// Parameter name.
    pub name: String,
    /// Parameter description (optional).
    pub description: Option<String>,
    /// Whether it is required.
    pub required: bool,
}

/// A prompt definition exposed by an MCP server (mirrors a protocol
/// `prompts/list` result item).
///
/// The protocol requires prompts to auto-convert into inline skills: the
/// conversion needs `prompts/get` content fetches, which depend on a real
/// transport and are not wired into the composition root yet (the skills
/// crate already carries the `SkillSource::Mcp` source placeholder).
#[derive(Debug, Clone, PartialEq)]
pub struct McpPromptDef {
    /// Prompt name.
    pub name: String,
    /// Description (optional).
    pub description: Option<String>,
    /// Parameter list.
    pub arguments: Vec<McpPromptArgument>,
}

/// A resource exposed by an MCP server (mirrors a protocol `resources/list`
/// result item).
#[derive(Debug, Clone, PartialEq)]
pub struct McpResourceDef {
    /// Resource URI (the `resources/read` key, e.g. `file:///logs/app.log`).
    pub uri: String,
    /// Human-readable name (defaults to the URI when the server omits it).
    pub name: String,
    /// Description (optional).
    pub description: Option<String>,
    /// MIME type (optional).
    pub mime_type: Option<String>,
}

/// One `contents` entry of a protocol `resources/read` result.
///
/// First version is text-only: a `blob` (base64) entry surfaces as an
/// omission note at the bridge layer, matching the non-text content-block
/// handling of tool outputs.
#[derive(Debug, Clone, PartialEq)]
pub struct McpResourceContent {
    /// URI of the resource this content belongs to.
    pub uri: String,
    /// MIME type (optional).
    pub mime_type: Option<String>,
    /// Text payload; binary (`blob`) entries carry an omission note instead.
    pub text: String,
}

/// One message of a protocol `prompts/get` result.
///
/// First version is text-only: a non-text content block surfaces as an
/// omission note at the bridge layer.
#[derive(Debug, Clone, PartialEq)]
pub struct McpPromptMessage {
    /// Message role (`user` or `assistant`).
    pub role: String,
    /// Text payload of the message content.
    pub text: String,
}

/// MCP client errors.
///
/// Only transport / protocol-level failures travel as `Err`; tool business
/// failures are expressed by the server via [`McpToolOutput::is_error`], and
/// implementations must not wrap business failures as `Err` (aligned with the
/// wavecode `Tool::execute` error contract).
#[derive(Debug, thiserror::Error)]
pub enum McpError {
    /// Transport-layer failure (dropped connection, exited process, HTTP
    /// error, timeout, …).
    #[error("MCP transport error: {0}")]
    Transport(String),
    /// Protocol-layer failure (failed init handshake, unparseable response,
    /// server-returned JSON-RPC error, …).
    #[error("MCP protocol error: {0}")]
    Protocol(String),
}

/// MCP client: the session surface to one external MCP server.
///
/// Methods mirror the protocol capability surface (`tools/list` /
/// `tools/call` / `resources/*` / `prompts/*`); object-safe + Send + Sync,
/// held by the bridge layer as `Arc<dyn McpClient>`.
///
/// **Implementation status**: real stdio and streamable-http implementations
/// live in this crate's [`bridge`] module (`StdioMcpClient` /
/// `HttpMcpClient`); streamable-http re-initializes once when the server
/// expires a session (404). Demand-driven reconnection on dropped
/// connections lives in the bridge's private `ResilientMcpClient`, which
/// wraps either client without changing this trait surface.
#[async_trait::async_trait]
pub trait McpClient: Send + Sync {
    /// List every tool the server exposes (`tools/list`).
    async fn list_tools(&self) -> Result<Vec<McpToolDef>, McpError>;
    /// Call a tool (`tools/call`; `name` is the server-side original, without
    /// the `mcp__{server}__` prefix — the prefix is only a registry namespace).
    async fn call_tool(&self, name: &str, input: Value) -> Result<McpToolOutput, McpError>;
    /// List the prompts the server exposes (`prompts/list`). Optional
    /// capability: the default impl returns an empty list when the server
    /// does not support prompts.
    async fn list_prompts(&self) -> Result<Vec<McpPromptDef>, McpError> {
        Ok(vec![])
    }
    /// Render one prompt (`prompts/get`; `arguments` maps prompt argument
    /// names to their string values). Optional capability: the default impl
    /// returns no messages when the server does not support prompts.
    async fn get_prompt(
        &self,
        _name: &str,
        _arguments: HashMap<String, String>,
    ) -> Result<Vec<McpPromptMessage>, McpError> {
        Ok(vec![])
    }
    /// List the resources the server exposes (`resources/list`). Optional
    /// capability: the default impl returns an empty list when the server
    /// does not support resources.
    async fn list_resources(&self) -> Result<Vec<McpResourceDef>, McpError> {
        Ok(vec![])
    }
    /// Read one resource by URI (`resources/read`). Optional capability:
    /// the default impl returns no contents when the server does not
    /// support resources.
    async fn read_resource(&self, _uri: &str) -> Result<Vec<McpResourceContent>, McpError> {
        Ok(vec![])
    }
}

/// MCP server config (the two shapes of the
/// `[mcp_servers.<name>]` section).
///
/// Conversion from the config crate's raw tables happens on the connect
/// path ([`bridge::connect_all`] and its per-server `connect_one`), which
/// also enforces the either-or rule (stdio `command` vs http `url`); the
/// config crate has no workspace-internal dependencies and only does raw
/// parsing.
#[derive(Debug, Clone, PartialEq)]
pub enum McpServerConfig {
    /// stdio transport: spawn a child process, exchanging JSON-RPC over
    /// standard input/output.
    Stdio {
        /// Executable command (e.g. `npx`).
        command: String,
        /// Command arguments.
        args: Vec<String>,
        /// Extra environment variables injected into the child process
        /// (overriding the inherited environment).
        env: HashMap<String, String>,
    },
    /// streamable-http transport (incl. OAuth 2.0 client-credentials).
    Http {
        /// Server endpoint URL.
        url: String,
        /// Extra request headers (e.g. a pre-provisioned `Authorization`
        /// value; a static `Authorization` header wins over OAuth).
        headers: HashMap<String, String>,
        /// OAuth token endpoint URL (client-credentials grant). Set together
        /// with `oauth_client_id` + `oauth_client_secret`, or not at all
        /// (see `validate`).
        // nosemgrep: codacy.yaml.security.hard-coded-tokens
        oauth_token_url: Option<String>,
        /// OAuth client id (client-credentials grant).
        oauth_client_id: Option<String>,
        /// OAuth client secret (client-credentials grant; never logged).
        oauth_client_secret: Option<String>,
        /// Optional OAuth scope to request.
        oauth_scope: Option<String>,
    },
}

impl McpServerConfig {
    /// Transport kind name (for `/mcp` status display and diagnostics).
    pub fn transport_kind(&self) -> &'static str {
        match self {
            Self::Stdio { .. } => "stdio",
            Self::Http { .. } => "http",
        }
    }

    /// Convert one raw `[mcp_servers.<name>]` entry into the validated
    /// config shape, enforcing the full entry contract in one place:
    /// the server name must be valid ([`is_valid_server_name`]), exactly
    /// one of `command` / `url` must be set, and the endpoint details
    /// must pass [`Self::validate`]. `Err` carries the skip reason.
    ///
    /// Single definition of the raw→typed conversion: the connect path
    /// (`bridge::connect_one`) and `doctor` report from the same
    /// function, so their verdicts cannot drift.
    pub fn from_raw(name: &str, raw: &wavecode_config::McpServerRaw) -> Result<Self, String> {
        if !is_valid_server_name(name) {
            return Err(format!(
                "invalid MCP server name {name:?}: must be non-empty without `__`"
            ));
        }
        let config = match (&raw.command, &raw.url) {
            (Some(_), Some(_)) => {
                return Err(format!(
                    "MCP server {name:?} sets both command and url; use one"
                ));
            }
            (Some(command), None) => Self::Stdio {
                command: command.clone(),
                args: raw.args.clone(),
                env: raw.env.clone(),
            },
            (None, Some(url)) => Self::Http {
                url: url.clone(),
                headers: raw.headers.clone(),
                // nosemgrep: codacy.yaml.security.hard-coded-tokens
                oauth_token_url: raw.oauth_token_url.clone(),
                oauth_client_id: raw.oauth_client_id.clone(),
                oauth_client_secret: raw.oauth_client_secret.clone(),
                oauth_scope: raw.oauth_scope.clone(),
            },
            (None, None) => {
                return Err(format!("MCP server {name:?} sets neither command nor url"));
            }
        };
        config.validate()?;
        Ok(config)
    }

    /// Validate the server endpoint configuration: stdio requires a
    /// non-empty `command`; http requires a non-empty `url` starting with
    /// `http://` or `https://`, and an OAuth block whose token endpoint
    /// satisfies the transport's https/loopback policy. Returns `Err`
    /// with the reason so the assembly layer can surface it as a
    /// configuration error.
    pub fn validate(&self) -> Result<(), String> {
        match self {
            Self::Stdio { command, .. } => {
                if command.trim().is_empty() {
                    return Err("stdio MCP server command must not be empty".to_owned());
                }
                Ok(())
            }
            Self::Http {
                url,
                oauth_token_url,
                oauth_client_id,
                oauth_client_secret,
                ..
            } => {
                let trimmed = url.trim();
                if trimmed.is_empty() {
                    return Err("http MCP server url must not be empty".to_owned());
                }
                if !trimmed.starts_with("http://") && !trimmed.starts_with("https://") {
                    return Err(format!(
                        "http MCP server url must start with http:// or https://: {trimmed}"
                    ));
                }
                let triple = [
                    oauth_token_url.is_some(),
                    oauth_client_id.is_some(),
                    oauth_client_secret.is_some(),
                ];
                if triple.iter().any(|set| *set) && triple.iter().any(|set| !set) {
                    return Err("OAuth client-credentials needs all of `oauth_token_url`, \
                         `oauth_client_id`, `oauth_client_secret` (or none)"
                        .to_owned());
                }
                // Same policy the transport enforces on connect, applied
                // here so doctor and assembly surface the reason up front
                // instead of at first use.
                if let Some(token_url) = oauth_token_url {
                    transport_mcp::http::validate_token_url(token_url)
                        .map_err(|e| format!("oauth_token_url: {e}"))?;
                    transport_mcp::http::validate_oauth_endpoint(trimmed)
                        .map_err(|e| format!("oauth_endpoint: {e}"))?;
                }
                Ok(())
            }
        }
    }

    /// One-line summary (the `/mcp` display surface): command + args joined
    /// for stdio, the URL for http. env / headers never display (they may
    /// hold secrets).
    pub fn summary(&self) -> String {
        match self {
            Self::Stdio { command, args, .. } => {
                let mut line = command.clone();
                for arg in args {
                    line.push(' ');
                    line.push_str(arg);
                }
                format!("stdio: {line}")
            }
            Self::Http { url, .. } => format!("http: {url}"),
        }
    }
}

/// MCP server-side placeholder trait.
///
/// **Implementation status**: the shipped serve surface is
/// `operations-gateway`'s `mcp_serve` loop, which serves a `wavecode_tools`
/// registry directly through a `runtime_runner::ToolExecutor` and does not
/// implement this trait. The trait stays as the interface-boundary
/// placeholder for a richer server (resources / prompts capability reuse of
/// the client types); adopt it or remove it when that server evolves.
/// The mirrored method surface with [`McpClient`] is deliberate: two
/// directions of one protocol.
#[async_trait::async_trait]
pub trait McpServerHandler: Send + Sync {
    /// Answer `tools/list`.
    async fn list_tools(&self) -> Result<Vec<McpToolDef>, McpError>;
    /// Answer `tools/call`.
    async fn call_tool(&self, name: &str, input: Value) -> Result<McpToolOutput, McpError>;
    /// Answer `prompts/list` (optional capability, empty list by default).
    async fn list_prompts(&self) -> Result<Vec<McpPromptDef>, McpError> {
        Ok(vec![])
    }
}

pub mod bridge;

pub use bridge::{McpConnectReport, StdioMcpClient, connect_all};

#[cfg(test)]
mod tests {
    use super::*;

    /// Name assembly and split round-trip; a tool name containing `__` does
    /// not break the round trip (split on the first separator).
    #[test]
    fn tool_name_roundtrip() {
        assert_eq!(tool_name("playwright", "click"), "mcp__playwright__click");
        assert_eq!(
            parse_tool_name("mcp__playwright__click"),
            Some(("playwright", "click"))
        );
        assert_eq!(
            parse_tool_name("mcp__srv__a__b"),
            Some(("srv", "a__b")),
            "tool segments holding the separator split on the first one; the server segment is unaffected"
        );
    }

    /// Splitting rejects malformed shapes: no prefix / missing separator /
    /// empty server or tool segment.
    #[test]
    fn parse_tool_name_rejects_invalid() {
        assert_eq!(parse_tool_name("read_file"), None);
        assert_eq!(parse_tool_name("mcp__noserver"), None);
        assert_eq!(parse_tool_name("mcp____tool"), None);
        assert_eq!(parse_tool_name("mcp__srv__"), None);
        assert_eq!(parse_tool_name(""), None);
    }

    /// Config types: transport kind names and one-line summaries (env /
    /// headers stay out of the summary).
    #[test]
    fn server_config_kind_and_summary() {
        let stdio = McpServerConfig::Stdio {
            command: "npx".into(),
            args: vec!["@playwright/mcp@latest".into()],
            env: HashMap::from([("SECRET".into(), "x".into())]),
        };
        assert_eq!(stdio.transport_kind(), "stdio");
        assert_eq!(stdio.summary(), "stdio: npx @playwright/mcp@latest");
        assert!(!stdio.summary().contains("SECRET"));

        let http = McpServerConfig::Http {
            url: "https://mcp.example.com/sse".into(),
            headers: HashMap::from([("Authorization".into(), "Bearer x".into())]),
            // nosemgrep: codacy.yaml.security.hard-coded-tokens
            oauth_token_url: None,
            oauth_client_id: None,
            oauth_client_secret: None,
            oauth_scope: None,
        };
        assert_eq!(http.transport_kind(), "http");
        assert_eq!(http.summary(), "http: https://mcp.example.com/sse");
        assert!(!http.summary().contains("Bearer"));
    }

    /// Validated name assembly accepts well-formed pairs and rejects empty
    /// segments or a server name containing the separator.
    #[test]
    fn try_tool_name_validates_segments() {
        assert_eq!(
            try_tool_name("playwright", "click"),
            Some("mcp__playwright__click".to_owned())
        );
        // Tool names may contain the separator; only the server may not.
        assert_eq!(
            try_tool_name("srv", "a__b"),
            Some("mcp__srv__a__b".to_owned())
        );
        assert_eq!(try_tool_name("", "click"), None);
        assert_eq!(try_tool_name("srv", ""), None);
        assert_eq!(try_tool_name("a__b", "tool"), None);
        assert!(!is_valid_server_name(""));
        assert!(!is_valid_server_name("a__b"));
        assert!(is_valid_server_name("playwright"));
    }

    /// Qualified names round-trip through the parser; invalid servers yield
    /// `None` instead of a silently unparseable name.
    #[test]
    fn qualified_name_roundtrips() {
        let def = McpToolDef {
            name: "click".to_owned(),
            description: None,
            input_schema: serde_json::json!({}),
            read_only_hint: false,
        };
        let qualified = def.qualified_name("playwright").expect("valid server");
        assert_eq!(parse_tool_name(&qualified), Some(("playwright", "click")));
        assert_eq!(def.qualified_name("a__b"), None);
    }

    /// `from_raw` enforces the full entry contract (name, either-or,
    /// endpoint validation) with the exact skip reasons the connect path
    /// has always surfaced — the wording is part of the startup-warning
    /// and doctor surfaces, so it is locked here.
    #[test]
    fn from_raw_validates_name_either_or_and_endpoint() {
        let raw = |command: Option<&str>, url: Option<&str>| wavecode_config::McpServerRaw {
            command: command.map(|s| s.to_string()),
            args: Vec::new(),
            env: HashMap::new(),
            url: url.map(|s| s.to_string()),
            headers: HashMap::new(),
            // nosemgrep: codacy.yaml.security.hard-coded-tokens
            oauth_token_url: None,
            oauth_client_id: None,
            oauth_client_secret: None,
            oauth_scope: None,
        };
        // Well-formed stdio and http entries convert unchanged.
        assert_eq!(
            McpServerConfig::from_raw("srv", &raw(Some("npx"), None)).unwrap(),
            McpServerConfig::Stdio {
                command: "npx".into(),
                args: vec![],
                env: HashMap::new(),
            }
        );
        assert!(
            McpServerConfig::from_raw("srv", &raw(None, Some("https://mcp.example.com"))).is_ok()
        );
        // The three skip reasons, byte-identical to the connect path's.
        assert_eq!(
            McpServerConfig::from_raw("bad__name", &raw(Some("cmd"), None)).unwrap_err(),
            "invalid MCP server name \"bad__name\": must be non-empty without `__`"
        );
        assert_eq!(
            McpServerConfig::from_raw("both", &raw(Some("cmd"), Some("http://x"))).unwrap_err(),
            "MCP server \"both\" sets both command and url; use one"
        );
        assert_eq!(
            McpServerConfig::from_raw("neither", &raw(None, None)).unwrap_err(),
            "MCP server \"neither\" sets neither command nor url"
        );
        // Endpoint validation rides the same path (non-http scheme).
        assert!(
            McpServerConfig::from_raw("srv", &raw(None, Some("ftp://mcp.example.com")))
                .unwrap_err()
                .contains("must start with http:// or https://")
        );
    }

    /// Server config validation rejects empty commands, empty urls, and
    /// non-http(s) urls while accepting well-formed configs.
    #[test]
    fn server_config_validate() {
        let ok_stdio = McpServerConfig::Stdio {
            command: "npx".into(),
            args: vec![],
            env: HashMap::new(),
        };
        assert!(ok_stdio.validate().is_ok());
        let empty_cmd = McpServerConfig::Stdio {
            command: "  ".into(),
            args: vec![],
            env: HashMap::new(),
        };
        assert!(empty_cmd.validate().is_err());

        let ok_http = McpServerConfig::Http {
            url: "https://mcp.example.com/mcp".into(),
            headers: HashMap::new(),
            oauth_token_url: None,
            oauth_client_id: None,
            oauth_client_secret: None,
            oauth_scope: None,
        };
        assert!(ok_http.validate().is_ok());
        let empty_url = McpServerConfig::Http {
            url: "".into(),
            headers: HashMap::new(),
            oauth_token_url: None,
            oauth_client_id: None,
            oauth_client_secret: None,
            oauth_scope: None,
        };
        assert!(empty_url.validate().is_err());
        let bad_scheme = McpServerConfig::Http {
            url: "ftp://mcp.example.com/x".into(),
            headers: HashMap::new(),
            oauth_token_url: None,
            oauth_client_id: None,
            oauth_client_secret: None,
            oauth_scope: None,
        };
        assert!(bad_scheme.validate().is_err());

        // OAuth client-credentials is all-or-nothing: a partial triple fails.
        let mut partial = McpServerConfig::Http {
            url: "https://mcp.example.com/mcp".into(),
            headers: HashMap::new(),
            oauth_token_url: Some("https://auth.example.com/token".into()),
            oauth_client_id: Some("wave".into()),
            oauth_client_secret: None,
            oauth_scope: None,
        };
        assert!(partial.validate().is_err());
        if let McpServerConfig::Http {
            oauth_client_secret,
            ..
        } = &mut partial
        {
            *oauth_client_secret = Some("s3cret".into());
        }
        assert!(partial.validate().is_ok());
    }

    /// The config layer applies the same token-endpoint policy the
    /// transport enforces on connect, so doctor and assembly surface a
    /// plain-http non-loopback `oauth_token_url` before first use.
    #[test]
    fn from_raw_rejects_cleartext_oauth_token_url_off_loopback() {
        let raw = |token_url: &str| wavecode_config::McpServerRaw {
            command: None,
            args: Vec::new(),
            env: HashMap::new(),
            url: Some("https://mcp.example.com/mcp".into()),
            headers: HashMap::new(),
            oauth_token_url: Some(token_url.into()),
            oauth_client_id: Some("wave".into()),
            // nosemgrep: codacy.yaml.security.hard-coded-tokens
            oauth_client_secret: Some("s3cret".into()),
            oauth_scope: None,
        };
        assert!(
            McpServerConfig::from_raw("srv", &raw("https://auth.example.com/token")).is_ok(),
            "https token url accepted"
        );
        assert!(
            McpServerConfig::from_raw("srv", &raw("http://127.0.0.1:1/token")).is_ok(),
            "loopback http token url accepted"
        );
        let err = McpServerConfig::from_raw("srv", &raw("http://auth.example.com/token"))
            .expect_err("non-loopback http token url rejected");
        assert!(
            err.contains("oauth_token_url"),
            "rejection names the offending field: {err}"
        );
    }
}
