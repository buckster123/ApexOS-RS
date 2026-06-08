# ApexOS-RS — Build Roadmap

10 steps. Each independently testable. Steps 1-9 can be developed and tested on
any Linux desktop with `SLINT_BACKEND=winit`; step 10 deploys to Pi with KMS/DRM.

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

**Total: ~10-12 sessions** to a fully functional native distro.

---

## Deferred (post-v1)

- **Interactive PTY terminal** — `alacritty_terminal` crate for VTE parsing; needs custom Slint
  widget for rendering glyph grid. Complex but achievable. Replaces xterm.js.
- **Code editor** — evaluate `slint-ui/slint-viewer` or embedded Monaco via a minimal
  embedded webview (webkit2gtk-rs). Or accept: editor opens in SSH session instead.
- **Sub-agent windows** — after core stable; each child session gets a Slint Popup/Dialog.
- **Sketchpad** — HTML5 canvas equivalent via Slint custom painter.

---

## Step 1 in detail: WS skeleton

Goal: binary compiles, connects to `ws://localhost:8787/ws`, session_init handshake,
inbound events logged to a Slint status label.

Files to create/edit:
- `ui-slint/src/main.rs` — runtime + WS loop (already scaffolded)
- `ui-slint/src/ui/appwindow.slint` — minimal window with status label

Test: `AGENTD_WS=ws://apexos.local:8787/ws cargo run` → window appears, status shows session ID.

## Step 2 in detail: Agent chat

Goal: agent text streams into a ScrollView; user can type a message and send it.

New `.slint` components:
- `ChatView` — ScrollView wrapping a VerticalBox of message bubbles
- `InputBar` — TextInput + send Button + mic button

Rust additions:
- `handle_event` dispatches `agent_text`, `turn_started`, `turn_complete`
- `send_message()` serialises `{"type":"user_prompt","text":"..."}` to WS

## Step 5 in detail: Thermal heatmap

Slint custom painter using `slint::Image::from_rgba8_premultiplied` or the
`RenderingHelper` trait. Receive `sensor_reading` events with `thermal_frame: [[f32; 32]; 24]`
from agentd, map to RGBA pixels (blue→red colormap), push to Slint `Image` property.
Matches what the current JS canvas wallpaper does.

## Step 10 in detail: KMS/DRM deploy

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
