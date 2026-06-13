# 05 — Station & Functional-UI System

> apexos-world design dimension: **the Station system** — the catalog of station
> types, how each one's embedded UI is chosen by *function*, how activating a
> station binds it to an agentd session + the right surface, the generative-UI
> path (agent JSON → panels), and how new station types are added.
>
> Status: design. Branch `proto/world-3d`, scaffolded under `world/`.
> Ground truth read: `agentd/crates/core/src/types.rs` (the `Event` enum),
> `docs/architecture.md` (REST `/api/*` surface + WS protocol + multi-client
> caveat), `docs/ui-glowup.md` (the existing ui-slint views this maps onto),
> `docs/rust-ai-3d-hud-skill/` (the generative-UI placeholder protocol).

---

## 0. What a Station *is*

A **station** is one of ApexOS-RS's existing functional UI surfaces — chat, the
system dashboard, the sensor view, the council view, the terminal, the memory
browser — **given a place in 3D space**. You (or an embodied agent) walk up to a
station, activate it, and the view fills with the UI that suits that station's
*function*.

A station is therefore **not a new kind of program**. It is a *placement* + a
*binding*:

```
station = (a 3D anchor in the world)
        + (a StationKind that names a function)
        + (a binding: which agentd SessionId / endpoint / event-filter feeds it)
        + (a Surface: the Slint component that renders that function)
```

The world is just **another agentd client** — it speaks the exact same WS
`Event`/intent JSON that ui-slint and the browser speak on
`ws://HOST:8787/ws`. A station's surface is a *reuse* of the same conceptual
views ui-slint already ships (`chat_view`, `dashboard`, `sensor_view`,
`council_view`, `terminal`, the Cerebro link); the only new thing the world adds
is *where* the surface lives and *how* it is bound.

> **No agentd fork.** Every station feeds off (a) the WS `Event` stream, (b) the
> REST `/api/*` endpoints, or (c) a *new MCP plugin's* tools — never a core
> change. New capabilities (agent-vision, world state) ride agentd's documented
> extension surfaces. See §7 + the cross-dimension assumptions in §9.

---

## 1. The one screen-fact that shapes everything: surface = focus

Activating a station does **not** spawn a free-floating 2D window in 3D (that is
a later affordance — see §8). The launch model is **single-surface focus**,
mirroring ui-slint's own L0 view-router lesson (`ui-glowup.md` §L0: never paint
five live view-trees at once):

```
        walk up                 activate                 dismiss
  free-fly ───────►  station   ───────►  FOCUS state   ───────►  free-fly
  (3D nav)           prompt                (surface fills          (3D nav)
                     glows                  the viewport)
```

- **Free-fly**: 3D scene owns the screen. Stations render as world objects (a
  console mesh / agent avatar) with a proximity prompt.
- **Focus**: one station's Slint surface is composited over (or onto) the scene.
  Exactly one surface is "hot" at a time. This is the same anti-thrash rule
  ui-slint already enforces — only one functional view-tree is live.

This keeps the Nano-tier lesson honest even though world is Standard/Pro-only:
one surface = one set of WS subscriptions = bounded work.

> **Depends on [3D-scene dimension]:** the proximity/activation gesture, the
> camera transition into focus, and the compositing of a Slint surface over the
> Bevy/wgpu viewport. This doc assumes activation hands us `(station_id, kind)`
> and a place to mount a Slint component. See §9-A.

---

## 2. The StationKind registry (the catalog)

The catalog is a small Rust enum + a static descriptor table. This is the
direct analogue of ui-glowup's `AppKind` — and intentionally so, because the
world reuses those very surfaces.

```rust
/// world/crates/world-stations/src/kind.rs
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StationKind {
    Chat,        // agent conversation       (reuses chat_view)
    System,      // CPU/RAM/disk dashboard    (reuses dashboard)
    Sensors,     // IAQ + thermal             (reuses sensor_view)
    Council,     // multi-agent deliberation  (reuses council_view)
    Terminal,    // /bin/bash PTY             (reuses terminal)
    Memory,      // Cerebro browser           (NEW surface — see §6.5)
    Generative,  // agent-emitted panel spec  (NEW surface — see §5)
}
```

Each kind has a **static descriptor** that says how to bind it and what to draw.
The descriptor is the registry's data; the registry itself is the lookup +
extension seam (§7).

```rust
pub struct StationDesc {
    pub kind:        StationKind,
    pub title:       &'static str,
    pub icon:        &'static str,        // glyph / asset key for the world prompt
    pub binding:     BindingSpec,         // §3 — how it attaches to agentd
    pub surface:     SurfaceFactory,      // §4 — builds the Slint component
    pub feeds:       &'static [Feed],     // §3.2 — what data drives it (doc/debug + auto-subscribe)
    pub tier_min:    Tier,                // Standard for most; Pro for heavy 3D-adjacent
}
```

| StationKind | Function shown | agentd binding (summary) |
|-------------|----------------|--------------------------|
| `Chat` | streaming agent conversation + tool cards + approvals | a `SessionId` (own session); WS events filtered on it |
| `System` | CPU/RAM/disk/uptime bars, IAQ badge | REST poll `POST /api/run` (system tools) — no session |
| `Sensors` | IAQ stats + thermal heatmap | WS `sensor_reading` (broadcast) + `GET /api/snapshot`/thermal frame |
| `Council` | per-agent streaming cards + convergence + synthesis | `council_id` + WS `council_*` events; `POST /api/council` |
| `Terminal` | line-mode PTY | `/terminal-ws` WS (its own socket, not `/ws`) |
| `Memory` | Cerebro recall/search/graph | **agent-vision/world MCP plugin tools** or the external `:8765` link |
| `Generative` | agent-authored panel layout | a `SessionId` + a `world.render_ui` tool result (§5) |

---

## 3. Binding: activate → (session × surface × feed)

Activation is the moment a placement becomes a live UI. The binder runs in the
world's tokio side (the world is a normal agentd WS client; one shared
connection, many station bindings multiplexed over it).

### 3.1 The binding kinds

```rust
pub enum BindingSpec {
    /// Owns a dedicated agentd session. Sends session_init, filters inbound
    /// Events on the returned SessionId. (Chat, Generative.)
    Session { auto_init: bool },

    /// Reads broadcast Events with no session of its own. (Sensors.)
    BroadcastFilter,

    /// Polls a REST endpoint on an interval *only while focused*. (System.)
    RestPoll { path: &'static str, method: HttpMethod, hz: f32 },

    /// Starts/attaches a council; keyed by council_id. (Council.)
    Council,

    /// Opens a side WebSocket distinct from /ws. (Terminal → /terminal-ws.)
    SideSocket { path: &'static str },

    /// Calls MCP plugin tools via a Chat-style session, or the external link. (Memory.)
    Tooled { tool_prefix: &'static str },
}
```

### 3.2 The activation sequence (Session-bound, the common case)

```
 user activates Chat station S
        │
        ▼
 StationManager.activate(S)
        │  look up StationDesc for S.kind
        │
        ├─ BindingSpec::Session{auto_init:true}
        │     └─ send  {"type":"session_init"}            ── over the world's /ws
        │        recv  {"type":"hello","session_id":42}
        │        bind  S ⇄ SessionId(42)
        │
        ├─ build Surface (SurfaceFactory) → a Slint component instance
        │     with a SessionScope(42) so it ignores every other session's events
        │
        ├─ subscribe: route inbound Events where event.session == 42 to S's surface
        │     agent_text → bubble · tool_requested → card · approval_pending → buttons
        │
        └─ enter FOCUS: composite S's surface over the viewport (§1)
```

> **The multi-client filter is mandatory.** `docs/architecture.md` is explicit:
> the gateway broadcasts *every* session's events to *every* socket with no
> server-side filter, and the same `session` field appears on outbound frames.
> The world holds **one** WS connection but may bind **many** sessions (one Chat
> station per avatar, several Generative panels). **Every surface MUST filter on
> `event.session`** or it renders another station's output. A station's
> `SessionScope` is the filter; the `StationManager` is the demux.

Outbound intents carry the same id. To send a prompt into station S bound to
session 42:

```json
{"type":"user_prompt","session":42,"text":"hello"}
```

> **NB on the wire shape (from types.rs):** `SessionId`/`ActionId` are newtypes
> that serialize as **bare numbers**. Tool fields nest under `call` (a
> `ToolCall`): read `call.id` (number), stringify for the row key. Approvals use
> `{"type":"user_approval","session":42,"action":<call.id number>,"granted":true}`
> — the field is `action`, **not** `call_id`/`approved`. Cancel
> (`{"type":"user_cancel","session":42}`) aborts the turn but emits **no**
> `TurnComplete`, so the surface must clear its own busy + pending cards.
> A frame that fails to deserialize is **silently dropped** — wrong field names
> = nothing happens, no error.

### 3.3 Lifecycle

| Phase | What happens | Notes |
|-------|--------------|-------|
| **placed** | station exists in the world, no binding | descriptor known, surface not built |
| **bound** | session/socket/poll acquired | survives leaving focus (session persists) |
| **focused** | surface composited, subscriptions live, polls running | exactly one at a time |
| **blurred** | surface hidden, REST polls paused, session kept | rebinding is free; cheap re-focus |
| **released** | binding torn down (session left to expire, side-socket closed) | on explicit close / world unload |

Sessions are *sticky* across blur so an agent conversation at a station
survives walking away and back (the session id is replayable with
`{"type":"session_init","session_id":42}` — the same replay ui-slint uses).
REST polling (System) pauses on blur — the "cheap when nobody's looking" rule
from the thermal-grid design (`ui-glowup.md` §12).

---

## 4. Surfaces: reuse the ui-slint views, don't reinvent them

A `SurfaceFactory` builds a Slint component for a kind. The crucial design
choice: **the world's surfaces are the same components ui-slint already
ships**, parameterized by a `SessionScope` instead of being globals.

ui-glowup already did this migration once: it retired the chat-only
`agent_text` alias for an `AgentBridge` global and a `VecModel<MessageItem>`
per row, so `ChatView` is "no longer single-instance" (`ui-glowup.md` G2).
The world *extends* that: each surface takes an explicit scope so N instances
can coexist (one per station), all multiplexed over one WS connection.

```
            ui-slint view              world surface (same .slint, scoped)
            ─────────────              ──────────────────────────────────
 Chat       chat_view + input_bar  →   ChatSurface(scope: SessionId)
 System     dashboard              →   SystemSurface(poll: /api/run)
 Sensors    sensor_view            →   SensorSurface(broadcast filter)
 Council    council_view           →   CouncilSurface(council_id)
 Terminal   terminal               →   TerminalSurface(side-socket /terminal-ws)
 Memory     (external link today)  →   MemorySurface  (NEW — §6.5)
 Generative (none today)           →   GenerativeSurface(spec)  (NEW — §5)
```

Reuse is conceptual + literal: the world crate can `import` the existing
`components/*.slint` if the proto vendors them, or re-implement the same prop
contract. Either way the **prop/callback contract is identical** to ui-slint's,
so a fix in one place is portable. The world adds the scope wrapper, not new
view logic.

The `SurfaceFactory` is a fn pointer in the descriptor:

```rust
pub type SurfaceFactory = fn(&BindingHandle) -> StationSurface;
// StationSurface = a handle to the mounted Slint component + its update sink.
```

---

## 5. The generative-UI path (`StationKind::Generative`)

This is the skill's headline idea (`SKILL.md` step 4/7, `ai_protocol.rs`
`RequestRender`/`EntityUpdate` placeholders) mapped onto **real agentd
mechanics**. The skill's `AgentMessage`/`UiEvent` enums are placeholders — the
real path uses agentd's `Event::ToolResult` + a new MCP tool, no new WS message
type.

### 5.1 How it works

An agent in a bound session emits a **tool call** to a world-provided MCP tool
(`world.render_ui`, see §7 + §9-B). The supervisor runs it, and the result
returns to the world over the *normal* `Event::ToolResult` path. The world maps
the structured JSON into a fixed set of **panel templates** — exactly the
skill's "agent emits JSON specs that map to pre-defined component templates +
data" (`SKILL.md` step 4, line 99), and exactly Slint's honest limit (`SKILL.md`
Limitations: Slint is compiled — generative UI = data→templates, not runtime
widget creation).

```
 agent (session 42)                         world Generative station
 ──────────────────                         ────────────────────────
 calls tool world.render_ui                 (bound, focused)
   args = { title, panels:[…] }
        │                                          ▲
        │ Event::ToolRequested {call}              │
        ▼  (supervisor runs the world MCP plugin)  │
   Event::ToolResult {                             │  filter session==42
     session:42, call:<id>,                        │  parse output.content
     output:{ ok:true, content: <UiSpec JSON> } } ─┘  → map to panel templates
                                                       → mount/refresh surface
```

The world client subscribes to `tool_result` where `call.tool == "world.render_ui"`
(it already knows the `call.id` from the preceding `tool_requested`) and feeds
`output.content` to the `GenerativeSurface`.

### 5.2 The UiSpec (the contract the agent writes)

A small, closed, versioned schema. Closed = every `panel.kind` maps to a Slint
template the world already compiled (type-safe, no arbitrary widgets):

```jsonc
{
  "v": 1,
  "title": "Air Quality — last 24h",
  "panels": [
    { "kind": "metric",  "label": "IAQ",      "value": 42, "unit": "",    "trend": "down" },
    { "kind": "gauge",   "label": "CO2 eq",   "value": 610, "min": 400, "max": 2000 },
    { "kind": "text",    "markdown": "Ventilation looks healthy." },
    { "kind": "bar_list","items": [ {"label":"VOC","value":0.3}, {"label":"RH","value":48} ] },
    { "kind": "actions", "buttons": [
        { "label": "Open vents", "intent": { "type":"user_prompt", "text":"open the vents" } },
        { "label": "Snapshot",   "tool":   { "tool":"system_snapshot", "args":{} } } ] }
  ]
}
```

Panel kinds at launch: `metric`, `gauge`, `bar_list`, `text` (markdown),
`actions`, `image` (a base64/URL surface — reuses the texture path the skill
anticipates for live surfaces). Each is a `for`-driven Slint template fed from a
`VecModel<PanelItem>` (the standard Slint data-driven pattern, `SKILL.md`
"Dynamic Slint from data"). Adding a panel kind = add a template + a match arm
(§7), the same extension shape as adding a station kind.

### 5.3 Action affordances close the loop

An `actions` panel button carries either a raw **intent** (sent verbatim over
`/ws` with the station's `session` injected) or a **tool** request routed back
to the agent. This is the skill's `UiEvent` → real agentd intent mapping: the
generative UI is not read-only; the agent can offer the user (or another agent)
buttons that drive `user_prompt` / approvals. Unknown panel kinds render as a
labeled fallback card (forward-compatible — a v2 agent talking to a v1 world
degrades gracefully, never silently blanks).

---

## 6. The launch catalog (4–6 concrete station types)

### 6.1 Chat station  — `StationKind::Chat`
- **Function:** talk to one agent. The world's primary surface; usually attached
  to an **agent avatar** (the avatar *is* the chat station — walk up, talk).
- **Binding:** `Session{auto_init:true}` → owns a `SessionId`.
- **Feeds (WS, filtered on its session):** `agent_text` (delta → bubble; also the
  *only* busy signal — `turn_started` is Python-only, never emitted by the Rust
  daemon), `tool_requested` (`call` → card), `tool_result` (update card by
  `call.id`), `approval_pending` (approve/reject → `user_approval`),
  `turn_complete` (clear busy). Sub-agent spawns surface as `sub_agent_started`.
- **Sends:** `user_prompt`, `user_approval`, `user_cancel` (all with `session`).
- **Reuses:** `chat_view` + `input_bar`.

### 6.2 System station — `StationKind::System`
- **Function:** host vitals — CPU/RAM/disk/uptime bars + IAQ badge. A "monolith"
  console in the world center.
- **Binding:** `RestPoll{ path:"/api/run", method:POST, hz: 0.5 }` (or the
  individual apexos-tools telemetry verbs cpu_temp/memory/disk/uptime). No
  session. Polls **only while focused**.
- **Feeds:** REST JSON. Optionally also listens to broadcast `sensor_reading`
  for the IAQ badge without polling.
- **Reuses:** `dashboard`.

### 6.3 Sensor station — `StationKind::Sensors`
- **Function:** environment — IAQ stats (BME688 `air_quality`) + thermal heatmap
  (MLX90640). A garden/greenhouse-themed node.
- **Binding:** `BroadcastFilter` on `Event::SensorReading { reading: {kind …} }`
  — `air_quality` and `thermal_frame` variants. The 32×24 raw grid is **not** on
  the broadcast (types.rs keeps events small); for the live heatmap, on-demand
  fetch `GET /api/snapshot`/thermal-frame *while focused* (the on-demand pattern
  from `ui-glowup.md` §12). Summary min/mean/max from the WS drives an ambient
  "breathing" effect even when blurred.
- **Reuses:** `sensor_view`.

### 6.4 Council station — `StationKind::Council`
- **Function:** watch / convene a multi-agent deliberation — per-agent streaming
  cards (accent dot from `CouncilAgentDef.color`), convergence bar, synthesis.
  Naturally a **ring of avatars** in the world; the council station is the table
  they sit around.
- **Binding:** `Council` — `POST /api/council` to start (topic + agents), key on
  `council_id`; or `GET /api/council[/:id]` to attach to a running one.
- **Feeds (WS):** `council_started`, `council_round_start` (clears the per-round
  transcript), `council_agent_delta`, `council_agent_done`, `council_round_done`
  (convergence), `council_complete` (synthesis; reason = consensus|max_rounds|
  stopped), `council_butt_in`. Human butt-in → `POST /api/council/:id/butt-in`.
- **Reuses:** `council_view`.

### 6.5 Memory station — `StationKind::Memory`
- **Function:** browse Cerebro — recall, semantic search, the memory graph,
  episodes. The "library/archive" node (agent FORGE's brain).
- **Binding:** two routes, tier/availability-gated:
  1. **In-world surface (preferred):** `Tooled{ tool_prefix:"cerebro_" }` — a
     hidden Chat-style session whose only job is to call cerebro-mcp tools
     (`memory_search`, `recall`, `session_recall`, `memory_neighbors`,
     `list_episodes`, `get_episode_memories`). Results return as `tool_result`
     and render into a search/results/graph surface. **Caveat:** ANN search
     needs an embed model (Micro+); on Nano it's FTS5-only — but world is
     Standard/Pro anyway, so embeddings are present.
  2. **External link fallback:** open `http://HOST:8765/?token=…` (the
     `cerebro-api` dashboard) in the system browser — Slint has no webview
     (`ui-glowup.md` L2/§10), so this matches ui-slint's current behavior.
- **Reuses:** new `MemorySurface` (search box + result list + neighbor graph),
  conceptually the cerebro dashboard reimplemented as native panels.
- **Depends on [agent-vision/world-MCP dimension]:** whether memory browsing
  goes through a dedicated world plugin tool or directly via cerebro-mcp (which
  is already a registered plugin). See §9-B.

### 6.6 Terminal station — `StationKind::Terminal`
- **Function:** a real `/bin/bash` PTY (log-watching, quick commands). A
  "maintenance terminal" node.
- **Binding:** `SideSocket{ path:"/terminal-ws" }` — its **own** WebSocket,
  distinct from `/ws` (the PTY backend already exists). Line-mode: stream
  ANSI-stripped output into a ring buffer, `TextInput` submits a line to stdin.
  No cursor grid (curses apps garble — full VTE deferred, same as ui-slint).
- **Reuses:** `terminal`.

> Launch set = the 6 above. Chat + Council are avatar-attached; System/Sensors/
> Memory/Terminal are object consoles. Generative (§5) is the 7th kind but is a
> *mechanism*, not a fixed catalog entry — agents conjure it on demand.

---

## 7. Adding a new station type (the extension pattern)

The registry is closed-by-design (compiled Slint), so a new station kind is a
**small, localized diff** in the world crate — never an agentd change:

```
1. StationKind         add a variant                         (kind.rs)
2. StationDesc          add a row: title, icon, binding,      (registry.rs)
                        feeds, tier, SurfaceFactory
3. Surface              add the .slint component +            (surfaces/)
                        its SessionScope wrapper + update sink
4. Binder               if it needs a binding shape not in    (binding.rs)
                        BindingSpec, add the variant + its arm
5. World placement      register the spawn (mesh + prompt)    [3D-scene dim]
```

For **generative panels** the diff is even smaller: one `panel.kind` string +
one Slint template + one match arm in the `UiSpec` mapper (§5.2). No registry
touch.

For **new agentd-side capability** (e.g. live world-state, agent-vision
snapshots, a richer memory query): add it as an **MCP plugin** registered in
`plugins.toml` exposing tools (`ToolSpec`s announced via `Event::PluginUp`), and
consume the results over the existing `Event::ToolResult` path. This is the
*only* sanctioned way to extend agentd (the supervisor host + virtual-tool
pattern, `docs/architecture.md`). The world's own server-side additions
(`world.render_ui`, `world.snapshot`, world-state queries) all live in **one new
world MCP plugin** — see §9-B.

```
            ┌──────────── world client (Bevy + Slint) ────────────┐
            │  StationManager ── demux on event.session ──► surfaces│
            └───────────▲───────────────────────────┬──────────────┘
                        │ Event JSON                 │ intent JSON
                        │                            ▼
              ws://HOST:8787/ws  ◄────────── agentd gateway (UNCHANGED)
                        ▲                            │
                        │ Event::ToolResult          │ ToolRequested
                        │                            ▼
              ┌─────────┴──────────┐      ┌──────────────────────┐
              │ world-mcp plugin    │◄────│ supervisor (UNCHANGED)│
              │ world.render_ui     │     └──────────────────────┘
              │ world.snapshot …    │
              └─────────────────────┘   (registered in plugins.toml — extension surface)
```

---

## 8. Deferred / post-proto

- **Free-floating windowed stations in 3D** — a station rendered as a 3D quad you
  can pull off into space (maps to ui-glowup's WindowDesc/AppWindowFrame, but in
  world coordinates). Launch model is single-surface focus (§1) first.
- **Multiple simultaneous live surfaces** (e.g. a wall of dashboards) — bounded
  by tier; one focused surface at launch.
- **Station persistence / world save** — which stations exist + their bindings
  serialized with the world. The descriptor is `Serialize`-ready; the layout
  store is a [world-state dimension] concern.
- **VR (Quest 3)** — surfaces as 3D quads with controller raycast input; the
  station model is unchanged, only the mount + input route differ (SKILL.md VR
  section).
- **Agent-authored *station placement*** — an agent `world.place_station(kind,
  pos)` tool (vs only `world.render_ui` content). Extends §7-step-5 to runtime.

---

## 9. Cross-dimension assumptions (named, not assumed-solved)

**A — [3D scene / navigation / avatar dimension].** This doc assumes:
- activation delivers `(station_id, StationKind)` to `StationManager.activate`;
- there is a mount point to composite a Slint surface over the wgpu viewport
  (the skill's Slint-hosts-Bevy Pattern A, or Bevy-primary + Slint-overlay
  Pattern B);
- avatars expose an attachment so Chat/Council stations bind to an avatar;
- a focus/blur camera transition exists and calls our lifecycle hooks (§3.3).

**B — [agent-vision / world-MCP plugin dimension].** This doc assumes a single
new MCP plugin (registered in `plugins.toml`, surfaced via `Event::PluginUp`)
provides the world's server-side tools: at minimum `world.render_ui` (the
generative-UI sink, §5) and `world.snapshot` (agent-vision — render the avatar
camera, return an image ref). Memory-station tool routing (§6.5) may reuse the
existing `cerebro-mcp` plugin directly rather than proxy through world-mcp — that
choice belongs to that dimension. **No agentd core change** is assumed or
permitted by either.

**C — [session/world-state dimension].** This doc assumes the world holds **one**
`/ws` connection and multiplexes many bindings over it (matching the gateway's
broadcast-to-all model + mandatory client-side `session` filtering). If a future
design instead opens one socket per station, the `SessionScope` filter becomes
redundant but the descriptor/binding model is unchanged.

**D — [tier / hardware dimension].** World targets **Standard/Pro only** (per the
brief + CLAUDE.md tiers) — Nano/Micro are excluded, so embeddings (Memory ANN)
and GPU 3D are assumed present. `tier_min` in `StationDesc` still gates
individual heavy stations within Standard/Pro.

---

## 10. Build order for this dimension (proto)

| Step | Deliverable | Gate |
|------|-------------|------|
| S0 | `StationKind` + `StationDesc` registry + `StationManager` skeleton (no 3D) | unit test: lookup + activate returns a binding for Chat |
| S1 | `ChatSurface` scoped to a `SessionId`, multiplexed over one `/ws` | two Chat stations, two sessions, no cross-talk (filter works) |
| S2 | `System` (REST poll, focus-gated) + `Sensors` (broadcast filter) | live bars + IAQ against a real agentd |
| S3 | `Council` + `Terminal` (side-socket) | council streams; PTY echoes a command |
| S4 | `Generative` surface + UiSpec mapper (5 panel kinds + actions) | agent `world.render_ui` JSON renders panels; an action button drives `user_prompt` |
| S5 | `Memory` (cerebro-mcp tool route, in-world results) | `memory_search` results render natively |

Discipline mirrors the repo: each gate = a commit; surfaces reuse ui-slint
prop contracts so fixes stay portable.
