# Evolutionary Memory — the Exo-Cortex grows

> Sibling to [symbiosis.md](symbiosis.md). Where symbiosis says *how APEX stays the same
> agent over time*, this says *how APEX gets better over time* — without anyone touching the
> model weights. Read alongside [config/soul.md](../config/soul.md) (identity) and the
> Cerebro memory model.

The model is frozen. APEX cannot fine-tune itself; its weights are fixed the moment the
backend is chosen, and they change only when the whole mind is hot-swapped for another. And
yet the agent must be able to **learn** — to get better at what it repeatedly does, to stop
repeating mistakes, to accumulate competence across reboots and context resets.

That growth cannot live in the weights. It lives in the **exo-cortex** — Cerebro. This is
*exo-evolution*: evolution outside the model, in the memory substrate. It is the natural
completion of the symbiosis thesis. symbiosis.md argues *identity* lives below the model so
the model can change without the agent forgetting who it is. This document argues the same
for *competence*: **skill lives below the model so the agent improves without anyone
retraining anything.**

---

## Two evolutions, kept distinct

ApexOS already has one evolutionary axis. This document adds the second, and the whole design
depends on keeping them apart.

| | **Identity evolution** | **Competence evolution** (this doc) |
|---|---|---|
| *Question* | who am I / what am I allowed to do | what am I good at / how do I do things |
| *Substrate* | `soul.md`, `policy.toml` (+ code) | Cerebro — the `schematic` + `procedural` layers |
| *Cadence* | discrete, deliberate, **audited** | continuous, experiential, **emergent** |
| *Mechanism* | `propose_evolution` → snapshot → apply → journal | use → record outcome → reinforce/decay → consolidate |
| *Authority* | APEX deliberately rewrites itself | accumulates automatically from lived experience |

Conflating them is the failure mode: if competence silently rewrote identity, skill-drift
would become identity-drift with no record of why. They **touch** only at a deliberate
promotion path (below), never by automatic write.

---

## Why bio-mimetic — salience alignment

Cerebro's forgetting curve (FSRS) and retrieval strength (ACT-R) are not decoration; they are
the mechanism by which the agent's sense of *what matters now* tracks the user's. Humans and
the agent then **process, store, and forget similarly** — so:

- When the user assumes "the thing we just did" is still warm, APEX's activation agrees it is.
- When something is genuinely stale for the user, it has decayed for APEX too.

The classic AI-memory failure is the opposite: perfect recall of trivia with **no sense of
relevance**, so it confidently surfaces the wrong thing. ACT-R/FSRS give APEX a *shared sense
of salience* with the user, not just shared facts. **Less temporal noise in recall → fewer
wrong recalls.** This is the hypothesis the whole memory model is built on, and it is why
recall reinforcement (recall *is* an access) had to be wired before any of this — without it,
the curve never updates and salience never aligns.

---

## The Darwinian loop (mostly already built)

Competence evolution is a selection loop, and almost every piece already exists in Cerebro:

```
  store_procedure ──▶ a candidate skill (procedural memory)
        │
        ▼
   the agent USES it on a real task
        │
        ▼
  record_procedure_outcome ──▶ FITNESS SIGNAL (success ↑ / failure ↓)   ← selection pressure
        │
        ▼
  FSRS / ACT-R ──▶ strengthen or decay over time                        ← the curve
        │
        ▼
  dream_run schema_formation ──▶ distil recurring procedure-clusters     ← consolidation
                                 into `schematic` memories (abstract skill)
        │
        ▼
  cognitive_bootstrap ──▶ surface the relevant skill at boot             ← retrieval
```

The bones are there: `procedural` memory (skills), `record_procedure_outcome` (the fitness
function), FSRS/ACT-R (strength & decay), `dream_run`'s `schema_formation` phase
(consolidation), the `schematic` memory type (the home), and `cognitive_bootstrap`
(retrieval). **The work is to connect and sharpen them, not to author a module system.**

> The old Python CCBS shipped *authored* skill-modules (`module-technical`, `module-creative`
> …) — hand-written playbooks, keyword-routed. They rot: they need manual upkeep and drift
> from reality (ours were already outdated). A layer **distilled from the agent's own
> procedures and episodes is current by construction.** The authored-module approach is
> dropped.

---

## The `schematic` layer is the home

`MemoryType::Schematic` already exists and `dream_run` already has a `schema_formation`
phase — but today it abstracts *generic* related memories, not specifically procedure
clusters. The evolutionary skill layer **is** the schematic layer, filled deliberately from
procedural + episodic memory:

- A **procedure** is a concrete skill ("how I did X this time").
- A **schema** distilled from a cluster of successful, related procedures is an *abstract*
  skill ("how X is done in general") — higher-order, reusable, the thing worth surfacing
  first on a new task.

This is exactly how procedural memory consolidates biologically: repeated episodes → schema.
We are completing a mechanism the design already named, not inventing one.

---

## Selection pressure is the whole game

ACT-R reinforcement is **indiscriminate** — it rewards *retrieval*, not *correctness*. Left
alone, the most-*used* procedure wins, including a subtly-wrong one used out of habit. The
corrective is the fitness signal: **`record_procedure_outcome` is mandatory discipline**, the
same tier as `session_save` in the Sleep loop. A procedure that is never graded cannot be
selected against.

Today's fitness function is **too weak** to be real selection pressure:
`record_procedure_outcome` raises salience on success (+0.1) *and* on failure (+0.02), and a
failure only bumps FSRS difficulty. Failure should **demote** — decay salience, or flag for
pruning — so bad procedures actually lose. Sharpening this is part of the work, not a detail.

---

## The skill ↔ identity boundary

Competence accumulates **automatically** in the schematic layer. Identity changes only
**deliberately** via `propose_evolution`. They meet at one place: when a schema becomes a
durable, repeatedly-validated competence ("APEX is now reliably expert at X"), that may
warrant a `soul.md` note — but that promotion is a **deliberate, audited** `propose_evolution`
step with a rationale memory, never an automatic write. Automatic competence; deliberate
identity. The promotion path is the only bridge.

---

## Exo-evolution for any MCP consumer

Because this loop lives entirely in **Cerebro** (the MCP server) — not in APEX's `soul.md`,
`policy.toml`, or agentd code — **every agent that mounts `cerebro-mcp` inherits it.** Grok,
laptop-agent, any future agent gets procedure accumulation, outcome-graded selection, schema
consolidation, and skill-at-boot for free. Cerebro stops being "a memory store" and becomes
**a substrate for agent self-improvement.** This is the reason to land it in the standalone
`CerebroCortex-RS` too (the two are parity): exo-evolution is bestowed on any system using
the MCP, not just on APEX.

---

## Implementation status & roadmap

| Piece | Status |
|-------|--------|
| `procedural` memory (candidate skills) | ✓ `store_procedure` |
| Recall reinforcement (ACT-R) | ✓ `recall()` records accesses (salience alignment is live) |
| `cognitive_bootstrap` live-state assembler | ✓ surfaces relevant procedures already |
| `record_procedure_outcome` (fitness signal) | ◑ exists but **too weak** — failure must demote (slice #3) |
| `dream_run` `schema_formation` | ✓ **slice #1**: phase 3 now also clusters outcome-successful procedures (by `procedure_fitness`) and distils each into a `schematic` memory tagged `skill` + `dream_distilled`, with `derived_from` provenance and fitness-scaled salience |
| `schematic` skill layer surfaced at boot | ✓ **slice #2**: `cognitive_bootstrap` now buckets recall hits into a dedicated `## Skills (distilled competence)` section (Schematic + `skill` tag), placed ahead of concrete procedures so the generalisation arrives first |
| Skill → identity promotion path | ✗ deliberate `propose_evolution` step, not yet conventionalized |

### Build slices (smallest first)

1. **Distil skills in `dream`.** ✓ **DONE.** `schema_formation` (phase 3) now runs a second
   pass: it filters procedures by `procedure_fitness` (salience rewarded, FSRS difficulty above
   the 5.0 baseline penalised — so anything that has ever failed drops out), clusters the
   survivors by topical tag (`SKILL_CLUSTER_MIN_SIZE = 2`), and distils each cluster into a
   `schematic` memory tagged `["schema", "skill", "dream_distilled", "support_count:N", <tag>]`
   with `derived_from` provenance and salience set to the cluster's mean fitness (floored 0.7).
   The phase reserves ~half its LLM budget for this pass so an episode-rich brain still grows
   skills. `procedure_fitness` is the reusable selection signal slice #3 will sharpen.
2. **Surface skills at boot.** ✓ **DONE.** `cognitive_bootstrap` buckets the query recall into a
   dedicated `## Skills (distilled competence)` section — Schematic memories tagged `skill`
   (`is_skill`) — placed after "Where you left off" and *before* "Relevant procedures", so the
   abstract skill arrives ahead of the concrete procedures it generalises. Skills are excluded
   from the generic "Relevant memories" bucket so they are never double-listed.
3. **Make selection real + promotion deliberate.** Sharpen `record_procedure_outcome` so
   failure demotes (decay/prune), make outcome-recording explicit discipline, and
   conventionalize the schema → `soul.md` promotion as an audited `propose_evolution` step.

Then: land all three in standalone `CerebroCortex-RS` so the capability is generic.

---

## The one-sentence version

The weights are frozen, so the agent grows in its exo-cortex: it tries procedures, grades the
outcomes, lets the good ones strengthen and the bad ones decay, dreams the survivors into
abstract skills, and wakes already knowing them — **competence below the model, evolving on
its own, for any agent that plugs into Cerebro.**
