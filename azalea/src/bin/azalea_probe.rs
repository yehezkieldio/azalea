//! Run the core media pipeline without Discord upload and print a JSON report.
//!
//! Usage:
//! `cargo run -p azalea --bin azalea-probe -- [--compact] [--keep-output DIR] <tweet-url>`
#![allow(unused_crate_dependencies)]

#[path = "../ids.rs"]
#[allow(dead_code)]
mod ids;

#[path = "../config.rs"]
#[allow(dead_code)]
mod config;

use anyhow::{Context as _, Result};
use azalea_core::Engine;
use azalea_core::media::parse_tweet_urls;
use azalea_core::pipeline::PreparedUpload;
use azalea_core::pipeline::probe::{self, Report};
use azalea_core::pipeline::{Progress, RequestId};
use config::AppConfig;
use serde::Serialize;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::Instant;
use tokio::sync::mpsc;

#[derive(Debug)]
struct Args {
    url: String,
    keep_output_dir: Option<PathBuf>,
    compact: bool,
}

#[derive(Debug, Serialize)]
struct CliReport {
    #[serde(flatten)]
    probe: Report,
    progress: Vec<ProgressEvent>,
}

#[derive(Debug, Serialize)]
struct ProgressEvent {
    elapsed_ms: u64,
    stage: &'static str,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    done: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    total: Option<usize>,
}

fn main() -> Result<()> {
    let args = Args::parse()?;
    init_tracing();

    let config = AppConfig::load_probe()?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(config.runtime.worker_threads)
        .max_blocking_threads(config.runtime.max_blocking_threads)
        .thread_stack_size(config.runtime.thread_stack_size)
        .enable_all()
        .build()?;

    runtime.block_on(run(args, config))
}

async fn run(args: Args, config: AppConfig) -> Result<()> {
    let tweet_url = parse_single_url(&args.url)?;
    let mut engine_config = config.engine;
    engine_config.storage.dedup_persistent = false;
    engine_config.storage.metrics_enabled = false;
    engine_config.validate()?;
    let engine = Engine::new(engine_config)?;

    let (progress_tx, progress_rx) = mpsc::channel(64);
    let progress_task = tokio::spawn(collect_progress(progress_rx));

    let input = probe::Input::new(RequestId(1), 0, 1, tweet_url);
    let run = probe::run(input, &engine, Some(progress_tx)).await;
    let progress = progress_task.await.context("progress collector panicked")?;
    let run = run?;
    let mut report = run.report;

    if let Some(dir) = args.keep_output_dir.as_ref() {
        keep_outputs(&run.prepared, &mut report, dir).await?;
    }

    let stdout = std::io::stdout();
    let mut handle = stdout.lock();
    let report = CliReport {
        probe: report,
        progress,
    };
    if args.compact {
        serde_json::to_writer(&mut handle, &report)?;
    } else {
        serde_json::to_writer_pretty(&mut handle, &report)?;
    }
    writeln!(handle)?;

    drop(run.prepared);
    engine.temp_files.shutdown().await;
    Ok(())
}

impl Args {
    fn parse() -> Result<Self> {
        let mut values = std::env::args().skip(1);
        let mut url = None;
        let mut keep_output_dir = None;
        let mut compact = false;

        while let Some(arg) = values.next() {
            match arg.as_str() {
                "-h" | "--help" => {
                    print_usage();
                    std::process::exit(0);
                }
                "--compact" => compact = true,
                "--keep-output" => {
                    let Some(dir) = values.next() else {
                        anyhow::bail!("--keep-output requires a directory");
                    };
                    keep_output_dir = Some(PathBuf::from(dir));
                }
                value if value.starts_with('-') => anyhow::bail!("unknown option: {value}"),
                value => {
                    if url.replace(value.to_owned()).is_some() {
                        anyhow::bail!(
                            "usage: azalea-probe [--compact] [--keep-output DIR] <tweet-url>"
                        );
                    }
                }
            }
        }

        let Some(url) = url else {
            anyhow::bail!("usage: azalea-probe [--compact] [--keep-output DIR] <tweet-url>");
        };

        Ok(Self {
            url,
            keep_output_dir,
            compact,
        })
    }
}

fn init_tracing() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_writer(std::io::stderr)
        .compact()
        .init();
}

fn parse_single_url(input: &str) -> Result<azalea_core::media::TweetLink> {
    let urls = parse_tweet_urls(input);
    let mut urls = urls.into_iter();
    let Some(url) = urls.next() else {
        anyhow::bail!("probe input must contain one supported Twitter/X URL");
    };
    if urls.next().is_some() {
        anyhow::bail!("probe input must contain exactly one Twitter/X URL");
    }
    Ok(url)
}

async fn collect_progress(mut rx: mpsc::Receiver<Progress>) -> Vec<ProgressEvent> {
    let started = Instant::now();
    let mut events = Vec::new();
    while let Some(progress) = rx.recv().await {
        events.push(ProgressEvent::from_progress(
            started.elapsed().as_millis() as u64,
            progress,
        ));
    }
    events
}

impl ProgressEvent {
    fn from_progress(elapsed_ms: u64, progress: Progress) -> Self {
        let (stage, done, total) = match progress {
            Progress::Resolving => ("resolving", None, None),
            Progress::Downloading => ("downloading", None, None),
            Progress::Optimizing => ("optimizing", None, None),
            Progress::Transcoding(done, total) => ("transcoding", Some(done), Some(total)),
            Progress::Uploading => ("uploading", None, None),
            Progress::UploadingSegment(done, total) => {
                ("uploading_segment", Some(done), Some(total))
            }
        };

        Self {
            elapsed_ms,
            stage,
            message: progress.to_string(),
            done,
            total,
        }
    }
}

async fn keep_outputs(prepared: &PreparedUpload, report: &mut Report, dir: &Path) -> Result<()> {
    tokio::fs::create_dir_all(dir).await?;
    let parts = prepared_parts(prepared);
    for (index, part) in parts.into_iter().enumerate() {
        let kept_path = dir.join(kept_file_name(report.tweet_id, index + 1, part.path()));
        tokio::fs::copy(part.path(), &kept_path)
            .await
            .with_context(|| {
                format!(
                    "failed to copy probe output {} to {}",
                    part.path().display(),
                    kept_path.display()
                )
            })?;
        if let Some(report_part) = report.output.parts.get_mut(index) {
            report_part.kept_path = Some(kept_path.display().to_string());
        }
    }
    Ok(())
}

fn prepared_parts(prepared: &PreparedUpload) -> Vec<&azalea_core::pipeline::PreparedPart> {
    match prepared {
        PreparedUpload::Single { part, .. } => vec![part],
        PreparedUpload::Split { parts, .. } => parts.iter().collect(),
    }
}

fn kept_file_name(tweet_id: u64, index: usize, path: &Path) -> String {
    let extension = path
        .extension()
        .and_then(std::ffi::OsStr::to_str)
        .filter(|value| !value.is_empty())
        .unwrap_or("bin");
    format!("azalea-probe-{tweet_id}-part-{index}.{extension}")
}

fn print_usage() {
    println!("usage: azalea-probe [--compact] [--keep-output DIR] <tweet-url>");
}
