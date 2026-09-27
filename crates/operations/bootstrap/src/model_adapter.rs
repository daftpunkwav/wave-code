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
    ModelGateway, SampleBlock, SampleDelta, SampleError, SampleRequest, SampleResponse, ToolRef,
};
use state_store::{Block, Role as HistoryRole};
use wavecode_llm::{
    ChatModel, ChatRequest, ContentBlock, EventStream, LlmError, Message, Role, StreamEvent,
    ToolSpec,
};

/// Samples a legacy chat model through the gateway seam.
pub struct ModelAdapter {
    model: Arc<dyn ChatModel>,
    /// Wire model name, interior-mutable so `/model` switches it
    /// mid-session (same provider/endpoint only: base URL and credentials
    /// are baked into the wrapped client).
    model_name: std::sync::RwLock<String>,
    max_tokens: u32,
    registry: Arc<wavecode_tools::Registry>,
    /// Fallback window for per-name resolution through the capability
    /// table, when the provider leaves limits to the table (the assembly
    /// condition mirrors this). `None` when the window is fixed by
    /// explicit provider config: a name switch cannot move it.
    per_model_window: Option<u64>,
}

impl ModelAdapter {
    /// Wrap a shared model; tool schemas come from the registry so the
    /// model always sees the same tools the executor can run. The context
    /// window is fixed (`None` as `per_model_window`); per-name windows
    /// ride [`ModelAdapter::with_per_model_window`].
    pub fn new(
        model: Arc<dyn ChatModel>,
        model_name: String,
        max_tokens: u32,
        registry: Arc<wavecode_tools::Registry>,
    ) -> Self {
        Self {
            model,
            model_name: std::sync::RwLock::new(model_name),
            max_tokens,
            registry,
            per_model_window: None,
        }
    }

    /// Declare that the context window resolves per model name through the
    /// capability table, falling back to `fallback` for unknown names.
    /// This is what lets the loop's budget gate follow a `/model` switch
    /// onto a smaller-window model instead of gating with a stale window.
    pub fn with_per_model_window(mut self, fallback: u64) -> Self {
        self.per_model_window = Some(fallback);
        self
    }

    /// Convert seam messages block by block; assistant entries with no
    /// content are dropped because providers reject empty assistant
    /// messages, while entries carrying tool blocks always count as
    /// non-empty.
    fn messages(request: &SampleRequest) -> Vec<Message> {
        request
            .messages
            .iter()
            .filter(|entry| {
                entry.role != HistoryRole::Assistant
                    || entry.blocks.iter().any(|block| match block {
                        Block::Text(text) => !text.is_empty(),
                        _ => true,
                    })
            })
            .map(|entry| Message {
                role: if entry.role == HistoryRole::Assistant {
                    Role::Assistant
                } else {
                    Role::User
                },
                content: entry.blocks.iter().map(to_content_block).collect(),
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

/// Map one history block onto the provider block shape.
pub(crate) fn to_content_block(block: &Block) -> ContentBlock {
    match block {
        Block::Text(text) => ContentBlock::Text { text: text.clone() },
        Block::ToolUse {
            call_id,
            name,
            input,
        } => ContentBlock::ToolUse {
            id: call_id.clone(),
            name: name.clone(),
            input: input.clone(),
        },
        Block::ToolResult {
            call_id,
            content,
            is_error,
            ..
        } => ContentBlock::ToolResult {
            tool_use_id: call_id.clone(),
            content: content.clone(),
            is_error: *is_error,
        },
        Block::Image { id, mime, base64 } => ContentBlock::Image {
            id: id.clone(),
            mime: mime.clone(),
            base64: base64.clone(),
        },
        Block::Thinking { text, signature } => ContentBlock::Thinking {
            text: text.clone(),
            signature: signature.clone(),
        },
    }
}

/// Build one provider client from resolved config.
///
/// The primary and every fallback go through this single point so wiring
/// stays identical across the chain. `reasoning_effort` rides the
/// OpenAI-compatible client best-effort (`effort_override` from a saved
/// picker default wins over the provider config); Anthropic has no such
/// wire param and ignores it. Keys arrive already resolved per provider
/// and are never shared between the clients built here.
pub fn build_chat_model(
    provider: &wavecode_config::ProviderConfig,
    api_key: String,
    model_name: &str,
    effort_override: Option<&str>,
) -> Arc<dyn ChatModel> {
    let model: Arc<dyn ChatModel> = match provider.kind {
        wavecode_config::ProviderKind::OpenAiCompatible => {
            let client = wavecode_llm::OpenAIClient::new(
                provider.base_url.clone(),
                api_key,
                model_name.to_string(),
            );
            match effort_override.or(provider.reasoning_effort.as_deref()) {
                Some(effort) => Arc::new(client.with_reasoning_effort(effort.to_string())),
                None => Arc::new(client),
            }
        }
        wavecode_config::ProviderKind::OpenAiResponses => {
            let client = wavecode_llm::ResponsesClient::new(
                provider.base_url.clone(),
                api_key,
                model_name.to_string(),
            );
            // Same best-effort effort contract as the chat client; the
            // Responses wire nests it under `reasoning.effort`.
            match effort_override.or(provider.reasoning_effort.as_deref()) {
                Some(effort) => Arc::new(client.with_reasoning_effort(effort.to_string())),
                None => Arc::new(client),
            }
        }
        wavecode_config::ProviderKind::Anthropic => {
            let client = wavecode_llm::AnthropicClient::new(provider.base_url.clone(), api_key);
            // Prompt caching defaults ON (unset = enabled); thinking only
            // when a budget is configured. Both are per-provider choices.
            let client = match provider.prompt_caching {
                Some(false) => client.with_prompt_caching(false),
                _ => client,
            };
            // Cache-entry lifetime: "1h" survives hour-scale quiet stretches
            // that would expire the default five-minute prefix (the trade is
            // 2x-base writes instead of 1.25x); unknown values warn and keep
            // the default rather than guessing.
            let client = match provider.prompt_cache_ttl.as_deref() {
                Some("1h") => client.with_cache_ttl(wavecode_llm::CacheTtl::OneHour),
                Some(other) if other != "5m" => {
                    tracing::warn!(
                        "prompt_cache_ttl {other:?} is not \"5m\" or \"1h\"; keeping the 5m default"
                    );
                    client
                }
                _ => client,
            };
            match provider.thinking_budget_tokens {
                Some(budget) => Arc::new(client.with_thinking_budget(budget)),
                None => Arc::new(client),
            }
        }
    };
    // Optional local throttling keeps a retry loop from hammering the
    // endpoint; provider-side 429 handling stays the second line.
    match provider.rpm_limit {
        Some(rpm) => Arc::new(crate::rate_limit::RateLimitedModel::new(model, rpm)),
        None => model,
    }
}

/// Ordered provider failover over [`ChatModel`] request establishment.
///
/// Tries each model in order (primary first, then fallbacks); only
/// transport/server errors advance to the next provider. Auth errors fail
/// fast on the spot: credentials are encapsulated inside each provider
/// client and are never copied or retried across providers. Other
/// non-retryable errors (4xx, [`LlmError::PromptTooLong`]) also return
/// immediately so compaction triggers stay intact.
///
/// Failover covers request establishment ([`ChatModel::stream`]) only: a
/// stream that fails mid-flight surfaces its item error unchanged instead
/// of resuming on another provider, which could duplicate tool side
/// effects and already-emitted deltas.
pub struct FallbackModel {
    models: Vec<Arc<dyn ChatModel>>,
}

impl FallbackModel {
    /// Wrap the chain in try order; must be non-empty (the primary alone
    /// when no fallbacks resolve).
    pub fn new(models: Vec<Arc<dyn ChatModel>>) -> Self {
        Self { models }
    }

    /// Number of providers in the chain (primary + resolved fallbacks).
    pub fn len(&self) -> usize {
        self.models.len()
    }

    /// True when the chain holds no provider.
    pub fn is_empty(&self) -> bool {
        self.models.is_empty()
    }
}

#[async_trait::async_trait]
impl ChatModel for FallbackModel {
    async fn stream(&self, req: ChatRequest) -> wavecode_llm::Result<EventStream> {
        use wavecode_llm::retry::RetryPolicy;
        let policy = RetryPolicy::default();
        let mut last_error: Option<LlmError> = None;
        for model in &self.models {
            match model.stream(req.clone()).await {
                Ok(stream) => return Ok(stream),
                Err(error) if policy.is_retryable(&error) => {
                    last_error = Some(error);
                }
                Err(error) => return Err(error),
            }
        }
        Err(last_error.unwrap_or_else(|| LlmError::Http("no providers configured".to_string())))
    }

    fn set_thinking(&self, effort: &str) -> bool {
        // The level applies to the whole chain so a failover mid-turn
        // keeps sampling at the chosen effort.
        let mut applied = false;
        for model in &self.models {
            applied |= model.set_thinking(effort);
        }
        applied
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
        self.sample_streaming(request, &|_| {}).await
    }

    async fn sample_streaming(
        &self,
        request: SampleRequest,
        on_delta: &(dyn Fn(SampleDelta) + Send + Sync),
    ) -> Result<SampleResponse, SampleError> {
        let req = ChatRequest {
            model: self
                .model_name
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .clone(),
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
        let mut cache_read_tokens: u64 = 0;
        let mut cache_creation_tokens: u64 = 0;
        let mut truncated = false;
        while let Some(event) = stream.next().await {
            let event = event.map_err(|e| map_error(&e))?;
            match event {
                StreamEvent::TextDelta { text: delta } => {
                    text.push_str(&delta);
                    on_delta(SampleDelta::Text(delta));
                }
                StreamEvent::ThinkingDelta { text } => {
                    // Forwarded for live display; the whole block arrives
                    // again in `ThinkingComplete`, which is what history keeps
                    // (providers that require the block back — Anthropic on a
                    // tool-call turn — read it from there).
                    on_delta(SampleDelta::Thinking(text));
                }
                StreamEvent::SignatureDelta { .. } => {
                    // Signatures only matter for the preserved block; the
                    // parser accumulates them and reports them with
                    // `ThinkingComplete`.
                }
                StreamEvent::ThinkingComplete {
                    text: thinking,
                    signature,
                } => {
                    if !thinking.is_empty() {
                        flush_text(&mut text, &mut blocks);
                        blocks.push(SampleBlock::Thinking {
                            text: thinking,
                            signature,
                        });
                    }
                }
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
                    cache_read_tokens = usage.cache_read_tokens;
                    cache_creation_tokens = usage.cache_creation_tokens;
                }
            }
        }
        // A clean stream end while a tool call was still open (no BlockEnd,
        // no MessageComplete) is a torn response, not a completed one:
        // silently dropping the pending call would surface as an empty
        // "completed" turn, so fail the sample into the existing error path.
        if pending.is_some() {
            return Err(SampleError::Transport(
                "stream ended mid-tool-call without a completion event".to_string(),
            ));
        }
        Ok(SampleResponse {
            blocks,
            input_tokens,
            output_tokens,
            cache_read_tokens,
            cache_creation_tokens,
            truncated,
        })
    }

    fn set_model(&self, name: &str) -> bool {
        let name = name.trim();
        if name.is_empty() {
            return false;
        }
        *self.model_name.write().unwrap_or_else(|e| e.into_inner()) = name.to_string();
        true
    }

    fn context_window(&self) -> Option<u64> {
        let fallback = self.per_model_window?;
        let name = self
            .model_name
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        Some(
            wavecode_llm::ModelCapabilities::resolve_or(&name, fallback, self.max_tokens)
                .context_window,
        )
    }

    fn set_thinking(&self, effort: &str) -> bool {
        let effort = effort.trim();
        if effort.is_empty() {
            return false;
        }
        self.model.set_thinking(effort)
    }
}

/// Parse accumulated tool input into the object form tools and provider
/// wires require. An empty buffer means "no arguments"; a non-object or
/// unparseable payload is coerced via [`wavecode_llm::normalize_tool_input`]
/// so tool validation fails honestly on the coerced shape instead of the
/// block poisoning session history with a wire-illegal `tool_use.input`
/// (a permanent http_400 on replay).
fn parse_tool_input(buf: &str) -> serde_json::Value {
    if buf.trim().is_empty() {
        return serde_json::Value::Object(serde_json::Map::new());
    }
    match serde_json::from_str(buf) {
        Ok(value @ serde_json::Value::Object(_)) => value,
        Ok(other) => wavecode_llm::normalize_tool_input(other),
        Err(_) => wavecode_llm::normalize_tool_input(serde_json::Value::String(buf.to_string())),
    }
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
    use state_store::{Block, HistoryEntry};
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
            messages: vec![HistoryEntry {
                role: state_store::Role::User,
                blocks: vec![Block::Text("hello".to_string())],
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
                    ..Usage::default()
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

    #[test]
    fn parse_tool_input_keeps_every_shape_wire_legal() {
        let empty = serde_json::Map::new();
        // No deltas at all means "no arguments", never a null input.
        assert_eq!(parse_tool_input(""), serde_json::json!(empty));
        assert_eq!(parse_tool_input("  "), serde_json::json!(empty));
        assert_eq!(parse_tool_input("null"), serde_json::json!(empty));
        // Well-formed objects pass through untouched.
        assert_eq!(
            parse_tool_input(r#"{"path":"a.txt"}"#),
            serde_json::json!({"path": "a.txt"})
        );
        // Valid JSON of a non-object type is wrapped, not replayed bare.
        assert_eq!(
            parse_tool_input(r#""bare string""#),
            serde_json::json!({"_raw": "bare string"})
        );
        assert_eq!(
            parse_tool_input("[1,2]"),
            serde_json::json!({"_raw": [1, 2]})
        );
        // Unparseable fragments keep their text for honest validation.
        assert_eq!(
            parse_tool_input(r#"{"path": "#),
            serde_json::json!({"_raw": r#"{"path": "#})
        );
    }

    /// A model that streams a non-object tool input must not poison the
    /// history: the stored block stays an object so replaying it never
    /// fails the whole request with a 400 on tool_use.input.
    #[tokio::test]
    async fn non_object_tool_input_is_wrapped_into_an_object() {
        let response = adapter(vec![
            StreamEvent::ToolUseBegin {
                id: "c1".to_string(),
                name: "write".to_string(),
            },
            StreamEvent::ToolUseInputDelta {
                partial_json: r#""just a string""#.to_string(),
            },
            StreamEvent::BlockEnd,
            StreamEvent::MessageComplete {
                stop_reason: "end_turn".to_string(),
                usage: Usage {
                    input_tokens: 1,
                    output_tokens: 2,
                    ..Usage::default()
                },
            },
        ])
        .sample(request())
        .await
        .unwrap();
        assert_eq!(
            response.blocks,
            vec![SampleBlock::ToolUse {
                call_id: "c1".to_string(),
                name: "write".to_string(),
                input: serde_json::json!({"_raw": "just a string"}),
            }]
        );
    }

    /// A stream that ends cleanly mid-tool-call (no BlockEnd, no
    /// MessageComplete) must fail the sample instead of silently returning
    /// an empty "completed" response with the model's tool call dropped.
    #[tokio::test]
    async fn stream_ending_mid_tool_call_is_an_error() {
        let outcome = adapter(vec![StreamEvent::ToolUseBegin {
            id: "c1".to_string(),
            name: "shell".to_string(),
        }])
        .sample(request())
        .await;
        assert!(
            matches!(outcome, Err(SampleError::Transport(ref message)) if message.contains("mid-tool-call")),
            "{outcome:?}"
        );
    }

    #[tokio::test]
    async fn streaming_forwards_deltas_in_order() {
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen_clone = seen.clone();
        let response = adapter(vec![
            StreamEvent::TextDelta {
                text: "a".to_string(),
            },
            StreamEvent::TextDelta {
                text: "b".to_string(),
            },
            StreamEvent::MessageComplete {
                stop_reason: "end_turn".to_string(),
                usage: Usage {
                    input_tokens: 1,
                    output_tokens: 1,
                    ..Usage::default()
                },
            },
        ])
        .sample_streaming(request(), &|delta| {
            // Thinking deltas (if any) are display-only and not asserted here.
            if let SampleDelta::Text(text) = delta {
                seen_clone.lock().unwrap().push(text);
            }
        })
        .await
        .unwrap();
        assert_eq!(
            *seen.lock().unwrap(),
            vec!["a".to_string(), "b".to_string()]
        );
        assert_eq!(response.blocks, vec![SampleBlock::Text("ab".to_string())]);
    }

    #[tokio::test]
    async fn empty_assistant_entries_are_dropped_from_requests() {
        let req = SampleRequest {
            system: String::new(),
            messages: vec![
                HistoryEntry {
                    role: state_store::Role::Assistant,
                    blocks: vec![Block::Text(String::new())],
                },
                HistoryEntry {
                    role: state_store::Role::User,
                    blocks: vec![Block::Text(String::new())],
                },
                // Tool blocks make an assistant entry non-empty even
                // without text.
                HistoryEntry {
                    role: state_store::Role::Assistant,
                    blocks: vec![Block::ToolUse {
                        call_id: "c1".to_string(),
                        name: "shell".to_string(),
                        input: serde_json::json!({"command": "ls"}),
                    }],
                },
            ],
            tools: vec![],
        };
        let messages = __test_messages(&req);
        // Empty assistant entries are dropped; user entries and tool
        // entries are kept.
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].role, Role::User);
        assert_eq!(messages[1].role, Role::Assistant);
    }

    #[test]
    fn block_entries_translate_to_provider_blocks() {
        let req = SampleRequest {
            system: String::new(),
            messages: vec![
                HistoryEntry {
                    role: state_store::Role::Assistant,
                    blocks: vec![Block::ToolUse {
                        call_id: "c1".to_string(),
                        name: "shell".to_string(),
                        input: serde_json::json!({"command": "ls"}),
                    }],
                },
                HistoryEntry {
                    role: state_store::Role::User,
                    blocks: vec![Block::ToolResult {
                        call_id: "c1".to_string(),
                        content: "a.rs".to_string(),
                        is_error: false,
                        produced_at: None,
                    }],
                },
            ],
            tools: vec![],
        };
        let messages = __test_messages(&req);
        assert_eq!(
            messages[0].content[0],
            ContentBlock::ToolUse {
                id: "c1".to_string(),
                name: "shell".to_string(),
                input: serde_json::json!({"command": "ls"}),
            }
        );
        assert_eq!(
            messages[1].content[0],
            ContentBlock::ToolResult {
                tool_use_id: "c1".to_string(),
                content: "a.rs".to_string(),
                is_error: false,
            }
        );
    }

    /// Scripted ChatModel for failover tests: fails once when armed, else
    /// answers with fixed text; counts establishment attempts.
    struct FailoverScript {
        error: Option<LlmError>,
        attempts: std::sync::atomic::AtomicUsize,
    }

    impl FailoverScript {
        fn failing(error: LlmError) -> Self {
            Self {
                error: Some(error),
                attempts: std::sync::atomic::AtomicUsize::new(0),
            }
        }

        fn answering() -> Self {
            Self {
                error: None,
                attempts: std::sync::atomic::AtomicUsize::new(0),
            }
        }

        fn attempts(&self) -> usize {
            self.attempts.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    #[async_trait::async_trait]
    impl ChatModel for FailoverScript {
        async fn stream(&self, _req: ChatRequest) -> wavecode_llm::Result<EventStream> {
            self.attempts
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            match &self.error {
                Some(LlmError::Http(message)) => Err(LlmError::Http(message.clone())),
                Some(LlmError::Api { kind, message }) => Err(LlmError::Api {
                    kind: kind.clone(),
                    message: message.clone(),
                }),
                Some(LlmError::PromptTooLong { message }) => Err(LlmError::PromptTooLong {
                    message: message.clone(),
                }),
                Some(other) => Err(LlmError::Http(other.to_string())),
                None => Ok(Box::pin(futures::stream::iter(vec![
                    Ok(StreamEvent::TextDelta {
                        text: "fallback-answer".to_string(),
                    }),
                    Ok(StreamEvent::MessageComplete {
                        stop_reason: "end_turn".to_string(),
                        usage: Usage {
                            input_tokens: 1,
                            output_tokens: 1,
                            ..Usage::default()
                        },
                    }),
                ]))),
            }
        }
    }

    fn transport() -> LlmError {
        LlmError::Http("connection reset".to_string())
    }

    fn auth_denied() -> LlmError {
        LlmError::Api {
            kind: "http_401".to_string(),
            message: "invalid api key".to_string(),
        }
    }

    async fn collect_text(model: &FallbackModel) -> String {
        use futures::StreamExt;
        let req = ChatRequest {
            model: "test-model".to_string(),
            system: String::new(),
            messages: Arc::new(Vec::new()),
            tools: Vec::new(),
            max_tokens: 10,
        };
        let mut stream = model.stream(req).await.unwrap();
        let mut text = String::new();
        while let Some(event) = stream.next().await {
            if let StreamEvent::TextDelta { text: delta } = event.unwrap() {
                text.push_str(&delta);
            }
        }
        text
    }

    #[tokio::test]
    async fn fallback_tries_providers_in_order() {
        let primary = Arc::new(FailoverScript::failing(transport()));
        let fallback = Arc::new(FailoverScript::answering());
        let chain = FallbackModel::new(vec![primary.clone(), fallback.clone()]);
        assert_eq!(chain.len(), 2);
        assert_eq!(collect_text(&chain).await, "fallback-answer");
        assert_eq!(primary.attempts(), 1);
        assert_eq!(fallback.attempts(), 1);
    }

    #[tokio::test]
    async fn auth_errors_fail_fast_without_touching_fallbacks() {
        let primary = Arc::new(FailoverScript::failing(auth_denied()));
        let fallback = Arc::new(FailoverScript::answering());
        let chain = FallbackModel::new(vec![primary.clone(), fallback.clone()]);
        let req = ChatRequest {
            model: "test-model".to_string(),
            system: String::new(),
            messages: Arc::new(Vec::new()),
            tools: Vec::new(),
            max_tokens: 10,
        };
        assert!(chain.stream(req).await.is_err());
        assert_eq!(primary.attempts(), 1);
        // The fallback key is never exercised after an auth failure.
        assert_eq!(fallback.attempts(), 0);
    }

    #[tokio::test]
    async fn overlong_prompts_fail_fast_without_touching_fallbacks() {
        let primary = Arc::new(FailoverScript::failing(LlmError::PromptTooLong {
            message: "too long".to_string(),
        }));
        let fallback = Arc::new(FailoverScript::answering());
        let chain = FallbackModel::new(vec![primary.clone(), fallback.clone()]);
        let req = ChatRequest {
            model: "test-model".to_string(),
            system: String::new(),
            messages: Arc::new(Vec::new()),
            tools: Vec::new(),
            max_tokens: 10,
        };
        assert!(matches!(
            chain.stream(req).await,
            Err(LlmError::PromptTooLong { .. })
        ));
        assert_eq!(fallback.attempts(), 0);
    }
}
