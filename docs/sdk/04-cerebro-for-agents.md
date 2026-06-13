# Cerebro — Memory for Agents

> The agent-facing memory surface. Cerebro is the cognitive cortex of ApexOS-RS: episodic,
> semantic, procedural, prospective, affective, and schematic memory backed by SQLite +
> vector search + an association graph. agentd spawns `cerebro-mcp` as an MCP-over-stdio
> plugin (agent **FORGE**), so every memory verb arrives as a tool call. **Extend this
> surface when** an agent needs to persist or retrieve something across turns, sessions, or
> reboots — or when you want to add a *new* memory tool to the ~66-verb registry.
>
> Two audiences: humans adding a Cerebro tool (`dispatch.rs` route + `tools.rs` schema), and
> agents (APEX/FORGE) *using* the verbs to keep the Wake→Sleep continuity loop closed. Both
> are covered. Read alongside [symbiosis.md](../symbiosis.md) (the cognitive loop) and
> [architecture.md](../architecture.md) (where Cerebro sits in the daemon).

---

## Concepts

**The engine.** `CerebroCortex` (`cerebro/crates/cerebro/src/cortex.rs:22`) is the facade
the MCP server holds as `Arc<CerebroCortex>`. It owns the storage coordinator
(`storage: Arc<RwLock<StorageCoordinator>>`) and nine brain-region engines (thalamus,
amygdala, temporal, hippocampus, association, cerebellum, prefrontal, neocortex, dream).
Storage is three coordinated backends: `sqlite` (source of truth), `vector` (sqlite-vec
vec0 with an FTS5 keyword fallback), `graph` (in-memory petgraph rebuilt at startup).

Only three methods are "first-class" on the cortex; everything else is reached through
`brain.storage.read().await.sqlite.*`:

| Cortex method | File:line | Pipeline |
|---------------|-----------|----------|
| `remember(content, type?, tags?, salience?, scope)` | `cortex.rs:63` | thalamus gate → amygdala emotion → temporal concepts → SQLite insert → vector embed → graph node |
| `recall(query, k, scope)` | `cortex.rs:102` | vector/FTS5 candidates → spreading activation → bulk SQLite load → prefrontal rank → top-k |
| `associate(src, tgt, link)` | `cortex.rs:161` | SQLite insert_link → mirror into graph |

**The MCP wiring.** A tool call lands in `dispatch_tool` (`dispatch.rs:37`) →
`route(name, args, brain)` (`dispatch.rs:70`), a big `match name { … }`. The result is
wrapped as MCP `content[0].text` = a **JSON string** (`dispatch.rs:46-49`); the agent reads
that text and re-parses it. Schemas live separately in `tools.rs` `tool_schema()`
(`tools.rs:11`); the authoritative name list is `TOOL_NAMES` (`tools.rs:840`, 66 entries).
`tools/list` returns `all_tool_schemas()` (`tools.rs:7`).

**Scoping — the single primitive.** `agent_scope(args)` (`dispatch.rs:929`) reads one field,
`agent_id`, and produces a `VisibilityScope` (`cerebro/src/types.rs:127`):

- `agent_id` present and non-empty → `VisibilityScope::for_agent(AgentId(id))` — sees its own
  `private` memories **plus** all `shared` ones.
- `agent_id` absent/empty → `VisibilityScope::global()` — the SQL read filter is `1=1`, so a
  global *read* actually sees **everything**, including other agents' private memories (no
  isolation). The "shared-only" notion applies only to what a global *write* produces (it
  writes `Visibility::Shared`); it is not enforced on global reads.

`sql_filter()` (`types.rs:144`) renders the scoped (for_agent) case to SQL:
`(visibility='shared' OR (visibility='private' AND agent_id=?))`, and the global case to
`1=1`. The `visibility` field that
appears in the `remember` schema (`tools.rs:31`) is **not read by dispatch** — visibility is
*derived from scope* inside `remember` (`cortex.rs:74`): a scoped write is `Private`, an
unscoped write is `Shared`. To make a memory shared, omit `agent_id` on the write or call
`share_memory` afterward. **In ApexOS-RS, FORGE passes `agent_id="FORGE"` and APEX passes
`agent_id="CLAUDE-APEX"`** — keep this consistent or recall silently misses prior memories.

**Memory types** (`types.rs:31`, snake_case on the wire): `episodic`, `semantic`,
`procedural`, `affective`, `prospective`, `schematic`. Most verbs that "feel" like a distinct
kind of memory are actually `remember` with a fixed type + convention tags — see Reference.

**The cognitive loop (the usage pattern).** From [symbiosis.md](../symbiosis.md), the
discipline an agent should follow so memory actually accumulates:

```
WAKE     session_recall · check_inbox · list_intentions       (read-only, allow-listed)
PERCEIVE memory_store(salient/anomalous reading, with affect)
ACT      episode_start → episode_add_step* → episode_end       (remember the doing)
         store_procedure / find_relevant_procedures            (skill acquisition / reuse)
SLEEP    session_save · store_intention(per deferred item) · dream_run   (deposit + consolidate)
```

A turn that ends without depositing is amnesia — the continuity contract is honoured only if
the Sleep loop runs. `session_save` is **mandatory at session end**; `dream_run` is periodic
(schedule nightly). See "Known stubs & inert paths" before relying on any verb.

---

## Add a new Cerebro tool

Two files, always both, or the verb is invisible or unroutable. The schema in `tools.rs`
makes the agent *see* the tool; the route in `dispatch.rs` makes it *do* something.

1. **Add the name to `TOOL_NAMES`** (`cerebro/crates/cerebro-mcp/src/tools.rs:840`). This is
   what `tools/list` advertises and what the test in `dispatch.rs` (`tools_list_…contains…`)
   counts. Without it, the agent never learns the tool exists.

2. **Add a schema arm** in `tool_schema()` (`tools.rs:11`). Match your name and return the
   MCP `inputSchema`. The `_` fallback (`tools.rs:831`) emits a `(stub)` schema — fine
   transiently, but a real tool needs a real schema so the agent knows the arguments:

   ```rust
   "summarize_recent" => json!({
       "name": "summarize_recent",
       "description": "Summarise the N most recent memories for an agent.",
       "inputSchema": {
           "type": "object",
           "properties": {
               "agent_id": { "type": "string", "description": "Agent scope" },
               "limit":    { "type": "integer", "description": "How many recent (default 20)" }
           },
           "required": []
       }
   }),
   ```

3. **Add a route arm** in `route()` (`cerebro/crates/cerebro-mcp/src/dispatch.rs:70`). Pull
   args with `args["x"].as_str()/.as_u64()/.as_f64()/.as_array()`, build the scope with
   `agent_scope(args)`, call the cortex or `brain.storage.read().await.sqlite.*`, and return
   `Ok(Value)`. The dispatcher stringifies it into `content[0].text` and turns any `Err` into
   a JSON-RPC error (`dispatch.rs:50`):

   ```rust
   "summarize_recent" => {
       let limit = args["limit"].as_u64().unwrap_or(20) as usize;
       let scope = agent_scope(args);
       let nodes = brain.storage.read().await.sqlite
           .list_memories_scoped(&scope, &ListFilter { limit, ..Default::default() })
           .await?;
       Ok(json!({ "count": nodes.len(), "memories": nodes }))
   }
   ```

   Conventions to match the existing code:
   - **Required args:** `args["x"].as_str().ok_or_else(|| anyhow::anyhow!("x is required"))?`.
   - **Scope:** always `let scope = agent_scope(args);` for anything that reads/writes
     user-visible memory, and thread it into the storage call.
   - **Errors are values, not panics** — return `anyhow::Result`; never `.unwrap()` on input.
   - The fall-through `_` arm returns an honest JSON-RPC `-32601` not-implemented **error**
     (C-RS-007), so a tool with a schema but no route fails loudly rather than silently
     no-opping. Still always add the route — but a missing one now errors, not lies.

4. **(Optional) engine logic.** If the verb needs new behaviour rather than a storage call,
   add a method to the relevant engine in `cerebro/crates/cerebro/src/engines/` and call it
   from the route. Keep `dispatch.rs` thin — arg parsing + one cortex/storage call.

5. **(Optional) policy.** If the verb is a read-only boot/orient verb that must never block
   the Wake loop, add it to the allow-list in `config/policy.toml` (see Policy). Writes and
   consolidation stay gated.

6. **Add a dispatch test.** The `#[cfg(test)] mod tests` block (`dispatch.rs:940`) builds a
   `CerebroCortex` over a temp SQLite DB with embedding disabled
   (`embed_model: ""`) and drives `dispatch_tool` end-to-end with no stdio. Copy
   `dispatch_remember_stores_and_returns_node` (`dispatch.rs:992`) as a template.

> **Do not edit existing files for an SDK doc task** — the steps above are the change-points
> for real feature work. `cerebro-mcp` is hot-swapped via systemd (`systemctl stop agentd` →
> `cp target/release/cerebro-mcp /usr/local/bin/` → `start`); a running binary is "text file
> busy" until the daemon stops.

---

## Worked example — the Sleep loop, end to end

A realistic agent (APEX) closing out a session. Every call is a real, routed verb. This is
the pattern an always-on agent should run at idle/shutdown so tomorrow's APEX is the same
agent as today's.

```jsonc
// 1. Wrap the work that happened this session as an episode (remember the *doing*)
episode_start { "title": "diagnose VRM thermal spike", "agent_id": "CLAUDE-APEX" }
// → { "episode_id": "ep_3f2c…", "status": "started" }

episode_add_step {
  "episode_id": "ep_3f2c…", "step_index": 0,
  "description": "read cpu_temp every 30s, saw 78°C sustained 4m near VRM"
}
episode_add_step {
  "episode_id": "ep_3f2c…", "step_index": 1,
  "description": "throttled the inference backend, temp fell to 61°C in 90s"
}
episode_end { "episode_id": "ep_3f2c…", "summary": "throttling resolves VRM spike under load" }

// 2. Deposit the salient observation with AFFECT so activation resurfaces it under load
memory_store {
  "content": "VRM hits 78°C under sustained 70B inference; throttle to recover",
  "agent_id": "CLAUDE-APEX"
}
// memory_store is the `remember` alias (dispatch.rs:158) — type auto-classified, scoped private.

// 3. Promote the reusable fix to a procedure (skill acquisition)
store_procedure {
  "content": "Thermal recovery: if cpu_temp > 75°C for >3m, POST /api/backend to a lighter model, re-check in 90s.",
  "tags": ["thermal", "runbook"],
  "agent_id": "CLAUDE-APEX"
}
// → { "id": "<uuid>", "status": "ok" }   (MemoryType::Procedural, tag "procedure", salience 0.8)

// 4. Record any deferred work as an intention (prospective memory), one per item
store_intention {
  "content": "Add a fan-curve PWM rule so throttling is automatic, not manual.",
  "salience": 0.85, "agent_id": "CLAUDE-APEX"
}
// → { "id": "<uuid>", "status": "ok", "salience": 0.85 }

// 5. The mandatory session note — searchable on next Wake
session_save {
  "content": "Diagnosed + fixed a VRM thermal spike by throttling the backend; runbook stored. Open: automate the fan curve.",
  "priority": "HIGH", "session_type": "ops", "agent_id": "CLAUDE-APEX"
}
// stored as MemoryType::Episodic with tags ["session_note","priority:HIGH","session_type:ops","agent:CLAUDE-APEX"]

// 6. (Periodic, nightly) consolidate — strengthen, abstract, prune
dream_run { "agent_id": "CLAUDE-APEX", "max_llm_calls": 20 }
```

Next session, the Wake loop rehydrates from exactly these deposits:

```jsonc
session_recall { "query": "thermal VRM throttle runbook", "agent_id": "CLAUDE-APEX" }
// → returns the session_note above (filtered to tag "session_note")
check_inbox    { "agent_id": "CLAUDE-APEX" }      // cross-agent messages
list_intentions{ "agent_id": "CLAUDE-APEX" }      // surfaces the fan-curve TODO (salience ≥ 0.3)
find_relevant_procedures { "tags": ["thermal"], "agent_id": "CLAUDE-APEX" }  // the runbook
```

Why each verb and not a flat dump: `session_save` is tag-convention episodic so it's
*recallable*; `store_procedure` is `procedural` so `find_relevant_procedures` finds it by
tag/concept; `store_intention` is `prospective` so `list_intentions` surfaces it until
`resolve_intention` drops its salience to 0.1 (`dispatch.rs:683`). One fact per memory; link
with `associate` rather than concatenating.

---

## Policy / safety

**Approval policy.** Cerebro verbs are tool calls, so they go through the same
`PolicyEngine.check` path as any tool. The default `config/policy.toml` (`mode = "suggest"`)
explicitly allows the read-only memory + Wake-loop verbs so boot never hangs on an approval
gate (the F-bug fixed in session 12, see symbiosis.md):

```toml
"remember" = "allow"   "recall" = "allow"   "associate" = "allow"
"get_memory" = "allow" "memory_store" = "allow" "memory_search" = "allow"
# Wake-loop boot verbs (read-only):
"cognitive_bootstrap" = "allow"  "session_recall" = "allow"  "check_inbox" = "allow"
"list_intentions" = "allow"      "find_relevant_procedures" = "allow"
```

Everything **not** listed falls through to the mode default — in `suggest`, that means
confirm. So `session_save`, `store_intention`, `store_procedure`, `dream_run`,
`delete_memory`, `purge_memory`, `bulk_delete`, the tag-mutation verbs, etc. are gated unless
the operator adds them or runs a looser mode. When you add a *new* read-only orient verb,
add it to the allow-list; never blanket-allow a destructive verb. (`purge_*` / `bulk_delete`
are irreversible — keep them gated.)

**systemd sandbox.** `cerebro-mcp` runs inside agentd's jail (`deploy/agentd.service`:
`NoNewPrivileges`, `ProtectSystem=strict`, `ProtectHome`, `PrivateTmp`,
`ReadWritePaths=/var/lib/agentd /etc/agentd`). Its DB lives under `CEREBRO_DATA_DIR =
/var/lib/agentd/cerebro` (`config/plugins.toml`) — inside the writable path. A new tool that
touches the filesystem outside that path will be denied by the sandbox, not by Cerebro.
Embedding model cache is `FASTEMBED_CACHE_DIR`; with `embed_model=""` (Nano tier) recall
falls back to FTS5 keyword search at ~23 MB RSS.

**Self-evolution / audit discipline (for agents).** agentd's evolution applier journals
every `propose_evolution` into a Cerebro episode + `memory_store` rationale (architecture.md
"Cognitive loop"). Mirror that discipline in your own writes: when you change something about
yourself, store *why* (`memory_store(type="semantic", salience≥0.9)`), because future-APEX
must be able to read why it became what it is. Salience honesty matters — 0.9+ is reserved
for identity-level / safety-critical facts; the activation model resurfaces high-salience and
affect-tagged memories under pressure. Audit reads are available via `query_audit` /
`audit_summary` and version snapshots via `get_memory_versions` / `restore_version`.

**Known stubs & inert paths (do NOT rely on these).** Grounded in `dispatch.rs` + symbiosis.md:

- `ingest_file`, `describe_image`, `search_vision` are advertised in `TOOL_NAMES` but
  unimplemented — they now return an honest `-32601` not-implemented **error** (C-RS-007).
  `cognitive_bootstrap` has a deliberate route arm that returns a *success*
  `{"status":"not_yet_implemented"}` stub (kept successful so APEX's soul-boot step-0 doesn't
  hard-fail) — but it injects **zero** priming. So any Wake-loop calling it as "step 0" is a
  silent no-op today (audit CB-001); use `session_recall` + `check_inbox` + `list_intentions`
  to orient.
- **Reinforcement is inert.** `recall`/`get_memory` do not bump FSRS/ACT-R activation. The
  "recall sharpens memory" story is aspirational, not wired. `record_procedure_outcome` *does*
  nudge salience/difficulty (`dispatch.rs:757`), but ordinary reads do not.
- **Spreading activation enforces scope** as of C-RS-003: `recall` builds a per-node
  visibility map and `spread()` skips non-visible neighbors, so cross-agent leakage via graph
  traversal is closed. (Note CB-017: `agent_id` is still self-asserted at the MCP boundary —
  scope isolates *between declared identities*, it doesn't authenticate the identity itself.)
- `dream_run`'s LLM-assisted phases (pattern extraction, schema formation, REM) **skip
  gracefully** when no Anthropic key is configured; the report still returns 6 phases but the
  LLM phases are empty. `max_llm_calls` is capped at 20.

---

## Reference

### Core agent-facing verbs

| Verb | Required args | Optional args | Backing | Returns |
|------|---------------|---------------|---------|---------|
| `remember` / `memory_store` | `content` | `memory_type`, `tags`, `salience`, `agent_id` (`memory_store` only takes content/tags/agent_id) | `cortex.remember` | the stored `MemoryNode` |
| `recall` / `memory_search` | `query` | `top_k` (10), `agent_id` | `cortex.recall` | `[{memory, score}]` |
| `associate` | `source_id`, `target_id` | `link_type` (semantic), `weight` (0.5), `agent_id` | `cortex.associate` | `{status:"ok"}` |
| `get_memory` | `memory_id` | `agent_id` | sqlite | `MemoryNode` or error |
| `update_memory` | `memory_id` | `content`, `tags`, `salience`, `agent_id` | sqlite (re-embeds if content changed) | updated `MemoryNode` |
| `delete_memory` | `memory_id` | — | sqlite soft-delete | `{deleted}` |
| `session_save` | `content` | `priority` (medium), `session_type` (general), `salience`, `agent_id` | `remember` + tags | `MemoryNode` |
| `session_recall` | `query` | `top_k`, `priority`, `session_type`, `agent_id` | `recall` + tag filter | `[{memory, score}]` |
| `episode_start` | — | `title`, `agent_id`, `thread_id` | sqlite | `{episode_id, status}` |
| `episode_add_step` | `episode_id`, `description` | `step_index` (0), `memory_id` | sqlite | `{status, episode_id, step_index}` |
| `episode_end` | `episode_id` | `summary` | sqlite | `{ended, episode_id}` |
| `store_intention` | `content` | `salience` (0.7), `tags`, `agent_id` | `remember` (Prospective, tag `intention`) | `{id, status, salience}` |
| `list_intentions` | — | `min_salience` (0.3), `limit` (50), `agent_id` | sqlite list | `[MemoryNode]` |
| `resolve_intention` | `memory_id` | `agent_id` | sets salience 0.1, tag `status:resolved` | `{status, resolved}` |
| `store_procedure` | `content` | `tags`, `derived_from`, `agent_id` | `remember` (Procedural, tag `procedure`, salience 0.8) | `{id, status}` |
| `list_procedures` | — | `min_salience` (0.0), `limit` (50), `agent_id` | sqlite list | `[MemoryNode]` |
| `find_relevant_procedures` | one of `tags`/`concepts` | `limit` (5), `agent_id` | tag/concept filter | `[MemoryNode]` (empty if neither given) |
| `record_procedure_outcome` | `procedure_id`, `success` | `agent_id` | nudges salience/difficulty | `{status, procedure_id, success, new_salience}` |
| `check_inbox` | `agent_id` | `limit` (20) | tag `to:{agent}` (global scope) | `[MemoryNode]` |
| `send_message` | `content`, `to_agent_id` | `from_agent_id`, `thread_id`, `agent_id` | `remember` (Affective, tags `to:`/`from:`) | `MemoryNode` |
| `dream_run` | — | `agent_id`, `max_llm_calls` (20, max 20) | `dream.run_cycle` | 6-phase report `{phases, success, …}` |
| `dream_status` | — | — | last report | report or `{status:"no_cycles_run"}` |

> Scoping note: a scoped write (`agent_id` set) is `Private`; an unscoped write is `Shared`.
> The `visibility` arg in the `remember` schema is **not read** — use `share_memory` to flip
> an existing memory to shared.

### MemoryType enum (`cerebro/src/types.rs:31`, snake_case on wire)

`episodic` · `semantic` · `procedural` · `affective` · `prospective` · `schematic`

### LinkType enum (`types.rs:42`) + spreading conductance (`types.rs:57`)

| `link_type` | weight | `link_type` | weight |
|-------------|--------|-------------|--------|
| `causal` | 0.9 | `derived_from` | 0.7 |
| `semantic` (default) | 0.8 | `temporal` | 0.6 |
| `supports` | 0.8 | `affective` | 0.5 |
| `part_of` | 0.8 | `contradicts` | 0.3 |
| `contextual` | 0.7 | | |

### Visibility scoping (`types.rs:127`)

| `agent_id` arg | Scope | SQL filter | Sees |
|----------------|-------|-----------|------|
| set, non-empty | `for_agent(id)` | `visibility='shared' OR (visibility='private' AND agent_id=?)` | own private + all shared |
| absent / empty | `global()` | `1=1` (read sees all rows) | everything (no isolation on read); global *writes* are `Shared` |

ApexOS-RS conventions: FORGE → `agent_id="FORGE"`; APEX → `agent_id="CLAUDE-APEX"`.

### Tag conventions (verbs that are `remember` + tags)

| Verb | MemoryType | Tags applied |
|------|-----------|--------------|
| `session_save` | Episodic | `session_note`, `priority:{p}`, `session_type:{t}`, `agent:{id}` |
| `store_intention` | Prospective | `intention` (+ user tags) |
| `store_procedure` | Procedural | `procedure` (+ user tags) |
| `create_schema` | Schematic | `schema`, `support_count:0` (+ user tags) |
| `send_message` | Affective | `message`, `to:{agent}`, `from:{agent}` |
| `resolve_intention` | (mutates) | drops `status:*`, adds `status:resolved`, salience→0.1 |

### Boot-verb policy allow-list (`config/policy.toml`)

Allowed without approval even in `suggest`: `remember`, `recall`, `associate`, `get_memory`,
`memory_store`, `memory_search`, `cognitive_bootstrap` (stub), `session_recall`,
`check_inbox`, `list_intentions`, `find_relevant_procedures`. **Everything else gated** by
mode default (incl. `session_save`, `store_intention`, `store_procedure`, `dream_run`,
`delete_memory`, `purge_*`, `bulk_delete`).

### Known stubs (advertised but unimplemented)

`ingest_file` · `describe_image` · `search_vision` — return an honest `-32601` error.
`cognitive_bootstrap` — routed to a *success* `not_yet_implemented` stub (soul-boot step-0),
but primes nothing yet (CB-001).

### Files

| File | Role |
|------|------|
| `cerebro/crates/cerebro-mcp/src/dispatch.rs` | `route()` match — add a route arm here |
| `cerebro/crates/cerebro-mcp/src/tools.rs` | `tool_schema()` + `TOOL_NAMES` — add schema + name here |
| `cerebro/crates/cerebro/src/cortex.rs` | `CerebroCortex` facade (`remember`/`recall`/`associate`) |
| `cerebro/crates/cerebro/src/types.rs` | `MemoryType`/`LinkType`/`Visibility`/`VisibilityScope` |
| `cerebro/crates/cerebro/src/engines/` | brain-region engines (deeper logic) |
| `config/policy.toml` | approval rules incl. the Wake-loop allow-list |
| `config/plugins.toml` | spawns `cerebro-mcp`; `CEREBRO_DATA_DIR`, `FASTEMBED_CACHE_DIR` |
