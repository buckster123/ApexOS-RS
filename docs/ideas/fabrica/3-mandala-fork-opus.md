# MANDALA MODE — a deep-nesting / recursion tier for extreme long-horizon runs

**Status:** design sketch v0.1 · **For:** ApexOS-RS, sitting above the worker tier (W1a–W1d) · **Date:** 2026-07-30

---

## 0. The honest verdict on the analogy, first

Your gut is right, and it's right for a reason worth stating precisely, because the precise reason is also the design.

**What transfers, rigorously:**

1. **Contraction.** A fractal is stable — bounded, terminating, self-similar — because each iteration is a *contraction mapping*: strictly smaller by a fixed ratio. That's the Banach fixed-point theorem, and it is exactly the termination proof a recursive agent tree needs. Not an analogy. The same theorem.
2. **Closed load paths.** What makes a tensegrity or a triangulated dome stable is that every force has a path back to ground, and the compression members never touch each other — they hang in a tension net. The software reading is exact: every spawned node's result must have a guaranteed consumer, and no two workers may hold anything the other needs. That single rule makes the wait-for graph a tree, and trees are acyclic, so deadlock becomes structurally impossible.
3. **Closure under composition, verified by enumeration.** There are exactly five Platonic solids. The transferable content isn't the solids — it's *finiteness*: a small closed catalog of legal forms, exhaustively checkable, rather than arbitrary graphs you can only test by sampling. This is the deepest one, and §3 turns it into a 64-cell table with a unit test.
4. **The 6-bit space is real.** A hexagram is two trigrams, 8 × 8 = 64. If you define eight primitive nesting shapes, then "shape here × shape permitted below" is *exactly* 64 combinations. And the I Ching's changing-line mechanic — mutate one line, get an adjacent hexagram — is a Hamming-distance-1 walk on a 6-cube, which is precisely the right rule for letting a run adapt its shape mid-flight without becoming illegible. The isomorphism holds at the mechanism level, not just the vibe level.

**What doesn't transfer, and would hurt if you shipped it:**

- **Hexagram meanings as dispatch semantics.** "Hexagram 24 is Return, therefore it's the retry pattern" is post-hoc pattern-matching. Build branch logic on it and in six months nobody — including the agent — can explain why a run took a path. The structure is load-bearing; the poetry is not.
- **The golden ratio specifically.** φ⁻¹ ≈ 0.618 is a perfectly valid contraction ratio (Σφ⁻ⁿ = 2.618, bounded, fine). But nothing about φ makes it better here, and r = 0.5 gives a cleaner bound (total work ≤ 2× root) that you can state in one line in a doc. Use 0.5; note φ as taste.
- **Numerological 64.** The real reason 64 is the right size: it's 8², where 8 is what you get from three orthogonal risk axes, and 64 is small enough to enumerate exhaustively in a unit test while large enough to cover real workflow variety. Four axes would give 16 shapes and 256 cells — still machine-checkable, no longer human-reasonable. **64 is a sweet spot for joint human + machine verification.** That's a better reason than a cosmic one, and it's the one that survives code review.

So: keep the skeleton, drop the ornament. What follows uses only the four transferable properties.

---

## 1. The problem this tier actually solves

The worker evaluation deliberately capped fan-out at depth 1 (§6.5: *"Workers do not get the `task` tool — no nested batches, no recursion hazard"*). That was correct for v1 and is the wrong permanent answer: a genuine long-horizon coding run (port a subsystem, land a cross-cutting refactor, drive a multi-day migration) is inherently hierarchical. This tier lifts the cap **without** re-opening the seven ways deep agent recursion actually dies:

| # | Failure mode | What it looks like here |
|---|---|---|
| F1 | **Budget explosion** | breadth 8 at depth 5 = 32,768 workers; the node melts |
| F2 | **Livelock** | propose → critique → revise → propose forever; budget burns, nothing converges |
| F3 | **Deadlock** | parent's barrier waits on a child waiting on an approval slot the parent holds |
| F4 | **Orphaning** | a parent parks or dies; children run on, results land nowhere |
| F5 | **Context dilution** | by depth 4 the objective has been paraphrased four times and no longer means anything |
| F6 | **Verification collapse** | depth-4 declares success against a local criterion nobody traced to the root goal |
| F7 | **Restart mid-tree** | the daemon self-updates its own binary at 03:00 — a deep run **will** be interrupted, by design |

F7 deserves emphasis: in most systems a restart mid-run is an edge case. In ApexOS it is a *scheduled feature*. Anything long-horizon here must be reconstructable from disk, or it is not long-horizon.

---

## 2. Eight primitives — three orthogonal risk axes

Every nesting shape is three bits. Each bit switches on exactly one unbounded dimension, and therefore mandates exactly one guard. That correspondence is the whole safety argument.

| Bit | Axis | Set means | Unbounded dimension | **Mandatory guard** |
|---|---|---|---|---|
| **B** | breadth | many children in flight | fan-out width | breadth cap; product across levels ≤ node cap |
| **R** | recurrence | the node re-enters (laps) | number of laps | a **measure**: evidence-computed integer, strictly decreasing |
| **J** | join | barrier: collect + verify before returning | wait time | barrier timeout < remaining parent budget |

Two guards are **universal** (all eight shapes, always): **contraction** (child budget = r × parent, r ≤ 0.5) and **floor** (below the floor, any shape collapses to a leaf).

The eight shapes, in Hamming weight order — which is also risk order, and also the ship order (§9):

```
weight 0
 000  SPINE      ●──●──●──●        serial refinement; zero children = LEAF (same shape, degenerate)
                                    use: bisect a bug, iterative deepening, a linear port
                                    terminates by: contraction alone

weight 1
 001  GATE       ●──●──▣           one child, then a verify barrier before returning
                                    use: build→test→review checkpoints (self-update's own shape)
                                    guard: barrier timeout

 010  SPIRAL     ●──●──●           laps, each narrower and cheaper than the last
                  ╰──↺──╯          use: converge on a fuzzy target (a design doc, an API surface)
                                    guard: measure

 100  FAN        ●──┬─●            N independent children, results stream back, no barrier
                    ├─●            use: the W1a shape — independent parallel edits
                    └─●            guard: breadth cap

weight 2
 011  FORGE      ●──▣──↺          lap → verify → lap; adversarial refinement with a hard gate
                                    use: fix-until-green; the council/critique loop
                                    guards: measure + barrier timeout

 101  DIAMOND    ●──┬─●─┬──▣      N children, collect ALL, integrate + verify here
                    ├─●─┤          use: split-then-merge; the classic coding fan-out
                    └─●─┘          guards: breadth cap + barrier timeout

 110  SWARM      ●──┬─●──↺        waves of parallel work, re-entering, no barrier
                    └─●──↺         use: broad exploratory search
                                    guards: breadth cap + measure  ← the explosive corner

weight 3
 111  MANDALA    ●──┬─●─┬──▣──↺   waves of parallel work, each wave closed by a verify barrier
                    ├─●─┤          use: multi-day migrations; the full long-horizon form
                    └─●─┘          guards: all three
```

Three properties fall out that are worth naming:

- **LEAF is not a special case.** It is SPINE with zero children. The contraction rule *produces* leaves at the budget floor, so termination needs no separate branch in the composition table.
- **Hamming weight = number of armed guards.** A shape is well-formed iff every set bit has its guard configured. That's a one-line validity check, and it's total.
- **The danger gradient is legible.** 000 is safest, 111 is most capable and most constrained. Nobody has to remember which shapes are risky; count the bits.

---

## 3. The 64-cell table — the actual centerpiece

A **hexagram** here is an ordered pair: upper trigram = this node's shape, lower trigram = the shape its children are permitted to take. 8 × 8 = 64 legal-ness questions, and *you can answer all of them and test all of them*.

That is the real content of your intuition. Not that 64 is significant — that **a recursion system is provably non-wedging exactly when its composition table is closed, finite, and exhaustively verified.** The I Ching is a 64-cell closed composition table over 8 primitives. So is this.

### The rules that generate the table

- **C1 · Contraction (universal).** child.budget = ⌊r × parent.budget⌋, r ≤ 0.5. Total work over any subtree ≤ budget/(1−r) ≤ 2× root. Below the floor → LEAF.
- **C2 · No nested recurrence.** R over R is **forbidden**. An inner loop can lap forever while the outer measure never moves — the classic nested-loop livelock, and the single most common way these systems die. (v2 may permit it only with a *proven measure refinement*: inner decrease must imply outer decrease. Not v1.)
- **C3 · Breadth product.** B over B is **conditional**: Π(breadth caps down the path) ≤ node worker cap. A DIAMOND of 4 over FANs of 4 is 16 concurrent workers — legal on a GPU tier, forbidden on a Pi.
- **C4 · Barrier acyclicity (tensegrity).** A barrier may wait **only on its own descendants**, never on siblings, never on an ancestor. The wait-for graph is then a tree → acyclic → **F3 deadlock is structurally impossible**, not merely unlikely. Every J bit additionally carries a timeout < remaining parent budget.
- **C5 · Floor collapse (universal).** Any node at the budget floor executes as LEAF regardless of declared shape. This is the termination backstop that makes every other rule safe to get slightly wrong.

### The resulting partition

| Class | Cells | Which |
|---|---|---|
| **Forbidden** | **16** | R-over-R: upper ∈ {SPIRAL, FORGE, SWARM, MANDALA} × lower ∈ {SPIRAL, FORGE, SWARM, MANDALA} |
| **Conditional** | **12** | B-over-B not already forbidden — admitted only if the breadth product fits |
| **Free** | **36** | legal under the universal guards (C1, C4-timeout, C5) alone |

Sixteen forbidden cells *are* the wedge patterns, enumerated and named rather than discovered in production at 4 a.m. And the whole table is one exhaustive test:

```rust
#[test]
fn composition_table_is_total_and_closed() {
    let mut free = 0; let mut cond = 0; let mut forbidden = 0;
    for u in 0..8u8 { for l in 0..8u8 {
        match Shape(u).may_nest(Shape(l)) {          // total function, no panics
            Legality::Free        => free += 1,
            Legality::Conditional => cond += 1,
            Legality::Forbidden   => forbidden += 1,
        }
    }}
    assert_eq!((free, cond, forbidden), (36, 12, 16));
}
```

Sixty-four assertions, exhaustive, run in microseconds, and they encode the entire stability argument for the recursion tier. That is what "these structures are stable by nature" cashes out to in Rust.

### Changing lines — adaptation without illegibility

A run may mutate its descriptor **one bit at a time**, re-validating against the table. A DIAMOND (101) whose barrier keeps timing out drops J → becomes FAN (100) and streams results instead of blocking. A FAN whose outputs turn out to conflict gains J → DIAMOND, and the next wave integrates. A SPIRAL that stops converging drops R → SPINE, does one final pass, and returns.

Because every intermediate is a *named, tabled, tested* shape, the run can adapt continuously and still be explained in one sentence at every instant. This is the changing-line mechanic doing real work: a Hamming-1 walk on the 6-cube, where every vertex is a verified configuration.

---

## 4. The axis — invariant propagation (kills F5, F6)

In a mandala the vertical axis is the one element that doesn't transform under the symmetry group. Here that's literal:

The root writes an **invariant** once — objective, definition-of-done, and the *verify command* (e.g. `cargo test -p apexos-core`) — to a content-addressed file. Every descendant, at every depth, carries **a reference to those exact bytes**, never a paraphrase. A depth-4 worker reads what the root wrote.

Two consequences: the telephone game (F5) becomes impossible, because no level restates the goal; and every barrier at every depth runs the *root's* verify command, not a local proxy for it, so local success can't diverge from global progress (F6). Charters still contract — each level's *task* narrows — but the invariant is rigid. Contracting task, rigid axis.

This also makes reparenting safe (§6): a child whose parent vanished still holds a valid, unmutated contract with the root.

---

## 5. The measure — well-foundedness (kills F2)

Any R-bit shape must declare a **measure**: a non-negative integer computed by *running a command*, never asserted by the model.

```
measure ∈ { failing tests, clippy warnings, unmerged conflict count,
            open subtask count, |diff| vs target, unresolved TODO markers }
```

The ring closes only while the measure strictly decreases. If it fails to decrease for K consecutive laps (default K = 2), the ring **breaks** and the node escalates to its parent with the measure history attached. Not a retry cap — a *variant*, the standard termination-proof device, which is why a well-formed SPIRAL cannot livelock rather than merely tending not to.

The geometric reading is exact, too: a ring under tension is only stable if something takes up slack each lap. The measure is the slack.

---

## 6. Address space — the fractal doing real work (kills F4, F7)

Every node's identity **is** its position: `0.3.1.2` = root → 3rd child → 1st → 2nd. From that one string you get, for free:

- **On-disk layout.** `logs/worktrees/<root_id>/0.3.1.2.json` — descriptor, parent, budget vector, measure history, state, artifact path, invariant hash.
- **Restart recovery (F7).** The tree is rebuilt by scanning the directory. **The filesystem is the tree.** No in-memory structure is authoritative — which is the only posture compatible with a daemon that swaps its own binary mid-run. This is the `goals.json` / `workers.json` pattern, generalized to a hierarchy.
- **Reparenting (F4).** On reload, a node whose parent is missing attaches to its nearest living ancestor, by string prefix. Safe precisely because of §4: its contract is with the root, not with the parent it lost.
- **Git worktree names.** `apex/w/0.3.1.2` — see below.
- **Self-similar subtrees.** Any subtree rebuilds independently of its siblings. That's the genuine fractal property: the reconstruction algorithm is scale-free.

**Closed load path (F4, properly).** A node reaches `Done` only when its parent has *read* its artifact. Produced-but-unconsumed is a distinct terminal state, `Orphaned`, and it's a bug class you can query for. Forces must return to ground.

### Git worktrees — your word, taken literally

The tools crate has `git_branch`, `git_checkout`, `git_merge`, `git_commit` — but **no `git_worktree`**. For parallel coding recursion that's the missing primitive, and it's a small addition with outsized payoff:

Every B-bit node gives each child its own **git worktree on a branch named by address**. Parallel workers then physically cannot collide on a working tree — the tensegrity rule (§3, C4) enforced by the filesystem rather than by convention. A J-bit barrier's job becomes concrete and honest: **merge the children's branches, resolve conflicts, run the root's verify command, commit.** Conflict resolution stops being an exception and becomes the barrier's declared work.

That also gives a deep run a genuinely inspectable artifact: `git log --graph` of a MANDALA run *is* the structure diagram.

---

## 7. Budget as a 3-vector — the multi-dimensionality, concretely

```rust
/// Every spawn debits all three. A node may spawn only if all three afford it —
/// which is what makes F1 impossible rather than unlikely.
#[derive(Clone, Copy, Serialize, Deserialize)]
pub struct Budget {
    pub work:    u32,  // steps/tokens; contracts by r at each level (C1)
    pub depth:   u8,   // decrements by 1; hard ceiling (existing subagents.max_depth precedent)
    pub breadth: u8,   // this node's fan-out cap; product down the path bounded by C3
}

impl Budget {
    pub const FLOOR: u32 = /* one useful step */;
    /// Below the floor, the node executes as LEAF regardless of declared shape (C5).
    pub fn spent_out(&self) -> bool { self.work < Self::FLOOR || self.depth == 0 }
    pub fn child(&self, r_num: u32, r_den: u32) -> Self {
        Self { work: self.work * r_num / r_den, depth: self.depth - 1, breadth: self.breadth }
    }
}
```

Three independent bounds, each debited on every spawn. Total work over any subtree ≤ 2× root at r = 0.5, provable in one line and enforceable in one branch.

---

## 8. Core types

```rust
/// A nesting shape: three orthogonal risk bits. Hamming weight = number of
/// guards that must be armed for the shape to be well-formed.
#[derive(Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Shape(pub u8);   // bit0 = J (join), bit1 = R (recur), bit2 = B (breadth)

impl Shape {
    pub const SPINE: Self = Shape(0b000);  pub const GATE:    Self = Shape(0b001);
    pub const SPIRAL: Self = Shape(0b010); pub const FORGE:   Self = Shape(0b011);
    pub const FAN:   Self = Shape(0b100);  pub const DIAMOND: Self = Shape(0b101);
    pub const SWARM: Self = Shape(0b110);  pub const MANDALA: Self = Shape(0b111);

    pub fn joins(self)  -> bool { self.0 & 0b001 != 0 }
    pub fn recurs(self) -> bool { self.0 & 0b010 != 0 }
    pub fn branches(self)-> bool { self.0 & 0b100 != 0 }
    pub fn risk(self)   -> u32  { self.0.count_ones() }

    /// Well-formedness: every set bit has its guard configured (§2).
    pub fn guards_armed(self, g: &Guards) -> bool {
        (!self.branches() || g.breadth_cap.is_some())
            && (!self.recurs() || g.measure.is_some())
            && (!self.joins()  || g.barrier_timeout.is_some())
    }

    /// The 64-cell table (§3). TOTAL — every pair answered, no panics.
    pub fn may_nest(self, child: Shape) -> Legality {
        if self.recurs() && child.recurs()       { return Legality::Forbidden; }   // C2
        if self.branches() && child.branches()   { return Legality::Conditional; } // C3
        Legality::Free
    }
}

/// The vertical axis (§4): written once at the root, referenced by hash at every
/// depth, never paraphrased. `verify` is the root's own command — every barrier,
/// at every level, runs THIS.
#[derive(Serialize, Deserialize)]
pub struct Invariant { pub objective: String, pub done_when: String,
                       pub verify: String, pub hash: String }

/// Well-foundedness for R shapes (§5). Evidence-computed, never model-asserted.
pub struct Measure { pub command: String, pub history: Vec<u32>, pub stall_k: u8 }
impl Measure {
    /// The ring stays closed only while this holds.
    pub fn still_converging(&self) -> bool {
        let h = &self.history; let k = self.stall_k as usize;
        h.len() <= k || h[h.len()-1-k..].windows(2).any(|w| w[1] < w[0])
    }
}

/// Position IS identity (§6): "0.3.1.2". Prefix relations give parent, ancestry,
/// and — after a restart — reparenting, by string operations alone.
pub struct Addr(pub String);
impl Addr {
    pub fn parent(&self) -> Option<Addr> { /* strip last segment */ }
    pub fn depth(&self)  -> u8           { /* count segments */ }
    pub fn branch(&self) -> String       { format!("apex/w/{}", self.0) }
    pub fn path(&self, root: u64) -> PathBuf { /* logs/worktrees/<root>/<addr>.json */ }
}
```

The driver is `worker.rs`'s select-loop with three additions: validate `may_nest` + `guards_armed` + `Budget::spent_out` **before** admitting a spawn; evaluate measures on each lap and break stalled rings; and run barriers as descendant-only waits with timeouts. No new concurrency machinery — the worker tier's admission gate already bounds live sessions; this bounds their *shape*.

---

## 9. Shape selection — which form for which workflow

| Workflow | Shape | Why |
|---|---|---|
| Track down a regression | **SPINE** | serial narrowing; bisect is a contraction by construction |
| Fix N independent lint sites | **FAN** | no integration needed; stream results |
| Land a cross-cutting refactor | **DIAMOND** | edits must merge; the barrier *is* the conflict resolution |
| Make the suite green | **FORGE** | lap → verify → lap; measure = failing test count |
| Draft-and-refine a design doc | **SPIRAL** | narrowing scope per lap; measure = open questions |
| Port a subsystem (multi-day) | **MANDALA** | waves of parallel work, each closed by a verify barrier |
| Explore an unfamiliar codebase | **SWARM** → collapse to FAN | broad first pass, then drop R once the map exists |
| Ship a self-update | **GATE** | build → test → adversarial review; already this shape today |

Note the last row: the self-update pipeline is already a GATE, and the council is already a FORGE. Two of the eight primitives are in production; this design names them and makes them composable.

---

## 10. Failure modes → mechanisms

| | Mechanism | Guarantee |
|---|---|---|
| F1 explosion | contraction (C1) + breadth product (C3) + depth debit | total ≤ 2× root; provable |
| F2 livelock | measure, evidence-computed, strictly decreasing; R-over-R forbidden (C2) | well-founded; rings cannot spin |
| F3 deadlock | barriers wait only on descendants (C4) + timeouts | wait-for graph is a tree → acyclic |
| F4 orphaning | address-space reconstruction + reparent-by-prefix + `Orphaned` terminal state | no dangling results, ever |
| F5 dilution | invariant by content hash, never paraphrased (§4) | depth-4 reads the root's bytes |
| F6 verify collapse | every barrier runs the *root's* verify command | local done ⇒ global evidence |
| F7 restart | filesystem is the tree (§6) | survives the daemon swapping its own binary |

---

## 11. Slice plan — ship in Hamming-weight order

The rollout ordering falls out of the design, which is a good sign for it:

- **N1 · weight ≤ 0** — `Shape`, `Addr`, `Budget`, the on-disk tree, reconstruction + reparenting, the invariant file. SPINE/LEAF only. Depth > 1 with *zero* new concurrency: pure recursion safety, testable in isolation.
- **N2 · weight 1** — add J and B: GATE, FAN, DIAMOND(-as-GATE-over-FAN). **`git_worktree` tool** + address-named branches. Barriers with timeouts, descendant-only.
- **N3 · weight 2** — add R: SPIRAL, FORGE. Measures, stall detection, ring-breaking. This is where long-horizon convergence actually arrives.
- **N4 · weight 3** — SWARM, MANDALA, the full 64-cell table + its exhaustive test, changing-line adaptation, the board rendering the live tree by address.
- **N5** — mesh: a subtree's root routed to a peer (W2). The address space already namespaces cleanly across nodes.

Each slice adds one bit, one guard, and one class of test. If a slice destabilizes, the previous shapes are unaffected — the composition table tells you exactly which cells the new bit opened.

---

## 12. What I'd cut, explicitly

So the design survives review by someone who doesn't share the frame:

- Hexagram names/meanings anywhere in code or logs. Ship `Shape::MANDALA` (descriptive) — never `Hexagram(47)`.
- The King Wen ordering: no engineering content here.
- φ as the contraction ratio: use 0.5 for the clean 2× bound; note φ⁻¹ as an aesthetic variant that still converges.
- Any claim that 64 is *necessary*. It's 8², where 8 = 2³ risk axes, and it's the largest table two people can still argue about at a whiteboard. That's the defensible reason, and it's enough.

---

## 13. Bottom line

Your instinct was tracking something specific: these systems feel stable because they are **closed, finite, self-similar under contraction, and exhaustively enumerable** — and those four properties are, exactly, what makes a recursion scheme provably non-wedging. Sacred geometry isn't the mechanism; it's a very old, very well-compressed *encoding* of the mechanism, which is why it reads as a blueprint.

The concrete payload: three risk bits → eight shapes → a 64-cell composition table with 16 named forbidden patterns → one exhaustive unit test that *is* the stability argument. Plus a rigid axis so depth can't dilute meaning, a well-founded measure so rings can't spin, descendant-only barriers so nothing can deadlock, and an address space that makes the whole tree reconstructable from disk after the daemon rewrites itself at 3 a.m.

Ship it one bit at a time.
