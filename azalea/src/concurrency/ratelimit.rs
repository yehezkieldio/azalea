//! Per-user rate limiting for message-triggered workloads.
//!
//! ## Algorithm overview
//! Maintains a per-user counter in a TTL cache, approximating a fixed window.
//!
//! ## Trade-off acknowledgment
//! This is a simple, approximate limiter; it prioritizes low overhead over
//! perfect precision (see `moka` TTL semantics).

use crate::ids::{ChannelId, UserId};
use moka::future::Cache;
use std::sync::{
    Arc,
    atomic::{AtomicU32, Ordering},
};
use std::time::Duration;

/// Sliding-window limiter backed by a TTL cache keyed on raw Discord ids.
///
/// ## Invariants
/// - Counters are stored per Discord id and expire after the TTL window.
/// - `max_requests == 0` disables limiting (see [`RateLimiter::check_raw`]).
#[derive(Debug, Clone)]
struct RateLimiter {
    cache: Cache<u64, Arc<AtomicU32>>,
    max_requests: u32,
}

impl RateLimiter {
    /// Create a per-user limiter using a time-based cache for windowing.
    ///
    /// ## Preconditions
    /// - `window_secs` must be > 0 for meaningful throttling.
    pub fn new(max_requests: u32, window_secs: u64) -> Self {
        let cache = Cache::builder()
            .max_capacity(10_000)
            .time_to_live(Duration::from_secs(window_secs))
            .build();

        Self {
            cache,
            max_requests,
        }
    }

    /// Returns `true` when the user is within the request budget.
    ///
    /// ## Concurrency assumptions
    /// Atomic increments allow concurrent checks without locks.
    ///
    /// The counter is updated with relaxed ordering because we only need
    /// eventual convergence on a per-user count, not a total ordering.
    async fn check_raw(&self, id: u64) -> bool {
        if self.max_requests == 0 {
            // Explicitly disabled: always allow.
            return true;
        }

        let counter = self
            .cache
            .get_with(id, async { Arc::new(AtomicU32::new(0)) })
            .await;
        // Relaxed ordering is sufficient for a per-user approximate count.
        let current = counter.fetch_add(1, Ordering::Relaxed);
        current < self.max_requests
    }
}

#[derive(Debug, Clone)]
pub struct UserRateLimiter(RateLimiter);

impl UserRateLimiter {
    pub fn new(max_requests: u32, window_secs: u64) -> Self {
        Self(RateLimiter::new(max_requests, window_secs))
    }

    pub async fn check(&self, id: UserId) -> bool {
        self.0.check_raw(id.get()).await
    }
}

#[derive(Debug, Clone)]
pub struct ChannelRateLimiter(RateLimiter);

impl ChannelRateLimiter {
    pub fn new(max_requests: u32, window_secs: u64) -> Self {
        Self(RateLimiter::new(max_requests, window_secs))
    }

    pub async fn check(&self, id: ChannelId) -> bool {
        self.0.check_raw(id.get()).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn zero_limit_disables_rate_limiting() {
        let limiter = UserRateLimiter::new(0, 60);
        let user = UserId::new(1);
        for _ in 0..10 {
            assert!(limiter.check(user).await);
        }
    }

    #[tokio::test]
    async fn per_user_limit_is_enforced() {
        let limiter = UserRateLimiter::new(2, 60);
        let user = UserId::new(42);

        assert!(limiter.check(user).await);
        assert!(limiter.check(user).await);
        assert!(!limiter.check(user).await);
    }

    #[tokio::test]
    async fn users_are_isolated() {
        let limiter = UserRateLimiter::new(1, 60);
        let user_a = UserId::new(1);
        let user_b = UserId::new(2);

        assert!(limiter.check(user_a).await);
        assert!(limiter.check(user_b).await);
        assert!(!limiter.check(user_a).await);
        assert!(!limiter.check(user_b).await);
    }
}
