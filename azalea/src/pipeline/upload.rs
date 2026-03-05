//! # Module overview
//! Upload stage for sending prepared media to Discord.
//!
//! ## Algorithm overview
//! Iterates over prepared files, uploads sequentially, and threads replies so
//! multi-part uploads are chained in a single conversation.
//!
//! ## Trade-off acknowledgment
//! We upload parts sequentially to simplify ordering and rate limit handling.

use crate::pipeline::{Error, Progress, UploadOutcome};
use azalea_core::concurrency::Permits;
use azalea_core::config::EngineSettings;
use azalea_core::media::TweetId;
use azalea_core::pipeline::PreparedUpload;
use std::hash::{BuildHasher, Hash, Hasher};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::fs;
use tokio::sync::mpsc;
use twilight_http::Client;
use twilight_http::api_error::ApiError;
use twilight_http::error::Error as TwilightError;
use twilight_http::error::ErrorType;
use twilight_http::response::StatusCode;
use twilight_model::http::attachment::Attachment;
use twilight_model::id::marker::MessageMarker;
use twilight_model::id::{Id, marker::ChannelMarker};

/// Upload prepared media, chaining messages when multiple parts are required.
///
/// ## Preconditions
/// - `prepared` comes from `azalea_core::pipeline::run`.
/// - `config` has been validated for upload size constraints.
///
/// ## Postconditions
/// - Returns message ids for downstream cleanup.
pub async fn upload(
    prepared: &PreparedUpload,
    channel_id: Id<ChannelMarker>,
    tweet_id: TweetId,
    discord: &Client,
    permits: &Permits,
    config: &EngineSettings,
    progress_tx: Option<&mpsc::Sender<Progress>>,
) -> Result<UploadOutcome, Error> {
    tracing::info!(
        channel_id = channel_id.get(),
        tweet_id = tweet_id.0,
        "Starting upload"
    );
    let _permit = permits.upload.acquire().await.map_err(|_| {
        Error::Core(azalea_core::pipeline::Error::Io(std::io::Error::other(
            "upload semaphore closed",
        )))
    })?;

    // Normalize upload inputs into a flat list of paths.
    let (paths, total_files) = match prepared {
        PreparedUpload::Single { path, .. } => (vec![path.clone()], 1),
        PreparedUpload::Split { paths, .. } => (paths.clone(), paths.len()),
    };
    tracing::info!(parts = total_files, "Uploading media");

    let mut first_message_id = None;
    let mut last_message_id = None;

    if paths.is_empty() {
        // Guard against unexpected empty payloads from the pipeline.
        return Err(Error::UploadFailed {
            part: 0,
            total: 0,
            source: Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "no upload files provided",
            )),
        });
    }

    for (index, file_path) in paths.iter().enumerate() {
        if let Some(tx) = progress_tx {
            let stage = if total_files > 1 {
                Progress::UploadingSegment(index + 1, total_files)
            } else {
                Progress::Uploading
            };
            let _ = tx.send(stage).await;
        }

        // Read each part with size enforcement before hitting the API.
        let file_size = file_size_checked(file_path, config).await?;
        tracing::trace!(
            part = index + 1,
            total = total_files,
            path = %file_path.display(),
            size_bytes = file_size,
            "Prepared upload payload"
        );
        let extension = file_path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("mp4");
        let filename = if total_files > 1 {
            format!("tweet_{}_part{}.{}", tweet_id.0, index + 1, extension)
        } else {
            format!("tweet_{}.{}", tweet_id.0, extension)
        };

        let ctx = UploadRetryContext {
            discord,
            channel_id,
            reply_to: last_message_id,
            config,
            part: index + 1,
            total: total_files,
        };

        let response = send_with_retry(&ctx, file_path, filename).await?;

        let message = response.model().await.map_err(|e| Error::UploadFailed {
            part: index + 1,
            total: total_files,
            source: Box::new(e),
        })?;

        if first_message_id.is_none() {
            // Capture the first message for cleanup and threading.
            first_message_id = Some(message.id);
        }
        // Keep the last message id to thread subsequent replies.
        last_message_id = Some(message.id);
        tracing::info!(
            part = index + 1,
            total = total_files,
            message_id = message.id.get(),
            "Upload part complete"
        );
    }

    Ok(UploadOutcome {
        first_message_id: first_message_id.ok_or_else(|| Error::UploadFailed {
            part: 0,
            total: total_files,
            source: Box::new(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "upload produced no messages",
            )),
        })?,
        messages_sent: total_files,
    })
}

/// Read the file into memory with a size check against the upload limit.
///
/// ## Security-sensitive paths
/// Ensures file size does not exceed configured upload caps before reading.
async fn file_size_checked(path: &Path, config: &EngineSettings) -> Result<u64, Error> {
    let metadata = fs::metadata(path)
        .await
        .map_err(|e| Error::Core(azalea_core::pipeline::Error::Io(e)))?;
    let file_size = metadata.len();

    tracing::trace!(
        path = %path.display(),
        size_bytes = file_size,
        "Reading upload file"
    );

    if file_size > config.transcode.max_upload_bytes {
        // Hard size limit to prevent Discord API errors.
        return Err(Error::UploadFailed {
            part: 1,
            total: 1,
            source: Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "file too large",
            )),
        });
    }

    if file_size > usize::MAX as u64 {
        // Avoid reading into memory on platforms where size exceeds addressable space.
        return Err(Error::UploadFailed {
            part: 1,
            total: 1,
            source: Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "file size exceeds addressable memory",
            )),
        });
    }

    Ok(file_size)
}

async fn read_file_with_limit(path: &Path, config: &EngineSettings) -> Result<Vec<u8>, Error> {
    let _ = file_size_checked(path, config).await?;

    let path = path.to_path_buf();
    // Use a blocking task for file IO to avoid starving async tasks.
    let data = tokio::task::spawn_blocking(move || read_file_preallocated(&path))
        .await
        .map_err(|e| Error::Core(azalea_core::pipeline::Error::Io(std::io::Error::other(e))))?
        .map_err(|e| Error::Core(azalea_core::pipeline::Error::Io(e)))?;
    Ok(data)
}

/// Read the file into a preallocated buffer to minimize reallocations.
///
/// ## Performance hints
/// Preallocation reduces reallocations for large uploads.
fn read_file_preallocated(path: &Path) -> std::io::Result<Vec<u8>> {
    let mut file = std::fs::File::open(path)?;
    let size = file.metadata()?.len();
    let capacity = checked_preallocation_len(size)?;
    // Preallocate to reduce reallocations for large uploads.
    let mut buf = Vec::with_capacity(capacity);
    use std::io::Read;
    file.read_to_end(&mut buf)?;
    Ok(buf)
}

fn checked_preallocation_len(size: u64) -> std::io::Result<usize> {
    if size > isize::MAX as u64 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "file size exceeds addressable memory",
        ));
    }

    usize::try_from(size).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "file size exceeds addressable memory",
        )
    })
}

struct UploadRetryContext<'a> {
    discord: &'a Client,
    channel_id: Id<ChannelMarker>,
    reply_to: Option<Id<MessageMarker>>,
    config: &'a EngineSettings,
    part: usize,
    total: usize,
}

async fn send_with_retry(
    ctx: &UploadRetryContext<'_>,
    file_path: &Path,
    filename: String,
) -> Result<twilight_http::Response<twilight_model::channel::Message>, Error> {
    const MAX_RETRIES: usize = 3;

    let mut attempt = 0usize;
    loop {
        // Re-read each attempt to avoid holding large buffers across retries.
        let file_bytes = read_file_with_limit(file_path, ctx.config).await?;
        let attachment = Attachment {
            description: None,
            file: file_bytes,
            filename: filename.clone(),
            id: 0,
        };

        let mut request = ctx.discord.create_message(ctx.channel_id);
        if let Some(reply_to) = ctx.reply_to {
            request = request.reply(reply_to);
        }

        match request.attachments(&[attachment]).await {
            Ok(response) => return Ok(response),
            Err(err) => {
                // Retry only for transient errors with a bounded backoff.
                if attempt < MAX_RETRIES
                    && let Some(delay) = retry_delay(attempt, &err)
                {
                    attempt += 1;
                    tracing::warn!(
                        part = ctx.part,
                        total = ctx.total,
                        attempt,
                        delay_ms = delay.as_millis(),
                        error = %err,
                        "Upload failed; retrying"
                    );
                    tokio::time::sleep(delay).await;
                    continue;
                }

                return Err(Error::UploadFailed {
                    part: ctx.part,
                    total: ctx.total,
                    source: Box::new(err),
                });
            }
        }
    }
}

/// Decide whether an upload error is worth retrying and compute backoff.
///
/// ## Rationale
/// Only transient failures (timeouts, 5xx, 429) are retried to avoid spamming
/// the API on permanent client errors.
fn retry_delay(attempt: usize, err: &TwilightError) -> Option<Duration> {
    match err.kind() {
        ErrorType::RequestTimedOut | ErrorType::RequestError => {
            Some(exponential_backoff(attempt, 500, 8_000))
        }
        ErrorType::Response { status, error, .. } => {
            if *status == StatusCode::TOO_MANY_REQUESTS {
                if let ApiError::Ratelimited(rate) = error {
                    // Respect Discord's retry-after header and add jitter.
                    let delay = Duration::from_secs_f64(rate.retry_after.max(0.0));
                    return Some(delay + Duration::from_millis(jitter_millis(250)));
                }
                return Some(exponential_backoff(attempt, 1_000, 15_000));
            }

            if status.is_server_error() || *status == 502 || *status == 503 || *status == 504 {
                // Retry on transient server-side errors.
                return Some(exponential_backoff(attempt, 500, 8_000));
            }

            None
        }
        ErrorType::RequestCanceled => Some(exponential_backoff(attempt, 500, 8_000)),
        _ => None,
    }
}

fn exponential_backoff(attempt: usize, base_ms: u64, max_ms: u64) -> Duration {
    let exponent = attempt.min(8) as u32;
    let multiplier = 1u64.checked_shl(exponent).unwrap_or(u64::MAX);
    let mut delay = base_ms.saturating_mul(multiplier);
    if delay > max_ms {
        delay = max_ms;
    }
    let jitter = jitter_millis(250);
    Duration::from_millis(delay.saturating_add(jitter))
}

fn jitter_millis(max_ms: u64) -> u64 {
    static JITTER_COUNTER: AtomicU64 = AtomicU64::new(0);

    if max_ms == 0 {
        return 0;
    }
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let counter = JITTER_COUNTER.fetch_add(1, Ordering::Relaxed);

    // Deterministic jitter without pulling in an RNG dependency.
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    nanos.hash(&mut hasher);
    std::thread::current().id().hash(&mut hasher);
    counter.hash(&mut hasher);

    hasher.finish() % max_ms
}

#[cfg(test)]
mod tests {
    use super::*;
    use azalea_core::config::EngineSettings;
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};

    fn unique_temp_file_path(label: &str) -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|value| value.as_nanos())
            .unwrap_or_default();
        let sequence = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("azalea-upload-{label}-{nanos}-{sequence}"))
    }

    fn write_temp_file(label: &str, bytes: &[u8]) -> PathBuf {
        let path = unique_temp_file_path(label);
        assert!(std::fs::write(&path, bytes).is_ok());
        path
    }

    fn cleanup_file(path: &Path) {
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn exponential_backoff_grows_and_caps() {
        let a = exponential_backoff(0, 500, 2_000);
        let b = exponential_backoff(1, 500, 2_000);
        let c = exponential_backoff(8, 500, 2_000);

        assert!(b >= a);
        assert!(c <= Duration::from_millis(2_249));
    }

    #[test]
    fn jitter_stays_within_requested_bound() {
        for _ in 0..32 {
            let jitter = jitter_millis(250);
            assert!(jitter < 250);
        }
        assert_eq!(jitter_millis(0), 0);
    }

    #[tokio::test]
    async fn file_size_checked_missing_file_returns_not_found() {
        let config = EngineSettings::default();
        let path = unique_temp_file_path("missing-size-check");
        cleanup_file(&path);

        let result = file_size_checked(&path, &config).await;
        assert!(matches!(
            result,
            Err(Error::Core(azalea_core::pipeline::Error::Io(ref err)))
                if err.kind() == std::io::ErrorKind::NotFound
        ));
    }

    #[tokio::test]
    async fn file_size_checked_oversized_file_is_rejected() {
        let path = write_temp_file("oversized-size-check", b"12345678");
        let mut config = EngineSettings::default();
        config.transcode.max_upload_bytes = 4;

        let result = file_size_checked(&path, &config).await;
        cleanup_file(&path);

        let rendered = result.as_ref().err().map(ToString::to_string);
        assert!(matches!(
            result,
            Err(Error::UploadFailed {
                part: 1,
                total: 1,
                ..
            })
        ));
        assert!(
            rendered
                .as_deref()
                .is_some_and(|message| message.contains("file too large"))
        );
    }

    #[tokio::test]
    async fn file_size_checked_normal_file_returns_size() {
        let payload = b"normal-file";
        let path = write_temp_file("normal-size-check", payload);
        let mut config = EngineSettings::default();
        config.transcode.max_upload_bytes = payload.len() as u64 + 1;

        let result = file_size_checked(&path, &config).await;
        cleanup_file(&path);

        assert_eq!(result.ok(), Some(payload.len() as u64));
    }

    #[test]
    fn read_file_preallocated_missing_file_returns_not_found() {
        let path = unique_temp_file_path("missing-read-preallocated");
        cleanup_file(&path);

        let result = read_file_preallocated(&path);
        assert!(matches!(
            result,
            Err(ref err) if err.kind() == std::io::ErrorKind::NotFound
        ));
    }

    #[test]
    fn read_file_preallocated_oversized_size_is_rejected() {
        let result = checked_preallocation_len((isize::MAX as u64).saturating_add(1));
        assert!(matches!(
            result,
            Err(ref err)
                if err.kind() == std::io::ErrorKind::InvalidInput
                    && err.to_string() == "file size exceeds addressable memory"
        ));
    }

    #[test]
    fn read_file_preallocated_normal_file_returns_contents() {
        let payload = b"normal-read";
        let path = write_temp_file("normal-read-preallocated", payload);
        let result = read_file_preallocated(&path);
        cleanup_file(&path);

        assert_eq!(result.ok().as_deref(), Some(payload.as_slice()));
    }
}
