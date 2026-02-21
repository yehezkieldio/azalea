//! # Module overview
//! Application pipeline types and Discord upload stage.
//!
//! ## Data flow
//! Gateway → [`Job`] → `azalea_core::pipeline::run` → [`upload::upload`].

pub mod upload;

use azalea_core::media::TweetLink;
use azalea_core::pipeline::{Error as CoreError, Job as CoreJob};
pub use azalea_core::pipeline::{Progress, RequestId};
use std::fmt;
use twilight_model::id::{
    Id,
    marker::{ChannelMarker, MessageMarker, UserMarker},
};

/// Input to the application pipeline.
///
/// ## Invariants
/// - `tweet_url` is canonicalized (see `azalea_core::media`).
#[derive(Debug, Clone)]
pub struct Job {
    pub request_id: RequestId,
    pub channel_id: Id<ChannelMarker>,
    pub trigger_id: u64,
    pub source_message_id: Option<Id<MessageMarker>>,
    pub author_id: Id<UserMarker>,
    pub tweet_url: TweetLink,
}

impl Job {
    /// Construct a pipeline job from gateway context and parsed tweet URL.
    pub fn new(
        request_id: RequestId,
        channel_id: Id<ChannelMarker>,
        trigger_id: u64,
        source_message_id: Option<Id<MessageMarker>>,
        author_id: Id<UserMarker>,
        tweet_url: TweetLink,
    ) -> Self {
        Self {
            request_id,
            channel_id,
            trigger_id,
            source_message_id,
            author_id,
            tweet_url,
        }
    }

    /// Convert to the core engine job representation.
    ///
    /// ## Rationale
    /// Keeps the core engine interface independent of Discord-specific IDs.
    pub fn core(&self) -> CoreJob {
        CoreJob::new(
            self.request_id,
            self.channel_id.get(),
            self.trigger_id,
            self.tweet_url.clone(),
        )
    }
}

/// Output of upload stage.
///
/// ## Postconditions
/// `first_message_id` is always set when `messages_sent > 0`.
#[derive(Debug)]
pub struct UploadOutcome {
    pub first_message_id: Id<MessageMarker>,
    pub messages_sent: usize,
}

/// Pipeline errors spanning core engine and Discord upload.
///
/// ## Design rationale
/// Consolidates core and Discord errors so response handling stays centralized.
#[derive(Debug)]
pub enum Error {
    Core(CoreError),
    UploadFailed {
        part: usize,
        total: usize,
        source: Box<dyn std::error::Error + Send + Sync>,
    },
    DiscordApi {
        operation: &'static str,
        source: Box<dyn std::error::Error + Send + Sync>,
    },
}

impl Error {
    /// User-facing message suitable for Discord responses.
    pub fn user_message(&self) -> &'static str {
        match self {
            Self::Core(err) => err.user_message(),
            Self::UploadFailed { .. } => "❌ Failed to upload media to Discord.",
            Self::DiscordApi { .. } => "❌ An internal error occurred.",
        }
    }

    /// Whether this error should surface to the user.
    pub fn should_notify_user(&self) -> bool {
        match self {
            Self::Core(err) => err.should_notify_user(),
            _ => true,
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Core(err) => write!(f, "{}", err),
            Self::UploadFailed {
                part,
                total,
                source,
            } => write!(f, "upload failed ({}/{}): {}", part, total, source),
            Self::DiscordApi { operation, source } => {
                write!(f, "discord api error ({}): {}", operation, source)
            }
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Core(err) => Some(err),
            Self::UploadFailed { source, .. } => Some(source.as_ref()),
            Self::DiscordApi { source, .. } => Some(source.as_ref()),
        }
    }
}

impl From<CoreError> for Error {
    fn from(value: CoreError) -> Self {
        Self::Core(value)
    }
}
