# ApexOS-RS — Build Roadmap

10 steps — **all shipped ✅** (live on the Pi 5 KMS/DRM kiosk; per-step gates in
CLAUDE.md's build-order table). Each was independently testable: steps 1-9 develop
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
