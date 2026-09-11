/*!
 * @file TokenBucket
 * @description Token-bucket rate limiting with an explicit clock.
 *
 * Responsibilities:
 * - Admit bursts up to capacity, then throttle to the refill rate.
 * - Refill lazily from caller-supplied timestamps.
 * - Stay deterministic under test with no hidden time reads.
 * - Reject invalid configurations via a validated constructor.
 * - Support batch acquisition and retry-hint queries.
 *
 * This module must not depend on: any other workspace crate.
 */

//! Rate limiting as pure arithmetic over caller time.

/// Configuration failures for [`TokenBucket::try_new`].
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum BucketConfigError {
    /// Burst capacity must be at least one token.
    #[error("capacity must be at least 1, got {0}")]
    EmptyCapacity(u32),
    /// Sustained rate must be a finite, non-negative number.
    #[error("refill rate must be finite and >= 0, got {0}")]
    InvalidRefillRate(f64),
    /// The stamp must be a finite timestamp.
    #[error("timestamp must be finite, got {0}")]
    InvalidTimestamp(f64),
}

/// Token bucket: `capacity` burst, `refill_per_sec` sustained rate.
#[derive(Debug, Clone)]
pub struct TokenBucket {
    capacity: f64,
    refill_per_sec: f64,
    tokens: f64,
    last_secs: f64,
}

impl TokenBucket {
    /// Create a full bucket stamped at `now_secs`.
    pub fn new(capacity: u32, refill_per_sec: f64, now_secs: f64) -> Self {
        Self {
            capacity: capacity as f64,
            refill_per_sec: refill_per_sec.max(0.0),
            tokens: capacity as f64,
            last_secs: now_secs,
        }
    }

    /// Create a full bucket, rejecting invalid configurations.
    ///
    /// Unlike [`TokenBucket::new`], this reports a zero capacity, a
    /// negative or non-finite refill rate, and a non-finite timestamp
    /// instead of clamping or accepting them silently.
    pub fn try_new(
        capacity: u32,
        refill_per_sec: f64,
        now_secs: f64,
    ) -> Result<Self, BucketConfigError> {
        if capacity == 0 {
            return Err(BucketConfigError::EmptyCapacity(capacity));
        }
        if !refill_per_sec.is_finite() || refill_per_sec < 0.0 {
            return Err(BucketConfigError::InvalidRefillRate(refill_per_sec));
        }
        if !now_secs.is_finite() {
            return Err(BucketConfigError::InvalidTimestamp(now_secs));
        }
        Ok(Self::new(capacity, refill_per_sec, now_secs))
    }

    /// Try to take one token at `now_secs`; false leaves state unchanged
    /// except for the refill accounting.
    pub fn try_acquire(&mut self, now_secs: f64) -> bool {
        self.try_acquire_n(1, now_secs)
    }

    /// Try to take `n` tokens at `now_secs`.
    ///
    /// A zero request always succeeds without consuming anything.
    /// A request larger than the burst capacity can never succeed and
    /// returns false, while still applying the refill accounting.
    /// Denials leave the token count unchanged except for refills.
    pub fn try_acquire_n(&mut self, n: u32, now_secs: f64) -> bool {
        self.refill(now_secs);
        let need = n as f64;
        if self.tokens >= need {
            self.tokens -= need;
            true
        } else {
            false
        }
    }

    /// Seconds from `now_secs` until `n` tokens are available.
    ///
    /// Returns `Some(0.0)` when the request can be satisfied immediately
    /// and `None` when it can never be satisfied: either `n` exceeds the
    /// burst capacity or the bucket never refills. Never mutates state.
    pub fn time_until_available(&self, n: u32, now_secs: f64) -> Option<f64> {
        let need = n as f64;
        if need > self.capacity {
            return None;
        }
        let elapsed = (now_secs - self.last_secs).max(0.0);
        let projected = (self.tokens + elapsed * self.refill_per_sec).min(self.capacity);
        if projected >= need {
            return Some(0.0);
        }
        if !self.refill_per_sec.is_finite() || self.refill_per_sec <= 0.0 {
            return None;
        }
        Some((need - projected) / self.refill_per_sec)
    }

    /// Whole tokens currently available (floored for display).
    pub fn available(&self) -> u32 {
        self.tokens.floor().max(0.0) as u32
    }

    /// Refill from elapsed time, clamping at capacity and ignoring
    /// backwards clocks without panicking.
    fn refill(&mut self, now_secs: f64) {
        let elapsed = (now_secs - self.last_secs).max(0.0);
        self.last_secs = now_secs;
        self.tokens = (self.tokens + elapsed * self.refill_per_sec).min(self.capacity);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn burst_then_throttles_then_refills() {
        let mut bucket = TokenBucket::new(2, 1.0, 0.0);
        assert!(bucket.try_acquire(0.0));
        assert!(bucket.try_acquire(0.0));
        assert!(!bucket.try_acquire(0.0));
        // Half a second refills half a token: still denied.
        assert!(!bucket.try_acquire(0.5));
        assert!(bucket.try_acquire(1.0));
        assert_eq!(bucket.available(), 0);
    }

    #[test]
    fn backwards_clocks_never_overfill() {
        let mut bucket = TokenBucket::new(1, 100.0, 10.0);
        assert!(bucket.try_acquire(10.0));
        assert!(!bucket.try_acquire(5.0));
        assert!(bucket.try_acquire(10.02));
    }

    #[test]
    fn try_new_rejects_invalid_configs() {
        assert_eq!(
            TokenBucket::try_new(0, 1.0, 0.0).unwrap_err(),
            BucketConfigError::EmptyCapacity(0)
        );
        assert_eq!(
            TokenBucket::try_new(1, -1.0, 0.0).unwrap_err(),
            BucketConfigError::InvalidRefillRate(-1.0)
        );
        assert!(matches!(
            TokenBucket::try_new(1, f64::NAN, 0.0),
            Err(BucketConfigError::InvalidRefillRate(_))
        ));
        assert_eq!(
            TokenBucket::try_new(1, f64::INFINITY, 0.0).unwrap_err(),
            BucketConfigError::InvalidRefillRate(f64::INFINITY)
        );
        assert!(matches!(
            TokenBucket::try_new(1, 1.0, f64::NAN),
            Err(BucketConfigError::InvalidTimestamp(_))
        ));
        assert!(TokenBucket::try_new(2, 1.0, 0.0).is_ok());
    }

    #[test]
    fn batch_acquire_consumes_n_tokens() {
        let mut bucket = TokenBucket::new(4, 1.0, 0.0);
        assert!(bucket.try_acquire_n(3, 0.0));
        assert_eq!(bucket.available(), 1);
        assert!(!bucket.try_acquire_n(2, 0.0));
        // Denial leaves the remainder intact.
        assert!(bucket.try_acquire_n(1, 0.0));
        // Zero-cost requests succeed without consuming.
        assert!(bucket.try_acquire_n(0, 0.0));
        assert_eq!(bucket.available(), 0);
    }

    #[test]
    fn oversized_batch_never_succeeds_but_keeps_refilling() {
        let mut bucket = TokenBucket::new(2, 1.0, 0.0);
        assert!(!bucket.try_acquire_n(3, 0.0));
        assert!(!bucket.try_acquire_n(3, 100.0));
        // Single-token traffic still flows after the refill.
        assert!(bucket.try_acquire(100.0));
    }

    #[test]
    fn retry_hint_tracks_refill_without_mutating() {
        let mut bucket = TokenBucket::new(1, 1.0, 0.0);
        assert!(bucket.try_acquire(0.0));
        assert_eq!(bucket.time_until_available(1, 0.0), Some(1.0));
        assert_eq!(bucket.time_until_available(1, 0.5), Some(0.5));
        assert_eq!(bucket.time_until_available(1, 1.0), Some(0.0));
        // A pure query must not consume the refilled token.
        assert_eq!(bucket.available(), 0);
        assert!(bucket.try_acquire(1.0));
    }

    #[test]
    fn retry_hint_never_when_unfillable() {
        let mut no_refill = TokenBucket::new(1, 0.0, 0.0);
        assert!(no_refill.try_acquire(0.0));
        assert_eq!(no_refill.time_until_available(1, 0.0), None);
        assert_eq!(no_refill.time_until_available(1, 3600.0), None);

        let bucket = TokenBucket::new(2, 1.0, 0.0);
        assert_eq!(bucket.time_until_available(3, 0.0), None);
        assert_eq!(bucket.time_until_available(0, 0.0), Some(0.0));
    }
}
