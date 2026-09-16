/*!
 * @file RetryPolicy
 * @description Bounded retry executor for streaming chat-model requests.
 *
 * Responsibilities:
 * - Classify provider errors into retryable vs fail-fast classes.
 * - Re-issue failed requests with capped exponential backoff.
 * - Enforce a global deadline so retries never hang unboundedly.
 *
 * This module must not depend on: runtime, config, or UI-layer components.
 */

//! Retry wrapper over [`crate::ChatModel`].
//!
//! [`RetryingModel`] clones the request per attempt (cheap: history rides
//! an `Arc`) and retries only transient failures: stall timeouts,
//! connection errors, 5xx/server-overload shapes, and rate limiting
//! (429). Authentication, other 4xx, and
//! [`crate::LlmError::PromptTooLong`] fail fast so bad credentials never
//! burn retries and compaction triggers stay intact.
//!
//! Retry covers request establishment only ([`ChatModel::stream`]); a
//! stream that fails mid-flight surfaces its item error unchanged (no
//! cross-attempt resume, which could duplicate side effects).

use std::time::{Duration, Instant};

use crate::{ChatModel, ChatRequest, EventStream, LlmError, Result};

/// Which transport failure classes may be retried.
#[derive(Debug, Clone)]
pub struct RetryPolicy {
    /// Total attempts including the first try; clamped to >= 1.
    pub max_attempts: usize,
    /// Base backoff between attempts; doubled per attempt.
    pub base_delay_ms: u64,
    /// Upper bound for any single backoff sleep.
    pub max_delay_ms: u64,
    /// Retry stall-timeout errors.
    pub retry_timeout: bool,
    /// Retry transient API shapes: 5xx / overloaded / server-error and
    /// rate-limit (429) responses.
    pub retry_transient: bool,
    /// Retry connection-level transport errors.
    pub retry_connection: bool,
    /// Global budget from the first attempt; when the next backoff would
    /// overrun it, the last error returns immediately instead of sleeping.
    /// `None` means attempts (not time) bound the loop.
    pub deadline_ms: Option<u64>,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 3,
            base_delay_ms: 200,
            max_delay_ms: 2_000,
            retry_timeout: true,
            retry_transient: true,
            retry_connection: true,
            deadline_ms: Some(30_000),
        }
    }
}

impl RetryPolicy {
    /// Backoff after `failed_attempt` (1-based) failures, capped.
    pub fn delay_for_attempt(&self, failed_attempt: usize) -> Duration {
        let shift = failed_attempt.saturating_sub(1).min(16) as u32;
        let doubled = self.base_delay_ms.saturating_mul(1u64 << shift);
        Duration::from_millis(doubled.min(self.max_delay_ms))
    }

    /// True when `error` may be retried under this policy.
    ///
    /// Auth failures always return false, even when their carrier looks
    /// retryable: retrying bad credentials across attempts (or providers)
    /// only burns budget and risks lockout.
    pub fn is_retryable(&self, error: &LlmError) -> bool {
        if is_auth_error(error) {
            return false;
        }
        match error {
            LlmError::Timeout(_) => self.retry_timeout,
            LlmError::Http(_) => self.retry_connection,
            LlmError::Api { kind, message } => self.retry_transient && is_transient_error(kind, message),
            // PromptTooLong drives compaction, Json/Sse are deterministic:
            // retrying cannot change the outcome.
            LlmError::PromptTooLong { .. } | LlmError::Sse(_) | LlmError::Json(_) => false,
        }
    }
}

/// True for authentication / authorization failures, which must fail fast
/// and must never trigger cross-provider fallback (keys stay with the
/// provider they were issued for).
pub fn is_auth_error(error: &LlmError) -> bool {
    match error {
        LlmError::Api { kind, message } => {
            let kind = kind.to_lowercase();
            let message = message.to_lowercase();
            kind.contains("http_401")
                || kind.contains("http_403")
                || kind.contains("unauthorized")
                || kind.contains("forbidden")
                || kind.contains("authentication_error")
                || kind.contains("invalid_api_key")
                || message.contains("invalid api key")
                || message.contains("invalid_api_key")
                || message.contains("incorrect api key")
                || message.contains("unauthorized")
                || message.contains("authentication")
                || message.contains("forbidden")
        }
        LlmError::Http(message) => {
            let message = message.to_lowercase();
            message.contains("invalid api key")
                || message.contains("unauthorized")
                || message.contains("authentication")
        }
        LlmError::Timeout(_)
        | LlmError::PromptTooLong { .. }
        | LlmError::Sse(_)
        | LlmError::Json(_) => false,
    }
}

/// True for transient API shapes worth retrying: explicit 5xx status
/// kinds, the well-known overload markers, and rate limiting (429) — all
/// carriers are checked because providers split status and message across
/// them inconsistently.
fn is_transient_error(kind: &str, message: &str) -> bool {
    const MARKERS: &[&str] = &[
        "http_500",
        "http_502",
        "http_503",
        "http_504",
        "http_529",
        "http_429",
        "rate_limit",
        "overloaded",
        "overloaded_error",
        "server_error",
        "service_unavailable",
        "bad_gateway",
        "gateway_timeout",
        "temporarily_unavailable",
    ];
    let kind = kind.to_lowercase();
    let message = message.to_lowercase();
    MARKERS
        .iter()
        .any(|m| kind.contains(m) || message.contains(m))
}

/// [`ChatModel`] wrapper applying [`RetryPolicy`] to request establishment.
pub struct RetryingModel<M> {
    inner: M,
    policy: RetryPolicy,
}

impl<M> RetryingModel<M> {
    /// Wrap `inner`; a zero `max_attempts` is clamped to 1 (fail fast,
    /// never zero attempts).
    pub fn new(inner: M, mut policy: RetryPolicy) -> Self {
        if policy.max_attempts < 1 {
            policy.max_attempts = 1;
        }
        Self { inner, policy }
    }

    /// The active policy (attempt counts, backoff bound, deadline).
    pub fn policy(&self) -> &RetryPolicy {
        &self.policy
    }
}

#[async_trait::async_trait]
impl<M: ChatModel> ChatModel for RetryingModel<M> {
    async fn stream(&self, req: ChatRequest) -> Result<EventStream> {
        let start = Instant::now();
        let mut attempt: usize = 0;
        loop {
            attempt += 1;
            match self.inner.stream(req.clone()).await {
                Ok(stream) => return Ok(stream),
                Err(error) => {
                    let exhausted = attempt >= self.policy.max_attempts;
                    if exhausted || !self.policy.is_retryable(&error) {
                        return Err(error);
                    }
                    let delay = self.policy.delay_for_attempt(attempt);
                    let overrun = self.policy.deadline_ms.is_some_and(|budget| {
                        start.elapsed() + delay > Duration::from_millis(budget)
                    });
                    if overrun {
                        return Err(error);
                    }
                    tokio::time::sleep(delay).await;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn request() -> ChatRequest {
        ChatRequest {
            model: "test".to_string(),
            system: String::new(),
            messages: std::sync::Arc::new(Vec::new()),
            tools: Vec::new(),
            max_tokens: 10,
        }
    }

    fn empty_stream() -> EventStream {
        Box::pin(futures::stream::iter(Vec::new()))
    }

    /// Fails with the queued errors, then succeeds; counts attempts.
    struct Script {
        failures: Mutex<VecDeque<LlmError>>,
        attempts: AtomicUsize,
    }

    impl Script {
        fn failing(errors: Vec<LlmError>) -> Self {
            Self {
                failures: Mutex::new(errors.into()),
                attempts: AtomicUsize::new(0),
            }
        }

        fn attempts(&self) -> usize {
            self.attempts.load(Ordering::SeqCst)
        }
    }

    #[async_trait::async_trait]
    impl ChatModel for Script {
        async fn stream(&self, _req: ChatRequest) -> Result<EventStream> {
            self.attempts.fetch_add(1, Ordering::SeqCst);
            match self
                .failures
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .pop_front()
            {
                Some(error) => Err(error),
                None => Ok(empty_stream()),
            }
        }
    }

    fn fast_policy(max_attempts: usize) -> RetryPolicy {
        RetryPolicy {
            max_attempts,
            base_delay_ms: 0,
            max_delay_ms: 0,
            retry_timeout: true,
            retry_transient: true,
            retry_connection: true,
            deadline_ms: None,
        }
    }

    fn timeout() -> LlmError {
        LlmError::Timeout("stall".to_string())
    }

    fn overloaded() -> LlmError {
        LlmError::Api {
            kind: "overloaded_error".to_string(),
            message: "overloaded".to_string(),
        }
    }

    fn auth() -> LlmError {
        LlmError::Api {
            kind: "http_401".to_string(),
            message: "invalid api key".to_string(),
        }
    }

    #[tokio::test]
    async fn retries_then_succeeds_counting_attempts() {
        let script = Script::failing(vec![timeout(), overloaded()]);
        let model = RetryingModel::new(script, fast_policy(3));
        let stream = model.stream(request()).await.unwrap();
        drop(stream);
        assert_eq!(model.inner.attempts(), 3);
    }

    #[tokio::test]
    async fn gives_up_after_max_attempts() {
        let script = Script::failing(vec![timeout(), timeout(), timeout(), timeout()]);
        let model = RetryingModel::new(script, fast_policy(2));
        assert!(model.stream(request()).await.is_err());
        assert_eq!(model.inner.attempts(), 2);
    }

    #[tokio::test]
    async fn non_retryable_errors_fail_fast() {
        for error in [
            auth(),
            LlmError::PromptTooLong {
                message: "too long".to_string(),
            },
            LlmError::Api {
                kind: "http_400".to_string(),
                message: "bad request".to_string(),
            },
        ] {
            let script = Script::failing(vec![error]);
            let model = RetryingModel::new(script, fast_policy(3));
            assert!(model.stream(request()).await.is_err());
            assert_eq!(model.inner.attempts(), 1);
        }
    }

    #[tokio::test]
    async fn zero_max_attempts_clamps_to_one() {
        let script = Script::failing(vec![timeout()]);
        let model = RetryingModel::new(script, fast_policy(0));
        assert_eq!(model.policy().max_attempts, 1);
        assert!(model.stream(request()).await.is_err());
        assert_eq!(model.inner.attempts(), 1);
    }

    #[tokio::test]
    async fn deadline_skips_sleep_and_returns_last_error() {
        let script = Script::failing(vec![timeout(), timeout()]);
        let model = RetryingModel::new(
            script,
            RetryPolicy {
                deadline_ms: Some(0),
                ..fast_policy(3)
            },
        );
        assert!(model.stream(request()).await.is_err());
        assert_eq!(model.inner.attempts(), 1);
    }

    #[test]
    fn backoff_doubles_then_caps() {
        let policy = RetryPolicy {
            base_delay_ms: 100,
            max_delay_ms: 250,
            ..RetryPolicy::default()
        };
        assert_eq!(policy.delay_for_attempt(1), Duration::from_millis(100));
        assert_eq!(policy.delay_for_attempt(2), Duration::from_millis(200));
        assert_eq!(policy.delay_for_attempt(3), Duration::from_millis(250));
        assert_eq!(policy.delay_for_attempt(30), Duration::from_millis(250));
    }

    #[test]
    fn retry_class_flags_gate_each_class() {
        let http: LlmError = LlmError::Http("connection reset".to_string());
        let off = RetryPolicy {
            retry_timeout: false,
            retry_transient: false,
            retry_connection: false,
            ..fast_policy(3)
        };
        assert!(!off.is_retryable(&timeout()));
        assert!(!off.is_retryable(&overloaded()));
        assert!(!off.is_retryable(&http));
        let on = fast_policy(3);
        assert!(on.is_retryable(&timeout()));
        assert!(on.is_retryable(&overloaded()));
        assert!(on.is_retryable(&http));
        // Auth never retries even with every class enabled.
        assert!(!on.is_retryable(&auth()));
    }
}
