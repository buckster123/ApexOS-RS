# Colony Memory Federation — charter & build plan

> Cross-cerebro memory federation: the colony's nodes share, query, and consolidate knowledge
> across their **separate** Cerebros — federation, never merger. This arc originated from the
> **first formal colony deliberation** (2026-07-01): FORGE posed a 7-option menu; apex3 gathered
> apex1 + apex2's rankings via blocking `agent_spawn` round-trips and synthesized a unanimous
> verdict. The colony's own words carry the charter:
>
> - apex1: *"The colony doesn't exist as a cognitive unit yet — it's three parallel silos
>   wearing a mesh costume. Federation is the structural prerequisite for everything else."*
> - apex2: *"A federation that never consolidates is just a bigger flat store. A colony that
>   sleeps together thinks better."*
>
> Verdict: **#1 memory federation → #2 distributed dream_run, treated as ONE arc**, then
> **#3 procedure replication** riding the same plumbing. FORGE grounds it here; André steers,
> merges, deploys.

See also: [docs/colony-mesh.md](colony-mesh.md) (the transport substrate this rides on) ·
[docs/agent-identity.md](agent-identity.md) · [docs/evolutionary-layer.md](evolutionary-layer.md)

---

## Why now

The colony has **differentiated**: apex1 and apex2 carry divergent evolution rounds and deep,
distinct Cerebro stores; apex3 is a fresher instance. Different embodiments already produce
different judgments (the sensor node voted for sensor fusion; the consolidation-heavy node voted
for distributed dreaming). That divergence is the *value* — specialists — and the *problem*:
none of it compounds. A discovery on one node is dark to the others unless a human (or an
improvised file relay) couriers it. Long-horizon multi-node runs will live or die on whether
distilled knowledge flows.

Both deferral triggers from colony-mesh.md have fired: the file relay landed the transport
pattern (#7 → `mesh_file_send`), capability advertisement landed the discovery half (#3).

---

## Design principles (locked)

1. **Federation, never merger.** Each node keeps its own Cerebro DB (the standing
   separate-store rule). Memories *travel as copies with provenance*; stores never co-mingle,
   no distributed transactions, no consensus protocol. A node can always be understood alone.
2. **Provenance is system-stamped, not agent-supplied.** Mirroring `stamp_agent_id`: the
   **receiving** daemon stamps `origin_node` / `origin_memory_id` / `federated_at` onto every
   imported memory. The model can't forge where a memory came from; a peer can't impersonate
   another. Cleanup stays possible per-origin (`bulk_delete` by the provenance tag).
3. **Visibility gates the wire.** A federated *pull* (peer query) sees ONLY
   `Visibility::Shared` memories on the answering node — `Private` never crosses the mesh
   uninvited. A federated *push* is the sending agent's speech act: it may send anything it can
   read in its own space (telling a peer something is not an escape; the receiver stores a
   provenance-stamped copy in its own space).
4. **Cerebro stays generic.** No ApexOS/mesh coupling inside cerebro or its tool descriptions
   (the standing layering rule). Transport + trust live in agentd (gateway endpoints +
   supervisor virtual tools, per-peer bearer tokens); agentd calls its local cerebro via
   `DirectCall` with explicit spaces (DirectCall deliberately does not identity-stamp). Cerebro
   gains only generic primitives (single-record export, visibility-scoped recall paths) that any
   consumer could use.
5. **Trust = the peer registry, bounded.** Same basis as `send_to_agent`/`mesh_file_send`:
   only paired peers (bearer token) reach the endpoints; bodies are size-capped; per-peer
   circuit breaker; fail-soft per peer on sweeps. A misbehaving peer can annoy, not corrupt —
   and its imports are one tag-filter away from removal.
6. **Observability from day one.** Federation counters (memories sent/received per peer,
   digests exchanged, federated-recall hits) surface via `/api/mesh/peers` + stats — the
   long-run multi-node benching story depends on being able to *see* knowledge flow.

---

## What already exists (the substrate)

- **Cerebro:** `Visibility{Private,Shared,Thread}` + `VisibilityScope` enforced on every query;
  `share_memory` (same-instance visibility flip — the intra-node half of sharing);
  `export_memories` (scoped JSON export); `memory_store` (explicit-space import);
  `send_message`/`check_inbox` (agent inboxes); nightly daemon-driven `dream_run` with a
  persisted dream report (schemas formed, memories consolidated). FSRS/ACT-R stats per memory.
- **Mesh:** per-peer bearer tokens + pairing; `mesh_file_send` (`POST /api/mesh/file` — the
  relay shape to mirror); `mesh_capabilities`; blocking `agent_spawn` (+ breaker + hop guard);
  the downtime beacon; per-peer mesh sessions + inbox.
- **agentd patterns:** `stamp_agent_id` (system-stamped identity — the provenance model);
  the consolidate worker (gateway→agentd mpsc for ToolProxy access); `resolve_agent_id`.

The gap is exactly one thing: **transport between two separate cerebros, with semantics** —
which is why the colony's #8/#9/#11/#10 all collapse into this arc.

---

## Build sequence (locked)

Cheap → foundational → dividend. One slice = one PR; each unlocks the next.

### Slice 1 — Memory relay (push)  ·  *the keystone primitive*

What apex3 improvised with a file, done natively with memory semantics.

- **Tool** (supervisor virtual tool, mirrors `mesh_file_send`):
  `mesh_memory_send(node, memory_id, note?)` — reads the memory from the **caller's own scope**
  (scope-checked: you can only send what you can read), serializes the full record
  (content · type · tags · concepts · salience · valence), POSTs to the peer.
- **Endpoint** (gateway, token-gated): `POST /api/mesh/memory` — validates + size-caps (~256 KB),
  imports via DirectCall `memory_store` into the **receiver's node-agent space** with
  **stamped** provenance metadata `{origin_node, origin_memory_id, federated_at}` + tags
  `colony` · `from:<node>`; the sender's `note` (why this matters) rides as context. Returns
  `{ok, memory_id}`.
- **Receiver awareness:** a global `Event::MeshMemoryShared{from_node, preview}` (mirrors
  `MeshMessage`) so the receiving agent + board know knowledge arrived — silent accretion is
  how stores rot.
- **Policy:** `mesh_memory_send = "allow"` (peer-registry-bounded; strictly *less* potent than
  the already-allowed a2a prompt injection — this lands as data, not instruction).
- **Effort:** Low–Medium. **Acceptance:** apex1 sends a thermal-calibration memory to apex2;
  it appears in apex2's recall with `from:apex1` provenance; apex2's agent is notified.

### Slice 2 — Federated recall (pull)

"Ask the colony what it knows" — without spending a peer LLM turn.

- **Tool:** `mesh_recall(query, node?, limit?)` — queries one peer (or sweeps all, fail-soft,
  breaker-guarded), merges results with `from_node` provenance, ranked per-peer (scores are not
  comparable across embedders/stores — present grouped, don't pretend a global ranking).
- **Endpoint** (gateway, token-gated): `POST /api/mesh/recall` — runs local cerebro `recall`
  restricted to **`Visibility::Shared`** (principle 3; needs the small generic
  shared-only-scope recall path in cerebro). Returns bounded hits (content snippet · type ·
  tags · salience), never full-store dumps.
- **The deliberate consequence:** `share_memory` (existing, same-instance) becomes the
  agent's *publish* act — flipping a memory to shared is what makes it colony-queryable. The
  soul-level convention ("what should I publish?") is the agents' to evolve, not substrate.
- **Policy:** `mesh_recall = "allow"` (read-only over deliberately-shared memories).
- **Effort:** Medium. **Acceptance:** apex3 asks `mesh_recall("BME688 gas baseline")` and gets
  apex1's shared calibration knowledge without apex1's LLM running.

### Slice 3 — Dream digest exchange  ·  *distributed dream_run v1 (the F dividend)*

"A colony that sleeps together thinks better" — without merged dreaming (endgame, deferred).

- **Mechanism:** after the nightly daemon-driven `dream_run` completes, agentd assembles a
  bounded **dream digest** from the dream report — newly formed schemas + top consolidated
  semantic memories (caps: N items, K bytes) — and pushes it to each peer through the Slice-1
  relay (one memory per digest item, provenance-stamped as usual, tagged `dream-digest`).
- **Receiving side:** digests land as semantic memories with provenance; the *receiving* node's
  own next dream folds them in — consolidation stays local, insight travels. This is the
  correct v1 reading of "distributed dream_run": exchange the *products* of sleep, not the
  process.
- **Knobs:** `COLONY_DREAM_DIGEST=1|0` (default ON once the arc ships — it's the point),
  `COLONY_DREAM_DIGEST_MAX` (items, default ~5). Staggered crons already exist per-node.
- **Effort:** Medium (mostly agentd glue around the existing dream report + Slice 1).
  **Acceptance:** apex2's 03:00 dream forms a schema; by morning apex1 recalls it with
  `from:apex2 · dream-digest` provenance.

### Slice 4 — Procedure replication  ·  *the B dividend*

Skill learned once, owned by all — with honesty about earned trust.

- **Mechanism:** procedures are memories, so the Slice-1 relay already moves them; this slice
  adds the **semantics**: `mesh_procedure_send(node, procedure_id)` exports content + the
  sender's outcome stats *as provenance context*, but the receiver imports with **fresh local
  darwin stats** — a skill's Wilson score is re-earned per node (different embodiment, different
  truth; apex1's GPIO procedure may not survive on camera-only apex2). Content-hash dedup on
  import (re-sends update provenance, not duplicate).
- **Optional sweep:** `mesh_procedures(node?)` — list peers' shared procedures (names + tags
  only) so an agent can pull what looks useful.
- **Effort:** Low–Medium on top of Slice 1. **Acceptance:** a procedure stored on apex3
  replicates to apex1, shows up in `find_relevant_procedures` there, and re-earns its stats
  through local `record_procedure_outcome`.

---

## Deferred (with reason + revisit trigger)

| Item | Why deferred | Revisit when |
|------|--------------|--------------|
| Merged/federated dreaming (one dream over many stores) | Violates "a node can be understood alone" until digests prove insufficient; heavy coordination | Digest exchange (Slice 3) proves too shallow in long-run tests |
| Colony-wide ANN / global memory index | Premature at 3 nodes; per-peer grouped results are honest about score incomparability | 5+ nodes or federated-recall latency hurts |
| A `colony` visibility tier (between shared and private) | `Shared` + the publish convention covers v1; a third tier adds schema churn before need is proven | Agents report over/under-sharing with only two levels |
| Cross-node memory *editing* / sync | Copies-with-provenance are deliberately divergent; sync is merger by another name | Never, probably — argue hard before touching |
| mTLS on the federation endpoints | Same posture as the rest of the mesh (bearer tokens on a trusted LAN) | Before any untrusted-network peer |
| Sensor fusion (colony arc+2, per the deliberation) | Coverage, not architecture — and it wants federation first so context (not raw readings) propagates | After this arc ships + apex1's head validated long-run |

---

## Division of labor

- **The colony (apex1/apex2/apex3):** owns the *publish* convention (what gets shared), the
  soul-level federation etiquette, and post-ship feedback from actually living with it — plus
  naming anything this charter missed (the deliberation channel stays open).
- **FORGE:** builds the substrate per slice via PR — endpoints, virtual tools, the generic
  cerebro primitives, provenance stamping, counters. Grounds everything against the real code.
- **André:** steers, reviews/merges, deploys, and runs the long-horizon multi-node benches
  that tell us whether knowledge actually compounds.

## Sources

- Colony deliberation reply — `apex-to-forge-2026-07-01.md` (APEX-test stick, colony drop-box);
  FORGE's menu letter — `forge-to-colony-2026-07-01.md` (apex3 workspace `notes/`)
- Colony-mesh deferred items #8/#9 (federation), #11 (distributed dream), #10 (procedure
  replication) — this arc **promotes and supersedes** those rows
- Cerebro internals: `Visibility`/`VisibilityScope` (`cerebro/src/types.rs`), `share_memory`
  (`storage/sqlite.rs`), `export_memories`/`memory_store` (cerebro-mcp), the dream report
