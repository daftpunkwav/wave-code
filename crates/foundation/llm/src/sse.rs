//! SSE byte-stream framing plus Anthropic Messages event translation.
//!
//! The framing half is provider-agnostic: chunk buffering, frame-boundary
//! scanning, read-stall detection ([`stall_guard`]), and the shared
//! [`decode_sse_frames`] loop both provider clients stream through.
//! The translation half ([`SseParser`]) is Anthropic-specific: it turns the
//! `data` JSON text of each SSE message into a [`StreamEvent`]
//! (see [`SseParser::feed`]). Unknown event types are always ignored for
//! forward compatibility.

use std::time::Duration;

use futures::{Stream, StreamExt};
use serde::Deserialize;

use crate::{LlmError, Result, StreamEvent, Usage};

/// Anthropic Messages streaming SSE parser.
///
/// Stateful: records the input_tokens reported by `message_start` (a repeated
/// one replaces, never accumulates) and returns them when synthesizing
/// [`StreamEvent::MessageComplete`] on `message_delta`. Also accumulates an
/// open thinking block's text and signature so its end can hand the whole
/// block over in one [`StreamEvent::ThinkingComplete`].
#[derive(Default)]
pub struct SseParser {
    input_tokens: u64,
    cache_read_tokens: u64,
    cache_creation_tokens: u64,
    /// Open thinking block: `(text, signature)`; `None` outside one.
    thinking: Option<(String, Option<String>)>,
}

impl SseParser {
    /// Creates a new parser.
    pub fn new() -> Self {
        Self::default()
    }

    /// Feeds one SSE message's data JSON text, producing zero or one [`StreamEvent`].
    ///
    /// Event-type dispatch rules (unknown types / `ping` / `message_stop` return `Ok(None)`):
    /// - `message_start`: accumulate `message.usage.input_tokens` plus the cache
    ///   read/creation counters (third-party gateways may omit them - they default to 0);
    /// - `content_block_start`: `tool_use` becomes [`StreamEvent::ToolUseBegin`],
    ///   `thinking` opens a reasoning block (its deltas surface via
    ///   [`StreamEvent::ThinkingDelta`]), `text` and other types are ignored;
    /// - `content_block_delta`: `text_delta` becomes [`StreamEvent::TextDelta`],
    ///   `thinking_delta` becomes [`StreamEvent::ThinkingDelta`],
    ///   `signature_delta` becomes [`StreamEvent::SignatureDelta`],
    ///   `input_json_delta` becomes [`StreamEvent::ToolUseInputDelta`], others are ignored;
    /// - `content_block_stop`: [`StreamEvent::ThinkingComplete`] when the block
    ///   that just closed was a reasoning block (carrying its whole text and
    ///   signature, so history can send it back), otherwise [`StreamEvent::BlockEnd`];
    /// - `message_delta`: synthesize [`StreamEvent::MessageComplete`] (with the accumulated
    ///   input_tokens and cache counters; a null `stop_reason` is treated as an empty string);
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
                // Assignment, never accumulation: one stream declares exactly
                // one message_start, so a duplicated one (broken proxy) must
                // replace the counters instead of doubling them.
                //
                // Anthropic's `input_tokens` is the uncached remainder, so the
                // full prompt is the sum of the three counters (see
                // [`crate::Usage::input_tokens`]); the context meter and the
                // compaction budget compare the sum against the window, and a
                // warm cache would otherwise make a 100k-token prompt read as
                // a handful of fresh tokens.
                self.input_tokens = ev
                    .message
                    .usage
                    .input_tokens
                    .saturating_add(ev.message.usage.cache_read_input_tokens)
                    .saturating_add(ev.message.usage.cache_creation_input_tokens);
                self.cache_read_tokens = ev.message.usage.cache_read_input_tokens;
                self.cache_creation_tokens = ev.message.usage.cache_creation_input_tokens;
                Ok(None)
            }
            "ping" => Ok(None),
            "content_block_start" => {
                let ev: ContentBlockStartEvent = serde_json::from_value(value)?;
                match ev.content_block {
                    StartedBlock::ToolUse { id, name } => {
                        Ok(Some(StreamEvent::ToolUseBegin { id, name }))
                    }
                    StartedBlock::Thinking => {
                        self.thinking = Some((String::new(), None));
                        Ok(None)
                    }
                    StartedBlock::Text | StartedBlock::Other => Ok(None),
                }
            }
            "content_block_delta" => {
                let ev: ContentBlockDeltaEvent = serde_json::from_value(value)?;
                match ev.delta {
                    Delta::TextDelta { text } => Ok(Some(StreamEvent::TextDelta { text })),
                    Delta::ThinkingDelta { thinking } => {
                        if let Some((text, _)) = self.thinking.as_mut() {
                            text.push_str(&thinking);
                        }
                        Ok(Some(StreamEvent::ThinkingDelta { text: thinking }))
                    }
                    Delta::SignatureDelta { signature } => {
                        if let Some((_, slot)) = self.thinking.as_mut() {
                            // Concatenate: the signature is one opaque token
                            // split across deltas on some gateways.
                            match slot {
                                Some(existing) => existing.push_str(&signature),
                                None => *slot = Some(signature.clone()),
                            }
                        }
                        Ok(Some(StreamEvent::SignatureDelta { signature }))
                    }
                    Delta::InputJsonDelta { partial_json } => {
                        Ok(Some(StreamEvent::ToolUseInputDelta { partial_json }))
                    }
                    Delta::Other => Ok(None),
                }
            }
            "content_block_stop" => match self.thinking.take() {
                Some((text, signature)) => {
                    Ok(Some(StreamEvent::ThinkingComplete { text, signature }))
                }
                None => Ok(Some(StreamEvent::BlockEnd)),
            },
            "message_delta" => {
                let ev: MessageDeltaEvent = serde_json::from_value(value)?;
                Ok(Some(StreamEvent::MessageComplete {
                    stop_reason: ev.delta.stop_reason.unwrap_or_default(),
                    usage: Usage {
                        input_tokens: self.input_tokens,
                        output_tokens: ev.usage.output_tokens,
                        cache_read_tokens: self.cache_read_tokens,
                        cache_creation_tokens: self.cache_creation_tokens,
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
    /// Cache-read counter; third-party gateways may omit it (defaults to 0).
    #[serde(default)]
    cache_read_input_tokens: u64,
    /// Cache-write counter; third-party gateways may omit it (defaults to 0).
    #[serde(default)]
    cache_creation_input_tokens: u64,
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
    /// Extended-thinking block; its start carries no payload, the content
    /// arrives as thinking_delta / signature_delta events. (`redacted_thinking`
    /// blocks carry opaque data and no deltas - they fall into Other.)
    Thinking,
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
/// Variant names deliberately mirror the SSE protocol type field (text_delta /
/// thinking_delta / signature_delta / input_json_delta).
#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[allow(clippy::enum_variant_names)]
enum Delta {
    TextDelta {
        text: String,
    },
    ThinkingDelta {
        thinking: String,
    },
    SignatureDelta {
        signature: String,
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

// ---- Byte-stream framing shared by both providers ----
//
// Chunk buffering, frame-boundary scanning, and read-stall detection are
// provider-agnostic memory-safety-critical logic; both clients funnel their
// `bytes_stream` through here so a fix lands once, never twice.

/// Hard cap on the SSE byte buffer (8 MiB): exceeding it means the server is not sending SSE frame
/// boundaries, so yield Err and terminate the stream - this stops a malicious/broken server from
/// blowing up memory with boundary-less data (OOM).
pub(crate) const MAX_SSE_BUF: usize = 8 * 1024 * 1024;

/// Read-stall timeout: when the gap between adjacent byte chunks exceeds this value, the upstream
/// is considered stalled (wedged connection), and the stream ends with Err. Only "idle time between
/// chunks" is constrained, never the whole-stream duration - a long reply that keeps producing
/// bytes (provider keep-alive pings / deltas both count) is unaffected; at this scale, 120s with
/// zero bytes basically leaves a dead connection as the only explanation.
pub(crate) const STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(120);

/// Await a request-establishment future (POST issued, awaiting response
/// headers) under the same idle bound as the stream: `connect_timeout` only
/// covers the TCP/TLS connect and [`stall_guard`] only starts once headers
/// are back, so a server that accepts the connection and never answers would
/// otherwise hang `.send()` - and with it the whole sampling turn - forever.
/// Timing out maps to [`LlmError::Timeout`], which the retry policy treats as
/// transient (bounded attempts and deadline), so a blackholed endpoint
/// recovers through the existing retry path.
pub(crate) async fn send_with_idle_bound(
    fut: impl std::future::Future<Output = std::result::Result<reqwest::Response, reqwest::Error>>,
) -> Result<reqwest::Response> {
    match tokio::time::timeout(STREAM_IDLE_TIMEOUT, fut).await {
        Ok(result) => result.map_err(|e| LlmError::Http(e.to_string())),
        Err(_) => Err(LlmError::Timeout(format!(
            "request establishment timed out: no response headers within {STREAM_IDLE_TIMEOUT:?}"
        ))),
    }
}

/// Hard cap on one non-2xx error body read (64 KiB). Only the first
/// [`crate::MAX_ERROR_BODY_CHARS`] characters are ever retained for the
/// error text, so reading is stopped at the cap and the response dropped:
/// an endpoint answering an error with an endless body must not balloon
/// memory (the same bound the SSE buffer and the MCP transport's body
/// cap enforce).
pub(crate) const MAX_ERROR_BODY_BYTES: usize = 64 * 1024;

/// Read one non-2xx error response body with a hard byte cap. Reading
/// stops at [`MAX_ERROR_BODY_BYTES`]; a transport failure mid-body keeps
/// whatever arrived, so the status error still carries the server's
/// message (and its `Retry-After` hint) instead of the read failure
/// replacing it.
///
/// Each chunk read carries the same idle bound as the stream path
/// ([`STREAM_IDLE_TIMEOUT`]): the clients never set a global request
/// timeout (healthy SSE streams are long-lived), so a server that answers
/// the error headers and then stalls would otherwise hang the sampling
/// turn forever with no retry policy in sight.
pub(crate) async fn read_error_body_capped(mut response: reqwest::Response) -> String {
    let mut body: Vec<u8> = Vec::new();
    while body.len() < MAX_ERROR_BODY_BYTES {
        match tokio::time::timeout(STREAM_IDLE_TIMEOUT, response.chunk()).await {
            // A chunk may straddle the cap: only the fitting prefix is kept,
            // so the buffer never exceeds the bound by one chunk.
            Ok(Ok(Some(chunk))) => {
                let room = MAX_ERROR_BODY_BYTES - body.len();
                let end = room.min(chunk.len());
                body.extend_from_slice(&chunk[..end]);
            }
            // EOF, transport failure, or a stalled body: keep whatever
            // arrived (the status error still explains the failure).
            Ok(Ok(None)) | Ok(Err(_)) | Err(_) => break,
        }
    }
    String::from_utf8_lossy(&body).into_owned()
}

/// Byte-stream stall guard: wraps each `next()` with an idle timeout; a timeout ends the stream with
/// [`LlmError::Timeout`]. It sits upstream of [`decode_sse_frames`] rather than inside it,
/// keeping frame-parsing logic orthogonal to the timeout policy (tests can drive each independently).
pub(crate) fn stall_guard<S>(
    byte_stream: S,
    idle_timeout: Duration,
) -> impl Stream<Item = Result<bytes::Bytes>> + Send
where
    S: Stream<Item = Result<bytes::Bytes>> + Send,
{
    let mut byte_stream = Box::pin(byte_stream);
    async_stream::try_stream! {
        loop {
            // Only counts while the consumer polls: a suspended consumer (e.g. waiting on approval) spends no budget.
            match tokio::time::timeout(idle_timeout, byte_stream.as_mut().next()).await {
                // Stalled: the upstream produced no bytes within the timeout.
                Err(_) => {
                    Err::<(), _>(LlmError::Timeout(format!(
                        "stream idle timeout: no data for {idle_timeout:?} (upstream connection may have stalled)"
                    )))?;
                }
                Ok(None) => break,
                Ok(Some(chunk)) => yield chunk?,
            }
        }
    }
}

/// Byte-chunk stream to event stream: buffers bytes, splits SSE frames on blank lines, and hands
/// each frame's `data` payload to `on_data`.
///
/// chunk boundaries are arbitrary (TCP may split one frame across chunks), so framing must happen at the byte-buffer layer;
/// when the buffer reaches `max_buf` with no frame boundary, yield Err and terminate the stream.
pub(crate) fn decode_sse_frames<S, F>(
    byte_stream: S,
    max_buf: usize,
    mut on_data: F,
) -> impl Stream<Item = Result<StreamEvent>> + Send
where
    S: Stream<Item = Result<bytes::Bytes>> + Send,
    F: FnMut(&str) -> Result<Vec<StreamEvent>> + Send,
{
    let mut byte_stream = Box::pin(byte_stream);
    async_stream::try_stream! {
        let mut buf: Vec<u8> = Vec::new();
        // buf[..scanned] is confirmed to hold no complete frame boundary; resuming the next scan at scanned - 3 suffices
        // (step 3 bytes back: the longest separator \r\n\r\n is 4 bytes and may straddle a chunk boundary),
        // avoiding an O(n²) full-buffer rescan per chunk under unbounded long frames.
        let mut scanned: usize = 0;
        while let Some(chunk) = byte_stream.next().await {
            let chunk = chunk?;
            buf.extend_from_slice(&chunk);
            if buf.len() > max_buf {
                // Yield Err and terminate the stream (same as the `?` behavior on feed below).
                Err::<(), _>(LlmError::Sse(format!(
                    "SSE buffer exceeded the {max_buf}-byte cap (server sent no frame boundary)"
                )))?;
            }
            let mut from = scanned.saturating_sub(3);
            while let Some((body_end, sep_len)) = find_frame_boundary(&buf[from..]) {
                let end = from + body_end + sep_len;
                // The frame body is parsed straight out of the buffer (no
                // per-frame copy; long streams cut thousands of frames).
                if let Some(data) = extract_data(&buf[..from + body_end])? {
                    // callback returned Err: yield Err and terminate the stream (`?` operator behavior).
                    for event in on_data(&data)? {
                        yield event;
                    }
                }
                // Cutting one frame shifts the remaining bytes forward; keep cutting from the head (one chunk may hold several frames).
                buf.drain(..end);
                from = 0;
            }
            scanned = buf.len();
        }
        // A trailing incomplete frame at end of stream is dropped per SSE convention.
    }
}

/// Torn-EOF guard shared by every provider's decode loop: a clean end that
/// never delivered a terminal [`StreamEvent::MessageComplete`] is a torn
/// stream, not a success — the run loop would otherwise read the sample as
/// completed with no billing data. All events pass through unchanged; the
/// guard only appends one error at EOF. An error already seen on the stream
/// suppresses the guard (that failure already explains the truncation).
/// `eof_error` names the provider-specific missing terminal frame.
pub(crate) fn reject_torn_eof<S>(
    inner: S,
    eof_error: &'static str,
) -> impl Stream<Item = Result<StreamEvent>> + Send
where
    S: Stream<Item = Result<StreamEvent>> + Send,
{
    async_stream::stream! {
        tokio::pin!(inner);
        let mut saw_completion = false;
        let mut failed = false;
        while let Some(item) = inner.next().await {
            if matches!(&item, Ok(StreamEvent::MessageComplete { .. })) {
                saw_completion = true;
            }
            failed |= item.is_err();
            yield item;
        }
        if !saw_completion && !failed {
            yield Err(LlmError::Sse(eof_error.to_string()));
        }
    }
}

/// Finds a frame boundary in the buffer (`\n\n` or `\r\n\r\n`, whichever comes first).
///
/// Returns `(body length, separator length)`.
fn find_frame_boundary(buf: &[u8]) -> Option<(usize, usize)> {
    let lf = find_subsequence(buf, b"\n\n").map(|i| (i, 2));
    let crlf = find_subsequence(buf, b"\r\n\r\n").map(|i| (i, 4));
    match (lf, crlf) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (only, None) | (None, only) => only,
    }
}

/// Substring search: returns the first index of `needle` in `haystack`.
pub(crate) fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Extracts the `data:` payload from a single SSE frame (multiple data lines are joined with `\n`).
///
/// A frame with no data lines (e.g. a bare `event:` / comment line) returns `Ok(None)`.
fn extract_data(frame: &[u8]) -> Result<Option<String>> {
    let text = std::str::from_utf8(frame).map_err(|e| LlmError::Sse(e.to_string()))?;
    let mut data_lines: Vec<&str> = Vec::new();
    for line in text.split('\n') {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if let Some(payload) = line.strip_prefix("data:") {
            // SSE spec: at most one leading space may follow the colon.
            data_lines.push(payload.strip_prefix(' ').unwrap_or(payload));
        }
    }
    Ok((!data_lines.is_empty()).then(|| data_lines.join("\n")))
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
                    ..Usage::default()
                },
            })
        );
    }

    /// A duplicated message_start (broken proxy replaying the stream head)
    /// replaces the declared usage instead of doubling it.
    #[test]
    fn repeated_message_start_replaces_usage() {
        let mut p = SseParser::new();
        assert!(
            p.feed(r#"{"type":"message_start","message":{"usage":{"input_tokens":42,"cache_read_input_tokens":9}}}"#)
                .unwrap()
                .is_none()
        );
        assert!(
            p.feed(r#"{"type":"message_start","message":{"usage":{"input_tokens":50}}}"#)
                .unwrap()
                .is_none()
        );
        let done = p
            .feed(r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":1}}"#)
            .unwrap();
        assert_eq!(
            done,
            Some(StreamEvent::MessageComplete {
                stop_reason: "end_turn".into(),
                usage: Usage {
                    input_tokens: 50,
                    output_tokens: 1,
                    cache_read_tokens: 0,
                    cache_creation_tokens: 0,
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
                    output_tokens: 1,
                    ..Usage::default()
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
        // Future delta types (e.g. cite_delta) must not break the stream.
        // signature_delta is no longer "unknown": it parses to SignatureDelta.
        assert!(p.feed(r#"{"type":"content_block_delta","index":0,"delta":{"type":"cite_delta","cite":"x"}}"#).unwrap().is_none());
        assert_eq!(
            p.feed(r#"{"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"x"}}"#).unwrap(),
            Some(StreamEvent::SignatureDelta { signature: "x".into() })
        );
    }

    #[test]
    fn malformed_json_is_err() {
        let mut p = SseParser::new();
        assert!(p.feed("{not json").is_err());
    }

    /// Extended-thinking blocks surface as ThinkingDelta / SignatureDelta and,
    /// at the block's end, one ThinkingComplete carrying the whole block so
    /// history can send it back on the next tool round. The surrounding
    /// text/tool events are untouched.
    #[test]
    fn parses_thinking_and_signature_deltas() {
        let mut p = SseParser::new();
        assert!(
            p.feed(r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}"#)
                .unwrap()
                .is_none()
        );
        assert_eq!(
            p.feed(r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"let me"}}"#)
                .unwrap(),
            Some(StreamEvent::ThinkingDelta {
                text: "let me".into()
            })
        );
        assert_eq!(
            p.feed(r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":" check"}}"#)
                .unwrap(),
            Some(StreamEvent::ThinkingDelta {
                text: " check".into()
            })
        );
        assert_eq!(
            p.feed(r#"{"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"sig"}}"#)
                .unwrap(),
            Some(StreamEvent::SignatureDelta {
                signature: "sig".into()
            })
        );
        // A split signature concatenates instead of replacing.
        assert_eq!(
            p.feed(r#"{"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"9"}}"#)
                .unwrap(),
            Some(StreamEvent::SignatureDelta {
                signature: "9".into()
            })
        );
        // The block's end hands over the accumulated reasoning block.
        assert_eq!(
            p.feed(r#"{"type":"content_block_stop","index":0}"#)
                .unwrap(),
            Some(StreamEvent::ThinkingComplete {
                text: "let me check".into(),
                signature: Some("sig9".into()),
            })
        );
        // Text blocks still close with a plain BlockEnd.
        assert!(
            p.feed(r#"{"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}"#)
                .unwrap()
                .is_none()
        );
        assert_eq!(
            p.feed(r#"{"type":"content_block_stop","index":1}"#)
                .unwrap(),
            Some(StreamEvent::BlockEnd)
        );
        // A redacted_thinking block (no deltas) is ignored like Other.
        assert!(
            p.feed(r#"{"type":"content_block_start","index":2,"content_block":{"type":"redacted_thinking","data":"x"}}"#)
                .unwrap()
                .is_none()
        );
    }

    /// Cache counters from message_start flow into MessageComplete.usage, and
    /// the cache portions are folded into `input_tokens`: a cache hit means
    /// most of the prompt is served from cache, and the context meter must
    /// still see the whole prompt. Gateways that omit the counters degrade to
    /// 0 without breaking the stream.
    #[test]
    fn cache_usage_accumulates_into_the_full_prompt() {
        let mut p = SseParser::new();
        assert!(
            p.feed(r#"{"type":"message_start","message":{"usage":{"input_tokens":10,"cache_read_input_tokens":100,"cache_creation_input_tokens":7}}}"#)
                .unwrap()
                .is_none()
        );
        let done = p
            .feed(r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":3}}"#)
            .unwrap();
        assert_eq!(
            done,
            Some(StreamEvent::MessageComplete {
                stop_reason: "end_turn".into(),
                usage: Usage {
                    // 10 uncached + 100 read + 7 written.
                    input_tokens: 117,
                    output_tokens: 3,
                    cache_read_tokens: 100,
                    cache_creation_tokens: 7,
                },
            })
        );
        // Missing counters default to 0.
        let mut p = SseParser::new();
        assert!(
            p.feed(r#"{"type":"message_start","message":{"usage":{"input_tokens":5}}}"#)
                .unwrap()
                .is_none()
        );
        let done = p
            .feed(r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":1}}"#)
            .unwrap();
        assert_eq!(
            done,
            Some(StreamEvent::MessageComplete {
                stop_reason: "end_turn".into(),
                usage: Usage {
                    input_tokens: 5,
                    output_tokens: 1,
                    cache_read_tokens: 0,
                    cache_creation_tokens: 0,
                },
            })
        );
    }

    /// An establishment that never completes must surface as
    /// [`LlmError::Timeout`] (the retry policy's transient class) instead of
    /// hanging the sampling turn forever. Paused time fires the 120s idle
    /// bound instantly, so the test needs no real waiting.
    #[tokio::test(start_paused = true)]
    async fn request_establishment_hang_maps_to_timeout() {
        let outcome = send_with_idle_bound(std::future::pending()).await;
        assert!(
            matches!(outcome, Err(LlmError::Timeout(ref message)) if message.contains("request establishment timed out")),
            "{outcome:?}"
        );
    }

    /// A response that does arrive is handed back untouched: the bound must
    /// not turn healthy establishments into errors.
    #[tokio::test]
    async fn request_establishment_passes_successful_responses_through() {
        // Minimal HTTP/1.1 server on a loopback port; no client behavior
        // beyond "headers came back" is under test here.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 1024];
            let _ = tokio::io::AsyncReadExt::read(&mut socket, &mut buf).await;
            tokio::io::AsyncWriteExt::write_all(
                &mut socket,
                b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nhi",
            )
            .await
            .unwrap();
        });
        let client = reqwest::Client::new();
        let response = send_with_idle_bound(client.get(format!("http://{addr}/")).send())
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        server.await.unwrap();
    }

    /// The error-body reader stops at the byte cap, not at EOF: a server
    /// answering an error with an endless body must not balloon memory
    /// (the response is dropped once the cap is hit, closing the read).
    #[tokio::test]
    async fn error_body_read_stops_at_the_cap() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 1024];
            let _ = tokio::io::AsyncReadExt::read(&mut socket, &mut buf).await;
            // Announce far more body than the cap; the extra bytes never
            // need to arrive for the reader to give up.
            let _ = tokio::io::AsyncWriteExt::write_all(
                &mut socket,
                b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 100000000\r\n\r\n",
            )
            .await;
            // Keep the connection open: EOF must not be what ends the read.
            let _ =
                tokio::io::AsyncWriteExt::write_all(&mut socket, &vec![b'x'; 1024 * 1024]).await;
            let _ = tokio::io::AsyncWriteExt::flush(&mut socket).await;
        });
        let client = reqwest::Client::new();
        let response = client.get(format!("http://{addr}/")).send().await.unwrap();
        assert_eq!(
            response.status(),
            reqwest::StatusCode::INTERNAL_SERVER_ERROR
        );
        let body = read_error_body_capped(response).await;
        assert_eq!(
            body.chars().count(),
            MAX_ERROR_BODY_BYTES,
            "the cap, not the server, ends the read"
        );
        server.await.unwrap();
    }

    /// A server that answers the error headers, delivers a partial body,
    /// and then stalls must not hang the read forever: the per-chunk idle
    /// bound ends it with whatever arrived (time is frozen and advanced
    /// manually, so the 120s bound fires instantly).
    #[tokio::test]
    async fn stalled_error_body_read_ends_with_the_partial_body() {
        // Manual time control from the start: the loopback IO below runs on
        // the real reactor, while every timer waits for an explicit advance
        // (a `start_paused` test cannot switch to manual mode afterwards —
        // time is already frozen there, with auto-advance racing the first
        // chunk out of the socket buffer).
        tokio::time::pause();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 1024];
            let _ = tokio::io::AsyncReadExt::read(&mut socket, &mut buf).await;
            // Announce far more body than is ever sent, then stall with the
            // connection open: only the idle bound can end the read.
            let _ = tokio::io::AsyncWriteExt::write_all(
                &mut socket,
                b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 1000000\r\n\r\npartial",
            )
            .await;
            let _ = tokio::io::AsyncWriteExt::flush(&mut socket).await;
            // Hold the socket open well past the idle bound.
            tokio::time::sleep(STREAM_IDLE_TIMEOUT * 4).await;
        });
        let client = reqwest::Client::new();
        let response = client.get(format!("http://{addr}/")).send().await.unwrap();
        let read = tokio::spawn(read_error_body_capped(response));
        // Let the reader take the already-written chunk: small advances
        // pump real IO while staying far below the idle bound.
        for _ in 0..40 {
            tokio::time::advance(Duration::from_millis(50)).await;
        }
        // Past the bound the read must be finished: the yields pump the
        // executor WITHOUT advancing time, so the server's sleep stays
        // pending, the socket stays open, and the only way the read can
        // end is the inner idle bound (a missing bound fails the
        // assertion instead of hanging the test).
        tokio::time::advance(STREAM_IDLE_TIMEOUT).await;
        for _ in 0..100 {
            tokio::task::yield_now().await;
        }
        assert!(
            read.is_finished(),
            "the idle bound must end the stalled read"
        );
        let body = read.await.unwrap();
        assert_eq!(body, "partial", "whatever arrived is kept");
        server.abort();
    }
}
