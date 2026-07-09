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
the viewport-y math needs a real `viewport-height` to compute the bottom.

**Repo standard: every scrollable pane uses a std-widgets `ScrollView`, never a bare
`Flickable`.** The linuxkms backend drops scroll-wheel events (only pointer
motion/button libinput events are translated), so on the Pi kiosk the ScrollView's
drag-able scrollbar is the *only* scroll affordance — a bare Flickable is unscrollable
there. The pattern (chat, settings, session, council, terminal, etc.):
```slint
sv := ScrollView {
    vertical-stretch: 1;
    horizontal-scrollbar-policy: ScrollBarPolicy.always-off;
    viewport-width: sv.visible-width;
    viewport-height: col.preferred-height;
    col := VerticalLayout { /* … */ }
}
```
ScrollView auto-manages the viewport from its child layout when `viewport-height` is
left unset (settings does this), but auto-scroll views need it explicit: the
scroll-to-bottom tick is `sv.viewport-y = min(0px, -(sv.viewport-height -
sv.visible-height))`, and sparse content is bottom-anchored with
`viewport-height: max(col.preferred-height, sv.visible-height)` (chat).

---

## Custom GL under the rendering notifier (Phase-2 face)

`Window::set_rendering_notifier` lets you draw raw GL (via `glow`) inside the same
context femtovg renders with — the basis of the GL face (`ui-slint/src/face_gl.rs`,
default-on wherever a real `NativeOpenGL` context exists, silent 2D fallback
otherwise; `APEX_FACE_GL=0` forces 2D). Three things bit us:

**1. Scissor the GL to the element's live rect, not the whole window.** The face is
a movable desktop-shell window, so the GL pass must track it. The `FaceView` publishes
its stage `absolute-position` + size to the `FaceGl` Slint global; Rust reads them in
the `AfterRendering` notifier, converts logical→physical px (× `window().scale_factor()`),
**flips Y** for GL's bottom-left origin (`sy = win_h - (fy + fh)`), and sets
`glViewport` + `glScissor` to that rect. `gl_FragCoord` stays window-absolute even with
a viewport set, so pass the rect's bottom-left as a `u_origin` uniform to localise it.
Restore full-frame state afterward (`disable(SCISSOR_TEST)`, viewport back to the
window) or the next femtovg frame is clipped.

**2. Never read a layout-coupled property (e.g. `absolute-position`) inside a `changed`
handler.** It re-enters the layout pass and panics with **"Recursion detected"**
(`i-slint-core properties.rs`). Sample such values from a `Timer { triggered => … }`
instead — the Timer fires in event-loop context (between frames), where the read is
safe. The face geometry is sampled at 32 ms (gated by a `FaceGl.active` flag Rust sets
only on the real-GL path, so the 2D Nano fallback pays nothing).

**3. `Window::take_snapshot()` *does* capture the notifier's GL overlay** on
winit/femtovg — taking a snapshot re-runs a render pass, which fires `AfterRendering`,
so raw-GL draws land in the PNG. This makes the loopback snapshot server
(`APEXOS_UI_SNAPSHOT_ADDR`, `/snapshot`) a way to verify GL work headlessly:
`curl …:8788/snapshot -o shot.png` and inspect.

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

Slint (1.16, parley + fontique) resolves fonts from the **system** — on Linux the
fallback chain goes through the real fontconfig lib (honours `/etc/fonts`). To
bundle a custom `.ttf`, `import` it from a `.slint` file (there is **no**
`@font-face` in Slint):
```slint
import "../../assets/fonts/JetBrainsMono-Regular.ttf";
// then use it:  font-family: "JetBrains Mono";
```
Place the font in `ui-slint/assets/fonts/`. An imported font is embedded in the
binary at compile time (the Rust output defaults to embed-all-resources) — zero
runtime font dependency.

**Emoji render monochrome by design.** We compile only the femtovg + software
renderers, and femtovg rasterizes glyph *outlines* only — a colour-bitmap font
("Noto Color Emoji") comes out as tofu. `ensure_mono_emoji_fontconfig()`
(ui-slint `main.rs`, runs before `AppWindow::new()`) writes a per-process
`FONTCONFIG_FILE` that `<include>`s the system config then `<rejectfont>`s
"Noto Color Emoji", so fallback lands on the bundled mono outline font
(`deploy/fonts/NotoEmoji-mono.ttf`, installed by install.sh) — scoped to
ui-slint only; the rest of the machine keeps colour emoji. An existing
`FONTCONFIG_FILE` (user/operator override) is respected.

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
