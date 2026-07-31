# Evaluation — the Conductor/Child Loop PRD as a coding-task driver for ApexOS-RS

**Scope:** assess the PRD (`session-orchestration-loop-prd.md`) against the actual tree at `buckster123/ApexOS-RS` (cloned 2026-07-30, v0.1.0-beta line), reusing the existing goal driver and state machines where they fit, and specifying new code where they don't.

## 1. Verdict

**High usefulness as a design target; adopt with four amendments — not as a literal spec.** The PRD names, precisely, the missing tier in ApexOS-RS's execution model: something between a **goal** (persistent, resumable, but strictly serial — one gated turn per step) and an **agent_spawn** (parallel-capable but ephemeral, blocking, one child per call, ≤300 s). For coding tasks — "fix these 9 clippy lints in parallel, then integrate and run the workspace tests" — neither tier works today: goals can't fan out beyond short-lived blocking spawns inside a step, and spawns can't survive a step, be messaged, or leave durable evidence. The PRD's loop is exactly that missing tier.

Better still, its three "forbidden behavior" rules are already house law here, two of them structurally enforced. What's genuinely new is small and additive — one driver (a `goal.rs` sibling), one virtual tool, two events, one persistence file — which is the exact shape the repo's own goal-driver design doc used for its "reused vs. new" budget.

The amendments (detailed in §6): the fixed cap of 8 becomes tier-aware; idle/parked become **memory-residency** states (RAM vs evicted-to-JSONL), which is what actually matters on Pi-class nodes; inline return is demoted to short batches (async is the coding path, for reasons measurable in the tree); and `agent://` / `history://` map onto the existing artifacts (an outputs file + the session JSONL + a Cerebro episode) rather than a new URI scheme.

## 2. Ground truth — what the tree already has

The relevant machinery, with receipts:

**The bus is the hub.** `core/bus.rs` is a single mpsc→fold→broadcast event bus; `SystemState::apply` (`core/state.rs`) is a pure event fold over the whole session tree (parent links, `spawned` children, `subtree()` cancel cascade). The PRD's three hub capability groups already ride this one surface: *messaging* = `send_to_agent` → `Event::AgentMessage {from,to,body}` → routed as a `UserPrompt` into the target session (main.rs ~1667) — i.e. **a send literally wakes a session**; *jobs* = the goal tools (`goal_create/step/resume/cancel/list_goals`) plus the scheduler's crons and wakeups; *process supervision* = the plugin supervisor (`plugins/supervisor.rs`, start/logs/restart-policy/stop). PRD FR-7 is ~already satisfied.

**The goal driver is the conductor loop, minus fan-out.** `agentd/goal.rs`: `GoalState {Acting, Done, Blocked, Failed, Cancelled}`; deterministic control plane ("LLM-proposes / code-disposes"): a `max_steps` budget (default 12, ceiling 100), a per-step stall timeout (900 s, env-tunable with a 30 s floor), verdicts via the `goal_step{continue|done|blocked}` tool applied on `TurnComplete`, persistence to `goals.json` with restart→`Blocked{"interrupted by daemon restart"}` and `goal_resume`, `ApprovalPending`→park, goal-scoped yolo, and a Cerebro episode wrapping each run. This is the PRD's Main in all but one respect: it drives **one** session. The design doc's own reuse table (`docs/ideas/goal-driver-design.md` §"What's reused vs. new") lists fan-out as "`agent_spawn` (fan-out within a step)" — bounded, as shown next.

**agent_spawn is the PRD's forbidden pattern, institutionalized.** `supervisor.rs` (~1940): one child per call; the parent's tool call **blocks** until the child's `ToolResult` arrives, timeout default 90 s, clamped 5–300 s. `main.rs` `child_turn` (~2238): the child runs **one turn**, its final text returns as the parent's tool result, then `tracker.finish_child` tears it down. Spawn sessions live in a reserved id range (`SPAWN_SESSION_BASE`, `core/identity.rs`) and are deliberately **not persisted** (`session_store.rs`: "Ephemeral spawn sessions are not persisted"). So today's only parallelism primitive is spawn-one-then-wait with a five-minute ceiling — PB-1 is not just violated, it's the design. (To be fair to the tree: for its intended use — a quick cross-node lookup inside a step — it's the right tool. It's the wrong tool for a batch of coding tasks, which is the PRD's point.)

**Isolation is already stronger than the PRD asks.** The child's history is exactly `[User{prompt}]` — parent history is structurally unreachable (PB-2 enforced by construction, `main.rs` ~1648). And H6 "task-scoping by subtraction" (BACKLOG, shipped 2026-07-09) went further than the PRD's "carries: context · local:// · skills · plan ref": with no explicit `system`, a child gets a **minimal task charter** instead of the parental soul; `inherit_soul:true` is the deliberate, explicit widening; spawn-minted Cerebro memories are system-stamped `spawn-derived`. The PRD's carry list composes cleanly on top: charter = system prompt; carry payload = `{context, files, plan_ref, skills}`, where **skills → Cerebro procedure ids** and **plan_ref → a Cerebro intention/episode id** — a genuinely nice unification of the PRD's vocabulary with the cortex that already exists.

**Concurrency is gated at a different layer.** `TurnEngine` (`agent/turn.rs`) holds a global `Semaphore(16)` (`main.rs` ~467) — but the permit is held **only across the provider API call**, not across tool execution or approval waits (turn.rs ~225, deliberate). `TurnGate` (main.rs ~3449) serializes turns per session with FIFO queueing. So there is a *turn/API* gate, but no *session-admission* gate — nothing plays the PRD's `sem ≤ 8` role, and nothing queues a batch.

**Persistence and transcripts exist for everyone except children.** `session_store.rs` appends every session to `logs/sessions/<id>.jsonl` (the `history://` analog), with `core/history.rs` providing window-trimming + load-time integrity repair. Spawn sessions are the one exclusion. There is no `agent://` analog at all: a child's output exists only inside the parent's tool result, then evaporates.

**Precedents for everything the new tier needs:** the council (`agent/council.rs`) proves concurrent multi-model rounds; the scheduler's wakeups (`scheduler.rs`) prove capped, persisted, time-based self-revival (min delay 60 s, 90-day horizon, pending cap 16, daily cap 24); the self-update pipeline proves the build→test→adversarial-review verify gate the conductor's "verify" phase wants to reuse.

## 3. Requirement-by-requirement fit

| PRD requirement | ApexOS-RS today | Fit |
|---|---|---|
| FR-1 `task(tasks[])`, 1 call → N, inline/async | `agent_spawn`: 1 call → 1, blocking only | **Gap #1 — build** |
| FR-2 admission semaphore ≤ 8, overflow queues | Turn/API `Semaphore(16)` only; no session gate, no queue | **Gap — build (tier-aware, §6.1)** |
| FR-3 spawn-time-only carry | H6 charter + fresh history; carry fields partially exist (`prompt`, `system`, `inherit_soul`) | Aligned — extend with `{files, plan_ref, skills}` |
| FR-4 / PB-2 no parent history | Structural: child history = `[prompt]` | **Already satisfied** |
| FR-5 running→idle→parked, TTL, survives yield | Children torn down after one turn; only goals persist/park | **Gap #2 — build** (as goal generalization, §5) |
| FR-6 / PB-3 hub send wakes/revives; no isolated revive | `send_to_agent` → `UserPrompt` wakes any *live* session; `goal_resume` revives goals; no parked children exist | Partial — make revive-on-send the only Parked→Running edge |
| FR-7 single hub (messaging/jobs/supervision) | The bus + A2A + goal/schedule tools + plugin supervisor | **Already satisfied** |
| FR-8 inline return · async-result | Inline exists (`ToolResult`); no async push of child results | Partial — async is the important half (§6.3) |
| FR-9 `agent://` output, primary evidence | None — output lives only in the parent's tool result | **Gap #3 — build** |
| FR-10 `history://` incl. parked spans | `sessions/<id>.jsonl` for all *except* spawns | Extend to the new worker class |
| FR-11 split·integrate·verify conductor | Goal driver drives; no structured integrate/verify phase | Build as directives + a batch-await posture |
| PB-1 no spawn-one-then-wait | The only current pattern | Fix economically + by directive (§7) |

The three real gaps, then: **(1) batch fan-out with admission, (2) persistent/parkable workers, (3) durable output artifacts as verification evidence.** Everything else is either present, or a thin adapter.

## 4. Why this materially helps *coding* tasks specifically

Coding work is where the current shape hurts most. A refactor across a Cargo workspace decomposes naturally into N independent edits + one integration/verify pass — the PRD's split/integrate/verify triangle. Today a goal doing this must serialize edits across steps (12-step default budget burns fast) or fire ≤300 s blocking spawns whose transcripts and diffs vanish. With the worker tier: the conductor goal fans out one `task{...}` batch; each worker holds one file/module in an isolated session with only its charter + carry (small context = cheaper + more focused, per the apex1 field data that motivated H6); outputs land as artifacts the conductor **must read** to integrate; and "verify" is the shape the repo already trusts — `cargo build && cargo test` as a hard gate, the self-update pipeline one level down. Parked workers add a second coding-specific win: a worker blocked on a failing test can be evicted for hours and revived by a single `send_to_agent` when the conductor has new information — no context held hot, no goal budget burned while waiting.

## 5. Design recommendation — build workers as *goals with parents*, not as persistent spawns

The tempting move — make spawn sessions persistent — is the wrong one. Spawn ephemerality is load-bearing (persist-skip and `spawn-derived` provenance both key on the id range; the H6 charter's "honest ephemerality" wording assumes it). Keep `agent_spawn` exactly as is, for what it's for.

Instead, the PRD's Child maps almost 1:1 onto a **generalized goal**: a worker = a goal with a parent session, an admission gate, memory-residency states, and an output artifact. This reuses the entire hardened goal pattern — persistence + restart recovery, stall timeout, verdict tool, approval-park, scoped yolo, Cerebro episode, board card — and keeps the driver additive, exactly like `goal.rs` itself was.

Concretely (house style throughout — pure helpers for tests, env tunables with clamped floors, serde defaults for legacy files):

**New session class** (`core/identity.rs`):

```rust
/// Worker sessions: persistent parallel children (the PRD's "Child × N").
/// A distinct range so (a) session_store DOES persist them (unlike spawns),
/// (b) the worker driver can claim their bus events, (c) Cerebro provenance
/// can stamp `worker-derived` the way spawns stamp `spawn-derived`.
pub const WORKER_SESSION_BASE: u64 = /* above SPAWN_SESSION_BASE's range */;
pub fn is_worker_session(id: u64) -> bool { id >= WORKER_SESSION_BASE }
```

**State machine** (`core`, next to `GoalState`):

```rust
/// Worker lifecycle. Idle vs Parked is MEMORY RESIDENCY, not just time:
/// Idle = yielded, no in-flight turn, history resident in RAM (wake is free);
/// Parked = TTL-evicted, history lives only in sessions/<id>.jsonl (revive
/// reloads + repair_history's it, the boot path's exact code). On a Pi this
/// distinction is the whole point of having both states.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerState { Queued, Running, Idle, Parked, Done, Failed, Cancelled }
```

**Events** (twins of `GoalStateChanged`, so the board/WS path lifts unchanged):

```rust
Event::WorkerStateChanged { worker: WorkerId, parent: SessionId, state: WorkerState,
                            step: u32, max_steps: u32, detail: String },
Event::TaskBatchDone      { batch: BatchId, parent: SessionId,
                            done: u32, failed: u32, outputs: Vec<String> /* paths */ },
```

**The fan-out tool** (virtual, `gather_tools` sibling of `goal_create_spec`):

```rust
pub fn task_fanout_spec() -> ToolSpec {
    ToolSpec {
        name: "task".into(),
        description: "Fan a BATCH of tasks out to parallel persistent workers — one call → N \
            isolated child sessions, admitted through the worker gate (cap per node tier; \
            overflow queues FIFO and admits as slots free). Each worker carries ONLY what you \
            pass: `prompt` (required), optional `system` (else the minimal task charter), \
            `files` (workspace paths), `plan_ref` (a Cerebro intention/episode id), `skills` \
            (Cerebro procedure ids) — NEVER this session's history. mode:'async' (default, \
            recommended) returns worker ids now; one batch report arrives when all workers are \
            terminal, listing each output PATH — read the ones you need, don't paste them all. \
            mode:'inline' blocks and is for SHORT batches only. Prefer ONE task call with many \
            tasks over spawning workers one at a time.".into(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "tasks": { "type": "array", "items": { "type": "object",
                    "properties": {
                        "prompt":   { "type": "string" },
                        "system":   { "type": "string" },
                        "files":    { "type": "array", "items": { "type": "string" } },
                        "plan_ref": { "type": "string" },
                        "skills":   { "type": "array", "items": { "type": "string" } },
                        "node":     { "type": "string", "description": "mesh peer (phase 2)" }
                    }, "required": ["prompt"] } },
                "mode":      { "type": "string", "enum": ["async", "inline"] },
                "max_steps": { "type": "integer", "description": "per-worker step budget (default 6)" }
            },
            "required": ["tasks"]
        }),
    }
}
```

**Admission** — pure and unit-testable, `parse_step_timeout`-style:

```rust
/// Split a batch under the worker cap: (admit_now, queue). The cap gates worker
/// SESSIONS; the TurnEngine Semaphore(16) still gates provider concurrency
/// underneath. Two layers, deliberately — document cap ≤ turn-sem as the sane
/// configuration so admitted workers can't starve each other's API calls.
fn admit(batch: usize, running: usize, cap: usize) -> (usize, usize) {
    let now = batch.min(cap.saturating_sub(running));
    (now, batch - now)
}

/// AGENTD_WORKER_CAP, clamped ≥1 (a typo can't wedge fan-out to zero); default
/// comes from the hardware tier the embodiment block already detects —
/// micro: 2, standard: 4, gpu: 8. The PRD's "8" survives only as the top tier.
fn worker_cap_from_env(tier_default: usize) -> usize { /* env → parse → max(1) */ }
```

**The driver** (`agentd/worker.rs`, a `spawn_goal_driver` sibling — same select-loop skeleton):

```rust
/// Code-disposes control plane for the worker tier. Owns: admission + FIFO queue,
/// per-worker step loop (goal.rs's advance(), generalized with a parent), TTL
/// eviction, revive-on-send, workers.json persistence (restart: Running→Parked —
/// never lost, revive-able by one message), and output artifacts on terminal states.
pub fn spawn_worker_driver(/* bus, bcast_rx, req_rx, ids, paths, proxy, cap */) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(30));
        loop { tokio::select! {
            Some((session, call_id, tool, args)) = req_rx.recv() => match tool.as_str() {
                "task"          => fan_out(..).await,        // admit ≤ cap, queue rest, persist
                "worker_report" => record_report(..).await,  // goal_step's sibling, see below
                "list_workers"  => list(..).await,
                "worker_cancel" => cancel(..).await,         // UserCancel → cascade, artifact "cancelled"
                _ => {}
            },
            ev = bcast_rx.recv() => match ev {
                Ok(Event::TurnComplete { session }) if is_worker_session(session.0) =>
                    advance(..).await,        // verdict → next step | Done(write output) | Idle | Blocked
                Ok(Event::UserPrompt { session, .. }) if is_worker_session(session.0) =>
                    revive_if_parked(..).await,  // PB-3 lives HERE — see below
                Ok(Event::ApprovalPending { session, call }) =>
                    park_on_approval(..).await,  // goal.rs pattern, verbatim
                _ => {}
            },
            _ = tick.tick() => {
                park_idle_past_ttl(..).await;   // Idle → Parked: evict RAM history (JSONL is truth)
                fail_stalled(..).await;          // goal.rs's stall breaker, per worker
                admit_queued(..).await;          // freed slots pull from the FIFO
                maybe_finish_batches(..).await;  // all terminal → TaskBatchDone + report to parent
            }
        }}
    });
}
```

**The worker's verdict tool** — `goal_step`'s mirror, with the PRD's yield made explicit:

```rust
/// worker_report{status: continue|done|blocked|yield, summary?, artifacts?[]}
/// continue (default when not called) = next step, like a goal — a coding worker
/// keeps driving until done or budget. `yield` = the PRD's hidden edge, explicit:
/// go Idle and await a send (use when you need the conductor's input). `done`
/// REQUIRES `summary` and may declare `artifacts` (workspace-confined paths) —
/// these become the output record. Yield is "hidden" in the PRD's sense: it emits
/// no batch event, only the board state change.
```

**Revive-on-send** — the PRD's sharpest rule, made structural:

```rust
/// PB-3 is enforced by construction: this is the ONLY code path that flips
/// Parked → Running, and it fires exclusively off a bus message
/// (send_to_agent → AgentMessage → UserPrompt into the worker's session).
/// Revive = reload sessions/<id>.jsonl through repair_history (the boot path's
/// exact code) into the in-memory histories BEFORE the TurnGate admits the
/// prompt — the message that woke the worker is the first thing it processes.
/// A `worker_resume` convenience tool exists but is sugar: it SENDS.
async fn revive_if_parked(..) { .. }
```

**The output artifact** — `agent://<id>` in the tree's idiom (paths + events, no new URI scheme):

```rust
/// logs/agents/<worker_id>.json — the durable output record ("agent://").
/// The batch report hands the conductor PATHS, not payloads: evidence is read on
/// demand (context budget), which is also what makes "verify" honest — the
/// conductor must actually open the file. The worker's Cerebro episode (goal.rs's
/// episode_start/end pattern, valence by outcome) is the recallable twin.
#[derive(Serialize, Deserialize)]
struct WorkerOutput {
    worker: u64, session: u64, parent: u64, batch: u64,
    ok: bool, summary: String, artifacts: Vec<String>,
    steps: u32, ended_at: u64,
}
```

**Conductor integration** (the one touch inside `goal.rs`): a goal step that calls `task{mode:"async"}` must not stall-fail at 900 s while the batch runs. Add the design doc's dormant `GoalPosture` as `AwaitingBatch{batch}`: the stall clock pauses; `TaskBatchDone` re-prompts the goal with the integrate/verify directive:

```rust
fn directive_integrate(objective: &str, outputs: &[(u64, bool, String)]) -> String {
    // Lists (worker, ok, output_path) — paths, not contents. Instructs: read what
    // you need; run the workspace verify gate (cargo build && cargo test — the
    // self-update build→test shape, one level down); then goal_step{done} or fan
    // out a FIX batch for the failures. Failed workers are data, not exceptions.
}
```

## 6. Amendments to the PRD for this codebase

**6.1 — `sem ≤ 8` → tier-aware `AGENTD_WORKER_CAP`.** ApexOS spans Pi Zero → DGX; a constant is wrong at both ends. Tier defaults (micro 2 / standard 4 / gpu 8) from the existing hardware-tier detection, env-overridable, floor 1. Additionally: on a local-model node, 8 concurrent worker contexts is a KV-cache/RAM statement, not a scheduling one — the cap doc must say so.

**6.2 — Idle/Parked = memory residency.** The PRD treats them as a time gradient. Here, a session with no in-flight turn already costs ~nothing except its RAM history — so make the TTL edge mean *eviction* (drop the in-memory history; the JSONL is truth), and revive mean *reload + repair*. This turns the lifecycle into something a 2 GB node actually needs, and it reuses `history.rs` instead of inventing state.

**6.3 — Demote inline mode.** Measured against the tree: an inline `task()` inside a goal step collides with the 900 s step-stall breaker, and any inline call is bounded by the 1800 s tool-result timeout (turn.rs). The API-permit design (held only across provider calls) means inline *won't* deadlock the semaphore — it's merely the worse mode. Async + `AwaitingBatch` + `TaskBatchDone` is the coding path; inline stays for sub-minute batches, documented as such in the tool spec.

**6.4 — No new URI scheme.** `agent://<id>` → `logs/agents/<id>.json` + a Cerebro episode; `history://<id>` → the existing `sessions/<id>.jsonl`, now *including* worker sessions (spawns stay excluded — that exclusion is a feature). The PRD's "primary evidence" clause survives intact and is the best idea in it: the conductor verifies against the artifact, not the return value.

**6.5 — Depth-1 fan-out in v1.** Workers do not get the `task` tool (subtraction, the H6 move) — no nested batches, no recursion hazard, mirroring the spawn `max_depth` guard. Revisit only with field data.

**6.6 — Phase 2 is the colony.** Per-task `node` routes a worker to a mesh peer (the `mesh_agent_spawn` precedent + the per-node A2A threads already exist) — a Pi conductor fanning coding tasks to the GPU box is where this PRD stops being an abstraction and becomes the distributed build farm. Deferred because remote worker *persistence* (who owns the parked state, who revives across a peer restart) is its own design.

## 7. The forbidden behaviors, enforced honestly

**PB-2 (no parent history):** already structural for spawns; workers inherit the same construction (fresh history = charter + carry). `inherit_soul` remains the single, explicit widening. Nothing to build.

**PB-3 (no isolated revive):** structural in the proposed driver — one code path, message-triggered (§5). `worker_resume` is sugar that sends.

**PB-1 (no spawn-one-then-wait):** the only one that can't be *proven* mechanically — a model can always serialize. Enforce it economically and by authorship, the repo's own pattern: (a) make `task()` the cheap path (one call, no per-child 300 s ceiling); (b) add an `EXECUTION_DISCIPLINE`-style line to goal directives ("parallelizable work goes through ONE task{} batch — never a spawn-then-wait chain"); (c) optionally, a soft breaker in the driver: ≥3 sequential `agent_spawn` calls inside one goal emits a board-visible nudge. `agent_spawn` itself is not deprecated — its own spec already scopes it to blocking lookups.

## 8. Risks and open questions (grounded)

**Approval storms.** Eight workers each hitting an ask-gated tool = eight Blocked cards and a dead batch. Mitigation: batch-scoped yolo as an explicit `task{yolo:"inherit"}` — workers inherit the *parent goal's* yolo bit and never more (the goal-scoped-yolo strictness, one level down). Default off.

**Token/RAM pressure.** N workers × context on a local model is the real cap, not the semaphore. Worker `max_steps` defaults low (6), charters stay minimal (H6), `history.rs` trimming applies per worker. The cap doc should name the failure mode.

**Queue fairness.** v1 is one global FIFO; two concurrent conductor goals can starve each other's batches. Acceptable at current scale; per-parent round-robin is a contained follow-up.

**Board/UI surface.** `WorkerStateChanged` needs a lane; the `GoalStateChanged` rendering path is the template, and the WS session-scoping fix (BACKLOG, done) already classifies per-session events — the twin slots in.

**Open:** does a Parked worker release its admission slot? Recommendation: **yes** (parked = evicted = costless; the slot gates *running* residency), which also answers the PRD's own §12.3 for this codebase. And: should `TaskBatchDone` fire on all-terminal or first-failure? Recommendation: all-terminal — failed workers are integration data for the fix batch, matching the "verify honestly" stance.

## 9. Slice plan (goal-driver P2a/2b house style)

**W1a** — session class + `WorkerState` + driver skeleton + `task{mode:"async"}` + events + `workers.json` (restart: Running→Parked) + board twin. Depth-1, no yolo inherit, env cap.
**W1b** — `worker_report` verdicts incl. `yield` + TTL eviction + revive-on-send (+ `repair_history` on reload) + slot release on park.
**W1c** — output artifacts + Cerebro episodes + `TaskBatchDone` + `AwaitingBatch` posture in `goal.rs` + the integrate/verify directive.
**W1d** — batch-scoped yolo inherit + `worker_cancel`/batch cancel cascade + bounded inline mode + the PB-1 soft breaker.
**W2** — mesh workers (`node` per task): the colony as the worker pool.

## 10. Bottom line

The PRD earns its place: it isn't describing a foreign architecture, it's naming the next rung of the ladder this codebase is already climbing — the goal driver gave one session a deterministic loop; this gives that loop hands. Its lasting contributions here are the **admission-gated batch primitive**, the **parked-but-revivable worker**, and above all the **evidence rule** (outputs are artifacts the conductor must read, not return values it may trust). Its overreaches — the literal 8, the URI schemes, inline-as-peer-of-async, persistent-children-as-modified-spawns — all have cheaper native answers in the tree. Build it as §5/§9: one additive driver in the house pattern, and the loop in the diagram becomes the coding engine the goal driver was always one fan-out short of being.



My notes (Andre):

First of all this need eval against the full code where applicable like anything entering the eco-system, brainstormed with incognito instance (i a new way that worked really well hehe, tell you more later)...not a plan but a draft at best, which we refine and iterate until it is exactly what we need, before any code is written into ApexOS-RS and comitted.

ApexCode(r) mode w/diff sys-prompt, or just part of the toolkit and loads good general "coder" instructions inline, with specialization in proc/skills in cerebro or other sources?

Making the working board tap into this for viz, or sep gui/prog-display? Something that enables interventions by me when needed, for permissions, forks even if subs should never experience hard forks that the code and logic cant handle. Kind of the emergency entrance to anywhere in the worktree(s) so to speak.




