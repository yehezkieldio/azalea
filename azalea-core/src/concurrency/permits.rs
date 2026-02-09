//! Permit configuration used by pipeline stages.
//!
//! ## Preconditions
//! - [`ConcurrencySettings`] should already be validated (see
//!   [`crate::config::EngineSettings::validate`]).
//!
//! ## Postconditions
//! - All semaphores are created with a non-zero initial permit count.
//!
//! ## Usage footguns
//! - Holding a permit across a long-running CPU task can starve other stages.

use crate::config::ConcurrencySettings;
use std::sync::Arc;
use tokio::sync::Semaphore;

/// All concurrency permits in one place.
///
/// ## Invariants
/// Each semaphore contains at least one permit, even if config requested 0.
///
/// ## Concurrency assumptions
/// Clones share the same underlying semaphore to coordinate across tasks.
#[derive(Clone, Debug)]
pub struct Permits {
    pub download: Arc<Semaphore>,
    pub upload: Arc<Semaphore>,
    pub transcode: Arc<Semaphore>,
    pub ytdlp: Arc<Semaphore>,
    pub pipeline: Arc<Semaphore>,
}

impl Permits {
    /// Build semaphores from configuration, clamping to at least one permit.
    ///
    /// ## Rationale
    /// Clamping avoids a total deadlock when a configuration value is 0.
    pub fn new(config: &ConcurrencySettings) -> Self {
        Self {
            download: Arc::new(Semaphore::new(config.download.max(1) as usize)),
            upload: Arc::new(Semaphore::new(config.upload.max(1) as usize)),
            transcode: Arc::new(Semaphore::new(config.transcode.max(1) as usize)),
            ytdlp: Arc::new(Semaphore::new(config.ytdlp.max(1) as usize)),
            pipeline: Arc::new(Semaphore::new(config.pipeline.max(1) as usize)),
        }
    }
}
