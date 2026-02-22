//! # Module overview
//! Pipeline error taxonomy shared across resolve/download/transcode stages.
//!
//! ## Design rationale
//! A single error enum keeps pipeline stages composable and avoids ad-hoc
//! stringly-typed errors.
//!
//! ## Non-obvious behavior
//! - `Duplicate` is treated as a soft error (no user notification).
//! - `Timeout` encapsulates stage-local durations to aid diagnostics.

use std::{fmt, time::Duration};

/// Canonical error type for pipeline stages.
///
/// ## Invariants
/// - `DiskSpace` values are in MiB.
/// - `Timeout` durations match stage-specific configuration.
#[derive(Debug)]
pub enum Error {
    Duplicate,
    ResolveFailed {
        resolver: &'static str,
        source: ResolveError,
    },
    DownloadFailed {
        source: DownloadError,
    },
    TranscodeFailed {
        stage: TranscodeStage,
        exit_code: Option<i32>,
        stderr_tail: String,
    },
    Timeout {
        operation: &'static str,
        duration: Duration,
    },
    DiskSpace {
        available_mb: u64,
        required_mb: u64,
    },
    Io(std::io::Error),
}

/// Errors encountered while resolving tweet media metadata.
///
/// ## Protocol/spec mapping
/// Maps HTTP status and process failures into structured pipeline errors.
#[derive(Debug, Clone)]
pub enum ResolveError {
    HttpStatus(u16),
    ParseFailed(String),
    ProcessFailed {
        exit_code: Option<i32>,
        stderr: String,
    },
}

/// Errors encountered while downloading media payloads.
#[derive(Debug)]
pub enum DownloadError {
    HttpStatus(u16),
    TooLarge { size_mb: u64, max_mb: u64 },
    WriteFailed(std::io::Error),
    EmptyResponse,
    SsrfBlocked(String),
}

/// Stage label used when reporting transcode failures.
#[derive(Debug, Clone, Copy)]
pub enum TranscodeStage {
    Remux,
    Transcode,
    Split,
    ImageCompress,
}

impl Error {
    /// User-facing message suitable for Discord responses.
    pub fn user_message(&self) -> &'static str {
        match self {
            Self::Duplicate => "This tweet was already archived recently.",
            Self::ResolveFailed { .. } => {
                "❌ Could not extract media from tweet. The tweet may be protected, deleted, or contain no media."
            }
            Self::DownloadFailed { source } => match source {
                DownloadError::SsrfBlocked(_) => {
                    "❌ The media URL was blocked for security reasons."
                }
                _ => "❌ Failed to download media. Please try again later.",
            },
            Self::TranscodeFailed { .. } => "❌ Failed to process media for upload.",
            Self::Timeout { .. } => {
                "❌ Operation timed out. The media may be too large or the service is slow."
            }
            Self::DiskSpace { .. } => "❌ Not enough disk space to process the media.",
            Self::Io(_) => "❌ An internal error occurred.",
        }
    }

    /// Whether this error should surface to the user.
    pub fn should_notify_user(&self) -> bool {
        !matches!(self, Self::Duplicate)
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Duplicate => write!(f, "duplicate tweet"),
            Self::ResolveFailed { resolver, source } => {
                write!(f, "resolve failed ({}): {}", resolver, source)
            }
            Self::DownloadFailed { source } => write!(f, "download failed: {}", source),
            Self::TranscodeFailed {
                stage, exit_code, ..
            } => {
                write!(
                    f,
                    "transcode failed ({:?}, exit={:?}): {}",
                    stage,
                    exit_code,
                    self.transcode_stderr()
                )
            }
            Self::Timeout {
                operation,
                duration,
            } => write!(f, "{} timed out after {}s", operation, duration.as_secs()),
            Self::DiskSpace {
                available_mb,
                required_mb,
            } => write!(
                f,
                "insufficient disk space (available {} MiB, required {} MiB)",
                available_mb, required_mb
            ),
            Self::Io(err) => write!(f, "io error: {}", err),
        }
    }
}

impl fmt::Display for ResolveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::HttpStatus(code) => write!(f, "http status {}", code),
            Self::ParseFailed(msg) => write!(f, "parse failed: {}", msg),
            Self::ProcessFailed { exit_code, stderr } => {
                write!(f, "process failed (exit {:?}): {}", exit_code, stderr)
            }
        }
    }
}

impl fmt::Display for DownloadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::HttpStatus(code) => write!(f, "http status {}", code),
            Self::TooLarge { size_mb, max_mb } => {
                write!(f, "file too large ({} MiB > {} MiB)", size_mb, max_mb)
            }
            Self::WriteFailed(err) => write!(f, "write failed: {}", err),
            Self::EmptyResponse => write!(f, "empty response"),
            Self::SsrfBlocked(reason) => write!(f, "ssrf blocked: {}", reason),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(err) => Some(err),
            _ => None,
        }
    }
}

impl std::error::Error for ResolveError {}
impl std::error::Error for DownloadError {}

impl From<std::io::Error> for Error {
    fn from(err: std::io::Error) -> Self {
        Self::Io(err)
    }
}

impl Error {
    fn transcode_stderr(&self) -> &str {
        match self {
            Self::TranscodeFailed { stderr_tail, .. } => stderr_tail.as_str(),
            _ => "",
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]
    use super::*;
    use std::error::Error as _;

    #[test]
    fn duplicate_is_not_user_notified() {
        let error = Error::Duplicate;
        assert!(!error.should_notify_user());
        assert_eq!(
            error.user_message(),
            "This tweet was already archived recently."
        );
    }

    #[test]
    fn ssrf_errors_have_security_user_message() {
        let error = Error::DownloadFailed {
            source: DownloadError::SsrfBlocked("blocked".to_string()),
        };

        assert!(error.should_notify_user());
        assert_eq!(
            error.user_message(),
            "❌ The media URL was blocked for security reasons."
        );
    }

    #[test]
    fn transcode_display_contains_stage_and_stderr_tail() {
        let error = Error::TranscodeFailed {
            stage: TranscodeStage::Split,
            exit_code: Some(1),
            stderr_tail: "encoder failed".to_string(),
        };

        let rendered = error.to_string();
        assert!(rendered.contains("Split"));
        assert!(rendered.contains("encoder failed"));
        assert!(rendered.contains("exit=Some(1)"));
    }

    #[test]
    fn io_error_propagates_source() {
        let inner = std::io::Error::other("boom");
        let error = Error::Io(inner);
        let source = error.source().expect("io error should expose source");
        assert_eq!(source.to_string(), "boom");
    }
}
