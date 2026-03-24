//! # Module overview
//! Core pipeline data types shared across resolver, downloader, and optimizer.
//!
//! ## Invariants
//! - Guards own temporary files until upload completes.
//! - `RequestId` is stable across all log statements for a job.

use std::{borrow::Cow, fmt, path::PathBuf};

use crate::media::{TempFileGuard, TweetLink};

/// Identifier used to correlate logs across pipeline stages.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RequestId(pub u64);

/// Input to the pipeline.
///
/// ## Preconditions
/// - `tweet_url` is canonicalized via [`crate::media::parse_tweet_urls`].
/// - `scope_id` is stable across retries to preserve dedup behavior.
#[derive(Debug, Clone)]
pub struct Job {
    pub request_id: RequestId,
    pub scope_id: u64,
    pub job_id: u64,
    pub tweet_url: TweetLink,
}

impl Job {
    /// Construct a pipeline job from gateway context and parsed tweet URL.
    pub fn new(request_id: RequestId, scope_id: u64, job_id: u64, tweet_url: TweetLink) -> Self {
        Self {
            request_id,
            scope_id,
            job_id,
            tweet_url,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaType {
    Video,
    Image,
}

/// Output of resolve stage.
///
/// ## Postconditions
/// URL is SSRF-validated before download (see `pipeline::ssrf`).
#[derive(Debug, Clone)]
pub struct ResolvedMedia {
    pub url: Cow<'static, str>,
    pub media_type: MediaType,
    pub duration: Option<f64>,
    pub resolution: Option<(u32, u32)>,
    pub extension: Box<str>,
}

/// Container identity used to preflight stream-copy strategies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MediaContainer {
    Mp4,
    Mov,
    Webm,
    Matroska,
    Avi,
    MpegTs,
    Gif,
    #[default]
    Unknown,
}

impl MediaContainer {
    pub fn from_extension(ext: &str) -> Self {
        match sanitize_extension(ext).as_str() {
            "mp4" => Self::Mp4,
            "mov" => Self::Mov,
            "webm" => Self::Webm,
            "mkv" => Self::Matroska,
            "avi" => Self::Avi,
            "ts" => Self::MpegTs,
            "gif" => Self::Gif,
            _ => Self::Unknown,
        }
    }

    pub fn from_ffprobe_name(format_name: &str) -> Self {
        for name in format_name.split(',') {
            match name.trim() {
                "mp4" | "m4a" | "3gp" | "3g2" | "mj2" => return Self::Mp4,
                "mov" => return Self::Mov,
                "webm" => return Self::Webm,
                "matroska" => return Self::Matroska,
                "avi" => return Self::Avi,
                "mpegts" => return Self::MpegTs,
                "gif" => return Self::Gif,
                _ => {}
            }
        }

        Self::Unknown
    }

    pub fn is_mp4_family(self) -> bool {
        matches!(self, Self::Mp4 | Self::Mov)
    }
}

/// Video codec identity used to gate stream-copy remux and split paths.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum VideoCodec {
    H264,
    H265,
    Vp8,
    Vp9,
    Av1,
    #[default]
    Other,
}

impl VideoCodec {
    pub fn from_ffprobe_name(codec_name: &str) -> Self {
        match codec_name {
            "h264" => Self::H264,
            "hevc" | "h265" => Self::H265,
            "vp8" => Self::Vp8,
            "vp9" => Self::Vp9,
            "av1" => Self::Av1,
            _ => Self::Other,
        }
    }
}

/// Audio codec identity used to gate stream-copy remux and split paths.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AudioCodec {
    Aac,
    Opus,
    Vorbis,
    Mp3,
    None,
    #[default]
    Other,
}

impl AudioCodec {
    pub fn from_ffprobe_name(codec_name: &str) -> Self {
        match codec_name {
            "aac" => Self::Aac,
            "opus" => Self::Opus,
            "vorbis" => Self::Vorbis,
            "mp3" => Self::Mp3,
            _ => Self::Other,
        }
    }
}

/// Probe facts reused across the optimization strategy ladder.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MediaFacts {
    pub container: MediaContainer,
    pub video_codec: VideoCodec,
    pub audio_codec: AudioCodec,
    pub bitrate_kbps: Option<u32>,
}

impl MediaFacts {
    pub fn from_extension(ext: &str) -> Self {
        Self {
            container: MediaContainer::from_extension(ext),
            ..Self::default()
        }
    }
}

/// Output of download stage.
///
/// ## Invariant-preserving notes
/// `path` is kept alive by `_guard` until subsequent stages complete.
#[derive(Debug)]
pub struct DownloadedFile {
    pub path: PathBuf,
    pub size: u64,
    pub duration: Option<f64>,
    pub resolution: Option<(u32, u32)>,
    pub facts: MediaFacts,
    /// Guard ensures the temp file lives through later pipeline stages.
    pub _guard: TempFileGuard,
    /// Optional guard for a temp directory holding additional artifacts.
    pub _dir_guard: Option<TempFileGuard>,
}

/// Output of optimize stage.
///
/// ## Postconditions
/// All paths are ready for upload; segments are ordered when `Split`.
#[derive(Debug)]
pub enum PreparedUpload {
    Single {
        path: PathBuf,
        /// Keep the single output file alive until upload completes.
        _guard: TempFileGuard,
        /// Directory guard for intermediate artifacts.
        _dir_guard: TempFileGuard,
    },
    Split {
        paths: Vec<PathBuf>,
        /// Guards for each produced segment.
        _guards: Vec<TempFileGuard>,
        /// Directory guard for intermediate artifacts.
        _dir_guard: TempFileGuard,
    },
}

/// Stages of pipeline processing for progress updates.
///
/// ## Protocol / spec mapping
/// These stages map to Discord progress messages in the `azalea` crate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Progress {
    Resolving,
    Downloading,
    Optimizing,
    Transcoding(usize, usize),
    Uploading,
    UploadingSegment(usize, usize),
}

/// Normalize and restrict extensions to a known-safe allowlist.
///
/// ## Security-sensitive paths
/// Blocks path traversal and exotic extensions before filesystem writes.
pub fn sanitize_extension(ext: &str) -> String {
    let candidate = ext
        .trim()
        .trim_start_matches('.')
        .split(['/', '\\', '?', '#'])
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();

    match candidate.as_str() {
        "mp4" | "webm" | "mov" | "mkv" | "avi" | "ts" | "gif" | "jpg" | "jpeg" | "png" | "webp" => {
            candidate
        }
        _ => {
            if !candidate.is_empty() {
                tracing::warn!(extension = %candidate, "Unknown extension; defaulting to mp4");
            }
            "mp4".to_string()
        }
    }
}

impl fmt::Display for Progress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Resolving => write!(f, "Resolving metadata..."),
            Self::Downloading => write!(f, "Downloading media..."),
            Self::Optimizing => write!(f, "Processing media..."),
            Self::Transcoding(done, total) => {
                write!(f, "Transcoding segments ({}/{})", done, total)
            }
            Self::Uploading => write!(f, "Uploading..."),
            Self::UploadingSegment(done, total) => {
                write!(f, "Uploading segment {}/{}...", done, total)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use std::path::{Component, Path};

    fn is_allowed_extension(ext: &str) -> bool {
        matches!(
            ext,
            "mp4" | "webm" | "mov" | "mkv" | "avi" | "ts" | "gif" | "jpg" | "jpeg" | "png" | "webp"
        )
    }

    #[test]
    fn sanitize_extension_normalizes_known_values() {
        assert_eq!(sanitize_extension(".MP4"), "mp4");
        assert_eq!(sanitize_extension(" jpg "), "jpg");
        assert_eq!(sanitize_extension("png?size=large"), "png");
    }

    #[test]
    fn sanitize_extension_rejects_unknown_and_path_like_values() {
        assert_eq!(sanitize_extension("../etc/passwd"), "mp4");
        assert_eq!(sanitize_extension(""), "mp4");
        assert_eq!(sanitize_extension("tar.gz"), "mp4");
    }

    #[test]
    fn media_container_parses_extensions_and_ffprobe_names() {
        assert_eq!(MediaContainer::from_extension("mov"), MediaContainer::Mov);
        assert_eq!(
            MediaContainer::from_extension("mkv"),
            MediaContainer::Matroska
        );
        assert_eq!(
            MediaContainer::from_ffprobe_name("mov,mp4,m4a,3gp,3g2,mj2"),
            MediaContainer::Mov
        );
        assert_eq!(
            MediaContainer::from_ffprobe_name("matroska,webm"),
            MediaContainer::Matroska
        );
    }

    #[test]
    fn codec_parsing_recognizes_common_stream_copy_cases() {
        assert_eq!(VideoCodec::from_ffprobe_name("h264"), VideoCodec::H264);
        assert_eq!(VideoCodec::from_ffprobe_name("vp9"), VideoCodec::Vp9);
        assert_eq!(AudioCodec::from_ffprobe_name("aac"), AudioCodec::Aac);
        assert_eq!(AudioCodec::from_ffprobe_name("opus"), AudioCodec::Opus);
    }

    #[test]
    fn progress_display_messages_are_stable() {
        assert_eq!(Progress::Resolving.to_string(), "Resolving metadata...");
        assert_eq!(Progress::Downloading.to_string(), "Downloading media...");
        assert_eq!(Progress::Optimizing.to_string(), "Processing media...");
        assert_eq!(
            Progress::Transcoding(2, 5).to_string(),
            "Transcoding segments (2/5)"
        );
        assert_eq!(Progress::Uploading.to_string(), "Uploading...");
        assert_eq!(
            Progress::UploadingSegment(1, 4).to_string(),
            "Uploading segment 1/4..."
        );
    }

    proptest! {
        #[test]
        fn sanitize_extension_always_returns_safe_suffix(ext in any::<String>()) {
            let sanitized = sanitize_extension(&ext);

            prop_assert!(is_allowed_extension(&sanitized));
            prop_assert!(sanitized.chars().all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit()));
            prop_assert!(!sanitized.contains(".."));
            prop_assert!(!sanitized.contains(['/', '\\']));
            prop_assert_ne!(sanitized.as_str(), ".");
            prop_assert_ne!(sanitized.as_str(), "..");
            prop_assert!(Path::new(&sanitized)
                .components()
                .all(|component| matches!(component, Component::Normal(_))));
        }
    }
}
