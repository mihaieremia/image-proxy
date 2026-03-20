//! Configuration parsed from environment variables.
//!
//! Cloudflare Worker build — reads from `worker::Env`.

use worker::Env;

/// Default maximum output width in pixels.
const DEFAULT_MAX_WIDTH: u32 = 4096;
/// Default maximum output height in pixels.
const DEFAULT_MAX_HEIGHT: u32 = 4096;
/// Default maximum source image size in megabytes.
/// Lowered from 25 to 10 — a 25MB JPEG can decompress to >100MB raw pixels,
/// exceeding the 128MB WASM memory limit.
const DEFAULT_MAX_SIZE_MB: u64 = 10;
/// Default cache TTL in seconds (90 days).
const DEFAULT_CACHE_TTL: u64 = 7_776_000;
/// Default allowed origins when `ALLOWED_ORIGINS` env var is not set.
const DEFAULT_ALLOWED_ORIGINS: &[&str] = &["https://chartex.com", "https://www.chartex.com"];

/// All configurable limits and settings, parsed once per request from env vars.
///
/// Avoids repeated `env.var()` lookups during request processing.
pub struct Config {
    /// Maximum allowed output width in pixels.
    pub max_width: u32,
    /// Maximum allowed output height in pixels.
    pub max_height: u32,
    /// Maximum allowed source image body size in bytes.
    pub max_size_bytes: u64,
    /// Cache-Control max-age / s-maxage in seconds.
    pub cache_ttl: u64,
    /// Comma-separated list of allowed source image domains, or `None` for all.
    pub allowed_domains: Option<Vec<String>>,
    /// Referer header value sent to upstream CDNs.
    pub referer: String,
    /// Parsed list of origins allowed to call the proxy (from `ALLOWED_ORIGINS`).
    pub allowed_origins: Vec<String>,
}

impl Config {
    /// Parse all configuration from Cloudflare Worker environment variables.
    /// Falls back to compiled defaults for any missing or unparseable values.
    pub fn from_env(env: &Env) -> Self {
        let max_size_mb = env_parse(env, "MAX_SIZE_MB", DEFAULT_MAX_SIZE_MB);
        let referer = env
            .var("UPSTREAM_REFERER")
            .ok()
            .map(|v| v.to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "https://chartex.com/".into());

        Self {
            max_width: env_parse(env, "MAX_WIDTH", DEFAULT_MAX_WIDTH),
            max_height: env_parse(env, "MAX_HEIGHT", DEFAULT_MAX_HEIGHT),
            max_size_bytes: max_size_mb * 1024 * 1024,
            cache_ttl: env_parse(env, "CACHE_TTL", DEFAULT_CACHE_TTL),
            allowed_domains: parse_domain_list(env),
            referer,
            allowed_origins: parse_origin_list(env),
        }
    }
}

/// Generic env var parser: read a string, parse to `T`, fallback to `default`.
fn env_parse<T: std::str::FromStr>(env: &Env, key: &str, default: T) -> T {
    env.var(key)
        .ok()
        .and_then(|v| v.to_string().parse().ok())
        .unwrap_or(default)
}

/// Parse the `ALLOWED_ORIGINS` env var as a comma-separated list.
/// Falls back to `DEFAULT_ALLOWED_ORIGINS` if unset or empty.
fn parse_origin_list(env: &Env) -> Vec<String> {
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

/// Parse the `ALLOWED_DOMAINS` env var as a comma-separated, lowercased list.
/// Returns `None` if unset or empty (meaning all domains are allowed).
fn parse_domain_list(env: &Env) -> Option<Vec<String>> {
    env.var("ALLOWED_DOMAINS")
        .ok()
        .map(|v| v.to_string())
        .filter(|s| !s.is_empty())
        .map(|s| {
            s.split(',')
                .map(|d| d.trim().trim_start_matches('.').to_lowercase())
                .filter(|d| !d.is_empty())
                .collect()
        })
}
