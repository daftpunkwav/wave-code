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
        self.registry.get(tool).is_some_and(|t| t.is_destructive())
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
        let mut registry = wavecode_tools::Registry::builtin();
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

    #[tokio::test]
    async fn implementation_faults_stay_distinguishable() {
        let out = adapter().execute(call("faulty_tool")).await;
        assert!(out.is_error);
        assert!(out.content.starts_with("tool fault:"));
    }
}
