# Evaluating "State Machines for ApexOS Agents" — verdict + reshaped design

> Evaluation of [`state-machine.md`](state-machine.md) (a Grok research sketch) against the
> **actual** ApexOS-RS codebase. Context: the rise of autonomous "goal/loop" runners for overnight
> no-human-in-the-loop work, and the "if you like loops, wait until you hear about state machines"
> thread. Also folds in André's standing idea: a **kanban-style work-board** for multi-agent +
> loop/yolo runs.
>
> **TL;DR — right instinct, wrong blueprint.** We already do state machines in several places; the
> genuine gap is a *first-class, observable, bounded, resumable autonomous-run object* + a board to
> watch it. The correct version is **much smaller** than Grok's new-FSM-crate, and it should be
> **board-first**.

---

## Verdict (BLUF)

- **Pursue the concept** — it's a real capability gap *and* a strong product differentiator for the
  coding / long-workflow audience.
- **Do not build Grok's blueprint as written.** It models ApexOS as an in-process, loop-owning,
  directly-linked system. Ours is **event-reactive, bus-driven, plugin-isolated**. Following the
  sketch literally would fight the architecture, reinvent `agent_spawn`, and break turn-serialization
  safety.
- **The right shape is small:** a thin **Goal/Run orchestration object** that drives the *existing*
  reactive loop by emitting bus directives (exactly how the scheduler already works), plus a
  **work-board** that is mostly a new *view* over events we already emit.

---

## 1. We already do state machines (the existing substrate)

The instinct "we kind of do state-machines already" is correct — more than expected. These are real,
explicit, and (mostly) tested:

| Machine | Where | Governs |
|---|---|---|
| `TurnGate` (admit/complete/cancel + `TurnSlotGuard`) | `agentd/crates/agentd/src/main.rs:2559` | per-session turn lifecycle; serializes turns, FIFO-queues concurrent prompts |
| `SystemState::apply` | `agentd/crates/core/src/state.rs` | the event bus *is* a reducer: events → state transitions |
| evolution | `agentd/crates/agentd/src/evolution.rs` | propose → snapshot → apply → deferred-ack → rollback (pure, unit-tested) |
| self-update | `agentd/crates/agentd/src/self_update.rs` + `docs/self-update.md` | build → test → LLM-review → commit → health → confirm/rollback, with a privilege-boundary watchdog. The most rigorous gated FSM in the repo |
| council | `agentd/crates/agent/src/council.rs` | N personas → detect convergence → synthesize → store |
| turn loop | `agentd/crates/agent/src/turn.rs` | stream → emit ToolRequested → await ToolResult → loop → TurnComplete |
| `VastPhase` | `agentd/crates/plugins/src/vast.rs` | GPU-instance lifecycle |

**Consequence:** Grok's proposed states — `Acting`, `Reflecting`, `Evolving`, `SelfUpdating`,
`Dreaming` — are **already-existing operations**, not new code. A goal layer should *invoke* them,
not reimplement them.

---

## 2. Where the blueprint breaks against the code

Four concrete mismatches (it got the gist, missed the seams):

1. **Loop-owning vs. event-reactive.** Grok's `AgentMachine::run_loop { step }` has each agent own a
   polling loop. ApexOS spawns turns **reactively** in the router in response to `UserPrompt` events —
   from WS, sensors, a2a, **and the scheduler** (`scheduler.rs:103` is literally
   `bus.emit(Event::UserPrompt{…})`), every one gated through the `TurnGate`. A per-agent owning loop
   fights this model.
2. **`AgentContext` invents APIs we don't have.** `cerebro: Arc<CerebroCortex>` with
   `cerebro.recall_agent_state()` / `store_agent_state()` — but **Cerebro is a spawned MCP child
   process** reached via `ToolProxy` (agentd has *no* `cerebro` dependency; the only mention is a
   "wait for it to start" comment). Likewise `llm: Arc<dyn Provider>` lives inside the `TurnEngine`,
   and `mesh: Arc<MeshClient>` doesn't exist (mesh = the gateway's `PeerRegistry` + a2a over HTTP).
   Those handlers wouldn't compile against this tree.
3. **"Replace or wrap `spawn_agent_router`"** — that *is* the `TurnGate`, built deliberately to
   serialize turns and kill the concurrent-turn race (two prompts → racing history writes → corrupted
   JSONL). Wrapping it with a per-agent loop reintroduces exactly that race.
4. **`MultiAgentSupervisor`** reinvents what already exists: `agent_spawn` (local `child_turn` + the
   cross-node *blocking* spawn keystone), `SessionBindings`, the `Supervisor`.

Net: implementing it verbatim = fighting the bus/gate/plugin design, reinventing `agent_spawn`, and
weakening a safety invariant. **Reject the blueprint; keep the goals it lists.** (Grok's *goals*
section — observability, bounded loops, guards, HITL points, export-to-graph — is sound; only the
code shape is wrong.)

---

## 3. The genuine gap

The "state machine over loop" insight, stated precisely: **a loop is an implicit, uninspectable
flow; a state machine makes the autonomous run a first-class *object* you can observe, bound, pause,
resume, and rewind.**

We have the *pieces* (above) but not the *object*: there is **no `Goal` / `Run` controller** with a
state, a step budget, a termination guard, a transition history, and a UI. Autonomy today is
yolo/auto-mode + scheduled tasks + clever prompting — capable, but the *run itself* is not a thing you
can hold, watch, or stop cleanly. That object is the real upgrade.

---

## 4. The reshaped design — Goal/Run as a bus driver

A **`Goal` is a thin, persisted orchestration object that drives the existing reactive loop** — not a
new owning FSM. The driver pattern already exists and is proven: the scheduler, a2a, and sensor
alerts all advance work by **emitting the next `UserPrompt` onto the bus**. A goal loop is the same
move with a state and a guard:

```
Goal {
  id, objective, owner (session/agent),
  state,                 // Planning | Acting | Blocked(approval) | Reflecting | Done | Failed
  step_budget,           // hard ceiling on turns (overnight-run governor)
  termination_guard,     // success predicate / max-steps / cost cap
  history: Vec<(state, ts)>,
}
```

A **driver** (sibling of `scheduler.rs`):
1. emit the next directive as a `UserPrompt` (or a structured goal-step) onto the bus;
2. the **existing** `TurnGate`/router/turn-engine executes it (one goal step = one gated turn);
3. on `TurnComplete`, evaluate the result against the goal → transition state;
4. schedule the next step **or stop**: done / budget exhausted / guard tripped / blocked-on-approval.

| Concern | Reused (already in the tree) | New (small) |
|---|---|---|
| Execute a step | `TurnGate` + router + `run_turn` | — |
| "Acting / Reflecting / Evolving / Dreaming" | turn engine · `propose_evolution` · `dream_run` | — |
| Fan-out / sub-tasks | `agent_spawn` (local + cross-node) | — |
| Drive the next step | the scheduler's `emit(UserPrompt)` pattern | the Goal driver task |
| History / observability | append-only event log (JSONL) | `GoalStateChanged` bus event |
| Persistence | session JSONL + Cerebro episodes/intentions | a `goals.json` (like `mesh_sessions.json`) |

**Persistence uses what we have** — session JSONL + Cerebro episodes (the same way the rollback store
restores from episodes), *not* Grok's invented `recall_agent_state`. Resume-on-reboot rides the
existing boot path.

Why it fits: it's the **scheduler generalized** — instead of "fire a prompt at a cron time", it's
"fire the next goal-step until the guard says stop". Same bus, same gate, same turn engine.

---

## 5. The work-board (kanban) — the product prize

This is the audience magnet for overnight / multi-agent / coding runs, and **most of it is a new
*view* over events we already emit**:

| Board element | Backed by (existing) event |
|---|---|
| Card moves to "Running" | `UserPrompt` admitted → turn starts |
| Card "Blocked / needs approval" column | `ApprovalPending` |
| Sub-task cards / agent lanes | `SubAgentStarted` |
| Card "Done" | `TurnComplete` |
| "Evolved" badge on a card | `EvolutionApplied` |
| Cross-node lane | `MeshMessage` / cross-node `agent_spawn` |
| Card state + history | (new) `GoalStateChanged` + the event log |

CLAUDE.md already defers *"Sub-agent windows — Popup per child session, maps to SubAgentStarted
events"* — the board is the grown-up version of that. Columns = goal/turn states; cards =
goals/subtasks/sub-agents; lanes = agents/nodes. A **read-only board ships today** against the
existing stream, before any Goal-driver exists.

---

## 6. What NOT to build

- ❌ A new `apexos-statemachine` crate with a per-agent owning `run_loop`.
- ❌ `Arc<CerebroCortex>` / `Arc<dyn Provider>` / `MeshClient` inside an `AgentContext` (wrong process
  model; Cerebro is an MCP child, the provider lives in the TurnEngine, mesh is HTTP a2a).
- ❌ Replacing / wrapping `spawn_agent_router` (it's the `TurnGate` — keep the serialization invariant).
- ❌ Reimplementing `agent_spawn` / `SessionBindings` as a `MultiAgentSupervisor`.

---

## 7. Phasing (value early, risk low)

1. **Phase 1 — read-only work-board.** ✅ **SHIPPED** — `ui-slint/src/ui/components/work_board.slint`
   (🗂 Work Board, `AppKind::Board`): four live columns (Active · Needs-approval · Sub-agents ·
   Recent) driven entirely off the *existing* WS event stream (`AgentText`/`ToolRequested`/
   `ToolResult`/`ApprovalPending`/`TurnComplete`/`SubAgentStarted` + the global `EvolutionApplied`/
   `MeshMessage`). Zero agentd change. Single-client scope (its session + globals) is honest for
   watching one autonomous/yolo run fan out into sub-agents; god's-eye multi-session needs the
   Phase-2 board-state endpoint.
2. **Phase 2 — the Goal/Run driver.** The orchestration object + driver task (scheduler-sibling) with
   step budget + termination guard + `GoalStateChanged` event + `goals.json` persistence. The real
   autonomy upgrade. A handful of new files, zero rewrites. **Full design:
   [`goal-driver-design.md`](goal-driver-design.md)** — code control-plane + two LLM hooks
   (the work-turn + a `goal_step` tool), with the policy stance and failure-visibility lessons from
   the first live multi-agent run baked in.
3. **Phase 3 — interactive board + mesh lanes.** Drag a card = re-prioritize / pause / approve;
   cross-node lanes; rewind to a prior goal state (the event log already has the history).

---

## 8. Open questions / risks (resolve in Phase 2 design)

- **Bounding an overnight run safely.** The termination guard + step budget are the governor; they
  should compose with the policy engine and the yolo/auto posture (a goal in yolo still respects
  `ask` rules → those become `Blocked` cards, not silent stalls).
- **Goal ↔ TurnGate interaction.** One goal-step = one gated turn keeps the serialization invariant;
  a goal must not spawn turns outside the gate (the rule the gate exists to enforce).
- **Multi-goal concurrency.** Goals likely map to distinct sessions (not the always-on root session 0,
  which is the sensor/scheduler funnel). Per-session gating already serializes within a goal.
- **HITL.** `ApprovalPending` is the natural pause point; "blocked on approval" is a first-class board
  column, and resume is just answering the approval.
- **Rewind.** The append-only event log already records the transition history; "rewind" is replaying
  to a prior goal state, not new persistence machinery.

---

*This evaluation supersedes the implementation specifics in [`state-machine.md`](state-machine.md);
keep that file as the original research input. Build decisions: Phase 1 first, or design Phase 2 —
André's call.*
