//! Keyframe-aligned stream-copy split for sources that are already
//! upload-compatible (H.264 video, AAC or no audio).
//!
//! ## Rationale
//! Re-encoding a split at a bitrate near the source's spends seconds of CPU
//! per minute of video only to lose quality: a 114 s 1080p source took 12.9 s
//! on 4 cores to become four parts at the source bitrate. Cutting at
//! keyframes with `-c copy` produces upload-sized parts in ~0.3 s (one
//! demux-only packet scan plus one copy pass) at source quality.
//!
//! ## Algorithm
//! 1. Scan packets with ffprobe (no decode) to get the byte offset of every
//!    video keyframe in demux order.
//! 2. Greedily cut at the furthest keyframe that keeps the current part under
//!    the byte budget. Furthest-fit is optimal for the minimum number of
//!    contiguous parts under a maximum size.
//! 3. Cut with ffmpeg's segment muxer at exactly those keyframe times.
//! 4. Verify every output against the upload limit; any miss returns `None` so
//!    the caller falls back to the transcode split.

use crate::config::EngineSettings;
use crate::pipeline::errors::{Error, TranscodeStage};
use crate::pipeline::ffmpeg;
use crate::pipeline::optimize::SegmentOutput;
use crate::pipeline::process::{JsonSubprocessError, run_json_subprocess};
use std::path::Path;
use std::time::Duration;
use tokio::fs;
use tokio::process::Command;

/// ~75 bytes per packet line; covers well over an hour of typical media.
const PACKET_SCAN_STDOUT_LIMIT: usize = 64 * 1024 * 1024;
const PACKET_SCAN_STDERR_LINES: usize = 10;
/// Cut times are nudged before the keyframe so ffprobe's 6-decimal rounding
/// can never push the segment muxer past the intended keyframe.
const CUT_EPSILON_SECS: f64 = 0.001;

/// Video keyframe positions of a file, in demux order.
#[derive(Debug, Default, PartialEq)]
struct PacketScan {
    /// `(pts seconds, bytes before this keyframe)` for every keyframe after
    /// the first byte.
    keyframes: Vec<(f64, u64)>,
    total_bytes: u64,
}

/// Split `input` into keyframe-aligned stream-copied parts of at most
/// `max_parts`, or return `Ok(None)` when that is not possible.
///
/// ## Preconditions
/// The caller verified the codecs are MP4 stream-copy viable.
pub(super) async fn split(
    input: &Path,
    config: &EngineSettings,
    max_parts: u32,
) -> Result<Option<Vec<SegmentOutput>>, Error> {
    let scan = scan_packets(input, config).await?;
    let budget = (config.transcode.max_upload_bytes as f64
        * (1.0 - config.transcode.container_overhead_ratio)) as u64;
    let Some(cuts) = plan_cuts(&scan, budget) else {
        tracing::info!(
            budget,
            "Stream-copy split skipped: a keyframe interval exceeds the part budget"
        );
        return Ok(None);
    };
    let parts = cuts.len() + 1;
    if parts > max_parts as usize {
        tracing::info!(
            parts,
            max_parts,
            "Stream-copy split skipped: too many parts"
        );
        return Ok(None);
    }

    let stem = input.file_stem().unwrap_or_default().to_string_lossy();
    let prefix = format!("{stem}_copy");
    let pattern = input.with_file_name(format!("{prefix}%03d.mp4"));
    let args = ffmpeg::copy_segment_args(input, &pattern, &cuts);
    ffmpeg::execute(
        &config.binaries.ffmpeg,
        &args,
        Duration::from_secs(config.transcode.ffmpeg_timeout_secs),
        TranscodeStage::Split,
    )
    .await?;

    let mut segments = Vec::with_capacity(parts);
    for index in 0..parts {
        let path = input.with_file_name(format!("{prefix}{index:03}.mp4"));
        let size = fs::metadata(&path).await.map_or(0, |meta| meta.len());
        segments.push(SegmentOutput { path, size });
    }
    let fits = segments
        .iter()
        .all(|segment| segment.size > 0 && segment.size <= config.transcode.max_upload_bytes);
    if !fits {
        tracing::warn!(
            parts,
            "Stream-copy split produced an unusable part; falling back"
        );
        for segment in &segments {
            let _ = fs::remove_file(&segment.path).await;
        }
        return Ok(None);
    }

    tracing::info!(parts, "Stream-copy split completed");
    Ok(Some(segments))
}

async fn scan_packets(input: &Path, config: &EngineSettings) -> Result<PacketScan, Error> {
    let timeout = Duration::from_secs(config.transcode.ffprobe_timeout_secs);
    let mut command = Command::new(&config.binaries.ffprobe);
    command
        .args([
            "-v",
            "error",
            "-show_entries",
            "packet=codec_type,pts_time,size,flags",
            "-of",
            "compact=p=0",
        ])
        .arg(input.as_os_str());
    let output = run_json_subprocess(
        &mut command,
        timeout,
        PACKET_SCAN_STDOUT_LIMIT,
        PACKET_SCAN_STDERR_LINES,
    )
    .await
    .map_err(|error| match error {
        JsonSubprocessError::Io(error) => Error::Io(error),
        JsonSubprocessError::Timeout => Error::Timeout {
            operation: "ffprobe packet scan",
            duration: timeout,
        },
        JsonSubprocessError::OutputLimit { stderr_tail } => Error::TranscodeFailed {
            stage: TranscodeStage::Split,
            exit_code: None,
            stderr_tail: format!("packet scan output exceeded limit: {stderr_tail}"),
        },
    })?;
    if !output.status.success() {
        return Err(Error::TranscodeFailed {
            stage: TranscodeStage::Split,
            exit_code: output.status.code(),
            stderr_tail: output.stderr_tail.into(),
        });
    }
    Ok(parse_packets(&output.stdout))
}

/// Parse `ffprobe -of compact=p=0` packet lines (`key=value|key=value`).
fn parse_packets(stdout: &[u8]) -> PacketScan {
    let mut scan = PacketScan::default();
    for line in stdout.split(|byte| *byte == b'\n') {
        let Ok(line) = std::str::from_utf8(line) else {
            continue;
        };
        let (mut video, mut keyframe, mut pts, mut size) = (false, false, None, 0u64);
        for field in line.trim_end().split('|') {
            match field.split_once('=') {
                Some(("codec_type", value)) => video = value == "video",
                Some(("pts_time", value)) => pts = value.parse::<f64>().ok(),
                Some(("size", value)) => size = value.parse().unwrap_or(0),
                Some(("flags", value)) => keyframe = value.starts_with('K'),
                _ => {}
            }
        }
        if video
            && keyframe
            && scan.total_bytes > 0
            && let Some(pts) = pts
        {
            scan.keyframes.push((pts, scan.total_bytes));
        }
        scan.total_bytes = scan.total_bytes.saturating_add(size);
    }
    scan
}

/// Furthest-fit cut planning: returns cut times, or `None` when some
/// keyframe interval alone exceeds `budget`.
fn plan_cuts(scan: &PacketScan, budget: u64) -> Option<Vec<f64>> {
    let mut cuts = Vec::new();
    let mut part_start = 0u64;
    let mut last_fit: Option<(f64, u64)> = None;
    let end = (f64::INFINITY, scan.total_bytes);

    for &(time, offset) in scan.keyframes.iter().chain(std::iter::once(&end)) {
        if offset.saturating_sub(part_start) <= budget {
            last_fit = Some((time, offset));
            continue;
        }
        let (cut_time, cut_offset) = last_fit.filter(|(_, fit)| *fit > part_start)?;
        cuts.push((cut_time - CUT_EPSILON_SECS).max(0.0));
        part_start = cut_offset;
        if offset.saturating_sub(part_start) > budget {
            return None;
        }
        last_fit = Some((time, offset));
    }
    Some(cuts)
}

#[cfg(test)]
mod tests {
    use super::{CUT_EPSILON_SECS, PacketScan, parse_packets, plan_cuts};

    #[test]
    fn parses_keyframe_offsets_in_demux_order() {
        let stdout = b"codec_type=video|pts_time=0.000000|size=100|flags=K__\n\
codec_type=audio|pts_time=0.000000|size=10|flags=K__\n\
codec_type=video|pts_time=0.041000|size=50|flags=___\n\
codec_type=video|pts_time=2.000000|size=90|flags=K__\n\
codec_type=audio|pts_time=2.000000|size=10|flags=K__\n";
        assert_eq!(
            parse_packets(stdout),
            PacketScan {
                keyframes: vec![(2.0, 160)],
                total_bytes: 260,
            }
        );
    }

    #[test]
    fn furthest_fit_minimizes_parts() {
        let scan = PacketScan {
            keyframes: vec![(2.0, 40), (4.0, 80), (6.0, 120), (8.0, 160)],
            total_bytes: 200,
        };
        let cuts = plan_cuts(&scan, 100).unwrap_or_default();
        assert_eq!(cuts, [4.0 - CUT_EPSILON_SECS, 8.0 - CUT_EPSILON_SECS]);
    }

    #[test]
    fn whole_file_under_budget_needs_no_cut() {
        let scan = PacketScan {
            keyframes: vec![(2.0, 40)],
            total_bytes: 90,
        };
        assert_eq!(plan_cuts(&scan, 100), Some(Vec::new()));
    }

    #[test]
    fn oversized_keyframe_interval_is_rejected() {
        let scan = PacketScan {
            keyframes: vec![(2.0, 150)],
            total_bytes: 200,
        };
        assert_eq!(plan_cuts(&scan, 100), None);

        let tail = PacketScan {
            keyframes: vec![(2.0, 50)],
            total_bytes: 200,
        };
        assert_eq!(plan_cuts(&tail, 100), None);
    }
}
