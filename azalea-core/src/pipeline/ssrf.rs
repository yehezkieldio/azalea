//! SSRF guardrails for untrusted media URLs.
//!
//! ## Security-sensitive paths
//! This module defends against local network access via crafted URLs. It is
//! called before any outbound request in the download/resolve pipeline.
//!
//! ## Algorithm overview
//! 1. Parse URL and enforce HTTPS.
//! 2. Reject localhost, local suffixes, and IP literals.
//! 3. Resolve DNS and reject private/link-local ranges.
//!
//! ## References
//! - SSRF guidance: <https://owasp.org/www-community/attacks/Server_Side_Request_Forgery>

use std::net::IpAddr;

use reqwest::Url;
use tokio::net::lookup_host;

use crate::pipeline::errors::{DownloadError, Error};

const DANGEROUS_SUFFIXES: [&str; 6] = [".local", ".internal", ".arpa", ".corp", ".home", ".lan"];

const ALLOWED_PORTS: [u16; 2] = [443, 8443];
const ALLOWED_MEDIA_HOSTS: [&str; 4] = [
    "pbs.twimg.com",
    "video.twimg.com",
    "api.vxtwitter.com",
    "abs.twimg.com",
];

pub(crate) async fn validate_media_url(url: &str) -> Result<Url, Error> {
    let (parsed, host, port) = validate_url_structure(url)?;
    // Resolve DNS and validate each resolved address.
    let addrs = lookup_host((host.as_str(), port))
        .await
        .map_err(|e| validation_error(e.to_string()))?;

    validate_resolved_ips(addrs.map(|addr| addr.ip()))?;
    Ok(parsed)
}

fn validate_url_structure(url: &str) -> Result<(Url, String, u16), Error> {
    // Security-sensitive: do not accept non-HTTPS or local network targets.
    let parsed = Url::parse(url).map_err(|e| validation_error(e.to_string()))?;

    if parsed.scheme() != "https" {
        // Only allow HTTPS to reduce downgrade and local protocol abuse.
        return Err(validation_error("non-https url"));
    }

    let host = parsed
        .host_str()
        .ok_or_else(|| validation_error("missing url host"))?
        .trim_end_matches('.')
        .to_string();
    let host_lower = host.to_ascii_lowercase();

    if host_lower == "localhost"
        || DANGEROUS_SUFFIXES
            .iter()
            .any(|suffix| host_lower.ends_with(suffix))
    {
        // Block obvious local hostnames and internal suffixes.
        return Err(validation_error("local hostname rejected"));
    }

    if host.parse::<IpAddr>().is_ok() {
        // Reject IP literals; only allow DNS names with vetted resolution.
        return Err(validation_error("ip literal rejected"));
    }

    if !is_allowed_media_host(&host_lower) {
        return Err(validation_error("host not on allowlist"));
    }

    let port = parsed.port_or_known_default().unwrap_or(443);
    if !ALLOWED_PORTS.contains(&port) {
        return Err(validation_error("port not allowed"));
    }

    Ok((parsed, host, port))
}

fn validate_resolved_ips(ips: impl IntoIterator<Item = IpAddr>) -> Result<(), Error> {
    let mut resolved_any = false;
    for ip in ips {
        resolved_any = true;
        if is_blocked_ip(ip) {
            // Block link-local, private, loopback, and multicast ranges.
            return Err(validation_error("resolved to blocked ip"));
        }
    }

    if !resolved_any {
        // Some resolvers return zero addresses; treat as invalid input.
        return Err(validation_error("dns lookup returned no addresses"));
    }

    Ok(())
}

fn is_blocked_ip(ip: IpAddr) -> bool {
    // Non-obvious behavior: treat broadcast/multicast as blocked as well.
    match ip {
        IpAddr::V4(v4) => {
            v4.is_private()
                || v4.is_loopback()
                || v4.is_link_local()
                || v4.is_multicast()
                || v4.is_unspecified()
                || v4.is_broadcast()
        }
        IpAddr::V6(v6) => {
            if v6.to_ipv4().is_some() {
                return true;
            }
            v6.is_loopback()
                || v6.is_unique_local()
                || v6.is_unicast_link_local()
                || v6.is_multicast()
                || v6.is_unspecified()
        }
    }
}

fn is_allowed_media_host(host_lower: &str) -> bool {
    if ALLOWED_MEDIA_HOSTS.contains(&host_lower) {
        return true;
    }

    host_lower.ends_with(".twimg.com")
}

fn validation_error(message: impl Into<String>) -> Error {
    // SSRF blocks surface as security-specific download errors.
    Error::DownloadFailed {
        source: DownloadError::SsrfBlocked(message.into()),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]
    use super::*;
    use std::net::Ipv4Addr;

    fn validate_media_url_with_resolved_ips(
        url: &str,
        ips: impl IntoIterator<Item = IpAddr>,
    ) -> Result<Url, Error> {
        let (parsed, _, _) = validate_url_structure(url)?;
        validate_resolved_ips(ips)?;
        Ok(parsed)
    }

    #[tokio::test]
    async fn rejects_denylist_urls_with_expected_reasons() {
        let cases = [
            ("http://pbs.twimg.com/media/test.mp4", "non-https url"),
            (
                "https://localhost/media/test.mp4",
                "local hostname rejected",
            ),
            (
                "https://printer.local/media/test.mp4",
                "local hostname rejected",
            ),
            ("https://127.0.0.1/media/test.mp4", "ip literal rejected"),
        ];

        for (url, reason) in cases {
            let err = validate_media_url(url)
                .await
                .expect_err("denylisted url must be rejected");
            assert!(
                err.to_string().contains(reason),
                "expected reason `{reason}` for `{url}`, got `{err}`"
            );
        }
    }

    #[tokio::test]
    async fn rejects_disallowed_hosts_without_dns_lookup() {
        let err = validate_media_url("https://example.com/media/test.mp4")
            .await
            .expect_err("host not on allowlist must be rejected");
        assert!(err.to_string().contains("host not on allowlist"));
    }

    #[tokio::test]
    async fn rejects_unapproved_ports() {
        let err = validate_media_url("https://pbs.twimg.com:444/media/test.mp4")
            .await
            .expect_err("port should be rejected");
        assert!(err.to_string().contains("port not allowed"));
    }

    #[test]
    fn accepts_allowed_urls_with_public_dns_results() {
        let cases = [
            "https://pbs.twimg.com/media/test.mp4",
            "https://video.twimg.com:8443/media/test.mp4",
            "https://foo.twimg.com/media/test.mp4",
        ];

        for url in cases {
            validate_media_url_with_resolved_ips(url, [IpAddr::V4(Ipv4Addr::new(1, 1, 1, 1))])
                .expect("allowed host with public IP should pass");
        }
    }

    #[test]
    fn rejects_rfc1918_dns_results_with_reason() {
        let err = validate_media_url_with_resolved_ips(
            "https://pbs.twimg.com/media/test.mp4",
            [IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1))],
        )
        .expect_err("rfc1918 dns result must be rejected");
        assert!(err.to_string().contains("resolved to blocked ip"));
    }
}
