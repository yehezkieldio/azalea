//! # Module overview
//! Media resolver chain with caching and fallbacks.
//!
//! ## Protocol / spec mapping
//! - Primary: VxTwitter API (`api.vxtwitter.com`).
//! - Fallback: `yt-dlp` using the Twitter extractor.
//!
//! ## Algorithm overview
//! 1. Check negative cache.
//! 2. Check positive cache.
//! 3. Resolve via VxTwitter; fall back to yt-dlp on failure.
//!
//! ## Trade-off acknowledgment
//! A negative cache reduces repeated slow failures but may hide transient
//! outages; see [`should_negative_cache`] for rules.
//!
//! ## References
//! - yt-dlp: <https://github.com/yt-dlp/yt-dlp>
//! - VxTwitter: <https://github.com/dylanpdx/BetterTwitFix>

use crate::concurrency::Permits;
use crate::config::{PipelineSettings, StorageSettings, USER_AGENT};
use crate::media::{TweetId, TweetLink};
use crate::pipeline::errors::{Error, ResolveError};
use crate::pipeline::process::{JsonSubprocessError, run_json_subprocess};
use crate::pipeline::ssrf::validate_media_url;
use crate::pipeline::types::{MediaType, ResolvedMedia, sanitize_extension};
use moka::future::Cache;
use serde::Deserialize;
use std::borrow::Cow;
use std::sync::Arc;
use std::time::Duration;
use tokio::process::Command;
use tracing::Instrument as _;

/// Resolves tweet media using a primary API and a yt-dlp fallback.
///
/// ## Invariants
/// - Positive cache entries correspond to successful resolves.
/// - Negative cache entries are only stored for durable errors.
#[derive(Debug, Clone)]
pub struct ResolverChain {
    vx: VxTwitter,
    ytdlp: YtDlp,
    positive_cache: Cache<TweetId, Arc<ResolvedMedia>>,
    negative_cache: Cache<TweetId, ResolveError>,
}

/// Snapshot of resolver cache sizes for diagnostics.
#[derive(Debug, Clone, Copy)]
pub struct ResolverCacheStats {
    pub positive_entries: u64,
    pub negative_entries: u64,
}

impl ResolverChain {
    /// Build a resolver chain with positive/negative caches.
    pub fn new(
        storage: &StorageSettings,
        pipeline: &PipelineSettings,
        binaries: &crate::config::BinarySettings,
    ) -> Self {
        let positive_cache = Cache::builder()
            .max_capacity(storage.resolver_cache_size as u64)
            .time_to_live(Duration::from_secs(storage.resolver_cache_ttl_secs))
            .build();

        let negative_cache = Cache::builder()
            .max_capacity(storage.resolver_cache_size as u64)
            .time_to_live(Duration::from_secs(storage.resolver_negative_ttl_secs))
            .build();

        Self {
            vx: VxTwitter::new(pipeline.resolver_timeout_secs),
            ytdlp: YtDlp::new(binaries.ytdlp.clone(), pipeline.ytdlp_timeout_secs),
            positive_cache,
            negative_cache,
        }
    }

    /// Resolve media metadata for a tweet URL, consulting caches and fallbacks.
    ///
    /// ## Preconditions
    /// - `tweet_url` is canonicalized (see [`crate::media::TweetLink`]).
    ///
    /// ## Edge-case handling
    /// Transient errors (timeouts, rate limits) skip negative cache storage.
    pub async fn resolve(
        &self,
        tweet_url: &TweetLink,
        http: &reqwest::Client,
        permits: &Permits,
    ) -> Result<Arc<ResolvedMedia>, Error> {
        let resolve_span = tracing::info_span!(
            "resolver_chain.resolve",
            tweet_id = tweet_url.tweet_id.0,
            tweet_user = %tweet_url.user
        );

        async {
            tracing::trace!("Resolving media");
            if let Some(err) = self.negative_cache.get(&tweet_url.tweet_id).await {
                // Fail fast on known-bad URLs to avoid repeated slow lookups.
                tracing::trace!("Negative cache hit");
                return Err(Error::ResolveFailed {
                    resolver: "negative-cache",
                    source: err,
                });
            }

            if let Some(media) = self.positive_cache.get(&tweet_url.tweet_id).await {
                // Positive cache is authoritative for successful resolves.
                tracing::trace!("Positive cache hit");
                return Ok(media);
            }
            tracing::trace!("Resolver cache miss");

            let mut vx_negative: Option<ResolveError> = None;
            // Prefer the lightweight API; fall back to yt-dlp on failure.
            match self
                .vx
                .resolve(tweet_url, http)
                .instrument(tracing::info_span!("resolver.vxtwitter"))
                .await
            {
                Ok(media) => {
                    tracing::info!(
                        resolver = "vxtwitter",
                        media_type = ?media.media_type,
                        extension = %media.extension,
                        "Resolved media"
                    );
                    self.positive_cache
                        .insert(tweet_url.tweet_id, Arc::clone(&media))
                        .await;
                    return Ok(media);
                }
                Err(err) => {
                    if should_negative_cache(&err) {
                        tracing::trace!("Storing VxTwitter error candidate for negative cache");
                        vx_negative = Some(err.clone());
                    }
                    // VxTwitter is best-effort; fall back to yt-dlp for robustness.
                    tracing::warn!(error = %err, "vxtwitter failed, trying yt-dlp");
                }
            }

            tracing::trace!("Acquiring yt-dlp permit");
            // ytdlp is heavier; throttle concurrency via permits.
            let _permit = permits
                .ytdlp
                .acquire()
                .await
                .map_err(|_| Error::ResolveFailed {
                    resolver: "yt-dlp",
                    source: ResolveError::ProcessFailed {
                        exit_code: None,
                        stderr: "ytdlp semaphore closed".to_string(),
                    },
                })?;

            match self
                .ytdlp
                .resolve(tweet_url)
                .instrument(tracing::info_span!("resolver.ytdlp"))
                .await
            {
                Ok(media) => {
                    tracing::info!(
                        resolver = "yt-dlp",
                        media_type = ?media.media_type,
                        extension = %media.extension,
                        "Resolved media"
                    );
                    self.positive_cache
                        .insert(tweet_url.tweet_id, Arc::clone(&media))
                        .await;
                    Ok(media)
                }
                Err(err) => {
                    tracing::trace!("yt-dlp resolve failed");
                    // Only cache durable failures to avoid poisoning on transient outages.
                    if should_negative_cache(&err) {
                        tracing::trace!("Caching durable yt-dlp resolver failure");
                        self.negative_cache
                            .insert(tweet_url.tweet_id, err.clone())
                            .await;
                    } else if let Some(vx_err) = vx_negative {
                        tracing::trace!(
                            "Caching durable fallback failure from prior VxTwitter error"
                        );
                        self.negative_cache.insert(tweet_url.tweet_id, vx_err).await;
                    } else {
                        tracing::trace!("Skipping negative cache for transient resolver error");
                    }
                    Err(Error::ResolveFailed {
                        resolver: "yt-dlp",
                        source: err,
                    })
                }
            }
        }
        .instrument(resolve_span)
        .await
    }

    /// Current cache sizes used for stats reporting.
    ///
    /// ## Rationale
    /// Exposed for `/stats` reporting in the `azalea` crate.
    pub fn cache_stats(&self) -> ResolverCacheStats {
        ResolverCacheStats {
            positive_entries: self.positive_cache.entry_count(),
            negative_entries: self.negative_cache.entry_count(),
        }
    }
}

#[derive(Debug, Clone)]
struct VxTwitter {
    timeout: Duration,
}

#[derive(Debug, Deserialize)]
struct VxResponse {
    #[serde(default)]
    media_extended: Vec<VxMedia>,
}

#[derive(Debug, Deserialize)]
struct VxMedia {
    url: Box<str>,
    #[serde(rename = "type")]
    media_type: Box<str>,
    #[serde(default)]
    duration_millis: Option<u64>,
    #[serde(default)]
    width: Option<u32>,
    #[serde(default)]
    height: Option<u32>,
}

impl VxTwitter {
    fn new(timeout_secs: u64) -> Self {
        Self {
            timeout: Duration::from_secs(timeout_secs),
        }
    }

    async fn resolve(
        &self,
        tweet_url: &TweetLink,
        http: &reqwest::Client,
    ) -> Result<Arc<ResolvedMedia>, ResolveError> {
        let api_url = format!(
            "https://api.vxtwitter.com/{}/status/{}",
            tweet_url.user, tweet_url.tweet_id.0
        );
        let request_span = tracing::info_span!(
            "resolver.vxtwitter.request",
            tweet_id = tweet_url.tweet_id.0,
            api_url = %api_url
        );

        async {
            tracing::trace!("Requesting VxTwitter media metadata");
            let result = tokio::time::timeout(self.timeout, async {
                let response = http
                    .get(&api_url)
                    .send()
                    .await
                    .map_err(|e| ResolveError::ParseFailed(e.to_string()))?;

                let status = response.status();
                if !status.is_success() {
                    return Err(ResolveError::HttpStatus(status.as_u16()));
                }

                let vx_response: VxResponse = response
                    .json()
                    .await
                    .map_err(|e| ResolveError::ParseFailed(e.to_string()))?;

                let media = self.select_best_media(&vx_response.media_extended)?;
                let extension = if let Some(ext) =
                    extension_from_vxtwitter_metadata(&media.media_type, &media.url)
                {
                    sanitize_extension(&ext)
                } else {
                    sanitize_extension(
                        &infer_extension(&media.media_type, &media.url, Some(http)).await,
                    )
                };
                let is_image = matches!(media.media_type.as_ref(), "image" | "photo");

                Ok(Arc::new(ResolvedMedia {
                    url: Cow::Owned(media.url.clone().into()),
                    media_type: if is_image {
                        MediaType::Image
                    } else {
                        MediaType::Video
                    },
                    duration: media.duration_millis.map(|d| d as f64 / 1000.0),
                    resolution: match (media.width, media.height) {
                        (Some(w), Some(h)) => Some((w, h)),
                        _ => None,
                    },
                    extension: extension.into_boxed_str(),
                }))
            })
            .await;

            match result {
                Ok(resolved) => resolved,
                Err(_) => {
                    tracing::warn!("VxTwitter resolver timed out");
                    Err(ResolveError::ProcessFailed {
                        exit_code: None,
                        stderr: "vxtwitter timed out".to_string(),
                    })
                }
            }
        }
        .instrument(request_span)
        .await
    }

    fn select_best_media<'a>(&self, media: &'a [VxMedia]) -> Result<&'a VxMedia, ResolveError> {
        if media.is_empty() {
            return Err(ResolveError::ParseFailed("no media".to_string()));
        }

        // Prefer video over GIF over stills for richer embeds.
        let priority = |m: &VxMedia| match m.media_type.as_ref() {
            "video" => 0,
            "gif" => 1,
            "image" | "photo" => 2,
            _ => 3,
        };

        media
            .iter()
            .min_by_key(|m| priority(m))
            .ok_or_else(|| ResolveError::ParseFailed("no media".to_string()))
    }
}

#[derive(Debug, Clone)]
struct YtDlp {
    path: std::path::PathBuf,
    timeout: Duration,
}

#[derive(Debug, Deserialize)]
struct YtDlpOutput {
    url: Option<Box<str>>,
    #[serde(default)]
    formats: Vec<YtDlpFormat>,
    #[serde(default)]
    duration: Option<f64>,
    #[serde(default)]
    width: Option<u32>,
    #[serde(default)]
    height: Option<u32>,
    #[serde(default)]
    ext: Option<Box<str>>,
    #[serde(default)]
    thumbnail: Option<Box<str>>,
}

#[derive(Debug, Deserialize)]
struct YtDlpFormat {
    url: Option<Box<str>>,
    ext: Option<Box<str>>,
    #[serde(default)]
    vcodec: Option<Box<str>>,
    #[serde(default)]
    acodec: Option<Box<str>>,
    #[serde(default)]
    tbr: Option<f64>,
}

impl YtDlp {
    fn new(path: std::path::PathBuf, timeout_secs: u64) -> Self {
        Self {
            path,
            timeout: Duration::from_secs(timeout_secs),
        }
    }

    async fn resolve(&self, tweet_url: &TweetLink) -> Result<Arc<ResolvedMedia>, ResolveError> {
        const YTDLP_STDOUT_LIMIT: usize = 4 * 1024 * 1024;
        const YTDLP_STDERR_LINES: usize = 10;

        let resolve_span = tracing::info_span!(
            "resolver.ytdlp.process",
            tweet_id = tweet_url.tweet_id.0,
            binary = %self.path.display()
        );

        async {
            let mut command = Command::new(&self.path);
            command.args([
                "--dump-json",
                "--no-warnings",
                "--no-playlist",
                "--no-check-certificate",
                "--user-agent",
                USER_AGENT,
                "--extractor-args",
                "twitter:api=syndication",
                tweet_url.canonical_url(),
            ]);
            let output = run_json_subprocess(
                &mut command,
                self.timeout,
                YTDLP_STDOUT_LIMIT,
                YTDLP_STDERR_LINES,
            )
            .await
            .map_err(|error| match error {
                JsonSubprocessError::Io(error) => ResolveError::ProcessFailed {
                    exit_code: None,
                    stderr: error.to_string(),
                },
                JsonSubprocessError::Timeout => ResolveError::ProcessFailed {
                    exit_code: None,
                    stderr: "yt-dlp timed out".to_string(),
                },
                JsonSubprocessError::OutputLimit { stderr_tail } => {
                    tracing::warn!(
                        limit_bytes = YTDLP_STDOUT_LIMIT,
                        "yt-dlp output exceeded bounded read limit"
                    );
                    ResolveError::ProcessFailed {
                        exit_code: None,
                        stderr: format!("yt-dlp output exceeded limit: {stderr_tail}"),
                    }
                }
            })?;

            if !output.status.success() {
                let stderr_tail = output.stderr_tail.as_ref();
                tracing::warn!(
                    exit_code = output.status.code(),
                    "yt-dlp process returned non-zero exit status"
                );
                return Err(ResolveError::ProcessFailed {
                    exit_code: output.status.code(),
                    stderr: stderr_tail
                        .lines()
                        .next()
                        .unwrap_or("unknown error")
                        .to_string(),
                });
            }

            let ytdlp_output: YtDlpOutput = serde_json::from_slice(output.stdout.as_slice())
                .map_err(|e| ResolveError::ParseFailed(e.to_string()))?;

            let (url, extension, is_image) = select_best_format(&ytdlp_output)?;
            let extension = sanitize_extension(&extension);

            Ok(Arc::new(ResolvedMedia {
                url: Cow::Owned(url.into()),
                media_type: if is_image {
                    MediaType::Image
                } else {
                    MediaType::Video
                },
                duration: ytdlp_output.duration,
                resolution: match (ytdlp_output.width, ytdlp_output.height) {
                    (Some(w), Some(h)) => Some((w, h)),
                    _ => None,
                },
                extension: extension.into_boxed_str(),
            }))
        }
        .instrument(resolve_span)
        .await
    }
}

fn select_best_format(output: &YtDlpOutput) -> Result<(Box<str>, Box<str>, bool), ResolveError> {
    if let Some(url) = &output.url {
        // Some yt-dlp outputs provide a direct URL; prefer it when present.
        let ext = output.ext.clone().unwrap_or_else(|| "mp4".into());
        let is_image = is_image_extension(ext.as_ref());
        return Ok((url.clone(), ext, is_image));
    }

    let mut ranked_formats = output
        .formats
        .iter()
        .enumerate()
        .filter_map(|(index, format)| {
            score_format(format).map(|score| RankedFormat {
                format,
                score,
                index,
            })
        })
        .collect::<Vec<_>>();
    ranked_formats.sort_by(rank_formats);

    if let Some(best) = ranked_formats.first() {
        let url = best
            .format
            .url
            .clone()
            .ok_or_else(|| ResolveError::ParseFailed("format missing url".to_string()))?;
        let ext = best.format.ext.clone().unwrap_or_else(|| "mp4".into());
        return Ok((url, ext, false));
    }

    // Fall back to thumbnail for image-only tweets.
    if let Some(thumbnail_url) = &output.thumbnail {
        let ext = extension_from_url(thumbnail_url).unwrap_or_else(|| "jpg".into());
        return Ok((thumbnail_url.clone(), ext, true));
    }

    Err(ResolveError::ParseFailed("no formats".to_string()))
}

fn is_upload_compatible(format: &YtDlpFormat) -> bool {
    let vcodec = format.vcodec.as_deref().unwrap_or("");
    let acodec = format.acodec.as_deref().unwrap_or("");
    let is_h264 = vcodec.starts_with("avc") || vcodec.contains("h264");
    let is_aac = acodec.starts_with("mp4a") || acodec.contains("aac");
    is_h264 && is_aac
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct FormatScore {
    upload_compatible: bool,
    tbr: f64,
}

#[derive(Clone, Copy, Debug)]
struct RankedFormat<'a> {
    format: &'a YtDlpFormat,
    score: FormatScore,
    index: usize,
}

fn score_format(format: &YtDlpFormat) -> Option<FormatScore> {
    if format.url.is_none() || format.vcodec.as_deref() == Some("none") {
        return None;
    }

    Some(FormatScore {
        upload_compatible: is_upload_compatible(format),
        tbr: format.tbr.unwrap_or(0.0),
    })
}

fn rank_formats(left: &RankedFormat<'_>, right: &RankedFormat<'_>) -> std::cmp::Ordering {
    right
        .score
        .upload_compatible
        .cmp(&left.score.upload_compatible)
        .then_with(|| right.score.tbr.total_cmp(&left.score.tbr))
        .then_with(|| right.index.cmp(&left.index))
}

fn is_image_extension(ext: &str) -> bool {
    matches!(ext, "jpg" | "jpeg" | "png" | "webp" | "gif")
}

fn extension_from_url(url: &str) -> Option<Box<str>> {
    let path = url.split('?').next()?;
    let filename = path.rsplit('/').next()?;
    let ext = filename.rsplit('.').next()?;
    if ext == filename || ext.is_empty() {
        return None;
    }
    Some(ext.to_lowercase().into_boxed_str())
}

fn extension_from_query_format(url: &str) -> Option<Box<str>> {
    let (_, query) = url.split_once('?')?;
    for pair in query.split('&') {
        if let Some((key, value)) = pair.split_once('=')
            && key.eq_ignore_ascii_case("format")
            && !value.is_empty()
        {
            return Some(value.to_ascii_lowercase().into_boxed_str());
        }
    }
    None
}

fn extension_from_vxtwitter_metadata(media_type: &str, url: &str) -> Option<Box<str>> {
    if let Some(ext) = extension_from_url(url) {
        return Some(ext);
    }

    if let Some(ext) = extension_from_query_format(url) {
        return Some(ext);
    }

    match media_type {
        "gif" => Some("gif".into()),
        _ => None,
    }
}

async fn infer_extension(
    media_type: &str,
    url: &str,
    client: Option<&reqwest::Client>,
) -> Box<str> {
    if let Some(ext) = url.split('.').next_back() {
        let ext = ext.split('?').next().unwrap_or(ext);
        match ext {
            "mp4" | "webm" | "gif" | "jpg" | "jpeg" | "png" | "webp" => return ext.into(),
            _ => {}
        }
    }

    // Best-effort HEAD request when the URL doesn't include an extension.
    if let Some(client) = client
        && let Ok(validated_url) = validate_media_url(url).await
        && let Ok(response) =
            tokio::time::timeout(Duration::from_secs(2), client.head(validated_url).send()).await
        && let Ok(resp) = response
        && let Some(content_type) = resp.headers().get("content-type")
        && let Ok(ct_str) = content_type.to_str()
    {
        return extension_from_mime_type(ct_str);
    }

    match media_type {
        "video" => "mp4".into(),
        "gif" => "gif".into(),
        "image" | "photo" => "jpg".into(),
        _ => "mp4".into(),
    }
}

fn extension_from_mime_type(mime_type: &str) -> Box<str> {
    let mime_base = mime_type.split(';').next().unwrap_or(mime_type).trim();
    match mime_base {
        "video/mp4" => "mp4",
        "video/webm" => "webm",
        "video/quicktime" => "mov",
        "image/jpeg" => "jpg",
        "image/png" => "png",
        "image/webp" => "webp",
        "image/gif" => "gif",
        _ => mime_base.split('/').nth(1).unwrap_or("mp4"),
    }
    .into()
}

fn should_negative_cache(error: &ResolveError) -> bool {
    // Cache durable errors but skip transient failures (timeouts, rate limits).
    match error {
        ResolveError::HttpStatus(code) => *code == 404,
        ResolveError::ParseFailed(message) => {
            let lower = message.to_lowercase();
            !(lower.contains("eof")
                || lower.contains("unexpected end")
                || lower.contains("unterminated")
                || lower.contains("truncated")
                || lower.contains("partial"))
        }
        ResolveError::ProcessFailed { stderr, .. } => {
            let lower = stderr.to_lowercase();
            if lower.contains("timed out")
                || lower.contains("timeout")
                || lower.contains("rate limit")
                || lower.contains("429")
                || lower.contains("network")
                || lower.contains("connection")
                || lower.contains("temporar")
                || lower.contains("server")
            {
                return false;
            }

            lower.contains("private")
                || lower.contains("protected")
                || lower.contains("not found")
                || lower.contains("no media")
                || lower.contains("unavailable")
                || lower.contains("does not exist")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        YtDlpFormat, YtDlpOutput, extension_from_vxtwitter_metadata, select_best_format,
        should_negative_cache,
    };
    use crate::pipeline::errors::ResolveError;

    fn format(
        url: &str,
        ext: &str,
        vcodec: Option<&str>,
        acodec: Option<&str>,
        tbr: Option<f64>,
    ) -> YtDlpFormat {
        YtDlpFormat {
            url: Some(url.into()),
            ext: Some(ext.into()),
            vcodec: vcodec.map(Into::into),
            acodec: acodec.map(Into::into),
            tbr,
        }
    }

    #[test]
    fn select_best_format_prefers_h264_aac_over_vp9() -> Result<(), ResolveError> {
        let output = YtDlpOutput {
            url: None,
            formats: vec![
                format(
                    "https://example.invalid/vp9.webm",
                    "webm",
                    Some("vp9"),
                    Some("opus"),
                    Some(9.0),
                ),
                format(
                    "https://example.invalid/h264.mp4",
                    "mp4",
                    Some("avc1.640028"),
                    Some("mp4a.40.2"),
                    Some(2.0),
                ),
            ],
            duration: None,
            width: None,
            height: None,
            ext: None,
            thumbnail: Some("https://example.invalid/thumb.jpg".into()),
        };

        let (url, ext, is_image) = select_best_format(&output)?;

        assert_eq!(url.as_ref(), "https://example.invalid/h264.mp4");
        assert_eq!(ext.as_ref(), "mp4");
        assert!(!is_image);
        Ok(())
    }

    #[test]
    fn select_best_format_prefers_vp9_over_thumbnail_when_no_h264_aac_exists()
    -> Result<(), ResolveError> {
        let output = YtDlpOutput {
            url: None,
            formats: vec![format(
                "https://example.invalid/vp9.webm",
                "webm",
                Some("vp9"),
                Some("opus"),
                Some(9.0),
            )],
            duration: None,
            width: None,
            height: None,
            ext: None,
            thumbnail: Some("https://example.invalid/thumb.jpg".into()),
        };

        let (url, ext, is_image) = select_best_format(&output)?;

        assert_eq!(url.as_ref(), "https://example.invalid/vp9.webm");
        assert_eq!(ext.as_ref(), "webm");
        assert!(!is_image);
        Ok(())
    }

    #[test]
    fn select_best_format_uses_latest_format_on_equal_score() -> Result<(), ResolveError> {
        let output = YtDlpOutput {
            url: None,
            formats: vec![
                format(
                    "https://example.invalid/first.webm",
                    "webm",
                    Some("vp9"),
                    Some("opus"),
                    Some(9.0),
                ),
                format(
                    "https://example.invalid/second.webm",
                    "webm",
                    Some("vp9"),
                    Some("opus"),
                    Some(9.0),
                ),
            ],
            duration: None,
            width: None,
            height: None,
            ext: None,
            thumbnail: Some("https://example.invalid/thumb.jpg".into()),
        };

        let (url, ext, is_image) = select_best_format(&output)?;

        assert_eq!(url.as_ref(), "https://example.invalid/second.webm");
        assert_eq!(ext.as_ref(), "webm");
        assert!(!is_image);
        Ok(())
    }

    #[test]
    fn select_best_format_uses_thumbnail_when_no_downloadable_video_exists()
    -> Result<(), ResolveError> {
        let output = YtDlpOutput {
            url: None,
            formats: vec![],
            duration: None,
            width: None,
            height: None,
            ext: None,
            thumbnail: Some("https://example.invalid/thumb.jpg".into()),
        };

        let (url, ext, is_image) = select_best_format(&output)?;

        assert_eq!(url.as_ref(), "https://example.invalid/thumb.jpg");
        assert_eq!(ext.as_ref(), "jpg");
        assert!(is_image);
        Ok(())
    }

    #[test]
    fn select_best_format_direct_image_url_is_preserved() -> Result<(), ResolveError> {
        let output = YtDlpOutput {
            url: Some("https://example.invalid/image.jpg".into()),
            formats: vec![],
            duration: None,
            width: None,
            height: None,
            ext: Some("jpg".into()),
            thumbnail: None,
        };

        let (url, ext, is_image) = select_best_format(&output)?;

        assert_eq!(url.as_ref(), "https://example.invalid/image.jpg");
        assert_eq!(ext.as_ref(), "jpg");
        assert!(is_image);
        Ok(())
    }

    #[test]
    fn vxtwitter_metadata_extension_uses_query_format() {
        let ext = extension_from_vxtwitter_metadata(
            "image",
            "https://pbs.twimg.com/media/abcd1234?format=png&name=small",
        );

        assert_eq!(ext.as_deref(), Some("png"));
    }

    #[test]
    fn vxtwitter_metadata_extension_uses_gif_type_without_probe() {
        let ext = extension_from_vxtwitter_metadata("gif", "https://video.twimg.com/tweet");

        assert_eq!(ext.as_deref(), Some("gif"));
    }

    #[test]
    fn negative_cache_keeps_protected_tweet_failures() {
        let error = ResolveError::ProcessFailed {
            exit_code: Some(1),
            stderr: "tweet is protected".to_string(),
        };

        assert!(should_negative_cache(&error));
    }

    #[test]
    fn negative_cache_keeps_404_http_status() {
        let error = ResolveError::HttpStatus(404);

        assert!(should_negative_cache(&error));
    }

    #[test]
    fn negative_cache_keeps_durable_parse_failures() {
        let error =
            ResolveError::ParseFailed("invalid type: null, expected media array".to_string());

        assert!(should_negative_cache(&error));
    }

    #[test]
    fn negative_cache_skips_network_failures() {
        let error = ResolveError::ProcessFailed {
            exit_code: Some(1),
            stderr: "connection reset by peer while fetching media".to_string(),
        };

        assert!(!should_negative_cache(&error));
    }

    #[test]
    fn negative_cache_skips_timeout_failures() {
        let error = ResolveError::ProcessFailed {
            exit_code: Some(1),
            stderr: "yt-dlp timed out after 30s".to_string(),
        };

        assert!(!should_negative_cache(&error));
    }

    #[test]
    fn negative_cache_skips_429_rate_limit_failures() {
        let error = ResolveError::ProcessFailed {
            exit_code: Some(1),
            stderr: "HTTP 429 too many requests".to_string(),
        };

        assert!(!should_negative_cache(&error));
    }

    #[test]
    fn negative_cache_skips_partial_parse_failures() {
        let error = ResolveError::ParseFailed("EOF while parsing a value".to_string());

        assert!(!should_negative_cache(&error));
    }
}
