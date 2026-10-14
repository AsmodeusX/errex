//! Per-project token-bucket rate limiter.
//!
//! Memory: a single `Mutex<HashMap<String, TokenBucket>>` keyed by project
//! name. Self-host pequeno typically has a handful of projects so the map
//! stays tiny. The map grows only on first ingest from a new project; we
//! don't pre-populate.
//!
//! Time: callers pass `Instant::now()` rather than capturing it, so unit
//! tests can drive the limiter through synthetic timelines without sleeping.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Instant;

#[derive(Debug, Clone, Copy)]
pub struct TokenBucket {
    tokens: f64,
    last_refill: Instant,
}

impl TokenBucket {
    pub fn new(capacity: f64, now: Instant) -> Self {
        Self {
            tokens: capacity,
            last_refill: now,
        }
    }

    /// Returns true and decrements one token if available, refilling first
    /// based on elapsed time. False (no decrement) when the bucket is dry.
    pub fn try_acquire(&mut self, capacity: f64, refill_per_sec: f64, now: Instant) -> bool {
        let elapsed = now
            .saturating_duration_since(self.last_refill)
            .as_secs_f64();
        if elapsed > 0.0 {
            self.tokens = (self.tokens + elapsed * refill_per_sec).min(capacity);
            self.last_refill = now;
        }
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }

    /// Whole seconds until the next token, at least 1. Call after a refused
    /// `try_acquire`, which has just refilled the bucket.
    pub fn retry_after_secs(&self, refill_per_sec: f64) -> u64 {
        ((1.0 - self.tokens) / refill_per_sec).ceil().max(1.0) as u64
    }
}

#[derive(Debug)]
pub struct RateLimiter {
    buckets: Mutex<HashMap<String, TokenBucket>>,
    capacity: f64,
    refill_per_sec: f64,
}

impl RateLimiter {
    /// `per_min` is the steady-state ingest budget per project; `burst` is
    /// the max tokens the bucket can hold at once. `per_min == 0` disables
    /// rate limiting entirely (callers should branch and skip the check).
    pub fn new(per_min: u32, burst: u32) -> Self {
        Self {
            buckets: Mutex::new(HashMap::new()),
            capacity: f64::from(burst.max(1)),
            refill_per_sec: f64::from(per_min) / 60.0,
        }
    }

    pub fn enabled(&self) -> bool {
        self.refill_per_sec > 0.0
    }

    /// `Err(seconds)` when the project is over its rate: how long until it
    /// may send again, for `Retry-After` and `X-Sentry-Rate-Limits`.
    pub fn check(&self, project: &str, now: Instant) -> Result<(), u64> {
        if !self.enabled() {
            return Ok(());
        }
        let mut guard = self.buckets.lock().expect("rate limiter mutex poisoned");
        let bucket = guard
            .entry(project.to_string())
            .or_insert_with(|| TokenBucket::new(self.capacity, now));
        if bucket.try_acquire(self.capacity, self.refill_per_sec, now) {
            Ok(())
        } else {
            Err(bucket.retry_after_secs(self.refill_per_sec))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn t0() -> Instant {
        Instant::now()
    }

    // ----- TokenBucket -----

    #[test]
    fn fresh_bucket_grants_capacity_then_denies() {
        let mut b = TokenBucket::new(3.0, t0());
        let now = t0();
        assert!(b.try_acquire(3.0, 1.0, now));
        assert!(b.try_acquire(3.0, 1.0, now));
        assert!(b.try_acquire(3.0, 1.0, now));
        assert!(!b.try_acquire(3.0, 1.0, now), "4th must deny");
    }

    #[test]
    fn refill_restores_tokens_at_rate() {
        let start = t0();
        let mut b = TokenBucket::new(2.0, start);
        // exhaust
        assert!(b.try_acquire(2.0, 2.0, start));
        assert!(b.try_acquire(2.0, 2.0, start));
        assert!(!b.try_acquire(2.0, 2.0, start));
        // 1s later at 2 tokens/sec → 2 fresh tokens
        let later = start + Duration::from_secs(1);
        assert!(b.try_acquire(2.0, 2.0, later));
        assert!(b.try_acquire(2.0, 2.0, later));
        assert!(!b.try_acquire(2.0, 2.0, later));
    }

    #[test]
    fn refill_does_not_exceed_capacity() {
        // Bucket starts full; a long idle period shouldn't accrue tokens
        // beyond capacity (otherwise burst rules don't hold).
        let start = t0();
        let mut b = TokenBucket::new(3.0, start);
        let later = start + Duration::from_secs(60);
        // 60s × 1 tok/s = 60 hypothetical, but cap is 3.
        assert!(b.try_acquire(3.0, 1.0, later));
        assert!(b.try_acquire(3.0, 1.0, later));
        assert!(b.try_acquire(3.0, 1.0, later));
        assert!(!b.try_acquire(3.0, 1.0, later), "must cap at burst");
    }

    // ----- RateLimiter -----

    #[test]
    fn limiter_disabled_when_per_min_is_zero() {
        let l = RateLimiter::new(0, 100);
        assert!(!l.enabled());
        for _ in 0..10_000 {
            assert!(
                l.check("p", t0()).is_ok(),
                "disabled limiter must always pass"
            );
        }
    }

    #[test]
    fn limiter_enforces_burst_then_denies() {
        let l = RateLimiter::new(60, 5); // 1 tok/sec, burst 5
        let now = t0();
        for _ in 0..5 {
            assert!(l.check("p", now).is_ok());
        }
        assert!(
            l.check("p", now).is_err(),
            "6th request inside same instant must deny"
        );
    }

    #[test]
    fn limiter_isolates_projects() {
        let l = RateLimiter::new(60, 1); // 1 token capacity, then deny
        let now = t0();
        assert!(l.check("alpha", now).is_ok());
        assert!(l.check("alpha", now).is_err());
        // Beta is independent.
        assert!(l.check("beta", now).is_ok());
        assert!(l.check("beta", now).is_err());
    }

    #[test]
    fn denial_says_how_long_until_the_next_token() {
        let start = t0();
        let fast = RateLimiter::new(60, 1); // 1/sec
        assert_eq!(fast.check("p", start), Ok(()));
        assert_eq!(fast.check("p", start), Err(1));
        let slow = RateLimiter::new(6, 1); // one per 10 s
        assert_eq!(slow.check("p", start), Ok(()));
        assert_eq!(slow.check("p", start), Err(10));
        assert_eq!(slow.check("p", start + Duration::from_secs(4)), Err(6));
    }

    #[test]
    fn limiter_recovers_with_time() {
        let l = RateLimiter::new(60, 1); // 1/sec
        let start = t0();
        assert!(l.check("p", start).is_ok());
        assert!(l.check("p", start).is_err());
        let later = start + Duration::from_secs(2);
        assert!(
            l.check("p", later).is_ok(),
            "after 2s must have a fresh token"
        );
    }
}
