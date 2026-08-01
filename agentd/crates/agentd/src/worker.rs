//! The Worker driver — Fabrica W tier, slice W1a (docs/fabrica.md).
//!
//! Workers are goals with parents: one `task_fanout` call fans a batch of N
//! tasks to N dedicated, PERSISTED sessions (the `WORKER_SESSION_BASE` id
//! class — unlike ephemeral spawns), driven through the existing `TurnGate`
//! by emitting `UserPrompt`, exactly the goal driver's pattern. Admission is
//! tier-aware (`AGENTD_WORKER_CAP`): up to `cap` workers hold a thermal slot
//! (Running/Blocked); the rest wait Queued in one global FIFO and admit as
//! slots free. Restart parks every non-terminal worker — `workers.json` +
//! `sessions/<id>.jsonl` are truth, nothing is lost (revive-on-send is W1b).
//!
//! W1b: workers report through `worker_report{continue|done|blocked|yield}` —
//! `goal_step`'s mirror. `done` requires a summary; `yield` goes Idle (history
//! in RAM, no thermal slot, a send wakes it free); a verdict-`blocked` worker
//! likewise sits slot-free awaiting input. Idle/non-inflight-Blocked workers
//! past the idle TTL park: the state event carries the eviction (the router
//! removes the RAM history; `sessions/<id>.jsonl` stays truth) and a send is
//! the ONLY Parked→Running edge (PB-3) — the router hydrates through
//! `load_one`/`repair_history`, this driver flips the state off the same
//! UserPrompt event. Revive/wake deliberately BYPASSES the admission cap:
//! a send is human/conductor intent, the emergency entrance — Queued workers
//! just wait a little longer. No report at all still means Done with the
//! final text as the deliverable (the W1a single-turn rule, now the fallback).
//!
//! W1c — the evidence rule: every terminal worker writes
//! `<log_dir>/agents/<worker_id>.json` (tmp+rename) and closes a Cerebro
//! episode; `worker_report{done}` may declare workspace-confined `artifacts`
//! (the `apexos-confine` gate — a bad path refuses the report so the model
//! retries, never a silent drop). One `task_fanout` call = one batch with a
//! `batch_deadline_s` bound: `TaskBatchDone` fires on all-terminal or at the
//! deadline with stragglers marked `timed_out` (still revivable). Rows carry
//! evidence PATHS, never payloads — integration must read the artifacts. The
//! goal driver consumes `TaskBatchDone` for its AwaitingBatch posture; this
//! driver never prompts the conductor directly.
//!
//! M1b — J and B arm. A mandala fan may now be WIDE (`tasks:[N>1]` = N ring
//! cells, one batch — the ring IS a batch, so `TaskBatchDone` is ring
//! completion for free) and may declare a JOIN: `join:"…"` mints a GATE cell
//! first and the ring UNDER it — one call, one batch, because a goal
//! conductor holds AwaitingBatch on any pending batch and a two-call
//! gate-then-fan would wedge it until the gate batch's deadline. A gate's
//! worker sits Queued with `barrier_held` (skipped by admission) until its
//! barrier opens: no open cells left in its OWN subtree (descendant-only by
//! derivation — the wait-set comes from the address prefix, nothing else is
//! expressible; nested gates are well-founded by the depth descent), or the
//! J-guard timeout. Opening appends the descendant evidence list (paths, not
//! payloads) to the gate's task — it survives park/revive — and normal FIFO
//! admission takes it. Forms mutate one bit at a time as the run grows:
//! a >1 fan arms the parent's B (SPINE→FAN, GATE→DIAMOND).
//!
//! M1b also replaces the ad-hoc stall/TTL sweeps with THE REVIEW PROCEDURE
//! (`review.rs`): per due worker, posture × six-observable word → exactly one
//! single-line remediation, applied through the SAME terminal/park paths as
//! before (behavioral identity: stall > step_timeout still Fails with the
//! same detail string, idle TTL still Parks, approval-Blocked is still
//! stall-exempt, the batch deadline stays a REPORT bound — never a kill
//! switch). Reviews are scheduled at golden offsets (Weyl — siblings can't
//! phase-lock), deadline-exact for waiting clocks, and back off on the
//! Fibonacci ladder for repeated identical quiet words (LIVE workers never
//! back off — stall latency is semantics). Terminal workers are censused
//! once and reaped from the schedule (anti-zombie); the census accumulates
//! per mandala and rides `mandala_status`. The review owns mandala CLOSURE:
//! all non-root cells terminal + conductor done (goal-terminal event, or the
//! explicit `mandala_close` for interactive conductors) → root marked done,
//! mandala closed — `open_cells` stays honest.
//!
//! M2 — cross-node rings: a mandala ring cell may carry `node` and run its
//! execution BODY on a mesh peer while the CELL — geometry, budget, barrier
//! membership, closure — never leaves this node. The remote cell is a W2
//! mirror row bound to a tree record (same worker-id counter, so binding is
//! by position like every cell); the fully-composed cell directive (axis
//! verbatim) crosses as the task text, the budget's step ceiling crosses as
//! the `steps` assignment field, and the evidence MIRROR closes the loop:
//! `sync_remote_cells` mirrors terminal wire states into the cell files so
//! a gate over a remote ring opens the tick its last mirror lands. Only
//! plain ring cells of repo-less mandalas ship out (`remote_cell_veto`):
//! gates are bindu machinery, measures need the local lap boundary, vouchers
//! need the tree, and repos don't teleport. The peer stays 100%
//! mandala-free — it hosts ordinary depth-1 workers and cannot tell a ring
//! cell from a plain task.
//!
//! Deliberate departure from goal.rs: the worker map is a PLAIN `HashMap`
//! owned by the driver task — no `Arc<Mutex<…>>`. Every access is serialized
//! through the one select loop (true for goals too — their Mutex is never
//! contended), so the lock added shape without safety. Anything outside the
//! driver that later needs worker state (a board REST endpoint, W1c batch
//! reports) goes through the request channel, the house seam.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use apexos_core::{ActionId, BatchWorkerRow, BusHandle, Event, GoalState, GoalYoloSessions, SessionId, ToolOutput, ToolSpec, WorkerId, WorkerModels, WorkerState};

use crate::mandala::{self, Addr, BudgetVec, CellForm, CellRecord, Invariant, Lattice, MandalaRecord};
use crate::remote::{self, RemoteWorker};
use crate::review::{self, Posture, Remediation, Word};
use apexos_gateway::{LivenessMap, PeerRegistry, WorkerMeshKind, WorkerMeshReq};
use apexos_plugins::ToolProxy;
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tokio::sync::{broadcast, mpsc, RwLock};

/// Hard ceiling on tasks per `task_fanout` call — one batch is one conductor
/// thought, not a job queue (PRD open question 6: v1 takes a stance).
const MAX_BATCH_TASKS: usize = 32;

/// A worker whose admitted turn produces no `TurnComplete` within this window
/// is treated as stalled (turn errored/aborted → no completion event) → Failed.
/// Override via `WORKER_STEP_TIMEOUT_SECS` (30s floor), mirroring the goal knob.
const STEP_TIMEOUT: Duration = Duration::from_secs(900);

/// An Idle (yielded) or verdict-blocked worker that receives no send within
/// this window parks: RAM history evicted, JSONL stays truth. Override via
/// `WORKER_IDLE_TTL_SECS` (60s floor).
const IDLE_TTL: Duration = Duration::from_secs(1800);

/// Step ceiling for `worker_report{continue}` loops — code disposes, exactly
/// the goal budget's rule. Override via `WORKER_MAX_STEPS` (clamped 1–100).
const DEFAULT_MAX_STEPS: u32 = 12;

/// Default batch report bound: without a deadline, one forever-parked worker
/// wedges an AwaitingBatch conductor forever (the charter's exact warning).
/// Per-call override via `task_fanout{batch_deadline_s}`, clamped 60s–24h.
const DEFAULT_BATCH_DEADLINE_S: u64 = 3600;

/// Inline mode (W1d) is short-batch-only by charter: the conductor's turn
/// BLOCKS on the fan, so both the width and the wait are hard-bounded.
const INLINE_MAX_TASKS: usize = 4;
const INLINE_DEADLINE_CEIL_S: u64 = 240;

/// PB-1 soft breaker: ≥ this many local agent_spawns from one session inside
/// the window earns a nudge — parallel work goes through ONE task_fanout
/// batch, never a spawn-then-wait chain.
const PB1_SPAWN_THRESHOLD: usize = 3;
const PB1_WINDOW: Duration = Duration::from_secs(600);

/// Default J-guard: a barrier that hasn't opened by subtree completion opens
/// at this timeout (per-call `barrier_timeout_s`, clamped 60s..cell deadline).
const DEFAULT_BARRIER_TIMEOUT_S: u64 = 1800;

/// Review cadence (M1b): each worker's review pulse period. Workers de-phase
/// at golden offsets within it; waiting clocks are additionally scheduled
/// deadline-exact, so detection lands within a tick of the deadline.
const REVIEW_PERIOD: Duration = Duration::from_secs(30);

/// The driver tick — fine-grained so golden offsets mean something. The
/// semantic clocks (step timeout, idle TTL, barrier timeout, batch deadline)
/// are unchanged; this is detection granularity, not policy.
const TICK: Duration = Duration::from_secs(5);

/// Torus epoch period (M1d): each open mandala rolls a census/fingerprint
/// epoch on this cadence (golden-offset per mandala — epochs de-phase like
/// everything else). Settled trees rest: no open cells, no epochs.
const EPOCH_PERIOD: Duration = Duration::from_secs(600);

/// The worker's reported outcome for the in-flight step (via `worker_report`),
/// applied on `TurnComplete` — `goal.rs`'s Verdict, worker vocabulary.
/// M1c: verdicts may carry a MEASURE (an R-cell's lap reading — command-
/// computed by the worker, recorded on the cell, judged by the driver).
enum Verdict {
    Continue { steer: Option<String>, measure: Option<u64> },
    Done { summary: String, artifacts: Vec<String>, measure: Option<u64> }, // summary required; artifacts confined
    Blocked(String),          // reason; sits awaiting input, slot-free
    Yield,                    // go Idle awaiting input, wake free
}

struct Worker {
    batch:   u64,
    parent:  u64,   // conductor session that fanned this worker out
    session: u64,   // dedicated session in the WORKER_SESSION_BASE range (persisted)
    task:    String,
    state:   WorkerState,
    step:    u32,   // in-flight step, 1-indexed (the goal driver's convention)
    summary: Option<String>,   // the done-verdict summary (persisted — the human-facing line)
    artifacts: Vec<String>,    // done-declared, workspace-confined paths (W1c)
    episode: Option<String>,   // Cerebro episode id wrapping this run (W1c, best-effort)
    started: Instant,          // stall clock while Running; idle/TTL clock otherwise
    pending: Option<Verdict>,  // the worker_report verdict, applied on TurnComplete
    /// Whether a turn is live on this session right now. Distinguishes the two
    /// Blocked flavors: an approval-suspended turn (inflight — holds a slot, the
    /// human's clock) vs a verdict-blocked worker (no turn — slot-free, TTL clock).
    turn_inflight: bool,
    /// Batch-inherited yolo (W1d): armed in the shared auto-approve set while
    /// live; disarmed at terminal AND at park (a revived worker re-asks —
    /// the parent's grant does not outlive the residency it was given to).
    yolo: bool,
    /// Pinned model (`task_fanout{model?}`, W1d) — None = the node default.
    model: Option<String>,
    /// The last turn errored (root_turn's Err arm emits Error+TurnComplete):
    /// a no-report completion after an error is Failed, not Done. Transient.
    errored: bool,
    /// Barrier hold (M1b): a GATE/DIAMOND cell's worker, minted Queued but
    /// invisible to admission until its barrier opens. Transient by design —
    /// a restart parks the gate, and a send to a parked or held gate is the
    /// human override: the join runs with whatever context the send carries.
    barrier_held: bool,
    /// Per-worker step ceiling (M1c): a cell worker's budget.steps IS its
    /// contract (renewals raise it); 0 = the env global (plain workers).
    /// M1a/M1b admission-checked cell steps but never enforced them per lap
    /// — this closes that gap.
    step_ceiling: u32,
    /// When this worker is next reviewed (M1b). None = off the schedule
    /// (plain-Queued/Parked are inert; terminals are censused once, then
    /// reaped — the anti-zombie rule).
    next_review: Option<Instant>,
    /// Last review's census key — a repeated identical quiet word widens the
    /// re-review interval on the Fibonacci ladder (never for Live postures).
    last_review_key: Option<String>,
    review_attempt: u32,
}

/// One batch's report bookkeeping. Persisted (batches.json) so the deadline
/// survives a restart — a parked-by-restart batch still reports, else the
/// conductor's AwaitingBatch posture would wait forever.
struct BatchMeta {
    parent:        u64,
    created_epoch: u64, // unix seconds (Instant doesn't survive restarts)
    deadline_s:    u64,
    reported:      bool,
    /// Inline mode (W1d): the blocked task_fanout call awaiting the report as
    /// its ToolResult. Transient — a restart demotes the batch to async (the
    /// caller's turn died with the daemon; there is no one left to unblock).
    inline_ack:    Option<(u64, u64)>, // (session, action id)
    /// W2: a batch HOSTED for a remote conductor — (its node_id, its batch id).
    /// When this batch reports, the rows also POST home to the origin node.
    /// Persisted: a restart must not orphan the report edge.
    origin:        Option<(String, u64)>,
}

/// W2 mesh-worker wiring handed to the driver by main.rs: the gateway request
/// arm, the conductor-side mirror file, peer resolution, beacon liveness, the
/// kill switch, and the capabilities load gauges.
pub struct MeshDeps {
    pub mesh_rx:      mpsc::Receiver<WorkerMeshReq>,
    pub remotes_path: PathBuf,
    pub peers:        Arc<RwLock<PeerRegistry>>,
    pub liveness:     LivenessMap,
    pub node_id:      Arc<String>,
    pub enabled:      bool,
    pub slots_gauge:  Arc<AtomicUsize>,
    pub queued_gauge: Arc<AtomicUsize>,
}

/// Internal outcomes of spawned mesh HTTP calls (assigns + polls) — the
/// driver never blocks its select loop on the network; spawned tasks answer
/// here. `wids` echoes the conductor-side row ids the call was made for.
enum MeshOutcome {
    Assign { batch: u64, node: String, wids: Vec<u64>, result: Result<serde_json::Value, String> },
    Poll   { batch: u64, node: String, result: Result<serde_json::Value, String> },
}

/// One (batch, node) poll target — supervision across the wire rides the
/// review cadence: golden offsets, fib backoff on an unchanged snapshot,
/// dark peers skipped (the beacon owns dark detection).
struct PollTarget {
    next:        Instant,
    attempt:     u32,
    fingerprint: u64,
    inflight:    bool,
}

/// Resolve a peer's HTTP base + token from the registry (the supervisor's
/// ws→http rewrite, registry-backed instead of a per-call file read).
async fn peer_http(peers: &Arc<RwLock<PeerRegistry>>, node: &str) -> Option<(String, Option<String>)> {
    let reg = peers.read().await;
    reg.peers.iter().find(|p| p.node_id == node).map(|p| {
        (p.ws_url.replacen("ws://", "http://", 1).replacen("wss://", "https://", 1), p.token.clone())
    })
}

/// POST one mesh-worker JSON body; returns the parsed reply or an error
/// string. Bearer token when stored; MESH_HTTP_TIMEOUT_S bound.
async fn mesh_post(http_base: &str, path: &str, token: Option<&str>, body: &serde_json::Value) -> Result<serde_json::Value, String> {
    let url = format!("{http_base}{path}");
    let mut req = reqwest::Client::new()
        .post(&url)
        .timeout(Duration::from_secs(remote::MESH_HTTP_TIMEOUT_S))
        .header("x-mesh-hops", "1")
        .json(body);
    if let Some(t) = token { req = req.bearer_auth(t); }
    match req.send().await {
        Ok(resp) => resp.json::<serde_json::Value>().await.map_err(|e| format!("bad reply: {e}")),
        Err(e) => Err(format!("{e}")),
    }
}

/// Read one terminal worker's evidence doc back for a report/query payload
/// (peer side — the doc travels inline so the conductor mirrors in one hop).
fn read_evidence_doc(agents_dir: &Path, worker_id: u64) -> Option<serde_json::Value> {
    std::fs::read_to_string(evidence_path(agents_dir, worker_id)).ok()
        .and_then(|t| serde_json::from_str(&t).ok())
}

/// Refresh the capabilities load gauges (W2) — called at the driver's
/// persistence points, so the snapshot is current to within a tick.
fn update_gauges(workers: &HashMap<u64, Worker>, mesh: &MeshDeps) {
    mesh.slots_gauge.store(slots_used(workers), Ordering::Relaxed);
    let queued = workers.values()
        .filter(|w| w.state == WorkerState::Queued && !w.barrier_held)
        .count();
    mesh.queued_gauge.store(queued, Ordering::Relaxed);
}

/// The on-disk form (transient `started`/`pending`/`turn_inflight` dropped).
/// New fields added by later slices MUST carry `#[serde(default)]` so an old
/// workers.json still loads (the PersistedGoal discipline).
#[derive(Serialize, Deserialize)]
struct PersistedWorker {
    id:      u64,
    batch:   u64,
    parent:  u64,
    session: u64,
    task:    String,
    state:   WorkerState,
    #[serde(default = "default_step")]
    step:    u32,
    #[serde(default)]
    summary: Option<String>,
    #[serde(default)]
    artifacts: Vec<String>,
    #[serde(default)]
    episode: Option<String>,
    #[serde(default)]
    yolo: bool,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    step_ceiling: u32,
}

fn default_step() -> u32 { 1 }

/// A worker's live step ceiling: its own (cell budget / renewals) or the
/// env global when unset (the 0 sentinel — plain workers).
fn effective_ceiling(w: &Worker, max_steps: u32) -> u32 {
    if w.step_ceiling > 0 { w.step_ceiling } else { max_steps }
}

/// The mint hold rule (M1c refinement): pure-J cells (GATE/DIAMOND) barrier-
/// hold until their subtree settles; an R+J cell (FORGE) starts lapping —
/// its J guard governs the joins it runs, not its own admission.
fn holds_at_mint(barrier: Option<u64>, measure: Option<&str>) -> bool {
    barrier.is_some() && measure.is_none()
}

/// M2 — why a planned cell may not take a `node` (None = it may). Pure; the
/// text names the law so the conductor learns at plan time. Only plain
/// ring/leaf cells of repo-less mandalas ship out: the tree, its barriers
/// and its measures are bindu machinery.
fn remote_cell_veto(
    is_gate: bool,
    has_measure: bool,
    has_voucher: bool,
    repo_mandala: bool,
) -> Option<&'static str> {
    if repo_mandala {
        // Mandala-level: worktrees/repos don't teleport — a code cell works
        // a repo on THIS node's disk; the peer has no such path.
        Some("code mandalas keep every cell local — the repo lives on this node's disk and \
              worktrees don't teleport; run code rings here, or open a repo-less mandala for \
              remote work")
    } else if is_gate {
        // Barriers are conductor machinery (barrier_held, open_descendants,
        // check_barriers all read the local tree) — the bindu on the spine.
        Some("the join/gate stays on this node — barriers are conductor machinery (the bindu \
              on the spine); give node to ring cells, never the gate")
    } else if has_measure {
        // The measure law fires at the lap boundary in advance() — the lap
        // boundary lives where turns complete, and remote turns complete on
        // the peer. Relaying laps by poll would make the wire carry state.
        Some("a measured (R) cell cannot go remote — the lap boundary lives where turns \
              complete, and the wire never carries state; measure on this node or drop the \
              measure")
    } else if has_voucher {
        // Structurally impossible peer-side anyway (a hosted worker has no
        // cell binding, so its task_fanout refuses) — refuse HERE so the
        // conductor learns before minting, not from a stuck sub-conductor.
        Some("a vouchered cell cannot go remote — sub-conduction needs the tree, and the \
              tree stays on this node")
    } else {
        None
    }
}

#[derive(Serialize, Deserialize)]
struct PersistedBatch {
    batch:         u64,
    parent:        u64,
    created_epoch: u64,
    deadline_s:    u64,
    reported:      bool,
    /// W2: set on batches hosted for a remote conductor (its node, its batch).
    #[serde(default)]
    origin_node:   Option<String>,
    #[serde(default)]
    origin_batch:  Option<u64>,
}

fn epoch_now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

// ── Tool specs ───────────────────────────────────────────────────────────────

pub fn task_fanout_spec() -> ToolSpec {
    ToolSpec {
        name: "task_fanout".into(),
        description: "Fan a batch of independent tasks out to parallel WORKERS — one call, N \
                      persistent worker sessions, each executing its task on its own turn. The \
                      admission cap (hardware-tier-aware) bounds how many run at once; the rest \
                      queue FIFO and start as slots free. Progress shows live on the Work Board's \
                      WORKERS lane; check from anywhere with list_workers. Parallel work goes \
                      through ONE task_fanout batch — never a spawn-then-wait chain. Returns \
                      immediately with the batch and worker ids; workers run in the background. \
                      Under a MANDALA (pass mandala + parent_cell) each task becomes an addressed \
                      CELL with the invariant injected verbatim: tasks:[N] grows a ring of \
                      siblings, and join:\"…\" additionally mints a GATE cell above the ring — \
                      the one-call diamond: the gate's worker is held by a barrier until the \
                      ring settles, then integrates its evidence (merge/verify for code \
                      mandalas). Fanning wide? Give each task a disjoint scope; code mandalas \
                      give every ring cell its own git worktree branch automatically. THE \
                      COLONY IS THE WORKER POOL: give a task (or the whole batch) node:\"<peer>\" \
                      and it runs on that node's own worker tier — its cap, its policy \
                      (approvals land THERE, yolo never crosses), its evidence; the small \
                      evidence doc mirrors back here when it settles, artifacts stay on the \
                      peer. Mandala rings ship out the same way (M2): the ring cells' BODIES \
                      run on the peer while the tree, the gate and its barrier stay here — \
                      the gate reads the evidence mirrors when the ring settles. \
                      mesh_capabilities shows each peer's worker load.".into(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "tasks": {
                    "type": "array",
                    "description": "The tasks to fan out (1-32). Each item is either a task string or {prompt}.",
                    "items": {
                        "anyOf": [
                            { "type": "string" },
                            { "type": "object",
                              "properties": {
                                  "prompt": { "type": "string", "description": "The task." },
                                  "model":  { "type": "string", "description": "Model for this one worker (wins over the batch model)." },
                                  "node":   { "type": "string", "description": "Run this task on a MESH PEER's worker tier (wins over the batch node). Plain tasks and mandala RING cells (M2: the cell stays in this tree, its execution body runs there) — never the join/gate, a measured or vouchered cell, or any cell of a code mandala; not with inline. The peer's cap, policy and evidence apply; its report mirrors back here." },
                                  "measure": { "type": "string", "description": "Mandala only — arms the R bit (SPIRAL; with a barrier, FORGE): a command computing a non-negative integer (failing tests, TODO count, |diff|…) the cell runs each lap and reports; it must strictly decrease or the ring breaks (K-stall). A progressing R-cell at its step ceiling RENEWS by spending the parent cell's steps." },
                                  "voucher": { "type": "boolean", "description": "Mandala only — grants SUB-CONDUCTION: this cell's worker may task_fanout into its own subtree, funded by its own budget vector. Batch reports are delivered into its session." }
                              },
                              "required": ["prompt"] }
                        ]
                    }
                },
                "mode": { "type": "string", "enum": ["async", "inline"],
                          "description": "async (the default): fan and return immediately — the coding path. inline: BLOCK until the batch reports and return the rows as this call's result — short batches only (max 4 tasks, deadline capped at 240s)." },
                "model": { "type": "string",
                          "description": "Model for every worker in the batch (per-task model overrides this). Omit for the node default. The colony-model-mix lever: think on the big model, hammer on the small." },
                "node": { "type": "string",
                          "description": "Host every task in this batch on a MESH PEER's worker tier (per-task node overrides this). Omit for local workers. Under a mandala this ships the RING out while the tree, the gate and its barrier stay here — the cross-node ring (M2); measured/vouchered cells and code mandalas refuse. Not with inline mode." },
                "yolo": { "type": "string", "enum": ["inherit"],
                          "description": "inherit: workers auto-approve their OWN ask tools IF AND ONLY IF this calling session is itself yolo-armed (a yolo:true goal) — never more than the parent has. Default off." },
                "batch_deadline_s": { "type": "integer",
                          "description": "Report bound in seconds (default 3600, clamped 60-86400): at the deadline the batch reports with unfinished workers marked timed_out (still revivable) instead of waiting forever." },
                "mandala": { "type": "integer",
                          "description": "Grow this mandala instead of a plain batch: each task becomes an addressed cell under parent_cell, budget strictly descending, invariant injected verbatim." },
                "parent_cell": { "type": "string",
                          "description": "The cell to grow under (default the root, \"0\"). Ring widths come from the mandala's lattice." },
                "join": { "type": "string",
                          "description": "Mandala only — the JOIN task: mints a GATE cell above this call's ring (gate at parent_cell.child, ring under the gate). The gate is barrier-held until the ring settles, then its worker integrates the descendants' evidence. A join consumes a depth level of its own." },
                "barrier_timeout_s": { "type": "integer",
                          "description": "Mandala only — the gate's J guard (default 1800, clamped 60..cell deadline): at the timeout the barrier opens anyway with stragglers listed. With exactly one task and NO join, that task itself becomes a bare gate (fan under it in later calls — interactive conductors only; goal conductors should use join, one call)." }
            },
            "required": ["tasks"]
        }),
    }
}

pub fn mandala_close_spec() -> ToolSpec {
    ToolSpec {
        name: "mandala_close".into(),
        description: "Close a MANDALA whose work is finished: marks the root cell done (open_cells \
                      reaches 0, honestly) and the mandala closed — the tree stays on disk, \
                      browsable via mandala_status. Refuses while any non-root cell is still open: \
                      finish or worker_cancel{batch} them first — closing is bookkeeping, never a \
                      kill switch. Goal-driven conductors get this automatically when their goal \
                      ends; interactive conductors call it themselves.".into(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "mandala": { "type": "integer", "description": "The mandala id (mandala_status lists them)." }
            },
            "required": ["mandala"]
        }),
    }
}

pub fn worker_report_spec() -> ToolSpec {
    ToolSpec {
        name: "worker_report".into(),
        description: "Report the outcome of the CURRENT worker step — only meaningful while running \
                      as a fanned-out worker. `done`: the task is complete — REQUIRES a `summary` \
                      (one paragraph: what was delivered and where); your summary + final text are \
                      what the conductor reads. `continue`: take another step, optionally steering \
                      it via `next`. `yield`: pause Idle awaiting input (a send wakes you). \
                      `blocked`: an unresolvable dependency — park awaiting help with a `reason`. \
                      Not calling this at all also completes the task, with your final text as the \
                      deliverable. The driver applies your verdict when this turn completes.".into(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "status":  { "type": "string", "enum": ["continue", "done", "blocked", "yield"],
                             "description": "done = task complete (summary required); continue = another step; yield = pause for input; blocked = stuck." },
                "summary": { "type": "string", "description": "REQUIRED with done: what was delivered and where it lives." },
                "artifacts": { "type": "array", "items": { "type": "string" },
                             "description": "With done: workspace paths of files you produced (the evidence the conductor reads). Workspace-confined — outside paths are refused." },
                "next":    { "type": "string", "description": "Optional steer for the next step (status=continue)." },
                "reason":  { "type": "string", "description": "Why you're stuck (status=blocked)." },
                "measure": { "type": "integer", "description": "R-cells only: this lap's measure — the non-negative integer your declared measure command just produced. Must strictly decrease each lap; two non-decreasing laps break your ring." }
            },
            "required": ["status"]
        }),
    }
}

pub fn list_workers_spec() -> ToolSpec {
    ToolSpec {
        name: "list_workers".into(),
        description: "List fanned-out workers and their live state (worker id, batch, state \
                      queued/running/blocked/parked/done/failed, task) plus the admission cap — \
                      check on a batch from anywhere, without the Work Board open.".into(),
        input_schema: serde_json::json!({ "type": "object", "properties": {} }),
    }
}

pub fn worker_cancel_spec() -> ToolSpec {
    ToolSpec {
        name: "worker_cancel".into(),
        description: "Cancel a fanned-out worker by id, or a whole batch — terminal, not \
                      revivable. Aborts any in-flight turn, frees the slot, and leaves the \
                      normal terminal trail (evidence file + episode) so the batch report \
                      stays honest. The kill switch for a runaway or no-longer-wanted fan; \
                      cancelling a conductor GOAL cascades to its batch automatically.".into(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "worker": { "type": "integer", "description": "One worker id (from task_fanout's ack or list_workers)." },
                "batch":  { "type": "integer", "description": "A whole batch id — cancels every non-terminal worker in it." }
            }
        }),
    }
}

pub fn mandala_create_spec() -> ToolSpec {
    ToolSpec {
        name: "mandala_create".into(),
        description: "Open a MANDALA: a depth-capable recursion manifold over the worker tier for \
                      runs too large for one flat fan-out (multi-day refactors, ports, sustained \
                      research). You write the INVARIANT once — objective, definition of done, and \
                      the verify command — and every cell at every depth receives those exact bytes; \
                      no level can paraphrase the goal. Then grow the tree with \
                      task_fanout{mandala, parent_cell, tasks}: each cell is a real worker with an \
                      address (0.2.1 = root→2nd child→1st), a strictly descending budget, and the \
                      full evidence trail. Rings fan in parallel up to the lattice width; \
                      join:\"…\" adds a barrier-held GATE that integrates a ring when it settles \
                      (sub-conductors arrive in a later slice). Declare `repo` for code runs — \
                      cells then work on their own git worktree branches. Inspect with \
                      mandala_status; close with mandala_close when the work has settled.".into(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "objective":  { "type": "string", "description": "What the whole mandala exists to accomplish." },
                "done_when":  { "type": "string", "description": "The definition of done — concrete, checkable." },
                "verify":     { "type": "string", "description": "THE verify command (e.g. 'cargo test -p x') — every cell at every depth checks against this exact command, run through normal policied tools." },
                "lattice":    { "type": "string", "enum": ["spine", "quad", "fan", "spiral", "funnel"],
                                "description": "The geometry preset (default spine — bisection, ring width 2). quad = 4-way rings (balanced decomposition), fan = 8-wide (parallel sweeps), spiral = fibonacci growth, funnel = 9→4→1 synthesis. Ring widths are LIVE: a wide fan must fit its ring." },
                "repo":       { "type": "string", "description": "Code regime: a git repo directory inside your workspace (e.g. code/myproject). When set, wide-fan cells each get their own address-named branch + git worktree (collision-free parallel edits) and gates get the merge ritual — all injected mechanically. Code mandalas keep every cell LOCAL (the repo lives on this node's disk — no node tasks)." },
                "depth":      { "type": "integer", "description": "Depth budget (default 6, max 6)." },
                "steps":      { "type": "integer", "description": "Root step budget, contracts 0.5× per level (default 32)." },
                "deadline_s": { "type": "integer", "description": "Horizon for the whole mandala in seconds (default 86400)." }
            },
            "required": ["objective", "done_when", "verify"]
        }),
    }
}

pub fn mandala_status_spec() -> ToolSpec {
    ToolSpec {
        name: "mandala_status".into(),
        description: "Read a mandala's live tree: every cell's address, state, worker, budget and \
                      evidence path, plus the geometry picture (open cells vs the 64-cell budget). \
                      Omit `mandala` to list all mandalas.".into(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": { "mandala": { "type": "integer", "description": "A mandala id from mandala_create." } }
        }),
    }
}

// ── Charter + directive (the cache law: byte-stable, small) ─────────────────

/// The worker system prompt — the H6 minimal task charter, worker-tier form.
/// Replaces the soul for every worker session (root_turn branches on
/// `is_worker_session`), so all workers on a node share one small byte-stable
/// system prefix: the prompt cache serves the whole batch from one entry.
/// Volatile text is banned here (docs/fabrica.md cache law, one level down).
pub fn worker_system(parent_agent: &str) -> String {
    format!(
        "You are a task-scoped WORKER on {parent_agent}'s node — one of a batch fanned out by a \
         conductor. You exist for exactly one task, delivered in your prompt. Work it directly \
         with the minimum tools required. Report through worker_report: done{{summary}} when the \
         task is complete — the summary plus your final text are the deliverable the conductor \
         reads; continue to take another step; yield to pause for input; blocked{{reason}} if \
         truly stuck. Not reporting also completes the task, with your final text as the \
         deliverable. Skip orientation: no memory recall, inbox checks, or self-inspection unless \
         the task itself asks for them. Approval-gated tools still ask a human — prefer ungated \
         paths. Do not spawn agents. Fan out further work ONLY if your work order carries a \
         VOUCHER block granting sub-conduction — without one, workers are depth-1 by design."
    )
}

/// The first-step work order. Per-worker text rides here, never in the shared
/// system charter, so the system prefix stays identical across a batch.
fn directive_first(worker_id: u64, batch: u64, max_steps: u32, task: &str) -> String {
    format!(
        "WORKER {worker_id} (batch {batch}) — step 1/{max_steps}.\n\nTASK:\n{task}\n\n\
         Work the task NOW. When it is complete, call `worker_report{{status:\"done\", \
         summary:\"…\"}}` — don't burn steps you don't need. `continue` takes another step, \
         `yield` pauses for input, `blocked{{reason}}` parks it. No report also completes the \
         task: your final text is the deliverable."
    )
}

/// The continue-step work order (`worker_report{{continue}}` took another step).
fn directive_continue(worker_id: u64, batch: u64, step: u32, max_steps: u32, task: &str, steer: Option<&str>) -> String {
    let head = format!("Continue WORKER {worker_id} (batch {batch}) — step {step}/{max_steps}. TASK: {task}");
    match steer {
        Some(s) => format!("{head}\n\nFocus this step on: {s}\n\nCall `worker_report{{status:\"done\", summary:\"…\"}}` when complete."),
        None    => format!("{head}\n\nKeep making concrete progress. Call `worker_report{{status:\"done\", summary:\"…\"}}` when complete."),
    }
}

// ── Pure resolvers (unit-tested) ─────────────────────────────────────────────

/// Resolve the admission cap: a valid `AGENTD_WORKER_CAP` (≥1) wins, else the
/// hardware tier's default. Floor 1 — a typo can't wedge fan-out to zero. The
/// charter's "gpu" bucket is the `pro` tier string (RAM-threshold, no GPU
/// probe exists); nano and an unreadable RAM probe floor conservatively to 1.
/// Sane configs keep this ≤ the turn-engine semaphore (16) — the cap is
/// residency, not a provider-call guarantee.
pub fn worker_cap_from_env(raw: Option<&str>, tier: &str) -> usize {
    if let Some(n) = raw.and_then(|s| s.parse::<usize>().ok()).filter(|&n| n >= 1) {
        return n;
    }
    match tier {
        "micro"    => 2,
        "standard" => 4,
        "pro"      => 8,
        _          => 1, // nano, unknown — the conservative floor
    }
}

/// Pure stall-timeout resolver: a valid ≥30s value wins; anything else falls
/// back to the 900s default (the goal driver's exact clamp discipline).
fn parse_step_timeout(raw: Option<&str>) -> Duration {
    raw.and_then(|s| s.parse::<u64>().ok())
        .filter(|&n| n >= 30)
        .map(Duration::from_secs)
        .unwrap_or(STEP_TIMEOUT)
}

fn step_timeout_from_env() -> Duration {
    parse_step_timeout(std::env::var("WORKER_STEP_TIMEOUT_SECS").ok().as_deref())
}

/// Pure idle-TTL resolver: a valid ≥60s value wins; anything else falls back
/// to the 1800s default. Parking is cheap to undo (a send revives) but a typo
/// shouldn't churn park/revive cycles every sweep.
fn parse_idle_ttl(raw: Option<&str>) -> Duration {
    raw.and_then(|s| s.parse::<u64>().ok())
        .filter(|&n| n >= 60)
        .map(Duration::from_secs)
        .unwrap_or(IDLE_TTL)
}

fn idle_ttl_from_env() -> Duration {
    parse_idle_ttl(std::env::var("WORKER_IDLE_TTL_SECS").ok().as_deref())
}

/// Pure step-ceiling resolver: any parseable value clamps to 1–100, else the
/// default 12 (the goal budget's shape — code disposes on runaway continues).
fn parse_max_steps(raw: Option<&str>) -> u32 {
    raw.and_then(|s| s.parse::<u32>().ok())
        .map(|n| n.clamp(1, 100))
        .unwrap_or(DEFAULT_MAX_STEPS)
}

fn max_steps_from_env() -> u32 {
    parse_max_steps(std::env::var("WORKER_MAX_STEPS").ok().as_deref())
}

/// Map `worker_report` args to a verdict. Absent/unknown status = continue
/// (the goal_step convention); `done`'s summary requirement and artifact
/// confinement are enforced at record time so the model gets an actionable
/// refusal, not a silent default. `measure` rides continue/done (lenient:
/// non-integers read as absent — the ritual asks for a bare integer).
fn parse_verdict(args: &serde_json::Value) -> Verdict {
    let measure = args["measure"].as_u64();
    match args["status"].as_str() {
        Some("done") => Verdict::Done {
            summary: args["summary"].as_str().unwrap_or("").trim().to_string(),
            artifacts: artifact_strings(args),
            measure,
        },
        Some("blocked") => Verdict::Blocked(args["reason"].as_str().unwrap_or("blocked").to_string()),
        Some("yield")   => Verdict::Yield,
        _               => Verdict::Continue { steer: args["next"].as_str().map(str::to_owned), measure },
    }
}

fn artifact_strings(args: &serde_json::Value) -> Vec<String> {
    args["artifacts"].as_array().map(|a| {
        a.iter().filter_map(|v| v.as_str())
            .map(str::trim).filter(|s| !s.is_empty())
            .map(str::to_owned).collect()
    }).unwrap_or_default()
}

/// Pure batch-deadline resolver: absent → 3600s; anything given clamps to
/// 60s–24h (a deadline of zero would report the batch before it runs; no
/// deadline at all is the forever-wedged-conductor the charter forbids).
fn parse_batch_deadline(args: &serde_json::Value) -> u64 {
    args["batch_deadline_s"].as_u64()
        .map(|n| n.clamp(60, 86_400))
        .unwrap_or(DEFAULT_BATCH_DEADLINE_S)
}

/// Validate done-declared artifact paths against the node agent's workspace
/// (the apexos-confine gate — same law as every fs tool). Relative paths are
/// rooted at the workspace first. Returns the canonical forms, or the first
/// offending path as the refusal message.
fn confine_artifacts(paths: &[String], workspace: &Path) -> Result<Vec<String>, String> {
    let mut out = Vec::with_capacity(paths.len());
    for p in paths {
        let requested = if Path::new(p).is_absolute() {
            PathBuf::from(p)
        } else {
            workspace.join(p)
        };
        // Declarations point at files a confined tool already wrote inside the
        // workspace, so no read-roots and no secret filter apply here — the
        // only question is "is it inside the workspace".
        match apexos_confine::confine_fs(&requested, apexos_confine::Access::Read, workspace, &[], |_| false) {
            Ok(canon) => out.push(canon.to_string_lossy().into_owned()),
            Err(_) => return Err(format!(
                "artifact path '{p}' is outside the agent workspace — declare workspace paths only")),
        }
    }
    Ok(out)
}

/// One parsed task item: the prompt plus per-worker knobs. `measure` and
/// `voucher` (M1c) are mandala-cell properties — refused on plain fans.
/// `node` (W2) sends the task to a PEER's worker pool — refused on mandala
/// fans (cross-node rings are M2) and in inline mode.
#[derive(Clone)]
struct TaskSpecItem {
    prompt: String,
    model: Option<String>,
    /// The R guard: this cell's measure command (arms SPIRAL/FORGE).
    measure: Option<String>,
    /// Sub-conduction grant: this cell's worker may grow its own subtree.
    voucher: bool,
    /// W2: host this task on a peer node's worker tier (the colony as the
    /// worker pool). Per-task value wins over the batch-level `node`.
    node: Option<String>,
}

/// Extract the tasks from `task_fanout` args — item = string | {prompt,
/// model?, measure?, voucher?, node?}. The per-task model/node win over the
/// batch-level `model`/`node`. Errors are conductor-facing strings (the tool
/// result), not panics.
fn parse_tasks(args: &serde_json::Value) -> Result<Vec<TaskSpecItem>, String> {
    let items = args["tasks"].as_array().ok_or("tasks must be an array of task strings or {prompt} objects")?;
    if items.is_empty() { return Err("tasks is empty — nothing to fan out".into()); }
    if items.len() > MAX_BATCH_TASKS {
        return Err(format!("{} tasks exceeds the {MAX_BATCH_TASKS}-per-batch ceiling — split into sequential batches", items.len()));
    }
    let batch_model = args["model"].as_str().map(str::trim).filter(|s| !s.is_empty() && s.len() <= 64);
    let batch_node = args["node"].as_str().map(str::trim).filter(|s| !s.is_empty() && s.len() <= 64);
    let mut tasks = Vec::with_capacity(items.len());
    for (i, item) in items.iter().enumerate() {
        let prompt = item.as_str().or_else(|| item["prompt"].as_str()).unwrap_or("").trim();
        if prompt.is_empty() { return Err(format!("task {} has no prompt", i + 1)); }
        let model = item["model"].as_str().map(str::trim).filter(|s| !s.is_empty() && s.len() <= 64)
            .or(batch_model)
            .map(str::to_owned);
        let measure = item["measure"].as_str().map(str::trim)
            .filter(|s| !s.is_empty() && s.len() <= 200)
            .map(str::to_owned);
        let voucher = item["voucher"].as_bool().unwrap_or(false);
        let node = item["node"].as_str().map(str::trim).filter(|s| !s.is_empty() && s.len() <= 64)
            .or(batch_node)
            .map(str::to_owned);
        tasks.push(TaskSpecItem { prompt: prompt.to_string(), model, measure, voucher, node });
    }
    Ok(tasks)
}

/// The restart mapping: every non-terminal state parks (memory residency is
/// gone; `sessions/<id>.jsonl` + workers.json are truth). Terminal states pass
/// through. Queued parks too — the daemon never auto-runs fanned work after a
/// restart (the goal driver's never-auto-resume philosophy).
fn parked_form(state: WorkerState) -> WorkerState {
    match state {
        WorkerState::Done | WorkerState::Failed | WorkerState::Cancelled => state,
        _ => WorkerState::Parked,
    }
}

/// A worker occupies a thermal slot while a turn is live on its session:
/// Running, or Blocked with the turn suspended on an approval (still
/// resident, still mid-flight). The thermal budget is RUNNING residency
/// (docs/fabrica.md) — Idle (yielded), verdict-blocked (turn completed),
/// Parked, Queued, and terminal states hold no slot.
fn holds_slot(w: &Worker) -> bool {
    w.state == WorkerState::Running || (w.state == WorkerState::Blocked && w.turn_inflight)
}

fn slots_used(workers: &HashMap<u64, Worker>) -> usize {
    workers.values().filter(|w| holds_slot(w)).count()
}

/// The FIFO: the lowest-id Queued worker is next (ids are mint-ordered).
/// Barrier-held gates are invisible here — a gate enters the queue's view
/// only when its barrier opens (M1b).
fn next_queued(workers: &HashMap<u64, Worker>) -> Option<u64> {
    workers.iter()
        .filter(|(_, w)| w.state == WorkerState::Queued && !w.barrier_held)
        .map(|(id, _)| *id)
        .min()
}

/// The review posture (M1b) — the axis the six-bit word deliberately does
/// not encode. Pure over the worker record (unit-tested).
fn posture_of(w: &Worker) -> Posture {
    if is_terminal(w.state) {
        Posture::Terminal
    } else if w.barrier_held && w.state == WorkerState::Queued {
        Posture::BarrierWait
    } else if w.state == WorkerState::Running || (w.state == WorkerState::Blocked && w.turn_inflight) {
        Posture::Live
    } else {
        Posture::Waiting
    }
}

// ── Persistence (atomic — restarts are a first-class path here) ─────────────

fn save_workers(workers: &HashMap<u64, Worker>, path: &PathBuf) {
    let mut snapshot: Vec<PersistedWorker> = workers.iter().map(|(id, w)| PersistedWorker {
        id: *id, batch: w.batch, parent: w.parent, session: w.session,
        task: w.task.clone(), state: w.state, step: w.step, summary: w.summary.clone(),
        artifacts: w.artifacts.clone(), episode: w.episode.clone(),
        yolo: w.yolo, model: w.model.clone(), step_ceiling: w.step_ceiling,
    }).collect();
    snapshot.sort_by_key(|w| w.id); // deterministic file → readable diffs
    if let Some(parent) = path.parent() { let _ = std::fs::create_dir_all(parent); }
    if let Ok(json) = serde_json::to_string_pretty(&snapshot) {
        // Temp + rename (the self-update request idiom), NOT goals.json's direct
        // write: parking on restart is this file's core job, so a torn write is
        // a real failure mode, not a corner case.
        let tmp = path.with_extension("json.tmp");
        if std::fs::write(&tmp, &json).is_ok() {
            let _ = std::fs::rename(&tmp, path);
        }
    }
}

fn load_workers(path: &PathBuf) -> Vec<PersistedWorker> {
    std::fs::read_to_string(path).ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

fn save_batches(batches: &HashMap<u64, BatchMeta>, path: &PathBuf) {
    let mut snapshot: Vec<PersistedBatch> = batches.iter().map(|(id, b)| PersistedBatch {
        batch: *id, parent: b.parent, created_epoch: b.created_epoch,
        deadline_s: b.deadline_s, reported: b.reported,
        origin_node: b.origin.as_ref().map(|(n, _)| n.clone()),
        origin_batch: b.origin.as_ref().map(|(_, ob)| *ob),
    }).collect();
    snapshot.sort_by_key(|b| b.batch);
    if let Some(parent) = path.parent() { let _ = std::fs::create_dir_all(parent); }
    if let Ok(json) = serde_json::to_string_pretty(&snapshot) {
        let tmp = path.with_extension("json.tmp");
        if std::fs::write(&tmp, &json).is_ok() {
            let _ = std::fs::rename(&tmp, path);
        }
    }
}

fn load_batches(path: &PathBuf) -> Vec<PersistedBatch> {
    std::fs::read_to_string(path).ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

// ── Evidence + episodes (W1c — the terminal worker's trail) ─────────────────

fn evidence_path(agents_dir: &Path, worker_id: u64) -> PathBuf {
    agents_dir.join(format!("{worker_id}.json"))
}

/// Write the terminal evidence file — tmp+rename (this file is the whole
/// point of the evidence rule; a torn write here poisons integration). The
/// summary rides along for humans, but the batch report hands the conductor
/// this PATH — reading it is what integration means.
fn write_evidence(agents_dir: &Path, worker_id: u64, w: &Worker) {
    let _ = std::fs::create_dir_all(agents_dir);
    let doc = serde_json::json!({
        "worker":    worker_id,
        "batch":     w.batch,
        "parent":    w.parent,
        "session":   w.session,
        "task":      w.task,
        "state":     format!("{:?}", w.state).to_lowercase(),
        "step":      w.step,
        "summary":   w.summary,
        "artifacts": w.artifacts,
        "episode":   w.episode,
        "history":   format!("sessions/{}.jsonl", w.session), // the full transcript, same log_dir
        "completed_at": chrono::Utc::now().to_rfc3339(),
    });
    if let Ok(json) = serde_json::to_string_pretty(&doc) {
        let path = evidence_path(agents_dir, worker_id);
        let tmp  = path.with_extension("json.tmp");
        if std::fs::write(&tmp, &json).is_ok() {
            let _ = std::fs::rename(&tmp, path);
        }
    }
}

/// Start a Cerebro episode wrapping this worker's run (best-effort — None if
/// unreachable; never blocks admission). Attributed to the node agent, the
/// goal driver's convention.
async fn episode_start_worker(proxy: &ToolProxy, wid: u64, batch: u64, task: &str) -> Option<String> {
    let title = format!("worker {wid} (batch {batch}): {}", task.chars().take(80).collect::<String>());
    match proxy.call("episode_start", serde_json::json!({
        "title": title, "agent_id": apexos_core::node_agent_id(), "tags": ["worker"]
    })).await {
        Ok(out) if out.ok => crate::parse_cerebro_id(&out, "episode_id"),
        Ok(out) => { eprintln!("[worker] episode_start not ok: {:?}", out.content); None }
        Err(e)  => { eprintln!("[worker] episode_start: {e}"); None }
    }
}

/// Close a worker's episode with the outcome (best-effort) — the run becomes
/// a recallable, dream-able memory.
async fn episode_end_worker(proxy: &ToolProxy, episode_id: &str, w: &Worker, step: u32) {
    let (outcome, valence) = match w.state {
        WorkerState::Done      => ("completed", "positive"),
        WorkerState::Failed    => ("failed",    "negative"),
        WorkerState::Cancelled => ("cancelled", "neutral"),
        _                      => ("ended",     "neutral"),
    };
    let summary = match &w.summary {
        Some(s) => format!("worker {outcome} at step {step}: {s}"),
        None    => format!("worker {outcome} at step {step}: {}", w.task.chars().take(120).collect::<String>()),
    };
    if let Err(e) = proxy.call("episode_end", serde_json::json!({
        "episode_id": episode_id, "summary": summary, "valence": valence
    })).await {
        eprintln!("[worker] episode_end: {e}");
    }
}

/// The terminal trail, in one place: evidence file + episode close. Called on
/// every path into Done/Failed (verdict, fallback, budget, stall).
async fn finalize_terminal(proxy: &ToolProxy, agents_dir: &Path, wid: u64, w: &Worker) {
    write_evidence(agents_dir, wid, w);
    if let Some(ep) = w.episode.clone() {
        episode_end_worker(proxy, &ep, w, w.step).await;
    }
}

// ── Events ───────────────────────────────────────────────────────────────────

/// One emission chokepoint, the goal driver's `emit_state` twin. `task` is
/// truncated for the wire — cards render a title, not the carry.
async fn emit_state(bus: &BusHandle, id: u64, w: &Worker, detail: &str) {
    bus.emit(Event::WorkerStateChanged {
        worker:  WorkerId(id),
        batch:   w.batch,
        parent:  SessionId(w.parent),
        session: SessionId(w.session),
        task:    w.task.chars().take(80).collect(),
        state:   w.state,
        detail:  detail.into(),
        yolo:    w.yolo,
        node:    None, // local workers; remote rows emit via emit_remote_state
    }).await;
}

/// Arm/disarm helpers for the shared per-session state a live worker holds:
/// the auto-approve set (batch-inherited yolo) and the model-pin map. One
/// call each on the way in (admission/wake) and out (terminal/park) keeps
/// the invariant auditable: shared state exactly mirrors live residency.
fn arm_worker(yolo_set: &GoalYoloSessions, models: &WorkerModels, w: &Worker) {
    if w.yolo {
        if let Ok(mut s) = yolo_set.lock() { s.insert(w.session); }
    }
    if let Some(m) = &w.model {
        if let Ok(mut mm) = models.lock() { mm.insert(w.session, m.clone()); }
    }
}

fn disarm_worker(yolo_set: &GoalYoloSessions, models: &WorkerModels, session: u64) {
    if let Ok(mut s) = yolo_set.lock() { s.remove(&session); }
    if let Ok(mut mm) = models.lock() { mm.remove(&session); }
}

// ── The driver ───────────────────────────────────────────────────────────────

/// Spawn the worker driver: `task_fanout`/`worker_report`/`list_workers`
/// arrive on `req_rx` (supervisor-routed, deferred ack), worker turns complete
/// via the bus subscription, stalls fail and batches report on a 30s tick.
/// Owns every counter, the worker map, and the batch ledger — nothing else
/// touches worker state.
#[allow(clippy::too_many_arguments)]
pub fn spawn_worker_driver(
    bus:          BusHandle,
    mut bcast_rx: broadcast::Receiver<Event>,
    mut req_rx:   mpsc::Receiver<(SessionId, ActionId, String, serde_json::Value)>,
    workers_path: PathBuf,
    batches_path: PathBuf,
    agents_dir:   PathBuf,
    mandalas_path: PathBuf,
    worktrees_dir: PathBuf,
    cap:          usize,
    proxy:        ToolProxy,
    yolo_set:     GoalYoloSessions,
    models:       WorkerModels,
    mesh:         MeshDeps,
    council_tx:   mpsc::Sender<(SessionId, ActionId, serde_json::Value)>,
) {
    tokio::spawn(async move {
        let mut mesh = mesh;
        // M1d: per-mandala epoch clocks — seeded lazily in the tick (covers
        // create, reload, and post-restart uniformly, no special cases).
        let mut next_epoch: HashMap<u64, Instant> = HashMap::new();
        let mut workers: HashMap<u64, Worker>   = HashMap::new();
        let mut batches: HashMap<u64, BatchMeta> = HashMap::new();
        // Mandala state (M1a): records + per-mandala cell trees + worker→cell
        // binding. THE FILESYSTEM IS THE TREE — these maps are rebuilt from
        // disk at boot; the driver only caches what the tree dirs say.
        let mut mandalas: HashMap<u64, MandalaRecord> = HashMap::new();
        let mut trees: HashMap<u64, HashMap<String, CellRecord>> = HashMap::new();
        let mut cell_by_worker: HashMap<u64, (u64, Addr)> = HashMap::new();
        let mut next_mandala_id: u64 = 1;
        // W2 mesh state: conductor-side mirror rows (their OWN map + file —
        // peer session ids collide across nodes, so a remote row must never
        // masquerade as a local Worker), the (batch, node) poll schedule, and
        // the outcome channel spawned HTTP tasks answer on (the driver never
        // blocks its select loop on the network).
        let mut remotes: HashMap<u64, RemoteWorker> = HashMap::new();
        let mut polls: HashMap<(u64, String), PollTarget> = HashMap::new();
        let (mesh_out_tx, mut mesh_out_rx) = mpsc::channel::<MeshOutcome>(32);
        // Driver-private counters. Workers persist, so the reload MUST re-seed
        // all three past what's on disk (the next_goal_id discipline) — never
        // blind-reset like the spawn counter (safe there only because spawns
        // never persist). Remote rows spend the SAME worker-id counter, so the
        // remotes file re-seeds it too.
        let mut next_worker_id:  u64 = 1;
        let mut next_batch_id:   u64 = 1;
        let mut next_worker_sid: u64 = apexos_core::WORKER_SESSION_BASE;

        reload_workers(&mut workers, &bus, &workers_path,
                       &mut next_worker_id, &mut next_batch_id, &mut next_worker_sid).await;
        reload_batches(&mut batches, &batches_path, &mut next_batch_id);
        reload_mandalas(&mut mandalas, &mut trees, &mut cell_by_worker,
                        &mandalas_path, &worktrees_dir, &mut next_mandala_id);
        reload_remotes(&mut remotes, &mut polls, &batches, &bus, &mesh.remotes_path,
                       &mut next_worker_id, &mut next_batch_id).await;
        update_gauges(&workers, &mesh);

        // Artifact confinement root: the node agent's workspace (workers run
        // as the node agent — resolve_agent_id on an unbound worker session).
        let workspace = apexos_core::agent_workspace_root(&apexos_core::node_agent_id());

        let step_timeout = step_timeout_from_env();
        let idle_ttl     = idle_ttl_from_env();
        let max_steps    = max_steps_from_env();
        // PB-1 tracking: recent local agent_spawn timestamps per parent session.
        let mut spawn_log: HashMap<u64, Vec<Instant>> = HashMap::new();
        // Review censuses (M1b): per-mandala histogram of review words — the
        // run's diagnostic reading, surfaced by mandala_status. In-memory by
        // design (epoch persistence to Cerebro is M1d).
        let mut censuses: HashMap<u64, HashMap<String, u64>> = HashMap::new();
        let mut tick = tokio::time::interval(TICK);
        loop {
            tokio::select! {
                Some((session, call_id, tool, args)) = req_rx.recv() => {
                    match tool.as_str() {
                        "task_fanout" => {
                            // Mandala-scoped fans validate FIRST (address, descent,
                            // geometry, ring widths, join layout) and mint from fully
                            // composed plans; a plain fan passes None straight through.
                            // W2: tasks may carry `node` — remote rows mint beside the
                            // locals and their assigns spawn after the ack.
                            if let Some((ctx, minted)) = fanout(&mut workers, &mut batches, &mandalas, &trees, &cell_by_worker, &bus, cap, max_steps, &proxy, &yolo_set, &models, session, call_id, args,
                                   &mut next_worker_id, &mut next_batch_id, &mut next_worker_sid,
                                   &mesh, &mesh_out_tx, &mut remotes).await {
                                if let Some(ctx) = &ctx {
                                    bind_cells(&mut trees, &mut cell_by_worker, &worktrees_dir, ctx, &minted);
                                }
                                save_workers(&workers, &workers_path);
                                save_batches(&batches, &batches_path);
                                remote::save_remotes(&remotes, &mesh.remotes_path);
                                update_gauges(&workers, &mesh);
                            }
                        }
                        "mandala_close" => {
                            if close_mandala_request(&mut mandalas, &mut trees, &bus, &worktrees_dir, session, call_id, args).await {
                                mandala::save_mandalas(&mandalas, &mandalas_path);
                            }
                        }
                        "mandala_create" => {
                            create_mandala(&mut mandalas, &mut trees, &bus, &worktrees_dir, &workspace, &mut next_mandala_id, session, call_id, args).await;
                            mandala::save_mandalas(&mandalas, &mandalas_path);
                        }
                        "mandala_status" => handle_mandala_status(&mandalas, &trees, &remotes, &censuses, &bus, session, call_id).await,
                        "worker_report" => record_report(&mut workers, &bus, &workspace, session, call_id, args).await,
                        "worker_cancel" => {
                            if cancel_request(&mut workers, &mut remotes, &bus, &proxy, &agents_dir, &yolo_set, &models, &mesh, session, call_id, args).await {
                                // A cancelled subtree can settle a gate — sync, sweep, admit.
                                sync_cells(&mut trees, &cell_by_worker, &workers, &worktrees_dir, &agents_dir);
                                check_barriers(&mut workers, &mut trees, &cell_by_worker, &mandalas, &bus, &worktrees_dir).await;
                                admit_queued(&mut workers, &bus, &proxy, &yolo_set, &models, cap, max_steps).await;
                                let (chg, reports) = check_batches(&workers, &remotes, &mut batches, &bus, &agents_dir).await;
                                if chg { save_batches(&batches, &batches_path); }
                                spawn_report_home(&mesh, reports).await;
                                save_workers(&workers, &workers_path);
                                remote::save_remotes(&remotes, &mesh.remotes_path);
                                update_gauges(&workers, &mesh);
                            }
                        }
                        "list_workers" => handle_list_workers(&workers, &remotes, &bus, cap, &agents_dir, session, call_id).await,
                        _ => {}
                    }
                }
                // ── W2: gateway mesh requests (peer fanout/query/cancel + report-home) ──
                Some(req) = mesh.mesh_rx.recv() => {
                    let saved = handle_mesh_req(
                        &mut workers, &mut remotes, &mut batches, &mut polls,
                        &bus, &proxy, &agents_dir, &yolo_set, &models, cap, max_steps,
                        &mesh, &mut next_worker_id, &mut next_batch_id, &mut next_worker_sid,
                        req,
                    ).await;
                    if saved {
                        // M2: remote terminals (a report-home push) mirror into
                        // their cells BEFORE barriers read them — a gate over a
                        // remote ring opens the tick its last mirror lands.
                        sync_remote_cells(&mut trees, &cell_by_worker, &remotes, &worktrees_dir);
                        if check_barriers(&mut workers, &mut trees, &cell_by_worker, &mandalas, &bus, &worktrees_dir).await {
                            admit_queued(&mut workers, &bus, &proxy, &yolo_set, &models, cap, max_steps).await;
                        }
                        save_workers(&workers, &workers_path);
                        save_batches(&batches, &batches_path);
                        remote::save_remotes(&remotes, &mesh.remotes_path);
                        let (chg, reports) = check_batches(&workers, &remotes, &mut batches, &bus, &agents_dir).await;
                        if chg { save_batches(&batches, &batches_path); }
                        spawn_report_home(&mesh, reports).await;
                        update_gauges(&workers, &mesh);
                    }
                }
                // ── W2: outcomes of spawned mesh HTTP calls (assigns + polls) ──
                Some(out) = mesh_out_rx.recv() => {
                    match out {
                        MeshOutcome::Assign { batch, node, wids, result } => {
                            handle_assign_outcome(&mut remotes, &mut polls, &bus, &agents_dir, batch, &node, wids, result).await;
                            // M2: a refused/dead assign fails its rows terminal —
                            // the cells mirror the honest failure and a gate over
                            // them can open on integration data (dark-peer path).
                            sync_remote_cells(&mut trees, &cell_by_worker, &remotes, &worktrees_dir);
                            if check_barriers(&mut workers, &mut trees, &cell_by_worker, &mandalas, &bus, &worktrees_dir).await {
                                admit_queued(&mut workers, &bus, &proxy, &yolo_set, &models, cap, max_steps).await;
                                save_workers(&workers, &workers_path);
                            }
                            let (chg, reports) = check_batches(&workers, &remotes, &mut batches, &bus, &agents_dir).await;
                            if chg { save_batches(&batches, &batches_path); }
                            spawn_report_home(&mesh, reports).await;
                            remote::save_remotes(&remotes, &mesh.remotes_path);
                        }
                        MeshOutcome::Poll { batch, node, result } => {
                            let changed = handle_poll_outcome(&mut remotes, &mut polls, &bus, &agents_dir, batch, &node, result).await;
                            if changed {
                                // M2: poll-observed remote terminals close their
                                // cells before barriers read the tree.
                                sync_remote_cells(&mut trees, &cell_by_worker, &remotes, &worktrees_dir);
                                if check_barriers(&mut workers, &mut trees, &cell_by_worker, &mandalas, &bus, &worktrees_dir).await {
                                    admit_queued(&mut workers, &bus, &proxy, &yolo_set, &models, cap, max_steps).await;
                                    save_workers(&workers, &workers_path);
                                }
                                let (chg, reports) = check_batches(&workers, &remotes, &mut batches, &bus, &agents_dir).await;
                                if chg { save_batches(&batches, &batches_path); }
                                spawn_report_home(&mesh, reports).await;
                                remote::save_remotes(&remotes, &mesh.remotes_path);
                            }
                        }
                    }
                }
                ev = bcast_rx.recv() => {
                    match ev {
                        // A worker's turn completed → apply its reported verdict
                        // (or the no-report fallback: Done, final text = deliverable).
                        Ok(Event::TurnComplete { session }) if apexos_core::is_worker_session(session.0) => {
                            if advance(&mut workers, &bus, &proxy, &agents_dir, &yolo_set, &models, &mut trees, &cell_by_worker, &worktrees_dir, session.0, max_steps).await {
                                // Cells mirror BEFORE barriers read them; an opened
                                // gate then joins the admission pass with everyone.
                                sync_cells(&mut trees, &cell_by_worker, &workers, &worktrees_dir, &agents_dir);
                                check_barriers(&mut workers, &mut trees, &cell_by_worker, &mandalas, &bus, &worktrees_dir).await;
                                admit_queued(&mut workers, &bus, &proxy, &yolo_set, &models, cap, max_steps).await;
                                let (chg, reports) = check_batches(&workers, &remotes, &mut batches, &bus, &agents_dir).await;
                                if chg { save_batches(&batches, &batches_path); }
                                spawn_report_home(&mesh, reports).await;
                                save_workers(&workers, &workers_path);
                                update_gauges(&workers, &mesh);
                            }
                        }
                        // A worker's turn hit an ask-gated tool: the turn is suspended on
                        // the approval, NOT dead — a human can grant it from the board and
                        // the turn proceeds. Mark Blocked (stall-exempt) so the lane tells
                        // the truth; the slot stays held (the turn is still in flight).
                        Ok(Event::ApprovalPending { session, call }) if apexos_core::is_worker_session(session.0) => {
                            if block_on_approval(&mut workers, &bus, session.0, &call.tool).await {
                                save_workers(&workers, &workers_path);
                            }
                        }
                        // The approval resolved (either verdict — a decline still returns
                        // a result and the turn continues): back to Running, fresh stall clock.
                        Ok(Event::UserApproval { session, .. }) if apexos_core::is_worker_session(session.0) => {
                            let resumed = resume_from_approval(&mut workers, &bus, session.0).await;
                            if resumed { save_workers(&workers, &workers_path); }
                        }
                        // A send landed on a worker session — the revive/wake edge
                        // (PB-3). The router hydrates a parked worker's history off
                        // this same event; here the state flips and the clocks re-arm.
                        Ok(Event::UserPrompt { session, .. }) if apexos_core::is_worker_session(session.0) => {
                            let woke = wake_on_send(&mut workers, &bus, &models, session.0).await;
                            if woke { save_workers(&workers, &workers_path); }
                        }
                        // A worker turn errored (Error precedes the synthetic
                        // TurnComplete on the same session): remember, so the
                        // no-report completion lands Failed, never a hollow Done.
                        Ok(Event::Error { session: Some(session), .. }) if apexos_core::is_worker_session(session.0) => {
                            if let Some((_, w)) = workers.iter_mut().find(|(_, w)| w.session == session.0) {
                                w.errored = true;
                            }
                        }
                        // Cancel cascade (W1d): a conductor GOAL was cancelled →
                        // its whole batch goes with it. Session rides the event.
                        // M1b: any terminal conductor goal also tries closure —
                        // a settled mandala closes when its conductor is done
                        // (idempotent: boot re-emits persisted terminal states).
                        Ok(Event::GoalStateChanged { state, session: Some(parent), .. })
                            if matches!(state, GoalState::Cancelled | GoalState::Done | GoalState::Failed) => {
                            if state == GoalState::Cancelled {
                                let ids: Vec<u64> = workers.iter()
                                    .filter(|(_, w)| w.parent == parent.0 && !is_terminal(w.state))
                                    .map(|(id, _)| *id).collect();
                                // W2: the cascade reaches remote rows too — relayed
                                // to their hosting nodes, confirmed by poll/report.
                                let remote_ids: Vec<u64> = remotes.iter()
                                    .filter(|(_, r)| r.parent == parent.0 && !r.is_terminal())
                                    .map(|(id, _)| *id).collect();
                                if !ids.is_empty() || !remote_ids.is_empty() {
                                    eprintln!("[worker] cascade: conductor session {} cancelled → {} worker(s), {} remote", parent.0, ids.len(), remote_ids.len());
                                    cancel_workers(&mut workers, &bus, &proxy, &agents_dir, &yolo_set, &models, &ids).await;
                                    relay_remote_cancels(&mut remotes, &bus, &mesh, &remote_ids).await;
                                    admit_queued(&mut workers, &bus, &proxy, &yolo_set, &models, cap, max_steps).await;
                                    let (chg, reports) = check_batches(&workers, &remotes, &mut batches, &bus, &agents_dir).await;
                                    if chg { save_batches(&batches, &batches_path); }
                                    spawn_report_home(&mesh, reports).await;
                                    sync_cells(&mut trees, &cell_by_worker, &workers, &worktrees_dir, &agents_dir);
                                    save_workers(&workers, &workers_path);
                                    remote::save_remotes(&remotes, &mesh.remotes_path);
                                    update_gauges(&workers, &mesh);
                                }
                            }
                            if try_close_for_conductor(&mut mandalas, &mut trees, &worktrees_dir, parent.0) {
                                mandala::save_mandalas(&mandalas, &mandalas_path);
                            }
                        }
                        // PB-1 soft breaker: sequential local spawns from one
                        // session — nudge toward ONE task_fanout batch.
                        Ok(Event::SubAgentStarted { parent, .. }) => {
                            pb1_track(&mut spawn_log, &bus, parent.0).await;
                        }
                        // M1d: an orbit council finished deliberating → bank
                        // its synthesis on the mandala record so the next
                        // mandala_status carries the reading to the conductor
                        // (the driver never prompts a goal conductor — that
                        // edge stays the goal driver's).
                        Ok(Event::CouncilComplete { council_id, synthesis, .. })
                            if council_id.starts_with("mnd") => {
                            let mid = council_id[3..].split('e').next()
                                .and_then(|s| s.parse::<u64>().ok());
                            let banked = mid.and_then(|id| mandalas.get_mut(&id)).map(|m| {
                                if m.orbit_council.as_deref() == Some(council_id.as_str()) {
                                    m.orbit_synthesis = Some(synthesis.chars().take(300).collect());
                                    true
                                } else { false }
                            }).unwrap_or(false);
                            if banked {
                                mandala::save_mandalas(&mandalas, &mandalas_path);
                                eprintln!("[mandala] {} orbit council synthesis banked", mid.unwrap_or(0));
                            }
                        }
                        _ => {}
                    }
                }
                _ = tick.tick() => {
                    // THE REVIEW PROCEDURE (M1b): due workers → posture × word
                    // → one single-line remediation, applied through the same
                    // paths the old stall/TTL sweeps used — same detail
                    // strings, same clocks, same terminal trail.
                    let now = Instant::now();
                    let due: Vec<u64> = workers.iter()
                        .filter(|(_, w)| w.next_review.is_some_and(|t| t <= now))
                        .map(|(id, _)| *id)
                        .collect();
                    let mut failed_any = false;
                    let mut parked_any = false;
                    let mut reaped_any = false;
                    let mut barriers_due = false;
                    for wid in due {
                        let (barrier_ready, barrier_deadline_in) = if workers[&wid].barrier_held {
                            barrier_signals(&trees, &cell_by_worker, wid)
                        } else { (None, None) };
                        let batch_deadline_ok = batches.get(&workers[&wid].batch)
                            .map(|b| b.reported || epoch_now() < b.created_epoch.saturating_add(b.deadline_s))
                            .unwrap_or(true);
                        let (posture, word) = build_review(
                            &workers[&wid], barrier_ready, batch_deadline_ok,
                            max_steps, step_timeout, idle_ttl);
                        if let Some((mid, _)) = cell_by_worker.get(&wid) {
                            *censuses.entry(*mid).or_default()
                                .entry(review::census_key(posture, &word)).or_insert(0) += 1;
                        }
                        match review::review(posture, &word) {
                            Remediation::Healthy => {}
                            Remediation::Fail => {
                                let w = workers.get_mut(&wid).unwrap();
                                w.state = WorkerState::Failed;
                                w.turn_inflight = false;
                                disarm_worker(&yolo_set, &models, w.session);
                                let w = &workers[&wid];
                                emit_state(&bus, wid, w, "step stalled — no completion").await;
                                eprintln!("[worker] {wid} failed (stalled > {}s)", step_timeout.as_secs());
                                finalize_terminal(&proxy, &agents_dir, wid, w).await;
                                failed_any = true;
                            }
                            Remediation::Park => {
                                let w = workers.get_mut(&wid).unwrap();
                                if w.state != WorkerState::Parked {
                                    w.state = WorkerState::Parked;
                                    disarm_worker(&yolo_set, &models, w.session);
                                    let w = &workers[&wid];
                                    emit_state(&bus, wid, w, "idle TTL — parked (a send revives)").await;
                                    eprintln!("[worker] {wid} parked (idle TTL)");
                                    parked_any = true;
                                }
                            }
                            Remediation::Reap => { reaped_any = true; } // censused above; schedule drops below
                            Remediation::OpenBarrier => { barriers_due = true; }
                            Remediation::Cancel => {
                                // Unreached by M1b's conservative demand builder;
                                // defined so the table stays total. Honest if armed:
                                cancel_workers(&mut workers, &bus, &proxy, &agents_dir, &yolo_set, &models, &[wid]).await;
                            }
                        }
                        schedule_next_review(workers.get_mut(&wid).unwrap(), posture, &word,
                                             step_timeout, idle_ttl, barrier_deadline_in);
                    }
                    if barriers_due
                        && check_barriers(&mut workers, &mut trees, &cell_by_worker, &mandalas, &bus, &worktrees_dir).await {
                        admit_queued(&mut workers, &bus, &proxy, &yolo_set, &models, cap, max_steps).await;
                    }
                    if failed_any {
                        admit_queued(&mut workers, &bus, &proxy, &yolo_set, &models, cap, max_steps).await;
                    }
                    let (chg, reports) = check_batches(&workers, &remotes, &mut batches, &bus, &agents_dir).await;
                    if chg { save_batches(&batches, &batches_path); }
                    spawn_report_home(&mesh, reports).await;
                    if failed_any || reaped_any { sync_cells(&mut trees, &cell_by_worker, &workers, &worktrees_dir, &agents_dir); }
                    if failed_any || parked_any { save_workers(&workers, &workers_path); }
                    // W2: due remote polls — supervision across the wire on the
                    // review cadence (dark peers skipped; a poll is never the
                    // liveness probe, and a poll failure never fails a row —
                    // the batch deadline is the net).
                    run_due_polls(&remotes, &mut polls, &batches, &mesh, &mesh_out_tx).await;
                    // M1d: torus epochs — census/fingerprint rollovers, the
                    // orbit detector, the reading to Cerebro. Settled trees
                    // rest; an orbit convenes a council, never a restart.
                    if roll_due_epochs(&mut mandalas, &trees, &remotes, &mut censuses, &mut next_epoch,
                                       &proxy, &council_tx) {
                        mandala::save_mandalas(&mandalas, &mandalas_path);
                    }
                    update_gauges(&workers, &mesh);
                }
            }
        }
    });
}

/// Boot: reload the batch ledger, re-seeding the batch counter defensively
/// (workers.json usually re-seeds it too, but an all-terminal batch whose
/// workers were pruned by hand must still never collide).
fn reload_batches(batches: &mut HashMap<u64, BatchMeta>, path: &PathBuf, next_batch_id: &mut u64) {
    for pb in load_batches(path) {
        *next_batch_id = (*next_batch_id).max(pb.batch + 1);
        batches.insert(pb.batch, BatchMeta {
            parent: pb.parent, created_epoch: pb.created_epoch,
            deadline_s: pb.deadline_s, reported: pb.reported,
            inline_ack: None, // the blocked caller died with the old process
            origin: pb.origin_node.zip(pb.origin_batch),
        });
    }
}

/// Boot: reload persisted workers, parking every non-terminal one — the state
/// survives, nothing auto-runs (revive-on-send lands in W1b). Re-seeds all
/// three counters past the loaded maxima.
async fn reload_workers(
    workers: &mut HashMap<u64, Worker>, bus: &BusHandle, path: &PathBuf,
    next_worker_id: &mut u64, next_batch_id: &mut u64, next_worker_sid: &mut u64,
) {
    let loaded = load_workers(path);
    if loaded.is_empty() { return; }
    for pw in loaded {
        *next_worker_id  = (*next_worker_id).max(pw.id + 1);
        *next_batch_id   = (*next_batch_id).max(pw.batch + 1);
        *next_worker_sid = (*next_worker_sid).max(pw.session + 1);
        let state = parked_form(pw.state);
        workers.insert(pw.id, Worker {
            batch: pw.batch, parent: pw.parent, session: pw.session,
            task: pw.task, state, step: pw.step, summary: pw.summary,
            artifacts: pw.artifacts, episode: pw.episode,
            started: Instant::now(), pending: None, turn_inflight: false,
            yolo: pw.yolo, model: pw.model, errored: false,
            step_ceiling: pw.step_ceiling,
            // Parked/terminal reloads are inert — the review schedule picks a
            // worker up again at wake/revive (never-auto-resume, M1b too).
            barrier_held: false, next_review: None,
            last_review_key: None, review_attempt: 0,
        });
    }
    let mut ids: Vec<u64> = workers.keys().copied().collect();
    ids.sort_unstable();
    for id in ids {
        let w = &workers[&id];
        let detail = if w.state == WorkerState::Parked { "parked by daemon restart" } else { "" };
        emit_state(bus, id, w, detail).await;
    }
    eprintln!("[worker] reloaded workers from {} (non-terminal ones parked)", path.display());
}

// ── W2 mesh workers — the driver's remote seam ──────────────────────────────

/// The remote-row emission chokepoint, `emit_state`'s twin. Remote rows ride
/// `session: SessionId(0)` (the sentinel — their real session lives on the
/// peer; the router's eviction guard keys off the worker RANGE, so the
/// sentinel can never evict anything) plus `node: Some(..)`.
async fn emit_remote_state(bus: &BusHandle, id: u64, r: &RemoteWorker, detail: &str) {
    bus.emit(Event::WorkerStateChanged {
        worker:  WorkerId(id),
        batch:   r.batch,
        parent:  SessionId(r.parent),
        session: SessionId(0),
        task:    r.task.chars().take(80).collect(),
        state:   remote::row_state(&r.state_raw),
        detail:  if detail.is_empty() { format!("{} @ {}", r.state_raw, r.node) } else { detail.into() },
        yolo:    false,
        node:    Some(r.node.clone()),
    }).await;
}

/// Boot: reload conductor-side mirror rows, re-seed the shared counters, and
/// resume the poll schedule for unsettled batches — the reconcile path after
/// a conductor restart (the peer kept working while we were down; polls pick
/// up whatever happened).
async fn reload_remotes(
    remotes: &mut HashMap<u64, RemoteWorker>,
    polls: &mut HashMap<(u64, String), PollTarget>,
    batches: &HashMap<u64, BatchMeta>,
    bus: &BusHandle, path: &Path,
    next_worker_id: &mut u64, next_batch_id: &mut u64,
) {
    let loaded = remote::load_remotes(path);
    if loaded.is_empty() { return; }
    for pr in loaded {
        *next_worker_id = (*next_worker_id).max(pr.id + 1);
        *next_batch_id  = (*next_batch_id).max(pr.batch + 1);
        remotes.insert(pr.id, RemoteWorker {
            batch: pr.batch, parent: pr.parent, node: pr.node, task: pr.task,
            model: pr.model, remote_batch: pr.remote_batch, remote_worker: pr.remote_worker,
            remote_session: pr.remote_session,
            state_raw: if pr.state_raw.is_empty() { remote::STATE_ASSIGNING.into() } else { pr.state_raw },
            summary: pr.summary, evidence: pr.evidence, assigned_epoch: pr.assigned_epoch,
        });
    }
    let mut ids: Vec<u64> = remotes.keys().copied().collect();
    ids.sort_unstable();
    for id in &ids {
        emit_remote_state(bus, *id, &remotes[id], "").await;
    }
    // Resume polling any (batch, node) group that was accepted and is not yet
    // settled+reported — golden offsets de-phase the groups like reviews.
    for (id, r) in remotes.iter() {
        if r.remote_batch.is_none() { continue; }
        let unsettled = !r.is_terminal()
            || batches.get(&r.batch).map(|b| !b.reported).unwrap_or(false);
        if unsettled {
            polls.entry((r.batch, r.node.clone())).or_insert_with(|| PollTarget {
                next: Instant::now() + review::golden_offset(*id, REVIEW_PERIOD),
                attempt: 0, fingerprint: 0, inflight: false,
            });
        }
    }
    eprintln!("[worker] reloaded {} remote row(s) from {} ({} poll target(s) resumed)",
              remotes.len(), path.display(), polls.len());
}

/// Fail a set of remote rows honestly (assign refused / transport dead / peer
/// dark): terminal state, a mirror evidence file that names the cause, the
/// state card — so batch math and integration never see a silent hole.
async fn fail_remote_rows(
    remotes: &mut HashMap<u64, RemoteWorker>,
    bus: &BusHandle, agents_dir: &Path,
    wids: &[u64], cause: &str,
) {
    for wid in wids {
        let Some(r) = remotes.get_mut(wid) else { continue };
        if r.is_terminal() { continue; }
        r.state_raw = "failed".into();
        r.summary = Some(format!("remote assignment failed: {cause}"));
        let mirror = remote::fold_remote_evidence(
            remote::compose_mirror_doc(*wid, r, &chrono::Utc::now().to_rfc3339()), None);
        let path = evidence_path(agents_dir, *wid);
        let _ = std::fs::create_dir_all(agents_dir);
        if let Ok(json) = serde_json::to_string_pretty(&mirror) {
            let tmp = path.with_extension("json.tmp");
            if std::fs::write(&tmp, &json).is_ok() { let _ = std::fs::rename(&tmp, &path); }
        }
        r.evidence = Some(path.to_string_lossy().into_owned());
        let r = &remotes[wid];
        emit_remote_state(bus, *wid, r, &format!("assign failed: {cause}")).await;
        eprintln!("[worker] remote {wid} failed: {cause}");
    }
}

/// An assign POST answered (or died): join the peer's minted ids back onto
/// our rows by request order, arm the poll target — or fail the rows with
/// the honest cause.
#[allow(clippy::too_many_arguments)]
async fn handle_assign_outcome(
    remotes: &mut HashMap<u64, RemoteWorker>,
    polls: &mut HashMap<(u64, String), PollTarget>,
    bus: &BusHandle, agents_dir: &Path,
    batch: u64, node: &str, wids: Vec<u64>,
    result: Result<serde_json::Value, String>,
) {
    let accept = result.and_then(|v| remote::parse_fanout_accept(&v));
    match accept {
        Ok((peer_batch, rows)) => {
            for ar in rows {
                let Some(wid) = wids.get(ar.index) else { continue };
                if let Some(r) = remotes.get_mut(wid) {
                    r.remote_batch = Some(peer_batch);
                    r.remote_worker = Some(ar.worker);
                    r.remote_session = Some(ar.session);
                    r.state_raw = "queued".into();
                    let r = &remotes[wid];
                    emit_remote_state(bus, *wid, r, &format!("assigned to {node} (worker {}, session {})", ar.worker, ar.session)).await;
                }
            }
            polls.insert((batch, node.to_string()), PollTarget {
                next: Instant::now() + review::golden_offset(wids.first().copied().unwrap_or(batch), REVIEW_PERIOD),
                attempt: 0, fingerprint: 0, inflight: false,
            });
            eprintln!("[worker] batch {batch}: {} task(s) assigned to {node} (peer batch {peer_batch})", wids.len());
        }
        Err(e) => {
            fail_remote_rows(remotes, bus, agents_dir, &wids, &e).await;
        }
    }
}

/// Apply a rows payload (poll reply or report-home push) onto the mirror
/// rows: states follow the peer verbatim, terminal rows get their evidence
/// MIRROR written once (`agents/<local_wid>.json` — reading it IS the
/// integration step; artifacts stay on the peer). Returns whether anything
/// changed (the poll ladder's reset signal).
async fn apply_wire_rows(
    remotes: &mut HashMap<u64, RemoteWorker>,
    bus: &BusHandle, agents_dir: &Path,
    batch: u64, node: &str, rows: &[remote::WireRow],
) -> bool {
    let mut changed = false;
    for wire in rows {
        let hit = remotes.iter()
            .find(|(_, r)| r.batch == batch && r.node == node && r.remote_worker == Some(wire.worker))
            .map(|(id, _)| *id);
        let Some(wid) = hit else { continue };
        let r = remotes.get_mut(&wid).unwrap();
        let state_changed = r.state_raw != wire.state;
        if state_changed { r.state_raw = wire.state.clone(); }
        if wire.summary.is_some() && r.summary != wire.summary { r.summary = wire.summary.clone(); changed = true; }
        if state_changed { changed = true; }
        if r.is_terminal() && r.evidence.is_none() {
            let mirror = remote::fold_remote_evidence(
                remote::compose_mirror_doc(wid, r, &chrono::Utc::now().to_rfc3339()),
                wire.evidence_doc.as_ref());
            let path = evidence_path(agents_dir, wid);
            let _ = std::fs::create_dir_all(agents_dir);
            if let Ok(json) = serde_json::to_string_pretty(&mirror) {
                let tmp = path.with_extension("json.tmp");
                if std::fs::write(&tmp, &json).is_ok() { let _ = std::fs::rename(&tmp, &path); }
            }
            r.evidence = Some(path.to_string_lossy().into_owned());
            changed = true;
        }
        if state_changed {
            let r = &remotes[&wid];
            emit_remote_state(bus, wid, r, "").await;
            eprintln!("[worker] remote {wid} @ {node} → {}", r.state_raw);
        }
    }
    changed
}

/// A poll answered (or died): fold the snapshot in, ride the ladder — a
/// changed picture resets to the base period, a quiet or failed one climbs
/// fib. Poll failures NEVER fail a row (the batch deadline is the net).
async fn handle_poll_outcome(
    remotes: &mut HashMap<u64, RemoteWorker>,
    polls: &mut HashMap<(u64, String), PollTarget>,
    bus: &BusHandle, agents_dir: &Path,
    batch: u64, node: &str,
    result: Result<serde_json::Value, String>,
) -> bool {
    let mut changed = false;
    if let Some(t) = polls.get_mut(&(batch, node.to_string())) {
        t.inflight = false;
        match result {
            Ok(v) => {
                let rows = remote::parse_rows(&v);
                let fp = remote::rows_fingerprint(&rows);
                changed = apply_wire_rows(remotes, bus, agents_dir, batch, node, &rows).await;
                let fp_changed = fp != t.fingerprint;
                t.fingerprint = fp;
                t.attempt = if fp_changed || changed { 0 } else { t.attempt.saturating_add(1) };
                t.next = Instant::now() + remote::next_poll_in(t.attempt, fp_changed || changed, REVIEW_PERIOD);
            }
            Err(e) => {
                t.attempt = t.attempt.saturating_add(1);
                t.next = Instant::now() + remote::next_poll_in(t.attempt, false, REVIEW_PERIOD);
                eprintln!("[worker] poll {node} batch {batch}: {e} (deadline is the net)");
            }
        }
    }
    changed
}

/// Fire due polls: skip dark peers (attempt climbs so a dark stretch backs
/// off naturally), drop targets whose batch is settled AND reported, spawn
/// the query POST otherwise.
async fn run_due_polls(
    remotes: &HashMap<u64, RemoteWorker>,
    polls: &mut HashMap<(u64, String), PollTarget>,
    batches: &HashMap<u64, BatchMeta>,
    mesh: &MeshDeps,
    mesh_out_tx: &mpsc::Sender<MeshOutcome>,
) {
    let now = Instant::now();
    let due: Vec<(u64, String)> = polls.iter()
        .filter(|(_, t)| !t.inflight && t.next <= now)
        .map(|(k, _)| k.clone())
        .collect();
    for key in due {
        let (batch, node) = key.clone();
        let group: Vec<&RemoteWorker> = remotes.values()
            .filter(|r| r.batch == batch && r.node == node)
            .collect();
        let all_terminal = !group.is_empty() && group.iter().all(|r| r.is_terminal());
        let reported = batches.get(&batch).map(|b| b.reported).unwrap_or(true);
        if group.is_empty() || (all_terminal && reported) {
            polls.remove(&key);
            continue;
        }
        let Some(peer_batch) = group.iter().find_map(|r| r.remote_batch) else {
            // Never accepted — assign already failed the rows; drop the target.
            polls.remove(&key);
            continue;
        };
        if apexos_gateway::beacon::peer_liveness(&mesh.liveness, &node).await.0 == "dark" {
            let t = polls.get_mut(&key).unwrap();
            t.attempt = t.attempt.saturating_add(1);
            t.next = now + remote::next_poll_in(t.attempt, false, REVIEW_PERIOD);
            continue;
        }
        let Some((base, token)) = peer_http(&mesh.peers, &node).await else {
            let t = polls.get_mut(&key).unwrap();
            t.attempt = t.attempt.saturating_add(1);
            t.next = now + remote::next_poll_in(t.attempt, false, REVIEW_PERIOD);
            continue;
        };
        let t = polls.get_mut(&key).unwrap();
        t.inflight = true;
        let body = serde_json::json!({ "from": *mesh.node_id, "batch": peer_batch });
        let tx = mesh_out_tx.clone();
        tokio::spawn(async move {
            let result = mesh_post(&base, "/api/worker/query", token.as_deref(), &body).await;
            let _ = tx.send(MeshOutcome::Poll { batch, node, result }).await;
        });
    }
}

/// Push a settled hosted batch home to its origin conductor (peer role).
/// Fire-and-forget with a short retry ladder — a conductor that stays dark
/// reconciles by its own polls; its batch deadline is the final net.
async fn spawn_report_home(mesh: &MeshDeps, reports: Vec<(String, u64, u64, Vec<remote::WireRow>)>) {
    for (node, origin_batch, local_batch, rows) in reports {
        let Some((base, token)) = peer_http(&mesh.peers, &node).await else {
            eprintln!("[worker] report-home: origin peer '{node}' no longer registered — its polls must reconcile");
            continue;
        };
        let body = remote::build_rows_body(&mesh.node_id, origin_batch, local_batch, &rows);
        tokio::spawn(async move {
            for (i, delay) in std::iter::once(0u64).chain(remote::REPORT_RETRY_DELAYS_S).enumerate() {
                if delay > 0 { tokio::time::sleep(Duration::from_secs(delay)).await; }
                match mesh_post(&base, "/api/worker/report", token.as_deref(), &body).await {
                    Ok(v) if v["ok"].as_bool() == Some(true) => {
                        eprintln!("[worker] batch {local_batch} reported home to {node} (origin batch {origin_batch})");
                        return;
                    }
                    Ok(v) => eprintln!("[worker] report-home to {node} refused (try {}): {}", i + 1, v["error"].as_str().unwrap_or("?")),
                    Err(e) => eprintln!("[worker] report-home to {node} failed (try {}): {e}", i + 1),
                }
            }
            eprintln!("[worker] report-home to {node} gave up — the conductor's polls reconcile");
        });
    }
}

/// Relay a cancel to the hosting nodes for a set of remote rows. Rows go
/// `cancel requested` (NON-terminal — the peer's confirmation arrives by
/// poll/report; a peer that never answers is bounded by the batch deadline).
async fn relay_remote_cancels(
    remotes: &mut HashMap<u64, RemoteWorker>,
    bus: &BusHandle, mesh: &MeshDeps,
    ids: &[u64],
) {
    // Group by (batch, node) — one POST per hosting peer batch.
    let mut groups: HashMap<(u64, String), (Option<u64>, Vec<u64>)> = HashMap::new();
    for id in ids {
        let Some(r) = remotes.get_mut(id) else { continue };
        if r.is_terminal() { continue; }
        r.state_raw = remote::STATE_CANCEL_REQUESTED.into();
        let entry = groups.entry((r.batch, r.node.clone())).or_insert((r.remote_batch, Vec::new()));
        if let Some(rw) = r.remote_worker { entry.1.push(rw); }
        let r = &remotes[id];
        emit_remote_state(bus, *id, r, &format!("cancel relayed to {}", r.node)).await;
    }
    for ((_batch, node), (remote_batch, workers)) in groups {
        let Some(rb) = remote_batch else { continue }; // never accepted — assign path already failed it
        let Some((base, token)) = peer_http(&mesh.peers, &node).await else { continue };
        let body = serde_json::json!({
            "from": *mesh.node_id, "batch": rb,
            "workers": workers,
        });
        tokio::spawn(async move {
            match mesh_post(&base, "/api/worker/cancel", token.as_deref(), &body).await {
                Ok(v) if v["ok"].as_bool() == Some(true) => {}
                Ok(v) => eprintln!("[worker] cancel relay to {node} refused: {}", v["error"].as_str().unwrap_or("?")),
                Err(e) => eprintln!("[worker] cancel relay to {node} failed: {e} (deadline is the net)"),
            }
        });
    }
}

/// Dispatch one gateway mesh request (W2). Returns whether driver state
/// changed (the caller persists + re-checks batches).
#[allow(clippy::too_many_arguments)]
async fn handle_mesh_req(
    workers: &mut HashMap<u64, Worker>,
    remotes: &mut HashMap<u64, RemoteWorker>,
    batches: &mut HashMap<u64, BatchMeta>,
    polls: &mut HashMap<(u64, String), PollTarget>,
    bus: &BusHandle, proxy: &ToolProxy, agents_dir: &Path,
    yolo_set: &GoalYoloSessions, models: &WorkerModels,
    cap: usize, max_steps: u32,
    mesh: &MeshDeps,
    next_worker_id: &mut u64, next_batch_id: &mut u64, next_worker_sid: &mut u64,
    req: WorkerMeshReq,
) -> bool {
    let WorkerMeshReq { kind, from, body, parent, reply } = req;
    match kind {
        // ── peer role: host a batch for a remote conductor ──
        WorkerMeshKind::Fanout => {
            let origin_batch = body["origin_batch"].as_u64().unwrap_or(0);
            let Some(parent) = parent else {
                let _ = reply.send(serde_json::json!({ "ok": false, "error": "no landing session resolved" }));
                return false;
            };
            let items = body["tasks"].as_array().cloned().unwrap_or_default();
            if items.is_empty() || items.len() > MAX_BATCH_TASKS {
                let _ = reply.send(serde_json::json!({ "ok": false, "error": format!("tasks must be 1..={MAX_BATCH_TASKS}") }));
                return false;
            }
            let mut tasks: Vec<(String, Option<String>, u32)> = Vec::with_capacity(items.len());
            for it in &items {
                let prompt = it["prompt"].as_str().map(str::trim).unwrap_or("");
                if prompt.is_empty() {
                    let _ = reply.send(serde_json::json!({ "ok": false, "error": "a task has no prompt" }));
                    return false;
                }
                let model = it["model"].as_str().map(str::trim).filter(|s| !s.is_empty() && s.len() <= 64).map(str::to_owned);
                // M2: an assignment may carry a step ceiling (a remote CELL's
                // budget — the contract crosses as assignment data). Absent =
                // 0, the env-global sentinel: exactly a W2 plain task.
                let ceiling = remote::hosted_step_ceiling(it["steps"].as_u64());
                tasks.push((remote::provenance_prefix(&from, origin_batch, prompt), model, ceiling));
            }
            let deadline_s = body["deadline_s"].as_u64().map(|n| n.clamp(60, 86_400)).unwrap_or(DEFAULT_BATCH_DEADLINE_S);
            let batch = *next_batch_id; *next_batch_id += 1;
            batches.insert(batch, BatchMeta {
                parent: parent.0, created_epoch: epoch_now(), deadline_s, reported: false,
                inline_ack: None,
                origin: Some((from.clone(), origin_batch)),
            });
            let mut minted_rows: Vec<serde_json::Value> = Vec::with_capacity(tasks.len());
            let mut minted_ids: Vec<u64> = Vec::with_capacity(tasks.len());
            for (i, (task, model, ceiling)) in tasks.into_iter().enumerate() {
                // Ordinary local workers: this node's cap/FIFO/policy/review
                // all apply. yolo is ALWAYS false — it never crosses the wire.
                let (wid, sid) = mint_local_worker(workers, next_worker_id, next_worker_sid,
                    batch, parent.0, task, model, false, false, ceiling);
                minted_rows.push(serde_json::json!({ "index": i, "worker": wid, "session": sid }));
                minted_ids.push(wid);
            }
            for wid in &minted_ids {
                emit_state(bus, *wid, &workers[wid], "queued (mesh)").await;
            }
            admit_queued(workers, bus, proxy, yolo_set, models, cap, max_steps).await;
            let admitted = minted_ids.iter().filter(|id| workers[id].state == WorkerState::Running).count();
            eprintln!("[worker] hosting batch {batch} for {from} (origin batch {origin_batch}): {} task(s), {admitted} admitted", minted_ids.len());
            let _ = reply.send(serde_json::json!({
                "ok": true, "batch": batch, "workers": minted_rows,
                "cap": cap, "admitted": admitted, "queued": minted_ids.len() - admitted,
            }));
            true
        }
        // ── peer role: the origin conductor polling its hosted batch ──
        WorkerMeshKind::Query => {
            let Some(batch) = body["batch"].as_u64() else {
                let _ = reply.send(serde_json::json!({ "ok": false, "error": "missing batch" }));
                return false;
            };
            let owned = batches.get(&batch)
                .and_then(|b| b.origin.as_ref())
                .map(|(n, _)| n == &from)
                .unwrap_or(false);
            if !owned {
                let _ = reply.send(serde_json::json!({ "ok": false, "error": "not your hosted batch" }));
                return false;
            }
            let origin_batch = batches[&batch].origin.as_ref().map(|(_, ob)| *ob).unwrap_or(0);
            let rows = wire_rows_for_batch(workers, batch, agents_dir);
            let _ = reply.send(remote::build_rows_body(&mesh.node_id, origin_batch, batch, &rows));
            false
        }
        // ── peer role: the origin conductor cancelling its hosted batch ──
        WorkerMeshKind::Cancel => {
            let Some(batch) = body["batch"].as_u64() else {
                let _ = reply.send(serde_json::json!({ "ok": false, "error": "missing batch" }));
                return false;
            };
            let owned = batches.get(&batch)
                .and_then(|b| b.origin.as_ref())
                .map(|(n, _)| n == &from)
                .unwrap_or(false);
            if !owned {
                let _ = reply.send(serde_json::json!({ "ok": false, "error": "not your hosted batch" }));
                return false;
            }
            let asked: Option<Vec<u64>> = body["workers"].as_array()
                .map(|a| a.iter().filter_map(|v| v.as_u64()).collect());
            let ids: Vec<u64> = workers.iter()
                .filter(|(id, w)| w.batch == batch && !is_terminal(w.state)
                    && asked.as_ref().map(|a| a.contains(id)).unwrap_or(true))
                .map(|(id, _)| *id).collect();
            if !ids.is_empty() {
                cancel_workers(workers, bus, proxy, agents_dir, yolo_set, models, &ids).await;
                admit_queued(workers, bus, proxy, yolo_set, models, cap, max_steps).await;
            }
            eprintln!("[worker] mesh cancel from {from}: batch {batch}, {} worker(s)", ids.len());
            let _ = reply.send(serde_json::json!({ "ok": true, "cancelled": ids, "count": ids.len() }));
            !ids.is_empty()
        }
        // ── conductor role: a hosting peer pushing its settled batch home ──
        WorkerMeshKind::Report => {
            let Some(batch) = body["origin_batch"].as_u64() else {
                let _ = reply.send(serde_json::json!({ "ok": false, "error": "missing origin_batch" }));
                return false;
            };
            let known = remotes.values().any(|r| r.batch == batch && r.node == from);
            if !known {
                let _ = reply.send(serde_json::json!({ "ok": false, "error": "no such remote batch here" }));
                return false;
            }
            let rows = remote::parse_rows(&body);
            let changed = apply_wire_rows(remotes, bus, agents_dir, batch, &from, &rows).await;
            // The push replaces the next poll — the ladder resets so a
            // follow-up straggler revival is observed promptly.
            if let Some(t) = polls.get_mut(&(batch, from.clone())) {
                t.attempt = 0;
                t.next = Instant::now() + REVIEW_PERIOD;
            }
            eprintln!("[worker] report-home received from {from}: batch {batch}, {} row(s)", rows.len());
            let _ = reply.send(serde_json::json!({ "ok": true, "applied": rows.len() }));
            changed
        }
    }
}

/// Build the wire rows for a hosted batch (peer role): every worker of the
/// batch, terminal rows carrying their evidence DOC inline (one hop — the
/// conductor mirrors it; artifacts stay in this node's workspace).
fn wire_rows_for_batch(workers: &HashMap<u64, Worker>, batch: u64, agents_dir: &Path) -> Vec<remote::WireRow> {
    let mut rows: Vec<(u64, remote::WireRow)> = workers.iter()
        .filter(|(_, w)| w.batch == batch)
        .map(|(id, w)| {
            let terminal = is_terminal(w.state);
            (*id, remote::WireRow {
                worker: *id,
                session: w.session,
                state: remote::state_str(w.state),
                timed_out: false,
                summary: w.summary.clone(),
                evidence_doc: if terminal { read_evidence_doc(agents_dir, *id) } else { None },
            })
        })
        .collect();
    rows.sort_by_key(|(id, _)| *id);
    rows.into_iter().map(|(_, r)| r).collect()
}

/// Mint one local worker (Queued; a held gate enters the review schedule at
/// a golden offset — its barrier timeout is a review clock). Returns
/// (worker id, session id). The one Worker literal both fan paths share.
#[allow(clippy::too_many_arguments)]
fn mint_local_worker(
    workers: &mut HashMap<u64, Worker>,
    next_worker_id: &mut u64, next_worker_sid: &mut u64,
    batch: u64, parent: u64,
    task: String, model: Option<String>, yolo: bool, held: bool, ceiling: u32,
) -> (u64, u64) {
    let wid = *next_worker_id;  *next_worker_id  += 1;
    let sid = *next_worker_sid; *next_worker_sid += 1;
    workers.insert(wid, Worker {
        batch, parent, session: sid,
        task, state: WorkerState::Queued, step: 1, summary: None,
        artifacts: Vec::new(), episode: None,
        started: Instant::now(), pending: None, turn_inflight: false,
        yolo, model, errored: false, step_ceiling: ceiling,
        barrier_held: held,
        // A held gate is reviewed (its timeout is a review clock); plain
        // Queued workers are inert until admission schedules them.
        next_review: if held {
            Some(Instant::now() + review::golden_offset(wid, REVIEW_PERIOD))
        } else { None },
        last_review_key: None, review_attempt: 0,
    });
    (wid, sid)
}

/// Mint one remote mirror row (W2/M2) and join its per-node assign group in
/// request order (the accept joins peer ids back by index). Spends the SAME
/// worker-id counter as local workers — batch rows, evidence mirrors and
/// mandala cell bindings stay uniform. `steps` is the M2 assignment field
/// (a cell's budget crossing the wire); plain W2 tasks pass None.
#[allow(clippy::too_many_arguments)]
fn mint_remote_row(
    remotes: &mut HashMap<u64, RemoteWorker>,
    remote_groups: &mut Vec<(String, Vec<u64>, Vec<remote::RemoteTaskItem>)>,
    next_worker_id: &mut u64,
    batch: u64, parent: u64,
    node: String, prompt: String, model: Option<String>, steps: Option<u16>,
) -> u64 {
    let wid = *next_worker_id; *next_worker_id += 1;
    remotes.insert(wid, RemoteWorker {
        batch, parent, node: node.clone(), task: prompt.clone(),
        model: model.clone(), remote_batch: None, remote_worker: None,
        remote_session: None, state_raw: remote::STATE_ASSIGNING.into(),
        summary: None, evidence: None, assigned_epoch: epoch_now(),
    });
    let item = remote::RemoteTaskItem { prompt, model, steps };
    match remote_groups.iter_mut().find(|(n, _, _)| n == &node) {
        Some((_, wids, items)) => { wids.push(wid); items.push(item); }
        None => remote_groups.push((node, vec![wid], vec![item])),
    }
    wid
}

/// task_fanout: mint the batch, ack the conductor with the ids, then admit up
/// to the cap. Refused from a worker session — workers are depth-1 (vouchers
/// are the M-tier mechanism; there is no partial fan below the conductor).
/// Mandala fans (M1b) mint from validated CellPlans — rings, gates, diamonds
/// — and return the context so the driver can bind cells to workers. Returns
/// None when refused (nothing minted, honest ToolResult already emitted).
#[allow(clippy::too_many_arguments)]
async fn fanout(
    workers: &mut HashMap<u64, Worker>, batches: &mut HashMap<u64, BatchMeta>,
    mandalas: &HashMap<u64, MandalaRecord>,
    trees: &HashMap<u64, HashMap<String, CellRecord>>,
    cell_by_worker: &HashMap<u64, (u64, Addr)>,
    bus: &BusHandle, cap: usize, max_steps: u32, proxy: &ToolProxy,
    yolo_set: &GoalYoloSessions, models: &WorkerModels,
    call_session: SessionId, call_id: ActionId, args: serde_json::Value,
    next_worker_id: &mut u64, next_batch_id: &mut u64, next_worker_sid: &mut u64,
    mesh: &MeshDeps, mesh_out_tx: &mpsc::Sender<MeshOutcome>,
    remotes: &mut HashMap<u64, RemoteWorker>,
) -> Option<(Option<CellsCtx>, Vec<u64>)> {
    let refuse = |msg: String| Event::ToolResult {
        session: call_session, call: call_id,
        output: ToolOutput { ok: false, content: serde_json::json!(msg) },
    };
    let inline = match args["mode"].as_str() {
        None | Some("async") => false,
        Some("inline") => true,
        Some(other) => {
            bus.emit(refuse(format!("unknown mode \"{other}\" — async (default) or inline"))).await;
            return None;
        }
    };
    let tasks = match parse_tasks(&args) {
        Ok(t) => t,
        Err(e) => { bus.emit(refuse(e)).await; return None; }
    };
    if inline && tasks.len() > INLINE_MAX_TASKS {
        bus.emit(refuse(format!(
            "inline mode is short-batch-only ({} tasks > {INLINE_MAX_TASKS}) — use mode async; the batch report arrives via the AwaitingBatch loop", tasks.len()))).await;
        return None;
    }
    // The voucher gate (M1c): a worker session fans ONLY as a vouchered cell
    // sub-conducting its own subtree — async, same mandala, every law intact.
    let caller_cell: Option<Addr> = if apexos_core::is_worker_session(call_session.0) {
        let bound = workers.iter()
            .find(|(_, w)| w.session == call_session.0)
            .and_then(|(id, _)| cell_by_worker.get(id));
        let Some((mid, addr)) = bound else {
            bus.emit(refuse("workers cannot fan out work without a mandala-cell VOUCHER (depth-1 by design)".into())).await;
            return None;
        };
        let vouchered = trees.get(mid).and_then(|t| t.get(&addr.0)).map(|c| c.voucher).unwrap_or(false);
        if !vouchered {
            bus.emit(refuse("your cell carries no VOUCHER — only vouchered cells sub-conduct".into())).await;
            return None;
        }
        if args["mandala"].as_u64() != Some(*mid) {
            bus.emit(refuse(format!(
                "your voucher is for mandala {mid} — pass mandala: {mid} and grow your own subtree"))).await;
            return None;
        }
        if inline {
            bus.emit(refuse("sub-conductors fan async — an inline hold has no place inside the tree".into())).await;
            return None;
        }
        Some(addr.clone())
    } else { None };
    // W2 remote validation, refused EARLY (nothing minted): the kill switch,
    // the inline bound and the registry gate the whole fan before any row
    // exists. M2 lifted the old node+mandala refusal — cross-node RINGS are
    // legal now; which CELLS may carry a node is vetted per-plan inside
    // prepare_mandala_cells (`remote_cell_veto`: never the gate, a measured
    // or vouchered cell, or any cell of a code mandala).
    let remote_nodes: Vec<String> = {
        let mut ns: Vec<String> = tasks.iter().filter_map(|t| t.node.clone()).collect();
        ns.sort(); ns.dedup(); ns
    };
    if !remote_nodes.is_empty() {
        if !mesh.enabled {
            bus.emit(refuse("mesh workers are disabled on this node (AGENTD_MESH_WORKERS=0)".into())).await;
            return None;
        }
        if inline {
            bus.emit(refuse("node tasks fan async — remote latency has no place in an inline hold".into())).await;
            return None;
        }
        for n in &remote_nodes {
            if peer_http(&mesh.peers, n).await.is_none() {
                bus.emit(refuse(format!("'{n}' is not a registered mesh peer — list_mesh_peers shows them"))).await;
                return None;
            }
        }
    }

    // Mandala validation (M1b): geometry, ring widths, descent, join layout —
    // refused early, plans composed with the invariant + rituals verbatim.
    let ctx = match prepare_mandala_cells(mandalas, trees, bus, call_session, call_id, &args, &tasks, inline, caller_cell.as_ref()).await {
        Err(()) => return None, // honest refusal already emitted
        Ok(c) => c,
    };

    // Batch-inherited yolo: workers get the PARENT's auto-approve bit and never
    // more — explicit opt-in, and only if the calling session is itself armed.
    let inherit_requested = args["yolo"].as_str() == Some("inherit");
    let yolo = inherit_requested && apexos_core::goal_session_is_yolo(yolo_set, call_session.0);
    if inherit_requested && !yolo {
        eprintln!("[worker] yolo:inherit requested by session {} which is not yolo-armed — workers stay gated", call_session.0);
    }

    let batch = *next_batch_id;
    *next_batch_id += 1;
    let mut deadline_s = parse_batch_deadline(&args);
    if inline { deadline_s = deadline_s.min(INLINE_DEADLINE_CEIL_S); }
    if let Some(c) = &ctx { deadline_s = deadline_s.min(c.deadline_cap_s); }
    batches.insert(batch, BatchMeta {
        parent: call_session.0, created_epoch: epoch_now(), deadline_s, reported: false,
        inline_ack: if inline { Some((call_session.0, call_id.0)) } else { None },
        origin: None, // this node conducts; hosted batches mint in handle_mesh_req
    });
    // W2: split the plain fan into local and remote tasks. Remote tasks
    // become mirror rows spending the SAME worker-id counter, grouped per
    // node for one assign POST each. Mandala fans mint from plans below —
    // in PLAN ORDER, local or remote per plan (M2), so `cell_wids` lines up
    // with ctx.plans and cells bind by position whatever bodies them.
    let (local_tasks, remote_tasks): (Vec<TaskSpecItem>, Vec<TaskSpecItem>) = match &ctx {
        Some(_) => (Vec::new(), Vec::new()), // mandala path mints from plans below
        None => tasks.into_iter().partition(|t| t.node.is_none()),
    };
    // The hold rule (M1c refinement): pure-J cells hold at mint; an R+J cell
    // (FORGE) starts lapping immediately. Cell workers' ceilings come from
    // their budget (the contract, finally enforced) — and cross the wire as
    // the M2 `steps` assignment field when the body is remote. yolo NEVER
    // rides a remote row (the peer's policy is sovereign); model pins do.
    let mut minted: Vec<(u64, u64)> = Vec::new();      // local (worker_id, session)
    let mut cell_wids: Vec<u64> = Vec::new();          // plan-order wids (mandala fans)
    let mut remote_groups: Vec<(String, Vec<u64>, Vec<remote::RemoteTaskItem>)> = Vec::new();
    match &ctx {
        Some(c) => {
            for p in &c.plans {
                match &p.node {
                    Some(node) => {
                        let wid = mint_remote_row(remotes, &mut remote_groups, next_worker_id,
                            batch, call_session.0, node.clone(), p.prompt.clone(),
                            p.model.clone(), Some(p.budget.steps));
                        cell_wids.push(wid);
                    }
                    None => {
                        let held = holds_at_mint(p.barrier_timeout_s, p.measure.as_deref());
                        let ceiling = u32::from(p.budget.steps).clamp(1, 100);
                        let (wid, sid) = mint_local_worker(workers, next_worker_id, next_worker_sid,
                            batch, call_session.0, p.prompt.clone(), p.model.clone(), yolo, held, ceiling);
                        minted.push((wid, sid));
                        cell_wids.push(wid);
                    }
                }
            }
        }
        None => {
            for t in local_tasks {
                let (wid, sid) = mint_local_worker(workers, next_worker_id, next_worker_sid,
                    batch, call_session.0, t.prompt, t.model, yolo, false, 0);
                minted.push((wid, sid));
            }
            for t in &remote_tasks {
                mint_remote_row(remotes, &mut remote_groups, next_worker_id,
                    batch, call_session.0, t.node.clone().unwrap_or_default(),
                    t.prompt.clone(), t.model.clone(), None);
            }
        }
    }

    // Cards BEFORE the ack: bus order is delivery order, so the goal driver's
    // batch tracking (WorkerStateChanged → pending set) is provably armed
    // before the conductor's turn can resume off the ack and complete.
    // Remote rows emit here too — the pending set must cover mixed batches.
    for (wid, _) in &minted {
        let w = &workers[wid];
        emit_state(bus, *wid, w, if w.barrier_held { "barrier armed — waiting on descendants" } else { "queued" }).await;
    }
    for (_, wids, _) in &remote_groups {
        for wid in wids {
            let r = &remotes[wid];
            emit_remote_state(bus, *wid, r, &format!("assigning to {}", r.node)).await;
        }
    }

    let remote_count: usize = remote_groups.iter().map(|(_, wids, _)| wids.len()).sum();
    let n = minted.len() + remote_count;
    let admitted_now = (cap.saturating_sub(slots_used(workers))).min(minted.len());
    if !inline {
        // Async: ack now, work proceeds in background. Inline holds the ack —
        // the batch report IS this call's result (emitted by check_batches).
        let mut content = serde_json::json!({
            "batch": batch,
            "workers": minted.iter().map(|(w, s)| serde_json::json!({ "worker": w, "session": s })).collect::<Vec<_>>(),
            "count": n, "cap": cap,
            "admitted": admitted_now, "queued": minted.len() - admitted_now,
            "batch_deadline_s": deadline_s,
            "yolo_inherited": yolo,
            "status": "fanned",
        });
        if !remote_groups.is_empty() {
            content["remote"] = serde_json::json!(remote_groups.iter().flat_map(|(node, wids, _)| {
                wids.iter().map(move |w| serde_json::json!({ "worker": w, "node": node, "status": "assigning" }))
            }).collect::<Vec<_>>());
            content["remote_note"] = serde_json::json!(
                "remote tasks run on their peer's own worker tier (its cap, its policy — approvals land THERE); \
                 evidence mirrors land here when they settle");
        }
        if let Some(c) = &ctx {
            content["cells"] = serde_json::json!(c.plans.iter().map(|p| p.addr.0.clone()).collect::<Vec<_>>());
            if let Some(gate) = c.plans.iter().find(|p| p.barrier_timeout_s.is_some()) {
                content["gate"] = serde_json::json!({
                    "cell": gate.addr.0, "barrier_timeout_s": gate.barrier_timeout_s,
                    "note": "held until its descendant cells settle; fan under it if you haven't",
                });
            }
        }
        bus.emit(Event::ToolResult {
            session: call_session, call: call_id,
            output: ToolOutput { ok: true, content },
        }).await;
    }

    admit_queued(workers, bus, proxy, yolo_set, models, cap, max_steps).await;

    // W2: spawn one assign POST per hosting node — after the ack, never
    // blocking the loop. A beacon-dark peer fails fast through the SAME
    // outcome path (no 20s timeout burned against a known-dark node), and
    // the sends themselves are spawned so a full outcome channel can never
    // deadlock the driver against itself.
    for (node, wids, items) in remote_groups {
        let dark = apexos_gateway::beacon::peer_liveness(&mesh.liveness, &node).await.0 == "dark";
        let resolved = if dark { None } else { peer_http(&mesh.peers, &node).await };
        let tx = mesh_out_tx.clone();
        match resolved {
            Some((base, token)) => {
                let body = remote::build_fanout_body(&mesh.node_id, batch, deadline_s, &items);
                tokio::spawn(async move {
                    let result = mesh_post(&base, "/api/worker/fanout", token.as_deref(), &body).await;
                    let _ = tx.send(MeshOutcome::Assign { batch, node, wids, result }).await;
                });
            }
            None => {
                let cause = if dark { "peer dark (beacon)".to_string() } else { "peer vanished from the registry".to_string() };
                tokio::spawn(async move {
                    let _ = tx.send(MeshOutcome::Assign { batch, node, wids, result: Err(cause) }).await;
                });
            }
        }
    }

    eprintln!("[worker] batch {batch} fanned: {n} tasks from session {} (cap {cap}, deadline {deadline_s}s{}{}{})",
              call_session.0,
              if inline { ", inline" } else { "" },
              if yolo { ", yolo:inherit" } else { "" },
              if let Some(c) = &ctx { format!(", cells under {}", c.parent.0) } else { String::new() });
    Some((ctx, cell_wids))
}

/// Admit Queued workers (FIFO by id) while slots remain: Queued → Running,
/// stall clock armed, the work order goes out as an ordinary gated UserPrompt
/// on the worker's own session.
async fn admit_queued(workers: &mut HashMap<u64, Worker>, bus: &BusHandle, proxy: &ToolProxy, yolo_set: &GoalYoloSessions, models: &WorkerModels, cap: usize, max_steps: u32) {
    while slots_used(workers) < cap {
        let Some(id) = next_queued(workers) else { break };
        // Open the Cerebro episode at first admission — work actually begins
        // here, not at mint (a never-admitted worker leaves no episode).
        let episode = {
            let w = &workers[&id];
            if w.episode.is_none() { episode_start_worker(proxy, id, w.batch, &w.task).await } else { None }
        };
        let (session, text) = {
            let w = workers.get_mut(&id).unwrap();
            if episode.is_some() { w.episode = episode; }
            w.state = WorkerState::Running;
            w.started = Instant::now();
            w.turn_inflight = true;
            w.errored = false;
            // Enter the review schedule at a golden offset — siblings admitted
            // together still review de-phased (M1b).
            w.next_review = Some(Instant::now() + review::golden_offset(id, REVIEW_PERIOD));
            w.last_review_key = None;
            w.review_attempt = 0;
            (w.session, directive_first(id, w.batch, max_steps, &w.task))
        };
        let w = &workers[&id];
        arm_worker(yolo_set, models, w); // inherit-yolo + model pin follow residency
        emit_state(bus, id, w, "").await;
        bus.emit(Event::UserPrompt { session: SessionId(session), text, images: vec![] }).await;
        eprintln!("[worker] {id} admitted → session {session}");
    }
}

/// The worker called `worker_report` from within its turn — record the verdict
/// for the in-flight step (applied on the upcoming TurnComplete) and ack now.
/// `done` without a summary is refused so the model retries with one (the
/// charter: done REQUIRES a summary — it's the line the conductor reads).
async fn record_report(workers: &mut HashMap<u64, Worker>, bus: &BusHandle, workspace: &Path, call_session: SessionId, call_id: ActionId, args: serde_json::Value) {
    let status = args["status"].as_str().unwrap_or("continue").to_string();
    if status == "done" && args["summary"].as_str().map(str::trim).unwrap_or("").is_empty() {
        bus.emit(Event::ToolResult { session: call_session, call: call_id,
            output: ToolOutput { ok: false, content: serde_json::json!(
                "done requires a summary — one paragraph: what was delivered and where it lives") } }).await;
        return;
    }
    // Artifact declarations are workspace-confined (the evidence rule's
    // security line) — a bad path refuses the whole report so the model
    // corrects it; canonical forms replace the raw strings on accept.
    let mut verdict = parse_verdict(&args);
    if let Verdict::Done { artifacts, .. } = &mut verdict {
        match confine_artifacts(artifacts, workspace) {
            Ok(canonical) => *artifacts = canonical,
            Err(msg) => {
                bus.emit(Event::ToolResult { session: call_session, call: call_id,
                    output: ToolOutput { ok: false, content: serde_json::json!(msg) } }).await;
                return;
            }
        }
    }
    let recorded = workers.iter_mut()
        .find(|(_, w)| w.session == call_session.0 && matches!(w.state, WorkerState::Running | WorkerState::Blocked))
        .map(|(_, w)| { w.pending = Some(verdict); })
        .is_some();
    let content = if recorded {
        serde_json::json!({ "recorded": status, "note": "applied when this step completes" })
    } else {
        serde_json::json!("worker_report has no effect outside a running worker session")
    };
    bus.emit(Event::ToolResult { session: call_session, call: call_id,
        output: ToolOutput { ok: recorded, content } }).await;
}

/// A worker session's turn completed → apply the reported verdict: done (with
/// its summary), blocked (park awaiting input, slot-free), yield (Idle), or
/// continue (next step under the ceiling). No report = Done, the final text
/// is the deliverable (the W1a single-turn rule, now the fallback).
#[allow(clippy::too_many_arguments)]
async fn advance(
    workers: &mut HashMap<u64, Worker>, bus: &BusHandle, proxy: &ToolProxy, agents_dir: &Path,
    yolo_set: &GoalYoloSessions, models: &WorkerModels,
    trees: &mut HashMap<u64, HashMap<String, CellRecord>>,
    cell_by_worker: &HashMap<u64, (u64, Addr)>,
    worktrees_dir: &Path,
    session: u64, max_steps: u32,
) -> bool {
    let Some(id) = workers.iter()
        .find(|(_, w)| w.session == session && matches!(w.state, WorkerState::Running | WorkerState::Blocked))
        .map(|(id, _)| *id)
    else { return false };

    // M1c pre-read: the cell's measure context and the renewal source (the
    // effective parent's remaining steps — reparented cells spend their
    // adoptive ancestor's vector). Read-only over the tree here; writes
    // happen after the verdict lands.
    let cell_ctx = cell_by_worker.get(&id).and_then(|(mid, addr)| {
        trees.get(mid).and_then(|t| t.get(&addr.0)).map(|c| {
            let parent_eff = c.reparented_to.clone().or_else(|| addr.parent());
            let parent_steps = parent_eff.as_ref()
                .and_then(|pa| trees.get(mid).and_then(|t| t.get(&pa.0)))
                .map(|pc| pc.budget.steps);
            (*mid, addr.clone(), c.measure.is_some(), c.measure_history.clone(), parent_eff, parent_steps)
        })
    });
    let is_r_cell = cell_ctx.as_ref().map(|c| c.2).unwrap_or(false);
    let cell_history: Vec<u64> = cell_ctx.as_ref().map(|c| c.3.clone()).unwrap_or_default();
    let parent_steps: Option<u16> = cell_ctx.as_ref().and_then(|c| c.5);
    let mut lap_measure: Option<u64> = None;
    let mut kstall_broke = false;
    let mut renew: Option<u16> = None;

    // Mutate first (plain owned map — no lock-release dance needed), emit after.
    let (detail, next_directive) = {
        let w = workers.get_mut(&id).unwrap();
        w.turn_inflight = false;
        match w.pending.take() {
            Some(Verdict::Done { summary, artifacts, measure }) => {
                lap_measure = measure; // the final lap's reading joins the ledger
                w.state = WorkerState::Done;
                let detail: String = summary.chars().take(120).collect();
                w.summary = Some(summary);
                w.artifacts = artifacts;
                (detail, None)
            }
            Some(Verdict::Blocked(reason)) => {
                w.state = WorkerState::Blocked; // no turn in flight → slot-free, TTL clock
                w.started = Instant::now();
                (reason.chars().take(120).collect::<String>(), None)
            }
            Some(Verdict::Yield) => {
                w.state = WorkerState::Idle;
                w.started = Instant::now();
                ("yielded — awaiting input".to_string(), None)
            }
            Some(Verdict::Continue { steer, measure }) => {
                lap_measure = measure;
                let ceiling = effective_ceiling(w, max_steps);
                // The measure law is judged at the lap boundary, on the
                // projected ledger (history + this lap's reading).
                let mut projected = cell_history.clone();
                if is_r_cell {
                    if let Some(m) = measure { projected.push(m); }
                }
                if is_r_cell && mandala::k_stalled(&projected) {
                    // K-STALL: the ring breaks — blocked (slot-free,
                    // revivable, batch-deadline-bounded), history attached.
                    w.state = WorkerState::Blocked;
                    w.started = Instant::now();
                    kstall_broke = true;
                    let tail: Vec<u64> = projected.iter().rev().take(3).rev().copied().collect();
                    (format!("K-stall: {} — ring broken, escalated",
                             tail.iter().map(|m| m.to_string()).collect::<Vec<_>>().join("→")), None)
                } else if w.step >= ceiling {
                    // An R-cell still cutting its measure RENEWS — spending
                    // the PARENT's vector (grow where progress is; budget
                    // never from nowhere). Everything else: done at ceiling.
                    if let Some(g) = (is_r_cell)
                        .then(|| parent_steps.and_then(|ps| mandala::renewal_grant(ps, &projected)))
                        .flatten()
                    {
                        renew = Some(g);
                        w.step += 1;
                        w.step_ceiling = ceiling + u32::from(g);
                        w.started = Instant::now();
                        w.turn_inflight = true;
                        let d = directive_continue(id, w.batch, w.step, w.step_ceiling, &w.task, steer.as_deref());
                        (format!("renewed +{g} steps (measure decreasing)"), Some((w.session, d)))
                    } else {
                        w.state = WorkerState::Done; // budget reached — code disposes
                        ("step budget reached".to_string(), None)
                    }
                } else {
                    w.step += 1;
                    w.started = Instant::now();
                    w.turn_inflight = true;
                    let d = directive_continue(id, w.batch, w.step, ceiling, &w.task, steer.as_deref());
                    (String::new(), Some((w.session, d)))
                }
            }
            None => {
                // No report: Done with the final text as deliverable — UNLESS the
                // turn errored (Error+synthetic TurnComplete): that is a Failed
                // worker, never a hollow Done (a mistyped model lands here).
                if w.errored {
                    w.state = WorkerState::Failed;
                    ("turn error — no deliverable".to_string(), None)
                } else {
                    w.state = WorkerState::Done;
                    ("final text is the deliverable".to_string(), None)
                }
            }
        }
    };
    // M1c post-verdict tree bookkeeping: record the lap, spend renewals,
    // escalate breaks — the cell files tell the story before anyone reads it.
    if let Some((mid, addr, is_r, _, parent_eff, _)) = &cell_ctx {
        let tree_dir = worktrees_dir.join(mid.to_string());
        if *is_r {
            if let Some(m) = lap_measure {
                if let Some(cell) = trees.get_mut(mid).and_then(|t| t.get_mut(&addr.0)) {
                    cell.measure_history.push(m);
                    let overflow = cell.measure_history.len().saturating_sub(mandala::MEASURE_HISTORY_CAP);
                    if overflow > 0 { cell.measure_history.drain(..overflow); }
                    mandala::save_cell(&tree_dir, cell);
                }
            }
        }
        if let (Some(g), Some(pa)) = (renew, parent_eff.as_ref()) {
            if let Some(parent) = trees.get_mut(mid).and_then(|t| t.get_mut(&pa.0)) {
                parent.budget.steps = parent.budget.steps.saturating_sub(g);
                mandala::save_cell(&tree_dir, parent);
            }
            if let Some(cell) = trees.get_mut(mid).and_then(|t| t.get_mut(&addr.0)) {
                cell.budget.steps = cell.budget.steps.saturating_add(g);
                mandala::save_cell(&tree_dir, cell);
            }
            eprintln!("[mandala] {mid} cell {} renewed +{g} steps from {}", addr.0, pa.0);
        }
        if kstall_broke {
            let parent_session = workers[&id].parent;
            if apexos_core::is_worker_session(parent_session) {
                let tail: Vec<u64> = trees.get(mid)
                    .and_then(|t| t.get(&addr.0))
                    .map(|c| c.measure_history.iter().rev().take(3).rev().copied().collect())
                    .unwrap_or_default();
                bus.emit(Event::UserPrompt {
                    session: SessionId(parent_session),
                    text: kstall_note(addr, &tail),
                    images: vec![],
                }).await;
            }
        }
    }

    let w = &workers[&id];
    emit_state(bus, id, w, &detail).await;
    eprintln!("[worker] {id} → {:?} at step {}{}", w.state, w.step,
              if detail.is_empty() { String::new() } else { format!(" ({detail})") });
    // The evidence rule: every path into a terminal state leaves the trail —
    // and terminal residency ends: shared yolo/model arming goes with it.
    if matches!(w.state, WorkerState::Done | WorkerState::Failed) {
        disarm_worker(yolo_set, models, w.session);
        finalize_terminal(proxy, agents_dir, id, w).await;
    }
    // A fresh terminal gets one last review (Terminal census + reap — the
    // anti-zombie tick, M1b).
    if is_terminal(workers[&id].state) {
        workers.get_mut(&id).unwrap().next_review = Some(Instant::now());
    }
    if let Some((sid, directive)) = next_directive {
        bus.emit(Event::UserPrompt { session: SessionId(sid), text: directive, images: vec![] }).await;
    }
    true
}

/// A send landed on a worker session — the one revive/wake edge (PB-3):
/// Parked → Running (the router hydrated the history off this same event),
/// Idle → Running (wake free), verdict-Blocked → Running (unblocked by input).
/// A barrier-held gate also wakes: the send is the HUMAN OVERRIDE — the join
/// runs now, with whatever context the send carries (a revived-after-restart
/// gate gets its descendant paths from the conductor's send, not the driver).
/// Deliberately BYPASSES the admission cap — a send is human/conductor intent,
/// the emergency entrance; Queued workers just wait a little longer. Running /
/// approval-Blocked sends simply queue in the TurnGate (no state change);
/// plain-Queued and terminal workers are left untouched.
async fn wake_on_send(workers: &mut HashMap<u64, Worker>, bus: &BusHandle, models: &WorkerModels, session: u64) -> bool {
    let hit = workers.iter_mut()
        .find(|(_, w)| w.session == session
            && (matches!(w.state, WorkerState::Parked | WorkerState::Idle)
                || (w.state == WorkerState::Blocked && !w.turn_inflight)
                || (w.state == WorkerState::Queued && w.barrier_held)))
        .map(|(id, w)| {
            let from = w.state;
            let overridden = w.barrier_held;
            w.state = WorkerState::Running;
            w.started = Instant::now();
            w.turn_inflight = true;
            w.errored = false;
            w.barrier_held = false;
            // Fresh review lane for the fresh residency.
            w.next_review = Some(Instant::now() + review::golden_offset(*id, REVIEW_PERIOD));
            w.last_review_key = None;
            w.review_attempt = 0;
            (*id, from, overridden)
        });
    if let Some((id, from, overridden)) = hit {
        let w = &workers[&id];
        // Re-arm the model pin (it follows residency); NEVER re-arm inherited
        // yolo on revive — the parent's grant does not outlive the park.
        if let Some(m) = &w.model {
            if let Ok(mut mm) = models.lock() { mm.insert(w.session, m.clone()); }
        }
        let detail = match from {
            WorkerState::Parked => "revived by send",
            WorkerState::Idle   => "woken by send",
            _ if overridden     => "barrier overridden by send",
            _                   => "unblocked by send",
        };
        emit_state(bus, id, w, detail).await;
        eprintln!("[worker] {id} {detail}");
        true
    } else { false }
}

/// A worker's turn is suspended on an approval card → Blocked (stall-exempt,
/// slot held — the turn is alive and resumes if a human grants the tool).
async fn block_on_approval(workers: &mut HashMap<u64, Worker>, bus: &BusHandle, session: u64, tool: &str) -> bool {
    let hit = workers.iter_mut()
        .find(|(_, w)| w.session == session && w.state == WorkerState::Running)
        .map(|(id, w)| { w.state = WorkerState::Blocked; *id });
    if let Some(id) = hit {
        let w = &workers[&id];
        emit_state(bus, id, w, &format!("awaiting approval — {tool}")).await;
        eprintln!("[worker] {id} blocked on approval for '{tool}'");
        true
    } else { false }
}

/// The approval resolved (granted or declined — the turn continues either
/// way) → back to Running with a fresh stall window.
async fn resume_from_approval(workers: &mut HashMap<u64, Worker>, bus: &BusHandle, session: u64) -> bool {
    let hit = workers.iter_mut()
        .find(|(_, w)| w.session == session && w.state == WorkerState::Blocked)
        .map(|(id, w)| { w.state = WorkerState::Running; w.started = Instant::now(); *id });
    if let Some(id) = hit {
        let w = &workers[&id];
        emit_state(bus, id, w, "approval resolved").await;
        true
    } else { false }
}

/// Batch bookkeeping: an unreported batch reports when every worker is
/// terminal, or when its deadline passes — stragglers ride the report marked
/// `timed_out` (still revivable; a later revive finishes them outside it).
/// Rows carry evidence PATHS, never payloads. One report per batch.
/// W2: remote mirror rows join the terminal math and the rows; a batch HOSTED
/// for a remote conductor additionally returns a report-home entry the caller
/// pushes (the driver never blocks on the network here).
async fn check_batches(
    workers: &HashMap<u64, Worker>,
    remotes: &HashMap<u64, RemoteWorker>,
    batches: &mut HashMap<u64, BatchMeta>,
    bus: &BusHandle, agents_dir: &Path,
) -> (bool, Vec<(String, u64, u64, Vec<remote::WireRow>)>) {
    // (inline note: the held task_fanout ack is emitted AFTER TaskBatchDone —
    // bus order guarantees the goal driver's pending-batch set clears before
    // the blocked conductor turn can complete.)
    let now = epoch_now();
    let due: Vec<u64> = batches.iter()
        .filter(|(_, b)| !b.reported)
        .filter(|(id, b)| {
            let members: Vec<&Worker> = workers.values().filter(|w| w.batch == **id).collect();
            let rmembers: Vec<&RemoteWorker> = remotes.values().filter(|r| r.batch == **id).collect();
            let any = !members.is_empty() || !rmembers.is_empty();
            let all_terminal = any
                && members.iter().all(|w| is_terminal(w.state))
                && rmembers.iter().all(|r| r.is_terminal());
            let expired = now >= b.created_epoch.saturating_add(b.deadline_s);
            all_terminal || expired
        })
        .map(|(id, _)| *id)
        .collect();
    let changed = !due.is_empty();
    let mut report_home: Vec<(String, u64, u64, Vec<remote::WireRow>)> = Vec::new();
    for batch in due {
        let meta = batches.get_mut(&batch).unwrap();
        meta.reported = true;
        let parent = meta.parent;
        let inline_ack = meta.inline_ack.take();
        if let Some((node, origin_batch)) = meta.origin.clone() {
            report_home.push((node, origin_batch, batch, wire_rows_for_batch(workers, batch, agents_dir)));
        }
        let rows = batch_rows(workers, remotes, batch, agents_dir);
        // Cancelled is its own count — lumping it under "failed" reads as a
        // defect where there was a decision (first smoke's tally confusion).
        let (done, failed, cancelled, timed_out) = rows.iter().fold((0, 0, 0, 0), |(d, f, c, t), r| match () {
            _ if r.timed_out => (d, f, c, t + 1),
            _ if r.state == WorkerState::Done => (d + 1, f, c, t),
            _ if r.state == WorkerState::Cancelled => (d, f, c + 1, t),
            _ => (d, f + 1, c, t),
        });
        eprintln!("[worker] batch {batch} reported: {done} done, {failed} failed, {cancelled} cancelled, {timed_out} timed out");
        bus.emit(Event::TaskBatchDone { batch, parent: SessionId(parent), rows: rows.clone() }).await;
        // M1c — the sub-conductor return edge: a batch whose parent is a
        // WORKER gets its report DELIVERED (the goal driver owns the goal-
        // conductor edge and its pending set ignores worker parents). The
        // send wakes an idle/parked parent through the one revive edge.
        if apexos_core::is_worker_session(parent) {
            bus.emit(Event::UserPrompt {
                session: SessionId(parent),
                text: subconductor_report(batch, &rows),
                images: vec![],
            }).await;
        }
        // Inline: the report is the blocked task_fanout call's result — rows
        // plus each worker's summary (short batches want answers in hand; the
        // evidence paths still ride for the full trail).
        if let Some((ack_session, ack_action)) = inline_ack {
            let by_id: HashMap<u64, &Worker> = workers.iter()
                .filter(|(_, w)| w.batch == batch).map(|(id, w)| (*id, w)).collect();
            let inline_rows: Vec<serde_json::Value> = rows.iter().map(|r| serde_json::json!({
                "worker": r.worker.0,
                "state": format!("{:?}", r.state).to_lowercase(),
                "timed_out": r.timed_out,
                "summary": by_id.get(&r.worker.0).and_then(|w| w.summary.clone()),
                "evidence": r.evidence,
            })).collect();
            bus.emit(Event::ToolResult {
                session: SessionId(ack_session), call: ActionId(ack_action),
                output: ToolOutput { ok: true, content: serde_json::json!({
                    "batch": batch, "mode": "inline",
                    "done": done, "failed": failed, "cancelled": cancelled, "timed_out": timed_out,
                    "workers": inline_rows,
                }) },
            }).await;
        }
    }
    (changed, report_home)
}

fn is_terminal(state: WorkerState) -> bool {
    matches!(state, WorkerState::Done | WorkerState::Failed | WorkerState::Cancelled)
}

/// Build a batch's report rows — pure over the worker + remote maps
/// (unit-tested). Remote rows carry their hosting `node` and the local
/// MIRROR evidence path (or "" until mirrored); an unknown peer state maps
/// to the bounded typed fallback while `timed_out` carries the truth.
fn batch_rows(workers: &HashMap<u64, Worker>, remotes: &HashMap<u64, RemoteWorker>, batch: u64, agents_dir: &Path) -> Vec<BatchWorkerRow> {
    let mut rows: Vec<(u64, BatchWorkerRow)> = workers.iter()
        .filter(|(_, w)| w.batch == batch)
        .map(|(id, w)| {
            let terminal = is_terminal(w.state);
            (*id, BatchWorkerRow {
                worker: WorkerId(*id),
                state: w.state,
                evidence: if terminal {
                    evidence_path(agents_dir, *id).to_string_lossy().into_owned()
                } else { String::new() },
                timed_out: !terminal,
                node: None,
            })
        })
        .collect();
    rows.extend(remotes.iter()
        .filter(|(_, r)| r.batch == batch)
        .map(|(id, r)| {
            (*id, BatchWorkerRow {
                worker: WorkerId(*id),
                state: remote::row_state(&r.state_raw),
                evidence: r.evidence.clone().unwrap_or_default(),
                timed_out: !r.is_terminal(),
                node: Some(r.node.clone()),
            })
        }));
    rows.sort_by_key(|(id, _)| *id);
    rows.into_iter().map(|(_, r)| r).collect()
}

/// Cancel a set of workers — terminal, not revivable, full trail. An in-flight
/// turn is aborted (UserCancel emits no TurnComplete, so advance() never fires
/// for a dead worker — the goal_cancel precedent), the slot frees, shared
/// arming clears, and evidence + episode land like any terminal path so batch
/// reports stay honest.
#[allow(clippy::too_many_arguments)]
async fn cancel_workers(workers: &mut HashMap<u64, Worker>, bus: &BusHandle, proxy: &ToolProxy, agents_dir: &Path, yolo_set: &GoalYoloSessions, models: &WorkerModels, ids: &[u64]) {
    for id in ids {
        let Some(w) = workers.get_mut(id) else { continue };
        if is_terminal(w.state) { continue; }
        let had_turn = w.turn_inflight;
        w.state = WorkerState::Cancelled;
        w.turn_inflight = false;
        w.pending = None;
        w.barrier_held = false;
        w.next_review = Some(Instant::now()); // one last review: census + reap
        let session = w.session;
        if had_turn {
            bus.emit(Event::UserCancel { session: SessionId(session) }).await;
        }
        disarm_worker(yolo_set, models, session);
        let w = &workers[id];
        emit_state(bus, *id, w, "cancelled").await;
        eprintln!("[worker] {id} cancelled");
        finalize_terminal(proxy, agents_dir, *id, w).await;
    }
}

/// worker_cancel{worker?|batch?}: the fan's kill switch. Exactly one selector.
/// W2: remote rows RELAY to their hosting node and sit `cancel requested`
/// (non-terminal) until poll/report confirms — a peer that never answers is
/// bounded by the batch deadline, so the kill switch can't wedge either.
#[allow(clippy::too_many_arguments)]
async fn cancel_request(workers: &mut HashMap<u64, Worker>, remotes: &mut HashMap<u64, RemoteWorker>, bus: &BusHandle, proxy: &ToolProxy, agents_dir: &Path, yolo_set: &GoalYoloSessions, models: &WorkerModels, mesh: &MeshDeps, call_session: SessionId, call_id: ActionId, args: serde_json::Value) -> bool {
    let (ids, remote_ids): (Vec<u64>, Vec<u64>) = match (args["worker"].as_u64(), args["batch"].as_u64()) {
        (Some(w), None) => (
            workers.get(&w).filter(|wk| !is_terminal(wk.state)).map(|_| vec![w]).unwrap_or_default(),
            remotes.get(&w).filter(|r| !r.is_terminal()).map(|_| vec![w]).unwrap_or_default(),
        ),
        (None, Some(b)) => (
            workers.iter().filter(|(_, wk)| wk.batch == b && !is_terminal(wk.state)).map(|(id, _)| *id).collect(),
            remotes.iter().filter(|(_, r)| r.batch == b && !r.is_terminal()).map(|(id, _)| *id).collect(),
        ),
        _ => {
            bus.emit(Event::ToolResult { session: call_session, call: call_id,
                output: ToolOutput { ok: false, content: serde_json::json!(
                    "pass exactly one of worker (an id) or batch (cancels every non-terminal worker in it)") } }).await;
            return false;
        }
    };
    if ids.is_empty() && remote_ids.is_empty() {
        bus.emit(Event::ToolResult { session: call_session, call: call_id,
            output: ToolOutput { ok: false, content: serde_json::json!("no matching non-terminal worker(s)") } }).await;
        return false;
    }
    cancel_workers(workers, bus, proxy, agents_dir, yolo_set, models, &ids).await;
    relay_remote_cancels(remotes, bus, mesh, &remote_ids).await;
    let mut content = serde_json::json!({ "cancelled": ids, "count": ids.len() + remote_ids.len() });
    if !remote_ids.is_empty() {
        content["cancel_relayed"] = serde_json::json!(remote_ids);
        content["note"] = serde_json::json!("remote rows confirm terminal via their hosting node (poll/report); the batch deadline bounds a silent peer");
    }
    bus.emit(Event::ToolResult { session: call_session, call: call_id,
        output: ToolOutput { ok: true, content } }).await;
    true
}

/// PB-1 soft breaker: a session firing sequential local spawns gets ONE nudge
/// per window — parallel work goes through a single task_fanout batch. Soft by
/// design: an Error line in the spawner's own stream, never a refusal.
async fn pb1_track(spawn_log: &mut HashMap<u64, Vec<Instant>>, bus: &BusHandle, parent: u64) {
    if apexos_core::is_worker_session(parent) || apexos_core::is_spawn_session(parent) {
        return; // depth guards own those layers
    }
    let now = Instant::now();
    let log = spawn_log.entry(parent).or_default();
    log.retain(|t| now.duration_since(*t) < PB1_WINDOW);
    log.push(now);
    if log.len() == PB1_SPAWN_THRESHOLD {
        eprintln!("[worker] PB-1: session {parent} fired {PB1_SPAWN_THRESHOLD} agent_spawns in {}s — nudging toward task_fanout", PB1_WINDOW.as_secs());
        bus.emit(Event::Error {
            session: Some(SessionId(parent)),
            message: format!(
                "PB-1: {PB1_SPAWN_THRESHOLD} sequential agent_spawns in this session — parallel work goes through ONE task_fanout batch (persistent, parkable, evidence-leaving workers), never a spawn-then-wait chain."),
        }).await;
        log.clear(); // one nudge per burst
    }
}

/// Conductor visibility: a snapshot of all workers plus the admission picture.
/// Terminal workers carry their evidence path — paths, not payloads, even here.
/// W2: remote mirror rows ride along with their node + raw peer state (the
/// mirror is truth-as-last-observed; polls keep it honest).
async fn handle_list_workers(workers: &HashMap<u64, Worker>, remotes: &HashMap<u64, RemoteWorker>, bus: &BusHandle, cap: usize, agents_dir: &Path, call_session: SessionId, call_id: ActionId) {
    let mut rows: Vec<(u64, serde_json::Value)> = workers.iter().map(|(id, w)| {
        let mut row = serde_json::json!({
            "worker": id, "batch": w.batch, "parent": w.parent, "session": w.session,
            "state": format!("{:?}", w.state).to_lowercase(),
            "step": w.step,
            "task": w.task.chars().take(100).collect::<String>(),
            "summary": w.summary.as_deref().map(|s| s.chars().take(200).collect::<String>()),
            "evidence": if is_terminal(w.state) {
                serde_json::json!(evidence_path(agents_dir, *id).to_string_lossy())
            } else { serde_json::Value::Null },
        });
        if w.barrier_held { row["barrier_held"] = serde_json::json!(true); }
        (*id, row)
    }).collect();
    rows.extend(remotes.iter().map(|(id, r)| {
        (*id, serde_json::json!({
            "worker": id, "batch": r.batch, "parent": r.parent,
            "node": r.node,
            "remote_worker": r.remote_worker, "remote_session": r.remote_session,
            "state": r.state_raw,
            "task": r.task.chars().take(100).collect::<String>(),
            "summary": r.summary.as_deref().map(|s| s.chars().take(200).collect::<String>()),
            "evidence": r.evidence,
        }))
    }));
    rows.sort_by_key(|(id, _)| *id);
    let list: Vec<serde_json::Value> = rows.into_iter().map(|(_, j)| j).collect();
    bus.emit(Event::ToolResult { session: call_session, call: call_id,
        output: ToolOutput { ok: true, content: serde_json::json!({
            "workers": list, "count": list.len(),
            "cap": cap, "slots_used": slots_used(workers),
        }) } }).await;
}

// ── Mandala runtime — the driver's side of the tree ─────────────────────────

/// One planned cell for this fan: address, contracted budget, J guard, and
/// the fully composed worker prompt (invariant verbatim + CELL header +
/// rituals). Computed refused-early, minted mechanically.
pub(crate) struct CellPlan {
    addr: Addr,
    budget: BudgetVec,
    /// Some = the J bit is armed; the clamped timeout. Pure-J cells (GATE/
    /// DIAMOND) barrier-hold at mint; an R+J cell (FORGE) starts lapping.
    barrier_timeout_s: Option<u64>,
    /// Some = the R bit is armed (M1c); the cell's measure command.
    measure: Option<String>,
    /// The sub-conduction grant (M1c).
    voucher: bool,
    /// M2: the mesh peer hosting this cell's execution body (None = local).
    /// Vetted by `remote_cell_veto` — only plain ring cells of repo-less
    /// mandalas carry one; the gate never does.
    node: Option<String>,
    prompt: String,
    model: Option<String>,
}

/// A validated mandala fan (M1b). Plans are in MINT ORDER — the gate (when
/// present) first, so it takes the lowest worker id and fronts the FIFO the
/// moment its barrier opens.
pub(crate) struct CellsCtx {
    mandala: u64,
    parent: Addr,
    plans: Vec<CellPlan>,
    invariant_hash: String,
    /// A >1 fan landed under the parent → it gains the B bit (SPINE→FAN,
    /// GATE→DIAMOND) — forms mutate one bit at a time as the run grows.
    arm_parent_branch: bool,
    /// A >1 ring landed under this call's own gate → the gate is a DIAMOND.
    arm_gate_branch: bool,
    /// The cells' shared deadline — caps the batch deadline, as at M1a.
    deadline_cap_s: u64,
}

/// The worktree ritual injected into B-cell children of a code mandala —
/// driver-injected verbatim (the invariant's pattern): the collision-safety
/// rule cannot be paraphrased away at any level.
fn worktree_ritual(repo: &str, branch: &str) -> String {
    format!(
        "\n\nCODE CELL — repo: {repo}\nYour branch: {branch}\nFIRST call \
         git_worktree{{action:\"add\", path:\"{repo}\", branch:\"{branch}\"}} and work ONLY \
         inside the worktree directory it returns. Commit your work on your branch there \
         before reporting done — uncommitted work is invisible to the join."
    )
}

/// The gate's mint-time note: what the hold means, what arrives at open.
fn gate_note(timeout_s: u64) -> String {
    format!(
        "\n\nYou are this subtree's JOIN: a barrier holds you until your descendant cells \
         settle (guard timeout {timeout_s}s). When your turn begins, their addresses, \
         states and evidence paths will be appended to this work order — read the evidence \
         files, integrate, and only then report done."
    )
}

/// The measure ritual (M1c) — driver-injected verbatim into R-cells: the
/// command is the instrument, the worker runs it through its own policied
/// tools, the driver judges the trend. Two non-decreasing laps break the
/// ring (K-stall); a loop at zero self-terminates as a stall, so report
/// done at zero.
fn measure_ritual(cmd: &str) -> String {
    format!(
        "\n\nMEASURE (your lap gate): at the END of every lap run `{cmd}` through your \
         normal tools and report the resulting non-negative integer with \
         worker_report{{status:\"continue\", measure: N}} — or status:\"done\" with the \
         final measure on your last lap. The number must STRICTLY DECREASE every lap; two \
         non-decreasing laps break your ring and escalate with the history attached. When \
         it reaches 0, report done — looping at 0 counts as a stall."
    )
}

/// The voucher block (M1c) — the sub-conduction grant, injected at mint.
/// The cell's own budget vector is the slice; every law still applies.
fn voucher_block(mandala: u64, addr: &Addr) -> String {
    format!(
        "\n\nVOUCHER — you may SUB-CONDUCT: grow your OWN subtree with \
         task_fanout{{mandala: {mandala}, parent_cell: \"{}\" (or one of your descendants), \
         tasks:[…], join?, measure?, voucher?}}. Your cell's budget vector is your slice — \
         children contract from it and renewals spend YOUR steps, so fan late and fan \
         narrow. When a batch of your children settles, its report is delivered into your \
         session: read the evidence files, integrate, then continue your own task.",
        addr.0
    )
}

/// The K-stall escalation note delivered to a sub-conductor parent (goal
/// conductors learn at the batch report — that edge belongs to the goal
/// driver; interactive conductors read the board/status).
fn kstall_note(addr: &Addr, tail: &[u64]) -> String {
    format!(
        "SUPERVISION — your child cell {} K-stalled: its measure stopped decreasing \
         ({}). Its ring is broken and it sits blocked with the history attached. Steer it \
         with a send, cancel it, or integrate around it — one line at a time.",
        addr.0,
        tail.iter().map(|m| m.to_string()).collect::<Vec<_>>().join("→"),
    )
}

/// The batch report delivered INTO a sub-conductor's session (M1c). The
/// goal driver owns the goal-conductor edge; this is the worker-parent
/// twin — same paths-not-payloads law.
fn subconductor_report(batch: u64, rows: &[BatchWorkerRow]) -> String {
    let mut lines = String::new();
    for r in rows {
        lines.push_str(&format!(
            "- worker {} [{:?}]{}: {}\n",
            r.worker.0,
            r.state,
            if r.timed_out { " (timed_out — still revivable)" } else { "" },
            if r.evidence.is_empty() { "no evidence file" } else { &r.evidence },
        ));
    }
    format!(
        "BATCH {batch} REPORT — your child workers have settled:\n{lines}\
         Read each evidence file (and the artifacts it declares), integrate the results \
         into YOUR OWN deliverable, then continue your task — report through worker_report \
         as usual."
    )
}

/// The merge ritual appended at barrier open for code mandalas — the
/// J-barrier's declared work, concrete: merge, verify, commit. The artifacts
/// line is mechanical on purpose: the first field gate (2026-07-31) legally
/// skipped declaring them because the ritual didn't demand it.
fn merge_ritual(repo: &str, branches: &[String]) -> String {
    let list = if branches.is_empty() { "(none delivered)".to_string() } else { branches.join(", ") };
    format!(
        "\n\nMERGE RITUAL — repo: {repo}\nDelivered cell branches: {list}\nMerge each \
         delivered branch (git_merge), resolve conflicts, run the VERIFY command from the \
         invariant through your normal tools, and commit the merged result before \
         reporting done. Report done with the merged files declared in `artifacts` — the \
         evidence rule reaches the join too."
    )
}

/// Validate a mandala-scoped task_fanout (M1b: rings, gates, diamonds) and
/// compose its cell plans. The layouts:
///   - plain ring: `tasks:[N]` under the parent (width-1 = the M1a chain);
///   - bare gate: `tasks:[one join task]` + `barrier_timeout_s` — for
///     interactive conductors who fan under it in later calls;
///   - the one-call diamond: `tasks:[ring]` + `join:"…"` — gate minted at
///     the parent, ring UNDER the gate, one batch (a goal conductor holds
///     AwaitingBatch on any pending batch, so gate-then-fan in two calls
///     would wedge it until the gate batch's deadline).
///
/// Ok(None) = a plain (non-mandala) fan; Err(()) = refused, ToolResult sent.
#[allow(clippy::too_many_arguments)]
async fn prepare_mandala_cells(
    mandalas: &HashMap<u64, MandalaRecord>,
    trees: &HashMap<u64, HashMap<String, CellRecord>>,
    bus: &BusHandle,
    call_session: SessionId, call_id: ActionId,
    args: &serde_json::Value,
    tasks: &[TaskSpecItem],
    inline: bool,
    caller_cell: Option<&Addr>,
) -> Result<Option<CellsCtx>, ()> {
    let refuse = |msg: String| Event::ToolResult {
        session: call_session, call: call_id,
        output: ToolOutput { ok: false, content: serde_json::json!(msg) },
    };
    let join_task = args["join"].as_str().map(str::trim).filter(|s| !s.is_empty()).map(str::to_owned);
    let barrier_arg = args["barrier_timeout_s"].as_u64();
    let Some(mid) = args["mandala"].as_u64() else {
        if join_task.is_some() || barrier_arg.is_some() {
            bus.emit(refuse("join/barrier_timeout_s need a mandala — a plain batch already joins at TaskBatchDone".into())).await;
            return Err(());
        }
        if tasks.iter().any(|t| t.measure.is_some() || t.voucher) {
            bus.emit(refuse("measure/voucher are mandala-cell properties — open a mandala to use them".into())).await;
            return Err(());
        }
        return Ok(None);
    };
    let Some(m) = mandalas.get(&mid) else {
        bus.emit(refuse(format!("no mandala {mid} — mandala_create first, or mandala_status to list"))).await;
        return Err(());
    };
    if caller_cell.is_none() && m.conductor != call_session.0 {
        bus.emit(refuse(format!("mandala {mid} belongs to session {} — only its conductor (or a vouchered cell inside it) grows it", m.conductor))).await;
        return Err(());
    }
    if m.state == "closed" {
        bus.emit(refuse(format!("mandala {mid} is closed — open a new one to grow"))).await;
        return Err(());
    }
    if inline && (join_task.is_some() || barrier_arg.is_some()) {
        bus.emit(refuse("a join/barrier batch is async by nature — a gate can outlive any short inline window".into())).await;
        return Err(());
    }
    if barrier_arg.is_some() && join_task.is_none() && tasks.len() != 1 {
        bus.emit(refuse(format!(
            "barrier_timeout_s with {} tasks is ambiguous — a bare gate is exactly one task (the join), or pass join:\"…\" and let tasks be the ring", tasks.len()))).await;
        return Err(());
    }
    let empty = HashMap::new();
    let tree = trees.get(&mid).unwrap_or(&empty);
    let parent_addr = match args["parent_cell"].as_str() {
        // A sub-conductor's default parent is ITSELF, not the root.
        None => caller_cell.cloned().unwrap_or(Addr(Addr::ROOT.into())),
        Some(s) => match Addr::parse(s) {
            Some(a) => a,
            None => { bus.emit(refuse(format!("'{s}' is not a cell address (like 0 or 0.2.1)"))).await; return Err(()); }
        },
    };
    if let Some(own) = caller_cell {
        // Descendant-only conducting: a voucher covers the cell's own subtree.
        if !mandala::voucher_scope_ok(own, &parent_addr) {
            bus.emit(refuse(format!(
                "your voucher covers your own subtree ({}) — cell {} is outside it", own.0, parent_addr.0))).await;
            return Err(());
        }
    }
    let Some(parent) = tree.get(&parent_addr.0) else {
        bus.emit(refuse(format!("mandala {mid} has no cell {} — mandala_status shows the tree", parent_addr.0))).await;
        return Err(());
    };

    // ── The composition table (M1d): static legality, refused at admission ──
    // Two hooks. (1) MINT: each planned child's form must compose under the
    // parent's post-fan form — R-over-R is forbidden (a vouchered SPIRAL/FORGE
    // sub-conductor cannot mint measured children; nested recurrence has no
    // joint termination argument). (2) CHANGING-LINE: a wide fan arms the
    // parent's B, and the parent's NEW form must compose under the
    // GRANDPARENT's — B-over-B is conditional on the breadth product down the
    // path fitting the mandala's cell budget (stacked fans must not promise
    // more frontier than the geometry holds).
    {
        let wide = tasks.len() > 1 || (join_task.is_some() && tasks.len() > 1);
        let parent_post = if wide && join_task.is_none() { parent.form.arm_branch() } else { parent.form };
        // (1) each child's mint form vs the parent's post-fan form.
        let gate_planned = join_task.is_some() || barrier_arg.is_some();
        for (i, t) in tasks.iter().enumerate() {
            let mut child_form = CellForm::SPINE;
            // The bare-gate form consumes the single task as a join; a ring
            // under a one-call diamond hangs off the GATE, not the parent.
            let is_the_gate = gate_planned && join_task.is_none() && i == 0;
            if is_the_gate { child_form = child_form.arm_join(); }
            if t.measure.is_some() { child_form = child_form.arm_recur(); }
            let vs = if gate_planned && !is_the_gate { CellForm::GATE } else { parent_post };
            // Free composes; Conditional is a B-over-B property, checked below.
            if mandala::compose(vs, child_form) == mandala::Compose::Forbidden {
                bus.emit(refuse(format!(
                    "composition refused: {} over {} is R-over-R — nested recurrence is the classic livelock; drop the child's measure or conduct from an unmeasured cell",
                    child_form.name(), vs.name()))).await;
                return Err(());
            }
        }
        // The join task itself (one-call diamond): GATE (+R if measured via
        // bare-gate knobs) under the parent — same law.
        if let Some(_j) = &join_task {
            let gate_form = CellForm::GATE;
            if mandala::compose(parent_post, gate_form) == mandala::Compose::Forbidden {
                bus.emit(refuse("composition refused: a join cannot mint under this parent (R-over-R)".into())).await;
                return Err(());
            }
        }
        // (2) changing-line: the parent arming B, validated vs the grandparent.
        if wide {
            if let Some(gp_addr) = parent_addr.parent() {
                if let Some(gp) = tree.get(&gp_addr.0) {
                    let parent_new = parent.form.arm_branch();
                    if mandala::compose(gp.form, parent_new) == mandala::Compose::Conditional {
                        let width = tasks.len() as u8;
                        if !mandala::breadth_product_ok(tree, &parent_addr, width, m.budget.cells) {
                            bus.emit(refuse(format!(
                                "B-over-B breadth product refused: a {}-wide fan under {} (itself under a branching {}) would promise more frontier than the {}-cell budget — narrow the fan or integrate first",
                                width, parent_addr.0, gp.form.name(), m.budget.cells))).await;
                            return Err(());
                        }
                    }
                }
            }
        }
    }

    // The layout: bare gate consumes the single task as the join; the
    // one-call diamond takes the join from `join` and the ring from tasks.
    let bare_gate = join_task.is_none() && barrier_arg.is_some();
    let gate_task: Option<String> =
        join_task.or_else(|| if bare_gate { Some(tasks[0].prompt.clone()) } else { None });
    let ring_tasks: &[TaskSpecItem] = if bare_gate { &[] } else { tasks };

    // M2 — remote-body vetting, before any layout math (nothing minted on
    // refusal): only plain ring cells of repo-less mandalas ship out. The
    // diamond's join comes from `join` (never a task item), so the gate can
    // only meet a node through the bare-gate form — checked first.
    {
        let repo_mandala = m.repo.is_some();
        if bare_gate && tasks[0].node.is_some() {
            if let Some(msg) = remote_cell_veto(true, tasks[0].measure.is_some(), tasks[0].voucher, repo_mandala) {
                bus.emit(refuse(msg.into())).await;
                return Err(());
            }
        }
        for t in ring_tasks.iter().filter(|t| t.node.is_some()) {
            if let Some(msg) = remote_cell_veto(false, t.measure.is_some(), t.voucher, repo_mandala) {
                bus.emit(refuse(msg.into())).await;
                return Err(());
            }
        }
    }

    let needed = ring_tasks.len() + usize::from(gate_task.is_some());

    // Geometry: open cells vs the mandala's conserved budget — the whole fan
    // must fit, not just the first cell.
    let open = mandala::open_cells(tree);
    if open + needed > m.budget.cells as usize {
        bus.emit(refuse(format!(
            "geometry budget exhausted: {open} open cells + {needed} new > {} — integrate or cancel before growing", m.budget.cells))).await;
        return Err(());
    }

    let repo = m.repo.as_deref();
    let mk_prompt = |addr: &Addr, task: &str, extra: &str| format!(
        "{}\n\nCELL {} — your task within the mandala:\n{}{}",
        m.invariant.directive_block(), addr.0, task, extra
    );

    let mut plans: Vec<CellPlan> = Vec::with_capacity(needed);
    let mut arm_parent_branch = false;
    let mut arm_gate_branch = false;
    let deadline_cap_s;

    if let Some(gtask) = gate_task {
        // The gate takes one slot in the parent's ring.
        let ring = parent_addr.depth();
        let width = mandala::ring_width(m.lattice, ring);
        let ordinal = mandala::next_child_ordinal(tree, &parent_addr);
        if width == 0 || ordinal >= width as u32 {
            bus.emit(refuse(format!(
                "ring {ring} of the {:?} lattice is full (width {width}) — go deeper or pick another parent", m.lattice))).await;
            return Err(());
        }
        let gate_budget = mandala::contract_child(&parent.budget);
        if !mandala::admissible(&parent.budget, &gate_budget, width - ordinal as u8) {
            bus.emit(refuse(format!(
                "budget descent refused at cell {}: parent depth {} steps {} — the vector must strictly descend and stay positive",
                parent_addr.0, parent.budget.depth, parent.budget.steps))).await;
            return Err(());
        }
        let gate_addr = parent_addr.child(ordinal);
        let timeout = barrier_arg.unwrap_or(DEFAULT_BARRIER_TIMEOUT_S).clamp(60, gate_budget.deadline_s.max(60));
        deadline_cap_s = gate_budget.deadline_s;

        // The ring under the gate (the one-call diamond): fresh gate, fresh
        // ordinals 0..n — the whole ring must fit its preset width, and the
        // join consumes a depth level of its own.
        if !ring_tasks.is_empty() {
            let ring2 = gate_addr.depth();
            let width2 = mandala::ring_width(m.lattice, ring2);
            if ring_tasks.len() as u32 > width2 as u32 {
                bus.emit(refuse(format!(
                    "{} ring tasks exceed ring {ring2}'s width {width2} in the {:?} lattice — narrow the fan or pick a wider lattice", ring_tasks.len(), m.lattice))).await;
                return Err(());
            }
            let ring_budget = mandala::contract_child(&gate_budget);
            if !mandala::admissible(&gate_budget, &ring_budget, width2) {
                bus.emit(refuse(format!(
                    "budget descent refused under the gate: a join consumes a depth level, so cell {} needs depth ≥ 3 (it has {})",
                    parent_addr.0, parent.budget.depth))).await;
                return Err(());
            }
            let wide = ring_tasks.len() > 1;
            for (i, t) in ring_tasks.iter().enumerate() {
                let addr = gate_addr.child(i as u32);
                let mut extra = match repo {
                    Some(r) if wide => worktree_ritual(r, &addr.branch()),
                    _ => String::new(),
                };
                if let Some(m) = &t.measure { extra.push_str(&measure_ritual(m)); }
                if t.voucher { extra.push_str(&voucher_block(mid, &addr)); }
                plans.push(CellPlan {
                    prompt: mk_prompt(&addr, &t.prompt, &extra),
                    addr, budget: ring_budget,
                    barrier_timeout_s: None,
                    measure: t.measure.clone(), voucher: t.voucher,
                    node: t.node.clone(),
                    model: t.model.clone(),
                });
            }
            arm_gate_branch = wide;
        }
        // Gate FIRST in mint order — lowest wid fronts the FIFO at open.
        // A bare gate carries its task item's knobs: measure makes it a
        // FORGE (lap→verify→lap — it starts lapping, no mint hold), voucher
        // lets it sub-conduct its own ring per lap.
        let (gate_model, gate_measure, gate_voucher) = if bare_gate {
            (tasks[0].model.clone(), tasks[0].measure.clone(), tasks[0].voucher)
        } else {
            (args["model"].as_str().map(str::trim).filter(|s| !s.is_empty() && s.len() <= 64).map(str::to_owned), None, false)
        };
        let mut gate_extra = gate_note(timeout);
        if let Some(m) = &gate_measure { gate_extra.push_str(&measure_ritual(m)); }
        if gate_voucher { gate_extra.push_str(&voucher_block(mid, &gate_addr)); }
        plans.insert(0, CellPlan {
            prompt: mk_prompt(&gate_addr, &gtask, &gate_extra),
            addr: gate_addr, budget: gate_budget,
            barrier_timeout_s: Some(timeout),
            measure: gate_measure, voucher: gate_voucher,
            node: None, // the bindu on the spine — vetted above
            model: gate_model,
        });
    } else {
        // A plain ring under the parent (width 1 = the M1a chain, unchanged).
        let ring = parent_addr.depth();
        let width = mandala::ring_width(m.lattice, ring);
        let ordinal = mandala::next_child_ordinal(tree, &parent_addr);
        let n = ring_tasks.len() as u32;
        if width == 0 || ordinal + n > width as u32 {
            bus.emit(refuse(format!(
                "ring {ring} of the {:?} lattice fits {} more cell(s) ({n} asked) — go deeper, narrow the fan, or pick another parent",
                m.lattice, (width as u32).saturating_sub(ordinal)))).await;
            return Err(());
        }
        let child_budget = mandala::contract_child(&parent.budget);
        if !mandala::admissible(&parent.budget, &child_budget, width - ordinal as u8) {
            bus.emit(refuse(format!(
                "budget descent refused at cell {}: parent depth {} steps {} — the vector must strictly descend and stay positive",
                parent_addr.0, parent.budget.depth, parent.budget.steps))).await;
            return Err(());
        }
        deadline_cap_s = child_budget.deadline_s;
        let wide = ring_tasks.len() > 1;
        for (i, t) in ring_tasks.iter().enumerate() {
            let addr = parent_addr.child(ordinal + i as u32);
            let mut extra = match repo {
                Some(r) if wide => worktree_ritual(r, &addr.branch()),
                _ => String::new(),
            };
            if let Some(m) = &t.measure { extra.push_str(&measure_ritual(m)); }
            if t.voucher { extra.push_str(&voucher_block(mid, &addr)); }
            plans.push(CellPlan {
                prompt: mk_prompt(&addr, &t.prompt, &extra),
                addr, budget: child_budget,
                barrier_timeout_s: None,
                measure: t.measure.clone(), voucher: t.voucher,
                node: t.node.clone(),
                model: t.model.clone(),
            });
        }
        arm_parent_branch = wide;
    }

    Ok(Some(CellsCtx {
        mandala: mid,
        parent: parent_addr,
        plans,
        invariant_hash: m.invariant.hash.clone(),
        arm_parent_branch,
        arm_gate_branch,
        deadline_cap_s,
    }))
}

/// Bind minted workers to their cells: records land in the tree (position IS
/// identity), the runtime index follows, and forms arm one bit at a time —
/// the gate is born GATE (+B when its own ring is wide → DIAMOND), a wide
/// fan arms the PARENT's B. `wids` is in PLAN ORDER — local workers and M2
/// remote mirror rows spend the same counter, so a cell binds by position
/// whatever bodies it; the `node` stamp says where that body lives.
fn bind_cells(
    trees: &mut HashMap<u64, HashMap<String, CellRecord>>,
    cell_by_worker: &mut HashMap<u64, (u64, Addr)>,
    worktrees_dir: &Path,
    ctx: &CellsCtx, wids: &[u64],
) {
    let tree_dir = worktrees_dir.join(ctx.mandala.to_string());
    for (plan, wid) in ctx.plans.iter().zip(wids) {
        let mut form = CellForm::SPINE;
        if plan.barrier_timeout_s.is_some() {
            form = form.arm_join();
            if ctx.arm_gate_branch { form = form.arm_branch(); }
        }
        if plan.measure.is_some() {
            form = form.arm_recur(); // SPIRAL, or FORGE when J is also armed
        }
        let cell = CellRecord {
            addr: plan.addr.clone(),
            form,
            task: String::new(), // the worker record holds the full task; the tree holds structure
            budget: plan.budget,
            invariant_hash: ctx.invariant_hash.clone(),
            worker: Some(*wid),
            state: "open".into(),
            evidence: None,
            reparented_to: None,
            created_epoch: epoch_now(),
            barrier_timeout_s: plan.barrier_timeout_s,
            barrier_opened: false,
            measure: plan.measure.clone(),
            measure_history: Vec::new(),
            voucher: plan.voucher,
            node: plan.node.clone(),
        };
        mandala::save_cell(&tree_dir, &cell);
        cell_by_worker.insert(*wid, (ctx.mandala, plan.addr.clone()));
        trees.entry(ctx.mandala).or_default().insert(cell.addr.0.clone(), cell);
        eprintln!("[mandala] {} grew cell {} ({}, worker {wid}{})",
                  ctx.mandala, plan.addr.0, if plan.barrier_timeout_s.is_some() { "gate" } else { "cell" },
                  plan.node.as_deref().map(|n| format!(" @ {n}")).unwrap_or_default());
    }
    if ctx.arm_parent_branch {
        if let Some(parent) = trees.get_mut(&ctx.mandala).and_then(|t| t.get_mut(&ctx.parent.0)) {
            let armed = parent.form.arm_branch();
            if armed != parent.form {
                parent.form = armed;
                mandala::save_cell(&tree_dir, parent);
                eprintln!("[mandala] {} cell {} armed B → {}", ctx.mandala, ctx.parent.0, parent.form.name());
            }
        }
    }
}

/// Mirror worker terminal states into their cell files — the tree stays
/// readable without joining workers.json, and evidence paths ride along.
fn sync_cells(
    trees: &mut HashMap<u64, HashMap<String, CellRecord>>,
    cell_by_worker: &HashMap<u64, (u64, Addr)>,
    workers: &HashMap<u64, Worker>,
    worktrees_dir: &Path, agents_dir: &Path,
) {
    for (wid, (mid, addr)) in cell_by_worker {
        let Some(w) = workers.get(wid) else { continue };
        if !is_terminal(w.state) { continue; }
        let Some(cell) = trees.get_mut(mid).and_then(|t| t.get_mut(&addr.0)) else { continue };
        let state = format!("{:?}", w.state).to_lowercase();
        if cell.state == state { continue; }
        cell.state = state;
        cell.evidence = Some(evidence_path(agents_dir, *wid).to_string_lossy().into_owned());
        mandala::save_cell(&worktrees_dir.join(mid.to_string()), cell);
    }
}

/// sync_cells' remote twin (M2): mirror TERMINAL remote-row states into
/// their cell files — the wire string plus the local MIRROR evidence path
/// (reading the mirror IS integration, the gate's law too). Open rows leave
/// the tree untouched: the tree mirrors outcomes, not heartbeats — live
/// remote states are read from the remotes map where needed. Unknown wire
/// states are non-terminal by the skew law, so they can never close a cell.
fn sync_remote_cells(
    trees: &mut HashMap<u64, HashMap<String, CellRecord>>,
    cell_by_worker: &HashMap<u64, (u64, Addr)>,
    remotes: &HashMap<u64, RemoteWorker>,
    worktrees_dir: &Path,
) {
    for (wid, (mid, addr)) in cell_by_worker {
        let Some(r) = remotes.get(wid) else { continue };
        if !r.is_terminal() { continue; }
        let Some(cell) = trees.get_mut(mid).and_then(|t| t.get_mut(&addr.0)) else { continue };
        if cell.state == r.state_raw && cell.evidence == r.evidence { continue; }
        cell.state = r.state_raw.clone();
        cell.evidence = r.evidence.clone();
        mandala::save_cell(&worktrees_dir.join(mid.to_string()), cell);
    }
}

// ── Torus epochs + the orbit detector (M1d) ─────────────────────────────────

/// Roll due epochs: per open mandala on the golden-offset EPOCH_PERIOD clock,
/// drain this epoch's census, fingerprint (axis + evidence digests + census),
/// persist the reading to Cerebro (best-effort), and — when two consecutive
/// epochs produce the SAME fingerprint over open cells — convene a council
/// over the census instead of grinding a third lap. One council per distinct
/// stuck-state (`orbit_fingerprint` remembers what already convened).
///
/// Deliberate softening vs the charter's "park Blocked" phrasing: v1 detects,
/// records and deliberates but does NOT auto-park cells — the M1c field law
/// (a break is a brake, not a wall) postdates the charter, and the anti-
/// thrash rule owns remediation. Field data reopens this.
fn roll_due_epochs(
    mandalas: &mut HashMap<u64, MandalaRecord>,
    trees: &HashMap<u64, HashMap<String, CellRecord>>,
    remotes: &HashMap<u64, RemoteWorker>,
    censuses: &mut HashMap<u64, HashMap<String, u64>>,
    next_epoch: &mut HashMap<u64, Instant>,
    proxy: &ToolProxy,
    council_tx: &mpsc::Sender<(SessionId, ActionId, serde_json::Value)>,
) -> bool {
    let now = Instant::now();
    let mut dirty = false;
    let mids: Vec<u64> = mandalas.keys().copied().collect();
    for mid in mids {
        match next_epoch.get(&mid) {
            None => {
                // Lazy seed — covers create, reload and restart uniformly.
                next_epoch.insert(mid, now + review::golden_offset(mid, EPOCH_PERIOD));
                continue;
            }
            Some(t) if *t > now => continue,
            Some(_) => {}
        }
        next_epoch.insert(mid, now + EPOCH_PERIOD);
        let open = trees.get(&mid).map(mandala::open_cells).unwrap_or(0);
        if open == 0 { continue; } // settled trees rest — no epochs, no orbits
        // This epoch's histogram drains (the census is per-epoch by charter).
        let census: std::collections::BTreeMap<String, u64> =
            censuses.remove(&mid).unwrap_or_default().into_iter().collect();
        let mut ev_digests: Vec<String> = trees.get(&mid).map(|t| {
            t.values()
                .filter_map(|c| c.evidence.as_ref())
                .filter_map(|p| std::fs::read(p).ok())
                .map(|b| mandala::hex_digest(&b))
                .collect()
        }).unwrap_or_default();
        // M2: open remote-bodied cells fold their live wire state into the
        // fingerprint — remote work is invisible to the local review census,
        // and without this a hard-working remote ring would read as
        // sameness. Truth-as-last-observed (the polls keep it honest).
        let remote_states: std::collections::BTreeMap<String, String> = trees.get(&mid).map(|t| {
            t.values()
                .filter(|c| c.node.is_some() && mandala::is_open_state(&c.state))
                .filter_map(|c| c.worker
                    .and_then(|w| remotes.get(&w))
                    .map(|r| (c.addr.0.clone(), r.state_raw.clone())))
                .collect()
        }).unwrap_or_default();
        let m = mandalas.get_mut(&mid).unwrap();
        let fp = mandala::epoch_fingerprint(&m.invariant.hash, &mut ev_digests, &census, &remote_states);
        let orbit = mandala::is_orbit(m.last_fingerprint.as_deref(), &fp, open)
            && m.orbit_fingerprint.as_deref() != Some(fp.as_str());
        m.epoch += 1;
        m.last_census = census.clone();
        m.last_fingerprint = Some(fp.clone());
        eprintln!("[mandala] {mid} epoch {} rolled (fp {}, {} open cells){}",
                  m.epoch, &fp[..12], open, if orbit { " — ORBIT" } else { "" });
        // The run's reading → Cerebro (the council-handler idiom: best-effort,
        // spawned — a slow or absent Cerebro never stalls the driver tick).
        let mut reading = serde_json::json!({
            "mandala": mid, "epoch": m.epoch, "fingerprint": fp,
            "open_cells": open, "census": census,
            "orbit": orbit, "orbits_total": m.orbits + u64::from(orbit),
            "objective": m.invariant.objective.chars().take(120).collect::<String>(),
        });
        if !remote_states.is_empty() {
            reading["remote"] = serde_json::json!(remote_states);
        }
        let content = format!("mandala {mid} epoch {} reading: {reading}", m.epoch);
        let p2 = proxy.clone();
        tokio::spawn(async move {
            if let Err(e) = p2.call("memory_store", serde_json::json!({
                "content": content,
                "tags": ["mandala-census", "fabrica"],
                "agent_id": apexos_core::node_agent_id(),
            })).await {
                eprintln!("[mandala] census store: {e}");
            }
        });
        if orbit {
            m.orbits += 1;
            m.orbit_fingerprint = Some(fp.clone());
            let council_id = format!("mnd{mid}e{}", m.epoch);
            m.orbit_council = Some(council_id.clone());
            m.orbit_synthesis = None;
            let census_line = census.iter()
                .map(|(k, v)| format!("{k}×{v}")).collect::<Vec<_>>().join(", ");
            let topic = format!(
                "ORBIT on mandala {mid}, epoch {}: two consecutive epochs produced an identical \
                 fingerprint with {open} open cell(s) — the run is circling, not progressing. \
                 OBJECTIVE: {}. REVIEW CENSUS: [{census_line}]. Deliberate ONE next move for the \
                 conductor: which cells to cancel, steer, or integrate around — one line each, \
                 never a subtree restart (the anti-thrash rule).",
                m.epoch, m.invariant.objective.chars().take(200).collect::<String>());
            // The gateway's sentinel pattern: no ToolResult expected. Small
            // and cheap by design — 2 agents × ≤2 rounds + synthesis.
            let args = serde_json::json!({
                "council_id": council_id, "topic": topic,
                "agents": ["VAJRA", "KETHER"], "max_rounds": 2,
            });
            if council_tx.try_send((SessionId(u64::MAX), ActionId(u64::MAX), args)).is_err() {
                eprintln!("[mandala] {mid} orbit council not convened (channel busy) — the record still carries the orbit");
            }
        }
        dirty = true;
    }
    dirty
}

// ── Barriers (M1b) ──────────────────────────────────────────────────────────

/// The work-order block appended when a barrier opens: every descendant with
/// its state and evidence path (paths, not payloads — reading them IS the
/// join), stragglers named honestly. Pure (unit-tested).
fn barrier_block(tree: &HashMap<String, CellRecord>, gate: &Addr, timed_out: bool, repo: Option<&str>) -> String {
    let all = mandala::descendants(tree, gate);
    let mut lines = String::new();
    let mut delivered_branches: Vec<String> = Vec::new();
    for a in &all {
        let c = &tree[&a.0];
        if mandala::is_open_state(&c.state) {
            lines.push_str(&format!("- cell {} [OPEN — not delivered]\n", a.0));
        } else {
            lines.push_str(&format!(
                "- cell {} [{}]: {}\n",
                a.0, c.state, c.evidence.as_deref().unwrap_or("no evidence file")
            ));
            if c.state == "done" {
                delivered_branches.push(a.branch());
            }
        }
    }
    let mut block = format!(
        "\n\nBARRIER OPEN ({}) — your descendant cells:\n{lines}Read each evidence file (and the artifacts it declares) before integrating.",
        if timed_out { "guard timeout — stragglers listed" } else { "subtree settled" }
    );
    if let Some(r) = repo {
        block.push_str(&merge_ritual(r, &delivered_branches));
    }
    block
}

/// For a held gate: (ready = settled-or-timed-out, time until the J guard).
/// Settled means the subtree EXISTS and has no open cells — a childless gate
/// keeps waiting for its fan (creation order is gate-then-fan; the timeout
/// is the escape hatch). Failed/Cancelled descendants count as closed:
/// integration data opens the gate, honesty rides the evidence list.
fn barrier_signals(
    trees: &HashMap<u64, HashMap<String, CellRecord>>,
    cell_by_worker: &HashMap<u64, (u64, Addr)>,
    wid: u64,
) -> (Option<bool>, Option<Duration>) {
    let Some((mid, addr)) = cell_by_worker.get(&wid) else { return (None, None) };
    let Some(tree) = trees.get(mid) else { return (None, None) };
    let Some(cell) = tree.get(&addr.0) else { return (None, None) };
    let waiting = mandala::open_descendants(tree, addr);
    let has_any = !mandala::descendants(tree, addr).is_empty();
    let settled = waiting.is_empty() && has_any;
    let (timed_out, remaining) = match cell.barrier_timeout_s {
        Some(t) => {
            let due = cell.created_epoch.saturating_add(t);
            let now = epoch_now();
            (now >= due, Some(Duration::from_secs(due.saturating_sub(now))))
        }
        None => (false, None),
    };
    (Some(settled || timed_out), remaining)
}

/// Sweep barrier-held gates and open the ready ones: append the descendant
/// evidence block to the gate's task (it rides the task so a park/revive
/// keeps it), release it to FIFO admission, record the open in the cell.
/// Returns true when any gate opened (run admission after).
async fn check_barriers(
    workers: &mut HashMap<u64, Worker>,
    trees: &mut HashMap<u64, HashMap<String, CellRecord>>,
    cell_by_worker: &HashMap<u64, (u64, Addr)>,
    mandalas: &HashMap<u64, MandalaRecord>,
    bus: &BusHandle, worktrees_dir: &Path,
) -> bool {
    let held: Vec<u64> = workers.iter()
        .filter(|(_, w)| w.barrier_held && w.state == WorkerState::Queued)
        .map(|(id, _)| *id)
        .collect();
    let mut opened = false;
    for wid in held {
        let Some((mid, addr)) = cell_by_worker.get(&wid) else { continue };
        let (settled, timed_out) = {
            let Some(tree) = trees.get(mid) else { continue };
            let Some(cell) = tree.get(&addr.0) else { continue };
            let waiting = mandala::open_descendants(tree, addr);
            let has_any = !mandala::descendants(tree, addr).is_empty();
            let settled = waiting.is_empty() && has_any;
            let timed_out = cell.barrier_timeout_s
                .map(|t| epoch_now() >= cell.created_epoch.saturating_add(t))
                .unwrap_or(false);
            (settled, timed_out)
        };
        if !(settled || timed_out) { continue; }
        let repo = mandalas.get(mid).and_then(|m| m.repo.as_deref());
        let block = trees.get(mid).map(|t| barrier_block(t, addr, timed_out && !settled, repo)).unwrap_or_default();
        {
            let w = workers.get_mut(&wid).unwrap();
            w.task.push_str(&block);
            w.barrier_held = false;
        }
        if let Some(cell) = trees.get_mut(mid).and_then(|t| t.get_mut(&addr.0)) {
            cell.barrier_opened = true;
            mandala::save_cell(&worktrees_dir.join(mid.to_string()), cell);
        }
        let w = &workers[&wid];
        let detail = if timed_out && !settled {
            "barrier open — guard timeout, stragglers listed"
        } else {
            "barrier open — subtree settled"
        };
        emit_state(bus, wid, w, detail).await;
        eprintln!("[worker] {wid} {detail}");
        opened = true;
    }
    opened
}

// ── The review procedure's driver side (M1b) ────────────────────────────────

/// The observable builders — the clocks and maps live here, the decision is
/// `review::review`'s. Conservative at M1b: Demand/Capacity are constant
/// true (their builders arm in later slices); Horizon reads the batch
/// deadline; Budget reads step headroom; Verified reads the honest-failure
/// flag. Pure over the worker + precomputed signals (unit-tested).
fn build_review(
    w: &Worker,
    barrier_ready: Option<bool>,
    batch_deadline_ok: bool,
    max_steps: u32,
    step_timeout: Duration,
    idle_ttl: Duration,
) -> (Posture, Word) {
    let posture = posture_of(w);
    let progress = match posture {
        Posture::Live => {
            // Approval-suspended turns run on the human's clock (stall-exempt).
            (w.state == WorkerState::Blocked && w.turn_inflight)
                || w.started.elapsed() <= step_timeout
        }
        Posture::Waiting => w.started.elapsed() <= idle_ttl,
        Posture::BarrierWait => !barrier_ready.unwrap_or(false),
        Posture::Terminal => true,
    };
    let word = Word {
        progress,
        budget: w.step < max_steps || is_terminal(w.state),
        verified: !w.errored,
        demand: true,   // conservative at M1b — armed by later slices
        capacity: true, // conservative at M1b
        horizon: batch_deadline_ok,
    };
    (posture, word)
}

/// Post-review scheduling: LIVE workers hold the fixed period (stall latency
/// is semantics — never backed off); waiting clocks land deadline-exact
/// (detection within a tick of TTL/guard expiry — tighter than the old 30s
/// sweep); repeated identical quiet words climb the Fibonacci ladder; a
/// fresh terminal gets one last look (the Terminal census + reap), then None.
fn schedule_next_review(
    w: &mut Worker, posture: Posture, word: &Word,
    step_timeout: Duration, idle_ttl: Duration,
    barrier_deadline_in: Option<Duration>,
) {
    let now = Instant::now();
    if is_terminal(w.state) {
        w.next_review = if posture == Posture::Terminal { None } else { Some(now) };
        return;
    }
    let key = review::census_key(posture, word);
    let repeated = w.last_review_key.as_deref() == Some(key.as_str());
    w.review_attempt = if repeated { w.review_attempt.saturating_add(1) } else { 0 };
    w.last_review_key = Some(key);
    let interval = match posture {
        Posture::Live => REVIEW_PERIOD,
        _ => review::fib_backoff(w.review_attempt, REVIEW_PERIOD),
    };
    let deadline_in = match posture {
        Posture::Live if w.state == WorkerState::Running =>
            Some(step_timeout.saturating_sub(w.started.elapsed())),
        Posture::Waiting => Some(idle_ttl.saturating_sub(w.started.elapsed())),
        Posture::BarrierWait => barrier_deadline_in,
        _ => None,
    };
    let until = match deadline_in {
        Some(d) => interval.min(d + TICK),
        None => interval,
    };
    w.next_review = Some(now + until);
}

// ── Closure (M1b — the root-open ruling) ────────────────────────────────────

/// A mandala may close when every non-root cell is terminal. Pure.
fn mandala_closable(tree: &HashMap<String, CellRecord>) -> bool {
    tree.iter().all(|(addr, c)| addr.as_str() == Addr::ROOT || !mandala::is_open_state(&c.state))
}

/// Close: root cell marked done (open_cells reaches 0, honestly), mandala
/// state closed. The tree stays on disk, browsable via mandala_status.
fn close_mandala(
    mandalas: &mut HashMap<u64, MandalaRecord>,
    trees: &mut HashMap<u64, HashMap<String, CellRecord>>,
    worktrees_dir: &Path, mid: u64,
) {
    if let Some(m) = mandalas.get_mut(&mid) {
        m.state = "closed".into();
    }
    if let Some(root) = trees.get_mut(&mid).and_then(|t| t.get_mut(Addr::ROOT)) {
        if mandala::is_open_state(&root.state) {
            root.state = "done".into();
            mandala::save_cell(&worktrees_dir.join(mid.to_string()), root);
        }
    }
    eprintln!("[mandala] {mid} closed");
}

/// Auto-closure for a conductor whose goal reached a terminal state: every
/// open mandala it conducts whose cells have all settled closes. Idempotent
/// — boot re-emits persisted terminal goal states, and a closed mandala is
/// filtered out. Returns true when anything closed (persist mandalas.json).
fn try_close_for_conductor(
    mandalas: &mut HashMap<u64, MandalaRecord>,
    trees: &mut HashMap<u64, HashMap<String, CellRecord>>,
    worktrees_dir: &Path, conductor: u64,
) -> bool {
    let ids: Vec<u64> = mandalas.values()
        .filter(|m| m.conductor == conductor && m.state != "closed")
        .filter(|m| trees.get(&m.id).map(mandala_closable).unwrap_or(true))
        .map(|m| m.id)
        .collect();
    for mid in &ids {
        close_mandala(mandalas, trees, worktrees_dir, *mid);
    }
    !ids.is_empty()
}

/// mandala_close: the explicit closure path — interactive conductors never
/// emit a goal-terminal event, so without this their mandalas stay open
/// forever (the M1a field finding). Refuses while non-root cells are open:
/// closing is bookkeeping of finished work, never a kill switch.
async fn close_mandala_request(
    mandalas: &mut HashMap<u64, MandalaRecord>,
    trees: &mut HashMap<u64, HashMap<String, CellRecord>>,
    bus: &BusHandle, worktrees_dir: &Path,
    call_session: SessionId, call_id: ActionId, args: serde_json::Value,
) -> bool {
    let refuse = |msg: String| Event::ToolResult {
        session: call_session, call: call_id,
        output: ToolOutput { ok: false, content: serde_json::json!(msg) },
    };
    if apexos_core::is_worker_session(call_session.0) {
        bus.emit(refuse("workers cannot close mandalas".into())).await;
        return false;
    }
    let Some(mid) = args["mandala"].as_u64() else {
        bus.emit(refuse("mandala id is required — mandala_status lists them".into())).await;
        return false;
    };
    let Some(m) = mandalas.get(&mid) else {
        bus.emit(refuse(format!("no mandala {mid} — mandala_status lists them"))).await;
        return false;
    };
    if m.state == "closed" {
        bus.emit(Event::ToolResult { session: call_session, call: call_id,
            output: ToolOutput { ok: true, content: serde_json::json!({
                "mandala": mid, "status": "closed", "already": true }) } }).await;
        return false;
    }
    let mut open: Vec<String> = trees.get(&mid)
        .map(|t| t.iter()
            .filter(|(a, c)| a.as_str() != Addr::ROOT && mandala::is_open_state(&c.state))
            .map(|(a, _)| a.clone())
            .collect())
        .unwrap_or_default();
    if !open.is_empty() {
        open.sort();
        let preview: Vec<String> = open.iter().take(5).cloned().collect();
        bus.emit(refuse(format!(
            "{} cell(s) still open ({}{}) — finish them or worker_cancel{{batch}} first; closing is bookkeeping, not a kill switch",
            open.len(), preview.join(", "), if open.len() > 5 { ", …" } else { "" }))).await;
        return false;
    }
    close_mandala(mandalas, trees, worktrees_dir, mid);
    bus.emit(Event::ToolResult { session: call_session, call: call_id,
        output: ToolOutput { ok: true, content: serde_json::json!({
            "mandala": mid, "status": "closed",
            "note": "root marked done; the tree stays browsable via mandala_status" }) } }).await;
    true
}

/// mandala_create: write the invariant ONCE (content-addressed), mint the
/// root cell, persist. The conductor session owns the mandala. M1b adds the
/// code-regime hook: an optional workspace-confined `repo` — when set, wide
/// fans inject the worktree ritual and gates the merge ritual, mechanically.
#[allow(clippy::too_many_arguments)]
async fn create_mandala(
    mandalas: &mut HashMap<u64, MandalaRecord>,
    trees: &mut HashMap<u64, HashMap<String, CellRecord>>,
    bus: &BusHandle, worktrees_dir: &Path, workspace: &Path, next_mandala_id: &mut u64,
    call_session: SessionId, call_id: ActionId, args: serde_json::Value,
) {
    let refuse = |msg: &str| Event::ToolResult {
        session: call_session, call: call_id,
        output: ToolOutput { ok: false, content: serde_json::json!(msg) },
    };
    let (objective, done_when, verify) = match (args["objective"].as_str(), args["done_when"].as_str(), args["verify"].as_str()) {
        (Some(o), Some(d), Some(v)) if !o.trim().is_empty() && !d.trim().is_empty() && !v.trim().is_empty() =>
            (o.trim(), d.trim(), v.trim()),
        _ => { bus.emit(refuse("objective, done_when and verify are all required — the invariant is written once and never paraphrased")).await; return; }
    };
    if apexos_core::is_worker_session(call_session.0) {
        bus.emit(refuse("workers cannot open mandalas (sub-conducting arrives with M1c vouchers)")).await;
        return;
    }
    let lattice = match args["lattice"].as_str() {
        None => Lattice::Spine,
        Some(s) => match Lattice::parse(s) {
            Some(l) => l,
            None => { bus.emit(refuse("unknown lattice — spine, quad, fan, spiral or funnel")).await; return; }
        },
    };
    // The repo declaration is confined like every workspace path — a code
    // mandala's cells work inside the agent's own tree, nowhere else.
    let repo = match args["repo"].as_str().map(str::trim).filter(|s| !s.is_empty()) {
        None => None,
        Some(r) => {
            let requested = if Path::new(r).is_absolute() { PathBuf::from(r) } else { workspace.join(r) };
            match apexos_confine::confine_fs(&requested, apexos_confine::Access::Read, workspace, &[], |_| false) {
                Ok(canon) => Some(canon.to_string_lossy().into_owned()),
                Err(_) => {
                    bus.emit(refuse("repo must be an existing directory inside the agent workspace (e.g. code/myproject)")).await;
                    return;
                }
            }
        }
    };
    let budget = BudgetVec {
        depth: args["depth"].as_u64().map(|n| (n as u8).clamp(1, mandala::DEPTH_CEIL)).unwrap_or(mandala::DEPTH_CEIL),
        cells: mandala::CELLS_CEIL,
        steps: args["steps"].as_u64().map(|n| (n as u16).clamp(1, 512)).unwrap_or(32),
        deadline_s: args["deadline_s"].as_u64().map(|n| n.clamp(300, 604_800)).unwrap_or(86_400),
    };
    let id = *next_mandala_id;
    *next_mandala_id += 1;
    let invariant = Invariant::new(objective, done_when, verify);
    let root = CellRecord {
        addr: Addr(Addr::ROOT.into()),
        form: CellForm::SPINE,
        task: objective.to_string(),
        budget,
        invariant_hash: invariant.hash.clone(),
        worker: None, // the conductor itself is the root mind
        state: "open".into(),
        evidence: None,
        reparented_to: None,
        created_epoch: epoch_now(),
        barrier_timeout_s: None,
        barrier_opened: false,
        measure: None,
        measure_history: Vec::new(),
        voucher: false,
        node: None,
    };
    mandala::save_cell(&worktrees_dir.join(id.to_string()), &root);
    trees.entry(id).or_default().insert(root.addr.0.clone(), root);
    let hash = invariant.hash.clone();
    mandalas.insert(id, MandalaRecord {
        id, conductor: call_session.0, lattice, budget, invariant, state: "open".into(),
        repo: repo.clone(), created_epoch: epoch_now(),
        epoch: 0, last_fingerprint: None,
        last_census: std::collections::BTreeMap::new(), orbits: 0,
        orbit_fingerprint: None, orbit_council: None, orbit_synthesis: None,
    });
    let mut content = serde_json::json!({
        "mandala": id, "root": Addr::ROOT, "lattice": format!("{lattice:?}").to_lowercase(),
        "invariant_hash": hash,
        "budget": { "depth": budget.depth, "cells": budget.cells, "steps": budget.steps, "deadline_s": budget.deadline_s },
        "status": "open",
        "next": "grow with task_fanout{mandala, parent_cell?, tasks:[…]} — add join:\"…\" for a gated ring (the one-call diamond); close with mandala_close when settled",
    });
    if let Some(r) = &repo { content["repo"] = serde_json::json!(r); }
    bus.emit(Event::ToolResult { session: call_session, call: call_id,
        output: ToolOutput { ok: true, content } }).await;
    eprintln!("[mandala] {id} opened by session {} ({lattice:?}, depth {}{})",
              call_session.0, budget.depth,
              if repo.is_some() { ", code regime" } else { "" });
}

/// mandala_status: the tree as data — addresses, forms, states, workers,
/// evidence, barriers, and (M1b) the review census: the run's live reading.
/// M2: remote-bodied cells additionally show their hosting `node` and the
/// body's live wire state (truth-as-last-observed — polls keep it honest).
async fn handle_mandala_status(
    mandalas: &HashMap<u64, MandalaRecord>,
    trees: &HashMap<u64, HashMap<String, CellRecord>>,
    remotes: &HashMap<u64, RemoteWorker>,
    censuses: &HashMap<u64, HashMap<String, u64>>,
    bus: &BusHandle, call_session: SessionId, call_id: ActionId,
) {
    let render = |m: &MandalaRecord| {
        let tree = trees.get(&m.id).cloned().unwrap_or_default();
        let mut addrs: Vec<&String> = tree.keys().collect();
        addrs.sort();
        let mut out = serde_json::json!({
            "mandala": m.id, "conductor": m.conductor,
            "lattice": format!("{:?}", m.lattice).to_lowercase(),
            "state": m.state,
            "invariant_hash": m.invariant.hash,
            "objective": m.invariant.objective,
            "open_cells": mandala::open_cells(&tree),
            "cells_budget": m.budget.cells,
            "cells": addrs.iter().map(|a| {
                let c = &tree[a.as_str()];
                let mut cell = serde_json::json!({
                    "addr": c.addr.0, "form": c.form.name(),
                    "state": c.state, "worker": c.worker,
                    "depth_left": c.budget.depth, "steps_left": c.budget.steps,
                    "evidence": c.evidence,
                    "reparented_to": c.reparented_to.as_ref().map(|r| r.0.clone()),
                });
                if let Some(t) = c.barrier_timeout_s {
                    cell["barrier_timeout_s"] = serde_json::json!(t);
                    cell["barrier_opened"] = serde_json::json!(c.barrier_opened);
                }
                if let Some(m) = &c.measure {
                    cell["measure"] = serde_json::json!(m);
                    let tail: Vec<u64> = c.measure_history.iter().rev().take(6).rev().copied().collect();
                    cell["measure_history"] = serde_json::json!(tail);
                }
                if c.voucher { cell["voucher"] = serde_json::json!(true); }
                if let Some(n) = &c.node {
                    cell["node"] = serde_json::json!(n);
                    if let Some(r) = c.worker.and_then(|w| remotes.get(&w)) {
                        cell["body"] = serde_json::json!(r.state_raw);
                    }
                }
                cell
            }).collect::<Vec<_>>(),
        });
        if let Some(r) = &m.repo { out["repo"] = serde_json::json!(r); }
        if let Some(census) = censuses.get(&m.id) {
            // Sorted for stable output — the census key is "<posture>:<PBV DCH bits>".
            let sorted: std::collections::BTreeMap<&String, &u64> = census.iter().collect();
            out["census"] = serde_json::json!(sorted);
        } else if !m.last_census.is_empty() {
            // Post-restart: the persisted last-epoch reading (M1d) — the
            // in-memory histogram died with the old process, the epoch's didn't.
            out["census"] = serde_json::json!(m.last_census);
        }
        // M1d: the torus reading — epoch, fingerprint, and any orbit verdicts.
        if m.epoch > 0 {
            out["epoch"] = serde_json::json!(m.epoch);
            if let Some(fp) = &m.last_fingerprint {
                out["fingerprint"] = serde_json::json!(&fp[..fp.len().min(12)]);
            }
        }
        if m.orbits > 0 {
            out["orbits"] = serde_json::json!(m.orbits);
            if let Some(c) = &m.orbit_council { out["orbit_council"] = serde_json::json!(c); }
            if let Some(s) = &m.orbit_synthesis { out["orbit_synthesis"] = serde_json::json!(s); }
        }
        out
    };
    let content = {
        let mut ms: Vec<&MandalaRecord> = mandalas.values().collect();
        ms.sort_by_key(|m| m.id);
        serde_json::json!({ "mandalas": ms.iter().map(|m| render(m)).collect::<Vec<_>>(), "count": ms.len() })
    };
    bus.emit(Event::ToolResult { session: call_session, call: call_id,
        output: ToolOutput { ok: true, content } }).await;
}

/// Boot: reload mandalas.json + every tree dir (THE FILESYSTEM IS THE TREE —
/// reconstruction scans, reparenting heals), rebuild the worker→cell index,
/// re-save any reparented cells so the healing persists.
fn reload_mandalas(
    mandalas: &mut HashMap<u64, MandalaRecord>,
    trees: &mut HashMap<u64, HashMap<String, CellRecord>>,
    cell_by_worker: &mut HashMap<u64, (u64, Addr)>,
    mandalas_path: &Path, worktrees_dir: &Path, next_mandala_id: &mut u64,
) {
    *mandalas = mandala::load_mandalas(mandalas_path);
    let ids: Vec<u64> = mandalas.keys().copied().collect();
    for id in ids {
        *next_mandala_id = (*next_mandala_id).max(id + 1);
        let tree_dir = worktrees_dir.join(id.to_string());
        let mut tree = mandala::load_tree(&tree_dir);
        for cell in tree.values() {
            if let Some(rep) = &cell.reparented_to {
                mandala::save_cell(&tree_dir, cell);
                eprintln!("[mandala] {} cell {} reparented to {}", id, cell.addr.0, rep.0);
            }
            if let Some(w) = cell.worker {
                cell_by_worker.insert(w, (id, cell.addr.clone()));
            }
        }
        // Open cells whose workers vanished entirely (pruned by hand) stay
        // open in the tree — mandala_status surfaces them; the conductor or a
        // later slice's review procedure decides. Never silently closed.
        tree.shrink_to_fit();
        trees.insert(id, tree);
    }
    if !mandalas.is_empty() {
        eprintln!("[mandala] reloaded {} mandala(s) from {}", mandalas.len(), mandalas_path.display());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cap_env_wins_and_floors_at_one() {
        assert_eq!(worker_cap_from_env(Some("6"), "micro"), 6);    // valid env wins over tier
        assert_eq!(worker_cap_from_env(Some("0"), "pro"), 8);      // below floor → tier default
        assert_eq!(worker_cap_from_env(Some("oops"), "standard"), 4); // unparseable → tier default
        assert_eq!(worker_cap_from_env(Some("1"), "pro"), 1);      // explicit floor honored
    }

    #[test]
    fn cap_tier_defaults_match_the_charter() {
        // micro 2 · standard 4 · pro 8 (the charter's "gpu" bucket IS the pro
        // tier string — no GPU probe exists); nano/unknown floor to 1.
        assert_eq!(worker_cap_from_env(None, "nano"), 1);
        assert_eq!(worker_cap_from_env(None, "micro"), 2);
        assert_eq!(worker_cap_from_env(None, "standard"), 4);
        assert_eq!(worker_cap_from_env(None, "pro"), 8);
        assert_eq!(worker_cap_from_env(None, "unknown"), 1);
    }

    #[test]
    fn step_timeout_clamps_and_defaults() {
        assert_eq!(parse_step_timeout(None), STEP_TIMEOUT);
        assert_eq!(parse_step_timeout(Some("nope")), STEP_TIMEOUT);
        assert_eq!(parse_step_timeout(Some("5")), STEP_TIMEOUT);   // below the 30s floor
        assert_eq!(parse_step_timeout(Some("120")), Duration::from_secs(120));
    }

    #[test]
    fn parse_tasks_accepts_strings_and_objects_with_model_precedence() {
        let args = serde_json::json!({
            "tasks": ["write a haiku", { "prompt": "count to three", "model": "claude-haiku-4-5" }],
            "model": "claude-sonnet-5"
        });
        let tasks = parse_tasks(&args).unwrap();
        assert_eq!(tasks[0].prompt, "write a haiku");
        assert_eq!(tasks[0].model.as_deref(), Some("claude-sonnet-5"));
        // Per-task model wins over the batch default.
        assert_eq!(tasks[1].prompt, "count to three");
        assert_eq!(tasks[1].model.as_deref(), Some("claude-haiku-4-5"));
        assert!(tasks[1].measure.is_none());
        assert!(!tasks[1].voucher);
        // No models anywhere → node default (None).
        let bare = parse_tasks(&serde_json::json!({ "tasks": ["x"] })).unwrap();
        assert_eq!(bare[0].prompt, "x");
        assert!(bare[0].model.is_none());
        // M1c knobs parse per task.
        let r = parse_tasks(&serde_json::json!({ "tasks": [
            { "prompt": "shrink the count", "measure": "grep -rc TODO src", "voucher": true }
        ] })).unwrap();
        assert_eq!(r[0].measure.as_deref(), Some("grep -rc TODO src"));
        assert!(r[0].voucher);
    }

    #[test]
    fn parse_tasks_rejects_empty_missing_and_oversized() {
        assert!(parse_tasks(&serde_json::json!({})).is_err());                       // no array
        assert!(parse_tasks(&serde_json::json!({ "tasks": [] })).is_err());          // empty
        assert!(parse_tasks(&serde_json::json!({ "tasks": ["ok", ""] })).is_err());  // blank item
        assert!(parse_tasks(&serde_json::json!({ "tasks": [{"note": "no prompt"}] })).is_err());
        let too_many: Vec<&str> = vec!["t"; MAX_BATCH_TASKS + 1];
        assert!(parse_tasks(&serde_json::json!({ "tasks": too_many })).is_err());
    }

    #[test]
    fn restart_parks_every_non_terminal_state() {
        // The W1a restart contract: Running → Parked, and nothing auto-runs —
        // Queued/Idle/Blocked park too. Terminal states pass through untouched.
        for s in [WorkerState::Queued, WorkerState::Running, WorkerState::Idle,
                  WorkerState::Parked, WorkerState::Blocked] {
            assert_eq!(parked_form(s), WorkerState::Parked, "{s:?} must park");
        }
        for s in [WorkerState::Done, WorkerState::Failed, WorkerState::Cancelled] {
            assert_eq!(parked_form(s), s, "{s:?} is terminal");
        }
    }

    fn mk(state: WorkerState) -> Worker {
        Worker { batch: 1, parent: 0, session: apexos_core::WORKER_SESSION_BASE,
                 task: "t".into(), state, step: 1, summary: None,
                 artifacts: Vec::new(), episode: None,
                 started: Instant::now(), pending: None, turn_inflight: false,
                 yolo: false, model: None, errored: false, step_ceiling: 0,
                 barrier_held: false, next_review: None,
                 last_review_key: None, review_attempt: 0 }
    }

    #[test]
    fn slot_accounting_counts_live_turns_only() {
        // Thermal slot = a live turn: Running, or Blocked with the turn suspended
        // on an approval. Idle/verdict-blocked (turn completed) hold none — the
        // thermal budget is RUNNING residency (docs/fabrica.md).
        let mut m = HashMap::new();
        m.insert(1, mk(WorkerState::Running));
        m.insert(2, Worker { turn_inflight: true, ..mk(WorkerState::Blocked) });  // approval wait
        m.insert(3, mk(WorkerState::Blocked));                                   // verdict-blocked
        m.insert(4, mk(WorkerState::Idle));
        m.insert(5, mk(WorkerState::Queued));
        m.insert(6, mk(WorkerState::Parked));
        m.insert(7, mk(WorkerState::Done));
        assert_eq!(slots_used(&m), 2);
        assert_eq!(next_queued(&m), Some(5));
    }

    #[test]
    fn fifo_picks_the_lowest_queued_id() {
        let mut m = HashMap::new();
        m.insert(9, mk(WorkerState::Queued));
        m.insert(4, mk(WorkerState::Queued));
        m.insert(2, mk(WorkerState::Done));
        assert_eq!(next_queued(&m), Some(4));
    }

    #[test]
    fn persisted_worker_round_trips_json() {
        let pw = PersistedWorker {
            id: 7, batch: 2, parent: 3, session: apexos_core::WORKER_SESSION_BASE + 6,
            task: "refactor the parser".into(), state: WorkerState::Running,
            step: 3, summary: Some("parser refactored".into()),
            artifacts: vec!["out/parser.rs".into()], episode: Some("ep_x".into()),
            yolo: true, model: Some("claude-haiku-4-5".into()), step_ceiling: 8,
        };
        let back: PersistedWorker = serde_json::from_str(&serde_json::to_string(&pw).unwrap()).unwrap();
        assert_eq!(back.id, 7);
        assert_eq!(back.batch, 2);
        assert_eq!(back.session, apexos_core::WORKER_SESSION_BASE + 6);
        assert_eq!(back.state, WorkerState::Running);
        assert_eq!(back.task, "refactor the parser");
        assert_eq!(back.step, 3);
        assert_eq!(back.summary.as_deref(), Some("parser refactored"));
        assert_eq!(back.artifacts, vec!["out/parser.rs"]);
        assert_eq!(back.episode.as_deref(), Some("ep_x"));
        assert_eq!(back.step_ceiling, 8);
    }

    #[test]
    fn persisted_worker_earlier_slice_json_defaults_new_fields() {
        // A workers.json written by W1a/W1b lacks step/summary/artifacts/episode —
        // the serde defaults must carry it, never a parse failure.
        let legacy = format!(
            r#"{{"id":1,"batch":1,"parent":0,"session":{},"task":"x","state":"parked"}}"#,
            apexos_core::WORKER_SESSION_BASE
        );
        let pw: PersistedWorker = serde_json::from_str(&legacy).unwrap();
        assert_eq!(pw.step, 1);
        assert!(pw.summary.is_none());
        assert!(pw.artifacts.is_empty());
        assert!(pw.episode.is_none());
        assert!(!pw.yolo);
        assert!(pw.model.is_none());
        assert_eq!(pw.step_ceiling, 0, "pre-M1c workers fall back to the env global");
    }

    #[test]
    fn charter_and_directives_carry_the_contract() {
        let sys = worker_system("APEX");
        assert!(sys.contains("worker_report"), "the verdict tool must ride the charter");
        assert!(sys.contains("depth-1"), "the no-fanout law must ride the charter");
        assert!(sys.contains("VOUCHER"), "the sub-conduction exception must be named (M1c)");
        assert!(sys.contains("Skip orientation"));
        assert!(sys.contains("final text"), "the no-report fallback must be named");
        let first = directive_first(3, 1, 12, "write the tests");
        assert!(first.contains("WORKER 3 (batch 1)"));
        assert!(first.contains("step 1/12"));
        assert!(first.contains("write the tests"));
        assert!(first.contains("worker_report"));
        let cont = directive_continue(3, 1, 4, 12, "write the tests", Some("edge cases"));
        assert!(cont.contains("step 4/12"));
        assert!(cont.contains("edge cases"));
    }

    #[test]
    fn parse_verdict_maps_status_and_carries_measures() {
        assert!(matches!(parse_verdict(&serde_json::json!({"status":"done","summary":"shipped","artifacts":["a.md"]})),
                         Verdict::Done { summary, artifacts, measure: None } if summary == "shipped" && artifacts == vec!["a.md"]));
        assert!(matches!(parse_verdict(&serde_json::json!({"status":"blocked","reason":"no key"})),
                         Verdict::Blocked(r) if r == "no key"));
        assert!(matches!(parse_verdict(&serde_json::json!({"status":"yield"})), Verdict::Yield));
        assert!(matches!(parse_verdict(&serde_json::json!({"status":"continue","next":"tests"})),
                         Verdict::Continue { steer: Some(s), measure: None } if s == "tests"));
        // Absent/unknown status defaults to continue (the goal_step convention).
        assert!(matches!(parse_verdict(&serde_json::json!({})), Verdict::Continue { steer: None, measure: None }));
        // The measure rides continue and done; junk reads as absent (lenient).
        assert!(matches!(parse_verdict(&serde_json::json!({"status":"continue","measure": 7})),
                         Verdict::Continue { measure: Some(7), .. }));
        assert!(matches!(parse_verdict(&serde_json::json!({"status":"done","summary":"s","measure": 0})),
                         Verdict::Done { measure: Some(0), .. }));
        assert!(matches!(parse_verdict(&serde_json::json!({"status":"continue","measure": -3})),
                         Verdict::Continue { measure: None, .. }));
        assert!(matches!(parse_verdict(&serde_json::json!({"status":"continue","measure": "many"})),
                         Verdict::Continue { measure: None, .. }));
    }

    #[test]
    fn approval_detail_prefix_is_the_ui_digest_contract() {
        // ui-slint's per-batch approval digest keys off this exact prefix in
        // WorkerStateChanged.detail — change it there and here TOGETHER.
        let detail = format!("awaiting approval — {}", "run_command");
        assert!(detail.starts_with("awaiting approval"));
    }

    #[test]
    fn inline_bounds_are_hard() {
        assert!(INLINE_MAX_TASKS <= 4);
        assert!(INLINE_DEADLINE_CEIL_S <= 240);
        assert!(PB1_SPAWN_THRESHOLD >= 3, "the breaker is soft and late, never eager");
    }

    #[test]
    fn batch_deadline_clamps_and_defaults() {
        assert_eq!(parse_batch_deadline(&serde_json::json!({})), DEFAULT_BATCH_DEADLINE_S);
        assert_eq!(parse_batch_deadline(&serde_json::json!({"batch_deadline_s": 5})), 60);
        assert_eq!(parse_batch_deadline(&serde_json::json!({"batch_deadline_s": 999_999})), 86_400);
        assert_eq!(parse_batch_deadline(&serde_json::json!({"batch_deadline_s": 300})), 300);
    }

    #[test]
    fn confine_artifacts_gates_the_workspace() {
        let ws = std::env::temp_dir().join(format!("apexos-worker-ws-{}", std::process::id()));
        std::fs::create_dir_all(ws.join("out")).unwrap();
        std::fs::write(ws.join("out/report.md"), "x").unwrap();
        let ws = ws.canonicalize().unwrap();
        // Relative path roots at the workspace; canonical form comes back.
        let ok = confine_artifacts(&["out/report.md".into()], &ws).unwrap();
        assert!(ok[0].starts_with(&*ws.to_string_lossy()));
        // Traversal and absolute escapes refuse with the offending path named.
        assert!(confine_artifacts(&["../escape.md".into()], &ws).is_err());
        assert!(confine_artifacts(&["/etc/passwd".into()], &ws).is_err());
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn batch_rows_are_pointers_with_straggler_marks() {
        let agents = Path::new("/var/lib/agentd/events/agents");
        let mut m = HashMap::new();
        m.insert(1, Worker { batch: 7, ..mk(WorkerState::Done) });
        m.insert(2, Worker { batch: 7, ..mk(WorkerState::Failed) });
        m.insert(3, Worker { batch: 7, ..mk(WorkerState::Parked) });   // straggler
        m.insert(4, Worker { batch: 8, ..mk(WorkerState::Running) });  // other batch
        let rows = batch_rows(&m, &HashMap::new(), 7, agents);
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].worker, WorkerId(1));
        assert!(rows[0].evidence.ends_with("agents/1.json"));
        assert!(!rows[0].timed_out);
        assert!(rows[1].evidence.ends_with("agents/2.json"));
        assert!(rows[2].timed_out, "non-terminal at report time = timed_out");
        assert!(rows[2].evidence.is_empty(), "no evidence file for a straggler");
    }

    fn mk_remote(batch: u64, node: &str, state: &str) -> RemoteWorker {
        RemoteWorker {
            batch, parent: 42, node: node.into(), task: "remote task".into(),
            model: None, remote_batch: Some(3), remote_worker: Some(11),
            remote_session: Some(apexos_core::WORKER_SESSION_BASE),
            state_raw: state.into(), summary: None, evidence: None, assigned_epoch: 0,
        }
    }

    #[test]
    fn batch_rows_join_remote_mirrors_with_node_tags() {
        // W2: a mixed batch's rows carry local rows (node None) and remote
        // mirrors (node Some, MIRROR evidence path, tolerant state mapping).
        let agents = Path::new("/var/lib/agentd/events/agents");
        let mut locals = HashMap::new();
        locals.insert(1, Worker { batch: 7, ..mk(WorkerState::Done) });
        let mut remotes = HashMap::new();
        remotes.insert(2, RemoteWorker {
            evidence: Some("/var/lib/agentd/events/agents/2.json".into()),
            ..mk_remote(7, "apex-3", "done")
        });
        remotes.insert(3, mk_remote(7, "apex-3", "running"));            // straggler
        remotes.insert(4, mk_remote(7, "tvpi", "hibernating"));          // unknown state (newer peer)
        remotes.insert(9, mk_remote(8, "apex-3", "done"));               // other batch
        let rows = batch_rows(&locals, &remotes, 7, agents);
        assert_eq!(rows.len(), 4);
        assert!(rows[0].node.is_none(), "local rows carry no node");
        assert_eq!(rows[1].node.as_deref(), Some("apex-3"));
        assert_eq!(rows[1].evidence, "/var/lib/agentd/events/agents/2.json", "the MIRROR path rides the row");
        assert!(!rows[1].timed_out);
        assert!(rows[2].timed_out, "non-terminal remote at report time = timed_out");
        assert!(rows[2].evidence.is_empty());
        // The version-skew law: an unknown state is non-terminal (timed_out
        // carries the truth) and maps to the bounded typed fallback.
        assert!(rows[3].timed_out);
        assert_eq!(rows[3].state, WorkerState::Queued);
        assert_eq!(rows[3].node.as_deref(), Some("tvpi"));
    }

    #[test]
    fn wire_rows_carry_evidence_docs_for_terminals_only() {
        // Peer role: a hosted batch's wire rows inline the evidence DOC for
        // terminal workers (one hop — the conductor mirrors it) and never for
        // live ones. Missing files read None, never a panic.
        let agents = Path::new("/nonexistent/agents-dir");
        let mut m = HashMap::new();
        m.insert(1, Worker { batch: 7, summary: Some("shipped".into()), ..mk(WorkerState::Done) });
        m.insert(2, Worker { batch: 7, ..mk(WorkerState::Running) });
        let rows = wire_rows_for_batch(&m, 7, agents);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].state, "done");
        assert_eq!(rows[0].summary.as_deref(), Some("shipped"));
        assert!(rows[0].evidence_doc.is_none(), "unreadable evidence reads None (the doc travels when it exists)");
        assert_eq!(rows[1].state, "running");
        assert!(rows[1].evidence_doc.is_none(), "live rows never carry docs");
        assert_eq!(rows[0].session, apexos_core::WORKER_SESSION_BASE, "the peer's real session rides the row");
    }

    #[test]
    fn parse_tasks_carries_node_with_batch_default() {
        // W2: per-task node wins over the batch-level default; absent = local.
        let args = serde_json::json!({
            "tasks": ["local one", { "prompt": "remote one", "node": "apex-3" }, { "prompt": "defaulted" }],
            "node": "tvpi"
        });
        let tasks = parse_tasks(&args).unwrap();
        assert_eq!(tasks[0].node.as_deref(), Some("tvpi"), "batch node reaches bare-string tasks");
        assert_eq!(tasks[1].node.as_deref(), Some("apex-3"), "per-task node wins");
        assert_eq!(tasks[2].node.as_deref(), Some("tvpi"));
        let local = parse_tasks(&serde_json::json!({ "tasks": ["x"] })).unwrap();
        assert!(local[0].node.is_none(), "no node anywhere = local fan");
    }

    #[test]
    fn arm_disarm_mirror_residency() {
        use std::collections::HashSet;
        let yolo_set: GoalYoloSessions = std::sync::Arc::new(std::sync::Mutex::new(HashSet::new()));
        let models: WorkerModels = std::sync::Arc::new(std::sync::Mutex::new(HashMap::new()));
        let sid = apexos_core::WORKER_SESSION_BASE + 9;
        let w = Worker { yolo: true, model: Some("claude-haiku-4-5".into()),
                         session: sid, ..mk(WorkerState::Running) };
        arm_worker(&yolo_set, &models, &w);
        assert!(apexos_core::goal_session_is_yolo(&yolo_set, sid));
        assert_eq!(apexos_core::worker_model_for(&models, sid).as_deref(), Some("claude-haiku-4-5"));
        disarm_worker(&yolo_set, &models, sid);
        assert!(!apexos_core::goal_session_is_yolo(&yolo_set, sid));
        assert!(apexos_core::worker_model_for(&models, sid).is_none());
        // A non-yolo, unpinned worker arms nothing.
        let plain = Worker { session: sid + 1, ..mk(WorkerState::Running) };
        arm_worker(&yolo_set, &models, &plain);
        assert!(!apexos_core::goal_session_is_yolo(&yolo_set, sid + 1));
        assert!(apexos_core::worker_model_for(&models, sid + 1).is_none());
    }

    #[test]
    fn terminal_states_are_exactly_done_failed_cancelled() {
        for s in [WorkerState::Done, WorkerState::Failed, WorkerState::Cancelled] {
            assert!(is_terminal(s), "{s:?}");
        }
        for s in [WorkerState::Queued, WorkerState::Running, WorkerState::Idle,
                  WorkerState::Parked, WorkerState::Blocked] {
            assert!(!is_terminal(s), "{s:?}");
        }
    }

    #[test]
    fn idle_ttl_and_max_steps_clamp_and_default() {
        assert_eq!(parse_idle_ttl(None), IDLE_TTL);
        assert_eq!(parse_idle_ttl(Some("10")), IDLE_TTL);   // below the 60s floor
        assert_eq!(parse_idle_ttl(Some("300")), Duration::from_secs(300));
        assert_eq!(parse_max_steps(None), DEFAULT_MAX_STEPS);
        assert_eq!(parse_max_steps(Some("0")), 1);          // clamped to the floor
        assert_eq!(parse_max_steps(Some("500")), 100);      // clamped to the ceiling
        assert_eq!(parse_max_steps(Some("3")), 3);
    }

    #[test]
    fn load_workers_missing_file_is_empty() {
        assert!(load_workers(&PathBuf::from("/nonexistent/apexos-workers-xyz.json")).is_empty());
    }

    // ── M1b: barriers, postures, rituals, closure ───────────────────────────

    #[test]
    fn barrier_held_gates_are_invisible_to_the_fifo() {
        let mut m = HashMap::new();
        m.insert(1, Worker { barrier_held: true, ..mk(WorkerState::Queued) }); // the gate
        m.insert(2, mk(WorkerState::Queued));                                  // plain ring member
        assert_eq!(next_queued(&m), Some(2), "the held gate must not admit");
        // The barrier opens → the gate fronts the FIFO (lowest id).
        m.get_mut(&1).unwrap().barrier_held = false;
        assert_eq!(next_queued(&m), Some(1));
    }

    #[test]
    fn posture_maps_residency_not_state_names() {
        assert_eq!(posture_of(&mk(WorkerState::Running)), Posture::Live);
        // Approval-suspended turn: Blocked but LIVE (slot held, human's clock).
        assert_eq!(posture_of(&Worker { turn_inflight: true, ..mk(WorkerState::Blocked) }), Posture::Live);
        // Verdict-blocked (no turn): waiting.
        assert_eq!(posture_of(&mk(WorkerState::Blocked)), Posture::Waiting);
        assert_eq!(posture_of(&mk(WorkerState::Idle)), Posture::Waiting);
        assert_eq!(posture_of(&Worker { barrier_held: true, ..mk(WorkerState::Queued) }), Posture::BarrierWait);
        assert_eq!(posture_of(&mk(WorkerState::Queued)), Posture::Waiting); // plain queued (unscheduled)
        for s in [WorkerState::Done, WorkerState::Failed, WorkerState::Cancelled] {
            assert_eq!(posture_of(&mk(s)), Posture::Terminal);
        }
    }

    #[test]
    fn build_review_keeps_the_shipped_clock_semantics() {
        let step_timeout = Duration::from_secs(900);
        let idle_ttl = Duration::from_secs(1800);
        // A fresh Running worker is healthy.
        let (p, w) = build_review(&mk(WorkerState::Running), None, true, 12, step_timeout, idle_ttl);
        assert_eq!(p, Posture::Live);
        assert!(w.progress);
        assert_eq!(review::review(p, &w), Remediation::Healthy);
        // Approval-Blocked with a turn in flight: stall-EXEMPT even when the
        // clock is ancient (the human's clock, the W1b law).
        let old = Instant::now() - Duration::from_secs(10_000);
        let (p, w) = build_review(&Worker { turn_inflight: true, started: old, ..mk(WorkerState::Blocked) },
                                  None, true, 12, step_timeout, idle_ttl);
        assert_eq!(p, Posture::Live);
        assert!(w.progress, "approval waits never stall");
        // A Running worker past the step timeout fails — the same rule
        // fail_stalled enforced, now via the review table.
        let (p, w) = build_review(&Worker { started: old, ..mk(WorkerState::Running) },
                                  None, true, 12, step_timeout, idle_ttl);
        assert_eq!(review::review(p, &w), Remediation::Fail);
        // An Idle worker past the TTL parks (park_idle's rule).
        let (p, w) = build_review(&Worker { started: old, ..mk(WorkerState::Idle) },
                                  None, true, 12, step_timeout, idle_ttl);
        assert_eq!(review::review(p, &w), Remediation::Park);
        // A held gate whose barrier is ready opens; not ready waits.
        let gate = Worker { barrier_held: true, ..mk(WorkerState::Queued) };
        let (p, w) = build_review(&gate, Some(true), true, 12, step_timeout, idle_ttl);
        assert_eq!(review::review(p, &w), Remediation::OpenBarrier);
        let (p, w) = build_review(&gate, Some(false), true, 12, step_timeout, idle_ttl);
        assert_eq!(review::review(p, &w), Remediation::Healthy);
        // Terminal → Reap, whatever the clocks say.
        let (p, w) = build_review(&Worker { started: old, ..mk(WorkerState::Done) },
                                  None, false, 12, step_timeout, idle_ttl);
        assert_eq!(review::review(p, &w), Remediation::Reap);
        // A batch past its deadline does NOT kill a live worker — horizon is
        // censused, the batch deadline stays a report bound.
        let (p, w) = build_review(&mk(WorkerState::Running), None, false, 12, step_timeout, idle_ttl);
        assert!(!w.horizon);
        assert_eq!(review::review(p, &w), Remediation::Healthy);
    }

    fn cell(addr: &str, state: &str, evidence: Option<&str>) -> CellRecord {
        CellRecord {
            addr: Addr::parse(addr).unwrap(), form: CellForm::SPINE, task: String::new(),
            budget: BudgetVec { depth: 3, cells: 1, steps: 4, deadline_s: 600 },
            invariant_hash: "h".into(), worker: None, state: state.into(),
            evidence: evidence.map(str::to_owned), reparented_to: None,
            created_epoch: 0, barrier_timeout_s: None, barrier_opened: false,
            measure: None, measure_history: Vec::new(), voucher: false, node: None,
        }
    }

    #[test]
    fn barrier_block_lists_paths_and_marks_stragglers() {
        let mut tree = HashMap::new();
        for c in [cell("0", "open", None), cell("0.0", "open", None),
                  cell("0.0.0", "done", Some("/var/lib/agentd/events/agents/7.json")),
                  cell("0.0.1", "failed", Some("/var/lib/agentd/events/agents/8.json")),
                  cell("0.0.2", "open", None)] {
            tree.insert(c.addr.0.clone(), c);
        }
        let gate = Addr::parse("0.0").unwrap();
        let block = barrier_block(&tree, &gate, true, None);
        assert!(block.contains("guard timeout — stragglers listed"));
        assert!(block.contains("cell 0.0.0 [done]: /var/lib/agentd/events/agents/7.json"));
        assert!(block.contains("cell 0.0.1 [failed]"), "failed descendants ride the list — integration data");
        assert!(block.contains("cell 0.0.2 [OPEN — not delivered]"));
        assert!(!block.contains("MERGE RITUAL"), "no repo → no merge ritual");
        // Code mandala: the ritual lists only DELIVERED (done) branches.
        let block = barrier_block(&tree, &gate, false, Some("/ws/code/proj"));
        assert!(block.contains("subtree settled"));
        assert!(block.contains("MERGE RITUAL"));
        assert!(block.contains("apex/w/0.0.0"));
        assert!(!block.contains("apex/w/0.0.1"), "failed cells' branches are not 'delivered'");
        assert!(block.contains("VERIFY"), "the join runs the root's verify");
    }

    #[test]
    fn rituals_carry_the_mechanical_contract() {
        let wt = worktree_ritual("/ws/code/proj", "apex/w/0.1.2");
        assert!(wt.contains("git_worktree"));
        assert!(wt.contains("apex/w/0.1.2"));
        assert!(wt.contains("ONLY"), "the collision-safety rule rides verbatim");
        let note = gate_note(1200);
        assert!(note.contains("1200s"));
        assert!(note.contains("evidence"));
        // The join declares its artifacts — mechanical since the first field
        // gate legally skipped them (2026-07-31 smoke find).
        let mr = merge_ritual("/ws/code/proj", &["apex/w/0.1.0".into()]);
        assert!(mr.contains("artifacts"), "the evidence rule reaches the join");
        assert!(mr.contains("VERIFY"));
    }

    #[test]
    fn mandala_closable_ignores_the_root_and_respects_open_cells() {
        let mut tree = HashMap::new();
        tree.insert("0".into(), cell("0", "open", None)); // the root stays open till closure
        tree.insert("0.0".into(), cell("0.0", "done", None));
        tree.insert("0.1".into(), cell("0.1", "cancelled", None));
        assert!(mandala_closable(&tree), "root-open + all non-root terminal = closable");
        tree.insert("0.2".into(), cell("0.2", "open", None));
        assert!(!mandala_closable(&tree));
    }

    #[test]
    fn forge_cells_start_lapping_while_pure_gates_hold() {
        assert!(holds_at_mint(Some(900), None), "GATE/DIAMOND hold at mint");
        assert!(!holds_at_mint(Some(900), Some("grep -c TODO")), "FORGE (R+J) laps immediately");
        assert!(!holds_at_mint(None, Some("grep -c TODO")), "SPIRAL never holds");
        assert!(!holds_at_mint(None, None), "SPINE never holds");
    }

    #[test]
    fn step_ceiling_prefers_the_cell_budget_over_the_env_global() {
        let plain = mk(WorkerState::Running); // step_ceiling 0 = the sentinel
        assert_eq!(effective_ceiling(&plain, 12), 12);
        let cellw = Worker { step_ceiling: 8, ..mk(WorkerState::Running) };
        assert_eq!(effective_ceiling(&cellw, 12), 8, "the budget IS the contract");
        let renewed = Worker { step_ceiling: 20, ..mk(WorkerState::Running) };
        assert_eq!(effective_ceiling(&renewed, 12), 20, "renewals raise it past the global");
    }

    #[test]
    fn m1c_rituals_and_notes_carry_the_contract() {
        let mr = measure_ritual("cargo test 2>&1 | grep -c FAILED");
        assert!(mr.contains("STRICTLY DECREASE"));
        assert!(mr.contains("worker_report"));
        assert!(mr.contains("measure: N"));
        assert!(mr.contains("report done"), "the zero rule must be taught");
        let vb = voucher_block(2, &Addr::parse("0.3").unwrap());
        assert!(vb.contains("SUB-CONDUCT"));
        assert!(vb.contains("mandala: 2"));
        assert!(vb.contains("\"0.3\""));
        assert!(vb.contains("fan late"), "the economy rides the grant");
        let note = kstall_note(&Addr::parse("0.1.2").unwrap(), &[7, 7, 7]);
        assert!(note.contains("0.1.2"));
        assert!(note.contains("7→7→7"));
        assert!(note.contains("one line at a time"), "the anti-thrash doctrine rides escalation");
    }

    #[test]
    fn subconductor_report_hands_paths_not_payloads() {
        let rows = vec![
            BatchWorkerRow { worker: WorkerId(31), state: WorkerState::Done,
                             evidence: "/var/lib/agentd/events/agents/31.json".into(), timed_out: false, node: None },
            BatchWorkerRow { worker: WorkerId(32), state: WorkerState::Parked,
                             evidence: String::new(), timed_out: true, node: None },
        ];
        let r = subconductor_report(9, &rows);
        assert!(r.contains("BATCH 9 REPORT"));
        assert!(r.contains("worker 31 [Done]: /var/lib/agentd/events/agents/31.json"));
        assert!(r.contains("worker 32 [Parked] (timed_out — still revivable): no evidence file"));
        assert!(r.contains("integrate"), "integration is the instruction, not delivery");
        assert!(r.contains("worker_report"), "the sub-conductor still reports up its own chain");
    }

    #[test]
    fn schedule_lands_deadline_exact_and_backs_off_quiet_words() {
        let step_timeout = Duration::from_secs(900);
        let idle_ttl = Duration::from_secs(1800);
        // A live runner's next review never exceeds the period.
        let mut w = mk(WorkerState::Running);
        let (p, word) = build_review(&w, None, true, 12, step_timeout, idle_ttl);
        schedule_next_review(&mut w, p, &word, step_timeout, idle_ttl, None);
        let until = w.next_review.unwrap() - Instant::now();
        assert!(until <= REVIEW_PERIOD + Duration::from_secs(1));
        // Repeated identical quiet words widen the interval (fib), and the
        // TTL deadline caps it exactly: an idle worker 100s from its TTL is
        // re-reviewed within TICK of the deadline, not a backoff later.
        let mut idle = Worker { started: Instant::now() - (idle_ttl - Duration::from_secs(100)), ..mk(WorkerState::Idle) };
        let (p, word) = build_review(&idle, None, true, 12, step_timeout, idle_ttl);
        for _ in 0..4 { // grow the backoff ladder
            schedule_next_review(&mut idle, p, &word, step_timeout, idle_ttl, None);
        }
        assert!(idle.review_attempt >= 3, "identical words must climb the ladder");
        let until = idle.next_review.unwrap() - Instant::now();
        assert!(until <= Duration::from_secs(100) + TICK + Duration::from_secs(1),
                "deadline-exact scheduling beats the backoff: {until:?}");
        // A fresh terminal gets one last look; a reviewed terminal reaps.
        let mut done = mk(WorkerState::Done);
        let (_, word2) = build_review(&done, None, true, 12, step_timeout, idle_ttl);
        schedule_next_review(&mut done, Posture::Live, &word2, step_timeout, idle_ttl, None);
        assert!(done.next_review.is_some(), "fresh terminal → one Terminal review");
        schedule_next_review(&mut done, Posture::Terminal, &word2, step_timeout, idle_ttl, None);
        assert!(done.next_review.is_none(), "reviewed terminal is reaped — anti-zombie");
    }

    // ── M2: cross-node rings ────────────────────────────────────────────────

    #[test]
    fn remote_cell_veto_truth_table() {
        // All 16 combinations — only a plain ring cell of a repo-less
        // mandala ships out; everything else refuses with its law named.
        for is_gate in [false, true] {
            for measure in [false, true] {
                for voucher in [false, true] {
                    for repo in [false, true] {
                        let v = remote_cell_veto(is_gate, measure, voucher, repo);
                        if !is_gate && !measure && !voucher && !repo {
                            assert!(v.is_none(), "a plain ring cell may go remote");
                        } else {
                            assert!(v.is_some(),
                                "gate={is_gate} measure={measure} voucher={voucher} repo={repo} must refuse");
                        }
                    }
                }
            }
        }
        // The named laws, and repo (the mandala-level law) trumping the rest.
        assert!(remote_cell_veto(true, true, true, true).unwrap().contains("code mandalas"));
        assert!(remote_cell_veto(true, false, false, false).unwrap().contains("gate"));
        assert!(remote_cell_veto(false, true, false, false).unwrap().contains("lap boundary"));
        assert!(remote_cell_veto(false, false, true, false).unwrap().contains("sub-conduction"));
    }

    #[test]
    fn sync_remote_cells_mirrors_terminals_only() {
        let dir = std::env::temp_dir().join(format!("apexos-m2-sync-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mid = 3u64;
        let tree_dir = dir.join(mid.to_string());
        let mut mk_bound = |addr: &str, node: &str, wid: u64| {
            let mut c = cell(addr, "open", None);
            c.node = Some(node.into());
            c.worker = Some(wid);
            mandala::save_cell(&tree_dir, &c);
            c
        };
        let mut tree = HashMap::new();
        for (addr, node, wid) in [("0.0", "apex-3", 31u64), ("0.1", "apex-3", 32), ("0.2", "tvpi", 33)] {
            tree.insert(addr.to_string(), mk_bound(addr, node, wid));
        }
        let mut trees: HashMap<u64, HashMap<String, CellRecord>> = HashMap::new();
        trees.insert(mid, tree);
        let cbw: HashMap<u64, (u64, Addr)> = [
            (31u64, (mid, Addr::parse("0.0").unwrap())),
            (32u64, (mid, Addr::parse("0.1").unwrap())),
            (33u64, (mid, Addr::parse("0.2").unwrap())),
        ].into();
        let mut remotes = HashMap::new();
        remotes.insert(31, RemoteWorker {
            evidence: Some("/var/lib/agentd/events/agents/31.json".into()),
            ..mk_remote(7, "apex-3", "done")
        });
        remotes.insert(32, mk_remote(7, "apex-3", "running"));
        remotes.insert(33, mk_remote(7, "tvpi", "hibernating")); // unknown state, newer peer
        sync_remote_cells(&mut trees, &cbw, &remotes, &dir);
        let t = &trees[&mid];
        assert_eq!(t["0.0"].state, "done");
        assert_eq!(t["0.0"].evidence.as_deref(), Some("/var/lib/agentd/events/agents/31.json"),
                   "the MIRROR path rides the cell — reading it IS integration");
        assert_eq!(t["0.1"].state, "open", "live rows leave the tree untouched");
        assert_eq!(t["0.2"].state, "open",
                   "unknown wire states are non-terminal (skew law) — they can never close a cell");
        // The barrier derivation sees the settled cell drop out of the wait-set.
        let waiting = mandala::open_descendants(t, &Addr::parse("0").unwrap());
        assert_eq!(waiting.iter().map(|a| a.0.as_str()).collect::<Vec<_>>(), vec!["0.1", "0.2"]);
        // The on-disk record followed (the filesystem is the tree).
        let back = mandala::load_tree(&tree_dir);
        assert_eq!(back["0.0"].state, "done");
        assert_eq!(back["0.0"].node.as_deref(), Some("apex-3"), "the body's address survives the round trip");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
