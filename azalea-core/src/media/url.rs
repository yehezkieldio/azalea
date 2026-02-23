//! Parsing helpers for X (formerly Twitter) URLs.
//!
//! ## Algorithm overview
//! Uses a precompiled regex to extract `(user, tweet_id)` pairs from message
//! text. See [`parse_tweet_urls`] for the hot-path parser used by gateway code.
//!
//! ## Complexity
//! $O(n)$ in message length; captures are bounded by the regex engine.
//!
//! ## Non-obvious behavior
//! - The regex is compiled once and cached; parse returns empty if compilation
//!   fails.
//! - Usernames are limited to 1–15 characters to match Twitter constraints.

use regex::bytes::Regex;
use smallvec::SmallVec;
use std::sync::LazyLock;

/// Compiled regex patterns for X (formerly Twitter) URLs.
static TWITTER_URL_REGEX: LazyLock<Option<Regex>> = LazyLock::new(|| {
    match Regex::new(
        r"https?://(?:(?:www\.|mobile\.)?(?:twitter\.com|x\.com)|(?:fxtwitter\.com|vxtwitter\.com|fixupx\.com|twittpr\.com))/([A-Za-z0-9_]{1,15})/status/(\d+)",
    ) {
        Ok(regex) => Some(regex),
        Err(error) => {
            tracing::error!(%error, "Invalid Twitter/X URL regex");
            None
        }
    }
});

/// Initialize regex patterns at startup to avoid first-request latency.
///
/// ## Rationale
/// Keeps the first pipeline invocation from paying the regex compilation cost.
pub fn init() {
    let _ = &*TWITTER_URL_REGEX;
}

/// Tweet ID — always numeric.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TweetId(pub u64);

/// Parsed tweet URL with minimal allocations.
///
/// ## Invariants
/// - `tweet_id` is always numeric and extracted from a URL match.
/// - `canonical_url` is normalized to `x.com`.
#[derive(Debug, Clone)]
pub struct TweetLink {
    pub user: Box<str>,
    pub tweet_id: TweetId,
    pub original_url: Box<str>,
    canonical_url: Box<str>,
}

impl TweetLink {
    /// Construct a parsed tweet URL and its canonical X.com form.
    pub fn new(user: Box<str>, tweet_id: TweetId, original_url: Box<str>) -> Self {
        let canonical_url =
            format!("https://x.com/{}/status/{}", user, tweet_id.0).into_boxed_str();
        Self {
            user,
            tweet_id,
            original_url,
            canonical_url,
        }
    }

    /// Canonical URL normalized to x.com.
    pub fn canonical_url(&self) -> &str {
        &self.canonical_url
    }

    /// Original URL as seen in the incoming message.
    pub fn original_url(&self) -> &str {
        &self.original_url
    }
}

/// Parse X or Twitter URLs from message content.
///
/// ## Preconditions
/// - Input is UTF-8 text (callers typically pass Discord message content).
///
/// ## Postconditions
/// - Returned links are canonicalized to `x.com`.
/// - Invalid handles or IDs are skipped rather than erroring.
///
/// This function runs per-message; avoid allocations beyond captured URLs.
///
/// Returns a small vector to avoid heap allocation for the common case.
pub fn parse_tweet_urls(content: &str) -> SmallVec<[TweetLink; 4]> {
    let mut urls = SmallVec::new();
    let bytes = content.as_bytes();

    let Some(regex) = TWITTER_URL_REGEX.as_ref() else {
        return urls;
    };

    for cap in regex.captures_iter(bytes) {
        let original = cap
            .get(0)
            .and_then(|m| std::str::from_utf8(m.as_bytes()).ok());
        let user = cap
            .get(1)
            .and_then(|m| std::str::from_utf8(m.as_bytes()).ok());
        let id = cap
            .get(2)
            .and_then(|m| std::str::from_utf8(m.as_bytes()).ok());

        let Some(original) = original else { continue };
        let Some(user) = user else { continue };
        let Some(id) = id else { continue };
        let Ok(tweet_id) = id.parse::<u64>() else {
            continue;
        };

        urls.push(TweetLink::new(
            user.to_string().into_boxed_str(),
            TweetId(tweet_id),
            original.to_string().into_boxed_str(),
        ));
    }

    urls
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn test_parse_bare_x_com_url() {
        let content = "Check this out https://x.com/elonmusk/status/1234567890";
        let urls = parse_tweet_urls(content);
        assert_eq!(urls.len(), 1);
        assert_eq!(urls.first().map(|url| url.user.as_ref()), Some("elonmusk"));
        assert_eq!(urls.first().map(|url| url.tweet_id.0), Some(1234567890));
    }

    #[test]
    fn test_parse_bare_twitter_com_url() {
        let content = "Old link: https://twitter.com/jack/status/9876543210";
        let urls = parse_tweet_urls(content);
        assert_eq!(urls.len(), 1);
        assert_eq!(urls.first().map(|url| url.user.as_ref()), Some("jack"));
        assert_eq!(urls.first().map(|url| url.tweet_id.0), Some(9876543210));
    }

    #[test]
    fn test_parse_www_twitter_com_url() {
        let content = "WWW link: https://www.twitter.com/jack/status/9876543210";
        let urls = parse_tweet_urls(content);
        assert_eq!(urls.len(), 1);
        assert_eq!(urls.first().map(|url| url.user.as_ref()), Some("jack"));
        assert_eq!(urls.first().map(|url| url.tweet_id.0), Some(9876543210));
    }

    #[test]
    fn test_parse_www_x_com_url() {
        let content = "WWW link: https://www.x.com/elonmusk/status/1234567890";
        let urls = parse_tweet_urls(content);
        assert_eq!(urls.len(), 1);
        assert_eq!(urls.first().map(|url| url.user.as_ref()), Some("elonmusk"));
        assert_eq!(urls.first().map(|url| url.tweet_id.0), Some(1234567890));
    }

    #[test]
    fn test_parse_vxtwitter_url() {
        let content = "Better embed: https://vxtwitter.com/user/status/1111111111";
        let urls = parse_tweet_urls(content);
        assert_eq!(urls.len(), 1);
        assert_eq!(urls.first().map(|url| url.tweet_id.0), Some(1111111111));
    }

    #[test]
    fn test_parse_fxtwitter_url() {
        let content = "FX link: https://fxtwitter.com/user/status/2222222222";
        let urls = parse_tweet_urls(content);
        assert_eq!(urls.len(), 1);
        assert_eq!(urls.first().map(|url| url.tweet_id.0), Some(2222222222));
    }

    #[test]
    fn test_parse_fixupx_url() {
        let content = "Fixup link: https://fixupx.com/user/status/3333333333";
        let urls = parse_tweet_urls(content);
        assert_eq!(urls.len(), 1);
        assert_eq!(urls.first().map(|url| url.tweet_id.0), Some(3333333333));
    }

    #[test]
    fn test_parse_twittpr_url() {
        let content = "Twittpr link: https://twittpr.com/user/status/4444444444";
        let urls = parse_tweet_urls(content);
        assert_eq!(urls.len(), 1);
        assert_eq!(urls.first().map(|url| url.tweet_id.0), Some(4444444444));
    }

    #[test]
    fn test_parse_embedded_url_in_parentheses() {
        let content = "Embedded (https://x.com/user/status/1001) in text";
        let urls = parse_tweet_urls(content);
        assert_eq!(urls.len(), 1);
        assert_eq!(urls.first().map(|url| url.tweet_id.0), Some(1001));
    }

    #[test]
    fn test_parse_embedded_url_in_angle_brackets() {
        let content = "Embedded <https://twitter.com/user/status/1002> in text";
        let urls = parse_tweet_urls(content);
        assert_eq!(urls.len(), 1);
        assert_eq!(urls.first().map(|url| url.tweet_id.0), Some(1002));
    }

    #[test]
    fn test_parse_multiple_mixed_aliases() {
        let content = "Mix https://x.com/a/status/11 and https://vxtwitter.com/b/status/22 and https://twittpr.com/c/status/33";
        let urls = parse_tweet_urls(content);
        assert_eq!(urls.len(), 3);
        assert_eq!(urls.first().map(|url| url.tweet_id.0), Some(11));
        assert_eq!(urls.get(1).map(|url| url.tweet_id.0), Some(22));
        assert_eq!(urls.get(2).map(|url| url.tweet_id.0), Some(33));
    }

    #[test]
    fn test_parse_duplicate_identical_urls_are_kept() {
        let content = "Dupes https://x.com/user/status/777 https://x.com/user/status/777";
        let urls = parse_tweet_urls(content);
        assert_eq!(urls.len(), 2);
        assert_eq!(urls.first().map(|url| url.tweet_id.0), Some(777));
        assert_eq!(urls.get(1).map(|url| url.tweet_id.0), Some(777));
    }

    #[test]
    fn test_parse_duplicate_cross_host_same_tweet_are_kept() {
        let content = "Dupes https://x.com/user/status/888 https://twitter.com/user/status/888";
        let urls = parse_tweet_urls(content);
        assert_eq!(urls.len(), 2);
        assert_eq!(
            urls.first().map(|url| url.canonical_url()),
            Some("https://x.com/user/status/888")
        );
        assert_eq!(
            urls.get(1).map(|url| url.canonical_url()),
            Some("https://x.com/user/status/888")
        );
    }

    #[test]
    fn test_parse_url_with_query_string() {
        let content = "Query https://x.com/user/status/999?s=20&t=abc";
        let urls = parse_tweet_urls(content);
        assert_eq!(urls.len(), 1);
        let url = urls.first().expect("url should parse");
        assert_eq!(url.tweet_id.0, 999);
        assert_eq!(url.original_url(), "https://x.com/user/status/999");
    }

    #[test]
    fn test_reject_uppercase_host() {
        let content = "Upper host https://X.COM/user/status/123";
        let urls = parse_tweet_urls(content);
        assert!(urls.is_empty());
    }

    #[test]
    fn test_reject_uppercase_status_segment() {
        let content = "Upper status https://x.com/user/STATUS/123";
        let urls = parse_tweet_urls(content);
        assert!(urls.is_empty());
    }

    #[test]
    fn test_parse_multiple_urls() {
        let content = "Two tweets: https://x.com/a/status/111 and https://twitter.com/b/status/222";
        let urls = parse_tweet_urls(content);
        assert_eq!(urls.len(), 2);
    }

    #[test]
    fn test_reject_invalid_user() {
        let content = "Invalid handle https://x.com/user.name/status/333";
        let urls = parse_tweet_urls(content);
        assert!(urls.is_empty());
    }

    #[test]
    fn test_no_urls() {
        let content = "No Twitter links here, just text.";
        let urls = parse_tweet_urls(content);
        assert!(urls.is_empty());
    }

    #[test]
    fn test_canonical_url() {
        let url = TweetLink::new(
            "user".to_string().into_boxed_str(),
            TweetId(12345),
            "https://x.com/user/status/12345"
                .to_string()
                .into_boxed_str(),
        );
        assert_eq!(url.canonical_url(), "https://x.com/user/status/12345");
    }

    #[test]
    fn test_original_url() {
        let url = TweetLink::new(
            "user".to_string().into_boxed_str(),
            TweetId(12345),
            "https://twitter.com/user/status/12345"
                .to_string()
                .into_boxed_str(),
        );
        assert_eq!(url.original_url(), "https://twitter.com/user/status/12345");
    }

    #[test]
    fn differential_hosts_produce_same_canonical_url() {
        let x = parse_tweet_urls("https://x.com/user/status/424242");
        let twitter = parse_tweet_urls("https://twitter.com/user/status/424242");
        let vx = parse_tweet_urls("https://vxtwitter.com/user/status/424242");

        let x = x.first().expect("x url should parse");
        let twitter = twitter.first().expect("twitter url should parse");
        let vx = vx.first().expect("vxtwitter url should parse");

        assert_eq!(x.canonical_url(), twitter.canonical_url());
        assert_eq!(x.canonical_url(), vx.canonical_url());
    }

    fn valid_user() -> impl Strategy<Value = String> {
        proptest::string::string_regex("[A-Za-z0-9_]{1,15}").expect("valid regex strategy")
    }

    fn valid_host() -> impl Strategy<Value = &'static str> {
        prop_oneof![
            Just("x.com"),
            Just("twitter.com"),
            Just("www.x.com"),
            Just("www.twitter.com"),
            Just("mobile.x.com"),
            Just("mobile.twitter.com"),
            Just("fxtwitter.com"),
            Just("vxtwitter.com"),
            Just("fixupx.com"),
            Just("twittpr.com"),
        ]
    }

    proptest! {
        #[test]
        fn parse_extracts_correct_tweet_id_for_any_valid_url(
            user in valid_user(),
            host in valid_host(),
            tweet_id in any::<u64>(),
            is_https in any::<bool>(),
        ) {
            let scheme = if is_https { "https" } else { "http" };
            let content = format!("look {scheme}://{host}/{user}/status/{tweet_id}");
            let urls = parse_tweet_urls(&content);
            prop_assert_eq!(urls.len(), 1);
            let parsed = urls.first().expect("exactly one parsed url");
            prop_assert_eq!(parsed.user.as_ref(), user.as_str());
            prop_assert_eq!(parsed.tweet_id.0, tweet_id);
            prop_assert_eq!(
                parsed.canonical_url(),
                format!("https://x.com/{user}/status/{tweet_id}")
            );
        }
    }
}
