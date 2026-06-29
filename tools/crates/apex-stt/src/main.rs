//! apex-stt — local Whisper speech-to-text sidecar for ApexOS-RS.
//!
//! Mirrors apex-tts: a tiny resident HTTP server that loads a Whisper ggml model once
//! and answers `POST /transcribe` (a 16 kHz mono WAV body) with `{"text": "..."}`.
//! Its own cargo workspace keeps the whisper.cpp C++ build off the main workspace.
//!
//! The gateway's `local` STT step posts here; if it's down / has no model, the gateway
//! falls through to a hand-installed whisper-cpp binary, then to cloud STT.
//!
//! Env:
//!   WHISPER_MODEL   ggml model path                 [/var/lib/agentd/whisper/ggml-base.en.bin]
//!   APEX_STT_ADDR   loopback bind                    [127.0.0.1:8771]
//!   WHISPER_LANG    language hint                     [en]

use std::io::Cursor;

use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters};

const DEFAULT_MODEL: &str = "/var/lib/agentd/whisper/ggml-base.en.bin";
const DEFAULT_ADDR: &str = "127.0.0.1:8771";

fn main() {
    env_logger::init();

    let model = std::env::var("WHISPER_MODEL").unwrap_or_else(|_| DEFAULT_MODEL.to_string());
    let addr = std::env::var("APEX_STT_ADDR").unwrap_or_else(|_| DEFAULT_ADDR.to_string());

    // Load the model once. Fail fast so systemd surfaces a missing/bad model.
    let ctx = match WhisperContext::new_with_params(&model, WhisperContextParameters::default()) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[apex-stt] failed to load Whisper model {model}: {e}");
            std::process::exit(1);
        }
    };

    let server = match tiny_http::Server::http(&addr) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("[apex-stt] failed to bind {addr}: {e}");
            std::process::exit(1);
        }
    };
    eprintln!("[apex-stt] Whisper model loaded from {model}; serving on http://{addr}/transcribe");

    for mut req in server.incoming_requests() {
        if req.method() != &tiny_http::Method::Post {
            let _ = req.respond(tiny_http::Response::empty(405));
            continue;
        }
        let mut body = Vec::new();
        if req.as_reader().read_to_end(&mut body).is_err() || body.is_empty() {
            let _ = req.respond(tiny_http::Response::empty(400));
            continue;
        }
        match transcribe(&ctx, &body) {
            Ok(text) => {
                let payload = serde_json::json!({ "text": text }).to_string();
                let header =
                    tiny_http::Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..])
                        .unwrap();
                let _ = req.respond(
                    tiny_http::Response::from_string(payload).with_header(header),
                );
            }
            Err(e) => {
                eprintln!("[apex-stt] transcribe failed: {e}");
                let _ = req.respond(tiny_http::Response::empty(500));
            }
        }
    }
}

/// Decode a 16 kHz mono WAV and run Whisper, returning the joined transcript.
fn transcribe(ctx: &WhisperContext, wav: &[u8]) -> Result<String, String> {
    let audio = wav_to_mono_f32(wav)?;
    if audio.is_empty() {
        return Ok(String::new());
    }

    let mut state = ctx.create_state().map_err(|e| format!("create_state: {e}"))?;
    let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
    let lang = std::env::var("WHISPER_LANG").unwrap_or_else(|_| "en".to_string());
    params.set_language(Some(&lang));
    params.set_print_special(false);
    params.set_print_progress(false);
    params.set_print_realtime(false);
    params.set_print_timestamps(false);

    state.full(params, &audio).map_err(|e| format!("whisper full: {e}"))?;

    let n = state.full_n_segments();
    let mut text = String::new();
    for i in 0..n {
        let Some(seg) = state.get_segment(i) else { continue };
        if let Ok(s) = seg.to_str_lossy() {
            let t = s.trim();
            if !t.is_empty() && t != "[BLANK_AUDIO]" {
                if !text.is_empty() {
                    text.push(' ');
                }
                text.push_str(t);
            }
        }
    }
    Ok(text)
}

/// Decode a WAV (expected 16 kHz mono S16) to f32 samples. Downmixes if stereo
/// sneaks in; does NOT resample (the gateway already emits 16 kHz mono).
fn wav_to_mono_f32(wav: &[u8]) -> Result<Vec<f32>, String> {
    let reader = hound::WavReader::new(Cursor::new(wav)).map_err(|e| format!("wav parse: {e}"))?;
    let spec = reader.spec();
    let channels = spec.channels.max(1) as usize;

    let samples: Vec<f32> = match spec.sample_format {
        hound::SampleFormat::Int => reader
            .into_samples::<i32>()
            .filter_map(Result::ok)
            .map(|s| {
                // Normalize by bit depth to [-1, 1].
                let max = (1i64 << (spec.bits_per_sample - 1)) as f32;
                s as f32 / max
            })
            .collect(),
        hound::SampleFormat::Float => {
            reader.into_samples::<f32>().filter_map(Result::ok).collect()
        }
    };

    if channels <= 1 {
        return Ok(samples);
    }
    // Downmix interleaved channels to mono.
    Ok(samples
        .chunks(channels)
        .map(|frame| frame.iter().sum::<f32>() / channels as f32)
        .collect())
}
