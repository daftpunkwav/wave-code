/*!
 * @file OpenAiClient
 * @description OpenAI-compatible streaming chat client and model capability table.
 *
 * Responsibilities:
 * - Translate unified messages and tool specs into Chat Completions wire format.
 * - Stream Chat Completions SSE into unified stream events.
 * - Expose approximate per-model context and output limits.
 *
 * This module must not depend on: runtime, config, or UI-layer components.
 */

//! OpenAI-compatible chat client (`POST {base_url}/chat/completions` with `stream: true`).
//!
//! Covers DeepSeek, Kimi, Ollama, and any other Chat Completions endpoint:
//! tool calls arrive as `delta.tool_calls[]` fragments assembled by index,
//! `data: [DONE]` ends the stream, and usage flows via `stream_options.include_usage`.
//!
//! Timeout policy mirrors [`crate::AnthropicClient`]: `connect_timeout` guards
//! the connect phase only, never the long-lived SSE stream; read-stall detection
//! follows the same deferred path as the Anthropic client in this tree.
//!
//! [`ModelCapabilities::for_model`] holds approximate per-model limits; unknown
//! names return `None` so callers fall back to the session defaults derived
//! from `ProviderConfig` (config values always win over this table).

use futures::{Stream, StreamExt};
use serde::Deserialize;
use std::time::Duration;

use crate::sse;
use crate::{
    ChatModel, ChatRequest, ContentBlock, EventStream, LlmError, Message, Result, Role,
    StreamEvent, ToolSpec, Usage, validate_image,
};

/// Streaming client for OpenAI-compatible Chat Completions endpoints.
pub struct OpenAIClient {
    base_url: String,
    api_key: String,
    model: String,
    http: reqwest::Client,
    /// Best-effort reasoning effort sent as `reasoning_effort`; `None`
    /// omits the param so endpoints without it keep working unchanged.
    /// Behind a lock so `/effort`-style switches work through the shared
    /// `Arc<dyn ChatModel>` handle (mirrors `ModelAdapter::model_name`).
    reasoning_effort: std::sync::RwLock<Option<String>>,
}

impl OpenAIClient {
    /// Creates a new client fallibly, returning an error if HTTP client or TLS init fails.
    pub fn try_new(base_url: String, api_key: String, model: String) -> Result<Self> {
        Ok(Self {
            base_url,
            api_key,
            model,
            http: build_http_client()?,
            reasoning_effort: std::sync::RwLock::new(None),
        })
    }

    /// Creates a new client; `model` is sent as the wire model on every request.
    pub fn new(base_url: String, api_key: String, model: String) -> Self {
        Self::try_new(base_url, api_key, model)
            .expect("TLS backend initialization failed; verify system certificates are installed")
    }

    /// Set the best-effort reasoning effort (e.g. "low"); builder style so
    /// existing `new(...)` call sites keep compiling unchanged.
    pub fn with_reasoning_effort(self, effort: impl Into<String>) -> Self {
        *self
            .reasoning_effort
            .write()
            .unwrap_or_else(|e| e.into_inner()) = Some(effort.into());
        self
    }

    /// The configured reasoning effort, if any.
    pub fn reasoning_effort(&self) -> Option<String> {
        self.reasoning_effort
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

/// Builds the HTTP client.
///
/// - Only `connect_timeout` is set (10s): it guards the connect phase; never set
///   `Client::timeout` - that would cut off healthy long-lived SSE streams.
/// - Redirects are disabled: the endpoint has no legitimate redirect semantics,
///   and reqwest would otherwise carry the `Authorization` bearer token to a
///   cross-origin target.
fn build_http_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| LlmError::ClientInit(e.to_string()))
}

#[async_trait::async_trait]
impl ChatModel for OpenAIClient {
    async fn stream(&self, req: ChatRequest) -> Result<EventStream> {
        let url = chat_completions_url(&self.base_url);
        // The request's model name wins (so callers can switch models
        // mid-session); the constructor's name is the fallback for empty
        // requests. Mirrors the Anthropic client, where `req.model` is
        // authoritative.
        let model = if req.model.is_empty() {
            &self.model
        } else {
            &req.model
        };
        // Copy the effort out under the lock: the guard must not live
        // across the await below.
        let reasoning_effort = self
            .reasoning_effort
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let response = self
            .http
            .post(url)
            .bearer_auth(&self.api_key)
            .header("content-type", "application/json")
            .json(&build_request_body(
                &req,
                model,
                reasoning_effort.as_deref(),
            ))
            .send()
            .await
            .map_err(|e| LlmError::Http(e.to_string()))?;

        let status = response.status();
        if !status.is_success() {
            // Read before the body: Retry-After rides the 429 headers.
            let retry_after = crate::parse_retry_after(response.headers());
            let body = response
                .text()
                .await
                .map_err(|e| LlmError::Http(e.to_string()))?;
            return Err(api_error_with_retry_after(
                format!("http_{}", status.as_u16()),
                truncate_error_body(&body),
                retry_after,
            ));
        }

        let byte_stream = response
            .bytes_stream()
            .map(|r| r.map_err(|e| LlmError::Http(e.to_string())));
        Ok(Box::pin(decode_openai_stream(sse::stall_guard(
            byte_stream,
            sse::STREAM_IDLE_TIMEOUT,
        ))))
    }

    fn set_thinking(&self, effort: &str) -> bool {
        let level = effort.trim();
        if level.is_empty() {
            return false;
        }
        *self
            .reasoning_effort
            .write()
            .unwrap_or_else(|e| e.into_inner()) = if level.eq_ignore_ascii_case("off") {
            None
        } else {
            Some(level.to_string())
        };
        true
    }
}

/// Builds the Chat Completions URL: strips trailing `/` from base_url to avoid double slashes.
fn chat_completions_url(base_url: &str) -> String {
    format!("{}/chat/completions", base_url.trim_end_matches('/'))
}

/// Max retained chars of an error response body (by char, so multibyte chars are never split).
const MAX_ERROR_BODY_CHARS: usize = 2000;

/// Truncates an error response body; the API key only travels in request headers
/// and is never written into error text.
fn truncate_error_body(body: &str) -> String {
    body.chars().take(MAX_ERROR_BODY_CHARS).collect()
}

/// Single construction point for API errors (non-2xx responses and in-stream
/// `error` payloads): kind/message are preserved verbatim so upper layers can
/// match known shapes (e.g. the reactive-compact trigger in `core::session`).
fn api_error(kind: String, message: String) -> LlmError {
    LlmError::Api { kind, message }
}

/// Non-2xx classification with the `Retry-After` hint (429 →
/// [`LlmError::RateLimited`]); in-stream payloads have no headers and
/// keep using [`api_error`].
fn api_error_with_retry_after(
    kind: String,
    message: String,
    retry_after: Option<std::time::Duration>,
) -> LlmError {
    crate::classify_api_error_with_retry_after(kind, message, retry_after)
}

// ---- Request translation (pure functions) ----

/// Builds the Chat Completions request body (`stream` is always true;
/// `stream_options.include_usage` asks the server to report token usage).
/// `reasoning_effort` is best-effort: `Some` adds the top-level
/// `reasoning_effort` param, `None` omits it entirely.
pub(crate) fn build_request_body(
    req: &ChatRequest,
    model: &str,
    reasoning_effort: Option<&str>,
) -> serde_json::Value {
    let mut body = serde_json::json!({
        "model": model,
        "messages": translate_messages(&req.system, &req.messages),
        "tools": req.tools.iter().map(translate_tool).collect::<Vec<_>>(),
        "max_tokens": req.max_tokens,
        "stream": true,
        "stream_options": {"include_usage": true},
    });
    if let Some(effort) = reasoning_effort {
        body["reasoning_effort"] = serde_json::Value::String(effort.to_string());
    }
    body
}

/// Translates one tool definition into `{type: function, function: {...}}` wire shape.
pub(crate) fn translate_tool(spec: &ToolSpec) -> serde_json::Value {
    serde_json::json!({
        "type": "function",
        "function": {
            "name": spec.name,
            "description": spec.description,
            "parameters": spec.input_schema,
        },
    })
}

/// Translates the system prompt plus unified messages into Chat Completions messages.
/// An empty system prompt is skipped (some endpoints reject empty system content).
pub(crate) fn translate_messages(system: &str, messages: &[Message]) -> Vec<serde_json::Value> {
    let mut out = Vec::with_capacity(messages.len() + 1);
    if !system.is_empty() {
        out.push(serde_json::json!({"role": "system", "content": system}));
    }
    for message in messages {
        out.extend(translate_message(message));
    }
    out
}

/// Translates one unified message; a message mixing text and tool results expands
/// into several wire messages (one `user`/`assistant` plus one `tool` per result).
fn translate_message(message: &Message) -> Vec<serde_json::Value> {
    match message.role {
        Role::User => translate_user(&message.content),
        Role::Assistant => translate_assistant(&message.content),
    }
}

/// Renders a tool result as plain text; refusal/error results keep their content
/// with an `[error]` prefix so failures stay visible instead of being dropped.
fn tool_result_text(content: &str, is_error: bool) -> String {
    if is_error {
        format!("[error] {content}")
    } else {
        content.to_string()
    }
}

/// Render one validated image as an OpenAI `image_url` data-URL part.
/// Invalid images (wrong mime, bad base64, over the 5MB cap) become a visible
/// text part naming the constraint, never a silent drop.
fn image_part(mime: &str, base64_data: &str) -> serde_json::Value {
    match validate_image(mime, base64_data) {
        Ok(_) => serde_json::json!({
            "type": "image_url",
            "image_url": {"url": format!("data:{mime};base64,{base64_data}")},
        }),
        Err(reason) => {
            serde_json::json!({"type": "text", "text": format!("[invalid image: {reason}]")})
        }
    }
}

fn translate_user(blocks: &[ContentBlock]) -> Vec<serde_json::Value> {
    let mut texts: Vec<String> = Vec::new();
    let mut images: Vec<serde_json::Value> = Vec::new();
    let mut results: Vec<(&str, &str, bool)> = Vec::new();
    for block in blocks {
        match block {
            ContentBlock::Text { text } => texts.push(text.clone()),
            ContentBlock::Image { mime, base64, .. } => images.push(image_part(mime, base64)),
            ContentBlock::ToolUse { id, name, input } => {
                // A tool call inside a user message has no wire meaning; render it
                // as text so the intent is preserved instead of dropped.
                texts.push(format!("[tool call {name} ({id}): {input}]"));
            }
            ContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error,
            } => results.push((tool_use_id, content, *is_error)),
        }
    }
    let mut out = Vec::new();
    let text = texts.join("\n");
    if !images.is_empty() {
        // Mixed text+image content uses the array shape; text-only keeps the
        // legacy string shape for backwards compatibility.
        let mut parts: Vec<serde_json::Value> = Vec::new();
        if !text.is_empty() {
            parts.push(serde_json::json!({"type": "text", "text": text}));
        }
        parts.extend(images);
        out.push(serde_json::json!({"role": "user", "content": parts}));
    } else if !text.is_empty() || results.is_empty() {
        out.push(serde_json::json!({"role": "user", "content": text}));
    }
    for (tool_use_id, content, is_error) in results {
        out.push(serde_json::json!({
            "role": "tool",
            "tool_call_id": tool_use_id,
            "content": tool_result_text(content, is_error),
        }));
    }
    out
}

fn translate_assistant(blocks: &[ContentBlock]) -> Vec<serde_json::Value> {
    let mut texts: Vec<String> = Vec::new();
    let mut calls = Vec::new();
    for block in blocks {
        match block {
            ContentBlock::Text { text } => texts.push(text.clone()),
            ContentBlock::ToolUse { id, name, input } => {
                calls.push(serde_json::json!({
                    "id": id,
                    "type": "function",
                    "function": {
                        "name": name,
                        // Compact JSON string, never dropped even when empty or null.
                        // Normalized so gateways validating arguments as an object
                        // never see a legacy non-object payload either.
                        "arguments": crate::normalize_tool_input(input.clone()).to_string(),
                    },
                }));
            }
            ContentBlock::Image { mime, .. } => {
                // Defensive: assistants cannot emit images on the wire;
                // keep a visible placeholder so the intent is not dropped.
                texts.push(format!("[image {mime}]"));
            }
            ContentBlock::ToolResult {
                content, is_error, ..
            } => {
                // Defensive: a result inside an assistant message has no wire slot;
                // fold it into the text so the failure stays visible.
                texts.push(tool_result_text(content, *is_error));
            }
        }
    }
    let text = texts.join("\n");
    if calls.is_empty() {
        if text.is_empty() {
            // Providers reject empty assistant messages; drop instead of sending one.
            return Vec::new();
        }
        return vec![serde_json::json!({"role": "assistant", "content": text})];
    }
    vec![serde_json::json!({
        "role": "assistant",
        // Null content with tool calls is the canonical OpenAI shape.
        "content": if text.is_empty() { serde_json::Value::Null } else { serde_json::Value::String(text) },
        "tool_calls": calls,
    })]
}

// ---- Streaming decode ----

/// Byte-chunk stream to event stream: buffers bytes, splits SSE frames on
/// blank lines, and feeds each `data` payload to the Chat Completions
/// assembler.
///
/// Thin adapter over the shared [`sse::decode_sse_frames`] loop: framing,
/// buffering, and stall detection live there so both providers share one
/// memory-safety-critical implementation. Read-stall detection therefore
/// truly follows the same deferred path as [`crate::AnthropicClient`].
fn decode_openai_stream<S>(byte_stream: S) -> impl Stream<Item = Result<StreamEvent>> + Send
where
    S: Stream<Item = Result<bytes::Bytes>> + Send,
{
    let mut state = OpenAiStreamState::default();
    sse::decode_sse_frames(byte_stream, sse::MAX_SSE_BUF, move |data| {
        feed_openai_data(&mut state, data)
    })
}

/// Feeds one SSE `data` payload, returning zero or more stream events.
/// `data: [DONE]` finishes the turn: one [`StreamEvent::BlockEnd`] per begun
/// tool call, then [`StreamEvent::MessageComplete`].
fn feed_openai_data(state: &mut OpenAiStreamState, data: &str) -> Result<Vec<StreamEvent>> {
    if data.trim() == "[DONE]" {
        return Ok(finish_turn(state));
    }
    let value: serde_json::Value = serde_json::from_str(data)?;
    if let Some(error) = value.get("error") {
        return Err(openai_payload_error(error, data));
    }
    // Wrong-shape payloads (e.g. `"choices": 42`) fail here with an explicit JSON error.
    let chunk: OpenAiChunk = serde_json::from_value(value)?;
    let mut events = Vec::new();
    for choice in &chunk.choices {
        if let Some(content) = choice.delta.content.as_ref()
            && !content.is_empty()
        {
            events.push(StreamEvent::TextDelta {
                text: content.clone(),
            });
        } else if let Some(reasoning) = choice.delta.reasoning_content.as_ref()
            && !reasoning.is_empty()
        {
            events.push(StreamEvent::TextDelta {
                text: reasoning.clone(),
            });
        }
        for call in &choice.delta.tool_calls {
            events.extend(apply_tool_fragment(state, call));
        }
        if choice.finish_reason.is_some() && state.stop_reason.is_none() {
            state.stop_reason = choice.finish_reason.clone();
        }
    }
    if let Some(usage) = chunk.usage {
        state.prompt_tokens = usage.prompt_tokens;
        state.completion_tokens = usage.completion_tokens;
        state.cached_tokens = usage
            .prompt_tokens_details
            .map(|d| d.cached_tokens)
            .unwrap_or(0);
    }
    Ok(events)
}

/// Maps an in-stream `error` payload onto [`LlmError::Api`], preserving the
/// provider kind/message so upper layers keep matching known shapes.
fn openai_payload_error(error: &serde_json::Value, raw: &str) -> LlmError {
    let kind = error
        .get("type")
        .and_then(|v| v.as_str())
        .or_else(|| error.get("code").and_then(|v| v.as_str()))
        .unwrap_or("openai_error")
        .to_string();
    let message = error
        .get("message")
        .and_then(|v| v.as_str())
        .unwrap_or(raw)
        .to_string();
    api_error(kind, message)
}

/// Applies one `tool_calls[]` delta fragment: grows the slot table by index,
/// emits [`StreamEvent::ToolUseBegin`] once both id and name are known, then
/// forwards non-empty argument fragments as input deltas.
fn apply_tool_fragment(
    state: &mut OpenAiStreamState,
    call: &OpenAiToolCallDelta,
) -> Vec<StreamEvent> {
    while state.slots.len() <= call.index {
        state.slots.push(ToolSlot::default());
    }
    let slot = &mut state.slots[call.index];
    if let Some(id) = call.id.as_ref()
        && !id.is_empty()
    {
        slot.id = id.clone();
    }
    if let Some(name) = call.function.as_ref().and_then(|f| f.name.as_ref())
        && !name.is_empty()
    {
        slot.name = name.clone();
    }
    let mut events = Vec::new();
    if !slot.begun && !slot.id.is_empty() && !slot.name.is_empty() {
        slot.begun = true;
        events.push(StreamEvent::ToolUseBegin {
            id: slot.id.clone(),
            name: slot.name.clone(),
        });
    }
    if let Some(args) = call.function.as_ref().and_then(|f| f.arguments.as_ref())
        && !args.is_empty()
    {
        events.push(StreamEvent::ToolUseInputDelta {
            partial_json: args.clone(),
        });
    }
    events
}

/// Finishes the turn on `[DONE]`: closes every begun tool call, then completes
/// the message. `length` maps to `max_tokens` (the continuation trigger the
/// upper layers match on); other reasons pass through verbatim.
fn finish_turn(state: &OpenAiStreamState) -> Vec<StreamEvent> {
    let mut events = Vec::new();
    for _ in state.slots.iter().filter(|slot| slot.begun) {
        events.push(StreamEvent::BlockEnd);
    }
    let stop_reason = match state.stop_reason.as_deref() {
        Some("length") => "max_tokens".to_string(),
        Some(reason) => reason.to_string(),
        None => "stop".to_string(),
    };
    events.push(StreamEvent::MessageComplete {
        stop_reason,
        usage: Usage {
            input_tokens: state.prompt_tokens,
            output_tokens: state.completion_tokens,
            cache_read_tokens: state.cached_tokens,
            ..Usage::default()
        },
    });
    events
}

/// One tool call under assembly from per-index delta fragments.
#[derive(Default)]
struct ToolSlot {
    id: String,
    name: String,
    begun: bool,
}

/// Mutable decode state across SSE data payloads of one stream.
#[derive(Default)]
struct OpenAiStreamState {
    slots: Vec<ToolSlot>,
    stop_reason: Option<String>,
    prompt_tokens: u64,
    completion_tokens: u64,
    cached_tokens: u64,
}

// ---- Private deserialization structs for one Chat Completions chunk ----

#[derive(Deserialize, Default)]
struct OpenAiChunk {
    #[serde(default)]
    choices: Vec<OpenAiChoice>,
    #[serde(default)]
    usage: Option<OpenAiUsage>,
}

#[derive(Deserialize, Default)]
struct OpenAiChoice {
    #[serde(default)]
    delta: OpenAiDelta,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Deserialize, Default)]
struct OpenAiDelta {
    #[serde(default)]
    content: Option<String>,
    /// Reasoning-first models (DeepSeek reasoner) stream thinking here
    /// instead of `content`; surfaced as text only when `content` is
    /// absent so chat models never duplicate output.
    #[serde(default)]
    reasoning_content: Option<String>,
    #[serde(default)]
    tool_calls: Vec<OpenAiToolCallDelta>,
}

#[derive(Deserialize, Default)]
struct OpenAiToolCallDelta {
    #[serde(default)]
    index: usize,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    function: Option<OpenAiFunctionDelta>,
}

#[derive(Deserialize, Default)]
struct OpenAiFunctionDelta {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
}

#[derive(Deserialize, Default)]
struct OpenAiUsage {
    #[serde(default)]
    prompt_tokens: u64,
    #[serde(default)]
    completion_tokens: u64,
    /// Cache accounting (`prompt_tokens_details.cached_tokens`); absent on
    /// providers without the field, degrading to 0.
    #[serde(default)]
    prompt_tokens_details: Option<OpenAiPromptTokensDetails>,
}

#[derive(Deserialize, Default)]
struct OpenAiPromptTokensDetails {
    #[serde(default)]
    cached_tokens: u64,
}

/// Approximate per-model limits (context window and max output tokens).
///
/// Values are approximations for session sizing only; an explicit
/// `ProviderConfig.context_window` / `max_output_tokens` always overrides them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModelCapabilities {
    /// Approximate context window in tokens.
    pub context_window: u64,
    /// Approximate max output tokens per round.
    pub max_output_tokens: u32,
}

impl ModelCapabilities {
    /// Looks up approximate limits by wire model name (case-insensitive and
    /// prefix-tolerant, so dated variants like `kimi-k2-0905` or
    /// `gpt-4o-2024-11-20` match). Returns `None` for unknown names so the
    /// caller falls back to the session defaults already in use.
    pub fn for_model(name: &str) -> Option<Self> {
        let name = name.trim().to_lowercase();
        if name.starts_with("deepseek-chat") || name.starts_with("deepseek-reasoner") {
            Some(Self {
                context_window: 128_000,
                max_output_tokens: 8_192,
            })
        } else if name.starts_with("gpt-4o-mini") || name.starts_with("gpt-4o") {
            Some(Self {
                context_window: 128_000,
                max_output_tokens: 16_384,
            })
        } else if name.starts_with("kimi-k2") {
            Some(Self {
                context_window: 256_000,
                max_output_tokens: 16_384,
            })
        } else if name.starts_with("qwen3-coder") {
            Some(Self {
                context_window: 256_000,
                max_output_tokens: 32_768,
            })
        } else if name.starts_with("glm-4.6") {
            Some(Self {
                context_window: 200_000,
                max_output_tokens: 8_192,
            })
        } else if name.contains("ollama") || name.starts_with("local-") {
            Some(Self::local_default())
        } else {
            None
        }
    }

    /// Resolves limits with explicit caller-supplied fallbacks for unknown names
    /// (typically the session defaults derived from `ProviderConfig`).
    pub fn resolve_or(name: &str, context_window: u64, max_output_tokens: u32) -> Self {
        Self::for_model(name).unwrap_or(Self {
            context_window,
            max_output_tokens,
        })
    }

    /// Default limits for local endpoints (Ollama and similar small-context servers).
    pub fn local_default() -> Self {
        Self {
            context_window: 32_768,
            max_output_tokens: 8_192,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Role, ToolSpec};

    fn tool_spec() -> ToolSpec {
        ToolSpec {
            name: "read_file".to_string(),
            description: "Read a file".to_string(),
            input_schema: serde_json::json!({"type": "object"}),
        }
    }

    #[test]
    fn tool_spec_maps_to_function_shape() {
        let wire = translate_tool(&tool_spec());
        assert_eq!(wire["type"], "function");
        assert_eq!(wire["function"]["name"], "read_file");
        assert_eq!(wire["function"]["description"], "Read a file");
        assert_eq!(wire["function"]["parameters"]["type"], "object");
    }

    #[test]
    fn assistant_text_and_tool_use_map_to_one_message() {
        let messages = translate_messages(
            "sys",
            &[Message {
                role: Role::Assistant,
                content: vec![
                    ContentBlock::Text {
                        text: "reading".to_string(),
                    },
                    ContentBlock::ToolUse {
                        id: "call_1".to_string(),
                        name: "read_file".to_string(),
                        input: serde_json::json!({"path": "a.txt"}),
                    },
                ],
            }],
        );
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0]["role"], "system");
        let assistant = &messages[1];
        assert_eq!(assistant["role"], "assistant");
        assert_eq!(assistant["content"], "reading");
        assert_eq!(assistant["tool_calls"][0]["id"], "call_1");
        assert_eq!(assistant["tool_calls"][0]["type"], "function");
        assert_eq!(assistant["tool_calls"][0]["function"]["name"], "read_file");
        assert_eq!(
            assistant["tool_calls"][0]["function"]["arguments"],
            serde_json::json!({"path": "a.txt"}).to_string()
        );
    }

    /// A non-object tool input (legacy session or pre-normalization value)
    /// is wrapped before the arguments string is built so gateways that
    /// validate arguments as an object never see a bare payload.
    #[test]
    fn non_object_tool_input_is_wrapped_in_arguments() {
        let messages = translate_messages(
            "",
            &[Message {
                role: Role::Assistant,
                content: vec![ContentBlock::ToolUse {
                    id: "call_1".to_string(),
                    name: "write".to_string(),
                    input: serde_json::json!("just a string"),
                }],
            }],
        );
        assert_eq!(
            messages[0]["tool_calls"][0]["function"]["arguments"],
            serde_json::json!({"_raw": "just a string"}).to_string()
        );
    }

    #[test]
    fn assistant_tool_use_without_text_uses_null_content() {
        let messages = translate_messages(
            "",
            &[Message {
                role: Role::Assistant,
                content: vec![ContentBlock::ToolUse {
                    id: "call_1".to_string(),
                    name: "shell".to_string(),
                    input: serde_json::json!({"command": "ls"}),
                }],
            }],
        );
        // Empty system is skipped, so only the assistant message remains.
        assert_eq!(messages.len(), 1);
        assert!(messages[0]["content"].is_null());
        assert_eq!(messages[0]["tool_calls"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn user_text_and_tool_results_split_into_wire_messages() {
        let messages = translate_messages(
            "",
            &[Message {
                role: Role::User,
                content: vec![
                    ContentBlock::Text {
                        text: "done".to_string(),
                    },
                    ContentBlock::ToolResult {
                        tool_use_id: "call_1".to_string(),
                        content: "ok".to_string(),
                        is_error: false,
                    },
                    ContentBlock::ToolResult {
                        tool_use_id: "call_2".to_string(),
                        content: "nope".to_string(),
                        is_error: true,
                    },
                ],
            }],
        );
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[0]["role"], "user");
        assert_eq!(messages[0]["content"], "done");
        assert_eq!(messages[1]["role"], "tool");
        assert_eq!(messages[1]["tool_call_id"], "call_1");
        assert_eq!(messages[1]["content"], "ok");
        // Error results keep their content with a visible prefix, never dropped.
        assert_eq!(messages[2]["role"], "tool");
        assert_eq!(messages[2]["content"], "[error] nope");
    }

    #[test]
    fn empty_system_prompt_is_skipped() {
        let messages = translate_messages(
            "",
            &[Message {
                role: Role::User,
                content: vec![ContentBlock::Text {
                    text: "hi".to_string(),
                }],
            }],
        );
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0]["role"], "user");
    }

    #[test]
    fn request_body_carries_stream_and_model() {
        let req = ChatRequest {
            model: "deepseek-chat".to_string(),
            system: "sys".to_string(),
            messages: std::sync::Arc::new(vec![Message {
                role: Role::User,
                content: vec![ContentBlock::Text {
                    text: "hi".to_string(),
                }],
            }]),
            tools: vec![tool_spec()],
            max_tokens: 100,
        };
        let body = build_request_body(&req, "deepseek-chat", None);
        assert_eq!(body["model"], "deepseek-chat");
        assert_eq!(body["stream"], true);
        assert_eq!(body["stream_options"]["include_usage"], true);
        assert_eq!(body["max_tokens"], 100);
        assert_eq!(body["tools"][0]["function"]["name"], "read_file");
        assert_eq!(body["messages"][0]["role"], "system");
        // No effort configured means no param at all (best-effort omit).
        assert!(body.get("reasoning_effort").is_none());
    }

    #[test]
    fn request_body_forwards_reasoning_effort_when_set() {
        let req = ChatRequest {
            model: "deepseek-chat".to_string(),
            system: String::new(),
            messages: std::sync::Arc::new(Vec::new()),
            tools: Vec::new(),
            max_tokens: 100,
        };
        let body = build_request_body(&req, "deepseek-chat", Some("low"));
        assert_eq!(body["reasoning_effort"], "low");
        // The builder setter is the only way the client carries effort,
        // and `new(...)` alone keeps the omit behavior.
        let plain = OpenAIClient::new(
            "https://example.test".to_string(),
            "key".to_string(),
            "deepseek-chat".to_string(),
        );
        assert_eq!(plain.reasoning_effort(), None);
        let tuned = OpenAIClient::new(
            "https://example.test".to_string(),
            "key".to_string(),
            "deepseek-chat".to_string(),
        )
        .with_reasoning_effort("low");
        assert_eq!(tuned.reasoning_effort().as_deref(), Some("low"));

        let ok_client = OpenAIClient::try_new(
            "https://example.test".to_string(),
            "key".to_string(),
            "deepseek-chat".to_string(),
        );
        assert!(ok_client.is_ok());
    }

    /// Runtime effort switches go through the shared `&self` seam: `off`
    /// clears the param, other levels set it, empty rejects.
    #[test]
    fn set_thinking_switches_effort_through_shared_ref() {
        let plain = OpenAIClient::new(
            "https://example.test".to_string(),
            "key".to_string(),
            "deepseek-chat".to_string(),
        );
        assert!(!ChatModel::set_thinking(&plain, ""));
        assert!(ChatModel::set_thinking(&plain, "high"));
        assert_eq!(plain.reasoning_effort().as_deref(), Some("high"));
        assert!(ChatModel::set_thinking(&plain, "off"));
        assert_eq!(plain.reasoning_effort(), None);
    }

    /// Drives the byte-level decoder over canned chunks (no network).
    async fn run_decode(chunks: Vec<&'static [u8]>) -> Vec<Result<StreamEvent>> {
        use futures::StreamExt;
        let byte_stream = futures::stream::iter(
            chunks
                .into_iter()
                .map(|c| Ok::<_, LlmError>(bytes::Bytes::from_static(c))),
        );
        decode_openai_stream(byte_stream).collect().await
    }

    #[tokio::test]
    async fn assembles_tool_arguments_split_across_chunks() {
        let results = run_decode(vec![
            b"data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"function\":{\"name\":\"shell\",\"arguments\":\"{\\\"comma\"}}]}}]}\n\n",
            b"data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"nd\\\":\\\"ls\\\"}\"}}]}}]}\n\n",
            b"data: [DONE]\n\n",
        ])
        .await;
        let events: Vec<StreamEvent> = results.into_iter().collect::<Result<Vec<_>>>().unwrap();
        assert_eq!(
            events,
            vec![
                StreamEvent::ToolUseBegin {
                    id: "call_1".to_string(),
                    name: "shell".to_string(),
                },
                StreamEvent::ToolUseInputDelta {
                    partial_json: "{\"comma".to_string(),
                },
                StreamEvent::ToolUseInputDelta {
                    partial_json: "nd\":\"ls\"}".to_string(),
                },
                StreamEvent::BlockEnd,
                StreamEvent::MessageComplete {
                    stop_reason: "stop".to_string(),
                    usage: Usage::default(),
                },
            ]
        );
    }

    #[tokio::test]
    async fn assembles_multiple_parallel_tool_calls() {
        let results = run_decode(vec![
            b"data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"c0\",\"function\":{\"name\":\"a\",\"arguments\":\"{}\"}},{\"index\":1,\"id\":\"c1\",\"function\":{\"name\":\"b\",\"arguments\":\"{}\"}}]}}]}\n\n",
            b"data: [DONE]\n\n",
        ])
        .await;
        let events: Vec<StreamEvent> = results.into_iter().collect::<Result<Vec<_>>>().unwrap();
        assert_eq!(
            events,
            vec![
                StreamEvent::ToolUseBegin {
                    id: "c0".to_string(),
                    name: "a".to_string(),
                },
                StreamEvent::ToolUseInputDelta {
                    partial_json: "{}".to_string(),
                },
                StreamEvent::ToolUseBegin {
                    id: "c1".to_string(),
                    name: "b".to_string(),
                },
                StreamEvent::ToolUseInputDelta {
                    partial_json: "{}".to_string(),
                },
                StreamEvent::BlockEnd,
                StreamEvent::BlockEnd,
                StreamEvent::MessageComplete {
                    stop_reason: "stop".to_string(),
                    usage: Usage::default(),
                },
            ]
        );
    }

    #[tokio::test]
    async fn reasoning_content_surfaces_when_content_absent() {
        let results = run_decode(vec![
            b"data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"thinking\"}}]}\n\n",
            b"data: {\"choices\":[{\"delta\":{\"content\":\"answer\",\"reasoning_content\":\"ignored\"}}]}\n\n",
            b"data: [DONE]\n\n",
        ])
        .await;
        let events: Vec<StreamEvent> = results.into_iter().collect::<Result<Vec<_>>>().unwrap();
        let texts: Vec<&str> = events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::TextDelta { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        // Reasoning-only chunk surfaces; content wins when both present.
        assert_eq!(texts, vec!["thinking", "answer"]);
    }

    #[tokio::test]
    async fn empty_delta_chunks_produce_no_events() {
        let results = run_decode(vec![
            b"data: {\"choices\":[{\"delta\":{}}]}\n\n",
            b"data: {\"choices\":[{\"delta\":{\"content\":\"\"}}]}\n\n",
            b"data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n",
            b"data: [DONE]\n\n",
        ])
        .await;
        let events: Vec<StreamEvent> = results.into_iter().collect::<Result<Vec<_>>>().unwrap();
        assert_eq!(
            events,
            vec![
                StreamEvent::TextDelta {
                    text: "hi".to_string(),
                },
                StreamEvent::MessageComplete {
                    stop_reason: "stop".to_string(),
                    usage: Usage::default(),
                },
            ]
        );
    }

    #[tokio::test]
    async fn error_mid_stream_aborts_with_api_error() {
        let results = run_decode(vec![
            b"data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n",
            b"data: {\"error\":{\"message\":\"overloaded\",\"type\":\"server_error\"}}\n\n",
            b"data: [DONE]\n\n",
        ])
        .await;
        assert_eq!(results.len(), 2);
        assert!(matches!(&results[0], Ok(StreamEvent::TextDelta { .. })));
        assert!(
            matches!(&results[1], Err(LlmError::Api { .. })),
            "mid-stream error must surface and terminate: {:?}",
            results[1]
        );
    }

    #[tokio::test]
    async fn usage_and_finish_reason_reach_message_complete() {
        let results = run_decode(vec![
            b"data: {\"choices\":[{\"delta\":{\"content\":\"hi\"},\"finish_reason\":\"length\"}],\"usage\":{\"prompt_tokens\":5,\"completion_tokens\":7}}\n\n",
            b"data: [DONE]\n\n",
        ])
        .await;
        let events: Vec<StreamEvent> = results.into_iter().collect::<Result<Vec<_>>>().unwrap();
        assert_eq!(
            events.last(),
            Some(&StreamEvent::MessageComplete {
                // `length` maps to the continuation trigger the upper layers match on.
                stop_reason: "max_tokens".to_string(),
                usage: Usage {
                    input_tokens: 5,
                    output_tokens: 7,
                    ..Usage::default()
                },
            })
        );
    }

    #[tokio::test]
    async fn malformed_frames_are_explicit_errors() {
        let results = run_decode(vec![b"data: {not json}\n\n"]).await;
        assert_eq!(results.len(), 1);
        assert!(
            matches!(&results[0], Err(LlmError::Json(_))),
            "bad JSON must be an explicit error: {:?}",
            results[0]
        );
        let results = run_decode(vec![b"data: {\"choices\":42}\n\n"]).await;
        assert!(
            matches!(&results[0], Err(LlmError::Json(_))),
            "wrong-shape payload must be an explicit error: {:?}",
            results[0]
        );
    }

    #[tokio::test]
    async fn stream_errors_and_terminates_when_buffer_exceeds_cap() {
        use futures::StreamExt;
        let chunk = || Ok::<_, LlmError>(bytes::Bytes::from_static(b"data: no-boundary-here\n"));
        let byte_stream = futures::stream::iter([chunk(), chunk(), chunk()]);
        let mut state = OpenAiStreamState::default();
        let results: Vec<_> = sse::decode_sse_frames(byte_stream, 32, move |data| {
            feed_openai_data(&mut state, data)
        })
        .collect()
        .await;
        assert_eq!(results.len(), 1);
        assert!(
            matches!(&results[0], Err(LlmError::Sse(msg)) if msg.contains("cap")),
            "should report the buffer-over-cap error: {:?}",
            results[0]
        );
    }

    /// The OpenAI stream path wires the shared read-stall guard: an upstream
    /// that stops producing bytes ends the stream with a Timeout error
    /// instead of hanging forever (parity with the Anthropic client).
    #[tokio::test]
    async fn openai_stream_ends_with_timeout_when_upstream_stalls() {
        use futures::StreamExt;
        let first = Ok::<_, LlmError>(bytes::Bytes::from_static(b"data: {\"choices\":[]}\n\n"));
        let hang: futures::stream::Pending<Result<bytes::Bytes>> = futures::stream::pending();
        let guarded = sse::stall_guard(
            futures::stream::iter(vec![first]).chain(hang),
            std::time::Duration::from_millis(30),
        );
        let results: Vec<_> = decode_openai_stream(guarded).collect().await;
        assert_eq!(results.len(), 1);
        assert!(
            matches!(&results[0], Err(LlmError::Timeout(msg)) if msg.contains("idle timeout")),
            "a stalled upstream must end the stream with Timeout: {:?}",
            results[0]
        );
    }

    #[tokio::test]
    async fn non_2xx_response_becomes_api_error() {
        use std::io::Write;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let mut buf = [0u8; 4096];
            use std::io::Read;
            let _ = s.read(&mut buf);
            let body = r#"{"error":{"message":"bad key","type":"auth_error"}}"#;
            let resp = format!(
                "HTTP/1.1 401 Unauthorized\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = s.write_all(resp.as_bytes());
        });
        let client =
            OpenAIClient::new(format!("http://127.0.0.1:{port}"), "bad".into(), "m".into());
        let req = ChatRequest {
            model: "m".into(),
            system: String::new(),
            messages: std::sync::Arc::new(vec![]),
            tools: vec![],
            max_tokens: 1,
        };
        let err = match client.stream(req).await {
            Ok(_) => panic!("401 must error"),
            Err(e) => e,
        };
        assert!(
            matches!(&err, LlmError::Api { kind, .. } if kind == "http_401"),
            "non-2xx must map to the crate Api error: {err:?}"
        );
    }

    #[test]
    fn capability_table_covers_known_models() {
        let chat = ModelCapabilities::for_model("deepseek-chat").unwrap();
        assert_eq!(chat.context_window, 128_000);
        let reasoner = ModelCapabilities::for_model("deepseek-reasoner").unwrap();
        assert_eq!(reasoner.context_window, 128_000);
        // Case-insensitive and prefix-tolerant (dated variants match).
        assert_eq!(ModelCapabilities::for_model("DeepSeek-Chat").unwrap(), chat);
        assert_eq!(
            ModelCapabilities::for_model("gpt-4o-2024-11-20")
                .unwrap()
                .context_window,
            128_000
        );
        assert_eq!(
            ModelCapabilities::for_model("gpt-4o-mini")
                .unwrap()
                .context_window,
            128_000
        );
        assert_eq!(
            ModelCapabilities::for_model("kimi-k2-0905")
                .unwrap()
                .context_window,
            256_000
        );
        assert_eq!(
            ModelCapabilities::for_model("qwen3-coder-plus")
                .unwrap()
                .context_window,
            256_000
        );
        assert_eq!(
            ModelCapabilities::for_model("glm-4.6")
                .unwrap()
                .context_window,
            200_000
        );
        assert_eq!(
            ModelCapabilities::for_model("ollama/llama3").unwrap(),
            ModelCapabilities::local_default()
        );
    }

    #[test]
    fn unknown_models_fall_back_to_session_defaults() {
        assert_eq!(ModelCapabilities::for_model("mystery-9000"), None);
        assert_eq!(ModelCapabilities::for_model(""), None);
        let resolved = ModelCapabilities::resolve_or("mystery-9000", 200_000, 8_192);
        assert_eq!(
            resolved,
            ModelCapabilities {
                context_window: 200_000,
                max_output_tokens: 8_192,
            }
        );
        // Known names ignore the fallbacks.
        let resolved = ModelCapabilities::resolve_or("kimi-k2", 200_000, 8_192);
        assert_eq!(resolved.context_window, 256_000);
    }

    #[test]
    fn context_length_markers_survive_verbatim_in_api_error() {
        // `core::session::is_prompt_too_long` matches on Api kind/message strings;
        // the OpenAI context shapes must pass through untouched for future wiring.
        let err = api_error(
            "http_400".into(),
            "This model's maximum context length is 128000 tokens".into(),
        );
        let LlmError::Api { kind, message } = err else {
            panic!("must stay the Api variant");
        };
        assert!(message.contains("maximum context length"));
        assert_eq!(kind, "http_400");
        let err = openai_payload_error(
            &serde_json::json!({"code": "context_length_exceeded", "message": "too long"}),
            "raw",
        );
        assert!(
            matches!(&err, LlmError::Api { kind, .. } if kind == "context_length_exceeded"),
            "provider error code must be preserved: {err:?}"
        );
    }
}

#[cfg(test)]
mod image_translation_tests {
    use super::*;
    use crate::{ContentBlock, Message, Role};

    fn tiny_png_base64() -> String {
        // 1x1 PNG (68 bytes decoded), well under the 5MB cap.
        use base64::Engine as _;
        let bytes: Vec<u8> = vec![
            0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D,
        ];
        base64::engine::general_purpose::STANDARD.encode(&bytes)
    }

    #[test]
    fn user_image_maps_to_image_url_data_url() {
        let b64 = tiny_png_base64();
        let messages = translate_messages(
            "",
            &[Message {
                role: Role::User,
                content: vec![
                    ContentBlock::Text {
                        text: "see this".to_string(),
                    },
                    ContentBlock::Image {
                        id: None,
                        mime: "image/png".to_string(),
                        base64: b64.clone(),
                    },
                ],
            }],
        );
        assert_eq!(messages.len(), 1);
        let parts = messages[0]["content"].as_array().unwrap();
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0]["type"], "text");
        assert_eq!(parts[1]["type"], "image_url");
        assert_eq!(
            parts[1]["image_url"]["url"],
            serde_json::Value::String(format!("data:image/png;base64,{b64}"))
        );
    }

    #[test]
    fn text_only_user_keeps_legacy_string_shape() {
        let messages = translate_messages(
            "",
            &[Message {
                role: Role::User,
                content: vec![ContentBlock::Text {
                    text: "hi".to_string(),
                }],
            }],
        );
        assert!(messages[0]["content"].is_string());
    }

    #[test]
    fn invalid_image_becomes_visible_text_naming_constraint() {
        let messages = translate_messages(
            "",
            &[Message {
                role: Role::User,
                content: vec![ContentBlock::Image {
                    id: None,
                    mime: "image/bmp".to_string(),
                    base64: "aaaa".to_string(),
                }],
            }],
        );
        let parts = messages[0]["content"].as_array().unwrap();
        assert_eq!(parts[0]["type"], "text");
        assert!(
            parts[0]["text"]
                .as_str()
                .unwrap()
                .contains("unsupported image mime")
        );
    }

    #[test]
    fn validate_image_rejects_bad_mime_and_names_cap() {
        assert!(crate::validate_image("image/bmp", "aaaa").is_err());
        assert!(crate::validate_image("image/png", "!!!not-base64!!!").is_err());
        let err = crate::validate_image("image/png", &tiny_png_base64()).unwrap();
        assert!(!err.is_empty());
    }
}
