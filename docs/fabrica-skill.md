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
  two workers editing the same file WILL collide until git worktrees arrive
  (M1b). Until then, give code workers disjoint file scopes, explicitly.
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
- **Lattice choice** (widths arm fully in M1b; the names already matter):
  *spine* to bisect and narrow · *quad* for balanced 4-way decomposition
  (siblings cross-check at integration) · *fan* for embarrassingly parallel
  sweeps · *spiral* when the decomposition is unknown — grow toward
  progress · *funnel* to synthesize many sources into one.
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
