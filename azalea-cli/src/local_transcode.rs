//! Local (non-Discord) transcode path.
//!
//! ## Rationale
//! `azalea_core::pipeline::optimize::optimize` is shaped around Discord's
//! upload cap: it chooses remux/transcode/split strategies to fit
//! `max_upload_bytes` (<=50MB) and always splits oversized output into
//! multiple attachment-sized parts. A local download has no such limit by
//! default, so this module writes a single file directly to its final
//! destination instead of reusing that ladder. `--discord-cap` opts back
//! into `optimize::optimize` for users who explicitly want that behavior.

use anyhow::{Context, Result};
use azalea_core::Engine;
use azalea_core::config::{HardwareAcceleration, QualityPreset, TranscodeSettings};
use azalea_core::pipeline::errors::TranscodeStage;
use azalea_core::pipeline::ffmpeg;
use azalea_core::pipeline::quality::BitrateParams;
use azalea_core::pipeline::types::{DownloadedFile, ResolvedMedia};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::fs;

use crate::args::CodecChoice;

pub struct TranscodeOptions {
    pub codec: CodecChoice,
    pub quality: QualityPreset,
    pub crf: Option<u8>,
    pub max_size_bytes: Option<u64>,
    pub hw: HardwareAcceleration,
    /// Job concurrency (`-j`); threaded into `effective_ffmpeg_threads` so
    /// per-process thread counts shrink when multiple jobs run in parallel.
    pub concurrency: u32,
}

pub struct LocalOutput {
    pub path: PathBuf,
    pub size: u64,
    pub mode: &'static str,
    pub encoder: Option<&'static str>,
}

/// Produce the final local artifact for one downloaded file.
///
/// ## Preconditions
/// - `final_path`'s parent directory already exists.
/// - `final_path`'s extension already reflects the caller's container choice
///   (this function does not decide `.mp4` vs `.mkv`).
pub async fn produce(
    downloaded: &DownloadedFile,
    resolved: &ResolvedMedia,
    engine: &Engine,
    opts: &TranscodeOptions,
    final_path: &Path,
) -> Result<LocalOutput> {
    let Some(target_codec) = opts.codec.target_codec() else {
        // A tweet from Twitter's Nov-Dec 2023 broken-container window (see
        // `resolve::twitter_container_bug_window`) must not be copied
        // byte-for-byte: that would preserve the broken container. Remux
        // instead — still no re-encode, just a fresh container.
        if resolved.needs_container_fix {
            let is_mp4 = final_path
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("mp4"));
            let threads = engine
                .config
                .transcode
                .effective_ffmpeg_threads(opts.concurrency);
            let timeout = Duration::from_secs(engine.config.transcode.ffmpeg_timeout_secs);
            let args = remux_args_for(&downloaded.path, final_path, threads, is_mp4);
            ffmpeg::execute(
                &engine.config.binaries.ffmpeg,
                &args,
                timeout,
                TranscodeStage::Transcode,
            )
            .await
            .context("container-fix remux failed")?;
            let size = fs::metadata(final_path).await?.len();
            return Ok(LocalOutput {
                path: final_path.to_path_buf(),
                size,
                mode: "remux (container fix)",
                encoder: None,
            });
        }

        fs::copy(&downloaded.path, final_path)
            .await
            .with_context(|| {
                format!(
                    "copy {} to {}",
                    downloaded.path.display(),
                    final_path.display()
                )
            })?;
        let size = fs::metadata(final_path).await?.len();
        return Ok(LocalOutput {
            path: final_path.to_path_buf(),
            size,
            mode: "copy",
            encoder: None,
        });
    };

    let base_settings = TranscodeSettings {
        quality_preset: opts.quality,
        hardware_acceleration: opts.hw,
        target_codec,
        ..engine.config.transcode.clone()
    };
    let threads = base_settings.effective_ffmpeg_threads(opts.concurrency);
    let timeout = Duration::from_secs(engine.config.transcode.ffmpeg_timeout_secs);
    let is_mp4 = final_path
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("mp4"));

    if ffmpeg::stream_copy_viable_for(downloaded.facts, target_codec) {
        let args = remux_args_for(&downloaded.path, final_path, threads, is_mp4);
        ffmpeg::execute(
            &engine.config.binaries.ffmpeg,
            &args,
            timeout,
            TranscodeStage::Transcode,
        )
        .await
        .context("remux failed")?;
        let size = fs::metadata(final_path).await?.len();
        return Ok(LocalOutput {
            path: final_path.to_path_buf(),
            size,
            mode: "remux",
            encoder: None,
        });
    }

    let _permit = engine
        .permits
        .transcode
        .acquire()
        .await
        .context("transcode semaphore closed")?;

    let outcome = if let Some(max_size_bytes) = opts.max_size_bytes {
        let duration = downloaded
            .duration
            .or(resolved.duration)
            .context("cannot target a max size: source duration is unknown")?;
        let mut sized_settings = base_settings.clone();
        sized_settings.max_upload_bytes = max_size_bytes;
        let params = BitrateParams::compute(&sized_settings, duration)
            .map_err(|error| anyhow::anyhow!("compute bitrate budget: {error}"))?;

        ffmpeg::execute_with_hwacc_fallback(
            &engine.config.binaries.ffmpeg,
            timeout,
            TranscodeStage::Transcode,
            &sized_settings,
            &engine.transcode_runtime,
            |active| {
                ffmpeg::transcode_args(
                    &downloaded.path,
                    final_path,
                    params.video_bitrate_kbps,
                    params.audio_bitrate_kbps,
                    None,
                    active,
                    opts.concurrency,
                )
            },
        )
        .await
        .map_err(|error| anyhow::anyhow!("size-targeted transcode failed: {error}"))?
    } else {
        ffmpeg::execute_with_hwacc_fallback(
            &engine.config.binaries.ffmpeg,
            timeout,
            TranscodeStage::Transcode,
            &base_settings,
            &engine.transcode_runtime,
            |active| {
                ffmpeg::crf_encode_args(
                    &downloaded.path,
                    final_path,
                    None,
                    active,
                    opts.crf,
                    threads,
                )
            },
        )
        .await
        .map_err(|error| anyhow::anyhow!("encode failed: {error}"))?
    };

    let size = fs::metadata(final_path).await?.len();
    Ok(LocalOutput {
        path: final_path.to_path_buf(),
        size,
        mode: "encode",
        encoder: Some(outcome.encoder_used),
    })
}

/// [`ffmpeg::remux_args`] hardcodes `-movflags +faststart`, an MP4/MOV-only
/// muxer option; ffmpeg rejects it for other containers (e.g. MKV). Build a
/// minimal equivalent without that flag for non-MP4 destinations.
fn remux_args_for(input: &Path, output: &Path, threads: u32, is_mp4: bool) -> ffmpeg::Args {
    if is_mp4 {
        return ffmpeg::remux_args(input, output, threads);
    }

    let mut args = ffmpeg::Args::new();
    args.push("-y".into());
    args.push("-i".into());
    args.push(input.as_os_str().into());
    args.push("-c".into());
    args.push("copy".into());
    if threads > 0 {
        args.push("-threads".into());
        args.push(threads.to_string().into());
    }
    args.push(output.as_os_str().into());
    args
}
