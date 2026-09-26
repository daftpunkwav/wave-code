# crates/foundation/llm/ — 统一的多 provider 流式 LLM 抽象层

[English](README.md) | 中文

| 条目 | 职责 |
|---|---|
| `src/lib.rs` | 共享词汇：`Message`/`ContentBlock`/`ToolSpec`/`Role`/`Usage`、`StreamEvent`、`ChatRequest`、`ChatModel` trait（含 `EventStream`）、`LlmError` 错误分类、图像校验（`validate_image`、`IMAGE_MAX_BYTES`、`IMAGE_ALLOWED_MIMES`）、`normalize_tool_input` |
| `src/anthropic.rs` | `AnthropicClient`——Anthropic Messages SSE 流式客户端（`POST {base_url}/v1/messages`），注入 prompt-cache 断点与 extended-thinking 预算 |
| `src/openai.rs` | `OpenAIClient`——Chat Completions SSE 流式客户端（`POST {base_url}/chat/completions`），按索引组装 tool-call 分片；`ModelCapabilities::for_model` 提供近似的按模型限制 |
| `src/responses.rs` | `ResponsesClient`——OpenAI Responses API 流式客户端（`POST {base_url}/responses`，始终 `store: false`，扁平 `input` 条目，具名流事件） |
| `src/retry.rs` | `RetryPolicy` + `RetryingModel`——仅对瞬时失败做有界指数退避重试；遵循 `Retry-After`；认证、配额与 `PromptTooLong` 快速失败 |
| `src/sse.rs` | 与 provider 无关的 SSE 分帧与读停滞检测（`stall_guard`，crate 私有），以及把帧翻译为 `StreamEvent` 的 Anthropic 专用 `SseParser` |

一个 `ChatModel` trait 覆盖三种 wire 方言，上层采样时无需感知具体
provider。本 crate 只依赖外部 crate——按其模块契约，不得依赖
runtime、capabilities、config 或 frontends；配置值始终优先于
`ModelCapabilities` 表，表中未知的模型名返回 `None`，由调用方回退到
会话默认值。`Usage::input_tokens` 在每种 wire 上都归一化为完整请求
规模（Anthropic 报告的是未缓存余量，解析器会把 cache 计数加上），
使上下文统计与 wire 无关。组装根（composition root）用
`Arc<dyn ChatModel>` 构建调用链（retry 包装、fallback 路由）——一个
blanket impl 让 `ChatModel` 穿透 `Arc` 转发，各层无需重复包装即可嵌套。
