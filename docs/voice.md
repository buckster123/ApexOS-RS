# Voice — STT + TTS

> ApexOS-RS voice I/O: speech-in (whisper) and speech-out (Kokoro neural TTS via the
> `apex-tts` sidecar, with a graceful espeak-ng fallback). Server-side: the device's
> own mic and speakers, driven by agentd's gateway.

## At a glance

| Direction | Backend | How |
|-----------|---------|-----|
| **STT** (speech→text) | `whisper-cpp` binary | `arecord` → `whisper-cpp` (`/api/record/*`, `/api/transcribe`) |
| **TTS** (text→speech) | **Kokoro-82M** (neural) → espeak-ng | `/api/speak` → `apex-tts` sidecar → `aplay`; piper/espeak fallback |

Voice is **opt-in** (default off). Enable at install: the TUI add-on, a boot/USB
`APEXOS_VOICE=1`, or `--voice`. Disable with `--no-voice`. The choice persists in
`/etc/agentd/install.conf`, so `apexos-update` keeps it.

## Why Kokoro, why a sidecar

piper (the old default) is light but robotic. **Kokoro-82M** is near-studio quality,
runs CPU-only at realtime, and the int8 model is ~92 MB — good even on a Pi. The Rust
binding is [`tts-rs`](https://crates.io/crates/tts-rs) (MIT), which runs Kokoro's ONNX
through the `ort` crate.

The catch: `tts-rs` pins a **different, incompatible `ort` pre-release** than cerebro's
`fastembed` (bge-small / CLIP). `tts-rs` 2026.2.3 compiles only against `ort =2.0.0-rc.11`
(rc.10 has `Session::inputs` as a method-vs-field mismatch; rc.12 made `ort::Error`
generic and breaks it). cerebro's `fastembed` pins a *different* rc. Cargo resolves one
`ort` per binary/lock, so they can't share a workspace.

**Solution: `apex-tts` is a workspace-EXCLUDED crate** (`tools/crates/apex-tts`) with its
**own `Cargo.lock`** pinning `ort =2.0.0-rc.11`, fully decoupled from cerebro's `ort`.
The gateway talks to it over loopback HTTP. Two ONNX stacks, two locks, zero version war —
and cerebro stays untouched (frozen on `fastembed` 4). The committed sidecar lock freezes
the exact working rc, so neither side can drift the other.

## Components

```
/api/speak ──▶ apex-tts sidecar (127.0.0.1:8770) ──▶ Kokoro ONNX ──▶ WAV ──▶ aplay
   (gateway)        │  POST /synth {text,voice?} → 24kHz float WAV       (device speakers)
                    └─ unreachable / no model? → gateway falls back ▼
                                                   piper (if PIPER_MODEL) → espeak-ng
```

- **`tools/crates/apex-tts`** — a tiny resident `tiny_http` server. Loads the Kokoro model
  once into a `KokoroEngine` (behind a `std::Mutex`; tts-rs errors are `!Send`, so the
  server is synchronous), answers `POST /synth` with a 24 kHz float WAV.
- **`deploy/apex-tts.service`** — runs as the `agentd` user, loopback bind, sandboxed
  (`ProtectSystem=strict`, `ReadWritePaths=/var/lib/agentd/kokoro`). **Not** lifecycle-coupled
  to agentd — if it's down, `/api/speak` falls back, so coupling would only hurt.
- **gateway `speak_handler`** — POSTs to the sidecar; on any failure (connection refused on a
  voice-off node is ~instant) falls through to piper, then espeak-ng. No ort/tts-rs dep in agentd.

## Model files

`KOKORO_DIR` (default `/var/lib/agentd/kokoro`) must hold:
- an `*.onnx` Kokoro model (install.sh fetches the **int8**, ~92 MB),
- `voices-v1.0.bin` (~28 MB, 26 voices),
- optionally `config.json` (vocab; falls back to a hardcoded vocab).

install.sh downloads these from the [`thewh1teagle/kokoro-onnx`](https://github.com/thewh1teagle/kokoro-onnx)
`model-files-v1.0` release (override via `KOKORO_MODEL_URL` / `KOKORO_VOICES_URL`).

**espeak-ng is required** — Kokoro's phonemizer (apex-tts shells the `espeak-ng` binary),
and also the final `/api/speak` fallback. install.sh installs it when voice is enabled.

## Env vars

| Var | Default | Purpose |
|-----|---------|---------|
| `KOKORO_DIR` | `/var/lib/agentd/kokoro` | apex-tts: model dir (`*.onnx` + `voices-v1.0.bin`) |
| `APEX_TTS_ADDR` | `127.0.0.1:8770` | apex-tts: loopback bind |
| `APEX_TTS_URL` | `http://127.0.0.1:8770/synth` | gateway: where to reach the sidecar |
| `KOKORO_MODEL_URL` / `KOKORO_VOICES_URL` | thewh1teagle release | install.sh: model download sources |
| `PIPER_MODEL` | unset | gateway: legacy piper voice (2nd fallback, if set) |
| `WHISPER_MODEL` / `WHISPER_BIN` | `…/ggml-tiny.en.bin` / `whisper-cpp` | STT model + binary |

## Roadmap (this is slice 1)

All future slices converge on a tiered `AGENTD_VOICE_BACKEND=auto|local|api|off` selector
(mirroring `CEREBRO_VISION_BACKEND`):

1. ✅ **Kokoro local TTS** (this) — replace robotic piper, offline, the quality win.
2. **STT modernization** — `whisper-rs` / Candle Whisper as a crate (drop the external binary).
3. **API backend** — ElevenLabs Flash (75 ms, $50/M chars) / OpenAI TTS + Deepgram/OpenAI STT,
   for realtime studio quality on net-connected nodes.
4. **Tier selector** — `auto`: Nano → api/off, Micro+ → local Kokoro, Pro → GPU/api.
