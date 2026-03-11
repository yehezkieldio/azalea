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
use crate::pipeline::process::{SubprocessGuard, kill_process_group, read_bounded};
use crate::pipeline::ssrf::validate_media_url;
use crate::pipeline::types::{MediaType, ResolvedMedia, sanitize_extension};
use moka::future::Cache;
use serde::Deserialize;
use std::collections::VecDeque;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;

/// Resolves tweet media using a primary API and a yt-dlp fallback.
///
/// ## Invariants
/// - Positive cache entries correspond to successful resolves.
/// - Negative cache entries are only stored for durable errors.
#[derive(Debug, Clone)]
pub struct ResolverChain {
    vx: VxTwitter,
    ytdlp: YtDlp,
    positive_cache: Cache<TweetId, ResolvedMedia>,
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
    ) -> Result<ResolvedMedia, Error> {
        tracing::trace!(tweet_id = tweet_url.tweet_id.0, "Resolving media");
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

        let mut vx_negative: Option<ResolveError> = None;
        // Prefer the lightweight API; fall back to yt-dlp on failure.
        match self.vx.resolve(tweet_url, http).await {
            Ok(media) => {
                tracing::info!(
                    resolver = "vxtwitter",
                    media_type = ?media.media_type,
                    extension = %media.extension,
                    "Resolved media"
                );
                self.positive_cache
                    .insert(tweet_url.tweet_id, media.clone())
                    .await;
                return Ok(media);
            }
            Err(err) => {
                if should_negative_cache(&err) {
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

        match self.ytdlp.resolve(tweet_url).await {
            Ok(media) => {
                tracing::info!(
                    resolver = "yt-dlp",
                    media_type = ?media.media_type,
                    extension = %media.extension,
                    "Resolved media"
                );
                self.positive_cache
                    .insert(tweet_url.tweet_id, media.clone())
                    .await;
                Ok(media)
            }
            Err(err) => {
                tracing::trace!("yt-dlp resolve failed");
                // Only cache durable failures to avoid poisoning on transient outages.
                if should_negative_cache(&err) {
                    self.negative_cache
                        .insert(tweet_url.tweet_id, err.clone())
                        .await;
                } else if let Some(vx_err) = vx_negative {
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
    url: String,
    #[serde(rename = "type")]
    media_type: String,
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
    ) -> Result<ResolvedMedia, ResolveError> {
        let api_url = format!(
            "https://api.vxtwitter.com/{}/status/{}",
            tweet_url.user, tweet_url.tweet_id.0
        );

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
            let extension = sanitize_extension(
                &infer_extension(&media.media_type, &media.url, Some(http)).await,
            );
            let is_image = matches!(media.media_type.as_str(), "image" | "photo");

            Ok(ResolvedMedia {
                url: media.url.clone().into_boxed_str(),
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
            })
        })
        .await;

        match result {
            Ok(resolved) => resolved,
            Err(_) => Err(ResolveError::ProcessFailed {
                exit_code: None,
                stderr: "vxtwitter timed out".to_string(),
            }),
        }
    }

    fn select_best_media<'a>(&self, media: &'a [VxMedia]) -> Result<&'a VxMedia, ResolveError> {
        if media.is_empty() {
            return Err(ResolveError::ParseFailed("no media".to_string()));
        }

        // Prefer video over GIF over stills for richer embeds.
        let priority = |m: &VxMedia| match m.media_type.as_str() {
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
    url: Option<String>,
    #[serde(default)]
    formats: Vec<YtDlpFormat>,
    #[serde(default)]
    duration: Option<f64>,
    #[serde(default)]
    width: Option<u32>,
    #[serde(default)]
    height: Option<u32>,
    #[serde(default)]
    ext: Option<String>,
    #[serde(default)]
    thumbnail: Option<String>,
}

#[derive(Debug, Deserialize)]
struct YtDlpFormat {
    url: Option<String>,
    ext: Option<String>,
    #[serde(default)]
    vcodec: Option<String>,
    #[serde(default)]
    acodec: Option<String>,
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

    async fn resolve(&self, tweet_url: &TweetLink) -> Result<ResolvedMedia, ResolveError> {
        const YTDLP_STDOUT_LIMIT: usize = 4 * 1024 * 1024;
        const YTDLP_STDERR_LINES: usize = 10;

        let mut command = Command::new(&self.path);
        command
            .args([
                "--dump-json",
                "--no-warnings",
                "--no-playlist",
                "--no-check-certificate",
                "--user-agent",
                USER_AGENT,
                "--extractor-args",
                "twitter:api=syndication",
                tweet_url.canonical_url(),
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut guard =
            SubprocessGuard::spawn(&mut command).map_err(|e| ResolveError::ProcessFailed {
                exit_code: None,
                stderr: e.to_string(),
            })?;

        let stdout =
            guard
                .child_mut()
                .stdout
                .take()
                .ok_or_else(|| ResolveError::ProcessFailed {
                    exit_code: None,
                    stderr: "yt-dlp stdout missing".to_string(),
                })?;
        let stderr =
            guard
                .child_mut()
                .stderr
                .take()
                .ok_or_else(|| ResolveError::ProcessFailed {
                    exit_code: None,
                    stderr: "yt-dlp stderr missing".to_string(),
                })?;

        let (limit_tx, mut limit_rx) = mpsc::channel::<()>(1);
        let limit_guard = limit_tx.clone();

        // Bound stdout to prevent untrusted output from growing without limit.
        let stdout_handle = tokio::spawn(read_bounded(stdout, YTDLP_STDOUT_LIMIT, Some(limit_tx)));
        let stderr_handle = tokio::spawn(read_stderr_tail(stderr, YTDLP_STDERR_LINES));

        enum WaitOutcome {
            Exit(std::io::Result<std::process::ExitStatus>),
            Timeout,
            OutputLimit,
        }

        let wait_outcome = tokio::select! {
            res = tokio::time::timeout(self.timeout, guard.wait()) => {
                match res {
                    Ok(status) => WaitOutcome::Exit(status),
                    Err(_) => WaitOutcome::Timeout,
                }
            }
            _ = limit_rx.recv() => WaitOutcome::OutputLimit,
        };
        drop(limit_guard);

        let status = match wait_outcome {
            WaitOutcome::Exit(status) => status.map_err(|e| ResolveError::ProcessFailed {
                exit_code: None,
                stderr: e.to_string(),
            })?,
            WaitOutcome::Timeout => {
                // Ensure any child processes are terminated on timeout.
                kill_process_group(guard.child_mut()).await;
                let _ = guard.wait().await;
                stdout_handle.abort();
                stderr_handle.abort();
                return Err(ResolveError::ProcessFailed {
                    exit_code: None,
                    stderr: "yt-dlp timed out".to_string(),
                });
            }
            WaitOutcome::OutputLimit => {
                // Output guard triggered; terminate to cap memory usage.
                kill_process_group(guard.child_mut()).await;
                let _ = guard.wait().await;
                let _ = stdout_handle.await;
                let stderr_tail = stderr_handle
                    .await
                    .ok()
                    .and_then(|result| result.ok())
                    .unwrap_or_else(|| "unknown error".to_string());
                return Err(ResolveError::ProcessFailed {
                    exit_code: None,
                    stderr: format!("yt-dlp output exceeded limit: {stderr_tail}"),
                });
            }
        };

        let stdout = stdout_handle
            .await
            .map_err(|e| ResolveError::ProcessFailed {
                exit_code: status.code(),
                stderr: e.to_string(),
            })?
            .map_err(|e| ResolveError::ProcessFailed {
                exit_code: status.code(),
                stderr: e.to_string(),
            })?;

        let stderr_tail = stderr_handle
            .await
            .ok()
            .and_then(|result| result.ok())
            .unwrap_or_else(|| "unknown error".to_string());

        if stdout.exceeded {
            return Err(ResolveError::ProcessFailed {
                exit_code: status.code(),
                stderr: format!("yt-dlp output exceeded limit: {stderr_tail}"),
            });
        }

        if !status.success() {
            return Err(ResolveError::ProcessFailed {
                exit_code: status.code(),
                stderr: stderr_tail
                    .lines()
                    .next()
                    .unwrap_or("unknown error")
                    .to_string(),
            });
        }

        let stdout = String::from_utf8_lossy(&stdout.data);
        let ytdlp_output: YtDlpOutput =
            serde_json::from_str(&stdout).map_err(|e| ResolveError::ParseFailed(e.to_string()))?;

        let (url, extension, is_image) = select_best_format(&ytdlp_output)?;
        let extension = sanitize_extension(&extension);

        Ok(ResolvedMedia {
            url: url.into_boxed_str(),
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
        })
    }
}

fn select_best_format(output: &YtDlpOutput) -> Result<(String, String, bool), ResolveError> {
    if let Some(url) = &output.url {
        // Some yt-dlp outputs provide a direct URL; prefer it when present.
        let ext = output.ext.clone().unwrap_or_else(|| "mp4".to_string());
        let is_image = is_image_extension(&ext);
        return Ok((url.clone(), ext, is_image));
    }

    // Filter down to video formats with a real URL for download.
    let valid_formats: Vec<&YtDlpFormat> = output
        .formats
        .iter()
        .filter(|f| f.url.is_some())
        .filter(|f| f.vcodec.as_deref() != Some("none"))
        .collect();

    if valid_formats.is_empty() {
        // Fall back to thumbnail for image-only tweets.
        if let Some(thumbnail_url) = &output.thumbnail {
            let ext = extension_from_url(thumbnail_url).unwrap_or_else(|| "jpg".to_string());
            return Ok((thumbnail_url.clone(), ext, true));
        }
        return Err(ResolveError::ParseFailed("no formats".to_string()));
    }

    // Prefer formats that are likely to upload without re-encoding.
    let compatible_formats: Vec<&YtDlpFormat> = valid_formats
        .iter()
        .filter(|f| {
            let vcodec = f.vcodec.as_deref().unwrap_or("");
            let acodec = f.acodec.as_deref().unwrap_or("");
            let is_h264 = vcodec.starts_with("avc") || vcodec.contains("h264");
            let is_aac = acodec.starts_with("mp4a") || acodec.contains("aac");
            is_h264 && is_aac
        })
        .copied()
        .collect();

    if !compatible_formats.is_empty() {
        // Pick the highest bitrate compatible format to minimize re-encode work.
        let best = compatible_formats
            .iter()
            .max_by(|a, b| {
                let a_tbr = a.tbr.unwrap_or(0.0);
                let b_tbr = b.tbr.unwrap_or(0.0);
                a_tbr
                    .partial_cmp(&b_tbr)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .ok_or_else(|| ResolveError::ParseFailed("no compatible formats".to_string()))?;

        let url = best
            .url
            .clone()
            .ok_or_else(|| ResolveError::ParseFailed("format missing url".to_string()))?;
        let ext = best.ext.clone().unwrap_or_else(|| "mp4".to_string());
        return Ok((url, ext, false));
    }

    // Otherwise, pick the highest bitrate format and plan to transcode later.
    let best = valid_formats
        .iter()
        .max_by(|a, b| {
            let a_tbr = a.tbr.unwrap_or(0.0);
            let b_tbr = b.tbr.unwrap_or(0.0);
            a_tbr
                .partial_cmp(&b_tbr)
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .ok_or_else(|| ResolveError::ParseFailed("no formats".to_string()))?;

    let url = best
        .url
        .clone()
        .ok_or_else(|| ResolveError::ParseFailed("format missing url".to_string()))?;
    let ext = best.ext.clone().unwrap_or_else(|| "mp4".to_string());
    Ok((url, ext, false))
}

fn is_image_extension(ext: &str) -> bool {
    matches!(ext, "jpg" | "jpeg" | "png" | "webp" | "gif")
}

fn extension_from_url(url: &str) -> Option<String> {
    url.split('?')
        .next()?
        .split('.')
        .next_back()
        .map(|s| s.to_lowercase())
}

async fn infer_extension(media_type: &str, url: &str, client: Option<&reqwest::Client>) -> String {
    if let Some(ext) = url.split('.').next_back() {
        let ext = ext.split('?').next().unwrap_or(ext);
        if ["mp4", "webm", "gif", "jpg", "jpeg", "png", "webp"].contains(&ext) {
            return ext.to_string();
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
        "video" => "mp4".to_string(),
        "gif" => "gif".to_string(),
        "image" | "photo" => "jpg".to_string(),
        _ => "mp4".to_string(),
    }
}

fn extension_from_mime_type(mime_type: &str) -> String {
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
    .to_string()
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

async fn read_stderr_tail(
    stderr: tokio::process::ChildStderr,
    max_lines: usize,
) -> Result<String, std::io::Error> {
    const MAX_LINE_BYTES: usize = 4096;
    let mut reader = BufReader::new(stderr);
    let mut lines = VecDeque::with_capacity(max_lines);
    let mut buffer = Vec::with_capacity(256);

    loop {
        buffer.clear();
        let read = reader.read_until(b'\n', &mut buffer).await?;
        if read == 0 {
            break;
        }

        if buffer.len() > MAX_LINE_BYTES {
            buffer.truncate(MAX_LINE_BYTES);
        }

        if buffer.last() == Some(&b'\n') {
            buffer.pop();
        }

        let line = String::from_utf8_lossy(&buffer).to_string();
        if lines.len() >= max_lines {
            lines.pop_front();
        }
        lines.push_back(line);
    }

    Ok(lines.into_iter().collect::<Vec<_>>().join("\n"))
}

#[cfg(test)]
mod tests {
    use super::should_negative_cache;
    use crate::pipeline::errors::ResolveError;

    #[test]
    fn negative_cache_keeps_protected_tweet_failures() {
        let error = ResolveError::ProcessFailed {
            exit_code: Some(1),
            stderr: "tweet is protected".to_string(),
        };

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
    fn negative_cache_skips_partial_parse_failures() {
        let error = ResolveError::ParseFailed("EOF while parsing a value".to_string());

        assert!(!should_negative_cache(&error));
    }
}
