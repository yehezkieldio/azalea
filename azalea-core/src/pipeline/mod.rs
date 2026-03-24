//! End-to-end media pipeline stages.
//!
//! The pipeline runs resolve → download → optimize with metrics and
//! deduplication woven throughout. Each stage owns its own concurrency permits.
//!
//! ## Data flow explanation
//! [`run`] orchestrates stage progression and emits [`types::Progress`] updates
//! consumed by the `azalea` crate for user-visible status messaging.
//!
//! ## Concurrency assumptions
//! Each stage holds a semaphore permit while it executes; see
//! [`crate::concurrency::Permits`].

mod disk;
pub mod download;
pub mod errors;
pub mod ffmpeg;
pub mod optimize;
mod process;
pub mod quality;
pub mod resolve;
mod ssrf;
pub mod types;

pub use errors::Error;
pub use resolve::ResolverChain;
pub use types::{Job, PreparedPart, PreparedUpload, Progress, RequestId};

use crate::engine::Engine;
use crate::pipeline::errors::DownloadError;
use crate::storage::Stage;
use std::time::Instant;
use tokio::sync::mpsc;
use tracing::Instrument as _;

const RESOLVE_WARN_MS: u64 = 8_000;
const DOWNLOAD_WARN_MS: u64 = 20_000;
const OPTIMIZE_WARN_MS: u64 = 30_000;

fn warn_if_slow(stage: Stage, duration_ms: u64, threshold_ms: u64) {
    if duration_ms > threshold_ms {
        tracing::warn!(
            ?stage,
            duration_ms,
            threshold_ms,
            "Pipeline stage exceeded warning threshold"
        );
    } else {
        tracing::debug!(
            ?stage,
            duration_ms,
            "Pipeline stage completed within threshold"
        );
    }
}

/// Execute the pipeline up to the optimized media output, emitting progress updates.
///
/// ## Preconditions
/// - Engine has been fully initialized (see [`crate::Engine::new`]).
/// - Caller holds a live `progress` channel if updates are desired.
///
/// ## Postconditions
/// - Returns [`PreparedUpload`] ready for the upload stage.
/// - Dedup in-flight markers are cleared on failure paths.
pub async fn run(
    job: Job,
    engine: &Engine,
    progress: Option<mpsc::Sender<Progress>>,
) -> Result<PreparedUpload, Error> {
    let pipeline_span = tracing::info_span!(
        "core_pipeline",
        request_id = job.request_id.0,
        scope_id = job.scope_id,
        job_id = job.job_id,
        tweet_id = job.tweet_url.tweet_id.0
    );

    async {
        tracing::info!("Pipeline started");
        let pipeline_start = Instant::now();

        tracing::trace!("Checking in-memory dedup admission cache");
        if !engine
            .dedup
            .reserve_inflight(job.scope_id, job.tweet_url.tweet_id)
            .await
        {
            // Another identical request is already being processed or recently completed.
            tracing::info!("Duplicate request detected");
            return Err(Error::Duplicate);
        }

        let result = async {
            let resolve_span = tracing::info_span!("pipeline_stage", stage = "resolve");
            let resolved = async {
                let resolve_start = Instant::now();
                if let Some(tx) = &progress
                    && tx.send(Progress::Resolving).await.is_err()
                {
                    tracing::debug!("Progress channel closed before resolve stage update");
                }

                tracing::trace!("Starting resolve stage");
                let resolved = engine
                    .resolver
                    .resolve(&job.tweet_url, &engine.http, &engine.permits)
                    .await
                    .inspect_err(|_e| {
                        tracing::trace!(error = %_e, "Resolve stage failed");
                        engine.metrics.record_error("resolve_failed");
                        engine.metrics.record_failure();
                    })?;
                let resolve_duration_ms = resolve_start.elapsed().as_millis() as u64;
                engine
                    .metrics
                    .record_stage_duration(Stage::Resolve, resolve_duration_ms);
                warn_if_slow(Stage::Resolve, resolve_duration_ms, RESOLVE_WARN_MS);
                tracing::info!(
                    duration_ms = resolve_duration_ms,
                    media_type = ?resolved.media_type,
                    extension = %resolved.extension,
                    resolution = ?resolved.resolution,
                    media_duration_secs = resolved.duration,
                    "Resolved media"
                );

                Ok::<_, Error>(resolved)
            }
            .instrument(resolve_span)
            .await?;

            let download_span = tracing::info_span!("pipeline_stage", stage = "download");
            let downloaded = async {
                let download_start = Instant::now();
                if let Some(tx) = &progress
                    && tx.send(Progress::Downloading).await.is_err()
                {
                    tracing::debug!("Progress channel closed before download stage update");
                }

                tracing::trace!("Starting download stage");
                let downloaded = download::download(
                    resolved.as_ref(),
                    &job,
                    &engine.http,
                    &engine.permits,
                    &engine.temp_files,
                    &engine.config,
                )
                .await
                .inspect_err(|_e| {
                    tracing::trace!(error = %_e, "Download stage failed");
                    match _e {
                        Error::DownloadFailed {
                            source: DownloadError::SsrfBlocked(_),
                        } => {
                            engine.metrics.record_error("ssrf_blocked");
                        }
                        _ => engine.metrics.record_error("download_failed"),
                    }
                    engine.metrics.record_failure();
                })?;
                let download_duration_ms = download_start.elapsed().as_millis() as u64;
                engine
                    .metrics
                    .record_stage_duration(Stage::Download, download_duration_ms);
                warn_if_slow(Stage::Download, download_duration_ms, DOWNLOAD_WARN_MS);
                tracing::info!(
                    duration_ms = download_duration_ms,
                    size_bytes = downloaded.size,
                    resolution = ?downloaded.resolution,
                    media_duration_secs = downloaded.duration,
                    "Download completed"
                );

                Ok::<_, Error>(downloaded)
            }
            .instrument(download_span)
            .await?;

            let optimize_span = tracing::info_span!("pipeline_stage", stage = "optimize");
            let prepared = async {
                let optimize_start = Instant::now();
                if let Some(tx) = &progress
                    && tx.send(Progress::Optimizing).await.is_err()
                {
                    tracing::debug!("Progress channel closed before optimize stage update");
                }

                tracing::trace!("Starting optimize stage");
                let prepared = optimize::optimize(
                    downloaded,
                    resolved.as_ref(),
                    &engine.permits,
                    &engine.temp_files,
                    &engine.config,
                    &engine.transcode_runtime,
                    progress.clone(),
                )
                .await
                .inspect_err(|_e| {
                    tracing::trace!(error = %_e, "Optimize stage failed");
                    engine.metrics.record_error("optimize_failed");
                    engine.metrics.record_failure();
                })?;
                let optimize_duration_ms = optimize_start.elapsed().as_millis() as u64;
                engine
                    .metrics
                    .record_stage_duration(Stage::Optimize, optimize_duration_ms);
                warn_if_slow(Stage::Optimize, optimize_duration_ms, OPTIMIZE_WARN_MS);
                let (prepared_kind, prepared_parts) = match &prepared {
                    types::PreparedUpload::Single { .. } => ("single", 1),
                    types::PreparedUpload::Split { parts, .. } => ("split", parts.len()),
                };
                tracing::info!(
                    duration_ms = optimize_duration_ms,
                    kind = prepared_kind,
                    parts = prepared_parts,
                    "Optimize completed"
                );

                Ok::<_, Error>(prepared)
            }
            .instrument(optimize_span)
            .await?;

            Ok(prepared)
        }
        .await;

        match result {
            Ok(prepared) => {
                tracing::info!(
                    elapsed_ms = pipeline_start.elapsed().as_millis(),
                    "Pipeline finished"
                );
                Ok(prepared)
            }
            Err(err) => {
                tracing::warn!(error = %err, "Pipeline failed; clearing in-flight dedup marker");
                engine
                    .dedup
                    .clear_inflight(job.scope_id, job.tweet_url.tweet_id)
                    .await;
                Err(err)
            }
        }
    }
    .instrument(pipeline_span)
    .await
}
