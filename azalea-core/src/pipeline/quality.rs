//! # Module overview
//! Bitrate budgeting and quality ladder utilities.
//!
//! ## Algorithm overview
//! Computes audio/video budgets from upload size constraints and then suggests
//! a downscale ladder to fit within those budgets.
//!
//! ## Trade-off acknowledgment
//! The ladder is conservative; it favors successful uploads over maximal
//! resolution. See [`Ladder::recommend`].

use crate::config::{QualityPreset, TranscodeSettings};
use crate::pipeline::errors::{Error, TranscodeStage};

/// Computed bitrate targets for a transcode.
///
/// ## Postconditions
/// Returned bitrates are clamped to minimum viable values.
#[derive(Debug, Clone, Copy)]
pub struct BitrateParams {
    pub video_bitrate_kbps: u32,
    pub audio_bitrate_kbps: u32,
}

impl BitrateParams {
    /// Compute bitrate targets given duration and size constraints.
    ///
    /// ## Edge-case handling
    /// Extremely long media returns a `TranscodeFailed` error instead of a
    /// near-zero bitrate that would create unusable output.
    pub fn compute(config: &TranscodeSettings, duration: f64) -> Result<Self, Error> {
        validate_duration(duration, TranscodeStage::Transcode)?;
        // Budget total bits based on upload cap and target ratio.
        let target_total_bits =
            (config.max_upload_bytes as f64 * config.transcode_target_ratio) * 8.0;
        // Reserve space for container overhead.
        let usable_bits = target_total_bits * (1.0 - config.container_overhead_ratio);

        // Audio budget scales with duration to keep speech intelligible.
        let audio_bitrate_kbps = Self::audio_bitrate_for_duration(duration);
        let audio_total_bits =
            audio_bitrate_kbps as f64 * 1000.0 * duration * config.audio_vbr_padding;
        // Remaining bits are available for video.
        let usable_video_bits = usable_bits - audio_total_bits;

        if usable_video_bits <= 0.0 {
            return Err(Error::TranscodeFailed {
                stage: TranscodeStage::Transcode,
                exit_code: None,
                stderr_tail: "video too long to fit".to_string(),
            });
        }

        // Apply a safety margin for VBR variance.
        let video_bitrate_kbps =
            ((usable_video_bits / duration / 1000.0) * (1.0 - config.vbr_safety_margin)) as u32;

        Ok(Self {
            video_bitrate_kbps: video_bitrate_kbps.max(50),
            audio_bitrate_kbps,
        })
    }

    /// Compute a bitrate budget for split segments.
    ///
    /// ## Rationale
    /// Uses a fixed audio bitrate to keep segment sizing predictable.
    pub fn compute_for_split(
        config: &TranscodeSettings,
        segment_duration: f64,
    ) -> Result<Self, Error> {
        validate_duration(segment_duration, TranscodeStage::Split)?;
        let target_bits = (config.max_upload_bytes as f64 * config.split_target_ratio) * 8.0;
        // Fixed audio bitrate keeps segment sizing predictable.
        let audio_bitrate_kbps = 128u32;
        let audio_bits =
            audio_bitrate_kbps as f64 * 1000.0 * segment_duration * config.audio_vbr_padding;
        let usable_video_bits =
            (target_bits * (1.0 - config.container_overhead_ratio)) - audio_bits;
        if usable_video_bits <= 0.0 {
            return Err(Error::TranscodeFailed {
                stage: TranscodeStage::Split,
                exit_code: None,
                stderr_tail: "segment too long to fit".to_string(),
            });
        }

        let video_bitrate_kbps = ((usable_video_bits / segment_duration / 1000.0)
            * (1.0 - config.vbr_safety_margin)) as u32;

        Ok(Self {
            video_bitrate_kbps: video_bitrate_kbps.max(100),
            audio_bitrate_kbps,
        })
    }

    fn audio_bitrate_for_duration(duration: f64) -> u32 {
        if duration > 600.0 {
            64
        } else if duration > 300.0 {
            96
        } else {
            128
        }
    }
}

fn validate_duration(duration: f64, stage: TranscodeStage) -> Result<(), Error> {
    if !(duration.is_finite() && duration > 0.1) {
        return Err(Error::TranscodeFailed {
            stage,
            exit_code: None,
            stderr_tail: "invalid media duration".to_string(),
        });
    }
    Ok(())
}

/// Suggested resolution adjustment based on bitrate budget.
///
/// ## Non-obvious behavior
/// `target_height = None` signals that the source resolution can be preserved.
#[derive(Debug, Clone, Copy)]
pub struct Recommendation {
    pub target_height: Option<u32>,
}

/// Maps bitrate to a conservative target resolution for upload success.
///
/// ## Invariants
/// - `TIERS` is ordered from highest to lowest resolution.
/// - Minimum bitrate thresholds are monotonically decreasing.
pub struct Ladder;

impl Ladder {
    const TIERS: &'static [(u32, u32)] = &[
        (2160, 8000),
        (1440, 4000),
        (1080, 2000),
        (720, 1000),
        (480, 400),
        (360, 150),
        (240, 100),
    ];

    pub fn recommend(
        source_height: Option<u32>,
        video_bitrate_kbps: u32,
        quality_preset: QualityPreset,
    ) -> Recommendation {
        // Determine the highest tier that fits the current bitrate budget.
        let max_supported_height = Self::TIERS
            .iter()
            .find(|&&(_, min_kbps)| video_bitrate_kbps >= min_kbps)
            .map(|&(h, _)| h)
            .unwrap_or(240);

        let mut target_height = match source_height {
            Some(src_h) if src_h <= max_supported_height => None,
            Some(src_h) => {
                // Clamp to the largest tier below the source height.
                let best = Self::TIERS
                    .iter()
                    .filter(|&&(h, min_kbps)| h <= src_h && video_bitrate_kbps >= min_kbps)
                    .map(|&(h, _)| h)
                    .next()
                    .unwrap_or(240);
                Some(best)
            }
            None => {
                // Without source resolution, only downscale for low bitrates.
                if video_bitrate_kbps > 1000 {
                    None
                } else {
                    Some(max_supported_height)
                }
            }
        };

        if let Some(height) = target_height {
            let downshift = match quality_preset {
                QualityPreset::Size => 2,
                QualityPreset::Fast => 1,
                QualityPreset::Balanced | QualityPreset::Quality => 0,
            };

            if downshift > 0 {
                // Apply preset-specific downshift to bias toward size.
                target_height = Some(Self::downshift_height(height, downshift));
            }
        }

        Recommendation { target_height }
    }

    fn downshift_height(height: u32, steps: usize) -> u32 {
        let idx = Self::TIERS
            .iter()
            .position(|&(h, _)| h <= height)
            .unwrap_or(Self::TIERS.len() - 1);
        let next_idx = (idx + steps).min(Self::TIERS.len() - 1);
        Self::TIERS
            .get(next_idx)
            .map(|(tier_height, _)| *tier_height)
            .unwrap_or(240)
    }
}
