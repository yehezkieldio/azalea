//! Media helpers for parsing URLs and managing temporary files.
//!
//! ## Terminology
//! - **Tweet link**: A normalized representation of an X/Twitter status URL.
//! - **Temp guard**: RAII token that schedules cleanup on drop.

pub mod tempfile;
pub mod url;

pub use tempfile::{
    TempFileCleanup, TempFileGuard, cleanup_stale_temp_entries, cleanup_temp_dir_older_than,
    cleanup_temp_dir_sync,
};
pub use url::{TweetId, TweetLink, init as init_regex, parse_tweet_urls};
