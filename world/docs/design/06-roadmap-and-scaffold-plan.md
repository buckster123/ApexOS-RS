# 06 — Roadmap & Scaffold Plan

> apexos-world — an AI-native 3D world interface for ApexOS-RS.
> Branch `proto/world-3d`, scaffolded under `world/`, extracted to its own repo later.
> This doc defines the build **milestones** (M0–M3) with acceptance criteria, then the
> **exact scaffold spec** the scaffolding phase implements: workspace layout, crates,
> per-file contents, and an honest "what compiles in the first scaffold" note.

apexos-world is **just another agentd client**, peer to `ui-slint` and the browser PWA.
It speaks agentd's real WebSocket `Event` protocol on `ws://HOST:8787/ws`. New capability
(agent vision) extends agentd through its documented MCP-plugin surface — never by forking
core.

---

## 0. Ground-truth anchors (verified against the tree)

These are load-bearing facts the whole plan rests on. Verified by reading source, not docs:

- **Wire types**: `agentd/crates/core/src/types.rs` — `Event` enum, `ToolCall`, `ToolOutput`,
  `ToolSpec`, `Message`, `ContentBlock`. `SessionId(u64)` / `ActionId(u64)` serialize as
  **bare numbers**. `Event` is `#[serde(tag = "type", rename_all = "snake_case")]`.
- **Handshake** (`agentd/crates/gateway/src/lib.rs:180-304`): on connect the gateway
  **immediately** assigns a session and pushes
  `{"type":"session_init","session_id":<n>,"history":[...]}`. The client does **not** send
  `session_init`. To *resume*, the client sends `{"type":"hello","resume_session":<n>}` and
  gets a fresh `session_init` carrying that session's history.
- **Inbound framing**: the gateway injects `frame["session"] = <session_id>` into every
  client→server frame before `serde_json::from_value::<Event>`. So a `UserPrompt` sent by
  the client **omits** `session`; a frame that fails to deserialize is **silently dropped**.
- **Multi-client caveat** (`docs/architecture.md:144`): outbound frames carry `session`;
  **clients MUST filter on it** or they render another session's output. The world client is
  inherently multi-session (one station = one session), so this filter is mandatory, not
  optional.
- **MCP plugin contract** (`agentd/crates/plugins/src/mcp.rs`, `cerebro-mcp/src/*`): a plugin
  is a child process speaking **newline-delimited JSON-RPC 2.0 over stdio**. agentd sends
  `initialize` → expects a result → sends `notifications/initialized` → `tools/list` →
  `tools/call`. **Logs go to stderr; stdout is JSON-RPC only.** Tool result content is
  `{"content": [{"type":"text","text": "..."}], "isError": bool}`.
- **Hardware target**: Pro/Standard tier only (`winit` backend, GPU/wgpu). Explicitly NOT
  Pi Zero/Nano. See `docs/architecture.md` tier table + root `CLAUDE.md`.

> ASSUMPTION (cross-dimension): the brief cites `docs/sdk/01-core-and-protocol.md`,
> `02-mcp-plugins.md`, `03-adding-tools.md`, `05-desktop-apps.md`. **These files do not exist
> in the tree today.** All protocol facts above are taken directly from source. If the SDK
> docs are authored later, they must match the source facts cited here; where they differ,
> source wins.

---

## 1. The shape of the thing (so the milestones make sense)

```
                 ws://HOST:8787/ws  (agentd Event protocol — UNCHANGED)
                          │
   ┌──────────────────────┼───────────────────────────────────────────┐
   │  apexos-world (one process, Pro/Standard tier, winit + wgpu)       │
   │                                                                    │
   │   Bevy 3D scene  ◄── render-to-texture ──►  Slint window + HUD     │
   │   (hub, stations, avatars, picking)         (per-station UI panels)│
   │            ▲                                         ▲             │
   │            │  world-protocol: N WS connections       │             │
   │            │  (one per active session/station)       │             │
   └────────────┼─────────────────────────────────────────┼────────────┘
                │                                          │
                │ each station ⇄ one agentd session        │
                ▼                                          ▼
        Station "Chat" → session 42        Station "Sensors" → /api/run poll
        Avatar "FORGE" → session 51        Station "Council" → council_* events

   Agent vision (M2):  agentd ── tools/call agent_vision.snapshot ──► world-vision
                       world-vision ── HTTP/IPC ──► apexos-world render snapshot
```

Key architectural commitments (assumed from sibling design dims — name them if they drift):

- **Slint owns the OS window + event loop** (never `#[tokio::main]`; root `CLAUDE.md` rule).
- **Bevy renders to an offscreen wgpu texture** shared with Slint via the
  `unstable-wgpu-29` integration; Slint displays it as an `Image`. (Pattern A in the seed
  skill.)
- **A "station" or "avatar" is a world entity bound to one agentd session.** Activating it
  fills the view with that session's function-specific UI surface.
- **Each bound session = one WS connection** (simplest correct model: avoids the per-frame
  `session` demux being load-bearing for *correctness* — though we still filter defensively).
  Revisit single-multiplexed-connection only if connection count becomes a problem.

---

## 2. Build roadmap

Each milestone is a vertical slice that runs end-to-end against a **live agentd**
(dev: the Pi over LAN, `AGENTD_WS=ws://192.168.0.158:8787/ws` + token; or a local agentd).

### M0 — Hub scene + camera + connect + ONE real station

**Goal:** prove the spine. A 3D hub you can fly through, one station you walk up to and
activate, and that station is a working chat surface bound to a real agentd session.

Scope:
- Bevy hub scene: ground plane, lighting, one camera (free-fly or orbit), one station mesh
  (a lit quad / simple prism — placeholder, no glTF yet).
- Slint window hosting the Bevy render texture full-bleed, plus a minimal HUD overlay
  (connection status, FPS).
- `world-protocol` connects to agentd, receives `session_init`, captures `session_id`.
- Activating the station opens a chat panel (Slint overlay): typing sends
  `{"type":"user_prompt","text":...}` (no `session` field — gateway injects it).
- Inbound `agent_text` deltas (filtered to this station's `session`) stream into the panel;
  `turn_complete` clears the busy state.

Acceptance criteria:
- [ ] `cargo run -p world-app` opens a window; FPS counter shows >30 on a Standard-tier GPU.
- [ ] Camera moves (WASD/mouse or orbit); the station is visible and reachable.
- [ ] On launch, log shows `session_init` received with a numeric `session_id`.
- [ ] Walk to station → activate (key/click) → chat panel appears.
- [ ] Type "hello" → agentd streams a reply → text renders live in the panel.
- [ ] Disconnecting agentd shows a "reconnecting" HUD state; it reconnects when agentd returns.
- [ ] Frames whose `session` ≠ this station's session are ignored (logged, not rendered).

### M1 — Multiple stations + avatars + picking/activation

**Goal:** the world becomes navigable and populated; activation is generalized.

Scope:
- A small fixed set of stations/avatars laid out in the hub, each with a `kind`
  ("chat", "sensors", "council", "terminal", "memory"). Each binds to its own session
  (its own `world-protocol` connection) lazily on first activation.
- Raycast picking: hover highlights an entity; activate fills the view with that entity's
  function UI (Slint panel chosen by `kind`).
- Avatar = an entity with an `agent_id` (e.g. `FORGE`); its panel is a chat bound to a
  session, with a nameplate/persona color.
- "Sensors" station consumes `sensor_reading` events (already on the bus) and renders an
  IAQ/thermal readout — conceptually reuses ui-slint's sensor view.
- Deactivate returns to free navigation.

Acceptance criteria:
- [ ] ≥3 entities of ≥2 distinct `kind`s render in the hub.
- [ ] Mouse hover highlights the entity under the cursor (ray pick correct within a pixel or two).
- [ ] Activating a "chat" avatar and a separate "chat" station yields **two independent
      sessions** — text from one never bleeds into the other (session filter proven).
- [ ] The "sensors" station shows a live `air_quality` / `thermal_frame` reading when the
      bridge is feeding agentd.
- [ ] Tool-call events (`tool_requested` / `approval_pending` / `tool_result`) render as a
      card with approve/reject that sends `{"type":"user_approval","action":<id>,"granted":bool}`.
- [ ] No frame-rate cliff with all stations bound (still >30 FPS).

### M2 — Agent-vision MCP plugin

**Goal:** an agent embodied in the world can "see" through its avatar's camera.

Scope:
- `world-vision`: an MCP-over-stdio plugin (the cerebro-mcp pattern) registered in agentd's
  `plugins.toml`. Exposes a tool, e.g. `world_snapshot { view?: string, quality?: string }`.
- When agentd calls it, `world-vision` requests a render snapshot from the running
  apexos-world process over a **local IPC channel** (localhost HTTP or a UDS — see Risks),
  receives an image (PNG/JPEG bytes), and returns it as MCP image content.
- apexos-world grows a snapshot endpoint: render the named view (a station camera or the
  avatar's POV) to an offscreen target, encode, return bytes.
- The agent's turn can then reason over what it "saw" (vision-capable model assumed).

Acceptance criteria:
- [ ] `world-vision` passes the MCP handshake: responds to `initialize`, lists
      `world_snapshot` in `tools/list`, logs only to stderr.
- [ ] With the plugin registered, agentd emits `plugin_up` carrying the `world_snapshot` spec.
- [ ] An agent prompt like "look around and describe what you see" triggers
      `tool_requested{tool:"world_snapshot"}` → `tool_result` with image content.
- [ ] The returned image is a real frame from the live world (visually matches the on-screen
      view at request time, within one frame).
- [ ] If apexos-world is not running, the tool returns `isError:true` with a clear message
      (agentd's bounded-timeout error path is not tripped).

### M3 — VR / Quest 3 path

**Goal:** the same scene, stereo, on a headset. Treated as a **separate render/input path**,
not a rewrite.

Scope:
- Feature-gated `bevy_mod_openxr` (or `bevy_oxr`) render + input plugin; the hub/station/
  avatar entities are reused unchanged.
- Slint HUD either hidden, rendered as a 3D quad in-world, or replaced by in-world panels.
- VR controller rays drive the same pick/activate path as the desktop mouse ray.
- `world-protocol` and `world-vision` are untouched (they don't know about rendering).

Acceptance criteria:
- [ ] A `--features vr` build launches on Quest 3 (via PCVR link or standalone arm64) and
      renders the hub in stereo at the headset's native rate.
- [ ] Head tracking moves the camera; controller ray picks and activates a station.
- [ ] At least one station's UI is readable and interactable in-headset.
- [ ] Desktop build still works unchanged with `vr` off (no regression).

> M3 is explicitly **post-prototype**. The scaffold leaves seams for it (feature flag,
> render path isolated) but ships nothing VR-functional.

---

## 3. Scaffold spec (what the scaffolding phase builds)

A Cargo **workspace** under `world/`. Three crates with a hard dependency discipline:
`world-protocol` is the only crate every other thing depends on, and it stays
**heavy-dep-free** so it `cargo check`s on a headless machine with no GPU/X/fontconfig.

```
world/
├── Cargo.toml                 # [workspace] members, shared deps, release profile
├── README.md                  # how to run; tier note; AGENTD_WS env
├── rust-toolchain.toml        # pin (matches Pi/dev), optional
├── docs/
│   └── design/
│       └── 06-roadmap-and-scaffold-plan.md   # (this file)
└── crates/
    ├── world-protocol/        # (a) types mirror + WS client — light deps only
    ├── world-app/             # (b) Bevy + Slint binary skeleton — heavy deps
    └── world-vision/          # (c) snapshot MCP plugin skeleton — light deps
```

### (a) `world-protocol` — agentd types + WS client

**Purpose:** the wire boundary. Mirrors agentd's `Event`/intent types and provides a
tokio-tungstenite client with reconnect + per-session demux. **Must `cargo check` clean
with only `tokio` + `tokio-tungstenite` + `serde`/`serde_json` + `futures-util`** — no
Bevy, no Slint, no wgpu, no fontconfig. This is the crate later extracted as the reusable
"agentd client SDK".

> DECISION: **mirror** the types (copy the `serde` shapes), do not depend on `apexos-core`.
> Rationale: keeps `world/` extractable to its own repo with zero path deps into the agentd
> workspace, and `apexos-core` is not published as a library (see root `CLAUDE.md` deferred
> item). The mirror is small and the protocol is `#[serde(tag="type")]` + bare-number IDs —
> trivially re-derivable. A `// MIRRORS agentd/crates/core/src/types.rs — keep in sync` banner
> documents the coupling. Test against the live daemon catches drift.

Files:

| File | Contents |
|------|----------|
| `Cargo.toml` | deps: `tokio` (rt-multi-thread, macros, sync, time, net), `tokio-tungstenite`, `futures-util`, `serde` (derive), `serde_json`, `tracing`. **No** GPU/UI deps. |
| `src/lib.rs` | re-export `events`, `intents`, `client`; crate docs with the MIRRORS banner. |
| `src/ids.rs` | `SessionId(pub u64)`, `ActionId(pub u64)` — `Serialize`/`Deserialize` as transparent so they round-trip as bare numbers. `#[serde(transparent)]` newtypes. |
| `src/events.rs` | `Event` enum mirror (`#[serde(tag="type", rename_all="snake_case")]`): at minimum `SessionInit{session_id, history}`, `AgentText`, `AgentThinking`, `ToolRequested`, `ToolResult`, `TurnComplete`, `ApprovalPending`, `SensorReading`, `WakeTriggered`, `SubAgentStarted`, `Error`, and the council variants. Plus `ToolCall`, `ToolOutput`, `ToolSpec`, `SensorReading`, `Message`/`ContentBlock` structs. Unknown variants tolerated via a `#[serde(other)] Unknown` fallback so new agentd events don't crash the client. |
| `src/intents.rs` | outbound frames as serializers: `user_prompt(text)`, `user_approval(action, granted)`, `user_cancel()`, `hello(resume_session)`. Each returns a `serde_json::Value` / String that **omits `session`** (gateway injects it). |
| `src/client.rs` | `WorldClient`: `connect(url, token: Option<String>) -> (EventRx, IntentTx)`. Spawns a tokio task: connect → on first `session_init` capture `session_id` → forward parsed `Event`s on an mpsc, forward outbound intents to the socket. Reconnect loop with backoff. Exposes `session_id()`. **Filters/labels every inbound event with its session** so callers can demux. |
| `src/lib_tests.rs` or `#[cfg(test)]` in modules | round-trip tests: serialize a `user_prompt`, assert no `session` key + correct `type`; deserialize a captured `session_init` and `agent_text` fixture; assert `ActionId` ↔ bare number. These run on headless CI. |

### (b) `world-app` — Bevy + Slint binary

**Purpose:** the actual world. Hosts Slint (window + HUD), embeds Bevy (3D scene via shared
wgpu texture), drives N `WorldClient`s. This is the crate that needs the full GPU/UI
toolchain.

Files:

| File | Contents |
|------|----------|
| `Cargo.toml` | deps: `world-protocol` (path), `slint` (`backend-winit`, `renderer-wgpu`, `unstable-wgpu-29`), `bevy` (gated behind a `viz` feature, default on for desktop), `tokio`, `tracing`/`tracing-subscriber`, `image`, `anyhow`. `[features] default=["viz"]`, `vr=["dep:bevy_mod_openxr"]` (dep optional, **off**). `[build-dependencies] slint-build`. |
| `build.rs` | `slint_build::compile("ui/world.slint")`. |
| `src/main.rs` | manual multi-thread tokio runtime (NOT `#[tokio::main]`); `BackendSelector::require_wgpu_29(...)`; create Slint `WorldWindow`; `set_rendering_notifier` to grab the shared `device`/`queue` and init Bevy render-to-texture; spawn WS task(s); `slint::Timer` @16ms drains event channels → updates Slint models + forwards to Bevy; `app.run()`. |
| `src/app_state.rs` | `Station { id, kind, agent_id: Option<String>, world_pos, session: Option<SessionId> }`; the station registry; `Rc<VecModel<…>>` Slint models for the active panel; the per-station chat buffers. |
| `src/scene.rs` | Bevy world: `build_world_app()`, `setup_hub` (ground, light, camera, station meshes), `camera_controller` system, `pick_system` (ray from cursor/controller → highlight + activation event). Entities tagged with `StationEntity{id}`. |
| `src/bridge.rs` | the glue: channels Slint↔Bevy↔WS. `WorldEvent` (WS→app: text delta, tool req, sensor reading, …) and `WorldCommand` (app→WS: prompt, approval, cancel; app→Bevy: activate/deactivate, snapshot request). Owns the **session→station** map and the defensive `session` filter. |
| `src/sessions.rs` | spawns one `world_protocol::WorldClient` per activated station; maps inbound events to the owning station; reconnect status surfaced to HUD. |
| `src/snapshot.rs` | (M2 seam) render a named view to an offscreen target, encode PNG via `image`, hand bytes to whatever `world-vision` asks. Local IPC server (localhost HTTP, default `127.0.0.1:8799`) — **stubbed** in the scaffold (returns a fixed placeholder image), wired in M2. |
| `ui/world.slint` | `WorldWindow`: full-bleed `Image { source: viewport_texture }`; HUD overlay (connection status, FPS); a `station_panel` area that swaps content by `active_kind` (`if active_kind=="chat": ChatPanel …`). `ChatPanel`, `ToolCard`, `SensorPanel` components; `VecModel`-driven message list. Callbacks: `send_prompt(string)`, `approve(int,bool)`, `activate(string)`, `request_snapshot()`. |
| `ui/components/*.slint` | `chat.slint`, `tool_card.slint`, `sensor.slint`, `hud.slint` — split for sanity, imported by `world.slint`. |

### (c) `world-vision` — snapshot MCP plugin

**Purpose:** the agent-vision tool. Standalone binary, MCP-over-stdio, modeled exactly on
`cerebro-mcp`. **Light deps** (no GPU): it only talks JSON-RPC on stdio and HTTP to the
running world-app for the actual pixels.

Files:

| File | Contents |
|------|----------|
| `Cargo.toml` | deps: `tokio` (rt-multi-thread, io, macros), `serde`/`serde_json`, `anyhow`, `tracing`/`tracing-subscriber`, an HTTP client for the world-app IPC (`reqwest` blocking or `ureq`), `base64` (for image content). **No** Bevy/Slint/wgpu. |
| `src/main.rs` | `#[tokio::main]` (this is a leaf process, the Slint-main-thread rule does NOT apply); tracing to **stderr**; `StdioTransport`; `initialize` → loop on `tools/list` / `tools/call`; notifications (`id==null` / `notifications/*`) get no reply. Mirrors `cerebro-mcp/src/main.rs`. |
| `src/transport.rs` | newline-delimited JSON over stdin/stdout — copied from `cerebro-mcp/src/transport.rs` (`read_line`, `EOF` bail, write+flush+`\n`). |
| `src/dispatch.rs` | `handle_initialize` (protocolVersion `2024-11-05`, serverInfo `world-vision`), `tools_list` (one tool: `world_snapshot`), `dispatch_tool` → on success return `{content:[{type:"image", data, mimeType}]}` or text; on failure `isError:true`. |
| `src/tools.rs` | `world_snapshot` schema: `{ view?: string, quality?: "low"|"med"|"high" }`; the call fetches from world-app's snapshot endpoint and wraps the bytes. |
| `src/snapshot_client.rs` | HTTP client to `127.0.0.1:8799/snapshot?view=…&quality=…` (the world-app IPC); returns image bytes + mime; clear error if world-app is down. |
| `config/plugins.snippet.toml` | the stanza to paste into agentd's `plugins.toml` to register `world-vision` (command = path to the built binary, env for the IPC port). Documents the wiring; not auto-installed. |

---

## 4. What WILL and WON'T compile in the first scaffold (honest note)

**WILL compile + run / test on a headless dev box (no GPU):**
- `cargo check -p world-protocol` and `cargo test -p world-protocol` — pure tokio/serde,
  no system libs. This is the CI gate.
- `cargo check -p world-vision` / `cargo build -p world-vision` — leaf binary, light deps.
  It can complete the MCP handshake against a hand-fed stdin even before world-app exists
  (snapshot calls return a clear "world-app unreachable" error).

**WILL compile but needs the GPU/UI toolchain (Standard/Pro tier dev machine):**
- `world-app` requires `libfontconfig1-dev`, `libxkbcommon-dev`, plus a working wgpu/GPU
  stack. On the existing dev box those fontconfig/xkb link deps are already documented
  (root `CLAUDE.md`). It links winit + wgpu; **no** `backend-linuxkms` feature (this client
  is desktop/VR only, never Pi KMS).

**WON'T work / is stubbed in the first scaffold (by design):**
- The **Slint↔Bevy shared-texture handoff** is the single highest-risk integration. The
  scaffold wires `set_rendering_notifier` and a Bevy app, but the first cut may render Bevy
  to its own texture and blit, or show a placeholder texture, until the zero-copy share is
  proven. M0 acceptance only requires *a* 3D view on screen, not zero-copy.
- **Agent vision end-to-end** — `world-vision` returns a placeholder image; `world-app`'s
  `/snapshot` returns a fixed PNG. Real wiring is M2.
- **VR** — feature flag exists, `bevy_mod_openxr` is an *optional, off* dependency; the `vr`
  feature does not yet build a functional path. Pure seam.
- **glTF avatars, persona skins, council/terminal/memory panels** — entities are placeholder
  meshes; only the `chat` panel is functional in M0/M1. Other `kind`s are stubbed panels.

> Rule of thumb: **`world-protocol` is always green on CI; `world-vision` is always
> buildable; `world-app` is the only crate that needs the heavy toolchain and the only one
> that carries known-incomplete integrations.**

---

## 5. Risks & mitigations

| Risk | Severity | Mitigation |
|------|----------|------------|
| **Slint + Bevy shared wgpu texture** — version-locked (`unstable-wgpu-29`), API churn, both want to own the wgpu device. The seed skill flags Windows symbol clashes. | High | Pin `slint = 1.16` + Bevy version that targets the same wgpu (verify before scaffolding). Pattern A only (Slint hosts, Bevy renders to texture). Keep a "blit fallback" path so M0 ships even if zero-copy import fails. Spike this in M0, not later. |
| **Two event loops** (Slint owns main thread; Bevy normally wants `App::run`). | High | Do **not** call `bevy::App::run`. Drive Bevy with manual `app.update()` from the Slint `Timer`/notifier (seed `main.rs` pattern). Never `#[tokio::main]` (root `CLAUDE.md`). |
| **Perf** — N live sessions + 3D scene + per-frame texture copy on a Standard-tier GPU. | Med | One WS connection per *active* station, lazily created. Cap bound sessions. Profile early (target >30 FPS in every milestone's acceptance). Pro tier is the comfortable target; Standard is the floor. |
| **Session demux correctness** — outbound frames carry `session`; mixing them corrupts UIs (`architecture.md:144`). | Med | One connection per session sidesteps shared-stream demux for correctness; *still* filter defensively on `session` and log mismatches (M0/M1 acceptance enforce this). |
| **Silent frame drops** — a malformed intent deserializes to nothing, no error. | Med | `world-protocol` intent builders are unit-tested for exact field names (`action` not `call_id`, `granted` not `approved`, no `session` key). Test against the live daemon in each milestone. |
| **Type drift** — mirrored types diverge from `apexos-core`. | Med | MIRRORS banner + round-trip tests + `#[serde(other)] Unknown` fallback so new events don't crash. Live-daemon test each milestone catches breaking changes. |
| **VR maturity** — `bevy_mod_openxr` tracks Bevy versions tightly and may lag the version Slint forces. | Med (deferred) | Keep VR a pure feature seam; do not let it constrain the M0–M2 Bevy version choice. Re-evaluate at M3 with whatever Bevy/OpenXR pairing is current. |
| **world-vision IPC choice** (localhost HTTP vs UDS). | Low | Default localhost HTTP on a fixed port (simple, cross-platform, easy to curl in tests). Revisit UDS if security/port-collision matters; world-app binds loopback only. |
| **SDK docs absent** — the brief's `docs/sdk/*` files don't exist. | Low | All facts sourced from code (Section 0). If SDK docs land, reconcile; source wins on conflict. |

---

## 6. Milestone → crate touch map (at a glance)

```
            world-protocol   world-app        world-vision
M0  spine    connect+events   window+hub+chat   —
M1  populate (stable)         multi-station/    —
                              avatars/picking
M2  vision   (stable)         /snapshot impl    full MCP plugin
M3  VR       (stable)         vr render+input   (stable)
```

`world-protocol` is written once in M0 and only *extended* (more `Event` variants) after.
`world-vision` is a leaf that can be built and handshake-tested from day one but only does
something real in M2. `world-app` carries every milestone's net-new surface.
