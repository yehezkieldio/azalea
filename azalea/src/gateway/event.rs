//! # Module overview
//! Event filtering and routing into the pipeline and interaction handlers.
//!
//! ## Data flow explanation
//! Message events are parsed for tweet links and enqueued as jobs; interactions
//! are forwarded to the slash command handler.
//!
//! ## Security-sensitive paths
//! Only mentions of the bot are processed to avoid unsolicited scanning.

use crate::app::App;
use crate::discord;
use crate::pipeline::Job;
use azalea_core::media;
use azalea_core::pipeline::RequestId;
use std::collections::HashSet;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tokio::sync::mpsc;
use twilight_gateway::{Event, EventTypeFlags, Intents};

/// Only subscribe to the event types we actively handle.
pub const EVENT_TYPES: EventTypeFlags =
    EventTypeFlags::INTERACTION_CREATE.union(EventTypeFlags::MESSAGE_CREATE);

/// Minimal gateway intents required for mention-based triggers and commands.
pub const INTENTS: Intents = Intents::GUILD_MESSAGES.union(Intents::MESSAGE_CONTENT);

/// Handle a single gateway event and enqueue work where appropriate.
///
/// ## Preconditions
/// - `app` is fully initialized and cloneable.
/// - `job_sender` is open for enqueuing pipeline work.
///
/// ## Edge-case handling
/// - Duplicate tweet ids in a single message are collapsed.
/// - Backpressure triggers a friendly retry message.
pub async fn handle(event: Event, app: App, job_sender: mpsc::Sender<Job>) {
    match event {
        Event::MessageCreate(msg) => {
            if msg.author.bot {
                // Ignore bot messages to avoid feedback loops.
                return;
            }

            let application_id = app
                .config
                .application_id
                .get()
                .cast::<twilight_model::id::marker::UserMarker>();
            let bot_mentioned = msg
                .mentions
                .iter()
                .any(|mention| mention.id == application_id);
            if !bot_mentioned {
                // Only process explicit mentions to avoid unsolicited scanning.
                return;
            }

            let tweet_urls = media::parse_tweet_urls(&msg.content);
            if tweet_urls.is_empty() {
                // Nothing actionable; skip early.
                return;
            }

            if !app.user_rate_limiter.check(msg.author.id).await {
                // Rate limit exceeded: respond politely and avoid enqueue.
                let _ = app
                    .discord
                    .create_message(msg.channel_id)
                    .reply(msg.id)
                    .content("⏳ You're sending requests too quickly. Please slow down.")
                    .await;
                return;
            }

            if !app.channel_rate_limiter.check(msg.channel_id).await {
                let _ = app
                    .discord
                    .create_message(msg.channel_id)
                    .reply(msg.id)
                    .content(
                        "⏳ This channel is receiving too many requests. Please try again later.",
                    )
                    .await;
                return;
            }

            let mut seen = HashSet::new();
            for tweet_url in tweet_urls {
                if !seen.insert(tweet_url.tweet_id) {
                    // Collapse duplicates within a single message.
                    continue;
                }
                if app
                    .engine
                    .dedup
                    .is_duplicate(msg.channel_id.get(), tweet_url.tweet_id)
                    .await
                {
                    // Skip already-processed requests to avoid redundant work.
                    continue;
                }

                let job = Job::new(
                    RequestId(app.next_request_id()),
                    msg.channel_id,
                    msg.id,
                    msg.author.id,
                    tweet_url,
                );

                let enqueue = tokio::time::timeout(
                    Duration::from_millis(app.engine.config.pipeline.queue_backpressure_timeout_ms),
                    job_sender.send(job),
                )
                .await;

                match enqueue {
                    Ok(Ok(())) => {
                        // Queue depth is a lightweight metric, not a correctness signal.
                        app.queue_depth.fetch_add(1, Ordering::Relaxed);
                    }
                    Ok(Err(_)) => {
                        // Sender closed: pipeline worker has shut down.
                        tracing::warn!("Job queue closed; dropping request");
                    }
                    Err(_) => {
                        // Backpressure timed out: ask the user to retry later.
                        let _ = app
                            .discord
                            .create_message(msg.channel_id)
                            .content("Queue is busy right now. Please try again in a moment.")
                            .await;
                    }
                }
            }
        }
        Event::InteractionCreate(interaction) => {
            let interaction = interaction.0;
            // Forward slash commands and buttons to the Discord handler.
            if let Err(e) = discord::handle_interaction(&app, interaction).await {
                tracing::warn!(error = %e, "Failed to handle interaction");
            }
        }
        _ => {}
    }
}
