# ApexOS-RS — Architecture

## The Core Insight: WS Renderer

ApexOS-RS does **not** modify `agentd`. It does not import agentd as a Rust library.

The original ApexOS CLAUDE.md states: *"any display (local KVM via cage, or network browser) is a thin stateless renderer."* ApexOS-RS is exactly that — a native Rust renderer that connects to the same agentd WebSocket as the browser does, consuming the same `Event` JSON stream and sending the same `Intent` JSON.

```
┌──────────────────────────── Raspberry Pi ────────────────────────────────┐
│                                                                            │
│   agentd (UNCHANGED) ──── ws://localhost:8787/ws ──────┬─→ Browser       │
│        │                                               │                  │
│     HTTP API                                     ui-slint binary          │
│  /api/speak                                   (Slint + KMS/DRM)           │
│  /api/record/start                                                         │
│  /api/transcribe                                                           │
│                                                                            │
└────────────────────────────────────────────────────────────────────────────┘
```

This means:
- `agentd` keeps its HTTP + WebSocket server regardless of which UI is in use
- Both UIs can coexist: browser for remote access, native Slint for the local display
- No Cargo dependency on agentd — avoids the binary-crate import problem
- Protocol is stable JSON; no shared types needed (though we can vendor `apexos-core` if we want)

## Thread Model

Slint **requires** the main thread for its event loop. Tokio's `#[tokio::main]` macro
hijacks the main thread for the async executor, creating a conflict. The fix:

```rust
fn main() {
    // Build tokio runtime on background threads
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;

    let ui = AppWindow::new()?;     // Slint init on main thread

    rt.spawn(async move { /* WS I/O */ });   // async work in background

    ui.run()?;   // blocks main thread — Slint owns it
}
```

Cross-thread UI updates use `slint::invoke_from_event_loop(|| { ui.set_foo(...) })`.
Weak handles (`ui.as_weak()`) allow safe cloning into async closures.

## KMS/DRM (no Wayland, no cage)

Set `SLINT_BACKEND=linuxkms` at runtime. Slint renders directly via:
- `/dev/dri/card0` — DRM modesetting (output to HDMI)
- `/dev/dri/renderD128` — GPU rendering (OpenGL ES / Vulkan)

The `agentd` user needs group membership: `render`, `video`, `input`.

This replaces the current `cage` + `seatd` + `agentos-kiosk` setup entirely.
No compositor, no seat manager, no Wayland protocol — direct kernel framebuffer.

### Pi 5 (BCM2712 / VideoCore VII)
Debian trixie ships the `v3d` open-source driver. `linuxkms` should work out of the box.
Verify with: `ls /dev/dri/` → expect `card0` and `renderD128`.

### Dev on desktop (x86)
`SLINT_BACKEND=winit` (default when `DISPLAY` or `WAYLAND_DISPLAY` is set).
No special config needed for local development.

## Protocol (agentd WebSocket)

Send on connect:
```json
{"type": "session_init"}
```
agentd responds with:
```json
{"type": "hello", "session_id": 42}
```

Key inbound events to handle:
```
agent_text      delta: string           — streaming agent output
turn_started                            — new agent turn begins
turn_complete                           — agent done
tool_requested  call_id, name, input    — tool call (render in UI)
tool_result     call_id, output         — tool response
approval_pending call_id, name          — show approval buttons
sensor_reading  variant, data           — IAQ / thermal frame
wake_triggered                          — wake word fired
```

Send user message:
```json
{"type": "user_prompt", "text": "hello"}
```

Send approval:
```json
{"type": "user_approval", "call_id": "abc", "approved": true}
```
