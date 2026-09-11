//! Anthropic Messages streaming SSE event parsing.
//!
//! Translates the `data` JSON text of each SSE message into a [`StreamEvent`]
//! (see [`SseParser::feed`]). Unknown event types are always ignored for forward compatibility.

use serde::Deserialize;

use crate::{StreamEvent, Usage};

/// Anthropic Messages streaming SSE parser.
///
/// Stateful: accumulates the input_tokens reported by `message_start` and returns
/// them when synthesizing [`StreamEvent::MessageComplete`] on `message_delta`.
#[derive(Default)]
pub struct SseParser {
    input_tokens: u64,
}

impl SseParser {
    /// Creates a new parser.
    pub fn new() -> Self {
        Self::default()
    }

    /// Feeds one SSE message's data JSON text, producing zero or one [`StreamEvent`].
    ///
    /// Event-type dispatch rules (unknown types / `ping` / `message_stop` return `Ok(None)`):
    /// - `message_start`: accumulate `message.usage.input_tokens`;
    /// - `content_block_start`: `tool_use` becomes [`StreamEvent::ToolUseBegin`],
    ///   `text` and other types are ignored;
    /// - `content_block_delta`: `text_delta` becomes [`StreamEvent::TextDelta`],
    ///   `input_json_delta` becomes [`StreamEvent::ToolUseInputDelta`], others are ignored;
    /// - `content_block_stop`: [`StreamEvent::BlockEnd`];
    /// - `message_delta`: synthesize [`StreamEvent::MessageComplete`] (with the accumulated
    ///   input_tokens; a null `stop_reason` is treated as an empty string);
    /// - `error`: classified via `classify_api_error` - the too-long shape becomes
    ///   [`crate::LlmError::PromptTooLong`], everything else becomes [`crate::LlmError::Api`].
    pub fn feed(&mut self, data: &str) -> crate::Result<Option<StreamEvent>> {
        let value: serde_json::Value = serde_json::from_str(data)?;
        let Some(ty) = value.get("type").and_then(|t| t.as_str()) else {
            // Missing type field: treat as an unknown event for forward compatibility.
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
                Err(crate::classify_api_error(ev.error.kind, ev.error.message))
            }
            _ => Ok(None),
        }
    }
}

// ---- Private deserialization structs for each event type ----

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

/// Content block inside `content_block_start`; unknown types fall into Other and are ignored.
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

/// Delta inside `content_block_delta`; unknown types fall into Other and are ignored.
///
/// Variant names deliberately mirror the SSE protocol type field (text_delta / input_json_delta).
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
    /// Official endpoints always carry usage; third-party compatible gateways may omit it -
    /// default to 0 so the whole stream (including generated text) is not aborted over a missing stat field.
    #[serde(default)]
    usage: MessageDeltaUsage,
}

#[derive(Deserialize)]
struct MessageDeltaBody {
    /// The stop_reason of an in-flight message_delta event may be null.
    #[serde(default)]
    stop_reason: Option<String>,
}

#[derive(Deserialize, Default)]
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
            .feed(r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hello"}}"#)
            .unwrap();
        assert_eq!(
            ev,
            Some(StreamEvent::TextDelta {
                text: "hello".into()
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
        // Future delta types (e.g. signature_delta) must not break the stream
        assert!(p.feed(r#"{"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"x"}}"#).unwrap().is_none());
    }

    #[test]
    fn malformed_json_is_err() {
        let mut p = SseParser::new();
        assert!(p.feed("{not json").is_err());
    }
}
