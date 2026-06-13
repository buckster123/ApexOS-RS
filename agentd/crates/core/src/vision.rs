//! Vision shim — the image-into-context gate.
//!
//! Every image that reaches the model passes through [`prepare_image`] first.
//! This is a **first-class hard constraint**, not an optimisation: the SensorHead
//! Sony cam at full resolution is ≈ 500k tokens/frame, and a single un-shrunk frame
//! would be a context bomb. The shim decodes any PNG/JPEG, downscales so the longest
//! edge is at most `VISION_MAX_EDGE` (default 1024 px — never upscales), re-encodes,
//! and base64-encodes the result, returning an estimate of the tokens it will cost.
//!
//! Anthropic charges roughly `width × height / 750` tokens per image, so a 1024 px
//! ceiling caps any frame at ~1400 tokens regardless of source resolution.
//!
//! ## Vision tool-result convention
//!
//! A tool returns an image to the model by shaping its `ToolOutput.content` as:
//! ```json
//! { "vision": { "path": "/abs/path.png" }, "text": "optional caption" }
//! ```
//! (`"b64"` + `"media_type"` may replace `"path"` for tools that don't share a
//! filesystem with agentd). The agent turn loop runs the bytes through the shim and
//! rewrites the tool-result into a multimodal content-block array via
//! [`anthropic_tool_result_content`]. See `agentd/crates/agent/src/turn.rs`.

use base64::engine::general_purpose::STANDARD;
use base64::Engine as _;
use serde_json::{json, Value};
use std::path::Path;

/// Default cap on an image's longest edge, in pixels, when `VISION_MAX_EDGE` is unset.
pub const DEFAULT_MAX_EDGE: u32 = 1024;

/// Anthropic's approximate image token cost is `width × height / TOKENS_PER_PIXEL_DIVISOR`.
const TOKEN_PIXEL_DIVISOR: u64 = 750;

/// A shimmed image, ready to drop into a model request.
#[derive(Debug, Clone)]
pub struct PreparedImage {
    /// MIME type of the re-encoded bytes (`image/jpeg` or `image/png`).
    pub media_type: String,
    /// Base64 of the re-encoded bytes (standard alphabet, padded).
    pub b64: String,
    /// Width after any downscale.
    pub width: u32,
    /// Height after any downscale.
    pub height: u32,
    /// Estimated token cost in the Anthropic vision pricing model.
    pub est_tokens: u32,
}

/// Read the per-frame edge cap from `VISION_MAX_EDGE`, clamped to a sane range.
pub fn max_edge_from_env() -> u32 {
    std::env::var("VISION_MAX_EDGE")
        .ok()
        .and_then(|s| s.trim().parse::<u32>().ok())
        .map(|n| n.clamp(128, 4096))
        .unwrap_or(DEFAULT_MAX_EDGE)
}

/// Decode → downscale (longest edge ≤ `VISION_MAX_EDGE`) → re-encode → base64.
pub fn prepare_image(bytes: &[u8]) -> anyhow::Result<PreparedImage> {
    prepare_image_with_max_edge(bytes, max_edge_from_env())
}

/// Like [`prepare_image`] but with an explicit edge cap — used by tests so they
/// don't depend on process-global env state.
pub fn prepare_image_with_max_edge(bytes: &[u8], max_edge: u32) -> anyhow::Result<PreparedImage> {
    let img = image::load_from_memory(bytes)
        .map_err(|e| anyhow::anyhow!("decode image: {e}"))?;

    // Downscale only — `resize` preserves aspect ratio and would *upscale* a small
    // image up to the bounds, so guard on the longest edge first.
    let img = if img.width().max(img.height()) > max_edge {
        img.resize(max_edge, max_edge, image::imageops::FilterType::Lanczos3)
    } else {
        img
    };

    let (width, height) = (img.width(), img.height());

    // JPEG (q85) is far smaller on the wire; keep PNG only when alpha matters, since
    // JPEG can't carry transparency. Token cost is dimension-driven, not byte-driven,
    // so the format choice is purely about payload size.
    let (encoded, media_type) = if img.color().has_alpha() {
        let mut out = Vec::new();
        img.write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png)
            .map_err(|e| anyhow::anyhow!("encode png: {e}"))?;
        (out, "image/png")
    } else {
        let mut out = Vec::new();
        let mut enc = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 85);
        enc.encode_image(&img.to_rgb8())
            .map_err(|e| anyhow::anyhow!("encode jpeg: {e}"))?;
        (out, "image/jpeg")
    };

    let est_tokens =
        ((width as u64 * height as u64).div_ceil(TOKEN_PIXEL_DIVISOR)).min(u32::MAX as u64) as u32;

    Ok(PreparedImage {
        media_type: media_type.to_string(),
        b64: STANDARD.encode(&encoded),
        width,
        height,
        est_tokens,
    })
}

/// Decode a base64 string and prepare it. For tools that hand back bytes inline
/// (no shared filesystem with agentd) rather than a path.
pub fn prepare_b64(data: &str) -> anyhow::Result<PreparedImage> {
    let bytes = STANDARD
        .decode(data.trim())
        .map_err(|e| anyhow::anyhow!("decode base64: {e}"))?;
    prepare_image(&bytes)
}

/// Read a file and prepare it. Convenience for tools that hand back a path.
pub fn load_and_prepare(path: impl AsRef<Path>) -> anyhow::Result<PreparedImage> {
    let path = path.as_ref();
    let bytes = std::fs::read(path)
        .map_err(|e| anyhow::anyhow!("read {}: {e}", path.display()))?;
    prepare_image(&bytes)
}

/// True when a value is a content-block array carrying at least one `image` block —
/// i.e. a multimodal tool-result the agent loop built via
/// [`anthropic_tool_result_content`]. Providers pass these through verbatim; every
/// other tool-result shape (including ordinary MCP text-block arrays) is stringified
/// exactly as before, so this is the *only* serialization behaviour that changes.
pub fn contains_image_block(v: &Value) -> bool {
    v.as_array().is_some_and(|a| {
        a.iter()
            .any(|e| e.get("type").and_then(|t| t.as_str()) == Some("image"))
    })
}

/// Build the Anthropic-native content for a vision tool-result: an array holding an
/// `image` block and (optionally) a trailing `text` caption. The same array shape is
/// what `Message`/`ContentBlock` already mirror, so providers pass it through verbatim.
pub fn anthropic_tool_result_content(prepared: &PreparedImage, caption: Option<&str>) -> Value {
    let mut blocks = vec![json!({
        "type": "image",
        "source": {
            "type": "base64",
            "media_type": prepared.media_type,
            "data": prepared.b64,
        }
    })];
    if let Some(text) = caption.filter(|s| !s.is_empty()) {
        blocks.push(json!({ "type": "text", "text": text }));
    }
    Value::Array(blocks)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn synth_png(w: u32, h: u32) -> Vec<u8> {
        let buf = image::RgbImage::from_fn(w, h, |x, y| {
            image::Rgb([(x % 256) as u8, (y % 256) as u8, 128])
        });
        let img = image::DynamicImage::ImageRgb8(buf);
        let mut bytes = Vec::new();
        img.write_to(&mut std::io::Cursor::new(&mut bytes), image::ImageFormat::Png)
            .unwrap();
        bytes
    }

    fn synth_rgba_png(w: u32, h: u32) -> Vec<u8> {
        let buf = image::RgbaImage::from_fn(w, h, |x, _| {
            image::Rgba([(x % 256) as u8, 64, 200, 128])
        });
        let img = image::DynamicImage::ImageRgba8(buf);
        let mut bytes = Vec::new();
        img.write_to(&mut std::io::Cursor::new(&mut bytes), image::ImageFormat::Png)
            .unwrap();
        bytes
    }

    fn decode_b64_dims(prepared: &PreparedImage) -> (u32, u32) {
        let raw = STANDARD.decode(&prepared.b64).expect("valid base64");
        let img = image::load_from_memory(&raw).expect("re-decodes");
        (img.width(), img.height())
    }

    #[test]
    fn large_image_is_capped_in_dims_and_tokens() {
        let p = prepare_image_with_max_edge(&synth_png(2400, 1800), 1024).unwrap();
        // 2400×1800 fits into 1024×1024 preserving 4:3 -> 1024×768.
        assert_eq!(p.width.max(p.height), 1024, "longest edge capped");
        assert_eq!((p.width, p.height), (1024, 768));
        assert!(p.est_tokens <= 1400, "token ceiling held: {}", p.est_tokens);
        assert_eq!(p.media_type, "image/jpeg", "opaque -> jpeg");
        assert_eq!(decode_b64_dims(&p), (1024, 768), "b64 round-trips");
    }

    #[test]
    fn small_image_passes_through_without_upscale() {
        let p = prepare_image_with_max_edge(&synth_png(320, 240), 1024).unwrap();
        assert_eq!((p.width, p.height), (320, 240), "never upscales");
        assert_eq!(p.est_tokens, ((320u64 * 240).div_ceil(750)) as u32);
    }

    #[test]
    fn alpha_image_stays_png() {
        let p = prepare_image_with_max_edge(&synth_rgba_png(200, 200), 1024).unwrap();
        assert_eq!(p.media_type, "image/png", "alpha -> png");
        assert_eq!((p.width, p.height), (200, 200));
    }

    #[test]
    fn garbage_bytes_error() {
        assert!(prepare_image_with_max_edge(b"definitely not an image", 1024).is_err());
    }

    #[test]
    fn env_cap_is_clamped() {
        // Out-of-range values clamp into [128, 4096]; unparseable -> default.
        std::env::set_var("VISION_MAX_EDGE", "999999");
        assert_eq!(max_edge_from_env(), 4096);
        std::env::set_var("VISION_MAX_EDGE", "10");
        assert_eq!(max_edge_from_env(), 128);
        std::env::remove_var("VISION_MAX_EDGE");
        assert_eq!(max_edge_from_env(), DEFAULT_MAX_EDGE);
    }

    #[test]
    fn tool_result_content_has_image_and_caption() {
        let p = prepare_image_with_max_edge(&synth_png(64, 64), 1024).unwrap();
        let v = anthropic_tool_result_content(&p, Some("a sketch"));
        let arr = v.as_array().unwrap();
        assert_eq!(arr[0]["type"], "image");
        assert_eq!(arr[0]["source"]["media_type"], "image/jpeg");
        assert_eq!(arr[1]["type"], "text");
        assert_eq!(arr[1]["text"], "a sketch");
        // No caption -> image only.
        let v2 = anthropic_tool_result_content(&p, None);
        assert_eq!(v2.as_array().unwrap().len(), 1);
    }

    #[test]
    fn contains_image_block_is_precise() {
        // Only an array with an actual image block counts.
        assert!(contains_image_block(&json!([
            { "type": "image", "source": { "type": "base64" } },
            { "type": "text", "text": "x" }
        ])));
        // Ordinary MCP text-block arrays must NOT (preserves existing stringify path).
        assert!(!contains_image_block(&json!([{ "type": "text", "text": "hi" }])));
        assert!(!contains_image_block(&json!("a string")));
        assert!(!contains_image_block(&json!({ "vision": {} })));
        assert!(!contains_image_block(&json!([1, 2, 3])));
    }
}
