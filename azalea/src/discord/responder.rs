//! Discord response helpers for pipeline progress and error reporting.
//!
//! ## Data flow
//! Progress updates originate from [`azalea_core::pipeline::run`] and are
//! forwarded here to update Discord messages.
//!
//! ## Trade-off acknowledgment
//! Updates are best-effort; failures are logged but do not interrupt the
//! pipeline to preserve throughput.

use crate::pipeline::{Error, Progress};
use azalea_core::storage::Metrics;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use twilight_http::Client;
use twilight_model::id::{
    Id,
    marker::{ChannelMarker, MessageMarker},
};

/// Send the initial "processing" reply; returns the created message id if any.
///
/// ## Postconditions
/// Returns `Some(id)` when the message was created successfully.
pub async fn send_processing(
    client: &Client,
    channel_id: Id<ChannelMarker>,
    reply_to: Id<MessageMarker>,
) -> Option<Id<MessageMarker>> {
    match client
        .create_message(channel_id)
        .reply(reply_to)
        .content("Processing your tweet...")
        .await
    {
        Ok(response) => response.model().await.ok().map(|m| m.id),
        Err(e) => {
            tracing::warn!(error = %e, "Failed to send processing message");
            None
        }
    }
}

/// Update the status message with the latest pipeline stage.
///
/// ## Security-sensitive paths
/// Content is derived from internal state only; no user input is interpolated.
///
/// Errors are ignored to avoid disrupting the pipeline on transient API issues.
pub async fn update_progress(
    client: &Client,
    metrics: &Metrics,
    channel_id: Id<ChannelMarker>,
    message_id: Id<MessageMarker>,
    stage: &Progress,
) {
    if let Err(e) = client
        .update_message(channel_id, message_id)
        .content(Some(&format!("Processing: {}", stage)))
        .await
    {
        // Metrics are best-effort; avoid surfacing Discord hiccups as failures.
        metrics.record_error("progress_update_errors");
        tracing::debug!(error = %e, "Failed to update progress");
    }
}

/// Spawn a progress updater task with a debounce interval.
///
/// ## Concurrency assumptions
/// The returned task owns the receiver and terminates when the channel closes.
pub fn spawn_progress_updates(
    client: Arc<Client>,
    metrics: Metrics,
    channel_id: Id<ChannelMarker>,
    message_id: Id<MessageMarker>,
    mut progress_rx: mpsc::Receiver<Progress>,
    debounce: Duration,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut last_sent_at = std::time::Instant::now() - debounce;
        let mut last_sent_stage: Option<Progress> = None;
        while let Some(stage) = progress_rx.recv().await {
            // Avoid spamming Discord with tiny incremental updates.
            let should_send = match stage {
                Progress::Transcoding(done, total) => {
                    // Only send periodic progress updates for long transcodes.
                    done == total || last_sent_at.elapsed() >= debounce
                }
                _ => true,
            };

            if should_send {
                update_progress(&client, &metrics, channel_id, message_id, &stage).await;
                last_sent_at = std::time::Instant::now();
                last_sent_stage = Some(stage.clone());
            } else if last_sent_stage.as_ref() != Some(&stage) {
                // Track latest stage so the next update is accurate.
                last_sent_stage = Some(stage);
            }
        }
    })
}

/// Notify the user of a pipeline error, respecting notification policy.
///
/// ## Usage footguns
/// Callers should pass the original reply id if they want to edit-in-place.
pub async fn send_error(
    client: &Client,
    channel_id: Id<ChannelMarker>,
    reply_to: Option<Id<MessageMarker>>,
    error: &Error,
) {
    if !error.should_notify_user() {
        // If we shouldn't notify, remove the placeholder message when possible.
        if let Some(msg_id) = reply_to {
            let _ = client.delete_message(channel_id, msg_id).await;
        }
        return;
    }

    if let Some(msg_id) = reply_to {
        // Prefer editing the existing reply to keep the channel tidy.
        let _ = client
            .update_message(channel_id, msg_id)
            .content(Some(error.user_message()))
            .await;
        return;
    }

    let _ = client
        .create_message(channel_id)
        .content(error.user_message())
        .await;
}

/// Remove the temporary "processing" message after completion.
pub async fn cleanup_processing(
    client: &Client,
    channel_id: Id<ChannelMarker>,
    message_id: Id<MessageMarker>,
) {
    if let Err(e) = client.delete_message(channel_id, message_id).await {
        tracing::warn!(error = %e, "Failed to delete processing message");
    }
}

/// Delete the original triggering message, mapping API errors into pipeline errors.
///
/// ## Rationale
/// Keeps the channel tidy after successful processing.
pub async fn delete_original(
    client: &Client,
    channel_id: Id<ChannelMarker>,
    message_id: Id<MessageMarker>,
) -> Result<(), Error> {
    client
        .delete_message(channel_id, message_id)
        .await
        .map_err(|e| Error::DiscordApi {
            operation: "delete_message",
            source: Box::new(e),
        })?;
    Ok(())
}
