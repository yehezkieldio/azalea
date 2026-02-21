//! # Module overview
//! Configuration management for the Azalea application.
//!
//! ## Data flow
//! `AppConfig::load` → [`AppConfig::validate`] → [`crate::App::new`].
//!
//! ## Design rationale
//! Validation lives here to keep the main entrypoint free of configuration
//! edge-case handling.
//!
//! ## References
//! - TOML spec: <https://toml.io/en/>

use azalea_core::config::EngineSettings;
use serde::Deserialize;
use std::time::Duration;
use twilight_model::id::{Id, marker::ApplicationMarker};

/// Default configuration file path.
const CONFIG_FILE: &str = "azalea.config.toml";

/// Application ID wrapper for type safety.
///
/// ## Invariants
/// `0` is treated as invalid and rejected during deserialization.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[cfg_attr(feature = "schemars", schemars(transparent))]
pub struct ApplicationId(pub u64);

impl ApplicationId {
    /// Wrap a raw Discord application id.
    pub fn new(raw: u64) -> Self {
        Self(raw)
    }

    /// Convert to the strongly-typed Twilight id used by gateway/http clients.
    pub fn get(self) -> Id<ApplicationMarker> {
        Id::new(self.0)
    }
}

impl<'de> Deserialize<'de> for ApplicationId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = u64::deserialize(deserializer)?;
        if raw == 0 {
            return Err(serde::de::Error::custom("application_id must be non-zero"));
        }
        Ok(Self::new(raw))
    }
}

/// Runtime configuration.
///
/// ## Constraints
/// Thread counts must be non-zero; stack size is clamped by validation.
#[derive(Debug, Clone, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(default)]
pub struct RuntimeConfig {
    pub worker_threads: usize,
    pub max_blocking_threads: usize,
    pub thread_stack_size: usize,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        let cpus = std::thread::available_parallelism()
            .map(|value| value.get())
            .unwrap_or(4);
        Self {
            worker_threads: cpus,
            max_blocking_threads: cpus * 4,
            thread_stack_size: 2 * 1024 * 1024,
        }
    }
}

/// Top-level application configuration.
///
/// ## Business rules
/// An application id is required, either via config file or environment.
#[derive(Debug, Clone, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(default)]
pub struct AppConfig {
    pub application_id: ApplicationId,
    pub runtime: RuntimeConfig,
    #[serde(flatten)]
    pub engine: EngineSettings,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            application_id: ApplicationId::new(0),
            runtime: RuntimeConfig::default(),
            engine: EngineSettings::default(),
        }
    }
}

impl AppConfig {
    /// Load configuration from the config file, falling back to defaults.
    ///
    /// ## Edge-case handling
    /// Missing config files fall back to defaults, but validation still applies.
    pub fn load() -> anyhow::Result<Self> {
        let mut config = match std::fs::read_to_string(CONFIG_FILE) {
            Ok(contents) => {
                let config: AppConfig = toml::from_str(&contents)?;
                tracing::info!(path = CONFIG_FILE, "Loaded configuration");
                config
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                tracing::info!(path = CONFIG_FILE, "Config file not found, using defaults");
                Self::default()
            }
            Err(err) => return Err(err.into()),
        };

        // Allow env override for application id when the config leaves it unset.
        if config.application_id.0 == 0
            && let Ok(id_str) = std::env::var("APPLICATION_ID")
            && let Ok(parsed) = id_str.parse::<u64>()
        {
            config.application_id = ApplicationId::new(parsed);
        }

        config.validate()?;
        Ok(config)
    }

    /// Validate configuration values and enforce application constraints.
    ///
    /// ## Preconditions
    /// - Settings are already deserialized into structured types.
    ///
    /// ## Postconditions
    /// - All runtime constraints and core engine limits are satisfied.
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.application_id.0 == 0 {
            anyhow::bail!("application_id must be set (via config or APPLICATION_ID env var)");
        }

        if self.runtime.worker_threads == 0 {
            anyhow::bail!("runtime.worker_threads must be at least 1");
        }

        if self.runtime.max_blocking_threads == 0 {
            anyhow::bail!("runtime.max_blocking_threads must be at least 1");
        }

        if self.runtime.thread_stack_size < 1024 * 1024 {
            anyhow::bail!("runtime.thread_stack_size must be at least 1 MiB");
        }

        self.engine.validate()?;
        Ok(())
    }

    /// Convenience wrapper for the upload timeout used by Discord HTTP requests.
    ///
    /// ## Rationale
    /// Keeps HTTP timeout configuration aligned with pipeline settings.
    pub fn upload_timeout(&self) -> Duration {
        self.engine.upload_timeout()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config_validates() {
        let config = AppConfig {
            application_id: ApplicationId::new(123456789),
            ..Default::default()
        };
        assert!(config.validate().is_ok());
    }
}
