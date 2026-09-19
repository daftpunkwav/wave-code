//! Local client-side throttling: a token bucket in front of a chat model.
//!
//! Providers throttle server-side with 429s; this wrapper keeps a
//! misbehaving retry loop from ever getting there. The bucket admits
//! `rpm` requests per rolling minute; when empty, the caller waits for
//! the refill (bounded by a hard wait ceiling so a hung clock cannot
//! stall a turn forever — the deadline budget in the retry layer still
//! applies on top).

use std::sync::Mutex;
use std::time::Duration;

use infrastructure_ratelimit::TokenBucket;

/// Hard ceiling on how long one request may wait for a token.
const MAX_WAIT: Duration = Duration::from_secs(30);
/// Poll granularity while waiting for refill.
const POLL: Duration = Duration::from_millis(100);

/// Chat-model decorator gating every request through a token bucket.
pub struct RateLimitedModel<M> {
    inner: M,
    bucket: Mutex<TokenBucket>,
}

impl<M> RateLimitedModel<M> {
    /// Wrap `inner` with a `requests_per_minute` throttle.
    pub fn new(inner: M, requests_per_minute: u32) -> Self {
        let now = now_secs();
        Self {
            inner,
            bucket: Mutex::new(TokenBucket::new(
                requests_per_minute.max(1),
                requests_per_minute.max(1) as f64 / 60.0,
                now,
            )),
        }
    }

    /// Wait until a token is available (or the wait ceiling expires).
    fn acquire(&self) -> bool {
        let deadline = std::time::Instant::now() + MAX_WAIT;
        loop {
            {
                let mut bucket = self.bucket.lock().unwrap_or_else(|e| e.into_inner());
                if bucket.try_acquire(now_secs()) {
                    return true;
                }
            }
            if std::time::Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(POLL);
        }
    }
}

fn now_secs() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

#[async_trait::async_trait]
impl<M: wavecode_llm::ChatModel> wavecode_llm::ChatModel for RateLimitedModel<M> {
    async fn stream(
        &self,
        req: wavecode_llm::ChatRequest,
    ) -> wavecode_llm::Result<wavecode_llm::EventStream> {
        self.acquire();
        self.inner.stream(req).await
    }

    fn set_thinking(&self, effort: &str) -> bool {
        self.inner.set_thinking(effort)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wavecode_llm::ChatModel as _;

    struct CountingModel {
        calls: std::sync::atomic::AtomicUsize,
    }

    #[async_trait::async_trait]
    impl wavecode_llm::ChatModel for CountingModel {
        async fn stream(
            &self,
            _req: wavecode_llm::ChatRequest,
        ) -> wavecode_llm::Result<wavecode_llm::EventStream> {
            use std::sync::atomic::Ordering;
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(Box::pin(futures::stream::iter(vec![])))
        }

        fn set_thinking(&self, _effort: &str) -> bool {
            true
        }
    }

    fn request() -> wavecode_llm::ChatRequest {
        wavecode_llm::ChatRequest {
            model: "m".to_string(),
            system: String::new(),
            messages: std::sync::Arc::new(Vec::new()),
            tools: Vec::new(),
            max_tokens: 1,
        }
    }

    /// A generous bucket lets a burst through without waiting, and
    /// thinking-level changes forward.
    #[tokio::test]
    async fn burst_within_capacity_is_unthrottled() {
        use futures::StreamExt as _;
        let model = RateLimitedModel::new(
            CountingModel {
                calls: std::sync::atomic::AtomicUsize::new(0),
            },
            60,
        );
        for _ in 0..3 {
            let mut stream = model.stream(request()).await.unwrap();
            drop(stream.next());
        }
        assert!(model.set_thinking("low"));
    }
}
