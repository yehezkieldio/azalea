//! Slash command registration and response formatting.
//!
//! ## Design rationale
//! Responses are ephemeral to avoid channel noise and to keep status/config
//! output visible only to the requesting user.

use crate::app::App;
use crate::config::ApplicationId;
use azalea_core::storage::Stage;
use std::sync::atomic::Ordering;
use twilight_http::Client;
use twilight_model::application::command::CommandType;
use twilight_model::application::interaction::{Interaction, InteractionData};
use twilight_model::channel::message::MessageFlags;
use twilight_model::http::interaction::{InteractionResponse, InteractionResponseType};
use twilight_model::id::Id;
use twilight_model::id::marker::InteractionMarker;
use twilight_util::builder::InteractionResponseDataBuilder;
use twilight_util::builder::command::{CommandBuilder, SubCommandBuilder};

/// Register global slash commands for this application.
///
/// This is safe to call on startup; Discord will upsert existing commands.
pub async fn register(client: &Client, application_id: ApplicationId) -> anyhow::Result<()> {
    let commands = build_commands();
    client
        .interaction(application_id.get())
        .set_global_commands(&commands)
        .await?
        .model()
        .await?;
    Ok(())
}

/// Handle application commands for the `/azalea` namespace.
///
/// ## Preconditions
/// - Interaction data must be a command payload with name `azalea`.
pub async fn handle_interaction(app: &App, interaction: Interaction) -> anyhow::Result<()> {
    let Some(data) = interaction.data else {
        return Ok(());
    };

    let InteractionData::ApplicationCommand(command_data) = data else {
        return Ok(());
    };

    if command_data.name != "azalea" {
        return Ok(());
    }

    let subcommand = command_data
        .options
        .first()
        .map(|opt| opt.name.as_str())
        .unwrap_or("status");

    let content = match subcommand {
        "stats" => format_stats(app),
        "config" => format_config(app),
        _ => format_status(app).await,
    };

    // Ephemeral responses keep status/config details from cluttering channels.
    let response = InteractionResponse {
        kind: InteractionResponseType::ChannelMessageWithSource,
        data: Some(
            InteractionResponseDataBuilder::new()
                .content(content)
                .flags(MessageFlags::EPHEMERAL)
                .build(),
        ),
    };

    respond(
        &app.discord,
        app.config.application_id.get(),
        interaction.id,
        &interaction.token,
        response,
    )
    .await?;

    Ok(())
}

fn build_commands() -> Vec<twilight_model::application::command::Command> {
    let command = CommandBuilder::new("azalea", "Azalea commands", CommandType::ChatInput)
        .option(SubCommandBuilder::new("status", "Show bot status"))
        .option(SubCommandBuilder::new("stats", "Show pipeline statistics"))
        .option(SubCommandBuilder::new(
            "config",
            "Show configuration summary",
        ))
        .build();

    vec![command]
}

/// Issue a single interaction response and propagate errors.
async fn respond(
    client: &Client,
    application_id: Id<twilight_model::id::marker::ApplicationMarker>,
    interaction_id: Id<InteractionMarker>,
    token: &str,
    response: InteractionResponse,
) -> anyhow::Result<()> {
    client
        .interaction(application_id)
        .create_response(interaction_id, token, &response)
        .await?;
    Ok(())
}

async fn format_status(app: &App) -> String {
    let uptime = app.start_time.elapsed();
    let snapshot = app.engine.metrics.snapshot();
    let queue_depth = app.queue_depth.load(Ordering::Relaxed);
    let dedup_entries = app.engine.dedup.cache_entries();
    let pending_writes = app.engine.dedup.pending_writes_len().await;
    let resolver_stats = app.engine.resolver.cache_stats();

    format!(
        "Azalea is online.\nUptime: {}s\nQueue depth: {}\nTotal runs: {} ({} ok / {} failed)\nDedup cache: {} entries ({} pending writes)\nResolver cache: {} ok / {} negative",
        uptime.as_secs(),
        queue_depth,
        snapshot.total_runs,
        snapshot.successes,
        snapshot.failures,
        dedup_entries,
        pending_writes,
        resolver_stats.positive_entries,
        resolver_stats.negative_entries
    )
}

fn format_stats(app: &App) -> String {
    let snapshot = app.engine.metrics.snapshot();
    let stage_avg = snapshot.stage_avg_ms;
    let resolve_avg = stage_avg.get(Stage::Resolve as usize).copied().unwrap_or(0);
    let download_avg = stage_avg
        .get(Stage::Download as usize)
        .copied()
        .unwrap_or(0);
    let optimize_avg = stage_avg
        .get(Stage::Optimize as usize)
        .copied()
        .unwrap_or(0);
    let upload_avg = stage_avg.get(Stage::Upload as usize).copied().unwrap_or(0);

    format!(
        "Pipeline stats:\nTotal runs: {}\nSuccesses: {}\nFailures: {}\nAvg resolve: {} ms\nAvg download: {} ms\nAvg optimize: {} ms\nAvg upload: {} ms",
        snapshot.total_runs,
        snapshot.successes,
        snapshot.failures,
        resolve_avg,
        download_avg,
        optimize_avg,
        upload_avg
    )
}

/// Render a minimal, non-sensitive configuration summary.
///
/// ## Security-sensitive paths
/// This deliberately omits tokens, file paths, and hostnames.
fn format_config(app: &App) -> String {
    let config = &app.engine.config;

    format!(
        "Config summary:\nmax_upload_bytes={}\nconcurrency: pipeline={} download={} upload={} transcode={} ytdlp={}\nquality_preset={:?} hw_accel={:?}",
        config.transcode.max_upload_bytes,
        config.concurrency.pipeline,
        config.concurrency.download,
        config.concurrency.upload,
        config.concurrency.transcode,
        config.concurrency.ytdlp,
        config.transcode.quality_preset,
        config.transcode.hardware_acceleration,
    )
}
