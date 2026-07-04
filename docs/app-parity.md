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
`openWin`). Status as of the 2026-07 freshness sweep — **every Tier A–D app
except Camera and the IDE is shipped**:

| Original app | -RS status | Tier | agentd surface |
|--------------|-----------|------|----------------|
| 🤖 Agent (chat) | ✅ `chat_view` | — | `/ws` |
| 📡 Sensors | ✅ `sensor_view` | — | `sensor_reading` WS |
| ⚙ Settings | ✅ `settings_view` (+ VOICE / PROMPT CACHE / SENSOR ALERTS / LOGIN sections) | — | `/api/soul`,`/api/policy`,`/api/status`,`/api/voice`,`/api/cache`,`/api/sensors/config`,`/api/auth/*` |
| 💻 Terminal | ✅ `terminal_view` | — | `/terminal-ws` |
| ⚗ Council | ✅ `council_view` | — | `/api/council`, `Council*` WS |
| 🏠 Home | ✅ `dashboard` | — | `/api/run` |
| 🕘 Sessions | ✅ `session_view` (+ SELECT mode: bulk export/archive/delete/consolidate) | — | `/api/sessions` + `/api/sessions/{id}` CRUD (delete/archive/consolidate) + `/api/sessions/export` |
| 🎛️ Audio Editor | ✅ `audio_editor_view` (waveform bars + op chain) | A | `/api/audio/{files,analyze,waveform,process}` + `audio_*` tools |
| 🕸 Mesh | ✅ `mesh_view` (roster + per-peer INBOX + PAIR code flow) | A | `/api/mesh/{nodes,peers,inbox,pair/*}` |
| ⚡ Inference | ✅ `inference_view` (+ CACHE BANK card) | A | `/api/backend`,`/api/model(s)`,`/api/vast/*`,`/api/usage` |
| 📜 Event Log | ✅ `event_log_view` | A | `/api/events/recent` |
| 📝 Notes | ✅ `notes_view` | B | `/api/notes/{,read,write}` + `notes_{list,read,append}` tools |
| 🎨 Sketchpad | ✅ `sketchpad_view` — bidirectional | B | `POST /api/sketch` (tiny-skia raster) + `sketch_snapshot` (UI→agent) + `sketch_draw` (agent draws on the canvas, no tool card) |
| 😊 APEX Face | ✅ `face_view` (2D) + GL SDF face (default on GL tiers, `APEX_FACE_GL=0` forces 2D) | B | activity from the WS event stream + emotes from the `display_face` tool (UI consumes `tool_requested` directly) |
| 🎵 Sonus | ✅ player (list+play); gen needs APEX | C | `/api/sonus/{files,stream,play,stop}` (server-side play = `ffmpeg -f alsa`); `sonus-mcp` plugin (ext. hermes-sonus) |
| 📁 Explorer | ✅ `explorer_view` — browse/preview + file verbs (new folder · rename · delete · cut/copy→paste) + USB exo-workspace ("🔌 Use a USB drive" adopt + ⏏ eject); "Attach" stages an image into chat | C | `/api/workspace/{list,read,download,upload,mkdir,delete,rename,move,copy}` + `/api/media/{eject,candidates,prep}` + `list_dir`/`read_file`/`write_file`/`eject_media` tools |
| 📷 Camera | ✗ app window (agent eyes shipped: `camera_capture` tool + `GET /api/snapshot`) | C | needs video frames into a custom painter |
| 🧠 Cerebro | ✅ `web_view` tile | D | external-browser tile (`:8765`, host from agentd) |
| 👁 Sensor Head | ✅ `web_view` tile | D | external-browser tile (`:8080`, host from agentd) |
| 🌐 Browser | ✅ `web_view` URL bar | D | open-arbitrary-URL bar in the Web launcher |
| 🖥 IDE (Monaco) | ✗ | D | external editor / SSH+vim (deferred, CLAUDE.md) |

**Tiers** = real build effort, not priority:
- **A** — UI-only over agentd routes that already exist. Cheapest, no backend risk.
- **B** — new pure-Rust local app; light or no backend; prime symbiosis ground.
- **C** — needs a new backend slice (route, plugin, or video).
- **D** — webview-bound; in Slint these become dock tiles that `xdg-open` an
  external browser (locked deferral — Slint can't iframe).

**New -RS-native apps** (no original-ApexOS counterpart, not in the matrix):
🧮 Calculator (`calculator_view`), 📖 Occipital Reader (`occipital_view` —
follow-along mirror of the agent's web_fetch/search/recall, click-to-steer),
🗂 Work Board (`work_board` — read-only kanban off the WS event stream).
Separately, the **`web/` PWA** is a parallel -RS-owned browser/mobile frontend
(login · chat · tools/approvals · Files · voice), not a Slint app — see
`docs/web-ui.md`.

---

## Build sequence

1. **PR: Tier A batch** — ✅ shipped: Event Log · Mesh · Inference
   (`event_log_view` / `mesh_view` / `inference_view`); Mesh later grew the
   INBOX + pairing flow, Inference the CACHE BANK card.
2. **PR: Audio Editor** — ✅ shipped (`audio_editor_view` + the `audio_*` tool belt).
3. **PR: Sonus player** — ✅ shipped. Library UI over `/api/sonus/files` +
   server-side playback on the device speakers via a new `/api/sonus/{play,stop}`
   (agentd → `ffmpeg -f alsa` — not ffplay; see the Sonus gotcha in CLAUDE.md).
   The actual song *generation* is an **external** Python MCP
   (`hermes-sonus`), not -RS code. Diagnosis of the live flakiness:
   - It's a **3-step async flow** the model must drive: `generate_song` → `task_id`,
     then `check_status_until_done` (blocks ≤300s), then `download_track`. agentd's
     MCP client has **no request timeout**, so the long poll isn't killed by -RS.
   - **#1 cause:** a local model (Nemotron) fumbling that multi-step dance without
     guidance → fix is a **soul.md/skill proposal to APEX** (house rule: propose,
     don't edit). [[config-changes-suggest-to-agent]]
   - **#2 cause:** download-dir seam — the MCP default is `./suno_downloads` (CWD),
     but the gateway/UI look in `/var/lib/agentd/workspace/sonus`. The plugin stanza
     sets `SUNO_DOWNLOAD_DIR` to bridge it; **verify it's set in the live env**.
   - File-tool isolation is **not** a blocker: FS tools are now workspace-confined
     (`tools.rs::confine`, both reads and writes — the old "only `delete_path`
     checks containment" is history), but the sonus dir sits inside the workspace,
     so confinement never bites this flow.
   - Remaining: (a) APEX orchestration-guidance proposal; (b) flesh out the
     `plugins.toml` deploy stanza; (c) confirm live env on the Pi.
4. **Tier B apps** — Notes ✅, APEX Face ✅, Sketchpad ✅. **Tier B complete.**
5. **Tier D launcher tiles** ✅ — consolidated into one `Web` launcher (🌐): Cerebro + Sensor Head tiles (host derived from the agentd URL) + an open-any-URL bar. Opens via the host browser (xdg-open / `$BROWSER`), best-effort, and shows the URL so it's usable from any LAN device.
6. **New OS-standard apps** — see ideas below.

---

## AI⇄app symbiosis contract

The pattern Sonus proves, generalized: an app that benefits from APEX gets
**(a)** agent tools to drive it, and **(b)** a UI that reflects the resulting
state. New tools live in `tools/crates/apexos-tools`. Per-app, build the tools
when you build the app (no big upfront abstraction).

| App | Symbiosis tool(s) | Direction |
|-----|-------------------|-----------|
| 🎨 Sketchpad | `sketch_snapshot` (canvas PNG → APEX) + `sketch_draw` (APEX draws on the canvas) | both |
| 😊 APEX Face | `display_face` — 12 emotes + gaze + intensity, held past turn-end | agent → UI |
| 📝 Notes | `notes_read` / `notes_append` | both |
| 🎵 Sonus | music-gen + `audio_*` post-processing (exists) | agent → UI |
| 📁 Explorer | `list_dir`/`read_file`/`write_file` + `eject_media` (exist) | both |
| 🗓 Calendar (new) | `schedule_event` / `list_agenda` | both |

---

## New ideas — OS-standard gaps

What a typical desktop OS ships that -RS lacks (Rust-doable, several with strong
symbiosis): **Calculator** ✅ (`calculator_view`, pure-UI immediate-execution),
**Clock/Timer/Alarm**, **Image Viewer**, **Color Picker**, **Screenshot**,
**Calendar** (agent scheduling), **Clipboard manager**. Jot new ideas here as
they come up.

---

## How to add an app (recipe)

Reverse-engineered from the shipped set (20 `AppKind`s and counting). To add app `Foo`:

1. **`ui-slint/src/ui/types.slint`** — add `foo` to the `AppKind` enum. Add a
   `struct FooItem { … }` if the app has list data.
2. **`ui-slint/src/main.rs`** — add the variant to the three mappers:
   `kind_from_ordinal`, `kind_title`, `default_geom`.
3. **`ui-slint/src/ui/components/foo_view.slint`** — the view component. Take
   `in property`s for its data; emit callbacks for actions.
4. **`app_window_frame.slint`** — import `FooView`; add `in property`s for its
   data; add `if root.kind == AppKind.foo: FooView { … }`.
5. **`appwindow.slint`** — add `in-out property`s for the data; pass them through
   to `AppWindowFrame` in the `for w in root.windows` block.
6. **`start_menu.slint`** — add a `MenuRow` + `WinMenuRow` (gate behind
   `Personas.show-tech-apps` for deep-tech apps). The Win95 menu's `height:` is
   computed from hard-coded row counts — bump the count or the new row clips.
7. **`main.rs` `on_launch_app`** — on the new `AppKind`, trigger the data fetch
   (mirror `AppKind::Settings => ui.invoke_refresh_settings()`).
8. **Data fetch** — add an `on_refresh_foo` callback that spawns an async task on
   the tokio runtime, calls `json_get(&client, format!("{base}/api/…"))`, then
   `slint::invoke_from_event_loop` to populate the `VecModel` / set properties.
   Mirror `fetch_sys_stats` / `fetch_settings`.

**Gotcha:** the `for w in root.windows` repeater keys by index — every app's
data is passed to *every* frame (only the active `kind` reads its slice). That's
fine for small models; don't put huge per-frame state on the AppWindow.
