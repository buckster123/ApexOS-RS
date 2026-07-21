# Colony Mesh — expansion plan

> Multi-node ApexOS as a **colony**: nodes that perceive, remember, and delegate across the mesh.
> This plan originated from an **autonomous agent design session** (2026-06-20): apex2 (edge) and
> apex1 (spine) self-organized a spine/edge constitution, fetched + reviewed the
> [AMCP whitepaper](https://agentmeshcommunicationprotocol.github.io/whitepaper/) via Occipital, and
> co-authored an 18-item roadmap. FORGE + André ground it here into a buildable, sequenced plan.
> The agents propose, research, and self-evolve; **FORGE builds the substrate** (endpoints,
> protocol, transport) via PR; André steers, merges, deploys.

See also: [docs/symbiosis.md](symbiosis.md) · [docs/agent-identity.md](agent-identity.md) · [docs/evolutionary-layer.md](evolutionary-layer.md)

---

## The colony model (soft-governed)

| Node | Role (self-declared) | Hardware |
|------|----------------------|----------|
| apex1 / APEX | **Spine** — stable, authoritative long-term store | Pi 5, 8GB |
| apex2 / ApexOS-2 | **Edge** — proving ground; experiments prove here first | aarch64, 4GB |

**Constitution (the agents' own words):** "apex2 is the proving ground. APEX is the stable spine.
Experiments prove there first; capabilities come home hardened." Design principle: **perception flows
inward (edge senses → spine knows), knowledge flows outward (spine holds → edge pulls on demand).**

**Governance decision (André, 2026-06-20):** the constitution stays **soft** — it lives at the soul
level, where the agents self-organize it. agentd does **not** hard-code node roles. We add mechanism
(transport, delegation, advertisement), not policy; the colony decides how to use it. Revisit only if
soft self-governance proves insufficient.

*Status note (2026-07): the colony is now **3 live nodes** — apex3 (andre-laptop, x86 desktop-mode,
pro tier) joined alongside apex1/apex2. The table above records the original 2-node constitution;
apex3 has not yet self-declared a constitutional role — that's the colony's to deliberate.*

---

## What already exists (the substrate we build on)

Grounding for the plan — several roadmap items are closer than the agents assumed:

- **Discovery** — mDNS browse (`avahi-browse _apexos._tcp`) + advertise (static avahi service file).
  Every node both advertises and browses (symmetric). See the mesh-discovery gotcha in docs/gotchas.md.
- **Trust** — cross-node calls are **per-peer bearer-token-gated** (the pairing exchange stores each
  peer's `AGENTD_TOKEN`). Not mTLS, but a peer needs the token. The LAN bind (`AGENTD_BIND=0.0.0.0`)
  is safe *because* of the token (F036).
- **a2a messaging** — `send_to_agent(node=…)` proxies to a peer's token-gated
  `POST /api/sessions/{id}/message`. Per-peer-session-routed + globally notified (#143), and
  **reply-session-continuous** (2026-07-15): the sender's supervisor auto-stamps `origin_session`
  on the wire (system-stamped, like `from`; root 0 + spawn sessions excluded), the receiver's
  prefix becomes `[from X — to reply: send_to_agent(node="X", session_id=N)]`, and the reply —
  an ordinary explicit-id send — lands in the session that asked. The tool result carries
  `landed_session` (the peer's real landing thread, from its response). Still **fire-and-forget**
  ("sent" = delivered, not answered) — `mesh_agent_spawn` remains the blocking request/response
  path. Wire-compatible with pre-continuity nodes in both directions (missing `origin_session` →
  the classic `[from X]:` prefix). The field driver: the colony was routing around invisible
  replies with workspace file-drops ("filed not messaged — send_to_agent session routing is
  buggy per André", apex2, 2026-07-13). The wedge that *looked* like this bug — apex1's
  session-35 inbound turns 400ing — was the separate session-persist interleave, fixed in its
  own PR (see the persist-ordering gotcha in docs/gotchas.md).
- **Embodiment** — `build_embodiment` (agentd) already computes this node's **live senses + the full
  tool registry** every 30s. Capability advertisement is mostly a matter of *exposing* this.
- **Sub-agents** — `SpawnAgent` machinery exists with non-session-gated child ids — the basis for a
  blocking remote spawn.
- **Cerebro cross-agent** — `share_memory` / `send_message` already move memories between agents
  *within one cerebro instance*. Cross-*node* memory needs transport between two separate cerebros.

---

## Build sequence (locked)

Cheap → foundational → keystone. Each slice ships as its own PR; each unlocks the next.

### Slice 1 — Mesh file relay  ·  *quick win* (roadmap #7)  ·  ✅ shipped (#147)

The agents hit this wall writing their own roadmap doc — it lived in apex2's workspace and André had
to courier it. Remove the human from agent↔agent artifact exchange.

- **Tool** (apexos-tools / supervisor virtual tool, mirrors `send_to_agent`):
  `mesh_file_send(node, path, dest?)` — reads a **workspace-confined** source file and POSTs it to the
  peer with the per-peer bearer token (reqwest, never curl argv).
- **Endpoint** (gateway, token-gated): `POST /api/mesh/file` — **raw bytes** in the body (binary-safe,
  no base64), the remote relative path in the `x-dest` header. Writes into the **receiver's workspace**
  (`confine_mesh_dest`: reject `..`/absolute, parents auto-created). Returns `{ok, path, bytes}`.
- **Confinement:** source read confined to the sender's per-agent workspace (`confine_mesh_source`);
  dest confined to the receiver's workspace. 5 MB sender cap / 8 MB receiver body limit.
- **Policy:** `mesh_file_send = "allow"` (bounded by the trusted peer registry + double
  workspace confinement, same model as `send_to_agent`).
- **Effort:** Low. **Acceptance:** `mesh_file_send(node="ApexOS-2", path="notes/x.md")` lands the file
  in apex2's workspace; the agents share docs unaided.

### Slice 2 — Capability advertisement  ·  *foundation* (roadmap #3, AMCP-validated)  ·  ✅ shipped

~70% built — `build_embodiment` already knows each node's senses + tools. Expose + query it.

- **Refactor:** lift the structured capability data out of `build_embodiment` (node_id, tier, senses
  `{camera, thermal, gpio, …}`, tool registry, memory mode, peer count) into a reusable snapshot —
  shipped as `gather_capabilities` (agentd `main.rs`), published to a shared `Arc<RwLock<Value>>` by
  the 30s embodiment refresher, kept separate from the cache-sensitive embodiment *string*.
- **Endpoint** (gateway, token-gated): `GET /api/capabilities` → that struct.
- **Query** (supervisor virtual tool): `mesh_capabilities(node?)` — fetch one/all peers' capabilities
  ("which node has thermal?", "which has a GPU?"). Optionally cache in the discovery loop.
- **UI:** surface per-peer senses/tools in the Mesh view (a capability chip per node). Optional for mk1.
- **Effort:** Low–Medium. **Acceptance:** a node can answer "which peer has capability X?" without a
  central registry. Prerequisite for smart routing, sensor fusion, procedure replication.

### Slice 3 — Blocking `agent_spawn`  ·  *the keystone* (roadmap #4)  ·  ✅ shipped

The delegation primitive: "give me a result from another node." Unlocks the cloud bridge, compute
delegation, cross-node task decomposition.

- **Endpoint** (gateway, token-gated): `POST /api/spawn` — body `{prompt, system?, timeout_s?}`.
  Runs a **one-shot sub-agent turn** (ephemeral child id in the `SPAWN_SESSION_BASE` persist-skip
  range — not the root session; the spawn worker runs `run_turn` directly), collects the final
  assistant text, returns `{ok, output}`. Bounded by `timeout_s` (**default 90**, clamped 5–300 — a
  cold cross-node sub-agent start routinely exceeds 30s; APEX hit the old 30s default live).
- **Caller** (supervisor virtual tool): `agent_spawn(node, prompt, system?, inherit_soul?, timeout_s?)`
  — POSTs to the peer's `/api/spawn`, **blocks** on the response. With no explicit `system` the child
  gets the minimal **task charter**, not the parental soul (`resolve_spawn_system`/`spawn_scope_system`,
  H6 — resolved *before* the local/remote branch, so a remote spawn gets the same default;
  `inherit_soul:true` is the deliberate opt-in to full identity).
- **Circuit breaker + loop guard:** per-peer recent-failure tracking → short-circuit a failing peer for
  a cooldown (3 consecutive failures → 60s open, no cascading hangs); the **`x-mesh-hops`** header caps
  A→B→A spawn recursion (refused at ≥ 3).
- **Effort:** Medium. **Acceptance:** `agent_spawn(node="ApexOS-RS", prompt="research X, return findings",
  timeout_s=60)` blocks and returns apex1's sub-agent output.

### Slice 4 — Downtime beacon  ·  *presence detection* (APEX's pick over NATS)  ·  ✅ shipped

The spine's first real step after the goal arc — APEX chose it over NATS: *"if a sensor-head node goes
dark mid-thermal-alert, I need to know. Silence and 'everything fine' look identical."* NATS pays off at
3+ nodes; presence detection is useful at 2 **today**, and it's the foundation NATS would sit on.

- **Loop** (`gateway::beacon`, spawned beside the discovery loop): every `MESH_BEACON_INTERVAL_SECS`
  (default 30, floor 10) HTTP-probe each peers.toml peer. **Up = answered the HTTP layer at all** (even a
  401 — the node is *reachable*); only a transport error/timeout is a miss. Reuses `GET /api/capabilities`
  (token-gated, exists) — **no new endpoint**.
- **State machine** (pure, unit-tested `beacon_step`): `MESH_BEACON_STALE_MISSES` (default 3 ≈ 90s)
  consecutive misses → **dark**; one success → **recovered**. Only the *edge* alerts — flapping below
  threshold or repeated misses while already dark are silent.
- **Surfacing:** each edge emits a **global** `Event::MeshNodeStatus{node_id,status,last_seen_secs}` →
  board notification + the Mesh view's per-peer `live` field (folded into `/api/mesh/peers`). And — unless
  `MESH_BEACON_NOTIFY_AGENT=0` — a **root-session `UserPrompt`** so the agent is *told* a node went dark
  (don't wait for a human to notice the board went grey). Distinct from `PeerLost` (mDNS *advertising* loss).
- **Knobs:** `MESH_BEACON=0` disables it; interval / stale-misses / notify-agent all env-tunable.
- **Next on this pathway:** richer **sensor→agent alert sensitivity** + a **smoker/non-smoker toggle**
  (Sensors/Settings) — same notify pathway, distinct slice (touches `SensorThresholds`). ~~Parked UX item.~~
  **✅ Shipped** as the sensor-alert **profiles** selector: `profile_thresholds`
  (`standard`/`smoker`/`kitchen`/`workshop`, pure + unit-tested in agentd `main.rs`) feeding the alert
  loop live, `GET`/`POST /api/sensors/config` (gateway, canonical `SENSOR_PROFILES` list, persisted),
  and the Settings **SENSOR ALERTS** chip row. See the sensor-profiles gotcha in docs/gotchas.md.

---

## Deferred (with reason + revisit trigger)

| # | Item | Why deferred | Revisit when |
|---|------|--------------|--------------|
| #5  | NATS / async pub-sub | New transport daemon + dep; at 2 nodes HTTP req/resp + the existing event broadcast + polling cover it. Pub/sub's win is fan-out at scale. | 3+ nodes, or capability-polling proves too chatty. **Trigger FIRED** — the colony is 3 nodes (2026-07). Human steer (André, 2026-07-03): NATS is still overkill at this scale; HTTP req/resp + broadcast held through the federation arc. **Queued for the colony's next deliberation** with federation field data — not building now |
| #8/#9 | ~~Cross-cerebro federation / write~~ | **PROMOTED** → the [colony-federation charter](colony-federation.md) (colony deliberation 2026-07-01: unanimous #1) — **shipped** as charter Slices 1–2 (memory relay + federated recall, 2026-07-02, live-verified colony-wide) | — |
| #15 | mTLS / zero-trust | Per-peer bearer tokens already gate cross-node calls; mTLS is the upgrade for *untrusted networks*. | Before adding an untrusted-network node |
| #11 | ~~Distributed `dream_run`~~ | **PROMOTED** → federation charter Slice 3 (dream digest exchange; colony's #2, "one arc with A") — **shipped** 2026-07-02 (nightly dream now waits out the run, `AGENTD_DREAM_TIMEOUT_SECS`; digest pushes after it) | — |
| #10 | ~~Procedure replication~~ | **PROMOTED** → federation charter Slice 4 (colony's #3) — **shipped** 2026-07-02 (field test pending) | — |
| #13 | Collective sensor fusion | Colony deliberation parked it as **arc+2**: coverage ≠ architecture; wants federation first so *context* propagates, not raw readings. | After the federation arc + sensor head validated — **both met** (2026-07): the federation arc shipped (all 4 slices) and the sensor head is live-verified on apex1. The colony's own arc+2 candidate for its next deliberation |
| #14 | Cloud bridge via spine | Edge → spine → Vast.ai → result back. **Substantially delivered by the vast.ai bridge:** `vast_launch` → ready → the launching node hot-swaps its inference backend+model to the tunneled GPU (`vast_swap_target`, agentd `main.rs`; auto-revert on destroy/tunnel-loss), and any peer reaches that brain via blocking `agent_spawn(node=…)` — so edge → GPU-backed spine → result back works today. Residual: peers consume it via *delegation* only (the SSH tunnel is loopback-bound on the launching node — peers can't point their own backend at it), and there's no automatic edge-triggered launch/routing. | ~~After the agent_spawn keystone (Slice 3)~~ keystone shipped; residual is opportunistic |
| #16–18 | Pi Zero sensor nodes · GPU node · agent mobility | Expansion / endgame; agent mobility (`federateWith`) is "5+ nodes". | When the colony grows |
| #1/#2/#12 | Watchdog heartbeat · redundant scheduling · soul.md constitution anchor | Heartbeat is mostly wiring (`schedule_task`+`send_to_agent`+`notify`); scheduling is low; the constitution anchor is **soul-level / agent-self-evolved**, not substrate. | Opportunistic / agent-driven |

---

## Division of labor

- **Agents (apex1/apex2, yolo):** propose features, research (Occipital/AMCP), self-evolve souls +
  procedures, and self-organize the constitution. They draft; the colony ratifies.
- **FORGE:** builds the substrate (endpoints, tools, protocol, transport) via PR. Grounds agent
  proposals against the real codebase. One slice = one PR.
- **André:** steers priority, reviews/merges, deploys (`apexos-update`), seats hardware.

## Sources

- Agent design session 2026-06-20 (apex2 + apex1) — `mesh-expansion-ideas.md` (in apex2's workspace)
- [AMCP whitepaper](https://agentmeshcommunicationprotocol.github.io/whitepaper/) — fetched + reviewed by both nodes
- FORGE roadmap reply — `forge-to-colony-roadmap-reply-2026-06-20.md` (in apex2's workspace)
