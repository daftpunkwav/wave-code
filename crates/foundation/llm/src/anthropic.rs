/*!
 * @file AnthropicClient
 * @description Anthropic Messages API streaming HTTP client.
 *
 * Responsibilities:
 * - Build and send streaming requests to the Anthropic Messages endpoint.
 * - Manage prompt-caching breakpoints and extended thinking configurations.
 * - Provide fallible client construction (try_new) and safe TLS initialization.
 *
 * This module must not depend on: runtime, config, or UI-layer components.
 */

//! Anthropic Messages API streaming HTTP client.
//!
//! `POST {base_url}/v1/messages` starts an SSE streaming request; byte chunks flow through
//! buffering and framing, then [`crate::SseParser`] parses each frame into [`crate::StreamEvent`].

use std::time::Duration;

use futures::{Stream, StreamExt};

use crate::sse::{MAX_SSE_BUF, STREAM_IDLE_TIMEOUT, stall_guard};
use crate::{
    ChatModel, ChatRequest, ContentBlock, EventStream, LlmError, Message, Result, Role, SseParser,
    StreamEvent, ToolSpec, validate_image,
};

/// Prompt-cache entry lifetime for the injected breakpoints.
///
/// The provider default keeps an entry alive five minutes, refreshed on
/// every hit — fine for back-to-back turns, but one quiet stretch longer
/// than the TTL (a long build, an overnight pause in a multi-day run)
/// expires the whole cached prefix and the next request re-reads it at
/// full input price. The one-hour lifetime writes entries at twice the
/// base input rate instead of 1.25x, trading a small premium per write
/// for prefixes that survive hour-scale gaps; long-running sessions are
/// the case it pays for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CacheTtl {
    /// Provider default: five minutes, refreshed on every hit.
    #[default]
    FiveMinutes,
    /// One hour. No longer a beta on the current API (GA 2025-08); the
    /// historical flag still rides the request, see [`Self::beta_flag`].
    OneHour,
}

impl CacheTtl {
    /// Wire value for `cache_control.ttl`; `None` keeps the provider
    /// default (the field must stay absent for the five-minute lifetime).
    pub fn wire(self) -> Option<&'static str> {
        match self {
            Self::FiveMinutes => None,
            Self::OneHour => Some("1h"),
        }
    }

    /// `anthropic-beta` value the one-hour lifetime historically required.
    /// The provider graduated the feature (no header needed since
    /// 2025-08-13), but older Anthropic-protocol gateways may still gate
    /// `ttl: "1h"` behind the flag, so it keeps riding the request; the
    /// five-minute default needs no header.
    pub fn beta_flag(self) -> Option<&'static str> {
        match self {
            Self::FiveMinutes => None,
            Self::OneHour => Some("extended-cache-ttl-2025-04-11"),
        }
    }
}

/// Anthropic Messages API streaming client.
pub struct AnthropicClient {
    base_url: String,
    api_key: String,
    http: reqwest::Client,
    /// Inject prompt-cache breakpoints (system / last tool / last message
    /// block, `cache_control: ephemeral`) into the request body. On by
    /// default — repeated turns are the norm and cache reads are billed at a
    /// fraction of fresh input — but disableable for Anthropic-protocol
    /// gateways that reject the `cache_control` field.
    prompt_caching: bool,
    /// Lifetime of the injected cache entries (see [`CacheTtl`]).
    cache_ttl: CacheTtl,
    /// Extended-thinking budget in tokens (`thinking.budget_tokens`); `None`
    /// keeps thinking off so providers/agents that never asked for it are
    /// unchanged. The budget is clamped into the API-satisfiable range at
    /// request-build time (see [`thinking_body`]).
    thinking_budget: Option<u32>,
}

impl AnthropicClient {
    /// Creates a new client fallibly, returning an error if HTTP client or TLS init fails.
    pub fn try_new(base_url: String, api_key: String) -> Result<Self> {
        Ok(Self {
            base_url,
            api_key,
            http: build_http_client()?,
            prompt_caching: true,
            cache_ttl: CacheTtl::default(),
            thinking_budget: None,
        })
    }

    /// Creates a new client (prompt caching on, thinking off).
    pub fn new(base_url: String, api_key: String) -> Self {
        Self::try_new(base_url, api_key)
            .expect("TLS backend initialization failed; verify system certificates are installed")
    }

    /// Enable extended thinking with the given token budget. The budget is
    /// clamped per request into the API-satisfiable range (>= 1024 and
    /// < `max_tokens`); requests whose `max_tokens` cannot satisfy the
    /// 1024 minimum silently keep thinking off for that request.
    pub fn with_thinking_budget(mut self, budget_tokens: u32) -> Self {
        self.thinking_budget = Some(budget_tokens);
        self
    }

    /// Turn prompt-cache breakpoints off (third-party gateways that reject
    /// `cache_control`).
    pub fn with_prompt_caching(mut self, enabled: bool) -> Self {
        self.prompt_caching = enabled;
        self
    }

    /// Set the lifetime of the injected cache entries (see [`CacheTtl`]).
    pub fn with_cache_ttl(mut self, ttl: CacheTtl) -> Self {
        self.cache_ttl = ttl;
        self
    }
}

/// Builds the HTTP client.
///
/// - Only `connect_timeout` is set (10s): it guards against hangs in the connect phase; never set
///   `Client::timeout` - that would cut off healthy long-lived SSE streams. Read stalls are detected
///   at the stream layer by [`stall_guard`] (no bytes within [`STREAM_IDLE_TIMEOUT`] ends the stream with Err).
/// - Redirects are disabled: the Messages API endpoint has no legitimate redirect semantics,
///   so a redirect is an error; reqwest follows redirects by default and would carry `x-api-key`
///   to a cross-origin target (verified with a PoC), which must be ruled out.
fn build_http_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| LlmError::ClientInit(e.to_string()))
}

#[async_trait::async_trait]
impl ChatModel for AnthropicClient {
    async fn stream(&self, req: ChatRequest) -> Result<EventStream> {
        let url = messages_url(&self.base_url);
        // Bounded establishment: a server that accepts the connection but
        // never answers would otherwise hang `.send()` forever (see
        // [`sse::send_with_idle_bound`]).
        let response = crate::sse::send_with_idle_bound(
            {
                let request = self
                    .http
                    .post(url)
                    .header("x-api-key", &self.api_key)
                    .header("anthropic-version", "2023-06-01")
                    .header("content-type", "application/json");
                // The one-hour lifetime needs no beta header on the current
                // API; the historical flag still rides along for gateways
                // that predate its GA. The five-minute default sends none.
                let request = match self.cache_ttl.beta_flag() {
                    Some(flag) => request.header("anthropic-beta", flag),
                    None => request,
                };
                request.json(&build_request_body(
                    &req,
                    self.prompt_caching,
                    self.cache_ttl,
                    self.thinking_budget,
                ))
            }
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
                crate::truncate_error_body(&body, MAX_ERROR_BODY_CHARS),
                retry_after,
            ));
        }

        let byte_stream = response
            .bytes_stream()
            .map(|r| r.map_err(|e| LlmError::Http(e.to_string())));
        Ok(Box::pin(decode_event_stream(
            stall_guard(byte_stream, STREAM_IDLE_TIMEOUT),
            MAX_SSE_BUF,
        )))
    }
}

/// Builds the Messages API URL: strips trailing `/` from base_url to avoid double slashes.
fn messages_url(base_url: &str) -> String {
    format!("{}/v1/messages", base_url.trim_end_matches('/'))
}

/// Max retained chars of an error response body (the shared cap).
const MAX_ERROR_BODY_CHARS: usize = crate::MAX_ERROR_BODY_CHARS;

/// Builds the Anthropic Messages API request body (stream is always true).
///
/// `prompt_caching` injects up to three `cache_control: ephemeral` breakpoints
/// (system block, last tool, last message content block) so stable prefixes
/// (system prompt, tool schemas, older history) hit the provider cache on
/// every turn after the first; `ttl` sets the entries' lifetime (the
/// one-hour choice also sends a beta header, see [`CacheTtl::beta_flag`]);
/// `thinking_budget`
/// enables extended thinking (see [`thinking_body`]). All are
/// serialization-only: the caller's `ChatRequest` is never mutated.
pub(crate) fn build_request_body(
    req: &ChatRequest,
    prompt_caching: bool,
    ttl: CacheTtl,
    thinking_budget: Option<u32>,
) -> serde_json::Value {
    let merged = merge_adjacent_same_role(&req.messages);
    let mut messages = translate_messages(&merged, &req.model);
    if prompt_caching
        && let Some(last) = messages.last_mut()
        && let Some(last_block) = last
            .get_mut("content")
            .and_then(|c| c.as_array_mut())
            .and_then(|blocks| blocks.last_mut())
    {
        // The tail breakpoint caches each turn's prefix for the next turn
        // (incremental caching); the marker sits on the final content block.
        if let Some(obj) = last_block.as_object_mut() {
            obj.insert("cache_control".into(), cache_control(ttl));
        }
    }
    let mut body = serde_json::json!({
        "model": req.model,
        "messages": messages,
        "max_tokens": req.max_tokens,
        "stream": true,
    });
    body["system"] = system_body(&req.system, prompt_caching, ttl);
    body["tools"] = tools_body(&req.tools, prompt_caching, ttl);
    if let Some(budget) = thinking_budget
        && let Some(thinking) = thinking_body(budget, req.max_tokens)
    {
        body["thinking"] = thinking;
    }
    body
}

/// One `cache_control` breakpoint value: `ephemeral` with the configured
/// lifetime attached (the field stays absent for the provider default).
fn cache_control(ttl: CacheTtl) -> serde_json::Value {
    match ttl.wire() {
        Some(wire) => serde_json::json!({"type": "ephemeral", "ttl": wire}),
        None => serde_json::json!({"type": "ephemeral"}),
    }
}

/// Builds the `system` field: a single cached text block when caching is on
/// and the system prompt is non-empty; an empty system stays the empty string
/// (some gateways reject `[]`), and caching-off keeps the plain-string shape.
fn system_body(system: &str, prompt_caching: bool, ttl: CacheTtl) -> serde_json::Value {
    if system.is_empty() {
        return serde_json::Value::String(String::new());
    }
    if prompt_caching {
        serde_json::json!([{
            "type": "text",
            "text": system,
            "cache_control": cache_control(ttl),
        }])
    } else {
        serde_json::Value::String(system.to_owned())
    }
}

/// Builds the `tools` array: when caching is on the last tool carries the
/// cache breakpoint (tool schemas sit at the front of the cacheable prefix;
/// only one breakpoint is needed for the whole array).
fn tools_body(tools: &[ToolSpec], prompt_caching: bool, ttl: CacheTtl) -> serde_json::Value {
    let mut specs: Vec<serde_json::Value> = tools
        .iter()
        .map(serde_json::to_value)
        .collect::<std::result::Result<_, _>>()
        .unwrap_or_default();
    if prompt_caching
        && let Some(last) = specs.last_mut()
        && let Some(obj) = last.as_object_mut()
    {
        obj.insert("cache_control".into(), cache_control(ttl));
    }
    serde_json::Value::Array(specs)
}

/// Builds the `thinking` field: the API requires `budget_tokens >= 1024` and
/// `max_tokens > budget_tokens`. A `max_tokens` that cannot satisfy the
/// minimum keeps thinking off for that request (a config problem must not
/// fail every turn); otherwise the budget is clamped into the valid range.
fn thinking_body(budget: u32, max_tokens: u32) -> Option<serde_json::Value> {
    if max_tokens <= 1024 {
        return None;
    }
    let effective = budget.clamp(1024, max_tokens - 1);
    Some(serde_json::json!({
        "type": "enabled",
        "budget_tokens": effective,
    }))
}

/// Translates merged messages into Anthropic wire blocks.
/// Images become `image` blocks with a base64 source (mime/size validated);
/// invalid images become a visible text block naming the constraint, never a
/// silent drop. Text / tool_use / tool_result shapes match the unified serde
/// form so existing wire expectations are unchanged. Thinking blocks go back
/// as `thinking` (see [`translate_block`]); `for_model` decides whether
/// unsigned ones may be emitted at all.
pub(crate) fn translate_messages(messages: &[Message], for_model: &str) -> Vec<serde_json::Value> {
    let unsigned_ok = allows_unsigned_thinking(for_model);
    messages
        .iter()
        .map(|m| {
            let role = match m.role {
                Role::User => "user",
                Role::Assistant => "assistant",
            };
            let content: Vec<serde_json::Value> = m
                .content
                .iter()
                .filter_map(|block| translate_block(block, unsigned_ok))
                .collect();
            // Filtering can empty a message (e.g. an assistant turn whose
            // unsigned thinking was dropped for a Claude model); the API
            // rejects an empty `content` array with a 400, so a placeholder
            // text block keeps the turn well-formed.
            let content = if content.is_empty() {
                vec![serde_json::json!({"type": "text", "text": "(no sendable content)"})]
            } else {
                content
            };
            serde_json::json!({"role": role, "content": content})
        })
        .collect()
}

/// Whether an unsigned thinking block may travel to `model`.
///
/// api.anthropic.com validates the signature and rejects unsigned blocks from
/// Claude models, so those keep only signed history. Anthropic-compatible
/// endpoints stream thinking with no `signature_delta` yet reject a tool-call
/// turn whose thinking is missing, so their blocks must survive unsigned.
fn allows_unsigned_thinking(model: &str) -> bool {
    !model.to_ascii_lowercase().contains("claude")
}

/// Translates one content block into its Anthropic wire shape; `None` drops
/// the block (an unsigned thinking block bound for a Claude model, which can
/// only reject it).
fn translate_block(block: &ContentBlock, unsigned_ok: bool) -> Option<serde_json::Value> {
    let translated = match block {
        ContentBlock::Text { text } => serde_json::json!({"type": "text", "text": text}),
        ContentBlock::Image { mime, base64, .. } => match validate_image(mime, base64) {
            Ok(_) => serde_json::json!({
                "type": "image",
                "source": {"type": "base64", "media_type": mime, "data": base64},
            }),
            Err(reason) => {
                serde_json::json!({"type": "text", "text": format!("[invalid image: {reason}]")})
            }
        },
        ContentBlock::ToolUse { id, name, input } => {
            // History may still hold a pre-normalization non-object input
            // (a legacy session or a value stored before the parse-side
            // guard); coercing here keeps the request legal instead of
            // failing with a 400 on tool_use.input.
            serde_json::json!({
                "type": "tool_use",
                "id": id,
                "name": name,
                "input": crate::normalize_tool_input(input.clone()),
            })
        }
        ContentBlock::ToolResult {
            tool_use_id,
            content,
            is_error,
        } => serde_json::json!({
            "type": "tool_result",
            "tool_use_id": tool_use_id,
            "content": content,
            "is_error": is_error,
        }),
        ContentBlock::Thinking { text, signature } => match signature {
            // Signed thinking (what api.anthropic.com always sends) goes
            // back verbatim: a tool-call turn whose thinking is missing is
            // rejected.
            Some(signature) => serde_json::json!({
                "type": "thinking",
                "thinking": text,
                "signature": signature,
            }),
            // Unsigned thinking comes from Anthropic-compatible backends
            // that stream no signature_delta yet still reject a tool-call
            // turn whose thinking is gone. Claude models reject unsigned
            // blocks, so those history entries are dropped instead.
            None if unsigned_ok => serde_json::json!({
                "type": "thinking",
                "thinking": text,
            }),
            None => return None,
        },
    };
    Some(translated)
}

/// Merges adjacent same-role messages: the official endpoint auto-merges consecutive same-role
/// messages, but third-party gateways that enforce role alternation reject the "trailing
/// history tool_result (user) + appended instruction (user)" shape (a routine product of the
/// compact/sampling pipelines) with 400 - merging client-side stays compatible with every
/// endpoint. Merge means concatenating the content block arrays; adjacent Text blocks get a newline
/// separator so they cannot glue together, while ToolResult blocks sit side by side per protocol (several
/// tool_result blocks may legally belong to one user message).
fn merge_adjacent_same_role(messages: &[Message]) -> Vec<Message> {
    let mut out: Vec<Message> = Vec::with_capacity(messages.len());
    for msg in messages {
        if let Some(last) = out.last_mut()
            && last.role == msg.role
        {
            let mut content = std::mem::take(&mut last.content);
            match (content.last_mut(), msg.content.first()) {
                (
                    Some(ContentBlock::Text { text: prev }),
                    Some(ContentBlock::Text { text: next }),
                ) => {
                    prev.push('\n');
                    prev.push_str(next);
                    content.extend(msg.content.iter().skip(1).cloned());
                }
                _ => content.extend(msg.content.iter().cloned()),
            }
            last.content = content;
            continue;
        }
        out.push(msg.clone());
    }
    out
}

/// Byte-chunk stream to event stream: buffers bytes, splits SSE frames on blank lines, and hands data to [`SseParser`].
///
/// Thin adapter over the shared [`crate::sse::decode_sse_frames`] loop: framing,
/// buffering, and stall detection live there so both providers share one
/// memory-safety-critical implementation.
/// This function is the core parse path shared by [`ChatModel::stream`] and the tests.
fn decode_event_stream<S>(
    byte_stream: S,
    max_buf: usize,
) -> impl Stream<Item = Result<StreamEvent>> + Send
where
    S: Stream<Item = Result<bytes::Bytes>> + Send,
{
    let mut parser = SseParser::new();
    let inner = crate::sse::decode_sse_frames(byte_stream, max_buf, move |data| {
        Ok(parser.feed(data)?.into_iter().collect())
    });
    // A clean EOF with no `message_delta` completion is a torn stream, not a
    // success: the run loop would otherwise treat the sample as completed
    // with no billing data (and no MessageComplete ever closing the turn).
    crate::sse::reject_torn_eof(inner, "stream ended without a message_delta completion")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sse::find_subsequence;
    use crate::{ChatRequest, ContentBlock, Message, Role, ToolSpec};
    use futures::StreamExt;

    #[test]
    fn try_new_constructs_client() {
        let client = AnthropicClient::try_new(
            "https://api.anthropic.com".to_string(),
            "sk-ant-test".to_string(),
        );
        assert!(client.is_ok());
    }

    /// Stall guard: the first chunk passes through, then the upstream hangs - ends with Err
    /// within the idle timeout, and the timing is real (returns far below the generous upper bound).
    #[tokio::test]
    async fn stall_guard_errors_after_idle_timeout() {
        let first = Ok(bytes::Bytes::from_static(b"event: ping\n\n"));
        let hang: futures::stream::Pending<Result<bytes::Bytes>> = futures::stream::pending();
        let s = stall_guard(
            futures::stream::iter(vec![first]).chain(hang),
            Duration::from_millis(30),
        );
        let mut s = Box::pin(s);
        assert!(
            s.next().await.is_some(),
            "the first byte chunk should pass through"
        );
        let started = std::time::Instant::now();
        let second = s.next().await;
        let err = second
            .expect("a stall should yield an Err item, not end the stream")
            .unwrap_err();
        assert!(err.to_string().contains("idle timeout"), "{err}");
        assert!(
            started.elapsed() >= Duration::from_millis(30)
                && started.elapsed() < Duration::from_secs(5),
            "the timeout should count real time, not return immediately: {:?}",
            started.elapsed()
        );
    }

    /// A clean upstream end (None) is unaffected by the guard: the guard passes stream termination through.
    #[tokio::test]
    async fn stall_guard_passes_through_clean_eof() {
        let chunks = vec![
            Ok(bytes::Bytes::from_static(b"event: ping\n\n")),
            Ok(bytes::Bytes::from_static(b"event: message_stop\n\n")),
        ];
        let s = stall_guard(futures::stream::iter(chunks), Duration::from_millis(30));
        let events: Vec<_> = Box::pin(s).collect().await;
        assert_eq!(
            events.len(),
            2,
            "a clean EOF produces no stall Err: {events:?}"
        );
        assert!(events.iter().all(|r| r.is_ok()));
    }

    /// Test helper: turns string chunks into a byte-chunk stream, feeds the implementation-internal
    /// core parse function (the same parse path `stream()` uses), and collects all Ok events.
    async fn collect_events_from_chunks(chunks: Vec<&'static str>) -> Vec<crate::StreamEvent> {
        let byte_stream = futures::stream::iter(
            chunks
                .into_iter()
                .map(|s| Ok::<_, LlmError>(bytes::Bytes::from_static(s.as_bytes()))),
        );
        decode_event_stream(byte_stream, MAX_SSE_BUF)
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .filter_map(std::result::Result::ok)
            .collect()
    }

    #[test]
    fn messages_url_trims_trailing_slashes() {
        assert_eq!(
            messages_url("https://api.example.com"),
            "https://api.example.com/v1/messages"
        );
        assert_eq!(
            messages_url("https://api.example.com/"),
            "https://api.example.com/v1/messages"
        );
        assert_eq!(
            messages_url("https://api.example.com///"),
            "https://api.example.com/v1/messages"
        );
    }

    #[test]
    fn error_body_truncated_at_2000_chars() {
        let short = "x".repeat(100);
        assert_eq!(
            crate::truncate_error_body(&short, MAX_ERROR_BODY_CHARS),
            short
        );
        // Overlong bodies truncate to 2000 by char
        let long = "y".repeat(3000);
        assert_eq!(
            crate::truncate_error_body(&long, MAX_ERROR_BODY_CHARS)
                .chars()
                .count(),
            2000
        );
        // Multibyte chars count by char and are never split (splitting would garble at the from_utf8 level)
        let wide = "é".repeat(2500);
        let truncated = crate::truncate_error_body(&wide, MAX_ERROR_BODY_CHARS);
        assert_eq!(truncated.chars().count(), 2000);
        assert!(truncated.chars().all(|c| c == 'é'));
    }

    #[test]
    fn request_body_matches_anthropic_format() {
        let req = ChatRequest {
            model: "MiniMax-M3".into(),
            system: "sys".into(),
            messages: std::sync::Arc::new(vec![
                Message {
                    role: Role::User,
                    content: vec![ContentBlock::Text { text: "hi".into() }],
                },
                Message {
                    role: Role::Assistant,
                    content: vec![ContentBlock::ToolUse {
                        id: "t1".into(),
                        name: "read_file".into(),
                        input: serde_json::json!({"path": "a"}),
                    }],
                },
                Message {
                    role: Role::User,
                    content: vec![ContentBlock::ToolResult {
                        tool_use_id: "t1".into(),
                        content: "ok".into(),
                        is_error: false,
                    }],
                },
            ]),
            tools: vec![ToolSpec {
                name: "read_file".into(),
                description: "read".into(),
                input_schema: serde_json::json!({"type":"object"}),
            }],
            max_tokens: 8192,
        };
        let v = build_request_body(&req, false, CacheTtl::FiveMinutes, None);
        assert_eq!(v["model"], "MiniMax-M3");
        assert_eq!(v["system"], "sys");
        assert_eq!(v["stream"], true);
        assert_eq!(v["max_tokens"], 8192);
        assert_eq!(v["messages"][1]["content"][0]["type"], "tool_use");
        assert_eq!(v["messages"][2]["content"][0]["type"], "tool_result");
        assert_eq!(v["tools"][0]["name"], "read_file");
    }

    /// A non-object tool input (legacy session or pre-normalization value)
    /// is wrapped at the wire boundary so the request stays legal instead of
    /// failing with a 400 on tool_use.input.
    #[test]
    fn non_object_tool_input_is_wrapped_on_the_wire() {
        let req = ChatRequest {
            model: "MiniMax-M3".into(),
            system: "sys".into(),
            messages: std::sync::Arc::new(vec![Message {
                role: Role::Assistant,
                content: vec![ContentBlock::ToolUse {
                    id: "t1".into(),
                    name: "write".into(),
                    input: serde_json::json!("just a string"),
                }],
            }]),
            tools: vec![],
            max_tokens: 8192,
        };
        let v = build_request_body(&req, false, CacheTtl::FiveMinutes, None);
        assert_eq!(
            v["messages"][0]["content"][0]["input"],
            serde_json::json!({"_raw": "just a string"})
        );
    }

    /// Prompt caching injects exactly the three documented breakpoints
    /// (system block, last tool, last message content block); the caller's
    /// request and earlier blocks stay untouched.
    #[test]
    fn prompt_caching_injects_breakpoints() {
        let req = ChatRequest {
            model: "m1".into(),
            system: "sys".into(),
            messages: std::sync::Arc::new(vec![
                Message {
                    role: Role::User,
                    content: vec![ContentBlock::Text { text: "hi".into() }],
                },
                Message {
                    role: Role::Assistant,
                    content: vec![ContentBlock::Text {
                        text: "thinking".into(),
                    }],
                },
                Message {
                    role: Role::User,
                    content: vec![ContentBlock::Text { text: "go".into() }],
                },
            ]),
            tools: vec![
                ToolSpec {
                    name: "a".into(),
                    description: "da".into(),
                    input_schema: serde_json::json!({"type":"object"}),
                },
                ToolSpec {
                    name: "b".into(),
                    description: "db".into(),
                    input_schema: serde_json::json!({"type":"object"}),
                },
            ],
            max_tokens: 8192,
        };
        let v = build_request_body(&req, true, CacheTtl::FiveMinutes, None);
        // System becomes a single cached text block.
        let system = v["system"].as_array().unwrap();
        assert_eq!(system.len(), 1);
        assert_eq!(system[0]["type"], "text");
        assert_eq!(system[0]["cache_control"]["type"], "ephemeral");
        // Only the LAST tool carries the breakpoint.
        let tools = v["tools"].as_array().unwrap();
        assert!(tools[0].get("cache_control").is_none());
        assert_eq!(tools[1]["cache_control"]["type"], "ephemeral");
        // Only the last content block of the last message carries it.
        let messages = v["messages"].as_array().unwrap();
        assert!(messages[0]["content"][0].get("cache_control").is_none());
        assert!(messages[1]["content"][0].get("cache_control").is_none());
        assert_eq!(
            messages[2]["content"][0]["cache_control"]["type"],
            "ephemeral"
        );
        // The caller's request is untouched.
        assert_eq!(req.tools.len(), 2);
        // Caching off keeps the plain-string system and marker-free tools.
        let plain = build_request_body(&req, false, CacheTtl::FiveMinutes, None);
        assert_eq!(plain["system"], "sys");
        assert!(plain["tools"][1].get("cache_control").is_none());
    }

    /// The one-hour lifetime stamps every breakpoint with `ttl: "1h"`; the
    /// provider default must keep the field absent (the beta flag rides the
    /// request header, see [`AnthropicClient::stream`]).
    #[test]
    fn one_hour_ttl_marks_every_breakpoint() {
        let req = ChatRequest {
            model: "m1".into(),
            system: "sys".into(),
            messages: std::sync::Arc::new(vec![Message {
                role: Role::User,
                content: vec![ContentBlock::Text { text: "hi".into() }],
            }]),
            tools: vec![ToolSpec {
                name: "a".into(),
                description: "da".into(),
                input_schema: serde_json::json!({"type":"object"}),
            }],
            max_tokens: 8192,
        };
        assert_eq!(CacheTtl::FiveMinutes.wire(), None);
        assert_eq!(CacheTtl::OneHour.wire(), Some("1h"));
        let v = build_request_body(&req, true, CacheTtl::OneHour, None);
        assert_eq!(v["system"][0]["cache_control"]["ttl"], "1h");
        assert_eq!(v["tools"][0]["cache_control"]["ttl"], "1h");
        assert_eq!(v["messages"][0]["content"][0]["cache_control"]["ttl"], "1h");
        // Default lifetime: no ttl field at all.
        let v = build_request_body(&req, true, CacheTtl::FiveMinutes, None);
        assert!(v["system"][0]["cache_control"].get("ttl").is_none());
        assert!(v["tools"][0]["cache_control"].get("ttl").is_none());
        assert!(
            v["messages"][0]["content"][0]["cache_control"]
                .get("ttl")
                .is_none()
        );
    }

    /// The one-hour lifetime carries the historical beta header on the wire
    /// (the body-side ttl is pinned by [`one_hour_ttl_marks_every_breakpoint`]);
    /// the provider default sends no `anthropic-beta` at all.
    #[tokio::test]
    async fn one_hour_ttl_sends_beta_header_and_default_sends_none() {
        use std::io::Write;
        use std::net::TcpListener;
        use std::sync::mpsc;

        let req = ChatRequest {
            model: "m1".into(),
            system: "sys".into(),
            messages: std::sync::Arc::new(vec![Message {
                role: Role::User,
                content: vec![ContentBlock::Text { text: "hi".into() }],
            }]),
            tools: vec![],
            max_tokens: 8192,
        };
        // Local endpoint that records the request head; the response status
        // is irrelevant, only the head is under test.
        let capture_head = |listener: TcpListener| {
            let (tx, rx) = mpsc::channel::<String>();
            std::thread::spawn(move || {
                if let Ok((mut s, _)) = listener.accept() {
                    let head = read_http_request_head(&mut s);
                    let _ = tx.send(head);
                    let _ = s.write_all(
                        b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    );
                }
            });
            rx
        };

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        let head_rx = capture_head(listener);
        let client = AnthropicClient::new(url, "k".into()).with_cache_ttl(CacheTtl::OneHour);
        let _ = client.stream(req.clone()).await;
        let head = head_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        assert!(
            head.contains("anthropic-beta: extended-cache-ttl-2025-04-11"),
            "the one-hour lifetime should carry the historical beta flag: {head}"
        );

        // Default lifetime: no beta header at all.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        let head_rx = capture_head(listener);
        let client = AnthropicClient::new(url, "k".into());
        let _ = client.stream(req).await;
        let head = head_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        assert!(
            !head.contains("anthropic-beta"),
            "the five-minute default should send no beta header: {head}"
        );
    }

    /// Empty system + caching keeps the empty-string shape (some gateways
    /// reject `[]`); thinking is serialized with a clamped budget.
    #[test]
    fn thinking_and_empty_system_shapes() {
        let req = ChatRequest {
            model: "m1".into(),
            system: String::new(),
            messages: std::sync::Arc::new(vec![]),
            tools: vec![],
            max_tokens: 8192,
        };
        let v = build_request_body(&req, true, CacheTtl::FiveMinutes, Some(4096));
        assert_eq!(v["system"], "");
        assert_eq!(v["thinking"]["type"], "enabled");
        assert_eq!(v["thinking"]["budget_tokens"], 4096);

        // Budget below the 1024 minimum is clamped up; above max_tokens-1 clamped down.
        let v = build_request_body(&req, true, CacheTtl::FiveMinutes, Some(8));
        assert_eq!(v["thinking"]["budget_tokens"], 1024);
        let v = build_request_body(&req, true, CacheTtl::FiveMinutes, Some(u32::MAX));
        assert_eq!(v["thinking"]["budget_tokens"], 8191);

        // A max_tokens that cannot satisfy the minimum keeps thinking off.
        let tiny = ChatRequest {
            max_tokens: 1024,
            ..req.clone()
        };
        let v = build_request_body(&tiny, true, CacheTtl::FiveMinutes, Some(4096));
        assert!(v.get("thinking").is_none());
    }

    /// Adjacent same-role messages merge in the request body (third-party alternating-role endpoint compat):
    /// A trailing history tool_result (user) + appended instruction (user) is the routine product of compact/sampling pipelines.
    #[test]
    fn request_body_merges_adjacent_same_role_messages() {
        let req = ChatRequest {
            model: "m1".into(),
            system: "sys".into(),
            messages: std::sync::Arc::new(vec![
                Message {
                    role: Role::User,
                    content: vec![ContentBlock::Text {
                        text: "first".into(),
                    }],
                },
                Message {
                    role: Role::User,
                    content: vec![ContentBlock::Text {
                        text: "second".into(),
                    }],
                },
                Message {
                    role: Role::Assistant,
                    content: vec![ContentBlock::Text {
                        text: "reply".into(),
                    }],
                },
            ]),
            tools: vec![],
            max_tokens: 8,
        };
        let v = build_request_body(&req, false, CacheTtl::FiveMinutes, None);
        let messages = v["messages"].as_array().unwrap();
        assert_eq!(
            messages.len(),
            2,
            "adjacent user messages should merge into one"
        );
        assert_eq!(messages[0]["role"], "user");
        let text = messages[0]["content"][0]["text"].as_str().unwrap();
        assert_eq!(
            text, "first\nsecond",
            "adjacent Text blocks get a newline separator so they cannot glue together"
        );
        assert_eq!(messages[1]["role"], "assistant");
        // The input is untouched (merging only happens on the serialization side).
        assert_eq!(req.messages.len(), 3);
    }

    #[tokio::test]
    async fn stream_parses_recorded_sse() {
        let sse: &'static str = concat!(
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":10}}}\n\n",
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"OK\"}}\n\n",
            "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":3}}\n\n",
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
        );
        let events = collect_events_from_chunks(vec![sse]).await;
        let texts: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                crate::StreamEvent::TextDelta { text } => Some(text.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(texts, vec!["OK"]);
        assert!(events.iter().any(|e| matches!(e, crate::StreamEvent::MessageComplete { stop_reason, usage } if stop_reason == "end_turn" && usage.input_tokens == 10 && usage.output_tokens == 3)));
    }

    #[tokio::test]
    async fn stream_handles_split_frames() {
        // An SSE frame split across two TCP segments must not lose events
        let full: &'static str = "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"AB\"}}\n\n";
        let (a, b) = full.split_at(37);
        let events = collect_events_from_chunks(vec![a, b]).await;
        assert!(
            events
                .iter()
                .any(|e| matches!(e, crate::StreamEvent::TextDelta { text } if text == "AB"))
        );
    }

    #[tokio::test]
    async fn stream_handles_crlf_frames() {
        // Pure-CRLF frames plus mixed LF/CRLF streams must not lose events
        let sse: &'static str = concat!(
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"A\"}}\r\n\r\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"B\"}}\n\n",
        );
        let events = collect_events_from_chunks(vec![sse]).await;
        let texts: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                crate::StreamEvent::TextDelta { text } => Some(text.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(texts, vec!["A", "B"]);
    }

    #[tokio::test]
    async fn stream_joins_multi_line_data() {
        // Multiple data lines in one frame join with \n before reaching the parser; the joined text here is still valid JSON
        // (\n lands between JSON tokens), so it should parse into an event - this locks the "joining" behavior itself.
        let sse: &'static str = concat!(
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":\n",
            "data: {\"type\":\"text_delta\",\"text\":\"M\"}}\n\n",
        );
        let events = collect_events_from_chunks(vec![sse]).await;
        assert!(
            events
                .iter()
                .any(|e| matches!(e, crate::StreamEvent::TextDelta { text } if text == "M"))
        );
    }

    #[tokio::test]
    async fn stream_handles_crlf_separator_split_across_chunks() {
        // Feeding the 4-byte separator \r\n\r\n in 3-byte pieces guarantees a cross-chunk split:
        // locks the correctness of the resume-scan step-back (scanned - 3); no event may be lost.
        let frame: &'static str = "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"X\"}}\r\n\r\n";
        let byte_stream = futures::stream::iter(
            frame
                .as_bytes()
                .chunks(3)
                .map(|c| Ok::<_, LlmError>(bytes::Bytes::copy_from_slice(c)))
                .collect::<Vec<_>>(),
        );
        let events: Vec<_> = decode_event_stream(byte_stream, MAX_SSE_BUF)
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .filter_map(std::result::Result::ok)
            .collect();
        assert!(
            events
                .iter()
                .any(|e| matches!(e, crate::StreamEvent::TextDelta { text } if text == "X"))
        );
    }

    #[tokio::test]
    async fn stream_errors_and_terminates_when_buffer_exceeds_cap() {
        // Over-cap buffer: yield Err and terminate the stream - the third chunk is never consumed,
        // if it only errored without terminating, the results would carry extra error items from later chunks.
        let chunk = || Ok::<_, LlmError>(bytes::Bytes::from_static(b"data: no-boundary-here\n"));
        let byte_stream = futures::stream::iter([chunk(), chunk(), chunk()]);
        let results: Vec<_> = decode_event_stream(byte_stream, 32).collect().await;
        assert_eq!(
            results.len(),
            1,
            "the stream should terminate right after exceeding the cap: {results:?}"
        );
        assert!(
            matches!(&results[0], Err(LlmError::Sse(msg)) if msg.contains("cap")),
            "should report the buffer-over-cap error: {:?}",
            results[0]
        );
    }

    /// Regression test (review batch A2): no redirects, so `x-api-key` cannot leak to a cross-origin target.
    ///
    /// Service A answers POST with a 301 to B; assert the client errors directly (http_301) instead of following,
    /// and B never receives a request (reqwest follows redirects by default and would carry `x-api-key`, verified with a PoC).
    #[tokio::test]
    async fn redirect_is_not_followed_and_api_key_not_leaked() {
        use std::io::Write;
        use std::net::TcpListener;
        use std::sync::mpsc;

        // B: the redirect target; on receiving a request it sends the full request head back to the main thread.
        let (b_tx, b_rx) = mpsc::channel::<String>();
        let listener_b = TcpListener::bind("127.0.0.1:0").unwrap();
        let port_b = listener_b.local_addr().unwrap().port();
        std::thread::spawn(move || {
            if let Ok((mut s, _)) = listener_b.accept() {
                let head = read_http_request_head(&mut s);
                let _ = b_tx.send(head);
            }
        });

        // A: the entry service; records the request head (proving the key did reach A) then replies 301 to B.
        let (a_tx, a_rx) = mpsc::channel::<String>();
        let listener_a = TcpListener::bind("127.0.0.1:0").unwrap();
        let port_a = listener_a.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let (mut s, _) = listener_a.accept().unwrap();
            let head = read_http_request_head(&mut s);
            let _ = a_tx.send(head);
            let resp = format!(
                "HTTP/1.1 301 Moved Permanently\r\nLocation: http://127.0.0.1:{port_b}/v1/messages\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            );
            s.write_all(resp.as_bytes()).unwrap();
        });

        let client =
            AnthropicClient::new(format!("http://127.0.0.1:{port_a}"), "sk-secret-key".into());
        let req = ChatRequest {
            model: "m".into(),
            system: "s".into(),
            messages: std::sync::Arc::new(vec![]),
            tools: vec![],
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
        // Precondition: A did receive the request carrying the key (otherwise this test is meaningless).
        let head_a = a_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        assert!(
            head_a.contains("x-api-key: sk-secret-key"),
            "A should have received the request head carrying the key: {head_a}"
        );
        // B never receives a request: no connection on loopback within 500ms counts as not followed.
        assert!(
            b_rx.recv_timeout(std::time::Duration::from_millis(500))
                .is_err(),
            "the redirect was followed; the api key leaked to B"
        );
    }

    /// Test helper: reads the HTTP request head (up to `\r\n\r\n`) and then reads the full
    /// body per Content-Length - responding/closing before reading the body risks an RST while the client writes its body.
    fn read_http_request_head(s: &mut std::net::TcpStream) -> String {
        use std::io::Read;
        let _ = s.set_read_timeout(Some(std::time::Duration::from_secs(5)));
        let mut buf: Vec<u8> = Vec::new();
        let mut tmp = [0u8; 4096];
        let head_len = loop {
            match s.read(&mut tmp) {
                Ok(0) | Err(_) => return String::from_utf8_lossy(&buf).into_owned(),
                Ok(n) => {
                    buf.extend_from_slice(&tmp[..n]);
                    if let Some(i) = find_subsequence(&buf, b"\r\n\r\n") {
                        break i + 4;
                    }
                }
            }
        };
        let head = String::from_utf8_lossy(&buf[..head_len]).into_owned();
        let content_length: usize = head
            .lines()
            .find_map(|l| {
                l.to_ascii_lowercase()
                    .strip_prefix("content-length:")
                    .and_then(|v| v.trim().parse().ok())
            })
            .unwrap_or(0);
        while buf.len() < head_len + content_length {
            match s.read(&mut tmp) {
                Ok(0) | Err(_) => break,
                Ok(n) => buf.extend_from_slice(&tmp[..n]),
            }
        }
        head
    }
}

#[cfg(test)]
mod image_translation_tests {
    use super::*;
    use crate::{ContentBlock, Message, Role};

    fn tiny_png_base64() -> String {
        use base64::Engine as _;
        base64::engine::general_purpose::STANDARD.encode([0x89, b'P', b'N', b'G', 0x0D, 0x0A])
    }

    #[test]
    fn image_maps_to_anthropic_image_block() {
        let b64 = tiny_png_base64();
        let out = translate_messages(
            &[Message {
                role: Role::User,
                content: vec![ContentBlock::Image {
                    id: Some("a".to_string()),
                    mime: "image/png".to_string(),
                    base64: b64.clone(),
                }],
            }],
            "claude-sonnet-4-5",
        );
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["role"], "user");
        let block = &out[0]["content"][0];
        assert_eq!(block["type"], "image");
        assert_eq!(block["source"]["type"], "base64");
        assert_eq!(block["source"]["media_type"], "image/png");
        assert_eq!(block["source"]["data"], serde_json::Value::String(b64));
    }

    #[test]
    fn invalid_image_becomes_text_naming_constraint() {
        let out = translate_messages(
            &[Message {
                role: Role::User,
                content: vec![ContentBlock::Image {
                    id: None,
                    mime: "image/tiff".to_string(),
                    base64: "aaaa".to_string(),
                }],
            }],
            "claude-sonnet-4-5",
        );
        let block = &out[0]["content"][0];
        assert_eq!(block["type"], "text");
        assert!(
            block["text"]
                .as_str()
                .unwrap()
                .contains("unsupported image mime")
        );
    }

    #[test]
    fn text_tool_shapes_unchanged() {
        let out = translate_messages(
            &[Message {
                role: Role::Assistant,
                content: vec![ContentBlock::ToolUse {
                    id: "t1".to_string(),
                    name: "read_file".to_string(),
                    input: serde_json::json!({"path": "a"}),
                }],
            }],
            "claude-sonnet-4-5",
        );
        assert_eq!(out[0]["content"][0]["type"], "tool_use");
    }

    /// A signed thinking block goes back verbatim on a tool-call turn (the
    /// API rejects the turn if its thinking is gone); an unsigned one is
    /// preserved for Anthropic-compatible models, which stream no signature
    /// yet still require the block, and dropped for Claude models, which
    /// reject unsigned blocks outright.
    #[test]
    fn thinking_blocks_round_trip_per_model() {
        let history = |signature: Option<&str>| {
            vec![Message {
                role: Role::Assistant,
                content: vec![
                    ContentBlock::Thinking {
                        text: "weighing options".to_string(),
                        signature: signature.map(str::to_string),
                    },
                    ContentBlock::ToolUse {
                        id: "t1".to_string(),
                        name: "read_file".to_string(),
                        input: serde_json::json!({"path": "a"}),
                    },
                ],
            }]
        };

        let signed = translate_messages(&history(Some("sig-1")), "claude-sonnet-4-5");
        assert_eq!(signed[0]["content"][0]["type"], "thinking");
        assert_eq!(signed[0]["content"][0]["thinking"], "weighing options");
        assert_eq!(signed[0]["content"][0]["signature"], "sig-1");

        // Claude + unsigned: the block cannot be sent, so it is dropped and
        // the tool call still travels.
        let claude_unsigned = translate_messages(&history(None), "claude-opus-4-1");
        assert_eq!(claude_unsigned[0]["content"].as_array().unwrap().len(), 1);
        assert_eq!(claude_unsigned[0]["content"][0]["type"], "tool_use");

        // Anthropic-compatible model + unsigned: preserved without a signature.
        let compat_unsigned = translate_messages(&history(None), "kimi-k2-thinking");
        assert_eq!(compat_unsigned[0]["content"][0]["type"], "thinking");
        assert_eq!(
            compat_unsigned[0]["content"][0]["thinking"],
            "weighing options"
        );
        assert!(compat_unsigned[0]["content"][0].get("signature").is_none());
    }

    /// A turn whose only blocks were dropped (unsigned thinking bound for a
    /// Claude model) must not render as an empty `content` array: the API
    /// answers 400. A placeholder text block keeps the turn well-formed.
    #[test]
    fn fully_filtered_message_keeps_a_content_block() {
        let history = vec![Message {
            role: Role::Assistant,
            content: vec![ContentBlock::Thinking {
                text: "only thinking".to_string(),
                signature: None,
            }],
        }];
        let messages = translate_messages(&history, "claude-opus-4-1");
        let content = messages[0]["content"].as_array().unwrap();
        assert!(!content.is_empty(), "empty content arrays get a 400");
        assert_eq!(content[0]["type"], "text");
    }

    /// A clean EOF that never delivers the `message_delta` completion is a
    /// torn stream: the wrapper ends it with an error, not silent success.
    #[tokio::test]
    async fn clean_eof_without_completion_is_an_error() {
        let byte_stream = futures::stream::iter(vec![
            Ok::<_, LlmError>(bytes::Bytes::from_static(b"event: message_start\n\n")),
            Ok(bytes::Bytes::from_static(b"event: message_stop\n\n")),
        ]);
        let events: Vec<_> = decode_event_stream(byte_stream, MAX_SSE_BUF)
            .collect()
            .await;
        let last = events.last().expect("the wrapper must end the stream");
        assert!(
            matches!(last, Err(LlmError::Sse(msg)) if msg.contains("message_delta")),
            "EOF without a completion must surface as an error: {events:?}"
        );
        assert!(
            events[..events.len() - 1].iter().all(|r| r.is_ok()),
            "no parse error precedes the EOF marker: {events:?}"
        );
    }
}
