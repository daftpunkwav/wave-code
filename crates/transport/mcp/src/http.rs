/*! @file http
 * @description Streamable-HTTP transport for MCP JSON-RPC exchange.
 *
 * Responsibilities:
 * - POST JSON-RPC messages with `Accept: application/json, text/event-stream`.
 * - Persist the `mcp-session-id` response header across requests.
 * - Parse single-JSON and SSE-stream response bodies.
 * - Map a 404 on an established session to a re-initializable
 *   `SessionExpired` error (the session id is dropped).
 * - Static-header passthrough plus OAuth client-credentials bearer auth
 *   with expiry-minus-skew token caching.
 * - Hermetic tests over a hand-rolled TCP stub server (no external network).
 *
 * This module must not depend on: workspace crates or any interactive
 * browser/PKCE OAuth flow (a 401 challenge naming an authorization endpoint
 * fails with an explicit protocol error instead of attempting a browser flow).
 */

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use super::{FIRST_REQUEST_ID, TransportError};

/// Per-request timeout applied to every HTTP round trip (send + body read).
pub const REQUEST_TIMEOUT_SECS: u64 = 30;

/// One bounded re-dial when a request provably never left the process.
///
/// A connection-establishment failure (`is_connect`) means no request byte
/// was written, so re-issuing cannot double a side effect — but only for
/// the read-only methods below. `tools/call` is deliberately absent: it
/// may execute a write server-side, and one failed dial proves nothing
/// about whether a later attempt succeeded, so it must surface instead of
/// retry. Fixed short backoff, no jitter: exactly one retry for a
/// single-user client cannot form a retry storm.
const CONNECT_RETRY_BACKOFF: Duration = Duration::from_millis(100);

/// Methods safe to re-issue after a connect-phase failure. Handshake,
/// pings, and list/read calls are idempotent; `notifications/initialized`
/// never left the process, so the repeat delivery is impossible.
const CONNECT_RETRYABLE_METHODS: &[&str] = &[
    "initialize",
    "ping",
    "tools/list",
    "resources/list",
    "resources/read",
    "prompts/list",
    "prompts/get",
    "notifications/initialized",
];

/// Whether one connect-phase failure may be re-dialed for `method`.
fn is_connect_retryable(method: Option<&str>) -> bool {
    method.is_some_and(|method| CONNECT_RETRYABLE_METHODS.contains(&method))
}

/// Freshness skew for cached OAuth tokens: a token is reused only while it
/// stays valid longer than this skew, otherwise it is refreshed proactively.
const TOKEN_EXPIRY_SKEW_SECS: u64 = 30;

/// Response header (and follow-up request header) carrying the MCP session id.
/// Header names are case-insensitive; reqwest normalizes them on lookup.
const MCP_SESSION_ID_HEADER: &str = "mcp-session-id";

/// Fallback token lifetime when the token endpoint omits `expires_in`.
const DEFAULT_TOKEN_LIFETIME_SECS: u64 = 3600;

/// Cap on one response body read into memory. A hostile or wedged MCP
/// server can otherwise stream for the whole request timeout at link rate
/// and balloon the process (gigabytes over a localhost pipe). Above the
/// cap the response is a protocol error, never a truncated parse — a cut
/// JSON-RPC message cannot be trusted. Aligned with the LSP frame cap.
const MAX_BODY_BYTES: usize = 32 * 1024 * 1024;

/// OAuth 2.0 client-credentials grant configuration.
///
/// Only the machine-to-machine grant is supported. Interactive grants
/// (authorization-code / PKCE / device flow) are out of scope by design.
#[derive(Debug, Clone, PartialEq)]
pub struct OAuthClientCredentials {
    /// Token endpoint URL. Must be `https` outside the loopback interface:
    /// the client secret travels in the request body, so plain http to a
    /// non-loopback host is rejected at build time.
    pub token_url: String,
    /// OAuth client id.
    pub client_id: String,
    /// OAuth client secret.
    pub client_secret: String,
    /// Optional scope to request.
    pub scope: Option<String>,
}

/// Streamable-HTTP transport configuration (validated on build).
#[derive(Debug, Clone, PartialEq)]
pub struct HttpMcpConfig {
    /// MCP server endpoint URL (the single streamable-HTTP URL).
    pub endpoint: String,
    /// Static request headers applied to every request (e.g. `Authorization`
    /// for pre-provisioned keys). A static `Authorization` header wins over
    /// the OAuth bearer token.
    pub headers: HashMap<String, String>,
    /// Optional OAuth client-credentials grant for bearer auth.
    pub oauth: Option<OAuthClientCredentials>,
}

impl HttpMcpConfig {
    /// Validate the endpoint URL and the OAuth block (if present).
    pub fn validate(&self) -> Result<(), String> {
        validate_endpoint(&self.endpoint)?;
        if let Some(oauth) = &self.oauth {
            validate_token_url(&oauth.token_url).map_err(|e| format!("oauth_token_url: {e}"))?;
            if oauth.client_id.trim().is_empty() {
                return Err("oauth_client_id must not be empty".to_owned());
            }
            if oauth.client_secret.is_empty() {
                return Err("oauth_client_secret must not be empty".to_owned());
            }
        }
        Ok(())
    }
}

/// Check that a URL parses and uses the http(s) scheme.
fn validate_endpoint(url: &str) -> Result<(), String> {
    let parsed = reqwest::Url::parse(url).map_err(|e| format!("invalid URL {url:?}: {e}"))?;
    match parsed.scheme() {
        "http" | "https" => Ok(()),
        other => Err(format!(
            "URL must use http(s), got scheme {other:?}: {url:?}"
        )),
    }
}

/// Whether an http URL targets the loopback interface: `localhost` by name
/// or a loopback IP literal (IPv4/IPv6).
fn is_loopback_target(url: &reqwest::Url) -> bool {
    url.host_str().is_some_and(|host| {
        host.eq_ignore_ascii_case("localhost")
            || host
                .trim_start_matches('[')
                .trim_end_matches(']')
                .parse::<std::net::IpAddr>()
                .is_ok_and(|ip| ip.is_loopback())
    })
}

/// Check an OAuth token endpoint: the client secret travels in the request
/// body, so a plain-http endpoint is only acceptable on the loopback
/// interface (local stubs); anywhere else it would cross the network in
/// cleartext and the config is rejected at build time.
fn validate_token_url(url: &str) -> Result<(), String> {
    let parsed = reqwest::Url::parse(url).map_err(|e| format!("invalid URL {url:?}: {e}"))?;
    match parsed.scheme() {
        "https" => Ok(()),
        "http" if is_loopback_target(&parsed) => Ok(()),
        "http" => Err(format!(
            "must use https outside the loopback interface (the client secret would travel in cleartext): {url:?}"
        )),
        other => Err(format!("must use http(s), got scheme {other:?}: {url:?}")),
    }
}

/// Streamable-HTTP MCP transport: POSTs JSON-RPC messages and reads either
/// plain-JSON or SSE-stream responses.
///
/// All methods take `&self`: request ids, the session id, and the cached
/// bearer token use interior mutability, so the handle is cheap to share
/// across tasks.
pub struct HttpMcp {
    client: reqwest::Client,
    endpoint: String,
    headers: HashMap<String, String>,
    oauth: Option<OAuthClientCredentials>,
    session_id: Mutex<Option<String>>,
    token: Mutex<Option<CachedToken>>,
    next_id: AtomicU64,
    /// Send + body-read budget. Production uses [`REQUEST_TIMEOUT_SECS`];
    /// tests shrink it so a hung server does not depend on paused time
    /// (paused time can expire the budget before a real loopback accept
    /// is observed, which macOS CI does).
    request_timeout: Duration,
}

/// Cached bearer token with an absolute expiry.
#[derive(Debug, Clone)]
struct CachedToken {
    value: String,
    expires_at: Instant,
}

impl HttpMcp {
    /// Build a transport from validated config. Returns a protocol error when
    /// the endpoint URL or the OAuth block is invalid.
    pub fn new(config: HttpMcpConfig) -> Result<Self, TransportError> {
        config.validate().map_err(TransportError::Protocol)?;
        // No redirects, matching the LLM clients and web tools: a 30x must
        // not rewrite POST semantics or carry headers along a redirect
        // chain the operator never configured.
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| TransportError::Http(format!("failed to build HTTP client: {e}")))?;
        Ok(Self {
            client,
            endpoint: config.endpoint,
            headers: config.headers,
            oauth: config.oauth,
            session_id: Mutex::new(None),
            token: Mutex::new(None),
            next_id: AtomicU64::new(FIRST_REQUEST_ID),
            request_timeout: Duration::from_secs(REQUEST_TIMEOUT_SECS),
        })
    }

    /// Shorten the per-request budget. Test-only: production always uses
    /// [`REQUEST_TIMEOUT_SECS`].
    #[cfg(test)]
    fn with_request_timeout(mut self, timeout: Duration) -> Self {
        self.request_timeout = timeout;
        self
    }

    /// Endpoint this transport posts to, for diagnostics.
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    /// Send a JSON-RPC request and unwrap the `result` (or raise the `error`).
    pub async fn rpc(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, TransportError> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        // Skip id 0 on wrap so correlation ids stay non-zero.
        let id = if id == 0 {
            self.next_id.fetch_add(1, Ordering::SeqCst)
        } else {
            id
        };
        let body =
            serde_json::json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        match self.post(&body).await? {
            Some(response) => into_result(response, method, Some(id)),
            None => Err(TransportError::Protocol(format!(
                "MCP method {method:?} got an empty response"
            ))),
        }
    }

    /// Send a JSON-RPC notification (no `id`, no response body expected).
    pub async fn notify(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<(), TransportError> {
        let body = serde_json::json!({"jsonrpc": "2.0", "method": method, "params": params});
        self.post(&body).await.map(|_| ())
    }

    /// POST one JSON-RPC message and parse the response body (JSON or SSE).
    async fn post(
        &self,
        body: &serde_json::Value,
    ) -> Result<Option<serde_json::Value>, TransportError> {
        let expected_id = body.get("id").and_then(serde_json::Value::as_u64);
        let raw = serde_json::to_vec(body).map_err(|e| {
            TransportError::Protocol(format!("failed to encode JSON-RPC request: {e}"))
        })?;
        let session = self
            .session_id
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        // A static `Authorization` header wins: skip the OAuth grant entirely
        // so no token request is issued when explicit auth is configured.
        let bearer = if has_static_authorization(&self.headers) {
            None
        } else {
            self.bearer_token().await?
        };
        // Built per attempt: a sent reqwest builder is consumed.
        let build_request = || -> Result<reqwest::RequestBuilder, TransportError> {
            let mut request = self
                .client
                .post(&self.endpoint)
                .header(
                    reqwest::header::ACCEPT,
                    "application/json, text/event-stream",
                )
                .header(reqwest::header::CONTENT_TYPE, "application/json");
            for (name, value) in &self.headers {
                let name =
                    reqwest::header::HeaderName::from_bytes(name.as_bytes()).map_err(|e| {
                        TransportError::Protocol(format!("invalid static header {name:?}: {e}"))
                    })?;
                let value = reqwest::header::HeaderValue::from_str(value).map_err(|e| {
                    TransportError::Protocol(format!("invalid static header value for {name}: {e}"))
                })?;
                request = request.header(name, value);
            }
            if let Some(session) = session.as_deref() {
                request = request.header(MCP_SESSION_ID_HEADER, session);
            }
            if let Some(token) = bearer.as_deref() {
                request = request.header(reqwest::header::AUTHORIZATION, format!("Bearer {token}"));
            }
            Ok(request.body(raw.clone()))
        };
        let recall = self.request_timeout;
        let reported_secs = recall.as_secs();
        let method = body.get("method").and_then(serde_json::Value::as_str);
        let response = {
            let mut attempt = 0usize;
            loop {
                let sent = tokio::time::timeout(recall, build_request()?.send()).await;
                match sent {
                    // Timed out before a response: the server may have
                    // received and acted on the request — never retried.
                    Err(_elapsed) => return Err(TransportError::Timeout(reported_secs)),
                    Ok(Ok(response)) => break response,
                    Ok(Err(error)) => {
                        let may_retry =
                            attempt == 0 && error.is_connect() && is_connect_retryable(method);
                        if !may_retry {
                            return Err(TransportError::Http(format!(
                                "MCP HTTP request failed: {error}"
                            )));
                        }
                        tracing::warn!(
                            method = method.unwrap_or(""),
                            "MCP connect failed before the request was sent; retrying once"
                        );
                    }
                }
                attempt += 1;
                tokio::time::sleep(CONNECT_RETRY_BACKOFF).await;
            }
        };
        let status = response.status();
        let headers = response.headers().clone();
        self.store_session_id(&headers);
        if status == reqwest::StatusCode::UNAUTHORIZED {
            return Err(self.unauthorized_error(&headers));
        }
        // Per the Streamable-HTTP spec a 404 on an established session means
        // the session expired (or was terminated) server-side: drop the
        // session id and tell the caller to re-initialize. A 404 without a
        // session is a plain routing error, not an expiry.
        if status == reqwest::StatusCode::NOT_FOUND
            && self
                .session_id
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take()
                .is_some()
        {
            return Err(TransportError::SessionExpired);
        }
        if !status.is_success() {
            return Err(TransportError::Http(format!(
                "MCP HTTP request failed with status {status}"
            )));
        }
        let bytes = tokio::time::timeout(recall, read_capped(response, MAX_BODY_BYTES))
            .await
            .map_err(|_| TransportError::Timeout(reported_secs))??;
        parse_response_body_for_request(&bytes, expected_id)
    }

    /// Persist the `mcp-session-id` response header for later requests.
    fn store_session_id(&self, headers: &reqwest::header::HeaderMap) {
        let session = headers
            .get(MCP_SESSION_ID_HEADER)
            .and_then(|value| value.to_str().ok())
            .map(str::trim)
            .filter(|value| !value.is_empty());
        if let Some(session) = session {
            *self.session_id.lock().unwrap_or_else(|e| e.into_inner()) = Some(session.to_owned());
        }
    }

    /// Build the error for a 401 response: interactive challenges become a
    /// protocol error that names the unsupported flow, anything else is an
    /// HTTP error that points at the auth configuration.
    fn unauthorized_error(&self, headers: &reqwest::header::HeaderMap) -> TransportError {
        let challenge = headers
            .get(reqwest::header::WWW_AUTHENTICATE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_owned();
        if is_interactive_challenge(&challenge) {
            TransportError::Protocol(format!(
                "MCP server requires interactive OAuth authorization (browser/PKCE flow), \
                 which is not supported: configure static request `headers` (e.g. `Authorization`) \
                 or OAuth client-credentials (`oauth_token_url` + `oauth_client_id`/`oauth_client_secret`) \
                 instead. endpoint={} challenge={challenge:?}",
                self.endpoint,
            ))
        } else {
            TransportError::Http(format!(
                "MCP HTTP request unauthorized (401) for endpoint {} challenge={challenge:?}: \
                 check static `headers` or the OAuth client-credentials configuration",
                self.endpoint,
            ))
        }
    }

    /// Resolve the bearer token: reuse the cached one while fresh, otherwise
    /// run the client-credentials grant. Returns `None` when no OAuth block
    /// is configured.
    async fn bearer_token(&self) -> Result<Option<String>, TransportError> {
        let oauth = match self.oauth.clone() {
            Some(oauth) => oauth,
            None => return Ok(None),
        };
        if let Some(cached) = self.token.lock().unwrap_or_else(|e| e.into_inner()).clone() {
            let remaining = cached.expires_at.saturating_duration_since(Instant::now());
            if remaining > Duration::from_secs(TOKEN_EXPIRY_SKEW_SECS) {
                return Ok(Some(cached.value));
            }
        }
        let token = fetch_client_credentials(&self.client, &oauth).await?;
        let value = token.value.clone();
        *self.token.lock().unwrap_or_else(|e| e.into_inner()) = Some(token);
        Ok(Some(value))
    }
}

/// Whether static headers already carry an `Authorization` entry
/// (case-insensitive); explicit config wins over the OAuth bearer token.
fn has_static_authorization(headers: &HashMap<String, String>) -> bool {
    headers
        .keys()
        .any(|name| name.eq_ignore_ascii_case("authorization"))
}

/// Heuristic for "the server wants a browser round trip": the challenge names
/// an authorization endpoint or an interactive flow.
fn is_interactive_challenge(challenge: &str) -> bool {
    let lower = challenge.to_ascii_lowercase();
    ["authorization", "openid", "login", "browser", "pkce"]
        .iter()
        .any(|marker| lower.contains(marker))
}

/// Run the OAuth client-credentials grant against the token endpoint.
///
/// One bounded re-dial on a connect-phase failure, the same shape as the
/// RPC path above: `is_connect` proves no bytes were written, and the
/// grant is a token mint with no user-visible side effect, so a repeat
/// cannot double anything. A second failure — or any failure after the
/// request left the process — surfaces; a flaky auth endpoint must not
/// turn the first MCP call of a session into a hard business error.
async fn fetch_client_credentials(
    client: &reqwest::Client,
    oauth: &OAuthClientCredentials,
) -> Result<CachedToken, TransportError> {
    let mut body = format!(
        "grant_type=client_credentials&client_id={}&client_secret={}",
        form_encode(&oauth.client_id),
        form_encode(&oauth.client_secret),
    );
    if let Some(scope) = oauth.scope.as_deref() {
        body.push_str("&scope=");
        body.push_str(&form_encode(scope));
    }
    let recall = Duration::from_secs(REQUEST_TIMEOUT_SECS);
    let mut attempt = 0usize;
    let response = loop {
        let sent = tokio::time::timeout(
            recall,
            client
                .post(&oauth.token_url)
                .header(reqwest::header::ACCEPT, "application/json")
                .header(
                    reqwest::header::CONTENT_TYPE,
                    "application/x-www-form-urlencoded",
                )
                .body(body.clone())
                .send(),
        )
        .await;
        match sent {
            // Timed out before a response: the endpoint may have seen the
            // grant — never retried, same rule as the RPC path.
            Err(_elapsed) => return Err(TransportError::Timeout(REQUEST_TIMEOUT_SECS)),
            Ok(Ok(response)) => break response,
            Ok(Err(error)) => {
                if attempt > 0 || !error.is_connect() {
                    return Err(TransportError::Http(format!(
                        "OAuth token request failed: {error}"
                    )));
                }
                attempt += 1;
                tracing::warn!(
                    "OAuth token endpoint connect failed before the request was sent; retrying once"
                );
            }
        }
        tokio::time::sleep(CONNECT_RETRY_BACKOFF).await;
    };
    let status = response.status();
    let bytes = tokio::time::timeout(recall, read_capped(response, MAX_BODY_BYTES))
        .await
        .map_err(|_| TransportError::Timeout(REQUEST_TIMEOUT_SECS))??;
    if !status.is_success() {
        return Err(TransportError::Http(format!(
            "OAuth token request failed with status {status}: {}",
            preview(&bytes),
        )));
    }
    let value: serde_json::Value = serde_json::from_slice(&bytes)
        .map_err(|e| TransportError::Protocol(format!("OAuth token response is not JSON: {e}")))?;
    let access = value
        .get("access_token")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| {
            TransportError::Protocol("OAuth token response has no `access_token` string".to_owned())
        })?;
    let lifetime = value
        .get("expires_in")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(DEFAULT_TOKEN_LIFETIME_SECS);
    Ok(CachedToken {
        value: access.to_owned(),
        // A bogus huge `expires_in` (server-controlled input) must not panic
        // on Instant overflow: clip to the default lifetime instead.
        expires_at: Instant::now()
            .checked_add(Duration::from_secs(lifetime))
            .unwrap_or_else(|| Instant::now() + Duration::from_secs(DEFAULT_TOKEN_LIFETIME_SECS)),
    })
}

/// Percent-encode one `application/x-www-form-urlencoded` value.
fn form_encode(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for byte in input.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char);
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// Truncate a body preview for error messages (bodies may be large HTML).
fn preview(bytes: &[u8]) -> String {
    const MAX: usize = 200;
    String::from_utf8_lossy(&bytes[..bytes.len().min(MAX)]).into_owned()
}

/// Read one response body into memory with a hard byte cap.
///
/// Streams chunk by chunk so the process never holds more than `cap`
/// bytes plus one chunk, whatever the server sends; a body past the cap
/// is a protocol error rather than a truncation (a cut JSON-RPC message
/// must never parse "successfully").
async fn read_capped(response: reqwest::Response, cap: usize) -> Result<Vec<u8>, TransportError> {
    let mut body: Vec<u8> = Vec::new();
    let mut response = response;
    loop {
        match response.chunk().await {
            Ok(Some(chunk)) => {
                if body.len() + chunk.len() > cap {
                    return Err(TransportError::Protocol(format!(
                        "MCP response body exceeds the {cap}-byte cap"
                    )));
                }
                body.extend_from_slice(&chunk);
            }
            Ok(None) => return Ok(body),
            Err(e) => {
                return Err(TransportError::Http(format!(
                    "MCP HTTP response read failed: {e}"
                )));
            }
        }
    }
}

/// Parse one response body: empty means "no content" (e.g. 202 for a
/// notification); otherwise try single-JSON first, then the SSE-stream path.
///
/// Production always parses with a known request id (the transport calls
/// [`parse_response_body_for_request`] directly); this uncorrelated form exists
/// only so tests exercise the shared path the way a caller without a
/// pending id would.
#[cfg(test)]
fn parse_response_body(body: &[u8]) -> Result<Option<serde_json::Value>, TransportError> {
    parse_response_body_for_request(body, None)
}

/// `parse_response_body` narrowed to one request id (see
/// [`parse_sse_stream_for_request`]): the single-JSON path is the whole body, so
/// correlation happens on the SSE branch.
fn parse_response_body_for_request(
    body: &[u8],
    expected_id: Option<u64>,
) -> Result<Option<serde_json::Value>, TransportError> {
    if body.iter().all(|byte| byte.is_ascii_whitespace()) {
        return Ok(None);
    }
    if let Ok(value) = serde_json::from_slice::<serde_json::Value>(body) {
        return Ok(Some(value));
    }
    let text = std::str::from_utf8(body).map_err(|e| {
        TransportError::Protocol(format!("MCP response is neither JSON nor UTF-8 SSE: {e}"))
    })?;
    match parse_sse_stream_for_request(text, expected_id) {
        Some(value) => Ok(Some(value)),
        None => Err(TransportError::Protocol(format!(
            "MCP response is neither a JSON-RPC object nor an SSE stream: {:?}",
            preview(body),
        ))),
    }
}

/// JSON-RPC candidates from one SSE stream: every `data:` payload that
/// parses as JSON, in stream order. `event:` / `id:` / `retry:` control
/// lines carry no payload.
fn sse_candidates(text: &str) -> Vec<serde_json::Value> {
    let mut candidates = Vec::new();
    for line in text.lines() {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line.is_empty() || line.starts_with(':') {
            continue;
        }
        if let Some(rest) = line.strip_prefix("data:") {
            // The SSE spec strips a single leading space after `data:`.
            let payload = rest.strip_prefix(' ').unwrap_or(rest);
            if let Ok(value) = serde_json::from_str::<serde_json::Value>(payload) {
                candidates.push(value);
            }
        }
    }
    candidates
}

/// Parse an SSE stream (`event:` / `data:` / comment lines): every `data:`
/// payload that parses as JSON is a candidate and the last candidate wins, so
/// interleaved progress events never shadow the final JSON-RPC message.
///
/// Production parses with a known request id (`parse_sse_stream_for_request`) so a
/// stale or interleaved frame cannot be mistaken for the response; this
/// uncorrelated form is the test-only variant of that same parse.
#[cfg(test)]
fn parse_sse_stream(text: &str) -> Option<serde_json::Value> {
    sse_candidates(text).pop()
}

/// `parse_sse_stream` narrowed to one request id: when `expected` is
/// known, only a frame carrying exactly that id counts — null-id frames
/// are server-initiated notifications, never responses, and a stale or
/// interleaved id can never be mistaken for this response. `None` means
/// the stream held no frame for this request.
fn parse_sse_stream_for_request(text: &str, expected: Option<u64>) -> Option<serde_json::Value> {
    sse_candidates(text)
        .into_iter()
        .rev()
        .find(|value| match expected {
            // A notification (null id) is never a response.
            Some(expected) => value.get("id").and_then(serde_json::Value::as_u64) == Some(expected),
            None => true,
        })
}

/// Unwrap one JSON-RPC response object into its `result`, raising `error`.
fn into_result(
    response: serde_json::Value,
    method: &str,
    expected_id: Option<u64>,
) -> Result<serde_json::Value, TransportError> {
    // Correlate the response with the request: a mismatched non-null id
    // means a stale or interleaved frame (the stdio side correlates by
    // id by construction). Null ids stay accepted — servers answer
    // protocol-level errors with a null id.
    if let Some(expected) = expected_id
        && let Some(actual) = response.get("id")
        && !actual.is_null()
        && actual.as_u64() != Some(expected)
    {
        return Err(TransportError::Protocol(format!(
            "MCP method {method:?} response id {actual} does not match request id {expected}"
        )));
    }
    if let Some(error) = response.get("error") {
        let code = error
            .get("code")
            .map(|code| code.to_string())
            .unwrap_or_else(|| "?".to_owned());
        let message = error
            .get("message")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unknown error");
        return Err(TransportError::Protocol(format!(
            "MCP method {method:?} failed (code {code}): {message}"
        )));
    }
    match response.get("result") {
        Some(result) => Ok(result.clone()),
        None => Err(TransportError::Protocol(format!(
            "MCP method {method:?} response has neither `result` nor `error`: {response}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{RecordedRequest, StubServer};
    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;
    // The shared stub's handler type, under the local name the tests use.
    use crate::test_support::StubHandler as Handler;

    /// The id-narrowed SSE parse skips frames for other requests (and
    /// notifications pass), and a stream with no matching frame is None.
    #[test]
    fn sse_parse_skips_frames_for_other_request_ids() {
        let text = concat!(
            "data: {\"jsonrpc\":\"2.0\",\"id\":7,\"result\":{\"stale\":true}}

",
            "data: {\"jsonrpc\":\"2.0\",\"method\":\"notify\"}

",
            "data: {\"jsonrpc\":\"2.0\",\"id\":9,\"result\":{\"ok\":true}}

",
        );
        let value = parse_sse_stream_for_request(text, Some(9)).unwrap();
        assert_eq!(value["id"], 9, "the stale id-7 frame must be skipped");
        assert!(
            parse_sse_stream_for_request(text, Some(8)).is_none(),
            "no frame matches request 8"
        );
    }

    fn json_headers() -> Vec<(String, String)> {
        vec![("content-type".to_owned(), "application/json".to_owned())]
    }

    /// One JSON-RPC **error** body echoing the request's own id.
    fn json_rpc_error(request: &RecordedRequest, code: i32, message: &str) -> Vec<u8> {
        let id = serde_json::from_slice::<serde_json::Value>(&request.body)
            .ok()
            .and_then(|value| value.get("id").cloned())
            .unwrap_or(serde_json::Value::Null);
        serde_json::to_vec(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {"code": code, "message": message},
        }))
        .unwrap()
    }

    /// One JSON-RPC response body echoing the request's own id: the
    /// client correlates responses by id, so a hardcoded one never matches.
    fn json_rpc(request: &RecordedRequest, result: serde_json::Value) -> Vec<u8> {
        let id = serde_json::from_slice::<serde_json::Value>(&request.body)
            .ok()
            .and_then(|value| value.get("id").cloned())
            .unwrap_or(serde_json::Value::Null);
        serde_json::to_vec(&serde_json::json!({"jsonrpc": "2.0", "id": id, "result": result}))
            .unwrap()
    }

    /// Main stub: initialize (with session header) + notification + paged
    /// `tools/list` (SSE envelope first, plain JSON second) + `tools/call`.
    fn main_handler(request: &RecordedRequest) -> (u16, Vec<(String, String)>, Vec<u8>) {
        match request.rpc_method().as_str() {
            "initialize" => (
                200,
                vec![
                    ("mcp-session-id".to_owned(), "sess-1".to_owned()),
                    ("content-type".to_owned(), "application/json".to_owned()),
                ],
                json_rpc(
                    request,
                    serde_json::json!({
                        "protocolVersion": "2025-03-26",
                        "capabilities": {},
                        "serverInfo": {"name": "stub", "version": "0"},
                    }),
                ),
            ),
            "notifications/initialized" => (202, vec![], vec![]),
            "tools/list" => {
                let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
                let cursor = body
                    .get("params")
                    .and_then(|params| params.get("cursor"))
                    .and_then(serde_json::Value::as_str);
                if cursor.is_none() {
                    let page = serde_json::json!({"tools": [
                        {"name": "alpha", "description": "first", "inputSchema": {"type": "object"}},
                        {"name": "beta", "inputSchema": {"type": "object"}},
                    ], "nextCursor": "c1"});
                    let sse = format!(
                        "event: message\ndata: {}\n\n",
                        // Echo the request id: the client correlates responses by id.
                        serde_json::json!({"jsonrpc": "2.0", "id": body.get("id").cloned().unwrap_or(serde_json::Value::Null), "result": page})
                    );
                    (
                        200,
                        vec![("content-type".to_owned(), "text/event-stream".to_owned())],
                        sse.into_bytes(),
                    )
                } else {
                    (
                        200,
                        json_headers(),
                        json_rpc(
                            request,
                            serde_json::json!({"tools": [
                                {"name": "gamma", "description": "third", "inputSchema": {"type": "object"}},
                            ]}),
                        ),
                    )
                }
            }
            "tools/call" => {
                let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
                let name = body
                    .get("params")
                    .and_then(|params| params.get("name"))
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("");
                if name == "boom" {
                    (
                        200,
                        json_headers(),
                        json_rpc_error(request, -32602, "bad args"),
                    )
                } else {
                    (
                        200,
                        json_headers(),
                        json_rpc(
                            request,
                            serde_json::json!({
                                "content": [{"type": "text", "text": format!("echo:{name}")}],
                                "isError": false,
                            }),
                        ),
                    )
                }
            }
            _ => (400, vec![], b"unknown method".to_vec()),
        }
    }

    fn test_config(server: &StubServer) -> HttpMcpConfig {
        HttpMcpConfig {
            endpoint: server.url("/mcp"),
            headers: HashMap::new(),
            oauth: None,
        }
    }

    /// Redirects must not be followed: a 302 surfaces as an error and no
    /// second request hits the redirect target path.
    #[tokio::test]
    async fn redirects_are_not_followed() {
        let handler: Handler = Arc::new(|req: &RecordedRequest| {
            if req.path.ends_with("/mcp") {
                (
                    302,
                    vec![("Location".to_string(), "/elsewhere".to_string())],
                    Vec::new(),
                )
            } else {
                (404, Vec::new(), b"gone".to_vec())
            }
        });
        let server = StubServer::spawn(handler).await;
        let transport = HttpMcp::new(test_config(&server)).unwrap();
        let outcome = transport
            .rpc(
                "initialize",
                serde_json::json!({"protocolVersion": "2025-03-26"}),
            )
            .await;
        assert!(outcome.is_err(), "a 302 must surface as an error");
        // Exactly one request was made: the redirect was not chased.
        assert_eq!(server.request_count(), 1);
    }

    /// Full round trip: initialize persists the session id, `tools/list`
    /// reads the SSE first page plus the JSON second page, `tools/call`
    /// echoes, and JSON-RPC errors surface as protocol errors.
    #[tokio::test]
    async fn session_lists_calls_and_surfaces_rpc_errors() {
        let server = StubServer::spawn(Arc::new(main_handler)).await;
        let transport = HttpMcp::new(test_config(&server)).unwrap();
        let init = transport
            .rpc(
                "initialize",
                serde_json::json!({"protocolVersion": "2025-03-26"}),
            )
            .await
            .unwrap();
        assert!(init.get("protocolVersion").is_some());
        transport
            .notify("notifications/initialized", serde_json::json!({}))
            .await
            .unwrap();

        // The session id from `initialize` is attached to later requests.
        assert_eq!(
            server.header(1, "mcp-session-id").as_deref(),
            Some("sess-1"),
            "notification after initialize carries the session"
        );

        let first = transport
            .rpc("tools/list", serde_json::json!({}))
            .await
            .unwrap();
        let tools = first
            .get("tools")
            .and_then(serde_json::Value::as_array)
            .expect("SSE page parses into a tools array");
        assert_eq!(tools.len(), 2, "SSE envelope page carries two tools");
        assert_eq!(
            server.header(2, "mcp-session-id").as_deref(),
            Some("sess-1")
        );

        let second = transport
            .rpc("tools/list", serde_json::json!({"cursor": "c1"}))
            .await
            .unwrap();
        assert_eq!(
            second
                .get("tools")
                .and_then(serde_json::Value::as_array)
                .map(Vec::len),
            Some(1),
            "cursor page arrives as plain JSON"
        );

        let out = transport
            .rpc(
                "tools/call",
                serde_json::json!({"name": "alpha", "arguments": {}}),
            )
            .await
            .unwrap();
        assert!(out.get("content").is_some());

        let error = transport
            .rpc(
                "tools/call",
                serde_json::json!({"name": "boom", "arguments": {}}),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(error, TransportError::Protocol(_)) && error.to_string().contains("bad args"),
            "JSON-RPC error becomes TransportError::Protocol, got: {error}"
        );
    }

    /// Static headers reach the server; the stub only shakes hands when both
    /// the custom header and the static bearer token are present.
    #[tokio::test]
    async fn static_headers_pass_through() {
        let handler: Handler = Arc::new(|request: &RecordedRequest| {
            let authed = request.headers.get("authorization").map(String::as_str)
                == Some("Bearer static-xyz");
            let tagged = request.headers.get("x-custom").map(String::as_str) == Some("1");
            if request.rpc_method() == "initialize" && authed && tagged {
                let mut headers = json_headers();
                headers.push(("mcp-session-id".to_owned(), "s-static".to_owned()));
                (
                    200,
                    headers,
                    json_rpc(request, serde_json::json!({"capabilities": {}})),
                )
            } else if request.rpc_method() == "notifications/initialized" && authed {
                (202, vec![], vec![])
            } else {
                (401, vec![], b"missing auth".to_vec())
            }
        });
        let server = StubServer::spawn(handler).await;
        let transport = HttpMcp::new(HttpMcpConfig {
            endpoint: server.url("/mcp"),
            headers: HashMap::from([
                ("Authorization".to_owned(), "Bearer static-xyz".to_owned()),
                ("X-Custom".to_owned(), "1".to_owned()),
            ]),
            oauth: None,
        })
        .unwrap();
        transport
            .rpc("initialize", serde_json::json!({}))
            .await
            .unwrap();
        assert_eq!(
            server.header(0, "accept").as_deref(),
            Some("application/json, text/event-stream")
        );
    }

    /// A 401 whose challenge points at an authorization endpoint must name the
    /// unsupported interactive flow instead of failing opaquely.
    #[tokio::test]
    async fn interactive_challenge_reports_the_gap() {
        let handler: Handler = Arc::new(|_: &RecordedRequest| {
            (
                401,
                vec![(
                    "www-authenticate".to_owned(),
                    "Bearer authorization_uri=\"https://auth.example.com/authorize\", resource=\"https://mcp.example.com\"".to_owned(),
                )],
                b"login required".to_vec(),
            )
        });
        let server = StubServer::spawn(handler).await;
        let transport = HttpMcp::new(test_config(&server)).unwrap();
        let error = transport
            .rpc("initialize", serde_json::json!({}))
            .await
            .unwrap_err();
        assert!(
            matches!(error, TransportError::Protocol(_))
                && error.to_string().contains("interactive")
                && error.to_string().contains("not supported"),
            "got: {error}"
        );
    }

    /// A plain 401 (no interactive hint) is an HTTP auth-config error.
    #[tokio::test]
    async fn plain_unauthorized_is_http_error() {
        let handler: Handler = Arc::new(|_: &RecordedRequest| (401, vec![], b"nope".to_vec()));
        let server = StubServer::spawn(handler).await;
        let transport = HttpMcp::new(test_config(&server)).unwrap();
        let error = transport
            .rpc("initialize", serde_json::json!({}))
            .await
            .unwrap_err();
        assert!(
            matches!(error, TransportError::Http(_)) && error.to_string().contains("401"),
            "got: {error}"
        );
    }

    /// OAuth client-credentials: the token endpoint is hit once and the token
    /// is reused for every later request.
    #[tokio::test]
    async fn oauth_token_cached_until_expiry() {
        let token_hits = Arc::new(AtomicUsize::new(0));
        let counter = token_hits.clone();
        let handler: Handler = Arc::new(move |request: &RecordedRequest| {
            if request.path == "/token" {
                counter.fetch_add(1, Ordering::SeqCst);
                let form = String::from_utf8_lossy(&request.body);
                assert!(form.contains("grant_type=client_credentials"), "{form}");
                assert!(form.contains("client_id=wave"), "{form}");
                assert!(form.contains("scope=tools"), "{form}");
                (
                    200,
                    json_headers(),
                    br#"{"access_token":"tok-1","token_type":"Bearer","expires_in":3600}"#.to_vec(),
                )
            } else if request.headers.get("authorization").map(String::as_str)
                != Some("Bearer tok-1")
            {
                (401, vec![], b"bad token".to_vec())
            } else {
                main_handler(request)
            }
        });
        let server = StubServer::spawn(handler).await;
        let transport = HttpMcp::new(HttpMcpConfig {
            endpoint: server.url("/mcp"),
            headers: HashMap::new(),
            oauth: Some(OAuthClientCredentials {
                token_url: server.url("/token"),
                client_id: "wave".to_owned(),
                client_secret: "s3cret".to_owned(),
                scope: Some("tools".to_owned()),
            }),
        })
        .unwrap();
        transport
            .rpc("initialize", serde_json::json!({}))
            .await
            .unwrap();
        let first = transport
            .rpc("tools/list", serde_json::json!({}))
            .await
            .unwrap();
        let second = transport
            .rpc("tools/list", serde_json::json!({"cursor": "c1"}))
            .await
            .unwrap();
        assert!(first.get("tools").is_some());
        assert!(second.get("tools").is_some());
        assert_eq!(
            token_hits.load(Ordering::SeqCst),
            1,
            "later requests reuse the cached token"
        );
    }

    /// An already-expired token (`expires_in: 0`, inside the freshness skew)
    /// is refreshed instead of reused.
    #[tokio::test]
    async fn oauth_token_refetched_after_expiry() {
        let token_hits = Arc::new(AtomicUsize::new(0));
        let counter = token_hits.clone();
        let handler: Handler = Arc::new(move |request: &RecordedRequest| {
            if request.path == "/token" {
                counter.fetch_add(1, Ordering::SeqCst);
                (
                    200,
                    json_headers(),
                    br#"{"access_token":"tok-9","token_type":"Bearer","expires_in":0}"#.to_vec(),
                )
            } else if request.headers.get("authorization").map(String::as_str)
                != Some("Bearer tok-9")
            {
                (401, vec![], b"bad token".to_vec())
            } else {
                main_handler(request)
            }
        });
        let server = StubServer::spawn(handler).await;
        let transport = HttpMcp::new(HttpMcpConfig {
            endpoint: server.url("/mcp"),
            headers: HashMap::new(),
            oauth: Some(OAuthClientCredentials {
                token_url: server.url("/token"),
                client_id: "wave".to_owned(),
                client_secret: "s3cret".to_owned(),
                scope: None,
            }),
        })
        .unwrap();
        // initialize + notification each resolve the bearer token.
        transport
            .rpc("initialize", serde_json::json!({}))
            .await
            .unwrap();
        transport
            .notify("notifications/initialized", serde_json::json!({}))
            .await
            .unwrap();
        assert_eq!(
            token_hits.load(Ordering::SeqCst),
            2,
            "expired tokens are never reused"
        );
    }

    /// A bogus huge `expires_in` (server-controlled input) clips the cached
    /// expiry instead of panicking on `Instant` overflow.
    #[tokio::test]
    async fn oauth_huge_expires_in_does_not_panic() {
        let token_hits = Arc::new(AtomicUsize::new(0));
        let counter = token_hits.clone();
        let handler: Handler = Arc::new(move |request: &RecordedRequest| {
            if request.path == "/token" {
                counter.fetch_add(1, Ordering::SeqCst);
                (
                    200,
                    json_headers(),
                    br#"{"access_token":"tok-huge","token_type":"Bearer","expires_in":18446744073709551615}"#.to_vec(),
                )
            } else if request.headers.get("authorization").map(String::as_str)
                != Some("Bearer tok-huge")
            {
                (401, vec![], b"bad token".to_vec())
            } else {
                main_handler(request)
            }
        });
        let server = StubServer::spawn(handler).await;
        let transport = HttpMcp::new(HttpMcpConfig {
            endpoint: server.url("/mcp"),
            headers: HashMap::new(),
            oauth: Some(OAuthClientCredentials {
                token_url: server.url("/token"),
                client_id: "wave".to_owned(),
                client_secret: "s3cret".to_owned(),
                scope: None,
            }),
        })
        .unwrap();
        // The saturated expiry keeps the token cached: one token fetch
        // serves both the handshake and the follow-up request.
        transport
            .rpc("initialize", serde_json::json!({}))
            .await
            .unwrap();
        transport
            .rpc("tools/list", serde_json::json!({}))
            .await
            .unwrap();
        assert_eq!(token_hits.load(Ordering::SeqCst), 1);
    }

    /// A static `Authorization` header wins over OAuth: the token endpoint is
    /// never hit and the static value is sent.
    #[tokio::test]
    async fn static_authorization_wins_over_oauth() {
        let handler: Handler = Arc::new(|request: &RecordedRequest| {
            if request.path == "/token" {
                return (500, vec![], b"token endpoint must not be hit".to_vec());
            }
            if request.headers.get("authorization").map(String::as_str) == Some("Bearer static-xyz")
            {
                main_handler(request)
            } else {
                (401, vec![], b"bad token".to_vec())
            }
        });
        let server = StubServer::spawn(handler).await;
        let transport = HttpMcp::new(HttpMcpConfig {
            endpoint: server.url("/mcp"),
            headers: HashMap::from([("Authorization".to_owned(), "Bearer static-xyz".to_owned())]),
            oauth: Some(OAuthClientCredentials {
                token_url: server.url("/token"),
                client_id: "wave".to_owned(),
                client_secret: "s3cret".to_owned(),
                scope: None,
            }),
        })
        .unwrap();
        transport
            .rpc("initialize", serde_json::json!({}))
            .await
            .unwrap();
    }

    /// Session expiry: a 404 for the established session clears it and maps
    /// to `SessionExpired`; after a fresh `initialize` (new session id) the
    /// exchange succeeds again. The re-initialize-and-retry orchestration
    /// lives at the bridge layer; the transport only resets state.
    #[tokio::test]
    async fn session_404_maps_to_expiry_and_resets() {
        // The stub rotates session ids: `sess-1` requests all 404 (expired),
        // `sess-2` requests succeed, and the issued id is echoed in a header.
        let issued = Arc::new(AtomicUsize::new(0));
        let counter = issued.clone();
        let handler: Handler =
            Arc::new(
                move |request: &RecordedRequest| match request.rpc_method().as_str() {
                    "initialize" => {
                        let session =
                            format!("sess-{}", counter.fetch_add(1, Ordering::SeqCst) + 1);
                        let mut headers = json_headers();
                        headers.push(("mcp-session-id".to_owned(), session));
                        (
                            200,
                            headers,
                            json_rpc(request, serde_json::json!({"capabilities": {}})),
                        )
                    }
                    "notifications/initialized" => (202, vec![], vec![]),
                    _ => {
                        if request.headers.get("mcp-session-id").map(String::as_str)
                            == Some("sess-2")
                        {
                            (
                                200,
                                json_headers(),
                                json_rpc(request, serde_json::json!({"ok": true})),
                            )
                        } else {
                            (404, vec![], b"session expired".to_vec())
                        }
                    }
                },
            );
        let server = StubServer::spawn(handler).await;
        let transport = HttpMcp::new(test_config(&server)).unwrap();
        transport
            .rpc("initialize", serde_json::json!({}))
            .await
            .unwrap();

        let error = transport
            .rpc("tools/list", serde_json::json!({}))
            .await
            .unwrap_err();
        assert!(
            matches!(error, TransportError::SessionExpired),
            "404 on an established session maps to SessionExpired, got: {error}"
        );
        assert_eq!(
            server.header(1, "mcp-session-id").as_deref(),
            Some("sess-1"),
            "the follow-up request carried the established session"
        );

        // Re-initialize (the bridge does this on SessionExpired): the new
        // session id replaces the dropped one and requests succeed again.
        transport
            .rpc("initialize", serde_json::json!({}))
            .await
            .unwrap();
        transport
            .notify("notifications/initialized", serde_json::json!({}))
            .await
            .unwrap();
        let ok = transport
            .rpc("tools/list", serde_json::json!({}))
            .await
            .unwrap();
        assert_eq!(
            ok.get("ok").and_then(serde_json::Value::as_bool),
            Some(true)
        );
        assert_eq!(
            server.header(4, "mcp-session-id").as_deref(),
            Some("sess-2"),
            "requests after re-initialization carry the fresh session"
        );
    }

    /// A 404 without an established session is a plain HTTP error (bad
    /// endpoint), never a session-expiry signal.
    #[tokio::test]
    async fn http_404_without_session_is_plain_error() {
        let handler: Handler = Arc::new(|_: &RecordedRequest| (404, vec![], b"no route".to_vec()));
        let server = StubServer::spawn(handler).await;
        let transport = HttpMcp::new(test_config(&server)).unwrap();
        let error = transport
            .rpc("initialize", serde_json::json!({}))
            .await
            .unwrap_err();
        assert!(
            matches!(error, TransportError::Http(_)) && error.to_string().contains("404"),
            "got: {error}"
        );
    }

    /// A body past the in-memory cap is a protocol error, never a truncated
    /// parse or an unbounded read (small cap so the test stays hermetic;
    /// the shipped cap is 32 MiB).
    #[tokio::test]
    async fn response_body_past_the_cap_is_a_protocol_error() {
        let handler: Handler =
            Arc::new(|_: &RecordedRequest| (200, json_headers(), vec![b'x'; 1024]));
        let server = StubServer::spawn(handler).await;
        let response = reqwest::get(server.url("/mcp")).await.unwrap();
        let error = read_capped(response, 16).await.unwrap_err();
        assert!(
            matches!(error, TransportError::Protocol(_)) && error.to_string().contains("cap"),
            "{error}"
        );
        // Under the cap the same body reads whole.
        let response = reqwest::get(server.url("/mcp")).await.unwrap();
        let body = read_capped(response, 2048).await.unwrap();
        assert_eq!(body.len(), 1024);
    }

    /// SSE edge cases: comments and control lines are ignored, broken `data:`
    /// lines are skipped, one leading space is stripped, last JSON wins.
    #[test]
    fn sse_parsing_ignores_noise_and_keeps_last_json() {
        let first = serde_json::json!({"jsonrpc": "2.0", "id": 1, "progress": 1});
        let last = serde_json::json!({"jsonrpc": "2.0", "id": 1, "result": {"done": true}});
        let stream = format!(
            ": keep-alive comment\nretry: 100\nevent: progress\ndata: {first}\ndata: not json\ndata: {last}\n\nevent: stray\n"
        );
        assert_eq!(parse_sse_stream(&stream), Some(last));
        assert_eq!(parse_sse_stream(": only a comment\n\n"), None);
        assert_eq!(parse_sse_stream(""), None);
        // CRLF line endings are tolerated.
        let crlf = format!("event: x\r\ndata: {first}\r\n\r\n");
        assert_eq!(parse_sse_stream(&crlf), Some(first));
    }

    /// Body parsing: blank means no content, single JSON wins directly, and
    /// garbage in both shapes is a protocol error.
    #[test]
    fn response_body_shapes() {
        assert_eq!(parse_response_body(b"  \r\n ").unwrap(), None);
        let value = serde_json::json!({"jsonrpc": "2.0", "id": 1, "result": {}});
        assert_eq!(
            parse_response_body(&serde_json::to_vec(&value).unwrap()).unwrap(),
            Some(value)
        );
        assert!(parse_response_body(b"<html>nope</html>").is_err());
    }

    /// Endpoint and OAuth validation rejects bad input at build time.
    #[test]
    fn config_validation() {
        let base = HttpMcpConfig {
            endpoint: "http://127.0.0.1:1/mcp".to_owned(),
            headers: HashMap::new(),
            oauth: None,
        };
        assert!(HttpMcp::new(base.clone()).is_ok());
        for bad in ["", "not-a-url", "ftp://host/mcp", "wss://host/mcp"] {
            let mut config = base.clone();
            config.endpoint = bad.to_owned();
            assert!(HttpMcp::new(config).is_err(), "endpoint {bad:?} rejected");
        }
        let mut oauth = base.clone();
        oauth.oauth = Some(OAuthClientCredentials {
            token_url: "https://auth.example.com/token".to_owned(),
            client_id: "".to_owned(),
            client_secret: "s".to_owned(),
            scope: None,
        });
        assert!(HttpMcp::new(oauth).is_err(), "empty client id rejected");
    }

    /// The OAuth token endpoint carries the client secret in the request
    /// body: plain http is only accepted on the loopback interface, and a
    /// non-loopback http token URL is rejected at build time.
    #[test]
    fn oauth_token_url_rejects_cleartext_off_loopback() {
        let base = HttpMcpConfig {
            endpoint: "http://127.0.0.1:1/mcp".to_owned(),
            headers: HashMap::new(),
            oauth: None,
        };
        for ok in [
            "https://auth.example.com/token",
            "http://127.0.0.1:1/token",
            "http://localhost:1/token",
            "http://[::1]:1/token",
        ] {
            let mut config = base.clone();
            config.oauth = Some(OAuthClientCredentials {
                token_url: ok.to_owned(),
                client_id: "wave".to_owned(),
                client_secret: "s".to_owned(),
                scope: None,
            });
            assert!(HttpMcp::new(config).is_ok(), "token url {ok:?} accepted");
        }
        for bad in [
            "http://auth.example.com/token",
            "http://192.168.1.10/token",
            "http://10.0.0.1/token",
            "ftp://auth.example.com/token",
        ] {
            let mut config = base.clone();
            config.oauth = Some(OAuthClientCredentials {
                token_url: bad.to_owned(),
                client_id: "wave".to_owned(),
                client_secret: "s".to_owned(),
                scope: None,
            });
            let error = match HttpMcp::new(config) {
                Err(error) => error,
                Ok(_) => panic!("token url {bad:?} must be rejected"),
            };
            assert!(
                error.to_string().contains("oauth_token_url"),
                "token url {bad:?} rejected with an oauth_token_url reason, got: {error}"
            );
        }
    }

    /// The connect-retry gate is a read-only allowlist: handshake, pings,
    /// list/read calls, and the never-sent notification may re-dial;
    /// everything with a side effect (`tools/call`) and unknown methods
    /// must surface instead of retrying.
    #[test]
    fn connect_retry_gate_is_a_read_only_allowlist() {
        for method in [
            "initialize",
            "ping",
            "tools/list",
            "resources/list",
            "resources/read",
            "prompts/list",
            "prompts/get",
            "notifications/initialized",
        ] {
            assert!(
                is_connect_retryable(Some(method)),
                "{method} may re-dial after a connect-phase failure"
            );
        }
        for method in [
            "tools/call",
            "resources/subscribe",
            "logging/setLevel",
            "unknown/method",
        ] {
            assert!(
                !is_connect_retryable(Some(method)),
                "{method} must never re-dial"
            );
        }
        assert!(!is_connect_retryable(None), "methodless body never retries");
    }

    /// A connect-refused endpoint fails both attempts for a retriable
    /// method and surfaces the transport error (bounded retry, no hang).
    #[tokio::test]
    async fn connect_refused_surfaces_after_bounded_retry() {
        let transport = HttpMcp::new(HttpMcpConfig {
            // Port 1 refuses connections; the dial never leaves the process,
            // so the retry fires and the second failure surfaces.
            endpoint: "http://127.0.0.1:1/mcp".to_owned(),
            headers: HashMap::new(),
            oauth: None,
        })
        .unwrap();
        let error = transport
            .rpc("tools/list", serde_json::json!({}))
            .await
            .unwrap_err();
        assert!(
            matches!(error, TransportError::Http(_)),
            "connect refusal surfaces as an HTTP transport error, got: {error}"
        );
    }

    /// A connect-refused token endpoint re-dials the grant once and then
    /// surfaces: the elapsed time spans the inter-attempt backoff, locking
    /// that the retry actually fired instead of failing fast. The grant
    /// mints a token with no user-visible side effect and a connect
    /// failure proves no bytes were written, so the re-dial is safe.
    #[tokio::test]
    async fn oauth_token_connect_refusal_retries_once_then_surfaces() {
        let transport = HttpMcp::new(HttpMcpConfig {
            // The token fetch fails before any MCP endpoint is dialed, so
            // the discard port here is never reached.
            endpoint: "http://127.0.0.1:1/mcp".to_owned(),
            headers: HashMap::new(),
            oauth: Some(OAuthClientCredentials {
                token_url: "http://127.0.0.1:1/token".to_owned(),
                client_id: "wave".to_owned(),
                client_secret: "s3cret".to_owned(),
                scope: None,
            }),
        })
        .unwrap();
        let started = std::time::Instant::now();
        let error = transport
            .rpc("initialize", serde_json::json!({}))
            .await
            .unwrap_err();
        assert!(
            matches!(error, TransportError::Http(_))
                && error.to_string().contains("OAuth token request failed"),
            "the token fetch surfaces as an HTTP transport error, got: {error}"
        );
        assert!(
            started.elapsed() >= CONNECT_RETRY_BACKOFF,
            "the backoff between the two dials must have run, got {:?}",
            started.elapsed()
        );
    }

    /// A per-request timeout is never retried: the server may have
    /// received and acted on the request, so one re-issue could double a
    /// side effect. The budget is one real second, not paused time: a
    /// paused runtime can fire the timer before macOS reports the
    /// loopback accept, so the dial count would stay at zero. The server
    /// holds the socket and never answers; the dial count proves no
    /// second attempt was made.
    #[tokio::test]
    async fn timeout_is_never_retried() {
        let dials = Arc::new(AtomicUsize::new(0));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let counter = dials.clone();
        tokio::spawn(async move {
            let mut held = Vec::new();
            loop {
                let Ok((socket, _)) = listener.accept().await else {
                    return;
                };
                counter.fetch_add(1, Ordering::SeqCst);
                // Hold the connection open; never answer.
                held.push(socket);
            }
        });
        let transport = HttpMcp::new(HttpMcpConfig {
            endpoint: format!("http://{addr}/mcp"),
            headers: HashMap::new(),
            oauth: None,
        })
        .unwrap()
        .with_request_timeout(Duration::from_secs(1));
        let error = transport
            .rpc("tools/list", serde_json::json!({}))
            .await
            .unwrap_err();
        assert!(
            matches!(error, TransportError::Timeout(1)),
            "an unanswered server surfaces as the request timeout, got: {error}"
        );
        assert_eq!(
            dials.load(Ordering::SeqCst),
            1,
            "a timeout must never be retried, even for a read-only method"
        );
    }

    /// A send failure outside the connect phase never retries, not even
    /// for a retriable method: the server closes after reading the
    /// request, so the bytes definitely left the process and a re-issue
    /// could double a side effect. The dial count locks the negative path.
    #[tokio::test]
    async fn non_connect_send_failure_is_never_retried() {
        use tokio::io::AsyncReadExt;
        let dials = Arc::new(AtomicUsize::new(0));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let counter = dials.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                counter.fetch_add(1, Ordering::SeqCst);
                // Consume part of the request (proving the connection and
                // the write succeeded), then drop without answering: the
                // client fails in the response phase, which is never a
                // connect error.
                let mut buf = [0u8; 8192];
                let _ = socket.read(&mut buf).await;
                drop(socket);
            }
        });
        let transport = HttpMcp::new(HttpMcpConfig {
            endpoint: format!("http://{addr}/mcp"),
            headers: HashMap::new(),
            oauth: None,
        })
        .unwrap();
        let error = transport
            .rpc("tools/list", serde_json::json!({}))
            .await
            .unwrap_err();
        assert_eq!(
            dials.load(Ordering::SeqCst),
            1,
            "a non-connect send failure must not retry"
        );
        assert!(
            matches!(error, TransportError::Http(_)),
            "the failure surfaces as an HTTP transport error, got: {error}"
        );
    }
}
