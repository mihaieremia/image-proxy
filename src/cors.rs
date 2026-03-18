//! CORS and origin validation for the Cloudflare Worker build.
//!
//! Validates that incoming requests originate from allowed websites
//! and sets appropriate CORS headers on responses.

use worker::{Headers, Request, Response};

use crate::error::ProxyError;

/// Default allowed origins when `ALLOWED_ORIGINS` env var is not set.
const DEFAULT_ALLOWED_ORIGINS: &[&str] = &["https://chartex.com", "https://www.chartex.com"];

/// Check if a given origin string exactly matches any entry in the allowlist.
fn is_origin_allowed(origin: &str, allowed: &[String]) -> bool {
    allowed.iter().any(|a| a == origin)
}

/// Parse the `ALLOWED_ORIGINS` env var as a comma-separated list.
/// Falls back to `DEFAULT_ALLOWED_ORIGINS` if unset or empty.
pub fn allowed_origins(env: &worker::Env) -> Vec<String> {
    env.var("ALLOWED_ORIGINS")
        .ok()
        .map(|v| v.to_string())
        .filter(|s| !s.is_empty())
        .map(|s| {
            s.split(',')
                .map(|o| o.trim().to_string())
                .filter(|o| !o.is_empty())
                .collect()
        })
        .unwrap_or_else(|| {
            DEFAULT_ALLOWED_ORIGINS
                .iter()
                .map(|s| s.to_string())
                .collect()
        })
}

/// Validate that the request's Origin or Referer matches an allowed origin.
///
/// Strategy:
/// 1. If `Origin` header present → must match allowlist (browsers send this cross-origin).
/// 2. Else if `Referer` present → extract origin portion, must match allowlist.
/// 3. Else (no headers) → allow through with `*` origin. Direct requests (cURL,
///    same-origin `<img>`, server-to-server) don't send Origin. The source domain
///    allowlist provides protection in this case.
///
/// Returns the matched origin for use in CORS response headers.
pub fn validate_request_origin(req: &Request, allowed: &[String]) -> Result<String, ProxyError> {
    let headers = req.headers();

    // Try Origin header first (set by browsers on cross-origin requests)
    if let Ok(Some(origin)) = headers.get("Origin") {
        if is_origin_allowed(&origin, allowed) {
            return Ok(origin);
        }
        return Err(ProxyError::OriginNotAllowed(origin));
    }

    // Fallback to Referer header (some browsers send this for <img> tags)
    if let Ok(Some(referer)) = headers.get("Referer") {
        if let Ok(parsed) = url::Url::parse(&referer) {
            let referer_origin =
                format!("{}://{}", parsed.scheme(), parsed.host_str().unwrap_or(""));
            if is_origin_allowed(&referer_origin, allowed) {
                return Ok(referer_origin);
            }
            return Err(ProxyError::OriginNotAllowed(referer_origin));
        }
    }

    // No Origin or Referer — allow through (cURL, same-origin, server-to-server).
    // ALLOWED_DOMAINS still protects against open-proxy abuse.
    Ok("*".into())
}

/// Build CORS response headers for the given matched origin.
/// Uses the specific origin (not `*`) with `Vary: Origin` for correct CDN behavior.
pub fn cors_headers_for(origin: &str) -> Result<Headers, worker::Error> {
    let headers = Headers::new();
    headers.set("Access-Control-Allow-Origin", origin)?;
    headers.set("Access-Control-Allow-Methods", "GET, HEAD, OPTIONS")?;
    headers.set("Access-Control-Allow-Headers", "Content-Type")?;
    headers.set("Vary", "Origin")?;
    Ok(headers)
}

/// Handle CORS preflight (OPTIONS) requests.
/// Matches the Origin against the allowlist and returns a 204 with CORS headers,
/// or 403 if the origin is not allowed.
pub fn handle_preflight(req: &Request, allowed: &[String]) -> Result<Response, worker::Error> {
    let origin = req
        .headers()
        .get("Origin")
        .ok()
        .flatten()
        .unwrap_or_default();

    if is_origin_allowed(&origin, allowed) {
        let headers = cors_headers_for(&origin)?;
        headers.set("Access-Control-Max-Age", "86400")?;
        Ok(Response::empty()?.with_status(204).with_headers(headers))
    } else {
        Response::error("Forbidden", 403)
    }
}
