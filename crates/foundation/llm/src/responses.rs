//! OpenAI Responses API (`POST {base_url}/responses`) streaming client.
//!
//! Third wire dialect alongside [`crate::anthropic`] (Anthropic Messages) and
//! [`crate::openai`] (Chat Completions). The Responses API is the one that
//! serves models with no Chat Completions endpoint (o1-pro, gpt-5-codex) and
//! the recommended path for the newer reasoning families, so it is a
//! first-class provider kind rather than a mode of the chat client.
//!
//! Shape differences from Chat Completions that this module bridges:
//! - the system prompt travels as a top-level `instructions` string, not a
//!   `system` message;
//! - history is a flat `input` item list: `input_text` / `input_image` parts
//!   for user turns, `output_text` for assistant prose, and **standalone**
//!   `function_call` / `function_call_output` items for tool exchange (tool
//!   pairing rides `call_id`, which is what wave stores as the call id);
//! - tool definitions are flat (`{type, name, description, parameters}`);
// nosemgrep: codacy.yaml.security.hard-coded-tokens
//! - the output cap is `max_output_tokens` and reasoning effort nests under
//!   `reasoning: {effort}` (with `summary: auto` so reasoning summaries
//!   stream for display);
//! - streaming uses named event types (`response.output_text.delta`, …)
//!   whose `output_index` identifies the item a delta belongs to.
//!
//! `store: false` is always sent: wave keeps conversations on the local
//! machine, and the Responses API otherwise persists them server-side.

use std::sync::RwLock;
use std::time::Duration;

use futures::StreamExt;
use serde_json::Value;

use crate::{
    ChatModel, ChatRequest, ContentBlock, EventStream, LlmError, Message, Result, Role,
    StreamEvent, ToolSpec, Usage,
};

/// Streaming client for the OpenAI Responses API.
pub struct ResponsesClient {
    base_url: String,
    api_key: String,
    http: reqwest::Client,
    /// Model name used when a request leaves `model` empty.
    model: String,
    /// Best-effort reasoning effort sent as `reasoning.effort`. A `RwLock`
    /// because the shared `&self` seam (`set_thinking`) switches it mid-session.
    reasoning_effort: RwLock<Option<String>>,
}

impl ResponsesClient {
    /// Creates a new client (streaming, no reasoning effort configured).
    ///
    /// # Panics
    /// Panics if the TLS backend fails to initialize; use [`Self::try_new`]
    /// where that must be a recoverable error.
    pub fn new(base_url: String, api_key: String, model: String) -> Self {
        Self::try_new(base_url, api_key, model)
            .expect("TLS backend initialization failed; verify system certificates are installed")
    }

    /// Fallible constructor (the TLS init failure is returned instead of
    /// panicking).
    pub fn try_new(base_url: String, api_key: String, model: String) -> Result<Self> {
        let http = reqwest::Client::builder()
            // Timeout policy mirrors the other clients: connect_timeout
            // guards the connect phase, so a blackholed endpoint fails in
            // 10s instead of waiting out the OS connect limit (the
            // establishment idle bound only starts on `.send()`). A
            // redirect would carry the Authorization header to another
            // host; fail the request instead.
            .connect_timeout(Duration::from_secs(10))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| LlmError::Http(e.to_string()))?;
        Ok(Self {
            base_url,
            api_key,
            http,
            model,
            reasoning_effort: RwLock::new(None),
        })
    }

    /// Set the best-effort reasoning effort (e.g. "low"); builder style.
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

#[async_trait::async_trait]
impl ChatModel for ResponsesClient {
    async fn stream(&self, req: ChatRequest) -> Result<EventStream> {
        let url = responses_url(&self.base_url);
        let model = if req.model.is_empty() {
            &self.model
        } else {
            &req.model
        };
        let reasoning_effort = self
            .reasoning_effort
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        // Bounded establishment: a server that accepts the connection but
        // never answers would otherwise hang `.send()` forever (see
        // [`crate::sse::send_with_idle_bound`]).
        let response = crate::sse::send_with_idle_bound(
            self.http
                .post(url)
                .bearer_auth(&self.api_key)
                .header("content-type", "application/json")
                .json(&build_request_body(
                    &req,
                    model,
                    reasoning_effort.as_deref(),
                ))
                .send(),
        )
        .await?;

        let status = response.status();
        if !status.is_success() {
            // Read before the body: Retry-After rides the 429 headers.
            let retry_after = crate::parse_retry_after(response.headers());
            let body = crate::sse::read_error_body_capped(response).await;
            return Err(crate::classify_api_error_with_retry_after(
                format!("http_{}", status.as_u16()),
                crate::truncate_error_body(&body, crate::MAX_ERROR_BODY_CHARS),
                retry_after,
            ));
        }

        let byte_stream = response
            .bytes_stream()
            .map(|r| r.map_err(|e| LlmError::Http(e.to_string())));
        Ok(Box::pin(decode_responses_stream(crate::sse::stall_guard(
            byte_stream,
            crate::sse::STREAM_IDLE_TIMEOUT,
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

/// Builds the Responses URL: strips trailing `/` from base_url to avoid
/// double slashes.
fn responses_url(base_url: &str) -> String {
    format!("{}/responses", base_url.trim_end_matches('/'))
}

// ---- Request translation (pure functions) ----

/// Builds the Responses request body (`stream` is always true).
///
/// `store: false` keeps the conversation local (the API otherwise persists
/// responses server-side). `instructions` carries the system prompt; an empty
/// one is omitted rather than sent as "". `reasoning.effort` is best-effort:
/// `Some` nests the effort and asks for a summary (`summary: auto`) so the
/// reasoning surfaces for display; `None` omits the whole `reasoning` object
/// for models that reject it.
pub(crate) fn build_request_body(
    req: &ChatRequest,
    model: &str,
    reasoning_effort: Option<&str>,
) -> Value {
    let mut body = serde_json::json!({
        "model": model,
        "input": translate_input(&req.messages),
        "stream": true,
        "store": false,
        // nosemgrep: codacy.yaml.security.hard-coded-tokens
        "max_output_tokens": req.max_tokens,
    });
    if !req.system.is_empty() {
        body["instructions"] = Value::String(req.system.clone());
    }
    if !req.tools.is_empty() {
        body["tools"] = Value::Array(req.tools.iter().map(translate_tool).collect());
    }
    if let Some(effort) = reasoning_effort {
        body["reasoning"] = serde_json::json!({"effort": effort, "summary": "auto"});
    }
    body
}

/// Translates one tool definition into the Responses flat tool shape.
pub(crate) fn translate_tool(spec: &ToolSpec) -> Value {
    serde_json::json!({
        "type": "function",
        "name": spec.name,
        "description": spec.description,
        "parameters": spec.input_schema,
    })
}

/// Translates the unified history into Responses `input` items.
///
/// Tool exchange becomes standalone items (not message content): a
/// `function_call` for the model's call and a `function_call_output` for its
/// result, paired by `call_id` — the same identifier wave stores, so pairing
/// survives the round trip. Consecutive user blocks that are not tool results
/// merge into one user message, keeping the alternation the API expects.
pub(crate) fn translate_input(messages: &[Message]) -> Vec<Value> {
    let mut items: Vec<Value> = Vec::new();
    for message in messages {
        match message.role {
            Role::User => translate_user_items(&message.content, &mut items),
            Role::Assistant => translate_assistant_items(&message.content, &mut items),
        }
    }
    items
}

/// One user message: text and images as input parts, tool results as
/// standalone `function_call_output` items.
fn translate_user_items(blocks: &[ContentBlock], items: &mut Vec<Value>) {
    let mut parts: Vec<Value> = Vec::new();
    for block in blocks {
        match block {
            ContentBlock::Text { text } => {
                if !text.is_empty() {
                    parts.push(serde_json::json!({"type": "input_text", "text": text}));
                }
            }
            ContentBlock::Image { mime, base64, .. } => {
                parts.push(image_part(mime, base64));
            }
            // A tool call inside a user message has no wire meaning; render
            // it as text so the intent is preserved instead of dropped.
            ContentBlock::ToolUse { id, name, input } => parts.push(serde_json::json!({
                "type": "input_text",
                "text": format!("[tool call {name} ({id}): {input}]"),
            })),
            ContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error,
            } => {
                // Results are items, not parts: flush the pending user
                // message first so ordering stays intact.
                flush_user_parts(&mut parts, items);
                items.push(serde_json::json!({
                    "type": "function_call_output",
                    "call_id": tool_use_id,
                    "output": tool_result_text(content, *is_error),
                }));
            }
            // Reasoning is Anthropic-wire state; the Responses input has no
            // slot for it here (reasoning items would need the item id and
            // encrypted payload, which wave does not retain), so the block is
            // dropped by design.
            ContentBlock::Thinking { .. } => {}
        }
    }
    flush_user_parts(&mut parts, items);
}

/// Emits the accumulated user parts as one message item (no-op when empty).
fn flush_user_parts(parts: &mut Vec<Value>, items: &mut Vec<Value>) {
    if !parts.is_empty() {
        items.push(serde_json::json!({
            "role": "user",
            "content": std::mem::take(parts),
        }));
    }
}

/// One assistant message: prose becomes `output_text` parts (merged with the
/// message that follows, matching the API's own item shape), tool calls
/// become standalone `function_call` items.
fn translate_assistant_items(blocks: &[ContentBlock], items: &mut Vec<Value>) {
    let mut parts: Vec<Value> = Vec::new();
    for block in blocks {
        match block {
            ContentBlock::Text { text } => {
                if !text.is_empty() {
                    parts.push(serde_json::json!({
                        "type": "output_text",
                        "text": text,
                        "annotations": [],
                    }));
                }
            }
            ContentBlock::ToolUse { id, name, input } => {
                flush_assistant_parts(&mut parts, items);
                items.push(serde_json::json!({
                    "type": "function_call",
                    "call_id": id,
                    "name": name,
                    // Compact JSON string; normalized so a legacy non-object
                    // payload never reaches the API as an invalid arguments
                    // shape.
                    "arguments": crate::normalize_tool_input(input.clone()).to_string(),
                }));
            }
            ContentBlock::Image { mime, .. } => {
                // Defensive: assistants cannot emit images on the wire; keep
                // a visible placeholder so the intent is not dropped.
                parts.push(serde_json::json!({
                    "type": "output_text",
                    "text": format!("[image {mime}]"),
                    "annotations": [],
                }));
            }
            ContentBlock::ToolResult {
                content, is_error, ..
            } => {
                // Defensive: a result inside an assistant message has no
                // wire slot; keep the failure visible as prose.
                parts.push(serde_json::json!({
                    "type": "output_text",
                    "text": tool_result_text(content, *is_error),
                    "annotations": [],
                }));
            }
            ContentBlock::Thinking { .. } => {}
        }
    }
    flush_assistant_parts(&mut parts, items);
}

/// Emits the accumulated assistant parts as one message item (no-op when
/// empty).
fn flush_assistant_parts(parts: &mut Vec<Value>, items: &mut Vec<Value>) {
    if !parts.is_empty() {
        items.push(serde_json::json!({
            "role": "assistant",
            "content": std::mem::take(parts),
        }));
    }
}

/// Renders a tool result as plain text; error results keep an `[error]`
/// prefix so failures stay visible instead of being dropped.
fn tool_result_text(content: &str, is_error: bool) -> String {
    if is_error {
        format!("[error] {content}")
    } else {
        content.to_string()
    }
}

/// Render one validated image as a Responses `input_image` part. The sandbox
/// applies the same mime/size rules as the other wires; an invalid image
/// becomes a visible text part naming the constraint, never a silent drop.
fn image_part(mime: &str, base64_data: &str) -> Value {
    match crate::validate_image(mime, base64_data) {
        Ok(_) => serde_json::json!({
            "type": "input_image",
            "image_url": format!("data:{mime};base64,{base64_data}"),
        }),
        Err(reason) => serde_json::json!({
            "type": "input_text",
            "text": format!("[invalid image: {reason}]"),
        }),
    }
}

// ---- Streaming decode ----

/// Byte-chunk stream to event stream: buffers bytes, splits SSE frames on
/// blank lines, and feeds each `data` payload to the Responses decoder.
fn decode_responses_stream<S>(
    byte_stream: S,
) -> impl futures::Stream<Item = Result<StreamEvent>> + Send
where
    S: futures::Stream<Item = std::result::Result<bytes::Bytes, LlmError>> + Send + 'static,
{
    let mut state = ResponsesStreamState::default();
    let inner = crate::sse::decode_sse_frames(byte_stream, crate::sse::MAX_SSE_BUF, move |data| {
        feed_responses_data(&mut state, data)
    });
    // A clean EOF with no `response.completed` / `response.incomplete` is a
    // torn stream, not a success — same wrapper as the Anthropic and chat
    // paths: the run loop must never read a cut stream as a finished sample.
    // Only those two events yield `MessageComplete`; the tolerated
    // chat-style `[DONE]` terminator sets no flag of its own.
    crate::sse::reject_torn_eof(inner, "stream ended without a response.completed frame")
}

/// One function call under assembly, identified by the item's `output_index`.
#[derive(Default)]
struct CallSlot {
    /// `call_id` from the API: the pairing key wave stores and sends back.
    call_id: String,
    name: String,
    /// Argument bytes for this `output_index`, including fragments that
    /// arrive while another call is also streaming.
    arguments: String,
    /// True once the closed begin/delta/end sequence has been emitted.
    emitted: bool,
}

/// Closed tool block for one finished call. Emitted when the call is
/// finished (`output_item.done`, or the response completing with the
/// call still open), so argument deltas never attach to a different call.
fn closed_call_events(slot: &CallSlot) -> Vec<StreamEvent> {
    let mut events = vec![StreamEvent::ToolUseBegin {
        id: slot.call_id.clone(),
        name: slot.name.clone(),
    }];
    if !slot.arguments.is_empty() {
        events.push(StreamEvent::ToolUseInputDelta {
            partial_json: slot.arguments.clone(),
        });
    }
    events.push(StreamEvent::BlockEnd);
    events
}

/// Close every function call that streaming never finished.
fn take_pending_calls(state: &mut ResponsesStreamState) -> Vec<StreamEvent> {
    let mut events = Vec::new();
    for slot in &mut state.slots {
        if slot.emitted || slot.call_id.is_empty() || slot.name.is_empty() {
            continue;
        }
        slot.emitted = true;
        events.extend(closed_call_events(slot));
    }
    events
}

/// Fill empty slots from the completed response's `output` array.
///
/// Some gateways send the finished function call only there, with no
/// `output_item.added` or `output_item.done`. The array position is the
/// item's `output_index`. Fields already accumulated from deltas stay.
fn absorb_output_calls(state: &mut ResponsesStreamState, response: &Value) {
    let Some(items) = response.get("output").and_then(Value::as_array) else {
        return;
    };
    for (index, item) in items.iter().enumerate() {
        if item.get("type").and_then(Value::as_str) != Some("function_call") {
            continue;
        }
        if index >= crate::MAX_TOOL_CALL_SLOTS {
            continue;
        }
        while state.slots.len() <= index {
            state.slots.push(CallSlot::default());
        }
        let slot = &mut state.slots[index];
        if let Some(call_id) = item.get("call_id").and_then(Value::as_str)
            && slot.call_id.is_empty()
            && !call_id.is_empty()
        {
            slot.call_id = call_id.to_string();
        }
        if let Some(name) = item.get("name").and_then(Value::as_str)
            && slot.name.is_empty()
            && !name.is_empty()
        {
            slot.name = name.to_string();
        }
        if slot.arguments.is_empty()
            && let Some(arguments) = item.get("arguments").and_then(Value::as_str)
            && !arguments.is_empty()
        {
            slot.arguments = arguments.to_string();
        }
    }
}

/// Mutable decode state across SSE data payloads of one stream.
#[derive(Default)]
struct ResponsesStreamState {
    /// Function-call items, indexed by `output_index`.
    slots: Vec<CallSlot>,
    // nosemgrep: codacy.yaml.security.hard-coded-tokens
    input_tokens: u64,
    // nosemgrep: codacy.yaml.security.hard-coded-tokens
    output_tokens: u64,
    // nosemgrep: codacy.yaml.security.hard-coded-tokens
    cached_tokens: u64,
    /// True once the response reported an output-token limit.
    truncated: bool,
    /// Provider status when it was not a clean completion (e.g.
    /// `content_filter`); surfaces as the stop reason.
    status: Option<String>,
}

/// Reads usage, output calls, and the incomplete reason off a terminal
/// `response.completed` / `response.incomplete` envelope. The terminating
/// events stay in the caller: they are emitted even when the envelope is
/// missing.
fn read_completion(state: &mut ResponsesStreamState, response: &Value) {
    read_usage(state, response.get("usage"));
    absorb_output_calls(state, response);
    if let Some(reason) = response
        .get("incomplete_details")
        .and_then(|details| details.get("reason"))
        .and_then(Value::as_str)
    {
        // nosemgrep: codacy.yaml.security.hard-coded-tokens
        if reason == "max_output_tokens" {
            state.truncated = true;
        } else {
            state.status = Some(reason.to_string());
        }
    }
}

/// Feeds one SSE `data` payload, returning zero or more stream events.
///
/// Named events (`response.output_text.delta`, `response.function_call_arguments.
/// delta`, `response.output_item.added|done`, `response.completed|incomplete`,
/// `response.failed`, `error`) drive the same [`StreamEvent`] vocabulary the
/// other providers emit; unknown event types are ignored for forward
/// compatibility.
fn feed_responses_data(state: &mut ResponsesStreamState, data: &str) -> Result<Vec<StreamEvent>> {
    // Chat-style terminator: the Responses API itself ends with
    // `response.completed`, but gateways bridging both dialects append
    // `[DONE]`. Tolerating it keeps such a bridge usable; the message is
    // already complete by then.
    if data.trim() == "[DONE]" {
        return Ok(Vec::new());
    }
    let value: Value = serde_json::from_str(data)?;
    let Some(ty) = value.get("type").and_then(Value::as_str) else {
        // Missing type field: treat as an unknown event.
        return Ok(Vec::new());
    };
    let mut events = Vec::new();
    match ty {
        "response.output_text.delta" => {
            if let Some(delta) = value.get("delta").and_then(Value::as_str)
                && !delta.is_empty()
            {
                events.push(StreamEvent::TextDelta {
                    text: delta.to_string(),
                });
            }
        }
        // Reasoning summaries stream for display only: wave keeps no
        // reasoning item ids, so the text is not round-tripped.
        "response.reasoning_summary_text.delta" => {
            if let Some(delta) = value.get("delta").and_then(Value::as_str)
                && !delta.is_empty()
            {
                events.push(StreamEvent::ThinkingDelta {
                    text: delta.to_string(),
                });
            }
        }
        "response.output_item.added" => {
            if let Some(item) = value.get("item")
                && item.get("type").and_then(Value::as_str) == Some("function_call")
            {
                let index = output_index(&value, state)?;
                let slot = &mut state.slots[index];
                if let Some(call_id) = item.get("call_id").and_then(Value::as_str)
                    && !call_id.is_empty()
                {
                    slot.call_id = call_id.to_string();
                }
                if let Some(name) = item.get("name").and_then(Value::as_str)
                    && !name.is_empty()
                {
                    slot.name = name.to_string();
                }
                if let Some(arguments) = item.get("arguments").and_then(Value::as_str)
                    && !arguments.is_empty()
                {
                    slot.arguments.push_str(arguments);
                }
            }
        }
        "response.function_call_arguments.delta" => {
            if let Some(delta) = value.get("delta").and_then(Value::as_str)
                && !delta.is_empty()
            {
                let index = output_index(&value, state)?;
                if !state.slots[index].emitted {
                    state.slots[index].arguments.push_str(delta);
                }
            }
        }
        "response.output_item.done" => {
            if let Some(item) = value.get("item") {
                match item.get("type").and_then(Value::as_str) {
                    Some("function_call") => {
                        let index = output_index(&value, state)?;
                        let slot = &mut state.slots[index];
                        // Defensive completion: a stream that skipped
                        // `output_item.added` still yields a usable call.
                        if let Some(call_id) = item.get("call_id").and_then(Value::as_str)
                            && slot.call_id.is_empty()
                        {
                            slot.call_id = call_id.to_string();
                        }
                        if let Some(name) = item.get("name").and_then(Value::as_str)
                            && slot.name.is_empty()
                        {
                            slot.name = name.to_string();
                        }
                        if slot.arguments.is_empty()
                            && let Some(arguments) = item.get("arguments").and_then(Value::as_str)
                            && !arguments.is_empty()
                        {
                            slot.arguments = arguments.to_string();
                        }
                        if !slot.emitted && !slot.call_id.is_empty() && !slot.name.is_empty() {
                            slot.emitted = true;
                            events.extend(closed_call_events(slot));
                        }
                    }
                    // A finished message item closes the prose block so the
                    // adapter flushes its text buffer at the same boundary
                    // the other providers use.
                    Some("message") => events.push(StreamEvent::BlockEnd),
                    _ => {}
                }
            }
        }
        "response.completed" | "response.incomplete" => {
            if let Some(response) = value.get("response") {
                read_completion(state, response);
            }
            // A gateway that skips `output_item.done` would otherwise drop
            // the call: arguments stay buffered until something closes the
            // slot, and `MessageComplete` cannot invent a call it never saw.
            events.extend(take_pending_calls(state));
            events.push(StreamEvent::MessageComplete {
                stop_reason: stop_reason(state),
                usage: Usage {
                    // nosemgrep: codacy.yaml.security.hard-coded-tokens
                    input_tokens: state.input_tokens,
                    // nosemgrep: codacy.yaml.security.hard-coded-tokens
                    output_tokens: state.output_tokens,
                    // nosemgrep: codacy.yaml.security.hard-coded-tokens
                    cache_read_tokens: state.cached_tokens,
                    // nosemgrep: codacy.yaml.security.hard-coded-tokens
                    cache_creation_tokens: 0,
                },
            });
        }
        "response.failed" => {
            let detail = value
                .get("response")
                .and_then(|response| response.get("error"))
                .and_then(|error| {
                    let message = error.get("message").and_then(Value::as_str)?;
                    let code = error
                        .get("code")
                        .and_then(Value::as_str)
                        .unwrap_or("failed");
                    Some(format!("{code}: {message}"))
                })
                .unwrap_or_else(|| "response.failed".to_string());
            return Err(crate::classify_api_error("response_failed".into(), detail));
        }
        "error" => {
            let kind = value
                .get("code")
                .and_then(Value::as_str)
                .unwrap_or("openai_error")
                .to_string();
            let message = value
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or(data)
                .to_string();
            return Err(crate::classify_api_error(kind, message));
        }
        _ => {}
    }
    Ok(events)
}

/// Slot index for an event: the item's `output_index` when present, else the
/// most recent slot (a gateway omitting it still streams one call at a time in
/// practice). Grows the slot table as needed.
///
/// `output_index` is provider-controlled input, so growth is bounded by
/// [`crate::MAX_TOOL_CALL_SLOTS`]: a bogus index surfaces as a parse error
/// instead of an allocation bomb.
fn output_index(value: &Value, state: &mut ResponsesStreamState) -> Result<usize> {
    let index = value
        .get("output_index")
        .and_then(Value::as_u64)
        .map(|index| index as usize)
        .unwrap_or_else(|| state.slots.len().saturating_sub(1));
    if index >= crate::MAX_TOOL_CALL_SLOTS {
        return Err(LlmError::Sse(format!(
            "output_index {index} exceeds the {}-slot cap",
            crate::MAX_TOOL_CALL_SLOTS
        )));
    }
    while state.slots.len() <= index {
        state.slots.push(CallSlot::default());
    }
    Ok(index)
}

/// Copies the usage block: `input_tokens` is the full prompt (cache reads are
/// a subset detail, mirroring the Chat Completions mapping), `output_tokens`
/// the completion.
fn read_usage(state: &mut ResponsesStreamState, usage: Option<&Value>) {
    let Some(usage) = usage else {
        return;
    };
    if let Some(input) = usage.get("input_tokens").and_then(Value::as_u64) {
        state.input_tokens = input;
    }
    if let Some(output) = usage.get("output_tokens").and_then(Value::as_u64) {
        state.output_tokens = output;
    }
    if let Some(cached) = usage
        .get("input_tokens_details")
        .and_then(|details| details.get("cached_tokens"))
        .and_then(Value::as_u64)
    {
        state.cached_tokens = cached;
    }
}

/// The stop reason wave's loop matches on: an output-token limit maps to
/// `max_tokens` (the continuation trigger), a provider status like
/// `content_filter` passes through, and everything else reads as a clean stop.
fn stop_reason(state: &ResponsesStreamState) -> String {
    if state.truncated {
        return "max_tokens".to_string();
    }
    state.status.clone().unwrap_or_else(|| "stop".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Message;
    use std::sync::Arc;

    fn tool_spec() -> ToolSpec {
        ToolSpec {
            name: "read_file".to_string(),
            description: "read a file".to_string(),
            input_schema: serde_json::json!({"type": "object", "properties": {}}),
        }
    }

    fn user_text(text: &str) -> Message {
        Message {
            role: Role::User,
            content: vec![ContentBlock::Text {
                text: text.to_string(),
            }],
        }
    }

    async fn run_decode(chunks: Vec<&'static [u8]>) -> Vec<Result<StreamEvent>> {
        let stream = futures::stream::iter(
            chunks
                .into_iter()
                .map(|c| Ok(bytes::Bytes::from_static(c)))
                .collect::<Vec<_>>(),
        );
        decode_responses_stream(stream).collect().await
    }

    #[test]
    fn request_body_carries_instructions_items_and_flat_tools() {
        let req = ChatRequest {
            model: "gpt-5".to_string(),
            system: "be terse".to_string(),
            messages: Arc::new(vec![user_text("hi")]),
            tools: Arc::new(vec![tool_spec()]),
            // nosemgrep: codacy.yaml.security.hard-coded-tokens
            max_tokens: 2048,
        };
        let body = build_request_body(&req, "gpt-5", None);
        assert_eq!(body["model"], "gpt-5");
        assert_eq!(body["instructions"], "be terse");
        assert_eq!(body["stream"], true);
        // Conversations stay local: never persisted server-side.
        assert_eq!(body["store"], false);
        // nosemgrep: codacy.yaml.security.hard-coded-tokens
        assert_eq!(body["max_output_tokens"], 2048);
        assert_eq!(body["input"][0]["role"], "user");
        assert_eq!(body["input"][0]["content"][0]["type"], "input_text");
        // Flat tool shape (no nested `function` object).
        assert_eq!(body["tools"][0]["type"], "function");
        assert_eq!(body["tools"][0]["name"], "read_file");
        assert!(body["tools"][0].get("function").is_none());
        assert!(body.get("reasoning").is_none());
    }

    #[test]
    fn reasoning_effort_nests_under_reasoning_with_a_summary() {
        let req = ChatRequest {
            model: "gpt-5".to_string(),
            system: String::new(),
            messages: Arc::new(Vec::new()),
            tools: Arc::new(Vec::new()),
            // nosemgrep: codacy.yaml.security.hard-coded-tokens
            max_tokens: 100,
        };
        let body = build_request_body(&req, "gpt-5", Some("low"));
        assert_eq!(body["reasoning"]["effort"], "low");
        assert_eq!(body["reasoning"]["summary"], "auto");
        // Empty system/tools stay out of the body entirely.
        assert!(body.get("instructions").is_none());
        assert!(body.get("tools").is_none());
    }

    #[test]
    fn tool_exchange_becomes_paired_standalone_items() {
        let messages = vec![
            user_text("list files"),
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::ToolUse {
                    id: "call_1".to_string(),
                    name: "shell".to_string(),
                    input: serde_json::json!({"command": "ls"}),
                }],
            },
            Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "call_1".to_string(),
                    content: "a.txt".to_string(),
                    is_error: false,
                }],
            },
        ];
        let items = translate_input(&messages);
        assert_eq!(items.len(), 3);
        assert_eq!(items[0]["content"][0]["type"], "input_text");
        assert_eq!(items[1]["type"], "function_call");
        assert_eq!(items[1]["call_id"], "call_1");
        assert_eq!(items[1]["name"], "shell");
        assert_eq!(items[1]["arguments"], "{\"command\":\"ls\"}");
        // The result is its own item, paired by the same call id.
        assert_eq!(items[2]["type"], "function_call_output");
        assert_eq!(items[2]["call_id"], "call_1");
        assert_eq!(items[2]["output"], "a.txt");
    }

    #[test]
    fn assistant_prose_uses_output_text_parts() {
        let items = translate_input(&[Message {
            role: Role::Assistant,
            content: vec![ContentBlock::Text {
                text: "done".to_string(),
            }],
        }]);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["role"], "assistant");
        assert_eq!(items[0]["content"][0]["type"], "output_text");
        assert_eq!(items[0]["content"][0]["text"], "done");
    }

    #[tokio::test]
    async fn streams_text_tool_call_and_completion() {
        let results = run_decode(vec![
            b"data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_1\"}}\n\n",
            b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"Hello\"}\n\n",
            b"data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"message\"}}\n\n",
            b"data: {\"type\":\"response.output_item.added\",\"output_index\":1,\"item\":{\"type\":\"function_call\",\"call_id\":\"call_9\",\"name\":\"shell\",\"arguments\":\"\"}}\n\n",
            b"data: {\"type\":\"response.function_call_arguments.delta\",\"output_index\":1,\"item_id\":\"fc_9\",\"delta\":\"{\\\"command\\\":\"}\n\n",
            b"data: {\"type\":\"response.function_call_arguments.delta\",\"output_index\":1,\"item_id\":\"fc_9\",\"delta\":\"\\\"ls\\\"}\"}\n\n",
            b"data: {\"type\":\"response.output_item.done\",\"output_index\":1,\"item\":{\"type\":\"function_call\",\"call_id\":\"call_9\",\"name\":\"shell\",\"arguments\":\"{\\\"command\\\":\\\"ls\\\"}\"}}\n\n",
            b"data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",\"usage\":{\"input_tokens\":120,\"output_tokens\":9,\"input_tokens_details\":{\"cached_tokens\":100}}}}\n\n",
        ])
        .await;
        let events: Vec<StreamEvent> = results.into_iter().collect::<Result<Vec<_>>>().unwrap();
        assert_eq!(
            events,
            vec![
                StreamEvent::TextDelta {
                    text: "Hello".to_string()
                },
                StreamEvent::BlockEnd,
                StreamEvent::ToolUseBegin {
                    id: "call_9".to_string(),
                    name: "shell".to_string()
                },
                StreamEvent::ToolUseInputDelta {
                    partial_json: "{\"command\":\"ls\"}".to_string()
                },
                StreamEvent::BlockEnd,
                StreamEvent::MessageComplete {
                    stop_reason: "stop".to_string(),
                    usage: Usage {
                        // nosemgrep: codacy.yaml.security.hard-coded-tokens
                        input_tokens: 120,
                        // nosemgrep: codacy.yaml.security.hard-coded-tokens
                        output_tokens: 9,
                        // Cache reads are a subset detail of input_tokens.
                        // nosemgrep: codacy.yaml.security.hard-coded-tokens
                        cache_read_tokens: 100,
                        // nosemgrep: codacy.yaml.security.hard-coded-tokens
                        cache_creation_tokens: 0,
                    },
                },
            ]
        );
    }

    #[tokio::test]
    async fn incomplete_response_maps_to_a_truncation_stop() {
        let results = run_decode(vec![
            // nosemgrep: codacy.yaml.security.hard-coded-tokens
            b"data: {\"type\":\"response.completed\",\"response\":{\"status\":\"incomplete\",\"incomplete_details\":{\"reason\":\"max_output_tokens\"},\"usage\":{\"input_tokens\":5,\"output_tokens\":4096}}}\n\n",
        ])
        .await;
        let events: Vec<StreamEvent> = results.into_iter().collect::<Result<Vec<_>>>().unwrap();
        match events.last() {
            Some(StreamEvent::MessageComplete { stop_reason, .. }) => {
                assert_eq!(stop_reason, "max_tokens")
            }
            other => panic!("expected MessageComplete, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn reasoning_summary_streams_for_display() {
        let results = run_decode(vec![
            b"data: {\"type\":\"response.reasoning_summary_text.delta\",\"delta\":\"weighing\"}\n\n",
            b"data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\"}}\n\n",
        ])
        .await;
        let events: Vec<StreamEvent> = results.into_iter().collect::<Result<Vec<_>>>().unwrap();
        assert!(matches!(
            events.first(),
            Some(StreamEvent::ThinkingDelta { text }) if text == "weighing"
        ));
    }

    #[tokio::test]
    async fn chat_style_done_frame_is_tolerated() {
        let results = run_decode(vec![
            b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"hi\"}\n\n",
            b"data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\"}}\n\n",
            b"data: [DONE]\n\n",
        ])
        .await;
        let events: Vec<StreamEvent> = results.into_iter().collect::<Result<Vec<_>>>().unwrap();
        assert!(matches!(
            events.first(),
            Some(StreamEvent::TextDelta { text }) if text == "hi"
        ));
        assert!(matches!(
            events.last(),
            Some(StreamEvent::MessageComplete { .. })
        ));
    }

    #[tokio::test]
    async fn failed_response_and_error_events_abort_the_stream() {
        let failed = run_decode(vec![
            b"data: {\"type\":\"response.failed\",\"response\":{\"error\":{\"code\":\"server_error\",\"message\":\"boom\"}}}\n\n",
        ])
        .await;
        let error = failed.into_iter().next().expect("one event");
        assert!(error.is_err(), "response.failed must abort: {error:?}");

        let streamed = run_decode(vec![
            b"data: {\"type\":\"error\",\"code\":\"invalid_request\",\"message\":\"bad tools\"}\n\n",
        ])
        .await;
        let error = streamed.into_iter().next().expect("one event");
        assert!(error.is_err(), "error event must abort: {error:?}");

        // A context-overflow shape classifies as PromptTooLong so the loop
        // can react with compaction instead of failing the turn.
        let too_long = run_decode(vec![
            b"data: {\"type\":\"error\",\"code\":\"invalid_request\",\"message\":\"prompt is too long\"}\n\n",
        ])
        .await;
        let error = too_long.into_iter().next().expect("one event");
        assert!(matches!(error, Err(LlmError::PromptTooLong { .. })));
    }

    /// A clean EOF that never delivers `response.completed` is a torn
    /// stream: the wrapper ends it with an error, not silent success.
    #[tokio::test]
    async fn clean_eof_without_completion_is_an_error() {
        let results = run_decode(vec![
            b"event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"partial\"}\n\n",
        ])
        .await;
        assert_eq!(results.len(), 2);
        assert!(matches!(&results[0], Ok(StreamEvent::TextDelta { .. })));
        assert!(
            matches!(&results[1], Err(LlmError::Sse(msg)) if msg.contains("response.completed")),
            "EOF without a completion must surface as an error: {:?}",
            results[1]
        );
    }

    /// A provider-side `output_index` beyond the slot cap must fail the
    /// stream instead of growing the slot table (allocation bomb).
    #[tokio::test]
    async fn oversized_output_index_is_an_error() {
        let results = run_decode(vec![
            b"event: response.output_item.added\ndata: {\"type\":\"response.output_item.added\",\"output_index\":100000000,\"item\":{\"type\":\"function_call\",\"call_id\":\"c1\",\"name\":\"f\"}}\n\n",
        ])
        .await;
        assert!(
            matches!(&results[0], Err(LlmError::Sse(msg)) if msg.contains("cap")),
            "a huge output_index must fail the stream: {:?}",
            results[0]
        );
    }

    /// One function call rides `output_item.added` (begin), argument
    /// deltas, and `output_item.done` (BlockEnd), then the response-level
    /// completion closes the turn.
    #[tokio::test]
    async fn tool_call_stream_pairs_begin_delta_and_end() {
        let results = run_decode(vec![
            b"event: response.output_item.added\ndata: {\"type\":\"response.output_item.added\",\"item\":{\"type\":\"function_call\",\"output_index\":0,\"call_id\":\"c1\",\"name\":\"shell\"}}\n\n",
            b"event: response.function_call_arguments.delta\ndata: {\"type\":\"response.function_call_arguments.delta\",\"output_index\":0,\"delta\":\"{\\\"command\\\":\"}\n\n",
            b"event: response.output_item.done\ndata: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"function_call\",\"output_index\":0}}\n\n",
            b"event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{}}\n\n",
        ])
        .await;
        let events: Vec<StreamEvent> = results.into_iter().collect::<Result<Vec<_>>>().unwrap();
        assert!(
            matches!(events[0], StreamEvent::ToolUseBegin { ref name, .. } if name == "shell"),
            "{events:?}"
        );
        assert!(
            matches!(events[1], StreamEvent::ToolUseInputDelta { .. }),
            "{events:?}"
        );
        assert!(matches!(events[2], StreamEvent::BlockEnd), "{events:?}");
        assert!(
            matches!(events.last(), Some(StreamEvent::MessageComplete { .. })),
            "{events:?}"
        );
    }

    /// A stream that skips `output_item.done` still closes the call when
    /// the response completes. The arguments were buffered on the slot.
    #[tokio::test]
    async fn completed_without_item_done_still_closes_the_call() {
        let results = run_decode(vec![
            b"data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"function_call\",\"call_id\":\"c1\",\"name\":\"shell\"}}\n\n",
            b"data: {\"type\":\"response.function_call_arguments.delta\",\"output_index\":0,\"delta\":\"{\\\"command\\\":\\\"ls\\\"}\"}\n\n",
            b"data: {\"type\":\"response.completed\",\"response\":{}}\n\n",
        ])
        .await;
        let events: Vec<StreamEvent> = results.into_iter().collect::<Result<Vec<_>>>().unwrap();
        assert_eq!(
            events,
            vec![
                StreamEvent::ToolUseBegin {
                    id: "c1".to_string(),
                    name: "shell".to_string(),
                },
                StreamEvent::ToolUseInputDelta {
                    partial_json: "{\"command\":\"ls\"}".to_string(),
                },
                StreamEvent::BlockEnd,
                StreamEvent::MessageComplete {
                    stop_reason: "stop".to_string(),
                    usage: Usage::default(),
                },
            ]
        );
    }

    /// A gateway that sends only the completed response, with the function
    /// call inside `output`, still yields one closed tool block.
    #[tokio::test]
    async fn completed_output_alone_still_yields_the_call() {
        let results = run_decode(vec![
            b"data: {\"type\":\"response.completed\",\"response\":{\"output\":[{\"type\":\"function_call\",\"call_id\":\"c1\",\"name\":\"shell\",\"arguments\":\"{\\\"command\\\":\\\"ls\\\"}\"}]}}\n\n",
        ])
        .await;
        let events: Vec<StreamEvent> = results.into_iter().collect::<Result<Vec<_>>>().unwrap();
        assert_eq!(
            events,
            vec![
                StreamEvent::ToolUseBegin {
                    id: "c1".to_string(),
                    name: "shell".to_string(),
                },
                StreamEvent::ToolUseInputDelta {
                    partial_json: "{\"command\":\"ls\"}".to_string(),
                },
                StreamEvent::BlockEnd,
                StreamEvent::MessageComplete {
                    stop_reason: "stop".to_string(),
                    usage: Usage::default(),
                },
            ]
        );
    }

    /// Argument deltas for two calls may interleave by `output_index`.
    /// Each call's bytes stay on its own slot and close as its own block.
    #[tokio::test]
    async fn interleaved_argument_deltas_stay_on_their_call() {
        let results = run_decode(vec![
            b"data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"function_call\",\"call_id\":\"c0\",\"name\":\"read\"}}\n\n",
            b"data: {\"type\":\"response.output_item.added\",\"output_index\":1,\"item\":{\"type\":\"function_call\",\"call_id\":\"c1\",\"name\":\"shell\"}}\n\n",
            b"data: {\"type\":\"response.function_call_arguments.delta\",\"output_index\":0,\"delta\":\"{\\\"path\\\":\"}\n\n",
            b"data: {\"type\":\"response.function_call_arguments.delta\",\"output_index\":1,\"delta\":\"{\\\"command\\\":\"}\n\n",
            b"data: {\"type\":\"response.function_call_arguments.delta\",\"output_index\":0,\"delta\":\"\\\"a.rs\\\"}\"}\n\n",
            b"data: {\"type\":\"response.function_call_arguments.delta\",\"output_index\":1,\"delta\":\"\\\"ls\\\"}\"}\n\n",
            b"data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"function_call\"}}\n\n",
            b"data: {\"type\":\"response.output_item.done\",\"output_index\":1,\"item\":{\"type\":\"function_call\"}}\n\n",
            b"data: {\"type\":\"response.completed\",\"response\":{}}\n\n",
        ])
        .await;
        let events: Vec<StreamEvent> = results.into_iter().collect::<Result<Vec<_>>>().unwrap();
        assert_eq!(
            events,
            vec![
                StreamEvent::ToolUseBegin {
                    id: "c0".to_string(),
                    name: "read".to_string(),
                },
                StreamEvent::ToolUseInputDelta {
                    partial_json: "{\"path\":\"a.rs\"}".to_string(),
                },
                StreamEvent::BlockEnd,
                StreamEvent::ToolUseBegin {
                    id: "c1".to_string(),
                    name: "shell".to_string(),
                },
                StreamEvent::ToolUseInputDelta {
                    partial_json: "{\"command\":\"ls\"}".to_string(),
                },
                StreamEvent::BlockEnd,
                StreamEvent::MessageComplete {
                    stop_reason: "stop".to_string(),
                    usage: Usage::default(),
                },
            ]
        );
    }

    /// The `reasoning` block rides only when an effort is set, and it
    /// always carries the auto summary.
    #[test]
    fn reasoning_effort_is_injected_with_auto_summary() {
        let req = ChatRequest {
            model: "gpt-5".to_string(),
            system: String::new(),
            messages: Arc::new(vec![user_text("hi")]),
            tools: Arc::new(Vec::new()),
            // nosemgrep: codacy.yaml.security.hard-coded-tokens
            max_tokens: 16,
        };
        let plain = build_request_body(&req, "gpt-5", None);
        assert!(plain.get("reasoning").is_none());
        let reasoned = build_request_body(&req, "gpt-5", Some("high"));
        assert_eq!(reasoned["reasoning"]["effort"], "high");
        assert_eq!(reasoned["reasoning"]["summary"], "auto");
    }

    /// Assistant history translates into standalone items: prose becomes
    /// a `message` item, the tool call a `function_call` keyed by the
    /// wave call id.
    #[test]
    fn assistant_history_becomes_message_and_function_call_items() {
        let history = vec![Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::Text {
                    text: "ans".to_string(),
                },
                ContentBlock::ToolUse {
                    id: "c1".to_string(),
                    name: "shell".to_string(),
                    input: serde_json::json!({"command": "ls"}),
                },
            ],
        }];
        let items = translate_input(&history);
        assert_eq!(items.len(), 2, "{items:?}");
        // The prose item keeps the assistant role and output_text parts;
        // the wire item omits an explicit `type` (server-side default).
        assert_eq!(items[0]["role"], "assistant");
        assert_eq!(items[0]["content"][0]["type"], "output_text");
        assert_eq!(items[1]["type"], "function_call");
        assert_eq!(items[1]["call_id"], "c1");
    }

    /// A user image block becomes an `input_image` part next to the
    /// text parts of the same message.
    #[test]
    fn user_images_become_input_image_parts() {
        let history = vec![Message {
            role: Role::User,
            content: vec![
                ContentBlock::Text {
                    text: "look".to_string(),
                },
                ContentBlock::Image {
                    id: None,
                    mime: "image/png".to_string(),
                    base64: "aGVsbG8=".to_string(),
                },
            ],
        }];
        let items = translate_input(&history);
        let content = items[0]["content"].as_array().unwrap();
        assert_eq!(content.len(), 2, "{items:?}");
        assert_eq!(content[0]["type"], "input_text");
        assert_eq!(content[1]["type"], "input_image");
    }

    /// Regression test: redirects are disabled, so the bearer token cannot
    /// leak to a cross-origin target (the same guarantee the Anthropic
    /// client locks for `x-api-key`).
    ///
    /// Service A answers POST with a 301 to B; assert the client errors
    /// directly (http_301) instead of following, and B never receives a
    /// request (reqwest follows redirects by default and would carry the
    /// `Authorization` header, verified with a PoC on the Anthropic side).
    #[tokio::test]
    async fn redirect_is_not_followed_and_bearer_token_not_leaked() {
        use std::io::Write;
        use std::net::TcpListener;
        use std::sync::mpsc;

        // B: the redirect target; on receiving a request it sends the full
        // request head back to the main thread.
        let (b_tx, b_rx) = mpsc::channel::<String>();
        let listener_b = TcpListener::bind("127.0.0.1:0").unwrap();
        let port_b = listener_b.local_addr().unwrap().port();
        std::thread::spawn(move || {
            if let Ok((mut s, _)) = listener_b.accept() {
                let head = crate::test_support::read_http_request_head(&mut s);
                let _ = b_tx.send(head);
            }
        });

        // A: the entry service; records the request head (proving the token
        // did reach A) then replies 301 to B.
        let (a_tx, a_rx) = mpsc::channel::<String>();
        let listener_a = TcpListener::bind("127.0.0.1:0").unwrap();
        let port_a = listener_a.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let (mut s, _) = listener_a.accept().unwrap();
            let head = crate::test_support::read_http_request_head(&mut s);
            let _ = a_tx.send(head);
            let resp = format!(
                "HTTP/1.1 301 Moved Permanently\r\nLocation: http://127.0.0.1:{port_b}/responses\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            );
            s.write_all(resp.as_bytes()).unwrap();
        });

        let client = ResponsesClient::new(
            format!("http://127.0.0.1:{port_a}"),
            "sk-secret-key".into(),
            "m".into(),
        );
        let req = ChatRequest {
            model: "m".into(),
            system: String::new(),
            messages: Arc::new(vec![]),
            tools: Arc::new(vec![]),
            // nosemgrep: codacy.yaml.security.hard-coded-tokens
            max_tokens: 1,
        };
        let err = match client.stream(req).await {
            Ok(_) => panic!("a 301 should error directly instead of being followed"),
            Err(e) => e,
        };
        // With redirects disabled, the 301 comes back as a plain response and stream() reports it as non-2xx.
        assert!(
            matches!(&err, LlmError::Api { kind, .. } if kind.as_str() == "http_301"),
            "a 301 should report http_301, not be followed: {err:?}"
        );
        // Precondition: A did receive the request carrying the token (otherwise this test is meaningless).
        let head_a = a_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        assert!(
            head_a
                .to_ascii_lowercase()
                .contains("authorization: bearer sk-secret-key"),
            "A should have received the request head carrying the bearer token: {head_a}"
        );
        // B never receives a request: no connection on loopback within 500ms counts as not followed.
        assert!(
            b_rx.recv_timeout(std::time::Duration::from_millis(500))
                .is_err(),
            "the redirect was followed; the bearer token leaked to B"
        );
    }
}
