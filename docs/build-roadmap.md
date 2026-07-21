# ApexOS-RS — Build Roadmap

10 steps — **all shipped ✅** (live on the Pi 5 KMS/DRM kiosk; per-step gates in
this file — CLAUDE.md now carries only the summary). Each was independently testable: steps 1-9 develop
and test on any Linux desktop with `SLINT_BACKEND=winit`; step 10 deploys to Pi
with KMS/DRM.

---

| # | Step | Output | Est. effort |
|---|------|--------|-------------|
| 1 | **WS skeleton** | Connects to agentd, prints events as Slint label | 1 session |
| 2 | **Agent chat** | Streaming text view, dark theme, send input | 1 session |
| 3 | **Tool call blocks** | Collapsible tool call / result cards | 1 session |
| 4 | **Home dashboard** | CPU/RAM/disk bars, IAQ badge (polls `/api/run`) | 1 session |
| 5 | **Sensor window** | IAQ stats + thermal heatmap (custom Slint painter) | 1-2 sessions |
| 6 | **Session management** | Session init with ID, session picker (past sessions) | 1 session |
| 7 | **Voice controls** | Mic button → `/api/record/start`, speaker toggle → `/api/speak` | 1 session |
| 8 | **Settings** | Soul.md editor (TextEdit), policy mode, plugin list | 1 session |
| 9 | **Power + model/policy** | Power modal (reboot/shutdown), model/policy ComboBox | 1 session |
| 10 | **KMS/DRM deploy** | `SLINT_BACKEND=linuxkms` on Pi, systemd service, remove cage | 1 session |

**Total: ~10-12 sessions** to a fully functional native distro — *done; all 10 gates passed.*

Step 7 as-built drifted from the plan: voice I/O went **client-side** — the mic button
records via a local `arecord` and POSTs the WAV to `/api/transcribe` (replacing the
server-side `/api/record/*`; wake-word listening stays server-side), and replies play
locally from `/api/tts` WAV bytes, falling back to `/api/speak`. See `docs/voice.md`.

---

## Deferred (post-v1)

- ~~**Interactive PTY terminal**~~ — **shipped as a line-mode pane**, not the planned
  `alacritty_terminal` VTE: agentd gateway `/terminal-ws` (libc `openpty`) streams PTY
  bytes; ui-slint `terminal_view.slint` renders ANSI-stripped lines and writes submitted
  lines to PTY stdin. A full cursor-grid VTE (curses apps: htop/vim) is the part that
  remains deferred.
- **Code editor** — evaluate `slint-ui/slint-viewer` or embedded Monaco via a minimal
  embedded webview (webkit2gtk-rs). Or accept: editor opens in SSH session instead.
- **Sub-agent windows** — after core stable; each child session gets a Slint Popup/Dialog.
  (So far only a running-sub-agents taskbar badge + a Work Board "SUB" card exist — the per-child window is still open.)
- ~~**Sketchpad**~~ — **shipped, bidirectional**: `sketchpad_view.slint` Path-stroke canvas
  + gateway `POST /api/sketch` (tiny-skia raster) + the `sketch_snapshot` read tool + the
  `sketch_draw` write tool (APEX draws onto the same canvas, normalized 0–1 coords, no
  tool card — mirrors `display_face`).

---

## Step 1 in detail: WS skeleton — ✅ DONE

Goal: binary compiles, connects to `ws://localhost:8787/ws`, session_init handshake,
inbound events logged to a Slint status label.

Files to create/edit:
- `ui-slint/src/main.rs` — runtime + WS loop (already scaffolded)
- `ui-slint/src/ui/appwindow.slint` — minimal window with status label

Test: `AGENTD_WS=ws://apexos.local:8787/ws cargo run` → window appears, status shows session ID.

## Step 2 in detail: Agent chat — ✅ DONE

Goal: agent text streams into a ScrollView; user can type a message and send it.

New `.slint` components:
- `ChatView` — ScrollView wrapping a VerticalBox of message bubbles
- `InputBar` — TextInput + send Button + mic button

Rust additions:
- `dispatch_event` dispatches `agent_text`, `turn_started`, `turn_complete`
- `send_message()` serialises `{"type":"user_prompt","text":"..."}` to WS

As-built note: the Rust agentd never emits `turn_started` (Python agentd does) — the UI
lazily creates the agent bubble + sets busy on the first `agent_text` delta and keeps the
`turn_started` handler only for cross-compat.

## Step 5 in detail: Thermal heatmap — ✅ DONE (#105)

Shipped, with one design correction: the `sensor_reading`/`thermal_frame` WS events
deliberately carry only min/max/mean (kept small), so the full 32×24 grid rides an
**on-demand HTTP path** instead — gateway `GET /api/thermal/frame` proxies the
SensorHead dashboard's `/api/thermal/data` (768 floats), ui-slint polls it (adaptive
2s/30s), maps each cell through an **ironbow** colormap into a `SharedPixelBuffer<
Rgba8Pixel>` → `slint::Image::from_rgba8`, and renders it (`image-rendering: pixelated`)
in the SensorView. Live on apex1's MLX90640.

## Step 10 in detail: KMS/DRM deploy — ✅ DONE

```bash
sudo usermod -aG render,video,input agentd
# build on Pi:
cargo build --release
sudo cp target/release/ui-slint /usr/local/bin/apexos-rs-ui
sudo cp deploy/apexos-rs-ui.service /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl disable cage-kiosk   # remove old Wayland compositor
sudo systemctl enable --now apexos-rs-ui
```

Verify: `journalctl -u apexos-rs-ui -f` — should see Slint render to `/dev/tty7`.

---

# Deferred / post-v1 ledger

> Moved verbatim from CLAUDE.md (2026-07-21 docs refactor). Shipped items keep their
> strikethrough history here — it doubles as the "how did X land" index.


- ~~**Shader/3D face — embodiment Phase 2**~~ — **shipped & live on the Pi 5 V3D.** Phase 1 (emote *control*, #51): APEX drives its face via the `display_face` tool (12 expressions + gaze + intensity, held past turn-end — see the face-two-layers gotcha). Phase 2 (GL *render*) is **default on GL tiers** (#52 spike → 4 slices, #54–#58): renders in-window via `Window::set_rendering_notifier` + femtovg `GraphicsAPI::NativeOpenGL` + `glow` (no Skia, no 2nd window/process); auto-on wherever a real GL context exists, 2D fallback otherwise (Nano), `APEX_FACE_GL=0` to force 2D. The arc: **(1)** `glScissor` to the Face-window rect (published via the `FaceGl` global, sampled by a Timer); **(2)** emote uniforms mirrored from `FaceView` (GL + 2D can't drift); **(3)** raymarched-SDF head (lit ellipsoid + nose, ink features on the true 3D normal, head-turn on gaze); **(4)** promoted to default (auto-detect) + redraw gated on a visible Face window. Then three expressiveness rounds: **glossy catchlight eyes + shaped brows (angry-⋀/worried-⋁) + blush** (#59), **motion** — talking lip-flap, head-tilt, blink, idle saccades, sad tear (#60), and **facial muscles** — Duchenne smiling-eye crinkle, lower-lid squint, teeth/tongue in the open mouth, dark-accent ambient lift (#61). Verify any change via the snapshot server (`take_snapshot()` captures the GL overlay) — see the procedure in cerebro + docs/slint-notes.md. *Remaining flourishes (optional): ✨ sparkle for proud, eye-waggle micro-motions.*
- ~~PTY terminal~~ — shipped (libc `openpty`, `/terminal-ws` WebSocket endpoint in agentd gateway)
- ~~Sketchpad~~ — shipped, now **bidirectional** (`sketchpad_view`: `Path`-stroke canvas + `POST /api/sketch` tiny-skia raster + `sketch_snapshot` read tool + line/rect/ellipse shape tools). **Write-back: the `sketch_draw` tool** (apexos-tools, `allow`) lets APEX draw on the canvas — it mirrors `display_face` exactly (the UI consumes the `tool_requested` event directly, **no** tool card, no new agentd event). Coords are **normalized 0–1** (origin top-left → resolution-independent); ui-slint scales them to canvas px via a reported size (`SketchpadView.report-canvas` → `SKETCH_CANVAS`), appends to the **same** `SKETCH_DATA`/`SKETCH_STROKES` the user draws into (so the existing save path persists a **user+agent composite** PNG → `sketch_snapshot` sees it), and **reveals the Sketchpad window** so the human watches it appear. Args: `strokes:[{points|shape+from+to, color?, width?}]`, `clear?`. User draw/save path byte-identical (zero regression). **The UI is the single canvas authority — don't add gateway/Cerebro canvas state.** Live nodes need `sketch_draw = "allow"` in their `/etc/agentd/policy.toml` (config seeds fresh only).
- ~~Cerebro web UI integration~~ — shipped as the `Web` launcher (🌐): external-browser tiles for Cerebro/Sensor Head + open-any-URL bar (Slint can't embed a webview; opens via `xdg-open`/`$BROWSER`)
- Monaco / code editor — SSH/vim or embedded webkit2gtk webview for soul.md heavy editing
- Sub-agent windows — `Popup` per child session, maps to `SubAgentStarted` events
- ~~`apexos-core` vendor for shared `Event` types~~ — **DONE (both slices)**: wire-protocol types live in a lean serde-only `apexos-protocol` crate (`core` re-exports it, so `apexos_core::Event` is unchanged daemon-side). The UI now deserializes WS frames into the typed `Event` (`serde_json::from_value::<Event>` → `match event { … }`) instead of `["field"].as_str()` string-matching, and **logs** an undecodable frame instead of silently dropping it. Outbound frontend-intent frames (`user_prompt`/`user_approval`/`user_cancel`) stay hand-built JSON on purpose — they omit `session` (the gateway injects it), which the required-`session` `Event` variants can't express
- ~~Vision input — core eyes~~ — shipped: the downscale **shim** (`apexos_core::vision`, `VISION_MAX_EDGE` cap = the SensorHead token-bomb guard) + the **vision tool-result path** (a tool returns `{"vision":{"path"|"b64"},"text"}` → `turn.rs::vision_rewrite` shims it → multimodal content block; Anthropic native, OAI/Ollama follow-up user msg). `sketch_snapshot` now hands APEX the drawing inline. Remaining vision follow-ups still deferred:
  - ~~**Screenshot "mirror" tool**~~ — shipped: `screenshot_mirror` (apexos-tools) → ui-slint serves its own `Window::take_snapshot()` PNG over a loopback endpoint (renderer-agnostic — winit/femtovg, linuxkms/skia, femtovg-software all snapshot the rendered scene, so **no** DRM readback and **no** Wayland screencopy) → tool writes it under the workspace and returns the same `{vision:{path}}` sentinel, zero agentd changes. Graceful "no display" when headless.
  - ~~**Camera eyes — physical-world capture**~~ — shipped: the `camera_capture` tool (apexos-tools) snaps one frame and returns the same `{vision:{path}}` sentinel — zero agentd changes, mirrors `screenshot_mirror`. Device-agnostic backend pick (the capture half of HW-tier detection): Pi CSI camera (`rpicam-jpeg`/`libcamera-jpeg`) → USB/laptop webcam over V4L2 (`ffmpeg -f v4l2`) → `fswebcam`; warmup frames per backend (no black first frame); `APEXOS_CAMERA_DEVICE`/`APEXOS_CAMERA_CMD` overrides; graceful "no camera" note. The PWA's `GET /api/snapshot` was generalized to the **same** multi-backend detection (was Pi-CSI-only) so laptop/USB-cam nodes work too. install.sh adds `rpicam-apps` on Pi (ffmpeg covers V4L2) and grants the `video` group to agentd even headless.
  - **User-attached images** — *plumbing shipped*: first-class `ContentBlock::Image` + `UserPrompt.images`, folded in `state`/router, serialized by both providers (Anthropic `image` / OpenAI `image_url`), gateway shims raw `path`|`b64` refs via `vision::prepare` on the WS `user_prompt` frame and at `POST /api/sessions/{id}/image`. Remaining surface: an **external-PWA upload/camera button** (`mobile.html` lives outside this repo) — the native Slint workspace image picker shipped (#31).
  - ~~**cerebro `describe_image`**~~ — shipped: a real VLM caption tool (`cerebro::vision`) with a **tiered backend** (`CEREBRO_VISION_BACKEND` = `auto`|`ollama`|`anthropic`|`off`). Two transports cover three tiers — a local/LAN **Ollama** VLM (`CEREBRO_VISION_URL`, default `localhost:11434`; `CEREBRO_VISION_MODEL`, default `moondream` — point the URL at a LAN node to hot-swap the cluster's vision backend) and the **Anthropic** API fallback (`claude-haiku-4-5`, needs `ANTHROPIC_API_KEY`). `auto` prefers a reachable Ollama, falls back to Anthropic, else errors honestly. Takes a workspace `path` or inline `b64`(+`media_type`); `remember:true` folds the caption into memory (tagged `vision`), closing the vision→memory loop. **cerebro `search_vision`** — ~~stubbed~~ **shipped**: CLIP visual recall (the read half). fastembed's `ClipVitB32` image+text towers map both into ONE shared 512-dim space (`cerebro::vision::clip_embed_image`/`clip_embed_text`, lazy-loaded), so a `query` text ranks stored images by visual content (text→image) and a `path`/`b64` image finds visually-similar ones (image→image). Vectors live in a plain `vision_embeddings` table (memory_id→512-dim blob + image_path), brute-force-cosine-ranked in Rust (`VectorStore::vision_search`; image counts are modest — no vec0). `describe_image{remember}` now ALSO indexes the image (`Cortex::index_image`), so the loop is store→recall. **Tier-gated** (`Cortex::vision_embed`, `vision_embed_enabled`): default follows text-embeddings — on for Micro+ (`CEREBRO_EMBED_MODEL` set), **off for Nano** (the ~350MB CLIP model stays off the smallest boards), env `CEREBRO_VISION_EMBED=off`/`on` overrides; the model lazy-loads on first visual recall, so an enabled-but-unused node pays nothing. CLIP-off / no-images-yet falls back to caption/FTS recall over `vision`-tagged memories (text query only). Scope-filtered via `get_memories_by_ids`; `cosine`/store/search unit-tested with fake vectors (CLIP itself needs the model). **Don't couple the `vision_embeddings` store to a specific embedder — it persists+ranks 512-dim vectors the `cerebro::vision` towers supply.**

---

