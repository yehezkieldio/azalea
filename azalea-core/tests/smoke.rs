#![allow(clippy::expect_used, unused_crate_dependencies)]

use azalea_core::config::{EngineSettings, StorageSettings, TranscodeSettings};
use azalea_core::media::{TweetId, parse_tweet_urls};
use azalea_core::storage::DedupCache;

#[test]
fn parse_and_validate_smoke() {
    let settings = EngineSettings::default();
    settings.validate().expect("default settings must validate");

    let urls = parse_tweet_urls("https://x.com/rustlang/status/123456");
    assert_eq!(urls.len(), 1);
    let url = urls.first().expect("single url");
    assert_eq!(url.canonical_url(), "https://x.com/rustlang/status/123456");
}

#[tokio::test]
async fn dedup_cache_smoke_flow() {
    let storage = StorageSettings {
        dedup_persistent: false,
        ..StorageSettings::default()
    };
    let cache = DedupCache::new(
        &storage,
        &azalea_core::config::PipelineSettings::default(),
        &TranscodeSettings::default(),
    )
    .expect("cache should initialize");

    let scope_id = 55;
    let tweet_id = TweetId(77);
    assert!(cache.reserve_inflight(scope_id, tweet_id).await);
    cache.mark_processed(scope_id, tweet_id).await;
    assert!(cache.is_duplicate(scope_id, tweet_id).await);
}
