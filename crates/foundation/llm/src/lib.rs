/*!
 * @file WavecodeLlm
 * @description Unified multi-provider LLM abstraction layer and streaming event parser.
 *
 * Responsibilities:
 * - Define unified message, tool, event, and error types across LLM backends.
 * - Provide unified ChatModel trait and streaming SSE abstractions.
 * - Enforce clean error taxonomy and fallible client initialization.
 *
 * This crate must not depend on: runtime, capabilities, config, or frontends.
 */

//! wavecode-llm - multi-provider abstraction layer.
//!
//! Defines the unified Messages request / streaming event interface (SSE) across
//! three wire dialects:
//! - Anthropic Messages ([`AnthropicClient`], `POST {base_url}/v1/messages`);
//! - OpenAI Chat Completions ([`OpenAIClient`], `POST {base_url}/chat/completions`),
//!   the dialect most third-party gateways speak;
//! - OpenAI Responses ([`ResponsesClient`], `POST {base_url}/responses`), the
//!   endpoint that serves reasoning families with no Chat Completions route.
//!
//! Shared types ([`Message`] / [`ContentBlock`] / [`ToolSpec`] / [`StreamEvent`]),
//! the [`ChatModel`] trait, the Anthropic SSE parser ([`SseParser`]), and the
//! approximate model capability table ([`ModelCapabilities`]) complete the layer.

use std::sync::Arc;

use serde::{Deserialize, Serialize};

pub mod anthropic;
pub mod openai;
pub mod responses;
pub mod retry;
mod sse;

pub use anthropic::{AnthropicClient, CacheTtl};
pub use openai::{ModelCapabilities, OpenAIClient};
pub use responses::ResponsesClient;
pub use sse::SseParser;

/// Max retained chars of a provider error response body, shared by every
/// client so the taxonomy stays uniform across wires.
pub(crate) const MAX_ERROR_BODY_CHARS: usize = 2000;

/// Truncates an error response body to `max_chars` characters (by char, so
/// multibyte text is never split). The API key only travels in request
/// headers and never enters error text.
pub(crate) fn truncate_error_body(body: &str, max_chars: usize) -> String {
    body.chars().take(max_chars).collect()
}

/// Conversation role.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    User,
    Assistant,
}

/// Maximum decoded image bytes accepted for an attachment (5 MB).
/// Oversized images are rejected with the cap named; no downscaling is
/// attempted (downscaling would silently degrade model input).
pub const IMAGE_MAX_BYTES: usize = 5 * 1024 * 1024;

/// Allowlisted image MIME types for attachments.
pub const IMAGE_ALLOWED_MIMES: &[&str] = &["image/png", "image/jpeg", "image/webp", "image/gif"];

/// Binary image attachment (validated, base64-encoded).
///
/// `mime` must be in [`IMAGE_ALLOWED_MIMES`]; `base64` must decode to at most
/// [`IMAGE_MAX_BYTES`] bytes. Use [`validate_image`] to check both sides
/// before sending.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Attachment {
    /// MIME type (allowlisted, see [`IMAGE_ALLOWED_MIMES`]).
    pub mime: String,
    /// Base64-encoded image bytes.
    pub base64: String,
}

/// Validate an image attachment: MIME allowlist plus base64 decode plus the
/// [`IMAGE_MAX_BYTES`] cap. Returns the decoded bytes on success; the error
/// string names the violated constraint (mime, base64, or the byte cap).
pub fn validate_image(mime: &str, base64_str: &str) -> std::result::Result<Vec<u8>, String> {
    if !IMAGE_ALLOWED_MIMES.contains(&mime) {
        return Err(format!(
            "unsupported image mime '{mime}': expected one of image/png, image/jpeg, image/webp, image/gif"
        ));
    }
    use base64::Engine as _;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(base64_str)
        .map_err(|e| format!("invalid image base64: {e}"))?;
    if bytes.len() > IMAGE_MAX_BYTES {
        return Err(format!(
            "image too large ({} bytes), max {} bytes (5MB cap, no downscaling)",
            bytes.len(),
            IMAGE_MAX_BYTES
        ));
    }
    Ok(bytes)
}

/// A message content block.
///
/// The `Image` variant carries a validated attachment inline (`id` is an
/// optional client-side label, kept through translation). Providers map it
/// onto their native wire shape (OpenAI `image_url` data-URL parts,
/// Anthropic `image` blocks); oversized or non-allowlisted images are
/// rejected during translation with the constraint named.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    /// Plain-text block.
    Text { text: String },
    /// Inline image attachment (see [`Attachment`]).
    Image {
        /// Optional client-side label (preserved, never sent to providers).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        /// MIME type (must be in [`IMAGE_ALLOWED_MIMES`]).
        mime: String,
        /// Base64-encoded image bytes (decoded size at most [`IMAGE_MAX_BYTES`]).
        base64: String,
    },
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
    /// Reasoning the model emitted alongside its turn (see
    /// [`state_store::Block::Thinking`]): the Anthropic wire requires the
    /// block back on a tool-call turn, so the adapter preserves it in
    /// history. Providers with no thinking wire shape skip it.
    Thinking {
        /// Reasoning text as streamed by the model.
        text: String,
        /// Provider signature binding the text to its turn; `None` for
        /// endpoints streaming unsigned thinking.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
    },
}

/// Key used by [`normalize_tool_input`] when wrapping a non-object payload.
pub const TOOL_INPUT_RAW_KEY: &str = "_raw";

/// Coerce a tool-use `input` into the object form every tool-calling wire
/// requires. Models occasionally emit a non-object JSON value (a bare
/// string, number, array, or a truncated fragment); replaying such a block
/// verbatim makes the whole request fail with a 400 on `tool_use.input`
/// and permanently poisons the session history. `null` means "no arguments"
/// and becomes an empty object; any other non-object payload is wrapped
/// verbatim under [`TOOL_INPUT_RAW_KEY`], keeping the block wire-legal while
/// tool validation still fails honestly on the wrapped shape instead of
/// inventing matching arguments.
pub fn normalize_tool_input(input: serde_json::Value) -> serde_json::Value {
    match input {
        value @ serde_json::Value::Object(_) => value,
        serde_json::Value::Null => serde_json::Value::Object(serde_json::Map::new()),
        other => serde_json::json!({ TOOL_INPUT_RAW_KEY: other }),
    }
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
    /// Total prompt tokens of the request, cache traffic included.
    ///
    /// Normalized across wire formats: Anthropic reports `input_tokens` as
    /// the *uncached remainder* and the parser adds the cache counters to
    /// it, while an OpenAI-compatible `prompt_tokens` already covers
    /// `cached_tokens` (a subset detail) and is passed through. Context
    /// accounting and the context meter compare this value against the
    /// window, so it must be the full request size on both paths.
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// Tokens served from the provider's prompt cache (Anthropic
    /// `cache_read_input_tokens`); 0 for providers without caching.
    /// Informational: already counted inside [`Self::input_tokens`].
    pub cache_read_tokens: u64,
    /// Tokens written to the provider's prompt cache by this request
    /// (Anthropic `cache_creation_input_tokens`); 0 for providers without
    /// caching. Cache writes are billed at a premium, reads at a discount.
    /// Informational: already counted inside [`Self::input_tokens`].
    pub cache_creation_tokens: u64,
}

/// A streaming response event.
#[derive(Debug, Clone, PartialEq)]
pub enum StreamEvent {
    /// Text delta.
    TextDelta { text: String },
    /// Extended-thinking text delta (Anthropic `thinking_delta`): forwarded
    /// incrementally for live display, then summarized by one
    /// [`Self::ThinkingComplete`] at the block's end.
    ThinkingDelta { text: String },
    /// Signature delta accompanying a thinking block (Anthropic
    /// `signature_delta`). Only meaningful to consumers that round-trip
    /// signed thinking blocks; others may ignore it.
    SignatureDelta { signature: String },
    /// A thinking block finished; carries the full text and signature so
    /// history can preserve the block (the Anthropic wire requires it back
    /// on tool-call turns). Emitted by parsers that can see block
    /// boundaries; parsers without them (Chat Completions) never emit it
    /// and their reasoning stays display-only.
    ThinkingComplete {
        text: String,
        signature: Option<String>,
    },
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
    /// History snapshots are shared via `Arc`: callers freeze
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

    /// Switch the reasoning-effort level for subsequent requests; false
    /// rejects the level.
    ///
    /// The level is a provider-specific wire string (`off` disables the
    /// parameter where the client supports that). Defaults to rejecting:
    /// only clients with a mutable effort field override this.
    fn set_thinking(&self, _effort: &str) -> bool {
        false
    }
}

/// Forwarding implementation so boxed models erase behind the trait
/// object: composition roots build `Arc<dyn ChatModel>` chains (retry
/// wrappers, fallbacks) without re-wrapping each layer.
#[async_trait::async_trait]
impl<M> ChatModel for std::sync::Arc<M>
where
    M: ChatModel + ?Sized,
{
    async fn stream(&self, req: ChatRequest) -> Result<EventStream> {
        self.as_ref().stream(req).await
    }

    fn set_thinking(&self, effort: &str) -> bool {
        self.as_ref().set_thinking(effort)
    }
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
    /// Rate-limit (429) response carrying the server's `Retry-After` hint
    /// when present. Split from [`LlmError::Api`] so the retry layer can
    /// honor the server's backoff instead of its own exponential guess;
    /// retryability itself follows the same transient rules as `Api`.
    #[error("rate limited: {message}")]
    RateLimited {
        message: String,
        retry_after: Option<std::time::Duration>,
    },
    /// Overlong-context error (the trigger for core reactive compact):
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
    /// HTTP client initialization failure (e.g. TLS backend init error).
    #[error("failed to initialize HTTP client: {0}")]
    ClientInit(String),
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

/// Non-2xx classification with the response's `Retry-After` hint: 429
/// responses become [`LlmError::RateLimited`] (keeping the hint), all
/// other shapes classify exactly like [`classify_api_error`].
pub(crate) fn classify_api_error_with_retry_after(
    kind: String,
    message: String,
    retry_after: Option<std::time::Duration>,
) -> LlmError {
    if kind.contains("http_429") {
        LlmError::RateLimited {
            message,
            retry_after,
        }
    } else {
        classify_api_error(kind, message)
    }
}

/// Parse the `Retry-After` header's delta-seconds form (`"2"`); the
/// HTTP-date form is ignored (returns `None`) — sub-second precision is
/// irrelevant at backoff scales.
pub(crate) fn parse_retry_after(
    headers: &reqwest::header::HeaderMap,
) -> Option<std::time::Duration> {
    let value = headers.get(reqwest::header::RETRY_AFTER)?.to_str().ok()?;
    let seconds: u64 = value.trim().parse().ok()?;
    (seconds > 0).then(|| std::time::Duration::from_secs(seconds))
}

/// Crate-wide unified Result alias.
pub type Result<T> = std::result::Result<T, LlmError>;

#[cfg(test)]
mod tests {
    use super::*;

    /// The public normalization contract: objects pass through untouched,
    /// null means "no arguments", and every other non-object payload is
    /// wrapped verbatim so the wire never sees a bare value.
    #[test]
    fn normalize_tool_input_coerces_every_shape_to_an_object() {
        // Objects pass through untouched.
        let obj = serde_json::json!({"path": "a.txt", "nested": {"k": [1]}});
        assert_eq!(normalize_tool_input(obj.clone()), obj);
        // null means "no arguments", never a wrapped null.
        assert_eq!(
            normalize_tool_input(serde_json::Value::Null),
            serde_json::json!({})
        );
        // Any other non-object payload is wrapped verbatim.
        assert_eq!(
            normalize_tool_input(serde_json::json!("bare string")),
            serde_json::json!({"_raw": "bare string"})
        );
        assert_eq!(
            normalize_tool_input(serde_json::json!([1, 2])),
            serde_json::json!({"_raw": [1, 2]})
        );
        assert_eq!(
            normalize_tool_input(serde_json::json!(42)),
            serde_json::json!({"_raw": 42})
        );
        assert_eq!(
            normalize_tool_input(serde_json::json!(true)),
            serde_json::json!({"_raw": true})
        );
    }

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
