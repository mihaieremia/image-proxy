//! Request handler for the Cloudflare Worker build.
//!
//! Orchestrates the full request pipeline: origin validation → param parsing →
//! security checks → cache lookup → upstream fetch → image processing → response.

use worker::*;

use crate::config::Config;
use crate::cors;
use crate::error::ProxyError;
use crate::params::ResizeParams;
use crate::process::{self, OutputFormat};
use crate::security;

/// Top-level handler that converts `ProxyError` into HTTP error responses.
pub async fn handle_resize(
    req: Request,
    ctx: Context,
    config: &Config,
    allowed_origins: &[String],
) -> Result<Response> {
    let result = handle_resize_inner(req, ctx, config, allowed_origins).await;
    match result {
        Ok(resp) => Ok(resp),
        Err(e) => e.into_response(),
    }
}

/// Set browser-like headers on outgoing upstream requests.
fn set_browser_headers(headers: &mut Headers, referer: &str) {
    let _ = headers.set("User-Agent", security::BROWSER_USER_AGENT);
    let _ = headers.set("Referer", referer);
    let _ = headers.set("Accept", "image/webp,image/apng,image/*,*/*;q=0.8");
    let _ = headers.set("Accept-Language", "en-US,en;q=0.9");
}

/// Validate content-type from a worker response.
fn validate_content_type(headers: &Headers) -> Result<String, ProxyError> {
    let raw = headers
        .get("content-type")
        .map_err(|e| ProxyError::FetchFailed(e.to_string()))?
        .unwrap_or_default()
        .to_lowercase();
    security::validate_media_type(&raw)
}

/// Inner handler — returns `ProxyError` (converted to HTTP by outer handler).
async fn handle_resize_inner(
    req: Request,
    ctx: Context,
    config: &Config,
    allowed_origins: &[String],
) -> Result<Response, ProxyError> {
    // 1. Validate request origin
    let matched_origin = cors::validate_request_origin(&req, allowed_origins)?;

    // 2. Parse & validate query params
    let url = req
        .url()
        .map_err(|e| ProxyError::InvalidParam(e.to_string()))?;
    let params = ResizeParams::from_url(&url, config.max_width, config.max_height)?;

    // 3. Validate source URL once (domain allowlist + SSRF)
    security::validate_source_url(&params.url, &config.allowed_domains)?;

    // 4. Check cache
    let cache_key = params.cache_key();
    let cache = Cache::default();
    if let Some(cached) = cache
        .get(&cache_key, false)
        .await
        .map_err(|e| ProxyError::FetchFailed(e.to_string()))?
    {
        return Ok(cached);
    }

    // 5. GET source image
    let mut get_req = Request::new(&params.url, Method::Get)
        .map_err(|e| ProxyError::FetchFailed(e.to_string()))?;
    set_browser_headers(
        get_req
            .headers_mut()
            .map_err(|e| ProxyError::FetchFailed(e.to_string()))?,
        &config.referer,
    );

    let mut get_resp = Fetch::Request(get_req)
        .send()
        .await
        .map_err(|e| ProxyError::FetchFailed(e.to_string()))?;

    let get_status = get_resp.status_code();
    if !(200..300).contains(&get_status) {
        return Err(ProxyError::FetchFailed(format!(
            "upstream returned status {get_status}"
        )));
    }

    // 6. Validate content-type and body size
    let content_type = validate_content_type(get_resp.headers())?;

    let bytes = get_resp
        .bytes()
        .await
        .map_err(|e| ProxyError::FetchFailed(e.to_string()))?;

    if bytes.len() as u64 > config.max_size_bytes {
        let max_mb = config.max_size_bytes / (1024 * 1024);
        return Err(ProxyError::TooLarge(
            bytes.len() as f64 / 1_048_576.0,
            max_mb,
        ));
    }

    let original_size = bytes.len();

    // 7. Process image
    let format = OutputFormat::from_content_type(&content_type, &params);
    let result = process::process_image(bytes, &params, format)?;
    let output_content_type = result.output_content_type(&content_type);
    let output_size = result.len();

    // 8. Build response + cache asynchronously
    let encoded = result.into_bytes();
    let cache_bytes = encoded.clone();
    let response = build_response(
        encoded,
        original_size,
        output_size,
        config.cache_ttl,
        &matched_origin,
        output_content_type,
    )?;
    let cache_resp = build_response(
        cache_bytes,
        original_size,
        output_size,
        config.cache_ttl,
        &matched_origin,
        output_content_type,
    )?;

    ctx.wait_until(async move {
        let _ = cache.put(&cache_key, cache_resp).await;
    });

    Ok(response)
}

/// Build an HTTP response with processed image bytes.
fn build_response(
    bytes: Vec<u8>,
    original_size: usize,
    output_size: usize,
    cache_ttl: u64,
    origin: &str,
    content_type: &str,
) -> Result<Response, ProxyError> {
    let headers =
        cors::cors_headers_for(origin).map_err(|e| ProxyError::FetchFailed(e.to_string()))?;
    headers
        .set("Content-Type", content_type)
        .map_err(|_| ProxyError::EncodeFailed)?;
    headers
        .set(
            "Cache-Control",
            &format!("public, immutable, no-transform, max-age={cache_ttl}, s-maxage={cache_ttl}"),
        )
        .map_err(|_| ProxyError::EncodeFailed)?;
    headers
        .set("X-Image-Original-Size", &original_size.to_string())
        .map_err(|_| ProxyError::EncodeFailed)?;
    headers
        .set("X-Image-Output-Size", &output_size.to_string())
        .map_err(|_| ProxyError::EncodeFailed)?;

    Response::from_bytes(bytes)
        .map_err(|_| ProxyError::EncodeFailed)
        .map(|r| r.with_headers(headers))
}
