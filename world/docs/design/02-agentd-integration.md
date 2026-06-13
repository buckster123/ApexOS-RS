# 02 — agentd Protocol Integration

> **Dimension:** agentd protocol integration (the feasibility core).
> **Verdict (TL;DR):** **FEASIBLE with ZERO agentd core changes for the entire
> interaction surface** (chat, tools, approvals, sensors, sessions, sub-agents,
> council). The world client is just another `/ws` client, exactly like
> `ui-slint` and the browser. The **only** thing that needs new agentd code is
> *agent-vision* (an avatar "seeing" the 3D world), and that plugs in as a new
> **stdio MCP plugin** through the documented `plugins.toml` extension surface —
> **never** a core fork.

This is the load-bearing doc. Everything here is checked against the real wire
types in `agentd/crates/core/src/types.rs` and the real gateway behaviour in
`agentd/crates/gateway/src/lib.rs`. Where this design leans on another design
dimension (world model, rendering, MCP-plugin internals), the assumption is named
inline as **[ASSUMES: …]**.

---

## 0. Ground truth (read before trusting anything below)

| Source | What it pins down |
|--------|-------------------|
| `agentd/crates/core/src/types.rs` | The `Event` enum (the wire protocol), `ToolCall`, `ToolOutput`, `ToolSpec`, `SessionId`/`ActionId` newtypes |
| `agentd/crates/gateway/src/lib.rs` | `handle_socket` — session assignment, `session_init`/`hello`, the **broadcast-to-all-sockets** relay, the `frame["session"] = session_id` injection |
| `docs/architecture.md` | Supervisor spawns **stdio MCP plugins** from `plugins.toml`; `PluginUp{tools}` announces tool schemas; virtual tools intercepted in `supervisor.rs` |

Two facts dominate this entire design, both verified in `handle_socket`:

1. **One socket is bound to exactly one `session_id` at a time.** The gateway
   assigns a fresh id on connect (`next_session_id.fetch_add`) and *injects it*
   into every inbound frame (`frame["session"] = json!(session_id)`), so the
   client **omits** `session` on outbound intents. A `hello {resume_session}`
   frame re-points the socket at an existing session.

2. **The broadcast is global.** The write task does `rx.recv()` on
   `state.bcast` (a `broadcast::Sender<Event>`, capacity 1024) and forwards
   **every** event for **every** session to **every** socket. There is no
   server-side per-session filtering on the outbound path.

> **Wire encoding reminders** (from `types.rs`, confirmed against the gateway):
> - `Event` is `#[serde(tag = "type", rename_all = "snake_case")]` → discriminant
>   field is `"type"`, values are snake_case (`agent_text`, `tool_requested`, …).
> - `SessionId(u64)` / `ActionId(u64)` serialize as **bare numbers**, not strings.
> - On `ToolRequested`/`ApprovalPending` the tool data nests under `call`
>   (`call.id`, `call.tool`, `call.args`, `call.needs_approval`). On
>   `ToolResult` the field `call` is the **bare `ActionId` number**, and the
>   result body is `output: {ok, content}`.
> - A frame that fails to deserialize into `Event` is **silently dropped** by the
>   gateway read task (`if let Ok(event) = … bus.emit`). Wrong field names = no
>   error, just nothing happens. The world client must mirror this discipline and
>   log-and-drop unknown inbound frames rather than crash.

---

## 1. The world client is a `/ws` peer

```
┌──────────────────────────── agentd (UNCHANGED) ───────────────────────────┐
│  Bus  ─emit→  broadcast::Sender<Event> (cap 1024) ─┬─→ socket: ui-slint     │
│   ▲                                                ├─→ socket: browser/PWA   │
│   │ bus.emit(Event)                                └─→ socket: apexos-world  │ ← us
│   │                                                                          │
│  agent turn engine · plugin supervisor · policy engine · scheduler          │
└──────────────────────────────────────────────────────────────────────────┘
            ▲                                   │
   outbound │ Intents (UserPrompt/…)            │ inbound: ALL sessions' Events
   (session omitted; gateway injects)           ▼
┌──────────────────────────── apexos-world client ──────────────────────────┐
│  ws layer (tokio-tungstenite)                                              │
│    • N sockets, one per "live surface" (see §3 session model)              │
│    • parse → apexos_core::Event   • filter on `session`   • route to world │
│  world model (Bevy ECS)  ── agent avatars · stations · ambient telemetry   │
│  Slint overlays          ── activated-surface UI (chat / dashboard / …)    │
└────────────────────────────────────────────────────────────────────────────┘
```

The client speaks **the real `apexos_core::Event` enum** — not the skill's
placeholder `AgentMessage`/`UiEvent`. See §6 for the concrete migration off the
template.

---

## 2. Inbound events → world updates

Every row is a real `Event` variant from `types.rs`. "Surface" = the per-agent /
per-station Slint UI that fills the view when you activate that world element.
"Ambient" = something rendered into the 3D space itself (orbs, glow, motes).

| `Event` variant | Fields used | World effect |
|-----------------|-------------|--------------|
| `agent_text` | `session`, `delta` | Append delta to the **chat surface** of the avatar/station bound to `session`. While streaming, drive that avatar's "speaking" animation/glow. |
| `agent_thinking` | `session`, `delta` | Optional: a dim "thought" stream on the surface, or a subtle thinking shimmer on the avatar. Safe to ignore on Nano-equivalent low detail. |
| `turn_started` | *(NB: see §2.1)* | — |
| `turn_complete` | `session` | Clear that avatar's busy/speaking state; if voice enabled, hand the accumulated text to TTS (`POST /api/speak`). |
| `tool_requested` | `session`, `call:{id,tool,args,needs_approval}` | Spawn an in-world **tool affordance** near the avatar (a floating card / construct labelled `call.tool`), status = running. Key it by `call.id.to_string()`. |
| `tool_result` | `session`, `call` *(bare id)*, `output:{ok,content}` | Find the affordance whose key == `call`; `ok` → settle to "done" (green), `!ok` → "error" (red). Render `content` into the affordance / station detail panel. |
| `approval_pending` | `session`, `call:{id,tool,args}` | Raise an **approve/reject gesture target** in-world (e.g. two glowing pads, or a gate the user/agent must touch). Holds the turn until answered. |
| `sensor_reading` | `node_id`, `reading`, `timestamp` | **Ambient/spatial telemetry** — not session-scoped. Drive a Sensor Station: `AirQuality.iaq` → ambient color/fog of a zone; `ThermalFrame{min,max,mean}` → a heat readout; `Motion` → a presence ping. (See §2.2.) |
| `wake_triggered` | *(no session)* | Flash a world-wide "listening" indicator; the active avatar perks up. Then follow the existing record→submit flow. |
| `sub_agent_started` | `parent`, `child`, `prompt` | **Spawn a new child avatar** in the world next to its parent, bound to session `child`. This is how the world visualizes the agent tree. |
| `spawn_agent` | `parent`, `call_id`, `prompt`, `system` | Internal routing signal (supervisor→router). Client may ignore; `sub_agent_started` is the UI-facing one (its doc-comment says "so the UI can open a new agent window"). |
| `agent_message` / `agent_message_ack` | `from`, `to`, `body`, `msg_id` | Draw a **message arc** between the two agents' avatars (A2A). Ack closes the arc. |
| `plugin_up` | `plugin`, `tools:[ToolSpec]` | A tool plugin came online → optionally materialize a **Station** representing that plugin's capabilities; cache `tools` schemas for affordance labels/icons. This is also how the world discovers the **agent-vision tool** (§5). |
| `plugin_down` | `plugin`, `reason` | Dim/teardown that plugin's Station. |
| `error` | `session?`, `message` | If `session` present → error toast on that surface; else a world-level error banner. |
| `council_*` (7 variants) | `council_id`, `round`, `agent_id`, … | A **Council View** station: a ring of avatars (one per `CouncilAgentDef`), `council_agent_delta` streams into each, `council_round_done.convergence` drives a "consensus meter", `council_complete.synthesis` fills the center. Pure rendering; zero protocol changes. |
| `evolution_proposed` / `evolution_applied` / `evolution_rolled_back` | `id`, `proposal`, … | A **self-evolution log** affordance (the agent rewriting its own soul/policy). Render as world events / a ledger station. |
| `peer_seen` / `peer_registered` / `peer_lost` | `node_id`, `ip`, `ws_url`, `role` | **Mesh portals** — other agentd nodes appear as gateways/doorways in the world. (See §7 multi-node.) |
| `vast_*` (4 variants) | `instance_id`, … | Optional "summoning a GPU" effect for a Pro/Titan inference node spinning up. Cosmetic. |

### 2.1 `turn_started` does not exist as a wire event

The roadmap/CLAUDE.md table lists a `turn_started` event, but **it is not a
variant of the `Event` enum** in `types.rs`. The real "agent is now busy" signal
is the **first `agent_text` / `tool_requested` after a `UserPrompt`** for that
session, and the turn ends with `turn_complete`. The world client sets a
per-session `busy` flag when it sends `UserPrompt` (optimistic) and clears it on
`turn_complete` (or on `user_cancel`, see §4.1). **Do not** wait for a
`turn_started` frame — it will never arrive.

### 2.2 `sensor_reading` is node-scoped, not session-scoped

`SensorReading{node_id, reading, timestamp}` carries **no `session`**. It is
broadcast to every socket regardless of which session the socket is bound to.
This is *correct* for the world: sensor telemetry is ambient world-state, not
conversation. Route it to Sensor Stations / zone ambience by `node_id`, never
attempt to filter it by session. `reading` is the `SensorReading` enum
(`#[serde(tag="kind")]`): match on `kind` ∈ {`temperature`, `humidity`,
`pressure`, `motion`, `distance`, `gpio_level`, `air_quality`, `thermal_frame`}.

---

## 3. The session model (the decisive design choice)

**Question:** does each agent-avatar / station map to its own agentd
`session_id`, and how does the client cope with the global broadcast?

**Answer, in two parts:**

### 3.1 What a session *is* here

A `session_id` is one conversation thread / agent context (`AgentContext` in
`types.rs`: `id`, optional `parent`, `history`). Root sessions stream to a
frontend; child sessions (`parent = Some`) are sub-agents. So the natural
mapping is:

> **One conversational agent-avatar ⇔ one root `session_id`.**
> **One sub-agent avatar ⇔ one child `session_id`** (announced by `sub_agent_started`).

Non-conversational world elements (Sensor Station, Mesh Portal, Evolution Ledger)
are **not** sessions — they render broadcast events that carry no session (sensor,
peer, vast) or aggregate across sessions (evolution). They need no session of
their own.

### 3.2 Coping with the global broadcast — two patterns

Because the broadcast is global (every socket sees every session's events), the
client must decide how to *acquire* sessions and how to *filter*. There are two
viable patterns; **this design recommends the hybrid (Pattern C).**

**Pattern A — one socket per conversational avatar (clean binding).**
Open a fresh `/ws` per avatar the user/agent wants to talk to. Each socket:
- On connect, receives `session_init {session_id, history}` → that becomes the
  avatar's session, and `history` replays its past messages (free session
  resume / scrollback).
- Sends `UserPrompt` with `session` omitted — the gateway injects this socket's
  bound id, so prompts land in the right conversation **automatically**.
- **Still must filter inbound**, because the broadcast carries other sessions
  too. Filter rule: *accept an event only if its `session` field equals this
  socket's bound id* (events with no `session` are handled by the ambient router,
  §3.3).

Pros: outbound routing is automatic (no need to track ids on send); session
resume is trivial. Cons: N sockets = N copies of the global broadcast arriving N
times — wasteful at high avatar counts; every socket still filters anyway.

**Pattern B — single socket, filter everything.**
Open **one** `/ws`. It gets one bound session (use it for the "primary" avatar or
a console). For *every other* session:
- **Inbound:** filter the single broadcast stream by `event.session` and fan out
  to the matching avatar. (You were filtering anyway under Pattern A.)
- **Outbound:** you cannot use the socket's auto-injected session for a *different*
  session, because the gateway always injects *this socket's* bound id. To send a
  `UserPrompt` to session 7 from a socket bound to session 3, use the REST escape
  hatch instead: **`POST /api/sessions/7/message {message}`** — verified in
  `session_message_handler`, it emits `UserPrompt{session: 7}` on the bus exactly
  like a same-socket prompt would. To *start* a new conversation, briefly open a
  second socket to mint a fresh `session_id`, or send `hello{resume_session}` to
  hop the socket.

Pros: one broadcast subscription; clean fan-out; cheap at scale. Cons: outbound
to non-bound sessions goes via REST, and `user_approval`/`user_cancel` are
**WS-only** intents (no REST equivalent exists — see §4) so a single socket can
only approve/cancel its own bound session.

**Pattern C — RECOMMENDED hybrid.**
Open **one socket per *active/foregrounded* conversational session**, lazily.

- Idle avatars hold **no socket**; they render purely from the *console socket's*
  filtered broadcast (so you still see their `agent_text` ambiently).
- **Activating** an avatar (walking up + activating its surface) opens a
  dedicated socket for that session and (if resuming a known id) sends
  `hello{resume_session: id}` to bind + replay history. Now `UserPrompt`,
  `UserApproval`, `UserCancel` all route automatically and correctly to that
  session over its own socket.
- One always-on **"console" socket** stays subscribed to catch ambient
  cross-session events (other avatars talking, sub-agents spawning, sensors,
  council, mesh) and to mint new sessions on demand.
- Deactivating an avatar (walking away) can close its socket after a grace period.

This bounds socket count to "number of surfaces you're actively interacting with
+ 1", keeps outbound routing automatic for the foregrounded agent, and uses the
filtered console socket for everything ambient.

> **[ASSUMES: world-model dimension]** that "activate an avatar" is a discrete,
> rare-ish interaction (you're not activating 50 avatars/second), so lazy
> socket open/close is acceptable. If the world needs *simultaneous live input*
> to many sessions at once, fall back to Pattern B + `/api/sessions/:id/message`.

### 3.3 The inbound router (concrete)

Every inbound frame (on any socket) runs through one router:

```
fn route(ev: Event, this_socket_session: SessionId) {
    match ev {
        // session-scoped → dispatch to the avatar/station bound to ev.session
        AgentText{session,..} | AgentThinking{session,..}
        | ToolRequested{session,..} | TurnComplete{session}
        | ToolResult{session,..} | ApprovalPending{session,..}
        | Error{session: Some(session), ..}
            => world.surface_for(session).apply(ev),

        // tree-shaping
        SubAgentStarted{parent, child, ..} => world.spawn_child_avatar(parent, child),
        AgentMessage{from, to, ..} | AgentMessageAck{..} => world.draw_a2a_arc(ev),

        // ambient / node-scoped → no session filter
        SensorReading{node_id,..}      => world.sensor_station(node_id).apply(ev),
        WakeTriggered                  => world.flash_listening(),
        PluginUp{..} | PluginDown{..}  => world.update_station(ev),
        Council*{council_id,..}        => world.council_view(council_id).apply(ev),
        Peer*{..} | Vast*{..}          => world.mesh_or_inference(ev),
        Evolution*{..}                 => world.evolution_ledger(ev),
        Error{session: None, message}  => world.banner(message),

        SpawnAgent{..} => { /* internal; ignore, sub_agent_started is UI-facing */ }
    }
}
```

**De-duplication under Pattern A/C:** when multiple sockets are open, a
session-scoped event arrives on every socket. The router must dedupe: a
session-scoped event is applied **only by the socket whose bound id matches**
`ev.session`; on all other sockets it is dropped. Ambient events (no session) are
applied by exactly **one** designated socket (the console socket) to avoid
double-application. This is the single most important correctness rule in the
integration — get it wrong and chat lines double up.

---

## 4. Outbound: 3D interactions → real Intents

The "intent" events are just `Event` variants the gateway accepts inbound. **The
client omits `session`** — the gateway injects the socket's bound id. Three are
WS-only intents; one REST alternative exists for prompts.

| World interaction | Wire frame (session omitted) | Notes |
|-------------------|------------------------------|-------|
| Speak to an avatar (type/voice into its chat surface) | `{"type":"user_prompt","text":"…"}` | → `UserPrompt{session, text}`. To a *non-bound* session: `POST /api/sessions/:id/message {message}` instead. |
| In-world **approve** gesture (touch the green pad) | `{"type":"user_approval","action":<call.id>,"granted":true}` | → `UserApproval{session, action, granted}`. **`action` is the numeric `ToolCall.id`** from the `approval_pending` event — *not* a `call_id` string, *not* an `approved` bool field. |
| In-world **reject** gesture | `{"type":"user_approval","action":<call.id>,"granted":false}` | same shape, `granted:false`. |
| **Cancel** the current turn (e.g. an "abort" gesture) | `{"type":"user_cancel"}` | → `UserCancel{session}`. **See §4.1 — no `TurnComplete` follows.** |

### 4.1 `user_cancel` quirk (must be handled client-side)

`UserCancel` triggers `cascade_cancel` in the agent router, which aborts the turn
**but emits no `TurnComplete`** (documented in CLAUDE.md, consistent with the
router design). Therefore on sending `user_cancel` the world client must
**itself** clear that session's `busy` flag, settle/teardown any in-flight tool
affordances (the `running` ones from `tool_requested` that will never get a
`tool_result`), and drop any open approval gesture. Do not wait for a server
frame to confirm the cancel.

### 4.2 What is NOT an intent

`user_approval` and `user_cancel` have **no REST equivalent** — they exist only as
WS frames bound to a socket's session. Consequence: **to approve or cancel a
given session you must hold a socket bound to that session** (Pattern A/C). Under
pure Pattern B (single socket), you can only approve/cancel the socket's own bound
session. This is the concrete reason the hybrid (Pattern C) opens a real socket
on avatar activation rather than driving everything via REST.

---

## 5. Agent-vision — the ONLY piece that needs new agentd code

The felt feature: an **agent can "see" through its avatar's camera**. The agent
asks for a snapshot of what its avatar currently faces in the 3D world, and gets
back an image to reason over. agentd has **no** world-state and **no** way to
render the 3D scene — so this capability cannot come from the core. It plugs in
the documented, fork-free way: **a new stdio MCP plugin.**

### 5.1 How tools plug into agentd (verified surface)

From `architecture.md` + `supervisor.rs`: agentd's supervisor spawns **stdio MCP
child processes** declared in `plugins.toml` (newline-delimited JSON-RPC, like
`apexos-tools` and `cerebro-mcp`). On startup each plugin announces its tools →
gateway emits `PluginUp{plugin, tools:[ToolSpec]}`. When the agent calls a tool,
the supervisor runs it through the `PolicyEngine` then dispatches → the plugin
returns a result → `ToolResult{session, call, output}` on the bus. **No core
edit; just a new row in `plugins.toml` and a new binary.** This is the same
extension surface every existing tool uses.

### 5.2 The `apex-world-mcp` plugin

A new crate, e.g. `world/crates/apex-world-mcp`, registered in `plugins.toml`:

```toml
[[plugins]]
name    = "apex-world"
command = "/usr/local/bin/apex-world-mcp"
# env as needed (e.g. WORLD_BRIDGE_ADDR)
```

Tools it exposes (each a `ToolSpec{name, description, input_schema}`):

| Tool | Args | Returns (`ToolOutput.content`) | Purpose |
|------|------|-------------------------------|---------|
| `world_snapshot` | `{ session?, fov?, width?, height? }` | `{ image_b64, format:"jpeg", camera:{pos,yaw,pitch}, t }` | Render the calling avatar's camera view → JPEG. **This is the agent-vision loop.** |
| `world_look_at` | `{ target_id }` | `{ ok }` | Aim the avatar's camera at a world element before snapshotting. |
| `world_describe` | `{ radius? }` | `{ elements:[{id,kind,pos,label}] }` | Cheap structured "what's around me" (no render) — for Nano-tier or when vision is disabled. |
| `world_move` | `{ to:{x,y,z} | element_id }` | `{ ok, pos }` | Let the agent navigate its avatar (embodiment). |

### 5.3 The render/transport problem (named, not hand-waved)

The MCP plugin is a **separate process** spawned by agentd; the actual 3D scene
lives in the **world client process** (Bevy). The plugin cannot render the scene
itself. So `world_snapshot` is a **proxy**: the plugin forwards the request to the
world client, which renders an offscreen frame and returns the image.

```
agent turn ─→ supervisor ─→ apex-world-mcp (stdio child)
                                  │  local IPC (e.g. ws://127.0.0.1:PORT,
                                  │  unix socket, or HTTP) — WORLD_BRIDGE_ADDR
                                  ▼
                          apexos-world client  (owns the Bevy scene)
                                  │  render offscreen target → JPEG
                                  ▼
                          image_b64 ──► back up the chain ──► ToolResult.content
```

- **Which avatar's camera?** The `session` that called the tool identifies the
  agent; the world client maps `session → avatar → camera`. **[ASSUMES:
  world-model dimension]** maintains a `session_id → avatar entity` map (it
  already must, to route `agent_text`).
- **Image return path.** MCP tool results are JSON (`ToolOutput.content:
  serde_json::Value`). A JPEG goes back as **base64** in `content.image_b64`.
  Binary WS frames are *not* part of the agentd `Event` protocol (the gateway
  read task only handles `Message::Text`), so do not attempt to stream the image
  over `/ws` — it travels inside the normal `tool_result` JSON.
- **Vision model.** The returned image is then fed to a vision-capable model.
  That is the **provider's** job (Anthropic/OAI vision via the turn engine) — the
  tool just returns the image; how the agent "sees" it is the existing
  multimodal-content path (`ContentBlock`), **[ASSUMES: provider/model dimension
  supports image content in tool results]**. If a tier's model is text-only,
  `world_describe` is the graceful fallback (Nano design rule).

### 5.4 Trust / loopback

The world↔plugin bridge is **localhost-only** (both run on the same node as
agentd in kiosk/desktop mode). It must bind loopback and, if exposed, carry a
shared token — mirroring agentd's own `AGENTD_TOKEN`/`SENSOR_BRIDGE_TOKEN`
discipline. **[ASSUMES: deployment dimension]** that the world client and agentd
co-reside (true for Standard/Pro kiosk + desktop tiers — the only tiers this
interface targets per CLAUDE.md).

---

## 6. Migrating off the skill's placeholder protocol

The seed skill's `ai_protocol.rs` defines a **placeholder** `AgentMessage` /
`UiEvent` with `#[serde(tag="type", content="payload")]`. **Delete it.** The real
protocol is `apexos_core::Event` with `#[serde(tag="type")]` (no `content`
wrapper — fields are flattened at top level). Mapping:

| Skill placeholder | Real agentd reality |
|-------------------|---------------------|
| `AgentMessage::StreamUpdate` | `Event::AgentText{session,delta}` |
| `AgentMessage::ToolResult{tool,result}` | `Event::ToolResult{session,call,output}` (id-keyed, not name-keyed) |
| `AgentMessage::EntityUpdate` | no such event — agent/avatar entities are **client-side**, seeded by `SubAgentStarted` + the session model, *not* pushed by agentd |
| `AgentMessage::ImageFrame{data:Vec<u8>}` (binary) | **does not exist** — agentd `/ws` is text-only JSON; agent-vision images ride inside `ToolResult.content.image_b64` (§5) |
| `AgentMessage::RequestRender` | inverted: the *agent* pulls a render via the `world_snapshot` **tool**, not a server push |
| `UiEvent::ToolCall` | the world client does **not** call tools directly; the **agent** calls tools. The client only emits `UserPrompt`/`UserApproval`/`UserCancel`. |
| `UiEvent::AgentSelected` | purely client-side (which avatar is foregrounded) → drives §3.2 socket open/close, **not** sent to agentd |

**Concrete dependency:** vendor or depend on `apexos_core` for the `Event` /
`ToolCall` / `ToolOutput` types so the world client deserializes the *real* enum
instead of string-matching JSON. CLAUDE.md lists this under Deferred ("optionally
vendor agentd's core crate for shared `Event` types") — for the world client it
should be a **hard dependency from day one** to avoid the silent-drop trap of
hand-rolled JSON. `world/crates/*` is a sibling workspace; add a path dep on
`../../agentd/crates/core` (read-only use; no modification of agentd).

> **[ASSUMES: workspace/build dimension]** `apexos_core` is a clean lib crate with
> no Pi-only/link-time deps that would burden the desktop-targeted world client
> (it is — `types.rs` is pure serde). If a path dep proves awkward when the world
> repo is later extracted, fall back to a vendored copy of `types.rs` kept in
> sync, but the path dep is strongly preferred while co-located.

---

## 7. Multi-node / mesh (no new protocol, just more sockets)

agentd is mesh-aware (`peers.toml`, `PeerRegistered{node_id, ws_url, role}`). The
world client can connect a `/ws` to **each** peer node's gateway and represent
each node as a region/portal in the world. Each peer connection is an independent
socket with its own session space (session ids are per-daemon, so namespace
avatars by `(node_id, session_id)` to avoid collisions across nodes). This is
purely additive: same `/ws` protocol, more endpoints. **[ASSUMES: world-model
dimension]** keys avatars by `(node, session)` rather than bare `session`.

---

## 8. Auth & connection lifecycle

- **Token.** All `/ws` and `/api/*` routes are gated by `AGENTD_TOKEN`
  (`require_token` middleware). For a WS upgrade the token goes as a **query
  param** `?token=<tok>` (browsers can't set headers on WS); REST may use either
  `Authorization: Bearer` or `?token=`. Loopback dev with token unset = no auth.
  The world client reads `AGENTD_WS` + an `AGENTD_TOKEN` env (same convention as
  `ui-slint`).
- **session_init handshake.** On connect the gateway *immediately* (before any
  broadcast) sends `session_init{session_id, history}` on a biased priority
  channel. The client must read this first to learn its bound id. To resume a
  known session, send `hello{resume_session: <id>}` and the gateway replies with
  a fresh `session_init` carrying that session's `history` (replay scrollback
  into the surface).
- **Reconnect.** On socket drop, reconnect and **re-`hello`** each session that
  had one (Pattern C). Sensor/ambient state is rebuilt from the next broadcasts;
  there is no replay for ambient events, only for per-session `history`.
- **Lag.** The write task drops to `continue` on `broadcast::error::RecvError::
  Lagged` (it does not close). A slow world client that can't drain 1024-deep
  broadcast fast enough will **silently miss events**. Keep the WS read loop
  cheap: parse + push onto an mpsc to the Bevy/Slint side, never block on render
  inside the read loop. **[ASSUMES: rendering dimension]** decouples WS ingest
  from frame rendering via channels (it must — standard Slint/tokio rule).

---

## 9. Feasibility verdict

| Capability | agentd change needed? | Mechanism |
|------------|----------------------|-----------|
| Talk to an avatar (chat) | **None** | `UserPrompt` ⇄ `AgentText`/`TurnComplete` |
| In-world tool affordances | **None** | `ToolRequested`/`ToolResult` (id-keyed) |
| In-world approve/reject gesture | **None** | `ApprovalPending` → `UserApproval{action,granted}` |
| Cancel a turn | **None** | `UserCancel` (+ client-side busy clear, §4.1) |
| Ambient sensor telemetry | **None** | `SensorReading` (node-scoped, no filter) |
| Sub-agent / agent-tree avatars | **None** | `SubAgentStarted`, A2A `AgentMessage` |
| Multi-session (many avatars) | **None** | session-per-avatar + global-broadcast filter (§3) |
| Council view, evolution ledger, mesh portals, vast effects | **None** | existing `council_*`/`evolution_*`/`peer_*`/`vast_*` events |
| Cross-session send from one socket | **None** | existing `POST /api/sessions/:id/message` |
| **Agent-vision (avatar camera → agent eyes)** | **NEW MCP PLUGIN** (no core fork) | `apex-world-mcp` in `plugins.toml`; `world_snapshot` returns `image_b64` via a localhost world↔plugin bridge; image reaches the model through the normal `ToolResult.content` path |

**Verdict: FEASIBLE.** The world client is a drop-in third `/ws` client; the
entire human/agent interaction surface works against **agentd as-is with zero
core changes**. The single genuinely new capability — embodied agent-vision —
is delivered through agentd's documented MCP-plugin extension point, exactly as
`apexos-tools` and `cerebro-mcp` already are, with the only real engineering
challenge being the localhost bridge that lets the out-of-process MCP plugin pull
a rendered frame from the in-process Bevy scene (§5.3). No fork. No protocol
extension on the wire. The two hard correctness rules to honour are
**filter-and-dedupe inbound on `session` (§3.3)** and **clear busy locally on
`user_cancel` (§4.1)**; both are pure client-side discipline.
