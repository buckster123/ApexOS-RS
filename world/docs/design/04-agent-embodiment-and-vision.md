# 04 — Agent Embodiment & the Vision Loop

> Design dimension: how agents are **embodied** in the 3D world, how they **see**
> from their avatar's camera, and how they **act** in-world — all expressed through
> agentd's real protocol surfaces (the `Event`/`ToolCall` wire types and the stdio
> MCP plugin contract), never by forking agentd core.

Codename: `apexos-world` (prototype, branch `proto/world-3d`, under `world/`).
Target tier: **Pro/Standard** desktop monitors first (the world renderer assumes a
real GPU — it is explicitly NOT a Nano/Pi-Zero experience). VR (Quest 3) later.

---

## 0. The two processes and why there are two

agentd does not host a renderer and must not. A renderer needs a GPU, a window/event
loop, and a wgpu device — none of which belong inside a headless daemon that also runs
on a Pi. So the design splits into **two cooperating processes**, exactly mirroring how
`apexos-tools` and `cerebro-mcp` already relate to agentd:

```
┌──────────────────────────── one desktop/Pro node ─────────────────────────────┐
│                                                                                │
│  ┌─────────────┐   ws://HOST:8787/ws         ┌──────────────────────────────┐ │
│  │   agentd    │◀────── Event / Intent ──────▶│  world-app  (the renderer)   │ │
│  │  (daemon)   │   (a normal WS client, like  │  Slint 1.16 + wgpu + Bevy    │ │
│  │             │    ui-slint or the browser)  │  owns the GPU + window loop  │ │
│  └─────┬───────┘                              └──────────────┬───────────────┘ │
│        │ spawns stdio MCP plugins                            │                 │
│        │ (plugins.toml)                                      │ local IPC       │
│        ▼                                            (unix socket / named pipe) │
│  ┌──────────────────────┐   newline-delimited JSON-RPC      │                 │
│  │  world-vision-mcp     │◀──────────(stdio)──── agentd ─────┘                 │
│  │  (the agentd-facing   │                                                     │
│  │   tool plugin)        │───── connects to world-app's local IPC ────────────┘
│  └──────────────────────┘
└────────────────────────────────────────────────────────────────────────────────┘
```

- **`world-app`** — the 3D renderer. A normal agentd WS client (speaks the real
  `Event`/`Intent` JSON on `ws://HOST:8787/ws`, see `docs/architecture.md` "multi-client").
  It owns the wgpu device, the Bevy ECS scene, the avatars, and the cameras. It is the
  *only* process that can render a viewport and read pixels back. It also hosts a small
  **local IPC server** (a unix domain socket) so the vision plugin can ask it for renders.
- **`world-vision-mcp`** — a stdio MCP plugin, declared in `plugins.toml` like any other
  (§5). agentd's `Supervisor` spawns it and lists its tools. It owns *no* renderer; every
  tool call it receives it forwards over the local IPC to `world-app`, waits for the
  result, and returns it to agentd as an MCP `tools/call` response.

This is the whole trick. The agent calls a tool → agentd routes it to the plugin → the
plugin asks the renderer → the renderer reads back pixels → the image flows back up the
same path as a normal tool result. No new agentd code; the plugin and the WS client are
both documented extension surfaces.

> **Assumption (dimension: world-app architecture, doc 01/02).** `world-app` exists as a
> single process owning both the WS connection and the Bevy scene. If the WS client and
> the renderer are split into separate processes, the local IPC endpoint (§4) moves to
> whichever process owns the wgpu `Device`/`Queue` and the entity transforms. Nothing
> else in this doc changes.

---

## 1. Avatar representation

An **avatar** is the in-world body of an agent *session*. The identity key is agentd's
`SessionId` (a bare `u64` on the wire — see `agentd/crates/core/src/types.rs`). Every
root session that the world chooses to embody gets exactly one avatar entity.

```
AvatarEntity (Bevy ECS components)
├── Avatar { session: u64, label: String }     // session id is the stable key
├── Transform                                    // position + yaw in the world
├── AvatarCamera (child entity)                  // the "eyes" — see §3
├── AvatarStatus { phase: Phase, accent: Color } // visual status, see §1.3
└── AvatarBody (one of:)
    ├── Primitive  — capsule + emissive "head" orb   (v0, ships first)
    └── GltfRigged — loaded scene + AnimationPlayer   (v1, later)
```

### 1.1 Placeholder primitive (v0 — ships first)

A **capsule** (body) + a smaller **emissive sphere** (head/"face"). The head's emissive
color is the agent's accent (from `CouncilAgentDef.color` when the session came from a
council, else a hash of the session id). A floating billboard label shows `label`
(session id, or a persona name if known). This is deliberately cheap — no rig, no
animation, just `Transform` motion. It is enough to *be somewhere* and *look somewhere*.

### 1.2 glTF rigged (v1 — later)

Swap `AvatarBody::Primitive` for `GltfRigged`: load a `.glb` via Bevy's `SceneRoot`,
drive an `AnimationPlayer`. The avatar's API surface (position, yaw, look-target, status
phase) is identical, so the upgrade is internal — a renderer-side material/mesh swap, not
a protocol change. Pose streams (idle/walk/talk/think clips) map to `AvatarStatus.phase`.

> **Assumption (dimension: assets/scene, doc 02).** A glTF asset pipeline and a chosen
> rig exist by v1. v0 needs none.

### 1.3 Status expressed visually

agentd already tells us everything we need about an agent's state through the `Event`
stream the world-app is *already consuming* as a WS client. We do not invent a status
channel — we **derive avatar status from existing events** keyed by `session`:

| agentd `Event` (by `session`)         | Avatar visual                                  |
|---------------------------------------|------------------------------------------------|
| `TurnComplete` → (idle)               | `Phase::Idle` — head slow-pulse, dim accent    |
| `AgentText { delta }`                 | `Phase::Speaking` — head bright, lip/glow blink |
| `AgentThinking { delta }`             | `Phase::Thinking` — head shimmer, particle wisp |
| `ToolRequested { call }`              | `Phase::Acting` — accent ring spins, tool icon |
| `ApprovalPending { call }`            | `Phase::Blocked` — amber halo, "!" billboard   |
| `ToolResult { ok:false }`             | flash red once                                 |
| `Error { session }`                   | red halo                                        |
| `WakeTriggered`                       | all avatars flash wake indicator               |
| `SubAgentStarted { parent, child }`   | spawn child avatar near `parent`, tether line  |

The world-app's existing WS event handler (the same loop that drives chat/sensor views in
`ui-slint`) gains one extra fan-out: route each session-keyed event into a Bevy event so
`sync_avatar_status` can mutate `AvatarStatus`. No new wire types.

---

## 2. How an agent ACTS in-world

Acting in the world is just **more MCP tools** in the same `world-vision-mcp` plugin (or a
sibling `world-act-mcp` — they can be one plugin). The agent calls them; agentd routes a
`ToolRequested` through the policy engine; on dispatch the plugin forwards the verb over
local IPC to `world-app`, which mutates the Bevy scene; the plugin returns a small JSON
`ToolOutput`.

| Tool                        | args                                              | effect in world-app                                  |
|-----------------------------|---------------------------------------------------|------------------------------------------------------|
| `world_move`                | `{ avatar?: u64, to:[x,y,z], yaw?: f32 }`         | tween avatar transform (avatar defaults to caller's session) |
| `world_look_at`             | `{ avatar?: u64, target:[x,y,z] \| entity:str }`  | aim the avatar camera; updates `AvatarCamera`        |
| `world_activate`            | `{ station: str }`                                | "activate" a station/world-element (open its surface) |
| `world_point`               | `{ at:[x,y,z] \| entity:str, ttl_ms?: u32 }`      | spawn a transient pointer beam from the avatar       |
| `world_say`                 | `{ text: str }`                                   | speech billboard above avatar (UI nicety, no LLM)    |
| `world_list_entities`       | `{ kind?: str }`                                  | returns stations/avatars/elements in view (JSON)     |
| `world_look`                | see §3                                             | **the vision tool** — returns an image               |

**`avatar` defaulting.** When `avatar` is omitted, the plugin substitutes the **caller's
own session id**. The plugin learns the caller's session because agentd injects `session`
into the tool-call context — but MCP `tools/call` arguments do *not* carry it by default.
So the plugin maps caller→avatar via the **registration handshake** (§4.1): the world-app
tells the plugin which `SessionId` owns which avatar. If the mapping is unknown, the tool
returns an error asking for an explicit `avatar`/`station`/`free_cam`. This keeps "move me"
ergonomic without guessing.

> **Assumption (dimension: protocol, doc 01).** The plugin is given the caller session by
> the world-app over the local IPC (the world-app sees `ToolRequested.session` on its WS
> feed and can correlate by `call.id`). If correlation proves unreliable, drop the default
> and require an explicit target — every tool still works, just less ergonomically.

Movement is **advisory and tweened**, never teleport-snapped, so the human watching sees
the agent walk. `world-app` clamps targets to navmesh/bounds and rejects illegal moves
with `ok:false` — the agent gets a normal error tool result and can retry.

---

## 3. The vision loop — `world_look`

The core capability: an agent **sees from its avatar's camera** (or a station's camera, or
a free camera) by calling one MCP tool that returns an encoded image.

### 3.1 Tool contract (the agentd-facing surface)

```jsonc
// tools/list entry (what agentd advertises to the model)
{
  "name": "world_look",
  "description": "Render what an avatar/station/free camera currently sees in the 3D world and return it as an image. Use to inspect the world, read a station's surface, or check where another agent is.",
  "inputSchema": {
    "type": "object",
    "properties": {
      "view":    { "oneOf": [
                     { "const": "self" },               // caller's own avatar camera
                     { "type": "object", "properties": { "avatar":   { "type": "integer" } } },
                     { "type": "object", "properties": { "station":  { "type": "string"  } } },
                     { "type": "object", "properties": { "free_cam": {
                         "type": "object",
                         "properties": { "eye":[3], "target":[3], "fov_deg":{} } } } }
                   ] },
      "width":   { "type": "integer", "default": 1024, "maximum": 1920 },
      "height":  { "type": "integer", "default": 576,  "maximum": 1080 },
      "format":  { "type": "string", "enum": ["jpeg","png"], "default": "jpeg" },
      "annotate":{ "type": "boolean", "default": true }   // overlay entity labels/markers
    },
    "required": ["view"]
  }
}
```

### 3.2 What the tool RESULT looks like on the wire (this is the load-bearing detail)

agentd's `McpClient::call_tool` (in `agentd/crates/plugins/src/mcp.rs`) reads the JSON-RPC
`tools/call` response and builds a `ToolOutput`:

```rust
// agentd/crates/plugins/src/mcp.rs — verbatim behaviour
let is_error = result.get("isError").and_then(|v| v.as_bool()).unwrap_or(false);
ToolOutput {
    ok:      !is_error,
    content: result.get("content").cloned().unwrap_or(Value::Null),  // ← passed through VERBATIM
}
```

That `content` is forwarded unchanged into `Event::ToolResult { output }` **and** into the
agent's conversation as `ContentBlock::ToolResult { content, .. }` (see `types.rs`). The
turn engine replays that block to the provider. Therefore: **to make the agent actually
see the image, the plugin must return MCP image content blocks** in the shape the provider
expects (which agentd's anthropic provider maps to a vision content block):

```jsonc
// world-vision-mcp's JSON-RPC reply to agentd's tools/call
{
  "jsonrpc": "2.0",
  "id": 7,
  "result": {
    "isError": false,
    "content": [
      { "type": "text", "text": "view=self avatar=42, 1024x576, 3 entities visible: station:sensors, avatar:7, element:memory-orb" },
      { "type": "image",
        "source": { "type": "base64", "media_type": "image/jpeg", "data": "<base64 JPEG>" } }
    ]
  }
}
```

> **Assumption (dimension: protocol/provider, doc 01).** agentd's provider layer
> (`anthropic.rs`/`oai.rs`) forwards an MCP `image` content block in a `ToolResult` to the
> model as a vision block. This is the standard MCP image-result shape; the doc-01 owner
> should confirm the exact key the provider expects (`source.type=base64` +
> `media_type`+`data` is the Anthropic shape). If the provider only forwards text, the
> fallback is to return a `text` block with a `file://` path or a short-lived
> `http://world-app:PORT/frame/<id>.jpg` URL the model can fetch — same render path,
> different envelope. The render-readback design below is unaffected either way.

The `text` block alongside the image is the **annotation manifest** — entity ids/labels in
the frame — so a text-only model still gets situational awareness, and a vision model gets
grounding for what it's looking at.

### 3.3 The render-readback path (wgpu async readback)

This runs entirely inside `world-app`. It is an **offscreen** render — it must NOT disturb
the on-screen camera or steal the swapchain.

```
world-vision-mcp                world-app (renderer, owns wgpu Device/Queue)
      │  IPC: LookRequest{view,w,h,fmt}
      ├───────────────────────────────▶│
      │                                 │ 1. resolve `view` → a Bevy Camera:
      │                                 │      self/avatar → that AvatarCamera's Transform+proj
      │                                 │      station     → that station's fixed camera
      │                                 │      free_cam    → spawn an ephemeral Camera3d
      │                                 │ 2. create an OFFSCREEN render target:
      │                                 │      a wgpu::Texture (RENDER_ATTACHMENT|COPY_SRC,
      │                                 │      Rgba8UnormSrgb, w×h) wrapped as a Bevy Image
      │                                 │      RenderTarget::Image — does NOT touch swapchain
      │                                 │ 3. drive ONE render of just that camera to the target
      │                                 │      (Bevy: a camera with `order` + that target;
      │                                 │       or a manual render graph run for one frame)
      │                                 │ 4. ASYNC READBACK (the wgpu dance):
      │                                 │      - encoder.copy_texture_to_buffer(tex → buffer)
      │                                 │        with bytes_per_row padded to 256
      │                                 │      - queue.submit(); buffer.slice(..).map_async(Read)
      │                                 │      - device.poll(Wait) / await the map callback
      │                                 │      - copy mapped bytes, un-pad rows → RGBA Vec<u8>
      │                                 │ 5. ENCODE on a blocking task (image crate):
      │                                 │      RGBA → jpeg(q~80) or png; drop alpha
      │                                 │ 6. base64 the bytes
      │  IPC: LookReply{ b64, manifest }│
      │◀────────────────────────────────┤
      │ wrap into MCP content blocks (§3.2), reply to agentd's JSON-RPC
      ▼
```

Implementation notes that matter:

- **Use Bevy's readback machinery, don't hand-roll if avoidable.** Bevy 0.16+ ships
  `bevy_render::gpu_readback::Readback` (observer-based) and the `bevy_image_export`
  community crate; either gives the copy-to-buffer + map_async + un-pad loop for free.
  Hand-rolling means: `COPY_BYTES_PER_ROW_ALIGNMENT = 256` row padding is the #1 footgun —
  you must allocate `padded_bytes_per_row = align_up(w*4, 256)` and strip the pad after
  mapping.
- **One frame, on demand.** The offscreen camera is normally `is_active=false`; the IPC
  request flips it active for exactly one render, captures, then deactivates. This keeps
  the steady-state cost at zero — a `world_look` is the only thing that pays for a render.
- **Latency budget.** Render + readback + jpeg-encode of 1024×576 is ~10–30 ms on a
  desktop GPU; map_async adds up to one frame of latency. Well under the 30 s LLM-call
  floor the project mandates. Encode runs on `tokio::task::spawn_blocking` so the Slint/
  Bevy event loop never stalls.
- **Shared device.** world-app already shares one `wgpu::Device`/`Queue` between Slint
  (`unstable-wgpu-29`) and Bevy (the skill's Pattern A). The offscreen target is allocated
  on that same device — zero extra context, and the readback buffer is pooled/reused
  across calls keyed by `(w,h,fmt)`.
- **Annotation overlay (`annotate:true`).** Before readback, project visible entity
  centroids to screen space and either (a) draw label quads into the same offscreen pass,
  or (b) — simpler for v0 — skip pixel overlay and only fill the **text manifest** with
  `{entity_id, screen_xy, label, kind}`. v0 ships (b); pixel labels are a v1 polish.

### 3.4 Free-cam and station views

- `free_cam` spawns an **ephemeral** `Camera3d` at `eye` looking at `target`, renders one
  frame, despawns it. Lets an agent inspect the world from anywhere without a body.
- `station` resolves a named world-element's pre-placed camera (e.g. the sensor dashboard's
  front-facing cam). Station registry lives in world-app; the plugin just passes the name.

---

## 4. The local IPC between plugin and renderer

The plugin (`world-vision-mcp`) and the renderer (`world-app`) talk over a **unix domain
socket** at `$XDG_RUNTIME_DIR/apexos-world.sock` (override `WORLD_IPC_PATH`). Wire format:
**newline-delimited JSON**, identical in spirit to the MCP stdio transport already used
across the workspace (`cerebro-mcp/src/transport.rs`) — one request object per line, one
reply per line, correlated by a `req` id. This deliberately reuses a pattern the team
already knows cold.

```jsonc
// plugin → world-app
{ "req": 7, "op": "look", "view": {"self": true}, "caller_session": 42,
  "width": 1024, "height": 576, "format": "jpeg", "annotate": true }
{ "req": 8, "op": "move", "caller_session": 42, "to": [1.0,0.0,3.5], "yaw": 90.0 }

// world-app → plugin
{ "req": 7, "ok": true, "image_b64": "...", "media_type": "image/jpeg",
  "manifest": [ {"entity":"station:sensors","kind":"station","label":"Sensors","xy":[210,140]} ] }
{ "req": 8, "ok": false, "error": "target out of bounds" }
```

The plugin connects on startup and **reconnects with backoff** if world-app is down (same
fixed-backoff pattern as `apex-sensor-bridge`). If the socket is unavailable, every world
tool returns `ok:false` with `"world renderer not running"` — the agent degrades
gracefully (consistent with the project's "graceful when a capability is disabled" rule).
Only `world-app` may bind the socket; the plugin only connects — so a missing renderer can
never wedge a turn (agentd's bounded tool-result timeout would synthesize an error anyway).

### 4.1 Registration handshake (caller → avatar mapping)

On connect, world-app pushes a **roster** to the plugin and updates it on
`SubAgentStarted`/session changes (world-app sees these on its WS feed):

```jsonc
// world-app → plugin, unsolicited
{ "op": "roster", "avatars": [ {"session": 42, "label": "root"}, {"session": 7, "label": "scout"} ],
  "stations": ["sensors","council","terminal","memory"] }
```

This is what lets `world_look {view:"self"}` and `world_move` (no `avatar`) resolve to the
caller's body — the plugin looks up `caller_session` in the roster.

---

## 5. Plugin registration (`plugins.toml`)

`world-vision-mcp` plugs into agentd exactly like `cerebro` and `apexos-tools` — one
`[[plugin]]` block. No agentd code change; the `Supervisor` spawns it, calls
`initialize` + `tools/list`, advertises its tools, and routes `tools/call` to it.

```toml
# /etc/agentd/plugins.toml  (added on Pro/Standard nodes that run the world)
[[plugin]]
id      = "world"
cmd     = "/usr/local/bin/world-vision-mcp"
args    = []
restart = "always"
[plugin.env]
WORLD_IPC_PATH = "/run/user/1000/apexos-world.sock"   # matches world-app's bind
RUST_LOG       = "warn"
```

Because tools route through the policy engine (`policy.toml`), the read-only `world_look`
/ `world_list_entities` default to `allow`, while world-mutating verbs
(`world_move`/`world_activate`/`world_point`) can be set to `ask` until trusted — same
`exact-then-prefix.*` rule mechanism every other tool uses. Suggested default:

```toml
# policy.toml [rules]
"world_look"          = "allow"
"world_list_entities" = "allow"
"world.*"             = "ask"     # move/activate/point/say — prompt until trusted
```

---

## 6. End-to-end sequence (agent looks, then moves)

```
model: "Let me look around."  → tool_use world_look {view:"self"}
  agentd turn engine → ToolRequested{session:42, call:{id:91, tool:"world_look", args:{view:"self"}}}
  policy: world_look = allow → Supervisor dispatches to plugin "world"
    plugin: tools/call → IPC look(req,caller_session=42,self)
      world-app: resolve avatar 42's camera → offscreen render 1024×576
                 → wgpu copy_texture_to_buffer → map_async → un-pad → jpeg → b64
      world-app → plugin: {ok, image_b64, manifest}
    plugin → agentd JSON-RPC: { content:[ {text:manifest}, {image:base64 jpeg} ], isError:false }
  agentd → Event::ToolResult{session:42, call:91, output:{ok:true, content:[text,image]}}
  turn engine replays ToolResult (text+image) to provider as a vision block
model: "I see the sensors station ahead-left. Walk to it." → tool_use world_move {to:[..]}
  ... world_move → IPC move → world-app tweens avatar 42 → human watching sees it walk ...
  (avatar status auto-updates from AgentText/ToolRequested events, no extra wiring)
```

The human and the agent share one consistent scene: the agent's `world_move` tweens the
same avatar whose status the human sees pulsing from `AgentText`/`ToolRequested` — because
both the status fan-out (§1.3) and the act tools (§2) drive the *same* Bevy `Transform`/
`AvatarStatus` components in the *same* world-app.

---

## 7. Build order for this dimension

| Step | Deliverable                                                                 | Gate |
|------|-----------------------------------------------------------------------------|------|
| E0   | world-app spawns primitive avatars keyed by `SessionId` from the WS feed    | avatar appears per session |
| E1   | Status fan-out: `AgentText`/`Thinking`/`ToolRequested`→`AvatarStatus` visual | head pulses/spins per event |
| E2   | Local IPC server in world-app + `world-vision-mcp` skeleton (`world_list_entities`) | plugin lists entities over JSON-RPC |
| E3   | `world_look` offscreen render + wgpu readback + jpeg + base64 (free_cam first) | agent receives a JPEG it can describe |
| E4   | `view:"self"`/`station` via roster handshake; annotation manifest           | agent looks through its own eyes |
| E5   | Act tools `world_move`/`world_look_at`/`world_point`; policy `ask` gate      | agent walks; human watches it move |
| E6   | glTF rigged avatars + pose clips driven by `AvatarStatus.phase`             | rigged avatar idles/walks/talks |
| E7   | (later) pixel-space label overlay in the offscreen pass                     | labels burned into the frame |

E3 is the keystone — it proves the entire vision loop end-to-end. Everything before it is
scaffolding; everything after is enrichment.

---

## 8. Open questions (cross-dimension)

- **Provider image-block forwarding (doc 01).** Confirm agentd's anthropic/oai provider
  forwards an MCP `image` content block from a `ToolResult` to the model as a vision block,
  and the exact key shape. Drives §3.2; fallback is a fetchable URL/path.
- **Caller→session correlation (doc 01).** Can the world-app reliably learn
  `ToolRequested.session` for a given `call.id` from its WS feed to feed the roster? If not,
  world tools require an explicit `avatar`/`station` and lose the `self` default.
- **Who owns the wgpu device + entity transforms (doc 02).** Confirmed single `world-app`
  process assumed (§0). If WS-client and renderer split, the IPC endpoint relocates to the
  renderer half.
- **Station registry & camera placement (doc 02/03).** This doc treats stations as named
  pre-placed cameras; the station/world-element dimension owns the registry and naming.
- **Avatar lifecycle policy (doc 01).** Which sessions get embodied? All roots? Only ones
  the human "summons"? When does an avatar despawn — on session end, or persist? Assumed:
  embody root sessions on first `AgentText`, despawn on a TTL after session close.
- **Multi-client write storms (architecture).** world-app is another writer to agentd; the
  "multi-client caveat" in `docs/architecture.md` applies. Acting tools are low-rate and
  human-paced, so likely fine, but worth a load check at E5.
```
