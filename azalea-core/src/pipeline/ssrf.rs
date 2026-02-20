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
    // Security-sensitive: do not accept non-HTTPS or local network targets.
    let parsed = Url::parse(url).map_err(|e| validation_error(e.to_string()))?;

    if parsed.scheme() != "https" {
        // Only allow HTTPS to reduce downgrade and local protocol abuse.
        return Err(validation_error("non-https url"));
    }

    let host = parsed
        .host_str()
        .ok_or_else(|| validation_error("missing url host"))?;
    let host_trimmed = host.trim_end_matches('.');
    let host_lower = host_trimmed.to_ascii_lowercase();

    if host_lower == "localhost"
        || DANGEROUS_SUFFIXES
            .iter()
            .any(|suffix| host_lower.ends_with(suffix))
    {
        // Block obvious local hostnames and internal suffixes.
        return Err(validation_error("local hostname rejected"));
    }

    if !is_allowed_media_host(&host_lower) {
        return Err(validation_error("host not on allowlist"));
    }

    if host_trimmed.parse::<IpAddr>().is_ok() {
        // Reject IP literals; only allow DNS names with vetted resolution.
        return Err(validation_error("ip literal rejected"));
    }

    let port = parsed.port_or_known_default().unwrap_or(443);
    if !ALLOWED_PORTS.contains(&port) {
        return Err(validation_error("port not allowed"));
    }
    // Resolve DNS and validate each resolved address.
    let addrs = lookup_host((host_trimmed, port))
        .await
        .map_err(|e| validation_error(e.to_string()))?;

    let mut resolved_any = false;
    for addr in addrs {
        resolved_any = true;
        if is_blocked_ip(addr.ip()) {
            // Block link-local, private, loopback, and multicast ranges.
            return Err(validation_error("resolved to blocked ip"));
        }
    }

    if !resolved_any {
        // Some resolvers return zero addresses; treat as invalid input.
        return Err(validation_error("dns lookup returned no addresses"));
    }

    Ok(parsed)
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
