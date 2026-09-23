/*!
 * @file ToolAdapter
 * @description Adapts the legacy tool registry to the ToolExecutor seam.
 *
 * Responsibilities:
 * - Look up tools by stable name and execute them with a fixed context.
 * - Translate unknown tools and implementation faults into error results.
 *
 * This module must not depend on: runtime internals beyond its trait seam.
 */

//! [`runtime_runner::ToolExecutor`] implemented over `wavecode-tools`.

use std::path::PathBuf;
use std::sync::Arc;

use runtime_runner::{ToolCall, ToolExecutor, ToolRef, ToolResult};

/// Executes tools from a shared legacy registry with a fixed context.
pub struct ToolAdapter {
    registry: Arc<wavecode_tools::Registry>,
    ctx: wavecode_tools::ToolCtx,
}

impl ToolAdapter {
    /// Wrap a shared registry with the execution context for all calls.
    pub fn new(registry: Arc<wavecode_tools::Registry>, ctx: wavecode_tools::ToolCtx) -> Self {
        Self { registry, ctx }
    }

    /// Build the credential-free `mcp serve` surface: the full builtin
    /// registry (including `todowrite`) plus an executor over it with a
    /// cwd-only execution context. Registry construction lives here, in
    /// the composition root, so frontends keep depending on this crate
    /// instead of naming the tools crate directly; the serving loop
    /// itself lives in the gateway.
    pub fn mcp_serve_tools(cwd: PathBuf) -> (Arc<wavecode_tools::Registry>, Self) {
        let (registry, _todos) = wavecode_tools::Registry::builtin_with_todos();
        let registry = Arc::new(registry);
        let executor = Self::new(
            registry.clone(),
            wavecode_tools::ToolCtx {
                cwd,
                deny_env: Vec::new(),
            },
        );
        (registry, executor)
    }
}

#[async_trait::async_trait]
impl ToolExecutor for ToolAdapter {
    async fn execute(&self, call: ToolCall) -> ToolResult {
        let Some(tool) = self.registry.get(&call.name) else {
            return ToolResult {
                call_id: call.call_id,
                content: format!("unknown tool: {}", call.name),
                is_error: true,
            };
        };
        match tool.execute(call.input, &self.ctx).await {
            Ok(output) => ToolResult {
                call_id: call.call_id,
                content: output.content,
                is_error: output.is_error,
            },
            Err(fault) => ToolResult {
                call_id: call.call_id,
                // Implementation faults also ride as error results so the
                // model can self-correct; the `fault` prefix keeps them
                // distinguishable from business failures in transcripts.
                content: format!("tool fault: {fault}"),
                is_error: true,
            },
        }
    }

    /// Attributes come from the tool itself, keeping dispatch truthful.
    fn is_read_only(&self, tool: &str) -> bool {
        self.registry.get(tool).is_some_and(|t| t.is_read_only())
    }

    /// Unknown names stay destructive so dispatch stays serial and safe.
    fn is_destructive(&self, tool: &str) -> bool {
        self.registry.get(tool).is_none_or(|t| t.is_destructive())
    }

    /// Advertise exactly the registered tools.
    fn available_tools(&self) -> Vec<ToolRef> {
        self.registry
            .specs()
            .into_iter()
            .map(|spec| ToolRef {
                name: spec.name.clone(),
                description: spec.description.clone(),
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wavecode_tools::{Result as ToolResultAlias, Tool, ToolCtx, ToolOutput};

    struct EchoTool;

    #[async_trait::async_trait]
    impl Tool for EchoTool {
        fn name(&self) -> &str {
            "echo_tool"
        }
        fn description(&self) -> &str {
            "test echo tool"
        }
        fn input_schema(&self) -> serde_json::Value {
            serde_json::Value::Null
        }
        fn is_read_only(&self) -> bool {
            true
        }
        async fn execute(
            &self,
            input: serde_json::Value,
            _ctx: &ToolCtx,
        ) -> ToolResultAlias<ToolOutput> {
            Ok(ToolOutput {
                content: input.to_string(),
                is_error: false,
            })
        }
    }

    struct FaultyTool;

    #[async_trait::async_trait]
    impl Tool for FaultyTool {
        fn name(&self) -> &str {
            "faulty_tool"
        }
        fn description(&self) -> &str {
            "test faulty tool"
        }
        fn input_schema(&self) -> serde_json::Value {
            serde_json::Value::Null
        }
        fn is_read_only(&self) -> bool {
            true
        }
        async fn execute(
            &self,
            _input: serde_json::Value,
            _ctx: &ToolCtx,
        ) -> ToolResultAlias<ToolOutput> {
            Err(std::io::Error::other("disk gone").into())
        }
    }

    fn adapter() -> ToolAdapter {
        let registry = wavecode_tools::Registry::builtin();
        registry.register(Arc::new(EchoTool));
        registry.register(Arc::new(FaultyTool));
        ToolAdapter::new(
            Arc::new(registry),
            ToolCtx {
                cwd: std::path::PathBuf::from("/tmp"),
                deny_env: Vec::new(),
            },
        )
    }

    fn call(name: &str) -> ToolCall {
        ToolCall {
            call_id: "c1".to_string(),
            name: name.to_string(),
            input: serde_json::Value::Null,
        }
    }

    #[tokio::test]
    async fn executes_registered_tools() {
        let out = adapter().execute(call("echo_tool")).await;
        assert!(!out.is_error);
        assert_eq!(out.call_id, "c1");
    }

    #[tokio::test]
    async fn unknown_tools_become_error_results() {
        let out = adapter().execute(call("nope")).await;
        assert!(out.is_error);
        assert!(out.content.contains("unknown tool"));
    }

    #[test]
    fn unknown_tools_stay_destructive() {
        let adapter = adapter();
        // Unknown names take the cautious path (serial dispatch), matching
        // the native and composite executors plus the policy adapter.
        assert!(adapter.is_destructive("nope"));
        assert!(!adapter.is_read_only("nope"));
    }

    #[tokio::test]
    async fn implementation_faults_stay_distinguishable() {
        let out = adapter().execute(call("faulty_tool")).await;
        assert!(out.is_error);
        assert!(out.content.starts_with("tool fault:"));
    }
}
