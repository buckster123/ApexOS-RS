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

---

## Concepts

**The thread model is the load-bearing invariant.** Slint's event loop owns the
**main OS thread**; tokio runs on a background pool. Violate this and the app
deadlocks at runtime.

- `main()` builds the runtime manually — **never** `#[tokio::main]`
  (`ui-slint/src/main.rs:1002-1005`). It creates the `AppWindow`
  (`main.rs:1007`), spawns all async work via `rt.spawn(...)`, then calls
  `ui.run()` last (`main.rs:1673`), which blocks the main thread.
- Async tasks **never** touch UI handles directly. They marshal every mutation
  through `slint::invoke_from_event_loop(move || { … })`, which queues a closure
  onto the Slint thread (e.g. the WS task at `main.rs:1259`, the stats poll at
  `main.rs:1347`). It's fire-and-forget — returns immediately, runs later.
- The cross-thread handle is `ui.as_weak()` → `slint::Weak<AppWindow>` (`Send +
  Clone`). Clone it per spawned task; `.upgrade()` inside the closure
  (`main.rs:1242`, used everywhere).

**Models live on the Slint thread.** Dynamic lists are `Rc<VecModel<T>>` parked
in `thread_local!` cells (`main.rs:25-54`) and only ever mutated on the Slint
thread — `MESSAGES`, `SESSIONS`, `MODELS`, `TOASTS`, `NOTIF_LOG`, `WINDOWS`,
`COUNCIL`. Each is created in `main()`, handed to the UI via
`ui.set_<name>(ModelRc::from(model.clone()))`, and stashed in its thread-local
(e.g. `main.rs:1034-1036`). `VecModel<T>`'s `T` is a **struct defined in
`types.slint`** and surfaced to Rust by `slint::include_modules!()`
(`main.rs:12`).

**The shell has two modes** (`ShellMode`, `types.slint:7`):
- **Focus** — the legacy full-screen tabbed surface (`appwindow.slint:287-456`),
  switched by `current-view: int`. The tab strip there is hard-coded per view.
- **Desktop** — the windowed face (`appwindow.slint:459-622`): wallpaper →
  window layer → taskbar. This is where "apps" live as draggable windows.

**The window manager** (glowup G2) is hand-rolled, ~250 lines in `main.rs`:
- Rust owns the window *set*: `WINDOWS: VecModel<WindowDesc>` where **model
  order == z-order** (last row paints on top). `WindowDesc` is
  `{id, kind, title, x, y, w, h, minimized, maximized}` (`types.slint:52-62`).
- Slint owns *live drag/resize geometry* (frame-local deltas) and commits back
  to Rust on pointer release — see `AppWindowFrame` (`app_window_frame.slint`),
  the chrome+content host. Round-tripping every pointer move would lag.
- The helpers `wm_launch` / `wm_focus` / `wm_refocus_top` / `wm_update_row`
  (`main.rs:296-367`) run on the Slint thread, driven by callbacks wired at
  `main.rs:1112-1204`.

**`AppKind`** (`types.slint:13`) is the discriminant that ties everything
together: it's the window's `kind`, the launcher ordinal, and the `if
root.kind == AppKind.x` content switch in `AppWindowFrame`. Its ordinal is
mirrored in Rust by `kind_ordinal` / `kind_from_ordinal` (`main.rs:111-133`) —
**these two must agree with the enum order.**

**Personas** (glowup G4, `personas.slint`) bundle theme + chrome + wallpaper +
default shell mode. The `Personas` global derives structural bits from
`current`, including behaviour gates like `show-tech-apps` (`personas.slint:32`)
that hide deep-tech apps from the warm/simple persona. New apps decide whether
they're "core" (always visible) or gated behind `Personas.show-tech-apps`.

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
export enum AppKind { chat, system, sensor, sessions, settings, terminal, council, myapp }
```

Append, don't reorder — ordinals are positional and Rust hard-codes them.

If your app needs structured rows (a list), define the row struct here too, e.g.
`export struct MyRow { name: string, value: float }`. It becomes a Rust struct
via `include_modules!()`.

### 3. Mirror the ordinal in Rust — `ui-slint/src/main.rs`

Add the arm to **both** `kind_ordinal` (`main.rs:111`) and `kind_from_ordinal`
(`main.rs:123`), plus `kind_title` (`main.rs:268`) and `default_geom`
(`main.rs:282`):

```rust
// kind_ordinal
AppKind::MyApp => 7,
// kind_from_ordinal
7 => AppKind::MyApp,
// kind_title
AppKind::MyApp => "My App",
// default_geom
AppKind::MyApp => (520.0, 460.0),
```

### 4. Host the content in the window frame — `app_window_frame.slint`

Import the view at the top, declare any new `in` properties + `callback`s on
`AppWindowFrame`, and add a content arm beside the existing ones
(`app_window_frame.slint:315-387`):

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
`in-out property`/`callback` on `AppWindow` (near `:43-56`/`:90`), and pass
them into the `AppWindowFrame` in the `for w in root.windows` loop
(`appwindow.slint:509-564`):

```slint
// on AppWindow
in-out property <string> myapp-headline: "";
callback myapp-do-thing();
// inside the AppWindowFrame instance in the desktop window loop
myapp-headline: root.myapp-headline;
myapp-do-thing => { root.myapp-do-thing(); }
```

### 5. Add the launcher entry + wire Rust data

**Launcher** — `start_menu.slint`. Core app → always-shown row; deep-tech →
gate it. Use the new ordinal (7):

```slint
// always-shown:
MenuRow { glyph: "✨"; label: "My App"; clicked => { root.launch(7); } }
// or deep-tech, hidden by the simple persona (start_menu.slint:86-88):
if Personas.show-tech-apps: MenuRow { glyph: "✨"; label: "My App"; clicked => { root.launch(7); } }
```

`launch(ord)` already routes to `ui.on_launch_app` (`main.rs:1118`), which calls
`wm_launch` and fires a per-app refresh hook — add yours to that `match` if the
window should fetch on open (like Settings/Sessions do, `main.rs:1124-1129`):

```rust
AppKind::MyApp => ui.invoke_refresh_myapp(),
```

**Data plumbing** — in `main()`, wire the property and the callback. For an
`/api` poll, add a `refresh` callback that spawns a fetch and marshals the
result back (mirror `on_refresh_settings`, `main.rs:1548-1564`):

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

For a **WS-event-driven** app, add a `match` arm in `dispatch_event`
(`main.rs:1686`) keyed on the event's `type`, exactly like `sensor_reading`
(`main.rs:1943`) or the `council_*` family (`main.rs:1990-2112`). Push into your
`VecModel` (parked in a new thread-local) or `ui.set_…` a property — always
inside `invoke_from_event_loop`.

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

**2. `types.slint`** — `export enum AppKind { …, council, logs }`

**3. `main.rs`** — add `AppKind::Logs => 7` to `kind_ordinal`, `7 =>
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

**5. `start_menu.slint`** — deep-tech, so gate it:
`if Personas.show-tech-apps: MenuRow { glyph: "📜"; label: "Logs"; clicked => { root.launch(7); } }`

**6. `main.rs` wiring** — refresh on open (`on_launch_app` match,
`main.rs:1124`): `AppKind::Logs => ui.invoke_logs_refresh(),`. Then the
callback:

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
stats poll (`main.rs:1341-1360`) and gate it on the window existing.

> Note: `journalctl`/`run_command` is a **gated** verb (see Policy below). The
> *UI* call to `/api/run` succeeds, but agentd may emit an approval-pending
> round-trip or reject it depending on policy mode. For a kiosk daemon the
> command runs as the `agentd` user inside the systemd sandbox.

---

## Policy / safety

A UI app is a renderer; it has no privileges of its own. Its safety boundary is
entirely **agentd's**, reached over the wire.

- **No approval for the UI itself.** Adding a view, a window, or a launcher entry
  changes nothing agentd evaluates. Drawing pixels and reading properties is
  unprivileged. The approval `PolicyEngine` lives in agentd
  (`agentd/crates/plugins/src/policy.rs`); the UI only *renders* its decisions
  (the tool card + approve/reject buttons in `tool_card.slint`, wired via
  `AgentBridge.approve-tool`/`reject-tool`, `main.rs:1364-1396`).
- **What your app *fetches* may be gated.** Every `/api/*` call your app makes is
  subject to the same auth + policy as any client. The shared `http_client`
  carries the bearer token (`main.rs:1227-1239`); without it, calls 401 when
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
  (show "waiting…"/empty states, as `terminal_view.slint:32` does). Persona
  default mode is tier-clamped to Focus on femtovg (`apply_persona`,
  `main.rs:208`); your app should not assume Desktop mode exists.

---

## Reference

### Files to edit for a new app

| File | What you add |
|------|--------------|
| `ui-slint/src/ui/components/<name>_view.slint` | the view `export component` (new file) |
| `ui-slint/src/ui/types.slint` | the `AppKind` variant + any row/data structs |
| `ui-slint/src/ui/components/app_window_frame.slint` | import + `in` props + `callback`s + `if root.kind == AppKind.<x>` content arm |
| `ui-slint/src/ui/appwindow.slint` | matching `in-out`/`callback` on `AppWindow` + pass-through into the `for w in root.windows` `AppWindowFrame` |
| `ui-slint/src/ui/components/start_menu.slint` | a `MenuRow` (core) or `if Personas.show-tech-apps: MenuRow` (deep-tech) launching the ordinal |
| `ui-slint/src/main.rs` | `kind_ordinal`/`kind_from_ordinal`/`kind_title`/`default_geom` arms; `on_launch_app` refresh hook; the refresh callback or `dispatch_event` arm |

### `AppKind` ordinals (`types.slint:13`, mirrored `main.rs:111-133`)

| Variant | Ordinal | Title | Default geom (w,h) |
|---------|---------|-------|--------------------|
| `chat` | 0 | Chat | 760×540 |
| `system` | 1 | System | 440×460 |
| `sensor` | 2 | Sensors | 560×480 |
| `sessions` | 3 | Sessions | 500×520 |
| `settings` | 4 | Settings | 660×560 |
| `terminal` | 5 | Terminal | 640×420 |
| `council` | 6 | Council | 560×560 |

### Thread-local models (`main.rs:25-54`)

| Thread-local | Type | Bound via | Mutate on |
|--------------|------|-----------|-----------|
| `MESSAGES` | `VecModel<MessageItem>` | `ui.set_messages` | Slint thread only |
| `SESSIONS` | `VecModel<SessionItem>` | `ui.set_sessions` | Slint thread only |
| `MODELS` | `VecModel<ModelItem>` | `ui.set_available_models` | Slint thread only |
| `TOASTS` | `VecModel<ToastItem>` | `Notifications.set_toasts` | Slint thread only |
| `NOTIF_LOG` | `VecModel<ToastItem>` | `Notifications.set_log` | Slint thread only |
| `WINDOWS` | `VecModel<WindowDesc>` | `ui.set_windows` | Slint thread only |
| `COUNCIL` | `VecModel<CouncilAgent>` | `ui.set_council_agents` | Slint thread only |

### Window-manager callbacks (`appwindow.slint:92-99`, wired `main.rs:1112-1204`)

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

### Inbound WS events you can drive an app from (`dispatch_event`, `main.rs:1686`)

| Event `type` | Key fields | Existing consumer |
|--------------|------------|-------------------|
| `agent_text` | `delta` | chat bubble |
| `turn_complete` | `session` | clear busy, TTS |
| `tool_requested` | `call:{id,tool,args}` | tool card |
| `tool_result` | `call:<id>, output:{ok,content}` | tool card update |
| `approval_pending` | `call:{id,tool,args}` | approve/reject |
| `sensor_reading` | `reading:{kind,…}` | dashboard/sensor |
| `council_*` | `topic/agent_id/delta/…` | council view |
| `sub_agent_started` | `child` | taskbar badge |
| `wake_triggered` | — | start recording |

> The gateway broadcasts **every event** with a bare-number `session` field; a
> frame that fails to deserialize is silently dropped. For a multi-client build,
> filter on `session` (see `architecture.md` "Multi-client caveat").

### REST endpoints (UI base = `ws_to_http(AGENTD_WS)`, `main.rs:733`)

| Endpoint | Method | Used by |
|----------|--------|---------|
| `/api/run` | POST `{command}` | stats poll, shell (gated) |
| `/api/status` `/api/soul` `/api/models` | GET | settings fetch |
| `/api/soul` | POST `{content}` | save soul (gated) |
| `/api/policy` `/api/model` | POST | settings (gated) |
| `/api/sessions` | GET | session list |
| `/api/record/start` `/api/record/stop` | POST | voice |
| `/api/speak` | POST `{text}` | TTS |
| `/api/power` | POST `{action}` | power modal (gated) |
| `/terminal-ws` | WS | terminal PTY |

### Slint gotchas that bite new views (`slint-notes.md`)

| Gotcha | Fix |
|--------|-----|
| A `Flickable`/scroll pane doesn't scroll | bind `viewport-height: <col>.preferred-height` to the content layout (`slint-notes.md` + `terminal_view.slint:26`) |
| Scroll-to-bottom needs a trigger | bump an `in property <int> scroll-tick` from Rust; `changed scroll-tick => { flick.viewport-y = min(0px, -(flick.viewport-height - flick.height)); }` |
| A bare `Rectangle` row collapses to 0 height | rows that must size to content need a `VerticalLayout`/`HorizontalLayout`, not a `Rectangle` (`CLAUDE.md` gotchas) |
| `.slint` edit not recompiled | `touch ui-slint/build.rs` |
| `include_modules!()` can't find your component | it must be `export component X` |
| `SharedString` mismatch | convert Rust `String`/`&str` with `.into()`; read back with `.to_string()` |
| `invoke_from_event_loop` type error | closure must be `FnOnce() + Send + 'static`; clone/`Arc` captures, don't borrow |
| No key auto-repeat on linuxkms | backend limitation; don't design an app around held keys on the Pi |
