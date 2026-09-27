//! Shared JSON-RPC 2.0 framing for the gateway's stdio surfaces.
//!
//! [`crate::acp`] and [`crate::mcp_serve`] both speak newline-delimited
//! JSON-RPC over stdio; this module holds the envelope helpers and the
//! bounded line reader so shapes and limits cannot drift apart between
//! the two surfaces.

/// Upper bound for one incoming NDJSON line (1 MiB — generous for tool
/// payloads, far below what a newline-less peer could otherwise allocate).
pub(crate) const MAX_LINE_BYTES: usize = 1024 * 1024;

/// `jsonrpc` invalid-request error code, used when no request could be
/// parsed far enough to echo an id.
pub(crate) const INVALID_REQUEST: i32 = -32600;

use tokio::io::AsyncBufReadExt as _;

/// One read from a stdio peer.
#[derive(Debug)]
pub(crate) enum IncomingLine {
    /// The peer closed the stream.
    Eof,
    /// A complete line (newline stripped; a final unterminated line counts).
    Line(String),
    /// The line exceeded [`MAX_LINE_BYTES`]. The over-cap remainder is
    /// discarded through the next newline (or EOF) so the serve loop can
    /// answer with a protocol error and stay in sync.
    TooLong,
}

/// Read one line without letting a newline-less peer grow memory without
/// bound — `AsyncBufReadExt::read_line` would buffer unboundedly, which
/// turns any peer (or a forwarded MCP server's stdout) into an allocation
/// denial-of-service.
pub(crate) async fn read_line_capped<R>(reader: &mut R) -> std::io::Result<IncomingLine>
where
    R: tokio::io::AsyncBufRead + Unpin,
{
    let mut bytes: Vec<u8> = Vec::new();
    let mut overflow = false;
    loop {
        let (found_newline, consume_len) = {
            let available = reader.fill_buf().await?;
            if available.is_empty() {
                // EOF: a pending unterminated line still counts, but an
                // empty buffer is a true EOF (read_line's `Ok(0)`).
                return if overflow {
                    Ok(IncomingLine::TooLong)
                } else if bytes.is_empty() {
                    Ok(IncomingLine::Eof)
                } else {
                    finish_line(bytes)
                };
            }
            match available.iter().position(|&b| b == b'\n') {
                Some(pos) => {
                    if !overflow {
                        if bytes.len() + pos > MAX_LINE_BYTES {
                            overflow = true;
                        } else {
                            bytes.extend_from_slice(&available[..pos]);
                        }
                    }
                    (true, pos + 1)
                }
                None => {
                    if !overflow {
                        if bytes.len() + available.len() > MAX_LINE_BYTES {
                            overflow = true;
                        } else {
                            bytes.extend_from_slice(available);
                        }
                    }
                    (false, available.len())
                }
            }
        };
        reader.consume(consume_len);
        if found_newline {
            return if overflow {
                Ok(IncomingLine::TooLong)
            } else {
                finish_line(bytes)
            };
        }
    }
}

/// Decode a capped byte line as UTF-8 (matching the `read_line` contract
/// this reader replaces).
fn finish_line(bytes: Vec<u8>) -> std::io::Result<IncomingLine> {
    match String::from_utf8(bytes) {
        Ok(text) => Ok(IncomingLine::Line(text)),
        Err(_) => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "line is not UTF-8",
        )),
    }
}

/// Request id echoed back, or null when absent (notifications/errors).
pub(crate) fn id_or_null(object: &serde_json::Map<String, serde_json::Value>) -> serde_json::Value {
    object.get("id").cloned().unwrap_or(serde_json::Value::Null)
}

/// Success envelope for one request id.
pub(crate) fn success_response(
    id: serde_json::Value,
    result: serde_json::Value,
) -> serde_json::Value {
    serde_json::json!({"jsonrpc": "2.0", "id": id, "result": result})
}

/// Error envelope for one request id (null when no id can be echoed).
pub(crate) fn error_response(
    id: serde_json::Value,
    code: i32,
    message: impl Into<String>,
) -> serde_json::Value {
    serde_json::json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message.into()}})
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Lines, blank lines, and EOF all read back exactly as the peer sent
    /// them over a duplex that the peer then drops (the shutdown path the
    /// serve loops rely on).
    #[tokio::test]
    async fn lines_then_eof_read_back_over_duplex() {
        use tokio::io::AsyncWriteExt as _;
        let (mut client_write, server_read) = tokio::io::duplex(64 * 1024);
        let mut reader = tokio::io::BufReader::new(server_read);
        client_write.write_all(b"hello\n\nworld\n").await.unwrap();
        drop(client_write);
        assert!(matches!(
            read_line_capped(&mut reader).await.unwrap(),
            IncomingLine::Line(l) if l == "hello"
        ));
        assert!(matches!(
            read_line_capped(&mut reader).await.unwrap(),
            IncomingLine::Line(l) if l.is_empty()
        ));
        assert!(matches!(
            read_line_capped(&mut reader).await.unwrap(),
            IncomingLine::Line(l) if l == "world"
        ));
        assert!(matches!(
            read_line_capped(&mut reader).await.unwrap(),
            IncomingLine::Eof
        ));
    }

    /// One newline-less frame beyond the cap must surface as `TooLong`
    /// (and swallow the rest through EOF), never grow memory without
    /// bound — the allocation-denial case the cap exists for. The writer
    /// runs concurrently because the duplex buffer is far smaller than
    /// the oversize frame.
    #[tokio::test]
    async fn oversized_line_reports_too_long_and_resyncs() {
        use tokio::io::AsyncWriteExt as _;
        let (mut client_write, server_read) = tokio::io::duplex(64 * 1024);
        let mut reader = tokio::io::BufReader::new(server_read);
        let writer_task = tokio::spawn(async move {
            let junk = vec![b'x'; MAX_LINE_BYTES + 1];
            client_write.write_all(&junk).await.unwrap();
            client_write.write_all(b"\nnext\n").await.unwrap();
        });
        assert!(matches!(
            read_line_capped(&mut reader).await.unwrap(),
            IncomingLine::TooLong
        ));
        assert!(matches!(
            read_line_capped(&mut reader).await.unwrap(),
            IncomingLine::Line(l) if l == "next"
        ));
        writer_task.await.unwrap();
    }
}
