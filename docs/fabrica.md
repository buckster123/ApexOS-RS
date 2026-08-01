# Fabrica — the workshop charter

> **Fabrica** (Latin: *workshop* — the root of "fabricate", and of "forge" in
> the Romance languages) is ApexOS-RS's work-orchestration surface: the
> renamed workboard, the goal driver, the **worker tier** (parallel fan-out),
> **Mandala Mode** (deep nesting), and the **code regime** — one roof.
> Locked 2026-07-31 by André + FORGE from a four-document brainstorm
> (preserved in `docs/ideas/fabrica/`). This file is the charter: decisions
> are LOCKED unless a field finding reopens them; deep rationale lives in the
> idea docs, not re-argued here.

**Provenance.** `1-orchestration-loop-prd.md` — the conductor/child loop,
reverse-engineered by incognito instances from a production harness's own
orchestration shape (batch fan-out, admission gate, parked-but-revivable
children, durable evidence): a daily-battle-tested pattern, not speculation.
`2-repo-evaluation.md` — that PRD mapped onto this tree with receipts; its
four amendments are adopted wholesale. `3-…-opus.md` + `4-…-fable.md` — two
forked deep-nesting designs, **fused** here: they type different layers (Opus
the *node*, Fable the *tree*) and compose rather than compete. Method note
for future brainstorms: forking the same prompt across models produced
complementary designs, not redundant ones.

---

## The three tiers

| Tier | What it is | Status |
|---|---|---|
| **Goal driver** | One session, deterministic serial loop (`goal.rs`) | Shipped |
| **Worker tier** (W) | One conductor goal fans a batch to N persistent, parkable, evidence-leaving child sessions | W1 shipped (`#306`–`#311`); W2 mesh shipped (`#318`) |
| **Mandala Mode** (M) | Workers that may themselves conduct — depth-N recursion under conservation laws | M1a–M2 shipped (`#312`–`#321`) — the ladder is complete |

The code regime rides on all three; Fabrica the app surfaces all of it.

---

## Locked decisions — worker tier (W)

- **Workers are goals with parents, not persistent spawns.** One additive
  driver (`agentd/worker.rs`, the `goal.rs` select-loop pattern), a new
  `WORKER_SESSION_BASE` id class (persisted — unlike spawns, whose
  ephemerality stays load-bearing and untouched). `agent_spawn` keeps its
  scope: blocking cross-node lookups inside a step.
- **The fan-out tool is `task_fanout`** (bare `task` collides with
  `schedule_task` in the model's tool namespace). One call → N workers.
  `mode:"async"` is the default and the coding path; inline is documented as
  short-batch-only. Anti-pattern line rides in goal directives: parallel work
  goes through ONE `task_fanout` batch, never a spawn-then-wait chain (PB-1
  enforced economically + by authorship; optional soft breaker ≥3 sequential
  spawns → board nudge).
- **Carry is spawn-time-only and explicit** (PB-2, already structural):
  `{prompt, system?, files, plan_ref, skills}` — `skills` = Cerebro procedure
  ids, `plan_ref` = a Cerebro intention/episode id. No parent history, ever.
  Default system = the H6 minimal task charter; `inherit_soul:true` stays the
  explicit widening. *(As-shipped: task items carry `{prompt, model, measure,
  voucher}`; the `system`/`files`/`plan_ref`/`skills` carry is still open —
  the no-parent-history law is what's structural.)*
- **Admission is tier-aware**: `AGENTD_WORKER_CAP`, defaults from the
  embodiment hardware tier (micro 2 · standard 4 · gpu 8), env-overridable,
  floor 1. FIFO overflow queue; freed slots pull from it. The turn-engine
  `Semaphore(16)` still gates provider calls underneath — document cap ≤
  turn-sem as the sane config. *(W1a mapping note: the code's tier strings are
  `nano/micro/standard/pro` — "gpu" is the `pro` tier, a RAM≥8GB threshold with
  no GPU probe; `nano`/`unknown` floor to 1.)*
- **Idle/Parked = memory residency.** Idle: yielded, history in RAM, wake
  free. Parked: TTL-evicted, `sessions/<id>.jsonl` is truth; revive reloads
  through `repair_history` (the boot path's code). **A parked worker releases
  its thermal slot.** Restart → Running workers go Parked, never lost.
- **Revive-on-send is the only Parked→Running edge** (PB-3, structural): the
  one code path fires off `send_to_agent → AgentMessage → UserPrompt`.
  *(A `worker_resume` sugar tool was sketched but never built — sends are
  the whole interface.)*
- **Worker verdicts**: `worker_report{status: continue|done|blocked|yield,
  summary?, artifacts?[]}` — `goal_step`'s mirror; `yield` = go Idle awaiting
  input; `done` requires a summary and may declare workspace-confined
  artifact paths.
- **The evidence rule** (the PRD's best idea, kept whole): every terminal
  worker writes `<log dir>/agents/<worker_id>.json` (default `events/`) + a Cerebro episode. Batch
  reports hand the conductor **paths, not payloads** — integration and verify
  must actually read the artifacts. Trusting a summary string is a hinge;
  hinges fold (the verification triangle: artifact + mechanical gate +
  integrator's read).
- **`TaskBatchDone` fires on all-terminal — bounded by `batch_deadline_s`.**
  The conductor's stall clock pauses in the `AwaitingBatch` posture (the
  dormant `GoalPosture` from the goal-driver design, activated), so without a
  deadline one forever-parked worker wedges the conductor forever. On
  deadline: the batch reports with stragglers marked `timed_out` — still
  revivable — and the conductor regains control. Failed workers are
  integration data for the fix batch, not exceptions.
- **Approval hygiene**: batch-scoped yolo as explicit `task_fanout{yolo:
  "inherit"}` — workers inherit the parent goal's yolo bit and never more;
  default off. Approvals **batch to one board card per batch** with a count —
  never N cards (the digest principle).
- **Cache law, one level down**: worker charters are byte-stable across the
  worker's steps — no volatile text — so each worker's small prefix caches
  across its own loop. The conductor holds the one big context. (Same law as
  the soul; see the prompt-cache gotcha.)

## Locked decisions — Mandala Mode (M)

The fusion: **Opus typed the node, Fable typed the tree.** Both 64s survive
because they answer different questions — one static, one runtime.

- **Cell form = three risk bits** (Opus): B(readth) / R(ecurrence) / J(oin).
  Each set bit arms one unbounded dimension and therefore **mandates one
  guard** (breadth cap / measure / barrier timeout). Well-formed = every set
  bit's guard configured — a total one-line check. LEAF is SPINE at the
  budget floor, not a special case; below the floor any form collapses to
  LEAF (the termination backstop).
- **Static legality = the composition table** (Opus): form-over-form, 64
  cells, partitioned 36 free / 12 conditional (B-over-B admitted only if the
  breadth *product* down the path fits the node cap) / 16 forbidden
  (R-over-R — nested recurrence is the classic livelock; v2 may revisit only
  with a proven measure refinement). The table is a total function plus ONE
  exhaustive unit test asserting `(36, 12, 16)` — the stability argument, in
  microseconds. Adaptation = changing-line: a run mutates its form **one bit
  at a time**, re-validated against the table, so every intermediate is a
  named, tested configuration.
- **Lattice presets** (Fable's shapes, renamed — "shape" was doing double
  duty): declared factorizations of the geometry budget into ring widths.
  **Spine** 2⁶ (bisection) · **Quad** 4³ (balanced refactors; four mutually
  adjacent siblings → free peer cross-check) · **Fan** 8² (embarrassingly
  parallel sweeps) · **Spiral** Fibonacci widths (unknown decompositions;
  grow where progress is) · **Funnel** decreasing (synthesis toward one
  report). `ring_width()` is pure and table-driven.
- **Two conserved quantities, two gates** (Fable — the Pi-critical split):
  the **geometry budget** (≤ 64 *open* cells per mandala, tunable per tier;
  parked cells hold their geometry cell) is distinct from the **thermal
  budget** (`AGENTD_WORKER_CAP` *running* residency; parked releases it). A
  mandala may hold 64 open cells while 4 run on a standard node.
- **Budget descent is a theorem**: `BudgetVec{depth, cells, steps, deadline}`
  — admission requires strict decrease on depth, non-increase elsewhere, all
  components positive; renewal spends the *parent's* vector. Depth ceiling 6
  (default, tunable): both doctrine and debuggability.
- **The invariant axis** (Opus): the root writes objective + definition-of-
  done + the **verify command** once, content-addressed; every descendant
  carries a reference to those exact bytes, never a paraphrase. Every barrier
  at every depth runs the *root's* verify command — local success cannot
  diverge from global progress. Charters contract; the axis is rigid.
- **Runtime supervision = the review procedure** (Fable): per parent↔child
  edge, six binary observables — child: Progress · **Budget** · Verified;
  parent: Demand · **Capacity** · Horizon — a **total** decision procedure
  over all 64 words with centrality precedence (existence → the two centers →
  correctness), returning exactly **one single-line remediation, never a
  subtree restart** (the anti-thrash rule). `Done` is the least stable state:
  reaped within one review tick — primal work and dual supervision freed
  together — and a stale Done is itself a health-check defect (the
  anti-zombie rule). The driver keeps a **review census** (histogram of words
  per epoch) → the run's diagnostic reading, persisted as a Cerebro episode.
- **Measures are evidence, never assertion** (both forks): any R-bit cell
  declares a command-computed non-negative integer (failing tests, clippy
  count, |diff|, open TODOs…) that must strictly decrease; K-stall (default
  2) breaks the ring and escalates with the measure history attached.
- **Scheduling is golden** (Fable — the one place φ is mechanism): sibling
  review ticks at Weyl offsets `(i·φ⁻¹·period) mod period`, retries on
  Fibonacci backoff — N siblings structurally cannot phase-lock into herd
  storms. Applies to every recurring pulse in the structure.
- **Epochs and the orbit detector** (Fable): epoch fingerprint =
  hash(objective digest, sorted artifact digests, census). Two consecutive
  equal fingerprints = a true orbit (the A→B→A re-planning loop) → park
  `Blocked{"orbit detected"}` + convene a **council** over the census.
  Epoch rollover checkpoints the whole structure to `mandalas.json`;
  restart — including the nightly self-update swap — reloads it Parked,
  revivable ring by ring.
- **Address space: position IS identity** (Opus): `0.3.1.2` → the disk file
  (`<log dir>/worktrees/<root>/<addr>.json`), the branch name (`apex/w/0.3.1.2`),
  ancestry by string prefix. **The filesystem is the tree** — the only
  authoritative structure, which is the only posture compatible with a daemon
  that swaps its own binary mid-run. Reparent-by-prefix on reload; a node
  whose parent vanished still holds a valid contract with the root (the
  axis). Produced-but-unconsumed is a distinct terminal state (`Orphaned`) —
  a queryable bug class, not a silent leak.
- **Git worktrees per branching cell** (Opus): a new `git_worktree` tool in
  the git family; every B-cell child gets its own worktree on its
  address-named branch — parallel workers *physically cannot* collide. A
  J-barrier's declared work is concrete: merge child branches, resolve
  conflicts, run the root's verify, commit. `git log --graph` of a mandala
  run is its structure diagram.
- **Vouchers** (Fable, the H6 move one level down): workers do NOT have
  `task_fanout` — a worker becomes a sub-conductor only by receiving, at
  spawn, a voucher carrying its slice of the budget vector and ring geometry,
  enforced at the tool-router seam. W-tier ships depth-1 (no vouchers at
  all); M-tier introduces them.
- **Doctrine placement** (the fusion's philosophical settlement): code speaks
  engineering — `CellForm`, `Lattice`, `review()`, named observables; no
  hexagram semantics, no King Wen, no numerology in enums or logs. The
  mandala doctrine — "completion is unstable", "centrality first", "as above
  so below", the Bagua pre-fan-out coverage rubric — lives in the **skill
  layer** (`docs/fabrica-skill.md`, M-era) as the transmission format a
  self-rewriting agent internalizes via Cerebro. The math is the guarantee;
  the doctrine is how it survives self-evolution; the enum is neither.

## Locked decisions — the code regime

- **No separate chat app, no soul churn, no system-prompt fork.** The mode is
  a regime on goals: `goal_create{mode:"code"}` injects one **stable, cached**
  engineering-discipline block into the conductor's layer (byte-stable ⇒
  prefix-cache-safe). Workers get H6 charters regardless of mode. *(Not yet
  built — the shipped code-regime entry point is `mandala_create{repo}`,
  which injects the worktree/merge rituals per cell.)*
- **Specialization lives where the evolutionary layer says competence lives**:
  Cerebro procedures + `docs/fabrica-skill.md` (the imagine-craft-skill
  pattern — seed doc, `store_procedure` invitation, agents evolve it).
- **Workspace**: `workspace/code/<project>/` for clones,
  `workspace/code/.worktrees/<addr>/` for cell worktrees — everything inside
  the existing confinement boundary; the git-roots story is unchanged.
- **Model mix, formalized**: `task_fanout{model?}` per task + lattice-level
  defaults — the conductor thinks on the big model, leaf workers hammer on
  small/local (the colony-model-mix economy). Rides the per-request provider
  seam the council + vast-swap already prove.
- **Anthropic Batches API**: NOT for worker loops (interactive tool rounds
  can't ride an async batch endpoint). The seam is designed now — a cell may
  declare `latency:"batch"` — and built post-v1 as the 50%-off lever for wide
  single-turn leaf fans (classify/extract sweeps) on API-backed nodes only.

## Security lines (non-negotiable)

- **The invariant's verify command executes through the standard policied
  exec path** — same approval semantics as any agent exec. The driver never
  raw-execs it; barriers must not be a policy bypass.
- Artifact paths in `worker_report` are workspace-confined (the
  `apexos-confine` gate, as everywhere).
- Batch yolo never exceeds the parent goal's grant; default off.
- Worktrees live under the workspace; no git operation escapes the
  configured roots.

## Fabrica the app (UI)

- The kanban **workboard is renamed Fabrica** — one surface for goals,
  batches, and mandala trees.
- `WorkerStateChanged` is `GoalStateChanged`'s twin (same event → board-lane
  path); mandala trees render by address; the census renders as the run's
  live reading.
- **The emergency entrance** (André's requirement): every cell is a real
  session — the board exposes per-cell message / park / cancel / revive; the
  same entrance exists from any mesh seat via `send_to_agent(session_id=N)`
  — including the Callosum seat, so FORGE can reach into a running mandala
  from Claude Code over the wire.

## Slice ladder

W-tier (each = one PR, house style):

- **W1a** ✅ `#306` — session class + `WorkerState` + driver skeleton +
  `task_fanout{mode:"async"}` + events + `workers.json` (restart:
  Running→Parked) + board twin lane. Depth-1, env cap.
- **W1b** ✅ `#307` — `worker_report` verdicts incl. `yield` + TTL eviction +
  revive-on-send (`repair_history` reload) + slot release on park.
- **W1c** ✅ `#308`+`#309` — output artifacts + Cerebro episodes +
  `TaskBatchDone` **with `batch_deadline_s`** + `AwaitingBatch` posture +
  the integrate/verify directive (paths, not payloads).
- **W1d** ✅ `#310`+`#311` — batch yolo inherit + batched approval cards +
  cancel cascade + bounded inline mode + PB-1 soft breaker +
  `task_fanout{model?}`.
- **W2** ✅ `#318` — mesh workers (`node` per task): the colony as the worker
  pool. THE OWNERSHIP RULING: the peer owns everything stateful (worker record,
  session JSONL, cap/FIFO, policy — yolo never crosses the wire — review
  procedure, evidence, episode); the conductor owns batch bookkeeping only
  (`remote_workers.json` mirror rows + the deadline as the unbreakable net).
  Wire carries assignments out + reports home, never state — one writing
  daemon per file. Report push (3 retries) + review-cadence polls (golden
  offsets, fib backoff, beacon-dark skip) reconcile both restart directions;
  evidence DOCS mirror to `agents/<local_wid>.json` (artifacts stay on the
  peer); cross-node revive = the ordinary `send_to_agent(node, session_id)`.
  `/api/worker/{fanout,query,cancel,report}`, from-validated + token-gated;
  capabilities gains `worker:{cap,slots_used,queued}` for routing.

M-tier (Hamming-weight order — each slice adds one bit, one guard, one test
class):

- **M1a** ✅ `#312` — `CellForm`, `Addr`, `BudgetVec`, `admissible`,
  `ring_width` (pure, tested); the on-disk tree + reconstruction +
  reparenting; the invariant file; SPINE/LEAF only — depth with zero new
  concurrency.
- **M1b** ✅ `#313`+`#314` — J and B: GATE/FAN/DIAMOND; `git_worktree` tool +
  address-named branches; descendant-only barriers with timeouts; the review
  procedure replaces ad-hoc stall/TTL checks; golden offsets + Fibonacci
  backoff. Field-proven incl. the restart composition.
- **M1c** ✅ `#315` — R: SPIRAL/FORGE-form; measures + K-stall ring-breaking;
  vouchers (sub-conductors live); reap rule + dual-tree integrity in the
  health probe. Field-proven incl. renewal + the brake-not-wall law.
- **M1d** ✅ `#320` — the 64-cell composition table (36 free / 12
  conditional / 16 forbidden, ONE exhaustive test) gating admission:
  R-over-R refuses (reachable via vouchered sub-conductors), B-over-B
  conditional on the breadth product down the actual tree; changing-line
  re-validation when a fan arms the parent's B. Torus epochs (600s golden
  offsets, per-epoch census drain, fingerprint = axis + evidence digests +
  census, persisted on the record) + the orbit detector → a small council
  (one per distinct stuck-state; synthesis rides `mandala_status`; v1
  deliberately does not auto-park — brake-not-wall, field data reopens);
  census reading → Cerebro per epoch. Plus two board-truth fixes: the
  never-bound WORKERS lane, and the surgical approvals sweep. The tree-view
  WINDOW moved to the Fabrica-app track (recipe in BACKLOG).
- **M2** ✅ `#321` — cross-node rings: a ring cell may carry `node` — the
  CELL (geometry, budget, barrier membership, closure) stays on the bindu,
  its execution BODY is a W2 remote row on the peer; the composed cell
  directive (axis verbatim) crosses as task text, the budget's step ceiling
  as the `steps` assignment field, and the evidence mirror closes the loop
  (`sync_remote_cells` — a gate over a remote ring opens the tick its last
  mirror lands). Gates, measured cells, vouchered cells and code mandalas
  stay local (`remote_cell_veto`, the four named laws). Heartbeats = the W2
  golden-offset polls; remote-cell revive = the ordinary
  `send_to_agent(node, session)`. Epoch fingerprints refined: census word-SET
  (count jitter under-detected orbits on busy trees) + open remote-cell
  state lines (a working remote ring is never invisible sameness).
  **Field-proven 2026-08-02**, four exhibits on apex1 ⇄ andre-laptop: the
  colony diamond (25s fan→integrate→close), bindu restart mid-ring (reload +
  reconcile + parked-gate revival), cross-node cell revive (parked crossed
  in one poll; revive-by-send; gate self-opened), dark-peer fail-fast (both
  siblings failed ~100μs apart; the gate integrated failure as data). Smoke
  finds fixed same day: remote-aware health probe, override-open recording,
  wake-edge episodes.

Fabrica-app slices (parallel track): board rename; worker lane ✅ (W1a, lane
bound at M1d); **tree/census view ✅ `#324`** — the Mandalas window (AppKind
ordinal 21, slug `mandala`): a flat depth-indented mirror of every mandala's
cells — form, state, worker, remote body (`@ node (state)`), measure tails,
barrier/voucher/reparent marks — plus census, epoch/fingerprint, orbit count
and council synthesis. Data rides the occipital follow-along idiom: a
successful `mandala_status` anywhere is shape-sniffed off the ToolResult
stream (`mandalas` array = the signature, no tool name on the wire) and
mirrored in, with latch-aware auto-reveal. Remaining: per-cell intervention
controls. `docs/fabrica-skill.md` landed with M1a.

## Deferred — reopened only by field data

Per-parent queue fairness (v1 = one global FIFO) · R-over-R with proven
measure refinement (v2 at earliest) · the Bagua rubric (optional checklist,
skill-layer) · Batches-API activation · 64-cell / depth-6 retuning per tier
(the *law* is conservation; the constants are tunable) · PB-1 breaker
threshold · φ⁻¹ as an alternative contraction ratio (0.5 is the shipped
bound).

## What Fabrica v1 means (acceptance shape)

A conductor goal on a standard-tier node fans 12 coding tasks through one
`task_fanout` call: cap admits 4, 8 queue and admit as slots free; each
worker holds only its charter + carry; a parked worker revives from one send
with prior state intact; every terminal worker leaves an artifact + episode;
the batch report lists paths; the conductor demonstrably reads them, runs
the workspace verify gate, and either finishes or fans a fix batch — and a
daemon restart mid-run loses nothing. That loop, green on a live node, is
v1. The mandala is v2's ceiling on the same contract.
