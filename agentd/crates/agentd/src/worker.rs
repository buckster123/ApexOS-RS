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
use apexos_plugins::ToolProxy;
use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, mpsc};

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

/// The worker's reported outcome for the in-flight step (via `worker_report`),
/// applied on `TurnComplete` — `goal.rs`'s Verdict, worker vocabulary.
enum Verdict {
    Continue(Option<String>), // optional steer for the next step
    Done { summary: String, artifacts: Vec<String> }, // summary required; artifacts confined
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
}

fn default_step() -> u32 { 1 }

#[derive(Serialize, Deserialize)]
struct PersistedBatch {
    batch:         u64,
    parent:        u64,
    created_epoch: u64,
    deadline_s:    u64,
    reported:      bool,
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
                      Workers are single-turn in this slice: each delivers its result as the \
                      final text of its one turn.".into(),
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
                                  "model":  { "type": "string", "description": "Model for this one worker (wins over the batch model)." }
                              },
                              "required": ["prompt"] }
                        ]
                    }
                },
                "mode": { "type": "string", "enum": ["async", "inline"],
                          "description": "async (the default): fan and return immediately — the coding path. inline: BLOCK until the batch reports and return the rows as this call's result — short batches only (max 4 tasks, deadline capped at 240s)." },
                "model": { "type": "string",
                          "description": "Model for every worker in the batch (per-task model overrides this). Omit for the node default. The colony-model-mix lever: think on the big model, hammer on the small." },
                "yolo": { "type": "string", "enum": ["inherit"],
                          "description": "inherit: workers auto-approve their OWN ask tools IF AND ONLY IF this calling session is itself yolo-armed (a yolo:true goal) — never more than the parent has. Default off." },
                "batch_deadline_s": { "type": "integer",
                          "description": "Report bound in seconds (default 3600, clamped 60-86400): at the deadline the batch reports with unfinished workers marked timed_out (still revivable) instead of waiting forever." }
            },
            "required": ["tasks"]
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
                "reason":  { "type": "string", "description": "Why you're stuck (status=blocked)." }
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
                      task_fanout{mandala, parent_cell}: each cell is a real worker with an address \
                      (0.2.1 = root→2nd child→1st), a strictly descending budget, and the full \
                      evidence trail. This slice grows depth one cell at a time (parallel rings and \
                      sub-conductors arrive in later slices). Inspect with mandala_status.".into(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "objective":  { "type": "string", "description": "What the whole mandala exists to accomplish." },
                "done_when":  { "type": "string", "description": "The definition of done — concrete, checkable." },
                "verify":     { "type": "string", "description": "THE verify command (e.g. 'cargo test -p x') — every cell at every depth checks against this exact command, run through normal policied tools." },
                "lattice":    { "type": "string", "enum": ["spine", "quad", "fan", "spiral", "funnel"],
                                "description": "The geometry preset (default spine — serial refinement; width caps beyond 1 activate in later slices)." },
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
         paths. Do not spawn agents or fan out further work: workers are depth-1 by design."
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
/// refusal, not a silent default.
fn parse_verdict(args: &serde_json::Value) -> Verdict {
    match args["status"].as_str() {
        Some("done") => Verdict::Done {
            summary: args["summary"].as_str().unwrap_or("").trim().to_string(),
            artifacts: artifact_strings(args),
        },
        Some("blocked") => Verdict::Blocked(args["reason"].as_str().unwrap_or("blocked").to_string()),
        Some("yield")   => Verdict::Yield,
        _               => Verdict::Continue(args["next"].as_str().map(str::to_owned)),
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

/// Extract the tasks from `task_fanout` args — item = string | {prompt, model?}.
/// The per-task model wins over the batch-level `model`. Errors are
/// conductor-facing strings (the tool result), not panics.
fn parse_tasks(args: &serde_json::Value) -> Result<Vec<(String, Option<String>)>, String> {
    let items = args["tasks"].as_array().ok_or("tasks must be an array of task strings or {prompt} objects")?;
    if items.is_empty() { return Err("tasks is empty — nothing to fan out".into()); }
    if items.len() > MAX_BATCH_TASKS {
        return Err(format!("{} tasks exceeds the {MAX_BATCH_TASKS}-per-batch ceiling — split into sequential batches", items.len()));
    }
    let batch_model = args["model"].as_str().map(str::trim).filter(|s| !s.is_empty() && s.len() <= 64);
    let mut tasks = Vec::with_capacity(items.len());
    for (i, item) in items.iter().enumerate() {
        let prompt = item.as_str().or_else(|| item["prompt"].as_str()).unwrap_or("").trim();
        if prompt.is_empty() { return Err(format!("task {} has no prompt", i + 1)); }
        let model = item["model"].as_str().map(str::trim).filter(|s| !s.is_empty() && s.len() <= 64)
            .or(batch_model)
            .map(str::to_owned);
        tasks.push((prompt.to_string(), model));
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
fn next_queued(workers: &HashMap<u64, Worker>) -> Option<u64> {
    workers.iter().filter(|(_, w)| w.state == WorkerState::Queued).map(|(id, _)| *id).min()
}

// ── Persistence (atomic — restarts are a first-class path here) ─────────────

fn save_workers(workers: &HashMap<u64, Worker>, path: &PathBuf) {
    let mut snapshot: Vec<PersistedWorker> = workers.iter().map(|(id, w)| PersistedWorker {
        id: *id, batch: w.batch, parent: w.parent, session: w.session,
        task: w.task.clone(), state: w.state, step: w.step, summary: w.summary.clone(),
        artifacts: w.artifacts.clone(), episode: w.episode.clone(),
        yolo: w.yolo, model: w.model.clone(),
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
) {
    tokio::spawn(async move {
        let mut workers: HashMap<u64, Worker>   = HashMap::new();
        let mut batches: HashMap<u64, BatchMeta> = HashMap::new();
        // Mandala state (M1a): records + per-mandala cell trees + worker→cell
        // binding. THE FILESYSTEM IS THE TREE — these maps are rebuilt from
        // disk at boot; the driver only caches what the tree dirs say.
        let mut mandalas: HashMap<u64, MandalaRecord> = HashMap::new();
        let mut trees: HashMap<u64, HashMap<String, CellRecord>> = HashMap::new();
        let mut cell_by_worker: HashMap<u64, (u64, Addr)> = HashMap::new();
        let mut next_mandala_id: u64 = 1;
        // Driver-private counters. Workers persist, so the reload MUST re-seed
        // all three past what's on disk (the next_goal_id discipline) — never
        // blind-reset like the spawn counter (safe there only because spawns
        // never persist).
        let mut next_worker_id:  u64 = 1;
        let mut next_batch_id:   u64 = 1;
        let mut next_worker_sid: u64 = apexos_core::WORKER_SESSION_BASE;

        reload_workers(&mut workers, &bus, &workers_path,
                       &mut next_worker_id, &mut next_batch_id, &mut next_worker_sid).await;
        reload_batches(&mut batches, &batches_path, &mut next_batch_id);
        reload_mandalas(&mut mandalas, &mut trees, &mut cell_by_worker,
                        &mandalas_path, &worktrees_dir, &mut next_mandala_id);

        // Artifact confinement root: the node agent's workspace (workers run
        // as the node agent — resolve_agent_id on an unbound worker session).
        let workspace = apexos_core::agent_workspace_root(&apexos_core::node_agent_id());

        let step_timeout = step_timeout_from_env();
        let idle_ttl     = idle_ttl_from_env();
        let max_steps    = max_steps_from_env();
        // PB-1 tracking: recent local agent_spawn timestamps per parent session.
        let mut spawn_log: HashMap<u64, Vec<Instant>> = HashMap::new();
        let mut tick = tokio::time::interval(Duration::from_secs(30));
        loop {
            tokio::select! {
                Some((session, call_id, tool, args)) = req_rx.recv() => {
                    match tool.as_str() {
                        "task_fanout" => {
                            // Mandala-scoped fan (M1a): validate the cell FIRST — address,
                            // budget descent, geometry — and hand fanout the invariant-
                            // bearing context; a plain fan passes None straight through.
                            match prepare_mandala_cell(&mandalas, &trees, &bus, session, call_id, &args).await {
                                Err(()) => {} // honest refusal already emitted
                                Ok(ctx) => {
                                    let minted = fanout(&mut workers, &mut batches, &bus, cap, max_steps, &proxy, &yolo_set, &models, session, call_id, args,
                                           &mut next_worker_id, &mut next_batch_id, &mut next_worker_sid, ctx.as_ref()).await;
                                    if let (Some(ctx), Some(wid)) = (ctx, minted) {
                                        bind_cell_worker(&mut trees, &mut cell_by_worker, &worktrees_dir, ctx, wid);
                                    }
                                    save_workers(&workers, &workers_path);
                                    save_batches(&batches, &batches_path);
                                }
                            }
                        }
                        "mandala_create" => {
                            create_mandala(&mut mandalas, &mut trees, &bus, &worktrees_dir, &mut next_mandala_id, session, call_id, args).await;
                            mandala::save_mandalas(&mandalas, &mandalas_path);
                        }
                        "mandala_status" => handle_mandala_status(&mandalas, &trees, &bus, session, call_id).await,
                        "worker_report" => record_report(&mut workers, &bus, &workspace, session, call_id, args).await,
                        "worker_cancel" => {
                            if cancel_request(&mut workers, &bus, &proxy, &agents_dir, &yolo_set, &models, session, call_id, args).await {
                                admit_queued(&mut workers, &bus, &proxy, &yolo_set, &models, cap, max_steps).await;
                                if check_batches(&workers, &mut batches, &bus, &agents_dir).await {
                                    save_batches(&batches, &batches_path);
                                }
                                sync_cells(&mut trees, &cell_by_worker, &workers, &worktrees_dir, &agents_dir);
                                save_workers(&workers, &workers_path);
                            }
                        }
                        "list_workers" => handle_list_workers(&workers, &bus, cap, &agents_dir, session, call_id).await,
                        _ => {}
                    }
                }
                ev = bcast_rx.recv() => {
                    match ev {
                        // A worker's turn completed → apply its reported verdict
                        // (or the no-report fallback: Done, final text = deliverable).
                        Ok(Event::TurnComplete { session }) if apexos_core::is_worker_session(session.0) => {
                            if advance(&mut workers, &bus, &proxy, &agents_dir, &yolo_set, &models, session.0, max_steps).await {
                                admit_queued(&mut workers, &bus, &proxy, &yolo_set, &models, cap, max_steps).await;
                                if check_batches(&workers, &mut batches, &bus, &agents_dir).await {
                                    save_batches(&batches, &batches_path);
                                }
                                sync_cells(&mut trees, &cell_by_worker, &workers, &worktrees_dir, &agents_dir);
                                save_workers(&workers, &workers_path);
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
                        Ok(Event::GoalStateChanged { state: GoalState::Cancelled, session: Some(parent), .. }) => {
                            let ids: Vec<u64> = workers.iter()
                                .filter(|(_, w)| w.parent == parent.0 && !is_terminal(w.state))
                                .map(|(id, _)| *id).collect();
                            if !ids.is_empty() {
                                eprintln!("[worker] cascade: conductor session {} cancelled → {} worker(s)", parent.0, ids.len());
                                cancel_workers(&mut workers, &bus, &proxy, &agents_dir, &yolo_set, &models, &ids).await;
                                admit_queued(&mut workers, &bus, &proxy, &yolo_set, &models, cap, max_steps).await;
                                if check_batches(&workers, &mut batches, &bus, &agents_dir).await {
                                    save_batches(&batches, &batches_path);
                                }
                                sync_cells(&mut trees, &cell_by_worker, &workers, &worktrees_dir, &agents_dir);
                                save_workers(&workers, &workers_path);
                            }
                        }
                        // PB-1 soft breaker: sequential local spawns from one
                        // session — nudge toward ONE task_fanout batch.
                        Ok(Event::SubAgentStarted { parent, .. }) => {
                            pb1_track(&mut spawn_log, &bus, parent.0).await;
                        }
                        _ => {}
                    }
                }
                _ = tick.tick() => {
                    let stalled = fail_stalled(&mut workers, &bus, &proxy, &agents_dir, &yolo_set, &models, step_timeout).await;
                    let parked  = park_idle(&mut workers, &bus, &yolo_set, &models, idle_ttl).await;
                    if stalled { admit_queued(&mut workers, &bus, &proxy, &yolo_set, &models, cap, max_steps).await; }
                    if check_batches(&workers, &mut batches, &bus, &agents_dir).await {
                        save_batches(&batches, &batches_path);
                    }
                    if stalled { sync_cells(&mut trees, &cell_by_worker, &workers, &worktrees_dir, &agents_dir); }
                    if stalled || parked { save_workers(&workers, &workers_path); }
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

/// task_fanout: mint the batch, ack the conductor with the ids, then admit up
/// to the cap. Refused from a worker session — workers are depth-1 (vouchers
/// are the M-tier mechanism; there is no partial fan below the conductor).
#[allow(clippy::too_many_arguments)]
async fn fanout(
    workers: &mut HashMap<u64, Worker>, batches: &mut HashMap<u64, BatchMeta>,
    bus: &BusHandle, cap: usize, max_steps: u32, proxy: &ToolProxy,
    yolo_set: &GoalYoloSessions, models: &WorkerModels,
    call_session: SessionId, call_id: ActionId, args: serde_json::Value,
    next_worker_id: &mut u64, next_batch_id: &mut u64, next_worker_sid: &mut u64,
    cell: Option<&CellCtx>,
) -> Option<u64> {
    let refuse = |msg: String| Event::ToolResult {
        session: call_session, call: call_id,
        output: ToolOutput { ok: false, content: serde_json::json!(msg) },
    };
    if apexos_core::is_worker_session(call_session.0) {
        bus.emit(refuse("workers cannot fan out further work (depth-1 by design; sub-conducting arrives with M-tier vouchers)".into())).await;
        return None;
    }
    let inline = match args["mode"].as_str() {
        None | Some("async") => false,
        Some("inline") => true,
        Some(other) => {
            bus.emit(refuse(format!("unknown mode \"{other}\" — async (default) or inline"))).await;
            return None;
        }
    };
    let mut tasks = match parse_tasks(&args) {
        Ok(t) => t,
        Err(e) => { bus.emit(refuse(e)).await; return None; }
    };
    if inline && tasks.len() > INLINE_MAX_TASKS {
        bus.emit(refuse(format!(
            "inline mode is short-batch-only ({} tasks > {INLINE_MAX_TASKS}) — use mode async; the batch report arrives via the AwaitingBatch loop", tasks.len()))).await;
        return None;
    }
    // Mandala cell (M1a): depth grows one cell at a time — zero new
    // concurrency until the B bit arms (M1b). The invariant axis rides the
    // task verbatim, injected here mechanically, never model-relayed.
    if let Some(ctx) = cell {
        if tasks.len() != 1 {
            bus.emit(refuse(format!(
                "a mandala cell is one task ({} given) — parallel rings arm in M1b; grow depth cell by cell", tasks.len()))).await;
            return None;
        }
        let (prompt, model) = tasks.remove(0);
        tasks.push((format!("{}

CELL {} — your task within the mandala:
{}", ctx.directive_prefix, ctx.addr.0, prompt), model));
    }

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
    if let Some(ctx) = cell { deadline_s = deadline_s.min(ctx.budget.deadline_s); }
    batches.insert(batch, BatchMeta {
        parent: call_session.0, created_epoch: epoch_now(), deadline_s, reported: false,
        inline_ack: if inline { Some((call_session.0, call_id.0)) } else { None },
    });
    let mut minted: Vec<(u64, u64)> = Vec::with_capacity(tasks.len()); // (worker_id, session)
    for (task, model) in tasks {
        let wid = *next_worker_id;  *next_worker_id  += 1;
        let sid = *next_worker_sid; *next_worker_sid += 1;
        workers.insert(wid, Worker {
            batch, parent: call_session.0, session: sid,
            task, state: WorkerState::Queued, step: 1, summary: None,
            artifacts: Vec::new(), episode: None,
            started: Instant::now(), pending: None, turn_inflight: false,
            yolo, model, errored: false,
        });
        minted.push((wid, sid));
    }

    // Cards BEFORE the ack: bus order is delivery order, so the goal driver's
    // batch tracking (WorkerStateChanged → pending set) is provably armed
    // before the conductor's turn can resume off the ack and complete.
    for (wid, _) in &minted {
        let w = &workers[wid];
        emit_state(bus, *wid, w, "queued").await;
    }

    let n = minted.len();
    let admitted_now = (cap.saturating_sub(slots_used(workers))).min(n);
    if !inline {
        // Async: ack now, work proceeds in background. Inline holds the ack —
        // the batch report IS this call's result (emitted by check_batches).
        bus.emit(Event::ToolResult {
            session: call_session, call: call_id,
            output: ToolOutput { ok: true, content: serde_json::json!({
                "batch": batch,
                "workers": minted.iter().map(|(w, s)| serde_json::json!({ "worker": w, "session": s })).collect::<Vec<_>>(),
                "count": n, "cap": cap,
                "admitted": admitted_now, "queued": n - admitted_now,
                "batch_deadline_s": deadline_s,
                "yolo_inherited": yolo,
                "status": "fanned",
            }) },
        }).await;
    }

    admit_queued(workers, bus, proxy, yolo_set, models, cap, max_steps).await;
    eprintln!("[worker] batch {batch} fanned: {n} tasks from session {} (cap {cap}, deadline {deadline_s}s{}{}{})",
              call_session.0,
              if inline { ", inline" } else { "" },
              if yolo { ", yolo:inherit" } else { "" },
              if let Some(c) = cell { format!(", cell {}", c.addr.0) } else { String::new() });
    minted.first().map(|(wid, _)| *wid)
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
async fn advance(workers: &mut HashMap<u64, Worker>, bus: &BusHandle, proxy: &ToolProxy, agents_dir: &Path, yolo_set: &GoalYoloSessions, models: &WorkerModels, session: u64, max_steps: u32) -> bool {
    let Some(id) = workers.iter()
        .find(|(_, w)| w.session == session && matches!(w.state, WorkerState::Running | WorkerState::Blocked))
        .map(|(id, _)| *id)
    else { return false };

    // Mutate first (plain owned map — no lock-release dance needed), emit after.
    let (detail, next_directive) = {
        let w = workers.get_mut(&id).unwrap();
        w.turn_inflight = false;
        match w.pending.take() {
            Some(Verdict::Done { summary, artifacts }) => {
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
            Some(Verdict::Continue(steer)) => {
                if w.step >= max_steps {
                    w.state = WorkerState::Done; // budget reached — code disposes
                    ("step budget reached".to_string(), None)
                } else {
                    w.step += 1;
                    w.started = Instant::now();
                    w.turn_inflight = true;
                    let d = directive_continue(id, w.batch, w.step, max_steps, &w.task, steer.as_deref());
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
    if let Some((sid, directive)) = next_directive {
        bus.emit(Event::UserPrompt { session: SessionId(sid), text: directive, images: vec![] }).await;
    }
    true
}

/// A send landed on a worker session — the one revive/wake edge (PB-3):
/// Parked → Running (the router hydrated the history off this same event),
/// Idle → Running (wake free), verdict-Blocked → Running (unblocked by input).
/// Deliberately BYPASSES the admission cap — a send is human/conductor intent,
/// the emergency entrance; Queued workers just wait a little longer. Running /
/// approval-Blocked sends simply queue in the TurnGate (no state change);
/// Queued and terminal workers are left untouched.
async fn wake_on_send(workers: &mut HashMap<u64, Worker>, bus: &BusHandle, models: &WorkerModels, session: u64) -> bool {
    let hit = workers.iter_mut()
        .find(|(_, w)| w.session == session
            && (matches!(w.state, WorkerState::Parked | WorkerState::Idle)
                || (w.state == WorkerState::Blocked && !w.turn_inflight)))
        .map(|(id, w)| {
            let from = w.state;
            w.state = WorkerState::Running;
            w.started = Instant::now();
            w.turn_inflight = true;
            w.errored = false;
            (*id, from)
        });
    if let Some((id, from)) = hit {
        let w = &workers[&id];
        // Re-arm the model pin (it follows residency); NEVER re-arm inherited
        // yolo on revive — the parent's grant does not outlive the park.
        if let Some(m) = &w.model {
            if let Ok(mut mm) = models.lock() { mm.insert(w.session, m.clone()); }
        }
        let detail = match from {
            WorkerState::Parked => "revived by send",
            WorkerState::Idle   => "woken by send",
            _                   => "unblocked by send",
        };
        emit_state(bus, id, w, detail).await;
        eprintln!("[worker] {id} {detail}");
        true
    } else { false }
}

/// Park Idle / verdict-blocked workers that have sat past the idle TTL: the
/// state event carries the eviction (the router drops the RAM history);
/// `sessions/<id>.jsonl` stays truth and a send revives. No slot changes —
/// these states hold none.
async fn park_idle(workers: &mut HashMap<u64, Worker>, bus: &BusHandle, yolo_set: &GoalYoloSessions, models: &WorkerModels, idle_ttl: Duration) -> bool {
    let parked: Vec<u64> = workers.iter_mut()
        .filter(|(_, w)| matches!(w.state, WorkerState::Idle)
            || (w.state == WorkerState::Blocked && !w.turn_inflight))
        .filter(|(_, w)| w.started.elapsed() > idle_ttl)
        .map(|(id, w)| { w.state = WorkerState::Parked; *id })
        .collect();
    let changed = !parked.is_empty();
    for id in parked {
        let w = &workers[&id];
        disarm_worker(yolo_set, models, w.session); // arming follows residency
        emit_state(bus, id, w, "idle TTL — parked (a send revives)").await;
        eprintln!("[worker] {id} parked (idle TTL)");
    }
    changed
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

/// Fail any Running worker whose turn has stalled past the timeout (errored/
/// aborted turns emit no TurnComplete). Blocked is exempt — an approval wait
/// runs on the human's clock, not the stall clock.
async fn fail_stalled(workers: &mut HashMap<u64, Worker>, bus: &BusHandle, proxy: &ToolProxy, agents_dir: &Path, yolo_set: &GoalYoloSessions, models: &WorkerModels, step_timeout: Duration) -> bool {
    let stalled: Vec<u64> = workers.iter_mut()
        .filter(|(_, w)| w.state == WorkerState::Running && w.started.elapsed() > step_timeout)
        .map(|(id, w)| { w.state = WorkerState::Failed; w.turn_inflight = false; *id })
        .collect();
    let changed = !stalled.is_empty();
    for id in stalled {
        let w = &workers[&id];
        disarm_worker(yolo_set, models, w.session);
        emit_state(bus, id, w, "step stalled — no completion").await;
        eprintln!("[worker] {id} failed (stalled > {}s)", step_timeout.as_secs());
        finalize_terminal(proxy, agents_dir, id, w).await;
    }
    changed
}

/// Batch bookkeeping: an unreported batch reports when every worker is
/// terminal, or when its deadline passes — stragglers ride the report marked
/// `timed_out` (still revivable; a later revive finishes them outside it).
/// Rows carry evidence PATHS, never payloads. One report per batch.
async fn check_batches(workers: &HashMap<u64, Worker>, batches: &mut HashMap<u64, BatchMeta>, bus: &BusHandle, agents_dir: &Path) -> bool {
    // (inline note: the held task_fanout ack is emitted AFTER TaskBatchDone —
    // bus order guarantees the goal driver's pending-batch set clears before
    // the blocked conductor turn can complete.)
    let now = epoch_now();
    let due: Vec<u64> = batches.iter()
        .filter(|(_, b)| !b.reported)
        .filter(|(id, b)| {
            let members: Vec<&Worker> = workers.values().filter(|w| w.batch == **id).collect();
            let all_terminal = !members.is_empty() && members.iter().all(|w| is_terminal(w.state));
            let expired = now >= b.created_epoch.saturating_add(b.deadline_s);
            all_terminal || expired
        })
        .map(|(id, _)| *id)
        .collect();
    let changed = !due.is_empty();
    for batch in due {
        let meta = batches.get_mut(&batch).unwrap();
        meta.reported = true;
        let parent = meta.parent;
        let inline_ack = meta.inline_ack.take();
        let rows = batch_rows(workers, batch, agents_dir);
        let (done, failed, timed_out) = rows.iter().fold((0, 0, 0), |(d, f, t), r| match () {
            _ if r.timed_out => (d, f, t + 1),
            _ if r.state == WorkerState::Done => (d + 1, f, t),
            _ => (d, f + 1, t),
        });
        eprintln!("[worker] batch {batch} reported: {done} done, {failed} failed, {timed_out} timed out");
        bus.emit(Event::TaskBatchDone { batch, parent: SessionId(parent), rows: rows.clone() }).await;
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
                    "done": done, "failed": failed, "timed_out": timed_out,
                    "workers": inline_rows,
                }) },
            }).await;
        }
    }
    changed
}

fn is_terminal(state: WorkerState) -> bool {
    matches!(state, WorkerState::Done | WorkerState::Failed | WorkerState::Cancelled)
}

/// Build a batch's report rows — pure over the worker map (unit-tested).
fn batch_rows(workers: &HashMap<u64, Worker>, batch: u64, agents_dir: &Path) -> Vec<BatchWorkerRow> {
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
            })
        })
        .collect();
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
#[allow(clippy::too_many_arguments)]
async fn cancel_request(workers: &mut HashMap<u64, Worker>, bus: &BusHandle, proxy: &ToolProxy, agents_dir: &Path, yolo_set: &GoalYoloSessions, models: &WorkerModels, call_session: SessionId, call_id: ActionId, args: serde_json::Value) -> bool {
    let ids: Vec<u64> = match (args["worker"].as_u64(), args["batch"].as_u64()) {
        (Some(w), None) => workers.get(&w).filter(|wk| !is_terminal(wk.state)).map(|_| vec![w]).unwrap_or_default(),
        (None, Some(b)) => workers.iter().filter(|(_, wk)| wk.batch == b && !is_terminal(wk.state)).map(|(id, _)| *id).collect(),
        _ => {
            bus.emit(Event::ToolResult { session: call_session, call: call_id,
                output: ToolOutput { ok: false, content: serde_json::json!(
                    "pass exactly one of worker (an id) or batch (cancels every non-terminal worker in it)") } }).await;
            return false;
        }
    };
    if ids.is_empty() {
        bus.emit(Event::ToolResult { session: call_session, call: call_id,
            output: ToolOutput { ok: false, content: serde_json::json!("no matching non-terminal worker(s)") } }).await;
        return false;
    }
    cancel_workers(workers, bus, proxy, agents_dir, yolo_set, models, &ids).await;
    bus.emit(Event::ToolResult { session: call_session, call: call_id,
        output: ToolOutput { ok: true, content: serde_json::json!({ "cancelled": ids, "count": ids.len() }) } }).await;
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
async fn handle_list_workers(workers: &HashMap<u64, Worker>, bus: &BusHandle, cap: usize, agents_dir: &Path, call_session: SessionId, call_id: ActionId) {
    let mut rows: Vec<(u64, serde_json::Value)> = workers.iter().map(|(id, w)| (*id, serde_json::json!({
        "worker": id, "batch": w.batch, "parent": w.parent, "session": w.session,
        "state": format!("{:?}", w.state).to_lowercase(),
        "step": w.step,
        "task": w.task.chars().take(100).collect::<String>(),
        "summary": w.summary.as_deref().map(|s| s.chars().take(200).collect::<String>()),
        "evidence": if is_terminal(w.state) {
            serde_json::json!(evidence_path(agents_dir, *id).to_string_lossy())
        } else { serde_json::Value::Null },
    }))).collect();
    rows.sort_by_key(|(id, _)| *id);
    let list: Vec<serde_json::Value> = rows.into_iter().map(|(_, j)| j).collect();
    bus.emit(Event::ToolResult { session: call_session, call: call_id,
        output: ToolOutput { ok: true, content: serde_json::json!({
            "workers": list, "count": list.len(),
            "cap": cap, "slots_used": slots_used(workers),
        }) } }).await;
}

// ── Mandala runtime (M1a) — the driver's side of the tree ───────────────────

/// A validated cell-spawn context: everything fanout needs to mint the cell's
/// worker, computed and refused-early by prepare_mandala_cell.
pub(crate) struct CellCtx {
    mandala: u64,
    addr: Addr,
    budget: BudgetVec,
    invariant_hash: String,
    directive_prefix: String,
}

/// Validate a mandala-scoped task_fanout: known mandala, live parent cell,
/// geometry budget (open cells), ring width, and the budget-descent law.
/// Ok(None) = a plain (non-mandala) fan; Err(()) = refused, ToolResult sent.
async fn prepare_mandala_cell(
    mandalas: &HashMap<u64, MandalaRecord>,
    trees: &HashMap<u64, HashMap<String, CellRecord>>,
    bus: &BusHandle,
    call_session: SessionId, call_id: ActionId, args: &serde_json::Value,
) -> Result<Option<CellCtx>, ()> {
    let Some(mid) = args["mandala"].as_u64() else { return Ok(None) };
    let refuse = |msg: String| Event::ToolResult {
        session: call_session, call: call_id,
        output: ToolOutput { ok: false, content: serde_json::json!(msg) },
    };
    let Some(m) = mandalas.get(&mid) else {
        bus.emit(refuse(format!("no mandala {mid} — mandala_create first, or mandala_status to list"))).await;
        return Err(());
    };
    if m.conductor != call_session.0 {
        bus.emit(refuse(format!("mandala {mid} belongs to session {} — only its conductor grows it (vouchers arrive in M1c)", m.conductor))).await;
        return Err(());
    }
    let tree = trees.get(&mid).cloned().unwrap_or_default();
    let parent_addr = match args["parent_cell"].as_str() {
        None => Addr(Addr::ROOT.into()),
        Some(s) => match Addr::parse(s) {
            Some(a) => a,
            None => { bus.emit(refuse(format!("'{s}' is not a cell address (like 0 or 0.2.1)"))).await; return Err(()); }
        },
    };
    let Some(parent) = tree.get(&parent_addr.0) else {
        bus.emit(refuse(format!("mandala {mid} has no cell {} — mandala_status shows the tree", parent_addr.0))).await;
        return Err(());
    };
    // Geometry: open cells vs the mandala's conserved budget.
    let open = mandala::open_cells(&tree);
    if open >= m.budget.cells as usize {
        bus.emit(refuse(format!(
            "geometry budget exhausted: {open} open cells of {} — integrate or cancel before growing", m.budget.cells))).await;
        return Err(());
    }
    // Ring width: the child ring's preset cap (M1a spawns serially, but the
    // lattice's width law already bounds how many siblings may EXIST).
    let ring = parent_addr.depth(); // children of depth-d sit in ring d
    let width = mandala::ring_width(m.lattice, ring);
    let ordinal = mandala::next_child_ordinal(&tree, &parent_addr);
    if width == 0 || ordinal >= width as u32 {
        bus.emit(refuse(format!(
            "ring {ring} of the {:?} lattice is full (width {width}) — go deeper or pick another parent", m.lattice))).await;
        return Err(());
    }
    // The descent law.
    let child_budget = mandala::contract_child(&parent.budget);
    if !mandala::admissible(&parent.budget, &child_budget, width - ordinal as u8) {
        bus.emit(refuse(format!(
            "budget descent refused at cell {}: parent depth {} steps {} — the vector must strictly descend and stay positive",
            parent_addr.0, parent.budget.depth, parent.budget.steps))).await;
        return Err(());
    }
    Ok(Some(CellCtx {
        mandala: mid,
        addr: parent_addr.child(ordinal),
        budget: child_budget,
        invariant_hash: m.invariant.hash.clone(),
        directive_prefix: m.invariant.directive_block(),
    }))
}

/// The minted cell binds to its worker: the cell file lands in the tree
/// (position IS identity) and the runtime index follows.
fn bind_cell_worker(
    trees: &mut HashMap<u64, HashMap<String, CellRecord>>,
    cell_by_worker: &mut HashMap<u64, (u64, Addr)>,
    worktrees_dir: &Path,
    ctx: CellCtx, worker_id: u64,
) {
    let cell = CellRecord {
        addr: ctx.addr.clone(),
        form: CellForm::SPINE,
        task: String::new(), // the worker record holds the full task; the tree holds structure
        budget: ctx.budget,
        invariant_hash: ctx.invariant_hash,
        worker: Some(worker_id),
        state: "open".into(),
        evidence: None,
        reparented_to: None,
    };
    let tree_dir = worktrees_dir.join(ctx.mandala.to_string());
    mandala::save_cell(&tree_dir, &cell);
    cell_by_worker.insert(worker_id, (ctx.mandala, ctx.addr.clone()));
    trees.entry(ctx.mandala).or_default().insert(cell.addr.0.clone(), cell);
    eprintln!("[mandala] {} grew cell {} (worker {worker_id})", ctx.mandala, ctx.addr.0);
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

/// mandala_create: write the invariant ONCE (content-addressed), mint the
/// root cell, persist. The conductor session owns the mandala.
#[allow(clippy::too_many_arguments)]
async fn create_mandala(
    mandalas: &mut HashMap<u64, MandalaRecord>,
    trees: &mut HashMap<u64, HashMap<String, CellRecord>>,
    bus: &BusHandle, worktrees_dir: &Path, next_mandala_id: &mut u64,
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
    };
    mandala::save_cell(&worktrees_dir.join(id.to_string()), &root);
    trees.entry(id).or_default().insert(root.addr.0.clone(), root);
    let hash = invariant.hash.clone();
    mandalas.insert(id, MandalaRecord {
        id, conductor: call_session.0, lattice, budget, invariant, state: "open".into(),
    });
    bus.emit(Event::ToolResult { session: call_session, call: call_id,
        output: ToolOutput { ok: true, content: serde_json::json!({
            "mandala": id, "root": Addr::ROOT, "lattice": format!("{lattice:?}").to_lowercase(),
            "invariant_hash": hash,
            "budget": { "depth": budget.depth, "cells": budget.cells, "steps": budget.steps, "deadline_s": budget.deadline_s },
            "status": "open",
            "next": "grow the tree with task_fanout{mandala, parent_cell?, tasks:[one task]}",
        }) } }).await;
    eprintln!("[mandala] {id} opened by session {} ({lattice:?}, depth {})", call_session.0, budget.depth);
}

/// mandala_status: the tree as data — addresses, states, workers, evidence.
async fn handle_mandala_status(
    mandalas: &HashMap<u64, MandalaRecord>,
    trees: &HashMap<u64, HashMap<String, CellRecord>>,
    bus: &BusHandle, call_session: SessionId, call_id: ActionId,
) {
    let render = |m: &MandalaRecord| {
        let tree = trees.get(&m.id).cloned().unwrap_or_default();
        let mut addrs: Vec<&String> = tree.keys().collect();
        addrs.sort();
        serde_json::json!({
            "mandala": m.id, "conductor": m.conductor,
            "lattice": format!("{:?}", m.lattice).to_lowercase(),
            "state": m.state,
            "invariant_hash": m.invariant.hash,
            "objective": m.invariant.objective,
            "open_cells": mandala::open_cells(&tree),
            "cells_budget": m.budget.cells,
            "cells": addrs.iter().map(|a| {
                let c = &tree[a.as_str()];
                serde_json::json!({
                    "addr": c.addr.0, "state": c.state, "worker": c.worker,
                    "depth_left": c.budget.depth, "steps_left": c.budget.steps,
                    "evidence": c.evidence,
                    "reparented_to": c.reparented_to.as_ref().map(|r| r.0.clone()),
                })
            }).collect::<Vec<_>>(),
        })
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
        assert_eq!(tasks[0], ("write a haiku".to_string(), Some("claude-sonnet-5".to_string())));
        // Per-task model wins over the batch default.
        assert_eq!(tasks[1], ("count to three".to_string(), Some("claude-haiku-4-5".to_string())));
        // No models anywhere → node default (None).
        let bare = parse_tasks(&serde_json::json!({ "tasks": ["x"] })).unwrap();
        assert_eq!(bare[0], ("x".to_string(), None));
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
                 yolo: false, model: None, errored: false }
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
            yolo: true, model: Some("claude-haiku-4-5".into()),
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
    }

    #[test]
    fn charter_and_directives_carry_the_contract() {
        let sys = worker_system("APEX");
        assert!(sys.contains("worker_report"), "the verdict tool must ride the charter");
        assert!(sys.contains("depth-1"), "the no-fanout law must ride the charter");
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
    fn parse_verdict_maps_status() {
        assert!(matches!(parse_verdict(&serde_json::json!({"status":"done","summary":"shipped","artifacts":["a.md"]})),
                         Verdict::Done { summary, artifacts } if summary == "shipped" && artifacts == vec!["a.md"]));
        assert!(matches!(parse_verdict(&serde_json::json!({"status":"blocked","reason":"no key"})),
                         Verdict::Blocked(r) if r == "no key"));
        assert!(matches!(parse_verdict(&serde_json::json!({"status":"yield"})), Verdict::Yield));
        assert!(matches!(parse_verdict(&serde_json::json!({"status":"continue","next":"tests"})),
                         Verdict::Continue(Some(s)) if s == "tests"));
        // Absent/unknown status defaults to continue (the goal_step convention).
        assert!(matches!(parse_verdict(&serde_json::json!({})), Verdict::Continue(None)));
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
        let rows = batch_rows(&m, 7, agents);
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].worker, WorkerId(1));
        assert!(rows[0].evidence.ends_with("agents/1.json"));
        assert!(!rows[0].timed_out);
        assert!(rows[1].evidence.ends_with("agents/2.json"));
        assert!(rows[2].timed_out, "non-terminal at report time = timed_out");
        assert!(rows[2].evidence.is_empty(), "no evidence file for a straggler");
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
}
