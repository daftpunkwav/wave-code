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
    /// 上下文超长错误（core reactive compact 的触发条件，SPEC §5.2）：
    /// provider 明确返回 prompt / request 过大（如 Anthropic 400
    /// "prompt is too long"、413 request_too_large）。从通用 Api 错误中
    /// 单列变体，让上层做枚举匹配而非字符串嗅探。
    #[error("prompt 超出上下文上限: {message}")]
    PromptTooLong { message: String },
    /// SSE 帧解析错误。
    #[error("SSE 解析错误: {0}")]
    Sse(String),
    /// JSON 序列化 / 反序列化错误。
    #[error("JSON 错误: {0}")]
    Json(#[from] serde_json::Error),
}

/// API 错误的统一构造点（anthropic 非 2xx 响应与 SSE error 事件共用）：
/// 识别 prompt_too_long 已知形态——kind 或 message 含
/// `prompt_too_long` / `request_too_large` 标记，或 message 含
/// "prompt is too long"（Anthropic 400 文案）——归入
/// [`LlmError::PromptTooLong`]；其余按通用 [`LlmError::Api`] 返回。
/// 分类集中在此单点，新增 provider 形态只改这里。
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

/// crate 内统一 Result 别名。
pub type Result<T> = std::result::Result<T, LlmError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_maps_known_too_long_shapes() {
        // Anthropic 400 文案形态
        assert!(matches!(
            classify_api_error(
                "http_400".into(),
                "prompt is too long: 210000 tokens > 200000 maximum".into()
            ),
            LlmError::PromptTooLong { .. }
        ));
        // 413 request_too_large（kind 与 message 两种携带位置）
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
        // SSE error 事件的 kind 形态
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
            "非超长形态应保持 Api 变体: {err:?}"
        );
        // 401 等鉴权错误不误判
        assert!(matches!(
            classify_api_error("http_401".into(), "invalid api key".into()),
            LlmError::Api { .. }
        ));
    }
}
