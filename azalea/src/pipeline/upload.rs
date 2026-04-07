//! # Module overview
//! Upload stage for sending prepared media to Discord.
//!
//! ## Algorithm overview
//! Uploads prepared files either as standalone messages or batched requests of
//! up to Discord's attachment limit, depending on configuration.
//!
//! ## Trade-off acknowledgment
//! Batched uploads minimize Discord requests; bounded parallel preparation keeps
//! ordering deterministic without forcing file reads to happen one-by-one.

use crate::ids::{ChannelId, MessageId};
use crate::pipeline::{Error, Progress, UploadOutcome};
use azalea_core::concurrency::Permits;
use azalea_core::config::EngineSettings;
use azalea_core::media::TweetId;
use azalea_core::pipeline::{PreparedPart, PreparedUpload};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tracing::Instrument as _;
use twilight_http::Client;
use twilight_http::api_error::ApiError;
use twilight_http::error::Error as TwilightError;
use twilight_http::error::ErrorType;
use twilight_http::response::StatusCode;
use twilight_model::http::attachment::Attachment;

const MAX_ATTACHMENTS_PER_REQUEST: usize = 10;

#[derive(Debug, Clone, Copy, Default)]
pub struct Metrics {
    pub attachment_prepare_ms: u64,
    pub http_send_ms: u64,
    pub response_parse_ms: u64,
    pub post_upload_cleanup_ms: u64,
    pub payload_bytes: u64,
    pub request_count: usize,
}

impl Metrics {
    pub fn total_duration_ms(self) -> u64 {
        self.attachment_prepare_ms
            .saturating_add(self.http_send_ms)
            .saturating_add(self.response_parse_ms)
            .saturating_add(self.post_upload_cleanup_ms)
    }

    pub fn effective_throughput_bytes_per_sec(self) -> Option<f64> {
        (self.http_send_ms > 0)
            .then(|| self.payload_bytes as f64 / (self.http_send_ms as f64 / 1000.0))
    }

    pub fn slowest_stage(self) -> (&'static str, u64) {
        let stages = [
            ("attachment_preparation", self.attachment_prepare_ms),
            ("http_send", self.http_send_ms),
            ("response_parsing", self.response_parse_ms),
            ("post_upload_cleanup", self.post_upload_cleanup_ms),
        ];

        stages
            .into_iter()
            .max_by_key(|(_, elapsed_ms)| *elapsed_ms)
            .unwrap_or(("attachment_preparation", 0))
    }
}

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
    channel_id: ChannelId,
    tweet_id: TweetId,
    discord: &Client,
    permits: &Permits,
    config: &EngineSettings,
    progress_tx: Option<&mpsc::Sender<Progress>>,
) -> Result<(UploadOutcome, Metrics), Error> {
    tracing::trace!("Entered upload stage");
    tracing::info!("Starting upload");
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
    channel_id: ChannelId,
    tweet_id: TweetId,
    discord: &Client,
    config: &EngineSettings,
    progress_tx: Option<&mpsc::Sender<Progress>>,
) -> Result<(UploadOutcome, Metrics), Error> {
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

    if should_batch_upload(parts, config) {
        return upload_parts_batched(parts, channel_id, tweet_id, discord, config, progress_tx)
            .await;
    }

    upload_parts_sequential(parts, channel_id, tweet_id, discord, config, progress_tx).await
}

fn should_batch_upload(parts: &[PreparedPart], config: &EngineSettings) -> bool {
    parts.len() > 1 && config.pipeline.batch_upload_multiple_media
}

fn batch_request_count(parts: usize) -> usize {
    parts.div_ceil(MAX_ATTACHMENTS_PER_REQUEST)
}

async fn upload_parts_sequential(
    parts: &[PreparedPart],
    channel_id: ChannelId,
    tweet_id: TweetId,
    discord: &Client,
    config: &EngineSettings,
    progress_tx: Option<&mpsc::Sender<Progress>>,
) -> Result<(UploadOutcome, Metrics), Error> {
    let total_files = parts.len();
    let mut first_message_id = None;
    let mut metrics = Metrics::default();

    for (index, part) in parts.iter().enumerate() {
        if let Some(tx) = progress_tx {
            let stage = if total_files > 1 {
                Progress::UploadingSegment(index + 1, total_files)
            } else {
                Progress::Uploading
            };
            let _ = tx.send(stage).await;
        }

        let prepare_start = std::time::Instant::now();
        let attachment =
            build_attachment(part, config, tweet_id, index + 1, total_files, 0).await?;
        metrics.attachment_prepare_ms = metrics
            .attachment_prepare_ms
            .saturating_add(prepare_start.elapsed().as_millis() as u64);
        metrics.payload_bytes = metrics
            .payload_bytes
            .saturating_add(attachment_payload_bytes(std::slice::from_ref(&attachment)));

        let ctx = UploadRetryContext {
            discord,
            channel_id,
            part: index + 1,
            total: total_files,
        };

        let (response, http_send_ms) = send_with_retry(&ctx, std::slice::from_ref(&attachment))
            .instrument(tracing::info_span!(
                "upload_part",
                part = index + 1,
                total = total_files
            ))
            .await?;
        metrics.http_send_ms = metrics.http_send_ms.saturating_add(http_send_ms);
        metrics.request_count = metrics.request_count.saturating_add(1);

        let parse_start = std::time::Instant::now();
        let message = response.model().await.map_err(|e| Error::UploadFailed {
            part: index + 1,
            total: total_files,
            source: Box::new(e),
        })?;
        metrics.response_parse_ms = metrics
            .response_parse_ms
            .saturating_add(parse_start.elapsed().as_millis() as u64);

        if first_message_id.is_none() {
            first_message_id = Some(MessageId::from(message.id));
        }
        tracing::info!(
            part = index + 1,
            total = total_files,
            message_id = message.id.get(),
            "Upload part complete"
        );

        let cleanup_start = std::time::Instant::now();
        drop(message);
        drop(attachment);
        metrics.post_upload_cleanup_ms = metrics
            .post_upload_cleanup_ms
            .saturating_add(cleanup_start.elapsed().as_millis() as u64);
    }

    Ok((
        UploadOutcome {
            first_message_id: first_message_id.ok_or_else(|| Error::UploadFailed {
                part: 0,
                total: total_files,
                source: Box::new(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "upload produced no messages",
                )),
            })?,
            messages_sent: total_files,
        },
        metrics,
    ))
}

async fn upload_parts_batched(
    parts: &[PreparedPart],
    channel_id: ChannelId,
    tweet_id: TweetId,
    discord: &Client,
    config: &EngineSettings,
    progress_tx: Option<&mpsc::Sender<Progress>>,
) -> Result<(UploadOutcome, Metrics), Error> {
    let total_files = parts.len();
    let total_batches = batch_request_count(total_files);
    let mut first_message_id = None;
    let mut metrics = Metrics::default();

    for (batch_index, batch_parts) in parts.chunks(MAX_ATTACHMENTS_PER_REQUEST).enumerate() {
        let batch_start = batch_index * MAX_ATTACHMENTS_PER_REQUEST;
        let batch_end = batch_start + batch_parts.len();
        let prepare_start = std::time::Instant::now();
        let attachments = build_attachments(
            batch_parts,
            config,
            tweet_id,
            batch_start,
            total_files,
            progress_tx,
        )
        .await?;
        metrics.attachment_prepare_ms = metrics
            .attachment_prepare_ms
            .saturating_add(prepare_start.elapsed().as_millis() as u64);
        metrics.payload_bytes = metrics
            .payload_bytes
            .saturating_add(attachment_payload_bytes(&attachments));
        let ctx = UploadRetryContext {
            discord,
            channel_id,
            part: batch_end,
            total: total_files,
        };

        let (response, http_send_ms) = send_with_retry(&ctx, &attachments)
            .instrument(tracing::info_span!(
                "upload_batch",
                batch = batch_index + 1,
                total_batches,
                batch_parts = batch_parts.len(),
                batch_start = batch_start + 1,
                batch_end
            ))
            .await?;
        metrics.http_send_ms = metrics.http_send_ms.saturating_add(http_send_ms);
        metrics.request_count = metrics.request_count.saturating_add(1);
        let parse_start = std::time::Instant::now();
        let message = response.model().await.map_err(|e| Error::UploadFailed {
            part: batch_end,
            total: total_files,
            source: Box::new(e),
        })?;
        metrics.response_parse_ms = metrics
            .response_parse_ms
            .saturating_add(parse_start.elapsed().as_millis() as u64);

        if first_message_id.is_none() {
            first_message_id = Some(MessageId::from(message.id));
        }
        tracing::info!(
            batch = batch_index + 1,
            total_batches,
            batch_parts = batch_parts.len(),
            message_id = message.id.get(),
            "Batch upload complete"
        );

        let cleanup_start = std::time::Instant::now();
        drop(message);
        drop(attachments);
        metrics.post_upload_cleanup_ms = metrics
            .post_upload_cleanup_ms
            .saturating_add(cleanup_start.elapsed().as_millis() as u64);
    }

    Ok((
        UploadOutcome {
            first_message_id: first_message_id.ok_or_else(|| Error::UploadFailed {
                part: 0,
                total: total_files,
                source: Box::new(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "upload produced no messages",
                )),
            })?,
            messages_sent: total_batches,
        },
        metrics,
    ))
}

fn file_size_checked_from_metadata(
    file_path: &Path,
    file_size: u64,
    max_upload_bytes: u64,
    part_number: usize,
    total: usize,
) -> Result<u64, Error> {
    tracing::trace!(
        path = %file_path.display(),
        size_bytes = file_size,
        "Reading upload file"
    );

    if file_size > max_upload_bytes {
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

#[cfg(test)]
async fn read_file_with_limit(
    part: &PreparedPart,
    config: &EngineSettings,
    part_number: usize,
    total: usize,
) -> Result<Vec<u8>, Error> {
    let size = file_size_checked_from_metadata(
        part.path(),
        part.size(),
        config.transcode.max_upload_bytes,
        part_number,
        total,
    )?;
    read_file(part.path().to_path_buf(), size).await
}

async fn read_file(path: std::path::PathBuf, size: u64) -> Result<Vec<u8>, Error> {
    // Use a blocking task for file IO to avoid starving async tasks.
    let data = tokio::task::spawn_blocking(move || read_file_preallocated(&path, size))
        .await
        .map_err(|e| Error::Core(azalea_core::pipeline::Error::Io(std::io::Error::other(e))))?
        .map_err(|e| Error::Core(azalea_core::pipeline::Error::Io(e)))?;
    Ok(data)
}

fn attachment_filename(path: &Path, tweet_id: TweetId, part_number: usize, total: usize) -> String {
    let extension = path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or("mp4");

    if total > 1 {
        format!("tweet_{}_part{}.{}", tweet_id.0, part_number, extension)
    } else {
        format!("tweet_{}.{}", tweet_id.0, extension)
    }
}

async fn build_attachment(
    part: &PreparedPart,
    config: &EngineSettings,
    tweet_id: TweetId,
    part_number: usize,
    total: usize,
    attachment_id: u64,
) -> Result<Attachment, Error> {
    if let Some(upload_ready_bytes) = part.upload_ready_bytes() {
        let file_size = file_size_checked_from_metadata(
            part.path(),
            part.size(),
            config.transcode.max_upload_bytes,
            part_number,
            total,
        )?;
        if upload_ready_bytes.len() as u64 != file_size {
            return Err(Error::UploadFailed {
                part: part_number,
                total,
                source: Box::new(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "in-memory upload payload size mismatch",
                )),
            });
        }

        tracing::trace!(
            part = part_number,
            total,
            path = %part.path().display(),
            size_bytes = file_size,
            "Prepared upload payload from memory"
        );
        return Ok(Attachment::from_bytes(
            attachment_filename(part.path(), tweet_id, part_number, total),
            upload_ready_bytes.as_ref().to_vec(),
            attachment_id,
        ));
    }

    build_attachment_from_path(
        part.path().to_path_buf(),
        part.size(),
        config.transcode.max_upload_bytes,
        tweet_id,
        part_number,
        total,
        attachment_id,
    )
    .await
}

async fn build_attachment_from_path(
    file_path: PathBuf,
    file_size: u64,
    max_upload_bytes: u64,
    tweet_id: TweetId,
    part_number: usize,
    total: usize,
    attachment_id: u64,
) -> Result<Attachment, Error> {
    let file_size = file_size_checked_from_metadata(
        &file_path,
        file_size,
        max_upload_bytes,
        part_number,
        total,
    )?;
    tracing::trace!(
        part = part_number,
        total,
        path = %file_path.display(),
        size_bytes = file_size,
        "Prepared upload payload"
    );

    let data = read_file(file_path.clone(), file_size).await?;
    Ok(Attachment::from_bytes(
        attachment_filename(&file_path, tweet_id, part_number, total),
        data,
        attachment_id,
    ))
}

async fn build_attachments(
    parts: &[PreparedPart],
    config: &EngineSettings,
    tweet_id: TweetId,
    start_part_index: usize,
    total_files: usize,
    progress_tx: Option<&mpsc::Sender<Progress>>,
) -> Result<Vec<Attachment>, Error> {
    let max_upload_bytes = config.transcode.max_upload_bytes;
    let concurrency_limit = config
        .pipeline
        .attachment_prepare_concurrency
        .min(parts.len().max(1));
    let mut next_to_spawn = 0usize;
    let mut join_set = JoinSet::new();
    let mut attachments = (0..parts.len()).map(|_| None).collect::<Vec<_>>();

    while next_to_spawn < parts.len() || !join_set.is_empty() {
        while next_to_spawn < parts.len() && join_set.len() < concurrency_limit {
            let Some(part) = parts.get(next_to_spawn) else {
                return Err(Error::UploadFailed {
                    part: start_part_index + next_to_spawn + 1,
                    total: total_files,
                    source: Box::new(std::io::Error::other(
                        "attachment preparation index out of range",
                    )),
                });
            };
            let slot = next_to_spawn;
            let part_number = start_part_index + slot + 1;
            if let Some(tx) = progress_tx {
                let stage = if total_files > 1 {
                    Progress::UploadingSegment(part_number, total_files)
                } else {
                    Progress::Uploading
                };
                let _ = tx.send(stage).await;
            }

            let file_path = part.path().to_path_buf();
            let file_size = part.size();
            join_set.spawn(async move {
                build_attachment_from_path(
                    file_path,
                    file_size,
                    max_upload_bytes,
                    tweet_id,
                    part_number,
                    total_files,
                    slot as u64,
                )
                .await
                .map(|attachment| (slot, attachment))
            });
            next_to_spawn += 1;
        }

        let Some(result) = join_set.join_next().await else {
            break;
        };
        let (slot, attachment) = result.map_err(|e| {
            Error::Core(azalea_core::pipeline::Error::Io(std::io::Error::other(e)))
        })??;
        let Some(target) = attachments.get_mut(slot) else {
            return Err(Error::UploadFailed {
                part: start_part_index + slot + 1,
                total: total_files,
                source: Box::new(std::io::Error::other("attachment slot out of range")),
            });
        };
        *target = Some(attachment);
    }

    attachments
        .into_iter()
        .enumerate()
        .map(|(slot, attachment)| {
            attachment.ok_or_else(|| Error::UploadFailed {
                part: start_part_index + slot + 1,
                total: total_files,
                source: Box::new(std::io::Error::other(
                    "attachment preparation produced incomplete batch",
                )),
            })
        })
        .collect()
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
    channel_id: ChannelId,
    part: usize,
    total: usize,
}

async fn send_with_retry(
    ctx: &UploadRetryContext<'_>,
    attachments: &[Attachment],
) -> Result<
    (
        twilight_http::Response<twilight_model::channel::Message>,
        u64,
    ),
    Error,
> {
    const MAX_RETRIES: usize = 3;

    let mut attempt = 0usize;
    let send_start = std::time::Instant::now();
    loop {
        tracing::trace!(
            part = ctx.part,
            total = ctx.total,
            attempt = attempt + 1,
            attachments = attachments.len(),
            "Sending upload attempt"
        );

        match ctx
            .discord
            .create_message(ctx.channel_id.into())
            .attachments(attachments)
            .await
        {
            Ok(response) => {
                let http_send_ms = send_start.elapsed().as_millis() as u64;
                tracing::info!(
                    part = ctx.part,
                    total = ctx.total,
                    attempt = attempt + 1,
                    attachments = attachments.len(),
                    http_send_ms,
                    "Upload request succeeded"
                );
                return Ok((response, http_send_ms));
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

fn attachment_payload_bytes(attachments: &[Attachment]) -> u64 {
    attachments
        .iter()
        .map(|attachment| attachment.file.len() as u64)
        .sum()
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
    #![allow(clippy::panic)]

    use super::*;
    use azalea_core::config::EngineSettings;
    use azalea_core::media::TempFileCleanup;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
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

    fn prepared_part_with_upload_ready_bytes(path: &Path, bytes: &[u8]) -> PreparedPart {
        let temp_files = TempFileCleanup::new();
        PreparedPart::with_upload_ready_bytes(
            path.to_path_buf(),
            bytes.len() as u64,
            temp_files.guard(path.to_path_buf()),
            Arc::<[u8]>::from(bytes.to_vec()),
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
        let result = file_size_checked_from_metadata(
            part.path(),
            part.size(),
            config.transcode.max_upload_bytes,
            1,
            1,
        );
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
        let result = file_size_checked_from_metadata(
            part.path(),
            part.size(),
            config.transcode.max_upload_bytes,
            1,
            1,
        );
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

    #[tokio::test]
    async fn build_attachments_assigns_split_filenames_and_unique_ids() {
        let first_payload = b"part-one";
        let second_payload = b"part-two";
        let first_path = write_temp_file("batch-1", first_payload);
        let second_path = write_temp_file("batch-2", second_payload);
        let mut config = EngineSettings::default();
        config.transcode.max_upload_bytes = 1024;

        let parts = vec![
            prepared_part(&first_path, first_payload.len() as u64),
            prepared_part(&second_path, second_payload.len() as u64),
        ];

        let attachments =
            match build_attachments(&parts, &config, TweetId(42), 0, parts.len(), None).await {
                Ok(attachments) => attachments,
                Err(error) => panic!("attachments should build: {error}"),
            };
        cleanup_file(&first_path);
        cleanup_file(&second_path);

        assert_eq!(attachments.len(), 2);
        let [first, second] = attachments.as_slice() else {
            panic!("expected exactly two attachments");
        };
        assert_eq!(first.filename, "tweet_42_part1.mp4");
        assert_eq!(second.filename, "tweet_42_part2.mp4");
        assert_eq!(first.id, 0);
        assert_eq!(second.id, 1);
        assert_eq!(first.file, first_payload);
        assert_eq!(second.file, second_payload);
    }

    #[tokio::test]
    async fn build_attachment_uses_in_memory_payload_without_reopening_file() {
        let path = unique_temp_file_path("missing-upload-fast-path.mp4");
        cleanup_file(&path);
        let payload = b"in-memory-upload";
        let config = EngineSettings::default();
        let part = prepared_part_with_upload_ready_bytes(&path, payload);

        let attachment = match build_attachment(&part, &config, TweetId(7), 1, 1, 0).await {
            Ok(attachment) => attachment,
            Err(error) => {
                panic!("memory-backed payload should upload without reopening file: {error}")
            }
        };

        assert_eq!(attachment.filename, "tweet_7.mp4");
        assert_eq!(attachment.file, payload);
        assert_eq!(attachment.id, 0);
    }

    #[test]
    fn batch_request_count_uses_discord_attachment_limit() {
        assert_eq!(batch_request_count(1), 1);
        assert_eq!(batch_request_count(10), 1);
        assert_eq!(batch_request_count(11), 2);
        assert_eq!(batch_request_count(20), 2);
        assert_eq!(batch_request_count(21), 3);
    }

    #[test]
    fn batch_upload_only_applies_to_multi_part_uploads() {
        let first_path = write_temp_file("batch-toggle-1", b"x");
        let second_path = write_temp_file("batch-toggle-2", b"y");
        let mut config = EngineSettings::default();
        config.pipeline.batch_upload_multiple_media = true;

        let first = prepared_part(&first_path, 1);
        let second = prepared_part(&second_path, 1);

        assert!(!should_batch_upload(std::slice::from_ref(&first), &config));
        assert!(should_batch_upload(&[first, second], &config));

        cleanup_file(&first_path);
        cleanup_file(&second_path);
    }
}
