//! Configuration parsed from environment variables.
//!
//! Cloudflare Worker build — reads from `worker::Env`.

use worker::Env;

/// Default maximum output width in pixels.
const DEFAULT_MAX_WIDTH: u32 = 4096;
/// Default maximum output height in pixels.
const DEFAULT_MAX_HEIGHT: u32 = 4096;
/// Default maximum source image size in megabytes.
const DEFAULT_MAX_SIZE_MB: u64 = 25;
/// Default cache TTL in seconds (90 days).
const DEFAULT_CACHE_TTL: u64 = 7_776_000;

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

/// Parse the `ALLOWED_DOMAINS` env var as a comma-separated, lowercased list.
/// Returns `None` if unset or empty (meaning all domains are allowed).
fn parse_domain_list(env: &Env) -> Option<Vec<String>> {
    env.var("ALLOWED_DOMAINS")
        .ok()
        .map(|v| v.to_string())
        .filter(|s| !s.is_empty())
        .map(|s| {
            s.split(',')
                .map(|d| d.trim().to_lowercase())
                .filter(|d| !d.is_empty())
                .collect()
        })
}
