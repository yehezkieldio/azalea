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
use crate::config::{EngineSettings, HardwareAcceleration, TranscodeSettings, USER_AGENT};
use crate::media::TempFileCleanup;
use crate::pipeline::ResolverChain;
use crate::storage::{DedupCache, Metrics};
use std::sync::{
    Arc,
    atomic::{AtomicU8, AtomicU64, Ordering},
};
use std::time::Duration;

#[derive(Debug)]
struct TranscodeRuntimeInner {
    configured_backend: HardwareAcceleration,
    active_backend: AtomicU8,
    fallback_transitions: AtomicU64,
}

/// Runtime view of the currently active encoder backend.
///
/// ## Rationale
/// Startup probes can pass while the first real encode still fails on a host,
/// so the active backend must be swappable without mutating static config.
#[derive(Clone, Debug)]
pub struct TranscodeRuntime {
    inner: Arc<TranscodeRuntimeInner>,
}

impl TranscodeRuntime {
    pub fn new(configured_backend: HardwareAcceleration) -> Self {
        Self {
            inner: Arc::new(TranscodeRuntimeInner {
                configured_backend,
                active_backend: AtomicU8::new(configured_backend.as_repr()),
                fallback_transitions: AtomicU64::new(0),
            }),
        }
    }

    pub fn configured_backend(&self) -> HardwareAcceleration {
        self.inner.configured_backend
    }

    pub fn active_backend(&self) -> HardwareAcceleration {
        HardwareAcceleration::from_repr(self.inner.active_backend.load(Ordering::Relaxed))
            .unwrap_or(HardwareAcceleration::None)
    }

    pub fn software_fallback_active(&self) -> bool {
        self.configured_backend().is_hardware()
            && self.active_backend() == HardwareAcceleration::None
    }

    pub fn fallback_transitions(&self) -> u64 {
        self.inner.fallback_transitions.load(Ordering::Relaxed)
    }

    pub fn effective_settings(&self, template: &TranscodeSettings) -> TranscodeSettings {
        let mut settings = template.clone();
        settings.hardware_acceleration = self.active_backend();
        settings
    }

    pub fn activate_software_fallback(&self) -> bool {
        if !self.configured_backend().is_hardware() {
            return false;
        }

        let previous = self
            .inner
            .active_backend
            .swap(HardwareAcceleration::None.as_repr(), Ordering::Relaxed);
        if previous == HardwareAcceleration::None.as_repr() {
            return false;
        }

        self.inner
            .fallback_transitions
            .fetch_add(1, Ordering::Relaxed);
        true
    }
}

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
    pub transcode_runtime: TranscodeRuntime,
    pub http: reqwest::Client,
    pub permits: Permits,
    pub reserved_download_bytes: Arc<AtomicU64>,
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
        let transcode_runtime = TranscodeRuntime::new(config.transcode.hardware_acceleration);
        let permits = Permits::new(&config.concurrency);
        let reserved_download_bytes = Arc::new(AtomicU64::new(0));
        let dedup = DedupCache::new(&config.storage, &config.pipeline, &config.transcode)?;
        let metrics = Metrics::new(&config.storage)?;
        let resolver = ResolverChain::new(&config.storage, &config.pipeline, &config.binaries);
        let temp_files = TempFileCleanup::new();

        Ok(Self {
            config,
            transcode_runtime,
            http,
            permits,
            reserved_download_bytes,
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
    let mut builder = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(config.http.timeout_secs))
        .pool_max_idle_per_host(config.http.pool_max_idle_per_host)
        .pool_idle_timeout(Duration::from_secs(config.http.pool_idle_timeout_secs))
        .connect_timeout(Duration::from_secs(config.http.connect_timeout_secs))
        .tcp_keepalive(Duration::from_secs(30))
        .tcp_nodelay(true)
        .http2_max_frame_size(config.http.http2_max_frame_size_bytes)
        .deflate(true)
        .gzip(true)
        .brotli(true)
        .user_agent(USER_AGENT);

    builder = if config.http.http2_adaptive_window {
        builder.http2_adaptive_window(true)
    } else {
        builder
            .http2_initial_stream_window_size(config.http.http2_initial_stream_window_size_bytes)
            .http2_initial_connection_window_size(
                config.http.http2_initial_connection_window_size_bytes,
            )
    };

    let http = builder.build()?;

    Ok(http)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hardware_fallback_latches_once() {
        let runtime = TranscodeRuntime::new(HardwareAcceleration::Vaapi);

        assert_eq!(runtime.configured_backend(), HardwareAcceleration::Vaapi);
        assert_eq!(runtime.active_backend(), HardwareAcceleration::Vaapi);
        assert!(!runtime.software_fallback_active());

        assert!(runtime.activate_software_fallback());
        assert_eq!(runtime.active_backend(), HardwareAcceleration::None);
        assert!(runtime.software_fallback_active());
        assert_eq!(runtime.fallback_transitions(), 1);

        assert!(!runtime.activate_software_fallback());
        assert_eq!(runtime.fallback_transitions(), 1);
    }

    #[test]
    fn effective_settings_follow_runtime_backend() {
        let runtime = TranscodeRuntime::new(HardwareAcceleration::Nvenc);
        let settings = TranscodeSettings {
            hardware_acceleration: HardwareAcceleration::Nvenc,
            ..TranscodeSettings::default()
        };

        runtime.activate_software_fallback();
        let effective = runtime.effective_settings(&settings);

        assert_eq!(effective.hardware_acceleration, HardwareAcceleration::None);
    }
}
