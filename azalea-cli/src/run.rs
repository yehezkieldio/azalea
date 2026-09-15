use anyhow::{Context, Result};
use azalea_core::Engine;
use azalea_core::config::EngineSettings;
use azalea_core::media::TweetLink;
use azalea_core::pipeline::download::DownloadProgress;
use azalea_core::pipeline::types::sanitize_extension;
use azalea_core::pipeline::{Job, PreparedUpload, RequestId, download, optimize};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
use tokio::task::JoinSet;

use crate::args::CliArgs;
use crate::filename;
use crate::local_transcode::{self, TranscodeOptions};

pub async fn run(args: CliArgs) -> Result<()> {
    for tool in ["ffmpeg", "ffprobe", "yt-dlp"] {
        which::which(tool).with_context(|| {
            format!("`{tool}` was not found on PATH; azalea-cli needs it to run")
        })?;
    }

    tokio::fs::create_dir_all(&args.output_dir)
        .await
        .with_context(|| format!("create output directory {}", args.output_dir.display()))?;

    let engine = build_engine(&args)?;

    if args.is_interactive() {
        run_interactive(&engine, &args).await;
    } else {
        let urls = collect_urls(&args).await?;
        if urls.is_empty() {
            anyhow::bail!("no valid Twitter/X URLs found in the given input");
        }
        run_batch(&engine, &args, urls).await;
    }

    engine.temp_files.shutdown().await;
    Ok(())
}

/// The bot's timeouts (`pipeline.download_timeout_secs` defaults to 60s,
/// tuned for Discord's own upload urgency) are too tight for a CLI
/// downloading large files or transcoding slowly on weak hardware — see
/// the target-hardware notes on software AV1 encode time. `EngineSettings`
/// has no "no timeout" option, and `validate` hard-caps every timeout at
/// this value, so this is the longest a stage can run rather than truly
/// unbounded.
const MAX_STAGE_TIMEOUT_SECS: u64 = 3600;

fn build_engine(args: &CliArgs) -> Result<Engine> {
    let mut engine_config = EngineSettings::default();
    engine_config.storage.dedup_persistent = false;
    engine_config.storage.metrics_enabled = false;
    engine_config.concurrency.download = args.concurrency;
    engine_config.concurrency.transcode = args.concurrency;
    engine_config.concurrency.pipeline = args.concurrency;
    engine_config.pipeline.download_timeout_secs = MAX_STAGE_TIMEOUT_SECS;
    engine_config.pipeline.resolver_timeout_secs = MAX_STAGE_TIMEOUT_SECS;
    engine_config.pipeline.ytdlp_timeout_secs = MAX_STAGE_TIMEOUT_SECS;
    engine_config.pipeline.upload_timeout_secs = MAX_STAGE_TIMEOUT_SECS;
    engine_config.transcode.ffmpeg_timeout_secs = MAX_STAGE_TIMEOUT_SECS;
    engine_config.transcode.ffprobe_timeout_secs = MAX_STAGE_TIMEOUT_SECS;
    // `Engine::new` seeds `TranscodeRuntime`'s configured backend from this
    // value; `execute_with_hwacc_fallback` substitutes the runtime's active
    // backend into every hardware encode attempt (see
    // `TranscodeRuntime::effective_settings`), so `--hw` has no effect
    // unless it is set here rather than only on the per-job settings built
    // in `local_transcode::produce`.
    engine_config.transcode.hardware_acceleration = args.hw.resolve();
    engine_config
        .validate()
        .context("invalid engine configuration")?;

    Engine::new(engine_config).context("build engine")
}

/// Process a fixed batch of URLs (positional args and/or `--input`),
/// bounded by `-j` concurrent jobs.
async fn run_batch(engine: &Engine, args: &CliArgs, urls: Vec<TweetLink>) {
    let job_id = Arc::new(AtomicU64::new(1));

    let mut tasks = JoinSet::new();
    let mut permits_used = 0usize;
    let mut pending = urls.into_iter();

    // Bound in-flight jobs by `-j` with a simple fill-as-you-go JoinSet loop:
    // spawn up to the limit, then spawn one more each time a task finishes.
    loop {
        while permits_used < args.concurrency as usize {
            let Some(tweet_url) = pending.next() else {
                break;
            };
            permits_used += 1;
            let engine = engine.clone();
            let job_id = Arc::clone(&job_id);
            let args = args.clone();
            tasks.spawn(async move {
                let id = job_id.fetch_add(1, Ordering::Relaxed);
                eprintln!("[{id}] got {} — resolving...", tweet_url.original_url());
                process_one(&engine, &args, tweet_url, id).await
            });
        }

        let Some(result) = tasks.join_next().await else {
            break;
        };
        permits_used -= 1;

        match result {
            Ok(Ok(outcome)) => {
                println!("{outcome}");
            }
            Ok(Err(error)) => {
                eprintln!("error: {error:#}");
            }
            Err(join_error) => {
                eprintln!("error: job panicked: {join_error}");
            }
        }
    }
}

/// Interactive mode: prompt for one URL at a time, process it immediately,
/// then prompt again — avoids re-typing the whole command for each link.
/// Runs one job at a time by design (matches how a person pastes links),
/// but reuses the same `Engine` across the whole session (warm resolver
/// caches, no repeated client/permit setup).
async fn run_interactive(engine: &Engine, args: &CliArgs) {
    println!("azalea-cli interactive mode — paste a Twitter/X URL and press Enter.");
    println!("Type 'exit', 'quit', or press Ctrl+D to stop.\n");

    let mut job_id = 1u64;
    let mut stdout = tokio::io::stdout();
    let mut lines = tokio::io::BufReader::new(tokio::io::stdin()).lines();

    loop {
        print!("azalea> ");
        if stdout.flush().await.is_err() {
            break;
        }

        let line = match lines.next_line().await {
            Ok(Some(line)) => line,
            Ok(None) => break, // EOF (Ctrl+D)
            Err(error) => {
                eprintln!("error reading input: {error}");
                continue;
            }
        };

        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if matches!(line, "exit" | "quit") {
            break;
        }

        let urls = azalea_core::media::parse_tweet_urls(line).into_vec();
        if urls.is_empty() {
            eprintln!("no Twitter/X URL found in that line — try again.");
            continue;
        }

        for tweet_url in urls {
            job_id += 1;
            eprintln!("[{job_id}] got {} — resolving...", tweet_url.original_url());
            match process_one(engine, args, tweet_url, job_id).await {
                Ok(outcome) => println!("{outcome}"),
                Err(error) => eprintln!("error: {error:#}"),
            }
        }
    }

    println!("bye!");
}

/// Poll `progress` on an interval and print a self-overwriting line, so a
/// long download shows live movement instead of a single static
/// "downloading..." message. Caller aborts the returned task once the
/// download completes (success or failure).
fn spawn_download_ticker(
    job_id: u64,
    progress: Arc<DownloadProgress>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_millis(500));
        loop {
            interval.tick().await;
            let downloaded = progress.downloaded.load(Ordering::Relaxed);
            let total = progress.total.load(Ordering::Relaxed);
            let downloaded_mb = downloaded as f64 / 1024.0 / 1024.0;

            let line = if total > 0 {
                let percent = (downloaded as f64 / total as f64 * 100.0).min(100.0);
                let total_mb = total as f64 / 1024.0 / 1024.0;
                format!(
                    "[{job_id}] downloading... {downloaded_mb:.1} MB / {total_mb:.1} MB ({percent:.0}%)"
                )
            } else {
                format!("[{job_id}] downloading... {downloaded_mb:.1} MB")
            };
            // Left-aligned padding to 80 columns overwrites any leftover
            // characters from a longer previous line.
            eprint!("\r{line:80}");
            let _ = std::io::Write::flush(&mut std::io::stderr());
        }
    })
}

async fn collect_urls(args: &CliArgs) -> Result<Vec<TweetLink>> {
    let mut content = args.urls.join("\n");
    if let Some(path) = &args.input_file {
        let file_content = tokio::fs::read_to_string(path)
            .await
            .with_context(|| format!("read --input file {}", path.display()))?;
        content.push('\n');
        content.push_str(&file_content);
    }

    Ok(azalea_core::media::parse_tweet_urls(&content).into_vec())
}

async fn process_one(
    engine: &Engine,
    args: &CliArgs,
    tweet_url: TweetLink,
    job_id: u64,
) -> Result<String> {
    let job = Job::new(RequestId(job_id), 0, job_id, tweet_url);

    let resolved = engine
        .resolver
        .resolve(&job.tweet_url, &engine.http, &engine.permits)
        .await
        .with_context(|| format!("resolve {}", job.tweet_url.original_url()))?;

    eprintln!(
        "[{job_id}] resolved ({:?}) — downloading...",
        resolved.media_type
    );

    let progress = Arc::new(DownloadProgress::default());
    let ticker = spawn_download_ticker(job_id, Arc::clone(&progress));

    let downloaded = download::download(
        resolved.as_ref(),
        &job,
        &engine.permits,
        &engine.reserved_download_bytes,
        &engine.temp_files,
        &engine.config,
        &engine.pinned_media_clients,
        Some(progress.as_ref()),
    )
    .await;

    ticker.abort();
    eprint!("\r{:80}\r", "");
    let downloaded =
        downloaded.with_context(|| format!("download {}", job.tweet_url.original_url()))?;

    eprintln!(
        "[{job_id}] downloaded {} bytes — {}...",
        downloaded.size,
        if args.discord_cap {
            "optimizing"
        } else {
            "transcoding"
        }
    );

    if args.discord_cap {
        let prepared = optimize::optimize(
            downloaded,
            resolved.as_ref(),
            &engine.permits,
            &engine.temp_files,
            &engine.config,
            &engine.transcode_runtime,
            None,
        )
        .await
        .with_context(|| format!("optimize {}", job.tweet_url.original_url()))?;

        return keep_discord_parts(&prepared, args, &job.tweet_url).await;
    }

    let ext = if args.codec.target_codec().is_none() {
        downloaded
            .path
            .extension()
            .and_then(std::ffi::OsStr::to_str)
            .map(sanitize_extension)
            .unwrap_or_else(|| "mp4".to_string())
    } else if args.codec.target_codec() == Some(azalea_core::config::TargetVideoCodec::H264) {
        "mp4".to_string()
    } else {
        "mkv".to_string()
    };

    let name = filename::base_name(&job.tweet_url.user, job.tweet_url.tweet_id.0, &ext);
    let final_path = filename::unique_path(&args.output_dir, &name).await;

    let opts = TranscodeOptions {
        codec: args.codec,
        quality: args.quality,
        crf: args.crf,
        max_size_bytes: args.max_size_bytes,
        hw: args.hw.resolve(),
        concurrency: args.concurrency,
    };

    let output =
        local_transcode::produce(&downloaded, resolved.as_ref(), engine, &opts, &final_path)
            .await
            .with_context(|| format!("transcode {}", job.tweet_url.original_url()))?;

    Ok(format!(
        "{} -> {} ({} bytes, {}{})",
        job.tweet_url.original_url(),
        output.path.display(),
        output.size,
        output.mode,
        output
            .encoder
            .map(|encoder| format!(", {encoder}"))
            .unwrap_or_default()
    ))
}

async fn keep_discord_parts(
    prepared: &PreparedUpload,
    args: &CliArgs,
    tweet_url: &TweetLink,
) -> Result<String> {
    let parts: Vec<_> = match prepared {
        PreparedUpload::Single { part, .. } => vec![part],
        PreparedUpload::Split { parts, .. } => parts.iter().collect(),
    };

    let mut lines = Vec::with_capacity(parts.len());
    for (index, part) in parts.iter().enumerate() {
        let ext = part
            .path()
            .extension()
            .and_then(std::ffi::OsStr::to_str)
            .map(sanitize_extension)
            .unwrap_or_else(|| "mp4".to_string());
        let base = filename::base_name(&tweet_url.user, tweet_url.tweet_id.0, &ext);
        let name = if parts.len() > 1 {
            with_part_suffix(&base, index + 1)
        } else {
            base
        };
        let final_path = filename::unique_path(&args.output_dir, &name).await;
        tokio::fs::copy(part.path(), &final_path)
            .await
            .with_context(|| {
                format!("copy {} to {}", part.path().display(), final_path.display())
            })?;
        lines.push(format!(
            "{} -> {} ({} bytes, discord-cap)",
            tweet_url.original_url(),
            final_path.display(),
            part.size()
        ));
    }

    Ok(lines.join("\n"))
}

fn with_part_suffix(name: &str, index: usize) -> String {
    let path = Path::new(name);
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    match path.extension() {
        Some(ext) => format!("{stem}-part{index}.{}", ext.to_string_lossy()),
        None => format!("{stem}-part{index}"),
    }
}
