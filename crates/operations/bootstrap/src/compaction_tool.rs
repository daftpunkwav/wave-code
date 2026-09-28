/*!
 * @file CompactContextTool
 * @description The model-invokable compaction request tool.
 *
 * Responsibilities:
 * - Queue a compaction request into the loop's shared slot, with a
 *   model-supplied reason for the review gate and the session journal.
 * - Return immediately: the grant decision belongs to the loop's gate
 *   at the next loop head, never to tool execution (a grant rewrites
 *   the history this very result still rides).
 *
 * This module must not depend on: the run loop, drivers, or actors.
 * Assembly owns the slot handle and wires both sides.
 */

//! Model-requested compaction: a thin [`Tool`] over the runner's
//! [`CompactionRequests`] slot. The tool validates the reason, queues it,
//! and answers with what the model can rely on — that the loop will
//! review the request — without promising a grant.

use std::sync::Arc;

use runtime_runner::CompactionRequests;
use wavecode_tools::{Result, Tool, ToolCtx, ToolOutput};

/// The `compact_context` tool: requests a history compaction.
pub struct CompactContextTool {
    requests: Arc<CompactionRequests>,
}

impl CompactContextTool {
    /// Bind the tool to the loop's request slot.
    pub fn new(requests: Arc<CompactionRequests>) -> Self {
        Self { requests }
    }
}

#[async_trait::async_trait]
impl Tool for CompactContextTool {
    fn name(&self) -> &str {
        "compact_context"
    }

    fn kind(&self) -> wavecode_protocol::ToolKind {
        wavecode_protocol::ToolKind::SessionState
    }

    fn is_read_only(&self) -> bool {
        // It mutates session state only through the loop's reviewed grant;
        // the tool itself touches nothing, so read-only parallel dispatch
        // stays correct.
        true
    }

    fn description(&self) -> &str {
        "Request compaction of the conversation history into a summary. \
         Use when the Context usage note shows heavy usage and earlier \
         tool output is no longer needed verbatim, or before a long \
         focused phase that needs the room. The loop reviews the request: \
         it is granted only when usage is high enough and budget allows, \
         and otherwise denied with the reason in the next Context usage \
         note. The turn continues either way; this call never blocks."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "reason": {
                    "type": "string",
                    "description": "Why the history should be compacted now (recorded with the request)"
                }
            },
            "required": ["reason"]
        })
    }

    async fn execute(&self, input: serde_json::Value, _ctx: &ToolCtx) -> Result<ToolOutput> {
        let reason = match input.get("reason").and_then(serde_json::Value::as_str) {
            Some(reason) if !reason.trim().is_empty() => reason.trim(),
            _ => {
                return Ok(ToolOutput {
                    content: "missing or empty parameter 'reason' (string required)"
                        .to_string(),
                    is_error: true,
                });
            }
        };
        self.requests.request(reason.to_string());
        Ok(ToolOutput {
            content: "compaction requested; the loop reviews it before the \
                      next sample and reports the verdict in the next \
                      Context usage note"
                .to_string(),
            is_error: false,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tool() -> CompactContextTool {
        CompactContextTool::new(Arc::new(CompactionRequests::new()))
    }

    fn ctx() -> ToolCtx {
        ToolCtx {
            cwd: std::path::PathBuf::from("."),
            deny_env: Vec::new(),
        }
    }

    #[tokio::test]
    async fn queues_the_reason_and_reports_the_review() {
        let requests = Arc::new(CompactionRequests::new());
        let tool = CompactContextTool::new(requests.clone());
        let output = tool
            .execute(json!({"reason": "history is heavy"}), &ctx())
            .await
            .unwrap();
        assert!(!output.is_error, "{:?}", output.content);
        let drained = requests.take_all();
        assert_eq!(drained, vec!["history is heavy".to_string()]);
        assert!(
            requests.take_all().is_empty(),
            "a drained slot stays drained"
        );
    }

    #[tokio::test]
    async fn blank_reasons_are_business_errors() {
        let tool = tool();
        for payload in [json!({}), json!({"reason": ""}), json!({"reason": "   "})] {
            let output = tool.execute(payload.clone(), &ctx()).await.unwrap();
            assert!(output.is_error, "{payload:?} must fail: {:?}", output.content);
            assert!(output.content.contains("reason"));
        }
    }

    #[test]
    fn session_state_kind_and_schema_shape() {
        let tool = tool();
        assert_eq!(
            tool.kind(),
            wavecode_protocol::ToolKind::SessionState,
            "session-state classification keeps every approval mode frictionless"
        );
        let schema = tool.input_schema();
        assert_eq!(schema["required"][0], "reason");
        assert_eq!(schema["properties"]["reason"]["type"], "string");
    }
}
