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
//! (429, honoring the server's `Retry-After` hint over local backoff).
//! Authentication, quota exhaustion, other 4xx, and
//! [`crate::LlmError::PromptTooLong`] fail fast so bad credentials never
//! burn retries, an empty account never burns backoff, and compaction
//! triggers stay intact.
//!
//! Mid-stream tears retry only while nothing has been forwarded: the
//! half response is dropped wholesale and the request re-issued. After
//! the first event reaches the consumer an error surfaces unchanged —
//! re-issuing then would duplicate delivered text and re-run tool side
//! effects.

use std::time::{Duration, Instant};

use futures::StreamExt as _;

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

    /// De-synchronize retries: multiplicative jitter over the deterministic
    /// backoff so N clients of one provider do not all re-hit at the same
    /// tick. Seeded from the clock (no rng dependency); the result lands in
    /// `[delay / 2, delay)`, never below zero and never above the cap the
    /// deterministic delay already respects.
    pub fn jittered(&self, delay: Duration, failed_attempt: usize) -> Duration {
        if delay.is_zero() {
            return delay;
        }
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|t| t.subsec_nanos() as u64 ^ ((failed_attempt as u64) << 32))
            .unwrap_or(0);
        let half = delay.as_nanos() as u64 / 2;
        let offset = if half == 0 { 0 } else { nanos % half };
        Duration::from_nanos(delay.as_nanos() as u64 - half + offset)
    }

    /// True when `error` may be retried under this policy.
    ///
    /// Auth failures and quota exhaustion always return false, even when
    /// their carrier looks retryable: retrying bad credentials burns
    /// budget and risks lockout, and retrying an exhausted quota only
    /// burns the backoff budget on an error that needs human action.
    pub fn is_retryable(&self, error: &LlmError) -> bool {
        if is_auth_error(error) {
            return false;
        }
        if is_quota_error(error) {
            return false;
        }
        match error {
            LlmError::Timeout(_) => self.retry_timeout,
            LlmError::Http(_) => self.retry_connection,
            LlmError::RateLimited { .. } => self.retry_transient,
            LlmError::Api { kind, message } => {
                self.retry_transient && is_transient_error(kind, message)
            }
            // PromptTooLong drives compaction, Json/Sse are deterministic, ClientInit is configuration:
            // retrying cannot change the outcome.
            LlmError::PromptTooLong { .. }
            | LlmError::Sse(_)
            | LlmError::Json(_)
            | LlmError::ClientInit(_) => false,
        }
    }

    /// Backoff after `failed_attempt` failures, honoring the server's
    /// `Retry-After` hint when the error carries one: the hint wins over
    /// both the exponential guess and `max_delay_ms` (only the global
    /// deadline bounds it).
    pub fn delay_for_error(&self, error: &LlmError, failed_attempt: usize) -> Duration {
        if let LlmError::RateLimited {
            retry_after: Some(hint),
            ..
        } = error
        {
            return *hint;
        }
        self.delay_for_attempt(failed_attempt)
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
        | LlmError::RateLimited { .. }
        | LlmError::PromptTooLong { .. }
        | LlmError::Sse(_)
        | LlmError::Json(_)
        | LlmError::ClientInit(_) => false,
    }
}

/// True for transient API shapes worth retrying: explicit 5xx status
/// kinds, the well-known overload markers, and rate limiting (429) — all
/// carriers are checked because providers split status and message across
/// them inconsistently.
///
/// Consulted only after [`is_auth_error`] and [`is_quota_error`] rejected
/// the error (see [`RetryPolicy::is_retryable`]): the three marker tables
/// are kept deliberately disjoint, and this one must never grow a billing
/// marker — a quota shape that lands here would burn the retry budget on
/// an error only a human can fix.
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

/// True for quota / credit exhaustion: the account is out of budget, so
/// retrying cannot succeed and only burns the backoff budget. Marker
/// list is deliberately narrow (billing-specific shapes) to avoid
/// fail-fast on an ordinary rate limit, which IS worth retrying.
///
/// Priority contract: checked before `is_transient_error` in
/// [`RetryPolicy::is_retryable`], so a billing marker added here takes
/// precedence over any retryable marker an error also carries — keep the
/// two tables disjoint or extend `quota_errors_fail_fast` first.
pub fn is_quota_error(error: &LlmError) -> bool {
    const MARKERS: &[&str] = &[
        "insufficient_quota",
        "quota_exceeded",
        "quota exceeded",
        "exceeded your current quota",
        "billing",
        "credit balance",
        "arrears",
    ];
    let matches = |kind: &str, message: &str| {
        let kind = kind.to_lowercase();
        let message = message.to_lowercase();
        MARKERS
            .iter()
            .any(|m| kind.contains(m) || message.contains(m))
    };
    match error {
        LlmError::Api { kind, message } => matches(kind, message),
        LlmError::RateLimited { message, .. } => matches("", message),
        _ => false,
    }
}

/// [`ChatModel`] wrapper applying [`RetryPolicy`] to request establishment
/// and to stream tears before the first event reaches the consumer.
pub struct RetryingModel<M> {
    inner: std::sync::Arc<M>,
    policy: RetryPolicy,
}

impl<M> RetryingModel<M> {
    /// Wrap `inner`; a zero `max_attempts` is clamped to 1 (fail fast,
    /// never zero attempts).
    pub fn new(inner: M, mut policy: RetryPolicy) -> Self {
        if policy.max_attempts < 1 {
            policy.max_attempts = 1;
        }
        Self {
            inner: std::sync::Arc::new(inner),
            policy,
        }
    }

    /// The active policy (attempt counts, backoff bound, deadline).
    pub fn policy(&self) -> &RetryPolicy {
        &self.policy
    }
}

#[async_trait::async_trait]
impl<M: ChatModel + 'static> ChatModel for RetryingModel<M> {
    async fn stream(&self, req: ChatRequest) -> Result<EventStream> {
        // First attempt stays eager with the plain `Err` return: callers
        // key establishment failures off it (PromptTooLong drives the
        // reactive compactor, auth errors surface immediately).
        let start = Instant::now();
        let mut attempt: usize = 0;
        loop {
            attempt += 1;
            match self.inner.stream(req.clone()).await {
                Ok(stream) => {
                    let wrapped = retrying_items(
                        self.inner.clone(),
                        self.policy.clone(),
                        req,
                        stream,
                        attempt,
                        start,
                    );
                    return Ok(Box::pin(wrapped));
                }
                Err(error) => {
                    let exhausted = attempt >= self.policy.max_attempts;
                    if exhausted || !self.policy.is_retryable(&error) {
                        return Err(error);
                    }
                    let delay = self
                        .policy
                        .jittered(self.policy.delay_for_error(&error, attempt), attempt);
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

    fn set_thinking(&self, effort: &str) -> bool {
        self.inner.set_thinking(effort)
    }
}

/// Wrap a successfully established stream so a tear before any event was
/// forwarded re-issues the request; tears after forwarding surface
/// unchanged (re-issuing would duplicate already-delivered content and
/// re-run side effects). Retry attempts share the original deadline.
fn retrying_items<M: ChatModel + 'static>(
    model: std::sync::Arc<M>,
    policy: RetryPolicy,
    req: ChatRequest,
    first: EventStream,
    initial_attempt: usize,
    start: Instant,
) -> impl futures::Stream<Item = Result<crate::StreamEvent>> + Send + 'static {
    let deadline_ms = policy.deadline_ms;
    let overruns = move |delay: Duration| {
        deadline_ms.is_some_and(|budget| start.elapsed() + delay > Duration::from_millis(budget))
    };
    async_stream::stream! {
        let mut attempt = initial_attempt;
        let mut pending = Some(first);
        loop {
            let mut stream = match pending.take() {
                Some(stream) => stream,
                None => loop {
                    attempt += 1;
                    match model.stream(req.clone()).await {
                        Ok(stream) => break stream,
                        Err(error) => {
                            let delay = policy.jittered(policy.delay_for_error(&error, attempt), attempt);
                            if attempt >= policy.max_attempts
                                || !policy.is_retryable(&error)
                                || overruns(delay)
                            {
                                yield Err(error);
                                return;
                            }
                            tokio::time::sleep(delay).await;
                        }
                    }
                },
            };
            let mut forwarded = false;
            let mut torn: Option<LlmError> = None;
            while let Some(item) = stream.next().await {
                match item {
                    Ok(event) => {
                        forwarded = true;
                        yield Ok(event);
                    }
                    Err(error) => {
                        if !forwarded
                            && attempt < policy.max_attempts
                            && policy.is_retryable(&error)
                        {
                            let delay = policy.delay_for_error(&error, attempt);
                            if !overruns(delay) {
                                torn = Some(error);
                                break;
                            }
                        }
                        yield Err(error);
                        return;
                    }
                }
            }
            match torn {
                // The tear is retried wholesale: nothing reached the
                // consumer, so the half response is simply dropped.
                Some(error) => {
                    let delay = policy.jittered(policy.delay_for_error(&error, attempt), attempt);
                    tokio::time::sleep(delay).await;
                }
                None => return,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn request() -> ChatRequest {
        ChatRequest {
            model: "test".to_string(),
            system: String::new(),
            messages: std::sync::Arc::new(Vec::new()),
            tools: Arc::new(Vec::new()),
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

    /// Jitter lands in `[delay / 2, delay)` and zero stays zero: retries
    /// de-synchronize without ever out-waiting the deterministic cap.
    #[test]
    fn jitter_spreads_but_never_inverts() {
        let policy = RetryPolicy {
            base_delay_ms: 100,
            max_delay_ms: 250,
            ..RetryPolicy::default()
        };
        assert_eq!(policy.jittered(Duration::ZERO, 1), Duration::ZERO);
        let delay = policy.delay_for_attempt(1);
        for attempt in 1..=20 {
            let j = policy.jittered(delay, attempt);
            assert!(
                j >= delay / 2 && j < delay,
                "attempt {attempt}: {j:?} out of range"
            );
        }
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

    /// Streams queued item lists per attempt, then empty streams.
    struct StreamScript {
        attempts_streams: Mutex<VecDeque<Vec<Result<crate::StreamEvent>>>>,
        attempts: AtomicUsize,
    }

    impl StreamScript {
        fn scripted(streams: Vec<Vec<Result<crate::StreamEvent>>>) -> Self {
            Self {
                attempts_streams: Mutex::new(streams.into()),
                attempts: AtomicUsize::new(0),
            }
        }

        fn attempts(&self) -> usize {
            self.attempts.load(Ordering::SeqCst)
        }
    }

    #[async_trait::async_trait]
    impl ChatModel for StreamScript {
        async fn stream(&self, _req: ChatRequest) -> Result<EventStream> {
            self.attempts.fetch_add(1, Ordering::SeqCst);
            let items = self
                .attempts_streams
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .pop_front()
                .unwrap_or_default();
            Ok(Box::pin(futures::stream::iter(items)))
        }
    }

    async fn collect(model: &RetryingModel<StreamScript>) -> Vec<Result<crate::StreamEvent>> {
        use futures::StreamExt as _;
        let mut stream = model.stream(request()).await.unwrap();
        let mut items = Vec::new();
        while let Some(item) = stream.next().await {
            items.push(item);
        }
        items
    }

    #[tokio::test]
    async fn tear_before_first_event_is_retried_wholesale() {
        let script = StreamScript::scripted(vec![
            vec![Err(timeout())],
            vec![Ok(crate::StreamEvent::TextDelta {
                text: "hi".to_string(),
            })],
        ]);
        let model = RetryingModel::new(script, fast_policy(3));
        let items = collect(&model).await;
        // The torn attempt's error never surfaces; the clean retry does.
        assert_eq!(items.len(), 1);
        assert!(items[0].is_ok());
        assert_eq!(model.inner.attempts(), 2);
    }

    #[tokio::test]
    async fn tear_after_forwarding_surfaces_unchanged() {
        let script = StreamScript::scripted(vec![vec![
            Ok(crate::StreamEvent::TextDelta {
                text: "partial".to_string(),
            }),
            Err(timeout()),
        ]]);
        let model = RetryingModel::new(script, fast_policy(3));
        let items = collect(&model).await;
        assert_eq!(items.len(), 2);
        assert!(items[0].is_ok());
        assert!(items[1].is_err(), "delivered content forbids a re-issue");
        assert_eq!(model.inner.attempts(), 1);
    }

    #[test]
    fn quota_errors_fail_fast() {
        let quota = LlmError::Api {
            kind: "insufficient_quota".to_string(),
            message: "You exceeded your current quota".to_string(),
        };
        assert!(is_quota_error(&quota));
        assert!(!fast_policy(3).is_retryable(&quota));
        // An ordinary rate limit stays retryable — only billing shapes
        // fail fast.
        let rate = LlmError::Api {
            kind: "http_429".to_string(),
            message: "too many requests".to_string(),
        };
        assert!(!is_quota_error(&rate));
        assert!(fast_policy(3).is_retryable(&rate));
    }

    #[test]
    fn retry_after_hint_wins_over_local_backoff() {
        let policy = RetryPolicy {
            base_delay_ms: 100,
            max_delay_ms: 200,
            ..RetryPolicy::default()
        };
        let hinted = LlmError::RateLimited {
            message: "slow down".to_string(),
            retry_after: Some(Duration::from_secs(5)),
        };
        assert_eq!(
            policy.delay_for_error(&hinted, 1),
            Duration::from_secs(5),
            "the server hint wins over base backoff and max_delay"
        );
        let unhinted = LlmError::RateLimited {
            message: "slow down".to_string(),
            retry_after: None,
        };
        assert_eq!(
            policy.delay_for_error(&unhinted, 2),
            Duration::from_millis(200),
            "no hint falls back to the exponential schedule"
        );
        // Rate limits retry under the transient class.
        assert!(fast_policy(3).is_retryable(&hinted));
    }
}
