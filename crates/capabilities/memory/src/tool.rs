/*!
 * @file MemoryWriteTool
 * @description Durable memory writes as a model-invokable tool.
 *
 * Responsibilities:
 * - Validate category and content before touching the filesystem.
 * - Append entries through the memory store (index updated atomically).
 * - Report business failures as model-readable errors, never panics.
 *
 * This module must not depend on: drivers, actors, or sessions. The tool
 * owns its store handle; assembly registers it only when memory is
 * configured (a home directory exists).
 */

//! Memory writes: explicit persistence behind the tool seam.

use crate::{MemoryCategory, MemoryStore};
use wavecode_tools::{Result, Tool, ToolCtx, ToolOutput};

/// `memory_write`: persist one durable memory entry.
///
/// The model calls this for facts worth keeping across sessions (user
/// preferences, project conventions, reusable feedback). Session-end
/// extraction covers the rest; this tool is the explicit path.
#[derive(Debug, Clone)]
pub struct MemoryWrite {
    store: MemoryStore,
}

impl MemoryWrite {
    /// Wrap a memory store handle (same root the prompt index reads).
    pub fn new(store: MemoryStore) -> Self {
        Self { store }
    }
}

#[async_trait::async_trait]
impl Tool for MemoryWrite {
    fn name(&self) -> &str {
        "memory_write"
    }

    fn description(&self) -> &str {
        "Persist one durable memory for future sessions: user preferences, \
         project conventions, or reusable feedback. Category is one of \
         user, feedback, project, reference. Prefer this over stuffing the \
         current conversation; session-end extraction only keeps what the \
         transcript still shows."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "category": {
                    "type": "string",
                    "description": "Memory category: user, feedback, project, or reference",
                    "enum": ["user", "feedback", "project", "reference"],
                },
                "content": {
                    "type": "string",
                    "description": "The memory text; blank content is rejected",
                },
            },
            "required": ["category", "content"],
        })
    }

    fn is_read_only(&self) -> bool {
        false
    }

    async fn execute(&self, input: serde_json::Value, _ctx: &ToolCtx) -> Result<ToolOutput> {
        let category = input.get("category").and_then(|v| v.as_str()).unwrap_or("");
        let Some(category) = MemoryCategory::parse(category) else {
            return Ok(ToolOutput {
                content: format!(
                    "unknown memory category {category:?}: use user, feedback, project, or reference"
                ),
                is_error: true,
            });
        };
        let content = input.get("content").and_then(|v| v.as_str()).unwrap_or("");
        // Small synchronous writes: bridge out of async explicitly rather
        // than blocking the executor on filesystem IO.
        let store = self.store.clone();
        let content = content.to_string();
        let written = tokio::task::spawn_blocking(move || store.append(category, &content)).await;
        match written {
            Ok(Ok(())) => Ok(ToolOutput {
                content: format!("memory saved to {}", category.as_str()),
                is_error: false,
            }),
            Ok(Err(e)) => Ok(ToolOutput {
                content: format!("memory store failed: {e}"),
                is_error: true,
            }),
            Err(e) => Ok(ToolOutput {
                content: format!("memory write task failed: {e}"),
                is_error: true,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn valid_writes_persist_and_read_back() {
        let root = tempfile::tempdir().unwrap();
        let tool = MemoryWrite::new(MemoryStore::new(root.path().to_path_buf()));
        let output = tool
            .execute(
                serde_json::json!({"category": "user", "content": "prefers tabs"}),
                &ToolCtx {
                    cwd: root.path().to_path_buf(),
                    deny_env: Vec::new(),
                },
            )
            .await
            .unwrap();
        assert!(!output.is_error);
        let store = MemoryStore::new(root.path().to_path_buf());
        assert!(
            store
                .read_category(MemoryCategory::User)
                .unwrap()
                .contains("prefers tabs")
        );
        assert!(store.read_index().unwrap().contains("[user]"));
    }

    #[tokio::test]
    async fn unknown_categories_and_blanks_fail_openly() {
        let dir = tempfile::tempdir().unwrap();
        let tool = MemoryWrite::new(MemoryStore::new(dir.path().to_path_buf()));
        let ctx = ToolCtx {
            cwd: dir.path().to_path_buf(),
            deny_env: Vec::new(),
        };
        let bad_category = tool
            .execute(
                serde_json::json!({"category": "dream", "content": "x"}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(bad_category.is_error);
        assert!(bad_category.content.contains("unknown memory category"));
        let blank = tool
            .execute(
                serde_json::json!({"category": "user", "content": "   "}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(blank.is_error);
        assert!(!tool.is_read_only());
    }
}
