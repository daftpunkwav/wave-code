//! The five read-only LSP tools: four spawn (or reuse a pooled) language
//! server for one navigation request, and `lsp_diagnostics` renders the
//! recorded diagnostics pushes.

use std::sync::Arc;

use serde_json::{Value, json};

use crate::{Result, Tool, ToolCtx, ToolOutput, err_output, ok_output};

use super::providers::LspProviders;
use super::{resolve_uri, run_lsp_call};

/// List document symbols (outline) for a file (read-only).
pub struct DocumentSymbols {
    providers: Option<Arc<LspProviders>>,
}

impl DocumentSymbols {
    /// Build without a registry (explicit `server_command` required per call).
    pub fn new() -> Self {
        Self { providers: None }
    }

    /// Build resolving default servers from `providers` (`server_command`
    /// becomes a per-call override).
    pub fn with_providers(providers: Arc<LspProviders>) -> Self {
        Self {
            providers: Some(providers),
        }
    }
}

impl Default for DocumentSymbols {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl Tool for DocumentSymbols {
    fn name(&self) -> &str {
        "lsp_symbols"
    }

    fn description(&self) -> &str {
        "List the symbol outline (functions, classes, etc.) of a file via a language \
         server. Omit server_command to use the registered provider for the file \
         extension, or provide it as an override (e.g. rust-analyzer, \
         pyright-langserver --stdio); no server is bundled. Path is relative to \
         the working directory."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "server_command": {
                    "type": "string",
                    "description": "Language server command to spawn (program plus whitespace-separated args)"
                },
                "path": {
                    "type": "string",
                    "description": "Path of the file, relative to the working directory"
                },
                "timeout_ms": {
                    "type": "integer",
                    "description": "Timeout per LSP round trip in milliseconds (default 30000, clamped to max 300000)"
                }
            },
            "required": ["path"]
        })
    }

    fn is_read_only(&self) -> bool {
        true
    }

    async fn execute(&self, input: Value, ctx: &ToolCtx) -> Result<ToolOutput> {
        run_lsp_call(
            &input,
            ctx,
            self.providers.as_ref(),
            false,
            "documentSymbol",
            async |c, uri, _, _, t| c.document_symbol(uri, t).await,
        )
        .await
    }
}

/// Jump to the definition of the symbol at a position (read-only).
pub struct GotoDefinition {
    providers: Option<Arc<LspProviders>>,
}

impl GotoDefinition {
    /// Build without a registry (explicit `server_command` required per call).
    pub fn new() -> Self {
        Self { providers: None }
    }

    /// Build resolving default servers from `providers` (`server_command`
    /// becomes a per-call override).
    pub fn with_providers(providers: Arc<LspProviders>) -> Self {
        Self {
            providers: Some(providers),
        }
    }
}

impl Default for GotoDefinition {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl Tool for GotoDefinition {
    fn name(&self) -> &str {
        "lsp_definition"
    }

    fn description(&self) -> &str {
        "Jump to the definition of the symbol at a 0-based line/character position \
         via a language server. Omit server_command to use the registered provider \
         for the file extension, or provide it as an override (e.g. rust-analyzer, \
         pyright-langserver --stdio); no server is bundled. Path is relative to \
         the working directory."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "server_command": {
                    "type": "string",
                    "description": "Language server command to spawn (program plus whitespace-separated args)"
                },
                "path": {
                    "type": "string",
                    "description": "Path of the file, relative to the working directory"
                },
                "line": {
                    "type": "integer",
                    "description": "0-based line number"
                },
                "character": {
                    "type": "integer",
                    "description": "0-based character offset on the line"
                },
                "timeout_ms": {
                    "type": "integer",
                    "description": "Timeout per LSP round trip in milliseconds (default 30000, clamped to max 300000)"
                }
            },
            "required": ["path", "line", "character"]
        })
    }

    fn is_read_only(&self) -> bool {
        true
    }

    async fn execute(&self, input: Value, ctx: &ToolCtx) -> Result<ToolOutput> {
        run_lsp_call(
            &input,
            ctx,
            self.providers.as_ref(),
            true,
            "definition",
            async |c, uri, line, ch, t| c.definition(uri, line, ch, t).await,
        )
        .await
    }
}

/// Hover documentation for the symbol at a position (read-only).
pub struct Hover {
    providers: Option<Arc<LspProviders>>,
}

impl Hover {
    /// Build without a registry (explicit `server_command` required per call).
    pub fn new() -> Self {
        Self { providers: None }
    }

    /// Build resolving default servers from `providers` (`server_command`
    /// becomes a per-call override).
    pub fn with_providers(providers: Arc<LspProviders>) -> Self {
        Self {
            providers: Some(providers),
        }
    }
}

impl Default for Hover {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl Tool for Hover {
    fn name(&self) -> &str {
        "lsp_hover"
    }

    fn description(&self) -> &str {
        "Show hover documentation for the symbol at a 0-based line/character position \
         via a language server. Omit server_command to use the registered provider \
         for the file extension, or provide it as an override (e.g. rust-analyzer, \
         pyright-langserver --stdio); no server is bundled. Path is relative to \
         the working directory."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "server_command": {
                    "type": "string",
                    "description": "Language server command to spawn (program plus whitespace-separated args)"
                },
                "path": {
                    "type": "string",
                    "description": "Path of the file, relative to the working directory"
                },
                "line": {
                    "type": "integer",
                    "description": "0-based line number"
                },
                "character": {
                    "type": "integer",
                    "description": "0-based character offset on the line"
                },
                "timeout_ms": {
                    "type": "integer",
                    "description": "Timeout per LSP round trip in milliseconds (default 30000, clamped to max 300000)"
                }
            },
            "required": ["path", "line", "character"]
        })
    }

    fn is_read_only(&self) -> bool {
        true
    }

    async fn execute(&self, input: Value, ctx: &ToolCtx) -> Result<ToolOutput> {
        run_lsp_call(
            &input,
            ctx,
            self.providers.as_ref(),
            true,
            "hover",
            async |c, uri, line, ch, t| c.hover(uri, line, ch, t).await,
        )
        .await
    }
}

/// Find all references to the symbol at a position (read-only).
pub struct FindReferences {
    providers: Option<Arc<LspProviders>>,
}

impl FindReferences {
    /// Build without a registry (explicit `server_command` required per call).
    pub fn new() -> Self {
        Self { providers: None }
    }

    /// Build resolving default servers from `providers` (`server_command`
    /// becomes a per-call override).
    pub fn with_providers(providers: Arc<LspProviders>) -> Self {
        Self {
            providers: Some(providers),
        }
    }
}

impl Default for FindReferences {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl Tool for FindReferences {
    fn name(&self) -> &str {
        "lsp_references"
    }

    fn description(&self) -> &str {
        "Find all references (including the declaration) to the symbol at a 0-based \
         line/character position via a language server. Omit server_command to use \
         the registered provider for the file extension, or provide it as an \
         override (e.g. rust-analyzer, pyright-langserver --stdio); no server is \
         bundled. Path is relative to the working directory."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "server_command": {
                    "type": "string",
                    "description": "Language server command to spawn (program plus whitespace-separated args)"
                },
                "path": {
                    "type": "string",
                    "description": "Path of the file, relative to the working directory"
                },
                "line": {
                    "type": "integer",
                    "description": "0-based line number"
                },
                "character": {
                    "type": "integer",
                    "description": "0-based character offset on the line"
                },
                "timeout_ms": {
                    "type": "integer",
                    "description": "Timeout per LSP round trip in milliseconds (default 30000, clamped to max 300000)"
                }
            },
            "required": ["path", "line", "character"]
        })
    }

    fn is_read_only(&self) -> bool {
        true
    }

    async fn execute(&self, input: Value, ctx: &ToolCtx) -> Result<ToolOutput> {
        run_lsp_call(
            &input,
            ctx,
            self.providers.as_ref(),
            true,
            "references",
            async |c, uri, line, ch, t| c.references(uri, line, ch, t).await,
        )
        .await
    }
}

/// Read back the diagnostics captured from server pushes (read-only).
///
/// Language servers push `textDocument/publishDiagnostics` while a tool
/// call's requests are in flight; registry-backed clients record those pushes
/// into the registry's shared bounded store (see [`LspProviders`]). This tool
/// renders the store — it never spawns a server or issues a request, so a
/// path filter only needs to resolve (same path guard as the other LSP
/// tools) to match the URIs earlier calls addressed.
pub struct LspDiagnostics {
    providers: Option<Arc<LspProviders>>,
}

impl LspDiagnostics {
    /// Build without a registry: there is no store to read, so every call is
    /// a business error (diagnostics only exist with registry-backed
    /// clients).
    pub fn new() -> Self {
        Self { providers: None }
    }

    /// Build reading the shared store of `providers`.
    pub fn with_providers(providers: Arc<LspProviders>) -> Self {
        Self {
            providers: Some(providers),
        }
    }
}

impl Default for LspDiagnostics {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl Tool for LspDiagnostics {
    fn name(&self) -> &str {
        "lsp_diagnostics"
    }

    fn description(&self) -> &str {
        "Return the diagnostics (errors, warnings, hints) that language servers \
         pushed for files during earlier language-server tool calls. Provide path \
         to see one file's diagnostics, or omit it to see every tracked file. \
         Entries render as line:col (1-based) with severity and message. \
         Read-only: reads the recorded diagnostics, never spawns a server."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Path of the file, relative to the working directory; omit to return all tracked files"
                }
            }
        })
    }

    fn is_read_only(&self) -> bool {
        true
    }

    async fn execute(&self, input: Value, ctx: &ToolCtx) -> Result<ToolOutput> {
        let Some(providers) = &self.providers else {
            return Ok(err_output(
                "no language server registry configured: lsp_diagnostics reads diagnostics recorded by registry-backed language-server tool calls",
            ));
        };
        // Optional path: absent/null renders every tracked file; a present
        // path resolves through the same guard as the other LSP tools so the
        // URI matches what earlier pooled calls addressed.
        let filter = match input.get("path") {
            None | Some(Value::Null) => None,
            Some(Value::String(path)) => match resolve_uri(ctx, path)? {
                Ok(uri) => Some(uri),
                Err(out) => return Ok(out),
            },
            Some(_) => {
                return Ok(err_output(
                    "missing or invalid parameter 'path' (string required)",
                ));
            }
        };
        Ok(ok_output(providers.diagnostics_text(filter.as_deref())))
    }
}
