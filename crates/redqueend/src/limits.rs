//! Per-client rate limiting for requests that change hardware.
//!
//! The embedded controller shouldn't be flooded by a buggy or hostile
//! client. Each client (bus connection) gets a token bucket.

use std::collections::HashMap;
use std::time::Instant;

/// Requests allowed in a burst.
pub const BURST: f64 = 8.0;
/// Sustained requests per second.
pub const REFILL_PER_SEC: f64 = 1.0;
/// Most clients tracked at once; the least recently used are forgotten
/// first, which only ever gives them a fresh bucket.
const MAX_CLIENTS: usize = 256;

#[derive(Debug, Clone, Copy)]
struct Bucket {
    tokens: f64,
    last: Instant,
}

/// Token-bucket limiter keyed by client name.
#[derive(Debug, Default)]
pub struct RateLimiter {
    buckets: HashMap<String, Bucket>,
}

impl RateLimiter {
    /// An empty limiter.
    pub fn new() -> Self {
        Self::default()
    }

    /// Takes a token for `client`. `false` means "too many requests".
    pub fn allow(&mut self, client: &str, now: Instant) -> bool {
        if self.buckets.len() >= MAX_CLIENTS && !self.buckets.contains_key(client) {
            self.evict_oldest();
        }
        let b = self.buckets.entry(client.to_owned()).or_insert(Bucket {
            tokens: BURST,
            last: now,
        });
        let elapsed = now.saturating_duration_since(b.last);
        b.tokens = (b.tokens + elapsed.as_secs_f64() * REFILL_PER_SEC).min(BURST);
        b.last = now;
        if b.tokens >= 1.0 {
            b.tokens -= 1.0;
            true
        } else {
            false
        }
    }

    /// Forgets a client that disconnected.
    pub fn forget(&mut self, client: &str) {
        self.buckets.remove(client);
    }

    /// Number of tracked clients.
    pub fn len(&self) -> usize {
        self.buckets.len()
    }

    /// Whether no clients are tracked.
    pub fn is_empty(&self) -> bool {
        self.buckets.is_empty()
    }

    fn evict_oldest(&mut self) {
        if let Some(oldest) = self
            .buckets
            .iter()
            .min_by_key(|(_, b)| b.last)
            .map(|(k, _)| k.clone())
        {
            self.buckets.remove(&oldest);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn burst_then_throttle_then_refill() {
        let mut l = RateLimiter::new();
        let t0 = Instant::now();
        for i in 0..BURST as usize {
            assert!(l.allow(":1.1", t0), "request {i} within the burst");
        }
        assert!(!l.allow(":1.1", t0), "burst exhausted");
        assert!(
            !l.allow(":1.1", t0 + Duration::from_millis(500)),
            "half a token isn't enough"
        );
        assert!(
            l.allow(":1.1", t0 + Duration::from_millis(1100)),
            "one token refilled"
        );
        assert!(!l.allow(":1.1", t0 + Duration::from_millis(1200)));
    }

    #[test]
    fn clients_are_independent() {
        let mut l = RateLimiter::new();
        let t0 = Instant::now();
        for _ in 0..BURST as usize {
            assert!(l.allow(":1.1", t0));
        }
        assert!(!l.allow(":1.1", t0));
        assert!(l.allow(":1.2", t0), "another client is unaffected");
    }

    #[test]
    fn tokens_never_exceed_the_burst() {
        let mut l = RateLimiter::new();
        let t0 = Instant::now();
        assert!(l.allow(":1.1", t0));
        let later = t0 + Duration::from_secs(3600);
        for _ in 0..BURST as usize {
            assert!(l.allow(":1.1", later));
        }
        assert!(
            !l.allow(":1.1", later),
            "idle time doesn't bank more than a burst"
        );
    }

    #[test]
    fn tracked_clients_are_bounded_and_forgettable() {
        let mut l = RateLimiter::new();
        let t0 = Instant::now();
        for i in 0..(MAX_CLIENTS * 2) {
            l.allow(&format!(":1.{i}"), t0 + Duration::from_millis(i as u64));
        }
        assert!(l.len() <= MAX_CLIENTS);
        l.forget(":1.500");
        assert!(l.len() <= MAX_CLIENTS);
        assert!(!l.is_empty());
    }
}
