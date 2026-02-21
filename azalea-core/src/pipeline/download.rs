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
use crate::pipeline::process::{SubprocessGuard, kill_process_group, read_bounded};
use crate::pipeline::ssrf::validate_media_url;
use crate::pipeline::types::{DownloadedFile, Job, ResolvedMedia, sanitize_extension};
use futures_util::StreamExt;
use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, Instant};
use tokio::fs;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::sync::mpsc;

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

        let response = fetch_with_redirects(http, validated_url).await?;

        if !response.status().is_success() {
            return Err(Error::DownloadFailed {
                source: DownloadError::HttpStatus(response.status().as_u16()),
            });
        }

        let total_size = response.content_length();
        let must_probe = total_size.is_none();
        if let Some(total) = total_size {
            tracing::trace!(total_bytes = total, "Content length provided");
        } else {
            tracing::trace!("Content length unavailable");
        }
        let max_download = config.pipeline.max_download_bytes;
        // Early reject when the server reports a size already above the cap.
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
            match probe_file(&output_path, config).await {
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
        Err(_) => Err(Error::Timeout {
            operation: "download",
            duration: download_timeout,
        }),
    }
}

async fn fetch_with_redirects(
    http: &reqwest::Client,
    start_url: reqwest::Url,
) -> Result<reqwest::Response, Error> {
    const MAX_REDIRECTS: usize = 5;

    let mut current = start_url;
    for hop in 0..=MAX_REDIRECTS {
        let response =
            http.get(current.clone())
                .send()
                .await
                .map_err(|e| Error::DownloadFailed {
                    source: DownloadError::WriteFailed(std::io::Error::other(e)),
                })?;

        if response.status().is_redirection() {
            if hop == MAX_REDIRECTS {
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
        .arg(path.as_os_str())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut guard = SubprocessGuard::spawn(&mut command).map_err(|e| Error::DownloadFailed {
        source: DownloadError::WriteFailed(e),
    })?;

    let stdout = guard
        .child_mut()
        .stdout
        .take()
        .ok_or_else(|| Error::DownloadFailed {
            source: DownloadError::WriteFailed(std::io::Error::other("ffprobe stdout missing")),
        })?;
    let stderr = guard
        .child_mut()
        .stderr
        .take()
        .ok_or_else(|| Error::DownloadFailed {
            source: DownloadError::WriteFailed(std::io::Error::other("ffprobe stderr missing")),
        })?;
    let (limit_tx, mut limit_rx) = mpsc::channel::<()>(1);
    let limit_guard = limit_tx.clone();

    let stdout_handle = tokio::spawn(read_bounded(
        stdout,
        FFPROBE_OUTPUT_LIMIT,
        Some(limit_tx.clone()),
    ));
    let stderr_handle = tokio::spawn(read_bounded(stderr, FFPROBE_OUTPUT_LIMIT, Some(limit_tx)));

    enum WaitOutcome {
        Exit(std::io::Result<std::process::ExitStatus>),
        Timeout,
        OutputLimit,
    }

    let wait_outcome = tokio::select! {
        res = tokio::time::timeout(timeout, guard.wait()) => {
            match res {
                Ok(status) => WaitOutcome::Exit(status),
                Err(_) => WaitOutcome::Timeout,
            }
        }
        _ = limit_rx.recv() => WaitOutcome::OutputLimit,
    };
    drop(limit_guard);

    let status = match wait_outcome {
        WaitOutcome::Exit(status) => status.map_err(|e| Error::DownloadFailed {
            source: DownloadError::WriteFailed(e),
        })?,
        WaitOutcome::Timeout => {
            // Timeout: kill the whole process group to avoid orphaned ffprobe processes.
            kill_process_group(guard.child_mut()).await;
            let _ = guard.wait().await;
            stdout_handle.abort();
            stderr_handle.abort();
            return Err(Error::Timeout {
                operation: "ffprobe",
                duration: timeout,
            });
        }
        WaitOutcome::OutputLimit => {
            // Output limit hit: terminate subprocess to cap memory usage.
            kill_process_group(guard.child_mut()).await;
            let _ = guard.wait().await;
            stdout_handle.abort();
            stderr_handle.abort();
            return Err(Error::DownloadFailed {
                source: DownloadError::WriteFailed(std::io::Error::other(
                    "ffprobe output exceeded limit",
                )),
            });
        }
    };

    let stdout = stdout_handle
        .await
        .map_err(|e| Error::DownloadFailed {
            source: DownloadError::WriteFailed(std::io::Error::other(e.to_string())),
        })?
        .map_err(|e| Error::DownloadFailed {
            source: DownloadError::WriteFailed(e),
        })?;

    let stderr = stderr_handle
        .await
        .map_err(|e| Error::DownloadFailed {
            source: DownloadError::WriteFailed(std::io::Error::other(e.to_string())),
        })?
        .map_err(|e| Error::DownloadFailed {
            source: DownloadError::WriteFailed(e),
        })?;

    if stdout.exceeded || stderr.exceeded {
        return Err(Error::DownloadFailed {
            source: DownloadError::WriteFailed(std::io::Error::other(
                "ffprobe output exceeded limit",
            )),
        });
    }

    if !status.success() {
        return Err(Error::DownloadFailed {
            source: DownloadError::WriteFailed(std::io::Error::other("ffprobe returned error")),
        });
    }

    // Parse is best-effort; missing fields are tolerated downstream.
    let stdout = String::from_utf8_lossy(&stdout.data);
    let probe: serde_json::Value =
        serde_json::from_str(&stdout).map_err(|e| Error::DownloadFailed {
            source: DownloadError::WriteFailed(std::io::Error::other(e)),
        })?;

    let duration = probe
        .get("format")
        .and_then(|format| format.get("duration"))
        .and_then(|duration| duration.as_str())
        .and_then(|s| s.parse::<f64>().ok());

    let video_stream = probe.get("streams").and_then(|streams| {
        streams.as_array().and_then(|streams| {
            streams.iter().find(|stream| {
                stream.get("codec_type").and_then(|codec| codec.as_str()) == Some("video")
            })
        })
    });

    let resolution = video_stream.and_then(|vs| {
        let width = vs.get("width").and_then(|v| v.as_u64())? as u32;
        let height = vs.get("height").and_then(|v| v.as_u64())? as u32;
        Some((width, height))
    });

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
