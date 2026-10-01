//! Shared fixtures for the provider client tests.
//!
//! Compiled only as part of the crate's unit tests (`#[cfg(test)]` on the
//! module declaration). Redirect-rejection tests in the Anthropic and
//! Responses clients both need to read a raw HTTP request head.

use std::io::Read;
use std::net::TcpStream;
use std::time::Duration;

/// Read an HTTP request head (up to `\r\n\r\n`) and then the body named by
/// `Content-Length`.
///
/// Responding or closing before the body is read risks a connection reset
/// while the client is still writing.
pub(crate) fn read_http_request_head(stream: &mut TcpStream) -> String {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let mut buf: Vec<u8> = Vec::new();
    let mut tmp = [0u8; 4096];
    let head_len = loop {
        match stream.read(&mut tmp) {
            Ok(0) | Err(_) => return String::from_utf8_lossy(&buf).into_owned(),
            Ok(n) => {
                buf.extend_from_slice(&tmp[..n]);
                if let Some(i) = crate::sse::find_subsequence(&buf, b"\r\n\r\n") {
                    break i + 4;
                }
            }
        }
    };
    let head = String::from_utf8_lossy(&buf[..head_len]).into_owned();
    let content_length: usize = head
        .lines()
        .find_map(|line| {
            line.to_ascii_lowercase()
                .strip_prefix("content-length:")
                .and_then(|value| value.trim().parse().ok())
        })
        .unwrap_or(0);
    while buf.len() < head_len + content_length {
        match stream.read(&mut tmp) {
            Ok(0) | Err(_) => break,
            Ok(n) => buf.extend_from_slice(&tmp[..n]),
        }
    }
    head
}
