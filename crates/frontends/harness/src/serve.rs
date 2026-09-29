//! Server-like surfaces: MCP stdio, ACP stdio, plugin inventory, HTTP.
//!
//! Thin runners over `operations-gateway` and `operations-bootstrap`.
//! Each one returns before the shared session assembly (or needs no
//! session at all) and owns no conversation state of its own.

use std::path::PathBuf;

use uuid::Uuid;

/// Serve the builtin tool registry over MCP stdio until EOF.
///
/// Needs no model credentials: it exposes local tools, so it returns
/// before session assembly.
pub(crate) async fn run_mcp_serve(cwd: PathBuf) -> anyhow::Result<()> {
    let (registry, executor) = operations_bootstrap::ToolAdapter::mcp_serve_tools(cwd);
    operations_gateway::mcp_serve::run_stdio_server(registry, executor)
        .await
        .map_err(|e| anyhow::anyhow!("mcp server failed: {e}"))
}

/// Drive ACP sessions over stdio until EOF.
///
/// Like `exec`, sessions assemble headless from config, so provider
/// credentials are required; unlike `exec`, turns arrive as
/// `session/prompt` requests instead of CLI arguments.
pub(crate) async fn run_acp(
    config: Option<PathBuf>,
    model: Option<String>,
    permission_mode: Option<String>,
    cwd: PathBuf,
    home: Option<PathBuf>,
) -> anyhow::Result<()> {
    operations_gateway::acp::run_stdio_server(
        operations_gateway::acp::AcpServerOptions {
            config_path: config,
            model_override: model,
            permission_override: permission_mode,
            cwd,
            home,
        },
        operations_bootstrap::assemble_session,
    )
    .await
    .map_err(|e| anyhow::anyhow!("acp server failed: {e}"))
}

/// List installed plugin packs with skill/MCP/hook counts.
pub(crate) fn run_plugin_list(home: Option<PathBuf>) {
    let (plugins, warnings) =
        operations_bootstrap::plugin_inventory::discover_plugin_summaries(home.as_deref());
    for warning in &warnings {
        eprintln!("[warn] {warning}");
    }
    if plugins.is_empty() {
        println!("(no plugins installed)");
        return;
    }
    for plugin in &plugins {
        println!(
            "{} {} (skills: {}, mcp servers: {}, hooks: {})",
            plugin.name, plugin.version, plugin.skills, plugin.mcp_servers, plugin.hooks
        );
    }
}

/// Serve live sessions over local HTTP until Ctrl-C or `POST /shutdown`.
///
/// The bearer token prints once; every route except `/healthz` requires
/// it, and the server binds the loopback interface only.
pub(crate) async fn run_serve(
    config: Option<PathBuf>,
    port: u16,
    token: Option<String>,
    cwd: PathBuf,
    home: Option<PathBuf>,
) -> anyhow::Result<()> {
    let token = token.unwrap_or_else(|| Uuid::new_v4().to_string());
    let handle = operations_gateway::app_server::serve(
        operations_gateway::app_server::ServeOptions {
            config_path: config,
            model_override: None,
            cwd,
            home,
            token: token.clone(),
            port,
        },
        operations_bootstrap::assemble_session,
    )
    .await?;
    // stderr: unbuffered even when the process is piped (stdout would
    // block-buffer and a spawning parent would never see the token).
    eprintln!(
        "wavecode serve listening on http://127.0.0.1:{} (Ctrl-C stops)",
        handle.port
    );
    eprintln!("bearer token: {token}");
    eprintln!(
        "try: curl -H 'Authorization: Bearer {token}' http://127.0.0.1:{}/healthz",
        handle.port
    );
    tokio::signal::ctrl_c().await?;
    handle.shutdown();
    Ok(())
}
