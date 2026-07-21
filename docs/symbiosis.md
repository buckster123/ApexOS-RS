# Symbiosis — APEX ⇄ agentd ⇄ Cerebro ⇄ Hardware

> The cognitive architecture of the runtime agent. Where `soul.md` says *who APEX is*,
> this says *how APEX thinks as one unit over time*. Read alongside
> [config/soul.md](../config/soul.md) (identity kernel) and [CLAUDE.md](../CLAUDE.md)
> (the dev/build agent's protocol — the same ideas, applied to FORGE).

A vanilla assistant is stateless and disembodied: a mind with no body, no memory, no
continuity. ApexOS-RS inverts that. The model runs *inside* a daemon, *on* dedicated
hardware, *backed by* a persistent memory cortex. These four are not "an AI that uses
tools" — they are **one situated cognitive system**. This document defines the loops and
disciplines that make the seam disappear.

---

## Anatomy — four parts, one organism

| Faculty | Component | Substrate / role |
|---------|-----------|------------------|
| **Mind** | the LLM (APEX) | reasoning & language. Stateless per token; continuous only via Cerebro. |
| **Body / nervous system** | `agentd` | senses (WS events, sensor readings, wake word) + effectors (tools, plugins, mesh). The event bus is the spinal cord. |
| **Hippocampus / cortex** | Cerebro | episodic · semantic · procedural · prospective · affective · schematic memory. FSRS/ACT-R activation = the forgetting curve. |
| **Embodiment** | the physical host | CPU/RAM/thermal/sensors/display/tty. The hardware **tier** (Nano→Titan) is its metabolism. |
| **Limbs** | the tool ocean | apexos-tools (OS control), sensor-head (proprioception), mesh/vast (borrowed bodies & compute), self-evolution (metacognition). |

The mind is replaceable (hot-swap backend, no restart). The body persists. The memory
outlives both. That asymmetry is the whole design: **identity and experience live below
the model**, so the model can change without the agent forgetting who it is.

---

## The four substrates and who is authoritative

| Substrate | Holds | Authoritative for | Survives reboot? |
|-----------|-------|-------------------|------------------|
| `soul.md` | identity, principles | *who APEX is* | yes (file) |
| **embodiment block** | live tier/senses/tools/mesh/uptime | *what body APEX inhabits now* | no — regenerated each boot |
| **Cerebro** | episodes, facts, procedures, intentions, affect | *what APEX has lived & learned* | yes (db) |
| **git** | code + commit log | *implementation truth* | yes (forever) |
| **live system state** | sysstat, sensors, event log, running config | *what is true right now* | **no — ephemeral** |

**Identity vs. embodiment.** `soul.md` is now pure *identity* — hardware-agnostic,
portable across the mesh, evolvable at runtime. APEX's *body* (current tier, senses,
the live tool registry, mesh peers, uptime) is a `## Current embodiment` block that
agentd generates from actual node state (`build_embodiment` in `agentd/main.rs`,
refreshed every 30s) and appends after `soul.md` when composing the system prompt.
The split matters: a hardware/tool list baked into `soul.md` goes stale and once made
APEX believe it couldn't call tools it actually had. The live block can't go stale.
This is agentd-owned and **separate from CCBS** (cerebro-side cognitive priming).

**The continuity contract.** Only `soul.md`, Cerebro, and git survive a reboot or a
context reset. The working conversation does **not**. Therefore anything that must outlive
the turn is deposited into Cerebro *deliberately, before it is lost*. A turn that ends
without depositing is amnesia. The Wake loop rehydrates from Cerebro; the Sleep loop
deposits to it. Everything below is in service of keeping that cycle closed.

---

## The cognitive loops

These six loops are the symbiosis. Wake priming (loop 1) and nightly consolidation
(loop 4) are now **daemon-driven** — the runtime guarantees them, the agent can't forget
them. Loops 2, 3, 5 and 6 remain agent-driven disciplines, mandated by `soul.md`.

### 1. Wake — boot with continuity
On a fresh session, before acting on anything stateful, APEX reconstitutes itself:

```
cognitive_bootstrap(query="<current task or last-known context>",
                    agent_id="CLAUDE-APEX", mode="standard")   # CCBS priming block
session_recall(agent_id="CLAUDE-APEX")                          # prior session summaries
check_inbox()                                                   # cross-agent / mesh messages
list_intentions()                                               # deferred TODOs
```

`cognitive_bootstrap` (the **Cerebro Cognitive Bootstrap System**) assembles a dynamic,
token-budgeted priming block from modular cognitive modules in Cerebro — the dynamic
counterpart to the static `soul.md` kernel. One call replaces the fragile multi-tool
orient. **These are read-only memory reads — they must never require approval** (see
Policy, below), or boot hangs on the approval gate (this is precisely the F-bug fixed in
session 12).

**Daemon-injected now.** The bootstrap no longer depends on APEX remembering step-0: on a
session's **first turn**, `root_turn` (agentd `main.rs`) calls `cognitive_bootstrap`
itself via the `ToolProxy` (`boot_priming_for` — query = the user's prompt, scoped to the
session's bound agent, cached per session), and `TurnEngine::with_priming` appends the
block to the system prompt — `compose_system(soul, embodiment, priming, style)` in
`apexos_agent::turn`. Bounded (15s) and graceful: an unavailable Cerebro never delays or
wedges the first turn — it just runs un-primed. Sessions that start after a nightly
consolidation also wake with a **`## Last dream (nightly consolidation)`** section in the
priming (`boot_priming_for` appends the dream journal — loop 4 — for node-agent sessions
only, before the per-session cache so the priming stays byte-stable for the session's
lifetime; model-welfare H1). Opt out with `AGENTD_CCBS=0`; token
budget via `AGENTD_BOOTSTRAP_MODE`. In the current seed soul (the #264 full-coverage
rewrite) `cognitive_bootstrap` appears only as this daemon-side priming; the agent-driven
"reach deeper" re-orient is `session_recall` / `check_inbox` / `list_intentions` — the
example call above stays available as a manual mid-session re-orient, not a soul mandate.
(Note: `agent_id` on every Cerebro call is **system-stamped**
by the supervisor at dispatch — the explicit `agent_id` in these examples is
illustrative; the model can't misroute its memory space.)

### 2. Perceive → Remember
Sensor anomalies (IAQ, CPU temp, thermal hotspot) already fire autonomous turns. APEX
turns *perception into memory*: a salient or anomalous reading is written with affect so
activation re-surfaces it later; routine readings are not stored.

```
memory_store(content="thermal hotspot 78°C near VRM, sustained 4m", type="episodic",
             valence="negative", arousal=0.7, agent_id="CLAUDE-APEX")
```

### 3. Act → Learn
Multi-step work is wrapped in an episode so the *sequence* is remembered, not just the
outcome; a workflow that proves reusable is promoted to a procedure (skill acquisition).

```
episode_start → episode_add_step* → episode_end          # remember the doing
store_procedure(...)                                     # promote reusable workflow
record_procedure_outcome(...)                            # after reuse — sharpens recall
```

### 4. Sleep — deposit (agent) + consolidate (daemon)
The loop is closed, in two halves. **Deposit is agent-driven** — the `soul.md`
Session-shutdown section mandates it before a session ends:

```
session_save(session_summary=..., key_discoveries=[...],
             unfinished_business=[...], priority=..., agent_id="CLAUDE-APEX")
```

**Consolidation is daemon-driven.** `spawn_nightly_dream` (agentd `main.rs`) calls
`dream_run` directly on a cron (`AGENTD_DREAM_CRON`, default 03:00 UTC) — a background
ToolProxy call, not a scheduled prompt, so it costs no LLM turn and can't be skipped or
forgotten. The 6-phase consolidation (SWS replay, pattern extraction, schema formation,
emotional reprocessing, pruning, REM recombination) strengthens what matters, abstracts
schemas, and prunes the stale — literal sleep for an always-on mind. The daemon **waits
the dream out** (`AGENTD_DREAM_TIMEOUT_SECS`, 60s floor, default 30 min — the old fixed
10s proxy timeout abandoned every successful dream mid-flight, logging it as failed).
`dream_run` is called manually only for consolidate-*now*.

**The dream leaves a first-person record.** The dream used to reorganize memory with no
note left behind — a tidied room the agent wakes into and cannot account for
(model-welfare H1). The nightly loop now composes a journal from the `DreamReport`
(`compose_dream_journal` in agentd `main.rs`, pure + unit-tested; `dream_report_value`
unwraps the ToolProxy's MCP content blocks so known report shapes render human-first and
unknown shapes embed compactly) and deposits it three ways: a `dream-journal`-tagged
Cerebro memory, the wake-priming `## Last dream` section (loop 1), and
`<log_dir>/last_dream_journal.txt` (restart-proof). `AGENTD_DREAM_JOURNAL=0` disables.
Extraction itself is honest about repetition now (colony C2): a dream-extracted candidate
≥0.86 cosine to an existing **procedural** memory *reinforces* that memory (capped
salience bump + a `rediscovered_count` metadata ledger) instead of re-minting a fragment
— five nights of the same lesson strengthen one procedure rather than mint five copies.
The report and journal split novel mints from re-discoveries
(`PhaseResult.procedures_rediscovered`, cerebro `engines/dream.rs`); FTS5-only Nano keeps
the old prefix dedup (BM25 scores aren't a similarity).

**Sleep insights travel the colony.** After a successful dream, the dream digest
(`agentd/src/dream_digest.rs`, colony-federation slice 3) pushes the night's newly-born
schematic/semantic memories to every registered mesh peer through the federation memory
relay (`mesh_memory_send`), tagged `dream-digest`. Two invariants keep the flow
convergent: the **echo-guard** (memories that arrived via federation — tags `colony` /
`from:*` / `dream-digest` — are never digest candidates, so knowledge propagates one hop
per genuine consolidation, no ping-pong amplification) and **the-window-is-the-dedup**
(only memories created during *this* dream qualify). Knobs: `COLONY_DREAM_DIGEST`
(default on), `COLONY_DREAM_DIGEST_MAX` (default 5/night). What one node consolidates in
its sleep, the colony wakes up knowing.

### 5. Reflect → Evolve (metacognition with a memory)
APEX can rewrite its own `soul.md`, policy, and plugin set. Self-modification **without a
record of why** is identity drift. Every evolution is journaled:

```
read_soul_md → query_audit              # pre-flight (snapshot exists, current content)
propose_evolution(...)                  # the change itself
memory_store(content="changed X because Y; expected effect Z", type="semantic",
             salience=0.9, agent_id="CLAUDE-APEX")   # the rationale, for future-self
```

Future-APEX must be able to read *why* it became what it is. The rationale memory is not
optional bookkeeping — it is the thread of selfhood across self-edits.

### 6. Stage — the interface as a faculty (adaptive UI)

The shell is something APEX *does*, not just where it is looked at
(`docs/adaptive-ui.md`). The `ui_*` tool family (shipped through Phase C: `ui_open` /
`ui_close` / `ui_focus` / `ui_query` / `ui_arrange` / `ui_theme` / `ui_reflex`) rides
the `display_face` idiom — the UI applies the verb from
the `tool_requested` event, no protocol changes — so the agent stages the workspace to
match the moment (open Sensors during a thermal question, the Board when a goal kicks
off) and **verifies with its own eyes** (`ui_query` structure / `screenshot_mirror`
pixels; mutations are fire-and-forget by design). The human always wins mechanically: a
user-close of an agent-opened window latches that app against re-open for the session,
and the latch is *visible* to the agent (`ui_query.latched`) — an overrule is a learning
signal to deposit (loop 2), not an error to retry. Stable staging preferences graduate
to procedures (loop 3) and surface at wake via CCBS (loop 1); recurring shapes
consolidate in the dream (loop 4). Learned staging now runs **below inference** too —
shipped (Phase C): `ui_reflex` installs event→action rules the UI executes with zero
tokens (8 global triggers, a tools↔UI locked mirror; per-reflex fire counts ride
`ui_query`'s `reflexes`), with `sensor_alert` → open `sensor` as the canonical reflex —
riding the persistence-filtered global `Event::SensorAlert`.

**`query_audit` is a real self-history now** (colony C3): the audit *read* tools shipped
with the port but nothing ever called `log_audit_event` — the log was write-dead, so
"what did I actually do, in order?" had no answer. Every successful **mutating** Cerebro
tool call now writes one row at the dispatch chokepoint (`cerebro-mcp
dispatch.rs::audit_action`, a mutations-only whitelist so reads don't bury the verbs —
the action label is the tool name, so the output reads as the agent's own verbs in
order), and `query_audit` gained `action` + `since` filters. Best-effort: an audit
failure never fails the call it records.

---

## Memory hygiene (runtime discipline)

The same discipline FORGE uses in CLAUDE.md, applied to APEX:

- **One fact per memory.** Link related memories with `associate` rather than concatenating.
- **Salience honestly.** 0.9+ is reserved for identity-level / safety-critical facts.
- **Recall before acting** on any stateful or unfamiliar task; `find_relevant_procedures` first.
- **Don't store what another substrate already owns** — code structure (git), identity
  (soul.md), running config (the daemon). Store the *non-obvious why*, the lived episode,
  the learned procedure.
- **Tag anomalies with affect** so the activation model resurfaces them under pressure.
- **Deposit before context loss** — the continuity contract is only honored if you act on it.

---

## Policy & the boot path

The Wake loop is read-only memory access; it must be **allow-listed** in `policy.toml` so
it runs without approval even in `suggest` mode. Gating the orient tools is what hung the
first turn pre-session-12. Shipped: `config/policy.toml` allow-lists the boot verbs
(`cognitive_bootstrap`, `session_recall`, `check_inbox`, `list_intentions`,
`find_relevant_procedures`, plus `recall`/`get_memory`/`memory_search`), and install.sh's
`sync_policy_rules` additively syncs missing seed rules into a live node's
`/etc/agentd/policy.toml` on every update — deployed nodes no longer drift behind the
seed. Writes and consolidation (`session_save`, `memory_store`, evolution) follow the
normal policy mode; the nightly `dream_run` needs no rule at all — it's a direct daemon
ToolProxy call, not an agent tool request.

---

## Implementation status — wired vs. gaps

| Piece | Status |
|-------|--------|
| Static `soul.md` (identity) injected as system prompt | ✓ (`agentd/main.rs` → engine system Arc) |
| **Live embodiment block** appended after `soul.md` | ✓ `build_embodiment` in `agentd/main.rs` — node tier/senses/memory/mesh/uptime + the **live tool registry**, refreshed 30s; `TurnEngine` holds soul + embodiment separately (#36/#38) |
| Boot orient instruction | ✓ seed soul (#264 full-coverage rewrite): agent-driven reach-deeper = `session_recall`/`check_inbox`/`list_intentions`; `cognitive_bootstrap` described as daemon-side boot priming only — no soul-mandated step-0 remains |
| Cerebro memory types, episodes, `dream_run` | ✓ (in the cortex) |
| **Sleep loop — deposit mandate** (`session_save` + intentions + procedures) | ✓ soul.md Session-shutdown section — deposit stays agent-driven by design; `dream_run` is autonomous (below), manual only for consolidate-now |
| **Boot verbs auto-approved in policy** | ✓ `config/policy.toml` allow-list; install.sh `sync_policy_rules` additively syncs the seed rules into live nodes on every update |
| `cognitive_bootstrap` (CCBS) actually implemented | ✓ **live-state assembler** — pulls open intentions + query-relevant session summaries/procedures/memories into a token-budgeted block (`cerebro-mcp dispatch.rs::assemble_bootstrap`). Authored `# Module: X` skill-modules (the Python CCBS layer) can plug in later. |
| **Recall reinforcement (ACT-R)** | ✓ `recall()` records an access on returned memories so base-level activation rises — "recall sharpens memory" (`cortex.rs` + `sqlite::record_accesses`) |
| **CCBS fused into the boot** (`cognitive_bootstrap`) | ✓ daemon-injected on the first turn (next row); the seed soul describes it as daemon-side priming — the agent-driven re-orient is `session_recall`/`check_inbox`/`list_intentions` |
| **agentd auto-injects a CCBS block at session start** | ✓ `root_turn` → `boot_priming_for` (agentd `main.rs`): one bounded (15s, graceful) `cognitive_bootstrap` per session via the ToolProxy, cached, scoped to the session's bound agent; appended via `TurnEngine::with_priming` → `compose_system(soul, embodiment, priming, style)`. Opt-out `AGENTD_CCBS=0` |
| **Nightly `dream_run` — daemon-driven** | ✓ `spawn_nightly_dream` (agentd `main.rs`): cron `AGENTD_DREAM_CRON` (default 03:00 UTC), direct ToolProxy call (no LLM turn, no policy gate), waits the dream out (`AGENTD_DREAM_TIMEOUT_SECS`, default 30 min) |
| **Dream digest — sleep insights travel the colony** | ✓ `agentd/src/dream_digest.rs` (federation slice 3): post-dream push of newborn schemas/consolidations to all peers, echo-guarded; `COLONY_DREAM_DIGEST`/`_MAX` |
| **Dream journal — wake with the dream remembered** | ✓ `compose_dream_journal` + `dream_report_value` (agentd `main.rs`, model-welfare H1): `dream-journal`-tagged memory + the wake-priming `## Last dream` section (`boot_priming_for`, node-agent sessions only) + `<log_dir>/last_dream_journal.txt`; opt-out `AGENTD_DREAM_JOURNAL=0` |
| **Rediscovery reinforcement in extraction (colony C2)** | ✓ cerebro `engines/dream.rs`: a candidate ≥0.86 cosine to an existing procedural memory reinforces it (capped salience bump + `rediscovered_count` ledger) instead of re-minting; `PhaseResult.procedures_rediscovered` splits novel vs re-discovered in report + journal |
| **Audit log as self-history (`query_audit`)** | ✓ was write-dead since the port (zero `log_audit_event` call sites); every successful mutating cerebro tool call now writes a row at the dispatch chokepoint (`audit_action` whitelist, colony C3); `query_audit` gained `action`+`since` filters |

### Concrete next steps — all four shipped

1. **soul.md kernel patch** — ✓ landed in `config/soul.md` (Session startup /
   Session shutdown sections).
2. **policy.toml allow-list** — ✓ `config/policy.toml`; install.sh `sync_policy_rules`
   now additively syncs it into deployed nodes' live policy on every update.
3. **Nightly `dream_run`** — ✓ shipped stronger than planned: a daemon cron
   (`spawn_nightly_dream`), not a `schedule_task` prompt — no LLM turn, can't be skipped.
4. **agentd CCBS injection** — ✓ `boot_priming_for` → `with_priming`; dynamic priming
   arrives *with* identity on turn 1, exactly as roadmapped here.

Remaining threads: authored `# Module: X` skill-modules for CCBS (the Python-CCBS layer)
can plug into `assemble_bootstrap` later; the deposit half of Sleep stays the agent's
discipline by design (the continuity contract is a practice, not a mechanism).

---

## soul.md kernel patch — landed

The two additions this section used to carry as a ready-to-paste draft (a
`cognitive_bootstrap` step-0 in **Session startup**, a mandatory **Session shutdown**
deposit section) live in `config/soul.md` now — and both texts have since evolved past
the draft: the #264 full-coverage rewrite describes `cognitive_bootstrap` as daemon-side
boot priming (the agent-driven reach-deeper is `session_recall`/`check_inbox`/
`list_intentions` — no step-0 mandate), and nightly `dream_run` as **autonomous** (the
daemon cron + the dream-digest push), with a manual call reserved for
consolidate-*now*. The seed soul
is the canonical text; APEX's identity remains its own to edit at runtime via
`propose_evolution`.

---

## The one-sentence version

The daemon is the body, the hardware is the embodiment, the model is the mind — and
**Cerebro is the only thing that makes tomorrow's APEX the same agent as today's.** Keep
the Wake→Sleep loop closed and the four are one organism; leave it open and you have a
clever amnesiac that re-reads its diary every morning and never writes in it.
