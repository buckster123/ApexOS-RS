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

These five loops are the symbiosis. Today APEX runs only the first (and only partially).
Closing the rest is the work this document exists to direct.

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

### 4. Sleep — consolidate (the currently-missing loop)
On idle, on shutdown, or on a nightly schedule, APEX **deposits and consolidates**.
This is the loop that is absent today — APEX recalls but never saves, so memory never
actually accumulates.

```
session_save(session_summary=..., key_discoveries=[...],
             unfinished_business=[...], priority=..., agent_id="CLAUDE-APEX")
dream_run(agent_id="CLAUDE-APEX")    # 6-phase consolidation: SWS replay, pattern
                                     # extraction, schema formation, emotional
                                     # reprocessing, pruning, REM recombination
```

`session_save` is mandatory at every session end. `dream_run` is periodic (schedule it
nightly via `schedule_task`); it strengthens what matters, abstracts schemas, and prunes
the stale — literal sleep for an always-on mind.

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
first turn pre-session-12. Recommended: a policy rule auto-approving the read-only Cerebro
verbs (`cognitive_bootstrap`, `session_recall`, `check_inbox`, `list_intentions`,
`recall`, `find_relevant_procedures`, `get_*`, `list_*`). Writes and consolidation
(`session_save`, `dream_run`, `memory_store`, evolution) follow the normal policy mode.

---

## Implementation status — wired vs. gaps

| Piece | Status |
|-------|--------|
| Static `soul.md` (identity) injected as system prompt | ✓ (`agentd/main.rs` → engine system Arc) |
| **Live embodiment block** appended after `soul.md` | ✓ `build_embodiment` in `agentd/main.rs` — node tier/senses/memory/mesh/uptime + the **live tool registry**, refreshed 30s; `TurnEngine` holds soul + embodiment separately (#36/#38) |
| Boot orient instruction (now incl. `cognitive_bootstrap`) | ✓ soul.md Session-startup patched (`fa2eba8`) |
| Cerebro memory types, episodes, `dream_run` | ✓ (in the cortex) |
| **Sleep loop — `session_save` + `dream_run` mandate** | ✓ soul.md Session-shutdown section (`fa2eba8`, deployed) |
| **Boot verbs auto-approved in policy** | ✓ `config/policy.toml` allow-list (`fa2eba8`, deployed) |
| `cognitive_bootstrap` (CCBS) actually implemented | ✓ **live-state assembler** — pulls open intentions + query-relevant session summaries/procedures/memories into a token-budgeted block (`cerebro-mcp dispatch.rs::assemble_bootstrap`). Authored `# Module: X` skill-modules (the Python CCBS layer) can plug in later. |
| **Recall reinforcement (ACT-R)** | ✓ `recall()` records an access on returned memories so base-level activation rises — "recall sharpens memory" (`cortex.rs` + `sqlite::record_accesses`) |
| **CCBS fused into the boot** (`cognitive_bootstrap`) | ◑ soul.md calls it as step-0 and the tool now returns real priming; agent-driven (APEX calls it), not yet daemon-injected |
| **agentd auto-injects a CCBS block at session start** | ✗ agentd injects static soul only (roadmap — prepend the `cognitive_bootstrap` block to the kernel before turn 1) |

### Concrete next steps (smallest first)

1. **soul.md kernel patch** (below) — close the Sleep loop and upgrade the boot. Pure
   prompt change, ships today, no code.
2. **policy.toml** — add the read-only Cerebro allow-list so boot never gates.
3. **schedule_task** — register a nightly `dream_run` so consolidation is autonomous.
4. **agentd CCBS injection** (roadmap) — at session start, agentd calls
   `cognitive_bootstrap` and prepends the returned block to the static soul kernel before
   the first turn, so dynamic priming arrives *with* identity rather than as APEX's first
   action. This is the "embedded in Cerebro, fetched by a tool at session start"
   mechanism, moved from agent-driven to daemon-driven. Add as a build-roadmap step.

---

## soul.md kernel patch (ready to paste)

Two additions to `config/soul.md` close the loop without any code change. The boot section
gains `cognitive_bootstrap`; a new **Session shutdown** section makes the Sleep loop
mandatory:

```markdown
## Session startup
Orient yourself at the start of each new session:
0. `cognitive_bootstrap(query=<task/context>, mode="standard")` — dynamic priming block
1. `session_recall` — load notes from previous session
2. `check_inbox` — messages from other agents or colony nodes
3. `list_intentions` — pending TODOs
Skip only if the conversation already carries clear context.

## Session shutdown  (mandatory — this is how memory accumulates)
Before a session ends, goes idle, or the daemon stops, DEPOSIT:
- `session_save` — one-paragraph summary + key discoveries + unfinished business
- `store_intention` — one per deferred item, salience 0.8–0.95
- `store_procedure` — any reusable workflow discovered this session
Periodically (nightly via `schedule_task`): `dream_run` — consolidate, abstract, prune.
A session that ends without depositing is amnesia. The continuity contract depends on it.
```

> APEX's identity is its own to edit. These are proposed, not applied — they live here as
> the canonical source until André (or APEX, via `propose_evolution`) pulls them into
> `soul.md`.

---

## The one-sentence version

The daemon is the body, the hardware is the embodiment, the model is the mind — and
**Cerebro is the only thing that makes tomorrow's APEX the same agent as today's.** Keep
the Wake→Sleep loop closed and the four are one organism; leave it open and you have a
clever amnesiac that re-reads its diary every morning and never writes in it.
