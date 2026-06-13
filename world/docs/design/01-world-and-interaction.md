# apexos-world — World & Interaction Design

> Design dimension: **World & interaction design** — the spatial metaphor and the
> core interaction loop.
> Codename: `apexos-world`. Prototype branch `proto/world-3d`, scaffolded under
> `/home/andre/Projects/ApexOS-RS/world/`, to be extracted to its own repo later.
>
> This is an **interface first, a world second.** It is not a game. It is a third
> client of agentd — peer to `ui-slint` and the browser/PWA — that happens to be a
> navigable 3D space. It speaks agentd's *real* `Event`/Intent wire protocol on
> `ws://HOST:8787/ws` (the same JSON `ui-slint` speaks), and earns its new powers
> (agent-vision, world state) through agentd's documented extension surfaces (a new
> MCP plugin), never by forking the core.

---

## 0. The one-sentence thesis

> An ApexOS-RS session is normally a *list*; here it is a *place*. You walk up to a
> thing, you activate it, the world recedes and the thing's own UI fills the view;
> you step back and you are in the space again. The 3D layer is a **router and a
> context**, not a renderer of business data — the function-appropriate UI (chat,
> sensors, council, terminal, memory) is still the real surface, lifted verbatim
> from `ui-slint`'s views.

Why a space at all, for an *AI* interface specifically:

1. **An agent already has a body of work that is plural and concurrent.** agentd
   runs many sessions at once (root + sub-agents, councils, the scheduler's
   wake-loop). A flat client shows one at a time and a session picker. Space lets
   *all of them coexist and be glanced at* — a sub-agent is literally a smaller
   figure standing next to its parent; a council is six figures in a ring.
2. **Agents can be embodied and can see.** Because an agent has an avatar with a
   camera, "show me what you're looking at" becomes a literal snapshot (the
   agent-vision loop, §7). A list UI has nowhere to point a camera.
3. **Telemetry has a natural elsewhere.** Sensor readings, mesh peers, and memory
   are *ambient* — they want to be in the periphery, felt, not modal. A space has a
   periphery; a list does not.
4. **Approach = intent.** Walking toward a station is a low-commitment, reversible
   gesture that pre-loads context without a click. Stepping back is "I'm done"
   without a close button. The loop maps to how attention actually moves.

Non-goals: combat, physics puzzles, scoring, procedural terrain, photoreal art.
Performance budget and rendering are owned by another dimension; here we only assume
"a 3D scene with avatars, station props, billboarded labels, and live-texture panels
runs at interactive framerate on Standard/Pro tier."

---

## 1. The space: an **Atrium** (hub), not a campus or a starfield

Three candidate metaphors were on the table. The decision:

| Metaphor | What it is | Verdict |
|----------|-----------|---------|
| **Campus** | many buildings, streets, you travel | Too big. Travel time is friction with no payoff for a single-node interface. Distance must *mean* something, and on one node it doesn't. |
| **Starfield** | agents float in void, fly between | Evocative but disorienting; no floor, no up, no "where am I", hard to teach. Good *far-future mesh* view, bad daily driver. |
| **Atrium (chosen)** | one bounded room you can see across; stations line the walls; agents stand on the floor; telemetry is the architecture itself | Everything visible at once → it's a dashboard you can walk in. Bounded → never lost. Extends cleanly to a *concourse of atria* (one per mesh node) when the mesh grows. |

> **Decision: the primary space is a single bounded Atrium** — a circular/hex room,
> ~20–30 m across, with a clear floor, a center, and a perimeter. You can see the
> whole thing from the center; navigation is about *facing and approaching*, not
> *traveling*.

```
                        ┌──────────── ATRIUM (one agentd node) ────────────┐
                        │                                                    │
   PERIMETER = stations │   [TERMINAL]   [SENSORS]   [MEMORY GATE]          │
   (functional surfaces)│  ┌────────┐   ┌────────┐   ┌────────┐            │
                        │  │  PTY   │   │ IAQ /  │   │ cerebro │            │
                        │  │ live   │   │ thermal│   │ browser │            │
                        │  └────────┘   └────────┘   └────────┘            │
                        │                                                    │
                        │            ◇ APEX (root agent avatar)            │
   FLOOR = agents       │              busy-glow when streaming            │
   (root, sub-agents,   │         · sub-agent      · sub-agent             │
    council ring)       │           (child sess)     (child sess)          │
                        │                                                    │
                        │     ┌──────────── COUNCIL RING ───────────┐       │
                        │     │  ◇   ◇   ◇   ◇   (convened agents)   │       │
                        │     └──────────────────────────────────────┘       │
                        │                                                    │
                        │  [SETTINGS]    [POWER]     [MESH PORTAL]          │
                        │  (soul.md,     (modal)     (peers → other         │
                        │   policy)                   nodes' atria)         │
   ARCHITECTURE =       │                                                    │
   ambient telemetry    │  floor pulse = CPU · wall tint = IAQ · light = busy│
                        └────────────────────────────────────────────────────┘
```

The Atrium has a **fixed canonical layout** the user learns once (muscle memory:
"sensors are back-left"). Agents are dynamic; stations are fixed furniture.

---

## 2. The two primary entity types

### 2.1 Agent avatars (dynamic, one per agentd `SessionId`)

An **agent avatar** is a figure on the floor that *is* an agentd session. It is the
one concept the seed (`bevy_viz.rs` `VizEntity { id, kind }`, and SKILL.md's "agent
avatars driven by pose streams") explicitly anticipates.

| Property | Source (real agentd) |
|----------|----------------------|
| identity / world id | `SessionId` (u64, serializes as a bare number — stringify for the entity registry key) |
| root vs. child | root = no parent; child appears via `SubAgentStarted { parent, child, prompt }` and stands beside its parent |
| **busy / streaming** | driven **only** by `AgentText { delta }` arriving (per architecture.md: there is *no* `turn_started` in the Rust daemon; `turn_complete` ends busy). Busy = avatar glows / leans forward / particle stream from head. |
| thinking | `AgentThinking { delta }` → a dimmer, inward "pondering" state distinct from speaking |
| persona color | council agents carry `CouncilAgentDef.color` (hex); root APEX has a fixed identity. Reuse `ui-slint`'s persona palette. |
| messaging between agents | `AgentMessage { from, to, body }` / `AgentMessageAck` → a visible arc/beam from avatar `from` to avatar `to` |
| spoke last / went quiet | local timing off `AgentText` / `TurnComplete` → idle avatars dim and settle |

There is always **at least one** avatar: **APEX**, the root session, standing near
center. Sub-agents are spawned and despawned with their child sessions. The council
is a *temporary ring* of avatars (§6).

> Avatar art is a placeholder (a glowing capsule / sigil with a billboarded name +
> persona color) in the prototype, per the seed's "simple PbrBundle, evolve to glTF
> later." Rigging/animation richness is another dimension's call.

### 2.2 Stations (fixed, one per agentd *function*)

A **station** is a piece of perimeter furniture that, when activated, fills the view
with one of `ui-slint`'s existing functional views. Stations are **not** sessions;
they are *function portals*. The mapping to real agentd surfaces:

| Station | Activated UI (lift from `ui-slint`) | Backing agentd surface |
|---------|--------------------------------------|------------------------|
| **Chat** (this is usually *the agent avatar itself*, see §4) | streaming chat + tool cards + approvals | `UserPrompt` out; `AgentText`/`ToolRequested`/`ToolResult`/`ApprovalPending` in |
| **Sensors** | IAQ stats + thermal heatmap | `SensorReading { node_id, reading, timestamp }` (`AirQuality`, `ThermalFrame`, …) |
| **Terminal** | PTY view | agentd `/terminal-ws` endpoint |
| **Memory Gate** | cerebro browser (search/episodes/graph) | cerebro-api REST (:8765) / cerebro-mcp tools via agent — *link/iframe-equivalent; see assumption A4* |
| **Settings** | soul.md editor, policy mode, plugin list | REST `/api/*`; `EvolutionProposed`/`EvolutionApplied` shown as world events |
| **Power** | power modal | REST |
| **Council** | council deliberation view | `CouncilStarted`/`…RoundStart`/`…AgentDelta`/`…Complete` — but rendered *spatially* as a ring, §6 |
| **Mesh Portal** | peer list → travel to another node's atrium | `PeerSeen`/`PeerRegistered`/`PeerLost`; reconnect WS to peer `ws_url` |

> **Decision: a station's activated surface is the *same Slint view* `ui-slint`
> already ships**, rendered onto a panel (live-texture or 2D overlay — the
> Slint/Bevy compositing mechanism is another dimension's call; this doc only
> requires "a station can host an existing Slint view"). We do **not** reinvent
> sensors/chat/terminal in 3D. The 3D layer routes to them.

---

## 3. The core interaction loop: **approach → activate → fill → dismiss**

This is the heart of the dimension. Four states, fully reversible, with concrete UX.

```
   ┌─────────┐   approach (proximity)   ┌──────────┐   activate (E / click / VR grip)
   │ ROAMING │ ───────────────────────► │ FOCUSED  │ ──────────────────────────────┐
   │ (in the │ ◄─────────────────────── │ (near a  │                                │
   │  space) │     step back / move     │  target, │                                ▼
   └─────────┘       away                │  primed) │                          ┌──────────┐
        ▲                                 └──────────┘                          │ ACTIVE   │
        │                                      ▲                                │ (view    │
        │              dismiss (Esc / step back / "done")                       │  fills   │
        └──────────────────────────────────────────────────────────────────────│  screen) │
                                                                                 └──────────┘
```

### State 1 — ROAMING
You move through the Atrium (navigation §5). Avatars and stations are at rest.
Ambient telemetry plays in the architecture. A faint reticle/cursor sits at screen
center. Nothing is selected. This is the default; you return here after every
interaction.

### State 2 — FOCUSED (proximity priming)
When your reticle/position brings a target within an **interaction radius** (or your
gaze ray hits it), the target *primes*:
- it highlights (outline glow in its persona/station color),
- a **billboard tooltip** appears with a one-line identity + live status —
  for an agent: name, `SessionId`, busy/idle, last-activity; for a station: name +
  a live preview metric (Sensors → current IAQ; Terminal → last line; Council →
  round N).
- a prompt hint: **"E — open"** (desktop) / a controller highlight (VR).

No commitment yet. Approaching is *reading*, not *opening*. Walking away returns to
ROAMING with zero side-effects. This is the cheap-glance affordance that a list UI
cannot give.

### State 3 — activate → ACTIVE (the view fills)
On the activate input (desktop: `E` or click; VR: controller grip/trigger):
- the camera **eases in** toward the target (a short ~250 ms dolly so you keep your
  bearings — *not* a hard cut),
- the world **dims and blurs** behind,
- the **function-appropriate UI fills the central focus plane**:
  - **Agent avatar → Chat surface** bound to *that* `SessionId`. Sending types
    `{"type":"user_prompt","text":…}`; streaming `AgentText` deltas fill the bubble;
    `ToolRequested` renders a tool card; `ApprovalPending` shows approve/reject
    (sends `{"type":"user_approval","action":<ToolCall.id>,"granted":bool}`). This is
    `ui-slint`'s chat view verbatim. **Critical:** filter inbound events on the
    injected `session` field — the gateway broadcasts *every* session to *every*
    socket with no server-side filter (architecture.md multi-client caveat), so the
    ACTIVE surface only renders events whose `session` == the focused avatar's id.
  - **Sensors station → sensor view** fed by `SensorReading`.
  - **Terminal → PTY**, **Settings → soul/policy editor**, etc.
- a persistent **"⤺ step back"** affordance (Esc hint) is always visible.

ACTIVE is *modal-ish but not trapping*: peripheral ambient telemetry (the floor
pulse, a wake flash) can still bleed through the blur as faint cues, so an alert
(e.g. `WakeTriggered`, a sensor alarm) is never fully hidden.

### State 4 — dismiss
Esc, the step-back affordance, or (VR / free-roam) physically moving away pulls the
camera back out, un-blurs the world, and returns to ROAMING. The session/station
keeps running underneath — dismiss is "look away," not "stop." A still-busy agent's
avatar keeps glowing after you step back, so you can wander off and watch it work
from across the room.

### Loop invariants
- **Every state is reversible with one input** (move away / Esc). No dead ends.
- **No modal dialog ever covers the whole screen without a visible exit.**
- **Approach never sends a network intent.** Only *activate* and explicit in-surface
  actions (send, approve, run) produce agentd intents. Browsing the room is free.
- **Busy is owned by the avatar, not the surface.** You can dismiss a chat and still
  see the agent is busy from the floor.

---

## 4. Special case: the agent avatar *is* the chat station

Worth stating plainly because it collapses two concepts: for an agent avatar,
"activate" opens that agent's chat surface. There is therefore **no separate "Chat"
furniture** for the root agent — you talk to APEX by walking up to APEX. Sub-agent
chats work the same way (activate the child figure → chat bound to the child
`SessionId`; useful for inspecting a spawned worker mid-task). Stations remain for
the *non-conversational* functions (sensors, terminal, settings, power, memory,
mesh, council-as-a-place).

This is the single most "AI-native" gesture in the design: **the conversation has a
location and a face**, and concurrent conversations are concurrent figures.

---

## 5. Navigation (desktop first, VR later)

Desktop monitor is the primary target (Standard/Pro tier; explicitly *not* Pi
Zero/Nano). Three locomotion modes, all available, layered by skill:

| Mode | Input | Use |
|------|-------|-----|
| **Walk** | WASD + mouse-look (reticle = gaze ray) | default; matches the "approach" semantics; FOV-limited so approaching genuinely changes what's primed |
| **Fly** | hold space / Q-E for vertical, same look | optional; for getting an overview of a busy room (many sub-agents, a council in session) — rise above the floor and look down |
| **Teleport / snap-to** | click a station or avatar from anywhere, or press its number key | *the fast path* — most of the time you don't want to walk. Click a primed/visible target and the camera arcs to its FOCUSED position. This is the keyboard-driver's affordance and the bridge to "it's still a dashboard." |

> **Decision: teleport/snap-to is a first-class navigation mode, not an
> accessibility afterthought.** Daily use is "snap to APEX, talk, snap to sensors,
> glance, snap back." Walking is for *spatial reading* (who's near whom, how full the
> room is), not for traversal cost. This keeps it an interface, not a chore.

A persistent minimal HUD (Slint overlay, always-on, never blurred): connection
status to agentd, current node name, a compass/heading, and an alert ribbon for
`WakeTriggered` / errors / approval-pending count. The HUD is the one thing that does
**not** obey the world (it is the "operating system chrome").

**VR (Quest 3, later):** Walk → roomscale + thumbstick glide; Teleport → arc
pointer (standard VR comfort locomotion); activate → controller trigger on a primed
target; the ACTIVE surface becomes a world-space quad you lean toward. Same four
states, same event wiring. Gated behind a feature flag; not in the v1 prototype.

---

## 6. Council as a *place* (not just a stream)

Council is the strongest argument for spatiality, so it gets a bespoke treatment.
agentd emits a full council event family: `CouncilStarted { council_id, topic,
agents: [CouncilAgentDef] }`, `CouncilRoundStart { round }`, `CouncilAgentDelta {
agent_id, delta }`, `CouncilAgentDone`, `CouncilRoundDone { convergence, agreements }`,
`CouncilComplete { reason, synthesis }`, `CouncilButtIn`.

Spatial mapping:
- `CouncilStarted` → a **ring of avatars** rises in the Atrium's council zone, one
  per `CouncilAgentDef`, colored by its `color`, labeled by `persona`.
- `CouncilAgentDelta` → the speaking avatar lights up and streams text into a
  billboard above it (you *see who is talking* without reading names).
- `CouncilRoundDone.convergence` (f32) → the ring **tightens** as convergence rises —
  agreement is visible as the agents drawing closer; `agreements` list floats at
  center.
- `CouncilComplete.synthesis` → the ring resolves into a single central panel with
  the synthesis; `reason` ("consensus" | "max_rounds" | "stopped") tints it.
- `CouncilButtIn` → the human's interjection appears as a beam from the user's
  position into the ring.

ACTIVE-ing the council zone fills the view with `ui-slint`'s council view for the
full transcript; ROAMING lets you watch the deliberation as choreography from across
the room. **This is the canonical "made spatial" win:** the same stream, but
convergence is a distance and turn-taking is a spotlight.

---

## 7. Agent vision — the embodied snapshot loop

The defining AI-native capability and the reason avatars carry cameras. The seed
anticipates it ("screenshot → agent-vision loop", `RequestRender`/`RequestScreenshot`
in `ai_protocol.rs`). The real wiring (this doc names the dependency; the MCP plugin
is another dimension's build):

```
 agent (in a turn) calls a tool:  world_snapshot { from: "self" | <session_id> | "overview" }
        │  (this is a NEW MCP tool — registered as an agentd plugin per
        │   docs/sdk/02-mcp-plugins.md + 03-adding-tools.md; NOT a core fork)
        ▼
 agentd Supervisor → ToolRequested { call:{ id, tool:"world_snapshot", args } }
        ▼  (broadcast on the bus; the world client is subscribed like any UI)
 apexos-world receives ToolRequested, recognizes tool=="world_snapshot",
   renders the requested view to an offscreen target, encodes (PNG/JPEG),
        ▼
 returns the image to the snapshot plugin → ToolResult { call:<id>, output:{ ok, content } }
        ▼
 the image content (or a cerebro vision-memory ref) flows back into the agent's
   turn as a tool result → the agent can now "see" the world it inhabits.
```

So an agent can literally request "what does my avatar see right now" or "give me an
overview of the room," and act on it (move toward a station, address a sub-agent,
notice a sensor alarm rendered on the wall). The image transport reuses the seed's
`ImageFrame`/binary-frame pattern conceptually but is delivered through agentd's
**real** `ToolResult` envelope, not a placeholder `AgentMessage`.

> **Assumption (names another dimension):** the `world_snapshot` MCP plugin and its
> tool schema, and whether the rendered bytes go back inline vs. via a cerebro
> `search_vision`/`describe_image` memory ref, are owned by the *agent-vision /
> protocol* dimension. This doc only fixes the *world side*: the client subscribes to
> `ToolRequested`, can render any avatar camera or an overview to an offscreen
> target, and replies via the normal tool round-trip.

---

## 8. Explorable world elements beyond pure utility

"Explorable" = things worth walking over to that aren't a button. Three, each backed
by a real event stream so they are *live*, not decoration:

### 8.1 Telemetry as architecture (ambient, always-on)
The room's *physical state* encodes node health, so you read it pre-attentively:
- **Floor pulse** ← CPU/load (polled via REST `/api/*` system telemetry, as the
  dashboard already does). Calm slow pulse idle, fast bright pulse under load.
- **Wall tint** ← IAQ from `SensorReading::AirQuality.iaq` — the whole room subtly
  greens/ambers/reds with air quality; a thermal hotspot from `ThermalFrame.max_c`
  can cast a warm patch on a wall.
- **Ambient light** ← agent busy state (any avatar streaming `AgentText` lifts the
  room light a touch). The Atrium "breathes" with the daemon.
- **Wake flash** ← `WakeTriggered` pulses the whole room once (you feel it even mid-
  conversation, through the ACTIVE blur).

Walking to the **Sensors station** and activating it drills from ambient → exact
numbers (the heatmap). Ambient is the glance; the station is the read.

### 8.2 Memory as a place (the Memory Gate)
cerebro is the agent's brain; it deserves a doorway, not a tab. The **Memory Gate**
is a portal-like station whose surface, when activated, is the cerebro browser
(search, episodes, the memory graph). Beyond utility, the gate can render **recent
episode activity as motes** drifting near it (driven by cerebro-mcp episode/recall
activity that the agent performs during turns), so a "thinking hard about memory"
moment is visible as the gate stirring. (Exact cerebro event/animation binding is
soft — see assumption A4.)

### 8.3 Mesh nodes as locations (the Mesh Portal → concourse)
This is where the Atrium scales past one node. mDNS discovery emits `PeerSeen {
node_id, ip }`; bootstrap emits `PeerRegistered { node_id, ws_url, role }`; dropout
emits `PeerLost`. The **Mesh Portal** is a ring of doorways, one per known peer:
- a newly-`PeerSeen` peer shimmers as a half-formed doorway (discovered, not joined),
- a `PeerRegistered` peer is a solid doorway labeled with `role` (e.g. an inference
  Pro/GPU node vs. a kiosk),
- a `PeerLost` peer's doorway fades.

**Stepping through** a peer doorway = **reconnect the WS client to that peer's
`ws_url`** and load *its* Atrium (its agents, its sensors, its telemetry). The
"campus" emerges only when you have a mesh: a **concourse of atria**, one room per
node, connected by portals. A single-node install never sees more than its own room
— which is correct; distance only appears when there's somewhere to go.

```
   [ this node's ATRIUM ]  ──portal──►  [ peer "spark-dgx" ATRIUM ]
          │                                  (role: inference, 70B)
       portal                                       │
          ▼                                       portal
   [ peer "pi-kiosk" ATRIUM ]  ◄───────────────────┘
        (role: kiosk)
```

---

## 9. How human and agent share the space

They are **co-present but asymmetric**, which is the point.

| | Human | Agent (APEX / a sub-agent) |
|---|-------|----------------------------|
| presence | a free camera (no self-avatar needed on desktop; a hand/controller in VR) | an avatar figure on the floor |
| moves by | WASD / fly / teleport (§5) | agent-driven motion is **optional/post-v1**; v1 avatars stay near their canonical spots and *express* state in place. A `world_move` MCP tool could later let an agent walk toward a station it wants to "use." |
| sees by | the monitor | `world_snapshot` (§7) — the agent's avatar camera |
| acts by | activate → in-surface actions → intents (`user_prompt`, `user_approval`) | tool calls (real agentd tools) that the world *reflects*: a tool round-trip lights the avatar; an `EvolutionProposed` from the agent surfaces at the Settings station as a pending world event |
| attention cue | reticle / where the camera faces | busy-glow, thinking-state, the message-beam of `AgentMessage` |

Shared moments that make co-presence legible:
- **You approach a busy APEX** → you see it's mid-turn (glowing) before you even open
  chat; you can wait, or open chat and watch the stream land.
- **APEX spawns a worker** (`SubAgentStarted`) → a new small figure appears beside it
  *while you're standing there*; you can step over and open the child's chat to watch
  it.
- **Agent-to-agent** (`AgentMessage from→to`) → a visible beam; the room shows the
  org chart of an active multi-agent task without you opening anything.
- **The agent asks to see** (`world_snapshot`) → optionally a brief "shutter" cue on
  the relevant avatar so the human knows the agent just looked. (Transparency: when
  the AI is watching the room, the room shows it.)

> **Assumption (names another dimension):** agent-initiated *movement* and any
> `world_move`/`world_face` tools belong to the agent-vision / world-state plugin
> dimension. v1 of *this* dimension assumes agents are expressive-in-place; motion is
> a clean later extension that does not change the four-state loop.

---

## 10. Concrete v1 scope (what the prototype must demonstrate)

The minimal loop that proves the metaphor, all against a live agentd over the real
protocol:

1. **One Atrium**, single node, fixed layout: floor + perimeter station slots +
   center for APEX.
2. **APEX avatar** = root session: connect WS, `{"type":"session_init"}` → `hello`
   gives the `SessionId`; avatar glows on `AgentText`, settles on `TurnComplete`.
3. **Approach → FOCUSED tooltip → activate → Chat surface** bound to APEX's
   `SessionId`, filtering inbound events on `session`. Send `user_prompt`; render
   `AgentText`/`ToolRequested`/`ToolResult`; handle `ApprovalPending` →
   `user_approval`. Dismiss back to ROAMING.
4. **One real station** end-to-end: **Sensors** (subscribe `SensorReading`, ambient
   wall tint from IAQ + activate → heatmap view) — proves a non-chat function maps in.
5. **Sub-agent**: on `SubAgentStarted`, spawn a child figure; activate → its chat.
6. **Navigation**: walk + teleport/snap-to + the always-on HUD with agentd connection
   status.

Deferred to later (named so they're not lost): full council ring choreography (§6),
Memory Gate motes (§8.2), Mesh concourse + portal travel (§8.3), `world_snapshot`
agent-vision (§7, needs the MCP plugin), VR (§5), agent-driven movement (§9).

---

## 11. Dependencies on other design dimensions (assumptions stated)

| # | Assumption this doc makes | Owning dimension |
|---|---------------------------|------------------|
| A1 | A 3D scene with avatars, billboards, and live-texture/overlay panels runs at interactive FPS on Standard/Pro tier; rendering/perf budget is set elsewhere. | Rendering / performance |
| A2 | An existing `ui-slint` view (chat, sensors, terminal, council, settings) can be hosted on a station's focus surface (live-texture import or 2D overlay). | UI integration / Slint↔Bevy compositing |
| A3 | The `world_snapshot` MCP plugin + tool schema, and whether snapshot bytes return inline vs. via a cerebro vision-memory ref, are defined elsewhere; this doc only fixes the client's render-and-reply behavior over the real `ToolRequested`/`ToolResult` round-trip. | Agent-vision / protocol & MCP plugin |
| A4 | Binding cerebro activity (episode/recall) and council convergence to *animation* is feasible; exact event/animation curves are a viz-design call. | Data-viz / animation |
| A5 | Agent-initiated movement (`world_move`/`world_face`) is a clean post-v1 extension and does not alter the four-state loop. | World-state plugin / agent embodiment |
| A6 | The multi-client session-filter discipline (filter inbound on injected `session`; gateway has no server-side filter) is honored by the client's event dispatch — this is a *hard* protocol fact from architecture.md, not a soft assumption. | (none — protocol fact; load-bearing) |

---

## 12. Anti-patterns this design explicitly rejects

- **Don't rebuild agentd's UIs in 3D.** The space *routes to* existing Slint views;
  it does not reimplement chat-in-floating-cubes. (Interface first.)
- **Don't make distance cost time on a single node.** Teleport is first-class;
  walking is for reading the room, not traversal tax.
- **Don't trap the user in a modal.** Every ACTIVE surface has a one-input exit, and
  alerts bleed through the blur.
- **Don't invent a wire protocol.** No placeholder `AgentMessage`/`UiEvent` (the
  seed's `ai_protocol.rs` is explicitly a placeholder); the client speaks agentd's
  real `Event` enum and intents (`user_prompt`/`user_approval`/`user_cancel`,
  `session_init`).
- **Don't fork agentd core for world powers.** New capabilities arrive as MCP
  plugins/tools through the documented extension surface only.
- **Don't assume `turn_started`.** It does not exist in the Rust daemon; busy is
  driven by `AgentText`, ended by `TurnComplete` (and a `user_cancel` requires the
  client to clear busy itself, since `cascade_cancel` emits no `TurnComplete`).
