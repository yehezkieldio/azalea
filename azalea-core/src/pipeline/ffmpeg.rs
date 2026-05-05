//! # Module overview
//! ffmpeg argument construction and execution helpers.
//!
//! ## Algorithm overview
//! Builds deterministic argument lists for remux/transcode/split, then executes
//! ffmpeg with bounded output capture and timeouts.
//!
//! ## Security-sensitive paths
//! This module executes an external process; all inputs are pre-sanitized
//! and paths are derived from temp directories (see [`pipeline::download`]).

use crate::config::{HardwareAcceleration, TranscodeSettings};
use crate::pipeline::errors::{Error, TranscodeStage};
use crate::pipeline::process::{SubprocessGuard, kill_process_group, read_bounded};
use crate::pipeline::types::{AudioCodec, MediaFacts, VideoCodec};
use smallvec::SmallVec;
use std::ffi::{OsStr, OsString};
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tracing::{Instrument as _, debug, info, trace, warn};

/// Small-vector argument list to avoid heap allocations for common cases.
///
/// ## Time/space complexity
/// $O(n)$ in argument count, with inline storage for up to 40 entries.
pub type Args = SmallVec<[OsString; 40]>;

const MAX_FFMPEG_KBPS: u64 = 100_000;
const FFMPEG_ERROR_STDERR_TAIL_LINES: usize = 10;
const FFMPEG_SUCCESS_STDERR_TAIL_LINES: usize = 20;
const STDERR_DRAIN_TIMEOUT_SECS: u64 = 2;
const PROGRESS_HEARTBEAT_SECS: u64 = 30;

fn clamp_kbps(value: u64) -> u64 {
    value.min(MAX_FFMPEG_KBPS)
}

fn has_flag(args: &[OsString], flag: &str) -> bool {
    args.iter().any(|arg| arg.as_os_str() == OsStr::new(flag))
}

fn arg_value<'a>(args: &'a [OsString], flag: &str) -> Option<&'a str> {
    args.iter().enumerate().find_map(|(index, arg)| {
        if arg.as_os_str() != OsStr::new(flag) {
            return None;
        }
        args.get(index + 1).and_then(|value| value.to_str())
    })
}

fn format_stderr_tail(stderr: &[u8], max_lines: usize, truncated: bool) -> String {
    let stderr = match stderr.strip_suffix(b"\n") {
        Some(stderr) => stderr.strip_suffix(b"\r").unwrap_or(stderr),
        None => stderr,
    };

    let tail_start = stderr_tail_start(stderr, max_lines);
    let tail = stderr.get(tail_start..).unwrap_or_default();
    let mut joined = String::new();
    push_stderr_tail(&mut joined, tail);

    if truncated {
        if !joined.is_empty() {
            joined.push('\n');
        }
        joined.push_str("[stderr truncated]");
    }
    joined
}

fn stderr_tail_start(stderr: &[u8], max_lines: usize) -> usize {
    if max_lines == 0 {
        return stderr.len();
    }

    let mut tail_len = 0usize;
    let mut kept_lines = 0usize;
    let mut segments = stderr.rsplitn(max_lines + 1, |byte| *byte == b'\n');

    while kept_lines < max_lines {
        let Some(segment) = segments.next() else {
            return 0;
        };

        tail_len += segment.len();
        if kept_lines > 0 {
            tail_len += 1;
        }
        kept_lines += 1;
    }

    if segments.next().is_some() {
        stderr.len().saturating_sub(tail_len)
    } else {
        0
    }
}

fn push_stderr_tail(output: &mut String, stderr_tail: &[u8]) {
    let tail = String::from_utf8_lossy(stderr_tail);
    if !tail.contains('\r') {
        output.push_str(&tail);
        return;
    }

    for line in tail.split('\n') {
        if !output.is_empty() {
            output.push('\n');
        }
        output.push_str(line.strip_suffix('\r').unwrap_or(line));
    }
}

fn push_hw_device_input_args(args: &mut Args, config: &TranscodeSettings, split: bool) {
    match config.hardware_acceleration {
        HardwareAcceleration::Vaapi => {
            let operation = if split { "split" } else { "transcode" };
            debug!(
                vaapi_device = %config.vaapi_device,
                operation,
                "Applying VAAPI hardware acceleration args"
            );
            args.push("-init_hw_device".into());
            args.push(format!("vaapi=va:{}", config.vaapi_device).into());
            args.push("-filter_hw_device".into());
            args.push("va".into());
            args.push("-hwaccel".into());
            args.push("vaapi".into());
            args.push("-hwaccel_output_format".into());
            args.push("vaapi".into());
            args.push("-hwaccel_device".into());
            args.push("va".into());
        }
        HardwareAcceleration::Qsv => {
            let operation = if split { "split" } else { "transcode" };
            debug!(operation, "Applying QSV hardware acceleration args");
            #[cfg(unix)]
            {
                args.push("-init_hw_device".into());
                args.push(format!("vaapi=va:{}", config.vaapi_device).into());
                args.push("-init_hw_device".into());
                args.push("qsv=qsv@va".into());
                args.push("-filter_hw_device".into());
                args.push("qsv".into());
            }
            #[cfg(not(unix))]
            {
                args.push("-init_hw_device".into());
                args.push("qsv=qsv:MFX_IMPL_hw_any".into());
                args.push("-filter_hw_device".into());
                args.push("qsv".into());
            }
        }
        HardwareAcceleration::None
        | HardwareAcceleration::Nvenc
        | HardwareAcceleration::VideoToolbox
        | HardwareAcceleration::Amf => {}
    }
}

fn video_filter(
    height: Option<u32>,
    hardware_acceleration: HardwareAcceleration,
) -> Option<String> {
    match (hardware_acceleration, height) {
        (HardwareAcceleration::Vaapi, Some(height)) => Some(format!(
            "format=nv12|vaapi,hwupload,scale_vaapi=-2:{height}"
        )),
        (HardwareAcceleration::Vaapi, None) => Some("format=nv12|vaapi,hwupload".into()),
        (HardwareAcceleration::Qsv, Some(height)) => Some(format!(
            "format=nv12,hwupload=extra_hw_frames=64,scale_qsv=-1:{height}"
        )),
        (HardwareAcceleration::Qsv, None) => {
            Some("format=nv12,hwupload=extra_hw_frames=64,format=qsv".into())
        }
        (_, Some(height)) => Some(format!("scale=-2:{height}")),
        (_, None) => None,
    }
}

/// Stream-copy paths write MP4 outputs, so only MP4-friendly codecs qualify.
pub fn mp4_stream_copy_viable(facts: MediaFacts) -> bool {
    matches!(facts.video_codec, VideoCodec::H264)
        && matches!(facts.audio_codec, AudioCodec::Aac | AudioCodec::None)
}

fn push_video_encoding_args(
    args: &mut Args,
    config: &TranscodeSettings,
    video_kbps: u32,
    transcode_concurrency: u32,
) {
    let video_kbps_u64 = video_kbps as u64;
    let video_kbps_clamped = clamp_kbps(video_kbps_u64);
    let video_buf_kbps = clamp_kbps(video_kbps_u64.saturating_mul(2));

    match config.hardware_acceleration {
        HardwareAcceleration::None => {
            let threads = config.effective_ffmpeg_threads(transcode_concurrency);
            let crf = config.quality_preset.crf();
            let preset = config.quality_preset.ffmpeg_preset();
            let x264_params = if preset == "ultrafast" || preset == "superfast" {
                format!("threads={}", threads)
            } else {
                let lookahead = threads.min(2);
                format!("threads={}:lookahead_threads={}", threads, lookahead)
            };

            args.push("-preset".into());
            args.push(preset.into());
            args.push("-crf".into());
            args.push(crf.to_string().into());
            args.push("-maxrate".into());
            args.push(format!("{}k", video_kbps_clamped).into());
            args.push("-bufsize".into());
            args.push(format!("{}k", video_buf_kbps).into());
            args.push("-threads".into());
            args.push(threads.to_string().into());
            args.push("-x264-params".into());
            args.push(x264_params.into());
            args.push("-tune".into());
            args.push("zerolatency".into());
        }
        HardwareAcceleration::Nvenc => {
            let cq = match config.quality_preset {
                crate::config::QualityPreset::Fast => 28,
                crate::config::QualityPreset::Balanced => 23,
                crate::config::QualityPreset::Quality => 19,
                crate::config::QualityPreset::Size => 32,
            };
            args.push("-preset".into());
            args.push("p4".into());
            args.push("-rc".into());
            args.push("vbr".into());
            args.push("-cq".into());
            args.push(cq.to_string().into());
            args.push("-b:v".into());
            args.push(format!("{}k", video_kbps_clamped).into());
            args.push("-maxrate".into());
            args.push(format!("{}k", video_kbps_clamped).into());
            args.push("-bufsize".into());
            args.push(format!("{}k", video_buf_kbps).into());
        }
        HardwareAcceleration::Vaapi => {
            args.push("-b:v".into());
            args.push(format!("{}k", video_kbps_clamped).into());
            args.push("-maxrate".into());
            args.push(format!("{}k", video_kbps_clamped).into());
        }
        HardwareAcceleration::VideoToolbox => {
            args.push("-b:v".into());
            args.push(format!("{}k", video_kbps_clamped).into());
            args.push("-realtime".into());
            args.push("true".into());
        }
        HardwareAcceleration::Qsv => {
            args.push("-b:v".into());
            args.push(format!("{}k", video_kbps_clamped).into());
            args.push("-maxrate".into());
            args.push(format!("{}k", video_kbps_clamped).into());
            args.push("-bufsize".into());
            args.push(format!("{}k", video_buf_kbps).into());
            args.push("-bitrate_limit".into());
            args.push("1".into());
            args.push("-low_delay_brc".into());
            args.push("1".into());
        }
        HardwareAcceleration::Amf => {
            let quality = match config.quality_preset {
                crate::config::QualityPreset::Fast => "speed",
                crate::config::QualityPreset::Balanced | crate::config::QualityPreset::Size => {
                    "balanced"
                }
                crate::config::QualityPreset::Quality => "quality",
            };
            args.push("-rc".into());
            args.push("vbr_latency".into());
            args.push("-quality".into());
            args.push(quality.into());
            args.push("-b:v".into());
            args.push(format!("{}k", video_kbps_clamped).into());
            args.push("-maxrate".into());
            args.push(format!("{}k", video_kbps_clamped).into());
            args.push("-bufsize".into());
            args.push(format!("{}k", video_buf_kbps).into());
        }
    }
}

/// Build args for a stream-copy remux.
pub fn remux_args(input: &Path, output: &Path, threads: u32) -> Args {
    let mut args = Args::new();
    args.push("-y".into());
    args.push("-i".into());
    args.push(input.as_os_str().into());
    args.push("-c".into());
    args.push("copy".into());
    if threads > 0 {
        args.push("-threads".into());
        args.push(threads.to_string().into());
    }
    args.push("-movflags".into());
    args.push("+faststart".into());
    args.push(output.as_os_str().into());
    args
}

pub fn transcode_args(
    input: &Path,
    output: &Path,
    video_kbps: u32,
    audio_kbps: u32,
    max_height: Option<u32>,
    config: &TranscodeSettings,
    transcode_concurrency: u32,
) -> Args {
    debug!(
        hardware_acceleration = ?config.hardware_acceleration,
        encoder = config.hardware_acceleration.encoder(),
        max_height,
        "Building ffmpeg transcode args"
    );
    let mut args = Args::new();
    args.push("-y".into());

    push_hw_device_input_args(&mut args, config, false);

    args.push("-i".into());
    args.push(input.as_os_str().into());

    if let Some(filter) = video_filter(max_height, config.hardware_acceleration) {
        args.push("-vf".into());
        args.push(filter.into());
    }

    args.push("-c:v".into());
    args.push(config.hardware_acceleration.encoder().into());

    push_video_encoding_args(&mut args, config, video_kbps, transcode_concurrency);

    args.push("-c:a".into());
    args.push("aac".into());
    args.push("-b:a".into());
    args.push(format!("{}k", audio_kbps).into());
    args.push("-ac".into());
    args.push("2".into());
    args.push("-movflags".into());
    args.push("+faststart".into());
    args.push(output.as_os_str().into());

    args
}

#[allow(clippy::too_many_arguments)]
pub fn transcode_segment_args(
    input: &Path,
    output: &Path,
    start_secs: f64,
    duration_secs: f64,
    video_kbps: u32,
    audio_kbps: u32,
    config: &TranscodeSettings,
    transcode_concurrency: u32,
) -> Args {
    debug!(
        hardware_acceleration = ?config.hardware_acceleration,
        encoder = config.hardware_acceleration.encoder(),
        start_secs,
        duration_secs,
        "Building ffmpeg segment transcode args"
    );
    let mut args = Args::new();
    args.push("-y".into());

    push_hw_device_input_args(&mut args, config, true);

    args.push("-ss".into());
    args.push(format!("{start_secs:.3}").into());
    args.push("-i".into());
    args.push(input.as_os_str().into());
    args.push("-t".into());
    args.push(format!("{duration_secs:.3}").into());

    if let Some(filter) = video_filter(None, config.hardware_acceleration) {
        args.push("-vf".into());
        args.push(filter.into());
    }

    args.push("-c:v".into());
    args.push(config.hardware_acceleration.encoder().into());

    push_video_encoding_args(&mut args, config, video_kbps, transcode_concurrency);

    args.push("-c:a".into());
    args.push("aac".into());
    args.push("-b:a".into());
    args.push(format!("{}k", audio_kbps).into());
    args.push("-ac".into());
    args.push("2".into());
    args.push("-force_key_frames".into());
    args.push("expr:gte(t,0)".into());
    args.push("-reset_timestamps".into());
    args.push("1".into());
    args.push("-avoid_negative_ts".into());
    args.push("make_zero".into());
    args.push("-movflags".into());
    args.push("+faststart".into());
    args.push(output.as_os_str().into());

    args
}

pub async fn execute(
    ffmpeg_path: &Path,
    args: &[OsString],
    timeout: Duration,
    stage: TranscodeStage,
) -> Result<(), Error> {
    const FFMPEG_STDERR_LIMIT: usize = 512 * 1024;
    const WAIT_TIMEOUT_SECS: u64 = 30;

    let ffmpeg_span = tracing::info_span!(
        "ffmpeg",
        ?stage,
        ffmpeg = %ffmpeg_path.display(),
        timeout_ms = timeout.as_millis() as u64,
        video_encoder = arg_value(args, "-c:v").unwrap_or("unknown"),
        hwaccel = arg_value(args, "-hwaccel").unwrap_or("none")
    );

    async {
        let started = std::time::Instant::now();
        debug!(
            ?stage,
            video_encoder = arg_value(args, "-c:v").unwrap_or("unknown"),
            hwaccel = arg_value(args, "-hwaccel").unwrap_or("none"),
            hwaccel_device = arg_value(args, "-hwaccel_device").unwrap_or("none"),
            init_hw_device = has_flag(args, "-init_hw_device"),
            "ffmpeg execution hardware acceleration plan"
        );
        trace!(?stage, args = ?args, "ffmpeg execution args");

        let mut command = Command::new(ffmpeg_path);
        command
            .args(args)
            .args(["-progress", "pipe:1", "-nostats"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut guard = SubprocessGuard::spawn(&mut command).map_err(Error::Io)?;

        let stdout = guard
            .child_mut()
            .stdout
            .take()
            .ok_or_else(|| Error::Io(std::io::Error::other("ffmpeg stdout missing")))?;
        let stderr = guard
            .child_mut()
            .stderr
            .take()
            .ok_or_else(|| Error::Io(std::io::Error::other("ffmpeg stderr missing")))?;

        let mut stderr_handle =
            tokio::spawn(async move { read_bounded(stderr, FFMPEG_STDERR_LIMIT, None).await });

        let start_time = std::time::Instant::now();
        let mut last_heartbeat = start_time;
        let mut reader = BufReader::new(stdout).lines();

        // Drain ffmpeg progress output to avoid stdout pipe backpressure.
        loop {
            if start_time.elapsed() > timeout {
                warn!(
                    ?stage,
                    timeout_ms = timeout.as_millis() as u64,
                    "ffmpeg execution timed out"
                );
                kill_process_group(&mut guard).await;
                let _ = guard.wait().await;
                stderr_handle.abort();
                return Err(Error::Timeout {
                    operation: "ffmpeg",
                    duration: timeout,
                });
            }

            let read_result =
                tokio::time::timeout(Duration::from_secs(5), reader.next_line()).await;

            match read_result {
                Ok(Ok(Some(_))) => {}
                Ok(Ok(None)) => break,
                Ok(Err(_)) => break,
                Err(_) => {
                    if last_heartbeat.elapsed() >= Duration::from_secs(PROGRESS_HEARTBEAT_SECS) {
                        info!(
                            ?stage,
                            elapsed_ms = start_time.elapsed().as_millis() as u64,
                            timeout_ms = timeout.as_millis() as u64,
                            "ffmpeg execution still running"
                        );
                        last_heartbeat = std::time::Instant::now();
                    }
                    continue;
                }
            }
        }

        let status = match tokio::time::timeout(
            Duration::from_secs(WAIT_TIMEOUT_SECS),
            guard.wait(),
        )
        .await
        {
            Ok(status) => status.map_err(Error::Io)?,
            Err(_) => {
                warn!(
                    wait_timeout_secs = WAIT_TIMEOUT_SECS,
                    "ffmpeg wait timed out"
                );
                kill_process_group(&mut guard).await;
                let _ = guard.wait().await;
                stderr_handle.abort();
                return Err(Error::Timeout {
                    operation: "ffmpeg-wait",
                    duration: Duration::from_secs(WAIT_TIMEOUT_SECS),
                });
            }
        };

        // Keep error payloads compact while still surfacing ffmpeg's final
        // encoder progress lines in debug logs on successful runs.
        let (stderr_error_tail, stderr_debug_tail) = match tokio::time::timeout(
            Duration::from_secs(STDERR_DRAIN_TIMEOUT_SECS),
            &mut stderr_handle,
        )
        .await
        {
            Ok(Ok(Ok(output))) => (
                format_stderr_tail(
                    &output.data,
                    FFMPEG_ERROR_STDERR_TAIL_LINES,
                    output.exceeded,
                ),
                format_stderr_tail(
                    &output.data,
                    FFMPEG_SUCCESS_STDERR_TAIL_LINES,
                    output.exceeded,
                ),
            ),
            Ok(Ok(Err(e))) => {
                let error = format!("stderr read failed: {e}");
                (error.clone(), error)
            }
            Ok(Err(_)) => {
                let error = "stderr task failed".to_string();
                (error.clone(), error)
            }
            Err(_) => {
                warn!(
                    ?stage,
                    drain_timeout_secs = STDERR_DRAIN_TIMEOUT_SECS,
                    "ffmpeg stderr drain timed out"
                );
                stderr_handle.abort();
                let error = "stderr drain timed out".to_string();
                (error.clone(), error)
            }
        };

        if !status.success() {
            warn!(
                ?stage,
                exit_code = status.code(),
                elapsed_ms = started.elapsed().as_millis() as u64,
                "ffmpeg execution failed"
            );
            return Err(Error::TranscodeFailed {
                stage,
                exit_code: status.code(),
                stderr_tail: stderr_error_tail,
            });
        }

        debug!(
            ?stage,
            elapsed_ms = started.elapsed().as_millis() as u64,
            stderr_tail = %stderr_debug_tail,
            "ffmpeg execution succeeded"
        );
        Ok(())
    }
    .instrument(ffmpeg_span)
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn to_strings(args: &[OsString]) -> Vec<String> {
        args.iter()
            .map(|value| value.to_string_lossy().to_string())
            .collect()
    }

    fn flag_count(args: &[String], flag: &str) -> usize {
        args.iter().filter(|arg| arg.as_str() == flag).count()
    }

    #[test]
    fn remux_args_include_copy_faststart_and_output() {
        let args = remux_args(Path::new("in.mp4"), Path::new("out.mp4"), 4);
        let as_text = to_strings(&args);
        assert!(as_text.windows(2).any(|w| w == ["-c", "copy"]));
        assert!(as_text.windows(2).any(|w| w == ["-movflags", "+faststart"]));
        assert!(as_text.contains(&"out.mp4".to_string()));
    }

    #[test]
    fn transcode_args_add_scale_filter_when_height_is_set() {
        let args = transcode_args(
            Path::new("input.mp4"),
            Path::new("output.mp4"),
            900,
            128,
            Some(720),
            &TranscodeSettings::default(),
            2,
        );
        let as_text = to_strings(&args);
        assert!(as_text.windows(2).any(|w| {
            w.first() == Some(&"-vf".to_string())
                && w.get(1).is_some_and(|value| value.contains("scale=-2:720"))
        }));
        assert!(as_text.windows(2).any(|w| w == ["-c:v", "libx264"]));
    }

    #[test]
    fn transcode_segment_args_seek_and_reset_segment_timestamps() {
        let args = transcode_segment_args(
            Path::new("input.mp4"),
            Path::new("seg001.mp4"),
            12.5,
            10.0,
            900,
            128,
            &TranscodeSettings::default(),
            2,
        );
        let as_text = to_strings(&args);

        assert!(as_text.windows(2).any(|w| w == ["-ss", "12.500"]));
        let seek_index = as_text.iter().position(|arg| arg == "-ss");
        let input_index = as_text.iter().position(|arg| arg == "-i");
        assert!(matches!(
            (seek_index, input_index),
            (Some(seek_index), Some(input_index)) if seek_index < input_index
        ));
        assert!(as_text.windows(2).any(|w| w == ["-t", "10.000"]));
        assert!(
            as_text
                .windows(2)
                .any(|w| w == ["-force_key_frames", "expr:gte(t,0)"])
        );
        assert!(as_text.windows(2).any(|w| w == ["-reset_timestamps", "1"]));
        assert!(
            as_text
                .windows(2)
                .any(|w| w == ["-avoid_negative_ts", "make_zero"])
        );
    }

    #[test]
    fn transcode_args_configure_qsv_device_and_filter() {
        let args = transcode_args(
            Path::new("input.mp4"),
            Path::new("output.mp4"),
            900,
            128,
            Some(720),
            &TranscodeSettings {
                hardware_acceleration: HardwareAcceleration::Qsv,
                ..TranscodeSettings::default()
            },
            2,
        );
        let as_text = to_strings(&args);

        #[cfg(unix)]
        assert!(
            as_text
                .windows(2)
                .any(|w| w == ["-init_hw_device", "vaapi=va:/dev/dri/renderD128"])
        );
        #[cfg(unix)]
        assert!(
            as_text
                .windows(2)
                .any(|w| w == ["-init_hw_device", "qsv=qsv@va"])
        );
        #[cfg(not(unix))]
        assert!(
            as_text
                .windows(2)
                .any(|w| w == ["-init_hw_device", "qsv=qsv:MFX_IMPL_hw_any"])
        );
        assert!(
            as_text
                .windows(2)
                .any(|w| w == ["-filter_hw_device", "qsv"])
        );
        assert!(as_text.windows(2).any(|w| w == ["-c:v", "h264_qsv"]));
        assert!(as_text.windows(2).any(|w| {
            w.first() == Some(&"-vf".to_string())
                && w.get(1)
                    .is_some_and(|value| value.contains("scale_qsv=-1:720"))
        }));
        assert!(as_text.windows(2).any(|w| w == ["-bitrate_limit", "1"]));
        assert!(as_text.windows(2).any(|w| w == ["-low_delay_brc", "1"]));
    }

    #[test]
    fn transcode_segment_args_configure_qsv_bitrate_control() {
        let args = transcode_segment_args(
            Path::new("input.mp4"),
            Path::new("seg001.mp4"),
            0.0,
            120.0,
            278,
            128,
            &TranscodeSettings {
                hardware_acceleration: HardwareAcceleration::Qsv,
                ..TranscodeSettings::default()
            },
            2,
        );
        let as_text = to_strings(&args);

        assert!(as_text.windows(2).any(|w| w == ["-c:v", "h264_qsv"]));
        assert!(as_text.windows(2).any(|w| w == ["-b:v", "278k"]));
        assert!(as_text.windows(2).any(|w| w == ["-maxrate", "278k"]));
        assert!(as_text.windows(2).any(|w| w == ["-bitrate_limit", "1"]));
        assert!(as_text.windows(2).any(|w| w == ["-low_delay_brc", "1"]));
        assert_eq!(flag_count(&as_text, "-b:a"), 1);
    }

    #[test]
    fn format_stderr_tail_keeps_last_20_lines_for_success_logs() {
        let stderr = (1..=25)
            .map(|line| format!("line-{line}"))
            .collect::<Vec<_>>()
            .join("\n");

        let tail = format_stderr_tail(stderr.as_bytes(), FFMPEG_SUCCESS_STDERR_TAIL_LINES, false);

        assert_eq!(
            tail,
            (6..=25)
                .map(|line| format!("line-{line}"))
                .collect::<Vec<_>>()
                .join("\n")
        );
    }

    #[test]
    fn format_stderr_tail_appends_truncation_marker() {
        let tail = format_stderr_tail(b"speed=8.2x\n", FFMPEG_SUCCESS_STDERR_TAIL_LINES, true);

        assert_eq!(tail, "speed=8.2x\n[stderr truncated]");
    }

    #[test]
    fn format_stderr_tail_normalizes_crlf_line_endings() {
        let tail = format_stderr_tail(
            b"frame=1\r\nfps=30\r\n",
            FFMPEG_SUCCESS_STDERR_TAIL_LINES,
            false,
        );

        assert_eq!(tail, "frame=1\nfps=30");
    }

    #[test]
    fn mp4_stream_copy_viability_requires_h264_and_aac_or_silence() {
        assert!(mp4_stream_copy_viable(MediaFacts {
            video_codec: VideoCodec::H264,
            audio_codec: AudioCodec::Aac,
            ..MediaFacts::default()
        }));
        assert!(mp4_stream_copy_viable(MediaFacts {
            video_codec: VideoCodec::H264,
            audio_codec: AudioCodec::None,
            ..MediaFacts::default()
        }));
        assert!(!mp4_stream_copy_viable(MediaFacts {
            video_codec: VideoCodec::Vp9,
            audio_codec: AudioCodec::Opus,
            ..MediaFacts::default()
        }));
        assert!(!mp4_stream_copy_viable(MediaFacts {
            video_codec: VideoCodec::H264,
            audio_codec: AudioCodec::Opus,
            ..MediaFacts::default()
        }));
    }
}
