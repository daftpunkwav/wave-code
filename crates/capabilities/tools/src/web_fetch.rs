/*! @file WebFetch
 * @description HTTP(S) URL fetch tool with size, redirect caps, and HTML
 * to Markdown conversion.
 *
 * Responsibilities:
 * - Fetch http/https URLs with manual redirect following (max 5 hops)
 * - Enforce a streaming size cap with a truncation marker
 * - Decode bodies as text with best-effort charset handling
 * - Render HTML responses as Markdown before they enter model context
 *   (raw=true opts out)
 *
 * This module must not depend on: UI-layer components, filesystem writes.
 */

//! `web_fetch` tool (read-only): fetch a URL as text. Only `http`/`https`
//! schemes are accepted (`file`/`ftp`/`data` URLs are business errors).
//! Redirects are followed manually up to 5 hops; the body is streamed with
//! a size cap (default 256 KB, hard cap 1 MB) and cut with a `[truncated]`
//! marker. Text decoding is UTF-8 with lossy fallback, except explicit
//! latin-1 family charsets which decode byte-to-codepoint.

use std::time::Duration;

use serde_json::{Value, json};

use crate::{Result, Tool, ToolCtx, ToolOutput};

/// Default body cap: 256 KB.
const DEFAULT_MAX_BYTES: usize = 256 * 1024;
/// Hard body cap: 1 MB, larger requests are clamped.
const CAP_MAX_BYTES: usize = 1024 * 1024;
/// Default request timeout: 30 s.
const DEFAULT_TIMEOUT_MS: u64 = 30_000;
/// Timeout ceiling: 300 s, values above are clamped.
const MAX_TIMEOUT_MS: u64 = 300_000;
/// Maximum redirect hops followed before giving up.
const MAX_REDIRECTS: u32 = 5;

/// Build a business-failure output so the model can self-correct.
fn err_output(reason: impl Into<String>) -> ToolOutput {
    ToolOutput {
        content: reason.into(),
        is_error: true,
    }
}

/// Build a success output.
fn ok_output(content: impl Into<String>) -> ToolOutput {
    ToolOutput {
        content: content.into(),
        is_error: false,
    }
}

/// Parse `max_bytes` (default 256 KB, clamped to 1 MB).
fn resolve_max_bytes(input: &Value) -> std::result::Result<usize, ToolOutput> {
    match input.get("max_bytes") {
        None | Some(Value::Null) => Ok(DEFAULT_MAX_BYTES),
        Some(v) => match v.as_u64() {
            Some(n) => Ok((n as usize).min(CAP_MAX_BYTES)),
            None => Err(err_output(
                "invalid parameter 'max_bytes' (non-negative integer required)",
            )),
        },
    }
}

/// Parse `timeout_ms` (default 30 s, clamped to 300 s).
fn resolve_timeout_ms(input: &Value) -> std::result::Result<u64, ToolOutput> {
    match input.get("timeout_ms") {
        None | Some(Value::Null) => Ok(DEFAULT_TIMEOUT_MS),
        Some(v) => match v.as_u64() {
            Some(n) => Ok(n.min(MAX_TIMEOUT_MS)),
            None => Err(err_output(
                "invalid parameter 'timeout_ms' (non-negative integer required)",
            )),
        },
    }
}

/// Validate the URL: must parse and use `http`/`https`. Anything else
/// (`file`, `ftp`, `data`, ...) is a business error.
fn validate_url(input: &Value) -> std::result::Result<reqwest::Url, ToolOutput> {
    let raw = input
        .get("url")
        .and_then(Value::as_str)
        .ok_or_else(|| err_output("missing or invalid parameter 'url' (string required)"))?;
    let url =
        reqwest::Url::parse(raw).map_err(|e| err_output(format!("invalid URL '{raw}': {e}")))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(err_output(format!(
            "unsupported URL scheme '{}': only http and https are allowed",
            url.scheme()
        )));
    }
    if let Some(host) = url.host_str()
        && is_link_local_host(host)
    {
        // Link-local targets (cloud metadata 169.254.169.254, fe80::/10)
        // are unreachable-by-design for a web-reading tool, and this tool
        // is read-only in every permission mode — an injected prompt must
        // not be able to pivot at instance credentials or local services.
        return Err(err_output(format!(
            "refusing to fetch link-local host '{host}': this tool never \
             reaches cloud metadata or local-link addresses"
        )));
    }
    Ok(url)
}

/// True when the host string is a link-local IP literal: the check holds
/// without DNS.
///
/// Scope: link-local only (IPv4 169.254/16 — including cloud instance
/// metadata at 169.254.169.254 — and IPv6 fe80::/10). Loopback and
/// RFC1918 targets stay reachable so local dev servers keep working,
/// and public DNS names resolving into link-local ranges are not caught
/// (blocking those would require pinning resolved IPs at connect time).
fn is_link_local_host(host: &str) -> bool {
    // Trim IPv6 brackets and one trailing root dot (`example.com.`).
    let host = host.trim().trim_start_matches('[').trim_end_matches(']');
    let host = host.strip_suffix('.').unwrap_or(host);
    match host.parse::<std::net::IpAddr>() {
        Ok(std::net::IpAddr::V4(v4)) => v4.is_link_local(),
        Ok(std::net::IpAddr::V6(v6)) => {
            // IPv4-mapped IPv6 (::ffff:a.b.c.d) connects as plain IPv4, so
            // it must be judged by the embedded v4 address, not its v6
            // prefix.
            if let Some(v4) = v6.to_ipv4_mapped() {
                return v4.is_link_local();
            }
            v6.segments()[0] & 0xffc0 == 0xfe80
        }
        Err(_) => false,
    }
}

/// Decode a body as text: explicit latin-1 family charsets decode each
/// byte to the same codepoint, everything else is UTF-8 (lossy fallback
/// so a truncation-split multibyte tail cannot fail the fetch).
fn decode_body(bytes: &[u8], content_type: Option<&str>) -> String {
    let charset = content_type.and_then(|ct| {
        ct.split(';').skip(1).find_map(|param| {
            let param = param.trim();
            let raw = param
                .strip_prefix("charset=")
                .or_else(|| param.strip_prefix("CHARSET="))?;
            Some(
                raw.trim()
                    .trim_matches('"')
                    .trim_matches('\'')
                    .to_lowercase(),
            )
        })
    });
    match charset.as_deref() {
        Some("iso-8859-1" | "latin-1" | "latin1" | "windows-1252" | "cp1252") => {
            bytes.iter().map(|&b| b as char).collect()
        }
        _ => String::from_utf8_lossy(bytes).into_owned(),
    }
}

/// Fetch a URL as text (read-only).
pub struct WebFetch;

#[async_trait::async_trait]
impl Tool for WebFetch {
    fn name(&self) -> &str {
        "web_fetch"
    }

    fn description(&self) -> &str {
        "Fetch an http/https URL and return it as text. HTML pages are \
         converted to Markdown (pass raw=true to get the original HTML). \
         Redirects are followed (up to 5 hops); other schemes (file, ftp, \
         data, ...) are rejected. Use max_bytes to bound the body (default \
         262144, clamped to max 1048576); oversize bodies are cut with a \
         [truncated] marker. Use timeout_ms to bound the whole fetch \
         (default 30000 ms, clamped to max 300000 ms)."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "url": {
                    "type": "string",
                    "description": "http/https URL to fetch"
                },
                "max_bytes": {
                    "type": "integer",
                    "description": "Maximum body bytes to return (default 262144, clamped to max 1048576)"
                },
                "timeout_ms": {
                    "type": "integer",
                    "description": "Timeout in milliseconds (default 30000, clamped to max 300000)"
                },
                "raw": {
                    "type": "boolean",
                    "description": "Return the original HTML instead of Markdown (default false)"
                }
            },
            "required": ["url"]
        })
    }

    fn is_read_only(&self) -> bool {
        true
    }

    async fn execute(&self, input: Value, _ctx: &ToolCtx) -> Result<ToolOutput> {
        let start = match validate_url(&input) {
            Ok(u) => u,
            Err(out) => return Ok(out),
        };
        let max_bytes = match resolve_max_bytes(&input) {
            Ok(n) => n,
            Err(out) => return Ok(out),
        };
        let timeout_ms = match resolve_timeout_ms(&input) {
            Ok(n) => n,
            Err(out) => return Ok(out),
        };
        let client = match reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_millis(timeout_ms))
            .build()
        {
            Ok(c) => c,
            Err(e) => return Ok(err_output(format!("failed to build HTTP client: {e}"))),
        };

        // Manual redirect loop so the hop cap and scheme policy stay explicit.
        let mut current = start;
        let mut hops: u32 = 0;
        let mut response = loop {
            let resp = match client.get(current.clone()).send().await {
                Ok(r) => r,
                Err(e) => return Ok(err_output(format!("request to {current} failed: {e}"))),
            };
            if !resp.status().is_redirection() {
                if !resp.status().is_success() {
                    return Ok(err_output(format!("HTTP {} for {current}", resp.status())));
                }
                break resp;
            }
            let location = match resp
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|v| v.to_str().ok())
            {
                Some(l) => l.to_owned(),
                None => return Ok(err_output("redirect without Location header")),
            };
            hops += 1;
            if hops > MAX_REDIRECTS {
                return Ok(err_output(format!(
                    "too many redirects (limit {MAX_REDIRECTS})"
                )));
            }
            let next = match current.join(&location) {
                Ok(u) => u,
                Err(e) => return Ok(err_output(format!("invalid redirect target: {e}"))),
            };
            if !matches!(next.scheme(), "http" | "https") {
                return Ok(err_output(format!(
                    "redirect to unsupported scheme '{}': only http and https are allowed",
                    next.scheme()
                )));
            }
            current = next;
        };

        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        // Stream with an enforced cap: stop at max_bytes, flag truncation.
        let mut body: Vec<u8> = Vec::new();
        let mut truncated = false;
        loop {
            match response.chunk().await {
                Ok(Some(chunk)) => {
                    let room = max_bytes.saturating_sub(body.len());
                    if chunk.len() > room {
                        body.extend_from_slice(&chunk[..room]);
                        truncated = true;
                        break;
                    }
                    body.extend_from_slice(&chunk);
                }
                Ok(None) => break,
                Err(e) => {
                    return Ok(err_output(format!("failed reading response body: {e}")));
                }
            }
        }
        let mut text = decode_body(&body, content_type.as_deref());
        if truncated {
            text.push_str("\n[truncated]");
        }
        // HTML pages are converted to Markdown before entering model
        // context: raw markup burns tokens on attributes and scripts while
        // carrying little meaning. `raw: true` opts out (verbatim HTML).
        let is_html = content_type.as_deref().is_some_and(|ct| {
            ct.split(';').next().map(str::trim).is_some_and(|mime| {
                mime.eq_ignore_ascii_case("text/html")
                    || mime.eq_ignore_ascii_case("application/xhtml+xml")
            })
        }) || content_type.is_none()
            && text
                .trim_start()
                .to_ascii_lowercase()
                .starts_with("<!doctype html");
        if is_html && !input.get("raw").and_then(Value::as_bool).unwrap_or(false) {
            let converted = crate::html::html_to_markdown(&text);
            if truncated {
                return Ok(ok_output(format!("{converted}\n[truncated]")));
            }
            return Ok(ok_output(converted));
        }
        Ok(ok_output(text))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// Minimal HTTP stub: routes `/ok` (200), `/redirect` (302 to `/ok`),
    /// `/loop` (302 to itself), `/big` (200, 200 KB body). One request per
    /// connection, `Connection: close`.
    async fn stub_server() -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            loop {
                let (mut sock, _) = listener.accept().await.unwrap();
                tokio::spawn(async move {
                    let mut buf = Vec::new();
                    let mut tmp = [0u8; 1024];
                    loop {
                        let n = sock.read(&mut tmp).await.unwrap_or(0);
                        if n == 0 {
                            break;
                        }
                        buf.extend_from_slice(&tmp[..n]);
                        if buf.windows(4).any(|w| w == b"\r\n\r\n") || buf.len() > 16 * 1024 {
                            break;
                        }
                    }
                    let path = String::from_utf8_lossy(&buf)
                        .split_whitespace()
                        .nth(1)
                        .unwrap_or("/")
                        .to_owned();
                    let head_ok = "HTTP/1.1 200 OK\r\nContent-Type: text/plain; charset=utf-8\r\nConnection: close\r\n";
                    let reply = match path.as_str() {
                        "/redirect" => {
                            b"HTTP/1.1 302 Found\r\nLocation: /ok\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec()
                        }
                        "/loop" => {
                            b"HTTP/1.1 302 Found\r\nLocation: /loop\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec()
                        }
                        "/big" => {
                            let body = vec![b'x'; 200 * 1024];
                            let mut out = format!(
                                "{head_ok}Content-Length: {}\r\n\r\n",
                                body.len()
                            )
                            .into_bytes();
                            out.extend_from_slice(&body);
                            out
                        }
                        "/ok" => {
                            let mut out = format!("{head_ok}Content-Length: 11\r\n\r\n").into_bytes();
                            out.extend_from_slice(b"hello fetch");
                            out
                        }
                        "/html" => {
                            let body = b"<html><head><title>T</title></head><body><h1>Hi</h1><p>Body <a href=\"/x\">link</a></p><script>var q=1;</script></body></html>";
                            let mut out = b"HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nConnection: close\r\n".to_vec();
                            out.extend_from_slice(format!("Content-Length: {}\r\n\r\n", body.len()).as_bytes());
                            out.extend_from_slice(body);
                            out
                        }
                        _ => {
                            b"HTTP/1.1 404 Not Found\r\nContent-Length: 9\r\nConnection: close\r\n\r\nnot found".to_vec()
                        }
                    };
                    let _ = sock.write_all(&reply).await;
                });
            }
        });
        (addr, handle)
    }

    fn ctx() -> (tempfile::TempDir, ToolCtx) {
        let dir = tempfile::tempdir().unwrap();
        let c = ToolCtx {
            cwd: dir.path().to_path_buf(),
            deny_env: Vec::new(),
        };
        (dir, c)
    }

    #[test]
    fn limits_default_and_clamp() {
        assert_eq!(
            resolve_max_bytes(&serde_json::json!({})).unwrap(),
            DEFAULT_MAX_BYTES
        );
        assert_eq!(
            resolve_max_bytes(&serde_json::json!({"max_bytes": null})).unwrap(),
            DEFAULT_MAX_BYTES
        );
        assert_eq!(
            resolve_max_bytes(&serde_json::json!({"max_bytes": 1024})).unwrap(),
            1024
        );
        assert_eq!(
            resolve_max_bytes(&serde_json::json!({"max_bytes": 64 * 1024 * 1024})).unwrap(),
            CAP_MAX_BYTES
        );
        assert!(resolve_max_bytes(&serde_json::json!({"max_bytes": -1})).is_err());
        assert_eq!(
            resolve_timeout_ms(&serde_json::json!({})).unwrap(),
            DEFAULT_TIMEOUT_MS
        );
        assert_eq!(
            resolve_timeout_ms(&serde_json::json!({"timeout_ms": 999_999_999})).unwrap(),
            MAX_TIMEOUT_MS
        );
        assert!(resolve_timeout_ms(&serde_json::json!({"timeout_ms": "fast"})).is_err());
    }

    #[test]
    fn url_policy_rejects_non_http() {
        for raw in [
            "file:///etc/passwd",
            "ftp://example.com/a",
            "data:text/plain,hi",
            "gopher://example.com/",
        ] {
            let out = validate_url(&serde_json::json!({"url": raw})).expect_err("must reject");
            assert!(
                out.content.contains("only http and https"),
                "{raw}: {}",
                out.content
            );
        }
        assert!(validate_url(&serde_json::json!({"url": "https://example.com/"})).is_ok());
        assert!(validate_url(&serde_json::json!({})).is_err());
        assert!(validate_url(&serde_json::json!({"url": "://bad"})).is_err());
    }

    #[test]
    fn decode_prefers_declared_latin_charset() {
        // 0xE9 is e-acute in latin-1 but invalid alone in UTF-8.
        let latin = decode_body(b"caf\xe9", Some("text/plain; charset=iso-8859-1"));
        assert_eq!(latin, "café");
        let lossy = decode_body(b"caf\xe9", Some("text/plain; charset=utf-8"));
        assert!(lossy.contains('\u{FFFD}'));
        let plain = decode_body("héllo".as_bytes(), None);
        assert_eq!(plain, "héllo");
    }

    #[test]
    fn link_local_and_metadata_hosts_are_refused() {
        for url in [
            "http://169.254.169.254/latest/meta-data/",
            "http://169.254.1.1/x",
            "http://[fe80::1]/",
            // IPv4-mapped IPv6 literals connect as plain IPv4: the
            // embedded address must be judged, not the v6 prefix.
            "http://[::ffff:169.254.169.254]/",
            "http://[::ffff:a9fe:a9fe]/",
        ] {
            let out = validate_url(&serde_json::json!({"url": url}))
                .err()
                .unwrap_or_else(|| panic!("'{url}' must be refused"));
            assert!(out.is_error, "'{url}'");
        }
    }

    #[test]
    fn loopback_private_and_public_hosts_still_pass_validation() {
        for url in [
            "https://example.com/x",
            "http://localhost/admin",  // loopback stays reachable (dev servers)
            "http://127.0.0.1:3000/",  // same
            "http://192.168.1.10/dev", // RFC1918 stays reachable
            "http://example.com.",     // root dot is trimmed, not link-local
        ] {
            assert!(
                validate_url(&serde_json::json!({"url": url})).is_ok(),
                "'{url}' must pass"
            );
        }
    }

    #[tokio::test]
    async fn fetch_ok_returns_text() {
        let (addr, server) = stub_server().await;
        let (_d, c) = ctx();
        let out = WebFetch
            .execute(serde_json::json!({"url": format!("http://{addr}/ok")}), &c)
            .await
            .unwrap();
        server.abort();
        assert!(!out.is_error, "unexpected failure: {}", out.content);
        assert!(out.content.contains("hello fetch"));
        assert!(!out.content.contains("[truncated]"));
    }

    /// HTML responses are converted to Markdown by default; `raw: true`
    /// keeps the original markup.
    #[tokio::test]
    async fn fetch_converts_html_to_markdown_unless_raw() {
        let (addr, server) = stub_server().await;
        let (_d, c) = ctx();
        let url = format!("http://{addr}/html");
        let out = WebFetch
            .execute(serde_json::json!({"url": url}), &c)
            .await
            .unwrap();
        assert!(!out.is_error, "unexpected failure: {}", out.content);
        assert!(
            out.content.contains("# Hi"),
            "heading converted: {}",
            out.content
        );
        assert!(
            out.content.contains("[link](/x)"),
            "link converted: {}",
            out.content
        );
        assert!(
            !out.content.contains("var q=1"),
            "script dropped: {}",
            out.content
        );
        assert!(
            !out.content.contains("<p>"),
            "markup must not leak: {}",
            out.content
        );
        let out = WebFetch
            .execute(serde_json::json!({"url": url, "raw": true}), &c)
            .await
            .unwrap();
        server.abort();
        assert!(
            out.content.contains("<h1>"),
            "raw keeps markup: {}",
            out.content
        );
    }

    #[tokio::test]
    async fn fetch_follows_redirect() {
        let (addr, server) = stub_server().await;
        let (_d, c) = ctx();
        let out = WebFetch
            .execute(
                serde_json::json!({"url": format!("http://{addr}/redirect")}),
                &c,
            )
            .await
            .unwrap();
        server.abort();
        assert!(!out.is_error, "unexpected failure: {}", out.content);
        assert!(out.content.contains("hello fetch"));
    }

    #[tokio::test]
    async fn fetch_truncates_oversize_body() {
        let (addr, server) = stub_server().await;
        let (_d, c) = ctx();
        let out = WebFetch
            .execute(
                serde_json::json!({"url": format!("http://{addr}/big"), "max_bytes": 1024}),
                &c,
            )
            .await
            .unwrap();
        server.abort();
        assert!(!out.is_error, "unexpected failure: {}", out.content);
        assert!(out.content.ends_with("[truncated]"));
        let body = out.content.strip_suffix("\n[truncated]").unwrap();
        assert_eq!(body.len(), 1024);
    }

    #[tokio::test]
    async fn fetch_rejects_redirect_loop_and_http_errors() {
        let (addr, server) = stub_server().await;
        let (_d, c) = ctx();
        let out = WebFetch
            .execute(
                serde_json::json!({"url": format!("http://{addr}/loop")}),
                &c,
            )
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(out.content.contains("too many redirects"));
        let out = WebFetch
            .execute(
                serde_json::json!({"url": format!("http://{addr}/missing")}),
                &c,
            )
            .await
            .unwrap();
        server.abort();
        assert!(out.is_error);
        assert!(out.content.contains("404"));
    }
}
