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
use crate::config::{EngineSettings, HardwareAcceleration, QualityPreset, TranscodeSettings};
use crate::engine::TranscodeRuntime;
use crate::media::{TempFileCleanup, TempFileGuard};
use crate::pipeline::disk::ensure_disk_space;
use crate::pipeline::errors::{Error, TranscodeStage};
use crate::pipeline::ffmpeg;
use crate::pipeline::quality::{BitrateParams, Ladder, SplitTranscodePlan};
use crate::pipeline::types::{
    DownloadedFile, MediaType, PreparedPart, PreparedUpload, Progress, ResolvedMedia,
};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::fs;
use tokio::sync::mpsc;
use tokio::sync::mpsc::error::TrySendError;
use tracing::Instrument as _;

const MIN_PLAUSIBLE_SPLIT_COPY_SEGMENT_SECS: f64 = 10.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TranscodeOutcome {
    encoder_used: &'static str,
    backend_used: HardwareAcceleration,
    used_hardware: bool,
    fallback_occurred: bool,
    duration_ms: u64,
}

impl TranscodeOutcome {
    fn new(backend_used: HardwareAcceleration, fallback_occurred: bool, duration_ms: u64) -> Self {
        Self {
            encoder_used: backend_used.encoder(),
            backend_used,
            used_hardware: backend_used.is_hardware(),
            fallback_occurred,
            duration_ms,
        }
    }
}

struct TranscodeContext<'a> {
    permits: &'a Permits,
    config: &'a EngineSettings,
    runtime: &'a TranscodeRuntime,
}

#[derive(Debug)]
struct ParallelSegmentTranscodeContext {
    ffmpeg_path: PathBuf,
    transcode_settings: TranscodeSettings,
    transcode_runtime: TranscodeRuntime,
}

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
    transcode_runtime: &TranscodeRuntime,
    progress_tx: Option<mpsc::Sender<Progress>>,
) -> Result<PreparedUpload, Error> {
    let transcode = TranscodeContext {
        permits,
        config,
        runtime: transcode_runtime,
    };

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
        tracing::info!(
            mode = "pass-through",
            size_bytes = downloaded.size,
            limit_bytes = max_upload_bytes,
            "Upload ready without re-encode"
        );
        let dir_guard = downloaded
            ._dir_guard
            .take()
            .ok_or_else(|| Error::Io(std::io::Error::other("missing temp dir guard")))?;
        return Ok(PreparedUpload::single(
            PreparedPart::new(downloaded.path, downloaded.size, downloaded._guard),
            dir_guard,
        ));
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

    let strategy_plan = build_strategy_plan(
        &downloaded,
        duration,
        balanced_height,
        aggressive_height,
        config,
    );
    let split_copy_plan = strategy_plan.split_copy;

    for strategy in strategy_plan.strategies.into_iter().flatten() {
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
                        tracing::info!(
                            mode = "remux",
                            size_bytes = size,
                            limit_bytes = max_upload_bytes,
                            "Remux completed without re-encode"
                        );
                        let dir_guard = downloaded._dir_guard.take().ok_or_else(|| {
                            Error::Io(std::io::Error::other("missing temp dir guard"))
                        })?;
                        let guard = temp_files.guard(remux_path.clone());
                        return Ok(PreparedUpload::single(
                            PreparedPart::new(remux_path, size, guard),
                            dir_guard,
                        ));
                    }
                    Ok(size) => {
                        tracing::info!(
                            mode = "remux",
                            size_bytes = size,
                            limit_bytes = max_upload_bytes,
                            "Remux completed but still exceeded upload limit"
                        );
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
                    temp_files,
                    &transcode,
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
                    temp_files,
                    &transcode,
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
                let split_copy_plan = split_copy_plan
                    .ok_or_else(|| Error::Io(std::io::Error::other("missing split-copy plan")))?;
                let result = try_split_copy(
                    &downloaded,
                    split_copy_plan,
                    permits,
                    temp_files,
                    config,
                    &mut dir_guard,
                )
                .instrument(tracing::info_span!(
                    "optimize.strategy.split_copy",
                    duration_secs = duration,
                    segment_duration_secs = split_copy_plan.segment_duration,
                    estimated_segments = split_copy_plan.estimated_segments
                ))
                .await?;
                if let Some(result) = result {
                    return Ok(result);
                }
                downloaded._dir_guard = dir_guard;
            }
            TranscodeStrategy::SplitTranscode => {
                let split_transcode_plan = strategy_plan.split_transcode.ok_or_else(|| {
                    Error::Io(std::io::Error::other("missing split-transcode plan"))
                })?;
                return split_video(
                    downloaded,
                    split_transcode_plan,
                    temp_files,
                    &transcode,
                    progress_tx,
                )
                .instrument(tracing::info_span!(
                    "optimize.strategy.split_transcode",
                    duration_secs = duration,
                    segment_duration_secs = split_transcode_plan.segment_duration,
                    estimated_segments = split_transcode_plan.estimated_segments
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

const STRATEGY_PLAN_CAPACITY: usize = 6;

#[derive(Debug, Clone)]
struct StrategyPlan {
    strategies: [Option<TranscodeStrategy>; STRATEGY_PLAN_CAPACITY],
    split_copy: Option<SplitCopyPlan>,
    split_transcode: Option<SplitTranscodePlan>,
}

#[derive(Debug, Clone, Copy)]
struct SplitCopyPlan {
    segment_duration: f64,
    estimated_segments: u32,
}

fn build_strategy_plan(
    downloaded: &DownloadedFile,
    duration: f64,
    balanced_height: Option<u32>,
    aggressive_height: Option<u32>,
    config: &EngineSettings,
) -> StrategyPlan {
    let remux_viable = remux_is_plausible(downloaded);
    let single_transcode_viable = single_transcode_is_plausible(duration, config);
    let split_copy = split_copy_is_plausible(downloaded, duration, config);
    let split_transcode = SplitTranscodePlan::compute(&config.transcode, duration).ok();

    let remux = if remux_viable {
        Some(TranscodeStrategy::Remux)
    } else {
        tracing::trace!(
            container = ?downloaded.facts.container,
            video_codec = ?downloaded.facts.video_codec,
            audio_codec = ?downloaded.facts.audio_codec,
            "Skipping remux preflight"
        );
        None
    };

    let (transcode_balanced, transcode_aggressive) = if single_transcode_viable {
        (
            Some(TranscodeStrategy::TranscodeBalanced),
            (aggressive_height != balanced_height)
                .then_some(TranscodeStrategy::TranscodeAggressive),
        )
    } else {
        tracing::trace!(
            duration_secs = duration,
            "Skipping full-file transcode preflight"
        );
        (None, None)
    };

    let split_copy_strategy = if let Some(plan) = split_copy {
        tracing::trace!(
            segment_duration_secs = plan.segment_duration,
            estimated_segments = plan.estimated_segments,
            "Split-copy preflight succeeded"
        );
        Some(TranscodeStrategy::SplitCopy)
    } else {
        tracing::trace!(
            bitrate_kbps = downloaded.facts.bitrate_kbps,
            duration_secs = duration,
            "Skipping split-copy preflight"
        );
        None
    };

    let split_transcode_strategy = if let Some(plan) = split_transcode {
        tracing::trace!(
            segment_duration_secs = plan.segment_duration,
            estimated_segments = plan.estimated_segments,
            video_bitrate_kbps = plan.bitrate.video_bitrate_kbps,
            audio_bitrate_kbps = plan.bitrate.audio_bitrate_kbps,
            "Split-transcode plan computed"
        );
        Some(TranscodeStrategy::SplitTranscode)
    } else {
        tracing::trace!(
            duration_secs = duration,
            "Skipping split-transcode preflight"
        );
        None
    };

    let strategy_candidates = [
        remux,
        transcode_balanced,
        transcode_aggressive,
        split_copy_strategy,
        split_transcode_strategy,
        None,
    ];
    let mut strategies = [None; STRATEGY_PLAN_CAPACITY];
    for (slot, strategy) in strategies
        .iter_mut()
        .zip(strategy_candidates.into_iter().flatten())
    {
        *slot = Some(strategy);
    }

    StrategyPlan {
        strategies,
        split_copy,
        split_transcode,
    }
}

fn remux_is_plausible(downloaded: &DownloadedFile) -> bool {
    ffmpeg::mp4_stream_copy_viable(downloaded.facts) && !downloaded.facts.container.is_mp4_family()
}

fn single_transcode_is_plausible(duration: f64, config: &EngineSettings) -> bool {
    match BitrateParams::compute(&config.transcode, duration) {
        Ok(params) => params.video_bitrate_kbps >= config.transcode.min_bitrate_kbps,
        Err(_) => false,
    }
}

fn split_copy_is_plausible(
    downloaded: &DownloadedFile,
    duration: f64,
    config: &EngineSettings,
) -> Option<SplitCopyPlan> {
    if !ffmpeg::mp4_stream_copy_viable(downloaded.facts) {
        return None;
    }

    copy_segment_plan(downloaded, duration, config)
}

fn split_parallel_is_plausible(
    downloaded: &DownloadedFile,
    num_segments: u32,
    config: &EngineSettings,
) -> bool {
    ffmpeg::mp4_stream_copy_viable(downloaded.facts)
        && config.concurrency.transcode > 1
        && num_segments >= config.pipeline.parallel_segment_threshold
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
                    let guard = temp_files.guard(output_path.clone());
                    return Ok(PreparedUpload::single(
                        PreparedPart::new(output_path, metadata.len(), guard),
                        dir_guard,
                    ));
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
    temp_files: &TempFileCleanup,
    ctx: &TranscodeContext<'_>,
    dir_guard: &mut Option<TempFileGuard>,
) -> Result<Option<PreparedUpload>, Error> {
    let transcode_path = transcode_path(&downloaded.path);
    match transcode(&downloaded.path, &transcode_path, duration, max_height, ctx).await {
        Ok(size) if size <= ctx.config.transcode.max_upload_bytes => {
            tracing::info!(
                mode = "transcode",
                size_bytes = size,
                limit_bytes = ctx.config.transcode.max_upload_bytes,
                max_height,
                "Transcode completed and fit upload limit"
            );
            // Success path: keep the transcode output and attach temp guards.
            let dir_guard = dir_guard
                .take()
                .ok_or_else(|| Error::Io(std::io::Error::other("missing temp dir guard")))?;
            let guard = temp_files.guard(transcode_path.clone());
            Ok(Some(PreparedUpload::single(
                PreparedPart::new(transcode_path, size, guard),
                dir_guard,
            )))
        }
        Ok(size) => {
            tracing::info!(
                mode = "transcode",
                size_bytes = size,
                limit_bytes = ctx.config.transcode.max_upload_bytes,
                max_height,
                "Transcode completed but still exceeded upload limit"
            );
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
    ctx: &TranscodeContext<'_>,
) -> Result<u64, Error> {
    let _permit = ctx
        .permits
        .transcode
        .acquire()
        .await
        .map_err(|_| Error::Io(std::io::Error::other("transcode semaphore closed")))?;

    let params = BitrateParams::compute(&ctx.config.transcode, duration)?;
    execute_with_hwacc_fallback(
        &ctx.config.binaries.ffmpeg,
        Duration::from_secs(ctx.config.transcode.ffmpeg_timeout_secs),
        TranscodeStage::Transcode,
        &ctx.config.transcode,
        ctx.runtime,
        |active_settings| {
            ffmpeg::transcode_args(
                input,
                output,
                params.video_bitrate_kbps,
                params.audio_bitrate_kbps,
                max_height,
                active_settings,
                ctx.config.concurrency.transcode,
            )
        },
    )
    .await?;

    Ok(fs::metadata(output).await?.len())
}

async fn split_video(
    mut downloaded: DownloadedFile,
    plan: SplitTranscodePlan,
    temp_files: &TempFileCleanup,
    ctx: &TranscodeContext<'_>,
    progress_tx: Option<mpsc::Sender<Progress>>,
) -> Result<PreparedUpload, Error> {
    let dir_guard = downloaded
        ._dir_guard
        .take()
        .ok_or_else(|| Error::Io(std::io::Error::other("missing temp dir guard")))?;
    let use_parallel =
        split_parallel_is_plausible(&downloaded, plan.estimated_segments, ctx.config);

    tracing::info!(
        segment_duration_secs = plan.segment_duration,
        num_segments = plan.estimated_segments,
        video_bitrate_kbps = plan.bitrate.video_bitrate_kbps,
        audio_bitrate_kbps = plan.bitrate.audio_bitrate_kbps,
        raw_split_copy_viable = ffmpeg::mp4_stream_copy_viable(downloaded.facts),
        transcode_concurrency = ctx.config.concurrency.transcode,
        parallel = use_parallel,
        "Splitting media"
    );

    if use_parallel {
        // Parallel path: split quickly, then transcode segments concurrently.
        split_parallel(
            downloaded,
            plan.segment_duration,
            &plan.bitrate,
            temp_files,
            ctx,
            progress_tx,
            dir_guard,
        )
        .await
    } else {
        split_serial(
            downloaded,
            plan.segment_duration,
            &plan.bitrate,
            temp_files,
            ctx,
            SplitTranscodeProgress::new(progress_tx, plan.estimated_segments as usize),
            dir_guard,
        )
        .await
    }
}

fn send_progress_best_effort(progress_tx: &Option<mpsc::Sender<Progress>>, stage: Progress) {
    let Some(tx) = progress_tx.as_ref() else {
        return;
    };

    match tx.try_send(stage.clone()) {
        Ok(()) => {}
        Err(TrySendError::Closed(_)) => {
            tracing::debug!(stage = %stage, "Progress channel closed before stage update");
        }
        Err(TrySendError::Full(stage)) if stage.is_terminal_transcoding() => {
            let tx = tx.clone();
            tokio::spawn(async move {
                if tx.send(stage).await.is_err() {
                    tracing::debug!("Progress channel closed before terminal transcode update");
                }
            });
        }
        Err(TrySendError::Full(stage)) => {
            tracing::debug!(stage = %stage, "Progress channel full; dropping stage update");
        }
    }
}

#[derive(Clone)]
struct SplitTranscodeProgress {
    total_segments: usize,
    progress_tx: Option<mpsc::Sender<Progress>>,
}

impl SplitTranscodeProgress {
    fn new(progress_tx: Option<mpsc::Sender<Progress>>, total_segments: usize) -> Self {
        Self {
            total_segments: total_segments.max(1),
            progress_tx,
        }
    }

    fn start(&self) {
        send_progress_best_effort(
            &self.progress_tx,
            Progress::Transcoding(0, self.total_segments),
        );
    }

    fn advance(&self, done: usize) {
        send_progress_best_effort(
            &self.progress_tx,
            Progress::Transcoding(done, self.total_segments),
        );
    }

    fn finish(&self) {
        self.advance(self.total_segments);
    }
}

async fn split_serial(
    downloaded: DownloadedFile,
    segment_duration: f64,
    segment_params: &BitrateParams,
    temp_files: &TempFileCleanup,
    ctx: &TranscodeContext<'_>,
    progress: SplitTranscodeProgress,
    dir_guard: TempFileGuard,
) -> Result<PreparedUpload, Error> {
    tracing::trace!(
        segment_duration_secs = segment_duration,
        "Starting serial split"
    );
    let _permit = ctx
        .permits
        .transcode
        .acquire()
        .await
        .map_err(|_| Error::Io(std::io::Error::other("transcode semaphore closed")))?;

    progress.start();

    let stem = downloaded
        .path
        .file_stem()
        .unwrap_or_default()
        .to_string_lossy();
    let output_pattern = downloaded
        .path
        .with_file_name(format!("{}_seg%03d.mp4", stem));

    execute_with_hwacc_fallback(
        &ctx.config.binaries.ffmpeg,
        Duration::from_secs(ctx.config.transcode.ffmpeg_timeout_secs),
        TranscodeStage::Split,
        &ctx.config.transcode,
        ctx.runtime,
        |active_settings| {
            ffmpeg::split_args(
                &downloaded.path,
                &output_pattern,
                segment_duration,
                segment_params.video_bitrate_kbps,
                segment_params.audio_bitrate_kbps,
                active_settings,
                ctx.config.concurrency.transcode,
            )
        },
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

    let segments = ensure_split_segments_fit_limit(segments, ctx.config).await?;
    let segments_len = segments.len();
    progress.finish();
    let parts = into_prepared_parts(segments, temp_files);
    tracing::info!(segments = segments_len, "Serial split completed");
    Ok(PreparedUpload::split(parts, dir_guard))
}

#[allow(clippy::too_many_arguments)]
async fn split_parallel(
    downloaded: DownloadedFile,
    segment_duration: f64,
    segment_params: &BitrateParams,
    temp_files: &TempFileCleanup,
    ctx: &TranscodeContext<'_>,
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
        let _permit = ctx
            .permits
            .transcode
            .acquire()
            .await
            .map_err(|_| Error::Io(std::io::Error::other("transcode semaphore closed")))?;

        let split_args = ffmpeg::split_copy_args(&downloaded.path, &raw_pattern, segment_duration);
        ffmpeg::execute(
            &ctx.config.binaries.ffmpeg,
            &split_args,
            Duration::from_secs(ctx.config.transcode.ffmpeg_timeout_secs),
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

    let total_segments = raw_segments.len();
    tracing::info!(segments = total_segments, "Raw segments created");

    let output_prefix = format!("{}_seg", stem);
    send_progress_best_effort(&progress_tx, Progress::Transcoding(0, total_segments));

    let mut join_set = tokio::task::JoinSet::new();
    let mut pending_segments: VecDeque<_> = raw_segments.into_iter().enumerate().collect();

    let parallel_transcode = Arc::new(ParallelSegmentTranscodeContext {
        ffmpeg_path: ctx.config.binaries.ffmpeg.clone(),
        transcode_settings: ctx.config.transcode.clone(),
        transcode_runtime: ctx.runtime.clone(),
    });
    let transcode_concurrency = ctx.config.concurrency.transcode;
    let segment_video_kbps = segment_params.video_bitrate_kbps;
    let segment_audio_kbps = segment_params.audio_bitrate_kbps;

    let mut final_segments = vec![None; total_segments];
    let mut finished = 0;

    spawn_parallel_segment_tasks(
        &mut join_set,
        &mut pending_segments,
        &stem,
        Arc::clone(&ctx.permits.transcode),
        Duration::from_secs(ctx.config.transcode.ffmpeg_timeout_secs),
        &parallel_transcode,
        transcode_concurrency,
        segment_video_kbps,
        segment_audio_kbps,
    )
    .await?;

    while let Some(res) = join_set.join_next().await {
        match res {
            Ok(Ok((index, segment))) => {
                let slot = final_segments
                    .get_mut(index)
                    .ok_or_else(|| Error::TranscodeFailed {
                        stage: TranscodeStage::Split,
                        exit_code: None,
                        stderr_tail: "parallel split reported an out-of-range segment".to_string(),
                    })?;
                *slot = Some(segment);
                finished += 1;
                send_progress_best_effort(
                    &progress_tx,
                    Progress::Transcoding(finished, total_segments),
                );
                spawn_parallel_segment_tasks(
                    &mut join_set,
                    &mut pending_segments,
                    &stem,
                    Arc::clone(&ctx.permits.transcode),
                    Duration::from_secs(ctx.config.transcode.ffmpeg_timeout_secs),
                    &parallel_transcode,
                    transcode_concurrency,
                    segment_video_kbps,
                    segment_audio_kbps,
                )
                .await?;
            }
            Ok(Err(e)) => {
                join_set.abort_all();
                while join_set.join_next().await.is_some() {}
                cleanup_segment_prefix(&downloaded.path, &raw_prefix).await;
                cleanup_segment_prefix(&downloaded.path, &output_prefix).await;
                return Err(e);
            }
            Err(e) => {
                join_set.abort_all();
                while join_set.join_next().await.is_some() {}
                cleanup_segment_prefix(&downloaded.path, &raw_prefix).await;
                cleanup_segment_prefix(&downloaded.path, &output_prefix).await;
                return Err(Error::TranscodeFailed {
                    stage: TranscodeStage::Split,
                    exit_code: None,
                    stderr_tail: format!("segment task panicked: {}", e),
                });
            }
        }
    }

    let final_segments = final_segments
        .into_iter()
        .map(|segment| {
            segment.ok_or_else(|| Error::TranscodeFailed {
                stage: TranscodeStage::Split,
                exit_code: None,
                stderr_tail: "parallel split finished with missing segment".to_string(),
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    let final_segments = ensure_split_segments_fit_limit(final_segments, ctx.config).await?;
    let parts = into_prepared_parts(final_segments, temp_files);

    tracing::info!(segments = parts.len(), "Parallel split completed");
    Ok(PreparedUpload::split(parts, dir_guard))
}

#[allow(clippy::too_many_arguments)]
async fn spawn_parallel_segment_tasks(
    join_set: &mut tokio::task::JoinSet<Result<(usize, SegmentOutput), Error>>,
    pending_segments: &mut VecDeque<(usize, SegmentOutput)>,
    stem: &str,
    permit: Arc<tokio::sync::Semaphore>,
    timeout: Duration,
    parallel_transcode: &Arc<ParallelSegmentTranscodeContext>,
    transcode_concurrency: u32,
    segment_video_kbps: u32,
    segment_audio_kbps: u32,
) -> Result<(), Error> {
    while let Ok(permit) = Arc::clone(&permit).try_acquire_owned() {
        let Some((idx, raw_segment)) = pending_segments.pop_front() else {
            return Ok(());
        };
        spawn_parallel_segment_task(
            join_set,
            idx,
            raw_segment,
            stem,
            permit,
            timeout,
            Arc::clone(parallel_transcode),
            transcode_concurrency,
            segment_video_kbps,
            segment_audio_kbps,
        );
    }

    if join_set.is_empty()
        && let Some((idx, raw_segment)) = pending_segments.pop_front()
    {
        let permit = Arc::clone(&permit)
            .acquire_owned()
            .await
            .map_err(|_| Error::Io(std::io::Error::other("transcode semaphore closed")))?;
        spawn_parallel_segment_task(
            join_set,
            idx,
            raw_segment,
            stem,
            permit,
            timeout,
            Arc::clone(parallel_transcode),
            transcode_concurrency,
            segment_video_kbps,
            segment_audio_kbps,
        );
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn spawn_parallel_segment_task(
    join_set: &mut tokio::task::JoinSet<Result<(usize, SegmentOutput), Error>>,
    idx: usize,
    raw_segment: SegmentOutput,
    stem: &str,
    permit: tokio::sync::OwnedSemaphorePermit,
    timeout: Duration,
    parallel_transcode: Arc<ParallelSegmentTranscodeContext>,
    transcode_concurrency: u32,
    segment_video_kbps: u32,
    segment_audio_kbps: u32,
) {
    let input_path = raw_segment.path;
    let output_path = input_path.with_file_name(format!("{}_seg{:03}.mp4", stem, idx));
    join_set.spawn(async move {
        let result = execute_with_hwacc_fallback(
            &parallel_transcode.ffmpeg_path,
            timeout,
            TranscodeStage::Split,
            &parallel_transcode.transcode_settings,
            &parallel_transcode.transcode_runtime,
            |active_settings| {
                ffmpeg::transcode_args(
                    &input_path,
                    &output_path,
                    segment_video_kbps,
                    segment_audio_kbps,
                    None,
                    active_settings,
                    transcode_concurrency,
                )
            },
        )
        .await;
        drop(permit);
        let _ = fs::remove_file(&input_path).await;
        result?;
        let size = fs::metadata(&output_path).await?.len();
        Ok((
            idx,
            SegmentOutput {
                path: output_path,
                size,
            },
        ))
    });
}

async fn execute_with_hwacc_fallback<F>(
    ffmpeg_path: &Path,
    timeout: Duration,
    stage: TranscodeStage,
    base_settings: &TranscodeSettings,
    transcode_runtime: &TranscodeRuntime,
    build_args: F,
) -> Result<TranscodeOutcome, Error>
where
    F: Fn(&TranscodeSettings) -> ffmpeg::Args,
{
    let started_at = Instant::now();
    let active_settings = transcode_runtime.effective_settings(base_settings);
    let args = build_args(&active_settings);

    match ffmpeg::execute(ffmpeg_path, &args, timeout, stage).await {
        Ok(()) => {
            let outcome = TranscodeOutcome::new(
                active_settings.hardware_acceleration,
                false,
                elapsed_ms(started_at),
            );
            record_transcode_outcome(transcode_runtime, stage, outcome);
            Ok(outcome)
        }
        Err(error) if should_retry_with_software(&error, active_settings.hardware_acceleration) => {
            let latched = transcode_runtime.activate_software_fallback();
            let retry_settings = transcode_runtime.effective_settings(base_settings);

            tracing::warn!(
                ?stage,
                error = %error,
                configured_backend = %transcode_runtime.configured_backend(),
                failed_backend = %active_settings.hardware_acceleration,
                fallback_encoder = retry_settings.hardware_acceleration.encoder(),
                latched,
                "Hardware encoder failed; retrying with software encoder"
            );

            let retry_args = build_args(&retry_settings);
            ffmpeg::execute(ffmpeg_path, &retry_args, timeout, stage)
                .await
                .map(|()| {
                    let outcome = TranscodeOutcome::new(
                        retry_settings.hardware_acceleration,
                        true,
                        elapsed_ms(started_at),
                    );
                    record_transcode_outcome(transcode_runtime, stage, outcome);
                    outcome
                })
        }
        Err(error) => Err(error),
    }
}

fn elapsed_ms(started_at: Instant) -> u64 {
    started_at.elapsed().as_millis() as u64
}

fn record_transcode_outcome(
    transcode_runtime: &TranscodeRuntime,
    stage: TranscodeStage,
    outcome: TranscodeOutcome,
) {
    if outcome.used_hardware {
        transcode_runtime.record_hw_encode(outcome.duration_ms);
    } else {
        transcode_runtime.record_sw_encode(outcome.duration_ms);
    }

    tracing::info!(
        ?stage,
        encoder = outcome.encoder_used,
        backend = %outcome.backend_used,
        used_hardware = outcome.used_hardware,
        fallback_occurred = outcome.fallback_occurred,
        duration_ms = outcome.duration_ms,
        "Hardware acceleration diagnostics"
    );
}

fn should_retry_with_software(error: &Error, backend: HardwareAcceleration) -> bool {
    if !backend.is_hardware() {
        return false;
    }

    match error {
        Error::TranscodeFailed { stderr_tail, .. } => backend.matches_failure_output(stderr_tail),
        _ => false,
    }
}

async fn try_split_copy(
    downloaded: &DownloadedFile,
    plan: SplitCopyPlan,
    permits: &Permits,
    temp_files: &TempFileCleanup,
    config: &EngineSettings,
    dir_guard: &mut Option<TempFileGuard>,
) -> Result<Option<PreparedUpload>, Error> {
    tracing::trace!(
        segment_duration_secs = plan.segment_duration,
        estimated_segments = plan.estimated_segments,
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

    let args = ffmpeg::split_copy_args(&downloaded.path, &output_pattern, plan.segment_duration);
    let _permit = permits
        .transcode
        .acquire()
        .await
        .map_err(|_| Error::Io(std::io::Error::other("transcode semaphore closed")))?;

    if let Err(error) = ffmpeg::execute(
        &config.binaries.ffmpeg,
        &args,
        Duration::from_secs(config.transcode.ffmpeg_timeout_secs),
        TranscodeStage::Split,
    )
    .await
    {
        abandon_split_copy(&downloaded.path, &prefix, format!("ffmpeg failed: {error}")).await;
        return Ok(None);
    }

    let segments = match collect_segments(&downloaded.path, &prefix).await {
        Ok(segments) => segments,
        Err(error) => {
            let cleaned_outputs = cleanup_segment_prefix(&downloaded.path, &prefix).await;
            tracing::warn!(
                error = %error,
                cleaned_outputs,
                prefix = %prefix,
                "Split-copy strategy scan failed after ffmpeg completed"
            );
            return Err(error);
        }
    };
    if segments.is_empty() {
        abandon_split_copy(
            &downloaded.path,
            &prefix,
            "produced no usable segments".to_string(),
        )
        .await;
        return Ok(None);
    }

    if !segments_fit_limit(&segments, config) {
        abandon_split_copy(
            &downloaded.path,
            &prefix,
            format!(
                "produced {} segment(s) exceeding the {} byte upload limit",
                segments.len(),
                config.transcode.max_upload_bytes
            ),
        )
        .await;
        return Ok(None);
    }

    let dir_guard = match dir_guard.take() {
        Some(dir_guard) => dir_guard,
        None => {
            cleanup_segment_outputs(&segments).await;
            return Err(Error::Io(std::io::Error::other("missing temp dir guard")));
        }
    };
    let parts = into_prepared_parts(segments, temp_files);
    Ok(Some(PreparedUpload::split(parts, dir_guard)))
}

async fn abandon_split_copy(path: &Path, prefix: &str, reason: String) {
    let cleaned_outputs = cleanup_segment_prefix(path, prefix).await;
    tracing::info!(
        reason,
        cleaned_outputs,
        prefix = %prefix,
        "Abandoning split-copy strategy"
    );
}

fn copy_segment_plan(
    downloaded: &DownloadedFile,
    duration: f64,
    config: &EngineSettings,
) -> Option<SplitCopyPlan> {
    if duration <= 0.0 {
        return None;
    }

    let bitrate_kbps = downloaded.facts.bitrate_kbps?;
    let bytes_per_sec = bitrate_kbps as f64 * 1000.0 / 8.0;
    if !(bytes_per_sec.is_finite() && bytes_per_sec > 0.0) {
        return None;
    }

    let target_segment_size =
        (config.transcode.max_upload_bytes as f64 * config.transcode.split_target_ratio).max(1.0);
    let mut segment_duration = target_segment_size / bytes_per_sec;

    // Copy-split sizing becomes too noisy below this threshold because keyframe
    // alignment and mux overhead dominate the estimate.
    if !segment_duration.is_finite() || segment_duration < MIN_PLAUSIBLE_SPLIT_COPY_SEGMENT_SECS {
        return None;
    }

    let max_segment = config.transcode.max_single_video_duration_secs as f64;
    if segment_duration > max_segment {
        segment_duration = max_segment;
    }

    if segment_duration >= duration {
        return None;
    }

    Some(SplitCopyPlan {
        segment_duration,
        estimated_segments: (duration / segment_duration).ceil() as u32,
    })
}

#[derive(Debug, Clone)]
struct SegmentOutput {
    path: PathBuf,
    size: u64,
}

fn into_prepared_parts(
    segments: Vec<SegmentOutput>,
    temp_files: &TempFileCleanup,
) -> Vec<PreparedPart> {
    segments
        .into_iter()
        .map(|segment| {
            let guard = temp_files.guard(segment.path.clone());
            PreparedPart::new(segment.path, segment.size, guard)
        })
        .collect()
}

async fn collect_segments(path: &Path, prefix: &str) -> Result<Vec<SegmentOutput>, Error> {
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
                segments.push(SegmentOutput { path, size });
            }
        }
    }

    segments.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(segments)
}

fn segments_fit_limit(segments: &[SegmentOutput], config: &EngineSettings) -> bool {
    for segment in segments {
        if segment.size > config.transcode.max_upload_bytes {
            return false;
        }
    }
    true
}

async fn ensure_split_segments_fit_limit(
    segments: Vec<SegmentOutput>,
    config: &EngineSettings,
) -> Result<Vec<SegmentOutput>, Error> {
    if segments_fit_limit(&segments, config) {
        return Ok(segments);
    }

    cleanup_segment_outputs(&segments).await;
    Err(Error::TranscodeFailed {
        stage: TranscodeStage::Split,
        exit_code: None,
        stderr_tail: "split segment exceeded upload limit".to_string(),
    })
}

async fn cleanup_segment_outputs(segments: &[SegmentOutput]) {
    for segment in segments {
        let _ = fs::remove_file(&segment.path).await;
    }
}

async fn cleanup_segment_prefix(path: &Path, prefix: &str) -> usize {
    let parent_dir = path.parent().unwrap_or(Path::new("."));
    let Ok(mut entries) = fs::read_dir(parent_dir).await else {
        return 0;
    };
    let mut removed = 0;

    while let Ok(Some(entry)) = entries.next_entry().await {
        let candidate = entry.path();
        if let Some(name) = candidate.file_name().and_then(|name| name.to_str())
            && name.starts_with(prefix)
            && name.ends_with(".mp4")
            && fs::remove_file(candidate).await.is_ok()
        {
            removed += 1;
        }
    }

    removed
}

fn remux_path(input: &Path) -> PathBuf {
    let stem = input.file_stem().unwrap_or_default().to_string_lossy();
    input.with_file_name(format!("{}_remux.mp4", stem))
}

fn transcode_path(input: &Path) -> PathBuf {
    let stem = input.file_stem().unwrap_or_default().to_string_lossy();
    input.with_file_name(format!("{}_transcode.mp4", stem))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::panic)]

    use super::{
        BitrateParams, SplitCopyPlan, SplitTranscodeProgress, TranscodeContext, TranscodeStrategy,
        build_strategy_plan, execute_with_hwacc_fallback, send_progress_best_effort,
        split_copy_is_plausible, split_parallel, split_parallel_is_plausible, split_serial,
        try_split_copy,
    };
    use crate::concurrency::Permits;
    use crate::config::{EngineSettings, HardwareAcceleration, TranscodeSettings};
    use crate::engine::TranscodeRuntime;
    use crate::media::TempFileCleanup;
    use crate::pipeline::errors::Error;
    use crate::pipeline::types::{
        AudioCodec, DownloadedFile, MediaContainer, MediaFacts, Progress, VideoCodec,
    };
    use crate::pipeline::{self, errors::TranscodeStage};
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};
    use std::time::{Duration, SystemTime, UNIX_EPOCH};
    use tokio::sync::mpsc;

    fn downloaded_file(path: &str, size: u64, duration: f64, facts: MediaFacts) -> DownloadedFile {
        let temp_files = TempFileCleanup::new();
        DownloadedFile {
            path: PathBuf::from(path),
            size,
            duration: Some(duration),
            resolution: Some((1920, 1080)),
            facts,
            _guard: temp_files.guard(PathBuf::from(path)),
            _dir_guard: None,
        }
    }

    fn downloaded_file_at(
        path: PathBuf,
        size: u64,
        duration: f64,
        facts: MediaFacts,
        temp_files: &TempFileCleanup,
    ) -> DownloadedFile {
        DownloadedFile {
            path: path.clone(),
            size,
            duration: Some(duration),
            resolution: Some((1920, 1080)),
            facts,
            _guard: temp_files.guard(path),
            _dir_guard: None,
        }
    }

    fn unique_temp_path(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time should be after unix epoch")
            .as_nanos();
        std::env::temp_dir().join(format!("azalea-{name}-{nanos}"))
    }

    fn write_executable(path: &Path, contents: &str) {
        fs::write(path, contents).expect("write script");
        let mut permissions = fs::metadata(path).expect("stat script").permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(path, permissions).expect("chmod script");
    }

    fn split_oversize_script() -> String {
        "#!/bin/sh\ncopy_mode=0\npattern=''\nlast_mp4=''\nfor arg in \"$@\"; do\n  case \"$arg\" in\n    copy)\n      copy_mode=1\n      ;;\n    *%03d.mp4)\n      pattern=\"$arg\"\n      ;;\n    *.mp4)\n      last_mp4=\"$arg\"\n      ;;\n  esac\ndone\nif [ -n \"$pattern\" ]; then\n  prefix=${pattern%%%03d.mp4}\n  if [ \"$copy_mode\" -eq 1 ]; then\n    printf 'raw0' > \"${prefix}000.mp4\"\n    printf 'raw1' > \"${prefix}001.mp4\"\n  else\n    printf 'ok' > \"${prefix}000.mp4\"\n    printf '0123456789ABCDEF' > \"${prefix}001.mp4\"\n  fi\n  exit 0\nfi\nif [ -n \"$last_mp4\" ]; then\n  case \"$last_mp4\" in\n    *seg000.mp4)\n      printf 'ok' > \"$last_mp4\"\n      ;;\n    *)\n      printf '0123456789ABCDEF' > \"$last_mp4\"\n      ;;\n  esac\n  exit 0\nfi\nprintf 'unexpected args\\n' >&2\nexit 1\n".to_string()
    }

    fn split_copy_partial_failure_script() -> String {
        "#!/bin/sh\ncopy_mode=0\npattern=''\nfor arg in \"$@\"; do\n  case \"$arg\" in\n    copy)\n      copy_mode=1\n      ;;\n    *%03d.mp4)\n      pattern=\"$arg\"\n      ;;\n  esac\ndone\nif [ \"$copy_mode\" -eq 1 ] && [ -n \"$pattern\" ]; then\n  prefix=${pattern%%%03d.mp4}\n  printf 'partial' > \"${prefix}000.mp4\"\n  printf 'copy split failed\\n' >&2\n  exit 1\nfi\nprintf 'unexpected args\\n' >&2\nexit 2\n".to_string()
    }

    fn split_copy_zero_byte_script() -> String {
        "#!/bin/sh\ncopy_mode=0\npattern=''\nfor arg in \"$@\"; do\n  case \"$arg\" in\n    copy)\n      copy_mode=1\n      ;;\n    *%03d.mp4)\n      pattern=\"$arg\"\n      ;;\n  esac\ndone\nif [ \"$copy_mode\" -eq 1 ] && [ -n \"$pattern\" ]; then\n  prefix=${pattern%%%03d.mp4}\n  : > \"${prefix}000.mp4\"\n  : > \"${prefix}001.mp4\"\n  exit 0\nfi\nprintf 'unexpected args\\n' >&2\nexit 2\n".to_string()
    }

    fn assert_split_oversize(err: Error) {
        match err {
            Error::TranscodeFailed {
                stage: TranscodeStage::Split,
                stderr_tail,
                ..
            } => assert_eq!(stderr_tail, "split segment exceeded upload limit"),
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn strategy_plan_skips_remux_for_already_h264_mp4() {
        let config = EngineSettings::default();
        let downloaded = downloaded_file(
            "already-h264.mp4",
            20 * 1024 * 1024,
            60.0,
            MediaFacts {
                container: MediaContainer::Mp4,
                video_codec: VideoCodec::H264,
                audio_codec: AudioCodec::Aac,
                bitrate_kbps: Some(2_800),
            },
        );

        let plan = build_strategy_plan(&downloaded, 60.0, Some(720), Some(480), &config);

        assert_eq!(
            plan.strategies,
            [
                Some(TranscodeStrategy::TranscodeBalanced),
                Some(TranscodeStrategy::TranscodeAggressive),
                Some(TranscodeStrategy::SplitCopy),
                Some(TranscodeStrategy::SplitTranscode),
                None,
                None,
            ]
        );
        assert!(plan.split_copy.is_some());
        let split_transcode = plan
            .split_transcode
            .expect("split-transcode plan should be computed");
        assert_eq!(split_transcode.segment_duration, 60.0);
        assert_eq!(split_transcode.estimated_segments, 1);
    }

    #[test]
    fn strategy_plan_skips_stream_copy_for_incompatible_codecs() {
        let config = EngineSettings::default();
        let downloaded = downloaded_file(
            "vp9.webm",
            20 * 1024 * 1024,
            60.0,
            MediaFacts {
                container: MediaContainer::Webm,
                video_codec: VideoCodec::Vp9,
                audio_codec: AudioCodec::Opus,
                bitrate_kbps: Some(2_800),
            },
        );

        let plan = build_strategy_plan(&downloaded, 60.0, Some(720), Some(480), &config);

        assert_eq!(
            plan.strategies,
            [
                Some(TranscodeStrategy::TranscodeBalanced),
                Some(TranscodeStrategy::TranscodeAggressive),
                Some(TranscodeStrategy::SplitTranscode),
                None,
                None,
                None,
            ]
        );
        assert!(plan.split_copy.is_none());
    }

    #[test]
    fn strategy_plan_skips_full_transcodes_for_long_oversized_video() {
        let config = EngineSettings::default();
        let downloaded = downloaded_file(
            "long.mp4",
            120 * 1024 * 1024,
            720.0,
            MediaFacts {
                container: MediaContainer::Mp4,
                video_codec: VideoCodec::H264,
                audio_codec: AudioCodec::Aac,
                bitrate_kbps: Some(1_400),
            },
        );

        let plan = build_strategy_plan(&downloaded, 720.0, Some(720), Some(480), &config);

        assert_eq!(
            plan.strategies,
            [
                Some(TranscodeStrategy::SplitCopy),
                Some(TranscodeStrategy::SplitTranscode),
                None,
                None,
                None,
                None,
            ]
        );
        assert!(plan.split_copy.is_some());
        let split_transcode = plan
            .split_transcode
            .expect("split-transcode plan should be computed");
        assert_eq!(split_transcode.segment_duration, 120.0);
        assert_eq!(split_transcode.estimated_segments, 6);
    }

    #[test]
    fn split_copy_preflight_rejects_tiny_segments() {
        let config = EngineSettings::default();
        let downloaded = downloaded_file(
            "tiny-segments.mp4",
            80 * 1024 * 1024,
            20.0,
            MediaFacts {
                container: MediaContainer::Mp4,
                video_codec: VideoCodec::H264,
                audio_codec: AudioCodec::Aac,
                bitrate_kbps: Some(32_000),
            },
        );

        assert!(split_copy_is_plausible(&downloaded, 20.0, &config).is_none());
    }

    #[test]
    fn split_parallel_preflight_rejects_non_mp4_stream_copy_input() {
        let mut config = EngineSettings::default();
        config.concurrency.transcode = 4;

        let downloaded = downloaded_file(
            "long.webm",
            120 * 1024 * 1024,
            720.0,
            MediaFacts {
                container: MediaContainer::Webm,
                video_codec: VideoCodec::Vp9,
                audio_codec: AudioCodec::Opus,
                bitrate_kbps: Some(1_400),
            },
        );

        assert!(!split_parallel_is_plausible(&downloaded, 6, &config));
    }

    #[test]
    fn split_parallel_preflight_rejects_single_transcode_permit() {
        let mut config = EngineSettings::default();
        config.concurrency.transcode = 1;

        let downloaded = downloaded_file(
            "long.mp4",
            120 * 1024 * 1024,
            720.0,
            MediaFacts {
                container: MediaContainer::Mp4,
                video_codec: VideoCodec::H264,
                audio_codec: AudioCodec::Aac,
                bitrate_kbps: Some(1_400),
            },
        );

        assert!(!split_parallel_is_plausible(&downloaded, 6, &config));
    }

    #[test]
    fn split_parallel_preflight_accepts_copyable_input_with_parallel_capacity() {
        let mut config = EngineSettings::default();
        config.concurrency.transcode = 4;

        let downloaded = downloaded_file(
            "long.mp4",
            120 * 1024 * 1024,
            720.0,
            MediaFacts {
                container: MediaContainer::Mp4,
                video_codec: VideoCodec::H264,
                audio_codec: AudioCodec::Aac,
                bitrate_kbps: Some(1_400),
            },
        );

        assert!(split_parallel_is_plausible(&downloaded, 6, &config));
    }

    #[tokio::test]
    async fn split_serial_rejects_oversize_segments_and_cleans_up() {
        let temp_files = TempFileCleanup::new();
        let temp_dir = unique_temp_path("split-serial-dir");
        let script_path = unique_temp_path("split-serial-ffmpeg.sh");
        fs::create_dir_all(&temp_dir).expect("create temp dir");
        write_executable(&script_path, &split_oversize_script());

        let input_path = temp_dir.join("input.mp4");
        fs::write(&input_path, b"input").expect("write input");

        let mut config = EngineSettings::default();
        config.binaries.ffmpeg = script_path.clone();
        config.storage.temp_dir = temp_dir.clone();
        config.transcode.max_upload_bytes = 8;
        config.transcode.ffmpeg_timeout_secs = 1;

        let permits = Permits::new(&config.concurrency);
        let runtime = TranscodeRuntime::new(HardwareAcceleration::None);
        let ctx = TranscodeContext {
            permits: &permits,
            config: &config,
            runtime: &runtime,
        };
        let downloaded = downloaded_file_at(
            input_path.clone(),
            32,
            30.0,
            MediaFacts {
                container: MediaContainer::Mp4,
                video_codec: VideoCodec::H264,
                audio_codec: AudioCodec::Aac,
                bitrate_kbps: Some(1_400),
            },
            &temp_files,
        );
        let dir_guard = temp_files.guard(temp_dir.clone());
        let segment_params = BitrateParams {
            video_bitrate_kbps: 500,
            audio_bitrate_kbps: 128,
        };

        let err = split_serial(
            downloaded,
            10.0,
            &segment_params,
            &temp_files,
            &ctx,
            SplitTranscodeProgress::new(None, 2),
            dir_guard,
        )
        .await
        .expect_err("oversize split segment must fail");

        assert_split_oversize(err);
        assert!(!temp_dir.join("input_seg000.mp4").exists());
        assert!(!temp_dir.join("input_seg001.mp4").exists());

        temp_files.shutdown().await;
        let _ = fs::remove_file(script_path);
        let _ = fs::remove_dir_all(temp_dir);
    }

    #[tokio::test]
    async fn split_parallel_rejects_oversize_segments_and_cleans_up() {
        let temp_files = TempFileCleanup::new();
        let temp_dir = unique_temp_path("split-parallel-dir");
        let script_path = unique_temp_path("split-parallel-ffmpeg.sh");
        fs::create_dir_all(&temp_dir).expect("create temp dir");
        write_executable(&script_path, &split_oversize_script());

        let input_path = temp_dir.join("input.mp4");
        fs::write(&input_path, b"input").expect("write input");

        let mut config = EngineSettings::default();
        config.binaries.ffmpeg = script_path.clone();
        config.storage.temp_dir = temp_dir.clone();
        config.transcode.max_upload_bytes = 8;
        config.transcode.ffmpeg_timeout_secs = 1;
        config.concurrency.transcode = 2;

        let permits = Permits::new(&config.concurrency);
        let runtime = TranscodeRuntime::new(HardwareAcceleration::None);
        let ctx = TranscodeContext {
            permits: &permits,
            config: &config,
            runtime: &runtime,
        };
        let downloaded = downloaded_file_at(
            input_path.clone(),
            32,
            30.0,
            MediaFacts {
                container: MediaContainer::Mp4,
                video_codec: VideoCodec::H264,
                audio_codec: AudioCodec::Aac,
                bitrate_kbps: Some(1_400),
            },
            &temp_files,
        );
        let dir_guard = temp_files.guard(temp_dir.clone());
        let segment_params = BitrateParams {
            video_bitrate_kbps: 500,
            audio_bitrate_kbps: 128,
        };

        let err = split_parallel(
            downloaded,
            10.0,
            &segment_params,
            &temp_files,
            &ctx,
            None,
            dir_guard,
        )
        .await
        .expect_err("oversize split segment must fail");

        assert_split_oversize(err);
        assert!(!temp_dir.join("input_raw000.mp4").exists());
        assert!(!temp_dir.join("input_raw001.mp4").exists());
        assert!(!temp_dir.join("input_seg000.mp4").exists());
        assert!(!temp_dir.join("input_seg001.mp4").exists());

        temp_files.shutdown().await;
        let _ = fs::remove_file(script_path);
        let _ = fs::remove_dir_all(temp_dir);
    }

    #[tokio::test]
    async fn try_split_copy_cleans_partial_outputs_after_ffmpeg_failure() {
        let temp_files = TempFileCleanup::new();
        let temp_dir = unique_temp_path("split-copy-partial-dir");
        let script_path = unique_temp_path("split-copy-partial-ffmpeg.sh");
        fs::create_dir_all(&temp_dir).expect("create temp dir");
        write_executable(&script_path, &split_copy_partial_failure_script());

        let input_path = temp_dir.join("input.mp4");
        fs::write(&input_path, b"input").expect("write input");

        let mut config = EngineSettings::default();
        config.binaries.ffmpeg = script_path.clone();
        config.storage.temp_dir = temp_dir.clone();
        config.transcode.ffmpeg_timeout_secs = 1;

        let permits = Permits::new(&config.concurrency);
        let downloaded = downloaded_file_at(
            input_path.clone(),
            32,
            30.0,
            MediaFacts {
                container: MediaContainer::Mp4,
                video_codec: VideoCodec::H264,
                audio_codec: AudioCodec::Aac,
                bitrate_kbps: Some(1_400),
            },
            &temp_files,
        );
        let mut dir_guard = Some(temp_files.guard(temp_dir.clone()));

        let result = try_split_copy(
            &downloaded,
            SplitCopyPlan {
                segment_duration: 10.0,
                estimated_segments: 2,
            },
            &permits,
            &temp_files,
            &config,
            &mut dir_guard,
        )
        .await
        .expect("split-copy failure should fall back cleanly");

        assert!(result.is_none());
        assert!(dir_guard.is_some());
        assert!(!temp_dir.join("input_copy000.mp4").exists());

        temp_files.shutdown().await;
        let _ = fs::remove_file(script_path);
        let _ = fs::remove_dir_all(temp_dir);
    }

    #[tokio::test]
    async fn try_split_copy_cleans_zero_byte_outputs_when_no_segments_are_usable() {
        let temp_files = TempFileCleanup::new();
        let temp_dir = unique_temp_path("split-copy-empty-dir");
        let script_path = unique_temp_path("split-copy-empty-ffmpeg.sh");
        fs::create_dir_all(&temp_dir).expect("create temp dir");
        write_executable(&script_path, &split_copy_zero_byte_script());

        let input_path = temp_dir.join("input.mp4");
        fs::write(&input_path, b"input").expect("write input");

        let mut config = EngineSettings::default();
        config.binaries.ffmpeg = script_path.clone();
        config.storage.temp_dir = temp_dir.clone();
        config.transcode.ffmpeg_timeout_secs = 1;

        let permits = Permits::new(&config.concurrency);
        let downloaded = downloaded_file_at(
            input_path.clone(),
            32,
            30.0,
            MediaFacts {
                container: MediaContainer::Mp4,
                video_codec: VideoCodec::H264,
                audio_codec: AudioCodec::Aac,
                bitrate_kbps: Some(1_400),
            },
            &temp_files,
        );
        let mut dir_guard = Some(temp_files.guard(temp_dir.clone()));

        let result = try_split_copy(
            &downloaded,
            SplitCopyPlan {
                segment_duration: 10.0,
                estimated_segments: 2,
            },
            &permits,
            &temp_files,
            &config,
            &mut dir_guard,
        )
        .await
        .expect("split-copy should fall back when scan finds no usable segments");

        assert!(result.is_none());
        assert!(dir_guard.is_some());
        assert!(!temp_dir.join("input_copy000.mp4").exists());
        assert!(!temp_dir.join("input_copy001.mp4").exists());

        temp_files.shutdown().await;
        let _ = fs::remove_file(script_path);
        let _ = fs::remove_dir_all(temp_dir);
    }

    #[tokio::test]
    async fn non_terminal_transcode_progress_is_dropped_when_channel_is_full() {
        let (tx, mut rx) = mpsc::channel(1);
        tx.try_send(Progress::Optimizing)
            .expect("initial progress update should fit");
        let progress_tx = Some(tx.clone());

        send_progress_best_effort(&progress_tx, Progress::Transcoding(1, 3));

        assert_eq!(rx.recv().await, Some(Progress::Optimizing));
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn terminal_transcode_progress_is_queued_even_when_channel_is_full() {
        let (tx, mut rx) = mpsc::channel(1);
        tx.try_send(Progress::Optimizing)
            .expect("initial progress update should fit");
        let progress_tx = Some(tx.clone());

        send_progress_best_effort(&progress_tx, Progress::Transcoding(3, 3));

        assert_eq!(rx.recv().await, Some(Progress::Optimizing));
        assert_eq!(rx.recv().await, Some(Progress::Transcoding(3, 3)));
    }

    #[tokio::test]
    async fn hardware_failure_retries_with_software_encoder() {
        let script_path = unique_temp_path("ffmpeg-fallback.sh");
        let log_path = unique_temp_path("ffmpeg-fallback.log");
        let script = format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"{}\"\ncase \" $* \" in\n  *\" h264_vaapi \"*)\n    printf '%s\\n' 'vaapi init failed' >&2\n    exit 1\n    ;;\n  *\" libx264 \"*)\n    exit 0\n    ;;\nesac\nprintf '%s\\n' 'unexpected encoder args' >&2\nexit 2\n",
            log_path.display()
        );
        fs::write(&script_path, script).expect("write ffmpeg stub");
        let mut permissions = fs::metadata(&script_path)
            .expect("stat ffmpeg stub")
            .permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&script_path, permissions).expect("chmod ffmpeg stub");

        let runtime = TranscodeRuntime::new(HardwareAcceleration::Vaapi);
        let settings = TranscodeSettings {
            hardware_acceleration: HardwareAcceleration::Vaapi,
            ..TranscodeSettings::default()
        };

        let outcome = execute_with_hwacc_fallback(
            &script_path,
            Duration::from_secs(1),
            TranscodeStage::Transcode,
            &settings,
            &runtime,
            |active_settings| {
                let mut args = pipeline::ffmpeg::Args::new();
                args.push("-c:v".into());
                args.push(active_settings.hardware_acceleration.encoder().into());
                args
            },
        )
        .await
        .expect("fallback retry should succeed");

        let invocations = fs::read_to_string(&log_path).expect("read ffmpeg invocation log");
        assert!(invocations.contains("h264_vaapi"));
        assert!(invocations.contains("libx264"));
        assert_eq!(outcome.encoder_used, "libx264");
        assert_eq!(outcome.backend_used, HardwareAcceleration::None);
        assert!(!outcome.used_hardware);
        assert!(outcome.fallback_occurred);
        assert_eq!(runtime.active_backend(), HardwareAcceleration::None);
        assert!(runtime.software_fallback_active());
        assert_eq!(runtime.hw_encode_count(), 0);
        assert_eq!(runtime.sw_encode_count(), 1);
        assert!(runtime.sw_avg_duration_ms() <= outcome.duration_ms);

        let _ = fs::remove_file(script_path);
        let _ = fs::remove_file(log_path);
    }
}
