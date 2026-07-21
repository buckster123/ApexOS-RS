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

The original fitness function was **too weak** to be real selection pressure: it raised
salience on success (+0.1) *and* on failure (+0.02), so retrieval reinforced bad habits.
**Slice #3 fixed this:** failure now decays salience (−0.15, asymmetric so it bites harder
than a win rewards), and a procedure that decays to the `PRUNE_CANDIDATE_SALIENCE` floor is
flagged `prune_candidate` and retired by dream's pruning phase. Bad procedures now actually
lose. `procedure_fitness` (the dream-side selection signal) reads the sharpened salience and
difficulty directly, so the loop is coherent end to end.

But slice #3 is only **absolute** selection: a procedure is demoted when it *fails*, judged
against itself. The keystone of Darwinian selection is **competition** — alternatives for the
same task contending, the fitter displacing the weaker. Without it a subtly-worse procedure
survives indefinitely as long as it is rarely *failed*; it never loses a head-to-head to the
better one, and recall can keep picking it out of habit. The frontier extension closes this:
**niche competition** adds *relative* selection — a procedure can now lose simply by being
worse than a rival at the same task. See the dedicated section below.

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

## Niche competition — relative selection (the frontier)

The three foundational slices gave the loop variation, absolute selection, consolidation,
retrieval, and death. **Niche competition** adds the missing keystone: rivals contending for
the same task, the fittest displacing the rest. It is a new **algorithmic** dream phase
(`skill_competition`, no LLM budget) plus a real evidence base behind the fitness signal.

**The fitness ledger.** Inferring fitness from salience alone punishes novelty: a brand-new
procedure starts at salience 0.8 with no track record, so it would look "worse" than an
established one purely for being new. So `record_procedure_outcome` now also writes a
count-based ledger into the procedure's metadata —
`outcomes: {successes, failures}` — the genuine win/loss record (additive to the existing
salience/difficulty effects, which still drive ACT-R recall and the failure→prune path).

**Confidence-aware fitness.** Competition ranks procedures by the **Wilson score lower
bound** of their success rate, not the raw rate. A lucky `1/1` scores ≈0.21; a proven `8/2`
scores ≈0.49 — so a single fluke can't dominate a niche over a procedure with real evidence,
and more trials at the same rate rank strictly higher. This is the standard small-sample-safe
way to rank by success rate.

**The niche.** A niche is a **topical tag** shared by ≥2 *eligible* procedures (structural
markers like `procedure`/`skill`/`prune_candidate` are not niches — competition forms on
subject matter). Within a niche the highest-fitness procedure is the **champion** (tagged
`skill_champion`, which retrieval prefers via `retrieval_rank`); rivals trailing the champion's Wilson fitness
by more than a margin are **dominated** and take a bounded salience decay (gradual, not a
one-shot kill) plus a difficulty bump — so a persistent loser drifts to the prune floor, is
flagged `prune_candidate`, and is retired by the very next pruning phase (competition runs
right before pruning for exactly this).

**Two invariants keep it honest:**
- **Novelty is protected.** A procedure below a minimum number of graded outcomes is *exempt*
  — it never appears in a niche, so it can be neither champion nor demoted. Fresh procedures
  get exercised before selection can retire them; variation (the raw material) is preserved.
- **A champion of any niche is never demoted.** A procedure that wins one niche but loses
  another is still the best at *something* — it is marked champion, not penalised. Selection
  removes the also-rans, never the specialists.

Net effect: each task niche converges on its fittest procedure over successive dreams, the
also-rans fade, and retrieval (`find_relevant_procedures` / `cognitive_bootstrap`) surfaces
champions first — competence sharpening
itself with no weight update and no human in the loop. The selection core
(`compute_competition_verdicts`) is a pure function, unit-tested independently of any database.

## Implementation status & roadmap

| Piece | Status |
|-------|--------|
| `procedural` memory (candidate skills) | ✓ `store_procedure` |
| Recall reinforcement (ACT-R) | ✓ `recall()` records accesses (salience alignment is live) |
| `cognitive_bootstrap` live-state assembler | ✓ surfaces relevant procedures already |
| `record_procedure_outcome` (fitness signal) | ✓ **slice #3**: failure now DEMOTES (salience −0.15, asymmetric vs +0.1 success) and flags `prune_candidate` at the floor; dream's pruning phase retires flagged procedures. Real selection pressure |
| `dream_run` `schema_formation` | ✓ **slice #1**: phase 3 now also clusters outcome-successful procedures (by `procedure_fitness`) and distils each into a `schematic` memory tagged `skill` + `dream_distilled`, with `derived_from` provenance and fitness-scaled salience |
| Rediscovery reinforcement (dream extraction) | ✓ **colony C2 (welfare arc)**: `pattern_extraction` semantically dedups each extracted candidate against the whole store — a candidate ≥0.86 cosine (`REDISCOVERY_SIMILARITY`) to an existing *procedural* memory **reinforces** it (bounded +0.05 salience, capped 0.95, plus a `rediscovered_count` metadata ledger) instead of re-minting a fragment. Recurring evidence strengthens the survivor rather than spawning rivals-by-paraphrase; `PhaseResult.procedures_rediscovered` carries the novel/rediscovery split into the report + dream journal. FTS5-only Nano keeps the prefix dedup (BM25 isn't a similarity) |
| `schematic` skill layer surfaced at boot | ✓ **slice #2**: `cognitive_bootstrap` now buckets recall hits into a dedicated `## Skills (distilled competence)` section (Schematic + `skill` tag), placed ahead of concrete procedures so the generalisation arrives first |
| Fitness ledger (win/loss evidence base) | ✓ **slice #E1**: `record_procedure_outcome` writes `metadata.outcomes:{successes,failures}` — a real count-based record, additive to the salience/difficulty effects, so fitness no longer has to be inferred from salience alone |
| Niche competition (relative selection) | ✓ **slice #E1**: new algorithmic `skill_competition` dream phase — procedures sharing a topical tag contend; the Wilson-fittest is tagged `skill_champion`, dominated rivals decay toward the prune floor. Novelty-exempt below 2 graded uses; a champion of any niche is never demoted |
| Variation / mutation (fresh alternatives) | ✓ **slice #E2 + #E2b**: LLM `variation` dream phase. E2 *refines* underperformers (`dream_mutated`); E2b *merges* two strong distinct same-niche procedures into a hybrid (`dream_merged`). Variants inherit niche tags, link via `derived_from`, start un-graded → exempt until tried → re-compete. Pure `refine_candidates` + `merge_candidates`, unit-tested |
| Champion-aware retrieval | ✓ **E1 follow-up**: `find_relevant_procedures` (full-sorts) + `cognitive_bootstrap` (stable champion-promotion) surface the crowned procedure first, via the shared `retrieval_rank` (champion +1.0 band → Wilson → salience). Same metric as competition — one source of truth. Matcher since retooled (**colony C6**): normalized tag match + a semantic `brain.recall` stage + an object response — see the frontier bullet |
| Cross-agent skill flow | ✓ **E3**: shipped as `mesh_procedure_send` (colony-federation Slice 4, agentd's mesh layer): a procedure travels as a provenance-stamped copy; the origin's `metadata.outcomes` ledger rides the note as *context* (`track_record_note`), the receiver drops sender salience + the import starts an empty ledger — fitness re-earned per embodiment |
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
3. **Make selection real + promotion deliberate.** ◑ **Code DONE; one convention pending.**
   `record_procedure_outcome` now sharpens the fitness signal: failure decays salience
   (−0.15, floored) and bumps FSRS difficulty; once a procedure decays to the
   `PRUNE_CANDIDATE_SALIENCE` floor it is tagged `prune_candidate` and dream's pruning phase
   retires it. Success promotes (+0.1) and eases difficulty so a procedure can recover. The
   remaining parts are **conventions, not cerebro code**:
   (a) outcome-recording as mandatory Sleep-loop discipline — ✓ **seeded** via the #264
   full-coverage seed soul (`config/soul.md`: "after using one, `record_procedure_outcome`
   — outcomes feed the nightly darwin competition"); live nodes still adopt it via their
   own `propose_evolution`, per house rule;
   (b) conventionalize the schema → `soul.md` promotion path as an audited `propose_evolution`
   step with a rationale memory — still pending, the sole remaining convention (a proposal
   to surface to APEX, tracked separately).

#### Beyond the foundation — the frontier

The three slices above complete the *single-agent* Darwinian loop's foundation. The frontier
extends it with genuinely new mechanisms:

- **E1. Niche competition + fitness ledger.** ✓ **DONE.** `record_procedure_outcome` keeps a
  `metadata.outcomes:{successes,failures}` ledger; a new algorithmic `skill_competition` dream
  phase ranks same-niche procedures by their Wilson-lower-bound success rate, marks the
  champion (`skill_champion`), and decays dominated rivals toward the prune floor. Novelty is
  exempt below `COMPETITION_MIN_GRADED_USES` (2) graded uses; a champion of any niche is never
  demoted. Pure selection core (`compute_competition_verdicts`) is unit-tested without a DB.
  See the *Niche competition* section above.
- **E2. Variation / mutation.** ✓ **DONE (both operators).** The LLM `variation` dream phase
  generates fresh variants so competition has new alternatives, via two operators sharing the
  phase budget:
  - **Refinement** (E2): refine a genuinely-underperforming procedure (graded, ≥1 failure,
    Wilson fitness below `REFINE_FITNESS_CEILING`) into an improved variant (`dream_mutated`).
  - **Merge/recombination** (E2b): take the two fittest DISTINCT procedures in a niche, both
    above `MERGE_FITNESS_FLOOR`, and synthesise a hybrid combining their strengths
    (`dream_merged`) — crossover, not refinement.

  Every variant inherits its parent(s)' niche tags, links via `derived_from`, and starts
  *un-graded* so E1 treats it as novelty (exempt) until tried — then it competes against its
  parent(s) on its own record. **Lose / recombine → re-compete** is the variation→selection
  loop. Guards: one untested variant per parent + one pending merged child per niche (no
  pile-up), prefix dedup, distinct-content merge parents, bounded budget (refinement
  worst-fitness first, ~half reserved for merge); junk variants self-correct via selection.
  Pure selectors `refine_candidates` + `merge_candidates` are unit-tested. *(E2b also fixed a
  latent E2 bug: `dream_mutated`/`dream_merged` are now `is_structural_tag`, so role markers
  never form a spurious cross-task niche in competition or skill distillation.)*
- **E3. Cross-agent skill flow.** ✓ **DONE** — shipped as **`mesh_procedure_send`**
  (colony-federation Slice 4; agentd's mesh layer, not cerebro — see
  `docs/colony-federation.md`): a procedure travels the mesh as a provenance-stamped copy so
  agents don't each re-evolve from scratch. Skill semantics on the wire: the origin's
  `metadata.outcomes` ledger rides the note as *context* (pure `track_record_note`), while the
  receiver **drops sender salience** on procedural imports and the import starts an **empty
  ledger** — **fitness is re-earned per embodiment, never transferred**, so the receiving
  node's own Darwinian loop grades the import from its first graded use. The
  "exo-evolution for any MCP consumer" thesis made concrete across the mesh.
- **Champion-aware retrieval.** ✓ **DONE.** `find_relevant_procedures` and `cognitive_bootstrap`
  now surface the procedure competition crowned, ranking by the **same** fitness the dream phase
  uses (`retrieval_rank` = a `skill_champion` +1.0 band, then Wilson lower bound, ungraded
  falling back to salience) — single source of truth, no second drifting notion of "best".
  `find_relevant_procedures` (whose match stages are a binary relevance gate, no score)
  full-sorts by `retrieval_rank`; `cognitive_bootstrap` (recall hits already relevance-ordered)
  instead does a *stable* champion-promotion, so a champion only wins ties and fitness never
  overrides relevance for non-champions. Without this the matched set surfaced in arbitrary DB
  order. *(The matcher itself has since been retooled — colony C6, exact string equality was
  silently missing `mesh_recall` vs `mesh-recall` on the tool the souls mandate reaching for
  first: stage-1 exact matching is **normalized** (`norm_tag` — case/`-`/`_`-insensitive;
  `concepts` scan content too, not just metadata), a stage-2 **semantic** pass widens through
  the same `brain.recall` path (the explicit `query` arg, else tags+concepts as query text)
  only when exact matching leaves room, and the response is an **object** —
  `{procedures, matched:{exact,semantic}, procedures_in_scope, note}`, no longer a bare
  array — so an empty result says whether the matcher missed or nothing exists in scope.
  Ranking is unchanged: the whole matched set, whichever stage found it, full-sorts by
  `retrieval_rank`.)*

Then: land the foundation **and** the frontier mechanisms in standalone `CerebroCortex-RS`
so the capability is generic (the two cerebro trees are parity — see the cross-repo mirror).

---

## The one-sentence version

The weights are frozen, so the agent grows in its exo-cortex: it tries procedures, grades the
outcomes, lets the good ones strengthen and the bad ones decay, dreams the survivors into
abstract skills, and wakes already knowing them — **competence below the model, evolving on
its own, for any agent that plugs into Cerebro.**
