//! SSRF guardrails for untrusted media URLs.
//!
//! ## Security-sensitive paths
//! This module defends against local network access via crafted URLs. It is
//! applied to every outbound media request, including each redirect hop.
//!
//! ## Algorithm overview
//! 1. [`validate_media_url`]: parse the URL, enforce HTTPS, allowed ports, and
//!    the media host allowlist; reject localhost, local suffixes, and IP literals.
//! 2. [`Resolver`]: resolve DNS inside the HTTP client's connector and reject
//!    private/link-local ranges for allowlisted media hosts.
//!
//! ## Rejected alternative
//! Resolving in a separate pre-flight step and pinning a dedicated client to
//! the answer cost a second DNS lookup per hop, a ~5 ms client (root store)
//! build whenever the CDN rotated its answer, and a cold TCP+TLS handshake
//! because each pinned client had its own pool. Validating inside the
//! connector's resolver checks exactly the addresses that get connected, so it
//! closes the DNS-rebinding window without defeating connection reuse.
//!
//! ## References
//! - SSRF guidance: <https://owasp.org/www-community/attacks/Server_Side_Request_Forgery>

use std::net::{IpAddr, SocketAddr};

use reqwest::Url;
use reqwest::dns::{Addrs, Name, Resolve, Resolving};
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

/// A media URL whose structure passed [`validate_media_url`].
///
/// Address validation happens later, in [`Resolver`], at connect time.
#[derive(Debug, Clone)]
pub(crate) struct ValidatedMediaUrl {
    pub(crate) url: Url,
}

pub(crate) fn validate_media_url(url: &str) -> Result<ValidatedMediaUrl, Error> {
    let (url, _) = validate_url_structure(url)?;
    Ok(ValidatedMediaUrl { url })
}

/// DNS resolver for the media client that refuses blocked addresses.
///
/// ## Invariant
/// Names outside the media allowlist are resolved without address checks:
/// [`validate_media_url`] guarantees the media client only targets allowlisted
/// hosts, so any other name the connector resolves is an operator-configured
/// proxy.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Resolver;

impl Resolve for Resolver {
    fn resolve(&self, name: Name) -> Resolving {
        let host = name.as_str().trim_end_matches('.').to_ascii_lowercase();
        Box::pin(async move {
            let addrs = lookup_host((host.as_str(), 0)).await?;
            let addrs = if is_allowed_media_host(&host) {
                validate_resolved_addrs(addrs).map_err(|_| BlockedAddress)?
            } else {
                addrs.collect()
            };
            Ok(Box::new(addrs.into_iter()) as Addrs)
        })
    }
}

/// Connector error raised when an allowlisted host resolves to a blocked or
/// empty address set; recovered from reqwest's error chain by
/// [`is_blocked_address`] so SSRF blocks keep their own error category.
#[derive(Debug)]
struct BlockedAddress;

impl std::fmt::Display for BlockedAddress {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("resolved to blocked ip")
    }
}

impl std::error::Error for BlockedAddress {}

pub(crate) fn is_blocked_address(error: &(dyn std::error::Error + 'static)) -> bool {
    let mut current = Some(error);
    while let Some(error) = current {
        if error.is::<BlockedAddress>() {
            return true;
        }
        // `io::Error::source` skips its payload, so step into it explicitly.
        let io_payload = error
            .downcast_ref::<std::io::Error>()
            .and_then(std::io::Error::get_ref)
            .map(|payload| payload as &(dyn std::error::Error + 'static));
        current = io_payload.or_else(|| error.source());
    }
    false
}

pub(crate) fn blocked_address_error() -> Error {
    validation_error(BlockedAddress.to_string())
}

fn validate_url_structure(url: &str) -> Result<(Url, u16), Error> {
    // Security-sensitive: do not accept non-HTTPS or local network targets.
    let parsed = Url::parse(url).map_err(|e| validation_error(e.to_string()))?;

    if parsed.scheme() != "https" {
        // Only allow HTTPS to reduce downgrade and local protocol abuse.
        return Err(validation_error("non-https url"));
    }

    let host = parsed
        .host_str()
        .ok_or_else(|| validation_error("missing url host"))?
        .trim_end_matches('.');
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

    Ok((parsed, port))
}

fn validate_resolved_addrs(
    addrs: impl IntoIterator<Item = SocketAddr>,
) -> Result<Vec<SocketAddr>, Error> {
    let mut validated = Vec::new();
    for addr in addrs {
        if is_blocked_ip(addr.ip()) {
            return Err(validation_error("resolved to blocked ip"));
        }
        validated.push(addr);
    }

    if validated.is_empty() {
        return Err(validation_error("dns lookup returned no addresses"));
    }

    Ok(validated)
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
        let (parsed, port) = validate_url_structure(url)?;
        validate_resolved_addrs(ips.into_iter().map(|ip| SocketAddr::new(ip, port)))?;
        Ok(parsed)
    }

    #[test]
    fn rejects_denylist_urls_with_expected_reasons() {
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
            let err = validate_media_url(url).expect_err("denylisted url must be rejected");
            assert!(
                err.to_string().contains(reason),
                "expected reason `{reason}` for `{url}`, got `{err}`"
            );
        }
    }

    #[test]
    fn rejects_disallowed_hosts_without_dns_lookup() {
        let err = validate_media_url("https://example.com/media/test.mp4")
            .expect_err("host not on allowlist must be rejected");
        assert!(err.to_string().contains("host not on allowlist"));
    }

    #[test]
    fn rejects_unapproved_ports() {
        let err = validate_media_url("https://pbs.twimg.com:444/media/test.mp4")
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

    #[derive(Debug, Clone, Copy)]
    struct BlockingResolver;

    impl Resolve for BlockingResolver {
        fn resolve(&self, _name: Name) -> Resolving {
            Box::pin(async { Err(Box::new(BlockedAddress) as Box<_>) })
        }
    }

    #[tokio::test]
    async fn blocked_address_survives_reqwest_error_chain() {
        let client = reqwest::Client::builder()
            .no_proxy()
            .dns_resolver(BlockingResolver)
            .build()
            .expect("test client");
        let error = client
            .get("http://media.invalid/a.mp4")
            .send()
            .await
            .expect_err("blocked resolution must fail the request");

        assert!(is_blocked_address(&error));
        assert!(is_blocked_address(&std::io::Error::other(BlockedAddress)));
        assert!(!is_blocked_address(&std::io::Error::other("refused")));
    }

    #[tokio::test]
    async fn resolver_passes_through_names_outside_the_media_allowlist() {
        let name = "localhost".parse::<Name>().expect("valid name");
        let addrs = Resolver
            .resolve(name)
            .await
            .expect("proxy-style names resolve without address checks");
        assert!(addrs.count() > 0);
    }
}
