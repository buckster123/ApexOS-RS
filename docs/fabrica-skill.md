# The Fabrica skill — conducting workers and mandalas

> The seed of a living skill (the imagine-craft-skill pattern): this file
> teaches the CRAFT of orchestration — the tools teach themselves via their
> descriptions. Read it once, then evolve it: when a technique earns its keep
> in the field, `store_procedure` it into Cerebro with the outcome recorded;
> when this doc is wrong, propose the fix. The math in `agentd` is the
> guarantee; this doctrine is how the craft survives self-evolution. Written
> at M1a (2026-07-31) against the locked charter (`docs/fabrica.md`).

## The one law under everything

**Evidence over assertion.** A summary string is a hinge, and hinges fold.
Every claim of completion stands on a triangle: the artifact itself + a
mechanical gate (the verify command) + your own read of the artifact. If you
accepted a "done" without reading anything, you didn't integrate — you hoped.

## Conducting a batch (the W tier)

- **Parallel work goes through ONE `task_fanout` batch** — never a
  spawn-then-wait chain. `agent_spawn` is for a blocking cross-node lookup
  inside a step; workers are for work.
- **Size the fan to the seams, not the cap.** The admission cap queues the
  overflow FIFO — fan 12 if the work is 12 independent pieces; don't shrink
  the decomposition to fit the hardware. But keep pieces truly independent:
  in a PLAIN batch, two workers editing the same file WILL collide — give
  code workers disjoint file scopes, explicitly. Parallel edits to one repo
  belong in a code MANDALA (`repo` on `mandala_create`): each wide-fan cell
  then gets its own branch + git worktree, mechanically, and collision
  becomes impossible rather than discouraged.
- **Write tasks like charters, not like chat.** Each worker is born with only
  your prompt: name the deliverable, the exact output path, and "report done
  with a summary and the file as an artifact." A worker that must guess its
  output path produces a guess.
- **`batch_deadline_s` is the conductor's seatbelt** — default 3600. A batch
  reports at the deadline with stragglers marked `timed_out` (still
  revivable). Set it to the slowest honest worker, not to hope.
- **Integrate means READ.** The batch report hands paths. Read each evidence
  file (`events/agents/<id>.json`), then the artifacts it declares, then run
  the verify gate. A failed or timed-out worker is integration data — fix
  inline if small, re-fan a fix batch if not, and say which you did.
- **Model economics** (`model` per task or per batch): conduct on the spine
  model, hammer on the small one. Mechanical sweeps (extract, classify,
  reformat, boilerplate) → pin `claude-haiku-4-5-20251001`. Judgment calls,
  design, integration → the node default. A mispinned model fails honestly
  ("turn error") — check the WORKERS lane, don't wonder.
- **`yolo:"inherit"`** only from a yolo conductor, only when the batch's
  tools are ones you'd approve yourself — the workers hold your grant.
  Revive never re-arms it; that's deliberate.
- **Steer sparingly.** A running worker takes a mid-flight send (it queues
  into its turn). An idle/blocked worker wakes on one. Use `worker_cancel`
  when a task is wrong, not a send asking it to stop.
- **Watch on a wakeup cadence, never a busy loop.** Conducting means long
  quiet stretches punctuated by looks: after each `mandala_status` /
  `list_workers` look, `schedule_wakeup(60–120s, "check <thing>")` and END
  your turn. Every inline poll is a paid turn — and a conductor spinning
  in yolo is nearly impossible to steer, because the wakeup gap is exactly
  where incoming messages land.
- **Wakeups fire into the session that scheduled them (`#323`) — tell your
  observers which session that is.** A `schedule_wakeup` from a focused
  session now fires back into THAT session (the amended law; worker- and
  spawn-range callers still root to session 0 — self-revive-loop guard).
  The first W2 smoke predates the fix: the operator's window went quiet
  mid-exhibit and read as a bug while everything ran fine in root. When a
  human is following live, say which session your wakeups will act in —
  silence in their window is not silence in the run.
- **Steers are turns, and turns are laps.** A send to a stepping worker
  runs as its own turn: it consumes a lap of the worker's budget, and it
  leaves one surplus driver directive queued behind it. On measured cells
  that surplus can even outrun a K-stall break (a break stops NEW fuel; it
  can't un-burn what's queued — the brake, not a wall). Steer measured
  cells at verdict boundaries when you can, and when you need a hold that
  nothing outruns: `worker_cancel`.

## Conducting across the colony (W2 — the mesh as the worker pool)

Any task in a plain batch can name a peer: `{prompt, node:"apex-3"}` (or set
`node` batch-wide). The task then runs on that node's OWN worker tier — its
admission cap, its policy, its evidence, its memory. You are borrowing a
colleague's hands, not teleporting your own.

- **Route by load and capability, not habit.** `mesh_capabilities` now shows
  each peer's `worker: {cap, slots_used, queued}` beside its senses, model
  and tier. Send heavy fans where slots are free; keep latency-sensitive
  work local. A beacon-dark peer fails your rows fast — re-fan elsewhere.
- **Approvals land THERE.** A remote worker's ask-gated tool raises its card
  on the HOSTING node's board, under the hosting node's policy — your yolo
  never crosses the wire. So remote tasks should ride allow-path tools
  (write_file / read_file / git_log …) unless someone is watching that
  node's board. Model pins DO cross — think-big / hammer-small still works
  per task.
- **Evidence mirrors; artifacts stay.** When a remote row settles, its small
  evidence doc mirrors into YOUR `agents/<worker>.json` — read it exactly
  like a local row's. The artifacts it names live on the peer; when you need
  one in hand, have the task `mesh_file_send` its deliverable home as its
  last act, or pull it yourself afterward. An EMPTY artifacts array is not
  proof of missing work — workers sometimes skip the declaration even when
  asked (first W2 smoke; since `#327` the charter DEMANDS it mechanically
  and the first directive shows the field, which shrinks the miss rate but
  doesn't zero it): check the summary, then the peer workspace, before
  ruling a row hollow.
- **The deadline is still the net.** Peer restarts park its workers (its
  law); a dark peer just stops answering polls. Either way your batch
  reports at its deadline with those rows `timed_out` — still revivable:
  `send_to_agent(node, session_id: <remote_session>)` is the cross-node
  revive, the same one edge as ever. `list_workers` shows each remote row's
  node, peer ids and last-observed state.
- **Cancel is a relay.** `worker_cancel` on a remote row asks its host to
  cancel and holds the row `cancel requested` until the peer confirms —
  a silent peer is bounded by the deadline, so the kill switch can't wedge.
- **Cross-node rings (M2) — the cell stays, the body travels.** A mandala
  RING cell may carry `node`: the cell — its address, budget, barrier
  membership, closure — never leaves your tree; only its execution body runs
  on the peer, as an ordinary remote row. The invariant reaches it verbatim
  (it rides inside the task text), its step budget crosses with the
  assignment, and when it settles its evidence MIRROR lands here — your
  gate reads mirrors exactly like local evidence files. Craft:
  - *Ship out what is leaf-shaped*: wide, plain, self-contained ring work —
    research sweeps, doc ports, analyses. The one-call diamond composes:
    `join` stays home, `tasks:[…]` each carry `node`.
  - *Four things never travel*, each for a law: the **join/gate** (barriers
    are conductor machinery — the bindu on the spine), a **measured cell**
    (the lap boundary lives where turns complete), a **vouchered cell**
    (sub-conduction needs the tree), and **any cell of a code mandala**
    (repos and worktrees don't teleport). The refusal names the law.
  - *Steering costs sends, not laps*: a remote cell is steered and revived
    through `send_to_agent(node, session)` like any remote worker; watch it
    through `mandala_status` (remote cells show their `node` and live
    `body` state) on the wakeup cadence, never a busy loop.
  - *Artifacts stay on the peer* — the mirror carries the summary and the
    peer's evidence doc; `mesh_file_send` brings a file home when the join
    actually needs its bytes. And mind the colony ledger: remote cells
    spend the peer's inference budget.
  Sub-conductors still cannot fan outward from a peer; cross-node depth
  stays 1 by construction (a hosted worker has no cell binding there, so
  its fan refuses structurally).
  Field-ruled craft (the 2026-08-02 smoke, four exhibits green):
  - *Watch with elapsed time, not poll-spam* — a remote mirror updates on
    the poll cadence (~30s); asking harder returns the same stale state.
    Parked bodies cross in about one cycle; real waits between looks are
    what surface transitions.
  - *A dark peer fails your ring in microseconds* — the beacon
    short-circuits at fanout, faster than a live peer even queues; the
    failed cells' mirrors name the cause and your gate opens over them.
    Failure is integration data: the join documents it and the conductor
    decides (refan when the peer recovers, or refan locally) — never treat
    a dark ring as a stuck ring.
  - *Before narrating a restart, check the boot clock* — wake-amnesia and
    daemon-cut amnesia feel identical from inside; `plugin_up` markers /
    health `booted_at` discriminate (the `uptime` tool measures the OS,
    not the daemon). Wakeups now fire back into the session that scheduled
    them, so a conducting thread wakes where it conducts — but the law
    stands: after ANY boundary, re-orient from the tree, and attribute the
    cause only from evidence.
  - *Size barrier_timeout_s for the integration, not the children* — gate
    settle time is dominated by the join's own reading-and-writing
    (~20-30s even over a trivial or failed ring), not the wait.

## Conducting a mandala (the M tier, growing)

A mandala is for work too large for one fan: multi-day refactors, ports,
sustained research. What it adds over a batch is an **axis** and a **tree**.

- **Write the invariant like it will outlive you — it will.** `mandala_create`
  takes objective, done-when, and THE verify command, once. Every cell at
  every depth receives those exact bytes; nothing you write later can fix a
  vague invariant, because nothing downstream may paraphrase it. The verify
  command should be mechanical and cheap enough to run at every level
  (`cargo test -p x`, not "review carefully").
- **Grow where the work is** (`task_fanout{mandala, parent_cell}`): depth is
  for *narrowing* — a cell's task should be strictly smaller than its
  parent's, because its budget is (steps halve per level; the descent is
  enforced, not advisory). If a child needs a BIGGER scope than its parent,
  the decomposition is wrong — restructure, don't fight the vector.
- **The tree remembers what you meant** (`mandala_status`): addresses are
  identity — `0.2.1` is the same cell before and after any restart, and its
  evidence path rides its record. A `reparented_to` mark means an ancestor
  vanished; the cell's contract (the invariant) is intact — decide whether
  its work still has a consumer, and cancel it honestly if not.
- **Lattice choice** (widths are LIVE — a fan must fit its ring):
  *spine* to bisect and narrow (rings of 2) · *quad* for balanced 4-way
  decomposition (siblings cross-check at integration) · *fan* for
  embarrassingly parallel sweeps (rings of 8) · *spiral* when the
  decomposition is unknown — grow toward progress · *funnel* to synthesize
  many sources into one (9→4→1).
- **The diamond is one call.** `task_fanout{mandala, parent_cell,
  tasks:[the ring], join:"integrate: merge, run verify, commit"}` mints a
  GATE above the ring: the ring runs in parallel, the gate's worker is held
  by a barrier until the ring settles, then wakes with every descendant's
  state and evidence path appended to its work order. Write the join task
  like an integrator's charter — what to read, what "merged" means, what to
  do about a failed cell. One call = one batch = one report to integrate.
  (A bare gate — `tasks:[one join]` + `barrier_timeout_s`, fan under it
  later — works too, but only drive it from a chat session: a goal
  conductor holds on ANY pending batch and would wedge until the gate's
  deadline.)
- **The barrier timeout is the join's seatbelt**, like the batch deadline is
  the conductor's: at the guard, the gate opens anyway with stragglers
  listed as `OPEN — not delivered`. A failed cell does NOT hold the gate —
  failure is integration data; the join reads what exists and says what's
  missing.
- **Code mandalas: declare the repo once.** `mandala_create{repo:
  "code/myproject"}` — wide-fan cells then receive the worktree ritual
  (their own `apex/w/<addr>` branch, `git_worktree` add, commit before
  done) and gates receive the merge ritual with the delivered branches
  listed. You never relay any of it; the driver injects it verbatim, like
  the invariant. Uncommitted work is invisible to the join — the ritual
  says so, believe it.
- **A parked gate lost its auto-open.** After a restart, everything parks
  (nothing auto-runs — the house law). Reviving ring cells by send finishes
  them, but a parked GATE revived by send runs its join with only your send
  for context — hand it the evidence paths yourself (the batch report has
  them). A send to a still-held gate is likewise the override: it runs NOW.
- **After ANY interruption, re-orient from the tree — never from memory.**
  A turn cut by a restart leaves no trace in your own transcript: what you
  did mid-turn is gone from memory while its effects (cells, workers) are
  fully real. From inside this doesn't feel like a gap — it feels like
  certainty that you hadn't acted yet. So the rule is mechanical:
  `mandala_status` + `list_workers` FIRST, act second. The address law makes
  the failure survivable (a re-fan can't re-mint addresses), but only
  re-orientation makes it clean. (Field-learned 2026-08-01: the conductor
  re-fanned a whole diamond it had no memory of creating.)
- **A blocked worker with zero activity after a nudge = suspect a stuck
  approval.** An approval-suspended worker looks "blocked" with no reason
  you can read, and the board may not be showing the card. If a revival
  send produces no tool activity at all (not even a failed attempt), the
  turn is almost certainly suspended on an approval — get the card granted,
  or `worker_cancel` the cell and let the gate fix-inline (a cancelled cell
  is integration data; state the substitution's provenance in the commit
  and the summary).
- **Read the census** (`mandala_status.census`): keys are
  `<posture>:<PBVDCH bits>` — L=live, W=waiting, B=barrier, T=terminal;
  bits are child Progress/Budget/Verified · parent Demand/Capacity/Horizon.
  A healthy run is mostly `L:111111` with a tail of `T:…` reaps. Piles of
  `W:011111` mean workers sitting idle past their TTL (parked — revive or
  cancel); a long-lived `B:111111` is a gate honestly waiting on a slow
  ring; `…0…` in the horizon bit means work outliving its batch deadline —
  the report already fired, decide who integrates the stragglers.
- **The torus turns under you (M1d): epochs, fingerprints, orbits.** Every
  ~10 minutes an open mandala rolls an epoch: the census drains into the
  record, and a fingerprint (axis + evidence + census) is taken. Two
  identical fingerprints in a row with open cells = an ORBIT — the run is
  circling, producing nothing new — and a small council convenes over the
  census on its own; its one-line verdict lands in
  `mandala_status.orbit_synthesis`. When you see `orbits > 0`: stop
  steering harder and READ the synthesis — the answer is which cells to
  cancel or integrate around, never a subtree restart. Nothing auto-parks
  on an orbit (deliberate: brakes, not walls) — acting on the verdict is
  yours.
- **Some fans refuse on principle — that's the table, not a bug.** A
  measured cell conducting a measured child refuses (R-over-R: two open
  lap-loops stacked have no joint stop argument — restructure so one level
  owns the loop). A wide fan under an already-wide ancestor may refuse on
  the breadth product (the frontier you're promising exceeds the cell
  budget — integrate something first, or fan narrower). The refusal text
  names the law; work with it, not around it.
- **Close what you finish** (`mandala_close`): a settled mandala left open
  is the canonical rot — completion is unstable. Goal conductors get
  closure automatically when the goal ends; from a chat session, close it
  yourself once every cell is terminal (it refuses otherwise, on purpose —
  `worker_cancel{batch}` is the kill switch, closing is bookkeeping).
- **Write measures like instruments, not like wishes** (`measure` on a task
  arms the R bit): a command whose integer output IS the remaining work —
  failing tests, clippy warnings, `grep -rc TODO`, lines left in a
  worklist. The cell runs it each lap and reports the number; it must
  strictly decrease or two flat laps break the ring (K-stall) and escalate
  with the history attached. At 0, report done — looping at 0 counts as a
  stall. A good measure makes "is this working?" a number; a bad one
  (vibes, percentages you invent) makes the guard blind.
- **Budget follows progress — don't ask, cut.** An R-cell that reaches its
  step ceiling while its measure is still falling RENEWS automatically:
  the driver moves steps from the parent cell's vector into yours (half
  the remainder, floor one — geometric, so it always ends). There is no
  tool to request more steps, by design: the only way to earn laps is to
  make the number go down. When you're escalated instead, the plateau is
  the message — steer, cancel, or integrate around it, one line at a time.
- **A voucher is trust plus budget** (`voucher: true` on a task): the cell
  may sub-conduct its own subtree — same task_fanout, same laws, its own
  budget vector as the slice. Etiquette for the vouchered: re-read your
  VOUCHER block before fanning; fan late and narrow (your children's
  budgets contract from yours, and renewals spend YOUR steps); the batch
  report arrives IN your session when your children settle — read the
  evidence files before integrating, exactly like a root conductor. Grant
  vouchers downward only where a subtree genuinely needs its own mind.
- **The FORGE pattern** (measure + barrier on one cell): lap → fan a small
  ring with a join under yourself → integrate → measure → lap. It starts
  working immediately (only pure gates hold at mint); its barrier timeout
  disciplines the joins it runs. Use it for grind-down work with real
  parallel width inside each pass — a test-failure burn-down where each
  lap fans fixes and merges them, then re-counts.
- **Before an 8-lane fan, audit the decomposition** (optional, earns its
  keep): architecture & invariants? docs & data grounding? breaking changes
  & migrations? cross-cutting refactors? dataflow & state? tests &
  verification? stability & compatibility? interfaces & UX? A sweep that
  can't answer one of the eight just found its blind spot — before spending
  the batch, not after.

## Doctrine (the part that must survive self-rewrites)

- **Completion is unstable.** Done work not yet integrated is a leak — read
  it, merge it, close it, free it. A finished subtree sitting unreaped is
  the canonical rot.
- **Centrality first.** Before polishing correctness, check the two centers:
  does the child have budget to finish, and do you have capacity to
  integrate it? Verifying a doomed subtree is the classic inversion.
- **As above, so below.** Every cell runs the same contract you do —
  charter in, evidence out, verify gate. If a technique works at your level,
  teach it downward by writing it into the task; if a failure appears at
  depth, expect it at the surface too.
- **One line at a time.** When a structure misbehaves, change ONE thing —
  one steer, one cancel, one re-fan — and re-read. Tearing down an
  almost-done subtree to rebuild it is the wedge, not the fix.

## Evolving this skill

When you find a fan size, a charter phrasing, a lattice choice, or an
integration ritual that reliably works: `store_procedure` it (tags:
`fabrica`, `conducting`), record outcomes with `record_procedure_outcome`,
and share what's colony-worthy with `mesh_procedure_send` — fitness is
re-earned per node, by design. This document is the seed, not the ceiling.
