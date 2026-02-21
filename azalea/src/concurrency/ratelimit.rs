//! Per-user rate limiting for message-triggered workloads.
//!
//! ## Algorithm overview
//! Maintains a per-user counter in a TTL cache, approximating a fixed window.
//!
//! ## Trade-off acknowledgment
//! This is a simple, approximate limiter; it prioritizes low overhead over
//! perfect precision (see `moka` TTL semantics).

use moka::future::Cache;
use std::sync::{
    Arc,
    atomic::{AtomicU32, Ordering},
};
use std::time::Duration;
use twilight_model::id::Id;

/// Per-user rate limiter with sliding window.
///
/// ## Invariants
/// - Counters are stored per user id and expire after the TTL window.
/// - `max_requests == 0` disables limiting (see [`RateLimiter::check`]).
#[derive(Debug, Clone)]
pub struct RateLimiter {
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
    pub async fn check<Marker>(&self, id: Id<Marker>) -> bool {
        if self.max_requests == 0 {
            // Explicitly disabled: always allow.
            return true;
        }

        let user_key = id.get();
        let counter = self
            .cache
            .get_with(user_key, async { Arc::new(AtomicU32::new(0)) })
            .await;
        // Relaxed ordering is sufficient for a per-user approximate count.
        let current = counter.fetch_add(1, Ordering::Relaxed);
        current < self.max_requests
    }
}
