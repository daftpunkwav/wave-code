/*!
 * @file PruningExecutor
 * @description Executor decorator spilling oversized tool outputs to the
 * side-store before they reach conversation history.
 *
 * Responsibilities:
 * - Delegate execution to the composed executor unchanged.
 * - Prune result content over the threshold through `spill::prune_tool_output`.
 * - Pass through tool attributes and catalog untouched.
 *
 * Wiring lives here (composition root) because the runner must not depend
 * on capabilities: pruning is a composition-time policy, not a loop concern.
 */

use runtime_runner::{ToolCall, ToolExecutor, ToolRef, ToolResult};
use wavecode_context::spill::{DEFAULT_PRUNE_THRESHOLD_CHARS, SpillStore, prune_tool_output};

/// Executor decorator pruning oversized tool outputs.
///
/// Without this the full output of a chatty tool (long greps, verbose
/// builds) flows into history unbounded and burns context until the
/// compactor fires; pruned results carry a `spill://` URI the model can
/// read back through the `spill` tool.
pub struct PruningExecutor<E> {
    inner: E,
    store: SpillStore,
    threshold_chars: usize,
}

impl<E: ToolExecutor> PruningExecutor<E> {
    /// Wrap `inner` with the default prune threshold.
    pub fn new(inner: E, store: SpillStore) -> Self {
        Self {
            inner,
            store,
            threshold_chars: DEFAULT_PRUNE_THRESHOLD_CHARS,
        }
    }

    /// Override the prune threshold (tests).
    pub fn with_threshold(mut self, threshold_chars: usize) -> Self {
        self.threshold_chars = threshold_chars;
        self
    }
}

#[async_trait::async_trait]
impl<E: ToolExecutor> ToolExecutor for PruningExecutor<E> {
    async fn execute(&self, call: ToolCall) -> ToolResult {
        // The spill read-back passes through: re-pruning the content the
        // model just asked for would bury it behind another marker.
        let exempt = call.name == "spill";
        let mut result = self.inner.execute(call).await;
        if !exempt {
            result.content = prune_tool_output(&result.content, &self.store, self.threshold_chars);
        }
        result
    }

    fn is_read_only(&self, tool: &str) -> bool {
        self.inner.is_read_only(tool)
    }

    fn is_destructive(&self, tool: &str) -> bool {
        self.inner.is_destructive(tool)
    }

    fn available_tools(&self) -> Vec<ToolRef> {
        self.inner.available_tools()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct StubExecutor;

    #[async_trait::async_trait]
    impl ToolExecutor for StubExecutor {
        async fn execute(&self, call: ToolCall) -> ToolResult {
            ToolResult {
                call_id: call.call_id,
                content: "x".repeat(100),
                is_error: false,
            }
        }

        fn is_read_only(&self, tool: &str) -> bool {
            tool == "read"
        }
    }

    fn call(name: &str) -> ToolCall {
        ToolCall {
            call_id: "c1".to_string(),
            name: name.to_string(),
            input: serde_json::Value::Null,
        }
    }

    fn pruning_executor(threshold: usize) -> (PruningExecutor<StubExecutor>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let store = SpillStore::new(dir.path().to_path_buf());
        (
            PruningExecutor::new(StubExecutor, store).with_threshold(threshold),
            dir,
        )
    }

    #[tokio::test]
    async fn oversized_outputs_spill_and_small_ones_pass() {
        let (executor, _dir) = pruning_executor(10);
        let pruned = executor.execute(call("grep")).await;
        assert!(
            pruned.content.starts_with("[pruned:"),
            "{:?}",
            pruned.content
        );
        assert!(pruned.content.contains("spill://"), "{:?}", pruned.content);
        let (executor, _dir) = pruning_executor(200);
        let intact = executor.execute(call("grep")).await;
        assert_eq!(intact.content, "x".repeat(100));
    }

    #[tokio::test]
    async fn spill_read_back_is_exempt() {
        let (executor, _dir) = pruning_executor(10);
        let read_back = executor.execute(call("spill")).await;
        assert_eq!(read_back.content, "x".repeat(100));
    }

    #[tokio::test]
    async fn attributes_pass_through() {
        let (executor, _dir) = pruning_executor(10);
        assert!(executor.is_read_only("read"));
        assert!(!executor.is_read_only("grep"));
        assert!(executor.is_destructive("unknown"));
    }
}
