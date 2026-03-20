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

/// Top-level handler that converts [`ProxyError`] into HTTP error responses.
///
/// This is the primary public entry point for the image-proxy worker. It delegates
/// to [`handle_resize_inner`] for the actual pipeline and maps any `ProxyError`
/// variant to a well-formed HTTP response with the appropriate status code and
/// plain-text body.
///
/// # Parameters
///
/// * `req` — the incoming [`worker::Request`] from the Cloudflare runtime.
/// * `ctx` — the [`worker::Context`] used for background tasks (e.g. cache writes).
/// * `config` — shared [`Config`] parsed once at worker startup.
/// * `allowed_origins` — slice of origin strings permitted to call the proxy.
///
/// # Returns
///
/// Always returns `Ok(Response)` — errors are converted to HTTP error responses
/// via [`ProxyError::into_response`].
///
/// # Errors
///
/// The only `Err` this can surface is if `ProxyError::into_response` itself fails
/// to construct a `Response`, which would be a `worker::Error`.
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
    // Errors are intentionally ignored: these headers are advisory hints that make
    // upstream CDNs treat us like a browser. If any fail to set (which shouldn't
    // happen in practice), the request can still succeed without them.
    let _ = headers.set("User-Agent", security::BROWSER_USER_AGENT);
    let _ = headers.set("Referer", referer);
    let _ = headers.set("Accept", "image/webp,image/apng,image/*,*/*;q=0.8");
    let _ = headers.set("Accept-Language", "en-US,en;q=0.9");
}

/// Validate that the upstream response carries an allowed image content-type.
///
/// The raw `Content-Type` header value is lowercased before validation so that
/// comparisons are case-insensitive (per HTTP spec).
///
/// # Errors
///
/// * [`ProxyError::FetchFailed`] — if the header cannot be read from the response.
/// * [`ProxyError::InvalidContentType`] — if the media type is not in the
///   allowlist (delegated to [`security::validate_media_type`]).
fn validate_content_type(headers: &Headers) -> Result<&'static str, ProxyError> {
    let raw = headers
        .get("content-type")
        .map_err(|e| ProxyError::FetchFailed(e.to_string()))?
        .unwrap_or_default();
    // Fast path: most servers already send lowercase content-type.
    // Only allocate a lowercased copy if we find uppercase chars.
    if raw.bytes().any(|b| b.is_ascii_uppercase()) {
        let lower = raw.to_lowercase();
        security::validate_media_type(&lower)
    } else {
        security::validate_media_type(&raw)
    }
}

/// Inner handler — returns `ProxyError` (converted to HTTP by the outer handler).
///
/// Executes the 8-step image-proxy pipeline:
///
/// 1. **Origin validation** — verify the request's `Origin`/`Referer` against `allowed_origins`.
/// 2. **Param parsing** — extract and validate `url`, `w`, `h`, `q`, `fit` from query string.
/// 3. **Source URL validation** — domain allowlist check and SSRF protection.
/// 4. **Cache lookup** — check Cloudflare Cache API for a previously stored response.
/// 5. **Upstream GET** — fetch the source image with browser-like headers.
/// 6. **Content-type / size validation** — reject non-image types and oversized bodies.
/// 7. **Image processing** — decode, resize, and re-encode (JPEG→JPEG, PNG→WebP, GIF→passthrough).
/// 8. **Response + cache store** — build the HTTP response and asynchronously write to cache.
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

    // Best-effort pre-check: reject obviously oversized responses before downloading
    // the full body. Content-Length may be absent or inaccurate, so the post-download
    // check below remains the authoritative guard.
    if let Some(cl) = get_resp
        .headers()
        .get("content-length")
        .ok()
        .flatten()
    {
        if let Ok(len) = cl.parse::<u64>() {
            if len > config.max_size_bytes {
                let max_mb = config.max_size_bytes / (1024 * 1024);
                return Err(ProxyError::TooLarge(len as f64 / 1_048_576.0, max_mb));
            }
        }
    }

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
    let format = OutputFormat::from_content_type(content_type, &params);
    let result = process::process_image(bytes, &params, format)?;
    let output_content_type = result.output_content_type(content_type);
    let output_size = result.len();

    // 8. Build response + cache asynchronously
    // Use Response::cloned() to avoid duplicating the byte buffer in Rust.
    // The clone happens on the JS side (structured clone), which is cheaper
    // than a Rust Vec::clone() because it avoids a second WASM heap allocation.
    let encoded = result.into_bytes();
    let mut response = build_response(
        encoded,
        original_size,
        output_size,
        config.cache_ttl,
        &matched_origin,
        output_content_type,
    )?;

    let cache_resp = response
        .cloned()
        .map_err(|e| ProxyError::EncodeFailed(e.to_string()))?;

    ctx.wait_until(async move {
        let _ = cache.put(&cache_key, cache_resp).await;
    });

    Ok(response)
}

/// Build an HTTP response with processed image bytes and all required headers.
///
/// Headers set on the response:
///
/// * **Content-Type** — the output image MIME type (e.g. `image/jpeg`, `image/webp`).
/// * **Cache-Control** — aggressive caching directive:
///   `public, immutable, no-transform, max-age={cache_ttl}, s-maxage={cache_ttl}`.
/// * **CORS headers** — `Access-Control-Allow-Origin` and related headers for the
///   matched origin, produced by [`cors::cors_headers_for`].
/// * **X-Image-Original-Size** — byte size of the source image before processing.
/// * **X-Image-Output-Size** — byte size of the processed output image.
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
        .map_err(|e| ProxyError::EncodeFailed(e.to_string()))?;
    headers
        .set(
            "Cache-Control",
            &format!("public, immutable, no-transform, max-age={cache_ttl}, s-maxage={cache_ttl}"),
        )
        .map_err(|e| ProxyError::EncodeFailed(e.to_string()))?;
    headers
        .set("X-Image-Original-Size", &original_size.to_string())
        .map_err(|e| ProxyError::EncodeFailed(e.to_string()))?;
    headers
        .set("X-Image-Output-Size", &output_size.to_string())
        .map_err(|e| ProxyError::EncodeFailed(e.to_string()))?;

    Response::from_bytes(bytes)
        .map_err(|e| ProxyError::EncodeFailed(e.to_string()))
        .map(|r| r.with_headers(headers))
}
