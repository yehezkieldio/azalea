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
use azalea_core::pipeline::{PreparedPart, PreparedUpload};
use std::hash::{Hash, Hasher};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::sync::mpsc;
use tracing::Instrument as _;
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
    tracing::trace!("Entered upload stage");
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

    let total_files = prepared.len();
    tracing::info!(parts = total_files, "Uploading media");

    match prepared {
        PreparedUpload::Single { part, .. } => {
            upload_parts(
                std::slice::from_ref(part),
                channel_id,
                tweet_id,
                discord,
                config,
                progress_tx,
            )
            .await
        }
        PreparedUpload::Split { parts, .. } => {
            upload_parts(parts, channel_id, tweet_id, discord, config, progress_tx).await
        }
    }
}

async fn upload_parts(
    parts: &[PreparedPart],
    channel_id: Id<ChannelMarker>,
    tweet_id: TweetId,
    discord: &Client,
    config: &EngineSettings,
    progress_tx: Option<&mpsc::Sender<Progress>>,
) -> Result<UploadOutcome, Error> {
    if parts.is_empty() {
        return Err(Error::UploadFailed {
            part: 0,
            total: 0,
            source: Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "no upload files provided",
            )),
        });
    }

    let total_files = parts.len();
    let mut first_message_id = None;
    let mut last_message_id = None;

    for (index, part) in parts.iter().enumerate() {
        if let Some(tx) = progress_tx {
            let stage = if total_files > 1 {
                Progress::UploadingSegment(index + 1, total_files)
            } else {
                Progress::Uploading
            };
            let _ = tx.send(stage).await;
        }

        let file_path = part.path();
        let file_size = file_size_checked(part, config, index + 1, total_files)?;
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

        let response = send_with_retry(&ctx, part, filename)
            .instrument(tracing::info_span!(
                "upload.part",
                part = index + 1,
                total = total_files,
                path = %file_path.display()
            ))
            .await?;

        let message = response.model().await.map_err(|e| Error::UploadFailed {
            part: index + 1,
            total: total_files,
            source: Box::new(e),
        })?;

        if first_message_id.is_none() {
            first_message_id = Some(message.id);
        }
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

fn file_size_checked(
    part: &PreparedPart,
    config: &EngineSettings,
    part_number: usize,
    total: usize,
) -> Result<u64, Error> {
    let file_size = part.size();

    tracing::trace!(
        path = %part.path().display(),
        size_bytes = file_size,
        "Reading upload file"
    );

    if file_size > config.transcode.max_upload_bytes {
        return Err(Error::UploadFailed {
            part: part_number,
            total,
            source: Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "file too large",
            )),
        });
    }

    if file_size > usize::MAX as u64 {
        return Err(Error::UploadFailed {
            part: part_number,
            total,
            source: Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "file size exceeds addressable memory",
            )),
        });
    }

    Ok(file_size)
}

async fn read_file_with_limit(
    part: &PreparedPart,
    config: &EngineSettings,
    part_number: usize,
    total: usize,
) -> Result<Vec<u8>, Error> {
    let size = file_size_checked(part, config, part_number, total)?;

    let path = part.path().to_path_buf();
    // Use a blocking task for file IO to avoid starving async tasks.
    let data = tokio::task::spawn_blocking(move || read_file_preallocated(&path, size))
        .await
        .map_err(|e| Error::Core(azalea_core::pipeline::Error::Io(std::io::Error::other(e))))?
        .map_err(|e| Error::Core(azalea_core::pipeline::Error::Io(e)))?;
    Ok(data)
}

/// Read the file into a preallocated buffer to minimize reallocations.
///
/// ## Performance hints
/// Preallocation reduces reallocations for large uploads.
fn read_file_preallocated(path: &Path, size: u64) -> std::io::Result<Vec<u8>> {
    let mut file = std::fs::File::open(path)?;
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
    part: &PreparedPart,
    filename: String,
) -> Result<twilight_http::Response<twilight_model::channel::Message>, Error> {
    const MAX_RETRIES: usize = 3;

    let attachment = Attachment::from_bytes(
        filename,
        read_file_with_limit(part, ctx.config, ctx.part, ctx.total).await?,
        0,
    );
    let mut attempt = 0usize;
    loop {
        tracing::trace!(
            part = ctx.part,
            total = ctx.total,
            attempt = attempt + 1,
            path = %part.path().display(),
            "Sending upload attempt"
        );

        let mut request = ctx.discord.create_message(ctx.channel_id);
        if let Some(reply_to) = ctx.reply_to {
            request = request.reply(reply_to);
        }

        match request.attachments(std::slice::from_ref(&attachment)).await {
            Ok(response) => {
                tracing::info!(
                    part = ctx.part,
                    total = ctx.total,
                    attempt = attempt + 1,
                    "Upload request succeeded"
                );
                return Ok(response);
            }
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

fn jitter_millis_for_seed(max_ms: u64, seed: u64) -> u64 {
    if max_ms == 0 {
        return 0;
    }

    let mixed = seed.wrapping_add(0x9e37_79b9_7f4a_7c15);
    let mixed = (mixed ^ (mixed >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    let mixed = (mixed ^ (mixed >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    let mixed = mixed ^ (mixed >> 31);

    mixed % max_ms
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
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    nanos.hash(&mut hasher);
    std::thread::current().id().hash(&mut hasher);
    counter.hash(&mut hasher);

    jitter_millis_for_seed(max_ms, hasher.finish())
}

#[cfg(test)]
mod tests {
    use super::*;
    use azalea_core::config::EngineSettings;
    use azalea_core::media::TempFileCleanup;
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

    fn prepared_part(path: &Path, size: u64) -> PreparedPart {
        let temp_files = TempFileCleanup::new();
        PreparedPart::new(
            path.to_path_buf(),
            size,
            temp_files.guard(path.to_path_buf()),
        )
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
    fn exponential_backoff_max_delay_is_bounded_for_all_attempts() {
        const BASE_MS: u64 = 500;
        const MAX_MS: u64 = 2_000;
        const JITTER_MAX_MS: u64 = 250;

        for attempt in 0..=64 {
            let capped = BASE_MS
                .saturating_mul(1u64 << (attempt.min(8) as u32))
                .min(MAX_MS);
            let expected_max = Duration::from_millis(capped + (JITTER_MAX_MS - 1));
            for _ in 0..32 {
                let delay = exponential_backoff(attempt, BASE_MS, MAX_MS);
                assert!(
                    delay <= expected_max,
                    "attempt {attempt} exceeded max: delay={}ms expected_max={}ms",
                    delay.as_millis(),
                    expected_max.as_millis()
                );
            }
        }
    }

    #[test]
    fn exponential_backoff_saturates_without_overflow_for_large_attempts() {
        let expected = Duration::from_millis(u64::MAX);

        for attempt in [1_024, 1 << 20, usize::MAX - 1, usize::MAX] {
            assert_eq!(exponential_backoff(attempt, u64::MAX, u64::MAX), expected);
        }
    }

    #[test]
    fn jitter_seed_stays_within_requested_bound() {
        const SEED_LIMIT: u64 = u16::MAX as u64;

        for max_ms in [1, 2, 3, 7, 250, 1_024, u16::MAX as u64 + 1] {
            for seed in 0..=SEED_LIMIT {
                let jitter = jitter_millis_for_seed(max_ms, seed);
                assert!(
                    jitter < max_ms,
                    "seed {seed} exceeded bound: jitter={jitter} max_ms={max_ms}"
                );
            }
        }

        for seed in 0..=SEED_LIMIT {
            assert_eq!(jitter_millis_for_seed(0, seed), 0);
        }
    }

    #[test]
    fn jitter_runtime_path_stays_within_requested_bound() {
        for _ in 0..32 {
            let jitter = jitter_millis(250);
            assert!(jitter < 250);
        }
        assert_eq!(jitter_millis(0), 0);
    }

    #[tokio::test]
    async fn read_file_with_limit_missing_file_returns_not_found() {
        let config = EngineSettings::default();
        let path = unique_temp_file_path("missing-size-check");
        cleanup_file(&path);

        let part = prepared_part(&path, 4);
        let result = read_file_with_limit(&part, &config, 1, 1).await;
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

        let part = prepared_part(&path, 8);
        let result = file_size_checked(&part, &config, 1, 1);
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

        let part = prepared_part(&path, payload.len() as u64);
        let result = file_size_checked(&part, &config, 1, 1);
        cleanup_file(&path);

        assert_eq!(result.ok(), Some(payload.len() as u64));
    }

    #[test]
    fn read_file_preallocated_missing_file_returns_not_found() {
        let path = unique_temp_file_path("missing-read-preallocated");
        cleanup_file(&path);

        let result = read_file_preallocated(&path, 1);
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
        let result = read_file_preallocated(&path, payload.len() as u64);
        cleanup_file(&path);

        assert_eq!(result.ok().as_deref(), Some(payload.as_slice()));
    }
}
