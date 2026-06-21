# Goal Driver — Phase 2 design

> The autonomous "goal/loop" engine for ApexOS. Phase 2 of
> [`state-machine-eval.md`](state-machine-eval.md); Phase 1 (the read-only Work Board) shipped in
> `ui-slint`. This doc is the buildable design: the `Goal` object, the driver loop, the LLM hook,
> the hard guards, and the policy/observability lessons from the first live multi-agent run.
>
> **Core principle (settled): code controls, the LLM works.** A deterministic Rust control-plane
> owns the loop / budget / guards / transitions / persistence; the LLM does the actual work each
> step and *proposes* the next step. Code disposes — the budget and guards are a hard backstop the
> model can't talk past. This is the **LLM-proposes / code-disposes** pattern ApexOS already runs in
> the evolution applier, the self-update gate, and the council. The goal driver is the same shape.

---

## Why a driver at all (the gap)

We have the *pieces* of a state machine (TurnGate, evolution, self-update, council, the bus reducer)
but not the *object*: a first-class, observable, **bounded**, resumable run. Autonomy today is
yolo + scheduled tasks + prompting — capable, but the *run itself* can't be held, watched, budgeted,
or stopped cleanly. The `Goal` is that object; the board (Phase 1) is its window.

The driver is the **scheduler generalized**: `scheduler.rs` already advances work by emitting the
next `UserPrompt` on the bus at a cron time. A goal does the same — emit the next step until a guard
says stop. Same bus, same `TurnGate`, same turn engine.

---

## The `Goal` object

```rust
// new: apexos_core (wire type) + a goals.json store (like mesh_sessions.json)
struct Goal {
    id:         GoalId,        // newtype u64
    objective:  String,        // the human's goal, verbatim
    session:    SessionId,     // a DEDICATED session (not root 0) → its turns gate independently
    state:      GoalState,
    step:       u32,
    max_steps:  u32,           // HARD ceiling (the overnight-run governor)
    token_budget: u64,         // optional hard token cap (0 = none)
    consecutive_failures: u32, // failure breaker
    posture:    GoalPosture,   // policy stance for unattended steps (see below)
    history:    Vec<(GoalState, /* ts */ u64)>,
    last:       String,        // last directive sent (for resume / audit)
}

enum GoalState { Planning, Acting, Blocked, Reflecting, Done, Failed }

// Revives the currently-dead `SubagentsConfig.inherit_mode` field as a real knob:
enum GoalPosture {
    Yolo,       // unattended: auto-approve within the goal (overnight runs)
    AskBlocks,  // an `ask` tool → a BLOCKED card on the board, never a silent stall
}
```

A goal runs in its **own `SessionId`** (allocated from `next_session_id`, like a mesh session), so
its turns serialize through the `TurnGate` independently and never pollute root session 0 (the
sensor/scheduler funnel). The board gets a lane per goal.

---

## The driver loop (deterministic — a `scheduler.rs` sibling)

One `tokio` task owns the goal set. Per goal, the loop is:

```
start                → state = Acting; emit step directive on the bus (UserPrompt{goal.session})
on TurnComplete(g)   → read the goal_step signal the agent reported this turn:
    done             → state = Done.    stop.
    continue{next}   → step += 1;
                       if step >= max_steps OR budget exhausted → state = Failed (bounded). stop.
                       else emit `next` as the directive. (back to Acting)
    blocked{reason}  → state = Blocked. park (board card). await human resume.
    (none / errored) → consecutive_failures += 1;
                       if failures >= FAILURE_BREAKER (e.g. 3) → state = Failed. stop.
                       else re-emit with a corrective nudge.
emit Event::GoalStateChanged on every transition (board + event log).
```

**Hard guards, enforced in code (never the LLM):** `max_steps`, `token_budget`, the consecutive-
failure breaker. The LLM's `done` is *advisory* — the guards always win. This is what makes an
overnight run safe to leave alone.

Every step is **one gated turn** (`UserPrompt → TurnGate → run_turn`), so the driver never spawns a
turn outside the gate — it reuses the exact serialization invariant. A goal step may itself
`agent_spawn` sub-agents; those run as they do today.

---

## The LLM hooks (exactly two)

1. **The work-turn** — the existing `run_turn`. The model does the actual work each step (review the
   lander, iterate it, run tools, spawn sub-agents). No change.
2. **`goal_step` — a new virtual tool** the agent calls to report a step's outcome:
   ```
   goal_step { status: "continue" | "done" | "blocked", next?: string, reason?: string }
   ```
   Routed to the driver over a dedicated mpsc (mirrors `propose_evolution`'s `set_propose_tx` — a
   busy turn can't lag-drop it on the broadcast bus). The driver consumes it on `TurnComplete` and
   transitions. Companion tools: `goal_create{objective, max_steps?, posture?}` (start a goal),
   `goal_cancel{id}`, `list_goals`.

That's the entire LLM surface: do the work, and propose the next move. Everything else is code.

---

## Termination & safety (the governor)

- **`max_steps`** — hard ceiling on turns. Default conservative (e.g. 20); the operator raises it for
  a long build.
- **`token_budget`** — optional hard token cap (accumulate per-goal from the usage parser).
- **Failure breaker** — N consecutive failed/empty steps → `Failed`, stop. Prevents a stuck goal
  from burning the night on a wall. (Directly answers the sub-2 snag: a goal can't silently grind.)
- **Never silently stall** — the rule the snag exposed. A step that can't proceed becomes a
  `Blocked`/`Failed` card with a reason, never a turn hanging on an approval no one sees.

---

## Policy stance for unattended steps (reviving `inherit_mode`)

The first live multi-agent run surfaced two facts: sub-agents run under the **one node-global
`PolicyEngine`** (so on a yolo node they auto-approve, on a suggest node a sub-agent's `ask` tool
would stall on an approval no human is watching), and **`SubagentsConfig.inherit_mode` is dead code**
(defined, never read). Phase 2 gives it meaning via `GoalPosture`:

- **`Yolo`** — the goal's steps auto-approve `ask` tools (the overnight-autonomy choice). Bounded by
  the goal's own guards, not by a human.
- **`AskBlocks`** — an `ask` tool becomes a **Blocked card** on the board with the tool + args; the
  human approves from the board (Phase 3 interactive), then the goal resumes. The supervised choice.

Either way: **no silent stalls.** The posture is per-goal, declared at `goal_create`.

---

## Observability (the snag's real lesson)

A failed sub-agent is currently near-invisible: agentd logs supervisor lifecycle but **not**
sub-agent tool/turn errors (they return to the parent as an `ok:false` ToolResult), and the board's
Phase-1 sub card just *clears* on `TurnComplete` regardless of success or failure. Phase 2 closes
this:

- A step/sub-agent that ends `ok:false` → a **FAILED card** (red) carrying the error text, into the
  goal's lane / RECENT — not a silent disappearance.
- `Event::GoalStateChanged` drives a **goal lane** on the board: the goal card shows state + step
  count + budget remaining; its sub-steps nest under it.
- The append-only event log already records the transition history → "rewind a goal" (Phase 3) is
  replay, not new persistence.

> Near-term, this failure-visibility upgrade is also a standalone win for the *existing* board
> (show FAILED sub-agent cards) — buildable before the full driver if we want the opacity gap closed
> immediately.

---

## Persistence & resume

- A `goals.json` store (`<log_dir>/goals.json`, the `mesh_sessions.json` pattern): `GoalId → Goal`.
- On boot, reload goals; in-flight ones (`Acting`/`Blocked`) re-enter as `Blocked{reason:"daemon
  restarted"}` → the human (or a `goal_resume`) restarts them. No half-run is silently abandoned.
- Cerebro episodes wrap a goal for the long-term record (the evolution-applier pattern), so a
  finished goal is a recallable memory.

---

## What's reused vs. new

| Reused (already in tree) | New (small) |
|---|---|
| `TurnGate` + router + `run_turn` (each step) | the driver task (a `scheduler.rs` sibling) |
| `agent_spawn` (fan-out within a step) | the `Goal` object + `GoalState`/`GoalPosture` |
| the broadcast bus + event log (history) | `Event::GoalStateChanged` + `GoalCreated` |
| the scheduler's `emit(UserPrompt)` driver pattern | `goal_step` / `goal_create` / `goal_cancel` / `list_goals` virtual tools |
| the usage parser (token budget) | `goals.json` store |
| the Work Board (Phase 1) | goal lanes + FAILED cards on the board |

No rewrites. The driver is additive; the turn engine, gate, policy, and board are untouched in their
core.

---

## Open decisions (resolve before/while building)

1. **`done` trust model.** Confirmed: advisory — guards are the hard stop. Should a `done` also run a
   cheap **verify step** (an LLM "did we actually meet the objective?" turn) before closing? (Council-
   style self-check; optional, costs one turn.)
2. **Default `max_steps` / budget.** Conservative default + operator override at `goal_create`.
3. **Multi-goal concurrency.** Goals are independent sessions → the `TurnGate` already serializes
   within each; across goals they run concurrently (bounded by `max_concurrent`). Cap the number of
   live goals?
4. **Where `goal_create` comes from.** A tool the agent calls (APEX decomposes "build a lander" into
   a goal), and/or a board "＋ New goal" affordance (Phase 3).
5. **Policy default.** `Yolo` posture only on a yolo node? Or always require explicit opt-in to
   unattended auto-approve?

---

## Build slices

- **P2a — driver skeleton.** `Goal` + `GoalState`, the driver task, one goal end-to-end: `goal_create`
  → steps via the gate → `max_steps`/failure-breaker guards → `Done`/`Failed`. `GoalStateChanged` →
  a goal lane on the board. No `goal_step` yet (each step just re-prompts "continue the goal").
- **P2b — the `goal_step` hook.** The virtual tool + mpsc to the driver; LLM-driven `continue/done/
  blocked`; the advisory-done model.
- **P2c — posture + failure-visibility.** `GoalPosture` (revive `inherit_mode`), `AskBlocks` →
  board-blocked cards, FAILED sub-agent/step cards with error text. (The standalone board fix can
  land here or earlier.)
- **P2d — persistence + resume.** `goals.json`, boot reload, Cerebro episode wrap.

Each slice is its own PR. P2a is the keystone; the rest layer on without rewrites.
