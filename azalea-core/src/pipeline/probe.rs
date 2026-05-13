//! Programmatic end-to-end probe for the core media pipeline.
//!
//! The probe runs the real resolve -> download -> optimize path but deliberately
//! skips Discord upload. It returns structured timing and artifact data so CLI
//! tools and agents can validate behavior without scraping logs.
//!
//! Probe runs deliberately avoid persistent metrics and dedup writes: without a
//! Discord upload, success/failure counters would not match normal pipeline
//! completion semantics.

use crate::Engine;
use crate::config::HardwareAcceleration;
use crate::media::TweetLink;
use crate::pipeline::types::{
    AudioCodec, DownloadedFile, Job, MediaContainer, MediaFacts, MediaType, PreparedUpload,
    Progress, RequestId, ResolvedMedia, VideoCodec,
};
use crate::pipeline::{download, optimize};
use crate::storage::Stage;
use serde::Serialize;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::mpsc;

const RESOLVE_WARN_MS: u64 = 8_000;
const DOWNLOAD_WARN_MS: u64 = 20_000;
const OPTIMIZE_WARN_MS: u64 = 30_000;

#[derive(Debug, Clone)]
pub struct Input {
    pub request_id: RequestId,
    pub scope_id: u64,
    pub job_id: u64,
    pub tweet_url: TweetLink,
}

impl Input {
    pub fn new(request_id: RequestId, scope_id: u64, job_id: u64, tweet_url: TweetLink) -> Self {
        Self {
            request_id,
            scope_id,
            job_id,
            tweet_url,
        }
    }

    fn into_job(self) -> Job {
        Job {
            request_id: self.request_id,
            scope_id: self.scope_id,
            job_id: self.job_id,
            tweet_url: self.tweet_url,
            inflight_reserved: true,
        }
    }
}

#[derive(Debug)]
pub struct Run {
    pub report: Report,
    pub prepared: PreparedUpload,
}

#[derive(Debug, Clone, Serialize)]
pub struct Report {
    pub request_id: u64,
    pub scope_id: u64,
    pub job_id: u64,
    pub tweet_id: u64,
    pub user: String,
    pub canonical_url: String,
    pub elapsed_ms: u64,
    pub stages: StageTimings,
    pub resolved: Resolved,
    pub downloaded: Downloaded,
    pub output: Output,
    pub runtime: Runtime,
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct StageTimings {
    pub resolve_ms: u64,
    pub download_ms: u64,
    pub optimize_ms: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct Resolved {
    pub url: String,
    pub media_type: &'static str,
    pub extension: String,
    pub duration_secs: Option<f64>,
    pub resolution: Option<(u32, u32)>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Downloaded {
    pub path: String,
    pub size_bytes: u64,
    pub duration_secs: Option<f64>,
    pub resolution: Option<(u32, u32)>,
    pub facts: Facts,
    pub upload_ready_memory_bytes: Option<usize>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Facts {
    pub container: &'static str,
    pub video_codec: &'static str,
    pub audio_codec: &'static str,
    pub bitrate_kbps: Option<u32>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Output {
    pub kind: &'static str,
    pub parts: Vec<Part>,
    pub total_size_bytes: u64,
    pub upload_limit_bytes: u64,
    pub fits_single_upload: bool,
    pub size_ratio_to_download: Option<f64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Part {
    pub path: String,
    pub size_bytes: u64,
    pub upload_ready_memory_bytes: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kept_path: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct Runtime {
    pub configured_backend: &'static str,
    pub active_backend: &'static str,
    pub software_fallback_active: bool,
    pub fallback_transitions: u64,
    pub hw_encode_count: u64,
    pub sw_encode_count: u64,
    pub hw_avg_duration_ms: u64,
    pub sw_avg_duration_ms: u64,
}

pub async fn run(
    input: Input,
    engine: &Engine,
    progress: Option<mpsc::Sender<Progress>>,
) -> Result<Run, super::Error> {
    let job = input.into_job();
    let total_start = Instant::now();
    let (resolved, resolve_ms) = resolve(&job, engine, progress.as_ref()).await?;
    let (downloaded, download_ms) =
        download(&job, Arc::as_ref(&resolved), engine, progress.as_ref()).await?;
    let downloaded_report = Downloaded::from(&downloaded);
    let (prepared, optimize_ms) =
        optimize(downloaded, Arc::as_ref(&resolved), engine, progress).await?;
    let stages = StageTimings {
        resolve_ms,
        download_ms,
        optimize_ms,
    };
    let output = Output::from_prepared(&prepared, downloaded_report.size_bytes, engine);

    Ok(Run {
        report: Report {
            request_id: job.request_id.0,
            scope_id: job.scope_id,
            job_id: job.job_id,
            tweet_id: job.tweet_url.tweet_id.0,
            user: job.tweet_url.user.to_string(),
            canonical_url: job.tweet_url.canonical_url().to_string(),
            elapsed_ms: total_start.elapsed().as_millis() as u64,
            stages,
            resolved: Resolved::from(Arc::as_ref(&resolved)),
            downloaded: downloaded_report,
            output,
            runtime: Runtime::from(engine.transcode_runtime.snapshot()),
        },
        prepared,
    })
}

async fn resolve(
    job: &Job,
    engine: &Engine,
    progress: Option<&mpsc::Sender<Progress>>,
) -> Result<(Arc<ResolvedMedia>, u64), super::Error> {
    send_progress(progress, Progress::Resolving).await;
    let started = Instant::now();
    let resolved = engine
        .resolver
        .resolve(&job.tweet_url, &engine.http, &engine.permits)
        .await?;
    let duration_ms = started.elapsed().as_millis() as u64;
    warn_if_slow(Stage::Resolve, duration_ms, RESOLVE_WARN_MS);
    Ok((resolved, duration_ms))
}

async fn download(
    job: &Job,
    resolved: &ResolvedMedia,
    engine: &Engine,
    progress: Option<&mpsc::Sender<Progress>>,
) -> Result<(DownloadedFile, u64), super::Error> {
    send_progress(progress, Progress::Downloading).await;
    let started = Instant::now();
    let downloaded = download::download(
        resolved,
        job,
        &engine.permits,
        &engine.reserved_download_bytes,
        &engine.temp_files,
        &engine.config,
        &engine.pinned_media_clients,
    )
    .await?;
    let duration_ms = started.elapsed().as_millis() as u64;
    warn_if_slow(Stage::Download, duration_ms, DOWNLOAD_WARN_MS);
    Ok((downloaded, duration_ms))
}

async fn optimize(
    downloaded: DownloadedFile,
    resolved: &ResolvedMedia,
    engine: &Engine,
    progress: Option<mpsc::Sender<Progress>>,
) -> Result<(PreparedUpload, u64), super::Error> {
    send_progress(progress.as_ref(), Progress::Optimizing).await;
    let started = Instant::now();
    let prepared = optimize::optimize(
        downloaded,
        resolved,
        &engine.permits,
        &engine.temp_files,
        &engine.config,
        &engine.transcode_runtime,
        progress,
    )
    .await?;
    let duration_ms = started.elapsed().as_millis() as u64;
    warn_if_slow(Stage::Optimize, duration_ms, OPTIMIZE_WARN_MS);
    Ok((prepared, duration_ms))
}

async fn send_progress(progress: Option<&mpsc::Sender<Progress>>, stage: Progress) {
    if let Some(tx) = progress {
        let _ = tx.send(stage).await;
    }
}

fn warn_if_slow(stage: Stage, duration_ms: u64, threshold_ms: u64) {
    if duration_ms > threshold_ms {
        tracing::warn!(
            ?stage,
            duration_ms,
            threshold_ms,
            "Probe stage exceeded warning threshold"
        );
    }
}

impl From<&ResolvedMedia> for Resolved {
    fn from(value: &ResolvedMedia) -> Self {
        Self {
            url: value.url.to_string(),
            media_type: media_type(value.media_type),
            extension: value.extension.to_string(),
            duration_secs: value.duration,
            resolution: value.resolution,
        }
    }
}

impl From<&DownloadedFile> for Downloaded {
    fn from(value: &DownloadedFile) -> Self {
        Self {
            path: value.path.display().to_string(),
            size_bytes: value.size,
            duration_secs: value.duration,
            resolution: value.resolution,
            facts: Facts::from(value.facts),
            upload_ready_memory_bytes: value.upload_ready_bytes.as_ref().map(|bytes| bytes.len()),
        }
    }
}

impl From<MediaFacts> for Facts {
    fn from(value: MediaFacts) -> Self {
        Self {
            container: container(value.container),
            video_codec: video_codec(value.video_codec),
            audio_codec: audio_codec(value.audio_codec),
            bitrate_kbps: value.bitrate_kbps,
        }
    }
}

impl Output {
    fn from_prepared(prepared: &PreparedUpload, downloaded_size: u64, engine: &Engine) -> Self {
        let (kind, parts) = match prepared {
            PreparedUpload::Single { part, .. } => ("single", vec![Part::from(part)]),
            PreparedUpload::Split { parts, .. } => {
                ("split", parts.iter().map(Part::from).collect())
            }
        };
        let total_size_bytes = parts.iter().map(|part| part.size_bytes).sum();

        Self {
            kind,
            parts,
            total_size_bytes,
            upload_limit_bytes: engine.config.transcode.max_upload_bytes,
            fits_single_upload: total_size_bytes <= engine.config.transcode.max_upload_bytes,
            size_ratio_to_download: ratio(total_size_bytes, downloaded_size),
        }
    }
}

impl From<&crate::pipeline::types::PreparedPart> for Part {
    fn from(value: &crate::pipeline::types::PreparedPart) -> Self {
        Self {
            path: value.path().display().to_string(),
            size_bytes: value.size(),
            upload_ready_memory_bytes: value.upload_ready_bytes().map(|bytes| bytes.len()),
            kept_path: None,
        }
    }
}

impl From<crate::engine::TranscodeRuntimeSnapshot> for Runtime {
    fn from(value: crate::engine::TranscodeRuntimeSnapshot) -> Self {
        Self {
            configured_backend: hardware(value.configured_backend),
            active_backend: hardware(value.active_backend),
            software_fallback_active: value.software_fallback_active,
            fallback_transitions: value.fallback_transitions,
            hw_encode_count: value.hw_encode_count,
            sw_encode_count: value.sw_encode_count,
            hw_avg_duration_ms: value.hw_avg_duration_ms,
            sw_avg_duration_ms: value.sw_avg_duration_ms,
        }
    }
}

fn ratio(numerator: u64, denominator: u64) -> Option<f64> {
    if denominator == 0 {
        None
    } else {
        Some(numerator as f64 / denominator as f64)
    }
}

fn media_type(value: MediaType) -> &'static str {
    match value {
        MediaType::Video => "video",
        MediaType::Image => "image",
    }
}

fn hardware(value: HardwareAcceleration) -> &'static str {
    value.as_str()
}

fn container(value: MediaContainer) -> &'static str {
    match value {
        MediaContainer::Mp4 => "mp4",
        MediaContainer::Mov => "mov",
        MediaContainer::Webm => "webm",
        MediaContainer::Matroska => "matroska",
        MediaContainer::Avi => "avi",
        MediaContainer::MpegTs => "mpegts",
        MediaContainer::Gif => "gif",
        MediaContainer::Unknown => "unknown",
    }
}

fn video_codec(value: VideoCodec) -> &'static str {
    match value {
        VideoCodec::H264 => "h264",
        VideoCodec::H265 => "h265",
        VideoCodec::Vp8 => "vp8",
        VideoCodec::Vp9 => "vp9",
        VideoCodec::Av1 => "av1",
        VideoCodec::Other => "other",
    }
}

fn audio_codec(value: AudioCodec) -> &'static str {
    match value {
        AudioCodec::Aac => "aac",
        AudioCodec::Opus => "opus",
        AudioCodec::Vorbis => "vorbis",
        AudioCodec::Mp3 => "mp3",
        AudioCodec::None => "none",
        AudioCodec::Other => "other",
    }
}
