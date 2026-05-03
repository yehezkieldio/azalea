//! # Module overview
//! Download stage with size limits, SSRF guards, and metadata probing.
//!
//! ## Trade-off acknowledgment
//! We prefer bounded reads and timeouts over maximum throughput to avoid
//! unbounded memory growth on untrusted inputs.
//!
//! ## Algorithm overview
//! 1. Validate disk space and size limits.
//! 2. Stream to a temp file with size caps.
//! 3. Probe metadata via ffprobe if missing.

use crate::concurrency::Permits;
use crate::config::EngineSettings;
use crate::media::TempFileCleanup;
use crate::pipeline::disk::{ensure_disk_space, reserve_download_bytes};
use crate::pipeline::errors::{DownloadError, Error};
use crate::pipeline::process::{JsonSubprocessError, run_json_subprocess};
use crate::pipeline::ssrf::validate_media_url;
use crate::pipeline::types::{
    AudioCodec, DownloadedFile, Job, MediaContainer, MediaFacts, MediaType, ResolvedMedia,
    VideoCodec, sanitize_extension,
};
use futures_util::StreamExt;
use serde::Deserialize;
use std::future::Future;
use std::path::Path;
use std::sync::{Arc, atomic::AtomicU64};
use std::time::{Duration, Instant};
use tokio::fs;
use tokio::io::{AsyncWriteExt, BufWriter};
use tokio::process::Command;
use tracing::Instrument as _;

/// Download media to a temp file while enforcing size and safety constraints.
///
/// ## Preconditions
/// - `resolved` comes from [`crate::pipeline::resolve::ResolverChain`].
/// - `config` has been validated via [`crate::config::EngineSettings::validate`].
///
/// ## Postconditions
/// - The returned [`DownloadedFile`] owns a temp guard for cleanup.
/// - File size is bounded by `pipeline.max_download_bytes`.
pub async fn download(
    resolved: &ResolvedMedia,
    job: &Job,
    http: &reqwest::Client,
    permits: &Permits,
    reserved_download_bytes: &Arc<AtomicU64>,
    temp_files: &TempFileCleanup,
    config: &EngineSettings,
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
        // SSRF guardrails: validate and canonicalize before any network I/O.
        let validated_url = validate_media_url(resolved.url.as_ref()).await?;

        let response = fetch_with_redirects(http, validated_url)
            .instrument(tracing::info_span!("download.redirects"))
            .await?;

        if !response.status().is_success() {
            return Err(Error::DownloadFailed {
                source: DownloadError::HttpStatus(response.status().as_u16()),
            });
        }

        let header_content_length = content_length_header_bytes(response.headers());
        let total_size = response.content_length();
        let must_probe = total_size.is_none();
        if let Some(total) = total_size {
            tracing::trace!(total_bytes = total, "Content length provided");
        } else {
            tracing::trace!("Content length unavailable");
        }
        let max_download = config.pipeline.max_download_bytes;
        // Early reject using the raw Content-Length header before streaming any bytes.
        if max_download > 0
            && let Some(total) = header_content_length
            && total > max_download
        {
            return Err(Error::DownloadFailed {
                source: DownloadError::TooLarge {
                    size_mb: total / 1024 / 1024,
                    max_mb: max_download / 1024 / 1024,
                },
            });
        }

        let reserve_bytes = header_content_length
            .or(total_size)
            .filter(|size| *size > 0)
            .or_else(|| (max_download > 0).then_some(max_download))
            .unwrap_or(0);
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

        let mut stream = response.bytes_stream();
        // Preallocate to reduce fragmentation and avoid repeated growth syscalls.
        let preallocated = if let Some(total) = total_size.filter(|size| *size > 0) {
            let path = output_path.clone();
            match tokio::task::spawn_blocking(move || preallocate_file(&path, total)).await {
                Ok(Ok(())) => true,
                Ok(Err(e)) => {
                    tracing::warn!(error = %e, "Failed to preallocate download file");
                    false
                }
                Err(e) => {
                    tracing::warn!(error = %e, "Preallocation task failed");
                    false
                }
            }
        } else {
            false
        };
        if preallocated {
            tracing::trace!(path = %output_path.display(), "Preallocated download file");
        }

        let file = fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(!preallocated)
            .open(&output_path)
            .await?;
        let mut file = BufWriter::with_capacity(config.pipeline.download_write_buffer_bytes, file);
        let mut downloaded = 0u64;
        let mut upload_ready_bytes = bounded_upload_ready_buffer(total_size, config);
        let mut last_log = std::time::Instant::now();
        let log_interval = Duration::from_secs(5);

        while let Some(chunk_result) = stream.next().await {
            let chunk = chunk_result.map_err(|e| Error::DownloadFailed {
                source: DownloadError::WriteFailed(std::io::Error::other(e)),
            })?;

            let next_downloaded = downloaded.saturating_add(chunk.len() as u64);
            if let Some(bytes) = upload_ready_bytes.as_mut() {
                if next_downloaded <= config.transcode.max_upload_bytes {
                    bytes.extend_from_slice(&chunk);
                } else {
                    upload_ready_bytes = None;
                }
            }

            downloaded = next_downloaded;

            if max_download > 0 && downloaded > max_download {
                // Drop the partially written file to avoid leaving oversized artifacts.
                drop(file);
                let _ = fs::remove_file(&output_path).await;
                return Err(Error::DownloadFailed {
                    source: DownloadError::TooLarge {
                        size_mb: downloaded / 1024 / 1024,
                        max_mb: max_download / 1024 / 1024,
                    },
                });
            }

            file.write_all(&chunk)
                .await
                .map_err(|e| Error::DownloadFailed {
                    source: DownloadError::WriteFailed(e),
                })?;

            if last_log.elapsed() >= log_interval {
                // Periodic progress logs keep visibility without flooding logs.
                let percent = total_size
                    .map(|total| (downloaded as f64 / total as f64 * 100.0) as u32)
                    .unwrap_or(0);
                tracing::info!(
                    percent,
                    downloaded_mb = %format!("{:.2}", downloaded as f64 / 1024.0 / 1024.0),
                    "Downloading media..."
                );
                last_log = std::time::Instant::now();
            }
        }

        if downloaded == 0 {
            // Empty bodies are treated as invalid media even if the HTTP status was OK.
            return Err(Error::DownloadFailed {
                source: DownloadError::EmptyResponse,
            });
        }

        if let Some(total) = total_size
            && downloaded != total
        {
            // Mismatch suggests an interrupted stream; treat as a hard failure.
            tracing::warn!(expected = total, downloaded, "Download size mismatch");
            return Err(Error::DownloadFailed {
                source: DownloadError::WriteFailed(std::io::Error::other("download incomplete")),
            });
        }

        // Explicitly set final length in case the preallocation overshot.
        file.flush().await.map_err(|e| Error::DownloadFailed {
            source: DownloadError::WriteFailed(e),
        })?;
        let mut file = file.into_inner();
        file.set_len(downloaded)
            .await
            .map_err(|e| Error::DownloadFailed {
                source: DownloadError::WriteFailed(e),
            })?;

        file.flush().await.map_err(|e| Error::DownloadFailed {
            source: DownloadError::WriteFailed(e),
        })?;

        let should_probe = must_probe
            || resolved.duration.is_none()
            || resolved.resolution.is_none()
            || (resolved.media_type == MediaType::Video
                && downloaded > config.transcode.max_upload_bytes);

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
            // Prefer resolver metadata when Content-Length is known and
            // optimization preflight does not need additional facts.
            tracing::trace!("Using metadata from resolver");
        }

        facts.bitrate_kbps = facts
            .bitrate_kbps
            .or_else(|| estimate_bitrate_kbps(downloaded, duration));

        tracing::info!(
            duration_ms = download_start.elapsed().as_millis(),
            size_bytes = downloaded,
            path = %output_path.display(),
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

fn bounded_upload_ready_buffer(
    total_size: Option<u64>,
    config: &EngineSettings,
) -> Option<Vec<u8>> {
    let max_upload_bytes = config.transcode.max_upload_bytes;
    let buffer_limit = config
        .pipeline
        .upload_ready_buffer_max_bytes
        .min(max_upload_bytes);
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

async fn fetch_with_redirects(
    http: &reqwest::Client,
    start_url: reqwest::Url,
) -> Result<reqwest::Response, Error> {
    fetch_with_redirects_inner(
        start_url,
        |current| async move {
            let response =
                http.get(current.clone())
                    .send()
                    .await
                    .map_err(|e| Error::DownloadFailed {
                        source: DownloadError::WriteFailed(std::io::Error::other(e)),
                    })?;

            if !response.status().is_redirection() {
                return Ok(FetchStep::Complete(response));
            }

            let location = response
                .headers()
                .get(reqwest::header::LOCATION)
                .ok_or_else(|| Error::DownloadFailed {
                    source: DownloadError::WriteFailed(std::io::Error::other(
                        "redirect missing location header",
                    )),
                })?
                .to_str()
                .map_err(|e| Error::DownloadFailed {
                    source: DownloadError::WriteFailed(std::io::Error::other(e)),
                })?;

            Ok(FetchStep::Redirect {
                base: response.url().clone(),
                location: location.into(),
            })
        },
        |next| async move { validate_media_url(next.as_str()).await },
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

async fn fetch_with_redirects_inner<T, Fetch, FetchFuture, Validate, ValidateFuture>(
    start_url: reqwest::Url,
    mut fetch: Fetch,
    mut validate_redirect: Validate,
) -> Result<T, Error>
where
    Fetch: FnMut(reqwest::Url) -> FetchFuture,
    FetchFuture: Future<Output = Result<FetchStep<T>, Error>>,
    Validate: FnMut(reqwest::Url) -> ValidateFuture,
    ValidateFuture: Future<Output = Result<reqwest::Url, Error>>,
{
    let mut current = start_url;
    for hop in 0..=MAX_REDIRECTS {
        tracing::trace!(hop, url = %current, "Fetching media URL");
        match fetch(current.clone()).await? {
            FetchStep::Complete(response) => return Ok(response),
            FetchStep::Redirect { base, location } => {
                if hop == MAX_REDIRECTS {
                    tracing::warn!(
                        max_redirects = MAX_REDIRECTS,
                        "Too many redirects while downloading media"
                    );
                    return Err(Error::DownloadFailed {
                        source: DownloadError::WriteFailed(std::io::Error::other(
                            "too many redirects",
                        )),
                    });
                }

                let next = base
                    .join(location.as_ref())
                    .map_err(|e| Error::DownloadFailed {
                        source: DownloadError::WriteFailed(std::io::Error::other(e)),
                    })?;

                tracing::trace!(hop, location, next = %next, "Following redirect");
                current = validate_redirect(next).await?;
            }
        }
    }

    Err(Error::DownloadFailed {
        source: DownloadError::WriteFailed(std::io::Error::other("redirect loop")),
    })
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
/// ## Trade-off acknowledgment
/// We use `set_len` rather than platform-specific fallocate to avoid unsafe
/// syscalls and keep portability.
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

fn content_length_header_bytes(headers: &reqwest::header::HeaderMap) -> Option<u64> {
    headers
        .get(reqwest::header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
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
    #![allow(clippy::expect_used)]

    use super::{
        FetchStep, MAX_REDIRECTS, bounded_upload_ready_buffer, content_length_header_bytes,
        estimate_bitrate_kbps, fetch_with_redirects_inner, parse_bitrate_kbps,
    };
    use crate::config::EngineSettings;
    use crate::pipeline::errors::{DownloadError, Error};
    use reqwest::Url;
    use reqwest::header::{CONTENT_LENGTH, HeaderMap, HeaderValue};
    use std::sync::{Arc, Mutex};

    #[test]
    fn parses_content_length_header_bytes() {
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_LENGTH, HeaderValue::from_static("5242881"));

        assert_eq!(content_length_header_bytes(&headers), Some(5_242_881));
    }

    #[test]
    fn ignores_invalid_content_length_header() {
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_LENGTH, HeaderValue::from_static("abc"));

        assert_eq!(content_length_header_bytes(&headers), None);
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

    #[tokio::test]
    async fn fetch_with_redirects_returns_depth_error_at_limit() {
        let start_url = Url::parse("https://pbs.twimg.com/media/start.mp4").expect("valid url");
        let fetches = Arc::new(Mutex::new(Vec::new()));
        let validations = Arc::new(Mutex::new(Vec::new()));

        let err = fetch_with_redirects_inner(
            start_url,
            {
                let fetches = Arc::clone(&fetches);
                move |current| {
                    let fetches = Arc::clone(&fetches);
                    async move {
                        fetches
                            .lock()
                            .expect("fetch log should not be poisoned")
                            .push(current.as_str().to_string());

                        Ok(FetchStep::<()>::Redirect {
                            base: current,
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
                    let parsed = next;
                    async move { Ok(parsed) }
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
            start_url,
            {
                let fetches = Arc::clone(&fetches);
                move |current| {
                    let fetches = Arc::clone(&fetches);
                    async move {
                        let mut fetches = fetches.lock().expect("fetch log should not be poisoned");
                        let hop = fetches.len();
                        fetches.push(current.as_str().to_string());

                        let location = match hop {
                            0 => "https://video.twimg.com/media/next.mp4",
                            1 => "https://127.0.0.1/private.mp4",
                            _ => {
                                return Ok(FetchStep::Complete(()));
                            }
                        };

                        Ok(FetchStep::<()>::Redirect {
                            base: current,
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

                    let result = if next.as_str() == "https://127.0.0.1/private.mp4" {
                        Err(Error::DownloadFailed {
                            source: DownloadError::SsrfBlocked("ip literal rejected".to_string()),
                        })
                    } else {
                        Ok(next)
                    };

                    async move { result }
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
}
