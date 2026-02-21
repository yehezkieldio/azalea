//! # Module overview
//! Runtime wiring for the media engine, tying together config, caches, HTTP, and
//! pipeline helpers.
//!
//! ## Architecture decision
//! The engine owns long-lived singletons (HTTP client, caches, permits) so
//! pipeline stages can remain stateless and data-oriented.
//!
//! ## Data flow
//! [`Engine::new`] consumes [`crate::config::EngineSettings`] and produces the
//! shared state used by [`crate::pipeline::run`].
//!
//! ## Trade-offs
//! Centralizing ownership simplifies lifetimes at the cost of a larger, shared
//! object; this is a deliberate choice to keep pipeline stages pure.

use crate::concurrency::Permits;
use crate::config::{EngineSettings, USER_AGENT};
use crate::media::TempFileCleanup;
use crate::pipeline::ResolverChain;
use crate::storage::{DedupCache, Metrics};
use std::time::Duration;

/// Shared engine state used by pipeline stages.
///
/// ## Invariants
/// - `http` is configured with bounded pools and timeouts.
/// - `dedup` and `metrics` either persist or gracefully degrade based on config.
///
/// ## Load-bearing comments
/// Engine is `Clone` because subsystems are internally reference-counted; all
/// pipeline workers share the same underlying state.
///
/// ## Concurrency assumptions
/// All contained handles are `Clone`-safe and expected to be shared across tasks.
#[derive(Clone)]
pub struct Engine {
    pub config: EngineSettings,
    pub http: reqwest::Client,
    pub permits: Permits,
    pub dedup: DedupCache,
    pub metrics: Metrics,
    pub resolver: ResolverChain,
    pub temp_files: TempFileCleanup,
}

impl Engine {
    /// Build the engine and its storage-backed subsystems.
    ///
    /// ## Preconditions
    /// Callers should validate settings via [`EngineSettings::validate`].
    ///
    /// ## Postconditions
    /// All subsystems are ready for concurrent pipeline execution.
    ///
    /// ## Constraint documentation
    /// If persistence paths are unavailable, dedup/metrics still operate in
    /// memory-only mode to preserve pipeline availability.
    pub fn new(config: EngineSettings) -> anyhow::Result<Self> {
        let http = build_http_client(&config)?;
        let permits = Permits::new(&config.concurrency);
        let dedup = DedupCache::new(&config.storage, &config.pipeline, &config.transcode)?;
        let metrics = Metrics::new(&config.storage)?;
        let resolver = ResolverChain::new(&config.storage, &config.pipeline, &config.binaries);
        let temp_files = TempFileCleanup::new();

        Ok(Self {
            config,
            http,
            permits,
            dedup,
            metrics,
            resolver,
            temp_files,
        })
    }
}

/// Build the shared HTTP client used by resolver and downloader stages.
///
/// ## Security-sensitive paths
/// Enforces timeouts and idle pool sizing to avoid resource exhaustion under
/// untrusted input loads.
fn build_http_client(config: &EngineSettings) -> anyhow::Result<reqwest::Client> {
    let http = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(config.http.timeout_secs))
        .pool_max_idle_per_host(config.http.pool_max_idle_per_host)
        .pool_idle_timeout(Duration::from_secs(config.http.pool_idle_timeout_secs))
        .connect_timeout(Duration::from_secs(config.http.connect_timeout_secs))
        .tcp_keepalive(Duration::from_secs(30))
        .tcp_nodelay(true)
        .http2_adaptive_window(true)
        .http2_initial_stream_window_size(2 * 1024 * 1024)
        .http2_initial_connection_window_size(4 * 1024 * 1024)
        .deflate(true)
        .gzip(true)
        .brotli(true)
        .user_agent(USER_AGENT)
        .build()?;

    Ok(http)
}
