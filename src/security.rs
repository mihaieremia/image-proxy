//! Shared security validation: domain allowlist and SSRF protection.
//!
//! Used by both Cloudflare Worker and native server builds.

use crate::error::ProxyError;

/// Content types accepted from upstream image servers.
pub const ALLOWED_CONTENT_TYPES: &[&str] = &["image/jpeg", "image/png", "image/gif", "image/webp"];

/// Browser-like User-Agent sent to upstream CDNs.
pub const BROWSER_USER_AGENT: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";

/// Validate the source URL: parse once, check domain allowlist and SSRF.
///
/// Returns the parsed `Url` on success so callers may inspect scheme/host
/// without re-parsing, though the primary purpose is validation.
pub fn validate_source_url(
    raw_url: &str,
    allowed_domains: &Option<Vec<String>>,
) -> Result<url::Url, ProxyError> {
    let parsed = url::Url::parse(raw_url)
        .map_err(|_| ProxyError::InvalidParam(format!("invalid source URL: {raw_url}")))?;

    // Only allow HTTP(S)
    match parsed.scheme() {
        "http" | "https" => {}
        _ => {
            return Err(ProxyError::InvalidParam(
                "only http/https URLs allowed".into(),
            ))
        }
    }

    let host = parsed
        .host_str()
        .ok_or_else(|| ProxyError::InvalidParam("source URL has no host".into()))?;

    // SSRF: block private/reserved IPs and hostnames
    validate_not_private(host)?;

    // Domain allowlist (case-insensitive comparison without allocating a lowercased copy)
    if let Some(allowed) = allowed_domains {
        if !allowed.iter().any(|d| {
            host.eq_ignore_ascii_case(d)
                || (host.len() > d.len()
                    && host[host.len() - d.len()..].eq_ignore_ascii_case(d)
                    && host.as_bytes()[host.len() - d.len() - 1] == b'.')
        }) {
            return Err(ProxyError::DomainNotAllowed(host.to_lowercase()));
        }
    }

    Ok(parsed)
}

/// Block private/reserved IP ranges and known metadata hostnames.
///
/// Blocked IPv4 ranges:
/// - `127.0.0.0/8` — loopback
/// - `10.0.0.0/8`, `172.16.0.0/12`, `192.168.0.0/16` — RFC 1918 private
/// - `169.254.0.0/16` — link-local
/// - `100.64.0.0/10` — CGNAT (RFC 6598)
/// - `255.255.255.255` — broadcast
/// - `0.0.0.0` — unspecified
///
/// Blocked IPv6 ranges:
/// - `::1` — loopback
/// - `::` — unspecified
/// - `fc00::/7` — Unique Local Addresses (ULA)
/// - `fe80::/10` — link-local
/// - `::ffff:0:0/96` — IPv4-mapped (checked against IPv4 rules above)
///
/// **Caveat**: This function validates the literal host string only. It does
/// not perform DNS resolution, so it cannot guard against DNS-rebinding
/// attacks where a hostname resolves to a private IP after validation.
fn validate_not_private(host: &str) -> Result<(), ProxyError> {
    const BLOCKED_HOSTS: &[&str] = &[
        "localhost",
        "127.0.0.1",
        "[::1]",
        "0.0.0.0",
        "metadata.google.internal",
    ];
    if BLOCKED_HOSTS
        .iter()
        .any(|h| host.eq_ignore_ascii_case(h))
    {
        return Err(ProxyError::SsrfBlocked);
    }

    if let Ok(ip) = host.parse::<std::net::Ipv4Addr>() {
        check_ipv4_private(ip)?;
    }

    if let Ok(ip) = host
        .trim_matches(|c| c == '[' || c == ']')
        .parse::<std::net::Ipv6Addr>()
    {
        if ip.is_loopback() || ip.is_unspecified() {
            return Err(ProxyError::SsrfBlocked);
        }
        // ULA fc00::/7
        if ip.segments()[0] & 0xfe00 == 0xfc00 {
            return Err(ProxyError::SsrfBlocked);
        }
        // Link-local fe80::/10
        if ip.segments()[0] & 0xffc0 == 0xfe80 {
            return Err(ProxyError::SsrfBlocked);
        }
        // IPv4-mapped ::ffff:x.x.x.x — apply IPv4 rules
        if let Some(mapped) = ip.to_ipv4_mapped() {
            check_ipv4_private(mapped)?;
        }
    }

    Ok(())
}

/// Check an IPv4 address against all blocked private/reserved ranges.
fn check_ipv4_private(ip: std::net::Ipv4Addr) -> Result<(), ProxyError> {
    if ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        || ip.is_broadcast()
        || ip.is_unspecified()
        || (ip.octets()[0] == 100 && (64..=127).contains(&ip.octets()[1]))
    {
        return Err(ProxyError::SsrfBlocked);
    }
    Ok(())
}

/// Extract media type from a Content-Type header value, stripping parameters.
/// e.g., `"image/jpeg; charset=utf-8"` → `"image/jpeg"`
pub fn extract_media_type(content_type: &str) -> &str {
    content_type.split(';').next().unwrap_or("").trim()
}

/// Validate a content-type string against the allowed list.
/// Takes an already-lowercased raw content-type value.
pub fn validate_media_type(raw_content_type: &str) -> Result<String, ProxyError> {
    let media_type = extract_media_type(raw_content_type);
    if ALLOWED_CONTENT_TYPES.contains(&media_type) {
        Ok(media_type.to_string())
    } else {
        Err(ProxyError::InvalidContentType(raw_content_type.to_string()))
    }
}
