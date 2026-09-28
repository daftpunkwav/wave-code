/*! @file SpillTool
 * @description Read-only readback for the context spill side-store.
 *
 * Responsibilities:
 * - Resolve `spill://` URIs against a store root like the snapshot tools
 * - Return spilled output verbatim (read-only, never writes)
 *
 * This module must not depend on: git, the network, or UI-layer components.
 */

//! `spill` tool (read-only): read one spilled tool output by its
//! `spill://` URI. The store itself is injected at composition time —
//! session assembly builds the session's single shared `SpillStore` (the
//! same instance the shell tool spills into and the pruning executor
//! prunes through), so every writer shares one manifest ledger. Spills
//! live outside the working directory, so this dedicated tool (not
//! `read`, which is confined to `cwd` by `path_guard`) is the read path.

use std::sync::Arc;

use serde_json::{Value, json};
use wavecode_context::SpillStore;

use crate::{Result, Tool, ToolCtx, ToolOutput, err_output};

/// Read one spilled output by `spill://` URI (read-only).
pub struct SpillRead {
    store: Arc<SpillStore>,
}

impl SpillRead {
    /// Build against the session's shared spill store (injected; the
    /// tool never picks a root itself).
    pub fn new(store: Arc<SpillStore>) -> Self {
        Self { store }
    }
}

#[async_trait::async_trait]
impl Tool for SpillRead {
    fn name(&self) -> &str {
        "spill"
    }

    fn description(&self) -> &str {
        "Read a spilled tool output by its spill:// URI (see pruned tool \
         results carrying the [pruned] marker). Spills live outside the \
         working directory; this read-only tool is the only read path."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "uri": {
                    "type": "string",
                    "description": "Spill URI (spill://<id>) from a pruned tool result"
                }
            },
            "required": ["uri"]
        })
    }

    fn is_read_only(&self) -> bool {
        true
    }

    async fn execute(&self, input: Value, _ctx: &ToolCtx) -> Result<ToolOutput> {
        let uri = match input.get("uri").and_then(Value::as_str) {
            Some(u) => u,
            None => {
                return Ok(err_output(
                    "missing or invalid parameter 'uri' (string required)",
                ));
            }
        };
        match self.store.read(uri) {
            Ok(content) => Ok(ToolOutput {
                content,
                is_error: false,
            }),
            Err(e) => Ok(err_output(e.to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool_ctx(cwd: &std::path::Path) -> ToolCtx {
        ToolCtx {
            cwd: cwd.to_path_buf(),
            deny_env: Vec::new(),
        }
    }

    #[tokio::test]
    async fn reads_spilled_content() {
        let root = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let ctx = tool_ctx(cwd.path());
        let store = Arc::new(SpillStore::new(root.path().to_path_buf()));
        let uri = store.spill("spilled body").unwrap();
        let tool = SpillRead::new(store.clone());
        let out = tool.execute(json!({"uri": uri}), &ctx).await.unwrap();
        assert!(!out.is_error);
        assert_eq!(out.content, "spilled body");
    }

    #[tokio::test]
    async fn bad_uri_is_a_business_error() {
        let root = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let ctx = tool_ctx(cwd.path());
        let tool = SpillRead::new(Arc::new(SpillStore::new(root.path().to_path_buf())));
        let out = tool
            .execute(json!({"uri": "spill://../evil"}), &ctx)
            .await
            .unwrap();
        assert!(out.is_error);
        let out = tool.execute(json!({}), &ctx).await.unwrap();
        assert!(out.is_error);
    }

    /// The injected store is shared by construction: content a writer
    /// spills into the same `Arc` is readable through the tool with no
    /// root re-derivation (the single-instance contract the session
    /// assembly relies on).
    #[tokio::test]
    async fn injected_store_is_shared_by_reference() {
        let root = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let ctx = tool_ctx(cwd.path());
        let store = Arc::new(SpillStore::new(root.path().to_path_buf()));
        let uri = store.spill("written by the shell side").unwrap();
        // A second handle over the same Arc (what session assembly hands
        // to the pruning executor / shell tool) sees the entry.
        let same_store = store.clone();
        assert!(std::sync::Arc::ptr_eq(&store, &same_store));
        let tool = SpillRead::new(same_store);
        let out = tool.execute(json!({"uri": uri}), &ctx).await.unwrap();
        assert_eq!(out.content, "written by the shell side");
    }
}
