//! # Module overview
//! Optimization and transcoding stage with size-aware strategy ladder.
//!
//! ## Algorithm overview
//! Tries strategies from cheapest to most expensive (remux → transcode → split)
//! until the output fits the configured upload size cap.
//!
//! ## Trade-off acknowledgment
//! The strategy ladder prioritizes latency over perfect quality; see
//! [`TranscodeStrategy`] ordering for rationale.
//!
//! ## Performance hints
//! Strategy selection avoids repeated probes by reusing downloaded metadata.

use crate::concurrency::Permits;
use crate::config::{EngineSettings, QualityPreset};
use crate::media::{TempFileCleanup, TempFileGuard};
use crate::pipeline::disk::ensure_disk_space;
use crate::pipeline::errors::{Error, TranscodeStage};
use crate::pipeline::ffmpeg;
use crate::pipeline::quality::{BitrateParams, Ladder};
use crate::pipeline::types::{DownloadedFile, MediaType, PreparedUpload, Progress, ResolvedMedia};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::fs;
use tokio::sync::mpsc;
use tracing::Instrument as _;

/// Prepare media for Discord upload, choosing the cheapest viable strategy.
///
/// ## Preconditions
/// - The input file exists on disk and is owned by a temp guard.
/// - Config has been validated for upload limits.
///
/// ## Postconditions
/// - Returns [`PreparedUpload`] with temp guards for all outputs.
/// - Any intermediate artifacts are cleaned up on failure.
///
/// ## Hot-path markers
/// This function runs on the main pipeline path; keep allocations bounded.
pub async fn optimize(
    mut downloaded: DownloadedFile,
    resolved: &ResolvedMedia,
    permits: &Permits,
    temp_files: &TempFileCleanup,
    config: &EngineSettings,
    progress_tx: Option<mpsc::Sender<Progress>>,
) -> Result<PreparedUpload, Error> {
    tracing::trace!(path = %downloaded.path.display(), "Entered optimize stage");
    tracing::info!(
        size_bytes = downloaded.size,
        media_type = ?resolved.media_type,
        duration_secs = downloaded.duration.or(resolved.duration),
        resolution = ?downloaded.resolution.or(resolved.resolution),
        "Optimizing media"
    );
    validate_input_exists(&downloaded.path).await?;

    let max_upload_bytes = config.transcode.max_upload_bytes;
    tracing::trace!(max_upload_bytes, "Transcode limits");
    if downloaded.size <= max_upload_bytes {
        // Fast path: already within upload limits, so keep original bits.
        tracing::trace!(
            size_bytes = downloaded.size,
            limit_bytes = max_upload_bytes,
            "Pass-through eligible"
        );
        tracing::info!(strategy = %TranscodeStrategy::PassThrough, "Using transcode strategy");
        let dir_guard = downloaded
            ._dir_guard
            .take()
            .ok_or_else(|| Error::Io(std::io::Error::other("missing temp dir guard")))?;
        return Ok(PreparedUpload::Single {
            path: downloaded.path,
            _guard: downloaded._guard,
            _dir_guard: dir_guard,
        });
    }

    let min_space = config.pipeline.min_disk_space_bytes;
    let required = min_space.saturating_add(downloaded.size.saturating_mul(2));
    // Reserve headroom for intermediate artifacts (e.g., transcode + split).
    ensure_disk_space(&config.storage.temp_dir, required).await?;

    if resolved.media_type == MediaType::Image {
        // Image pipeline avoids video-specific strategies.
        tracing::info!(strategy = %TranscodeStrategy::ImageCompress, "Using transcode strategy");
        return optimize_image(downloaded, permits, temp_files, config).await;
    }

    let duration = downloaded.duration.or(resolved.duration).unwrap_or(60.0);
    let source_resolution = downloaded.resolution.or(resolved.resolution);
    let balanced_height = predict_resolution(
        duration,
        source_resolution,
        config,
        config.transcode.quality_preset,
    );
    let aggressive_height =
        predict_resolution(duration, source_resolution, config, QualityPreset::Size);

    tracing::trace!(
        duration_secs = duration,
        source_resolution = ?source_resolution,
        balanced_height = balanced_height,
        aggressive_height = aggressive_height,
        "Computed transcode targets"
    );

    // Ordered from cheapest to most expensive in CPU and latency.
    let mut strategies = vec![
        TranscodeStrategy::Remux,
        TranscodeStrategy::TranscodeBalanced,
    ];
    if aggressive_height != balanced_height {
        strategies.push(TranscodeStrategy::TranscodeAggressive);
    }
    strategies.push(TranscodeStrategy::SplitCopy);
    strategies.push(TranscodeStrategy::SplitTranscode);

    for strategy in strategies {
        tracing::info!(strategy = %strategy, "Using transcode strategy");
        match strategy {
            TranscodeStrategy::Remux => {
                let remux_path = remux_path(&downloaded.path);
                match remux(&downloaded.path, &remux_path, permits, config)
                    .instrument(tracing::info_span!(
                        "optimize.strategy.remux",
                        input = %downloaded.path.display(),
                        output = %remux_path.display()
                    ))
                    .await
                {
                    Ok(size) if size <= max_upload_bytes => {
                        let dir_guard = downloaded._dir_guard.take().ok_or_else(|| {
                            Error::Io(std::io::Error::other("missing temp dir guard"))
                        })?;
                        return Ok(PreparedUpload::Single {
                            path: remux_path.clone(),
                            _guard: temp_files.guard(remux_path),
                            _dir_guard: dir_guard,
                        });
                    }
                    Ok(_) => {
                        tracing::trace!(
                            strategy = %strategy,
                            output = %remux_path.display(),
                            limit_bytes = max_upload_bytes,
                            "Remux output exceeded upload limit"
                        );
                        let _ = fs::remove_file(&remux_path).await;
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "Remux strategy failed");
                        let _ = fs::remove_file(&remux_path).await;
                    }
                }
            }
            TranscodeStrategy::TranscodeBalanced => {
                let mut dir_guard = downloaded._dir_guard.take();
                let result = try_transcode(
                    &downloaded,
                    duration,
                    balanced_height,
                    permits,
                    temp_files,
                    config,
                    &mut dir_guard,
                )
                .instrument(tracing::info_span!(
                    "optimize.strategy.transcode",
                    variant = "balanced",
                    duration_secs = duration,
                    max_height = ?balanced_height
                ))
                .await?;
                if let Some(result) = result {
                    return Ok(result);
                }
                downloaded._dir_guard = dir_guard;
            }
            TranscodeStrategy::TranscodeAggressive => {
                let mut dir_guard = downloaded._dir_guard.take();
                let result = try_transcode(
                    &downloaded,
                    duration,
                    aggressive_height,
                    permits,
                    temp_files,
                    config,
                    &mut dir_guard,
                )
                .instrument(tracing::info_span!(
                    "optimize.strategy.transcode",
                    variant = "aggressive",
                    duration_secs = duration,
                    max_height = ?aggressive_height
                ))
                .await?;
                if let Some(result) = result {
                    return Ok(result);
                }
                downloaded._dir_guard = dir_guard;
            }
            TranscodeStrategy::SplitCopy => {
                let mut dir_guard = downloaded._dir_guard.take();
                let result = try_split_copy(
                    &downloaded,
                    duration,
                    permits,
                    temp_files,
                    config,
                    &mut dir_guard,
                )
                .instrument(tracing::info_span!(
                    "optimize.strategy.split_copy",
                    duration_secs = duration
                ))
                .await?;
                if let Some(result) = result {
                    return Ok(result);
                }
                downloaded._dir_guard = dir_guard;
            }
            TranscodeStrategy::SplitTranscode => {
                return split_video(
                    downloaded,
                    duration,
                    permits,
                    temp_files,
                    config,
                    progress_tx,
                )
                .instrument(tracing::info_span!(
                    "optimize.strategy.split_transcode",
                    duration_secs = duration
                ))
                .await;
            }
            TranscodeStrategy::PassThrough | TranscodeStrategy::ImageCompress => {}
        }
    }

    Err(Error::TranscodeFailed {
        stage: TranscodeStage::Transcode,
        exit_code: None,
        stderr_tail: "transcode strategy ladder exhausted".to_string(),
    })
}

/// Strategy ladder ordered by expected cost.
///
/// ## Design rationale
/// Cheaper strategies minimize CPU time and latency; fallbacks trade quality
/// for upload success.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TranscodeStrategy {
    PassThrough,
    Remux,
    TranscodeBalanced,
    TranscodeAggressive,
    SplitCopy,
    SplitTranscode,
    ImageCompress,
}

impl std::fmt::Display for TranscodeStrategy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let label = match self {
            Self::PassThrough => "pass-through",
            Self::Remux => "remux",
            Self::TranscodeBalanced => "transcode-balanced",
            Self::TranscodeAggressive => "transcode-aggressive",
            Self::SplitCopy => "split-copy",
            Self::SplitTranscode => "split-transcode",
            Self::ImageCompress => "image-compress",
        };
        write!(f, "{}", label)
    }
}

async fn optimize_image(
    mut downloaded: DownloadedFile,
    permits: &Permits,
    temp_files: &TempFileCleanup,
    config: &EngineSettings,
) -> Result<PreparedUpload, Error> {
    // Image compression is handled entirely via ffmpeg, tuned by size ratio.
    let _permit = permits
        .transcode
        .acquire()
        .await
        .map_err(|_| Error::Io(std::io::Error::other("transcode semaphore closed")))?;

    let input_ext = downloaded
        .path
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or("jpg")
        .to_ascii_lowercase();
    let output_ext = match input_ext.as_str() {
        "png" => "png",
        "webp" => "webp",
        "gif" => "gif",
        "jpg" | "jpeg" => "jpg",
        _ => "jpg",
    };
    let output_path = downloaded
        .path
        .with_extension(format!("opt.{}", output_ext));
    let size_ratio = downloaded.size as f64 / config.transcode.max_upload_bytes as f64;

    let (quality_levels, scale_factors): (Vec<u32>, Vec<f64>) = if size_ratio < 1.2 {
        (vec![85, 75], vec![1.0])
    } else if size_ratio < 2.0 {
        (vec![75, 65, 50], vec![1.0, 0.85])
    } else {
        (vec![65, 50, 35], vec![0.85, 0.65, 0.5])
    };

    for &scale in &scale_factors {
        for &quality in &quality_levels {
            tracing::trace!(quality, scale, "Attempting image compression");
            let mut args = ffmpeg::Args::new();
            args.push("-y".into());
            args.push("-i".into());
            args.push(downloaded.path.as_os_str().into());

            if scale < 1.0 {
                args.push("-vf".into());
                args.push(format!("scale=iw*{}:ih*{}", scale, scale).into());
            }

            args.push("-q:v".into());
            args.push(quality.to_string().into());
            args.push(output_path.as_os_str().into());

            if ffmpeg::execute(
                &config.binaries.ffmpeg,
                &args,
                Duration::from_secs(config.transcode.ffmpeg_timeout_secs),
                TranscodeStage::ImageCompress,
            )
            .await
            .is_ok()
            {
                if let Ok(metadata) = fs::metadata(&output_path).await
                    && metadata.len() <= config.transcode.max_upload_bytes
                {
                    tracing::info!(
                        quality,
                        scale,
                        size_bytes = metadata.len(),
                        "Image compression succeeded"
                    );
                    let dir_guard = downloaded._dir_guard.take().ok_or_else(|| {
                        Error::Io(std::io::Error::other("missing temp dir guard"))
                    })?;
                    return Ok(PreparedUpload::Single {
                        path: output_path.clone(),
                        _guard: temp_files.guard(output_path),
                        _dir_guard: dir_guard,
                    });
                }
                let _ = fs::remove_file(&output_path).await;
            }
        }
    }

    Err(Error::TranscodeFailed {
        stage: TranscodeStage::ImageCompress,
        exit_code: None,
        stderr_tail: "could not compress image".to_string(),
    })
}

async fn validate_input_exists(path: &Path) -> Result<(), Error> {
    if !tokio::fs::try_exists(path).await? {
        return Err(Error::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "input file missing",
        )));
    }
    Ok(())
}

fn predict_resolution(
    duration: f64,
    source_resolution: Option<(u32, u32)>,
    config: &EngineSettings,
    quality_preset: QualityPreset,
) -> Option<u32> {
    let Ok(params) = BitrateParams::compute(&config.transcode, duration) else {
        // Guardrail: fall back to a conservative height when bitrate calc fails.
        return Some(360);
    };

    let source_height = source_resolution.map(|(_, h)| h);
    let recommendation =
        Ladder::recommend(source_height, params.video_bitrate_kbps, quality_preset);

    recommendation.target_height
}

async fn try_transcode(
    downloaded: &DownloadedFile,
    duration: f64,
    max_height: Option<u32>,
    permits: &Permits,
    temp_files: &TempFileCleanup,
    config: &EngineSettings,
    dir_guard: &mut Option<TempFileGuard>,
) -> Result<Option<PreparedUpload>, Error> {
    let transcode_path = transcode_path(&downloaded.path);
    match transcode(
        &downloaded.path,
        &transcode_path,
        duration,
        max_height,
        permits,
        config,
    )
    .await
    {
        Ok(size) if size <= config.transcode.max_upload_bytes => {
            // Success path: keep the transcode output and attach temp guards.
            let dir_guard = dir_guard
                .take()
                .ok_or_else(|| Error::Io(std::io::Error::other("missing temp dir guard")))?;
            Ok(Some(PreparedUpload::Single {
                path: transcode_path.clone(),
                _guard: temp_files.guard(transcode_path),
                _dir_guard: dir_guard,
            }))
        }
        Ok(_) => {
            tracing::trace!(
                path = %transcode_path.display(),
                "Transcode output exceeded size limit"
            );
            let _ = fs::remove_file(&transcode_path).await;
            Ok(None)
        }
        Err(e) => {
            tracing::warn!(error = %e, "Transcode strategy failed");
            let _ = fs::remove_file(&transcode_path).await;
            Ok(None)
        }
    }
}

async fn remux(
    input: &Path,
    output: &Path,
    permits: &Permits,
    config: &EngineSettings,
) -> Result<u64, Error> {
    let _permit = permits
        .transcode
        .acquire()
        .await
        .map_err(|_| Error::Io(std::io::Error::other("transcode semaphore closed")))?;

    let args = ffmpeg::remux_args(
        input,
        output,
        config
            .transcode
            .effective_ffmpeg_threads(config.concurrency.transcode),
    );

    ffmpeg::execute(
        &config.binaries.ffmpeg,
        &args,
        Duration::from_secs(config.transcode.ffmpeg_timeout_secs),
        TranscodeStage::Remux,
    )
    .await?;

    Ok(fs::metadata(output).await?.len())
}

async fn transcode(
    input: &Path,
    output: &Path,
    duration: f64,
    max_height: Option<u32>,
    permits: &Permits,
    config: &EngineSettings,
) -> Result<u64, Error> {
    let _permit = permits
        .transcode
        .acquire()
        .await
        .map_err(|_| Error::Io(std::io::Error::other("transcode semaphore closed")))?;

    let params = BitrateParams::compute(&config.transcode, duration)?;
    let args = ffmpeg::transcode_args(
        input,
        output,
        params.video_bitrate_kbps,
        params.audio_bitrate_kbps,
        max_height,
        &config.transcode,
        config.concurrency.transcode,
    );

    ffmpeg::execute(
        &config.binaries.ffmpeg,
        &args,
        Duration::from_secs(config.transcode.ffmpeg_timeout_secs),
        TranscodeStage::Transcode,
    )
    .await?;

    Ok(fs::metadata(output).await?.len())
}

async fn split_video(
    mut downloaded: DownloadedFile,
    duration: f64,
    permits: &Permits,
    temp_files: &TempFileCleanup,
    config: &EngineSettings,
    progress_tx: Option<mpsc::Sender<Progress>>,
) -> Result<PreparedUpload, Error> {
    let dir_guard = downloaded
        ._dir_guard
        .take()
        .ok_or_else(|| Error::Io(std::io::Error::other("missing temp dir guard")))?;
    let target_segment_size =
        (config.transcode.max_upload_bytes as f64 * config.transcode.split_target_ratio) as u64;

    let segment_params = BitrateParams::compute_for_split(
        &config.transcode,
        config.transcode.max_single_video_duration_secs as f64,
    )?;

    let target_kbps = segment_params.video_bitrate_kbps + segment_params.audio_bitrate_kbps;
    let target_bps = target_kbps as f64 * 1000.0;

    // Estimate a segment duration that keeps each piece within upload size.
    let segment_duration = (target_segment_size as f64 * 8.0 / target_bps)
        .max(10.0)
        .min(config.transcode.max_single_video_duration_secs as f64);

    let num_segments = (duration / segment_duration).ceil() as u32;
    let use_parallel = num_segments >= config.pipeline.parallel_segment_threshold;

    tracing::info!(
        segment_duration_secs = segment_duration,
        num_segments,
        parallel = use_parallel,
        "Splitting media"
    );

    if use_parallel {
        // Parallel path: split quickly, then transcode segments concurrently.
        split_parallel(
            downloaded,
            segment_duration,
            &segment_params,
            permits,
            temp_files,
            config,
            progress_tx,
            dir_guard,
        )
        .await
    } else {
        split_serial(
            downloaded,
            segment_duration,
            &segment_params,
            permits,
            temp_files,
            config,
            dir_guard,
        )
        .await
    }
}

async fn split_serial(
    downloaded: DownloadedFile,
    segment_duration: f64,
    segment_params: &BitrateParams,
    permits: &Permits,
    temp_files: &TempFileCleanup,
    config: &EngineSettings,
    dir_guard: TempFileGuard,
) -> Result<PreparedUpload, Error> {
    tracing::trace!(
        segment_duration_secs = segment_duration,
        "Starting serial split"
    );
    let _permit = permits
        .transcode
        .acquire()
        .await
        .map_err(|_| Error::Io(std::io::Error::other("transcode semaphore closed")))?;

    let stem = downloaded
        .path
        .file_stem()
        .unwrap_or_default()
        .to_string_lossy();
    let output_pattern = downloaded
        .path
        .with_file_name(format!("{}_seg%03d.mp4", stem));

    let args = ffmpeg::split_args(
        &downloaded.path,
        &output_pattern,
        segment_duration,
        segment_params.video_bitrate_kbps,
        segment_params.audio_bitrate_kbps,
        &config.transcode,
        config.concurrency.transcode,
    );

    ffmpeg::execute(
        &config.binaries.ffmpeg,
        &args,
        Duration::from_secs(config.transcode.ffmpeg_timeout_secs),
        TranscodeStage::Split,
    )
    .await?;

    let prefix = format!("{}_seg", stem);
    let segments = collect_segments(&downloaded.path, &prefix).await?;

    if segments.is_empty() {
        return Err(Error::TranscodeFailed {
            stage: TranscodeStage::Split,
            exit_code: None,
            stderr_tail: "split produced no segments".to_string(),
        });
    }

    let guards = segments
        .iter()
        .map(|p| temp_files.guard(p.clone()))
        .collect();
    tracing::info!(segments = segments.len(), "Serial split completed");
    Ok(PreparedUpload::Split {
        paths: segments,
        _guards: guards,
        _dir_guard: dir_guard,
    })
}

#[allow(clippy::too_many_arguments)]
async fn split_parallel(
    downloaded: DownloadedFile,
    segment_duration: f64,
    segment_params: &BitrateParams,
    permits: &Permits,
    temp_files: &TempFileCleanup,
    config: &EngineSettings,
    progress_tx: Option<mpsc::Sender<Progress>>,
    dir_guard: TempFileGuard,
) -> Result<PreparedUpload, Error> {
    tracing::trace!(
        segment_duration_secs = segment_duration,
        "Starting parallel split"
    );
    let stem = downloaded
        .path
        .file_stem()
        .unwrap_or_default()
        .to_string_lossy();

    let raw_pattern = downloaded
        .path
        .with_file_name(format!("{}_raw%03d.mp4", stem));

    {
        // First pass: split without re-encoding for speed and determinism.
        let _permit = permits
            .transcode
            .acquire()
            .await
            .map_err(|_| Error::Io(std::io::Error::other("transcode semaphore closed")))?;

        let split_args = ffmpeg::split_copy_args(&downloaded.path, &raw_pattern, segment_duration);
        ffmpeg::execute(
            &config.binaries.ffmpeg,
            &split_args,
            Duration::from_secs(config.transcode.ffmpeg_timeout_secs),
            TranscodeStage::Split,
        )
        .await?;
    }

    let raw_prefix = format!("{}_raw", stem);
    let raw_segments = collect_segments(&downloaded.path, &raw_prefix).await?;
    if raw_segments.is_empty() {
        return Err(Error::TranscodeFailed {
            stage: TranscodeStage::Split,
            exit_code: None,
            stderr_tail: "split produced no raw segments".to_string(),
        });
    }

    tracing::info!(segments = raw_segments.len(), "Raw segments created");

    let output_segments: Vec<PathBuf> = (0..raw_segments.len())
        .map(|idx| {
            downloaded
                .path
                .with_file_name(format!("{}_seg{:03}.mp4", stem, idx))
        })
        .collect();

    let total_segments = raw_segments.len();
    if let Some(tx) = &progress_tx {
        let _ = tx.send(Progress::Transcoding(0, total_segments)).await;
    }

    let mut join_set = tokio::task::JoinSet::new();

    let ffmpeg_path = config.binaries.ffmpeg.clone();
    for (idx, raw_path) in raw_segments.iter().enumerate() {
        let out_path = output_segments.get(idx).cloned().unwrap_or_else(|| {
            downloaded
                .path
                .with_file_name(format!("{}_seg{:03}.mp4", stem, idx))
        });

        let args = ffmpeg::transcode_args(
            raw_path,
            &out_path,
            segment_params.video_bitrate_kbps,
            segment_params.audio_bitrate_kbps,
            None,
            &config.transcode,
            config.concurrency.transcode,
        );

        let permit = Arc::clone(&permits.transcode);
        let timeout = Duration::from_secs(config.transcode.ffmpeg_timeout_secs);
        let raw_path_clone = raw_path.clone();

        let ffmpeg_path = ffmpeg_path.clone();
        join_set.spawn(async move {
            // Segment transcodes run independently under the shared permit pool.
            let _permit = permit
                .acquire()
                .await
                .map_err(|_| Error::Io(std::io::Error::other("transcode semaphore closed")))?;
            let result = ffmpeg::execute(&ffmpeg_path, &args, timeout, TranscodeStage::Split).await;
            let _ = fs::remove_file(&raw_path_clone).await;
            result.map(|_| out_path)
        });
    }

    let mut final_segments = Vec::with_capacity(total_segments);
    let mut finished = 0;

    while let Some(res) = join_set.join_next().await {
        match res {
            Ok(Ok(path)) => {
                final_segments.push(path);
                finished += 1;
                if let Some(tx) = &progress_tx {
                    let _ = tx
                        .send(Progress::Transcoding(finished, total_segments))
                        .await;
                }
            }
            Ok(Err(e)) => {
                join_set.abort_all();
                while join_set.join_next().await.is_some() {}
                cleanup_segments(&raw_segments).await;
                cleanup_segments(&output_segments).await;
                return Err(e);
            }
            Err(e) => {
                join_set.abort_all();
                while join_set.join_next().await.is_some() {}
                cleanup_segments(&raw_segments).await;
                cleanup_segments(&output_segments).await;
                return Err(Error::TranscodeFailed {
                    stage: TranscodeStage::Split,
                    exit_code: None,
                    stderr_tail: format!("segment task panicked: {}", e),
                });
            }
        }
    }

    final_segments.sort();
    let guards = final_segments
        .iter()
        .map(|p| temp_files.guard(p.clone()))
        .collect();

    tracing::info!(segments = final_segments.len(), "Parallel split completed");
    Ok(PreparedUpload::Split {
        paths: final_segments,
        _guards: guards,
        _dir_guard: dir_guard,
    })
}

async fn try_split_copy(
    downloaded: &DownloadedFile,
    duration: f64,
    permits: &Permits,
    temp_files: &TempFileCleanup,
    config: &EngineSettings,
    dir_guard: &mut Option<TempFileGuard>,
) -> Result<Option<PreparedUpload>, Error> {
    let Some(segment_duration) = copy_segment_duration(downloaded, duration, config) else {
        return Ok(None);
    };

    tracing::trace!(
        segment_duration_secs = segment_duration,
        "Trying split-copy strategy"
    );

    let stem = downloaded
        .path
        .file_stem()
        .unwrap_or_default()
        .to_string_lossy();
    let prefix = format!("{}_copy", stem);
    let output_pattern = downloaded
        .path
        .with_file_name(format!("{}%03d.mp4", prefix));

    let args = ffmpeg::split_copy_args(&downloaded.path, &output_pattern, segment_duration);
    let _permit = permits
        .transcode
        .acquire()
        .await
        .map_err(|_| Error::Io(std::io::Error::other("transcode semaphore closed")))?;

    if ffmpeg::execute(
        &config.binaries.ffmpeg,
        &args,
        Duration::from_secs(config.transcode.ffmpeg_timeout_secs),
        TranscodeStage::Split,
    )
    .await
    .is_err()
    {
        return Ok(None);
    }

    let segments = collect_segments(&downloaded.path, &prefix).await?;
    if segments.is_empty() {
        return Ok(None);
    }

    if !segments_fit_limit(&segments, config).await? {
        cleanup_segments(&segments).await;
        return Ok(None);
    }

    let guards = segments
        .iter()
        .map(|p| temp_files.guard(p.clone()))
        .collect();

    let dir_guard = dir_guard
        .take()
        .ok_or_else(|| Error::Io(std::io::Error::other("missing temp dir guard")))?;
    Ok(Some(PreparedUpload::Split {
        paths: segments,
        _guards: guards,
        _dir_guard: dir_guard,
    }))
}

fn copy_segment_duration(
    downloaded: &DownloadedFile,
    duration: f64,
    config: &EngineSettings,
) -> Option<f64> {
    if duration <= 0.0 {
        return None;
    }

    // Estimate bitrate from source size to size copy-split segments.
    let bytes_per_sec = downloaded.size as f64 / duration;
    if bytes_per_sec <= 0.0 {
        return None;
    }

    let target_segment_size =
        (config.transcode.max_upload_bytes as f64 * config.transcode.split_target_ratio).max(1.0);
    let mut segment_duration = target_segment_size / bytes_per_sec;

    if !segment_duration.is_finite() || segment_duration <= 0.5 {
        return None;
    }

    let max_segment = config.transcode.max_single_video_duration_secs as f64;
    if segment_duration > max_segment {
        segment_duration = max_segment;
    }

    if segment_duration >= duration {
        return None;
    }

    Some(segment_duration)
}

async fn collect_segments(path: &Path, prefix: &str) -> Result<Vec<PathBuf>, Error> {
    let parent_dir = path.parent().unwrap_or(Path::new("."));
    let mut segments = Vec::new();

    // Scan the temp directory for segment files matching the prefix.
    let mut entries = fs::read_dir(parent_dir).await?;
    while let Some(entry) = entries.next_entry().await? {
        let path = entry.path();
        if let Some(name) = path.file_name().and_then(|n| n.to_str())
            && name.starts_with(prefix)
            && name.ends_with(".mp4")
        {
            let size = fs::metadata(&path).await.map(|m| m.len()).unwrap_or(0);
            if size > 0 {
                segments.push(path);
            }
        }
    }

    segments.sort();
    Ok(segments)
}

async fn segments_fit_limit(segments: &[PathBuf], config: &EngineSettings) -> Result<bool, Error> {
    for path in segments {
        let size = fs::metadata(path).await?.len();
        if size > config.transcode.max_upload_bytes {
            return Ok(false);
        }
    }
    Ok(true)
}

async fn cleanup_segments(segments: &[PathBuf]) {
    for path in segments {
        let _ = fs::remove_file(path).await;
    }
}

fn remux_path(input: &Path) -> PathBuf {
    let stem = input.file_stem().unwrap_or_default().to_string_lossy();
    input.with_file_name(format!("{}_remux.mp4", stem))
}

fn transcode_path(input: &Path) -> PathBuf {
    let stem = input.file_stem().unwrap_or_default().to_string_lossy();
    input.with_file_name(format!("{}_transcode.mp4", stem))
}
