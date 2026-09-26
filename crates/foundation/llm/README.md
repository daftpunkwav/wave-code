# crates/foundation/llm/ — unified multi-provider streaming LLM abstraction

English | [中文](README.zh.md)

| Item | Role |
|---|---|
| `src/lib.rs` | Shared vocabulary: `Message`/`ContentBlock`/`ToolSpec`/`Role`/`Usage`, `StreamEvent`, `ChatRequest`, the `ChatModel` trait (+ `EventStream`), the `LlmError` taxonomy, image validation (`validate_image`, `IMAGE_MAX_BYTES`, `IMAGE_ALLOWED_MIMES`), `normalize_tool_input` |
| `src/anthropic.rs` | `AnthropicClient` — Anthropic Messages SSE streaming (`POST {base_url}/v1/messages`) with prompt-cache breakpoints and an extended-thinking budget |
| `src/openai.rs` | `OpenAIClient` — Chat Completions SSE streaming (`POST {base_url}/chat/completions`) with tool-call fragment assembly; `ModelCapabilities::for_model` approximate per-model limits |
| `src/responses.rs` | `ResponsesClient` — OpenAI Responses API streaming (`POST {base_url}/responses`, always `store: false`, flat `input` items, named stream events) |
| `src/retry.rs` | `RetryPolicy` + `RetryingModel` — bounded retries with exponential backoff for transient failures only; `Retry-After` honored; auth, quota, and `PromptTooLong` fail fast |
| `src/sse.rs` | Provider-agnostic SSE framing and read-stall detection (`stall_guard`, crate-private) plus the Anthropic-specific `SseParser` translating frames into `StreamEvent`s |

One `ChatModel` trait covers three wire dialects, so upper layers
sample without knowing the provider. The crate depends on external
crates only — per its module contract it must not depend on runtime,
capabilities, config, or frontends; config values always win over the
`ModelCapabilities` table, whose unknown names return `None` so callers
fall back to session defaults. `Usage::input_tokens` is normalized to
the full request size on every wire (Anthropic reports the uncached
remainder; the parser adds cache counters), keeping context accounting
wire-independent. Composition roots build `Arc<dyn ChatModel>` chains
(retry wrappers, fallback routing) — a blanket impl forwards `ChatModel`
through `Arc` so layers nest without re-wrapping.
