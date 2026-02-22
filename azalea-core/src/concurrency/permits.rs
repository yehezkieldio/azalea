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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_config_values_are_clamped_to_one() {
        let permits = Permits::new(&ConcurrencySettings {
            download: 0,
            upload: 0,
            transcode: 0,
            ytdlp: 0,
            pipeline: 0,
        });

        assert_eq!(permits.download.available_permits(), 1);
        assert_eq!(permits.upload.available_permits(), 1);
        assert_eq!(permits.transcode.available_permits(), 1);
        assert_eq!(permits.ytdlp.available_permits(), 1);
        assert_eq!(permits.pipeline.available_permits(), 1);
    }

    #[test]
    fn configured_values_are_preserved() {
        let permits = Permits::new(&ConcurrencySettings {
            download: 3,
            upload: 2,
            transcode: 4,
            ytdlp: 5,
            pipeline: 6,
        });

        assert_eq!(permits.download.available_permits(), 3);
        assert_eq!(permits.upload.available_permits(), 2);
        assert_eq!(permits.transcode.available_permits(), 4);
        assert_eq!(permits.ytdlp.available_permits(), 5);
        assert_eq!(permits.pipeline.available_permits(), 6);
    }
}
