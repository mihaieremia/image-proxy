//! Image processing pipeline: decode → resize → encode.
//!
//! Shared between Cloudflare Worker and native server builds.
//! Pure Rust — no FFI, no platform-specific code.

use std::io::Cursor;

use image::codecs::jpeg::JpegEncoder;
use image::codecs::webp::WebPEncoder;
use image::imageops::FilterType;
use image::{DynamicImage, ExtendedColorType, ImageReader};

use crate::error::ProxyError;
use crate::params::{FitMode, ResizeParams};

/// Maximum total pixel count allowed for decode.
/// 16 million pixels ≈ 4096×4096. At 4 bytes/pixel (RGBA) = 64MB raw,
/// which leaves headroom for resize + encode within the 128MB WASM limit.
const MAX_PIXEL_COUNT: u64 = 16_000_000;

/// Output format decision based on input content-type and image properties.
///
/// Two-phase decision:
/// 1. `from_content_type` — initial format based on MIME type (before decode).
/// 2. `refine_after_decode` — adjusts based on actual pixel data (has alpha?).
///
/// The key optimisation: opaque PNGs are re-encoded as JPEG instead of WebP
/// lossless. JPEG encode is ~3-5x faster than the pure-Rust WebP encoder,
/// which matters under the Cloudflare Worker CPU time limit.
#[derive(Debug, Clone, Copy)]
pub enum OutputFormat {
    /// Re-encode as JPEG with quality control.
    Jpeg,
    /// Encode as lossless WebP (only for images with transparency).
    WebPLossless,
    /// Pass through unchanged (GIF animation, WebP without resize, oversized).
    Passthrough,
}


impl OutputFormat {
    /// Phase 1: choose initial format based on source content-type.
    ///
    /// Rules:
    /// - GIF → always passthrough (decoding destroys animation frames)
    /// - WebP + no resize → passthrough (already optimal)
    /// - WebP + resize → re-encode as lossless WebP (may have alpha)
    /// - JPEG → re-encode as JPEG (lossy with quality control)
    /// - PNG/other → tentatively WebP lossless (refined after decode)
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

    /// Phase 2: refine the format after decoding, based on actual pixel data.
    ///
    /// If the initial format is `WebPLossless` but the image has no alpha channel,
    /// switch to `Jpeg` — JPEG encode is ~3-5x faster and the output is comparable
    /// in size for opaque images (most PNGs from CDNs have no transparency).
    pub fn refine_after_decode(self, img: &DynamicImage) -> Self {
        match self {
            Self::WebPLossless if !has_meaningful_alpha(img) => Self::Jpeg,
            other => other,
        }
    }

    /// Returns the MIME type for the output format.
    pub fn content_type(&self) -> &'static str {
        match self {
            Self::Jpeg => "image/jpeg",
            Self::WebPLossless => "image/webp",
            Self::Passthrough => "application/octet-stream",
        }
    }
}

/// Check if an image actually uses its alpha channel.
///
/// For RGBA images, samples up to 1024 evenly-spaced pixels. If all sampled
/// pixels are fully opaque (alpha = 255), we treat the image as opaque.
/// This avoids the expensive WebP lossless encode for PNGs that have an
/// alpha channel but never use transparency (very common on the web).
fn has_meaningful_alpha(img: &DynamicImage) -> bool {
    match img {
        // These formats have no alpha channel at all
        DynamicImage::ImageRgb8(_) | DynamicImage::ImageLuma8(_) => false,
        // RGBA: sample pixels to check for actual transparency
        DynamicImage::ImageRgba8(rgba) => {
            let pixels = rgba.as_raw();
            let total_pixels = (rgba.width() as usize) * (rgba.height() as usize);
            if total_pixels == 0 {
                return false;
            }
            // Sample up to 1024 evenly-spaced pixels
            let step = (total_pixels / 1024).max(1);
            for i in (0..total_pixels).step_by(step) {
                if pixels[i * 4 + 3] != 255 {
                    return true;
                }
            }
            false
        }
        // LumaA: same sampling strategy
        DynamicImage::ImageLumaA8(la) => {
            let pixels = la.as_raw();
            let total_pixels = (la.width() as usize) * (la.height() as usize);
            if total_pixels == 0 {
                return false;
            }
            let step = (total_pixels / 1024).max(1);
            for i in (0..total_pixels).step_by(step) {
                if pixels[i * 2 + 1] != 255 {
                    return true;
                }
            }
            false
        }
        // 16-bit/32F variants: conservatively assume alpha is meaningful
        _ => true,
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
    pub fn output_content_type(&self, source_content_type: &'static str) -> &'static str {
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
            // Pre-flight: read dimensions from header only (~1ms) to check pixel budget.
            // If oversized, passthrough without decoding to avoid OOM.
            if let Ok(reader) = ImageReader::new(Cursor::new(&bytes))
                .with_guessed_format()
            {
                if let Ok((w, h)) = reader.into_dimensions() {
                    if w as u64 * h as u64 > MAX_PIXEL_COUNT {
                        return Ok(ProcessResult::Passthrough(bytes));
                    }
                }
            }

            let img = image::load_from_memory(&bytes).map_err(|e| ProxyError::DecodeFailed(e.to_string()))?;
            drop(bytes); // Free source bytes before resize allocation

            // Refine format based on actual pixel data (opaque PNG → JPEG)
            let format = format.refine_after_decode(&img);

            let img = resize(img, params);

            let encoded = match format {
                OutputFormat::Jpeg => encode_jpeg(img, params.quality)?,
                OutputFormat::WebPLossless => encode_webp_lossless(img)?,
                OutputFormat::Passthrough => unreachable!("Passthrough is handled above"),
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
///
/// Short-circuits when target dimensions match the source — avoids a full
/// pixel copy through the resize filter (saves ~20-30ms on large images).
fn resize_with_fit(
    img: DynamicImage,
    target_w: u32,
    target_h: u32,
    fit: FitMode,
    filter: FilterType,
) -> DynamicImage {
    let (orig_w, orig_h) = (img.width(), img.height());

    // No-op: target matches source — skip resize entirely
    if target_w == orig_w && target_h == orig_h {
        return img;
    }

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
