//! Native server entry point — standalone tokio/axum binary.
//!
//! Build with: `cargo build --release --features native --no-default-features --bin image-proxy-server`
//!
//! This provides the same image proxy functionality as the Cloudflare Worker
//! but runs as a standard HTTP server on any platform.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::extract::State;
use axum::http::header::{
    ACCESS_CONTROL_ALLOW_HEADERS, ACCESS_CONTROL_ALLOW_METHODS, ACCESS_CONTROL_ALLOW_ORIGIN,
    ACCESS_CONTROL_MAX_AGE, CACHE_CONTROL, CONTENT_TYPE, ORIGIN, REFERER, VARY,
};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;

use image_proxy::error::ProxyError;
use image_proxy::params::ResizeParams;
use image_proxy::process::{self, OutputFormat};

/// Newtype wrapper to implement `IntoResponse` for `ProxyError`
/// (orphan rule: can't impl foreign trait on foreign type directly).
struct AppError(ProxyError);

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Server configuration, parsed once from environment variables at startup.
#[derive(Clone)]
struct Config {
    /// Maximum output width in pixels (env `MAX_WIDTH`, default 4096).
    max_width: u32,
    /// Maximum output height in pixels (env `MAX_HEIGHT`, default 4096).
    max_height: u32,
    /// Maximum source image size in bytes, derived from `MAX_SIZE_MB` (default 25 MiB).
    max_size_bytes: u64,
    /// `Cache-Control` max-age / s-maxage value in seconds (env `CACHE_TTL`, default 90 days).
    cache_ttl: u64,
    /// Optional allowlist of source image domains. `None` means all domains are permitted.
    /// Parsed from the comma-separated env var `ALLOWED_DOMAINS`.
    allowed_domains: Option<Vec<String>>,
    /// Origins permitted to call the proxy, checked against `Origin`/`Referer` headers.
    /// Parsed from the comma-separated env var `ALLOWED_ORIGINS`.
    allowed_origins: Vec<String>,
    /// `Referer` header value sent to upstream CDNs (env `UPSTREAM_REFERER`).
    referer: String,
    /// TCP port the server listens on (env `PORT`, default 8080).
    port: u16,
}

impl Config {
    /// Parse configuration from `std::env`. Falls back to sensible defaults.
    fn from_env() -> Self {
        let max_size_mb = env_parse("MAX_SIZE_MB", 25u64);
        Self {
            max_width: env_parse("MAX_WIDTH", 4096u32),
            max_height: env_parse("MAX_HEIGHT", 4096u32),
            max_size_bytes: max_size_mb * 1024 * 1024,
            cache_ttl: env_parse("CACHE_TTL", 7_776_000u64),
            allowed_domains: parse_csv_env("ALLOWED_DOMAINS"),
            allowed_origins: parse_csv_env("ALLOWED_ORIGINS").unwrap_or_else(|| {
                vec![
                    "https://chartex.com".into(),
                    "https://www.chartex.com".into(),
                ]
            }),
            referer: std::env::var("UPSTREAM_REFERER")
                .unwrap_or_else(|_| "https://chartex.com/".into()),
            port: env_parse("PORT", 8080u16),
        }
    }
}

/// Generic env var parser with fallback.
fn env_parse<T: std::str::FromStr>(key: &str, default: T) -> T {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// Parse a comma-separated env var into a `Vec<String>`.
/// Returns `None` if the var is unset or empty.
fn parse_csv_env(key: &str) -> Option<Vec<String>> {
    std::env::var(key).ok().filter(|s| !s.is_empty()).map(|s| {
        s.split(',')
            .map(|v| v.trim().trim_start_matches('.').to_lowercase())
            .filter(|v| !v.is_empty())
            .collect()
    })
}

// ---------------------------------------------------------------------------
// Shared state
// ---------------------------------------------------------------------------

/// Application state shared across all request handlers.
struct AppState {
    /// Server configuration parsed once at startup from environment variables.
    config: Config,
    /// Shared HTTP client configured with a 30-second timeout and a 5-redirect limit.
    http: reqwest::Client,
}

// ---------------------------------------------------------------------------
// Error → HTTP response
// ---------------------------------------------------------------------------

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let status =
            StatusCode::from_u16(self.0.status_code()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        let body = self.0.to_string();
        (status, [(CONTENT_TYPE, "text/plain; charset=utf-8")], body).into_response()
    }
}

impl From<ProxyError> for AppError {
    fn from(e: ProxyError) -> Self {
        Self(e)
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

use image_proxy::security;

/// Validate the request's Origin/Referer against the allowed origins list.
///
/// This mirrors [`cors::validate_request_origin`] in the Cloudflare Worker build.
/// Security policy changes must be applied to both implementations.
fn validate_origin(headers: &HeaderMap, allowed: &[String]) -> Result<String, ProxyError> {
    if let Some(origin) = headers.get(ORIGIN).and_then(|v| v.to_str().ok()) {
        if allowed.iter().any(|a| a == origin) {
            return Ok(origin.to_string());
        }
        return Err(ProxyError::OriginNotAllowed(origin.into()));
    }

    if let Some(referer) = headers.get(REFERER).and_then(|v| v.to_str().ok()) {
        if let Ok(parsed) = url::Url::parse(referer) {
            let referer_origin =
                format!("{}://{}", parsed.scheme(), parsed.host_str().unwrap_or(""));
            if allowed.iter().any(|a| a == &referer_origin) {
                return Ok(referer_origin);
            }
            return Err(ProxyError::OriginNotAllowed(referer_origin));
        }
    }

    Ok("*".into())
}

/// Validate content-type from upstream reqwest response headers.
fn validate_content_type(headers: &reqwest::header::HeaderMap) -> Result<String, ProxyError> {
    let raw = headers
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_lowercase();
    security::validate_media_type(&raw)
}

/// Main resize handler for GET `/` and GET `/resize`.
///
/// Implements the same pipeline as the Cloudflare [`handler::handle_resize_inner`] —
/// origin check, param parsing, SSRF/domain validation, fetch, content-type
/// validation, size limit, decode/resize/encode — but **without caching**
/// (no Cloudflare Cache API layer) and uses `reqwest` instead of `worker::Fetch`.
async fn handle_resize(
    State(state): State<Arc<AppState>>,
    req: axum::extract::Request,
) -> Result<Response, AppError> {
    let config = &state.config;

    // 1. Validate request origin (extract before borrowing req further)
    let matched_origin = validate_origin(req.headers(), &config.allowed_origins)?;

    // 2. Parse params from the raw query string (not pre-decoded HashMap)
    // TODO(PERF-7): A `ResizeParams::from_query_str` method would avoid this
    // fake URL construction and the extra allocation + parse overhead.
    let raw_query = req.uri().query().unwrap_or("").to_string();
    let fake_url = url::Url::parse(&format!("http://localhost/?{raw_query}"))
        .map_err(|e| ProxyError::InvalidParam(e.to_string()))?;
    let params = ResizeParams::from_url(&fake_url, config.max_width, config.max_height)?;

    // 3. Validate source URL (domain allowlist + SSRF protection)
    security::validate_source_url(&params.url, &config.allowed_domains)?;

    // 4. Fetch source image with browser-like headers
    let resp = state
        .http
        .get(&params.url)
        .header("User-Agent", security::BROWSER_USER_AGENT)
        .header("Referer", &config.referer)
        .header("Accept", "image/webp,image/apng,image/*,*/*;q=0.8")
        .header("Accept-Language", "en-US,en;q=0.9")
        .send()
        .await
        .map_err(|e| ProxyError::FetchFailed(e.to_string()))?;

    if !resp.status().is_success() {
        return Err(
            ProxyError::FetchFailed(format!("upstream returned status {}", resp.status())).into(),
        );
    }

    // 5. Validate content-type
    let content_type = validate_content_type(resp.headers())?;

    // 6. Read body + enforce size limit
    // TODO(PERF-8): `process_image` could accept `bytes::Bytes` directly to
    // avoid this `.to_vec()` copy.
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| ProxyError::FetchFailed(e.to_string()))?
        .to_vec();

    if bytes.len() as u64 > config.max_size_bytes {
        let max_mb = config.max_size_bytes / (1024 * 1024);
        return Err(ProxyError::TooLarge(bytes.len() as f64 / 1_048_576.0, max_mb).into());
    }

    let original_size = bytes.len();

    // 7. Process image
    let format = OutputFormat::from_content_type(&content_type, &params);
    let result = process::process_image(bytes, &params, format)?;
    let output_content_type = result.output_content_type(&content_type);
    let output_size = result.len();
    let encoded = result.into_bytes();

    // 8. Build response with CORS and cache headers
    let content_type_val = HeaderValue::from_str(output_content_type)
        .map_err(|_| ProxyError::InvalidParam("invalid content-type header value".into()))?;
    let cache_control_val = HeaderValue::from_str(&format!(
        "public, immutable, no-transform, max-age={}, s-maxage={}",
        config.cache_ttl, config.cache_ttl
    ))
    .map_err(|_| ProxyError::InvalidParam("invalid cache-control header value".into()))?;
    let origin_val = HeaderValue::from_str(&matched_origin)
        .map_err(|_| ProxyError::InvalidParam("matched origin contains invalid header characters".into()))?;

    let mut response = (
        StatusCode::OK,
        [
            (CONTENT_TYPE, content_type_val),
            (CACHE_CONTROL, cache_control_val),
            (ACCESS_CONTROL_ALLOW_ORIGIN, origin_val),
            (
                ACCESS_CONTROL_ALLOW_METHODS,
                HeaderValue::from_static("GET, HEAD, OPTIONS"),
            ),
            (
                ACCESS_CONTROL_ALLOW_HEADERS,
                HeaderValue::from_static("Content-Type"),
            ),
            (VARY, HeaderValue::from_static("Origin")),
        ],
        encoded,
    )
        .into_response();

    // Add custom size headers (numeric .to_string() is always valid ASCII)
    if let Ok(v) = HeaderValue::from_str(&original_size.to_string()) {
        response.headers_mut().insert("X-Image-Original-Size", v);
    }
    if let Ok(v) = HeaderValue::from_str(&output_size.to_string()) {
        response.headers_mut().insert("X-Image-Output-Size", v);
    }

    Ok(response)
}

/// CORS preflight handler for OPTIONS requests.
async fn handle_preflight(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    let origin = headers
        .get(ORIGIN)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    if state.config.allowed_origins.iter().any(|a| a == origin) {
        let origin_val = HeaderValue::from_str(origin)
            .unwrap_or_else(|_| HeaderValue::from_static("*"));
        (
            StatusCode::NO_CONTENT,
            [
                (
                    ACCESS_CONTROL_ALLOW_ORIGIN,
                    origin_val,
                ),
                (
                    ACCESS_CONTROL_ALLOW_METHODS,
                    HeaderValue::from_static("GET, HEAD, OPTIONS"),
                ),
                (
                    ACCESS_CONTROL_ALLOW_HEADERS,
                    HeaderValue::from_static("Content-Type"),
                ),
                (ACCESS_CONTROL_MAX_AGE, HeaderValue::from_static("86400")),
                (VARY, HeaderValue::from_static("Origin")),
            ],
        )
            .into_response()
    } else {
        StatusCode::FORBIDDEN.into_response()
    }
}

/// Health check endpoint.
async fn health() -> &'static str {
    "OK"
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

/// Server entry point.
///
/// Registers three routes:
/// - `GET /` and `GET /resize` — image resize handler (with OPTIONS preflight).
/// - `GET /health` — plain-text health check returning `"OK"`.
///
/// Builds a shared `reqwest::Client` with a 30-second timeout and a 5-redirect
/// policy, then binds to `0.0.0.0:{PORT}` (default 8080).
#[tokio::main]
async fn main() {
    let config = Config::from_env();
    let port = config.port;

    let state = Arc::new(AppState {
        config,
        http: reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .redirect(reqwest::redirect::Policy::limited(5))
            .build()
            .expect("failed to build HTTP client"),
    });

    let app = Router::new()
        .route("/", get(handle_resize).options(handle_preflight))
        .route("/resize", get(handle_resize).options(handle_preflight))
        .route("/health", get(health))
        .with_state(state);

    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    println!("image-proxy-server listening on {addr}");

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .expect("failed to bind");

    axum::serve(listener, app).await.expect("server error");
}
