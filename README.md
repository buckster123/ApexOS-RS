<div align="center">

# ApexOS-RS

**Pure-Rust native UI distro of [ApexOS](https://github.com/buckster123/ApexOS)**

*Slint frontend · KMS/DRM direct rendering · No Chromium · No Wayland · ~10 MB RAM*

[![Status](https://img.shields.io/badge/status-planning-yellow?style=flat-square)]()
[![Rust](https://img.shields.io/badge/built_with-Rust-orange?style=flat-square)](https://www.rust-lang.org/)
[![UI](https://img.shields.io/badge/UI-Slint-blueviolet?style=flat-square)](https://slint.dev/)
[![Platform](https://img.shields.io/badge/platform-Pi_4_·_Pi_5_·_Zero_2W-red?style=flat-square)](https://www.raspberrypi.com/)

</div>

---

## What is this?

ApexOS-RS is the pure-Rust alternate distro of [ApexOS](https://github.com/buckster123/ApexOS).

It replaces the Chromium kiosk + Wayland compositor + HTML/JS frontend with a single native binary built with [Slint](https://slint.dev/). The UI renders directly to the framebuffer via Linux KMS/DRM — no cage, no seatd, no browser engine.

The `agentd` daemon is **unchanged**. ApexOS-RS is a thin WS renderer: it connects to the same `ws://localhost:8787/ws` endpoint as the browser, consumes the same Event stream, and sends the same Intent JSON.

```
┌──────────────────────── Raspberry Pi ──────────────────────────┐
│                                                                  │
│  agentd (unchanged) ──── ws://localhost:8787/ws                  │
│                                    │                             │
│                            apexos-rs-ui                         │
│                         (Slint + KMS/DRM)                       │
│                        renders to /dev/tty7                      │
│                                                                  │
└──────────────────────────────────────────────────────────────────┘
```

---

## Why?

| | ApexOS (original) | ApexOS-RS |
|--|--|--|
| UI runtime | Chromium | Slint native |
| UI memory | ~300 MB | ~10 MB |
| Startup | ~5s (cage + Chromium) | ~200ms |
| Display stack | cage → Wayland → Chromium | KMS/DRM direct |
| Target hardware | Pi 5 primary | Pi 4, Pi 5, Zero 2W |
| Language | Rust + HTML/JS | 100% Rust |

ApexOS-RS is for:
- **Lower-spec hardware** — Pi 4 (2/4GB), Pi Zero 2W, any board where 300MB for Chromium is too much
- **Faster bring-up** — sub-second from boot to UI
- **Embedded / industrial** — no browser attack surface, no Wayland compositor to crash
- **Pure-Rust credibility** — the whole stack in one language

Both distros share the same `agentd` backend. You choose your frontend.

---

## Hardware Compatibility

| Board | RAM | Status |
|-------|-----|--------|
| Raspberry Pi 5 (8GB / 4GB) | plenty | Primary target |
| Raspberry Pi 4 (4GB / 2GB) | comfortable | Supported |
| Raspberry Pi 4 (1GB) | tight (agentd + LLM calls are fine) | Likely works |
| Raspberry Pi Zero 2W | 512MB — stretch | Soft renderer, limited models |
| Other Linux/KMS boards | varies | Should work if v3d/vc4 present |

---

## Status

> **Pre-alpha — planning + scaffold stage.**
> The WS client skeleton is in place. Feature implementation begins at step 1 of the [build roadmap](docs/build-roadmap.md).

---

## Architecture

See [`docs/architecture.md`](docs/architecture.md) — covers the WS renderer pattern,
thread model (Slint main thread + tokio pool), KMS/DRM setup, and agentd protocol.

## Build Roadmap

10 steps, ~10-12 sessions to a fully functional native desktop:

| # | Feature |
|---|---------|
| 1 | WS skeleton — connects to agentd, session handshake |
| 2 | Agent chat — streaming text, dark theme, send input |
| 3 | Tool call blocks — collapsible cards, inline approval |
| 4 | Home dashboard — CPU/RAM/disk bars, IAQ badge |
| 5 | Sensor window — IAQ stats + thermal heatmap (custom painter) |
| 6 | Session management — init, picker, history replay |
| 7 | Voice controls — mic → `/api/record/start`, speaker → `/api/speak` |
| 8 | Settings — soul.md editor, policy mode, plugin list |
| 9 | Power modal + model/policy selectors |
| 10 | KMS/DRM deploy — `linuxkms` backend, systemd service, remove cage |

Post-v1: PTY terminal (alacritty_terminal), sub-agent windows, sketchpad.

Full detail: [`docs/build-roadmap.md`](docs/build-roadmap.md)

---

## Docs

| File | Contents |
|------|---------|
| [`docs/architecture.md`](docs/architecture.md) | WS renderer pattern, thread model, KMS/DRM |
| [`docs/build-roadmap.md`](docs/build-roadmap.md) | 10-step plan with per-step detail |
| [`docs/slint-notes.md`](docs/slint-notes.md) | Slint gotchas, Pi GPU setup, common errors |
| [`docs/porting-guide.md`](docs/porting-guide.md) | Feature map: current JS → Slint equivalents |

---

## Relationship to ApexOS

ApexOS-RS is a **fork/distro**, not a replacement. The original [ApexOS](https://github.com/buckster123/ApexOS)
stays Chromium-based — best for Pi 5, full feature set including Monaco IDE and iframe embeds.
ApexOS-RS optimises for footprint, hardware range, and a fully Rust stack.

---

## License

MIT
