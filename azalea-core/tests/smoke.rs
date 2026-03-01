#![allow(clippy::expect_used, unused_crate_dependencies)]

use azalea_core::concurrency::Permits;
use azalea_core::config::{EngineSettings, StorageSettings, TranscodeSettings};
use azalea_core::media::{TempFileCleanup, TweetId, parse_tweet_urls};
use azalea_core::pipeline::optimize;
use azalea_core::pipeline::types::{DownloadedFile, MediaType, PreparedUpload, ResolvedMedia};
use azalea_core::storage::DedupCache;
use std::time::{SystemTime, UNIX_EPOCH};

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

#[tokio::test]
async fn pass_through_local_fixture_smoke_flow() {
    let fixture_root = std::env::temp_dir().join(format!(
        "azalea-pass-through-smoke-{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time must be after unix epoch")
            .as_nanos()
    ));
    let fixture_dir = fixture_root.join("fixture");
    tokio::fs::create_dir_all(&fixture_dir)
        .await
        .expect("fixture directory should be created");

    let fixture_path = fixture_dir.join("local-pass-through.mp4");
    let fixture_bytes = b"local fixture for pass-through smoke";
    tokio::fs::write(&fixture_path, fixture_bytes)
        .await
        .expect("fixture file should be written");

    let mut settings = EngineSettings::default();
    settings.storage.temp_dir = fixture_root;
    settings.pipeline.min_disk_space_bytes = 1;
    settings.transcode.max_upload_bytes = fixture_bytes.len() as u64 + 1024;
    settings
        .validate()
        .expect("test settings must remain valid");

    let temp_files = TempFileCleanup::new();
    let downloaded = DownloadedFile {
        path: fixture_path.clone(),
        size: fixture_bytes.len() as u64,
        duration: Some(1.0),
        resolution: Some((640, 360)),
        _guard: temp_files.guard(fixture_path.clone()),
        _dir_guard: Some(temp_files.guard(fixture_dir)),
    };
    let resolved = ResolvedMedia {
        url: "https://pbs.twimg.com/media/local-pass-through.mp4".into(),
        media_type: MediaType::Video,
        duration: Some(1.0),
        resolution: Some((640, 360)),
        extension: "mp4".into(),
    };
    let permits = Permits::new(&settings.concurrency);

    let prepared = optimize::optimize(
        downloaded,
        &resolved,
        &permits,
        &temp_files,
        &settings,
        None,
    )
    .await
    .expect("pass-through optimize should succeed");

    assert!(matches!(&prepared, PreparedUpload::Single { .. }));
    if let PreparedUpload::Single { path, .. } = &prepared {
        assert_eq!(path, &fixture_path);
        assert!(
            tokio::fs::try_exists(path)
                .await
                .expect("output file existence check should succeed")
        );
    }

    drop(prepared);
    temp_files.shutdown().await;
}
