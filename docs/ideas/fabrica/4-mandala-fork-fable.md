# Mandala Mode — deep-nesting recursion for extreme long-horizon runs in ApexOS-RS

**Series:** follows `1-orchestration-loop-prd.md` and `2-repo-evaluation.md` (this directory). This doc designs the depth-N extension that the worker-tier evaluation deliberately deferred (§6.5: "depth-1 fan-out in v1, revisit with field data") — by first deconstructing what is *actually load-bearing* in the I-Ching, sacred geometry, and other 64-principle systems, then building the mode from those extracted invariants.

## 0. Stance — translate, don't transplant

The claim being tested: these symbolic systems encode structural stability that deep-nesting systems need. The claim survives deconstruction, with one clarification: the guarantees come from the **mathematics these traditions compressed** — totality over finite state spaces, well-founded orderings, conservation laws, low-discrepancy sequences, triangulated rigidity, self-similar contracts — not from the symbolism itself. But the symbolism is not decoration either, in *this* codebase specifically: ApexOS agents read and rewrite their own soul, docs, and policy. A memorable structural doctrine ("completion is unstable", "centrality first", "as above, so below") is dramatically more likely to survive self-evolution intact than a paragraph of prose invariants. The names are compression for a system that must retain its own laws across self-rewrites. So: the math is the guarantee, the doctrine is the transmission format — which is, not coincidentally, exactly what these traditions were.

## 1. The deconstruction — eight structural principles, extracted

Each entry: what the tradition holds → the underlying mathematics → the engineering invariant → the wedge it kills.

**P1 — Totality (I-Ching, 64 = 2⁶).** Six binary lines generate all 64 hexagrams; the changing-line system defines transitions from *every* state — the space is closed. Math: the 6-cube Q6, a total transition function on its vertices. Invariant: the supervision state of every parent↔child edge is a 6-bit word, and the review table is total over all 64 words by construction. Kills: **wedging-by-undefined-state** — the single most common deep-nesting failure is entering a joint condition the designers never enumerated. A closed 64-cell space has no such territory.

**P2 — Centrality (lines 2 and 5).** The I-Ching privileges the central line of each trigram as the load-bearing position. Math: precedence ordering over repair actions. Invariant: the two center lines are the two **conservation invariants** — the child's budget and the parent's capacity — and they are checked before all correctness concerns. Kills: **priority inversion** — burning effort verifying a subtree that has no budget to finish or no parent capacity to be integrated.

**P3 — Completion instability (hexagrams 63 → 64).** After Completion is the *least* stable hexagram; the sequence immediately rolls to Before Completion — perfect order is the moment before disorder, and the cycle must turn. Invariant: a **Done cell must be reaped within one review tick** — completed-but-unreaped subtrees are the canonical leak (held sessions, held slots, held context). Corollary, from 64's "one step from completion": a cell with a *single* broken line gets a **single-line remediation, never a subtree restart**. Kills: **zombie subtrees** and **restart-thrashing** (the failure mode where "almost done" work is repeatedly torn down and redone from scratch).

**P4 — Conservation (64 = 2⁶ = 4³ = 8²).** The same cardinality factors into three clean tree shapes: depth-6 binary, depth-3 quaternary, depth-2 octal — plus 64 codons, 64 squares, 64 kalās; the number keeps appearing because it is the smallest rich coincidence of power-of-two factorizations. Invariant: a fixed **geometry budget of 64 open cells per mandala** (one conductor's recursion manifold), conserved across shapes — the product of ring widths never exceeds it, at any depth, in any shape. Kills: **combinatorial explosion** — the budget is shape-independent, so no decomposition strategy can blow up the tree.

**P5 — Self-similarity ("as above, so below"; fractals).** Every scale obeys the same generative rule. Math: structural induction. Invariant: the **fractal cell contract** — every node at every depth (root conductor, sub-conductor, leaf worker) runs the *identical* contract: same state machine, same verdict tool, same budget-vector law, same output-artifact schema, same verify gate shape. Recursion lives in the *data* (task decomposition), never in the *mechanism*. Kills: **novel failure modes at depth k** — if depth 1 is correct and the contract is depth-invariant, correctness at all depths follows by induction, and every observability tool works at every scale.

**P6 — Triangulation (tetrahedron; Fuller).** The triangle is the only self-rigid polygon; the tetrahedron the minimal rigid solid; two-point connections are hinges. Invariant: every claim of completion is held by a **rigid verification triangle**: the producer's artifact + an independent mechanical gate (`cargo build && cargo test`, lint, a verify command) + the integrator's actual read of the artifact. A parent trusting a child's summary string is a hinge, and hinges fold under load. Kills: **trust-collapse cascades** — one hallucinated "done" propagating upward through N levels of summaries.

**P7 — Golden non-resonance (phyllotaxis; φ).** Sunflowers place florets at the golden angle because φ has the slowest-converging continued fraction — the most irrational number — so successive placements never resonate into overlapping rays. Math: Weyl equidistribution; low-discrepancy sequences. Invariant: sibling review ticks, heartbeats, and retries are scheduled on **golden-ratio offsets and Fibonacci backoff** — N siblings can never phase-lock. Kills: **livelock and herd storms** — livelock *is* resonance (retries synchronizing into lockstep); an irrational rotation number is its antidote. This is jittered backoff, but provably optimal jitter.

**P8 — Bindu convergence (Sri Yantra) + the torus.** Nine interlocking triangles resolve through concentric rings to a single point; every layer strictly approaches the bindu. Math: a well-founded measure — the termination proof. Invariant: every recursive admission must **strictly decrease a lexicographic budget vector** (depth, then cells, then steps, then deadline) — termination becomes a theorem, not a hope. The torus supplies the long-horizon complement: a cyclic run (plan→work→integrate→verify→re-plan) that *returns but never to the identical point* — quasi-periodic, because the persistent trace grew. Two consecutive epochs with identical state fingerprints = a true orbit = a declared loop. Kills: **unbounded recursion** and **plan oscillation** (the A→B→A→B re-planning loop that burns weeks of budget).

Two smaller extractions, already law in the tree: the **vesica piscis** (two circles sharing only their lens) is PB-2 — adjacent scopes share exactly the explicit carry, siblings exchange only through the parent's lens; and **Platonic duality** (faces↔vertices) is the primal work tree vs. the dual supervision tree — results flow up the primal, cancels flow down the dual, and the dual must stay connected even when primal nodes park (the parked worker's live supervision edge *is* the revive path). Mandala rule: reap primal and dual together, orphan neither — an orphaned dual is zombie supervision, an orphaned primal is unsupervised work.

## 2. The mode

**Mandala Mode** is an opt-in regime on the worker tier for runs too large for one fan-out ring: multi-day refactors, whole-workspace ports, sustained research programs. It replaces the v1 rule "workers never get the `task` tool" with **shape vouchers**: a worker becomes a *sub-conductor* only by receiving, at spawn, a voucher carrying its slice of the mandala's budget vector and ring geometry. The driver — code-disposes, as always — enforces the voucher at the tool-router seam; a worker without one still has no `task` tool at all (subtraction, the H6 move).

One mandala = one root conductor goal + its recursion manifold: ≤ 64 open cells, depth ≤ 6 (six lines, six rings — the ceiling is both doctrine and sanity: a depth-6 chain of 900 s step-stall ceilings is still humanly debuggable). The **geometry budget** (open cells) is distinct from the **thermal budget** (`AGENT D_WORKER_CAP`, running residency): a mandala may hold 64 open cells while only 4 run on a standard-tier node — the rest sit Idle or Parked (evicted, costless, revivable by one send). Two conserved quantities, two gates, deliberately.

## 3. The fractal cell contract (P5)

Every cell, at every depth, is exactly this — no exceptions, ever:

```
Cell = {
  states:   Queued | Running | Idle | Parked | Done | Failed | Cancelled   (worker tier, unchanged)
  budget:   BudgetVec { depth, cells, steps, deadline }                    (strictly < parent's on depth)
  carry:    { charter, context, files, plan_ref, skills }                  (the vesica lens — PB-2)
  verdict:  worker_report{continue|done|blocked|yield, summary, artifacts}
  output:   logs/agents/<id>.json + Cerebro episode                        (the evidence, P6's first vertex)
  gate:     a mechanical verify command                                    (P6's second vertex)
  review:   the hexagram loop of §5                                        (P1–P3)
}
```

A sub-conductor is a cell whose steps may call `task{shape, tasks[]}` within its voucher. That is the *entire* difference. Depth-invariance is what makes the mandala provable (§1 P5) and observable — the board renders any subtree with the same lane code that renders one worker.

## 4. Shapes — different geometries for different workflows

A shape is a declared factorization of the cell budget: per-ring width caps the driver enforces at admission. `task{shape:"quad"}` at the root fixes the mandala's lattice; sub-conductors inherit their ring's geometry.

| Shape | Factorization | Geometry | Workflow it fits | Why it's stable there |
|---|---|---|---|---|
| **Spine** | 2⁶ — width 2, depth ≤ 6 | Gray-code walk on Q6 | Bisection, root-cause hunts, chained binary decisions | Each step flips **one** assumption (Gray property) → minimal re-verification per move; complete coverage of the hypothesis cube without revisiting a state |
| **Quad** | 4³ — width 4, depth ≤ 3 | Tetrahedron | Balanced refactors and ports: workspace → 4 subsystems → 4 modules → 4 files | Four siblings are mutually adjacent (every vertex touches the other three) → **peer cross-check at integration** comes free; rigid by triangulation |
| **Fan** | 8² — width 8, depth ≤ 2 | Bagua ring | Embarrassingly parallel sweeps: lint batches, per-crate upgrades, doc regen (the repo's own 8-agent docs swarm was this shape, unnamed) | Maximal width, minimal depth → failures can't propagate downward; the trigram rubric (below) audits coverage before fan-out |
| **Spiral** | Fibonacci ring widths 1,1,2,3,5… capped | Phyllotaxis | Open-ended research/exploration where the decomposition is unknown upfront | Grows **where progress is**; new probes placed at golden-angle offsets in the search space (least-crowded direction); Fibonacci partial sums degrade gracefully when budget shrinks |
| **Funnel** | Decreasing widths, e.g. 9→4→1 | Sri Yantra | Synthesis and consolidation: N sources → themes → one report (the dream-digest shape, generalized) | Monotone fan-*in* toward the bindu → convergence is built into the lattice, not hoped for |

**The Bagua coverage rubric** (Fan shape, optional but genuinely useful): before an 8-lane fan-out, the conductor answers the eight trigram questions as a decomposition-completeness checklist — ☰ Heaven: architecture & invariants touched? ☷ Earth: docs & data grounding? ☳ Thunder: breaking changes & migrations? ☴ Wind: cross-cutting refactors that permeate? ☵ Water: dataflow & state? ☲ Fire: tests & verification? ☶ Mountain: stability, freeze, compatibility? ☱ Lake: interfaces, API, UX? A sweep that can't answer one of the eight has found its blind spot *before* spending the batch. This is the symbolic layer earning its keep as a checklist mnemonic.

## 5. The hexagram supervisor (P1–P3) — the anti-wedge core

Every parent↔child edge is reviewed on its (golden-offset) tick by sampling six binary observables — lower trigram is the child's condition, upper is the parent's, exactly the inner/outer doctrine:

| Line | Position | Observable | Broken means |
|---|---|---|---|
| 1 | child · initiating | **Progress** — new artifact/step delta since last review | stalled |
| 2 | child · **center** | **Budget** — child's BudgetVec strictly positive | exhausted |
| 3 | child · outgoing | **Verified** — latest claim passed its gate (gate-can't-run counts as broken: fail-closed) | unproven |
| 4 | parent · receiving | **Demand** — the current plan epoch still needs this result | orphaned intent |
| 5 | parent · **center** | **Capacity** — parent has an integration slot; not over ring cap; not itself parked | backpressure |
| 6 | parent · outgoing | **Horizon** — the mandala's epoch deadline not exceeded | out of time |

64 joint words; the review is a **total decision procedure with centrality precedence** — existence first, then the two centers (the conservation invariants), then correctness — returning exactly one single-line action:

```rust
/// Total over all 64 states by construction (P1): there is no joint condition
/// without a defined move — wedging-by-undefined-state cannot occur. Precedence
/// encodes the centrality doctrine (P2): existence, then the centers, then
/// correctness. One action per review; single-line change, NEVER a subtree
/// restart (P3, hexagram-64 rule) — restart-thrashing is the wedge this ends.
pub fn review(h: Hexagram) -> Action {
    if !h.line(4) { return Action::Prune;              } // no demand → cheapest fix first
    if !h.line(2) { return Action::RenewOrReap;        } // child center: renewal spends PARENT budget (P4)
    if !h.line(5) { return Action::ParkForIntegration; } // parent center: backpressure → evict, never lose
    if !h.line(3) { return Action::RunVerifyGate;      }
    if !h.line(1) { return Action::SteerProbe;         } // one re-anchoring send; 2× consecutive escalates to line 2
    if !h.line(6) { return Action::EpochRollover;      } // checkpoint + torus turn (§7)
    Action::None                                          // ䷀ all-yang: healthy drive
}
```

Named states with doctrine attached: **䷀ (all lines whole)** — full drive, no action. **䷁ (all lines broken)** — but note line 4 short-circuits: a fully-dark cell is pruned; the *legal* quiescent ground state is Parked-with-demand (dark except lines 4 and 6), costless, all state in JSONL, one send from life — the Receptive. **The completion rule (63)**: `Done` is the least stable state — the driver reaps it (deliver output path to parent, close episode, free primal+dual together) within one tick; a Done cell older than one tick is itself a defect the health check flags. **The near-completion rule (64)**: exactly one broken line gets exactly that line's remediation — the "almost done" trap is escaped by a step, not a demolition.

The postmortem falls out for free: the driver keeps a **hexagram census** (histogram of words observed per epoch). A run's diagnostic report is literally its reading — "this mandala spent 40% of reviews in line-5-broken" is a backpressure diagnosis no log-grep would surface as cleanly, and it persists into Cerebro as a dream-able memory of *how this shape ran*.

## 6. Scheduling (P7) — φ against the herd

```rust
/// Sibling i's review tick lands at (i · φ⁻¹ · period) mod period — a Weyl
/// sequence, maximally non-resonant: N siblings never phase-lock into a herd
/// of simultaneous reviews/retries/API bursts. Retries back off on Fibonacci
/// (1,1,2,3,5,8 × base, capped): the same non-resonance in time-depth.
/// Livelock is resonance; φ is the antidote — this is jittered backoff with
/// the provably optimal jitter.
const PHI_CONJ: f64 = 0.618_033_988_749_894_8;
pub fn review_offset(i: u32, period: Duration) -> Duration {
    period.mul_f64((i as f64 * PHI_CONJ).fract())
}
pub fn fib_backoff(attempt: u32, base: Duration, cap: Duration) -> Duration { /* … */ }
```

This applies to review ticks, worker retries, revive-after-park probes, and cross-node heartbeats in phase 2 — every recurring pulse in the mandala carries a golden offset, so nothing in the structure can resonate.

## 7. Epochs and the torus loop detector (P8)

Long-horizon runs are cyclic: plan → fan → integrate → verify → re-plan. The torus doctrine — return, but never to the identical point — becomes mechanical:

```rust
/// Epoch fingerprint: hash(objective_digest, sorted artifact digests,
/// hexagram census). Honest progress always shifts the trace (quasi-periodic
/// winding); two consecutive epochs with EQUAL fingerprints mean the run is
/// orbiting an identical state — a true loop, e.g. the A→B→A re-planning
/// oscillation. Action: park the mandala Blocked{"orbit detected"} and convene
/// review — a council over the census is the natural escalation (the four
/// personas already exist for exactly this kind of deliberation).
pub fn epoch_fingerprint(objective: &str, artifacts: &[Digest], census: &Census) -> u64 { /* … */ }
```

Epoch rollover (line-6 action) is a **checkpoint**: the mandala's full state (cells, budgets, census, fingerprints) persists to `mandalas.json`; a daemon restart — including the nightly self-update swap — reloads the whole structure Parked, revivable ring by ring, exactly the goal driver's restart discipline scaled up.

## 8. Budgets and admission (P4 + P8) — the load-bearing arithmetic

```rust
/// The bindu measure: admission requires strict decrease on depth and
/// non-increase elsewhere, all components positive. Termination is therefore
/// a THEOREM (well-founded descent), not a hope — the geometry budget cannot
/// be dodged by any decomposition strategy, in any shape (P4: conservation
/// across factorizations).
pub struct BudgetVec { pub depth: u8, pub cells: u8, pub steps: u16, pub deadline: u64 }

pub fn admissible(parent: &BudgetVec, child: &BudgetVec, ring_free: u8) -> bool {
    child.depth < parent.depth
        && child.cells <= ring_free
        && child.steps <= parent.steps
        && child.deadline <= parent.deadline
        && child.depth > 0 && child.steps > 0
}

/// Shape → per-ring width. Pure, table-driven, unit-tested; Π(width) ≤ 64.
pub fn ring_width(shape: &Shape, ring: u8) -> u8 { /* Spine:2 · Quad:4 · Fan:8 · Spiral:fib(ring) · Funnel:decl */ }
```

Renewal (the line-2 action's "renew" branch) spends the *parent's* remaining vector — budget flows down the tree, never appears from nowhere, and the mandala total is conserved. Parked cells release their thermal slot (per the worker-tier decision) but hold their geometry cell — evicted work is still *open* work, and the 64-cell law counts what's open.

## 9. What each classical wedge meets in this design

| Deep-nesting failure mode | Mechanism that ends it | Principle |
|---|---|---|
| Undefined joint states → frozen supervisor | Total 64-word review table | P1 |
| Unbounded recursion / fork bombs | Bindu measure + 64-cell conservation + depth 6 | P8, P4 |
| Zombie completed subtrees leaking slots/context | One-tick reap rule; primal+dual freed together | P3, duality |
| Restart-thrashing ("almost done" demolished repeatedly) | Single-line remediation, never subtree restart | P3 |
| Retry storms / livelock / thundering approvals | Golden offsets + Fibonacci backoff; ring caps + line-5 backpressure | P7, P2 |
| Trust-collapse (hallucinated "done" propagating up) | Verification triangle: artifact + gate + integrator's read | P6 |
| Plan oscillation (A→B→A across epochs) | Torus fingerprint → declared orbit → council | P8 |
| Context bleed across scopes at depth | Vesica carry — lens-only sharing, parent-mediated siblings | PB-2 |
| Novel failure modes appearing only at depth k | Depth-invariant cell contract → induction | P5 |
| Priority inversion (polishing a doomed subtree) | Centrality precedence: demand, then centers, then correctness | P2 |

## 10. Honest limits — what the geometry cannot do

It cannot make an indivisible task divisible; a wrong decomposition fans out wrongness in a very stable shape. It cannot repair a weak verify gate — a triangle with one soft vertex is still a hinge (P6 is only as rigid as `cargo test` is meaningful for the task). It cannot exceed the model's per-cell competence; the mandala multiplies capability, it does not create it. And 64/6 are ceilings chosen for coincidence-richness and debuggability, not revealed truth — the conservation *law* matters, the constant is tunable (a DGX-tier node could run a 256-cell, depth-8 lattice; the doctrine survives the retuning, which is rather the point of doctrine). What the tradition-layer genuinely contributes beyond the math: transmission-stable names for invariants inside a self-rewriting system, and the Bagua rubric as a pre-fan-out blind-spot audit.

## 11. Slices (house style, continuing W1/W2)

**M1a** — `Shape`, `BudgetVec`, `admissible`, `ring_width` as pure tested fns; `task{shape}` accepted at the root; geometry budget enforced; mandala object + `mandalas.json` (restart → whole-structure Parked).
**M1b** — the hexagram review loop replaces the worker driver's ad-hoc stall/TTL checks (they become lines 1 and 6); golden review offsets + Fibonacci backoff; hexagram census + board ring view.
**M1c** — shape vouchers: sub-conductors (workers holding `task` within budget); depth > 1 live; reap rule + dual-tree integrity checks in the health probe.
**M1d** — torus epochs: checkpoints, fingerprints, orbit detection → Blocked + council convocation over the census; census → Cerebro episode ("the reading").
**M2** — cross-node rings: outer rings placed on mesh peers (the GPU box runs the wide ring, the Pi holds the bindu); golden-offset heartbeats; remote-cell revive semantics.

## 12. Bottom line

The intuition is sound and, deconstructed, it is not mystical at all: these systems endured because they are **closed, conserved, self-similar, non-resonant, convergent combinatorial structures** — which is a checklist of exactly the properties a deep recursion engine needs and exactly the properties ad-hoc nesting lacks. Mandala Mode takes the worker tier already specified and adds five theorems' worth of structure: a total supervision space (nothing undefined), a well-founded descent (nothing unbounded), a conservation law (nothing explosive), an irrational rotation (nothing resonant), and a depth-invariant contract (nothing novel in the deep). The hexagrams, the golden angle, and the bindu are the memorable names those theorems wear so that an agent which rewrites itself nightly does not forget them.
