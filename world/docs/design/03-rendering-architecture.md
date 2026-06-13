# 03 — Rendering & Engine Architecture

> Dimension: **Rendering & engine architecture** for `apexos-world` (codename), the
> AI-native 3D world interface for ApexOS-RS. Prototype on branch `proto/world-3d`,
> scaffolded under `world/`, extracted to its own repo later.
>
> This doc decides the Bevy + Slint + shared-wgpu composition, designs the
> station-screen mechanism (live Slint UI on a 3D surface + fullscreen takeover),
> and gates the whole thing to Pro/Standard hardware.
>
> **Crate binary name:** `apexos-world` (mirrors `apexos-rs-ui`). It is *another*
> agentd client — it speaks the same `ws://HOST:8787/ws` `Event`/Intent wire
> protocol as `ui-slint` and the browser. No fork of agentd core. New world
> capabilities (agent-vision snapshots, world state) arrive through agentd's
> documented MCP-plugin extension surface, never by editing the daemon.

---

## 0. Scope & what other dimensions own

This doc owns: the engine topology (Pattern A vs B), the wgpu sharing mechanism,
the station-screen render-to-texture path, scene graph layout, camera/picking,
3D text, LOD, and hardware-tier gating. It does **not** own:

| Concern | Owned by (assumed dimension) |
|---------|------------------------------|
| The agentd `Event`→world-state mapping, session-fan-out filtering | `01-protocol-and-state` (assumed) |
| The agent-vision MCP plugin (snapshot tool, `world.snapshot`) wire shape | `02-agent-vision-plugin` (assumed) |
| Which function-UI each station/avatar shows (chat, council, sensors…) | `04-station-functions` (assumed) |
| Input/navigation UX, VR | `05-interaction-and-vr` (assumed) |

Where this doc depends on those, the dependency is named inline as **[ASSUMES …]**.

---

## 1. The decision: **Pattern A — Slint hosts, Bevy renders to a viewport texture**

Two candidates from the seed skill:

- **Pattern A** — Slint owns the window + winit event loop; Bevy renders the 3D
  scene to an offscreen wgpu texture; Slint draws that texture into a full-window
  `Image`. Slint also draws all 2D chrome (HUD, the activated function-UIs) *on top*.
- **Pattern B** — Bevy owns the window + event loop; Slint is composited in as an
  overlay texture / 3D quad.

**We choose Pattern A.** Justification, weighted for this project specifically:

1. **The station screen IS a Slint surface.** The core mechanic — "walk to a
   station, the screen fills with a function UI" — is *a live Slint UI rendered to a
   texture*. The fullscreen-takeover mode is *Slint compositing over the 3D image*.
   Both are Slint-primary operations. Pattern A makes the UI layer the host, so
   takeover is a z-order change, not an event-loop handoff.

2. **Maximal reuse of `ui-slint`.** The function-UIs (chat bubbles `VecModel`,
   sensor heatmap painter, council roster, terminal, settings) are **already built**
   as Slint components in `ui-slint/src/ui/components/`. Pattern A lets us host those
   components essentially as-is. Pattern B would force every function-UI through a
   Bevy-managed texture handoff even for the fullscreen case, which is pure overhead.

3. **Thread-model continuity.** ApexOS-RS's load-bearing invariant is *Slint owns the
   main thread; never `#[tokio::main]`; tokio on background threads;
   `slint::invoke_from_event_loop` for all UI mutation* (CLAUDE.md, architecture.md).
   Pattern A preserves this exactly: Slint's winit loop is the main thread, Bevy is
   stepped from a Slint `Timer`, tokio (the WS client) stays on background threads.
   Pattern B inverts ownership (Bevy/winit drives), which fights the established model
   and every reusable snippet from `ui-slint/main.rs`.

4. **Single window, single backend.** Slint 1.16 `renderer-wgpu` + `unstable-wgpu-29`
   exposes its `wgpu::Device`/`Queue` via the rendering notifier. We hand the *same*
   device/queue to Bevy's `RenderDevice`/`RenderQueue` so the Bevy render target and
   the Slint `Image` are zero-copy on one GPU context. One window, one swapchain, one
   device — no second winit window, no cross-window present.

**Cost we accept:** Bevy normally wants to own its own `App` schedule + window. In
Pattern A we run Bevy *headless* (no `WindowPlugin` primary window) and drive
`app.update()` manually from Slint's frame timer. This is a supported-but-advanced
Bevy mode and is the chief integration risk — see §9.

```
                       ┌──────────────── main thread (Slint / winit event loop) ─────────────┐
                       │                                                                       │
  ws://HOST:8787/ws    │   AppWindow (Slint 1.16, renderer-wgpu)                                │
  ┌──────────────┐     │     ├─ full-window Image  <── world_texture (Bevy's render target)     │
  │  agentd      │     │     ├─ HUD overlay (crosshair, prompt bar, nameplates-as-2D-fallback)   │
  │  Event/Intent│◄────┼─    └─ Activated function-UI  (chat / council / sensors / terminal)     │
  └──────┬───────┘     │            ▲ fullscreen takeover OR rendered to station_texture[n]      │
         │ broadcast   │            │                                                            │
         ▼             │   slint::Timer(16ms): app.update()  ──► Bevy headless App               │
  ┌──────────────┐     │                                          ├─ scene graph (stations,…)    │
  │ tokio WS task│ ──► │  invoke_from_event_loop ─► VecModels     ├─ camera, raycast, LOD        │
  │ (bg thread)  │ mpsc│                                          └─ renders → world_texture     │
  └──────────────┘     └───────────────────────────────────────────────────────────────────────┘
            shared wgpu::Device + Queue (from Slint rendering notifier → Bevy RenderDevice/Queue)
```

---

## 2. Shared-wgpu mechanism (the load-bearing glue)

### 2.1 Backend init

```rust
// main.rs — Pattern: Slint owns main thread, tokio on bg threads (CLAUDE.md invariant)
fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Slint 1.16: request a shared wgpu-29 device BEFORE creating the window.
    slint::BackendSelector::new()
        .require_wgpu_29(slint::wgpu::WGPUConfiguration::default())
        .select()?;

    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    let ui = AppWindow::new()?;            // Slint on main thread
    let ui_weak = ui.as_weak();

    // tokio WS client on a background thread — NEVER #[tokio::main]
    rt.spawn(async move { ws_client::run(ui_weak.clone(), /* AGENTD_WS, token */).await });

    // Bevy + texture wiring happens inside the rendering notifier (next subsection)
    // ...
    ui.run()?;                             // Slint owns the loop
    Ok(())
}
```

### 2.2 Capturing the device and creating Bevy

`set_rendering_notifier` fires `RenderingSetup` once the wgpu context is live. We
capture `device`/`queue`, build the Bevy `App` with that device injected, and create
the persistent `world_texture` Bevy renders into.

```rust
ui.window().set_rendering_notifier(move |state, api| {
    if let (slint::RenderingState::RenderingSetup,
            slint::GraphicsAPI::WGPU29 { device, queue, .. }) = (&state, &api) {
        // 1. Build Bevy headless, injecting Slint's device/queue as RenderDevice/RenderQueue.
        //    (Bevy 0.16-era: RenderPlugin { render_creation: RenderCreation::Manual(...) }.)
        // 2. Allocate world_texture (wgpu::Texture, RENDER_ATTACHMENT | TEXTURE_BINDING),
        //    register it as a Bevy camera render target (manual RenderTarget::TextureView).
        // 3. Allocate station_texture[k] for the k visible station screens (see §4).
        // 4. Stash device/queue/textures in a thread-local + an Arc shared with the Bevy step.
        bevy_bridge::init(device.clone(), queue.clone(), /* sizes */);
    }
})?;
```

Per Slint's API, `Image::try_from(wgpu_texture)` wraps a shared texture so a Slint
`Image` element samples it directly. Each frame, after `app.update()`, we hand the
freshest texture to the bound `Image` property:

```rust
let timer = slint::Timer::default();
timer.start(slint::TimerMode::Repeated, Duration::from_millis(16), move || {
    if let Some(ui) = ui_weak.upgrade() {
        bevy_bridge::step();                                  // app.update() → renders world_texture
        ui.set_world_image(bevy_bridge::world_image());       // slint::Image from shared texture
        for (i, tex) in bevy_bridge::station_images() {       // live station screens (§4)
            ui.invoke_set_station_image(i as i32, tex);
        }
    }
});
```

### 2.3 Texture-readback (agent-vision) — extends agentd, does not fork it

The agent-vision loop ("an agent sees through its avatar's camera") is a **GPU
readback of a Bevy camera's render target** → PNG/JPEG → handed to the
`world.snapshot` MCP tool. **[ASSUMES `02-agent-vision-plugin`]** defines that tool's
wire shape and how the agent invokes it; this doc only owns the *render* side:

- Each embodied avatar owns a low-res off-screen Bevy camera (e.g. 512×384) with its
  own render target texture. Render-on-demand (not every frame) when a snapshot is
  requested, to keep cost bounded.
- Readback uses `wgpu` `copy_texture_to_buffer` + async `map_async`; the encode
  (image crate) runs on a tokio blocking task, never on the Slint thread.
- The encoded bytes are returned to the snapshot tool. The world client is just the
  agentd client that *answers* the tool — the tool itself is an MCP plugin agentd
  spawns (`02`), exactly like `cerebro-mcp` and `apexos-tools`.

---

## 3. Mapping the placeholder protocol onto the real agentd `Event` enum

The seed skill ships a **placeholder** `AgentMessage`/`UiEvent` (`ai_protocol.rs`).
It must be **deleted** and replaced with the real wire protocol from
`agentd/crates/core/src/types.rs`. The world client hand-matches the JSON exactly as
`ui-slint` does (no Cargo dep on agentd; vendoring `apexos-core` is the documented
post-v1 option).

Concrete mapping (skill placeholder → real `Event`, what the *renderer* does):

| Skill placeholder | Real agentd `Event` (`type` tag, snake_case) | Renderer effect |
|---|---|---|
| `StreamUpdate` | `agent_text { session, delta }` | append to the chat function-UI bound to the avatar/station for `session`; pulse that avatar's "speaking" emissive |
| — | `agent_thinking { session, delta }` | optional thought-bubble particle over the avatar |
| `ToolResult` | `tool_requested { session, call }` / `tool_result { session, call, output }` | spawn/transition a transient "tool orb" near the avatar; status color running→done/error |
| — | `approval_pending { session, call }` | station screen shows approve/reject; HUD badge on the avatar |
| `EntityUpdate` | **no direct agentd event** — world state is *our* extension | see §3.1 |
| `SystemState` | REST `/api/run` poll (cpu/mem/disk) + `sensor_reading` | ambient HUD + sensor-station heatmap |
| `ImageFrame` | — (binary frames are not in agentd's text protocol today) | station live-texture comes from our own `world.*` channel, not agentd |
| `RequestRender` | handled by the **`world.snapshot` MCP tool** (§2.3), not a WS event | GPU readback |

Outbound (Intents the world client sends), real shapes:

- User text at an avatar → `{"type":"user_prompt","text":"…"}` (the gateway injects
  `session`; frontends omit it on send — architecture.md).
- Approve/reject at a station → `{"type":"user_approval","action":<numeric ToolCall.id>,"granted":true}`
  (**`action`, not `call_id`; `granted`, not `approved`**).
- Cancel a turn → `{"type":"user_cancel"}`. agentd's `cascade_cancel` emits **no**
  `TurnComplete`, so the renderer must clear its own "busy" avatar state on cancel.
- Session bind/replay → `{"type":"session_init"}` (new) or `{"type":"session_init","session_id":N}` (replay).

**Two protocol traps the renderer must respect** (both from architecture.md / CLAUDE.md):

1. **`turn_started` does not exist in the Rust daemon.** Busy/"speaking" state is
   driven *solely* by `agent_text` arriving. A tool-first turn shows no speaking
   animation until text arrives — do not animate on a `turn_started` we'll never get.
2. **The gateway broadcasts every session's events to every socket with no
   server-side filter.** Each avatar maps to a `SessionId`; the renderer **must filter
   inbound events by `session`** and route to the correct avatar, or one agent's text
   lands on another's screen. **[ASSUMES `01-protocol-and-state`]** owns the
   session↔avatar registry; this doc consumes it.

### 3.1 World state is a side-channel, not an agentd core change

Avatar positions, station placement, and "which session is embodied where" are
**world-specific** and have no home in the agentd `Event` enum (and we must not add
one — no core fork). Two clean options, both via agentd's extension surface:

- **(preferred) `world.*` MCP plugin** — a small MCP server agentd spawns (sibling of
  `cerebro-mcp`) exposing tools like `world_place_avatar`, `world_state`,
  `world_snapshot`. Agents move/act in the world by *calling tools*, which is exactly
  the embodiment story. World state then flows back as `tool_result` content the
  renderer already sees. **[ASSUMES `02`]** specifies this plugin.
- **(fallback) local-only world state** — the world client owns avatar transforms
  client-side, keyed by `SessionId`, with no agentd round-trip. Simpler for the
  prototype; loses agent-driven movement. Start here, migrate to the MCP plugin once
  `02` lands.

---

## 4. The station screen — the key mechanism

A **station screen** = a flat quad in the 3D scene whose surface texture is **a live
Slint function-UI**. Two render modes, one shared component set.

### Mode I — In-world surface (the screen on the wall is live)

The function-UI is rendered to its own offscreen Slint texture and sampled by a Bevy
material on the station quad. Mechanism:

```
Slint sub-component (e.g. ChatPanel)  ─render→  station_texture[k] (wgpu)
        ▲ same VecModel<MessageItem>                     │
        │ (driven by agent_text for that session)        ▼
   agentd Event stream                      Bevy material on StationQuad[k] samples it
```

Slint 1.16 can render a *component* to a texture target (offscreen). We keep a small
pool of `station_texture[k]` (K = number of *visible, near* stations, typically ≤ 4;
see LOD §8) and round-robin them to whichever stations are in view + within
interaction range. Far/occluded stations show a cheap static placeholder material
(persona logo, dimmed) — not a live UI.

Design constraints for in-world mode:

- **Resolution budget:** 1024×768 per live station texture. At K=4 that's 4 extra
  offscreen passes/frame — acceptable on Pro/Standard, refused on Nano/Micro (§7).
- **Legibility:** in-world text is small and angled; this mode is for *glanceable*
  state (a chat scrolling, a sensor gauge, a council roster lighting up), not for
  typing. Real interaction promotes to Mode II.
- **No input on the angled surface.** We do *not* attempt raycast-to-UV → synthetic
  Slint pointer events for the prototype (high complexity, low payoff). Activation
  (§6) promotes to fullscreen for actual interaction.

### Mode II — Activated fullscreen takeover (you "enter" the station)

On activation, the *same* Slint function-UI component is shown full-window, composited
**over** the world `Image` (which keeps rendering, blurred/dimmed behind, for
presence). This is a pure Slint z-order/visibility change — the component is the same
one that was driving the in-world texture, so there is **no state handoff** and no
re-subscribe to agentd: the `VecModel` is shared.

```
            ┌─────────────────── AppWindow (Slint) ───────────────────┐
 inactive:  │  Image{ world_texture }  ← full-window 3D                │
            │  [station quads show live station_texture[k] in 3D]      │
            └──────────────────────────────────────────────────────────┘
                              │ activate(station_id)  (E key / click / agent intent)
                              ▼
            ┌─────────────────── AppWindow (Slint) ───────────────────┐
 active:    │  Image{ world_texture } (dimmed, still rendering)        │
            │  ┌───────────────── FunctionUI (fullscreen) ──────────┐  │
            │  │  ChatPanel / CouncilView / SensorView / Terminal   │  │
            │  │  (reused ui-slint components, full input + focus)   │  │
            │  └─────────────────────────────────────────────────────┘  │
            │  [Esc] → back to in-world                                  │
            └──────────────────────────────────────────────────────────┘
```

State machine (Slint property `active_station: int`, `-1` = none):

```
   in-world ──activate(id)──► fullscreen(id) ──Esc/back──► in-world
        ▲                                                     │
        └──────── agent intent can also drive activate ───────┘
```

Mode II is the *interaction* mode; Mode I is the *ambient/glance* mode. Both render
the identical component tree, so the function-UI catalogue is built once.
**[ASSUMES `04-station-functions`]** defines the catalogue (which component per
station kind). Reuse target from `ui-slint`: `ChatPanel`, `SensorView` (thermal
heatmap painter), `CouncilView`, `Terminal` (PTY over `/terminal-ws`), `Settings`.

### Why not "always fullscreen Slint, 3D only as wallpaper"?

Because the felt experience requires the screen to be alive *before* you walk up to
it (Mode I). A dead quad that only comes alive on click loses the "world of working
agents" presence. We pay for K live textures to get it.

---

## 5. Scene graph

Flat, kind-tagged ECS. Three top-level categories, each a Bevy `Component` marker:

```
WorldRoot
├─ Ambient                 (non-interactive, sets the mood)
│   ├─ ground / grid plane
│   ├─ DirectionalLight (key) + ambient fill
│   ├─ skybox / gradient
│   └─ ambient particles (optional, LOD-gated off on Standard)
├─ Station[*]              (a function surface you activate)
│   ├─ StationQuad         (the screen — material = station_texture[k] | placeholder)
│   ├─ StationFrame        (mesh/bezel; persona-themed)
│   ├─ Nameplate (3D text, §7)
│   └─ Interactable { kind: StationKind, fn_ui: FunctionUiId, session: Option<SessionId> }
└─ Avatar[*]               (an embodied agent = a SessionId)
    ├─ AvatarMesh          (placeholder capsule v0 → glTF rigged later)
    ├─ SpeakingEmissive    (pulsed by agent_text deltas for its session)
    ├─ VisionCamera        (off-screen, for world.snapshot §2.3)
    ├─ Nameplate (3D text)
    └─ Interactable { kind: Agent, session: SessionId }
```

Bevy components (sketch):

```rust
#[derive(Component)] struct Interactable { kind: NodeKind, target: ActivationTarget }
#[derive(Component)] struct AvatarOf(SessionId);          // links scene node ↔ agentd session
#[derive(Component)] struct StationScreen { slot: Option<u8> }   // which station_texture[k], if live
#[derive(Component)] struct Speaking { until: Instant }   // set by agent_text, decays
enum NodeKind { Agent, Station, Ambient }
enum ActivationTarget { Session(SessionId), Station(StationId) }
```

**Event→ECS sync system** (mirrors the skill's `sync_entities_from_agent_messages`,
but real): a Bevy `Update` system drains an mpsc `Receiver<WorldCmd>` filled by the
WS task. `WorldCmd` is the *renderer-internal* command type (not on the wire):
`Speak(SessionId)`, `ToolOrb(SessionId, ActionId, status)`, `PlaceAvatar(SessionId,
Vec3)`, `Activate(ActivationTarget)`. The WS task translates real `Event`s →
`WorldCmd` after session-filtering (§3).

---

## 6. Camera modes & picking / raycast for activation

### Camera modes (a `CameraMode` resource; cycle with a key, **[ASSUMES `05`]** owns UX)

| Mode | Use | Notes |
|------|-----|-------|
| **Orbit** | overview of the agent population | default on launch; mouse-drag orbit, scroll zoom |
| **Walk** (first-person) | "walk up to an avatar/station" | WASD + mouse-look; the felt experience |
| **Fixed/cinematic** | activation transition | lerps to a framed shot of the activated station, then Mode II takes the screen |

Single `Camera3d` whose `Transform` is driven by the active mode's controller system.
On activation we tween to the cinematic pose, *then* raise the fullscreen Slint UI —
so "entering" a station feels like the camera dollies in and the screen fills the view.

### Picking / raycast

Activation candidate = the `Interactable` under the reticle (Walk) or cursor (Orbit).

- **v0 (no extra deps):** manual ray from camera through cursor/screen-center; test
  against each `Interactable`'s AABB (broad) then triangle/quad (narrow) for the few
  near entities only (LOD §8 already limits the candidate set). This is cheap because
  K is small and we only test in-range entities.
- **v1 (optional):** adopt `bevy_mod_picking` / Bevy's built-in picking if the manual
  ray proves fiddly. Kept optional to limit dependency surface during de-risking (§9).

Activation triggers (all converge on `activate(ActivationTarget)`):

1. Human: `E`/click on the focused `Interactable`.
2. Agent: an agent can request the world surface its own UI via the `world.*` MCP tool
   (e.g. `world_activate`), which arrives as a `tool_result` → `WorldCmd::Activate`.
   This is how "an agent shows you something" works. **[ASSUMES `02`]**.

Proximity gate: in Walk mode, activation only arms within an interaction radius, and a
HUD prompt ("[E] open FORGE") appears — also the cue for which station gets a live
Mode-I texture promoted.

---

## 7. Text & label rendering in 3D

Three tiers, cheapest-first:

1. **Nameplates / short labels** — billboarded 3D text via Bevy's text-in-world
   (`Text` on a billboard quad, or `bevy_mod_billboard` if Bevy's built-in 3D text is
   insufficient). Always face the camera. Agent name + status glyph (#color from
   `CouncilAgentDef.color` / persona). Distance-faded; culled past `label_far`.
2. **Rich/scrolling UI text** (a chat transcript, council deltas) — **not** drawn by
   Bevy. It is a Slint function-UI on a station screen (§4). Slint's text shaping +
   fontconfig handle wrapping/scroll; we get `ui-slint`'s exact typography for free.
3. **HUD text** (reticle prompt, toasts, connection status) — plain Slint 2D overlay
   on the host window, never in the 3D pass.

Rule: **if it needs to be read, it's Slint; if it's a glanceable label, it's a
billboard.** This avoids reimplementing text layout in Bevy and keeps all real reading
surfaces in the engine (Slint) that already does it well in this codebase.

---

## 8. Performance & LOD for many agents

Targets (Pro/Standard, §9 gate): 60 fps Pro, ≥30 fps Standard, with up to ~50 avatars
+ ~12 stations in scene.

LOD bands keyed on distance + view frustum + interaction range:

| Band | Avatar | Station screen | Text |
|------|--------|----------------|------|
| **Near** (in range / focused) | full mesh, speaking emissive, vision cam armed | **live** `station_texture[k]` (Mode I) | full nameplate |
| **Mid** (visible, out of range) | full mesh, no per-frame effects | **static placeholder** material | faded nameplate |
| **Far** | impostor / instanced billboard | placeholder, low-res | culled |
| **Culled** (out of frustum) | not drawn | not drawn | — |

Key budgets & mechanisms:

- **Live station textures are the scarce resource.** Hard cap `K` (default 4 on
  Standard, up to 8 on Pro). Only Near stations get a slot; promotion/demotion happens
  as the camera moves. Slint offscreen passes are the dominant added cost over plain
  `ui-slint`, so this cap is the main throttle.
- **Instanced avatars** for Mid/Far (Bevy `InstancedMeshes` / GPU instancing) so 50
  agents are a handful of draw calls.
- **Vision cameras render on-demand only** (§2.3) — never per-frame. A snapshot is a
  single extra pass when the tool fires.
- **Frustum + distance culling** standard Bevy; avatars out of view cost ~nothing.
- **Adaptive frame timer:** the Slint `Timer` step interval can stretch (16→33 ms)
  under load; Bevy's fixed-vs-variable schedule decoupled from render so logic stays
  stable.
- **Bevy Tracy** (`bevy/trace_tracy`) behind a `profile` feature for measuring the
  added passes during de-risking.

---

## 9. Hardware-tier gating (Pro/Standard only — degrade or refuse)

ApexOS-RS tiers (CLAUDE.md): Nano (Pi Zero 2W, `linuxkms-femtovg`, 23 MB),
Micro (Pi 4, `linuxkms`), Standard (Pi 5 / x86 mini-PC, `linuxkms`),
Pro (x86 + CUDA/ROCm/Metal GPU, `winit`).

`apexos-world` is a **Pro/Standard-only** client. It is *not* a replacement for
`ui-slint` on Pi — on Nano/Micro the user runs `apexos-rs-ui` (or the browser/PWA) as
today. The world is the rich-hardware experience.

### Gate at launch (refuse cleanly, never half-render)

```
detect tier:
  • require a real GPU adapter from wgpu (DeviceType::DiscreteGpu | IntegratedGpu
    that passes a capability probe: render-to-texture + the texture sizes we need).
  • SLINT_BACKEND must be winit or linuxkms (GPU) — refuse on linuxkms-femtovg (sw).
  • RAM / SoC allowlist as a coarse secondary check (Pi Zero/Pi4 → refuse).

if tier ∈ {Nano, Micro}  OR  no GPU adapter  OR  femtovg backend:
    print: "apexos-world requires Standard/Pro hardware with a GPU.
            On this device, run apexos-rs-ui (Slint 2D) or the browser UI instead."
    exit(2)            # refuse — do not start Bevy

if tier == Standard:  K_live_stations=4; avatars_full<=24; ambient_particles=off; shadows=low
if tier == Pro:       K_live_stations=8; avatars_full<=50; ambient_particles=on;  shadows=high
```

`WORLD_TIER` env override (`standard`/`pro`) for testing on a fibbing box, mirroring
the project's `SLINT_BACKEND` override convention. A `--force` flag bypasses the
refusal for developers (logs a loud warning) but never changes the default safety.

### Why refuse rather than degrade to 2D

A 2D fallback already exists and is better: `apexos-rs-ui`. Shipping a degraded 3D
mode on Nano/Micro would duplicate `ui-slint` poorly. Clean refusal + a pointer to the
right client is the honest behavior and matches CLAUDE.md's "build for Nano first,
faster tiers respond faster" by simply *not being the Nano client*.

---

## 10. The Slint + Bevy wgpu pitfall — and a concrete de-risking path

The seed skill warns (SKILL.md §Version Guidance): *"Test … compatibility carefully
(known symbol issues with some Bevy+Slint combos — pin and verify)."* Concretely the
hazards are:

1. **Two `wgpu` versions in one binary.** Slint 1.16 pins a specific `wgpu` (exposed as
   `unstable-wgpu-29`). Bevy pins its own `wgpu`. If they differ, the `wgpu::Device`
   handed by Slint is a *different type* than Bevy's `RenderDevice` expects — it will
   not compile, or worse, link with duplicate symbols. **The device share only works if
   both crates resolve to the same `wgpu` semver.**
2. **`wgpu-hal`/`naga` transitive symbol clashes** when versions are close-but-not-equal
   (the "known symbol issues").
3. **Bevy expecting to own winit/the event loop**; we run it headless under Slint.

### De-risking path (do these in order; each is a gate)

**Spike 0 — version reconciliation (BLOCKING, do first, ~½ day).**
Before any feature work, build a throwaway crate that depends on *both*
`slint = "1.16"` (with `unstable-wgpu-29`) and the candidate `bevy` version, and run
`cargo tree -d -i wgpu` (and `naga`, `wgpu-hal`). **Success = exactly one `wgpu`
version in the tree.** If Bevy's `wgpu` ≠ Slint's wgpu-29, options in preference order:
  (a) pick the Bevy release whose `wgpu` matches Slint 1.16's (consult both changelogs;
      this likely pins Bevy to a specific 0.16.x/0.17.x);
  (b) use `[patch.crates-io]` to force a single `wgpu` (only if APIs are compatible —
      verify, don't assume);
  (c) if irreconcilable, **fall back to Pattern A-lite** (below) and revisit.
This spike decides the whole project's dependency pins. Record the winning pins in the
world crate's `Cargo.toml` with a comment, and in `docs/design/` once locked.

**Spike 1 — shared device, one triangle (~1 day).**
Slint window hosting one full-window `Image` fed by a Bevy headless app that renders a
single rotating triangle to a shared texture via the rendering notifier. This proves
device sharing + the per-frame `Image::try_from(texture)` handoff + `app.update()`
from a Slint `Timer`. No agentd, no scene graph. **Gate: the triangle spins in a Slint
window at 60 fps with one wgpu device in the tree.**

**Spike 2 — one live station texture (~1-2 days).**
Render a real reused `ui-slint` component (start with the simplest, e.g. a static
`SensorView` with fake data) to an offscreen Slint texture and sample it on a Bevy
quad in the Spike-1 scene. Proves Mode I (§4). **Gate: a live Slint panel visible on a
3D surface.**

**Spike 3 — agentd attach (~1 day).**
Wire the real WS client (`tokio-tungstenite 0.24`, same as `ui-slint`), session-filter
inbound `Event`s, drive a `ChatPanel`'s `VecModel` from `agent_text` for one avatar.
Mode II fullscreen takeover on `E`. **Gate: type at an avatar, see streamed `agent_text`
in both the in-world screen and the fullscreen takeover, against a live agentd.**

Only after Spikes 0-3 pass do we build the full scene graph, LOD, picking, and tier
gating. If Spike 0 cannot produce a single `wgpu` version:

### Pattern A-lite fallback (de-risk insurance)

If Bevy and Slint cannot share one `wgpu` in one binary, **do not abandon Pattern A**.
Degrade the *engine*, not the architecture:

- Replace Bevy with a **thin custom wgpu renderer** that uses *Slint's own*
  `wgpu::Device`/`Queue` directly (no Bevy, no second wgpu). We lose Bevy's ECS/asset
  pipeline but keep the entire Pattern-A composition, the station-screen mechanism, the
  tier gate, and all `ui-slint` reuse. The scene graph (§5) becomes a hand-rolled
  `Vec<Node>` instead of a Bevy `World`.
- This is strictly more work for the 3D side but **zero risk** on the version clash,
  because there is exactly one wgpu (Slint's). It is the safety net that lets us commit
  to Pattern A now without the Bevy version question blocking the whole prototype.

The decision tree: **prefer Bevy (Spike 0 green) → else Pattern A-lite custom wgpu.**
Either way the host, the protocol mapping, and the station screens are unchanged.

---

## 11. Crate skeleton (Pattern A, prototype)

```
world/crates/apexos-world/
├─ Cargo.toml          # slint 1.16 (backend-winit + renderer-wgpu + unstable-wgpu-29),
│                      # bevy <pin from Spike 0>, tokio (multi-thread, NOT macros#[main]),
│                      # tokio-tungstenite 0.24, serde/serde_json, image
├─ build.rs            # slint-build: compile ui/*.slint (touch to force recompile — CLAUDE.md gotcha)
├─ src/
│  ├─ main.rs          # Slint owns main thread; tokio bg; rendering-notifier wgpu capture; frame timer
│  ├─ bevy_bridge.rs   # build headless Bevy w/ injected device/queue; world+station textures; step()
│  ├─ scene.rs         # scene graph components + Event→WorldCmd sync system (§5)
│  ├─ camera.rs        # orbit/walk/cinematic controllers + activation tween (§6)
│  ├─ pick.rs          # manual raycast vs Interactable AABB/quad (§6)
│  ├─ stations.rs      # station texture pool, Mode I/II promotion, fullscreen takeover (§4)
│  ├─ protocol.rs      # REAL agentd Event/Intent (from types.rs) — NOT the skill placeholder (§3)
│  ├─ ws_client.rs     # tokio WS, session-filter, Event→WorldCmd, outbound Intents (§3)
│  ├─ vision.rs        # off-screen camera readback for world.snapshot (§2.3) [pairs with 02]
│  └─ tier.rs          # hardware-tier detect + refuse/configure (§9)
└─ ui/
   ├─ world.slint      # host: full-window Image{world}, HUD overlay, fullscreen FunctionUI slot
   └─ functions/       # reused/adapted ui-slint components: chat, sensor, council, terminal, settings
```

---

## 12. Open questions (hand-offs)

- **Exact Bevy version + the resolved single `wgpu` pin** — output of Spike 0; cannot
  be decided on paper.
- **`world.*` MCP tool surface** (place/move/activate/snapshot wire shape) — owned by
  `02-agent-vision-plugin`; this doc assumes it exists.
- **Session↔avatar registry & the broadcast session-filter** — owned by
  `01-protocol-and-state`; renderer consumes it.
- **Function-UI catalogue** (component per `StationKind`) — owned by
  `04-station-functions`.
- **Slint offscreen-component-to-texture** must be validated as a *stable* 1.16 API in
  Spike 2; if only the full-window render-to-texture path is stable, Mode I in-world
  screens may need a per-station hidden Slint window/surface — confirm in the spike.
- **Vendoring `apexos-core`** for shared `Event` types (vs hand-matched JSON) — same
  post-v1 question `ui-slint` carries; prototype hand-matches.
```
