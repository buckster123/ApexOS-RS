# README assets

Drop the banner + screenshots here; the root `README.md` references these exact paths.

## Banner — `assets/banner.png`

Wide hero image, **~1600×420** (≈3.8:1). Abstract/decorative, dark, modern. Image-gen prompt:

> A wide, dark, abstract tech banner for a self-evolving AI operating system. Centerpiece: a glowing **ouroboros** — a serpent of luminous circuit-traces and flowing code eating its own tail — coiled around a small single-board computer that radiates a soft neural/memory glow. Around it, faint constellation-like **mesh nodes** connected by thin light-threads, and delicate silicon die patterns dissolving into stardust at the edges. Palette: deep space near-black background (#0b1020) with neon **violet (#8b5cf6)**, warm **Rust orange (#f97316)**, and **teal (#06b6d4)** accents; subtle crimson (#e11d48) spark where the tail meets the mouth. Volumetric glow, clean negative space for a title, no text, no logos, painterly-meets-technical, high detail, cinematic, 16:4 ultrawide.

Keep a clear, calm region (center or left) so the overlaid title reads well. Export `banner.png` (PNG, ≤~600 KB ideally).

## Screenshots — `assets/screenshots/`

The gallery in `README.md` references these filenames (capture at a clean, legible resolution; PNG):

| File | Shot | How to capture |
|------|------|----------------|
| `chat.png` | agent chat mid-turn with a **tool card** (ideally one awaiting approval) | desktop `winit` build, or apex2 kiosk |
| `face.png` | the **GL face** showing a clear emote (happy/curious/proud) | `APEX_FACE_AUTOOPEN=1 APEX_FACE_STATE=happy`, or live during a turn |
| `sensors.png` | the **Sensor window** — IAQ history + MLX90640 **thermal heatmap** | apex1/apex2 with the sensor head live |
| `dashboard.png` | the **Home dashboard** — CPU/RAM/disk bars + air-quality badge | any node |

Capture options:
- **Desktop (winit):** run `cargo run -p ui-slint` against a live agentd (`AGENTD_WS=…`), screenshot the window.
- **On a Pi:** the ui-slint **snapshot server** serves a PNG of the live screen — `curl http://127.0.0.1:8788/snapshot -o shot.png` (loopback; tunnel over SSH if remote).
- Crop/downscale to a consistent width (~840 px works well for the 2-up table).

Same images can feed the landing site gallery later.
