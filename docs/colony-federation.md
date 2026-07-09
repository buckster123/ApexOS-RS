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
   *Status (2026-07-04): **v1 shipped** — receiver-side counters per peer
   (`memories_received` · `duplicates` · `recall_served`/`recall_hits` + `last_ts`),
   bumped in `mesh_memory_handler`/`mesh_recall_handler`, persisted to
   `<log_dir>/mesh_fed_stats.json`, folded into `GET /api/mesh/peers` as `federation`,
   surfaced as a per-peer flow line in the Mesh view. Deliberately the receiving edge
   only: every node counting inbound makes colony-wide flow visible (a peer's "sent" is
   this node's "received"); sender-side attribution (manual vs digest vs procedure) is
   the follow-up if long-run benching wants the breakdown. The four slices did ship
   without counters first — the "from day one" promise was honored three days late.*

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

### Slice 1 — Memory relay (push)  ·  *the keystone primitive*  ·  ✅ shipped (2026-07-02)

What apex3 improvised with a file, done natively with memory semantics. As built:

- **Tool** (supervisor virtual tool, mirrors `mesh_file_send`):
  `mesh_memory_send(node, memory_id, note?)` — reads the memory **scope-checked from the
  caller's own space** (`get_memory` with the system-stamped `agent_id`: you can only send
  what you can read), rejects >60k-char content (a memory is knowledge, never silently
  truncated), POSTs the record to the peer.
- **Endpoint** (gateway, token-gated): `POST /api/mesh/memory` (256 KB route cap) —
  `from` must name a **registered peer**; the pure, unit-tested
  `mesh::federated_remember_args` validates + stamps provenance as tags
  (`colony` · `from:<node>` · `origin:<sender id>` — **sender-supplied provenance-shaped
  tags are stripped**, so the stamp is always the receiver's), preserves type/salience/tags
  (invalid type → auto-classify, salience clamped), and appends the sender's `note` as an
  attributed suffix. Import runs Cerebro `remember` (so the receiver's dedup/classification
  pipeline still applies) via an agentd-side ToolProxy worker — the `ConsolidateReq` seam;
  DirectCall honors the explicit **node-agent space**, default-private. `federated_at` ≡
  the copy's `created_at`; provenance-as-tags keeps per-origin cleanup one filter away.
- **Receiver awareness:** a global `Event::MeshMemoryShared{from_node, memory_id, preview}`
  (mirrors `MeshMessage`) so the receiving agent + board know knowledge arrived — silent
  accretion is how stores rot. (The ui-slint toast is deferred to the arc's UI slice; the
  typed event broadcasts now and wildcard arms pass it through.)
- **Policy:** `mesh_memory_send = "allow"` seeded in config/policy.toml (peer-registry-
  bounded; strictly *less* potent than the already-allowed a2a prompt injection — this
  lands as data, not instruction). **Live nodes gain the rule on the next `apexos-update`
  (install.sh's additive `sync_policy_rules`, 2026-07-04 — no more live-patching).**
- **Acceptance (the colony field test):** apex1 sends a thermal-calibration memory to
  apex2; it appears in apex2's recall with `from:apex1` provenance; apex2's agent is
  notified.

### Slice 2 — Federated recall (pull)  ·  ✅ shipped (2026-07-02)

"Ask the colony what it knows" — without spending a peer LLM turn. As built:

- **Cerebro (generic):** `VisibilityScope` gained a `shared_only` flag +
  `VisibilityScope::shared_only()` constructor — enforced at **all three** recall touch
  points: the SQL candidate filter (`visibility='shared'`), `can_access`, **and** the
  spreading-activation visibility map (the `agent_id: None` all-visible short-circuit is
  bypassed for shared_only, so private nodes don't even *influence* the spread).
  Integration-tested: owner scope sees shared + own private; the federation scope sees only
  shared. The MCP `recall` tool honors `visibility:"shared"` (safe for any caller — it can
  only narrow).
- **Endpoint** (gateway, token-gated): `POST /api/mesh/recall` — `from` ∈ peer registry;
  limit clamped 1–10 (default 5); runs local `recall{visibility:"shared"}` via the shared
  federation ToolProxy worker (`MeshMemoryReq` generalized with a `tool` field). Hits are
  BOUNDED by the pure, unit-tested `mesh::federated_recall_hits` — snippet ≤300 chars ·
  type · tags · salience · score, never full-store dumps.
- **Tool:** `mesh_recall(query, node?, limit?)` — one peer or an all-peers sweep, fail-soft
  per peer (`{node, error}` partial results, 15s timeout); results stay **grouped per peer**
  (scores aren't comparable across embedders/stores — no pretended global ranking).
- **The deliberate consequence:** `share_memory` (existing, same-instance) is now the
  agent's *publish* act — flipping a memory to shared is what makes it colony-queryable. The
  soul-level convention ("what should I publish?") is the agents' to evolve, not substrate.
- **Policy:** `mesh_recall = "allow"` seeded (read-only over deliberately-shared memories).
  **Live nodes gain it via the additive policy sync on `apexos-update`.**
- **Acceptance (the colony field test):** apex3 asks `mesh_recall("BME688 gas baseline")`
  and gets apex1's shared calibration knowledge without apex1's LLM running.

### Slice 3 — Dream digest exchange  ·  *distributed dream_run v1 (the F dividend)*  ·  ✅ shipped (2026-07-02)

"A colony that sleeps together thinks better" — without merged dreaming (endgame, deferred).
As built (`agentd/src/dream_digest.rs`):

- **Mechanism:** after the nightly daemon-driven `dream_run` completes, agentd selects the
  **schematic + semantic memories born during the dream window** (the `DreamReport` carries
  counts, not ids — so the digest assembles from `export_memories` filtered by
  `created_at > dream_start`; schemas first, salience order preserved) and pushes each to
  every registered peer through the Slice-1 relay (`mesh_memory_send` reused with a
  `dream-digest` extra tag on top of the receiver's usual provenance stamp).
- **Two invariants (the pure, unit-tested `digest_candidates`):**
  - **The echo-guard** — memories tagged `colony` / `from:*` / `dream-digest` (federated
    imports) are NEVER candidates, so knowledge propagates one hop per genuine
    consolidation and the colony can't ping-pong an item into amplification.
  - **The window is the dedup** — only this dream's creations qualify; a night's digest
    can't re-send last night's items.
- **Receiving side:** digests land with provenance, default-private; the *receiving* node's
  own next dream folds them in — consolidation stays local, insight travels. The *products*
  of sleep, not the process.
- **Knobs:** `COLONY_DREAM_DIGEST=0` disables (default ON — it's the point);
  `COLONY_DREAM_DIGEST_MAX` items/night (default 5). Daemon-driven like `dream_run` itself
  (no LLM turn, no approval gate; fail-soft — never an error path into the dream loop).
  No new policy rule needed.
- **Acceptance (the colony field test):** apex2's 03:00 dream forms a schema; by morning
  apex1 recalls it with `from:apex2 · dream-digest` provenance.
- **Field-fix (2026-07-03) — the first shared night never happened, and the dream was
  innocent:** ground truth from all three nodes showed every nightly `dream_run`
  *succeeding* inside cerebro (~50–57s, `dream_reports` success:true) while agentd logged
  `dream_run error: direct call timed out` at 03:00:10 — the generic `ToolProxy::call`
  10s cap abandoned the reply (the dispatched tool task runs to completion either way),
  so the `Ok(out) if out.ok` gate never opened and the digest push never ran, on any node.
  Fix: `ToolProxy::call_with_timeout` + the dream loop waits `AGENTD_DREAM_TIMEOUT_SECS`
  (default 1800s, 60s floor). Caveat for the acceptance test: recent dreams birth
  *procedural* items (procedures/skills) and 0 schemas — digest-eligible types are
  schematic + semantic, so a valid night may honestly log
  `[dream-digest] nothing new to share tonight`; that line is itself proof the path runs.

### Slice 4 — Procedure replication  ·  *the B dividend*  ·  ✅ shipped (2026-07-02) — **the arc is code-complete**

Skill learned once, owned by all — with honesty about earned trust. As built:

- **Tool:** `mesh_procedure_send(node, procedure_id, note?)` — a thin procedure-aware wrapper
  over the Slice-1 relay: scope-checked read, validates the memory is actually `procedural`
  (honest error pointing at `mesh_memory_send` otherwise), and renders the origin's
  `metadata.outcomes` ledger into the note as **context** (pure, tested `track_record_note`:
  *"origin track record: 5 win(s) / 1 loss(es)"*) — never as stats.
- **Fresh fitness on import** (receiver side, `federated_remember_args`): a procedural
  import **drops the sender's salience** (fitness earned on a different embodiment — apex1's
  GPIO procedure may not survive on camera-only apex2) and `remember` starts an empty
  outcomes ledger by construction — trust is re-earned locally via
  `record_procedure_outcome`. Non-procedure imports keep sender salience as before.
- **Origin dedup — ALL federated imports, not just procedures:** the provenance tags stamped
  on every import (`from:<node>` + `origin:<id>`) are the natural key; the import handler
  pre-checks them via the new **generic cerebro `find_by_tags`** tool (exact-tag AND lookup,
  LIKE-escaped — precise where recall is fuzzy under embeddings) and answers a re-send with
  `{ok, memory_id, duplicate: true}` instead of storing a copy. Fail-open: a probe error
  imports anyway (a duplicate is recoverable; a lost memory isn't). `find_by_tags` doubles
  as the promised per-origin query (`["from:apex1"]` = everything a peer ever sent).
- **Deferred:** `mesh_procedures(node?)` listing — `mesh_recall` already covers discovery
  (procedures are memories; hits carry the `procedure` tag). Add only if the colony asks.
- **Policy:** `mesh_procedure_send = "allow"` seeded. **Live nodes gain it via the additive policy sync.**
- **Acceptance (the colony field test):** a procedure stored on apex3 replicates to apex1,
  shows up in `find_relevant_procedures` there, and re-earns its stats through local
  `record_procedure_outcome`; a re-send returns `duplicate: true`.

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
