//! Anthropic Messages API streaming HTTP client.
//!
//! `POST {base_url}/v1/messages` starts an SSE streaming request; byte chunks flow through
//! buffering and framing, then [`crate::SseParser`] parses each frame into [`crate::StreamEvent`].

use std::time::Duration;

use futures::{Stream, StreamExt};

use crate::{
    ChatModel, ChatRequest, ContentBlock, EventStream, LlmError, Message, Result, SseParser,
    StreamEvent,
};

/// Anthropic Messages API streaming client.
pub struct AnthropicClient {
    base_url: String,
    api_key: String,
    http: reqwest::Client,
}

impl AnthropicClient {
    /// Creates a new client.
    pub fn new(base_url: String, api_key: String) -> Self {
        Self {
            base_url,
            api_key,
            http: build_http_client(),
        }
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
fn build_http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        // Same as reqwest's own `Client::new()`: only TLS init failure can reach this path.
        .expect("failed to build HTTP client")
}

#[async_trait::async_trait]
impl ChatModel for AnthropicClient {
    async fn stream(&self, req: ChatRequest) -> Result<EventStream> {
        let url = messages_url(&self.base_url);
        let response = self
            .http
            .post(url)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
            .json(&build_request_body(&req))
            .send()
            .await
            .map_err(|e| LlmError::Http(e.to_string()))?;

        let status = response.status();
        if !status.is_success() {
            let body = response
                .text()
                .await
                .map_err(|e| LlmError::Http(e.to_string()))?;
            return Err(crate::classify_api_error(
                format!("http_{}", status.as_u16()),
                truncate_error_body(&body),
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

/// Max retained chars of an error response body.
const MAX_ERROR_BODY_CHARS: usize = 2000;

/// Truncates an error response body: keeps at most the first [`MAX_ERROR_BODY_CHARS`] chars (truncated
/// by char, so multibyte chars are never split); the API key only travels in request headers and is never written into error text.
fn truncate_error_body(body: &str) -> String {
    body.chars().take(MAX_ERROR_BODY_CHARS).collect()
}

/// Builds the Anthropic Messages API request body (stream is always true).
pub(crate) fn build_request_body(req: &ChatRequest) -> serde_json::Value {
    serde_json::json!({
        "model": req.model,
        "system": req.system,
        // Serialize via the Arc<Vec<Message>> snapshot deref (serde's rc feature is off,
        // so no need to enable an extra feature for this single serialization).
        "messages": merge_adjacent_same_role(&req.messages),
        "tools": req.tools,
        "max_tokens": req.max_tokens,
        "stream": true,
    })
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

/// Hard cap on the SSE byte buffer (8 MiB): exceeding it means the server is not sending SSE frame
/// boundaries, so yield Err and terminate the stream - this stops a malicious/broken server from
/// blowing up memory with boundary-less data (OOM).
const MAX_SSE_BUF: usize = 8 * 1024 * 1024;

/// Read-stall timeout: when the gap between adjacent byte chunks exceeds this value, the upstream
/// is considered stalled (wedged connection), and the stream ends with Err. Only "idle time between
/// chunks" is constrained, never the whole-stream duration - a long reply that keeps producing
/// bytes (Anthropic ping / delta both count) is unaffected; at this scale, 120s with zero bytes
/// basically leaves a dead connection as the only explanation.
const STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(120);

/// Byte-stream stall guard: wraps each `next()` with an idle timeout; a timeout ends the stream with
/// [`LlmError::Timeout`]. It sits upstream of [`decode_event_stream`] rather than inside it,
/// keeping frame-parsing logic orthogonal to the timeout policy (tests can drive each independently).
fn stall_guard<S>(
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

/// Byte-chunk stream to event stream: buffers bytes, splits SSE frames on blank lines, and hands data to [`SseParser`].
///
/// chunk boundaries are arbitrary (TCP may split one frame across chunks), so framing must happen at the byte-buffer layer;
/// when the buffer reaches `max_buf` with no frame boundary, yield Err and terminate the stream.
/// This function is the core parse path shared by [`ChatModel::stream`] and the tests.
fn decode_event_stream<S>(
    byte_stream: S,
    max_buf: usize,
) -> impl Stream<Item = Result<StreamEvent>> + Send
where
    S: Stream<Item = Result<bytes::Bytes>> + Send,
{
    let mut byte_stream = Box::pin(byte_stream);
    async_stream::try_stream! {
        let mut buf: Vec<u8> = Vec::new();
        // buf[..scanned] is confirmed to hold no complete frame boundary; resuming the next scan at scanned - 3 suffices
        // (step 3 bytes back: the longest separator \r\n\r\n is 4 bytes and may straddle a chunk boundary),
        // avoiding an O(n²) full-buffer rescan per chunk under unbounded long frames.
        let mut scanned: usize = 0;
        let mut parser = SseParser::new();
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
                let frame: Vec<u8> = buf.drain(..from + body_end + sep_len).collect();
                if let Some(data) = extract_data(&frame[..from + body_end])? {
                    // feed returned Err: yield Err and terminate the stream (`?` operator behavior).
                    if let Some(event) = parser.feed(&data)? {
                        yield event;
                    }
                }
                // After cutting one frame the remaining bytes shift forward; keep cutting from the head (one chunk may hold several frames).
                from = 0;
            }
            scanned = buf.len();
        }
        // A trailing incomplete frame at end of stream is dropped per SSE convention.
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
fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
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
    use crate::{ChatRequest, ContentBlock, Message, Role, ToolSpec};
    use futures::StreamExt;

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
        assert_eq!(truncate_error_body(&short), short);
        // Overlong bodies truncate to 2000 by char
        let long = "y".repeat(3000);
        assert_eq!(truncate_error_body(&long).chars().count(), 2000);
        // Multibyte chars count by char and are never split (splitting would garble at the from_utf8 level)
        let wide = "é".repeat(2500);
        let truncated = truncate_error_body(&wide);
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
        let v = build_request_body(&req);
        assert_eq!(v["model"], "MiniMax-M3");
        assert_eq!(v["system"], "sys");
        assert_eq!(v["stream"], true);
        assert_eq!(v["max_tokens"], 8192);
        assert_eq!(v["messages"][1]["content"][0]["type"], "tool_use");
        assert_eq!(v["messages"][2]["content"][0]["type"], "tool_result");
        assert_eq!(v["tools"][0]["name"], "read_file");
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
        let v = build_request_body(&req);
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
