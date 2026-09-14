//! Cross-platform-safe output filenames.
//!
//! ## Non-obvious behavior
//! Windows forbids `<>:"/\|?*` and trailing dots/spaces in filenames; Linux
//! only forbids `/` and NUL. Sanitizing to the stricter Windows rule keeps a
//! single code path correct on both of the project's target machines.

use std::path::{Path, PathBuf};

const WINDOWS_ILLEGAL: &[char] = &['<', '>', ':', '"', '/', '\\', '|', '?', '*'];

fn sanitize_component(raw: &str) -> String {
    let mut cleaned: String = raw
        .chars()
        .map(|ch| {
            if WINDOWS_ILLEGAL.contains(&ch) || ch.is_control() {
                '_'
            } else {
                ch
            }
        })
        .collect();

    while cleaned.ends_with('.') || cleaned.ends_with(' ') {
        cleaned.pop();
    }

    if cleaned.is_empty() {
        cleaned.push_str("download");
    }
    cleaned
}

/// Build a sanitized `<user>_<tweet_id>.<ext>` filename.
pub fn base_name(user: &str, tweet_id: u64, ext: &str) -> String {
    format!("{}_{}.{}", sanitize_component(user), tweet_id, ext)
}

/// Resolve `dir/name` to a path that doesn't already exist, appending
/// `-1`, `-2`, ... before the extension on collision.
pub async fn unique_path(dir: &Path, name: &str) -> PathBuf {
    let candidate = dir.join(name);
    if tokio::fs::metadata(&candidate).await.is_err() {
        return candidate;
    }

    let path = Path::new(name);
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let ext = path.extension().map(|e| e.to_string_lossy().into_owned());

    for suffix in 1u32.. {
        let candidate_name = match &ext {
            Some(ext) => format!("{stem}-{suffix}.{ext}"),
            None => format!("{stem}-{suffix}"),
        };
        let candidate = dir.join(&candidate_name);
        if tokio::fs::metadata(&candidate).await.is_err() {
            return candidate;
        }
    }

    unreachable!("u32 suffix space exhausted")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_strips_windows_illegal_characters() {
        let name = base_name("user/name:evil", 42, "mp4");
        assert_eq!(name, "user_name_evil_42.mp4");
    }

    #[test]
    fn sanitize_falls_back_when_empty_after_cleanup() {
        let name = base_name("...", 42, "mp4");
        assert_eq!(name, "download_42.mp4");
    }
}
