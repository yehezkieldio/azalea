//! Azalea bot entrypoint.
//!
//! This module wires configuration, runtime, gateway shards, and the media
//! pipeline worker. It keeps startup sequencing explicit to surface dependency
//! failures early and provide predictable shutdown ordering.
//!
//! ## Architecture decision
//! The runtime is built explicitly to keep startup failures localized and to
//! avoid global mutable state. See [`crate::app::App`] for shared state wiring.
//!
//! ## Data flow explanation
//! `main` → `async_main` → gateway shards + pipeline worker.
//!
//! ## Explicit non-goals
//! This entrypoint does not attempt automatic restarts; deployment tooling
//! should manage process supervision.

mod app;
mod concurrency;
mod config;
mod discord;
mod gateway;
mod pipeline;

use app::App;
use azalea_core::config::BinarySettings;
use azalea_core::media;
use azalea_core::pipeline as core_pipeline;
use azalea_core::storage::Stage;
use config::AppConfig;
use mimalloc::MiMalloc;
use std::{
    env,
    path::{Path, PathBuf},
    sync::{Arc, atomic::Ordering},
    time::{Duration, Instant},
};
use tokio::{process::Command, signal, sync::mpsc, task::JoinSet};
use tracing::Instrument as _;
use twilight_gateway::{ConfigBuilder, queue::InMemoryQueue};
use twilight_http::Client;
use twilight_model::gateway::{
    payload::outgoing::update_presence::UpdatePresencePayload,
    presence::{ActivityType, MinimalActivity, Status},
};

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

const JOB_QUEUE_CAPACITY: usize = 100;
const METRICS_FLUSH_INTERVAL_SECS: u64 = 60;
const DEDUP_PRUNE_INTERVAL_SECS: u64 = 6 * 60 * 60;
const DEDUP_PRUNE_BATCH_SIZE: usize = 500;
const PROGRESS_UPDATE_DEBOUNCE_SECS: u64 = 3;
const DEPENDENCY_CHECK_TIMEOUT_SECS: u64 = 10;
const SHUTDOWN_STEP_TIMEOUT_SECS: u64 = 5;
const TEMP_PRUNE_INTERVAL_SECS: u64 = 30 * 60;
const TEMP_PRUNE_MAX_AGE_SECS: u64 = 60 * 60;

/// RAII guard that decrements the queue depth when a job task completes.
///
/// ## Rationale
/// Avoids manual bookkeeping on every early-return path in the pipeline worker.
struct QueueDepthGuard(Arc<std::sync::atomic::AtomicUsize>);

impl QueueDepthGuard {
    fn new(counter: Arc<std::sync::atomic::AtomicUsize>) -> Self {
        Self(counter)
    }
}

impl Drop for QueueDepthGuard {
    fn drop(&mut self) {
        // Best-effort: queue depth is a heuristic for status output.
        let _ = self
            .0
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
                Some(value.saturating_sub(1))
            });
    }
}

fn main() -> anyhow::Result<()> {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));

    tracing_subscriber::fmt().with_env_filter(filter).init();

    dotenvy::dotenv().ok();

    let config = AppConfig::load()?;

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(config.runtime.worker_threads)
        .max_blocking_threads(config.runtime.max_blocking_threads)
        .thread_stack_size(config.runtime.thread_stack_size)
        .enable_all()
        .build()?;

    runtime.block_on(async_main(config))
}

/// Async application bootstrap separated for testability and clearer error paths.
///
/// ## Preconditions
/// Environment variables (e.g., `DISCORD_TOKEN`) must be set by the caller.
async fn async_main(config: AppConfig) -> anyhow::Result<()> {
    raise_fd_limit();
    let mut config = config;
    resolve_dependencies(&mut config.engine.binaries)?;
    verify_dependencies(&config.engine.binaries).await?;

    // Ensure temp storage is available before starting the worker.
    tokio::fs::create_dir_all(&config.engine.storage.temp_dir).await?;
    {
        let temp_dir = config.engine.storage.temp_dir.clone();
        let max_age = Duration::from_secs(TEMP_PRUNE_MAX_AGE_SECS);
        match tokio::task::spawn_blocking(move || {
            media::cleanup_temp_dir_older_than(&temp_dir, max_age)
        })
        .await
        {
            Ok(removed) if removed > 0 => {
                tracing::info!(removed, "Pruned stale temp entries on startup");
            }
            Ok(_) => {}
            Err(e) => {
                tracing::warn!(error = %e, "Failed to prune temp entries on startup");
            }
        }
    }
    // Initialize media regex once to avoid per-request overhead.
    media::init_regex();

    let token = env::var("DISCORD_TOKEN")?;

    let discord = Client::builder()
        .token(token.clone())
        .timeout(config.upload_timeout())
        .build();

    let app = App::new(config.clone(), discord)?;
    install_panic_flush_hook(app.engine.clone());

    {
        let temp_dir = app.engine.config.storage.temp_dir.clone();
        let prune_interval = Duration::from_secs(TEMP_PRUNE_INTERVAL_SECS);
        let max_age = Duration::from_secs(TEMP_PRUNE_MAX_AGE_SECS);
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(prune_interval);
            loop {
                ticker.tick().await;
                let temp_dir = temp_dir.clone();
                match tokio::task::spawn_blocking(move || {
                    media::cleanup_temp_dir_older_than(&temp_dir, max_age)
                })
                .await
                {
                    Ok(removed) if removed > 0 => {
                        tracing::debug!(removed, "Pruned stale temp entries");
                    }
                    Ok(_) => {}
                    Err(e) => {
                        tracing::warn!(error = %e, "Failed to prune temp entries");
                    }
                }
            }
        });
    }

    if let Err(e) = app.engine.metrics.load_from_db().await {
        // Metrics are best-effort; continue even on failures.
        tracing::warn!(error = %e, "Failed to load metrics from db");
    }

    app.engine
        .metrics
        .start_flush_task(Duration::from_secs(METRICS_FLUSH_INTERVAL_SECS))
        .await;
    app.engine
        .dedup
        .start_flush_task(Duration::from_secs(
            config.engine.storage.dedup_flush_interval_secs,
        ))
        .await;
    app.engine
        .dedup
        .start_maintenance_task(
            Duration::from_secs(DEDUP_PRUNE_INTERVAL_SECS),
            DEDUP_PRUNE_BATCH_SIZE,
        )
        .await;

    if let Err(e) = app.engine.dedup.load_from_db().await {
        // Dedup persistence failure should not block startup.
        tracing::warn!(error = %e, "Failed to load dedup cache");
    }

    if let Err(e) = discord::register(&app.discord, app.config.application_id).await {
        // Command registration is best-effort; continue if it fails.
        tracing::warn!(error = %e, "Failed to register slash commands");
    }

    let info = app.discord.gateway().authed().await?.model().await?;

    let queue = InMemoryQueue::new(
        info.session_start_limit.max_concurrency,
        info.session_start_limit.remaining,
        Duration::from_millis(info.session_start_limit.reset_after),
        info.session_start_limit.total,
    );

    let presence = UpdatePresencePayload::new(
        vec![
            MinimalActivity {
                kind: ActivityType::Playing,
                name: "Feeds prefer reaction.".into(),
                url: None,
            }
            .into(),
        ],
        false,
        None,
        Status::Online,
    )?;

    let gateway_config = ConfigBuilder::new(token, gateway::event::INTENTS)
        .queue(queue)
        .presence(presence)
        .build();

    let shards = gateway::restore(gateway_config, info.shards).await;

    let (job_sender, job_receiver) = mpsc::channel(JOB_QUEUE_CAPACITY);
    // Pipeline worker runs independently of gateway shards.
    let app_for_worker = app.clone();
    let pipeline_worker = tokio::spawn(run_pipeline_worker(app_for_worker, job_receiver));

    let mut shard_set = JoinSet::new();
    for shard in shards {
        let job_sender = job_sender.clone();
        let app_for_handler = app.clone();
        shard_set.spawn(gateway::run(shard, move |dispatcher, event| {
            let job_sender = job_sender.clone();
            let app = app_for_handler.clone();
            // Dispatch events onto the gateway handler.
            dispatcher.dispatch(gateway::event::handle(event, app, job_sender));
        }));
    }

    // Wait for shutdown signal before tearing down workers.
    signal::ctrl_c().await?;
    tracing::info!("shutting down; press CTRL-C to abort");

    let mut resume_info = Vec::new();
    let forced_shutdown = tokio::select! {
        _ = signal::ctrl_c() => true,
        _ = async {
            while let Some(result) = shard_set.join_next().await {
                match result {
                    Ok(info) => resume_info.push(info),
                    Err(error) => {
                        if error.is_panic() {
                            tracing::error!(error = %error, "Shard task panicked");
                        } else {
                            tracing::warn!(error = %error, "Shard task cancelled");
                        }
                    }
                }
            }
        } => false,
    };

    if forced_shutdown {
        tracing::warn!("Forced shutdown requested; aborting shard tasks");
        shard_set.abort_all();
        while let Some(result) = shard_set.join_next().await {
            if let Err(error) = result {
                tracing::warn!(error = %error, "Shard task aborted");
            }
        }
        resume_info.clear();
    }

    drop(job_sender);
    if forced_shutdown {
        // On forced shutdown, abort pipeline tasks with a timeout.
        pipeline_worker.abort();
        let _ = tokio::time::timeout(
            Duration::from_secs(SHUTDOWN_STEP_TIMEOUT_SECS),
            pipeline_worker,
        )
        .await;
    } else {
        // Graceful shutdown: wait for the pipeline worker to finish.
        let _ = pipeline_worker.await;
    }

    if forced_shutdown {
        // Bound shutdown steps to avoid hanging indefinitely.
        let timeout = Duration::from_secs(SHUTDOWN_STEP_TIMEOUT_SECS);
        let _ = tokio::time::timeout(timeout, app.engine.metrics.stop_flush_task()).await;
        let _ = tokio::time::timeout(timeout, app.engine.dedup.stop_flush_task()).await;
        let _ = tokio::time::timeout(timeout, app.engine.dedup.stop_maintenance_task()).await;
        let _ = tokio::time::timeout(timeout, app.engine.temp_files.shutdown()).await;
        let _ = tokio::time::timeout(timeout, app.engine.dedup.flush()).await;
        let _ = tokio::time::timeout(timeout, app.engine.metrics.flush()).await;
    } else {
        app.engine.metrics.stop_flush_task().await;
        app.engine.dedup.stop_flush_task().await;
        app.engine.dedup.stop_maintenance_task().await;
        app.engine.temp_files.shutdown().await;
        app.engine.dedup.flush().await;
        app.engine.metrics.flush().await;
    }

    media::cleanup_temp_dir_sync(&app.engine.config.storage.temp_dir);

    if let Err(e) = gateway::save(&resume_info).await {
        // Resume info is advisory; log and continue on failure.
        tracing::warn!(error = %e, "Failed to save resume info");
    }

    Ok(())
}

/// Raise the file descriptor soft limit where possible.
///
/// ## Rationale
/// High concurrency workloads can exhaust the default `RLIMIT_NOFILE`.
///
/// ## Trade-off acknowledgment
/// This is best-effort; failure does not abort startup.
fn raise_fd_limit() {
    // Best-effort only: the process can still run with the OS defaults.
    #[cfg(unix)]
    {
        use nix::sys::resource::{Resource, getrlimit, setrlimit};

        let Ok((soft_limit, hard_limit)) = getrlimit(Resource::RLIMIT_NOFILE) else {
            tracing::warn!("Failed to read RLIMIT_NOFILE");
            return;
        };

        let target = hard_limit.min(65_536);
        if soft_limit >= target {
            return;
        }

        if let Err(error) = setrlimit(Resource::RLIMIT_NOFILE, target, hard_limit) {
            // Failures are non-fatal; log and proceed.
            tracing::warn!(error = %error, "Failed to raise RLIMIT_NOFILE");
        } else {
            tracing::info!(rlimit_nofile = target, "Raised file descriptor limit");
        }
    }
}

/// Run the pipeline worker loop and execute jobs with bounded concurrency.
///
/// ## Concurrency assumptions
/// This loop owns the receiver; each job is processed in a task under the
/// pipeline semaphore.
async fn run_pipeline_worker(app: App, mut receiver: mpsc::Receiver<pipeline::Job>) {
    tracing::info!("Pipeline worker started");

    let max_concurrency = app.engine.config.concurrency.pipeline.max(1) as usize;
    let mut join_set = tokio::task::JoinSet::new();
    let log_join_error = |error: tokio::task::JoinError| {
        if error.is_panic() {
            tracing::error!(error = %error, "Pipeline task panicked");
        } else {
            tracing::warn!(error = %error, "Pipeline task cancelled");
        }
    };

    while let Some(job) = receiver.recv().await {
        let app = app.clone();
        let queue_guard = QueueDepthGuard::new(Arc::clone(&app.queue_depth));
        let permit = match Arc::clone(&app.engine.permits.pipeline)
            .acquire_owned()
            .await
        {
            Ok(permit) => permit,
            Err(_) => {
                tracing::warn!("Pipeline semaphore closed");
                drop(queue_guard);
                break;
            }
        };

        join_set.spawn(async move {
            let _queue_guard = queue_guard;
            let _permit = permit;
            let span = tracing::info_span!(
                "pipeline_run",
                request_id = job.request_id.0,
                tweet_id = job.tweet_url.tweet_id.0,
                channel_id = job.channel_id.get(),
                author_id = job.author_id.get(),
                tweet_url = %job.tweet_url.original_url()
            );

            async {
                let reply_message_id =
                    discord::send_processing(&app.discord, job.channel_id, job.message_id).await;

                let (progress_tx, progress_rx) = mpsc::channel(10);
                let progress_updater = if let Some(msg_id) = reply_message_id {
                    let debounce = Duration::from_secs(PROGRESS_UPDATE_DEBOUNCE_SECS);
                    // Spawn a debounced progress updater to limit API churn.
                    Some(discord::spawn_progress_updates(
                        Arc::clone(&app.discord),
                        app.engine.metrics.clone(),
                        job.channel_id,
                        msg_id,
                        progress_rx,
                        debounce,
                    ))
                } else {
                    None
                };

                let core_job = job.core();
                let result =
                    core_pipeline::run(core_job, &app.engine, Some(progress_tx.clone())).await;

                let outcome = match result {
                    Ok(prepared) => {
                        let upload_start = Instant::now();
                        let _ = progress_tx.send(pipeline::Progress::Uploading).await;
                        match pipeline::upload::upload(
                            &prepared,
                            job.channel_id,
                            job.tweet_url.tweet_id,
                            &app.discord,
                            &app.engine.permits,
                            &app.engine.config,
                        )
                        .await
                        {
                            Ok(outcome) => {
                                // Record metrics and mark dedup only on success.
                                app.engine.metrics.record_stage_duration(
                                    Stage::Upload,
                                    upload_start.elapsed().as_millis() as u64,
                                );
                                app.engine.metrics.record_success();
                                app.engine
                                    .dedup
                                    .mark_processed(job.channel_id.get(), job.tweet_url.tweet_id)
                                    .await;
                                Ok(outcome)
                            }
                            Err(error) => {
                                // Failure paths clear inflight markers for retry.
                                app.engine.metrics.record_error("upload_failed");
                                app.engine.metrics.record_failure();
                                app.engine
                                    .dedup
                                    .clear_inflight(job.channel_id.get(), job.tweet_url.tweet_id)
                                    .await;
                                Err(error)
                            }
                        }
                    }
                    Err(error) => Err(error.into()),
                };

                if let Some(updater) = progress_updater {
                    updater.abort();
                }

                match outcome {
                    Ok(outcome) => {
                        if let Err(e) =
                            discord::delete_original(&app.discord, job.channel_id, job.message_id)
                                .await
                        {
                            tracing::warn!(error = %e, "Failed to delete original message");
                        }

                        if let Some(reply_message_id) = reply_message_id {
                            discord::cleanup_processing(
                                &app.discord,
                                job.channel_id,
                                reply_message_id,
                            )
                            .await;
                        }

                        tracing::info!(
                            first_message_id = outcome.first_message_id.get(),
                            messages_sent = outcome.messages_sent,
                            "Job completed"
                        );
                    }
                    Err(error) => {
                        tracing::warn!(error = %error, "Job failed");
                        discord::send_error(&app.discord, job.channel_id, reply_message_id, &error)
                            .await;
                    }
                }
            }
            .instrument(span)
            .await
        });

        // Cap parallel pipeline runs; backpressure is handled by the channel.
        while join_set.len() >= max_concurrency {
            match join_set.join_next().await {
                Some(Ok(())) => {}
                Some(Err(error)) => log_join_error(error),
                None => break,
            }
        }
    }

    while let Some(result) = join_set.join_next().await {
        if let Err(error) = result {
            log_join_error(error);
        }
    }

    tracing::info!("Pipeline worker shutting down");
}

/// Install a panic hook that best-effort flushes persistence to disk.
///
/// ## Trade-off acknowledgment
/// This is a crash-only path and cannot guarantee durability, but it reduces
/// the chance of losing dedup/metrics state on abrupt termination.
fn install_panic_flush_hook(engine: azalea_core::Engine) {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        previous(info);
        let engine = engine.clone();
        let _ = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build();
            if let Ok(rt) = runtime {
                rt.block_on(async {
                    engine.dedup.flush().await;
                    engine.metrics.flush().await;
                });
            }
        })
        .join();
    }));
}

/// Normalize binary paths to absolute paths where possible.
///
/// ## Rationale
/// Resolving early avoids repeated PATH lookups in hot pipeline code.
fn resolve_dependencies(binaries: &mut BinarySettings) -> anyhow::Result<()> {
    binaries.ffmpeg = resolve_binary_path(&binaries.ffmpeg, "ffmpeg")?;
    binaries.ffprobe = resolve_binary_path(&binaries.ffprobe, "ffprobe")?;
    binaries.ytdlp = resolve_binary_path(&binaries.ytdlp, "yt-dlp")?;
    Ok(())
}

/// Resolve an explicit path or a PATH-based command to a canonical location.
///
/// ## Edge-case handling
/// Any path with separators is treated as explicit to avoid surprising PATH
/// resolution when a relative path was intended.
fn resolve_binary_path(path: &Path, label: &str) -> anyhow::Result<PathBuf> {
    if path.as_os_str().is_empty() {
        anyhow::bail!("{} path is empty", label);
    }

    let has_separator = path.components().count() > 1;
    if path.is_absolute() || has_separator {
        if path.exists() {
            return Ok(path.canonicalize().unwrap_or_else(|_| path.to_path_buf()));
        }
        anyhow::bail!("{} path '{}' does not exist", label, path.display());
    }

    which::which(path).map_err(|_| anyhow::anyhow!("{} not found on PATH", label))
}

/// Ensure external binaries are available before starting.
///
/// ## Security-sensitive paths
/// Only fixed command names are executed; no user input is interpolated.
async fn verify_dependencies(binaries: &BinarySettings) -> anyhow::Result<()> {
    tracing::info!("Verifying external dependencies...");

    let checks = [
        (&binaries.ffmpeg, "ffmpeg", "-version"),
        (&binaries.ffprobe, "ffprobe", "-version"),
        (&binaries.ytdlp, "yt-dlp", "--version"),
    ];

    for (cmd, label, flag) in checks {
        match tokio::time::timeout(
            Duration::from_secs(DEPENDENCY_CHECK_TIMEOUT_SECS),
            Command::new(cmd).arg(flag).output(),
        )
        .await
        {
            Ok(Ok(output)) => {
                // Surface stderr for easier diagnosis when the tool exists but fails.
                if !output.status.success() {
                    let err = String::from_utf8_lossy(&output.stderr);
                    anyhow::bail!("Dependency '{}' check failed: {}", label, err);
                }
            }
            Ok(Err(e)) => {
                if e.kind() == std::io::ErrorKind::NotFound {
                    anyhow::bail!("Required dependency '{}' is missing", label);
                } else {
                    anyhow::bail!("Failed to execute dependency '{}': {}", label, e);
                }
            }
            Err(_) => {
                // Timeout avoids hanging startup on broken dependencies.
                anyhow::bail!("Dependency '{}' check timed out", label);
            }
        }
    }

    Ok(())
}
