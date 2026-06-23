//! Tiered VLM client for the `describe_image` Tier-7 tool.
//!
//! One tool, two transports, three deployment tiers (André's steer so a node
//! with no local compute still gets eyes):
//!
//! | Tier | Hardware                  | Transport            |
//! |------|---------------------------|----------------------|
//! | a    | tiny VLM on the Pi        | Ollama @ localhost   |
//! | b    | the laptop over LAN       | Ollama @ a LAN URL   ← same transport as (a) |
//! | c    | external VLM API          | Anthropic (haiku)    |
//!
//! Tiers (a) and (b) are the **same** Ollama transport — point
//! `CEREBRO_VISION_URL` at a different node and the cluster's vision backend
//! hot-swaps with no code change, mirroring how inference backends move.
//!
//! Backend is selected by `CEREBRO_VISION_BACKEND` (`auto`|`ollama`|`anthropic`|
//! `off`). `auto` (default) prefers a reachable Ollama, falls back to Anthropic
//! when `ANTHROPIC_API_KEY` is set, else returns an honest "no backend" error.

use std::path::Path;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use base64::Engine;
use serde_json::json;

/// Default Ollama endpoint — covers Pi-local **and** a LAN vision node (just
/// change the URL).
const DEFAULT_OLLAMA_URL: &str = "http://localhost:11434";
/// Default Ollama vision model — moondream is tiny (~1.6B) and Pi-friendly.
const DEFAULT_OLLAMA_MODEL: &str = "moondream";
/// Cheapest Anthropic vision-capable model for captioning.
const ANTHROPIC_VISION_MODEL: &str = "claude-haiku-4-5";
/// Captioning instruction used when the caller passes none.
const DEFAULT_PROMPT: &str = "Describe this image in detail for a searchable memory. \
    Note any visible text, objects, people, and the overall scene.";

/// Which transport `describe` uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VisionBackend {
    /// Prefer a reachable Ollama, fall back to Anthropic.
    Auto,
    /// Local or LAN Ollama only.
    Ollama,
    /// External Anthropic VLM only.
    Anthropic,
    /// Vision disabled — `describe` returns an error.
    Off,
}

impl VisionBackend {
    fn parse(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "ollama" | "lan" | "local" => Self::Ollama,
            "anthropic" | "api" | "external" => Self::Anthropic,
            "off" | "none" | "0" | "false" | "disabled" => Self::Off,
            _ => Self::Auto,
        }
    }
}

/// Resolved vision configuration, read once per `describe_image` call.
#[derive(Debug, Clone)]
pub struct VisionConfig {
    pub backend: VisionBackend,
    /// Ollama base URL (no trailing slash). Point at a LAN node for tier (b).
    pub ollama_url: String,
    pub ollama_model: String,
    pub anthropic_key: Option<String>,
    pub anthropic_model: String,
}

impl VisionConfig {
    /// Read backend selection + endpoints from the environment.
    pub fn from_env() -> Self {
        let backend = std::env::var("CEREBRO_VISION_BACKEND")
            .map(|s| VisionBackend::parse(&s))
            .unwrap_or(VisionBackend::Auto);
        let ollama_url = std::env::var("CEREBRO_VISION_URL")
            .unwrap_or_else(|_| DEFAULT_OLLAMA_URL.to_string());
        let ollama_model = std::env::var("CEREBRO_VISION_MODEL")
            .unwrap_or_else(|_| DEFAULT_OLLAMA_MODEL.to_string());
        let anthropic_key = std::env::var("ANTHROPIC_API_KEY")
            .ok()
            .filter(|k| !k.trim().is_empty());
        Self {
            backend,
            ollama_url: ollama_url.trim_end_matches('/').to_string(),
            ollama_model,
            anthropic_key,
            anthropic_model: ANTHROPIC_VISION_MODEL.to_string(),
        }
    }
}

/// An image ready to send to a VLM: base64 payload + its media type.
#[derive(Debug, Clone)]
pub struct PreparedImage {
    pub b64: String,
    pub media_type: String,
}

impl PreparedImage {
    /// Decode the prepared base64 back to raw image bytes (for CLIP embedding).
    pub fn decoded(&self) -> Result<Vec<u8>> {
        base64::engine::general_purpose::STANDARD
            .decode(self.b64.trim())
            .map_err(|e| anyhow!("decode image b64: {e}"))
    }
}

/// Read an image off disk, sniff its media type, and base64-encode it.
pub fn prepare_from_path(path: &str) -> Result<PreparedImage> {
    let p = Path::new(path);
    let bytes = std::fs::read(p).with_context(|| format!("reading image {path}"))?;
    if bytes.is_empty() {
        return Err(anyhow!("image {path} is empty"));
    }
    let media_type = detect_media_type(&bytes, p);
    let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
    Ok(PreparedImage { b64, media_type })
}

/// Accept caller-supplied base64 (validating it decodes) with an optional
/// media-type hint; sniff the type from the bytes when none is given.
pub fn prepare_from_b64(b64: &str, media_type: Option<&str>) -> Result<PreparedImage> {
    let trimmed = b64.trim();
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(trimmed)
        .context("invalid base64 image data")?;
    if bytes.is_empty() {
        return Err(anyhow!("decoded image is empty"));
    }
    let media_type = media_type
        .map(|m| m.to_string())
        .filter(|m| !m.is_empty())
        .unwrap_or_else(|| detect_media_type(&bytes, Path::new("")));
    Ok(PreparedImage {
        b64: trimmed.to_string(),
        media_type,
    })
}

/// Detect an image media type from magic bytes, falling back to the file
/// extension, then to `image/png` (which every backend accepts).
fn detect_media_type(bytes: &[u8], path: &Path) -> String {
    if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        return "image/jpeg".into();
    }
    if bytes.starts_with(&[0x89, b'P', b'N', b'G']) {
        return "image/png".into();
    }
    if bytes.starts_with(b"GIF8") {
        return "image/gif".into();
    }
    if bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        return "image/webp".into();
    }
    match path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .as_deref()
    {
        Some("jpg") | Some("jpeg") => "image/jpeg".into(),
        Some("png") => "image/png".into(),
        Some("gif") => "image/gif".into(),
        Some("webp") => "image/webp".into(),
        _ => "image/png".into(),
    }
}

/// A produced caption plus provenance (which tier/model answered).
#[derive(Debug, Clone)]
pub struct Caption {
    pub text: String,
    pub backend: &'static str,
    pub model: String,
}

/// Describe `image`, routing to the configured backend. `prompt` overrides the
/// default captioning instruction when non-empty.
pub async fn describe(
    cfg: &VisionConfig,
    image: &PreparedImage,
    prompt: Option<&str>,
) -> Result<Caption> {
    let prompt = prompt
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .unwrap_or(DEFAULT_PROMPT);

    match cfg.backend {
        VisionBackend::Off => Err(anyhow!(
            "vision backend disabled (CEREBRO_VISION_BACKEND=off)"
        )),
        VisionBackend::Ollama => describe_ollama(cfg, image, prompt).await,
        VisionBackend::Anthropic => describe_anthropic(cfg, image, prompt).await,
        VisionBackend::Auto => match describe_ollama(cfg, image, prompt).await {
            Ok(c) => Ok(c),
            Err(ollama_err) => {
                if cfg.anthropic_key.is_some() {
                    describe_anthropic(cfg, image, prompt).await.map_err(|api_err| {
                        anyhow!("vision auto: ollama failed ({ollama_err}); anthropic failed ({api_err})")
                    })
                } else {
                    Err(anyhow!(
                        "vision auto: ollama unreachable ({ollama_err}) and no ANTHROPIC_API_KEY \
                         for fallback. Point CEREBRO_VISION_URL at a reachable Ollama (local or \
                         LAN), or set ANTHROPIC_API_KEY."
                    ))
                }
            }
        },
    }
}

/// Tier (a)/(b): an Ollama vision model over `/api/generate`.
async fn describe_ollama(
    cfg: &VisionConfig,
    image: &PreparedImage,
    prompt: &str,
) -> Result<Caption> {
    // Short connect timeout so `auto` fails fast to the Anthropic fallback when
    // no Ollama is running; generous overall timeout for slow Pi inference.
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(3))
        .timeout(Duration::from_secs(120))
        .build()?;
    let url = format!("{}/api/generate", cfg.ollama_url);
    let body = json!({
        "model": cfg.ollama_model,
        "prompt": prompt,
        "images": [image.b64],
        "stream": false,
    });
    let resp = client
        .post(&url)
        .json(&body)
        .send()
        .await
        .with_context(|| format!("POST {url}"))?;
    let status = resp.status();
    let data: serde_json::Value = resp.json().await.context("ollama response not JSON")?;
    if !status.is_success() {
        let msg = data["error"].as_str().unwrap_or("unknown error");
        return Err(anyhow!("ollama {status}: {msg}"));
    }
    let text = data["response"]
        .as_str()
        .ok_or_else(|| anyhow!("ollama: no 'response' field in {data}"))?
        .trim()
        .to_string();
    if text.is_empty() {
        return Err(anyhow!("ollama returned an empty caption"));
    }
    Ok(Caption {
        text,
        backend: "ollama",
        model: cfg.ollama_model.clone(),
    })
}

/// Tier (c): the Anthropic Messages API with a base64 image content block.
async fn describe_anthropic(
    cfg: &VisionConfig,
    image: &PreparedImage,
    prompt: &str,
) -> Result<Caption> {
    let key = cfg
        .anthropic_key
        .as_deref()
        .ok_or_else(|| anyhow!("anthropic vision: ANTHROPIC_API_KEY not set"))?;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(120))
        .build()?;
    let body = json!({
        "model": cfg.anthropic_model,
        "max_tokens": 1024,
        "messages": [{
            "role": "user",
            "content": [
                {"type": "image", "source": {
                    "type": "base64",
                    "media_type": image.media_type,
                    "data": image.b64,
                }},
                {"type": "text", "text": prompt},
            ],
        }],
    });
    let resp = client
        .post("https://api.anthropic.com/v1/messages")
        .header("x-api-key", key)
        .header("anthropic-version", "2023-06-01")
        .header("content-type", "application/json")
        .json(&body)
        .send()
        .await
        .context("POST anthropic messages")?;
    let status = resp.status();
    let data: serde_json::Value = resp.json().await.context("anthropic response not JSON")?;
    if !status.is_success() {
        let msg = data["error"]["message"].as_str().unwrap_or("unknown error");
        return Err(anyhow!("anthropic {status}: {msg}"));
    }
    let text = data["content"][0]["text"]
        .as_str()
        .ok_or_else(|| anyhow!("anthropic: unexpected response: {data}"))?
        .trim()
        .to_string();
    if text.is_empty() {
        return Err(anyhow!("anthropic returned an empty caption"));
    }
    Ok(Caption {
        text,
        backend: "anthropic",
        model: cfg.anthropic_model.clone(),
    })
}

// ── CLIP visual embeddings (search_vision) ──────────────────────────────────
// The recall half of the vision loop: CLIP's image + text towers map both into
// ONE shared 512-dim space (fastembed `ClipVitB32`), so a text query can rank
// stored images by visual content (text→image), and an image query finds similar
// images (image→image). Both towers lazy-load on first use (no cost until the
// agent actually does visual recall) and cache to fastembed's default dir. A load
// failure (offline first run, ONNX error) degrades to None — the Cortex then falls
// back to caption/FTS recall, never errors. Whether to use CLIP at all is the
// Cortex's call (tier-gated on text-embeddings being enabled); this module just
// supplies the vectors.

use std::sync::{Arc, OnceLock};

static CLIP_IMAGE: OnceLock<Option<Arc<fastembed::ImageEmbedding>>> = OnceLock::new();
static CLIP_TEXT:  OnceLock<Option<Arc<fastembed::TextEmbedding>>> = OnceLock::new();

fn clip_image_model() -> Option<Arc<fastembed::ImageEmbedding>> {
    CLIP_IMAGE.get_or_init(|| {
        use fastembed::{ImageEmbedding, ImageEmbeddingModel, ImageInitOptions};
        match ImageEmbedding::try_new(ImageInitOptions::new(ImageEmbeddingModel::ClipVitB32)) {
            Ok(m)  => { tracing::info!("CLIP image tower loaded (ClipVitB32, 512-dim)"); Some(Arc::new(m)) }
            Err(e) => { tracing::warn!("CLIP image tower load failed ({e}) — visual recall degrades to caption/FTS"); None }
        }
    }).clone()
}

fn clip_text_model() -> Option<Arc<fastembed::TextEmbedding>> {
    CLIP_TEXT.get_or_init(|| {
        use fastembed::{EmbeddingModel, InitOptions, TextEmbedding};
        match TextEmbedding::try_new(InitOptions::new(EmbeddingModel::ClipVitB32)) {
            Ok(m)  => { tracing::info!("CLIP text tower loaded (clip-ViT-B-32-text, 512-dim)"); Some(Arc::new(m)) }
            Err(e) => { tracing::warn!("CLIP text tower load failed ({e}) — text→image recall unavailable"); None }
        }
    }).clone()
}

/// Embed raw image bytes into the CLIP image space (512-dim). `Err` if the tower
/// can't load (offline / ONNX error) — caller decides the fallback.
pub async fn clip_embed_image(bytes: Vec<u8>) -> Result<Vec<f32>> {
    tokio::task::spawn_blocking(move || {
        let model = clip_image_model().ok_or_else(|| anyhow!("CLIP image tower unavailable"))?;
        let mut out = model.embed_bytes(&[bytes.as_slice()], None)?;
        if out.is_empty() { return Err(anyhow!("CLIP image embed produced no vector")); }
        Ok(out.remove(0))
    })
    .await?
}

/// Embed a text query into the CLIP text space (512-dim, shared with images).
pub async fn clip_embed_text(text: String) -> Result<Vec<f32>> {
    tokio::task::spawn_blocking(move || {
        let model = clip_text_model().ok_or_else(|| anyhow!("CLIP text tower unavailable"))?;
        let mut out = model.embed(vec![text], None)?;
        if out.is_empty() { return Err(anyhow!("CLIP text embed produced no vector")); }
        Ok(out.remove(0))
    })
    .await?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_backend_aliases() {
        assert_eq!(VisionBackend::parse("ollama"), VisionBackend::Ollama);
        assert_eq!(VisionBackend::parse("LAN"), VisionBackend::Ollama);
        assert_eq!(VisionBackend::parse("anthropic"), VisionBackend::Anthropic);
        assert_eq!(VisionBackend::parse("api"), VisionBackend::Anthropic);
        assert_eq!(VisionBackend::parse("off"), VisionBackend::Off);
        assert_eq!(VisionBackend::parse("disabled"), VisionBackend::Off);
        assert_eq!(VisionBackend::parse("whatever"), VisionBackend::Auto);
        assert_eq!(VisionBackend::parse(""), VisionBackend::Auto);
    }

    #[test]
    fn detects_media_type_from_magic_bytes() {
        assert_eq!(detect_media_type(&[0xFF, 0xD8, 0xFF, 0x00], Path::new("x")), "image/jpeg");
        assert_eq!(detect_media_type(&[0x89, b'P', b'N', b'G'], Path::new("x")), "image/png");
        assert_eq!(detect_media_type(b"GIF89a", Path::new("x")), "image/gif");
        let webp = b"RIFF\x00\x00\x00\x00WEBPVP8 ";
        assert_eq!(detect_media_type(webp, Path::new("x")), "image/webp");
    }

    #[test]
    fn falls_back_to_extension_then_png() {
        // Unknown magic, known extension.
        assert_eq!(detect_media_type(&[0, 1, 2, 3], Path::new("a.jpeg")), "image/jpeg");
        assert_eq!(detect_media_type(&[0, 1, 2, 3], Path::new("a.WEBP")), "image/webp");
        // Unknown everything → safe default.
        assert_eq!(detect_media_type(&[0, 1, 2, 3], Path::new("a.bin")), "image/png");
    }

    #[test]
    fn prepare_from_b64_roundtrips_and_sniffs() {
        // 1x1 PNG (magic bytes intact) → png sniffed from the decoded payload.
        let png = [0x89u8, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
        let b64 = base64::engine::general_purpose::STANDARD.encode(png);
        let img = prepare_from_b64(&b64, None).unwrap();
        assert_eq!(img.media_type, "image/png");
        assert_eq!(img.b64, b64);
        // Explicit media-type hint wins.
        let img = prepare_from_b64(&b64, Some("image/jpeg")).unwrap();
        assert_eq!(img.media_type, "image/jpeg");
    }

    #[test]
    fn prepare_from_b64_rejects_garbage() {
        assert!(prepare_from_b64("not valid base64!!!", None).is_err());
    }

    #[tokio::test]
    async fn describe_off_backend_errors() {
        let cfg = VisionConfig {
            backend: VisionBackend::Off,
            ollama_url: DEFAULT_OLLAMA_URL.into(),
            ollama_model: DEFAULT_OLLAMA_MODEL.into(),
            anthropic_key: None,
            anthropic_model: ANTHROPIC_VISION_MODEL.into(),
        };
        let img = PreparedImage { b64: "AAAA".into(), media_type: "image/png".into() };
        let err = describe(&cfg, &img, None).await.unwrap_err().to_string();
        assert!(err.contains("disabled"), "got: {err}");
    }

    #[tokio::test]
    async fn describe_anthropic_without_key_errors() {
        let cfg = VisionConfig {
            backend: VisionBackend::Anthropic,
            ollama_url: DEFAULT_OLLAMA_URL.into(),
            ollama_model: DEFAULT_OLLAMA_MODEL.into(),
            anthropic_key: None,
            anthropic_model: ANTHROPIC_VISION_MODEL.into(),
        };
        let img = PreparedImage { b64: "AAAA".into(), media_type: "image/png".into() };
        let err = describe(&cfg, &img, None).await.unwrap_err().to_string();
        assert!(err.contains("ANTHROPIC_API_KEY"), "got: {err}");
    }
}
