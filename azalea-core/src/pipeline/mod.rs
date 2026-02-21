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
pub use types::{Job, PreparedUpload, Progress, RequestId};

use crate::engine::Engine;
use crate::pipeline::errors::DownloadError;
use crate::storage::Stage;
use std::time::Instant;
use tokio::sync::mpsc;

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
    tracing::info!("Pipeline started");
    let pipeline_start = Instant::now();

    tracing::trace!("Checking dedup cache");
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
        let resolve_start = Instant::now();
        if let Some(tx) = &progress {
            let _ = tx.send(Progress::Resolving).await;
        }

        let resolved = engine
            .resolver
            .resolve(&job.tweet_url, &engine.http, &engine.permits)
            .await
            .inspect_err(|_e| {
                tracing::trace!(error = %_e, "Resolve stage failed");
                engine.metrics.record_error("resolve_failed");
                engine.metrics.record_failure();
            })?;
        engine
            .metrics
            .record_stage_duration(Stage::Resolve, resolve_start.elapsed().as_millis() as u64);
        tracing::info!(
            duration_ms = resolve_start.elapsed().as_millis(),
            media_type = ?resolved.media_type,
            extension = %resolved.extension,
            resolution = ?resolved.resolution,
            media_duration_secs = resolved.duration,
            "Resolved media"
        );

        let download_start = Instant::now();
        if let Some(tx) = &progress {
            let _ = tx.send(Progress::Downloading).await;
        }

        let downloaded = download::download(
            &resolved,
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
        engine
            .metrics
            .record_stage_duration(Stage::Download, download_start.elapsed().as_millis() as u64);
        tracing::info!(
            duration_ms = download_start.elapsed().as_millis(),
            size_bytes = downloaded.size,
            resolution = ?downloaded.resolution,
            media_duration_secs = downloaded.duration,
            "Download completed"
        );

        let optimize_start = Instant::now();
        if let Some(tx) = &progress {
            let _ = tx.send(Progress::Optimizing).await;
        }

        let prepared = optimize::optimize(
            downloaded,
            &resolved,
            &engine.permits,
            &engine.temp_files,
            &engine.config,
            progress.clone(),
        )
        .await
        .inspect_err(|_e| {
            tracing::trace!(error = %_e, "Optimize stage failed");
            engine.metrics.record_error("optimize_failed");
            engine.metrics.record_failure();
        })?;
        engine
            .metrics
            .record_stage_duration(Stage::Optimize, optimize_start.elapsed().as_millis() as u64);
        let (prepared_kind, prepared_parts) = match &prepared {
            types::PreparedUpload::Single { .. } => ("single", 1),
            types::PreparedUpload::Split { paths, .. } => ("split", paths.len()),
        };
        tracing::info!(
            duration_ms = optimize_start.elapsed().as_millis(),
            kind = prepared_kind,
            parts = prepared_parts,
            "Optimize completed"
        );

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
            engine
                .dedup
                .clear_inflight(job.scope_id, job.tweet_url.tweet_id)
                .await;
            Err(err)
        }
    }
}
