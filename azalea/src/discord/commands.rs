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
        return "this command can only be used in a channel.".to_string();
    };
    let Some(author_id) = author_id else {
        return "could not determine the command author.".to_string();
    };

    if !app.user_rate_limiter.check(author_id).await {
        return "you're sending requests too quickly. please slow down.".to_string();
    }
    if !app.channel_rate_limiter.check(channel_id).await {
        return "this channel is receiving too many requests. please try again later.".to_string();
    }

    let url = match options
        .iter()
        .find(|option| option.name == "url")
        .and_then(|option| match &option.value {
            CommandOptionValue::String(value) => Some(value.as_str()),
            _ => None,
        }) {
        Some(url) => url,
        None => return "missing required URL.".to_string(),
    };

    let mut tweet_urls = media::parse_tweet_urls(url).into_iter();
    let Some(tweet_url) = tweet_urls.next() else {
        return "please provide a valid tweet URL.".to_string();
    };
    if tweet_urls.next().is_some() {
        return "please provide exactly one tweet URL per command.".to_string();
    }

    // Admission checks are served from in-memory dedup state; persistence is
    // hydrated at startup and written via background flush.
    if app
        .engine
        .dedup
        .is_duplicate(channel_id.get(), tweet_url.tweet_id)
        .await
    {
        return "that tweet was already processed recently in this channel.".to_string();
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
        Ok(Err(_)) => "job queue is unavailable right now. please try again later.".to_string(),
        Err(_) => "queue is busy right now. please try again in a moment.".to_string(),
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
    let totals = snapshot.totals;
    let queue_depth = app.queue_depth.load(Ordering::Relaxed);
    let dedup_entries = app.engine.dedup.cache_entries();
    let pending_writes = app.engine.dedup.pending_writes_len().await;
    let resolver_stats = app.engine.resolver.cache_stats();

    format!(
        "Azalea is online.\nUptime: {}s\nQueue depth: {}\nTotal runs: {} ({} ok / {} failed)\nDedup cache: {} entries ({} pending writes)\nResolver cache: {} ok / {} negative",
        uptime.as_secs(),
        queue_depth,
        totals.total_runs,
        totals.successes,
        totals.failures,
        dedup_entries,
        pending_writes,
        resolver_stats.positive_entries,
        resolver_stats.negative_entries
    )
}

fn format_stats(app: &App) -> String {
    let snapshot = app.engine.metrics.snapshot();
    let totals = snapshot.totals;
    let stage_window = snapshot.stage_window;

    let mut lines = vec![
        "Pipeline stats:".to_string(),
        format!(
            "Lifetime totals: {} runs ({} ok / {} failed)",
            totals.total_runs, totals.successes, totals.failures
        ),
        "Stage timing window: since startup or last successful metrics flush".to_string(),
    ];

    for stage in Stage::ALL {
        let idx = stage as usize;
        let avg_ms = stage_window.avg_ms.get(idx).copied().unwrap_or(0);
        let sample_count = stage_window.sample_count.get(idx).copied().unwrap_or(0);
        let label = stage.as_str();

        if sample_count == 0 {
            lines.push(format!("{label}: no samples in current window"));
        } else {
            lines.push(format!(
                "{label}: {avg_ms} ms avg over {sample_count} samples"
            ));
        }
    }

    lines.join("\n")
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

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use super::*;
    use crate::config::{AppConfig, ApplicationId};
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};
    use twilight_model::id::marker::{ChannelMarker, UserMarker};

    fn unique_temp_path(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time should be after unix epoch")
            .as_nanos();
        std::env::temp_dir().join(format!("azalea-{name}-{nanos}"))
    }

    fn test_app() -> App {
        let mut config = AppConfig {
            application_id: ApplicationId::new(1),
            ..Default::default()
        };
        config.engine.storage.temp_dir = unique_temp_path("discord-commands-temp");
        config.engine.storage.dedup_persistent = false;
        config.engine.storage.metrics_enabled = false;
        std::fs::create_dir_all(&config.engine.storage.temp_dir).expect("create temp dir");

        let discord = Client::builder().token("test-token".to_owned()).build();
        App::new(config, discord).expect("build test app")
    }

    fn url_option(value: &str) -> CommandDataOption {
        CommandDataOption {
            name: "url".to_string(),
            value: CommandOptionValue::String(value.to_string()),
        }
    }

    #[tokio::test]
    async fn handle_media_command_rejects_missing_channel() {
        let app = test_app();
        let (job_sender, _job_receiver) = mpsc::channel(1);
        let author_id = Some(Id::<UserMarker>::new(42));

        let response = handle_media_command(
            &app,
            None,
            author_id,
            Id::new(10),
            &[url_option("https://x.com/rustlang/status/123")],
            &job_sender,
        )
        .await;

        assert_eq!(response, "this command can only be used in a channel.");
    }

    #[tokio::test]
    async fn handle_media_command_rejects_missing_author() {
        let app = test_app();
        let (job_sender, _job_receiver) = mpsc::channel(1);
        let channel_id = Some(Id::<ChannelMarker>::new(24));

        let response = handle_media_command(
            &app,
            channel_id,
            None,
            Id::new(11),
            &[url_option("https://x.com/rustlang/status/123")],
            &job_sender,
        )
        .await;

        assert_eq!(response, "could not determine the command author.");
    }

    #[tokio::test]
    async fn handle_media_command_rejects_missing_url() {
        let app = test_app();
        let (job_sender, _job_receiver) = mpsc::channel(1);
        let channel_id = Some(Id::<ChannelMarker>::new(25));
        let author_id = Some(Id::<UserMarker>::new(43));

        let response =
            handle_media_command(&app, channel_id, author_id, Id::new(12), &[], &job_sender).await;

        assert_eq!(response, "missing required URL.");
    }

    #[tokio::test]
    async fn handle_media_command_rejects_invalid_url() {
        let app = test_app();
        let (job_sender, _job_receiver) = mpsc::channel(1);
        let channel_id = Some(Id::<ChannelMarker>::new(26));
        let author_id = Some(Id::<UserMarker>::new(44));

        let response = handle_media_command(
            &app,
            channel_id,
            author_id,
            Id::new(13),
            &[url_option("not-a-url")],
            &job_sender,
        )
        .await;

        assert_eq!(response, "please provide a valid tweet URL.");
    }

    #[tokio::test]
    async fn handle_media_command_rejects_multiple_urls() {
        let app = test_app();
        let (job_sender, _job_receiver) = mpsc::channel(1);
        let channel_id = Some(Id::<ChannelMarker>::new(27));
        let author_id = Some(Id::<UserMarker>::new(45));

        let response = handle_media_command(
            &app,
            channel_id,
            author_id,
            Id::new(14),
            &[url_option(
                "https://x.com/rustlang/status/123 https://x.com/rustlang/status/456",
            )],
            &job_sender,
        )
        .await;

        assert_eq!(
            response,
            "please provide exactly one tweet URL per command."
        );
    }
}
