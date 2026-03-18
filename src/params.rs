//! Query parameter parsing and cache key generation.
//!
//! Shared between Cloudflare Worker and native server builds.

use std::fmt;

use crate::error::ProxyError;

/// Default JPEG quality when `q` param is not specified.
const DEFAULT_QUALITY: u8 = 80;

/// Resize strategy — how the image fits within the target dimensions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FitMode {
    /// Shrink to fit within target dimensions; never upscale.
    ScaleDown,
    /// Fill target dimensions exactly, cropping overflow (resize + center-crop).
    Cover,
    /// Fit within target dimensions, preserving aspect ratio (may upscale).
    Contain,
    /// Center-crop to exact target dimensions without resizing.
    Crop,
}

impl std::str::FromStr for FitMode {
    type Err = ProxyError;

    /// Parse a fit mode from a query string value.
    /// Accepts multiple common formats: `scale-down`, `scaledown`, `scale_down`.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "scale-down" | "scaledown" | "scale_down" => Ok(Self::ScaleDown),
            "cover" => Ok(Self::Cover),
            "contain" => Ok(Self::Contain),
            "crop" => Ok(Self::Crop),
            other => Err(ProxyError::InvalidParam(format!(
                "unknown fit mode: '{other}' (expected: scale-down, cover, contain, crop)"
            ))),
        }
    }
}

/// Stable string representation for cache keys.
/// Uses kebab-case (not Rust Debug formatting) so cache keys survive enum renames.
impl fmt::Display for FitMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ScaleDown => write!(f, "scale-down"),
            Self::Cover => write!(f, "cover"),
            Self::Contain => write!(f, "contain"),
            Self::Crop => write!(f, "crop"),
        }
    }
}

/// Validated and parsed resize parameters extracted from the request URL.
pub struct ResizeParams {
    /// Source image URL (percent-decoded from the `url` query param).
    pub url: String,
    /// Target width in pixels (optional, 1..=max_width).
    pub width: Option<u32>,
    /// Target height in pixels (optional, 1..=max_height).
    pub height: Option<u32>,
    /// JPEG output quality (1-100). Only affects JPEG→JPEG encoding.
    pub quality: u8,
    /// Resize strategy.
    pub fit: FitMode,
}

impl ResizeParams {
    /// Parse resize parameters from the proxy request URL's query string.
    ///
    /// Extracts `url`, `w`/`width`, `h`/`height`, `q`/`quality`, and `fit`.
    /// Validates dimensions against the provided maximums.
    /// The source `url` parameter must be percent-encoded by the caller.
    pub fn from_url(url: &url::Url, max_width: u32, max_height: u32) -> Result<Self, ProxyError> {
        let pairs: Vec<(String, String)> = url
            .query_pairs()
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();

        // query_pairs() already percent-decodes — no extra decode needed
        let source_url = pairs
            .iter()
            .find(|(k, _)| k == "url")
            .map(|(_, v)| v.clone())
            .ok_or(ProxyError::MissingUrl)?;

        let width = parse_optional_u32(&pairs, "w", "width")?;
        let height = parse_optional_u32(&pairs, "h", "height")?;

        if let Some(w) = width {
            if w == 0 || w > max_width {
                return Err(ProxyError::InvalidParam(format!(
                    "width must be between 1 and {max_width}"
                )));
            }
        }
        if let Some(h) = height {
            if h == 0 || h > max_height {
                return Err(ProxyError::InvalidParam(format!(
                    "height must be between 1 and {max_height}"
                )));
            }
        }

        let quality = parse_optional_u32(&pairs, "q", "quality")?
            .map(|q| q.min(100) as u8)
            .unwrap_or(DEFAULT_QUALITY);

        let fit = pairs
            .iter()
            .find(|(k, _)| k == "fit")
            .map(|(_, v)| v.parse::<FitMode>())
            .transpose()?
            .unwrap_or(FitMode::ScaleDown);

        Ok(Self {
            url: source_url,
            width,
            height,
            quality,
            fit,
        })
    }

    /// Returns true if no resize dimensions were requested.
    /// Used to decide whether to passthrough WebP/GIF images unchanged.
    pub fn is_passthrough(&self) -> bool {
        self.width.is_none() && self.height.is_none()
    }

    /// Build a canonical, order-independent cache key.
    ///
    /// Must be a fully-qualified URL — Cloudflare Cache API requirement.
    /// The source URL is stripped to scheme + host + path only (query params
    /// like auth tokens and signatures are excluded) so rotating signed URLs
    /// for the same image hit a single cache entry.
    pub fn cache_key(&self) -> String {
        let base_url = strip_query(&self.url);
        let mut key = format!("https://image-proxy.internal/v1?url={base_url}");
        if let Some(w) = self.width {
            key.push_str(&format!("&w={w}"));
        }
        if let Some(h) = self.height {
            key.push_str(&format!("&h={h}"));
        }
        key.push_str(&format!("&q={}", self.quality));
        key.push_str(&format!("&fit={}", self.fit));
        key
    }
}

/// Strip query string and fragment from a URL, keeping scheme + host + path.
/// Falls back to the original string if parsing fails.
fn strip_query(url: &str) -> String {
    match url::Url::parse(url) {
        Ok(mut parsed) => {
            parsed.set_query(None);
            parsed.set_fragment(None);
            parsed.to_string()
        }
        Err(_) => url.to_string(),
    }
}

/// Parse an optional `u32` query parameter, trying both short and long names.
/// For example, `w` (short) and `width` (long) for the width parameter.
fn parse_optional_u32(
    pairs: &[(String, String)],
    short: &str,
    long: &str,
) -> Result<Option<u32>, ProxyError> {
    pairs
        .iter()
        .find(|(k, _)| k == short || k == long)
        .map(|(_, v)| {
            v.parse::<u32>().map_err(|_| {
                ProxyError::InvalidParam(format!("'{long}' must be a positive integer"))
            })
        })
        .transpose()
}
