//! wavecode-llm — 多 provider 抽象层。
//!
//! 定义统一的 Messages 请求 / 流式事件接口（SSE），M1 阶段包含：
//! - 公共类型（[`Message`] / [`ContentBlock`] / [`ToolSpec`] / [`StreamEvent`] 等）
//!   与 [`ChatModel`] trait；
//! - Anthropic Messages streaming SSE 解析器（[`SseParser`]）；
//! - 内置实现：Anthropic Messages API 流式客户端（[`AnthropicClient`]）。
//!
//! OpenAI 兼容 provider 的 HTTP 客户端实现，以及 token 计数与
//! 模型能力表（上下文窗口、最大输出等）将在后续里程碑落地。

use std::sync::Arc;

use serde::{Deserialize, Serialize};

pub mod anthropic;
mod sse;

pub use anthropic::AnthropicClient;
pub use sse::SseParser;

/// 对话角色。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    User,
    Assistant,
}

/// 消息内容块。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    /// 纯文本块。
    Text { text: String },
    /// 模型发起的工具调用。
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    /// 工具执行结果，回填给模型。
    ToolResult {
        tool_use_id: String,
        content: String,
        #[serde(default)]
        is_error: bool,
    },
}

/// 一条对话消息。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Message {
    pub role: Role,
    pub content: Vec<ContentBlock>,
}

/// 工具定义（随请求发给模型）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
}

/// token 用量统计。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

/// 流式响应事件。
#[derive(Debug, Clone, PartialEq)]
pub enum StreamEvent {
    /// 文本增量。
    TextDelta { text: String },
    /// 工具调用块开始。
    ToolUseBegin { id: String, name: String },
    /// 工具调用 input JSON 增量。
    ToolUseInputDelta { partial_json: String },
    /// 当前内容块结束。
    BlockEnd,
    /// 整条消息完成。
    MessageComplete { stop_reason: String, usage: Usage },
}

/// 一次流式对话请求。
#[derive(Debug, Clone)]
pub struct ChatRequest {
    pub model: String,
    pub system: String,
    /// 历史快照以 `Arc` 共享（P3，SPEC §17.5 M4）：调用方每轮以 O(1)
    /// 指针克隆冻结当轮历史，取代逐轮深拷贝的 O(n²)；provider 实现
    /// 在 `stream()` 内即完成序列化，不长期持有快照。
    pub messages: Arc<Vec<Message>>,
    pub tools: Vec<ToolSpec>,
    pub max_tokens: u32,
}

/// 流式事件流的统一返回类型（`ChatModel::stream` 与各 provider 实现共用）。
pub type EventStream = std::pin::Pin<Box<dyn futures::Stream<Item = Result<StreamEvent>> + Send>>;

/// 统一的流式对话模型抽象。
#[async_trait::async_trait]
pub trait ChatModel: Send + Sync {
    /// 发起流式请求，返回事件流。
    async fn stream(&self, req: ChatRequest) -> Result<EventStream>;
}

/// llm crate 统一错误类型。
#[derive(Debug, thiserror::Error)]
pub enum LlmError {
    /// HTTP 传输层错误。
    #[error("HTTP 错误: {0}")]
    Http(String),
    /// API 返回的业务错误（如 overloaded_error）。
    #[error("API 错误 ({kind}): {message}")]
    Api { kind: String, message: String },
    /// SSE 帧解析错误。
    #[error("SSE 解析错误: {0}")]
    Sse(String),
    /// JSON 序列化 / 反序列化错误。
    #[error("JSON 错误: {0}")]
    Json(#[from] serde_json::Error),
}

/// crate 内统一 Result 别名。
pub type Result<T> = std::result::Result<T, LlmError>;
