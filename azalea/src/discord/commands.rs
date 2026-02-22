//! Slash command registration and response formatting.
//!
//! ## Design rationale
//! Responses are ephemeral to avoid channel noise and to keep status/config
//! output visible only to the requesting user.

use crate::app::App;
use crate::config::ApplicationId;
use crate::pipeline::{Job, RequestId};
use azalea_core::media;
use azalea_core::storage::Stage;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tokio::sync::mpsc;
use twilight_http::Client;
use twilight_model::application::command::CommandType;
use twilight_model::application::interaction::application_command::{
    CommandDataOption, CommandOptionValue,
};
use twilight_model::application::interaction::{Interaction, InteractionData};
use twilight_model::channel::message::MessageFlags;
use twilight_model::http::interaction::{InteractionResponse, InteractionResponseType};
use twilight_model::id::Id;
use twilight_model::id::marker::InteractionMarker;
use twilight_util::builder::InteractionResponseDataBuilder;
use twilight_util::builder::command::{CommandBuilder, StringBuilder};

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

/// Handle application commands.
///
/// ## Preconditions
/// - Interaction data must be a chat-input command payload.
pub async fn handle_interaction(
    app: &App,
    interaction: Interaction,
    job_sender: &mpsc::Sender<Job>,
) -> anyhow::Result<()> {
    let interaction_id = interaction.id;
    let token = interaction.token.clone();
    let channel_id = interaction.channel.as_ref().map(|channel| channel.id);
    let author_id = interaction.author_id();

    let Some(data) = interaction.data else {
        return Ok(());
    };

    let InteractionData::ApplicationCommand(command_data) = data else {
        return Ok(());
    };

    let content = match command_data.name.as_str() {
        "media" => {
            handle_media_command(
                app,
                channel_id,
                author_id,
                interaction_id,
                command_data.options.as_slice(),
                job_sender,
            )
            .await
        }
        "stats" => format_stats(app),
        "config" => format_config(app),
        "status" => format_status(app).await,
        _ => return Ok(()),
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
        interaction_id,
        &token,
        response,
    )
    .await?;

    Ok(())
}

fn build_commands() -> Vec<twilight_model::application::command::Command> {
    vec![
        CommandBuilder::new("media", "Process a tweet URL", CommandType::ChatInput)
            .option(StringBuilder::new("url", "Tweet URL to process").required(true))
            .build(),
        CommandBuilder::new("status", "Show bot status", CommandType::ChatInput).build(),
        CommandBuilder::new("stats", "Show pipeline statistics", CommandType::ChatInput).build(),
        CommandBuilder::new(
            "config",
            "Show configuration summary",
            CommandType::ChatInput,
        )
        .build(),
    ]
}

async fn handle_media_command(
    app: &App,
    channel_id: Option<Id<twilight_model::id::marker::ChannelMarker>>,
    author_id: Option<Id<twilight_model::id::marker::UserMarker>>,
    interaction_id: Id<InteractionMarker>,
    options: &[CommandDataOption],
    job_sender: &mpsc::Sender<Job>,
) -> String {
    let Some(channel_id) = channel_id else {
        return "This command can only be used in a channel.".to_string();
    };
    let Some(author_id) = author_id else {
        return "Could not determine the command author.".to_string();
    };

    if !app.user_rate_limiter.check(author_id).await {
        return "⏳ You're sending requests too quickly. Please slow down.".to_string();
    }
    if !app.channel_rate_limiter.check(channel_id).await {
        return "⏳ This channel is receiving too many requests. Please try again later."
            .to_string();
    }

    let url = match options
        .iter()
        .find(|option| option.name == "url")
        .and_then(|option| match &option.value {
            CommandOptionValue::String(value) => Some(value.as_str()),
            _ => None,
        }) {
        Some(url) => url,
        None => return "Missing required URL.".to_string(),
    };

    let mut tweet_urls = media::parse_tweet_urls(url).into_iter();
    let Some(tweet_url) = tweet_urls.next() else {
        return "Please provide a valid tweet URL.".to_string();
    };
    if tweet_urls.next().is_some() {
        return "Please provide exactly one tweet URL per command.".to_string();
    }

    if app
        .engine
        .dedup
        .is_duplicate(channel_id.get(), tweet_url.tweet_id)
        .await
    {
        return "That tweet was already processed recently in this channel.".to_string();
    }

    let job = Job::new(
        RequestId(app.next_request_id()),
        channel_id,
        interaction_id.get(),
        None,
        author_id,
        tweet_url,
    );

    let enqueue = tokio::time::timeout(
        Duration::from_millis(app.engine.config.pipeline.queue_backpressure_timeout_ms),
        job_sender.send(job),
    )
    .await;

    match enqueue {
        Ok(Ok(())) => {
            app.queue_depth.fetch_add(1, Ordering::Relaxed);
            "Queued for processing.".to_string()
        }
        Ok(Err(_)) => "Job queue is unavailable right now. Please try again later.".to_string(),
        Err(_) => "Queue is busy right now. Please try again in a moment.".to_string(),
    }
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
