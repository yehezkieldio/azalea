//! # Module overview
//! Download stage with size limits, SSRF guards, and metadata probing.
//!
//! ## Trade-off acknowledgment
//! We prefer bounded reads and timeouts over maximum throughput to avoid
//! unbounded memory growth on untrusted inputs.
//!
//! ## Algorithm overview
//! 1. Request the first `download_chunk_bytes` of the media (a plain GET when
//!    `download_connections == 1`).
//! 2. If that range is the whole file, stream it to memory or a temp file.
//!    Otherwise fill the preallocated temp file with fixed-size range chunks
//!    pulled by up to `download_connections` parallel connections.
//! 3. Probe metadata via ffprobe only when the resolver could not supply it or
//!    the optimizer needs codec facts.
//!
//! ## Performance model
//! Downloads are network-bound: time ≈ connection setup + TTFB + size /
//! per-connection throughput. CDN edges cap single-connection throughput
//! (especially on cache misses), so splitting the body across connections
//! scales the last term until the host link saturates. Small files gain
//! nothing from extra handshakes, hence the single-chunk fast path.

use crate::concurrency::Permits;
use crate::config::EngineSettings;
use crate::media::TempFileCleanup;
use crate::pipeline::disk::{ensure_disk_space, reserve_download_bytes};
use crate::pipeline::errors::{DownloadError, Error};
use crate::pipeline::process::{JsonSubprocessError, run_json_subprocess};
use crate::pipeline::ssrf::{
    ValidatedMediaUrl, blocked_address_error, is_blocked_address, validate_media_url,
};
use crate::pipeline::types::{
    AudioCodec, DownloadedFile, Job, MediaContainer, MediaFacts, MediaType, ResolvedMedia,
    VideoCodec, sanitize_extension,
};
use futures_util::StreamExt;
use reqwest::StatusCode;
use reqwest::header::{CONTENT_RANGE, HeaderMap, LOCATION, RANGE};
use serde::Deserialize;
use std::future::Future;
use std::io::SeekFrom;
use std::ops::Range;
use std::path::Path;
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};
use std::time::{Duration, Instant};
use tokio::fs;
use tokio::io::{AsyncSeekExt, AsyncWriteExt, BufWriter};
use tokio::process::Command;
use tracing::Instrument as _;

/// Download media to a temp file while enforcing size and safety constraints.
///
/// ## Preconditions
/// - `resolved` comes from [`crate::pipeline::resolve::ResolverChain`].
/// - `config` has been validated via [`crate::config::EngineSettings::validate`].
/// - `media_http` was built by [`build_media_client`].
///
/// ## Postconditions
/// - The returned [`DownloadedFile`] owns a temp guard for cleanup.
/// - File size is bounded by `pipeline.max_download_bytes`.
pub async fn download(
    resolved: &ResolvedMedia,
    job: &Job,
    permits: &Permits,
    reserved_download_bytes: &Arc<AtomicU64>,
    temp_files: &TempFileCleanup,
    config: &EngineSettings,
    media_http: &reqwest::Client,
) -> Result<DownloadedFile, Error> {
    tracing::trace!(
        request_id = job.request_id.0,
        tweet_id = job.tweet_url.tweet_id.0,
        "Entered download stage"
    );
    tracing::info!(
        url = %resolved.url,
        extension = %resolved.extension,
        "Starting download"
    );
    let download_start = Instant::now();

    let _permit = permits
        .download
        .acquire()
        .await
        .map_err(|_| Error::DownloadFailed {
            source: DownloadError::WriteFailed(std::io::Error::other("download semaphore closed")),
        })?;

    // Normalize extension to a safe, predictable filename suffix.
    let safe_extension = sanitize_extension(&resolved.extension);
    let filename = format!(
        "{}_{}.{}",
        job.job_id, job.tweet_url.tweet_id.0, safe_extension
    );
    let job_dir = config
        .storage
        .temp_dir
        .join(format!("{}_{}", job.job_id, job.tweet_url.tweet_id.0));
    fs::create_dir_all(&job_dir).await?;
    let output_path = job_dir.join(&filename);
    tracing::trace!(path = %output_path.display(), "Download target path");
    let dir_guard = temp_files.guard(job_dir);
    let guard = temp_files.guard(output_path.clone());

    let download_timeout = Duration::from_secs(config.pipeline.download_timeout_secs);
    let download_result = tokio::time::timeout(download_timeout, async move {
        let validated_url = validate_media_url(resolved.url.as_ref())?;
        let first_range = (config.pipeline.download_connections > 1)
            .then_some(0..config.pipeline.download_chunk_bytes);

        let response = fetch_with_redirects(media_http, validated_url, first_range)
            .instrument(tracing::info_span!("download.redirects"))
            .await?;

        let body = classify_response(&response)?;
        let total_size = body.total();
        let must_probe = total_size.is_none();
        if let Some(total) = total_size {
            tracing::trace!(
                total_bytes = total,
                ranged = body.is_ranged(),
                "Content length provided"
            );
        } else {
            tracing::trace!("Content length unavailable");
        }
        let max_download = config.pipeline.max_download_bytes;
        // Early reject using the declared size before streaming any bytes.
        if max_download > 0
            && let Some(total) = total_size
            && total > max_download
        {
            return Err(Error::DownloadFailed {
                source: DownloadError::TooLarge {
                    size_mb: total / 1024 / 1024,
                    max_mb: max_download / 1024 / 1024,
                },
            });
        }

        let should_probe = must_probe
            || resolver_metadata_missing(resolved)
            || (resolved.media_type == MediaType::Video
                && total_size.is_some_and(|size| size > config.transcode.max_upload_bytes));
        let memory_only =
            !body.is_ranged() && can_keep_download_memory_only(total_size, should_probe, config);

        let reserve_bytes = if memory_only {
            0
        } else {
            total_size
                .filter(|size| *size > 0)
                .or_else(|| (max_download > 0).then_some(max_download))
                .unwrap_or(0)
        };
        tracing::trace!(reserve_bytes, "Reserving disk budget for download");
        let _download_reservation = if reserve_bytes > 0 {
            Some(
                reserve_download_bytes(
                    &config.storage.temp_dir,
                    config.pipeline.min_disk_space_bytes,
                    reserve_bytes,
                    reserved_download_bytes,
                )
                .await?,
            )
        } else {
            ensure_disk_space(
                &config.storage.temp_dir,
                config.pipeline.min_disk_space_bytes,
            )
            .await?;
            None
        };

        let (downloaded, upload_ready_bytes) = match body {
            Body::Complete { total } => {
                stream_single(response, total, memory_only, &output_path, config).await?
            }
            Body::Ranged { first, total } => {
                download_ranged(response, first, total, &output_path, media_http, config)
                    .instrument(tracing::info_span!("download.ranged", total_bytes = total))
                    .await?;
                (total, None)
            }
        };

        let mut duration = resolved.duration;
        let mut resolution = resolved.resolution;
        let mut facts = MediaFacts::from_extension(&resolved.extension);

        if should_probe {
            tracing::trace!("Probing downloaded file for metadata");
            match probe_file(&output_path, config)
                .instrument(tracing::info_span!("download.ffprobe", path = %output_path.display()))
                .await
            {
                Ok(result) => {
                    duration = result.duration.or(duration);
                    resolution = result.resolution.or(resolution);
                    if matches!(facts.container, MediaContainer::Unknown) {
                        facts.container = result.facts.container;
                    }
                    facts.video_codec = result.facts.video_codec;
                    facts.audio_codec = result.facts.audio_codec;
                    facts.bitrate_kbps = result.facts.bitrate_kbps;
                }
                Err(e) => {
                    tracing::trace!(error = %e, "Probe failed");
                    if must_probe {
                        return Err(Error::DownloadFailed {
                            source: DownloadError::WriteFailed(std::io::Error::other(
                                "corrupt download",
                            )),
                        });
                    }
                }
            }
        } else {
            tracing::trace!("Using metadata from resolver");
        }

        facts.bitrate_kbps = facts
            .bitrate_kbps
            .or_else(|| estimate_bitrate_kbps(downloaded, duration));

        tracing::info!(
            duration_ms = download_start.elapsed().as_millis(),
            size_bytes = downloaded,
            path = %output_path.display(),
            memory_only,
            probed = should_probe,
            "Download finished"
        );

        Ok(DownloadedFile {
            path: output_path.clone(),
            size: downloaded,
            duration,
            resolution,
            facts,
            upload_ready_bytes: upload_ready_bytes.map(Arc::<[u8]>::from),
            _guard: guard,
            _dir_guard: Some(dir_guard),
        })
    })
    .await;

    match download_result {
        Ok(result) => result,
        Err(_) => {
            tracing::warn!(
                timeout_secs = download_timeout.as_secs(),
                "Download timed out"
            );
            Err(Error::Timeout {
                operation: "download",
                duration: download_timeout,
            })
        }
    }
}

/// Whether the resolver left out metadata the optimizer relies on.
///
/// Images never carry a duration, so requiring one would force an ffprobe
/// process spawn for every image.
fn resolver_metadata_missing(resolved: &ResolvedMedia) -> bool {
    resolved.resolution.is_none()
        || (resolved.media_type == MediaType::Video && resolved.duration.is_none())
}

/// Shape of the first response body.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Body {
    /// The response carries the whole file.
    Complete { total: Option<u64> },
    /// The response carries bytes `0..first` of a `total`-byte file.
    Ranged { first: u64, total: u64 },
}

impl Body {
    fn total(self) -> Option<u64> {
        match self {
            Self::Complete { total } => total,
            Self::Ranged { total, .. } => Some(total),
        }
    }

    fn is_ranged(self) -> bool {
        matches!(self, Self::Ranged { .. })
    }
}

fn classify_response(response: &reqwest::Response) -> Result<Body, Error> {
    let status = response.status();
    if !status.is_success() {
        return Err(Error::DownloadFailed {
            source: DownloadError::HttpStatus(status.as_u16()),
        });
    }
    if status != StatusCode::PARTIAL_CONTENT {
        // Servers that ignore `Range` answer 200 with the full body.
        return Ok(Body::Complete {
            total: response.content_length(),
        });
    }

    let range = content_range(response.headers())
        .filter(|range| range.start == 0)
        .ok_or_else(|| protocol_error("invalid content-range for initial range"))?;
    if range.end == range.total {
        Ok(Body::Complete {
            total: Some(range.total),
        })
    } else {
        Ok(Body::Ranged {
            first: range.end,
            total: range.total,
        })
    }
}

/// Stream a complete response body into memory and/or the temp file.
///
/// Returns the byte count and, for under-limit candidates, the retained
/// upload-ready buffer.
async fn stream_single(
    response: reqwest::Response,
    total_size: Option<u64>,
    memory_only: bool,
    output_path: &Path,
    config: &EngineSettings,
) -> Result<(u64, Option<Vec<u8>>), Error> {
    let max_download = config.pipeline.max_download_bytes;
    let mut stream = response.bytes_stream();
    let mut upload_ready_bytes = bounded_upload_ready_buffer(total_size, config);
    let mut file = if memory_only {
        None
    } else {
        Some(open_download_file(output_path, total_size, config).await?)
    };

    let mut downloaded = 0u64;
    let upload_ready_buffer_limit = upload_ready_buffer_limit(config);
    let mut last_log = Instant::now();
    let log_interval = Duration::from_secs(5);

    while let Some(chunk_result) = stream.next().await {
        let chunk = chunk_result.map_err(request_error)?;

        let next_downloaded = downloaded.saturating_add(chunk.len() as u64);
        if let Some(bytes) = upload_ready_bytes.as_mut() {
            if next_downloaded <= upload_ready_buffer_limit {
                bytes.extend_from_slice(&chunk);
            } else {
                let buffered = upload_ready_bytes.take();
                if file.is_none() {
                    transition_memory_download_to_file(
                        &mut file,
                        output_path,
                        total_size,
                        config,
                        buffered,
                    )
                    .await?;
                }
            }
        }

        downloaded = next_downloaded;

        if max_download > 0 && downloaded > max_download {
            if let Some(file) = file.take() {
                drop(file);
                let _ = fs::remove_file(output_path).await;
            }
            return Err(Error::DownloadFailed {
                source: DownloadError::TooLarge {
                    size_mb: downloaded / 1024 / 1024,
                    max_mb: max_download / 1024 / 1024,
                },
            });
        }

        if let Some(file) = file.as_mut() {
            file.write_all(&chunk).await.map_err(write_error)?;
        }

        if last_log.elapsed() >= log_interval {
            let percent = total_size
                .map(|total| (downloaded as f64 / total as f64 * 100.0) as u32)
                .unwrap_or(0);
            tracing::info!(
                percent,
                downloaded_mb = %format!("{:.2}", downloaded as f64 / 1024.0 / 1024.0),
                "Downloading media..."
            );
            last_log = Instant::now();
        }
    }

    if downloaded == 0 {
        return Err(Error::DownloadFailed {
            source: DownloadError::EmptyResponse,
        });
    }

    if let Some(total) = total_size
        && downloaded != total
    {
        tracing::warn!(expected = total, downloaded, "Download size mismatch");
        return Err(protocol_error("download incomplete"));
    }

    if let Some(file) = file {
        finish_download_file(file, downloaded).await?;
    }

    Ok((downloaded, upload_ready_bytes))
}

/// Work queue of fixed-size byte ranges shared by the range workers.
///
/// ## Rationale
/// Pulling chunks on demand (instead of a static 1/N split) lets fast
/// connections take more of the file and absorbs slow connection setup:
/// the first connection keeps streaming while the others are still
/// handshaking. The cost is one request round-trip per chunk on an already
/// warm keep-alive connection.
struct ChunkQueue {
    next: AtomicU64,
    chunk_bytes: u64,
    total: u64,
}

impl ChunkQueue {
    fn new(start: u64, chunk_bytes: u64, total: u64) -> Self {
        Self {
            next: AtomicU64::new(start),
            chunk_bytes,
            total,
        }
    }

    fn remaining_chunks(&self) -> u64 {
        self.total
            .saturating_sub(self.next.load(Ordering::Relaxed))
            .div_ceil(self.chunk_bytes)
    }

    fn claim(&self) -> Option<Range<u64>> {
        // Workers are polled on one task; the atomic only exists to keep the
        // shared queue `Sync` so the download future stays `Send`.
        let start = self.next.fetch_add(self.chunk_bytes, Ordering::Relaxed);
        (start < self.total).then(|| start..start.saturating_add(self.chunk_bytes).min(self.total))
    }
}

/// Fill a `total`-byte temp file from a ranged first response plus parallel
/// range requests against the same (post-redirect) URL.
///
/// ## Postconditions
/// On success every byte in `0..total` was written exactly once and the file
/// length is `total`.
async fn download_ranged(
    first_response: reqwest::Response,
    first: u64,
    total: u64,
    output_path: &Path,
    media_http: &reqwest::Client,
    config: &EngineSettings,
) -> Result<(), Error> {
    let url = first_response.url().clone();
    let queue = ChunkQueue::new(first, config.pipeline.download_chunk_bytes, total);
    let extra_workers = queue.remaining_chunks().min(u64::from(
        config.pipeline.download_connections.saturating_sub(1),
    ));
    tracing::debug!(
        total_bytes = total,
        first_bytes = first,
        chunks = queue.remaining_chunks() + 1,
        connections = extra_workers + 1,
        "Starting ranged download"
    );

    preallocate_download_file(output_path, total).await;

    let request_range = |range: Range<u64>| {
        let request = media_http
            .get(url.clone())
            .header(RANGE, range_header_value(&range));
        async move {
            let response = request.send().await.map_err(request_error)?;
            let served = content_range(response.headers());
            if response.status() != StatusCode::PARTIAL_CONTENT
                || served
                    != Some(ContentRange {
                        start: range.start,
                        end: range.end,
                        total,
                    })
            {
                return Err(protocol_error("range response does not match request"));
            }
            Ok(response)
        }
    };

    let mut workers = Vec::with_capacity(extra_workers as usize + 1);
    workers.push(range_worker(
        Some((first_response, 0..first)),
        &queue,
        output_path,
        config,
        &request_range,
    ));
    for _ in 0..extra_workers {
        workers.push(range_worker(
            None,
            &queue,
            output_path,
            config,
            &request_range,
        ));
    }
    futures_util::future::try_join_all(workers).await?;

    let file = fs::OpenOptions::new()
        .write(true)
        .open(output_path)
        .await
        .map_err(write_error)?;
    file.set_len(total).await.map_err(write_error)
}

async fn range_worker<F, Fut>(
    initial: Option<(reqwest::Response, Range<u64>)>,
    queue: &ChunkQueue,
    output_path: &Path,
    config: &EngineSettings,
    request_range: &F,
) -> Result<(), Error>
where
    F: Fn(Range<u64>) -> Fut,
    Fut: Future<Output = Result<reqwest::Response, Error>>,
{
    let mut file = fs::OpenOptions::new()
        .write(true)
        .open(output_path)
        .await
        .map_err(write_error)?;
    let buffer_bytes = config.pipeline.download_write_buffer_bytes;

    if let Some((response, range)) = initial {
        write_range(response, range, &mut file, buffer_bytes).await?;
    }
    while let Some(range) = queue.claim() {
        let response = request_range(range.clone()).await?;
        write_range(response, range, &mut file, buffer_bytes).await?;
    }
    Ok(())
}

/// Stream one range response to its offset in the temp file, rejecting bodies
/// that do not match the range length exactly.
async fn write_range(
    response: reqwest::Response,
    range: Range<u64>,
    file: &mut fs::File,
    buffer_bytes: usize,
) -> Result<(), Error> {
    let expected = range.end - range.start;
    file.seek(SeekFrom::Start(range.start))
        .await
        .map_err(write_error)?;
    let mut writer = BufWriter::with_capacity(buffer_bytes, file);
    let mut stream = response.bytes_stream();
    let mut written = 0u64;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(request_error)?;
        written = written.saturating_add(chunk.len() as u64);
        if written > expected {
            return Err(protocol_error("range response exceeded requested length"));
        }
        writer.write_all(&chunk).await.map_err(write_error)?;
    }
    if written != expected {
        return Err(protocol_error("range response ended early"));
    }
    writer.flush().await.map_err(write_error)
}

/// Parsed `Content-Range: bytes start-last/total`, stored half-open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ContentRange {
    start: u64,
    end: u64,
    total: u64,
}

fn content_range(headers: &HeaderMap) -> Option<ContentRange> {
    parse_content_range(headers.get(CONTENT_RANGE)?.to_str().ok()?)
}

fn parse_content_range(value: &str) -> Option<ContentRange> {
    let (range, total) = value.trim().strip_prefix("bytes ")?.split_once('/')?;
    let (start, last) = range.split_once('-')?;
    let start = start.parse::<u64>().ok()?;
    let end = last.parse::<u64>().ok()?.checked_add(1)?;
    let total = total.parse::<u64>().ok()?;
    (start < end && end <= total).then_some(ContentRange { start, end, total })
}

fn range_header_value(range: &Range<u64>) -> String {
    format!("bytes={}-{}", range.start, range.end - 1)
}

fn request_error(error: reqwest::Error) -> Error {
    if is_blocked_address(&error) {
        return blocked_address_error();
    }
    Error::DownloadFailed {
        source: DownloadError::WriteFailed(std::io::Error::other(error)),
    }
}

fn write_error(error: std::io::Error) -> Error {
    Error::DownloadFailed {
        source: DownloadError::WriteFailed(error),
    }
}

fn protocol_error(message: &'static str) -> Error {
    Error::DownloadFailed {
        source: DownloadError::WriteFailed(std::io::Error::other(message)),
    }
}

/// Best-effort preallocation; writes still succeed on filesystems without it.
async fn preallocate_download_file(output_path: &Path, total: u64) -> bool {
    let path = output_path.to_path_buf();
    match tokio::task::spawn_blocking(move || preallocate_file(&path, total)).await {
        Ok(Ok(())) => {
            tracing::trace!(path = %output_path.display(), "Preallocated download file");
            true
        }
        Ok(Err(e)) => {
            tracing::warn!(error = %e, "Failed to preallocate download file");
            false
        }
        Err(e) => {
            tracing::warn!(error = %e, "Preallocation task failed");
            false
        }
    }
}

async fn open_download_file(
    output_path: &Path,
    total_size: Option<u64>,
    config: &EngineSettings,
) -> Result<BufWriter<fs::File>, Error> {
    let preallocated = match total_size.filter(|size| *size > 0) {
        Some(total) => preallocate_download_file(output_path, total).await,
        None => false,
    };

    let file = fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(!preallocated)
        .open(output_path)
        .await?;
    Ok(BufWriter::with_capacity(
        config.pipeline.download_write_buffer_bytes,
        file,
    ))
}

async fn transition_memory_download_to_file(
    file: &mut Option<BufWriter<fs::File>>,
    output_path: &Path,
    total_size: Option<u64>,
    config: &EngineSettings,
    buffered: Option<Vec<u8>>,
) -> Result<(), Error> {
    let mut opened = open_download_file(output_path, total_size, config).await?;
    if let Some(buffered) = buffered
        && !buffered.is_empty()
    {
        opened.write_all(&buffered).await.map_err(write_error)?;
    }
    *file = Some(opened);
    Ok(())
}

async fn finish_download_file(mut file: BufWriter<fs::File>, downloaded: u64) -> Result<(), Error> {
    file.flush().await.map_err(write_error)?;
    let mut file = file.into_inner();
    file.set_len(downloaded).await.map_err(write_error)?;
    file.flush().await.map_err(write_error)
}

fn bounded_upload_ready_buffer(
    total_size: Option<u64>,
    config: &EngineSettings,
) -> Option<Vec<u8>> {
    let buffer_limit = upload_ready_buffer_limit(config);
    if buffer_limit == 0 || total_size.is_some_and(|size| size > buffer_limit) {
        return None;
    }

    let capacity = total_size
        .unwrap_or(0)
        .min(buffer_limit)
        .try_into()
        .unwrap_or(0);
    // Retain pass-through bytes only for under-limit candidates. This bounds
    // extra memory to one upload-sized buffer per eligible in-flight job.
    Some(Vec::with_capacity(capacity))
}

fn can_keep_download_memory_only(
    total_size: Option<u64>,
    should_probe: bool,
    config: &EngineSettings,
) -> bool {
    !should_probe
        && total_size.is_some_and(|size| size > 0 && size <= upload_ready_buffer_limit(config))
}

fn upload_ready_buffer_limit(config: &EngineSettings) -> u64 {
    config
        .pipeline
        .upload_ready_buffer_max_bytes
        .min(config.transcode.max_upload_bytes)
}

/// Build the shared client for media downloads.
///
/// ## Rationale
/// - One client (one connection pool) for every media host keeps TLS
///   connections warm across jobs; SSRF address checks run in its resolver.
/// - HTTP/1.1 only: HTTP/2 would multiplex every concurrent download and
///   every range chunk onto a single TCP connection per host, sharing one
///   congestion window and defeating parallel ranges. With HTTP/1.1 the pool
///   opens one connection per in-flight request and keeps them alive.
/// - No transparent decompression: media is already compressed, and an
///   encoded body would make `Content-Length`/`Content-Range` refer to bytes
///   other than the ones written to disk.
pub(crate) fn build_media_client(config: &EngineSettings) -> anyhow::Result<reqwest::Client> {
    Ok(crate::engine::base_client_builder(config)
        .http1_only()
        .gzip(false)
        .brotli(false)
        .deflate(false)
        .dns_resolver(crate::pipeline::ssrf::Resolver)
        .build()?)
}

async fn fetch_with_redirects(
    media_http: &reqwest::Client,
    start_url: ValidatedMediaUrl,
    range: Option<Range<u64>>,
) -> Result<reqwest::Response, Error> {
    let range = range.as_ref().map(range_header_value);
    fetch_with_redirects_inner(
        start_url,
        |validated| {
            let mut request = media_http.get(validated.url);
            if let Some(range) = &range {
                request = request.header(RANGE, range);
            }
            async move {
                let response = request.send().await.map_err(request_error)?;

                if !response.status().is_redirection() {
                    return Ok(FetchStep::Complete(response));
                }

                let location = response
                    .headers()
                    .get(LOCATION)
                    .ok_or_else(|| protocol_error("redirect missing location header"))?
                    .to_str()
                    .map_err(|e| Error::DownloadFailed {
                        source: DownloadError::WriteFailed(std::io::Error::other(e)),
                    })?;

                Ok(FetchStep::Redirect {
                    base: response.url().clone(),
                    location: location.into(),
                })
            }
        },
        |next| validate_media_url(next.as_str()),
    )
    .await
}

const MAX_REDIRECTS: usize = 5;

enum FetchStep<T> {
    Complete(T),
    Redirect {
        base: reqwest::Url,
        location: Box<str>,
    },
}

async fn fetch_with_redirects_inner<T, Fetch, FetchFuture, Validate>(
    start_url: ValidatedMediaUrl,
    mut fetch: Fetch,
    mut validate_redirect: Validate,
) -> Result<T, Error>
where
    Fetch: FnMut(ValidatedMediaUrl) -> FetchFuture,
    FetchFuture: Future<Output = Result<FetchStep<T>, Error>>,
    Validate: FnMut(reqwest::Url) -> Result<ValidatedMediaUrl, Error>,
{
    let mut current = start_url;
    for hop in 0..=MAX_REDIRECTS {
        tracing::trace!(hop, url = %current.url, "Fetching media URL");
        match fetch(current).await? {
            FetchStep::Complete(response) => return Ok(response),
            FetchStep::Redirect { base, location } => {
                if hop == MAX_REDIRECTS {
                    tracing::warn!(
                        max_redirects = MAX_REDIRECTS,
                        "Too many redirects while downloading media"
                    );
                    return Err(protocol_error("too many redirects"));
                }

                let next = base
                    .join(location.as_ref())
                    .map_err(|e| Error::DownloadFailed {
                        source: DownloadError::WriteFailed(std::io::Error::other(e)),
                    })?;

                tracing::trace!(hop, location, next = %next, "Following redirect");
                current = validate_redirect(next)?;
            }
        }
    }

    Err(protocol_error("redirect loop"))
}

struct ProbeResult {
    duration: Option<f64>,
    resolution: Option<(u32, u32)>,
    facts: MediaFacts,
}

#[derive(Debug, Deserialize)]
struct FfprobeOutput {
    #[serde(default)]
    format: Option<FfprobeFormat>,
    #[serde(default)]
    streams: Vec<FfprobeStream>,
}

#[derive(Debug, Deserialize)]
struct FfprobeFormat {
    #[serde(default)]
    duration: Option<Box<str>>,
    #[serde(default)]
    format_name: Option<Box<str>>,
    #[serde(default)]
    bit_rate: Option<Box<str>>,
}

#[derive(Debug, Deserialize)]
struct FfprobeStream {
    #[serde(default)]
    codec_type: Option<Box<str>>,
    #[serde(default)]
    codec_name: Option<Box<str>>,
    #[serde(default)]
    width: Option<u32>,
    #[serde(default)]
    height: Option<u32>,
}

/// Probe the file with ffprobe to fill in missing duration/resolution.
///
/// ## Rationale
/// We only invoke ffprobe when the resolver could not supply metadata.
///
/// ## Performance hints
/// stdout/stderr are bounded to avoid retaining large logs in memory.
async fn probe_file(path: &Path, config: &EngineSettings) -> Result<ProbeResult, Error> {
    let timeout = Duration::from_secs(config.transcode.ffprobe_timeout_secs);

    const FFPROBE_OUTPUT_LIMIT: usize = 4 * 1024 * 1024;
    const FFPROBE_STDERR_LINES: usize = 10;
    // Output limits prevent unbounded logs from exhausting memory.

    let mut command = Command::new(&config.binaries.ffprobe);
    command
        .args([
            "-v",
            "quiet",
            "-print_format",
            "json",
            "-show_format",
            "-show_streams",
        ])
        .arg(path.as_os_str());
    let output = run_json_subprocess(
        &mut command,
        timeout,
        FFPROBE_OUTPUT_LIMIT,
        FFPROBE_STDERR_LINES,
    )
    .await
    .map_err(|error| match error {
        JsonSubprocessError::Io(error) => Error::DownloadFailed {
            source: DownloadError::WriteFailed(error),
        },
        JsonSubprocessError::Timeout => Error::Timeout {
            operation: "ffprobe",
            duration: timeout,
        },
        JsonSubprocessError::OutputLimit { .. } => Error::DownloadFailed {
            source: DownloadError::WriteFailed(std::io::Error::other(
                "ffprobe output exceeded limit",
            )),
        },
    })?;

    if !output.status.success() {
        return Err(Error::DownloadFailed {
            source: DownloadError::WriteFailed(std::io::Error::other("ffprobe returned error")),
        });
    }

    // Parse is best-effort; missing fields are tolerated downstream.
    let probe: FfprobeOutput =
        serde_json::from_slice(output.stdout.as_slice()).map_err(|e| Error::DownloadFailed {
            source: DownloadError::WriteFailed(std::io::Error::other(e)),
        })?;

    let format = probe.format.as_ref();
    let video_stream = probe
        .streams
        .iter()
        .find(|stream| stream.codec_type.as_deref() == Some("video"));
    let audio_stream = probe
        .streams
        .iter()
        .find(|stream| stream.codec_type.as_deref() == Some("audio"));

    let duration = format
        .and_then(|format| format.duration.as_deref())
        .and_then(|s| s.parse::<f64>().ok());

    let resolution = video_stream.and_then(|stream| Some((stream.width?, stream.height?)));

    let facts = MediaFacts {
        container: format
            .and_then(|format| format.format_name.as_deref())
            .map(MediaContainer::from_ffprobe_name)
            .unwrap_or_default(),
        video_codec: video_stream
            .and_then(|stream| stream.codec_name.as_deref())
            .map(VideoCodec::from_ffprobe_name)
            .unwrap_or_default(),
        audio_codec: audio_stream
            .and_then(|stream| stream.codec_name.as_deref())
            .map(AudioCodec::from_ffprobe_name)
            .unwrap_or(AudioCodec::None),
        bitrate_kbps: format
            .and_then(|format| format.bit_rate.as_deref())
            .and_then(parse_bitrate_kbps),
    };

    Ok(ProbeResult {
        duration,
        resolution,
        facts,
    })
}

/// Reserve file space up front to keep writes contiguous when possible.
///
/// Uses `fallocate` on Linux and falls back to a sparse `set_len` elsewhere or
/// when the filesystem rejects it.
fn preallocate_file(path: &Path, size: u64) -> std::io::Result<()> {
    tracing::trace!(path = %path.display(), size_bytes = size, "Preallocating file");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(path)?;

    #[cfg(target_os = "linux")]
    {
        use nix::fcntl::{FallocateFlags, fallocate};

        if size == 0 {
            file.set_len(0)?;
            return Ok(());
        }

        if let Err(error) = fallocate(&file, FallocateFlags::empty(), 0, size as i64) {
            tracing::warn!(error = %error, "fallocate failed; falling back to set_len");
            file.set_len(size)?;
        }

        Ok(())
    }

    #[cfg(not(target_os = "linux"))]
    {
        file.set_len(size)?;
        Ok(())
    }
}

fn estimate_bitrate_kbps(size_bytes: u64, duration: Option<f64>) -> Option<u32> {
    let duration = duration?;
    if !(duration.is_finite() && duration > 0.1) {
        return None;
    }

    let bitrate_kbps = (size_bytes as f64 * 8.0 / duration / 1000.0).ceil();
    if !(bitrate_kbps.is_finite() && bitrate_kbps > 0.0 && bitrate_kbps <= u32::MAX as f64) {
        return None;
    }

    Some(bitrate_kbps as u32)
}

fn parse_bitrate_kbps(bit_rate: &str) -> Option<u32> {
    let bits_per_sec = bit_rate.parse::<f64>().ok()?;
    if !(bits_per_sec.is_finite() && bits_per_sec > 0.0) {
        return None;
    }

    let bitrate_kbps = (bits_per_sec / 1000.0).ceil();
    if bitrate_kbps > u32::MAX as f64 {
        return None;
    }

    Some(bitrate_kbps as u32)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::panic)]

    use super::{
        Body, ChunkQueue, ContentRange, FetchStep, MAX_REDIRECTS, bounded_upload_ready_buffer,
        can_keep_download_memory_only, classify_response, download_ranged, estimate_bitrate_kbps,
        fetch_with_redirects_inner, finish_download_file, parse_bitrate_kbps, parse_content_range,
        range_header_value, resolver_metadata_missing, transition_memory_download_to_file,
        upload_ready_buffer_limit,
    };
    use crate::config::EngineSettings;
    use crate::pipeline::errors::{DownloadError, Error};
    use crate::pipeline::ssrf::ValidatedMediaUrl;
    use crate::pipeline::types::{MediaType, ResolvedMedia};
    use reqwest::Url;
    use std::borrow::Cow;
    use std::sync::{Arc, Mutex};
    use std::time::{SystemTime, UNIX_EPOCH};
    use tokio::io::AsyncWriteExt;
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
    use tokio::net::TcpListener;

    fn validated(url: Url) -> ValidatedMediaUrl {
        ValidatedMediaUrl { url }
    }

    fn unique_temp_file(name: &str) -> std::path::PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time should be after unix epoch")
            .as_nanos();
        std::env::temp_dir().join(format!("azalea-download-{name}-{nanos}.bin"))
    }

    #[test]
    fn estimates_bitrate_from_size_and_duration() {
        assert_eq!(estimate_bitrate_kbps(8_000_000, Some(10.0)), Some(6400));
        assert_eq!(estimate_bitrate_kbps(8_000_000, Some(0.0)), None);
    }

    #[test]
    fn parses_ffprobe_bitrate_in_kbps() {
        assert_eq!(parse_bitrate_kbps("1234567"), Some(1235));
        assert_eq!(parse_bitrate_kbps("nope"), None);
    }

    #[test]
    fn upload_ready_buffer_is_only_allocated_for_under_limit_candidates() {
        let config = EngineSettings::default();

        let bounded = bounded_upload_ready_buffer(
            Some(config.pipeline.upload_ready_buffer_max_bytes),
            &config,
        );
        assert!(bounded.is_some());

        let oversized = bounded_upload_ready_buffer(
            Some(config.pipeline.upload_ready_buffer_max_bytes + 1),
            &config,
        );
        assert!(oversized.is_none());
    }

    #[test]
    fn upload_ready_buffer_respects_memory_cap_below_upload_limit() {
        let mut config = EngineSettings::default();
        config.pipeline.upload_ready_buffer_max_bytes = 1024;

        let bounded = bounded_upload_ready_buffer(Some(1024), &config);
        assert_eq!(bounded.map(|buffer| buffer.capacity()), Some(1024));
        assert_eq!(bounded_upload_ready_buffer(Some(1025), &config), None);
    }

    #[test]
    fn upload_ready_streaming_limit_uses_memory_cap_not_upload_cap() {
        let mut config = EngineSettings::default();
        config.transcode.max_upload_bytes = 8 * 1024;
        config.pipeline.upload_ready_buffer_max_bytes = 1024;

        assert_eq!(upload_ready_buffer_limit(&config), 1024);
        assert!(bounded_upload_ready_buffer(None, &config).is_some());
    }

    #[test]
    fn memory_only_download_requires_known_under_limit_size_and_no_probe() {
        let mut config = EngineSettings::default();
        config.pipeline.upload_ready_buffer_max_bytes = 1024;

        assert!(can_keep_download_memory_only(Some(1024), false, &config));
        assert!(!can_keep_download_memory_only(Some(1025), false, &config));
        assert!(!can_keep_download_memory_only(None, false, &config));
        assert!(!can_keep_download_memory_only(Some(1024), true, &config));
    }

    #[tokio::test]
    async fn memory_download_transition_preserves_buffered_prefix() {
        let config = EngineSettings::default();
        let path = unique_temp_file("overflow-prefix");
        let mut file = None;

        transition_memory_download_to_file(
            &mut file,
            &path,
            Some(3),
            &config,
            Some(b"abc".to_vec()),
        )
        .await
        .expect("transition should open disk file");

        let mut file = file.expect("transition should install file sink");
        file.write_all(b"def").await.expect("write suffix");
        finish_download_file(file, 6)
            .await
            .expect("finish transitioned file");

        let contents = tokio::fs::read(&path)
            .await
            .expect("read transitioned file");
        assert_eq!(contents, b"abcdef");
        let _ = tokio::fs::remove_file(path).await;
    }

    #[tokio::test]
    async fn fetch_with_redirects_returns_depth_error_at_limit() {
        let start_url = Url::parse("https://pbs.twimg.com/media/start.mp4").expect("valid url");
        let fetches = Arc::new(Mutex::new(Vec::new()));
        let validations = Arc::new(Mutex::new(Vec::new()));

        let err = fetch_with_redirects_inner(
            validated(start_url),
            {
                let fetches = Arc::clone(&fetches);
                move |current| {
                    let fetches = Arc::clone(&fetches);
                    async move {
                        fetches
                            .lock()
                            .expect("fetch log should not be poisoned")
                            .push(current.url.as_str().to_string());

                        Ok(FetchStep::<()>::Redirect {
                            base: current.url,
                            location: format!(
                                "/media/hop-{}",
                                fetches
                                    .lock()
                                    .expect("fetch log should not be poisoned")
                                    .len()
                            )
                            .into(),
                        })
                    }
                }
            },
            {
                let validations = Arc::clone(&validations);
                move |next| {
                    validations
                        .lock()
                        .expect("validation log should not be poisoned")
                        .push(next.to_string());
                    Ok(validated(next))
                }
            },
        )
        .await
        .expect_err("redirect chain should hit the hop limit");

        let rendered = err.to_string();
        assert!(rendered.contains("too many redirects"));
        assert_eq!(
            fetches
                .lock()
                .expect("fetch log should not be poisoned")
                .len(),
            MAX_REDIRECTS + 1
        );
        assert_eq!(
            validations
                .lock()
                .expect("validation log should not be poisoned")
                .len(),
            MAX_REDIRECTS
        );
    }

    #[tokio::test]
    async fn fetch_with_redirects_revalidates_each_redirect_target_for_ssrf() {
        let start_url = Url::parse("https://pbs.twimg.com/media/start.mp4").expect("valid url");
        let fetches = Arc::new(Mutex::new(Vec::new()));
        let validations = Arc::new(Mutex::new(Vec::new()));

        let err = fetch_with_redirects_inner(
            validated(start_url),
            {
                let fetches = Arc::clone(&fetches);
                move |current| {
                    let fetches = Arc::clone(&fetches);
                    async move {
                        let mut fetches = fetches.lock().expect("fetch log should not be poisoned");
                        let hop = fetches.len();
                        fetches.push(current.url.as_str().to_string());

                        let location = match hop {
                            0 => "https://video.twimg.com/media/next.mp4",
                            1 => "https://127.0.0.1/private.mp4",
                            _ => {
                                return Ok(FetchStep::Complete(()));
                            }
                        };

                        Ok(FetchStep::<()>::Redirect {
                            base: current.url,
                            location: location.into(),
                        })
                    }
                }
            },
            {
                let validations = Arc::clone(&validations);
                move |next| {
                    validations
                        .lock()
                        .expect("validation log should not be poisoned")
                        .push(next.to_string());

                    if next.as_str() == "https://127.0.0.1/private.mp4" {
                        Err(Error::DownloadFailed {
                            source: DownloadError::SsrfBlocked("ip literal rejected".to_string()),
                        })
                    } else {
                        Ok(validated(next))
                    }
                }
            },
        )
        .await
        .expect_err("ssrf validation should reject the second redirect");

        assert!(
            err.to_string()
                .contains("ssrf blocked: ip literal rejected")
        );
        assert_eq!(
            fetches
                .lock()
                .expect("fetch log should not be poisoned")
                .as_slice(),
            [
                "https://pbs.twimg.com/media/start.mp4",
                "https://video.twimg.com/media/next.mp4",
            ]
        );
        assert_eq!(
            validations
                .lock()
                .expect("validation log should not be poisoned")
                .as_slice(),
            [
                "https://video.twimg.com/media/next.mp4",
                "https://127.0.0.1/private.mp4",
            ]
        );
    }

    #[test]
    fn parses_content_range_as_half_open() {
        assert_eq!(
            parse_content_range("bytes 0-99/1000"),
            Some(ContentRange {
                start: 0,
                end: 100,
                total: 1000
            })
        );
        assert_eq!(parse_content_range("bytes 0-99/*"), None);
        assert_eq!(parse_content_range("bytes 5-4/10"), None);
        assert_eq!(parse_content_range("bytes 0-10/10"), None);
        assert_eq!(parse_content_range("items 0-9/10"), None);
        assert_eq!(range_header_value(&(100..200)), "bytes=100-199");
    }

    #[test]
    fn chunk_queue_covers_every_byte_once() {
        let queue = ChunkQueue::new(10, 4, 21);
        assert_eq!(queue.remaining_chunks(), 3);
        let claimed: Vec<_> = std::iter::from_fn(|| queue.claim()).collect();
        assert_eq!(claimed, [10..14, 14..18, 18..21]);
        assert_eq!(queue.claim(), None);
    }

    #[test]
    fn images_do_not_require_duration_metadata() {
        let mut resolved = ResolvedMedia {
            url: Cow::Borrowed("https://pbs.twimg.com/media/a.jpg"),
            media_type: MediaType::Image,
            duration: None,
            resolution: Some((1200, 800)),
            extension: "jpg".into(),
        };
        assert!(!resolver_metadata_missing(&resolved));

        resolved.media_type = MediaType::Video;
        assert!(resolver_metadata_missing(&resolved));
        resolved.duration = Some(9.3);
        assert!(!resolver_metadata_missing(&resolved));
    }

    /// Minimal keep-alive HTTP/1.1 server that honors single `Range` requests.
    async fn serve_ranges(payload: Arc<[u8]>) -> std::net::SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind test listener");
        let addr = listener.local_addr().expect("listener address");
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let payload = Arc::clone(&payload);
                tokio::spawn(async move {
                    let mut stream = BufReader::new(stream);
                    loop {
                        let mut range = None;
                        let mut line = String::new();
                        if stream.read_line(&mut line).await.unwrap_or(0) == 0 {
                            return;
                        }
                        loop {
                            line.clear();
                            if stream.read_line(&mut line).await.unwrap_or(0) == 0 {
                                return;
                            }
                            let header = line.trim_end();
                            if header.is_empty() {
                                break;
                            }
                            if let Some(value) = header.strip_prefix("range: bytes=") {
                                let (start, last) =
                                    value.split_once('-').expect("range must be start-last");
                                range = Some((
                                    start.parse::<usize>().expect("range start"),
                                    last.parse::<usize>().expect("range last"),
                                ));
                            }
                        }
                        let (start, last) = range.expect("test server only serves ranges");
                        let last = last.min(payload.len() - 1);
                        let body = payload.get(start..=last).expect("range within payload");
                        let head = format!(
                            "HTTP/1.1 206 Partial Content\r\ncontent-length: {}\r\ncontent-range: bytes {}-{}/{}\r\n\r\n",
                            body.len(),
                            start,
                            last,
                            payload.len()
                        );
                        let stream = stream.get_mut();
                        if stream.write_all(head.as_bytes()).await.is_err()
                            || stream.write_all(body).await.is_err()
                        {
                            return;
                        }
                    }
                });
            }
        });
        addr
    }

    #[tokio::test]
    async fn ranged_download_reassembles_payload_across_connections() {
        let payload: Arc<[u8]> = (0..3_000_000u32).map(|i| (i % 251) as u8).collect();
        let addr = serve_ranges(Arc::clone(&payload)).await;
        let client = reqwest::Client::builder()
            .no_proxy()
            .http1_only()
            .build()
            .expect("test client");

        let mut config = EngineSettings::default();
        config.pipeline.download_connections = 4;
        config.pipeline.download_chunk_bytes = 256 * 1024;
        config.pipeline.download_write_buffer_bytes = 64 * 1024;

        let first = client
            .get(format!("http://{addr}/media.mp4"))
            .header(
                reqwest::header::RANGE,
                range_header_value(&(0..config.pipeline.download_chunk_bytes)),
            )
            .send()
            .await
            .expect("initial range request");
        let body = classify_response(&first).expect("classify initial response");
        let Body::Ranged {
            first: first_len,
            total,
        } = body
        else {
            panic!("expected a ranged body, got {body:?}");
        };
        assert_eq!(total, payload.len() as u64);

        let path = unique_temp_file("ranged");
        download_ranged(first, first_len, total, &path, &client, &config)
            .await
            .expect("ranged download");

        let mut contents = Vec::new();
        tokio::fs::File::open(&path)
            .await
            .expect("open ranged output")
            .read_to_end(&mut contents)
            .await
            .expect("read ranged output");
        assert!(contents.as_slice() == payload.as_ref());
        let _ = tokio::fs::remove_file(path).await;
    }
}
