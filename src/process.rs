//! Image processing pipeline: decode → resize → encode.
//!
//! Shared between Cloudflare Worker and native server builds.
//! Pure Rust — no FFI, no platform-specific code.

use image::codecs::jpeg::JpegEncoder;
use image::codecs::webp::WebPEncoder;
use image::imageops::FilterType;
use image::{DynamicImage, ExtendedColorType};

use crate::error::ProxyError;
use crate::params::{FitMode, ResizeParams};

/// Output format decision based on input content-type.
///
/// The proxy doesn't force everything to WebP because `image-webp` 0.2.x
/// only supports lossless encoding — lossless WebP is often *larger* than
/// the JPEG input. Instead, we pick the best format per input type.
#[derive(Debug, Clone, Copy)]
pub enum OutputFormat {
    /// Re-encode as JPEG with quality control (for JPEG inputs).
    Jpeg,
    /// Encode as lossless WebP (for PNG/BMP inputs — usually smaller than source).
    WebPLossless,
    /// Pass through unchanged (for GIF animation, WebP without resize).
    Passthrough,
}

impl OutputFormat {
    /// Choose the optimal output format based on source content-type and
    /// whether any resize dimensions were requested.
    ///
    /// Rules:
    /// - GIF → always passthrough (decoding destroys animation frames)
    /// - WebP + no resize → passthrough (already optimal)
    /// - WebP + resize → re-encode as lossless WebP
    /// - JPEG → re-encode as JPEG (lossy with quality control)
    /// - PNG/other → lossless WebP (typically 30-50% smaller)
    pub fn from_content_type(content_type: &str, params: &ResizeParams) -> Self {
        if content_type.starts_with("image/gif") {
            return Self::Passthrough;
        }
        if content_type.starts_with("image/webp") && params.is_passthrough() {
            return Self::Passthrough;
        }
        if content_type.starts_with("image/jpeg") {
            return Self::Jpeg;
        }
        Self::WebPLossless
    }

    /// Returns the MIME type for the output format.
    /// Panics on `Passthrough` — callers must use the source content-type instead.
    pub fn content_type(&self) -> &'static str {
        match self {
            Self::Jpeg => "image/jpeg",
            Self::WebPLossless => "image/webp",
            Self::Passthrough => panic!("Passthrough has no content type — this is a bug"),
        }
    }
}

/// Result of the image processing pipeline.
pub enum ProcessResult {
    /// Image was decoded, resized, and re-encoded.
    Processed {
        bytes: Vec<u8>,
        format: OutputFormat,
    },
    /// Image was passed through unchanged (GIF, WebP without resize).
    Passthrough(Vec<u8>),
}

impl ProcessResult {
    /// Returns the output content-type. For passthrough, uses the source type.
    pub fn output_content_type<'a>(&self, source_content_type: &'a str) -> &'a str
    where
        Self: 'a,
    {
        match self {
            Self::Processed { format, .. } => format.content_type(),
            Self::Passthrough(_) => source_content_type,
        }
    }

    /// Consume the result and return the output bytes.
    pub fn into_bytes(self) -> Vec<u8> {
        match self {
            Self::Processed { bytes, .. } => bytes,
            Self::Passthrough(bytes) => bytes,
        }
    }

    /// Returns the output size in bytes without consuming the result.
    #[allow(clippy::len_without_is_empty)]
    pub fn len(&self) -> usize {
        match self {
            Self::Processed { bytes, .. } => bytes.len(),
            Self::Passthrough(bytes) => bytes.len(),
        }
    }
}

/// Main processing entry point: decode, resize, and encode an image.
///
/// Takes ownership of `bytes` so the source buffer can be freed immediately
/// after decoding, before the resize allocation happens.
///
/// # Errors
///
/// Returns [`ProxyError::DecodeFailed`] if the image bytes cannot be decoded
/// (corrupt data, unsupported sub-format, etc.).
///
/// Returns [`ProxyError::EncodeFailed`] if the resized image cannot be
/// encoded to the target output format (JPEG or lossless WebP).
pub fn process_image(
    bytes: Vec<u8>,
    params: &ResizeParams,
    format: OutputFormat,
) -> Result<ProcessResult, ProxyError> {
    match format {
        OutputFormat::Passthrough => Ok(ProcessResult::Passthrough(bytes)),
        _ => {
            let img = image::load_from_memory(&bytes).map_err(|e| ProxyError::DecodeFailed(e.to_string()))?;
            drop(bytes); // Free source bytes before resize allocation

            let img = resize(img, params);

            let encoded = match format {
                OutputFormat::Jpeg => encode_jpeg(img, params.quality)?,
                OutputFormat::WebPLossless => encode_webp_lossless(img)?,
                OutputFormat::Passthrough => panic!("Passthrough has no content type — this is a bug"),
            };

            Ok(ProcessResult::Processed {
                bytes: encoded,
                format,
            })
        }
    }
}

/// Apply resize/crop based on the requested dimensions and fit mode.
///
/// Uses CatmullRom (bicubic) filter — visually indistinguishable from
/// Lanczos3 for web images, but ~2x faster.
fn resize(img: DynamicImage, params: &ResizeParams) -> DynamicImage {
    let (orig_w, orig_h) = (img.width(), img.height());
    let filter = FilterType::CatmullRom;

    match (params.width, params.height) {
        (None, None) => img,
        (Some(w), None) => {
            // Width-only: compute height preserving aspect ratio
            let h = (orig_h as f64 * w as f64 / orig_w as f64).round() as u32;
            resize_with_fit(img, w, h.max(1), params.fit, filter)
        }
        (None, Some(h)) => {
            // Height-only: compute width preserving aspect ratio
            let w = (orig_w as f64 * h as f64 / orig_h as f64).round() as u32;
            resize_with_fit(img, w.max(1), h, params.fit, filter)
        }
        (Some(w), Some(h)) => resize_with_fit(img, w, h, params.fit, filter),
    }
}

/// Apply the specified fit mode to resize/crop the image to target dimensions.
fn resize_with_fit(
    img: DynamicImage,
    target_w: u32,
    target_h: u32,
    fit: FitMode,
    filter: FilterType,
) -> DynamicImage {
    let (orig_w, orig_h) = (img.width(), img.height());
    match fit {
        FitMode::ScaleDown => {
            // Only shrink, never upscale
            if target_w >= orig_w && target_h >= orig_h {
                img
            } else {
                img.resize(target_w, target_h, filter)
            }
        }
        FitMode::Contain => {
            // Fit within box, preserving aspect ratio (may upscale)
            img.resize(target_w, target_h, filter)
        }
        FitMode::Cover => {
            // Fill box exactly, cropping overflow
            img.resize_to_fill(target_w, target_h, filter)
        }
        FitMode::Crop => {
            // Center-crop without resizing
            let crop_x = orig_w.saturating_sub(target_w) / 2;
            let crop_y = orig_h.saturating_sub(target_h) / 2;
            let cw = target_w.min(orig_w);
            let ch = target_h.min(orig_h);
            img.crop_imm(crop_x, crop_y, cw, ch)
        }
    }
}

/// Encode a `DynamicImage` as JPEG with the specified quality (1-100).
///
/// Converts to RGB8 first (JPEG doesn't support alpha).
/// Pre-allocates the output buffer at ~12% of raw pixel size (typical for q80).
fn encode_jpeg(img: DynamicImage, quality: u8) -> Result<Vec<u8>, ProxyError> {
    let rgb = img.into_rgb8();
    let (w, h) = (rgb.width(), rgb.height());
    let data = rgb.into_raw();

    let estimated = w as usize * h as usize * 3 / 8;
    let mut buf = Vec::with_capacity(estimated);

    let mut encoder = JpegEncoder::new_with_quality(&mut buf, quality);
    encoder
        .encode(&data, w, h, ExtendedColorType::Rgb8)
        .map_err(|e| ProxyError::EncodeFailed(e.to_string()))?;

    Ok(buf)
}

/// Encode a `DynamicImage` as lossless WebP.
///
/// Matches on the `DynamicImage` variant to use `into_raw()` (zero-copy
/// buffer extraction) for common 8-bit color types. Falls back to
/// `into_rgba8()` for rare 16-bit/32F variants.
///
/// Pre-allocates the output buffer at ~50% of raw pixel size.
fn encode_webp_lossless(img: DynamicImage) -> Result<Vec<u8>, ProxyError> {
    let (data, w, h, color) = match img {
        DynamicImage::ImageLuma8(p) => {
            let (w, h) = (p.width(), p.height());
            (p.into_raw(), w, h, ExtendedColorType::L8)
        }
        DynamicImage::ImageLumaA8(p) => {
            let (w, h) = (p.width(), p.height());
            (p.into_raw(), w, h, ExtendedColorType::La8)
        }
        DynamicImage::ImageRgb8(p) => {
            let (w, h) = (p.width(), p.height());
            (p.into_raw(), w, h, ExtendedColorType::Rgb8)
        }
        DynamicImage::ImageRgba8(p) => {
            let (w, h) = (p.width(), p.height());
            (p.into_raw(), w, h, ExtendedColorType::Rgba8)
        }
        other => {
            // 16-bit/32F: consuming conversion to Rgba8
            let rgba = other.into_rgba8();
            let (w, h) = (rgba.width(), rgba.height());
            (rgba.into_raw(), w, h, ExtendedColorType::Rgba8)
        }
    };

    let bpp: usize = match color {
        ExtendedColorType::L8 => 1,
        ExtendedColorType::La8 => 2,
        ExtendedColorType::Rgb8 => 3,
        _ => 4,
    };
    let estimated = w as usize * h as usize * bpp / 2;
    let mut buf = Vec::with_capacity(estimated);

    let encoder = WebPEncoder::new_lossless(&mut buf);
    encoder
        .encode(&data, w, h, color)
        .map_err(|e| ProxyError::EncodeFailed(e.to_string()))?;

    Ok(buf)
}
