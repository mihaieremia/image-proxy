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
/// Returns the parsed URL on success (avoids re-parsing later).
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
        .ok_or_else(|| ProxyError::InvalidParam("source URL has no host".into()))?
        .to_lowercase();

    // SSRF: block private/reserved IPs and hostnames
    validate_not_private(&host)?;

    // Domain allowlist
    if let Some(allowed) = allowed_domains {
        if !allowed.iter().any(|d| {
            host == *d
                || (host.len() > d.len()
                    && host.ends_with(d.as_str())
                    && host.as_bytes()[host.len() - d.len() - 1] == b'.')
        }) {
            return Err(ProxyError::DomainNotAllowed(host));
        }
    }

    Ok(parsed)
}

/// Block private/reserved IP ranges and known metadata hostnames.
fn validate_not_private(host: &str) -> Result<(), ProxyError> {
    const BLOCKED_HOSTS: &[&str] = &[
        "localhost",
        "127.0.0.1",
        "[::1]",
        "0.0.0.0",
        "metadata.google.internal",
    ];
    if BLOCKED_HOSTS.contains(&host) {
        return Err(ProxyError::SsrfBlocked);
    }

    if let Ok(ip) = host.parse::<std::net::Ipv4Addr>() {
        if ip.is_loopback()
            || ip.is_private()
            || ip.is_link_local()
            || ip.is_broadcast()
            || ip.is_unspecified()
            || (ip.octets()[0] == 169 && ip.octets()[1] == 254)
            || (ip.octets()[0] == 100 && (64..=127).contains(&ip.octets()[1]))
        {
            return Err(ProxyError::SsrfBlocked);
        }
    }

    if let Ok(ip) = host
        .trim_matches(|c| c == '[' || c == ']')
        .parse::<std::net::Ipv6Addr>()
    {
        if ip.is_loopback() || ip.is_unspecified() {
            return Err(ProxyError::SsrfBlocked);
        }
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
