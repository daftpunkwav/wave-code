//! Raw MCP server config: the `[mcp_servers]` section.

use super::*;

/// Raw config of a single MCP server. The stdio form fills `command` (plus optional `args` / `env`); the http
/// form fills `url` (plus optional `headers`). The either-or validation of the
/// two forms does not live in this layer (config has no in-workspace
/// dependencies, same raw-parse discipline as hooks).
#[derive(Debug, Clone, serde::Deserialize)]
pub struct McpServerRaw {
    /// Executable command for the stdio transport (required in the stdio form).
    pub command: Option<String>,
    /// Command arguments.
    #[serde(default)]
    pub args: Vec<String>,
    /// Extra environment variables injected into the child process.
    #[serde(default)]
    pub env: HashMap<String, String>,
    /// streamable-http endpoint (required in the http form).
    pub url: Option<String>,
    /// Extra request headers.
    #[serde(default)]
    pub headers: HashMap<String, String>,
    /// OAuth token endpoint URL (client-credentials grant; optional, set
    /// together with `oauth_client_id` + `oauth_client_secret` or not at
    /// all; the all-or-nothing check lives in the assembly layer).
    pub oauth_token_url: Option<String>,
    /// OAuth client id (client-credentials grant).
    pub oauth_client_id: Option<String>,
    /// OAuth client secret (client-credentials grant).
    pub oauth_client_secret: Option<String>,
    /// Optional OAuth scope to request.
    pub oauth_scope: Option<String>,
}
