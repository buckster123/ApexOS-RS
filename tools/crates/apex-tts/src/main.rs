//! apex-tts — Kokoro-82M neural TTS sidecar for ApexOS-RS.
//!
//! A tiny resident HTTP server that loads the Kokoro ONNX model once and answers
//! `POST /synth` with a 24 kHz float WAV. It lives in its OWN cargo workspace so it
//! can pin `ort` to the rc tts-rs targets (rc.10), decoupled from cerebro's
//! fastembed→ort pin in the main workspace (see Cargo.toml).
//!
//! The gateway's `/api/speak` posts here; if this service is down or has no model,
//! the gateway falls back to piper/espeak-ng, so voice degrades gracefully.
//!
//! Env:
//!   KOKORO_DIR     model dir (holds `*.onnx` + `voices-v1.0.bin`)  [/var/lib/agentd/kokoro]
//!   APEX_TTS_ADDR  loopback bind                                    [127.0.0.1:8770]
//!
//! Requires `espeak-ng` on PATH (Kokoro's phonemizer).

use std::io::Cursor;
use std::path::PathBuf;
use std::sync::Mutex;

use tts_rs::engines::kokoro::KokoroEngine;
use tts_rs::{SynthesisEngine, SynthesisResult};

const DEFAULT_DIR: &str = "/var/lib/agentd/kokoro";
const DEFAULT_ADDR: &str = "127.0.0.1:8770";

fn main() {
    env_logger::init();

    let dir = std::env::var("KOKORO_DIR").unwrap_or_else(|_| DEFAULT_DIR.to_string());
    let addr = std::env::var("APEX_TTS_ADDR").unwrap_or_else(|_| DEFAULT_ADDR.to_string());

    // Load the model once. Fail fast (exit non-zero) so systemd surfaces a missing
    // model / bad dir instead of silently serving 500s.
    let mut engine = KokoroEngine::new();
    if let Err(e) = engine.load_model(&PathBuf::from(&dir)) {
        eprintln!("[apex-tts] failed to load Kokoro model from {dir}: {e}");
        eprintln!("[apex-tts] expected an .onnx + voices-v1.0.bin there (and espeak-ng on PATH)");
        std::process::exit(1);
    }
    let engine = Mutex::new(engine);

    let server = match tiny_http::Server::http(&addr) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("[apex-tts] failed to bind {addr}: {e}");
            std::process::exit(1);
        }
    };
    eprintln!("[apex-tts] Kokoro model loaded from {dir}; serving on http://{addr}/synth");

    for mut req in server.incoming_requests() {
        if req.method() != &tiny_http::Method::Post {
            let _ = req.respond(tiny_http::Response::empty(405));
            continue;
        }

        let mut body = String::new();
        if req.as_reader().read_to_string(&mut body).is_err() {
            let _ = req.respond(tiny_http::Response::empty(400));
            continue;
        }
        let Some((text, voice)) = parse_request(&body) else {
            let _ = req.respond(tiny_http::Response::empty(400));
            continue;
        };

        match synth_wav(&engine, &text, voice.as_deref()) {
            Some(bytes) => {
                let header =
                    tiny_http::Header::from_bytes(&b"Content-Type"[..], &b"audio/wav"[..]).unwrap();
                let _ = req.respond(tiny_http::Response::from_data(bytes).with_header(header));
            }
            None => {
                let _ = req.respond(tiny_http::Response::empty(500));
            }
        }
    }
}

/// Parse the request body into `(text, voice)`. Accepts a JSON object
/// `{"text": "...", "voice": "af_heart"}` or a bare plain-text body. Returns
/// `None` when there's no non-empty text.
fn parse_request(body: &str) -> Option<(String, Option<String>)> {
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(trimmed) {
        if v.is_object() {
            let text = v.get("text").and_then(|t| t.as_str()).unwrap_or("").trim();
            if text.is_empty() {
                return None;
            }
            let voice = v
                .get("voice")
                .and_then(|s| s.as_str())
                .map(str::to_string)
                .filter(|s| !s.trim().is_empty());
            return Some((text.to_string(), voice));
        }
    }
    // Fall back to treating the whole body as the text to speak.
    Some((trimmed.to_string(), None))
}

/// Synthesize `text` and encode a float WAV in memory. Returns `None` on failure
/// (the caller answers 500 and the gateway falls back to espeak-ng).
fn synth_wav(engine: &Mutex<KokoroEngine>, text: &str, voice: Option<&str>) -> Option<Vec<u8>> {
    let params = voice.map(|v| tts_rs::engines::kokoro::KokoroInferenceParams {
        voice: v.to_string(),
        ..Default::default()
    });

    let result = {
        let mut guard = match engine.lock() {
            Ok(g) => g,
            Err(_) => {
                eprintln!("[apex-tts] engine mutex poisoned");
                return None;
            }
        };
        match guard.synthesize(text, params) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("[apex-tts] synthesize failed: {e}");
                return None;
            }
        }
    };

    wav_bytes(&result)
}

/// Encode the f32 samples as a 32-bit float mono WAV in memory.
fn wav_bytes(result: &SynthesisResult) -> Option<Vec<u8>> {
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: result.sample_rate,
        bits_per_sample: 32,
        sample_format: hound::SampleFormat::Float,
    };
    let mut cursor = Cursor::new(Vec::<u8>::new());
    {
        let mut writer = hound::WavWriter::new(&mut cursor, spec).ok()?;
        for &sample in &result.samples {
            writer.write_sample(sample).ok()?;
        }
        writer.finalize().ok()?;
    }
    Some(cursor.into_inner())
}

#[cfg(test)]
mod tests {
    use super::parse_request;

    #[test]
    fn parse_request_handles_json_plain_and_empty() {
        // JSON with text + voice
        let (t, v) = parse_request(r#"{"text":"hello","voice":"bf_emma"}"#).unwrap();
        assert_eq!(t, "hello");
        assert_eq!(v.as_deref(), Some("bf_emma"));

        // JSON, text only → no voice
        let (t, v) = parse_request(r#"{"text":"hi there"}"#).unwrap();
        assert_eq!(t, "hi there");
        assert!(v.is_none());

        // Bare plain text
        let (t, v) = parse_request("just speak this").unwrap();
        assert_eq!(t, "just speak this");
        assert!(v.is_none());

        // Empty / whitespace / empty-text JSON → None
        assert!(parse_request("   ").is_none());
        assert!(parse_request(r#"{"text":"  "}"#).is_none());
        assert!(parse_request(r#"{"voice":"af_heart"}"#).is_none());
    }
}
