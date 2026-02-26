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
use azalea_core::config::{BinarySettings, HardwareAcceleration, TranscodeSettings};
use azalea_core::media;
use azalea_core::pipeline as core_pipeline;
use azalea_core::storage::Stage;
use config::AppConfig;
use mimalloc::MiMalloc;
use std::{
    collections::hash_map::DefaultHasher,
    hash::{Hash, Hasher},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
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
const MIN_TESTED_FFMPEG_VERSION: ParsedVersion = ParsedVersion::new(6, 0, 0);
const MIN_TESTED_FFPROBE_VERSION: ParsedVersion = ParsedVersion::new(6, 0, 0);
const SHUTDOWN_STEP_TIMEOUT_SECS: u64 = 5;
const TEMP_PRUNE_INTERVAL_SECS: u64 = 30 * 60;
const TEMP_PRUNE_MAX_AGE_SECS: u64 = 60 * 60;
const JOB_DURATION_WARN_MS: u64 = 45_000;
const UPLOAD_DURATION_WARN_MS: u64 = 20_000;
const QUEUE_DEPTH_WARN_THRESHOLD: usize = 80;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
struct ParsedVersion {
    major: u64,
    minor: u64,
    patch: u64,
}

impl ParsedVersion {
    const fn new(major: u64, minor: u64, patch: u64) -> Self {
        Self {
            major,
            minor,
            patch,
        }
    }
}

impl std::fmt::Display for ParsedVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

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

#[derive(Default)]
struct WorkerDiagnostics {
    completed_jobs: AtomicU64,
    failed_jobs: AtomicU64,
    timeout_jobs: AtomicU64,
    hwacc_failures: AtomicU64,
}

fn main() -> anyhow::Result<()> {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));

    tracing_subscriber::fmt().with_env_filter(filter).init();

    let loaded = AppConfig::load()?;
    let token = loaded.auth.discord_token.into_inner();
    let config = loaded.app;

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(config.runtime.worker_threads)
        .max_blocking_threads(config.runtime.max_blocking_threads)
        .thread_stack_size(config.runtime.thread_stack_size)
        .enable_all()
        .build()?;

    runtime.block_on(async_main(config, token))
}

/// Async application bootstrap separated for testability and clearer error paths.
///
async fn async_main(config: AppConfig, token: String) -> anyhow::Result<()> {
    raise_fd_limit();
    let mut config = config;
    resolve_dependencies(&mut config.engine.binaries)?;
    log_startup_snapshot(&config);
    verify_dependencies(&config.engine.binaries, &config.engine.transcode).await?;

    // Ensure temp storage is available and writable before starting the worker.
    let temp_dir = config.engine.storage.temp_dir.clone();
    tokio::fs::create_dir_all(&temp_dir).await.map_err(|e| {
        anyhow::anyhow!("failed to create temp dir '{}': {}", temp_dir.display(), e)
    })?;
    tokio::task::spawn_blocking({
        let temp_dir = temp_dir.clone();
        move || validate_temp_dir_writable(&temp_dir)
    })
    .await
    .map_err(|e| anyhow::anyhow!("temp dir writability check task failed: {e}"))??;
    {
        let max_age = Duration::from_secs(TEMP_PRUNE_MAX_AGE_SECS);
        match tokio::task::spawn_blocking(move || {
            media::cleanup_stale_temp_entries(&temp_dir, max_age)
        })
        .await
        {
            Ok(removed_paths) if !removed_paths.is_empty() => {
                for path in removed_paths {
                    tracing::warn!(path = %path.display(), "Removed stale temp entry on startup");
                }
            }
            Ok(_) => {}
            Err(e) => {
                tracing::warn!(error = %e, "Failed to prune temp entries on startup");
            }
        }
    }
    // Initialize media regex once to avoid per-request overhead.
    media::init_regex();

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

fn validate_temp_dir_writable(temp_dir: &Path) -> anyhow::Result<()> {
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let probe_path = temp_dir.join(format!(
        ".azalea-writable-{}-{nonce}.tmp",
        std::process::id()
    ));

    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&probe_path)
        .map_err(|e| {
            anyhow::anyhow!(
                "temp dir '{}' is not writable: failed to create probe file '{}': {}",
                temp_dir.display(),
                probe_path.display(),
                e
            )
        })?;

    std::fs::remove_file(&probe_path).map_err(|e| {
        anyhow::anyhow!(
            "temp dir '{}' failed writability probe cleanup for '{}': {}",
            temp_dir.display(),
            probe_path.display(),
            e
        )
    })?;

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
    tracing::info!(
        max_concurrency = app.engine.config.concurrency.pipeline.max(1),
        queue_warn_threshold = QUEUE_DEPTH_WARN_THRESHOLD,
        "Pipeline worker started"
    );

    let max_concurrency = app.engine.config.concurrency.pipeline.max(1) as usize;
    let diagnostics = Arc::new(WorkerDiagnostics::default());
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
        let diagnostics = Arc::clone(&diagnostics);
        let queue_guard = QueueDepthGuard::new(Arc::clone(&app.queue_depth));
        let queue_depth_at_start = app.queue_depth.load(Ordering::Relaxed);
        if queue_depth_at_start >= QUEUE_DEPTH_WARN_THRESHOLD {
            tracing::warn!(
                queue_depth = queue_depth_at_start,
                queue_capacity = JOB_QUEUE_CAPACITY,
                "Queue depth is approaching capacity"
            );
        }
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
                trigger_id = job.trigger_id,
                tweet_id = job.tweet_url.tweet_id.0,
                channel_id = job.channel_id.get(),
                author_id = job.author_id.get(),
                source_kind = if job.source_message_id.is_some() { "message" } else { "command" },
                queue_depth = queue_depth_at_start,
                tweet_url = %job.tweet_url.original_url()
            );

            async {
                let job_started_at = Instant::now();
                let reply_message_id =
                    discord::send_processing(&app.discord, job.channel_id, job.source_message_id)
                        .await;

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
                        match pipeline::upload::upload(
                            &prepared,
                            job.channel_id,
                            job.tweet_url.tweet_id,
                            &app.discord,
                            &app.engine.permits,
                            &app.engine.config,
                            Some(&progress_tx),
                        )
                        .await
                        {
                            Ok(outcome) => {
                                let upload_elapsed_ms = upload_start.elapsed().as_millis() as u64;
                                // Record metrics and mark dedup only on success.
                                app.engine
                                    .metrics
                                    .record_stage_duration(Stage::Upload, upload_elapsed_ms);
                                if upload_elapsed_ms > UPLOAD_DURATION_WARN_MS {
                                    tracing::warn!(
                                        upload_elapsed_ms,
                                        threshold_ms = UPLOAD_DURATION_WARN_MS,
                                        "Upload duration exceeded warning threshold"
                                    );
                                } else {
                                    tracing::debug!(
                                        upload_elapsed_ms,
                                        "Upload completed within threshold"
                                    );
                                }
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
                        let elapsed_ms = job_started_at.elapsed().as_millis() as u64;
                        diagnostics.completed_jobs.fetch_add(1, Ordering::Relaxed);
                        if let Some(source_message_id) = job.source_message_id
                            && let Err(e) = discord::delete_original(
                                &app.discord,
                                job.channel_id,
                                source_message_id,
                            )
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
                            duration_ms = elapsed_ms,
                            first_message_id = outcome.first_message_id.get(),
                            messages_sent = outcome.messages_sent,
                            "Job completed"
                        );
                        if elapsed_ms > JOB_DURATION_WARN_MS {
                            tracing::warn!(
                                duration_ms = elapsed_ms,
                                threshold_ms = JOB_DURATION_WARN_MS,
                                "Job duration exceeded warning threshold"
                            );
                        }
                    }
                    Err(error) => {
                        diagnostics.completed_jobs.fetch_add(1, Ordering::Relaxed);
                        diagnostics.failed_jobs.fetch_add(1, Ordering::Relaxed);
                        if is_timeout_error(&error) {
                            diagnostics.timeout_jobs.fetch_add(1, Ordering::Relaxed);
                        }
                        if is_hwacc_error(&error) {
                            diagnostics.hwacc_failures.fetch_add(1, Ordering::Relaxed);
                        }
                        log_job_failure_diagnostics(&error, job_started_at.elapsed());
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

    tracing::info!(
        completed_jobs = diagnostics.completed_jobs.load(Ordering::Relaxed),
        failed_jobs = diagnostics.failed_jobs.load(Ordering::Relaxed),
        timeout_jobs = diagnostics.timeout_jobs.load(Ordering::Relaxed),
        hwacc_failures = diagnostics.hwacc_failures.load(Ordering::Relaxed),
        "Pipeline worker shutting down"
    );
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
async fn verify_dependencies(
    binaries: &BinarySettings,
    transcode: &TranscodeSettings,
) -> anyhow::Result<()> {
    tracing::info!("Verifying external dependencies...");

    let checks = [
        (
            &binaries.ffmpeg,
            "ffmpeg",
            "-version",
            Some(MIN_TESTED_FFMPEG_VERSION),
        ),
        (
            &binaries.ffprobe,
            "ffprobe",
            "-version",
            Some(MIN_TESTED_FFPROBE_VERSION),
        ),
        (&binaries.ytdlp, "yt-dlp", "--version", None),
    ];

    for (cmd, label, flag, minimum_tested_version) in checks {
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
                    anyhow::bail!("dependency '{}' check failed: {}", label, err);
                }
                let version = command_version_summary(&output);
                tracing::info!(
                    dependency = label,
                    path = %cmd.display(),
                    version = %version,
                    "Dependency ready"
                );
                if let Some(minimum_tested_version) = minimum_tested_version {
                    if let Some(parsed_version) = parse_version_from_summary(&version) {
                        if parsed_version < minimum_tested_version {
                            tracing::warn!(
                                dependency = label,
                                path = %cmd.display(),
                                version = %parsed_version,
                                minimum_tested_version = %minimum_tested_version,
                                "Dependency version is below minimum tested version"
                            );
                        }
                    } else {
                        tracing::warn!(
                            dependency = label,
                            path = %cmd.display(),
                            version = %version,
                            minimum_tested_version = %minimum_tested_version,
                            "Failed to parse dependency version for minimum tested version check"
                        );
                    }
                }
            }
            Ok(Err(e)) => {
                if e.kind() == std::io::ErrorKind::NotFound {
                    anyhow::bail!("required dependency '{}' is missing", label);
                } else {
                    anyhow::bail!("failed to execute dependency '{}': {}", label, e);
                }
            }
            Err(_) => {
                // Timeout avoids hanging startup on broken dependencies.
                anyhow::bail!("dependency '{}' check timed out", label);
            }
        }
    }

    if let Err(error) = log_hardware_acceleration_diagnostics(binaries, transcode).await {
        tracing::warn!(error = %error, "Failed to collect hardware acceleration diagnostics");
    }

    Ok(())
}

async fn log_hardware_acceleration_diagnostics(
    binaries: &BinarySettings,
    transcode: &TranscodeSettings,
) -> anyhow::Result<()> {
    let configured_backend = transcode.hardware_acceleration;
    let encoder = configured_backend.encoder();
    tracing::debug!(?configured_backend, "Running hardware acceleration check");
    tracing::info!(
        ?configured_backend,
        encoder,
        ffmpeg_path = %binaries.ffmpeg.display(),
        "Hardware acceleration configuration loaded"
    );

    if configured_backend == HardwareAcceleration::None {
        tracing::info!("Hardware acceleration disabled by config; using software encoding");
        return Ok(());
    }

    if configured_backend == HardwareAcceleration::Vaapi {
        let vaapi_device = Path::new(transcode.vaapi_device.as_ref());
        if vaapi_device.exists() {
            tracing::info!(vaapi_device = %vaapi_device.display(), "Configured VAAPI device exists");
        } else {
            tracing::warn!(
                vaapi_device = %vaapi_device.display(),
                "Configured VAAPI device not found; ffmpeg may fail"
            );
        }
    }

    let encoder_available = ffmpeg_supports_encoder(&binaries.ffmpeg, encoder).await?;
    if encoder_available {
        tracing::info!(
            ?configured_backend,
            encoder,
            "Configured hardware acceleration encoder is available"
        );
    } else {
        tracing::warn!(
            ?configured_backend,
            encoder,
            "Configured hardware acceleration encoder is not available in ffmpeg"
        );
    }

    let hwaccel_backend = match configured_backend {
        HardwareAcceleration::None => None,
        HardwareAcceleration::Nvenc => Some("cuda"),
        HardwareAcceleration::Vaapi => Some("vaapi"),
        HardwareAcceleration::VideoToolbox => Some("videotoolbox"),
    };
    if let Some(hwaccel_backend) = hwaccel_backend {
        let hwaccel_available = ffmpeg_supports_hwaccel(&binaries.ffmpeg, hwaccel_backend).await?;
        tracing::debug!(
            ?configured_backend,
            hwaccel_backend,
            hwaccel_available,
            "Hardware acceleration backend diagnostics"
        );
    }

    Ok(())
}

async fn ffmpeg_supports_encoder(ffmpeg_path: &Path, encoder: &str) -> anyhow::Result<bool> {
    let output = tokio::time::timeout(
        Duration::from_secs(DEPENDENCY_CHECK_TIMEOUT_SECS),
        Command::new(ffmpeg_path)
            .args(["-hide_banner", "-encoders"])
            .output(),
    )
    .await
    .map_err(|_| anyhow::anyhow!("ffmpeg encoder probe timed out"))??;

    if !output.status.success() {
        anyhow::bail!(
            "ffmpeg encoder probe failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let found = stdout
        .lines()
        .any(|line| line.split_whitespace().any(|token| token.trim() == encoder));
    tracing::trace!(encoder, found, "ffmpeg encoder probe result");
    Ok(found)
}

async fn ffmpeg_supports_hwaccel(ffmpeg_path: &Path, backend: &str) -> anyhow::Result<bool> {
    let output = tokio::time::timeout(
        Duration::from_secs(DEPENDENCY_CHECK_TIMEOUT_SECS),
        Command::new(ffmpeg_path)
            .args(["-hide_banner", "-hwaccels"])
            .output(),
    )
    .await
    .map_err(|_| anyhow::anyhow!("ffmpeg hwaccel probe timed out"))??;

    if !output.status.success() {
        anyhow::bail!(
            "ffmpeg hwaccel probe failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let found = stdout.lines().any(|line| line.trim() == backend);
    tracing::trace!(backend, found, "ffmpeg hwaccel backend probe result");
    Ok(found)
}

fn command_version_summary(output: &std::process::Output) -> String {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    stdout
        .lines()
        .chain(stderr.lines())
        .find(|line| !line.trim().is_empty())
        .map(|line| line.trim().to_string())
        .unwrap_or_else(|| "unknown-version".to_string())
}

fn parse_version_from_summary(summary: &str) -> Option<ParsedVersion> {
    let candidate = summary
        .split_whitespace()
        .skip_while(|token| *token != "version")
        .nth(1)
        .or_else(|| {
            summary
                .split_whitespace()
                .find(|token| token.chars().any(|ch| ch.is_ascii_digit()))
        })?;

    let mut values = [0_u64; 3];
    let mut found = 0_usize;
    let mut current = 0_u64;
    let mut in_digits = false;

    for ch in candidate.chars() {
        if ch.is_ascii_digit() {
            in_digits = true;
            current = current
                .saturating_mul(10)
                .saturating_add((ch as u8 - b'0') as u64);
            continue;
        }

        if in_digits {
            let slot = values.get_mut(found)?;
            *slot = current;
            found += 1;
            if found == values.len() {
                break;
            }
            current = 0;
            in_digits = false;
        }
    }

    if in_digits && found < values.len() {
        let slot = values.get_mut(found)?;
        *slot = current;
        found += 1;
    }

    if found == 0 {
        return None;
    }

    let mut values = values.into_iter();
    Some(ParsedVersion::new(
        values.next().unwrap_or(0),
        values.next().unwrap_or(0),
        values.next().unwrap_or(0),
    ))
}

fn log_startup_snapshot(config: &AppConfig) {
    let runtime = &config.runtime;
    let engine = &config.engine;
    let transcode = &engine.transcode;
    tracing::info!(
        worker_threads = runtime.worker_threads,
        max_blocking_threads = runtime.max_blocking_threads,
        pipeline_concurrency = engine.concurrency.pipeline,
        transcode_concurrency = engine.concurrency.transcode,
        upload_limit_bytes = transcode.max_upload_bytes,
        hwacc = ?transcode.hardware_acceleration,
        encoder = transcode.hardware_acceleration.encoder(),
        temp_dir = %engine.storage.temp_dir.display(),
        "Effective runtime snapshot"
    );
    tracing::debug!(
        ffmpeg = %engine.binaries.ffmpeg.display(),
        ffprobe = %engine.binaries.ffprobe.display(),
        ytdlp = %engine.binaries.ytdlp.display(),
        vaapi_device = %transcode.vaapi_device,
        max_download_bytes = engine.pipeline.max_download_bytes,
        download_timeout_secs = engine.pipeline.download_timeout_secs,
        upload_timeout_secs = engine.pipeline.upload_timeout_secs,
        queue_capacity = JOB_QUEUE_CAPACITY,
        "Startup diagnostics details"
    );
}

fn is_timeout_error(error: &pipeline::Error) -> bool {
    matches!(
        error,
        pipeline::Error::Core(core_pipeline::Error::Timeout { .. })
    )
}

fn is_hwacc_error(error: &pipeline::Error) -> bool {
    match error {
        pipeline::Error::Core(core_pipeline::Error::TranscodeFailed { stderr_tail, .. }) => {
            let stderr = stderr_tail.to_ascii_lowercase();
            stderr.contains("vaapi")
                || stderr.contains("nvenc")
                || stderr.contains("videotoolbox")
                || stderr.contains("cuda")
                || stderr.contains("hwaccel")
        }
        _ => false,
    }
}

fn stable_hash(value: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    value.hash(&mut hasher);
    hasher.finish()
}

fn log_job_failure_diagnostics(error: &pipeline::Error, elapsed: Duration) {
    match error {
        pipeline::Error::Core(core_pipeline::Error::TranscodeFailed {
            stage,
            exit_code,
            stderr_tail,
        }) => {
            tracing::warn!(
                error_kind = "transcode_failed",
                stage = ?stage,
                exit_code,
                duration_ms = elapsed.as_millis() as u64,
                stderr_tail_hash = stable_hash(stderr_tail),
                is_hwacc_path = is_hwacc_error(error),
                "Job failed"
            );
            tracing::debug!(stderr_tail = %stderr_tail, "Transcode stderr tail");
        }
        pipeline::Error::Core(core_pipeline::Error::Timeout {
            operation,
            duration,
        }) => {
            tracing::warn!(
                error_kind = "timeout",
                operation,
                timeout_ms = duration.as_millis() as u64,
                duration_ms = elapsed.as_millis() as u64,
                "Job failed"
            );
        }
        pipeline::Error::Core(core_pipeline::Error::ResolveFailed { resolver, source }) => {
            tracing::warn!(
                error_kind = "resolve_failed",
                resolver,
                source = %source,
                duration_ms = elapsed.as_millis() as u64,
                "Job failed"
            );
        }
        pipeline::Error::Core(core_pipeline::Error::DownloadFailed { source }) => {
            tracing::warn!(
                error_kind = "download_failed",
                source = %source,
                duration_ms = elapsed.as_millis() as u64,
                "Job failed"
            );
        }
        pipeline::Error::Core(core_pipeline::Error::DiskSpace {
            available_mb,
            required_mb,
        }) => {
            tracing::warn!(
                error_kind = "disk_space",
                available_mb,
                required_mb,
                duration_ms = elapsed.as_millis() as u64,
                "Job failed"
            );
        }
        pipeline::Error::Core(core_pipeline::Error::Duplicate) => {
            tracing::debug!(
                error_kind = "duplicate",
                duration_ms = elapsed.as_millis() as u64,
                "Duplicate request skipped"
            );
        }
        pipeline::Error::Core(core_pipeline::Error::Io(source)) => {
            tracing::warn!(
                error_kind = "io",
                source = %source,
                duration_ms = elapsed.as_millis() as u64,
                "Job failed"
            );
        }
        pipeline::Error::UploadFailed {
            part,
            total,
            source,
        } => {
            tracing::warn!(
                error_kind = "upload_failed",
                part,
                total,
                source = %source,
                duration_ms = elapsed.as_millis() as u64,
                "Job failed"
            );
        }
        pipeline::Error::DiscordApi { operation, source } => {
            tracing::warn!(
                error_kind = "discord_api",
                operation,
                source = %source,
                duration_ms = elapsed.as_millis() as u64,
                "Job failed"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]
    use super::*;

    fn unique_temp_file(name: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        std::env::temp_dir().join(format!("azalea-{name}-{nanos}"))
    }

    fn unique_temp_dir(name: &str) -> PathBuf {
        unique_temp_file(name)
    }

    #[test]
    fn resolve_binary_path_rejects_missing_explicit_paths() {
        let missing = Path::new("./definitely-missing-binary");
        let err = resolve_binary_path(missing, "test-binary")
            .expect_err("missing explicit path should fail");
        assert!(err.to_string().contains("does not exist"));
    }

    #[test]
    fn resolve_binary_path_accepts_existing_explicit_paths() {
        let file = unique_temp_file("bin-path");
        std::fs::write(&file, b"#!/bin/sh\nexit 0\n").expect("write temp file");

        let resolved = resolve_binary_path(&file, "temp-binary")
            .expect("existing explicit path should resolve");
        assert!(resolved.exists());

        let _ = std::fs::remove_file(file);
    }

    #[test]
    fn parse_version_from_summary_extracts_ffmpeg_semver() {
        let parsed = parse_version_from_summary("ffmpeg version 6.1.2-1ubuntu1")
            .expect("ffmpeg summary should parse");
        assert_eq!(parsed, ParsedVersion::new(6, 1, 2));
    }

    #[test]
    fn parse_version_from_summary_handles_prefixed_versions() {
        let parsed = parse_version_from_summary("ffprobe version n7.0-static")
            .expect("ffprobe summary should parse");
        assert_eq!(parsed, ParsedVersion::new(7, 0, 0));
    }

    #[test]
    fn validate_temp_dir_writable_accepts_writable_directory() {
        let dir = unique_temp_dir("writable-temp-dir");
        std::fs::create_dir_all(&dir).expect("create temp dir");

        validate_temp_dir_writable(&dir).expect("writable temp dir should pass");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn validate_temp_dir_writable_reports_clear_diagnostic() {
        let missing_dir = unique_temp_dir("missing-writable-dir");
        let err = validate_temp_dir_writable(&missing_dir).expect_err("missing dir should fail");
        let message = err.to_string();
        assert!(message.contains("is not writable"));
        assert!(message.contains(&missing_dir.display().to_string()));
    }
}
