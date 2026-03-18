//! Error types for the image proxy.
//!
//! `ProxyError` is the single error type used across the entire pipeline.
//! Platform-specific response conversion is behind feature flags.

/// All error conditions that can occur during image proxy processing.
///
/// Each variant maps to an HTTP status code via `status_code()`.
/// The `Display` impl (via `thiserror`) produces the response body text.
#[non_exhaustive]
#[derive(Debug, thiserror::Error)]
pub enum ProxyError {
    /// No `url` query parameter was provided.
    #[error("Missing required 'url' parameter")]
    MissingUrl,

    /// A query parameter has an invalid value (bad dimensions, unknown fit mode, etc.).
    #[error("Invalid parameter: {0}")]
    InvalidParam(String),

    /// The source image URL's host is not in the `ALLOWED_DOMAINS` allowlist.
    #[error("Domain not allowed: {0}")]
    DomainNotAllowed(String),

    /// The request's `Origin`/`Referer` header doesn't match `ALLOWED_ORIGINS`.
    #[error("Origin not allowed: {0}")]
    OriginNotAllowed(String),

    /// The source URL points to a private/reserved IP range (SSRF protection).
    #[error("Blocked request to private/reserved IP")]
    SsrfBlocked,

    /// The source image's content-type is not in the allowed list.
    #[error("Invalid content type: {0}")]
    InvalidContentType(String),

    /// The source image exceeds the configured maximum size.
    #[error("Payload too large: {0:.1} MB (max {1} MB)")]
    TooLarge(f64, u64),

    /// The upstream image fetch failed (network error, non-2xx status, etc.).
    #[error("Fetch failed: {0}")]
    FetchFailed(String),

    /// The image could not be decoded (corrupt data, unsupported sub-format).
    #[error("Failed to decode image: {0}")]
    DecodeFailed(String),

    /// The image could not be encoded to the output format.
    #[error("Failed to encode image: {0}")]
    EncodeFailed(String),
}

// TODO: handler.rs constructs `EncodeFailed` directly in multiple places
// (lines ~208-223). Those call sites must be updated to pass the error
// message string now that `EncodeFailed(String)` carries a root cause.

impl ProxyError {
    /// Map each error variant to an HTTP status code.
    pub fn status_code(&self) -> u16 {
        match self {
            Self::MissingUrl | Self::InvalidParam(_) => 400,
            Self::DomainNotAllowed(_) | Self::OriginNotAllowed(_) | Self::SsrfBlocked => 403,
            Self::InvalidContentType(_) => 415,
            Self::TooLarge(_, _) => 413,
            Self::FetchFailed(_) => 502,
            Self::DecodeFailed(_) | Self::EncodeFailed(_) => 422,
            #[allow(unreachable_patterns)]
            _ => 500,
        }
    }
}

// --- Cloudflare Worker response conversion ---

#[cfg(feature = "cloudflare")]
impl ProxyError {
    /// Convert the error into a Cloudflare Worker `Response`.
    /// Returns plain text body with the appropriate HTTP status code.
    pub fn into_response(self) -> Result<worker::Response, worker::Error> {
        let status = self.status_code();
        let body = self.to_string();
        let headers = worker::Headers::new();
        headers.set("Content-Type", "text/plain; charset=utf-8")?;

        Ok(worker::Response::error(body, status)?.with_headers(headers))
    }
}
