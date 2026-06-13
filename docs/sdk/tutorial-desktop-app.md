# Tutorial: Build a desktop app window for ApexOS-RS

> A guided, end-to-end walkthrough: go from nothing to a brand-new draggable
> app window visible in the ApexOS-RS shell. We build a **Clock & Uptime**
> panel — a tiny, real feature that shows the live wall-clock time (driven by
> the existing 1-second Slint timer) plus the host's `uptime`, fetched on
> demand from agentd's `/api/run` REST endpoint.
>
> By the end you will have touched the exact six files every new app touches,
> understood the thread-model rules that keep the UI from deadlocking, and
> hot-swapped the binary onto the Pi.
>
> Companion reference (terse, no narrative): `docs/sdk/05-desktop-apps.md`.
> Slint patterns + gotchas: `docs/slint-notes.md`.

All paths below are relative to the repo root unless absolute. The binary is
`apexos-rs-ui`; the crate is `ui-slint`.

---

## What we're building

A new window of `AppKind` `clock`:

- **Top half** — big live time + date, read from the `Clock` Slint global (a
  Rust `chrono::Local` timer already ticks it every second; we reuse it for
  free).
- **Bottom half** — an "UPTIME" line and a REFRESH button. Pressing REFRESH (or
  just opening the window) does `POST /api/run {"command":"uptime -p"}` and
  shows the result.

This exercises **both** data paths a real app uses:

1. A **Slint global** already fed by a Rust timer (no new plumbing — pure read).
2. An **`/api` poll** driven by a callback, marshalled back across threads with
   `invoke_from_event_loop`.

> Picking the feature: a UI *app* is something a human or the agent should
> *see and click*. If instead you want a new capability the agent *invokes*,
> that's a **tool** (different SDK) — not this.

---

## The mental model (read this before editing)

Five facts make everything below make sense.

1. **Slint owns the main OS thread; tokio runs on a background pool.**
   `main()` builds the runtime by hand (`tokio::runtime::Builder::new_multi_thread()`),
   creates `AppWindow`, spawns all async work with `rt.spawn(...)`, then calls
   `ui.run()` **last** (it blocks the main thread). **Never** `#[tokio::main]` —
   it hijacks the main thread and Slint deadlocks.

2. **Async tasks never touch UI handles directly.** They marshal every mutation
   through `slint::invoke_from_event_loop(move || { … })`, which queues a
   closure onto the Slint thread. It is fire-and-forget: it returns immediately
   and the closure runs later.

3. **The cross-thread handle is `ui.as_weak()`** → a `slint::Weak<AppWindow>`
   (`Send + Clone`). Clone it per spawned task; call `.upgrade()` inside the
   closure to get the live `AppWindow` back (it may be `None` if the window
   closed — just bail).

4. **Dynamic lists are `Rc<VecModel<T>>` parked in `thread_local!` cells** and
   mutated **only on the Slint thread**. `T` is a struct declared in
   `types.slint` and surfaced to Rust by `slint::include_modules!()`. (Our
   Clock app needs no list, so we skip this — but the WS-event variant at the
   end shows it.)

5. **`AppKind` is the spine.** The enum in `types.slint` is simultaneously: the
   window's `kind`, the launcher ordinal, and the discriminant of the
   `if root.kind == AppKind.x` content switch in `AppWindowFrame`. Its ordinal
   is **mirrored by hand** in Rust (`kind_ordinal` / `kind_from_ordinal` /
   `kind_title` / `default_geom`). These must agree with the enum order, which
   is positional. **Append, never reorder.**

The window manager itself (drag/resize/focus/minimise) is already built and
generic over `kind` — you get all of it for free just by adding your `kind`.

---

## The six edits at a glance

| # | File | What you add |
|---|------|--------------|
| 1 | `ui-slint/src/ui/components/clock_view.slint` | the view component (new file) |
| 2 | `ui-slint/src/ui/types.slint` | the `clock` variant on `AppKind` |
| 3 | `ui-slint/src/main.rs` | `kind_ordinal` / `kind_from_ordinal` / `kind_title` / `default_geom` arms |
| 4 | `ui-slint/src/ui/components/app_window_frame.slint` | import + props/callback + content arm |
| 5 | `ui-slint/src/ui/appwindow.slint` | matching prop/callback on `AppWindow` + pass-through into the window loop |
| 6 | `ui-slint/src/main.rs` | launcher refresh hook + the `on_refresh_clock` callback |
| + | `ui-slint/src/ui/components/start_menu.slint` | a `MenuRow` launching the new ordinal |

We'll do them in dependency order so each step compiles conceptually before the
next.

---

## Step 1 — Write the view component

Create `ui-slint/src/ui/components/clock_view.slint`. A view is an
`export component` (it **must** be `export` or `include_modules!()` can't see
it) that takes `in` properties (data from Rust) and emits `callback`s (intents
to Rust). It reads theme tokens from `Palette` and the time from the `Clock`
global — never hard-code colours, and reuse the existing `Clock` rather than
adding a second timer.

```slint
// ClockView — live wall-clock (from the Clock global) + host uptime (/api/run).
// Rust owns the data: Clock.time/Clock.date are ticked by a 1s timer; `uptime`
// is filled by the on_refresh_clock callback. The REFRESH button re-fetches.
import { Palette } from "../palette.slint";
import { Clock } from "../types.slint";

export component ClockView {
    // Declare 0 preferred/min height so the parent VerticalLayout sizes us via
    // vertical-stretch, not from our internal content (mirrors Dashboard).
    preferred-height: 0px;
    min-height: 0px;

    in property <string> uptime;     // fed from Rust (e.g. "up 3 hours, 12 minutes")
    callback refresh();              // emitted to Rust → POST /api/run

    VerticalLayout {
        padding: 28px;
        spacing: 0px;
        alignment: start;

        // ── Big clock (read straight from the Clock global) ──────────────
        Text {
            text: Clock.time == "" ? "--:--" : Clock.time;
            color: Palette.text-bright;
            font-size: 56px;
            font-weight: 700;
            horizontal-alignment: center;
        }
        Text {
            text: Clock.date;
            color: Palette.text-dim;
            font-size: 13px;
            letter-spacing: 1.5px;
            horizontal-alignment: center;
        }

        Rectangle { height: 28px; }   // spacer

        // ── UPTIME section ───────────────────────────────────────────────
        Text {
            text: "UPTIME";
            color: Palette.text-dim;
            font-size: 9px;
            letter-spacing: 2px;
        }
        Rectangle { height: 8px; }
        Text {
            text: root.uptime == "" ? "waiting…" : root.uptime;
            color: Palette.text;
            font-size: 14px;
            wrap: word-wrap;
        }

        Rectangle { height: 20px; }

        // ── Refresh button ───────────────────────────────────────────────
        // Use a HorizontalLayout (not a bare Rectangle) so the row sizes to its
        // content; alignment: start keeps the button from stretching full-width.
        HorizontalLayout {
            alignment: start;
            Rectangle {
                width: 90px;
                height: 30px;
                border-radius: Palette.radius;
                background: btn.has-hover ? Palette.surface-hi : Palette.surface;
                border-width: 1px;
                border-color: Palette.border;
                animate background { duration: 120ms; }

                btn := TouchArea { clicked => { root.refresh(); } }

                Text {
                    text: "REFRESH";
                    color: Palette.text-dim;
                    font-size: 9px;
                    letter-spacing: 1px;
                    horizontal-alignment: center;
                    vertical-alignment: center;
                }
            }
        }
    }
}
```

Notes that match the existing components:

- `preferred-height: 0px; min-height: 0px;` is the same trick `Dashboard` uses
  so the hosting `VerticalLayout` doesn't try to derive its height from our
  content — we fill via `vertical-stretch: 1` (set by the frame in step 4).
- We import `Clock` from `types.slint` (it's an `export global` there) and read
  `Clock.time` / `Clock.date` directly. No Rust wiring needed for the time — the
  `update_clock` timer in `main.rs` already drives it.
- The empty-state strings ("waiting…", "--:--") matter: an app must render
  gracefully on the Nano femtovg renderer before any data arrives.

> **Gotcha (layout):** a bare `Rectangle` is *not* a layout — its children are
> absolutely positioned and it does not report their size upward. A row built on
> a bare `Rectangle` collapses to ~0 height and overlaps its siblings. Anything
> that must size to its content uses a `VerticalLayout` / `HorizontalLayout`.

---

## Step 2 — Add the `AppKind` variant

Edit `ui-slint/src/ui/types.slint`. Find the enum:

```slint
export enum AppKind { chat, system, sensor, sessions, settings, terminal, council }
```

Append `clock` (do **not** reorder the existing variants — ordinals are
positional and Rust hard-codes them):

```slint
export enum AppKind { chat, system, sensor, sessions, settings, terminal, council, clock }
```

`clock` is now ordinal **7** (chat=0 … council=6).

> If your app needed a list of structured rows, you'd also add an
> `export struct ClockRow { … }` here; it becomes a Rust struct via
> `include_modules!()`. The Clock app needs none.

---

## Step 3 — Mirror the ordinal in Rust

Edit `ui-slint/src/main.rs`. Four `match`es over `AppKind` must each gain a
`Clock` arm. They're all near the top of the file in the window-manager section.

**`kind_ordinal`** (the enum-order mirror — must equal the Slint ordinal, 7):

```rust
fn kind_ordinal(k: AppKind) -> i32 {
    match k {
        AppKind::Chat => 0,
        AppKind::System => 1,
        AppKind::Sensor => 2,
        AppKind::Sessions => 3,
        AppKind::Settings => 4,
        AppKind::Terminal => 5,
        AppKind::Council => 6,
        AppKind::Clock => 7,          // ← add
    }
}
```

**`kind_from_ordinal`** (reverse map; `_ => Chat` stays the catch-all):

```rust
fn kind_from_ordinal(o: i32) -> AppKind {
    match o {
        1 => AppKind::System,
        2 => AppKind::Sensor,
        3 => AppKind::Sessions,
        4 => AppKind::Settings,
        5 => AppKind::Terminal,
        6 => AppKind::Council,
        7 => AppKind::Clock,          // ← add
        _ => AppKind::Chat,
    }
}
```

**`kind_title`** (the window caption):

```rust
fn kind_title(k: AppKind) -> &'static str {
    match k {
        AppKind::Chat => "Chat",
        AppKind::System => "System",
        AppKind::Sensor => "Sensors",
        AppKind::Sessions => "Sessions",
        AppKind::Settings => "Settings",
        AppKind::Terminal => "Terminal",
        AppKind::Council => "Council",
        AppKind::Clock => "Clock",            // ← add
    }
}
```

**`default_geom`** (initial window size; the cascade `step` is added by the
function, so just give width/height):

```rust
fn default_geom(kind: AppKind, n: i32) -> (f32, f32, f32, f32) {
    let (w, h) = match kind {
        AppKind::Chat => (760.0, 540.0),
        AppKind::System => (440.0, 460.0),
        AppKind::Sensor => (560.0, 480.0),
        AppKind::Sessions => (500.0, 520.0),
        AppKind::Settings => (660.0, 560.0),
        AppKind::Terminal => (640.0, 420.0),
        AppKind::Council => (560.0, 560.0),
        AppKind::Clock => (360.0, 380.0),     // ← add
    };
    let step = (n % 6) as f32 * 30.0;
    (72.0 + step, 32.0 + step, w, h)
}
```

> **Why these are non-exhaustive `match`es matter:** the Rust compiler will
> *error* if you forget any of the four (the `AppKind::Clock` variant is now
> unhandled). That's your safety net — a missed arm is a build failure, not a
> runtime surprise. The only silent trap is getting the **ordinal number**
> wrong in `kind_ordinal`/`kind_from_ordinal`: keep it equal to the Slint enum
> position (7).

---

## Step 4 — Host the content in the window frame

Edit `ui-slint/src/ui/components/app_window_frame.slint`. This is the chrome +
content host; each `kind` gets one `if` arm in the content area.

**4a. Import the view** (with the other view imports near the top):

```slint
import { ClockView } from "clock_view.slint";
```

**4b. Declare the new `in` property + `callback`** on `AppWindowFrame`,
alongside the other "App data" / "App callbacks" blocks:

```slint
// with the other `in property` app-data lines:
in property <string> clock-uptime;

// with the other app `callback`s:
callback clock-refresh();
```

**4c. Add the content arm** beside the existing `if root.kind == ...` blocks
(e.g. right after the `council` arm):

```slint
if root.kind == AppKind.clock: ClockView {
    vertical-stretch: 1;
    uptime: root.clock-uptime;
    refresh => { root.clock-refresh(); }
}
```

`AppKind` is already imported in this file, so `AppKind.clock` resolves with no
extra import.

---

## Step 5 — Forward through `AppWindow`

`AppWindowFrame` is instantiated once per window inside the
`for w in root.windows` loop in `ui-slint/src/ui/appwindow.slint`. Data flows
**AppWindow → frame → view**, callbacks flow back **view → frame → AppWindow**,
so `AppWindow` needs the matching property + callback, plus a pass-through into
the loop.

**5a. On `AppWindow`**, add (near the other app properties / callbacks):

```slint
// in the "Properties bound from Rust" block:
in-out property <string> clock-uptime: "";

// in the "Callbacks sent to Rust" block:
callback clock-refresh();
```

**5b. Inside the `for w in root.windows: ... AppWindowFrame { … }`** instance
(the desktop window layer), add the pass-through lines (alongside `stats:`,
`terminal-text:`, etc.):

```slint
clock-uptime: root.clock-uptime;
clock-refresh => { root.clock-refresh(); }
```

That's the full Slint wiring. The Focus-mode tabbed surface (the legacy
full-screen face) does **not** need an entry — new apps live as Desktop windows.

---

## Step 6 — Wire the Rust data + launcher

Two pieces remain in `main.rs`: fetch-on-open, and the `refresh` callback that
actually hits `/api/run`.

**6a. Refresh on open.** In the `on_launch_app` handler there's a `match kind`
that fires a per-app refresh when a window opens (so Settings/Sessions don't
launch empty). Add a `Clock` arm.

> **Naming:** Slint dashes become Rust underscores, and Slint generates an
> `invoke_<callback>` method for every `callback`. We named our callback
> `clock-refresh` on `AppWindow` (step 5), so the generated invoker is
> `invoke_clock_refresh()` and the handler-setter is `on_clock_refresh(...)`.
> (The existing apps happen to name theirs `refresh-settings` →
> `invoke_refresh_settings`; the word order is just whatever you chose in the
> `.slint` — pick one and use it consistently.)

```rust
match kind {
    AppKind::Settings => ui.invoke_refresh_settings(),
    AppKind::Sessions => ui.invoke_refresh_sessions(),
    AppKind::Terminal => start_terminal(&rt_h_term, &term_url, ui.as_weak()),
    AppKind::Clock    => ui.invoke_clock_refresh(),   // ← add (matches `clock-refresh`)
    _ => {}
}
```

**6b. The refresh callback.** In `main()`, near the other `ui.on_refresh_*`
wirings (e.g. just after `on_refresh_settings`), add the handler. This mirrors
the settings/sessions pattern exactly: clone the runtime handle, the shared
`http_client` (it already carries the bearer token), and the HTTP base; spawn
the fetch on tokio; marshal the result back with `invoke_from_event_loop`.

```rust
// ── clock-refresh callback ────────────────────────────────────────────────
// POST /api/run {"command":"uptime -p"} and show the trimmed stdout. The shared
// http_client carries the AGENTD_TOKEN bearer header, so this works whether or
// not the token is set. Runs the fetch on tokio; marshals the string back to
// the Slint thread to set the property.
let rt_h_clk   = rt.handle().clone();
let client_clk = Arc::clone(&http_client);
let base_clk   = http_base.clone();
let ui_weak_clk = ui.as_weak();
ui.on_clock_refresh(move || {
    let client = Arc::clone(&client_clk);
    let base   = base_clk.clone();
    let ui_w   = ui_weak_clk.clone();
    rt_h_clk.spawn(async move {
        let uptime = match client
            .post(format!("{base}/api/run"))
            .json(&serde_json::json!({ "command": "uptime -p" }))
            .timeout(std::time::Duration::from_secs(8))
            .send()
            .await
        {
            Ok(resp) => resp
                .json::<serde_json::Value>()
                .await
                .ok()
                .and_then(|v| v["stdout"].as_str().map(|s| s.trim().to_string()))
                .unwrap_or_default(),
            Err(_) => String::new(),
        };
        // Back to the Slint thread to touch the UI.
        slint::invoke_from_event_loop(move || {
            if let Some(ui) = ui_w.upgrade() {
                ui.set_clock_uptime(uptime.into());
            }
        })
        .ok();
    });
});
```

Three thread-model rules you can see at work here:

- The closure passed to `rt_h_clk.spawn` runs on tokio — it never calls
  `ui.set_*` directly.
- The `invoke_from_event_loop` closure runs on the Slint thread — only there do
  we call `ui.set_clock_uptime(...)`.
- The closure must be `FnOnce() + Send + 'static`: we **move** an owned `String`
  (`uptime`) and a cloned `Weak` (`ui_w`) into it. Don't try to borrow.

Also note the Slint↔Rust string conversion: `uptime.into()` turns the Rust
`String` into the `SharedString` the property expects.

**6c. The launcher entry.** Edit
`ui-slint/src/ui/components/start_menu.slint`. The Clock is a "core" app
(useful for everyone), so add it to the always-shown group (not behind
`Personas.show-tech-apps`). Launch ordinal **7**:

```slint
// in the StartMenu component, with the core MenuRows:
MenuRow { glyph: "🕐"; label: "Clock"; clicked => { root.launch(7); } }
```

If you want it in the Win-98 persona's start menu too, add the matching
`WinMenuRow` in `Win98StartMenu` in the same file:

```slint
WinMenuRow { glyph: "🕐"; label: "Clock"; clicked => { root.launch(7); } }
```

`launch(7)` already routes to `ui.on_launch_app` → `wm_launch` (creates or
reveals the single Clock window) → the per-app refresh you added in 6a. No
other launcher wiring is needed.

---

## Step 7 — Build and run

`.slint` files are compiled by `build.rs` at build time. If you edit a `.slint`
file and `cargo build` doesn't pick up the change, `touch ui-slint/build.rs` to
force a rebuild.

**On your dev machine** (x86 with a display — `SLINT_BACKEND` auto-detects
`winit`):

```bash
# from the repo root
touch ui-slint/build.rs            # only if a .slint edit isn't detected
AGENTD_WS=ws://192.168.0.158:8787/ws \
AGENTD_TOKEN=$(ssh apex1@192.168.0.158 'sudo grep -oP "(?<=AGENTD_TOKEN=).*" /etc/agentd/env') \
cargo run -p ui-slint
```

Open **Start → Clock**. You should see a draggable, resizable, minimisable
window: a big live clock that ticks every second, an UPTIME line that fills in
on open, and a REFRESH button that re-fetches.

> One-time dev deps (link-time, even on desktop): `sudo apt-get install -y
> libfontconfig1-dev libxkbcommon-dev libinput-dev libgbm-dev libegl-dev
> libudev-dev`. Without `libfontconfig1-dev`, `cargo check -p ui-slint` panics.

**On the Pi** (always build on the Pi — never cross-compile):

```bash
# on the Pi, in ~/ApexOS-RS
git pull
cargo build --release --workspace

# hot-swap the UI binary (must stop it first — a running binary can't be
# overwritten: "text file busy")
sudo systemctl stop apexos-rs-ui
sudo cp target/release/ui-slint /usr/local/bin/apexos-rs-ui
sudo systemctl start apexos-rs-ui
sudo journalctl -u apexos-rs-ui -n 10 --no-pager
```

For a quick check without the service:

```bash
AGENTD_WS=ws://localhost:8787/ws SLINT_BACKEND=linuxkms ./target/release/ui-slint
```

---

## Variant: drive an app from a WebSocket event instead of an `/api` poll

If your app should react to live agentd events rather than poll, you push data
from `dispatch_event` instead of a `refresh` callback. The shape:

1. **Declare a `VecModel<T>` (if it's a list) in a `thread_local!`** next to
   `MESSAGES` / `COUNCIL`, create it in `main()`, bind it with
   `ui.set_<name>(ModelRc::from(model.clone()))`, and stash it in the
   thread-local.
2. **Add a `match` arm in `dispatch_event`** keyed on the event's `type` field
   (the gateway sends the raw `Event` enum; tool fields nest under `call`, and
   ids serialise as **bare numbers**). Inside the arm, do all UI mutation inside
   `invoke_from_event_loop` — push into your `VecModel` (only on the Slint
   thread) or `ui.set_<prop>(...)`.

This is exactly how `sensor_reading` updates `SysStats` and how the `council_*`
family drives the `COUNCIL` model. A skeleton:

```rust
// inside dispatch_event(ui_weak, ev, state, ctx):
match ev["type"].as_str() {
    // … existing arms …
    Some("my_event") => {
        let value = ev["payload"]["value"].as_str().unwrap_or("").to_string();
        let w = ui_weak.clone();
        slint::invoke_from_event_loop(move || {
            if let Some(ui) = w.upgrade() {
                ui.set_clock_uptime(value.into());   // or push into a VecModel
            }
        })
        .ok();
    }
    _ => {}
}
```

> A frame that fails to deserialize / match is **silently dropped** — a wrong
> field name produces no error, just nothing happens. Confirm the field names
> against `agentd/crates/core/src/types.rs` (`Event` enum) when an event seems
> to do nothing.

---

## Thread-model gotchas to avoid

| Symptom | Cause | Fix |
|---|---|---|
| UI freezes / never paints | `#[tokio::main]` or async work on the main thread | build the runtime by hand; `ui.run()` is last; all async via `rt.spawn` |
| Panic / nothing happens when a task sets a property | touching a `ui.set_*` from a tokio task | wrap it in `slint::invoke_from_event_loop(move || { … })` |
| `invoke_from_event_loop` closure won't compile | captures aren't `Send`/`'static` | move owned values + a cloned `Weak`; never borrow the `AppWindow` |
| Set a property, no visible change yet | `invoke_from_event_loop` is fire-and-forget | it queues; the closure runs later on the Slint thread — don't assume immediacy |
| `VecModel` mutated from a tokio task corrupts the list | models are not thread-safe | mutate the `Rc<VecModel<T>>` **only** on the Slint thread (inside the invoke closure) |
| Window launches empty | no per-app refresh on open | add your `AppKind` arm to the `on_launch_app` match |
| Compile error: non-exhaustive match on `AppKind` | forgot a `kind_*` arm | add the arm to all four: `kind_ordinal`, `kind_from_ordinal`, `kind_title`, `default_geom` |
| App opens the wrong kind / launcher does nothing | ordinal mismatch | the number in `kind_ordinal`/`kind_from_ordinal`/`launch(N)` must equal the Slint enum position |
| `include_modules!()` can't find your component | component isn't exported | it must be `export component X` |
| `.slint` edit not picked up | `build.rs` didn't re-run | `touch ui-slint/build.rs` |
| `SharedString` type error | passing `&str`/`String` where Slint wants `SharedString` | convert with `.into()`; read back with `.to_string()` |
| Rows overlap / collapse to 0 height | a row built on a bare `Rectangle` | use a `VerticalLayout`/`HorizontalLayout` for content-sized rows |
| Held key does nothing on the Pi | `linuxkms` has no key auto-repeat (backend limitation) | don't design an app around held keys on the Pi |

---

## Where this app's safety lives

The UI is a thin, unprivileged renderer — adding a view, a window, or a
launcher row changes nothing agentd evaluates. But what your app *fetches* is
governed by agentd's policy:

- `/api/run` (our `uptime -p` call) is a **gated** verb. The UI request
  succeeds, but depending on `config/policy.toml` mode, agentd may run it as the
  sandboxed `agentd` user, or emit an approval round-trip. For a passive display
  prefer read-only endpoints; anything that *acts* should expect (and surface) an
  approval.
- The shared `http_client` already carries the `AGENTD_TOKEN` bearer header, so
  REST calls don't 401 when the token is set (install.sh always sets it).
- A new app appears only after a rebuild + hot-swap of `ui-slint` — it's a code
  commit, not something the running agent can grant itself. Treat it like any
  other change: gate → commit → push → rebuild on the Pi. Record the design in
  Cerebro (`store_procedure` for the pattern, `session_save` for the build).

---

## Recap — the loop

`clock_view.slint` (view) → `AppKind.clock` (`types.slint`) → four Rust `kind_*`
arms → frame import + content arm (`app_window_frame.slint`) → `AppWindow`
pass-through (`appwindow.slint`) → launcher row (`start_menu.slint`) → refresh
hook + `on_clock_refresh` (`main.rs`). Build, hot-swap, open Start → Clock.

For the condensed reference and the full file/endpoint/event tables, see
`docs/sdk/05-desktop-apps.md`.
