//! # Module overview
//! Event filtering and routing into the pipeline and interaction handlers.
//!
//! ## Data flow explanation
//! Interactions are forwarded to the slash command handler, which performs
//! validation and enqueues pipeline jobs.

use crate::app::App;
use crate::discord;
use crate::pipeline::Job;
use tokio::sync::mpsc;
use twilight_gateway::{Event, EventTypeFlags, Intents};

/// Only subscribe to the event types we actively handle.
pub const EVENT_TYPES: EventTypeFlags = EventTypeFlags::INTERACTION_CREATE;

/// Minimal gateway intents required for interaction-driven commands.
pub const INTENTS: Intents = Intents::GUILDS;

/// Handle a single gateway event and enqueue work where appropriate.
///
/// ## Preconditions
/// - `app` is fully initialized and cloneable.
/// - `job_sender` is open for enqueuing pipeline work.
///
/// ## Edge-case handling
/// - Non-command interactions are ignored.
/// - Command failures are logged without terminating the shard loop.
pub async fn handle(event: Event, app: App, job_sender: mpsc::Sender<Job>) {
    if let Event::InteractionCreate(interaction) = event {
        let interaction = interaction.0;
        // Forward interactions to the Discord command handler.
        if let Err(e) = discord::handle_interaction(&app, interaction, &job_sender).await {
            tracing::warn!(error = %e, "Failed to handle interaction");
        }
    }
}
