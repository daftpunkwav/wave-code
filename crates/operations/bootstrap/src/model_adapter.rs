/*!
 * @file ModelAdapter
 * @description Adapts a legacy chat model to the ModelGateway seam.
 *
 * Responsibilities:
 * - Build provider requests from seam requests with registry schemas.
 * - Fold the provider event stream into response blocks.
 * - Map transport failures onto retry-relevant sample errors.
 *
 * This module must not depend on: runtime internals beyond its trait seam.
 */

//! [`runtime_runner::ModelGateway`] implemented over `wavecode-llm`.

use std::sync::Arc;

use futures::StreamExt;
use runtime_runner::{
    ModelGateway, SampleBlock, SampleError, SampleRequest, SampleResponse, ToolRef,
};
use wavecode_llm::{
    ChatModel, ChatRequest, ContentBlock, LlmError, Message, Role, StreamEvent, ToolSpec,
};

/// Samples a legacy chat model through the gateway seam.
pub struct ModelAdapter {
    model: Arc<dyn ChatModel>,
    model_name: String,
    max_tokens: u32,
    registry: Arc<wavecode_tools::Registry>,
}

impl ModelAdapter {
    /// Wrap a shared model; tool schemas come from the registry so the
    /// model always sees the same tools the executor can run.
    pub fn new(
        model: Arc<dyn ChatModel>,
        model_name: String,
        max_tokens: u32,
        registry: Arc<wavecode_tools::Registry>,
    ) -> Self {
        Self {
            model,
            model_name,
            max_tokens,
            registry,
        }
    }

    /// Convert seam messages; empty model-side texts are dropped because
    /// providers reject empty assistant messages.
    fn messages(request: &SampleRequest) -> Vec<Message> {
        request
            .messages
            .iter()
            .filter(|m| !m.from_model || !m.text.is_empty())
            .map(|m| Message {
                role: if m.from_model {
                    Role::Assistant
                } else {
                    Role::User
                },
                content: vec![ContentBlock::Text {
                    text: m.text.clone(),
                }],
            })
            .collect()
    }

    /// Advertise exactly the registered tools, schemas included.
    fn tools(&self, request: &SampleRequest) -> Vec<ToolSpec> {
        request
            .tools
            .iter()
            .filter_map(|ToolRef { name, .. }| self.registry.get(name))
            .map(|tool| ToolSpec {
                name: tool.name().to_string(),
                description: tool.description().to_string(),
                input_schema: tool.input_schema(),
            })
            .collect()
    }
}

/// One tool block under assembly from stream deltas.
struct PendingTool {
    call_id: String,
    name: String,
    input_buf: String,
}

/// Flush buffered text into the block list when non-empty.
fn flush_text(text: &mut String, blocks: &mut Vec<SampleBlock>) {
    if !text.is_empty() {
        blocks.push(SampleBlock::Text(std::mem::take(text)));
    }
}

#[async_trait::async_trait]
impl ModelGateway for ModelAdapter {
    async fn sample(&self, request: SampleRequest) -> Result<SampleResponse, SampleError> {
        let req = ChatRequest {
            model: self.model_name.clone(),
            system: request.system.clone(),
            messages: Arc::new(Self::messages(&request)),
            tools: self.tools(&request),
            max_tokens: self.max_tokens,
        };
        let mut stream = self.model.stream(req).await.map_err(|e| map_error(&e))?;
        let mut blocks = Vec::new();
        let mut text = String::new();
        let mut pending: Option<PendingTool> = None;
        let mut input_tokens: Option<u64> = None;
        let mut output_tokens: Option<u64> = None;
        let mut truncated = false;
        while let Some(event) = stream.next().await {
            let event = event.map_err(|e| map_error(&e))?;
            match event {
                StreamEvent::TextDelta { text: delta } => text.push_str(&delta),
                StreamEvent::ToolUseBegin { id, name } => {
                    flush_text(&mut text, &mut blocks);
                    pending = Some(PendingTool {
                        call_id: id,
                        name,
                        input_buf: String::new(),
                    });
                }
                StreamEvent::ToolUseInputDelta { partial_json } => {
                    if let Some(tool) = pending.as_mut() {
                        tool.input_buf.push_str(&partial_json);
                    }
                    // Deltas without a begun block violate the provider
                    // contract; dropping them keeps the loop honest instead
                    // of inventing a call id.
                }
                StreamEvent::BlockEnd => {
                    if let Some(tool) = pending.take() {
                        blocks.push(SampleBlock::ToolUse {
                            call_id: tool.call_id,
                            name: tool.name,
                            input: parse_tool_input(&tool.input_buf),
                        });
                    } else {
                        flush_text(&mut text, &mut blocks);
                    }
                }
                StreamEvent::MessageComplete { stop_reason, usage } => {
                    // Defensive flush: a well-formed stream ends every block,
                    // but a truncated stream must not lose content silently.
                    if let Some(tool) = pending.take() {
                        blocks.push(SampleBlock::ToolUse {
                            call_id: tool.call_id,
                            name: tool.name,
                            input: parse_tool_input(&tool.input_buf),
                        });
                    }
                    flush_text(&mut text, &mut blocks);
                    // Only the output-limit stop feeds the continuation
                    // path; every other stop ends the assistant message.
                    truncated = stop_reason == "max_tokens";
                    input_tokens = Some(usage.input_tokens);
                    output_tokens = Some(usage.output_tokens);
                }
            }
        }
        Ok(SampleResponse {
            blocks,
            input_tokens,
            output_tokens,
            truncated,
        })
    }
}

/// Parse accumulated tool input; unparseable input stays a string so the
/// tool validation layer fails honestly instead of receiving invented JSON.
fn parse_tool_input(buf: &str) -> serde_json::Value {
    if buf.is_empty() {
        return serde_json::Value::Null;
    }
    serde_json::from_str(buf).unwrap_or_else(|_| serde_json::Value::String(buf.to_string()))
}

/// Map provider errors: only the context-window signal is retryable via
/// compaction; everything else fails the run after usage settle.
fn map_error(error: &LlmError) -> SampleError {
    match error {
        LlmError::PromptTooLong { .. } => SampleError::PromptTooLong,
        LlmError::Timeout(_) => SampleError::Timeout,
        other => SampleError::Transport(other.to_string()),
    }
}

/// Re-exported for seam tests over [`LiteMessage`] conversions.
#[doc(hidden)]
pub fn __test_messages(request: &SampleRequest) -> Vec<Message> {
    ModelAdapter::messages(request)
}

#[cfg(test)]
mod tests {
    use super::*;
    use runtime_runner::LiteMessage;
    use wavecode_llm::{ChatModel, EventStream, Usage};

    #[derive(Debug, Clone)]
    struct ScriptedModel {
        events: Vec<StreamEvent>,
    }

    #[async_trait::async_trait]
    impl ChatModel for ScriptedModel {
        async fn stream(&self, _req: ChatRequest) -> wavecode_llm::Result<EventStream> {
            let events: Vec<wavecode_llm::Result<StreamEvent>> =
                self.events.iter().cloned().map(Ok).collect();
            Ok(Box::pin(futures::stream::iter(events)))
        }
    }

    fn adapter(events: Vec<StreamEvent>) -> ModelAdapter {
        ModelAdapter::new(
            Arc::new(ScriptedModel { events }),
            "test-model".to_string(),
            100,
            Arc::new(wavecode_tools::Registry::builtin()),
        )
    }

    fn request() -> SampleRequest {
        SampleRequest {
            system: "sys".to_string(),
            messages: vec![LiteMessage {
                from_model: false,
                text: "hello".to_string(),
            }],
            tools: vec![],
        }
    }

    #[tokio::test]
    async fn folds_deltas_into_ordered_blocks() {
        let response = adapter(vec![
            StreamEvent::TextDelta {
                text: "hi".to_string(),
            },
            StreamEvent::ToolUseBegin {
                id: "c1".to_string(),
                name: "shell".to_string(),
            },
            StreamEvent::ToolUseInputDelta {
                partial_json: "{\"command\":".to_string(),
            },
            StreamEvent::ToolUseInputDelta {
                partial_json: " \"ls\"}".to_string(),
            },
            StreamEvent::BlockEnd,
            StreamEvent::MessageComplete {
                stop_reason: "end_turn".to_string(),
                usage: Usage {
                    input_tokens: 1,
                    output_tokens: 2,
                },
            },
        ])
        .sample(request())
        .await
        .unwrap();
        assert_eq!(response.blocks.len(), 2);
        assert_eq!(response.blocks[0], SampleBlock::Text("hi".to_string()));
        assert_eq!(
            response.blocks[1],
            SampleBlock::ToolUse {
                call_id: "c1".to_string(),
                name: "shell".to_string(),
                input: serde_json::json!({"command": "ls"}),
            }
        );
        assert_eq!(response.output_tokens, Some(2));
    }

    #[tokio::test]
    async fn empty_model_texts_are_dropped_from_requests() {
        let req = SampleRequest {
            system: String::new(),
            messages: vec![
                LiteMessage {
                    from_model: true,
                    text: String::new(),
                },
                LiteMessage {
                    from_model: false,
                    text: String::new(),
                },
            ],
            tools: vec![],
        };
        let messages = __test_messages(&req);
        // Empty assistant texts are dropped; empty user texts are kept.
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].role, Role::User);
    }
}
