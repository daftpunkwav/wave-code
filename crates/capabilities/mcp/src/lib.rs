//! wavecode-mcp — two-way Model Context Protocol support (P9 interface boundary).
//!
//! - Client: stdio and streamable-http (incl. OAuth) transports, with external
//!   tools injected into the tool registry under the `mcp__{server}__{tool}`
//!   namespace;
//! - Server: exposes WaveCode's own capabilities as an MCP server for other
//!   agents / IDEs to call.
//!
//! **P9 scope (honest disclosure)**: this crate only defines the interface
//! boundary — the [`McpClient`] / [`McpServerHandler`] traits, the tool and
//! prompt data types, the naming convention, and the [`McpServerConfig`]
//! config type. **No real transports**: stdio / streamable-http connections
//! land in a later iteration (then aligned with the official rmcp crate's
//! capability surface; this trait surface is designed against the MCP
//! protocol's `tools/list`, `tools/call`, and `prompts/list` methods to avoid
//! rework at integration time, while the remaining protocol surface —
//! resources, notifications, etc. — extends on demand). Bridging MCP tools
//! into wavecode `Tool`s and config-parsing orchestration live on the core
//! side (the core->mcp edge is allowed by the SPEC section 3 matrix; mcp
//! itself has no workspace-internal dependencies).

use std::collections::HashMap;

use serde_json::Value;

/// Name prefix for MCP tools injected into the registry (SPEC section 10):
/// the full tool name is `mcp__{server}__{tool}`, which never collides with
/// builtin tools.
pub const MCP_TOOL_PREFIX: &str = "mcp__";

/// Separator between the server name and the tool name ([`parse_tool_name`]
/// splits on the first separator — a tool name containing `__` survives the
/// round trip, while a server name must not contain `__`, enforced by the
/// assembly layer (core config translation)).
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
/// SPEC section 10 requires prompts to auto-convert into inline skills: the
/// conversion needs `prompts/get` content fetches, which depend on a real
/// transport and wire up on the core side in a later iteration (the skills
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
/// `tools/call` / `prompts/list`); object-safe + Send + Sync, held by the
/// bridge layer as `Arc<dyn McpClient>`.
///
/// **Implementation status**: the stdio implementation is the assembly-side
/// `StdioMcpClient` (child process + `transport-mcp` framing);
/// streamable-http remains a later iteration. P9's verification surface is a
/// mock (see the assembly-side `McpToolBridge` tests). Exponential-backoff
/// reconnects on connection failure (SPEC section 10) are a transport
/// implementation detail and stay out of the trait surface.
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
}

/// MCP server config (the two shapes of the SPEC section 13
/// `[mcp_servers.<name>]` section).
///
/// Conversion from the config crate's raw tables happens on the core side (the
/// config crate has no workspace-internal dependencies and only does raw
/// parsing; either-or validation lives in core's `servers_from_config`).
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
    /// streamable-http transport (incl. OAuth 2.0 client-credentials, SPEC section 10).
    Http {
        /// Server endpoint URL.
        url: String,
        /// Extra request headers (e.g. a pre-provisioned `Authorization`
        /// value; a static `Authorization` header wins over OAuth).
        headers: HashMap<String, String>,
        /// OAuth token endpoint URL (client-credentials grant). Set together
        /// with `oauth_client_id` + `oauth_client_secret`, or not at all
        /// (see `validate`).
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

    /// Validate the server endpoint configuration: stdio requires a
    /// non-empty `command`; http requires a non-empty `url` starting with
    /// `http://` or `https://`. Returns `Err` with the reason so the
    /// assembly layer can surface it as a configuration error.
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
                    return Err("OAuth client-credentials needs all of `oauth_token_url`,                         `oauth_client_id`, `oauth_client_secret` (or none)"
                        .to_owned());
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

/// MCP server-side placeholder trait (SPEC section 10 server: `wavecode mcp
/// serve` exposes WaveCode's tool set and session capabilities over stdio for
/// IDEs / other agents to call).
///
/// **Implementation status**: lands after P10 — the Registry's tool surface is
/// then adapted into an impl of this trait and exposed via rmcp's server side;
/// auth (localhost-only by default, client allowlist) is completed at the
/// serve assembly layer and stays out of the trait surface. The mirrored
/// method surface with [`McpClient`] is deliberate: two directions of one
/// protocol.
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
}
