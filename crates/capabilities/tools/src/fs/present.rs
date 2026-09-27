/*! @file Present
 * @description Deliverable-declaration tool with a shared session manifest.
 *
 * Responsibilities:
 * - Resolve deliverable paths under cwd and record them in PresentStore
 * - Echo a session manifest the model can quote back to the user
 * - Document why no read-before-edit gate is built (stateless tools)
 *
 * This module must not depend on: UI-layer components, network access.
 */

//! `present` tool: declare deliverables and record them in the session manifest.
//!
//! The model calls `present {paths}` when work is ready for the user; the tool
//! resolves each path under `cwd` (escapes are business errors), records the
//! presented set in the shared [`PresentStore`], and echoes a manifest the
//! model can quote.
//!
//! Read-before-edit gate: deliberately NOT implemented here. Tool instances are
//! stateless across turns (`ToolCtx` is rebuilt every turn), so an in-tool
//! read-set would forget reads between turns; a shared read-set would couple
//! the tool registry to session state. Until session-scoped state exists,
//! freshness stays a documented convention, not a half-built gate.

use std::sync::{Arc, Mutex};

use serde_json::{Value, json};

use super::resolve_path;
use crate::{Result, Tool, ToolCtx, ToolOutput, lock};

/// Shared record of presented deliverable paths (per-session handle, like
/// `TodoStore`): session assembly holds one and injects it into the tool.
#[derive(Clone, Default, Debug)]
pub struct PresentStore {
    inner: Arc<Mutex<Vec<String>>>,
}

impl PresentStore {
    /// Record newly presented paths (deduped, order-preserving).
    pub fn record(&self, paths: &[String]) {
        let mut guard = lock(&self.inner);
        for p in paths {
            if !guard.contains(p) {
                guard.push(p.clone());
            }
        }
    }

    /// Snapshot of all presented paths.
    pub fn snapshot(&self) -> Vec<String> {
        lock(&self.inner).clone()
    }
}

/// Declare deliverables as presented (records + echoes the session manifest).
pub struct Present {
    store: PresentStore,
}

impl Present {
    /// Build sharing `store` with the session config.
    pub fn new(store: PresentStore) -> Self {
        Self { store }
    }

    /// Session-manifest rendering (shared with the execute path and tests).
    pub fn manifest(paths: &[String]) -> String {
        let mut out = format!("Presented {} deliverable(s):", paths.len());
        for p in paths {
            out.push_str(&format!("\n- {p}"));
        }
        out
    }
}

#[async_trait::async_trait]
impl Tool for Present {
    fn name(&self) -> &str {
        "present"
    }

    fn kind(&self) -> wavecode_protocol::ToolKind {
        wavecode_protocol::ToolKind::Present
    }

    fn description(&self) -> &str {
        "Declare deliverables as presented to the user (e.g. files created or \
         fixed). Paths are resolved inside the working directory and recorded \
         in the session manifest; the result echoes the manifest."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "paths": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "Deliverable paths, relative to the working directory"
                }
            },
            "required": ["paths"]
        })
    }

    fn is_read_only(&self) -> bool {
        true
    }

    async fn execute(&self, input: Value, ctx: &ToolCtx) -> Result<ToolOutput> {
        let paths = match input.get("paths").and_then(Value::as_array) {
            Some(a) => a,
            None => {
                return Ok(ToolOutput {
                    content: "missing or invalid parameter 'paths' (array of strings required)"
                        .to_owned(),
                    is_error: true,
                });
            }
        };
        if paths.is_empty() {
            return Ok(ToolOutput {
                content: "no paths presented (at least one path required)".to_owned(),
                is_error: true,
            });
        }
        let mut resolved: Vec<String> = Vec::with_capacity(paths.len());
        for raw in paths {
            let raw = match raw.as_str() {
                Some(s) => s,
                None => {
                    return Ok(ToolOutput {
                        content: "invalid parameter 'paths' (every entry must be a string)"
                            .to_owned(),
                        is_error: true,
                    });
                }
            };
            match resolve_path(ctx, raw)? {
                Ok(p) => resolved.push(p.display().to_string()),
                Err(out) => return Ok(out),
            }
        }
        self.store.record(&resolved);
        Ok(ToolOutput {
            content: Self::manifest(&self.store.snapshot()),
            is_error: false,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> (tempfile::TempDir, ToolCtx) {
        let dir = tempfile::tempdir().unwrap();
        let c = ToolCtx {
            cwd: dir.path().to_path_buf(),
            deny_env: Vec::new(),
        };
        (dir, c)
    }

    #[tokio::test]
    async fn present_records_and_echoes_manifest() {
        let (dir, c) = ctx();
        std::fs::write(dir.path().join("a.txt"), "x").unwrap();
        let store = PresentStore::default();
        let tool = Present::new(store.clone());
        let out = tool
            .execute(serde_json::json!({"paths": ["a.txt"]}), &c)
            .await
            .unwrap();
        assert!(!out.is_error);
        assert!(out.content.contains("Presented 1 deliverable"));
        assert!(out.content.contains("a.txt"));
        assert_eq!(store.snapshot().len(), 1);
        // Re-presenting dedupes in the store.
        let out = tool
            .execute(serde_json::json!({"paths": ["a.txt"]}), &c)
            .await
            .unwrap();
        assert!(!out.is_error);
        assert_eq!(store.snapshot().len(), 1);
    }

    #[tokio::test]
    async fn present_rejects_escape_and_empty() {
        let (_d, c) = ctx();
        let tool = Present::new(PresentStore::default());
        assert!(
            tool.execute(json!({"paths": []}), &c)
                .await
                .unwrap()
                .is_error
        );
        assert!(tool.execute(json!({}), &c).await.unwrap().is_error);
        let out = tool
            .execute(json!({"paths": ["../evil.txt"]}), &c)
            .await
            .unwrap();
        assert!(out.is_error);
    }
}
