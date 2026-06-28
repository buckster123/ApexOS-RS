# Voice — STT + TTS

> ApexOS-RS voice I/O: speech-in (whisper) and speech-out (Kokoro neural TTS via the
> `apex-tts` sidecar, with a graceful espeak-ng fallback). Server-side: the device's
> own mic and speakers, driven by agentd's gateway.

## At a glance

| Direction | Backend | How |
|-----------|---------|-----|
| **STT** (speech→text) | `whisper-cpp` binary | `arecord` → `whisper-cpp` (`/api/record/*`, `/api/transcribe`) |
| **TTS** (text→speech) | **Kokoro-82M** local *or* cloud API (ElevenLabs/OpenAI) → espeak-ng | `/api/speak`, backend per `AGENTD_VOICE_BACKEND` |

Voice is **opt-in** (default off). Enable at install: the TUI add-on, a boot/USB
`APEXOS_VOICE=1`, or `--voice`. Disable with `--no-voice`. The choice persists in
`/etc/agentd/install.conf`, so `apexos-update` keeps it. (Voice-enable installs the
local Kokoro path; the cloud API backends need only a key in `/etc/agentd/env` — no
build, so they work on a voice-off node too.)

## Backend selection (`AGENTD_VOICE_BACKEND`)

One knob picks the TTS path, mirroring `CEREBRO_VISION_BACKEND`:

| `AGENTD_VOICE_BACKEND` | Plan (tried in order, first that speaks wins) |
|------------------------|-----------------------------------------------|
| `auto` *(default)* | Kokoro local → cloud API → piper → espeak-ng |
| `local` | Kokoro local → piper → espeak-ng (never the paid API) |
| `api` | cloud API → espeak-ng (forces cloud) |
| `off` | silent |

`auto` deliberately prefers the **free local** voice — set `api` on a node where you
want cloud quality (e.g. the desktop). espeak-ng is always the final fallback, so a node
always talks. The plan resolver `tts_plan` is pure + unit-tested.

**Cloud provider** = `AGENTD_TTS_API` (`elevenlabs`|`openai`), or auto-picked by whichever
key is present (ElevenLabs preferred):
- **ElevenLabs** — `ELEVENLABS_API_KEY` (+ optional `ELEVENLABS_VOICE_ID`, `ELEVENLABS_MODEL`).
  Flash model, 75 ms, requests `pcm_24000` → `aplay`.
- **OpenAI** — `OPENAI_API_KEY` (a *real* api.openai.com key — the routing `OAI_API_KEY`
  may be OpenRouter, which doesn't serve `/v1/audio/speech`), `gpt-4o-mini-tts`, wav → `aplay`.

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
| `AGENTD_VOICE_BACKEND` | `auto` | gateway: `auto`\|`local`\|`api`\|`off` TTS backend |
| `AGENTD_TTS_API` | auto-by-key | gateway: `elevenlabs`\|`openai` cloud provider |
| `ELEVENLABS_API_KEY` | unset | gateway: ElevenLabs auth (enables the cloud API path) |
| `ELEVENLABS_VOICE_ID` / `ELEVENLABS_MODEL` | Rachel / `eleven_flash_v2_5` | gateway: ElevenLabs voice + model |
| `OPENAI_API_KEY` | unset | gateway: **real** api.openai.com key for OpenAI TTS |
| `OPENAI_TTS_MODEL` / `OPENAI_TTS_VOICE` | `gpt-4o-mini-tts` / `alloy` | gateway: OpenAI TTS model + voice |
| `KOKORO_DIR` | `/var/lib/agentd/kokoro` | apex-tts: model dir (`*.onnx` + `voices-v1.0.bin`) |
| `APEX_TTS_ADDR` | `127.0.0.1:8770` | apex-tts: loopback bind |
| `APEX_TTS_URL` | `http://127.0.0.1:8770/synth` | gateway: where to reach the sidecar |
| `KOKORO_MODEL_URL` / `KOKORO_VOICES_URL` | thewh1teagle release | install.sh: model download sources |
| `PIPER_MODEL` | unset | gateway: legacy piper voice (a fallback, if set) |
| `WHISPER_MODEL` / `WHISPER_BIN` | `…/ggml-tiny.en.bin` / `whisper-cpp` | STT model + binary |

## Roadmap

1. ✅ **Kokoro local TTS** — replace robotic piper, offline, the quality win (the `local` backend).
2. ✅ **TTS backend selector + cloud API** — `AGENTD_VOICE_BACKEND=auto|local|api|off` + ElevenLabs
   Flash / OpenAI TTS for realtime studio quality on net-connected nodes.
3. **STT modernization** — `whisper-rs` / Candle Whisper as a crate (drop the external binary),
   and a cloud STT option (Deepgram / OpenAI) under the same backend idea.
4. **UI + key onboarding** — a Settings backend selector (like PROMPT CACHE / SENSOR ALERTS),
   `GET`/`POST /api/voice`, and install.sh ElevenLabs/OpenAI key prompting.
