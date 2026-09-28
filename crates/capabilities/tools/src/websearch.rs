/*! @file WebSearch
 * @description Pluggable web-search tool with a keyless default backend.
 *
 * Responsibilities:
 * - Expose `web_search {query, count?}` as a read-only model tool
 * - Query the DuckDuckGo HTML endpoint by default (no API key), parse titles
 * - Keep backends pluggable behind `SearchBackend` for keyed APIs later
 *
 * This module must not depend on: UI-layer components, filesystem writes.
 */

//! `web_search` tool (read-only): search the web and return titles/URLs/snippets.
//! No per-result fetch (the `web_fetch` tool already covers fetching).
//!
//! Backend tradeoff: [`DuckDuckGoBackend`] scrapes the keyless HTML endpoint
//! (`https://html.duckduckgo.com/html/?q=...`), so search works with no API
//! key; the HTML shape is unofficial and may change, hence parse failures and
//! blocks are business errors (fed back to the model) and keyed APIs can plug
//! in later behind [`SearchBackend`] without touching the tool.

use std::sync::{Arc, LazyLock};
use std::time::Duration;

use serde_json::{Value, json};

use crate::{Result, Tool, ToolCtx, ToolOutput, err_output};

/// Shared HTTP client for search requests. The connection pool, DNS
/// cache, and TLS session state would otherwise be discarded with every
/// per-call client build. Redirects stay disabled (policy is per-call
/// unchanged) and the timeout rides each request, since callers pass
/// one per search.
static SEARCH_CLIENT: LazyLock<std::result::Result<reqwest::Client, reqwest::Error>> =
    LazyLock::new(|| {
        reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
    });

/// Default result count.
const DEFAULT_COUNT: usize = 5;
/// Maximum result count (larger requests are business errors, not clamped:
/// silently returning fewer than asked would mislead the model).
const MAX_COUNT: usize = 10;
/// Default request timeout: 15 s (search must stay interactive).
const DEFAULT_TIMEOUT_MS: u64 = 15_000;
/// Timeout ceiling: 60 s, values above are clamped.
const MAX_TIMEOUT_MS: u64 = 60_000;

/// One search hit.
#[derive(Debug, Clone, PartialEq)]
pub struct SearchResult {
    /// Result title (entity-decoded).
    pub title: String,
    /// Destination URL (redirect wrappers unwrapped).
    pub url: String,
    /// Text snippet (entity-decoded, may be empty).
    pub snippet: String,
}

/// Pluggable search backend (keyed APIs implement this later without touching
/// the `web_search` tool surface).
#[async_trait::async_trait]
pub trait SearchBackend: Send + Sync {
    /// Run `query`, returning at most `count` hits or a human-readable reason.
    async fn search(
        &self,
        query: &str,
        count: usize,
        timeout: Duration,
    ) -> std::result::Result<Vec<SearchResult>, String>;
}

/// DuckDuckGo HTML backend (default, no key): GETs
/// `{base_url}/html/?q=<query>` and parses the result anchors.
pub struct DuckDuckGoBackend {
    /// Base URL (production: `https://html.duckduckgo.com`; tests inject a stub).
    pub base_url: String,
}

impl DuckDuckGoBackend {
    /// Production endpoint.
    pub fn new() -> Self {
        Self {
            base_url: "https://html.duckduckgo.com".to_owned(),
        }
    }

    /// Test/stub endpoint override.
    pub fn with_base_url(base_url: String) -> Self {
        Self { base_url }
    }
}

impl Default for DuckDuckGoBackend {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl SearchBackend for DuckDuckGoBackend {
    async fn search(
        &self,
        query: &str,
        count: usize,
        timeout: Duration,
    ) -> std::result::Result<Vec<SearchResult>, String> {
        let client = match SEARCH_CLIENT.as_ref() {
            Ok(client) => client,
            Err(e) => return Err(format!("failed to build HTTP client: {e}")),
        };
        let url = format!(
            "{}/html/?q={}",
            self.base_url.trim_end_matches('/'),
            percent_encode(query)
        );
        let resp = client
            .get(&url)
            .timeout(timeout)
            .header("User-Agent", "Mozilla/5.0 (compatible; WaveCode/1.0)")
            .send()
            .await
            .map_err(|e| format!("search request failed: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!(
                "search blocked (HTTP {} for the search endpoint)",
                resp.status()
            ));
        }
        let html = resp
            .text()
            .await
            .map_err(|e| format!("failed reading search response: {e}"))?;
        let hits = parse_ddg_html(&html, count);
        if hits.is_empty() && !looks_like_ddg_results(&html) {
            return Err("search parse failed: no recognizable results in the response".to_owned());
        }
        Ok(hits)
    }
}

/// Percent-encode a query string (alphanumerics plus `-_.~` pass through).
fn percent_encode(raw: &str) -> String {
    let mut out = String::new();
    for b in raw.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else if b == b' ' {
            out.push('+');
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Percent-decode `%XX` plus `+` (best-effort; malformed sequences pass through).
fn percent_decode(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let (Some(h), Some(l)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2]))
        {
            out.push(h * 16 + l);
            i += 3;
            continue;
        }
        out.push(if bytes[i] == b'+' { b' ' } else { bytes[i] });
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Decode HTML entities: `&amp; &lt; &gt; &quot; &#39;` plus decimal/hex
/// numeric references; unknown entities pass through untouched.
pub fn decode_entities(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut rest = raw;
    while let Some(start) = rest.find('&') {
        out.push_str(&rest[..start]);
        let tail = &rest[start..];
        let end = tail.find(';').map(|i| start + i);
        match end {
            Some(end) => {
                let entity = &rest[start..=end];
                out.push_str(&decode_one_entity(entity).unwrap_or_else(|| entity.to_owned()));
                rest = &rest[end + 1..];
            }
            None => {
                out.push_str(tail);
                rest = "";
                break;
            }
        }
    }
    // Trailing text after the last entity (or all of `raw` when entity-free).
    out.push_str(rest);
    out
}

fn decode_one_entity(entity: &str) -> Option<String> {
    match entity {
        "&amp;" => Some("&".to_owned()),
        "&lt;" => Some("<".to_owned()),
        "&gt;" => Some(">".to_owned()),
        "&quot;" => Some("\"".to_owned()),
        "&apos;" | "&#39;" | "&#x27;" | "&#X27;" => Some("'".to_owned()),
        _ => {
            let inner = entity.strip_prefix('&')?.strip_suffix(';')?;
            if let Some(num) = inner.strip_prefix('#') {
                let code =
                    if let Some(hex) = num.strip_prefix('x').or_else(|| num.strip_prefix('X')) {
                        u32::from_str_radix(hex, 16).ok()?
                    } else {
                        num.parse().ok()?
                    };
                Some(char::from_u32(code)?.to_string())
            } else {
                None
            }
        }
    }
}

/// Strip HTML tags (naive: drops `<...>` spans; good enough for snippets).
fn strip_tags(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut in_tag = false;
    for c in raw.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    out
}

/// Unwrap DDG redirect wrappers (`//duckduckgo.com/l/?uddg=<encoded>&...`)
/// into the destination URL; direct URLs pass through.
fn unwrap_url(href: &str) -> String {
    if let Some(pos) = href.find("uddg=") {
        let enc = &href[pos + 5..];
        let enc = enc.split('&').next().unwrap_or(enc);
        return percent_decode(enc);
    }
    if href.starts_with("//") {
        return format!("https:{href}");
    }
    href.to_owned()
}

/// Whether the HTML looks like a DDG results page at all (guards the
/// parse-failure business error: a block page / captcha carries none of these
/// markers). The anchors are the result classes for hit pages and the
/// `no-results` marker / "no results" text for empty pages. The bare word
/// "results" is deliberately NOT an anchor — it appears on unrelated pages
/// (nav text, block pages), which would mask a DOM redesign as a silent
/// "no results found".
fn looks_like_ddg_results(html: &str) -> bool {
    html.contains("result__a")
        || html.contains("result__snippet")
        || html.contains("no-results")
        || html.to_ascii_lowercase().contains("no results")
}

/// Result link pattern (`class="result__a"` anchors with their href);
/// compiled once, shared across searches.
static LINK_RE: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r#"<a[^>]*class="[^"]*result__a[^"]*"[^>]*href="([^"]*)"[^>]*>(.*?)</a>"#)
        .expect("link regex compiles")
});
/// Snippet pattern (`class="result__snippet"` anchors); compiled once.
static SNIPPET_RE: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r#"<a[^>]*class="[^"]*result__snippet[^"]*"[^>]*>(.*?)</a>"#)
        .expect("snippet regex compiles")
});

/// Parse DDG HTML into hits (pure function, hermetic tests target this):
/// result anchors (`class="result__a"`) pair with the next snippet anchor
/// (`class="result__snippet"`); at most `count` hits.
pub fn parse_ddg_html(html: &str, count: usize) -> Vec<SearchResult> {
    let snippets: Vec<String> = SNIPPET_RE
        .captures_iter(html)
        .map(|c| decode_entities(strip_tags(c[1].trim()).trim()))
        .collect();
    LINK_RE
        .captures_iter(html)
        .take(count)
        .enumerate()
        .map(|(i, c)| SearchResult {
            title: decode_entities(strip_tags(c[2].trim()).trim()),
            url: unwrap_url(&decode_entities(&c[1])),
            snippet: snippets.get(i).cloned().unwrap_or_default(),
        })
        .collect()
}

/// Build a success output.
fn ok_output(content: impl Into<String>) -> ToolOutput {
    ToolOutput {
        content: content.into(),
        is_error: false,
    }
}

/// Parse `count` (default 5, max 10; over-max and zero are business errors).
fn resolve_count(input: &Value) -> std::result::Result<usize, ToolOutput> {
    match input.get("count") {
        None | Some(Value::Null) => Ok(DEFAULT_COUNT),
        Some(v) => match v.as_u64() {
            Some(n) if n >= 1 && n <= MAX_COUNT as u64 => Ok(n as usize),
            _ => Err(err_output(format!(
                "invalid parameter 'count' (integer 1-{MAX_COUNT} required)"
            ))),
        },
    }
}

/// Parse `timeout_ms` (default 15 s, clamped to 60 s).
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

/// `web_search` tool (read-only): search the web, no per-result fetch.
pub struct WebSearch {
    backend: Arc<dyn SearchBackend>,
}

impl WebSearch {
    /// Build with the default DuckDuckGo backend.
    pub fn new() -> Self {
        Self {
            backend: Arc::new(DuckDuckGoBackend::new()),
        }
    }

    /// Build with an explicit backend (keyed APIs, test stubs).
    pub fn with_backend(backend: Arc<dyn SearchBackend>) -> Self {
        Self { backend }
    }
}

impl Default for WebSearch {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl Tool for WebSearch {
    fn name(&self) -> &str {
        "web_search"
    }

    fn description(&self) -> &str {
        "Search the web and return titles, URLs, and snippets (no per-result \
         fetch; use web_fetch to read a page). Use count to bound the hits \
         (default 5, max 10); use timeout_ms to bound the whole search \
         (default 15000 ms, clamped to max 60000 ms)."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Search query"
                },
                "count": {
                    "type": "integer",
                    "description": "Maximum hits to return (default 5, max 10)"
                },
                "timeout_ms": {
                    "type": "integer",
                    "description": "Timeout in milliseconds (default 15000, clamped to max 60000)"
                }
            },
            "required": ["query"]
        })
    }

    fn is_read_only(&self) -> bool {
        true
    }

    async fn execute(&self, input: Value, _ctx: &ToolCtx) -> Result<ToolOutput> {
        let query = input
            .get("query")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|q| !q.is_empty())
            .ok_or_else(|| {
                err_output("missing or invalid parameter 'query' (non-empty string required)")
            });
        let query = match query {
            Ok(q) => q,
            Err(out) => return Ok(out),
        };
        let count = match resolve_count(&input) {
            Ok(n) => n,
            Err(out) => return Ok(out),
        };
        let timeout_ms = match resolve_timeout_ms(&input) {
            Ok(n) => n,
            Err(out) => return Ok(out),
        };
        match self
            .backend
            .search(query, count, Duration::from_millis(timeout_ms))
            .await
        {
            Ok(hits) => {
                if hits.is_empty() {
                    return Ok(ok_output("no results found"));
                }
                let mut out = String::new();
                for (i, hit) in hits.iter().enumerate() {
                    out.push_str(&format!("{}. {}\n   {}\n", i + 1, hit.title, hit.url));
                    if !hit.snippet.is_empty() {
                        out.push_str(&format!("   {}\n", hit.snippet));
                    }
                }
                Ok(ok_output(out.trim_end().to_owned()))
            }
            Err(reason) => Ok(err_output(reason)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    const STUB_HTML: &str = r#"
<html><body>
<div class="results">
<div class="result">
<a class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Ffirst&amp;rut=x">First &amp; Result</a>
<a class="result__snippet" href="x">snippet with &lt;tags&gt; here</a>
</div>
<div class="result">
<a class="result__a" href="https://example.com/second">Second &#39;Result&#39;</a>
<a class="result__snippet" href="x">plain snippet</a>
</div>
</div>
</body></html>"#;

    #[test]
    fn parses_stub_html_with_unwrap_and_entities() {
        let hits = parse_ddg_html(STUB_HTML, 10);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].title, "First & Result");
        assert_eq!(hits[0].url, "https://example.com/first");
        assert_eq!(hits[0].snippet, "snippet with <tags> here");
        assert_eq!(hits[1].title, "Second 'Result'");
        assert_eq!(hits[1].url, "https://example.com/second");
    }

    #[test]
    fn parse_respects_count_and_rejects_block_pages() {
        assert_eq!(parse_ddg_html(STUB_HTML, 1).len(), 1);
        assert!(parse_ddg_html("<html><body>captcha</body></html>", 5).is_empty());
        assert!(!looks_like_ddg_results("<html><body>captcha</body></html>"));
    }

    /// The anchor set distinguishes the three page shapes: hits (result
    /// classes), an honest empty page (no-results marker / text), and an
    /// unrecognized page (DOM redesign, block page) that must surface as a
    /// parse failure instead of a silent "no results found".
    #[test]
    fn page_shape_anchors_distinguish_hits_empty_and_unrecognized() {
        // Hit pages carry the result anchors.
        assert!(looks_like_ddg_results(STUB_HTML));
        // An honest empty page: the no-results marker or the text form.
        assert!(looks_like_ddg_results(
            r#"<div class="no-results">Nothing here.</div>"#
        ));
        assert!(looks_like_ddg_results("<p>No results for that query.</p>"));
        assert!(looks_like_ddg_results("<p>NO RESULTS.</p>"));
        // A redesigned page that still mentions "results" in prose carries
        // none of the anchors: it must NOT read as an empty result page.
        let redesigned = r#"<html><body>View results on the new portal. results</body></html>"#;
        assert!(!looks_like_ddg_results(redesigned));
        assert!(parse_ddg_html(redesigned, 5).is_empty());
    }

    #[test]
    fn entity_decoding_covers_named_and_numeric() {
        assert_eq!(decode_entities("a &amp; b"), "a & b");
        assert_eq!(decode_entities("&#65;&#x42;"), "AB");
        assert_eq!(decode_entities("&unknown;"), "&unknown;");
    }

    #[test]
    fn count_validation() {
        assert_eq!(resolve_count(&json!({})).unwrap(), DEFAULT_COUNT);
        assert_eq!(resolve_count(&json!({"count": 3})).unwrap(), 3);
        assert!(resolve_count(&json!({"count": 0})).is_err());
        assert!(resolve_count(&json!({"count": 11})).is_err());
        assert!(resolve_count(&json!({"count": "many"})).is_err());
    }

    /// Stub search endpoint: `/html/` serves STUB_HTML, `/hang` never replies.
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
                    if path.starts_with("/hang") {
                        // Never reply: the client timeout must fire.
                        tokio::time::sleep(Duration::from_secs(30)).await;
                        return;
                    }
                    let body = STUB_HTML.as_bytes();
                    let reply = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = sock.write_all(reply.as_bytes()).await;
                    let _ = sock.write_all(body).await;
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

    #[tokio::test]
    async fn search_against_stub_returns_hits() {
        let (addr, _server) = stub_server().await;
        let tool = WebSearch::with_backend(Arc::new(DuckDuckGoBackend::with_base_url(format!(
            "http://127.0.0.1:{}",
            addr.port()
        ))));
        let (_d, c) = ctx();
        let out = tool
            .execute(json!({"query": "rust", "count": 2}), &c)
            .await
            .unwrap();
        assert!(!out.is_error);
        assert!(out.content.contains("First & Result"));
        assert!(out.content.contains("https://example.com/first"));
    }

    #[tokio::test]
    async fn search_timeout_is_a_business_error() {
        let (addr, _server) = stub_server().await;
        struct HangBackend {
            base_url: String,
        }
        #[async_trait::async_trait]
        impl SearchBackend for HangBackend {
            async fn search(
                &self,
                _q: &str,
                _c: usize,
                timeout: Duration,
            ) -> std::result::Result<Vec<SearchResult>, String> {
                // Drive the real HTTP client with a tiny timeout against the
                // hanging stub path; the timeout must surface as a business error.
                let client = reqwest::Client::builder()
                    .redirect(reqwest::redirect::Policy::none())
                    .timeout(timeout)
                    .build()
                    .unwrap();
                match client.get(format!("{}/hang", self.base_url)).send().await {
                    Ok(_) => Ok(Vec::new()),
                    Err(e) => Err(format!("search request failed: {e}")),
                }
            }
        }
        let tool = WebSearch::with_backend(Arc::new(HangBackend {
            base_url: format!("http://127.0.0.1:{}", addr.port()),
        }));
        let (_d, c) = ctx();
        let out = tool
            .execute(json!({"query": "rust", "timeout_ms": 200}), &c)
            .await
            .unwrap();
        assert!(out.is_error, "a stalled search must be a business error");
    }

    #[tokio::test]
    async fn empty_query_and_bad_count_are_business_errors() {
        let (_d, c) = ctx();
        let tool = WebSearch::new();
        assert!(tool.execute(json!({}), &c).await.unwrap().is_error);
        assert!(
            tool.execute(json!({"query": "  "}), &c)
                .await
                .unwrap()
                .is_error
        );
        assert!(
            tool.execute(json!({"query": "x", "count": 99}), &c)
                .await
                .unwrap()
                .is_error
        );
    }
}
