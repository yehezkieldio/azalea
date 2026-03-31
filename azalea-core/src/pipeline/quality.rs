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

/// Segment sizing and bitrate budget for split-transcode output.
#[derive(Debug, Clone, Copy)]
pub struct SplitTranscodePlan {
    pub segment_duration: f64,
    pub estimated_segments: u32,
    pub bitrate: BitrateParams,
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
        let target_kbps = if duration > 600.0 {
            128
        } else if duration > 300.0 {
            160
        } else {
            192
        };
        target_kbps.clamp(128, 192)
    }
}

impl SplitTranscodePlan {
    /// Compute a single split-transcode plan from the source duration.
    ///
    /// The planner uses the actual segment duration that will be handed to
    /// ffmpeg, so short media can spend the full segment budget on one part
    /// instead of being budgeted as if it were already at the max segment cap.
    pub fn compute(config: &TranscodeSettings, media_duration: f64) -> Result<Self, Error> {
        validate_duration(media_duration, TranscodeStage::Split)?;
        let segment_duration = media_duration.min(config.max_single_video_duration_secs as f64);
        let bitrate = BitrateParams::compute_for_split(config, segment_duration)?;

        Ok(Self {
            segment_duration,
            estimated_segments: (media_duration / segment_duration).ceil() as u32,
            bitrate,
        })
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

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::panic)]
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn bitrate_compute_rejects_invalid_duration() {
        let err = BitrateParams::compute(&TranscodeSettings::default(), 0.0)
            .expect_err("duration <= 0.1 must fail");

        match err {
            Error::TranscodeFailed {
                stage: TranscodeStage::Transcode,
                ..
            } => {}
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn bitrate_compute_respects_lower_bounds() {
        let params = BitrateParams::compute(&TranscodeSettings::default(), 60.0)
            .expect("default config should produce bitrate params");

        assert!(params.video_bitrate_kbps >= 50);
        assert!(params.audio_bitrate_kbps > 0);
    }

    #[test]
    fn video_bitrate_fits_budget_minus_audio_reserve_for_boundaries_and_presets() {
        const BOUNDARY_CASES: &[(f64, u32)] =
            &[(300.0, 192), (300.001, 160), (600.0, 160), (600.001, 128)];
        const PRESETS: [QualityPreset; 4] = [
            QualityPreset::Fast,
            QualityPreset::Balanced,
            QualityPreset::Quality,
            QualityPreset::Size,
        ];

        for preset in PRESETS {
            for &(duration, expected_audio_kbps) in BOUNDARY_CASES {
                let mut at_cap = TranscodeSettings {
                    quality_preset: preset,
                    transcode_target_ratio: 1.0,
                    container_overhead_ratio: 0.0,
                    audio_vbr_padding: 1.0,
                    vbr_safety_margin: 0.05,
                    ..TranscodeSettings::default()
                };

                let audio_reserve_bits = expected_audio_kbps as f64 * 1000.0 * duration;
                let desired_video_bits = 1000.0 * 1000.0 * duration;
                let budget_before_margin = desired_video_bits / (1.0 - at_cap.vbr_safety_margin);
                at_cap.max_upload_bytes =
                    ((audio_reserve_bits + budget_before_margin) / 8.0).ceil() as u64;

                let params = BitrateParams::compute(&at_cap, duration)
                    .expect("at-cap budget should still leave positive video bitrate");
                assert_eq!(params.audio_bitrate_kbps, expected_audio_kbps);

                let usable_bits = (at_cap.max_upload_bytes as f64 * at_cap.transcode_target_ratio)
                    * 8.0
                    * (1.0 - at_cap.container_overhead_ratio);
                let allowed_video_bits = usable_bits
                    - params.audio_bitrate_kbps as f64
                        * 1000.0
                        * duration
                        * at_cap.audio_vbr_padding;
                let returned_video_bits = params.video_bitrate_kbps as f64 * 1000.0 * duration;
                assert!(
                    returned_video_bits <= allowed_video_bits + 1_000.0,
                    "preset={preset:?}, duration={duration}, returned={returned_video_bits}, allowed={allowed_video_bits}"
                );

                let mut over_cap = at_cap.clone();
                over_cap.max_upload_bytes = (audio_reserve_bits / 8.0).floor() as u64;
                let err = BitrateParams::compute(&over_cap, duration)
                    .expect_err("over-cap budget must fail once no video bits remain");
                match err {
                    Error::TranscodeFailed {
                        stage: TranscodeStage::Transcode,
                        stderr_tail,
                        ..
                    } => assert_eq!(stderr_tail, "video too long to fit"),
                    other => panic!("unexpected error: {other:?}"),
                }
            }
        }
    }

    #[test]
    fn audio_bitrate_short_medium_long_is_clamped_monotonic_and_floor_bounded() {
        let config = TranscodeSettings {
            max_upload_bytes: 256 * 1024 * 1024,
            ..TranscodeSettings::default()
        };

        let short = BitrateParams::compute(&config, 60.0)
            .expect("short clip should produce bitrate params")
            .audio_bitrate_kbps;
        let medium = BitrateParams::compute(&config, 420.0)
            .expect("medium clip should produce bitrate params")
            .audio_bitrate_kbps;
        let long = BitrateParams::compute(&config, 900.0)
            .expect("long clip should produce bitrate params")
            .audio_bitrate_kbps;

        assert_eq!(short, 192);
        assert_eq!(medium, 160);
        assert_eq!(long, 128);
        assert!((128..=192).contains(&short));
        assert!((128..=192).contains(&medium));
        assert!((128..=192).contains(&long));
        assert!(short >= medium && medium >= long);
        assert_eq!(long, 128);
    }

    proptest! {
        #[test]
        fn bitrate_compute_is_monotonic_for_upload_budget(
            duration in 0.2f64..1800.0,
            extra_budget_bytes in 0u64..1_000_000,
            larger_budget_bytes in 0u64..1_000_000,
        ) {
            let config = TranscodeSettings::default();
            let audio_bitrate_kbps = BitrateParams::audio_bitrate_for_duration(duration);
            let audio_total_bits =
                audio_bitrate_kbps as f64 * 1000.0 * duration * config.audio_vbr_padding;
            let usable_budget_scale =
                config.transcode_target_ratio * (1.0 - config.container_overhead_ratio) * 8.0;
            let minimum_upload_bytes = (audio_total_bits / usable_budget_scale).ceil() as u64 + 1;
            let smaller_budget = minimum_upload_bytes + extra_budget_bytes;
            let larger_budget = smaller_budget + larger_budget_bytes;

            let smaller = BitrateParams::compute(
                &TranscodeSettings {
                    max_upload_bytes: smaller_budget,
                    ..config.clone()
                },
                duration,
            )
            .expect("minimum valid budget should produce bitrate params");
            let larger = BitrateParams::compute(
                &TranscodeSettings {
                    max_upload_bytes: larger_budget,
                    ..config
                },
                duration,
            )
            .expect("larger budget should produce bitrate params");

            prop_assert_eq!(smaller.audio_bitrate_kbps, larger.audio_bitrate_kbps);
            prop_assert!(larger.video_bitrate_kbps >= smaller.video_bitrate_kbps);
        }
    }

    #[test]
    fn split_bitrate_uses_expected_audio_floor() {
        let params = BitrateParams::compute_for_split(&TranscodeSettings::default(), 30.0)
            .expect("split params should be computed");
        assert_eq!(params.audio_bitrate_kbps, 128);
        assert!(params.video_bitrate_kbps >= 100);
    }

    #[test]
    fn split_transcode_plan_uses_media_duration_for_short_input() {
        let config = TranscodeSettings {
            max_upload_bytes: 10_000_000,
            split_target_ratio: 1.0,
            container_overhead_ratio: 0.0,
            vbr_safety_margin: 0.0,
            audio_vbr_padding: 1.0,
            max_single_video_duration_secs: 120,
            ..TranscodeSettings::default()
        };

        let plan = SplitTranscodePlan::compute(&config, 60.0)
            .expect("short input should produce a split-transcode plan");

        assert_eq!(plan.segment_duration, 60.0);
        assert_eq!(plan.estimated_segments, 1);
        assert_eq!(plan.bitrate.audio_bitrate_kbps, 128);
        assert_eq!(plan.bitrate.video_bitrate_kbps, 1_205);
    }

    #[test]
    fn split_transcode_plan_caps_segment_duration_for_long_input() {
        let config = TranscodeSettings {
            max_upload_bytes: 10_000_000,
            split_target_ratio: 1.0,
            container_overhead_ratio: 0.0,
            vbr_safety_margin: 0.0,
            audio_vbr_padding: 1.0,
            max_single_video_duration_secs: 120,
            ..TranscodeSettings::default()
        };

        let plan = SplitTranscodePlan::compute(&config, 300.0)
            .expect("long input should produce a split-transcode plan");

        assert_eq!(plan.segment_duration, 120.0);
        assert_eq!(plan.estimated_segments, 3);
        assert_eq!(plan.bitrate.audio_bitrate_kbps, 128);
        assert_eq!(plan.bitrate.video_bitrate_kbps, 538);
    }

    #[test]
    fn ladder_downshifts_for_size_preset() {
        let balanced = Ladder::recommend(Some(1080), 1600, QualityPreset::Balanced);
        let size = Ladder::recommend(Some(1080), 1600, QualityPreset::Size);

        assert_eq!(balanced.target_height, Some(720));
        assert_eq!(size.target_height, Some(360));
    }

    proptest! {
        #[test]
        fn recommendation_is_monotonic_for_unknown_source(low in 100u32..4000, high in 100u32..4000) {
            let (a, b) = if low <= high { (low, high) } else { (high, low) };
            let ra = Ladder::recommend(None, a, QualityPreset::Balanced).target_height.unwrap_or(u32::MAX);
            let rb = Ladder::recommend(None, b, QualityPreset::Balanced).target_height.unwrap_or(u32::MAX);
            prop_assert!(rb >= ra || rb == u32::MAX);
        }
    }
}
