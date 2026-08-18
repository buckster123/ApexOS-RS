# Apex Loom — parked after live-tree review

> **2026-08-18.** Intake brainstorm (Grok + Harper + Benjamin + Lucas) held
> up against current `main`. **Do not implement the draft as written.**
> Original package kept in [`draft/`](draft/) as research input — same
> posture as [`state-machine.md`](../../state-machine.md) after
> [`state-machine-eval.md`](../../state-machine-eval.md).

**Stamp:** parked. Not dropped. Concept still has a spark; no pain point
today. Revisit only after a Fabrica pass, or if field data says the
conductor keeps dropping a choreography the control plane could have
held. Possible later shape: **(a)** a Goal-recipe table inside the
existing `advance()` — not a fourth driver.

---

## BLUF

The draft is the **third swing** at “make the autonomous run a graph you
can see.” Swing 1 (`docs/ideas/state-machine.md`) was an owning
`run_loop` — rejected. Swing 2 became the Goal driver + Fabrica
(workers + mandalas). Swing 3 (Loom) learned the hard no (don’t own
TurnGate, don’t wrap the router) and then proposed a YAML engine that
would *also* prompt, fan out, wait on `TaskBatchDone`, and nest.

Fabrica already does that work. The charter’s named missing tier was
**workers between a serial Goal and an ephemeral spawn**
(`2-repo-evaluation.md`). That tier shipped (W1–W2, M1a–M2). The
Mandalas window (`#324`) is already the visible structure of a run.
BACKLOG Ideas intake **#4** (workflow/canvas) is the in-repo cousin —
charter session first, not a silent `loom_*` family.

No current pain. The draft was a repo riff, not a field finding.

---

## What we checked (live `main`)

- `agentd/crates/agentd/src/goal.rs` — automatic driver + optional
  `goal_step`; directives are `UserPrompt`; `AwaitingBatch` is a bool,
  not persisted; **the goal driver is the only prompter of a goal
  conductor** after a batch.
- `worker.rs` / `mandala.rs` — `task_fanout`, evidence paths,
  `TaskBatchDone`, vouchers, compose table. Worker driver must **not**
  prompt a goal parent.
- Supervisor intercept + `VIRTUAL` list + `policy.toml` additive sync
  — the real cost of a new tool family.
- Cerebro `store_procedure` / PAC / `soul_rehearse` / `propose_evolution`
  — competence vs identity. Procedures already *are* workflows.
- Workspace deps: no `serde_yaml`, minijinja, or rhai. Config is TOML +
  JSON.
- Draft vs draft: `EXAMPLES.md` already wants `contains()`, `or`, and
  arithmetic the claimed v0 language does not have.

---

## Why not as written

| Draft move | Why it doesn’t land |
|---|---|
| `goal_create{machine}` drives the step loop | Changes the shipped **LLM-proposes / code-disposes** contract. Soul, skill, and board still teach `goal_step`. |
| `loom_*` adapter + free-floating instances | A second `select!` (or a second waiter on lossy `TaskBatchDone`) next to two drivers. Surplus `UserPrompt`s burn budgeted laps. |
| `action: fanout` + `wait_for: batch_done` | `AwaitingBatch` + one-call diamond already exist. Two waiters wedge or double-integrate. |
| Hierarchical `action: machine` | Second Mandala without descent, compose table, or the 64-cell budget. |
| `loom_signal` as a wake | Revive-on-send is the only Parked→Running edge. Never fire into worker/spawn range. |
| YAML in `skills/machines/` + `kind: machine` | Authored modules the evolutionary layer dropped; a new EDK enum variant; a third notation beside procedures and PAC. |
| “Architecture locked / ready to implement” | Five open questions in the draft itself; examples exceed v0; team had limited repo access. |

The useful remainder is small: a **data-driven next-directive table**
that `advance()` / `resume_after_batch()` can consult, so a named
choreography (writer-critic, research, plan→fan→integrate) is a
contract rather than a hope. Same TurnGate, same batch hold, same yolo,
same board. That is option **(a)** — not a Loom crate.

---

## What to do instead (now)

**(b)** — leave these files here. Finish already-chartered Fabrica
leftovers before considering any recipe layer:

- `goal_create{mode:"code"}` (charter-locked, unbuilt; today’s entry is
  `mandala_create{repo}`)
- worker carry `{system, files, plan_ref, skills}` (still open)
- per-cell board intervention (Mandalas window is structure-only)

Those, plus a richer mandala/Fabrica surface, may make even **(a)**
redundant. That is an allowed outcome.

---

## Reopen conditions

Reopen this folder when **one** of these is true:

1. A Fabrica pass (code regime + carry + board) still leaves a gap that
   a recipe table would close — then consider **(a)** only, with a
   charter amendment, sharing an artifact format with BACKLOG #4.
2. Field data: a conductor repeatedly drops a known choreography
   (`goal_step` forgotten, critic pass skipped, integrate-without-read)
   and a procedure in Cerebro is not enough.
3. The canvas (#4) is chartered and needs a compile target. Default
   target is still “procedure + `task_fanout`,” not a new engine.

Do **not** reopen for: a cool FlatMachines post, a desire for YAML, or
retries/HITL/hierarchy (those are review / Blocked / Mandala).

Open on purpose, not a blocker: competence-as-procedure vs executable
graph (does a recipe enter dream fitness, or stay a file?). Needs more
time; do not invent a third layer to dodge the question.

---

## Session notes (André + GROK, 2026-08-18)

1. **Pain today?** None. Caught the repo, riffed in a grok-heavy swarm.
2. **Who locks?** André + FORGE; pair-programming, reasoned pushback
   welcome; sudo and final say stay human.
3. **Executable graph vs living procedure?** Parked as an open
   question — not answered, not closed.
4. **Code regime / carry first?** Yes. Go over Fabrica before this.
   Loom may become redundant. The mandala app already shows the run’s
   structure and can grow toward the “graph I can see” the draft aimed
   at.

Drafts stay. This file is the hand-off.
