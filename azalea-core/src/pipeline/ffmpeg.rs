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
use smallvec::SmallVec;
use std::ffi::{OsStr, OsString};
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tracing::{Instrument as _, debug, trace, warn};

/// Small-vector argument list to avoid heap allocations for common cases.
///
/// ## Time/space complexity
/// $O(n)$ in argument count, with inline storage for up to 40 entries.
pub type Args = SmallVec<[OsString; 40]>;

const MAX_FFMPEG_KBPS: u64 = 100_000;

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

    if config.hardware_acceleration == HardwareAcceleration::Vaapi {
        debug!(
            vaapi_device = %config.vaapi_device,
            "Applying VAAPI hardware acceleration args for transcode"
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

    args.push("-i".into());
    args.push(input.as_os_str().into());

    if let Some(height) = max_height {
        let filter = match config.hardware_acceleration {
            HardwareAcceleration::Vaapi => {
                format!("format=nv12|vaapi,hwupload,scale_vaapi=-2:{}", height)
            }
            _ => format!("scale=-2:{}", height),
        };
        args.push("-vf".into());
        args.push(filter.into());
    } else if config.hardware_acceleration == HardwareAcceleration::Vaapi {
        args.push("-vf".into());
        args.push("format=nv12|vaapi,hwupload".into());
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

pub fn split_args(
    input: &Path,
    pattern: &Path,
    segment_duration: f64,
    video_kbps: u32,
    audio_kbps: u32,
    config: &TranscodeSettings,
    transcode_concurrency: u32,
) -> Args {
    debug!(
        hardware_acceleration = ?config.hardware_acceleration,
        encoder = config.hardware_acceleration.encoder(),
        "Building ffmpeg split args"
    );
    let mut args = Args::new();
    args.push("-y".into());

    if config.hardware_acceleration == HardwareAcceleration::Vaapi {
        debug!(
            vaapi_device = %config.vaapi_device,
            "Applying VAAPI hardware acceleration args for split"
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

    args.push("-i".into());
    args.push(input.as_os_str().into());

    if config.hardware_acceleration == HardwareAcceleration::Vaapi {
        args.push("-vf".into());
        args.push("format=nv12|vaapi,hwupload".into());
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

    args.extend([
        "-f".into(),
        "segment".into(),
        "-segment_time".into(),
        format!("{:.1}", segment_duration).into(),
        "-reset_timestamps".into(),
        "1".into(),
        "-movflags".into(),
        "+faststart".into(),
    ]);
    args.push(pattern.as_os_str().into());
    args
}

/// Build args to split without re-encoding.
pub fn split_copy_args(input: &Path, pattern: &Path, segment_duration: f64) -> Args {
    let mut args = Args::new();
    args.push("-y".into());
    args.push("-i".into());
    args.push(input.as_os_str().into());
    args.push("-c".into());
    args.push("copy".into());
    args.push("-f".into());
    args.push("segment".into());
    args.push("-segment_time".into());
    args.push(format!("{:.1}", segment_duration).into());
    args.push("-reset_timestamps".into());
    args.push("1".into());
    args.push("-movflags".into());
    args.push("+faststart".into());
    args.push(pattern.as_os_str().into());
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
        "ffmpeg.execute",
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

        let stderr_handle =
            tokio::spawn(async move { read_bounded(stderr, FFMPEG_STDERR_LIMIT, None).await });

        let start_time = std::time::Instant::now();
        let mut reader = BufReader::new(stdout).lines();

        // Drain ffmpeg progress output to avoid stdout pipe backpressure.
        loop {
            if start_time.elapsed() > timeout {
                warn!(
                    ?stage,
                    timeout_ms = timeout.as_millis() as u64,
                    "ffmpeg execution timed out"
                );
                kill_process_group(guard.child_mut()).await;
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
                Err(_) => continue,
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
                kill_process_group(guard.child_mut()).await;
                let _ = guard.wait().await;
                stderr_handle.abort();
                return Err(Error::Timeout {
                    operation: "ffmpeg-wait",
                    duration: Duration::from_secs(WAIT_TIMEOUT_SECS),
                });
            }
        };

        // Capture only the tail of stderr to keep error payloads bounded.
        let stderr_output = match stderr_handle.await {
            Ok(Ok(output)) => {
                let text = String::from_utf8_lossy(&output.data);
                let mut lines: Vec<&str> = text.lines().collect();
                if lines.len() > 10 {
                    lines = lines.split_off(lines.len() - 10);
                }
                let mut joined = lines.join("\n");
                if output.exceeded {
                    if !joined.is_empty() {
                        joined.push('\n');
                    }
                    joined.push_str("[stderr truncated]");
                }
                joined
            }
            Ok(Err(e)) => format!("stderr read failed: {e}"),
            Err(_) => "unknown stderr error".to_string(),
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
                stderr_tail: stderr_output,
            });
        }

        trace!(
            ?stage,
            elapsed_ms = started.elapsed().as_millis() as u64,
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
    fn split_copy_args_include_segment_settings() {
        let args = split_copy_args(Path::new("input.mp4"), Path::new("seg%03d.mp4"), 11.5);
        let as_text = to_strings(&args);
        assert!(as_text.windows(2).any(|w| w == ["-f", "segment"]));
        assert!(as_text.windows(2).any(|w| w == ["-segment_time", "11.5"]));
    }
}
