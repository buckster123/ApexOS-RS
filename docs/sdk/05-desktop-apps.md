# Building a Desktop App / UI View

> How to add a new window/view to the ApexOS-RS native UI (`ui-slint`, binary
> `apexos-rs-ui`). You extend this surface when you want a new *visible* thing on
> the Pi's display (or the desktop dev window): a dashboard, a tool inspector, a
> control panel — anything the agent or a human should *see* and click, as
> opposed to a new capability the agent *invokes* (that's a tool — see the tool
> SDK). A UI app is a Slint component hosted in a window frame, fed by Rust from
> WebSocket events or `/api/*` polls.

A new app touches exactly four files and (almost always) **zero agentd code**:
a Slint view component, the `types.slint` shared structs it reads, the
`AppKind` enum + window-manager wiring, and the `main.rs` data plumbing. The UI
is "a thin stateless renderer" over agentd's wire protocol — it has no Cargo
dependency on agentd, only the JSON contract.

> **Sizes (measured, not aspirational).** The `apexos-rs-ui` release binary is
> **~32 MB** and its RSS on the Pi 5 is **~160 MB** (`ps` rss — it links the GL
> stack: femtovg, glow, fontconfig, EGL/GBM). The "~10 MB binary / ~10 MB RAM"
> figures you'll see in older notes describe an early Nano-only spike and are
> wrong for the shipping UI. Nano (`linuxkms-femtovg`, software renderer) is
> lighter than the GL tiers but nowhere near 10 MB.

---

## The 18 apps

`AppKind` has **18 variants** (`types.slint`, `export enum AppKind`). The
ordinal is positional and mirrored in Rust by `kind_ordinal` /
`kind_from_ordinal` in `main.rs` — **these must agree with the enum order.**
Grouped by shape (this is also a map of which existing view to copy when you
build a new one):

| Group | Apps (ordinal) | View component | Driven by |
|-------|----------------|----------------|-----------|
| **Basic** (one prop / poll) | `system`=1, `calculator`=16, `web`=15 | `dashboard.slint`, `calculator_view.slint`, `web_view.slint` | `/api/run` poll, pure-Slint, or `xdg-open` |
| **Data-driven** (a `VecModel` + a `refresh`) | `sessions`=3, `event-log`=7, `mesh`=8, `inference`=9, `notes`=12, `explorer`=17 | `session_view.slint`, `event_log_view.slint`, `mesh_view.slint`, `inference_view.slint`, `notes_view.slint`, `explorer_view.slint` | `/api/*` GET → `VecModel` rows, refresh on open |
| **Complex multi-pane** (state + editors + sub-panels) | `chat`=0, `sensor`=2, `settings`=4, `council`=6, `audio-editor`=10, `sonus`=11 | `chat_view.slint`, `sensor_view.slint`, `settings_view.slint`, `council_view.slint`, `audio_editor_view.slint`, `sonus_view.slint` | WS event stream + several `/api/*` |
| **Canvas / GL** (custom painter or raw OpenGL) | `terminal`=5, `sketchpad`=14, `face`=13 | `terminal_view.slint`, `sketchpad_view.slint`, `face_view.slint` (+ `face_gl.rs`) | PTY WS, tiny-skia raster, rendering-notifier GL |

Notes on the less-obvious members:

- **`web`** (🌐) is a launcher of *external-browser tiles* + an open-any-URL bar.
  Slint can't embed a webview, so it shells out to `xdg-open`/`$BROWSER`.
- **`explorer`** (📁 "Files") and **`notes`** (📝) are thin views over the
  `notes_*` / file tools — `explorer` refreshes via `on_refresh_explorer`,
  `notes` via `on_refresh_notes`.
- **`mesh`** (🕸) renders the cluster: discovered avahi nodes, paired peers, and
  the pairing-code flow. See [the Mesh app](#the-mesh-app) below.
- **`inference`** (⚡) shows / hot-swaps the model backend (`/api/backend`,
  `/api/model`); **`audio-editor`** (🎛) and **`sonus`** (🎵) are the audio
  surfaces (the `audio_*` tools + `/api/sonus/*`).
- **`face`** (😊, titled **"APEX"**) is the agent's on-screen face — the single
  most involved app. See [the Face app](#the-face-app).

`kind_title`, `default_geom`, and the launcher rows in `start_menu.slint`
(`MenuRow` / `WinMenuRow`) all key off this same ordinal. Core apps (chat,
system, sonus, notes, **APEX/face**, sketchpad, sessions, settings, calculator,
files) are always shown; deep-tech apps (sensors, terminal, council, event-log,
mesh, inference, audio-editor, web) sit behind `if Personas.show-tech-apps`.

---

## Concepts

**The thread model is the load-bearing invariant.** Slint's event loop owns the
**main OS thread**; tokio runs on a background pool. Violate this and the app
deadlocks at runtime.

- `main()` builds the runtime manually — **never** `#[tokio::main]`. It creates
  the `AppWindow`, spawns all async work via `rt.spawn(...)`, then calls
  `ui.run()` last, which blocks the main thread.
- Async tasks **never** touch UI handles directly. They marshal every mutation
  through `slint::invoke_from_event_loop(move || { … })`, which queues a closure
  onto the Slint thread (the WS receive loop, the stats poll, etc.). It's
  fire-and-forget — returns immediately, runs later.
- The cross-thread handle is `ui.as_weak()` → `slint::Weak<AppWindow>` (`Send +
  Clone`). Clone it per spawned task; `.upgrade()` inside the closure.

**Models live on the Slint thread.** Dynamic lists are `Rc<VecModel<T>>` parked
in `thread_local!` cells (see the `thread_local!` block near the top of
`main.rs`) and only ever mutated on the Slint thread — `MESSAGES`, `SESSIONS`,
`MODELS`, `TOASTS`, `NOTIF_LOG`, `WINDOWS`, `COUNCIL` (plus per-app row models
for mesh / inference / event-log / notes / explorer). Each is created in
`main()`, handed to the UI via `ui.set_<name>(ModelRc::from(model.clone()))`,
and stashed in its thread-local. `VecModel<T>`'s `T` is a **struct defined in
`types.slint`** and surfaced to Rust by `slint::include_modules!()`.

**The shell has two modes** (`ShellMode`, `types.slint`):
- **Focus** — the legacy full-screen tabbed surface, switched by
  `current-view: int`. The tab strip there is hard-coded per view.
- **Desktop** — the windowed face (in `appwindow.slint`): wallpaper → window
  layer → taskbar. This is where "apps" live as draggable windows.

**The window manager** (glowup G2) is hand-rolled in `main.rs`:
- Rust owns the window *set*: `WINDOWS: VecModel<WindowDesc>` where **model
  order == z-order** (last row paints on top). `WindowDesc` is
  `{id, kind, title, x, y, w, h, minimized, maximized}` (`types.slint`).
- Slint owns *live drag/resize geometry* (frame-local deltas) and commits back
  to Rust on pointer release — see `AppWindowFrame` (`app_window_frame.slint`),
  the chrome+content host. Round-tripping every pointer move would lag.
- The helpers `wm_launch` / `wm_focus` / `wm_refocus_top` / `wm_update_row`
  (all in `main.rs`) run on the Slint thread, driven by the WM callbacks wired
  in `main()`. `wm_launch` is **singleton-per-kind**: re-launching an app that's
  already open un-minimises and focuses the existing window instead of opening a
  second one (it calls `wm_index_by_kind` first) — this is why the Face window
  is a single instance.

**`AppKind`** (`types.slint`) is the discriminant that ties everything
together: it's the window's `kind`, the launcher ordinal, and the `if
root.kind == AppKind.x` content switch in `AppWindowFrame`. Its ordinal is
mirrored in Rust by `kind_ordinal` / `kind_from_ordinal` in `main.rs` —
**these two must agree with the enum order.**

**Personas** (glowup G4, `personas.slint`) bundle theme + chrome + wallpaper +
default shell mode. The `Personas` global derives structural bits from
`current`, including behaviour gates like `show-tech-apps` that hide deep-tech
apps from the warm/simple persona. New apps decide whether they're "core"
(always visible) or gated behind `Personas.show-tech-apps`.

**`build.rs`** (`ui-slint/build.rs`) compiles `src/ui/appwindow.slint` (and
everything it imports) at build time via `slint_build::compile`. If you edit a
`.slint` file and `cargo build` doesn't pick it up, `touch ui-slint/build.rs`.

---

## Add a new app/view

We'll add an app of `AppKind` `myapp`. Five edits; the first is pure Slint, the
rest wire it into the WM and Rust.

### 1. Write the view component — `ui-slint/src/ui/components/myapp_view.slint`

A view is just an `export component` that takes `in` properties (data from Rust)
and emits `callback`s (intents to Rust). Follow the existing template
(`dashboard.slint`, `terminal_view.slint`). Read theme tokens from `Palette`;
never hard-code colours.

```slint
import { Palette } from "../palette.slint";

export component MyAppView {
    in property <string> headline;           // fed from Rust
    in property <int>    scroll-tick: 0;     // bump to scroll-to-bottom (see gotcha)
    callback do-thing();                     // emitted to Rust

    VerticalLayout {
        padding: 12px;
        spacing: 8px;
        Text {
            text: root.headline == "" ? "waiting…" : root.headline;
            color: Palette.text-bright;
            font-size: 14px;
        }
        // …a scrollable body must set viewport-height — see Gotchas.
    }
}
```

### 2. Add the `AppKind` variant — `ui-slint/src/ui/types.slint`

```slint
export enum AppKind { chat, system, sensor, sessions, settings, terminal,
                      council, event-log, mesh, inference, audio-editor, sonus,
                      notes, face, sketchpad, web, calculator, explorer, myapp }
```

Append, don't reorder — ordinals are positional and Rust hard-codes them. The
next free ordinal is **18** (chat=0 … explorer=17).

If your app needs structured rows (a list), define the row struct here too, e.g.
`export struct MyRow { name: string, value: float }`. It becomes a Rust struct
via `include_modules!()`.

### 3. Mirror the ordinal in Rust — `ui-slint/src/main.rs`

Add the arm to **both** `kind_ordinal` and `kind_from_ordinal`, plus
`kind_title` and `default_geom` (all four are small `match` functions in
`main.rs`):

```rust
// kind_ordinal
AppKind::MyApp => 18,
// kind_from_ordinal
18 => AppKind::MyApp,
// kind_title
AppKind::MyApp => "My App",
// default_geom  (w, h)
AppKind::MyApp => (520.0, 460.0),
```

### 4. Host the content in the window frame — `app_window_frame.slint`

Import the view at the top, declare any new `in` properties + `callback`s on
`AppWindowFrame`, and add a content arm beside the existing `if root.kind ==
AppKind.x` arms in `app_window_frame.slint`:

```slint
import { MyAppView } from "myapp_view.slint";
// …on AppWindowFrame:
in property <string> myapp-headline;
callback myapp-do-thing();
// …in the content area:
if root.kind == AppKind.myapp: MyAppView {
    vertical-stretch: 1;
    headline: root.myapp-headline;
    do-thing => { root.myapp-do-thing(); }
}
```

Then forward those through `AppWindow` in `appwindow.slint`: add the matching
`in-out property`/`callback` on `AppWindow`, and pass them into the
`AppWindowFrame` instance inside the `for w in root.windows` loop (the desktop
window layer):

```slint
// on AppWindow
in-out property <string> myapp-headline: "";
callback myapp-do-thing();
// inside the AppWindowFrame instance in the desktop window loop
myapp-headline: root.myapp-headline;
myapp-do-thing => { root.myapp-do-thing(); }
```

### 5. Add the launcher entry + wire Rust data

**Launcher** — `start_menu.slint` (and its sibling `WinMenuRow` block for the
Windows-persona menu — add the row in **both**). Core app → always-shown row;
deep-tech → gate it. Use the new ordinal (18):

```slint
// always-shown:
MenuRow { glyph: "✨"; label: "My App"; clicked => { root.launch(18); } }
// or deep-tech, hidden by the simple persona:
if Personas.show-tech-apps: MenuRow { glyph: "✨"; label: "My App"; clicked => { root.launch(18); } }
```

`launch(ord)` already routes to `ui.on_launch_app` in `main()`, which calls
`wm_launch` and fires a per-app refresh hook — add yours to that `match` if the
window should fetch on open (like Settings/Sessions/Mesh/Inference/Notes do):

```rust
AppKind::MyApp => ui.invoke_refresh_myapp(),
```

**Data plumbing** — in `main()`, wire the property and the callback. For an
`/api` poll, add a `refresh` callback that spawns a fetch and marshals the
result back (mirror `on_refresh_settings` / `on_refresh_mesh`):

```rust
let rt_h   = rt.handle().clone();
let client = Arc::clone(&http_client);
let base   = http_base.clone();
let uw     = ui.as_weak();
ui.on_refresh_myapp(move || {
    let (client, base, uw) = (Arc::clone(&client), base.clone(), uw.clone());
    rt_h.spawn(async move {
        let v = json_get(&client, format!("{base}/api/run")).await; // some endpoint
        let headline = v["stdout"].as_str().unwrap_or("").to_string();
        slint::invoke_from_event_loop(move || {
            if let Some(ui) = uw.upgrade() { ui.set_myapp_headline(headline.into()); }
        }).ok();
    });
});
```

For a **WS-event-driven** app, add a `match` arm in `dispatch_event` (the big
event router in `main.rs`) keyed on the event's `type`, exactly like the
`sensor_reading` or `council_*` arms already there. Push into your `VecModel`
(parked in a new thread-local) or `ui.set_…` a property — always inside
`invoke_from_event_loop`.

That's the whole loop. `cargo build`, and the app launches from Start in
Desktop mode.

---

## Worked example: a "Logs" app driven by `/api/run`

A read-only window that tails the last lines of `journalctl -u agentd` via the
existing `POST /api/run` endpoint, refreshing every time it's opened and on a
timer while visible. Realistic, useful for the agent watching its own daemon,
and needs **no agentd change**.

**1. `ui-slint/src/ui/components/logs_view.slint`**

```slint
import { Palette } from "../palette.slint";

export component LogsView {
    in property <string> text;
    in property <int>    scroll-tick: 0;
    callback refresh();

    changed scroll-tick => {
        flick.viewport-y = min(0px, -(flick.viewport-height - flick.height));
    }

    VerticalLayout {
        Rectangle {                       // tiny toolbar
            height: 32px; background: Palette.surface;
            HorizontalLayout {
                padding-left: 10px; alignment: start;
                Rectangle {
                    width: 70px; height: 22px; y: 5px;
                    border-radius: Palette.radius-sm;
                    background: ta.has-hover ? Palette.surface-hi : transparent;
                    ta := TouchArea { clicked => { root.refresh(); } }
                    Text { text: "REFRESH"; color: Palette.text-dim; font-size: 9px;
                           horizontal-alignment: center; vertical-alignment: center; }
                }
            }
        }
        flick := Flickable {
            vertical-stretch: 1;
            viewport-height: col.preferred-height;   // REQUIRED or it won't scroll
            col := VerticalLayout {
                padding: 10px;
                Text {
                    text: root.text == "" ? "no logs yet" : root.text;
                    color: Palette.text; font-family: "monospace";
                    font-size: 11px; wrap: word-wrap;
                }
            }
        }
    }
}
```

**2. `types.slint`** — append `logs` to `AppKind` (next free ordinal **18**):
`export enum AppKind { …, calculator, explorer, logs }`

**3. `main.rs`** — add `AppKind::Logs => 18` to `kind_ordinal`, `18 =>
AppKind::Logs` to `kind_from_ordinal`, `AppKind::Logs => "Logs"` to
`kind_title`, `AppKind::Logs => (640.0, 440.0)` to `default_geom`.

**4. `app_window_frame.slint`** — `import { LogsView } from "logs_view.slint";`,
add `in property <string> logs-text;`, `in property <int>
logs-scroll-tick;`, `callback logs-refresh();`, and the content arm:

```slint
if root.kind == AppKind.logs: LogsView {
    vertical-stretch: 1;
    text: root.logs-text;
    scroll-tick: root.logs-scroll-tick;
    refresh => { root.logs-refresh(); }
}
```

In `appwindow.slint`: add `in-out property <string> logs-text;`, `in-out
property <int> logs-scroll-tick;`, `callback logs-refresh();` on `AppWindow`;
pass `logs-text: root.logs-text; logs-scroll-tick: root.logs-scroll-tick;
logs-refresh => { root.logs-refresh(); }` into the `AppWindowFrame` instance.

**5. `start_menu.slint`** — deep-tech, so gate it (in both the `MenuRow` and
`WinMenuRow` blocks):
`if Personas.show-tech-apps: MenuRow { glyph: "📜"; label: "Logs"; clicked => { root.launch(18); } }`

**6. `main.rs` wiring** — refresh on open (the `on_launch_app` match):
`AppKind::Logs => ui.invoke_logs_refresh(),`. Then the callback:

```rust
let rt_h   = rt.handle().clone();
let client = Arc::clone(&http_client);
let base   = http_base.clone();
let uw     = ui.as_weak();
ui.on_logs_refresh(move || {
    let (client, base, uw) = (Arc::clone(&client), base.clone(), uw.clone());
    rt_h.spawn(async move {
        let body = client
            .post(format!("{base}/api/run"))
            .json(&serde_json::json!({
                "command": "journalctl -u agentd -n 200 --no-pager"
            }))
            .timeout(std::time::Duration::from_secs(8))
            .send().await.ok();
        let text = match body {
            Some(r) => r.json::<serde_json::Value>().await.ok()
                .and_then(|v| v["stdout"].as_str().map(str::to_owned))
                .unwrap_or_default(),
            None => String::new(),
        };
        slint::invoke_from_event_loop(move || {
            if let Some(ui) = uw.upgrade() {
                ui.set_logs_text(text.into());
                let t = ui.get_logs_scroll_tick();
                ui.set_logs_scroll_tick(t.wrapping_add(1));   // pin to bottom
            }
        }).ok();
    });
});
```

`cargo build`, launch the UI on desktop (`cargo run`), open Start → Logs. The
window is draggable, resizable, minimisable, and the REFRESH button (or
re-launch) re-fetches. To poll while visible, spawn an interval task like the
stats poll in `main()` and gate it on the window existing.

> Note: `journalctl`/`run_command` is a **gated** verb (see Policy below). The
> *UI* call to `/api/run` succeeds, but agentd may emit an approval-pending
> round-trip or reject it depending on policy mode. For a kiosk daemon the
> command runs as the `agentd` user inside the systemd sandbox.

---

## The Face app

`AppKind::Face` (ordinal **13**, titled **"APEX"** in the launcher, glyph 😊) is
the agent's on-screen face — and the only app where the agent *drives* the view
in real time. It's **shipped and default-on for GL tiers**. It launches at boot
(`wm_launch(&ui, &windows, AppKind::Face)` in `main()`) and is a single
instance (`wm_launch` is singleton-per-kind). Two layers control it:

**1. Activity (automatic).** Rust sets `face-state` from the event stream — the
six **activity** states `idle / thinking / speaking / listening / alert /
sleeping`. You'll see `ui.set_face_state("thinking")` on `tool_requested`,
`"speaking"` on streamed `agent_text`, `"listening"` while recording, etc.,
scattered through `dispatch_event`. The user never asks for these; they track
what the daemon is doing.

**2. Emote (agent-driven).** APEX calls the **`display_face`** tool
(apexos-tools, `allow` in `policy.toml`) to set one of the twelve **emotive**
states. The UI consumes the `tool_requested` event *directly* — `dispatch_event`
special-cases `tool_name == "display_face"`, reads `call.args`, calls
`set_face_emote(&ui, state, gaze, intensity)`, and shows **no tool card** (it'd
be noise). No new agentd event, no agentd change.

**Emote vocabulary** (the `display_face` `inputSchema`, validated in the tool's
`display_face` fn):

- **`state`** (required): one of the emotive set —
  `neutral · happy · curious · amused · confused · sad · surprised · wink ·
  skeptical · proud · love · focused` (the six activity states are also accepted
  but APEX rarely needs them). An unknown state returns a tool error.
- **`gaze`** (optional): `center | left | right | up | down` (default `center`).
- **`intensity`** (optional): `0.0–1.0`, clamped, default `0.7` — how strongly
  the expression reads.
- **`text`** (optional): a short (≤20 char) caption shown under the face.

**The emote is held past turn-end.** `set_face_emote` stashes
`(state, gaze, intensity)` in the `FACE_HELD` thread-local so the expression
*lingers* until the user's next prompt. `face_rest(&ui)` (called at turn-end)
restores the held emote rather than snapping to `idle`; `clear_face_hold()` (on
a fresh user prompt) drops it so the next exchange starts neutral. This is why
APEX can stay "amused" through several streamed deltas and a tool round-trip.

### Two render paths: GL face vs 2D fallback

The face has **two renderers**, both reading the *same* expression state so they
can't drift:

- **GL face (default on GL tiers)** — a **raymarched SDF head** (a real 3D
  ellipsoid with a protruding nose, glossy eyes, shaped brows, blush, talking
  mouth-flap, head-tilt/cock, head-pitch, blink, sliding teardrop, Duchenne
  smile/cheek-raise, teeth) rendered in `face_gl.rs` via Slint's
  **rendering notifier** (`Window::set_rendering_notifier`) + femtovg's
  `GraphicsAPI::NativeOpenGL` + **glow**. The GLSL is a single fullscreen
  triangle with the SDF in the fragment shader; uniforms (`u_eyes`, `u_brow`,
  `u_mouth`, `u_gaze`, `u_blush`, `u_head`, `u_anim`, `u_cheek`, …) come from a
  `FaceExpr` struct that Rust fills from the `FaceGl` global.
- **2D fallback** — `face_view.slint`, a pure-Slint vector face (eyes, brows,
  mouth curve, accent-coloured halo) driven off the same `state`/`gaze`/
  `intensity` properties. This is the **Nano** path (femtovg software renderer,
  no GL) and the fallback whenever GL init fails.

**The `FaceGl` global** (`types.slint`, `export global FaceGl`) is the bridge.
`face_view.slint` runs a sampling `Timer` that publishes its live on-window rect
(`x/y/w/h`, logical px) **and** all its derived feature values
(`eye-l`, `eye-r`, `brow`, `brow-skew`, `brow-angle`, `mouth`, `mouth-open`,
`gaze-x`, `gaze-y`, `intensity`, `blush`, `accent`, plus GL-only motion
`talk`, `head-roll`, `head-pitch`, `tear`, `cheek`) into `FaceGl`. The Rust
`AfterRendering` notifier reads them, converts the rect to physical px
(× scale-factor, Y-flipped for GL's bottom-left origin), sets `glViewport` +
`glScissor` so the shader face is **scissored to the movable/resizable face
window** instead of painting the whole surface, and feeds the features as shader
uniforms. So the 2D layer is both the fallback renderer *and* the state source
the GL path samples — one set of derivations, two outputs.

**Gating.** The GL path is set up only when `APEX_FACE_GL != "0"` (i.e.
default-on); `APEX_FACE_GL=0` forces the 2D fallback. "Is the face window open?"
gating stays in Rust (it owns the `WINDOWS` model — Slint has no
element-destroyed hook to clear `FaceGl.active`); the notifier no-ops when no
face window is visible, so the GL path is zero-cost when the face is closed and
on backends where the notifier never delivers a `NativeOpenGL` context.

> The GC9A01A round-TFT write inside the `display_face` tool is best-effort
> back-compat with original ApexOS hardware. On -RS the Slint UI is the
> renderer; on a headless node `display_face` is a **no-op, not an error**.

Cross-refs: `ui-slint/src/face_gl.rs` (GL renderer + `FaceExpr`),
`ui-slint/src/ui/components/face_view.slint` (2D fallback + state derivations),
`FaceGl` global in `types.slint`, the `display_face` fn in
`tools/crates/apexos-tools/src/tools.rs`, and `set_face_emote` / `face_rest` /
`clear_face_hold` / `FACE_HELD` in `main.rs`.

---

## Vision input (the agent's eyes)

Vision is **first-class** in the protocol — an app or tool can hand APEX an
actual image, not just text describing one. Three mechanisms, all converging on
`ContentBlock::Image { media_type, data }` (the base64 image block that both
providers serialize natively — Anthropic `image`, OpenAI `image_url`).

**1. User-attached images.** `Event::UserPrompt` carries
`images: Vec<ImageSource>` (`core/src/types.rs`). The gateway runs every raw
upload through the **vision shim** (`apexos_core::vision::load_and_prepare` for a
`path`, `prepare_b64` for inline b64) *before* building the event — decode →
downscale longest-edge ≤ `VISION_MAX_EDGE` (default 1024 px, the token-bomb
guard) → re-encode → base64 — so `UserPrompt.images` is always prepared b64.
The native Slint workspace image picker and the PWA's
`POST /api/sessions/{id}/image` both feed this path.

**2. Tool results that return an image — the `{vision:{…}}` sentinel.** A tool
can emit a result of the shape:

```json
{ "vision": { "path": "screenshots/latest.png" }, "text": "what I captured" }
```

(or `"b64": "<base64>"` instead of `path`). The agent turn loop's
`vision_rewrite` (`agent/src/turn.rs`) spots the sentinel via
`find_vision_sentinel`, runs the same `vision::prepare_*` shim, and rewrites the
tool result into a `ContentBlock::Image` so APEX *sees* the picture inline —
**zero agentd-protocol changes per tool**. Three shipped tools use this exact
convention:

- **`screenshot_mirror`** — ui-slint serves its own `Window::take_snapshot()`
  PNG over a loopback endpoint (`APEXOS_UI_SNAPSHOT_*`); the tool fetches it,
  writes it under the workspace, and returns the sentinel. Renderer-agnostic, no
  DRM readback. APEX can literally see its own screen.
- **`sketch_snapshot`** — hands APEX the current Sketchpad drawing inline.
- **`camera_capture`** — snaps one frame (Pi CSI `rpicam` → USB/laptop V4L2
  `ffmpeg` → `fswebcam`, with warmup frames; `APEXOS_CAMERA_DEVICE` /
  `APEXOS_CAMERA_CMD` overrides) and returns the sentinel — physical-world eyes.

**For UI-app authors:** you don't usually touch this path — vision is a *tool*
capability, not a *view* one. But it's the reason a "canvas" app like Sketchpad
is useful to the agent: the app draws pixels, a paired tool (`sketch_snapshot`)
hands those pixels to APEX. If you build a new canvas/capture app, mirror that
pairing — the view renders, a tool returns `{vision:{path}}`.

Cross-refs: `ContentBlock::Image` + `ImageSource` in `core/src/types.rs`; the
shim in `core/src/vision.rs` (`prepare_image`, `load_and_prepare`, `prepare_b64`,
`anthropic_tool_result_content`); `vision_rewrite` / `find_vision_sentinel` in
`agent/src/turn.rs`; the tool fns in `tools/crates/apexos-tools/src/tools.rs`.

---

## The Mesh app

`AppKind::Mesh` (ordinal **8**, 🕸, deep-tech) renders the cluster and runs the
**pairing flow**. It refreshes on open via `on_refresh_mesh` and shows two
lists: discovered avahi nodes (the daemon *browses* `_apexos._tcp` and
*advertises* itself via a static avahi service file — discovery is symmetric)
and **paired peers** (from `peers.toml`).

The view's job is the **pairing-code handshake** so cross-node agent-to-agent
delivery works without typing a token:

- **PAIR** — this node shows a single-use 6-digit code
  (`POST /api/mesh/pair/start`, 5-min TTL, 5-guess lockout).
- **+ ADD** — the other node enters that code (`/api/mesh/pair/redeem` →
  `/api/mesh/pair/claim`); **both nodes store each other with tokens** in one
  exchange.

Cross-node a2a then needs both halves on the target: a per-peer token
(`PeerRecord.token` in `peers.toml`, sent as `Authorization: Bearer`; redacted
to `has_token` over the API) **and** a LAN bind (`AGENTD_BIND=0.0.0.0:8787` —
agentd defaults to loopback). The token is exactly what makes the non-loopback
bind safe. This is all agentd/mesh machinery; the Mesh *app* is the thin view
over `/api/mesh/*` that surfaces it — a textbook "thin view over /api".

---

## Policy / safety

A UI app is a renderer; it has no privileges of its own. Its safety boundary is
entirely **agentd's**, reached over the wire.

- **No approval for the UI itself.** Adding a view, a window, or a launcher entry
  changes nothing agentd evaluates. Drawing pixels and reading properties is
  unprivileged. The approval `PolicyEngine` lives in agentd
  (`agentd/crates/plugins/src/policy.rs`); the UI only *renders* its decisions
  (the tool card + approve/reject buttons in `tool_card.slint`, wired via
  `AgentBridge.approve-tool`/`reject-tool` in `main()`).
- **What your app *fetches* may be gated.** Every `/api/*` call your app makes is
  subject to the same auth + policy as any client. The shared `http_client`
  carries the bearer token; without it, calls 401 when
  `AGENTD_TOKEN` is set (which install.sh always does). `/api/run` (shell),
  `/api/soul` writes, `/api/policy`, `/api/model`, `/api/power` are all
  privileged surfaces — read `config/policy.toml` (default `mode=suggest`:
  read-only allowed, write/delete/`run_command`/`http_fetch` gated). Prefer
  read-only endpoints for a passive display; anything that *acts* should expect
  an approval round-trip and surface it (toast + the tool card already do).
- **The systemd sandbox is the real confinement**, not the UI. `apexos-rs-ui`
  runs as **root** on the Pi (DRM master on a seatless board —
  `deploy/apexos-rs-ui.service`, `User=root`), but it does no privileged work
  itself; it only talks to the loopback gateway. `agentd` is the jailed party
  (`ProtectSystem=strict`, `ReadWritePaths=/var/lib/agentd /etc/agentd`). Do not
  add code to the UI that shells out, writes config, or touches the filesystem —
  route it through agentd so the sandbox + policy apply.
- **For agents self-extending the UI:** this surface is *additive and
  reversible* by construction — a new app is new Slint + new Rust plumbing,
  never a change to agentd's perimeter. The audit discipline is the build/commit
  log, not policy: a new app does not appear at runtime (it requires a rebuild +
  hot-swap of the `ui-slint` binary, `architecture.md` deploy section), so it is
  **not** something the agent can grant itself in a running session the way a
  `propose_evolution` config write is. Treat UI changes as code commits (gate →
  commit → push → rebuild on Pi), and record the design intent in Cerebro
  (`store_procedure` for the WM/Slint pattern, `session_save` for the build).
- **Tier-awareness (Nano-first, CLAUDE.md):** an app must work on the femtovg
  software renderer — no heavy animations assumed, graceful when data is absent
  (show "waiting…"/empty states, as `terminal_view.slint` does). Persona default
  mode is tier-clamped to Focus on femtovg (`apply_persona` in `main.rs`); your
  app should not assume Desktop mode exists. (The GL face is the one app that
  *prefers* a GL tier — it degrades to the 2D `face_view.slint` on Nano.)

---

## Reference

### Files to edit for a new app

| File | What you add |
|------|--------------|
| `ui-slint/src/ui/components/<name>_view.slint` | the view `export component` (new file) |
| `ui-slint/src/ui/types.slint` | the `AppKind` variant + any row/data structs |
| `ui-slint/src/ui/components/app_window_frame.slint` | import + `in` props + `callback`s + `if root.kind == AppKind.<x>` content arm |
| `ui-slint/src/ui/appwindow.slint` | matching `in-out`/`callback` on `AppWindow` + pass-through into the `for w in root.windows` `AppWindowFrame` |
| `ui-slint/src/ui/components/start_menu.slint` | a `MenuRow` (core) or `if Personas.show-tech-apps: MenuRow` (deep-tech) — **and the matching `WinMenuRow`** — launching the ordinal |
| `ui-slint/src/main.rs` | `kind_ordinal`/`kind_from_ordinal`/`kind_title`/`default_geom` arms; `on_launch_app` refresh hook; the refresh callback or `dispatch_event` arm |

### `AppKind` ordinals (`types.slint` `export enum AppKind`, mirrored by `kind_ordinal` / `kind_from_ordinal` / `kind_title` / `default_geom` in `main.rs`)

| Variant | Ordinal | Title | Default geom (w,h) | Launcher |
|---------|---------|-------|--------------------|----------|
| `chat` | 0 | Chat | 760×540 | core |
| `system` | 1 | System | 440×460 | core |
| `sensor` | 2 | Sensors | 560×480 | tech |
| `sessions` | 3 | Sessions | 500×520 | core |
| `settings` | 4 | Settings | 660×560 | core |
| `terminal` | 5 | Terminal | 640×420 | tech |
| `council` | 6 | Council | 560×560 | tech |
| `event-log` | 7 | Event Log | 560×520 | tech |
| `mesh` | 8 | Mesh | 520×460 | tech |
| `inference` | 9 | Inference | 520×520 | tech |
| `audio-editor` | 10 | Audio Editor | 660×600 | tech |
| `sonus` | 11 | Sonus | 480×540 | core |
| `notes` | 12 | Notes | 640×540 | core |
| `face` | 13 | APEX | 380×460 | core |
| `sketchpad` | 14 | Sketchpad | 600×580 | core |
| `web` | 15 | Web | 460×400 | tech |
| `calculator` | 16 | Calculator | 300×440 | core |
| `explorer` | 17 | Files | 680×520 | core |

("tech" = gated behind `Personas.show-tech-apps`.)

### Thread-local models (the `thread_local!` block near the top of `main.rs`)

| Thread-local | Type | Bound via | Mutate on |
|--------------|------|-----------|-----------|
| `MESSAGES` | `VecModel<MessageItem>` | `ui.set_messages` | Slint thread only |
| `SESSIONS` | `VecModel<SessionItem>` | `ui.set_sessions` | Slint thread only |
| `MODELS` | `VecModel<ModelItem>` | `ui.set_available_models` | Slint thread only |
| `TOASTS` | `VecModel<ToastItem>` | `Notifications.set_toasts` | Slint thread only |
| `NOTIF_LOG` | `VecModel<ToastItem>` | `Notifications.set_log` | Slint thread only |
| `WINDOWS` | `VecModel<WindowDesc>` | `ui.set_windows` | Slint thread only |
| `COUNCIL` | `VecModel<CouncilAgent>` | `ui.set_council_agents` | Slint thread only |

(Plus per-app row models — mesh peers, inference models, event-log, notes,
explorer — parked the same way and refreshed by their `on_refresh_*` callbacks.)

### Window-manager callbacks (declared on `AppWindow` in `appwindow.slint`, wired in `main()`)

| Callback | Signature | Rust handler |
|----------|-----------|--------------|
| `launch-app` | `(int ord)` | `wm_launch` + per-app refresh |
| `focus-window` | `(int id)` | `wm_focus` |
| `close-window` | `(int id)` | remove + `wm_refocus_top` |
| `minimize-window` | `(int id)` | set `minimized` + `wm_refocus_top` |
| `maximize-window` | `(int id)` | toggle `maximized` + focus |
| `task-activate` | `(int id)` | restore / focus / minimise-toggle |
| `move-window` | `(int id, length x, length y)` | commit base x/y on drop |
| `resize-window` | `(int id, length w, length h)` | commit base w/h on drop |

### Inbound WS events you can drive an app from (the `dispatch_event` router in `main.rs`)

| Event `type` | Key fields | Existing consumer |
|--------------|------------|-------------------|
| `agent_text` | `delta` | chat bubble (sets face `speaking`) |
| `turn_complete` | `session` | clear busy, TTS, `face_rest` |
| `tool_requested` | `call:{id,tool,args}` | tool card — **except `display_face`**, which drives the face directly (no card) |
| `tool_result` | `call:<id>, output:{ok,content}` | tool card update |
| `approval_pending` | `call:{id,tool,args}` | approve/reject |
| `sensor_reading` | `reading:{kind,…}` | dashboard/sensor |
| `council_*` | `topic/agent_id/delta/…` | council view |
| `sub_agent_started` | `child` | taskbar badge |
| `wake_triggered` | — | start recording (face `listening`) |

> The gateway broadcasts **every event** with a bare-number `session` field; a
> frame that fails to deserialize is silently dropped. For a multi-client build,
> filter on `session` (see `architecture.md` "Multi-client caveat").

### REST endpoints (UI base = `ws_to_http(AGENTD_WS)`)

| Endpoint | Method | Used by |
|----------|--------|---------|
| `/api/run` | POST `{command}` | stats poll, shell (gated) |
| `/api/status` `/api/soul` `/api/models` | GET | settings fetch |
| `/api/soul` | POST `{content}` | save soul (gated) |
| `/api/policy` `/api/model` `/api/backend` | POST | settings / inference (gated) |
| `/api/sessions` | GET | session list |
| `/api/sessions/{id}/image` | POST `{text?,images}` | attach image(s) → vision |
| `/api/mesh/nodes` `/api/mesh/peers` `/api/mesh/pair/*` | GET/POST | mesh app + pairing |
| `/api/sonus/play` `/api/sonus/stop` | POST | sonus playback |
| `/api/record/start` `/api/record/stop` | POST | voice |
| `/api/speak` | POST `{text}` | TTS |
| `/api/power` | POST `{action}` | power modal (gated) |
| `/api/snapshot` | GET | PWA camera (multi-backend capture) |
| `/terminal-ws` | WS | terminal PTY |

### Slint gotchas that bite new views (`slint-notes.md`)

| Gotcha | Fix |
|--------|-----|
| A `Flickable`/scroll pane doesn't scroll | bind `viewport-height: <col>.preferred-height` to the content layout (`slint-notes.md` + `terminal_view.slint`). On linuxkms the wheel is dead — prefer a std-widgets `ScrollView` (draggable bar) over a bare `Flickable` (see CLAUDE.md). |
| Scroll-to-bottom needs a trigger | bump an `in property <int> scroll-tick` from Rust; `changed scroll-tick => { flick.viewport-y = min(0px, -(flick.viewport-height - flick.height)); }` |
| A bare `Rectangle` row collapses to 0 height | rows that must size to content need a `VerticalLayout`/`HorizontalLayout`, not a `Rectangle` (`CLAUDE.md` gotchas) |
| `.slint` edit not recompiled | `touch ui-slint/build.rs` |
| `include_modules!()` can't find your component | it must be `export component X` |
| `SharedString` mismatch | convert Rust `String`/`&str` with `.into()`; read back with `.to_string()` |
| `invoke_from_event_loop` type error | closure must be `FnOnce() + Send + 'static`; clone/`Arc` captures, don't borrow |
| No key auto-repeat on linuxkms | backend limitation; don't design an app around held keys on the Pi |
