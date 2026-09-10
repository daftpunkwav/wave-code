/*!
 * @file TokenBucket
 * @description Token-bucket rate limiting with an explicit clock.
 *
 * Responsibilities:
 * - Admit bursts up to capacity, then throttle to the refill rate.
 * - Refill lazily from caller-supplied timestamps.
 * - Stay deterministic under test with no hidden time reads.
 *
 * This module must not depend on: any other workspace crate.
 */

//! Rate limiting as pure arithmetic over caller time.

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

    /// Try to take one token at `now_secs`; false leaves state unchanged
    /// except for the refill accounting.
    pub fn try_acquire(&mut self, now_secs: f64) -> bool {
        self.refill(now_secs);
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
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
}
