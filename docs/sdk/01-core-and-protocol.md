# SDK 01 — Core types & the WebSocket/event protocol

> **Surface:** `apexos-core` (`agentd/crates/core/`) — the `Event`/`Intent` wire
> types and the in-process event bus — plus the gateway WS read/write path that
> translates that bus to JSON over `ws://localhost:8787/ws`.
>
> **Extend this when** you need a new kind of message to flow through the system:
> a new agent/tool/sensor/mesh event, a new frontend intent, or a new field on an
> existing one. Everything else (tools, policy, UI views) is *downstream* of this
> surface — the wire contract defined here is the single coupling between the
> daemon and every client. Read this before adding any `Event` variant or writing
> a new frontend (browser, PWA, alternate UI).

This guide is ground-truthed against the source. **Where it contradicts the
CLAUDE.md protocol table, this file is correct and the table is stale** — see
[Gotchas vs. CLAUDE.md](#gotchas-vs-claudemd).

---

## Concepts

### One enum is the protocol

There is no separate `Intent` type. **`Event`** (`core/src/types.rs:162`) is a
single tagged enum that carries *both* directions:

- **Inbound (frontend → daemon)** — `UserPrompt`, `UserApproval`, `UserCancel`.
  The doc-level term "Intent" maps to these three variants; the code has no
  `Intent` type.
- **Outbound (daemon → frontend)** — everything else: `AgentText`, `ToolRequested`,
  `ToolResult`, `ApprovalPending`, `TurnComplete`, `SensorReading`, council/mesh/
  vast/evolution variants, etc.

Serde config: `#[serde(tag = "type", rename_all = "snake_case")]`
(`types.rs:163`). So every frame on the wire is a JSON object with a `"type"`
discriminant in **snake_case** and the variant's fields flattened alongside it:

```json
{"type": "agent_text", "session": 42, "delta": "hello"}
```

### ID newtypes serialize as bare numbers

`SessionId(pub u64)`, `ActionId(pub u64)`, `EvolutionId(pub u64)`
(`types.rs:8–23`) are `#[derive(Serialize, Deserialize)]` tuple structs. Serde
serializes a single-field tuple struct **transparently** — as the inner number,
**not** as `{"0": 42}` and **not** as a string. So on the wire:

```json
{"type": "user_approval", "session": 42, "action": 5, "granted": true}
```

`session` and `action` are **bare numbers**. `PluginId(pub String)` likewise
serializes as a bare string.

### The bus: emit → run → broadcast

`core/src/bus.rs` is the in-process hub. `Bus::new(state)` (`bus.rs:24`) returns a
triple:

| Returned | Type | Use |
|----------|------|-----|
| `Bus` | owns the inbox + state | `Bus::run` consumes it |
| `BusHandle` | `Clone`, `bus.emit(event).await` | every producer holds one |
| `broadcast::Sender<Event>` | `.subscribe()` for a `Receiver` | every consumer holds one |

Both channels are **capacity 1024** (`bus.rs:25–26`). `Bus::run` (`bus.rs:32`) is
the only mutator of canonical state:

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

`SystemState` (`core/src/state.rs:5`) holds `sessions`, `tools`, `plugins`,
`pending_approvals`. `apply(&mut self, event)` (`state.rs:18`) is **pure — no I/O,
no async** — and folds each event into that canonical state. Crucially, **most
event variants are deliberate no-ops in `apply`** (`state.rs:37–115`): streaming
deltas (`AgentText`/`AgentThinking`), council, sensor, mesh, vast, and evolution
events change no canonical state — they exist purely to fan out to subscribers.
Only a handful mutate state:

| Event | `apply` effect (`state.rs`) |
|-------|------------------------------|
| `UserPrompt` | create/get root session, push a `User` text message to its history (`:21`) |
| `ApprovalPending` | insert `call.id → session` into `pending_approvals` (`:42`) |
| `UserApproval` | remove `action` from `pending_approvals` (`:46`) |
| `PluginUp` | register each tool name → plugin, store plugin's tool list (`:65`) |
| `PluginDown` | drop that plugin's tools + entry (`:72`) |

**The event log (`apexos-store`) — not `SystemState` — is the authoritative audit
trail** for transient/evolution events (`state.rs:96`).

### ToolCall / ToolOutput / ToolSpec / ContentBlock

- **`ToolCall`** (`types.rs:272`): `{ id: ActionId, tool: String, args: Value, needs_approval: bool }`.
  Nested under the `call` field of `ToolRequested` / `ApprovalPending`.
- **`ToolOutput`** (`types.rs:281`): `{ ok: bool, content: Value }`. Nested under
  `output` of `ToolResult`.
- **`ToolSpec`** (`types.rs:287`): `{ name, description, input_schema }` — a tool's
  advertised schema, carried in `PluginUp`.
- **`ContentBlock`** (`types.rs:329`) `#[serde(tag="type", rename_all="snake_case")]`:
  `text` / `thinking` (carries `signature` — must be replayed across tool
  round-trips or the Anthropic API rejects the continuation, `types.rs:318`) /
  `tool_use` / `tool_result`. These make up `Message` history (`types.rs:322`),
  **not** the WS chat stream — chat streams as `agent_text` deltas.

### PolicyMode vs. PolicyRule — do not conflate

Two distinct kebab-case enums (`types.rs:27–73`):

- **`PolicyMode`** — the *global* mode: `suggest` (default) / `auto-edit` / `yolo`.
  Set via `POST /api/policy`.
- **`PolicyRule`** — the *per-tool* `[rules]` value in `policy.toml`:
  `allow` / `ask` / `workspace`. Referenced by
  `EvolutionProposal::UpdatePolicyRule`.

Writing a mode name into a rule slot (or vice versa) corrupts `policy.toml` on
reload (`types.rs:40`).

---

## Add a new Event variant (end-to-end)

Adding to the wire protocol touches **four layers in order**. Skipping any one
produces the silent-drop failure mode (a frame that doesn't deserialize is
discarded with no error — see [Reference](#reference)).

### 1. Declare the variant — `core/src/types.rs`

Add to the `Event` enum (`types.rs:164`). Snake_case is automatic via the
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

### 2. Handle it in `SystemState::apply` — `core/src/state.rs`

The `match` in `apply` (`state.rs:19`) is **exhaustive** — the crate will not
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

| Consumer | File | Add an arm if… |
|----------|------|----------------|
| Agent router | `agentd/src/main.rs:930` (`spawn_agent_router`) | the event should drive/cancel a turn (it has a catch-all `Ok(_) => {}`, so it ignores unknown variants safely) |
| Plugin supervisor | `plugins/src/supervisor.rs:158` | the event affects tool dispatch/approval |
| Evolution applier | `agentd/src/main.rs` (`spawn_evolution_applier`) | it's an `Evolution*` variant |
| Store writer | `store/src/lib.rs` | **automatic** — it logs *every* `Event` as JSONL; no change needed |
| Gateway WS write task | `gateway/src/lib.rs:202` | **automatic** — it relays *every* broadcast `Event` to every socket; no change needed |

Most new outbound events need **no consumer edits** — the store and gateway relay
everything. You only edit a consumer to make the daemon *act* on the event.

### 4. Speak it from the client / UI

**Outbound** (daemon → client): nothing in the daemon to do beyond emitting it;
the gateway already relays it. On the client, add a case to your inbound
dispatch keyed on `"type": "battery_status"`.

**Inbound** (client → daemon): the gateway read task (`gateway/src/lib.rs:243`)
takes the raw frame, **injects `frame["session"] = session_id`** (`lib.rs:245`),
then `serde_json::from_value::<Event>(frame)`. So:
- the client **omits** `session` (the gateway sets it to the socket's session);
- the frame must otherwise deserialize cleanly into your variant or it is
  **silently dropped** (`lib.rs:246` — the `if let Ok(event)` has no `else`).

For the Slint UI specifically, the inbound dispatch + any new `VecModel` row type
live in `ui-slint/src/main.rs` and `ui-slint/src/ui/types.slint` (the Slint struct
must mirror the Rust model). See SDK 05 (UI) for that surface.

### 5. (If it's a frontend intent) confirm the router consumes it

A new *inbound* variant that should *do* something must be matched in
`spawn_agent_router` (`main.rs:969` `loop { match rx.recv().await { ... } }`) or
the supervisor. The router's existing arms are the template: `UserPrompt`
(`main.rs:972`) spawns a turn; `UserCancel` (`main.rs:1059`) cascades an abort;
`UserApproval` is consumed by the **supervisor** (`supervisor.rs:180`), not the
router. Without an arm, the event applies to state and broadcasts but nothing
acts on it.

---

## Worked example: a `node_status` heartbeat event

Goal: a mesh node periodically reports liveness; the daemon logs it (free, via
the store) and broadcasts it; clients show a green dot. This is a pure
*outbound* event — no new intent, no state mutation — the minimal end-to-end case.

**1. `core/src/types.rs`** — add to `enum Event` (after the mesh variants,
`types.rs:245`):

```rust
/// Emitted by the mesh heartbeat task ~every 30s per known peer.
/// Consumers: UI peer roster (green/red dot). No canonical state.
NodeStatus { node_id: String, rss_mb: u32, load1: f32, healthy: bool },
```

**2. `core/src/state.rs`** — add the no-op arm (next to the other mesh no-ops,
`state.rs:106`):

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
every gateway WS write task already relays it (`gateway/src/lib.rs:204`).

**4. Client** — it arrives on the socket as:

```json
{"type": "node_status", "node_id": "pi-zero-3", "rss_mb": 41, "load1": 0.7, "healthy": true}
```

Add an inbound case for `node_status` (filtering is N/A — it carries `node_id`,
not `session`; see the multi-client note below). Done — the full path works
without touching the agent loop, the supervisor, or the policy engine.

> **If this were an inbound intent instead** (say `set_node_label`), you would
> also (a) include `session: SessionId` in the variant, (b) have the client send
> `{"type":"set_node_label", "label":"kitchen"}` *without* `session`, and (c) add
> a match arm in `spawn_agent_router` (`main.rs:969`) to act on it.

---

## How a client speaks the protocol correctly

This is the exact handshake — and it **differs from CLAUDE.md** (see Gotchas).

1. **Connect** to `ws://HOST:8787/ws`. If `AGENTD_TOKEN` is set on the daemon,
   you MUST authenticate: WebSocket upgrades can't carry an `Authorization`
   header from a browser, so pass `?token=<TOKEN>` (`gateway/src/lib.rs:106`).
   REST `/api/*` accepts either `Authorization: Bearer <TOKEN>` or `?token=`.

2. **The server speaks first.** On connect, the gateway assigns a session id and
   **immediately pushes** a `session_init` frame *before* you send anything
   (`lib.rs:189–192`):
   ```json
   {"type": "session_init", "session_id": 42, "history": []}
   ```
   This is the daemon→client `session_init` (it carries `history`). You do **not**
   send `session_init` to start — the CLAUDE.md "send `{type:session_init}` on
   connect" instruction is wrong for the Rust daemon.

3. **To resume a prior session**, send a `hello` frame with `resume_session`
   (`lib.rs:228–241`):
   ```json
   {"type": "hello", "resume_session": 42}
   ```
   If session 42 exists in the in-memory `histories` map, the server rebinds this
   socket to it and replies with a fresh `session_init` carrying the full
   `history`. If it doesn't exist, you keep your freshly-assigned id (no error).

4. **Send a prompt:**
   ```json
   {"type": "user_prompt", "text": "hello"}
   ```
   Omit `session` — the gateway injects it.

5. **Approve/reject a tool** when you receive `approval_pending`. Use the numeric
   `call.id` as `action`, and `granted` (boolean) — **not** `call_id`/`approved`:
   ```json
   {"type": "user_approval", "action": 5, "granted": true}
   ```

6. **Cancel a turn:**
   ```json
   {"type": "user_cancel"}
   ```
   The router runs `cascade_cancel` (`main.rs:1060`) which aborts the turn (and
   children) but emits **no** `TurnComplete`. Your client must clear its own busy
   state and any pending tool cards.

7. **Filter on `session`.** The gateway broadcasts **every session's events to
   every connected socket** with no server-side filter. Outbound frames carry the
   same `session` number the gateway injected inbound. A multi-client browser/PWA
   deployment MUST drop frames whose `session` ≠ its own, or it renders another
   session's output. (A single-display kiosk never hits this.)

### Minimal client (pseudocode)

```js
const ws = new WebSocket(`ws://pi:8787/ws?token=${TOKEN}`);
let mySession = null;
ws.onmessage = ({data}) => {
  const ev = JSON.parse(data);
  if (ev.type === "session_init") { mySession = ev.session_id; renderHistory(ev.history); return; }
  if ("session" in ev && ev.session !== mySession) return;   // multi-client filter
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
  `ToolCall.needs_approval = false` unconditionally (`turn.rs:118`); the
  **supervisor** is what decides, calling `policy.check(&call.tool, path)`
  (`supervisor.rs:163`) and emitting `ApprovalPending` on `Decision::Ask`
  (`supervisor.rs:175`). So **`needs_approval` on the wire is currently always
  `false`** — clients must rely on receiving an `approval_pending` event, not on
  that flag. (If you make `needs_approval` meaningful, you must set it in the
  supervisor's `ToolRequested` handling, not the agent.)

- **Evolution events are the audited self-modification path.** A new
  `EvolutionProposal` variant (`types.rs:88`) is the *correct* way to let the
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
  is dropped, not errored (`lib.rs:246`) — a malicious client can't crash the
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
| `hello` | `resume_session: u64?` | resume a session; server replies `session_init` with history |
| `user_prompt` | `text: string` | start/continue a turn |
| `user_approval` | `action: u64` (= `ToolCall.id`), `granted: bool` | resolve a pending approval |
| `user_cancel` | — | cascade-abort the turn (no `turn_complete` follows) |

### Outbound events (daemon → frontend), selected — client filters on `session`

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
| `council_*` | see `types.rs:217–224` | multi-agent deliberation stream |
| `error` | `session: u64?`, `message` | `session` is optional |
| `vast_*` | instance/tunnel lifecycle | `types.rs:230–237` |
| `peer_seen` / `peer_registered` / `peer_lost` | mesh discovery | `types.rs:241–245` |
| `evolution_proposed` / `evolution_applied` / `evolution_rolled_back` | self-modification audit | `types.rs:250–267` |

> `turn_started` is **not** emitted by the Rust daemon. Do not depend on it.

### Nested struct shapes (`types.rs`)

| Struct | JSON | Source |
|--------|------|--------|
| `ToolCall` | `{ "id": <num>, "tool": <str>, "args": {…}, "needs_approval": <bool> }` | `:272` |
| `ToolOutput` | `{ "ok": <bool>, "content": <any> }` | `:281` |
| `ToolSpec` | `{ "name", "description", "input_schema": {…} }` | `:287` |
| `ContentBlock` | tagged `type`: `text`/`thinking`(`+signature`)/`tool_use`/`tool_result` | `:329` |
| `SensorReading` | tagged `kind`: `temperature`/`humidity`/`pressure`/`motion`/`distance`/`gpio_level`/`air_quality`/`thermal_frame` | `:121` |
| `EvolutionProposal` | tagged `kind`: `register_mcp_server`/`unregister_mcp_server`/`update_policy_rule`/`update_system_prompt`/`hot_reload_subsystem` | `:88` |

### Enums

| Type | Wire values | Source |
|------|-------------|--------|
| `PolicyMode` (global) | `suggest` \| `auto-edit` \| `yolo` (kebab-case) | `:29` |
| `PolicyRule` (per-tool) | `allow` \| `ask` \| `workspace` (kebab-case) | `:45` |
| `Subsystem` | `plugins` \| `policy` \| `agent` \| `gateway` (snake_case) | `:77` |
| `Message` role | `user` \| `assistant` (tagged `role`) | `:322` |

### Bus / state facts

| Fact | Value | Source |
|------|-------|--------|
| Inbox (mpsc) capacity | 1024 | `bus.rs:25` |
| Broadcast capacity | 1024 | `bus.rs:26` |
| Emit | `BusHandle::emit(Event).await` | `bus.rs:17` |
| Subscribe | `broadcast::Sender::subscribe()` | `bus.rs:24` |
| State mutation | only in `SystemState::apply` (pure, sync) | `state.rs:18` |
| Lagged subscriber | `RecvError::Lagged(n)` → consumer skips (`continue`) | e.g. `lib.rs:208`, `turn.rs:170` |
| Tool-result wait timeout | `AGENTD_TOOL_RESULT_TIMEOUT_SECS`, default **1800s** | `turn.rs:68` |

### Gotchas vs. CLAUDE.md

The CLAUDE.md and (older) protocol tables contain stale claims. Trust the source:

| CLAUDE.md says | Reality (source) |
|----------------|------------------|
| client sends `{type:"session_init"}` on connect | server **pushes** `session_init` first; client sends nothing to start (`lib.rs:189`) |
| resume via `{type:"session_init", session_id}` | resume via `{type:"hello", resume_session:<id>}` (`lib.rs:228`) |
| `turn_started` clears buffer / sets busy | Rust daemon **never emits** `turn_started`; busy is driven by `agent_text` |
| `tool_result` has `call: <id>` | correct — `call` is a **bare number** here, *unlike* `tool_requested` where `call` is a full `ToolCall` object (`types.rs:177` vs `:173`) |

### The four-layer checklist for a new variant

1. `core/src/types.rs` — add the `Event` variant (snake_case auto).
2. `core/src/state.rs` — add an `apply` arm (no-op unless it holds canonical state).
3. consumers — add a match arm **only** where the daemon must *act* (router /
   supervisor / evolution applier); store + gateway relay automatically.
4. client — inbound dispatch case; for an intent, also send it without `session`
   and add a router arm.

Miss step 1/2 → won't compile. Miss step 4 inbound → silently dropped, no error.
