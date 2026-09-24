/*!
 * @file AskUserTool
 * @description Interactive question surface; the user's answer is the result.
 *
 * Responsibilities:
 * - Declare the ask_user tool surface (schema, read-only attribute).
 * - Validate the question payload; valid calls never execute here.
 *
 * Valid ask_user calls are routed by the sandbox into the question flow:
 * the runner parks, the frontend renders the question, and the user's
 * answer becomes the tool result. Execution only reaches this body when
 * the payload was invalid (schema errors) or the interactive gate was
 * bypassed, so both paths report openly.
 *
 * This module must not depend on: drivers, actors, or sessions.
 */

//! `ask_user`: ask the human a question mid-turn and continue with the
//! answer. Options are numbered for one-key selection; free text is
//! always allowed. Asking is read-only work: it never parks on an
//! approval gate, and it works in every permission mode.

use serde_json::Value;

use crate::{Result, Tool, ToolCtx, ToolOutput};

/// Cap on answer options; more would defeat one-key selection.
pub const MAX_QUESTION_OPTIONS: usize = 4;

/// Ask the user a question and continue with their answer (read-only).
pub struct AskUserTool;

#[async_trait::async_trait]
impl Tool for AskUserTool {
    fn name(&self) -> &str {
        "ask_user"
    }

    fn description(&self) -> &str {
        "Ask the user a question and continue with their answer. Use it \
         when requirements are ambiguous, choices are the user's to make, \
         or you need a decision you cannot derive. Provide up to 4 \
         numbered options for one-key selection; the user can always type \
         a free-form answer or dismiss the question. Asking is read-only \
         and works in every permission mode."
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "question": {
                    "type": "string",
                    "description": "The question to ask (non-empty)"
                },
                "options": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "Up to 4 answer options shown numbered; omit for free-text-only",
                    "maxItems": MAX_QUESTION_OPTIONS
                }
            },
            "required": ["question"]
        })
    }

    fn is_read_only(&self) -> bool {
        true
    }

    async fn execute(&self, input: Value, _ctx: &ToolCtx) -> Result<ToolOutput> {
        let question = input
            .get("question")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .unwrap_or("");
        if question.is_empty() {
            return Ok(ToolOutput {
                content: "missing parameter 'question' (non-empty string required)".into(),
                is_error: true,
            });
        }
        if let Some(items) = input.get("options").and_then(|v| v.as_array()) {
            if items.len() > MAX_QUESTION_OPTIONS {
                return Ok(ToolOutput {
                    content: format!(
                        "invalid parameter 'options': at most {MAX_QUESTION_OPTIONS} options"
                    ),
                    is_error: true,
                });
            }
            if items
                .iter()
                .any(|v| !v.as_str().map(|s| !s.trim().is_empty()).unwrap_or(false))
            {
                return Ok(ToolOutput {
                    content: "invalid parameter 'options' (non-empty strings required)".into(),
                    is_error: true,
                });
            }
        }
        // Valid payloads are parked by the question flow before execution;
        // reaching here means that flow was bypassed.
        Ok(ToolOutput {
            content: "no interactive question gate is attached to this session".into(),
            is_error: true,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> ToolCtx {
        ToolCtx {
            cwd: std::env::temp_dir(),
            deny_env: Vec::new(),
        }
    }

    #[tokio::test]
    async fn invalid_payloads_fail_openly_without_side_effects() {
        let tool = AskUserTool;
        assert!(tool.is_read_only());
        let blank = tool
            .execute(serde_json::json!({"question": "  "}), &ctx())
            .await
            .unwrap();
        assert!(blank.is_error);
        assert!(blank.content.contains("'question'"));
        let no_question = tool.execute(serde_json::json!({}), &ctx()).await.unwrap();
        assert!(no_question.is_error);
        let too_many = tool
            .execute(
                serde_json::json!({"question": "q", "options": ["1", "2", "3", "4", "5"]}),
                &ctx(),
            )
            .await
            .unwrap();
        assert!(too_many.is_error);
        assert!(too_many.content.contains("at most 4"));
        let blank_option = tool
            .execute(
                serde_json::json!({"question": "q", "options": ["ok", "  "]}),
                &ctx(),
            )
            .await
            .unwrap();
        assert!(blank_option.is_error);
    }
}
