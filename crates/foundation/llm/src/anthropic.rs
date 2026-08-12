//! Anthropic Messages API 流式 HTTP 客户端。
//!
//! `POST {base_url}/v1/messages` 发起 SSE 流式请求，字节块流经缓冲切帧后
//! 交给 [`crate::SseParser`] 逐条解析为 [`crate::StreamEvent`]。

use std::time::Duration;

use futures::{Stream, StreamExt};

use crate::{ChatModel, ChatRequest, EventStream, LlmError, Result, SseParser, StreamEvent};

/// Anthropic Messages API 流式客户端。
pub struct AnthropicClient {
    base_url: String,
    api_key: String,
    http: reqwest::Client,
}

impl AnthropicClient {
    /// 新建客户端。
    pub fn new(base_url: String, api_key: String) -> Self {
        Self {
            base_url,
            api_key,
            http: build_http_client(),
        }
    }
}

/// 构造 HTTP 客户端。
///
/// - 只设 `connect_timeout`（10s）：防 connect 阶段停滞挂死；**不能**设
///   `Client::timeout`——那会掐断正常的长 SSE 流。流读停滞检测是 M2 待办。
/// - 禁用重定向：Messages API 端点无合法重定向语义，重定向即报错；reqwest
///   默认跟随重定向会把 `x-api-key` 带到跨源目标（PoC 实测），必须杜绝。
fn build_http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        // 与 reqwest 自带 `Client::new()` 一致：仅 TLS 初始化失败才可能走到这里。
        .expect("HTTP 客户端构造失败")
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
            return Err(LlmError::Api {
                kind: format!("http_{}", status.as_u16()),
                message: truncate_error_body(&body),
            });
        }

        let byte_stream = response
            .bytes_stream()
            .map(|r| r.map_err(|e| LlmError::Http(e.to_string())));
        Ok(Box::pin(decode_event_stream(byte_stream, MAX_SSE_BUF)))
    }
}

/// 拼接 Messages API URL：base_url 尾部 `/` 剔除，避免双斜杠。
fn messages_url(base_url: &str) -> String {
    format!("{}/v1/messages", base_url.trim_end_matches('/'))
}

/// 错误响应体最大保留字符数。
const MAX_ERROR_BODY_CHARS: usize = 2000;

/// 错误响应体截断：最多保留前 [`MAX_ERROR_BODY_CHARS`] 个字符（按 char 截断，
/// 不会切碎多字节字符）；API key 仅在请求头中，绝不写入错误信息。
fn truncate_error_body(body: &str) -> String {
    body.chars().take(MAX_ERROR_BODY_CHARS).collect()
}

/// 构造 Anthropic Messages API 请求体（stream 固定 true）。
pub(crate) fn build_request_body(req: &ChatRequest) -> serde_json::Value {
    serde_json::json!({
        "model": req.model,
        "system": req.system,
        // Arc<Vec<Message>> 快照解引用序列化（serde 的 rc feature 未开，
        // 无须为单点序列化扩 feature）。
        "messages": &*req.messages,
        "tools": req.tools,
        "max_tokens": req.max_tokens,
        "stream": true,
    })
}

/// SSE 字节缓冲硬上限（8 MiB）：超出即判定服务端未按 SSE 帧边界发数据，
/// yield Err 终止流——防恶意/异常服务端用无帧边界数据撑爆内存（OOM）。
const MAX_SSE_BUF: usize = 8 * 1024 * 1024;

/// 字节块流 → 事件流：缓冲字节、按空行切 SSE 帧、提取 data 交给 [`SseParser`]。
///
/// chunk 边界任意（TCP 可能把一帧拆成多个 chunk），帧切分必须在字节缓冲层做；
/// 缓冲达到 `max_buf` 仍无帧边界即 yield Err 终止流。
/// 本函数是 [`ChatModel::stream`] 与测试共用的核心解析路径。
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
        // buf[..scanned] 已确认不含完整帧边界，下轮从 scanned - 3 续扫即可
        //（回退 3 字节：最长分隔符 \r\n\r\n 为 4 字节，可能跨 chunk 边界），
        // 避免无界长帧下每个 chunk 全缓冲重扫的 O(n²)。
        let mut scanned: usize = 0;
        let mut parser = SseParser::new();
        while let Some(chunk) = byte_stream.next().await {
            let chunk = chunk?;
            buf.extend_from_slice(&chunk);
            if buf.len() > max_buf {
                // yield Err 并终止流（与下方 feed 的 ? 行为一致）。
                Err::<(), _>(LlmError::Sse(format!(
                    "SSE 缓冲超过 {max_buf} 字节上限（服务端未发送帧边界）"
                )))?;
            }
            let mut from = scanned.saturating_sub(3);
            while let Some((body_end, sep_len)) = find_frame_boundary(&buf[from..]) {
                let frame: Vec<u8> = buf.drain(..from + body_end + sep_len).collect();
                if let Some(data) = extract_data(&frame[..from + body_end])? {
                    // feed 返回 Err：yield Err 并终止流（? 运算符行为）。
                    if let Some(event) = parser.feed(&data)? {
                        yield event;
                    }
                }
                // 切走一帧后剩余字节前移，从头继续切（同一 chunk 可能含多帧）。
                from = 0;
            }
            scanned = buf.len();
        }
        // 流末尾的不完整帧按 SSE 惯例丢弃。
    }
}

/// 在缓冲中查找帧边界（`\n\n` 或 `\r\n\r\n`，取先出现者）。
///
/// 返回 `(帧体长度, 分隔符长度)`。
fn find_frame_boundary(buf: &[u8]) -> Option<(usize, usize)> {
    let lf = find_subsequence(buf, b"\n\n").map(|i| (i, 2));
    let crlf = find_subsequence(buf, b"\r\n\r\n").map(|i| (i, 4));
    match (lf, crlf) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (only, None) | (None, only) => only,
    }
}

/// 子串查找：返回 `needle` 在 `haystack` 中首次出现的下标。
fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// 从单条 SSE 帧提取 `data:` 负载（多行 data 以 `\n` 拼接）。
///
/// 无 data 行的帧（如纯 `event:` / 注释行）返回 `Ok(None)`。
fn extract_data(frame: &[u8]) -> Result<Option<String>> {
    let text = std::str::from_utf8(frame).map_err(|e| LlmError::Sse(e.to_string()))?;
    let mut data_lines: Vec<&str> = Vec::new();
    for line in text.split('\n') {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if let Some(payload) = line.strip_prefix("data:") {
            // SSE 规范：冒号后可有至多一个前导空格。
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

    /// 测试辅助：把字符串块转成字节块流，喂给实现内部的核心解析函数
    /// （与 `stream()` 共用同一解析路径），收集全部 Ok 事件。
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
        // 超长按 char 截断到 2000
        let long = "y".repeat(3000);
        assert_eq!(truncate_error_body(&long).chars().count(), 2000);
        // 多字节字符按 char 计，不会被切碎（否则 from_utf8 层面就乱码了）
        let wide = "汉".repeat(2500);
        let truncated = truncate_error_body(&wide);
        assert_eq!(truncated.chars().count(), 2000);
        assert!(truncated.chars().all(|c| c == '汉'));
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
        // SSE 帧被 TCP 拆成两段到达，解析不得丢事件
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
        // 纯 CRLF 帧 + LF/CRLF 混合流，事件均不得丢失
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
        // 一帧内多行 data: 以 \n 拼接后交给 parser；本例拼接后仍是合法 JSON
        //（\n 落在 JSON token 之间），应正常解析出事件——锁定"拼接"行为本身。
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
        // 4 字节分隔符 \r\n\r\n 以 3 字节一片喂入必跨 chunk：
        // 锁定续扫回退（scanned - 3）逻辑的正确性，事件不得丢失。
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
        // 缓冲超上限：yield Err 并终止流——第三个 chunk 不再被消费，
        // 若只报错不终止，结果里会多出后续 chunk 的错误项。
        let chunk = || Ok::<_, LlmError>(bytes::Bytes::from_static(b"data: no-boundary-here\n"));
        let byte_stream = futures::stream::iter([chunk(), chunk(), chunk()]);
        let results: Vec<_> = decode_event_stream(byte_stream, 32).collect().await;
        assert_eq!(results.len(), 1, "超限后流应立即终止: {results:?}");
        assert!(
            matches!(&results[0], Err(LlmError::Sse(msg)) if msg.contains("上限")),
            "应报缓冲超限错误: {:?}",
            results[0]
        );
    }

    /// 回归测试（审查批 A2）：禁重定向防 `x-api-key` 泄露到跨源目标。
    ///
    /// A 服务对 POST 返回 301 指向 B；断言客户端直接报错（http_301）而非跟随，
    /// 且 B 永远收不到请求（reqwest 默认跟随重定向会携带 `x-api-key`，PoC 实测）。
    #[tokio::test]
    async fn redirect_is_not_followed_and_api_key_not_leaked() {
        use std::io::Write;
        use std::net::TcpListener;
        use std::sync::mpsc;

        // B：重定向目标服务，收到请求即把请求头全文送回主线程。
        let (b_tx, b_rx) = mpsc::channel::<String>();
        let listener_b = TcpListener::bind("127.0.0.1:0").unwrap();
        let port_b = listener_b.local_addr().unwrap().port();
        std::thread::spawn(move || {
            if let Ok((mut s, _)) = listener_b.accept() {
                let head = read_http_request_head(&mut s);
                let _ = b_tx.send(head);
            }
        });

        // A：入口服务，记录请求头（证明 key 确实发给了 A）后回 301 指向 B。
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
            Ok(_) => panic!("301 应直接报错而非跟随重定向"),
            Err(e) => e,
        };
        // 禁重定向后 301 作为普通响应返回，stream() 按非 2xx 报错。
        assert!(
            matches!(&err, LlmError::Api { kind, .. } if kind.as_str() == "http_301"),
            "301 应报 http_301 而非跟随: {err:?}"
        );
        // 前置条件：A 确实收到了带 key 的请求（否则本测试无意义）。
        let head_a = a_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        assert!(
            head_a.contains("x-api-key: sk-secret-key"),
            "A 应收到带 key 的请求头: {head_a}"
        );
        // B 永远收不到请求：本地回环 500ms 无连接即视为未跟随。
        assert!(
            b_rx.recv_timeout(std::time::Duration::from_millis(500))
                .is_err(),
            "重定向被跟随，api key 泄露到了 B"
        );
    }

    /// 测试辅助：读取 HTTP 请求头（至 `\r\n\r\n`），并按 Content-Length 读完
    /// body——不回读 body 就提前响应/关连接，客户端写 body 时可能撞 RST。
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
