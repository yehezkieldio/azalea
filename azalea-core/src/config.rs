//! Central configuration schema for the core media engine.
//!
//! ## Preconditions & postconditions
//! - [`EngineSettings::validate`] must succeed before starting the engine.
//! - After validation, runtime checks can assume limits (timeouts, upload caps) are sane.
//!
//! ## Invariants
//! - Timeouts are non-zero and bounded (see `validate_timeout`).
//! - Upload size caps never exceed Discord's hard limits.
//!
//! ## Design rationale & trade-offs
//! - Keeps validation centralized to avoid duplicate guardrails in each stage.
//! - Prioritizes explicit limits over auto-tuning to make operational behavior predictable.
//!
//! ## Rejected alternatives
//! - Per-stage validation: rejected to avoid redundant checks and drift.
//! - Auto-scaling limits: rejected to keep operational behavior deterministic.
//!
//! ## Non-goals
//! - This module does **not** load environment variables directly; callers should do that.
//! - It avoids runtime mutation; settings are treated as read-only after startup.

use serde::Deserialize;
use std::{path::PathBuf, time::Duration};

/// User agent string for outgoing HTTP requests.
pub const USER_AGENT: &str = "Azalea (https://github.com/yehezkieldio/azalea)";

/// Quality preset for video processing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "lowercase")]
pub enum QualityPreset {
    /// Prioritize speed over quality (lower resolution, higher compression).
    Fast,
    /// Default behavior with moderate quality and processing time.
    #[default]
    Balanced,
    /// Prioritize visual fidelity over processing time.
    Quality,
    /// Aggressive compression to minimize file size.
    Size,
}

impl QualityPreset {
    /// CRF target for the chosen quality/speed tradeoff.
    ///
    /// Lower values increase quality at the cost of larger output and CPU time.
    pub fn crf(self) -> u8 {
        match self {
            Self::Fast => 28,
            Self::Balanced => 23,
            Self::Quality => 18,
            Self::Size => 32,
        }
    }

    /// Map the preset onto ffmpeg's encoder speed presets.
    pub fn ffmpeg_preset(self) -> &'static str {
        match self {
            Self::Fast => "ultrafast",
            Self::Balanced => "superfast",
            Self::Quality => "fast",
            Self::Size => "veryfast",
        }
    }
}

/// Hardware acceleration provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "lowercase")]
pub enum HardwareAcceleration {
    /// Software encoding (CPU).
    #[default]
    None,
    /// NVIDIA NVENC (NVIDIA GPUs).
    Nvenc,
    /// VA-API (Intel/AMD on Linux).
    Vaapi,
    /// VideoToolbox (Apple Silicon/macOS).
    VideoToolbox,
}

impl HardwareAcceleration {
    /// Encoder name passed to ffmpeg for the selected acceleration backend.
    pub fn encoder(self) -> &'static str {
        match self {
            Self::None => "libx264",
            Self::Nvenc => "h264_nvenc",
            Self::Vaapi => "h264_vaapi",
            Self::VideoToolbox => "h264_videotoolbox",
        }
    }
}

/// Concurrency configuration.
#[derive(Debug, Clone, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(default)]
pub struct ConcurrencySettings {
    pub download: u32,
    pub upload: u32,
    pub transcode: u32,
    pub ytdlp: u32,
    pub pipeline: u32,
}

impl Default for ConcurrencySettings {
    fn default() -> Self {
        Self {
            download: 4,
            upload: 2,
            transcode: 1,
            ytdlp: 1,
            pipeline: 8,
        }
    }
}

/// HTTP client configuration.
#[derive(Debug, Clone, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(default)]
pub struct HttpSettings {
    pub pool_max_idle_per_host: usize,
    pub pool_idle_timeout_secs: u64,
    pub connect_timeout_secs: u64,
    pub timeout_secs: u64,
}

impl Default for HttpSettings {
    fn default() -> Self {
        Self {
            pool_max_idle_per_host: 4,
            pool_idle_timeout_secs: 90,
            connect_timeout_secs: 10,
            timeout_secs: 60,
        }
    }
}

/// Storage configuration.
#[derive(Debug, Clone, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(default)]
pub struct StorageSettings {
    pub temp_dir: PathBuf,
    pub dedup_ttl_hours: u64,
    pub dedup_cache_size: usize,
    pub dedup_persistent: bool,
    pub dedup_db_path: PathBuf,
    pub dedup_flush_interval_secs: u64,
    pub metrics_db_path: PathBuf,
    pub metrics_enabled: bool,
    pub resolver_cache_ttl_secs: u64,
    pub resolver_cache_size: usize,
    pub resolver_negative_ttl_secs: u64,
}

impl Default for StorageSettings {
    fn default() -> Self {
        Self {
            temp_dir: PathBuf::from("/tmp/azalea"),
            dedup_ttl_hours: 24,
            dedup_cache_size: 1000,
            dedup_persistent: true,
            dedup_db_path: PathBuf::from("azalea-dedup.redb"),
            dedup_flush_interval_secs: 60,
            metrics_db_path: PathBuf::from("azalea-metrics.redb"),
            metrics_enabled: true,
            resolver_cache_ttl_secs: 300,
            resolver_cache_size: 1000,
            resolver_negative_ttl_secs: 60,
        }
    }
}

/// Transcoding configuration.
#[derive(Debug, Clone, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(default)]
pub struct TranscodeSettings {
    pub quality_preset: QualityPreset,
    pub hardware_acceleration: HardwareAcceleration,
    pub ffmpeg_threads: u32,
    pub vaapi_device: Box<str>,
    pub max_upload_bytes: u64,
    pub container_overhead_ratio: f64,
    pub vbr_safety_margin: f64,
    pub transcode_target_ratio: f64,
    pub split_target_ratio: f64,
    pub audio_vbr_padding: f64,
    pub min_bitrate_kbps: u32,
    pub max_single_video_duration_secs: u64,
    pub ffprobe_timeout_secs: u64,
    pub ffmpeg_timeout_secs: u64,
}

impl Default for TranscodeSettings {
    fn default() -> Self {
        Self {
            quality_preset: QualityPreset::Fast,
            hardware_acceleration: HardwareAcceleration::None,
            ffmpeg_threads: 0,
            vaapi_device: "/dev/dri/renderD128".into(),
            max_upload_bytes: 8 * 1024 * 1024,
            container_overhead_ratio: 0.03,
            vbr_safety_margin: 0.05,
            transcode_target_ratio: 0.85,
            split_target_ratio: 0.80,
            audio_vbr_padding: 1.1,
            min_bitrate_kbps: 400,
            max_single_video_duration_secs: 120,
            ffprobe_timeout_secs: 30,
            ffmpeg_timeout_secs: 600,
        }
    }
}

impl TranscodeSettings {
    /// Compute a thread budget that avoids over-subscribing CPU when multiple
    /// transcodes run concurrently.
    ///
    /// ## Rationale
    /// Balances throughput with CPU contention by scaling per-task threads to
    /// the configured transcode concurrency.
    ///
    /// ## Concurrency assumptions
    /// Callers should ensure `transcode_concurrency >= 1` (validated upstream).
    ///
    /// ## Trade-off acknowledgment
    /// This favors predictable latency over squeezing maximum single-task speed.
    pub fn effective_ffmpeg_threads(&self, transcode_concurrency: u32) -> u32 {
        if self.ffmpeg_threads > 0 {
            return self.ffmpeg_threads;
        }

        let cpus = std::thread::available_parallelism()
            .map(|value| value.get())
            .unwrap_or(4) as u32;

        if cpus <= 4 {
            return 2;
        }

        (cpus / transcode_concurrency.max(1)).max(2)
    }
}

/// Pipeline configuration.
#[derive(Debug, Clone, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(default)]
pub struct PipelineSettings {
    pub queue_backpressure_timeout_ms: u64,
    pub download_timeout_secs: u64,
    pub upload_timeout_secs: u64,
    pub min_disk_space_bytes: u64,
    pub max_download_bytes: u64,
    pub resolver_timeout_secs: u64,
    pub ytdlp_timeout_secs: u64,
    pub user_rate_limit_requests: u32,
    pub user_rate_limit_window_secs: u64,
    pub channel_rate_limit_requests: u32,
    pub channel_rate_limit_window_secs: u64,
    pub parallel_segment_threshold: u32,
}

impl Default for PipelineSettings {
    fn default() -> Self {
        Self {
            queue_backpressure_timeout_ms: 1_000,
            download_timeout_secs: 60,
            upload_timeout_secs: 120,
            min_disk_space_bytes: 500 * 1024 * 1024,
            max_download_bytes: 500 * 1024 * 1024,
            resolver_timeout_secs: 10,
            ytdlp_timeout_secs: 30,
            user_rate_limit_requests: 10,
            user_rate_limit_window_secs: 60,
            channel_rate_limit_requests: 20,
            channel_rate_limit_window_secs: 60,
            parallel_segment_threshold: 4,
        }
    }
}

/// Top-level engine configuration.
#[derive(Debug, Clone, Default, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(default)]
pub struct EngineSettings {
    pub concurrency: ConcurrencySettings,
    pub http: HttpSettings,
    pub storage: StorageSettings,
    pub transcode: TranscodeSettings,
    pub pipeline: PipelineSettings,
    pub binaries: BinarySettings,
}

impl EngineSettings {
    /// Validate configuration values and enforce hard safety limits.
    ///
    /// ## Preconditions
    /// - Values are already deserialized into structured types.
    ///
    /// ## Postconditions
    /// - All numeric limits are within safe, documented ranges.
    /// - Timeouts are non-zero and bounded by a sane upper limit.
    ///
    /// ## Business rules
    /// - Discord upload size caps are enforced here to prevent runtime surprises.
    ///
    /// ## Non-obvious behavior
    /// - Some bounds are clamped via `bail!` rather than silently adjusted.
    pub fn validate(&self) -> anyhow::Result<()> {
        let concurrency = &self.concurrency;
        if concurrency.pipeline == 0
            || concurrency.download == 0
            || concurrency.upload == 0
            || concurrency.transcode == 0
            || concurrency.ytdlp == 0
        {
            anyhow::bail!("all concurrency values must be at least 1");
        }

        if self.transcode.max_upload_bytes == 0 {
            anyhow::bail!("transcode.max_upload_bytes must be greater than 0");
        }

        if self.transcode.max_upload_bytes > 50 * 1024 * 1024 {
            anyhow::bail!("transcode.max_upload_bytes exceeds Discord's 50 MiB limit");
        }

        if self.transcode.min_bitrate_kbps == 0 || self.transcode.min_bitrate_kbps > 100_000 {
            anyhow::bail!("transcode.min_bitrate_kbps must be between 1 and 100000");
        }

        let validate_ratio = |name: &str, value: f64| -> anyhow::Result<()> {
            if !value.is_finite() || value <= 0.0 || value > 1.0 {
                anyhow::bail!("{} must be finite and in (0, 1]", name);
            }
            Ok(())
        };

        let validate_positive_finite = |name: &str, value: f64| -> anyhow::Result<()> {
            if !value.is_finite() || value <= 0.0 {
                anyhow::bail!("{} must be finite and greater than 0", name);
            }
            Ok(())
        };

        validate_ratio(
            "transcode.container_overhead_ratio",
            self.transcode.container_overhead_ratio,
        )?;
        validate_ratio(
            "transcode.vbr_safety_margin",
            self.transcode.vbr_safety_margin,
        )?;
        validate_ratio(
            "transcode.transcode_target_ratio",
            self.transcode.transcode_target_ratio,
        )?;
        validate_ratio(
            "transcode.split_target_ratio",
            self.transcode.split_target_ratio,
        )?;
        validate_positive_finite(
            "transcode.audio_vbr_padding",
            self.transcode.audio_vbr_padding,
        )?;

        let max_timeout_secs = 3600;
        let validate_timeout = |name: &str, value: u64| -> anyhow::Result<()> {
            if value == 0 || value > max_timeout_secs {
                anyhow::bail!(
                    "{} must be between 1 and {} seconds",
                    name,
                    max_timeout_secs
                );
            }
            Ok(())
        };

        validate_timeout("http.connect_timeout_secs", self.http.connect_timeout_secs)?;
        validate_timeout("http.timeout_secs", self.http.timeout_secs)?;
        validate_timeout(
            "pipeline.download_timeout_secs",
            self.pipeline.download_timeout_secs,
        )?;
        validate_timeout(
            "pipeline.upload_timeout_secs",
            self.pipeline.upload_timeout_secs,
        )?;
        validate_timeout(
            "pipeline.resolver_timeout_secs",
            self.pipeline.resolver_timeout_secs,
        )?;
        validate_timeout(
            "pipeline.ytdlp_timeout_secs",
            self.pipeline.ytdlp_timeout_secs,
        )?;
        validate_timeout(
            "transcode.ffprobe_timeout_secs",
            self.transcode.ffprobe_timeout_secs,
        )?;
        validate_timeout(
            "transcode.ffmpeg_timeout_secs",
            self.transcode.ffmpeg_timeout_secs,
        )?;

        Ok(())
    }

    /// Wrapper for the upload timeout used by HTTP requests.
    pub fn upload_timeout(&self) -> Duration {
        Duration::from_secs(self.pipeline.upload_timeout_secs)
    }
}

/// External binary paths used by the pipeline.
#[derive(Debug, Clone, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(default)]
pub struct BinarySettings {
    pub ffmpeg: PathBuf,
    pub ffprobe: PathBuf,
    pub ytdlp: PathBuf,
}

impl Default for BinarySettings {
    fn default() -> Self {
        Self {
            ffmpeg: PathBuf::from("ffmpeg"),
            ffprobe: PathBuf::from("ffprobe"),
            ytdlp: PathBuf::from("yt-dlp"),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]
    use super::*;

    #[test]
    fn test_default_config_validates() {
        let config = EngineSettings::default();
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_quality_presets() {
        assert_eq!(QualityPreset::Fast.crf(), 28);
        assert_eq!(QualityPreset::Balanced.crf(), 23);
        assert_eq!(QualityPreset::Quality.crf(), 18);
        assert_eq!(QualityPreset::Size.crf(), 32);

        assert_eq!(QualityPreset::Fast.ffmpeg_preset(), "ultrafast");
        assert_eq!(QualityPreset::Balanced.ffmpeg_preset(), "superfast");
        assert_eq!(QualityPreset::Quality.ffmpeg_preset(), "fast");
        assert_eq!(QualityPreset::Size.ffmpeg_preset(), "veryfast");
    }

    #[test]
    fn test_ffmpeg_threads() {
        let cfg = TranscodeSettings::default();
        let threads = cfg.effective_ffmpeg_threads(2);
        assert!(threads >= 2);
    }

    #[test]
    fn validate_rejects_zero_concurrency() {
        let mut config = EngineSettings::default();
        config.concurrency.pipeline = 0;
        let err = config
            .validate()
            .expect_err("zero concurrency must be invalid");
        assert!(err.to_string().contains("concurrency"));
    }

    #[test]
    fn validate_rejects_invalid_transcode_limits() {
        let mut config = EngineSettings::default();
        config.transcode.max_upload_bytes = 51 * 1024 * 1024;
        let err = config
            .validate()
            .expect_err("upload size above discord cap must fail");
        assert!(err.to_string().contains("50 MiB limit"));

        let mut config = EngineSettings::default();
        config.transcode.container_overhead_ratio = 0.0;
        let err = config
            .validate()
            .expect_err("ratio outside (0, 1] must fail");
        assert!(err.to_string().contains("container_overhead_ratio"));
    }

    #[test]
    fn validate_rejects_out_of_range_timeouts() {
        let mut config = EngineSettings::default();
        config.http.connect_timeout_secs = 0;
        let err = config.validate().expect_err("zero timeout must fail");
        assert!(err.to_string().contains("connect_timeout_secs"));

        let mut config = EngineSettings::default();
        config.pipeline.upload_timeout_secs = 3601;
        let err = config.validate().expect_err("timeout above max must fail");
        assert!(err.to_string().contains("upload_timeout_secs"));
    }

    #[test]
    fn upload_timeout_uses_pipeline_setting() {
        let mut config = EngineSettings::default();
        config.pipeline.upload_timeout_secs = 321;
        assert_eq!(config.upload_timeout(), Duration::from_secs(321));
    }
}
