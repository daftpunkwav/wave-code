/*!
 * @file NativeExecutor
 * @description In-process tools implemented as plain async closures.
 *
 * Responsibilities:
 * - Register named handlers with declarative attributes.
 * - Execute calls through the ToolExecutor seam.
 * - Serve tests, evaluations, and drivers without processes.
 *
 * This module must not depend on: runtime internals beyond its trait seam.
 */

//! Native tools: in-process executable tool surface for tests, evaluations,
//! and drivers.
//!
//! Handlers are synchronous functions so tests stay deterministic; the
//! production IO-backed tools live in capability crates and enter execution
//! through `ToolAdapter` over the shared `wavecode_tools` registry.

use std::collections::HashMap;
use std::sync::Arc;

use runtime_runner::{ToolCall, ToolExecutor, ToolRef, ToolResult};

/// Native handler: input in, (content, is_error) out.
pub type NativeHandler = Arc<dyn Fn(&serde_json::Value) -> (String, bool) + Send + Sync>;

/// One native tool definition.
#[derive(Clone)]
pub struct NativeTool {
    /// Tool name as registered and advertised.
    pub name: String,
    /// One-line description shown to the model.
    pub description: String,
    /// True when the handler never mutates state.
    pub read_only: bool,
    /// True when the handler can destroy user data.
    pub destructive: bool,
    /// Pure handler: input in, (content, is_error) out.
    pub handler: NativeHandler,
}

/// Executor over registered native tools.
#[derive(Clone, Default)]
pub struct NativeExecutor {
    tools: HashMap<String, NativeTool>,
}

impl NativeExecutor {
    /// Create an empty executor.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register one tool; later registrations win name clashes so test
    /// doubles can shadow defaults without rebuilding the executor.
    pub fn register(&mut self, tool: NativeTool) {
        self.tools.insert(tool.name.clone(), tool);
    }

    /// True when a native tool shadows the name.
    pub fn contains(&self, name: &str) -> bool {
        self.tools.contains_key(name)
    }
}

#[async_trait::async_trait]
impl ToolExecutor for NativeExecutor {
    async fn execute(&self, call: ToolCall) -> ToolResult {
        match self.tools.get(&call.name) {
            Some(tool) => {
                let (content, is_error) = (tool.handler)(&call.input);
                ToolResult {
                    call_id: call.call_id,
                    content,
                    is_error,
                }
            }
            None => ToolResult {
                call_id: call.call_id,
                content: format!("unknown tool: {}", call.name),
                is_error: true,
            },
        }
    }

    fn is_read_only(&self, tool: &str) -> bool {
        self.tools.get(tool).is_some_and(|t| t.read_only)
    }

    fn is_destructive(&self, tool: &str) -> bool {
        self.tools.get(tool).is_none_or(|t| t.destructive)
    }

    fn available_tools(&self) -> Vec<ToolRef> {
        let mut refs: Vec<ToolRef> = self
            .tools
            .values()
            .map(|tool| ToolRef {
                name: tool.name.clone(),
                description: tool.description.clone(),
            })
            .collect();
        refs.sort_by(|a, b| a.name.cmp(&b.name));
        refs
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn executor() -> NativeExecutor {
        let mut executor = NativeExecutor::new();
        executor.register(NativeTool {
            name: "echo".to_string(),
            description: "echo input".to_string(),
            read_only: true,
            destructive: false,
            handler: Arc::new(|input| (input.to_string(), false)),
        });
        executor
    }

    fn call(name: &str) -> ToolCall {
        ToolCall {
            call_id: "c1".to_string(),
            name: name.to_string(),
            input: serde_json::json!({"a": 1}),
        }
    }

    #[tokio::test]
    async fn handlers_run_with_attributes_and_catalog() {
        let executor = executor();
        assert!(executor.is_read_only("echo"));
        assert!(!executor.is_destructive("echo"));
        // Unknown names stay serial and destructive (safe defaults).
        assert!(!executor.is_read_only("nope"));
        assert!(executor.is_destructive("nope"));
        let out = executor.execute(call("echo")).await;
        assert!(!out.is_error);
        assert!(out.content.contains("\"a\""));
        let missing = executor.execute(call("nope")).await;
        assert!(missing.is_error);
        assert_eq!(executor.available_tools().len(), 1);
    }
}
