# SDK 01 — Core types & the WebSocket/event protocol

> **Surface:** `apexos-protocol` (`apexos-protocol/`, the extracted wire-type
> crate — `apexos-core` re-exports it) for the `Event`/`Intent` wire types, plus
> `apexos-core` (`agentd/crates/core/`) for the in-process event bus, plus the
> gateway WS read/write path that translates that bus to JSON over
> `ws://localhost:8787/ws`.
>
> **Extend this when** you need a new kind of message to flow through the system:
> a new agent/tool/sensor/mesh event, a new frontend intent, or a new field on an
> existing one. Everything else (tools, policy, UI views) is *downstream* of this
> surface — the wire contract defined here is the single coupling between the
> daemon and every client. Read this before adding any `Event` variant or writing
> a new frontend (browser, PWA, alternate UI).

This guide was ground-truthed against the source **as of a June 2026 snapshot**.
Where it contradicts CLAUDE.md / `docs/gotchas.md` / `docs/agentd-protocol.md`
(all maintained continuously), **those win and this guide is stale**, pending an
SDK refresh.

---

## Concepts

### One enum is the protocol

There is no separate `Intent` type. **`Event`** (the `Event` enum in
`apexos-protocol/src/lib.rs:235` — `agentd/crates/core/src/types.rs` was folded
into the extracted `apexos-protocol` crate, which `apexos-core` re-exports so
`apexos_core::Event` still resolves) is a single tagged enum that carries *both*
directions:

- **Inbound (frontend → daemon)** — `UserPrompt`, `UserApproval`, `UserCancel`.
  The doc-level term "Intent" maps to these three variants; the code has no
  `Intent` type.
- **Outbound (daemon → frontend)** — everything else: `AgentText`, `ToolRequested`,
  `ToolResult`, `ApprovalPending`, `TurnComplete`, `SensorReading`, council/mesh/
  vast/evolution/goal variants (`SensorAlert`, `MeshNodeStatus`, `MeshMessage`,
  `MeshMemoryShared`, `GoalStateChanged`, … have shipped since this guide's
  snapshot — the enum in `apexos-protocol/src/lib.rs:235` is the authoritative
  inventory), etc.

Serde config: `#[serde(tag = "type", rename_all = "snake_case")]`
(the container attribute on `enum Event`). So every frame on the wire is a JSON object with a `"type"`
discriminant in **snake_case** and the variant's fields flattened alongside it:

```json
{"type": "agent_text", "session": 42, "delta": "hello"}
```

### ID newtypes serialize as bare numbers

`SessionId(pub u64)`, `ActionId(pub u64)`, `EvolutionId(pub u64)`
(the ID newtypes in `apexos-protocol/src/lib.rs`; they also derive `Ord` for the
`no_std` consumer) are `#[derive(Serialize, Deserialize)]` tuple structs. Serde
serializes a single-field tuple struct **transparently** — as the inner number,
**not** as `{"0": 42}` and **not** as a string. So on the wire:

```json
{"type": "user_approval", "session": 42, "action": 5, "granted": true}
```

`session` and `action` are **bare numbers**. `PluginId(pub String)` likewise
serializes as a bare string.

### The bus: emit → run → broadcast

`core/src/bus.rs` is the in-process hub. `Bus::new(state)` returns a
triple:

| Returned | Type | Use |
|----------|------|-----|
| `Bus` | owns the inbox + state | `Bus::run` consumes it |
| `BusHandle` | `Clone`, `bus.emit(event).await` | every producer holds one |
| `broadcast::Sender<Event>` | `.subscribe()` for a `Receiver` | every consumer holds one |

Both channels are **capacity 1024** (the channel constructors in `Bus::new`).
`Bus::run` is the only mutator of canonical state:

```rust
while let Some(event) = self.inbox.recv().await {
    self.state.apply(&event);          // fold into SystemState (pure)
    let _ = self.outbound.send(event); // fan out to every subscriber
}
```

So the lifecycle of any event is: **a producer `emit`s it onto the mpsc inbox →
`Bus::run` applies it to `SystemState` then rebroadcasts → all subscribers see
it.** Subscribers include: every gateway WS write task (one per connected
client), the plugin supervisor, the agent router, the evolution applier, the
store log writer, and the scheduler/council handlers.

### SystemState::apply is pure and lossy-by-design

`SystemState` (`core/src/state.rs`) holds `sessions`, `tools`, `plugins`,
`pending_approvals`. `SystemState::apply(&mut self, event)` is **pure — no I/O,
no async** — and folds each event into that canonical state. Crucially, **most
event variants are deliberate no-ops in `apply`**: streaming
deltas (`AgentText`/`AgentThinking`), council, sensor, mesh, vast, and evolution
events change no canonical state — they exist purely to fan out to subscribers.
Only a handful mutate state:

| Event | `apply` effect (the matching arm in `SystemState::apply`) |
|-------|------------------------------|
| `UserPrompt` | create/get root session, push a `User` text message to its history |
| `ApprovalPending` | insert `call.id → session` into `pending_approvals` |
| `UserApproval` | remove `action` from `pending_approvals` |
| `PluginUp` | register each tool name → plugin, store plugin's tool list |
| `PluginDown` | drop that plugin's tools + entry |

**The event log (`apexos-store`) — not `SystemState` — is the authoritative audit
trail** for transient/evolution events (the no-op arms in `SystemState::apply`).

### ToolCall / ToolOutput / ToolSpec / ContentBlock

- **`ToolCall`** (`struct ToolCall`): `{ id: ActionId, tool: String, args: Value, needs_approval: bool }`.
  Nested under the `call` field of `ToolRequested` / `ApprovalPending`.
- **`ToolOutput`** (`struct ToolOutput`): `{ ok: bool, content: Value }`. Nested under
  `output` of `ToolResult`.
- **`ToolSpec`** (`struct ToolSpec`): `{ name, description, input_schema }` — a tool's
  advertised schema, carried in `PluginUp`.
- **`ContentBlock`** (`enum ContentBlock`) `#[serde(tag="type", rename_all="snake_case")]`:
  `text` / `thinking` (carries `signature` — must be replayed across tool
  round-trips or the Anthropic API rejects the continuation) /
  `tool_use` / `tool_result`. These make up `Message` history (`struct Message`),
  **not** the WS chat stream — chat streams as `agent_text` deltas.

### PolicyMode vs. PolicyRule — do not conflate

Two distinct kebab-case enums (`enum PolicyMode` / `enum PolicyRule` in `types.rs`):

- **`PolicyMode`** — the *global* mode: `suggest` (default) / `auto-edit` / `yolo`.
  Set via `POST /api/policy`.
- **`PolicyRule`** — the *per-tool* `[rules]` value in `policy.toml`:
  `allow` / `ask` / `workspace`. Referenced by
  `EvolutionProposal::UpdatePolicyRule`.

Writing a mode name into a rule slot (or vice versa) corrupts `policy.toml` on
reload.

---

## Add a new Event variant (end-to-end)

Adding to the wire protocol touches **four layers in order**. Skipping any one
produces the silent-drop failure mode (a frame that doesn't deserialize is
discarded with no error — see [Reference](#reference)).

### 1. Declare the variant — `apexos-protocol/src/lib.rs`

Add to the `Event` enum (in `apexos-protocol/src/lib.rs` — the extracted
wire-contract crate; `apexos-core` re-exports it). Snake_case is automatic via the
container attribute; just name the variant in PascalCase and the fields:

```rust
// in enum Event { ... }
/// Emitted by <producer> when <condition>. Consumed by <consumers>.
BatteryStatus { node_id: String, percent: u8, charging: bool },
```

Rules:
- Use the **ID newtypes** (`SessionId`, `ActionId`) for ids so they wire as bare
  numbers and stay type-safe.
- For a *frontend-bound* (inbound) variant, include `session: SessionId` — the
  gateway injects it (see step 4); your client omits it.
- Field names are the **wire contract**. Renaming a field is a breaking change to
  every client. There is no version negotiation.
- **The crate is `no_std`-capable** (an external bare-metal consumer, ApexOS-RV,
  pins it): a map-bearing field uses the crate's **`Map<K,V>` alias**
  (`HashMap` under `std` ⇄ `BTreeMap` under `no_std` — identical JSON shape),
  never `HashMap` directly; no bare `std::` paths (`core::`/`alloc::` instead);
  ID newtypes derive `Ord`. Run **both** build gates:
  `cargo test -p apexos-protocol` AND
  `cargo test -p apexos-protocol --no-default-features --features alloc`.

### 2. Handle it in `SystemState::apply` — `core/src/state.rs`

The `match` in `SystemState::apply` is **exhaustive** — the crate will not
compile until your variant has an arm. If it carries no canonical state, make it
an explicit no-op (this is the common case and is intentional):

```rust
Event::BatteryStatus { .. } => {}
```

If it *does* mutate canonical state (new session, new pending approval, plugin
registration), mutate `self.*` here and **only** here — `apply` is the single
writer of `SystemState`, and it must stay pure (no I/O, no async).

### 3. Wire every consumer that cares

Producers call `bus.emit(Event::BatteryStatus { .. }).await`. Consumers
`match` on the broadcast `Receiver`. The relevant consumers:

| Consumer | Symbol | Add an arm if… |
|----------|------|----------------|
| Agent router | `spawn_agent_router` (`agentd/src/main.rs`) | the event should drive/cancel a turn (it has a catch-all `Ok(_) => {}`, so it ignores unknown variants safely) |
| Plugin supervisor | `Supervisor::run` (`plugins/src/supervisor.rs`) | the event affects tool dispatch/approval |
| Evolution applier | `spawn_evolution_applier` (`agentd/src/main.rs`) | it's an `Evolution*` variant |
| Store writer | `store/src/lib.rs` | **automatic** — it logs *every* `Event` as JSONL; no change needed |
| Gateway WS write task | the broadcast-relay loop in `gateway/src/lib.rs` | **automatic** — it relays every broadcast `Event` (session-scoped per-socket via `event_session`; session-less events go to all sockets); no change needed |

Most new outbound events need **no consumer edits** — the store and gateway relay
everything. You only edit a consumer to make the daemon *act* on the event.

### 4. Speak it from the client / UI

**Outbound** (daemon → client): nothing in the daemon to do beyond emitting it;
the gateway already relays it. On the client, add a case to your inbound
dispatch keyed on `"type": "battery_status"`.

**Inbound** (client → daemon): the gateway read task
takes the raw frame, **injects `frame["session"] = session_id`**,
then `serde_json::from_value::<Event>(frame)`. So:
- the client **omits** `session` (the gateway sets it to the socket's session);
- the frame must otherwise deserialize cleanly into your variant or it is
  **silently dropped** (the `if let Ok(event)` has no `else`).

For the Slint UI specifically, the inbound dispatch + any new `VecModel` row type
live in `ui-slint/src/main.rs` and `ui-slint/src/ui/types.slint` (the Slint struct
must mirror the Rust model). See SDK 05 (UI) for that surface.

### 5. (If it's a frontend intent) confirm the router consumes it

A new *inbound* variant that should *do* something must be matched in
`spawn_agent_router` (its `loop { match rx.recv().await { ... } }`) or
the supervisor. The router's existing arms are the template: `UserPrompt`
spawns a turn; `UserCancel` cascades an abort;
`UserApproval` is consumed by the **supervisor** (`Supervisor::run`), not the
router. Without an arm, the event applies to state and broadcasts but nothing
acts on it.

---

## Worked example: a `node_status` heartbeat event

Goal: a mesh node periodically reports liveness; the daemon logs it (free, via
the store) and broadcasts it; clients show a green dot. This is a pure
*outbound* event — no new intent, no state mutation — the minimal end-to-end case.

**1. `apexos-protocol/src/lib.rs`** — add to `enum Event` (after the mesh variants):

```rust
/// Emitted by the mesh heartbeat task ~every 30s per known peer.
/// Consumers: UI peer roster (green/red dot). No canonical state.
NodeStatus { node_id: String, rss_mb: u32, load1: f32, healthy: bool },
```

**2. `core/src/state.rs`** — add the no-op arm (next to the other mesh no-ops in
`SystemState::apply`):

```rust
Event::NodeStatus { .. } => {}
```

The crate now compiles (the `match` is exhaustive again).

**3. Producer** — wherever the heartbeat runs (e.g. a task in `main.rs` or
`gateway/src/mesh.rs`), holding a `BusHandle`:

```rust
bus.emit(Event::NodeStatus {
    node_id: node_id.clone(),
    rss_mb:  read_rss_mb(),
    load1:   read_loadavg(),
    healthy: true,
}).await;
```

No consumer edits: the store writer already persists it (`store/src/lib.rs`), and
every gateway WS write task already relays it (the broadcast-relay loop in `gateway/src/lib.rs`).

**4. Client** — it arrives on the socket as:

```json
{"type": "node_status", "node_id": "pi-zero-3", "rss_mb": 41, "load1": 0.7, "healthy": true}
```

Add an inbound case for `node_status` (it carries `node_id`, not `session`, so
the gateway's `event_session` scoping treats it as a global and delivers it to
every client). Done — the full path works
without touching the agent loop, the supervisor, or the policy engine.

> **If this were an inbound intent instead** (say `set_node_label`), you would
> also (a) include `session: SessionId` in the variant, (b) have the client send
> `{"type":"set_node_label", "label":"kitchen"}` *without* `session`, and (c) add
> a match arm in `spawn_agent_router` to act on it.

---

## How a client speaks the protocol correctly

This is the exact handshake — and it **differs from CLAUDE.md** (see Gotchas).

1. **Connect** to `ws://HOST:8787/ws`. If `AGENTD_TOKEN` is set on the daemon,
   you MUST authenticate: WebSocket upgrades can't carry an `Authorization`
   header from a browser, so pass `?token=<TOKEN>` (`gateway/src/lib.rs:106`).
   REST `/api/*` accepts either `Authorization: Bearer <TOKEN>` or `?token=`.

2. **The server speaks first.** On connect, the gateway assigns a session id and
   **immediately pushes** a `session_init` frame *before* you send anything
   (the connect handler in `gateway/src/lib.rs`):
   ```json
   {"type": "session_init", "session_id": 42, "history": []}
   ```
   This is the daemon→client `session_init` (it carries `history`). You do **not**
   send `session_init` to start — the CLAUDE.md "send `{type:session_init}` on
   connect" instruction is wrong for the Rust daemon.

3. **To resume a prior session**, send a `hello` frame with `resume_session`
   (the `hello` handling in `gateway/src/lib.rs`):
   ```json
   {"type": "hello", "resume_session": 42}
   ```
   If session 42 exists in the in-memory `histories` map, the server rebinds this
   socket to it and replies with a fresh `session_init` carrying the full
   `history`. If it doesn't exist, you keep your freshly-assigned id (no error).

   `hello` has grown since the original snapshot (all optional, all handled in
   the gateway read task):
   - `{"type":"hello","new":true}` mints a **new** session id with empty history
     on the live socket (the ui-slint "+ New" button).
   - `agent_id` binds the session to an agent identity (Cerebro space + soul);
     session-token connections are gated through `gate_agent_bind` — a human may
     only bind an agent they own.
   - `persona` carries the active persona voice; there is also a standalone
     `{"type":"set_persona","persona":"…"}` frame for a live switch (gateway-consumed,
     never re-emitted as an `Event`).

4. **Send a prompt:**
   ```json
   {"type": "user_prompt", "text": "hello"}
   ```
   Omit `session` — the gateway injects it. It may also carry `images`
   (`UserPrompt.images`, `apexos-protocol/src/lib.rs:237`): each `{path}` or
   `{b64, media_type}` ref is shimmed through `vision::prepare` (decode →
   downscale ≤`VISION_MAX_EDGE` → re-encode) before the event.

5. **Approve/reject a tool** when you receive `approval_pending`. Use the numeric
   `call.id` as `action`, and `granted` (boolean) — **not** `call_id`/`approved`:
   ```json
   {"type": "user_approval", "action": 5, "granted": true}
   ```

6. **Cancel a turn:**
   ```json
   {"type": "user_cancel"}
   ```
   The router runs `cascade_cancel` (in `spawn_agent_router`) which aborts the turn (and
   children) but emits **no** `TurnComplete`. Your client must clear its own busy
   state and any pending tool cards.

7. **Outbound frames are scoped server-side — don't filter them yourself.** The
   gateway write task filters per-socket via `event_session`
   (`gateway/src/lib.rs:468`): a session-scoped event (the conversation stream —
   `agent_text`/`tool_requested`/`turn_complete`/`approval_pending`/…) reaches
   only the socket bound to that session; global/status events (sensors, council,
   mesh, vast, evolution) go to every client. So a client receives **only its own
   session's stream + globals** — clients don't (and shouldn't) re-filter.
   (This replaced the original broadcast-everything contract, under which clients
   had to drop foreign-session frames themselves.)

### Minimal client (pseudocode)

```js
const ws = new WebSocket(`ws://pi:8787/ws?token=${TOKEN}`);
let mySession = null;
ws.onmessage = ({data}) => {
  const ev = JSON.parse(data);
  if (ev.type === "session_init") { mySession = ev.session_id; renderHistory(ev.history); return; }
  // no session filter needed — the gateway's event_session scoping already
  // delivers only this socket's session stream + global events
  switch (ev.type) {
    case "agent_text":       appendDelta(ev.delta); break;          // also: set busy
    case "tool_requested":   pushToolCard(String(ev.call.id), ev.call); break;
    case "tool_result":      updateToolCard(String(ev.call), ev.output); break; // ev.call is bare id
    case "approval_pending": showApprove(ev.call.id); break;
    case "turn_complete":    clearBusy(); break;
    // ... node_status, sensor_reading, council_*, etc.
  }
};
ws.send(JSON.stringify({type: "user_prompt", text: "hello"}));   // no `session`
```

> **Busy state is driven by `agent_text`, not `turn_started`.** The Rust daemon
> **never emits `turn_started`** (it's a Python-agentd-only event). A tool-first
> turn that emits no leading text won't set busy until text arrives. Do not wait
> on `turn_started`.

---

## Policy / safety

**Adding an `Event` variant does not, by itself, grant any capability** — it adds
a message shape. Capability is gated downstream:

- **Tool calls** go through the `PolicyEngine`. The agent turn engine sets
  `ToolCall.needs_approval = false` unconditionally (in `run_turn`, `agent/src/turn.rs`); the
  **supervisor** is what decides, calling `policy.check(&call.tool, path)`
  (in `Supervisor::dispatch_tool`) and emitting `ApprovalPending` on `Decision::Ask`.
  So **`needs_approval` on the wire is currently always
  `false`** — clients must rely on receiving an `approval_pending` event, not on
  that flag. (If you make `needs_approval` meaningful, you must set it in the
  supervisor's `ToolRequested` handling, not the agent.)

- **Evolution events are the audited self-modification path.** A new
  `EvolutionProposal` variant (`enum EvolutionProposal`) is the *correct* way to let the
  agent change config — it routes through the policy engine under the
  `evolution.*` namespace (default `suggest` → asks the user), the applier
  snapshots an undo state and journals it into a Cerebro episode, and
  `rollback_evolution` replays the snapshot. **Do not** bypass this by inventing
  an event that directly writes config; that loses the audit trail and the undo.

- **The systemd sandbox is the real boundary**, not the event layer. `agentd`
  runs as a jailed `agentd` user (`ProtectSystem=strict`, `ReadWritePaths` limited
  to `/var/lib/agentd /etc/agentd`). A new event can never escalate beyond that
  jail. `/sensor-bridge` is the one ungated route (own `SENSOR_BRIDGE_TOKEN`); a
  new ingest endpoint accepting untrusted `Event`s should follow the same
  pattern, not widen `/ws`.

- **Silent-drop is a safety feature *and* a footgun.** A malformed inbound frame
  is dropped, not errored (the no-`else` `if let Ok(event)` in the gateway read task) — a malicious client can't crash the
  daemon with garbage, but you also get no feedback when a field name is wrong.
  Test new inbound variants with a real round-trip, not by eyeballing.

**For agents self-extending at runtime:** you cannot add an `Event` variant at
runtime — it's a compile-time enum, requiring a code change, build, and
hot-swap (stop service → `cp` binary → start; "text file busy" if the binary is
running). That work belongs in a human-reviewed (or at minimum audited) commit.
The runtime-mutable surface is `propose_evolution` (config) and tool registration
(`plugins.toml`), **not** the protocol enum. Treat a protocol change as a
code-truth change: commit it, push it, and update the CLAUDE.md / architecture
protocol tables in the same commit.

---

## Reference

### Inbound events (frontend → daemon) — client omits `session`

| `type` | Fields (client sends) | Effect |
|--------|-----------------------|--------|
| `hello` | `resume_session: u64?`, `new: bool?`, `agent_id: string?`, `persona: string?` | resume (or mint, with `new:true`) a session; server replies `session_init` with history. `agent_id` = identity bind (gated for session-token humans); `persona` = voice |
| `set_persona` | `persona: string` | live persona switch — gateway-consumed, never re-emitted as an `Event` |
| `user_prompt` | `text: string`, `images: [{path}\|{b64, media_type}]?` | start/continue a turn; images are shimmed via `vision::prepare` |
| `user_approval` | `action: u64` (= `ToolCall.id`), `granted: bool` | resolve a pending approval |
| `user_cancel` | — | cascade-abort the turn (no `turn_complete` follows) |

### Outbound events (daemon → frontend), selected — scoped per-socket by `event_session`

| `type` | Key fields | Notes |
|--------|-----------|-------|
| `session_init` | `session_id: u64`, `history: Message[]` | server-pushed on connect / resume |
| `agent_text` | `session`, `delta: string` | append; also the busy signal |
| `agent_thinking` | `session`, `delta: string` | extended-thinking stream |
| `tool_requested` | `session`, `call: ToolCall` | `call.id` is a number; stringify for row key |
| `tool_result` | `session`, `call: u64`, `output: ToolOutput` | `call` is a **bare id**, not a `ToolCall` |
| `approval_pending` | `session`, `call: ToolCall` | show approve/reject |
| `turn_complete` | `session` | clear busy; TTS if enabled |
| `plugin_up` / `plugin_down` | `plugin: string`, `tools: ToolSpec[]` / `reason` | tool registry change |
| `sub_agent_started` | `parent`, `child`, `prompt` | open a child window |
| `sensor_reading` | `node_id: string`, `reading: SensorReading`, `timestamp: u64` | from `/sensor-bridge` |
| `wake_triggered` | — | flash wake indicator |
| `agent_message` / `agent_message_ack` | A2A routing | router re-injects as `user_prompt` |
| `council_*` | see `apexos-protocol/src/lib.rs:305–312` | multi-agent deliberation stream |
| `error` | `session: u64?`, `message` | `session` is optional |
| `vast_*` | instance/tunnel lifecycle | `apexos-protocol/src/lib.rs:318–329` |
| `peer_seen` / `peer_registered` / `peer_lost` | mesh discovery | `apexos-protocol/src/lib.rs:347–351` |
| `evolution_proposed` / `evolution_applied` / `evolution_rolled_back` | self-modification audit | `apexos-protocol/src/lib.rs:363–376` |

> `turn_started` is **not** emitted by the Rust daemon. Do not depend on it.

> This table is a mid-2026 selection. Variants shipped since — `sensor_alert`,
> `mesh_node_status`, `mesh_message`, `mesh_memory_shared`, `goal_state_changed`,
> among others — are not listed; the full enum at `apexos-protocol/src/lib.rs:235`
> is the authoritative inventory.

### Nested struct shapes (`apexos-protocol/src/lib.rs`)

| Struct | JSON | Source |
|--------|------|--------|
| `ToolCall` | `{ "id": <num>, "tool": <str>, "args": {…}, "needs_approval": <bool> }` | `:406` |
| `ToolOutput` | `{ "ok": <bool>, "content": <any> }` | `:415` |
| `ToolSpec` | `{ "name", "description", "input_schema": {…} }` | `:421` |
| `ContentBlock` | tagged `type`: `text`/`thinking`(`+signature`)/`tool_use`/`tool_result` | `:464` |
| `SensorReading` | tagged `kind`: `temperature`/`humidity`/`pressure`/`motion`/`distance`/`gpio_level`/`air_quality`/`thermal_frame` | `:192` |
| `EvolutionProposal` | tagged `kind`: `register_mcp_server`/`unregister_mcp_server`/`update_policy_rule`/`update_system_prompt`/`hot_reload_subsystem`/`request_hardware` | `:141` |

### Enums

| Type | Wire values | Source |
|------|-------------|--------|
| `PolicyMode` (global) | `suggest` \| `auto-edit` \| `yolo` (kebab-case) | `:82` |
| `PolicyRule` (per-tool) | `allow` \| `ask` \| `workspace` (kebab-case) | `:98` |
| `Subsystem` | `plugins` \| `policy` \| `agent` \| `gateway` (snake_case) | `:130` |
| `Message` role | `user` \| `assistant` (tagged `role`) | `:457` |

### Bus / state facts

| Fact | Value | Source |
|------|-------|--------|
| Inbox (mpsc) capacity | 1024 | `bus.rs:25` |
| Broadcast capacity | 1024 | `bus.rs:26` |
| Emit | `BusHandle::emit(Event).await` | `bus.rs` |
| Subscribe | `broadcast::Sender::subscribe()` | `Bus::new`, `bus.rs` |
| State mutation | only in `SystemState::apply` (pure, sync) | `state.rs` |
| Lagged subscriber | `RecvError::Lagged(n)` → consumer skips (`continue`) | e.g. the WS write loop (`gateway/src/lib.rs`), the turn's bus reader (`agent/src/turn.rs`) |
| Tool-result wait timeout | `AGENTD_TOOL_RESULT_TIMEOUT_SECS`, default **1800s** | `run_turn`, `agent/src/turn.rs` |

### Gotchas vs. CLAUDE.md

The CLAUDE.md and (older) protocol tables contain stale claims. Trust the source:

| CLAUDE.md says | Reality (source) |
|----------------|------------------|
| client sends `{type:"session_init"}` on connect | server **pushes** `session_init` first; client sends nothing to start (the connect handler in `gateway/src/lib.rs`) |
| resume via `{type:"session_init", session_id}` | resume via `{type:"hello", resume_session:<id>}` (the `hello` handling in `gateway/src/lib.rs`) |
| `turn_started` clears buffer / sets busy | Rust daemon **never emits** `turn_started`; busy is driven by `agent_text` |
| `tool_result` has `call: <id>` | correct — `call` is a **bare number** here, *unlike* `tool_requested` where `call` is a full `ToolCall` object (`apexos-protocol/src/lib.rs:248` vs `:244`) |

### The four-layer checklist for a new variant

1. `apexos-protocol/src/lib.rs` — add the `Event` variant (snake_case auto;
   respect the `no_std` rules — `Map<K,V>` alias, no bare `std::` — and run both
   protocol test gates).
2. `core/src/state.rs` — add an `apply` arm (no-op unless it holds canonical state).
3. consumers — add a match arm **only** where the daemon must *act* (router /
   supervisor / evolution applier); store + gateway relay automatically.
4. client — inbound dispatch case; for an intent, also send it without `session`
   and add a router arm.

Miss step 1/2 → won't compile. Miss step 4 inbound → silently dropped, no error.
