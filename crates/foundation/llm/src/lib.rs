//! wavecode-llm - multi-provider abstraction layer.
//!
//! Defines the unified Messages request / streaming event interface (SSE). M1 covers:
//! - Shared types ([`Message`] / [`ContentBlock`] / [`ToolSpec`] / [`StreamEvent`], etc.)
//!   and the [`ChatModel`] trait;
//! - The Anthropic Messages streaming SSE parser ([`SseParser`]);
//! - Built-in implementation: the Anthropic Messages API streaming client ([`AnthropicClient`]).
//!
//! The HTTP client implementation for OpenAI-compatible providers, plus token
//! counting and the model capability table (context window, max output, etc.), land in later milestones.

use std::sync::Arc;

use serde::{Deserialize, Serialize};

pub mod anthropic;
mod sse;

pub use anthropic::AnthropicClient;
pub use sse::SseParser;

/// Conversation role.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    User,
    Assistant,
}

/// A message content block.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    /// Plain-text block.
    Text { text: String },
    /// A tool call initiated by the model.
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    /// A tool execution result, fed back to the model.
    ToolResult {
        tool_use_id: String,
        content: String,
        #[serde(default)]
        is_error: bool,
    },
}

/// One conversation message.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Message {
    pub role: Role,
    pub content: Vec<ContentBlock>,
}

/// A tool definition (sent to the model with the request).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
}

/// Token usage statistics.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

/// A streaming response event.
#[derive(Debug, Clone, PartialEq)]
pub enum StreamEvent {
    /// Text delta.
    TextDelta { text: String },
    /// Start of a tool-call block.
    ToolUseBegin { id: String, name: String },
    /// Incremental input JSON of a tool call.
    ToolUseInputDelta { partial_json: String },
    /// End of the current content block.
    BlockEnd,
    /// The whole message is complete.
    MessageComplete { stop_reason: String, usage: Usage },
}

/// One streaming chat request.
#[derive(Debug, Clone)]
pub struct ChatRequest {
    pub model: String,
    pub system: String,
    /// History snapshots are shared via `Arc` (P3, SPEC section 17.5 M4): callers freeze
    /// the current round of history with an O(1) pointer clone instead of a per-round
    /// O(n²) deep copy; provider implementations serialize inside `stream()`,
    /// never holding the snapshot long-term.
    pub messages: Arc<Vec<Message>>,
    pub tools: Vec<ToolSpec>,
    pub max_tokens: u32,
}

/// Unified return type for streaming event streams (shared by `ChatModel::stream` and each provider implementation).
pub type EventStream = std::pin::Pin<Box<dyn futures::Stream<Item = Result<StreamEvent>> + Send>>;

/// Unified streaming chat-model abstraction.
#[async_trait::async_trait]
pub trait ChatModel: Send + Sync {
    /// Start a streaming request and return the event stream.
    async fn stream(&self, req: ChatRequest) -> Result<EventStream>;
}

/// Unified error type for the llm crate.
#[derive(Debug, thiserror::Error)]
pub enum LlmError {
    /// HTTP transport error.
    #[error("HTTP error: {0}")]
    Http(String),
    /// Business error returned by the API (e.g. overloaded_error).
    #[error("API error ({kind}): {message}")]
    Api { kind: String, message: String },
    /// Overlong-context error (the trigger for core reactive compact, SPEC section 5.2):
    /// the provider explicitly reports an oversized prompt / request (e.g. Anthropic 400
    /// "prompt is too long", 413 request_too_large). Split out from the generic Api error
    /// as its own variant so upper layers can match on the enum instead of sniffing strings.
    #[error("prompt exceeds context limit: {message}")]
    PromptTooLong { message: String },
    /// SSE frame parse error.
    #[error("SSE parse error: {0}")]
    Sse(String),
    /// Streaming-read stall timeout (decided by the stall watchdog, not a transport error):
    /// the gap between adjacent byte chunks exceeded the idle threshold, so the upstream
    /// connection may have stalled. Kept separate from [`LlmError::Http`] so upper layers
    /// can tell "connection error" apart from "upstream stall" (e.g. for future retry policy).
    #[error("stream stall timeout: {0}")]
    Timeout(String),
    /// JSON serialization / deserialization error.
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
}

/// Single construction point for API errors (shared by anthropic non-2xx responses and SSE error events):
/// Recognizes the known prompt_too_long shapes - when kind or message contains
/// the `prompt_too_long` / `request_too_large` markers, or message contains
/// "prompt is too long" (the Anthropic 400 wording) - it becomes
/// [`LlmError::PromptTooLong`]; everything else is returned as generic [`LlmError::Api`].
/// Classification is centralized at this single point; new provider shapes only change this spot.
pub(crate) fn classify_api_error(kind: String, message: String) -> LlmError {
    let too_long = kind.contains("prompt_too_long")
        || kind.contains("request_too_large")
        || message.contains("prompt is too long")
        || message.contains("prompt_too_long")
        || message.contains("request_too_large");
    if too_long {
        LlmError::PromptTooLong { message }
    } else {
        LlmError::Api { kind, message }
    }
}

/// Crate-wide unified Result alias.
pub type Result<T> = std::result::Result<T, LlmError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_maps_known_too_long_shapes() {
        // Anthropic 400 wording shape
        assert!(matches!(
            classify_api_error(
                "http_400".into(),
                "prompt is too long: 210000 tokens > 200000 maximum".into()
            ),
            LlmError::PromptTooLong { .. }
        ));
        // 413 request_too_large (carried in either kind or message)
        assert!(matches!(
            classify_api_error("request_too_large".into(), "request too large".into()),
            LlmError::PromptTooLong { .. }
        ));
        assert!(matches!(
            classify_api_error(
                "http_413".into(),
                r#"{"type":"error","error":{"type":"request_too_large"}}"#.into()
            ),
            LlmError::PromptTooLong { .. }
        ));
        // SSE error event kind shape
        assert!(matches!(
            classify_api_error("prompt_too_long".into(), "too long".into()),
            LlmError::PromptTooLong { .. }
        ));
    }

    #[test]
    fn classify_keeps_other_api_errors_generic() {
        let err = classify_api_error("overloaded_error".into(), "overloaded".into());
        assert!(
            matches!(&err, LlmError::Api { kind, message } if kind == "overloaded_error" && message == "overloaded"),
            "non-too-long shapes should stay the Api variant: {err:?}"
        );
        // 401-style auth errors must not be misclassified
        assert!(matches!(
            classify_api_error("http_401".into(), "invalid api key".into()),
            LlmError::Api { .. }
        ));
    }
}
