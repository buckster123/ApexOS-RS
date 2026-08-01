# PRD — Conductor / Child Session Orchestration Loop

**Status:** Draft v0.1 — preserved brainstorm input, superseded by `docs/fabrica.md` (locked 2026-07-31; the W tier shipped `#306`–`#311`) · **Source:** reverse-engineered from the "SESSION" architecture diagram · **Date:** 2026-07-30

## 1. Summary

This PRD specifies the orchestration loop shown in the source diagram. A single conductor session (**Main**) splits an objective into a task batch, fans the batch out to N persistent, isolated child sessions through one gated call, coordinates all runtime interaction through a single **hub**, and closes the loop by reading durable child outputs back as evidence. Children do not die when they stop talking: they yield into an idle state, park after a TTL, and can be brought back at any time by a hub message. Every child leaves its final output at `agent://<id>` and its transcript at `history://<id>`, so the loop's results outlive the sessions that produced them.

## 2. Background and problem

Multi-agent fan-out fails in predictable ways. Unbounded concurrency exhausts capacity. Serialized spawn-and-wait orchestration forfeits the parallelism that justified fanning out in the first place. Implicit context inheritance leaks the parent's entire history into every worker, bloating context and making runs irreproducible. And results that live only inside a child's conversation vanish when the child does. The design in scope answers each failure with a structural rule: an admission semaphore, a mandatory batch fan-out primitive, spawn-time-only context transfer, and addressable stores for outputs and transcripts.

## 3. Goals

1. One call fans out an entire batch: `task(tasks[])` creates up to N children per invocation, in inline or async mode.
2. Concurrency is bounded and graceful: at most 8 sessions admitted at once, with overflow queued rather than rejected.
3. Children are persistent and cheap to resume: running → idle → parked, with wake or revive triggered by a single hub send.
4. Context transfer is explicit, minimal, and happens exactly once, at spawn time.
5. One control surface — the hub — covers messaging, job control, and process supervision for Main and every child.
6. Every child leaves durable, addressable evidence: final output at `agent://<id>`, full transcript at `history://<id>`.

## 4. Non-goals (explicitly out of scope)

1. Inheriting or streaming parent history into children (forbidden; see PB-2).
2. Reviving parked sessions through any channel other than the hub (forbidden; see PB-3).
3. Serialized orchestration — spawn one child, wait for it, spawn the next (forbidden; see PB-1).

## 5. Roles and terminology

| Term | Meaning in this system |
|---|---|
| Main | The conductor session. Duties: split, integrate, verify. |
| Session gate | Semaphore capping admitted sessions at ≤ 8; excess submissions queue. |
| `task(tasks[])` | Batch fan-out primitive. One call → N children; runs inline or async. |
| Child × N | Persistent isolated session spawned per task. States: running, idle, parked. |
| Hub | The single runtime surface shared by Main and all children: messaging, jobs, process supervision. |
| `agent://<id>` | Durable location of a child's final output; Main's primary evidence. |
| `history://<id>` | Durable transcript of a session, spanning both live and parked periods. |
| Yield | Hidden (non-surfaced) transition from running to idle; the session survives it. |
| TTL | Idle timeout; on expiry the child parks. |

## 6. The loop, end to end

1. **Split.** Main decomposes the objective into `tasks[]`.
2. **Gate.** `tasks[]` passes through the session gate. Up to 8 sessions are admitted; the remainder queue and are admitted as capacity frees.
3. **Fan out.** A single `task(tasks[])` call spawns the admitted children, either inline (Main receives results in the call) or async (Main continues and results arrive later).
4. **Spawn.** Each child starts in `running`, carrying only its explicit spawn payload: context, `local://` file references, skills, and a plan reference. Nothing else crosses the boundary at spawn, and nothing crosses it afterward.
5. **Work and settle.** The child works, appending to `history://<id>` as it goes. When it yields — a hidden transition — it becomes idle; when its idle TTL expires it becomes parked. Neither transition destroys the session.
6. **Coordinate.** Main and children interact exclusively through the hub, in both directions. A hub send to an idle child wakes it; a hub send to a parked child revives it (revive-on-send).
7. **Return.** Results reach Main either as an inline return (sync mode) or as an async-result push (async mode).
8. **Publish.** Each child publishes its final output to `agent://<id>`.
9. **Integrate and verify.** Main reads `agent://<id>` as its primary evidence, integrates the batch, verifies it, and either finishes or emits the next `tasks[]` — restarting the loop.

## 7. Functional requirements

**FR-1 — Batch fan-out.** The system shall provide `task(tasks[])`, accepting an array of task specifications. A single call shall create up to N child sessions. The call shall support two execution modes: inline (results returned within the call) and async (call returns immediately; results delivered later per FR-8).

**FR-2 — Admission control.** A semaphore shall cap concurrently admitted sessions at 8. Tasks submitted above the cap shall be queued, not rejected, and admitted automatically as capacity frees.

**FR-3 — Spawn payload, spawn-time only.** A spawn shall carry only explicitly supplied material: context, `local://` file references, skills, and a plan reference. The transfer occurs once, at spawn time; there is no implicit post-spawn context synchronization.

**FR-4 — Isolation.** Each child is a persistent, isolated session. Children shall not receive the parent's conversation history in any form (hard constraint; see PB-2).

**FR-5 — Lifecycle.** A child has exactly three states: running, idle, parked. Running → idle occurs on yield, a hidden transition. Idle → parked occurs on TTL expiry. No lifecycle transition destroys the session or its state.

**FR-6 — Wake and revive.** A hub send addressed to an idle child shall return it to running (wake). A hub send addressed to a parked child shall return it to running (revive-on-send). No other revival mechanism shall exist (see PB-3).

**FR-7 — Single hub surface.** One hub shall serve as the sole runtime surface, exposing three capability groups: messaging (DM, list, revive-on-send), jobs (wait, cancel, async push), and process supervision (start, logs, stop). Main and every child shall each hold a bidirectional hub channel; no side channels are provided.

**FR-8 — Return paths.** Child results shall reach Main by exactly two paths: inline return, for inline-mode calls, and async-result push, for async-mode calls.

**FR-9 — Output publication.** Each child shall publish its final output to `agent://<id>`. Main shall read `agent://<id>` and treat it as the primary evidence during integration and verification.

**FR-10 — Transcripts.** Each session shall append its transcript to `history://<id>` continuously, covering both live and parked periods, so the record is gapless across the full lifecycle.

**FR-11 — Conductor duties.** Main is responsible for the three conductor functions named in the diagram: split (decompose into `tasks[]`), integrate (combine child results), and verify (validate against `agent://` evidence).

## 8. Prohibited behaviors (hard constraints)

These appear in the diagram as forbidden (red, stop-marked) edges and are requirements in their own right.

**PB-1 — No spawn-one-then-wait.** Main must not spawn a single child and block on it as its orchestration pattern. Parallel work goes through batch fan-out (`task(tasks[])`); serializing spawns defeats the loop's purpose.

**PB-2 — No parent-history transfer.** Parent conversation history must never be passed to, inherited by, or reconstructed inside a child. The spawn payload of FR-3 is the entire inbound context.

**PB-3 — No isolated revive.** A parked child must not be revived outside the hub. The only sanctioned revival path is a hub send (revive-on-send). Any other revival attempt shall fail.

## 9. Child lifecycle state machine

| From | Event | To | Notes |
|---|---|---|---|
| (none) | admitted + spawned | running | Carries spawn payload only (FR-3) |
| running | yield (hidden) | idle | Session persists |
| idle | hub send | running | Wake |
| idle | TTL expiry | parked | Session persists; transcript retained |
| parked | hub send | running | Revive-on-send; the only sanctioned revival |
| parked | any non-hub revival | — | Forbidden (PB-3) |

## 10. Data and addressing

`agent://<id>` holds a child's final output. The write path is the child's publish step; the read path is Main's read step. This store is designated Main's primary evidence, meaning verification is grounded in what was published there, not merely in what came back over the return path.

`history://<id>` holds the session transcript. It is written throughout the session's life and explicitly spans both live and parked periods, so a parked child's record remains complete and inspectable while it sleeps.

## 11. Acceptance criteria

1. Calling `task()` with 12 tasks yields 8 running children and 4 queued tasks; the queued tasks admit as slots free, and queuing alone surfaces no error.
2. A child that yields is observable as idle; after its TTL it is observable as parked. A hub DM at either stage returns it to running with its prior session state intact.
3. Inspecting a fresh child's context shows exactly the spawn payload — context, `local://` refs, skills, plan ref — and zero parent messages.
4. No API path revives a parked child except a hub send; all other attempts fail.
5. After a batch completes, every child has a final output at `agent://<id>` and a gapless transcript at `history://<id>`, including parked spans.
6. Main's integration and verification steps demonstrably read `agent://<id>` rather than relying solely on in-conversation returns.
7. Review of Main's orchestration shows batch fan-out; a spawn-then-block-on-one pattern is rejected.

## 12. Open questions

The diagram fixes the structure but leaves these parameters unspecified; they need decisions before implementation.

1. TTL duration, and whether it is configurable per child or global.
2. Queue discipline above the 8-session cap (FIFO assumed) and the semaphore's scope (global vs per-Main).
3. Whether a parked session releases its semaphore slot, or only a completed one.
4. Failure semantics: what happens on child crash — retry policy, and how the failure surfaces to Main.
5. Cancellation semantics for partially completed batches (the hub's jobs group exposes cancel; propagation rules are unstated).
6. Whether the cap of 8 is fixed or tunable, and any size limits on the spawn-time context payload.

## Appendix A — Diagram-edge → requirement traceability

| Edge (per diagram legend) | Meaning | Requirement |
|---|---|---|
| Heavy cyan: Main → semaphore → task → Child | Primary fan-out path | FR-1, FR-2, FR-3 |
| Green / gray: yield and TTL lifecycle | Lifecycle decay without termination | FR-5 |
| Amber loops: hub send and revive | Wake/revive; Main ⇄ hub; Child ⇄ hub | FR-6, FR-7 |
| Dashed cyan: inline or async return | Result return paths | FR-8 |
| Fine red + stop: forbidden transfer or behavior | spawn-one-then-wait, parent-history, isolated revive | PB-1, PB-2, PB-3 |
