# Voice — STT + TTS

> ApexOS-RS voice I/O: speech-in (whisper) and speech-out (Kokoro neural TTS via the
> `apex-tts` sidecar, with a graceful espeak-ng fallback). Server-side: the device's
> own mic and speakers, driven by agentd's gateway.

## At a glance

| Direction | Backend | How |
|-----------|---------|-----|
| **STT** (speech→text) | whisper (apex-stt) local *or* cloud (OpenAI / ElevenLabs Scribe) | UI records → `/api/transcribe` (client-side); `/api/record/*` (server-side); backend per `AGENTD_STT_BACKEND` |
| **TTS** (text→speech) | **Kokoro-82M** local *or* cloud API (ElevenLabs/OpenAI) → espeak-ng | UI plays `/api/tts` (client-side); `/api/speak` (server-side); backend per `AGENTD_VOICE_BACKEND` |

Voice is **opt-in** (default off). Enable at install: the TUI add-on, a boot/USB
`APEXOS_VOICE=1`, or `--voice`. Disable with `--no-voice`. The choice persists in
`/etc/agentd/install.conf`, so `apexos-update` keeps it. (Voice-enable installs the
local Kokoro path; the cloud API backends need only a key in `/etc/agentd/env` — no
build, so they work on a voice-off node too.)

## Where the audio I/O runs (server-side vs client-side)

This is the load-bearing distinction, learned the hard way on a desktop laptop:

- **Kiosk / headless:** agentd *is* the audio owner (no competing login session), so it
  can play/record directly — `/api/speak` runs `aplay` in agentd, `/api/record/*` runs
  `arecord`. **Server-side audio.**
- **Desktop (a personal laptop):** audio belongs to the **logged-in user's PipeWire
  session** (`/run/user/<uid>`, `0700`). agentd runs as the sandboxed `agentd` *system*
  user, which **cannot reach that session** — `aplay`/`arecord` as agentd hit a wall (raw
  ALSA = "device busy" because PipeWire holds the card; ALSA "default" routes through the
  user's PipeWire, unreachable). So server-side voice is silent/deaf on a desktop.

**Fix: client-side voice.** The UI (ui-slint) runs *in the user's session* (or as root on
the kiosk), so *it* can reach the audio. Both directions:
- **TTS:** the UI fetches synthesized audio from **`POST /api/tts`** (→ WAV bytes; same
  backend selection as `/api/speak` but returns the audio instead of playing it) and plays
  it locally (`aplay`), falling back to `/api/speak`.
- **STT:** the UI records the mic with a local **`arecord`** (16 kHz mono WAV, SIGINT on stop
  so the header finalizes cleanly) and POSTs the WAV to **`/api/transcribe`** (which runs the
  STT backend plan), instead of the server-side `/api/record/*`. Override the capture device
  with `ALSA_CAPTURE_DEVICE` (defaults to the session's ALSA "default").

This keeps **agentd fully sandboxed** (no security change) and works on desktop + (later)
web/PWA/phone. *Wake-word detection is still server-side (agentd listening), so it remains a
kiosk feature for now; the manual mic button is the desktop path.*

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

### STT backend (`AGENTD_STT_BACKEND`)

The STT side mirrors it: `auto|local|api|off`, resolved by the pure, unit-tested `stt_plan`.
`local` = the **apex-stt Whisper sidecar** (then a hand-installed whisper-cpp binary as a fallback);
`api` = cloud (OpenAI `/v1/audio/transcriptions` or ElevenLabs Scribe `/v1/speech-to-text`, both
multipart, both returning a `text` field); `auto` = local first, then api if a key is set. Unlike
TTS there's no trivial always-on fallback, so an empty/all-failed plan returns an honest error.
Provider = `AGENTD_STT_API` (`openai`|`elevenlabs`) or auto-by-key (**OpenAI preferred** — whisper
is the canonical STT). `/api/record/*` and `/api/transcribe` both route through the shared
`transcribe_wav` (which posts the 16 kHz mono WAV to apex-stt; if that's down it tries whisper-cpp,
then cloud).

**apex-stt** (`tools/crates/apex-stt`) is the STT twin of apex-tts: a workspace-excluded
`tiny_http` server that loads a Whisper ggml model once via [`whisper-rs`](https://crates.io/crates/whisper-rs)
(whisper.cpp; CPU by default, GPU features available for the Pro tier) and answers `POST /transcribe`
(a WAV body) with `{text}`. Excluded purely for build isolation — whisper.cpp has no `ort`, so no
version war (unlike the TTS sidecar). install.sh builds it + fetches `ggml-base.en.bin` (~148 MB,
override `WHISPER_GGML_URL`) into `/var/lib/agentd/whisper` when voice is enabled.

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
| `AGENTD_STT_BACKEND` | `auto` | gateway: STT backend `auto`\|`local`\|`api`\|`off` |
| `AGENTD_STT_API` | auto-by-key | gateway: cloud STT provider `openai`\|`elevenlabs` (OpenAI preferred) |
| `OPENAI_STT_MODEL` | `whisper-1` | gateway: OpenAI transcription model |
| `ELEVENLABS_STT_MODEL` | `scribe_v2` | gateway: ElevenLabs Scribe STT model |
| `APEX_STT_ADDR` | `127.0.0.1:8771` | apex-stt: loopback bind |
| `APEX_STT_URL` | `http://127.0.0.1:8771/transcribe` | gateway: where local STT reaches apex-stt |
| `WHISPER_GGML_URL` | ggerganov base.en | install.sh: Whisper ggml model download source |
| `WHISPER_LANG` | `en` | apex-stt: language hint |
| `AGENTD_VOICE_CONFIG` | `/var/lib/agentd/voice_config.json` | gateway: persisted live voice config (`/api/voice`) |
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
3. ✅ **STT selector + cloud STT** — `AGENTD_STT_BACKEND` + OpenAI / ElevenLabs Scribe.
4. ✅ **Local STT** — the `apex-stt` Whisper sidecar (whisper-rs), so `local` STT works out of
   the box (install.sh builds it + fetches the ggml model), no hand-installed whisper-cpp needed.
5. ◑ **Settings UI selector** — a native **VOICE** chip row (auto/local/api/off) in Settings,
   backed by `GET`/`POST /api/voice` (a process-global `VoiceConfig` seeded from env, retuned
   live, persisted to `AGENTD_VOICE_CONFIG`). One chip drives both TTS + STT; power users can
   split `tts_api`/`stt_api` via the endpoint. *Remaining:* install.sh ElevenLabs/OpenAI key
   prompting, GPU-feature `whisper-rs` builds (cuda/metal/vulkan) on the Pro tier, and a web-UI
   settings page.
6. ✅ **Client-side voice (native UI)** — TTS: `POST /api/tts` returns WAV bytes, ui-slint plays
   them locally (`aplay`). STT: ui-slint records the mic (`arecord`) → `/api/transcribe`. Both
   run in the user's session, so desktop voice works while agentd stays sandboxed. (Wake-word is
   still server-side → kiosk-only for now.)
7. ✅ **Web/PWA voice** — the `web/` client: a 🔊 toggle speaks replies (fetch `/api/tts` → play
   the WAV blob; works over plain HTTP) and a 🎤 button records (`MediaRecorder` webm →
   `/api/transcribe`). **The mic is gated on a secure context** — browsers only allow
   `getUserMedia` on **HTTPS or localhost**, so over `http://<LAN-IP>:8787` (the phone case) the
   mic button is hidden; TTS still works. Full phone mic needs the node served over TLS.
   *Remaining:* install.sh ElevenLabs/OpenAI key onboarding, GPU-feature `whisper-rs` builds, and
   (for phone mic) an HTTPS option.

## Runtime config (`/api/voice`)

The four selectors are **live-tunable** without a restart, mirroring `/api/cache` and
`/api/sensors/config`. `GET /api/voice` returns the current `{voice_backend, tts_api,
stt_backend, stt_api, has_elevenlabs, has_openai, backends}`; `POST` with any subset updates +
persists them. The env vars seed it on first run; the persisted file (`AGENTD_VOICE_CONFIG`,
default `/var/lib/agentd/voice_config.json`) then wins, so an operator's live choice survives
restart. `speak_handler` / `transcribe_wav` read this config, not the env directly.
