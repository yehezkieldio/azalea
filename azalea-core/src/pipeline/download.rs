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
use crate::pipeline::disk::ensure_disk_space;
use crate::pipeline::errors::{DownloadError, Error};
use crate::pipeline::process::{JsonSubprocessError, run_json_subprocess};
use crate::pipeline::ssrf::validate_media_url;
use crate::pipeline::types::{DownloadedFile, Job, ResolvedMedia, sanitize_extension};
use futures_util::StreamExt;
use serde::Deserialize;
use std::path::Path;
use std::time::{Duration, Instant};
use tokio::fs;
use tokio::io::AsyncWriteExt;
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

    tracing::trace!("Checking disk space");
    // Fail fast before allocating or downloading to avoid partial artifacts.
    ensure_disk_space(
        &config.storage.temp_dir,
        config.pipeline.min_disk_space_bytes,
    )
    .await?;

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

        let mut file = fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(!preallocated)
            .open(&output_path)
            .await?;
        let mut downloaded = 0u64;
        let mut last_log = std::time::Instant::now();
        let log_interval = Duration::from_secs(5);

        while let Some(chunk_result) = stream.next().await {
            let chunk = chunk_result.map_err(|e| Error::DownloadFailed {
                source: DownloadError::WriteFailed(std::io::Error::other(e)),
            })?;

            downloaded += chunk.len() as u64;

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
        file.set_len(downloaded)
            .await
            .map_err(|e| Error::DownloadFailed {
                source: DownloadError::WriteFailed(e),
            })?;

        file.flush().await.map_err(|e| Error::DownloadFailed {
            source: DownloadError::WriteFailed(e),
        })?;

        let (duration, resolution) = if resolved.duration.is_some() && !must_probe {
            // Prefer resolver metadata when Content-Length is known.
            tracing::trace!("Using metadata from resolver");
            (resolved.duration, resolved.resolution)
        } else {
            tracing::trace!("Probing downloaded file for metadata");
            match probe_file(&output_path, config)
                .instrument(tracing::info_span!("download.ffprobe", path = %output_path.display()))
                .await
            {
                Ok(result) => (result.duration, result.resolution),
                Err(e) => {
                    tracing::trace!(error = %e, "Probe failed");
                    if must_probe {
                        return Err(Error::DownloadFailed {
                            source: DownloadError::WriteFailed(std::io::Error::other(
                                "corrupt download",
                            )),
                        });
                    }
                    (None, None)
                }
            }
        };

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

async fn fetch_with_redirects(
    http: &reqwest::Client,
    start_url: reqwest::Url,
) -> Result<reqwest::Response, Error> {
    const MAX_REDIRECTS: usize = 5;

    let mut current = start_url;
    for hop in 0..=MAX_REDIRECTS {
        tracing::trace!(hop, url = %current, "Fetching media URL");
        let response =
            http.get(current.clone())
                .send()
                .await
                .map_err(|e| Error::DownloadFailed {
                    source: DownloadError::WriteFailed(std::io::Error::other(e)),
                })?;

        if response.status().is_redirection() {
            if hop == MAX_REDIRECTS {
                tracing::warn!(
                    max_redirects = MAX_REDIRECTS,
                    "Too many redirects while downloading media"
                );
                return Err(Error::DownloadFailed {
                    source: DownloadError::WriteFailed(std::io::Error::other("too many redirects")),
                });
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

            let next = response
                .url()
                .join(location)
                .map_err(|e| Error::DownloadFailed {
                    source: DownloadError::WriteFailed(std::io::Error::other(e)),
                })?;

            tracing::trace!(hop, location, next = %next, "Following redirect");
            current = validate_media_url(next.as_str()).await?;
            continue;
        }

        return Ok(response);
    }

    Err(Error::DownloadFailed {
        source: DownloadError::WriteFailed(std::io::Error::other("redirect loop")),
    })
}

struct ProbeResult {
    duration: Option<f64>,
    resolution: Option<(u32, u32)>,
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
}

#[derive(Debug, Deserialize)]
struct FfprobeStream {
    #[serde(default)]
    codec_type: Option<Box<str>>,
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

    let duration = probe
        .format
        .as_ref()
        .and_then(|format| format.duration.as_deref())
        .and_then(|s| s.parse::<f64>().ok());

    let resolution = probe
        .streams
        .iter()
        .find(|stream| stream.codec_type.as_deref() == Some("video"))
        .and_then(|stream| Some((stream.width?, stream.height?)));

    Ok(ProbeResult {
        duration,
        resolution,
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

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use super::content_length_header_bytes;
    use reqwest::header::{CONTENT_LENGTH, HeaderMap, HeaderValue};

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
}
