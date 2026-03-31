//! Discord response helpers for pipeline progress and error reporting.
//!
//! ## Data flow
//! Progress updates originate from [`azalea_core::pipeline::run`] and are
//! forwarded here to update Discord messages.
//!
//! ## Trade-off acknowledgment
//! Updates are best-effort; failures are logged but do not interrupt the
//! pipeline to preserve throughput.

use crate::ids::{ChannelId, MessageId};
use crate::pipeline::{Error, Progress};
use azalea_core::storage::{ErrorCategory, Metrics};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use twilight_http::Client;

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum ErrorNotificationPolicy {
    SilentDelete,
    VisibleError,
}

fn error_notification_policy(error: &Error) -> ErrorNotificationPolicy {
    if error.should_notify_user() {
        ErrorNotificationPolicy::VisibleError
    } else {
        ErrorNotificationPolicy::SilentDelete
    }
}

/// Send the initial "processing" reply; returns the created message id if any.
///
/// ## Postconditions
/// Returns `Some(id)` when the message was created successfully.
pub async fn send_processing(
    client: &Client,
    channel_id: ChannelId,
    reply_to: Option<MessageId>,
) -> Option<MessageId> {
    let mut request = client.create_message(channel_id.into());
    if let Some(reply_to) = reply_to {
        request = request.reply(reply_to.into());
    }

    match request.content("Processing your tweet...").await {
        Ok(response) => response.model().await.ok().map(|m| MessageId::from(m.id)),
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
    channel_id: ChannelId,
    message_id: MessageId,
    stage: &Progress,
) {
    if let Err(e) = client
        .update_message(channel_id.into(), message_id.into())
        .content(Some(&format!("Processing: {}", stage)))
        .await
    {
        // Metrics are best-effort; avoid surfacing Discord hiccups as failures.
        metrics.record_error(ErrorCategory::ProgressUpdateFailed);
        tracing::debug!(error = %e, "Failed to update progress");
    }
}

fn should_send_progress(
    stage: &Progress,
    last_sent_at: std::time::Instant,
    debounce: Duration,
) -> bool {
    !stage.is_inflight_transcoding() || last_sent_at.elapsed() >= debounce
}

/// Spawn a progress updater task with a debounce interval.
///
/// ## Concurrency assumptions
/// The returned task owns the receiver and terminates when the channel closes.
pub fn spawn_progress_updates(
    client: Arc<Client>,
    metrics: Metrics,
    channel_id: ChannelId,
    message_id: MessageId,
    mut progress_rx: mpsc::Receiver<Progress>,
    debounce: Duration,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut last_sent_at = std::time::Instant::now() - debounce;
        let mut last_sent_stage: Option<Progress> = None;
        let mut pending_stage: Option<Progress> = None;
        while let Some(stage) = progress_rx.recv().await {
            if should_send_progress(&stage, last_sent_at, debounce) {
                update_progress(&client, &metrics, channel_id, message_id, &stage).await;
                last_sent_at = std::time::Instant::now();
                last_sent_stage = Some(stage.clone());
                pending_stage = None;
            } else if pending_stage.as_ref() != Some(&stage) {
                // Keep only the most recent deferred stage so shutdown can flush it.
                pending_stage = Some(stage);
            }
        }

        if let Some(stage) = pending_stage
            && last_sent_stage.as_ref() != Some(&stage)
        {
            update_progress(&client, &metrics, channel_id, message_id, &stage).await;
        }
    })
}

/// Notify the user of a pipeline error, respecting notification policy.
///
/// ## Usage footguns
/// Callers should pass the original reply id if they want to edit-in-place.
pub async fn send_error(
    client: &Client,
    channel_id: ChannelId,
    reply_to: Option<MessageId>,
    error: &Error,
) {
    if error_notification_policy(error) == ErrorNotificationPolicy::SilentDelete {
        if let Some(msg_id) = reply_to {
            let _ = client
                .delete_message(channel_id.into(), msg_id.into())
                .await;
        }

        return;
    }

    if let Some(msg_id) = reply_to {
        let _ = client
            .update_message(channel_id.into(), msg_id.into())
            .content(Some(error.user_message()))
            .await;
    } else {
        let _ = client
            .create_message(channel_id.into())
            .content(error.user_message())
            .await;
    }
}

/// Remove the temporary "processing" message after completion.
pub async fn cleanup_processing(client: &Client, channel_id: ChannelId, message_id: MessageId) {
    if let Err(e) = client
        .delete_message(channel_id.into(), message_id.into())
        .await
    {
        tracing::warn!(error = %e, "Failed to delete processing message");
    }
}

/// Delete the original triggering message, mapping API errors into pipeline errors.
///
/// ## Rationale
/// Keeps the channel tidy after successful processing.
pub async fn delete_original(
    client: &Client,
    channel_id: ChannelId,
    message_id: MessageId,
) -> Result<(), Error> {
    client
        .delete_message(channel_id.into(), message_id.into())
        .await
        .map_err(|e| Error::DiscordApi {
            operation: "delete_message",
            source: Box::new(e),
        })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use azalea_core::pipeline::Error as CoreError;

    #[test]
    fn duplicate_errors_use_silent_delete_policy() {
        let error = Error::Core(CoreError::Duplicate);
        assert_eq!(
            error_notification_policy(&error),
            ErrorNotificationPolicy::SilentDelete
        );
    }

    #[test]
    fn non_duplicate_errors_use_visible_error_policy() {
        let error = Error::UploadFailed {
            part: 1,
            total: 1,
            source: Box::new(std::io::Error::other("upload failed")),
        };
        assert_eq!(
            error_notification_policy(&error),
            ErrorNotificationPolicy::VisibleError
        );
    }

    #[test]
    fn inflight_transcoding_updates_respect_debounce() {
        let debounce = Duration::from_secs(5);
        let last_sent_at = std::time::Instant::now();

        assert!(!should_send_progress(
            &Progress::Transcoding(1, 3),
            last_sent_at,
            debounce
        ));
        assert!(should_send_progress(
            &Progress::Transcoding(3, 3),
            last_sent_at,
            debounce
        ));
        assert!(should_send_progress(
            &Progress::Uploading,
            last_sent_at,
            debounce
        ));
    }
}
