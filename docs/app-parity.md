# App parity — bringing ApexOS apps to ApexOS-RS

> Original ApexOS (Chromium kiosk) ships ~20 WinBox "apps". ApexOS-RS is the
> native Slint port. This doc tracks which apps are ported, what each needs,
> and the **AI⇄app symbiosis** contract (apps expose agent tools + reflect state).

The north star: ApexOS-RS is **useful beyond chat + coding**. Every app that can
be done in Rust gets a native window; every app where it makes sense gets agent
tools so APEX can drive it (Sonus is the worked example).

---

## Parity matrix

Original app catalogue lives in `../ApexOS/ui/desktop-app.js` (`WIN_DEFAULTS` +
`openWin`). Status as of the parity push:

| Original app | -RS status | Tier | agentd surface |
|--------------|-----------|------|----------------|
| 🤖 Agent (chat) | ✅ `chat_view` | — | `/ws` |
| 📡 Sensors | ✅ `sensor_view` | — | `sensor_reading` WS |
| ⚙ Settings | ✅ `settings_view` | — | `/api/soul`,`/api/policy`,`/api/status` |
| 💻 Terminal | ✅ `terminal_view` | — | `/terminal-ws` |
| ⚗ Council | ✅ `council_view` | — | `/api/council`, `Council*` WS |
| 🏠 Home | ✅ `dashboard` | — | `/api/run` |
| 🕘 Sessions | ✅ `session_view` | — | `/api/sessions` |
| 🎛️ Audio Editor | ⏳ planned | A | `/api/audio/{analyze,process,waveform,files}` + `audio_*` tools |
| 🕸 Mesh | ⏳ planned | A | `/api/mesh/{nodes,peers}` |
| ⚡ Inference | ⏳ planned | A | `/api/backend`,`/api/model(s)`,`/api/vast/*` |
| 📜 Event Log | ⏳ planned | A | `/api/events/recent` |
| 📝 Notes | ✗ | B | local + `write_file`/`read_file` |
| 🎨 Sketchpad | ✗ | B | Slint canvas + **new** `sketch_snapshot` tool |
| 😊 APEX Face | ✗ | B | custom painter + existing `display_face` tool |
| 🎵 Sonus | ✗ (needs attention) | C | `/api/sonus/{files,stream}` exist; `sonus-mcp` plugin commented out |
| 📁 Explorer | ✗ | C | file ops are agent-tools, not HTTP — needs `/api/fs` or agent-driven |
| 📷 Camera | ✗ | C | needs video frames into a custom painter |
| 🧠 Cerebro | ✗ | D | was iframe → external-browser launcher tile |
| 👁 Sensor Head | ✗ | D | was iframe (`:8080`) → external-browser tile |
| 🌐 Browser | ✗ | D | Slint can't embed a webview → external-browser tile |
| 🖥 IDE (Monaco) | ✗ | D | external editor / SSH+vim (deferred, CLAUDE.md) |

**Tiers** = real build effort, not priority:
- **A** — UI-only over agentd routes that already exist. Cheapest, no backend risk.
- **B** — new pure-Rust local app; light or no backend; prime symbiosis ground.
- **C** — needs a new backend slice (route, plugin, or video).
- **D** — webview-bound; in Slint these become dock tiles that `xdg-open` an
  external browser (locked deferral — Slint can't iframe).

---

## Build sequence

1. **PR: Tier A batch** — Event Log · Mesh · Inference (read/light-action viewers).
2. **PR: Audio Editor** — same Tier A bucket, split out for the waveform painter.
3. **PR: Sonus attention** — uncomment/deploy `sonus-mcp`, debug Suno generation,
   wire the player UI to `/api/sonus/*`. (Overlaps the parked Suno-flakiness item.)
4. **Tier B apps** — Notes, Sketchpad (symbiosis showcase), APEX Face.
5. **Tier D launcher tiles** — cheap external-browser stubs for Cerebro/SensorHead/Browser.
6. **New OS-standard apps** — see ideas below.

---

## AI⇄app symbiosis contract

The pattern Sonus proves, generalized: an app that benefits from APEX gets
**(a)** agent tools to drive it, and **(b)** a UI that reflects the resulting
state. New tools live in `tools/crates/apexos-tools`. Per-app, build the tools
when you build the app (no big upfront abstraction).

| App | Symbiosis tool(s) | Direction |
|-----|-------------------|-----------|
| 🎨 Sketchpad | `sketch_snapshot` → hand the canvas (PNG) to APEX | UI → agent |
| 📝 Notes | `notes_read` / `notes_append` | both |
| 🎵 Sonus | music-gen + `audio_*` post-processing (exists) | agent → UI |
| 📁 Explorer | `list_dir`/`read_file`/`write_file` (exist) | both |
| 🗓 Calendar (new) | `schedule_event` / `list_agenda` | both |

---

## New ideas — OS-standard gaps

What a typical desktop OS ships that -RS lacks (Rust-doable, several with strong
symbiosis): **Calculator**, **Clock/Timer/Alarm**, **Image Viewer**, **Color
Picker**, **Screenshot**, **Calendar** (agent scheduling), **Clipboard manager**.
Jot new ideas here as they come up.

---

## How to add an app (recipe)

Reverse-engineered from the existing 7. To add app `Foo`:

1. **`ui-slint/src/ui/types.slint`** — add `foo` to the `AppKind` enum. Add a
   `struct FooItem { … }` if the app has list data.
2. **`ui-slint/src/main.rs`** — add the variant to the four mappers:
   `kind_ordinal`, `kind_from_ordinal`, `kind_title`, `default_geom`.
3. **`ui-slint/src/ui/components/foo_view.slint`** — the view component. Take
   `in property`s for its data; emit callbacks for actions.
4. **`app_window_frame.slint`** — import `FooView`; add `in property`s for its
   data; add `if root.kind == AppKind.foo: FooView { … }`.
5. **`appwindow.slint`** — add `in-out property`s for the data; pass them through
   to `AppWindowFrame` in the `for w in root.windows` block.
6. **`start_menu.slint`** — add a `MenuRow` + `WinMenuRow` (gate behind
   `Personas.show-tech-apps` for deep-tech apps).
7. **`main.rs` `on_launch_app`** — on the new `AppKind`, trigger the data fetch
   (mirror `AppKind::Settings => ui.invoke_refresh_settings()`).
8. **Data fetch** — add an `on_refresh_foo` callback that spawns an async task on
   the tokio runtime, calls `json_get(&client, format!("{base}/api/…"))`, then
   `slint::invoke_from_event_loop` to populate the `VecModel` / set properties.
   Mirror `fetch_sys_stats` / `fetch_settings`.

**Gotcha:** the `for w in root.windows` repeater keys by index — every app's
data is passed to *every* frame (only the active `kind` reads its slice). That's
fine for small models; don't put huge per-frame state on the AppWindow.
