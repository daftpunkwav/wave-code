//! Anthropic Messages streaming SSE 事件解析。
//!
//! 将每一条 SSE 消息的 `data` JSON 文本翻译为 [`StreamEvent`]
//! （见 [`SseParser::feed`]）。未知事件类型一律忽略，保证向前兼容。

use serde::Deserialize;

use crate::{LlmError, StreamEvent, Usage};

/// Anthropic Messages streaming SSE 解析器。
///
/// 有状态：累计 `message_start` 上报的 input_tokens，在 `message_delta`
/// 合成 [`StreamEvent::MessageComplete`] 时一并返回。
#[derive(Default)]
pub struct SseParser {
    input_tokens: u64,
}

impl SseParser {
    /// 新建解析器。
    pub fn new() -> Self {
        Self::default()
    }

    /// 输入一条 SSE 消息的 data JSON 文本，产出零或一个 [`StreamEvent`]。
    ///
    /// 事件类型分发规则（未知类型 / `ping` / `message_stop` 返回 `Ok(None)`）：
    /// - `message_start`：累计 `message.usage.input_tokens`；
    /// - `content_block_start`：`tool_use` → [`StreamEvent::ToolUseBegin`]，
    ///   `text` 及其他类型忽略；
    /// - `content_block_delta`：`text_delta` → [`StreamEvent::TextDelta`]，
    ///   `input_json_delta` → [`StreamEvent::ToolUseInputDelta`]，其他忽略；
    /// - `content_block_stop`：[`StreamEvent::BlockEnd`]；
    /// - `message_delta`：合成 [`StreamEvent::MessageComplete`]（含累计的
    ///   input_tokens；`stop_reason` 为 null 时按空串处理）；
    /// - `error`：[`LlmError::Api`]。
    pub fn feed(&mut self, data: &str) -> crate::Result<Option<StreamEvent>> {
        let value: serde_json::Value = serde_json::from_str(data)?;
        let Some(ty) = value.get("type").and_then(|t| t.as_str()) else {
            // 缺少 type 字段：按未知事件处理，保持向前兼容。
            return Ok(None);
        };
        match ty {
            "message_start" => {
                let ev: MessageStartEvent = serde_json::from_value(value)?;
                self.input_tokens += ev.message.usage.input_tokens;
                Ok(None)
            }
            "ping" => Ok(None),
            "content_block_start" => {
                let ev: ContentBlockStartEvent = serde_json::from_value(value)?;
                match ev.content_block {
                    StartedBlock::ToolUse { id, name } => {
                        Ok(Some(StreamEvent::ToolUseBegin { id, name }))
                    }
                    StartedBlock::Text | StartedBlock::Other => Ok(None),
                }
            }
            "content_block_delta" => {
                let ev: ContentBlockDeltaEvent = serde_json::from_value(value)?;
                match ev.delta {
                    Delta::TextDelta { text } => Ok(Some(StreamEvent::TextDelta { text })),
                    Delta::InputJsonDelta { partial_json } => {
                        Ok(Some(StreamEvent::ToolUseInputDelta { partial_json }))
                    }
                    Delta::Other => Ok(None),
                }
            }
            "content_block_stop" => Ok(Some(StreamEvent::BlockEnd)),
            "message_delta" => {
                let ev: MessageDeltaEvent = serde_json::from_value(value)?;
                Ok(Some(StreamEvent::MessageComplete {
                    stop_reason: ev.delta.stop_reason.unwrap_or_default(),
                    usage: Usage {
                        input_tokens: self.input_tokens,
                        output_tokens: ev.usage.output_tokens,
                    },
                }))
            }
            "message_stop" => Ok(None),
            "error" => {
                let ev: ErrorEvent = serde_json::from_value(value)?;
                Err(LlmError::Api {
                    kind: ev.error.kind,
                    message: ev.error.message,
                })
            }
            _ => Ok(None),
        }
    }
}

// ---- 以下为各事件类型的私有反序列化结构 ----

#[derive(Deserialize)]
struct MessageStartEvent {
    message: MessageStartMessage,
}

#[derive(Deserialize)]
struct MessageStartMessage {
    usage: MessageStartUsage,
}

#[derive(Deserialize)]
struct MessageStartUsage {
    #[serde(default)]
    input_tokens: u64,
}

#[derive(Deserialize)]
struct ContentBlockStartEvent {
    content_block: StartedBlock,
}

/// `content_block_start` 中的内容块；未知类型归入 Other 并忽略。
#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum StartedBlock {
    Text,
    ToolUse {
        id: String,
        name: String,
    },
    #[serde(other)]
    Other,
}

#[derive(Deserialize)]
struct ContentBlockDeltaEvent {
    delta: Delta,
}

/// `content_block_delta` 中的增量；未知类型归入 Other 并忽略。
///
/// 变体名刻意与 SSE 协议 type 字段（text_delta / input_json_delta）保持一致。
#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[allow(clippy::enum_variant_names)]
enum Delta {
    TextDelta {
        text: String,
    },
    InputJsonDelta {
        partial_json: String,
    },
    #[serde(other)]
    Other,
}

#[derive(Deserialize)]
struct MessageDeltaEvent {
    delta: MessageDeltaBody,
    usage: MessageDeltaUsage,
}

#[derive(Deserialize)]
struct MessageDeltaBody {
    /// 中途的 message_delta 事件 stop_reason 可能为 null。
    #[serde(default)]
    stop_reason: Option<String>,
}

#[derive(Deserialize)]
struct MessageDeltaUsage {
    #[serde(default)]
    output_tokens: u64,
}

#[derive(Deserialize)]
struct ErrorEvent {
    error: ErrorBody,
}

#[derive(Deserialize)]
struct ErrorBody {
    #[serde(rename = "type")]
    kind: String,
    message: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{StreamEvent, Usage};

    #[test]
    fn parses_text_delta() {
        let mut p = SseParser::new();
        let ev = p
            .feed(r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"你好"}}"#)
            .unwrap();
        assert_eq!(
            ev,
            Some(StreamEvent::TextDelta {
                text: "你好".into()
            })
        );
    }

    #[test]
    fn parses_tool_use_lifecycle() {
        let mut p = SseParser::new();
        let begin = p
            .feed(r#"{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_1","name":"write_file"}}"#)
            .unwrap();
        assert_eq!(
            begin,
            Some(StreamEvent::ToolUseBegin {
                id: "toolu_1".into(),
                name: "write_file".into()
            })
        );
        let delta = p
            .feed(r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"path\":"}}"#)
            .unwrap();
        assert_eq!(
            delta,
            Some(StreamEvent::ToolUseInputDelta {
                partial_json: "{\"path\":".into()
            })
        );
        let stop = p
            .feed(r#"{"type":"content_block_stop","index":1}"#)
            .unwrap();
        assert_eq!(stop, Some(StreamEvent::BlockEnd));
    }

    #[test]
    fn accumulates_usage_into_complete() {
        let mut p = SseParser::new();
        assert!(
            p.feed(r#"{"type":"message_start","message":{"usage":{"input_tokens":42}}}"#)
                .unwrap()
                .is_none()
        );
        assert!(p.feed(r#"{"type":"ping"}"#).unwrap().is_none());
        let done = p
            .feed(r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":7}}"#)
            .unwrap();
        assert_eq!(
            done,
            Some(StreamEvent::MessageComplete {
                stop_reason: "end_turn".into(),
                usage: Usage {
                    input_tokens: 42,
                    output_tokens: 7,
                },
            })
        );
    }

    #[test]
    fn api_error_is_err() {
        let mut p = SseParser::new();
        assert!(
            p.feed(
                r#"{"type":"error","error":{"type":"overloaded_error","message":"overloaded"}}"#
            )
            .is_err()
        );
    }

    #[test]
    fn unknown_event_type_is_ignored() {
        let mut p = SseParser::new();
        assert!(
            p.feed(r#"{"type":"some_future_event","x":1}"#)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn null_stop_reason_becomes_empty_string() {
        let mut p = SseParser::new();
        let ev = p.feed(r#"{"type":"message_delta","delta":{"stop_reason":null},"usage":{"output_tokens":1}}"#).unwrap();
        assert_eq!(
            ev,
            Some(StreamEvent::MessageComplete {
                stop_reason: String::new(),
                usage: Usage {
                    input_tokens: 0,
                    output_tokens: 1
                },
            })
        );
    }

    #[test]
    fn missing_type_field_is_ignored() {
        let mut p = SseParser::new();
        assert!(p.feed(r#"{"no_type":true}"#).unwrap().is_none());
    }

    #[test]
    fn unknown_nested_delta_is_ignored() {
        let mut p = SseParser::new();
        // 未来新增的 delta 类型（如 signature_delta）不应炸流
        assert!(p.feed(r#"{"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"x"}}"#).unwrap().is_none());
    }

    #[test]
    fn malformed_json_is_err() {
        let mut p = SseParser::new();
        assert!(p.feed("{not json").is_err());
    }
}
