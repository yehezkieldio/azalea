//! Local mass-download CLI for the azalea media pipeline.
//!
//! Reuses `azalea-core`'s resolve/download stages and hardware-fallback
//! machinery, but writes a single local file per URL instead of the bot's
//! Discord-shaped, upload-size-bounded optimize ladder (opt in with
//! `--discord-cap` to use that ladder instead). Metrics and dedup
//! persistence are disabled — see `run::run`.

mod args;
mod filename;
mod local_transcode;
mod run;

use anyhow::Result;

fn main() -> Result<()> {
    let args = args::CliArgs::parse()?;
    init_tracing();

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;

    runtime.block_on(run::run(args))
}

fn init_tracing() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn"));

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_writer(std::io::stderr)
        .compact()
        .init();
}
