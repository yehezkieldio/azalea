//! # Module overview
//! Application state and initialization.
//!
//! ## Data flow
//! [`App::new`] wires the Discord client, rate limiter, and
//! [`azalea_core::Engine`] used by the pipeline.
//!
//! ## Concurrency assumptions
//! `App` is cloneable and shared across gateway and pipeline tasks.
//!
//! ## Explicit non-goals
//! This module does not implement Discord request handling; see `discord`.

use crate::concurrency::{ChannelRateLimiter, UserRateLimiter};
use crate::config::AppConfig;
use azalea_core::Engine;
use std::sync::{
    Arc,
    atomic::{AtomicU64, AtomicUsize, Ordering},
};
use twilight_http::Client as DiscordClient;

/// Shared application state wired once at startup and cheaply cloned for tasks.
///
/// ## Invariants
/// - `request_counter` is monotonic and unique across tasks.
/// - `engine` is fully initialized and validated.
#[derive(Clone)]
pub struct App {
    pub discord: Arc<DiscordClient>,
    pub config: AppConfig,
    pub engine: Engine,
    pub user_rate_limiter: UserRateLimiter,
    pub channel_rate_limiter: ChannelRateLimiter,
    pub queue_depth: Arc<AtomicUsize>,
    // Monotonic-ish ids for log correlation; uniqueness matters, not ordering.
    request_counter: Arc<AtomicU64>,
    pub start_time: std::time::Instant,
}

impl App {
    /// Build the full application state, initializing storage-backed subsystems.
    ///
    /// ## Preconditions
    /// - `config` has already been validated via [`AppConfig::validate`].
    ///
    /// ## Postconditions
    /// - `engine` subsystems are ready for pipeline execution.
    pub fn new(config: AppConfig, discord: DiscordClient) -> anyhow::Result<Self> {
        let engine = Engine::new(config.engine.clone())?;
        let user_rate_limiter = UserRateLimiter::new(
            config.engine.pipeline.user_rate_limit_requests,
            config.engine.pipeline.user_rate_limit_window_secs,
        );
        let channel_rate_limiter = ChannelRateLimiter::new(
            config.engine.pipeline.channel_rate_limit_requests,
            config.engine.pipeline.channel_rate_limit_window_secs,
        );

        Ok(Self {
            discord: Arc::new(discord),
            config,
            engine,
            user_rate_limiter,
            channel_rate_limiter,
            queue_depth: Arc::new(AtomicUsize::new(0)),
            request_counter: Arc::new(AtomicU64::new(1)),
            start_time: std::time::Instant::now(),
        })
    }

    /// Allocate a new request id for tracing and user-visible logging.
    ///
    /// ## Non-obvious behavior
    /// The counter wraps on `u64::MAX`, but uniqueness within a runtime is
    /// sufficient for log correlation.
    pub fn next_request_id(&self) -> u64 {
        // Relaxed is sufficient: we only require uniqueness across threads.
        // NOTE: this counter wraps on u64::MAX.
        self.request_counter.fetch_add(1, Ordering::Relaxed)
    }
}
