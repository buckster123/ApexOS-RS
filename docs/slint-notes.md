# Slint Developer Notes

Footguns, patterns, and Pi-specific setup for the ApexOS-RS codebase.

> **Exact widget/element API:** see [`slint-reference/`](slint-reference/README.md) —
> std-widgets + element fundamentals vendored verbatim from the official Slint
> repo. Consult it instead of recalling Slint syntax from memory.

---

## Main Thread Ownership

Slint's event loop **must** run on the main OS thread. This conflicts with
`#[tokio::main]` which also wants to own the main thread.

**Wrong** (will deadlock or panic at runtime):
```rust
#[tokio::main]
async fn main() {
    let ui = AppWindow::new().unwrap();
    ui.run().unwrap();  // blocks — tokio executor starves
}
```

**Correct** — build the runtime manually, keep main thread for Slint:
```rust
fn main() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    let ui = AppWindow::new().unwrap();
    rt.spawn(async move { /* all async work here */ });
    ui.run().unwrap();  // main thread is Slint's
}
```

## Cross-Thread UI Updates

Never touch Slint UI objects from a tokio thread directly. Use:
```rust
slint::invoke_from_event_loop(move || {
    if let Some(ui) = ui_weak.upgrade() {
        ui.set_agent_text("hello".into());
    }
}).ok();
```

`invoke_from_event_loop` queues a closure onto the Slint event loop and returns
immediately — it is safe to call from any thread.

## Weak Handles

`ui.as_weak()` gives a `slint::Weak<AppWindow>` that is `Send + Clone`.
Clone it into each async closure before spawning.

```rust
let ui_weak = ui.as_weak();
rt.spawn(async move {
    let ui_weak2 = ui_weak.clone();  // clone per spawned task
    slint::invoke_from_event_loop(move || {
        ui_weak2.upgrade().map(|ui| ui.set_status("ok".into()));
    }).ok();
});
```

## Slint Property Types

Slint strings are `slint::SharedString`, not `String`. Convert with `.into()`:
```rust
ui.set_agent_text(some_rust_string.into());
let s: String = ui.get_agent_text().to_string();
```

## Slint Callbacks (Rust side)

Wire a `.slint` callback to a Rust closure:
```slint
// in .slint file
callback send-message(string);
```
```rust
// in main.rs
ui.on_send_message(move |text| {
    let msg = serde_json::json!({"type":"user_prompt","text": text.to_string()});
    // send to WS ...
});
```

## Dynamic Lists (Repeater / VecModel)

For chat messages, tool call list, session list, etc.:
```rust
// Rust model
use slint::{VecModel, ModelRc};
let messages: Rc<VecModel<MessageItem>> = Rc::new(VecModel::default());
ui.set_messages(ModelRc::from(messages.clone()));

// Push a new item (from Slint thread or invoke_from_event_loop)
messages.push(MessageItem { text: "hello".into(), ..Default::default() });
```

Define `MessageItem` as a `struct` in `.slint` and import it via `slint::include_modules!()`.

---

## Flickable / scrolling — set `viewport-height` or it won't scroll

A `Flickable` whose only child is a layout does **not** auto-scroll: with no explicit
`viewport-height`, Slint sizes the child layout to the *visible* area, so the content
never overflows and there's nothing to scroll. Symptom: content is clipped at the
bottom and drag does nothing.

**Fix** — bind `viewport-height` to the content layout's `preferred-height` (give the
layout an id; it must be a direct, non-conditional child so the id is in scope):
```slint
Flickable {
    vertical-stretch: 1;
    viewport-height: col.preferred-height;   // ← the missing piece
    col := VerticalLayout {
        // …rows, for-loops, etc.
    }
}
```
`viewport-width` may be left unset (defaults to the Flickable width → no horizontal
scroll). This is also required for any manual auto-scroll-to-bottom handler, since
`flick.viewport-y = min(0px, -(flick.viewport-height - flick.height))` needs a real
`viewport-height` to compute the bottom. Every scrollable pane in this repo follows
this pattern (chat, settings, session, council, terminal, persona, notif).

---

## Pi KMS/DRM Setup

### Environment variable
```bash
SLINT_BACKEND=linuxkms
```
Set in the systemd service `Environment=` line (already done in `deploy/apexos-rs-ui.service`).

### Required user groups
```bash
sudo usermod -aG render,video,input agentd
```
(Only needs doing once; survives reboots.)

### Device nodes
```
/dev/dri/card0       — DRM modesetting, HDMI output
/dev/dri/renderD128  — GPU rendering (OpenGL ES)
/dev/input/event*    — keyboard/touch input
```

### Pi 5 GPU
BCM2712 uses VideoCore VII. The `v3d` open-source driver is in Debian trixie.
`linuxkms` uses EGL + OpenGL ES 2.0 via this driver.
Check: `ls /dev/dri/` should show `card0` and `renderD128`.

### Pi 4 GPU
BCM2711, VideoCore VI, also uses `v3d`. Same setup. Expected ~2-3× slower rendering
than Pi 5 but perfectly smooth for a UI at 30fps.

### Pi Zero 2W
BCM2837, VideoCore IV. Driver is `vc4` not `v3d`. May need:
```bash
SLINT_BACKEND=linuxkms-femtovg   # software renderer fallback
```
Software renderer is much lighter (no GPU) — appropriate for 512MB/1GB RAM targets.

---

## Dev on Desktop (x86 / macOS)

Leave `SLINT_BACKEND` unset. Slint auto-detects `winit` when `DISPLAY` or
`WAYLAND_DISPLAY` is present. Everything else is identical.

To simulate kiosk (fullscreen, no window chrome):
```bash
SLINT_FULLSCREEN=1 cargo run
```

---

## Font Notes

Slint bundles Noto Sans by default. For the terminal-aesthetic monospace look,
bundle a `.ttf` via Slint's `@font-face` in `.slint`:
```slint
@font-face {
    font-family: "JetBrains Mono";
    sources: [ResourcePath("../assets/fonts/JetBrainsMono-Regular.ttf")];
}
```
Place the font in `ui-slint/assets/fonts/`. It gets embedded in the binary at
compile time — zero runtime font dependency.

---

## Common Compile Errors

### `slint::invoke_from_event_loop` type mismatch
Make sure the closure is `FnOnce() + Send + 'static`. Don't capture `&T` — clone or wrap in `Arc`.

### `include_modules!()` — no component named X
The component must be `export component X` in the `.slint` file (the `export` keyword is required).

### Build fails with `fontconfig` not found
```bash
sudo apt-get install -y libfontconfig1-dev
```

### Build fails with `libgl` not found
```bash
sudo apt-get install -y libgl1-mesa-dev
```
