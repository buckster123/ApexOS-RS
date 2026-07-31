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
//! W1a workers are SINGLE-TURN: the charter says the final text IS the work
//! product, and the turn's completion is the verdict (`TurnComplete` → Done).
//! `worker_report` verdicts (continue/yield/blocked) and multi-step loops land
//! in W1b; artifacts + Cerebro episodes + `TaskBatchDone` land in W1c.
//!
//! Deliberate departure from goal.rs: the worker map is a PLAIN `HashMap`
//! owned by the driver task — no `Arc<Mutex<…>>`. Every access is serialized
//! through the one select loop (true for goals too — their Mutex is never
//! contended), so the lock added shape without safety. Anything outside the
//! driver that later needs worker state (a board REST endpoint, W1c batch
//! reports) goes through the request channel, the house seam.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use apexos_core::{ActionId, BusHandle, Event, SessionId, ToolOutput, ToolSpec, WorkerId, WorkerState};
use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, mpsc};

/// Hard ceiling on tasks per `task_fanout` call — one batch is one conductor
/// thought, not a job queue (PRD open question 6: v1 takes a stance).
const MAX_BATCH_TASKS: usize = 32;

/// A worker whose admitted turn produces no `TurnComplete` within this window
/// is treated as stalled (turn errored/aborted → no completion event) → Failed.
/// Override via `WORKER_STEP_TIMEOUT_SECS` (30s floor), mirroring the goal knob.
const STEP_TIMEOUT: Duration = Duration::from_secs(900);

struct Worker {
    batch:   u64,
    parent:  u64,   // conductor session that fanned this worker out
    session: u64,   // dedicated session in the WORKER_SESSION_BASE range (persisted)
    task:    String,
    state:   WorkerState,
    started: Instant, // stall clock — re-armed on admission and on approval resolution
}

/// The on-disk form (transient `started` dropped). New fields added by later
/// slices MUST carry `#[serde(default)]` so an old workers.json still loads
/// (the PersistedGoal discipline).
#[derive(Serialize, Deserialize)]
struct PersistedWorker {
    id:      u64,
    batch:   u64,
    parent:  u64,
    session: u64,
    task:    String,
    state:   WorkerState,
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
                              "properties": { "prompt": { "type": "string", "description": "The task." } },
                              "required": ["prompt"] }
                        ]
                    }
                },
                "mode": { "type": "string", "enum": ["async"],
                          "description": "Batch mode. async (the default) is the only mode in this slice; inline lands later." }
            },
            "required": ["tasks"]
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
         with the minimum tools required. Your final text IS the work product — the conductor \
         reads it, so make it the complete deliverable (or a precise account of what you did and \
         where the results live), not a status update. Skip orientation: no memory recall, inbox \
         checks, or self-inspection unless the task itself asks for them. Approval-gated tools \
         still ask a human — prefer ungated paths. Do not spawn agents or fan out further work: \
         workers are depth-1 by design."
    )
}

/// The single-turn work order (W1a). Per-worker text rides here, never in the
/// shared system charter, so the system prefix stays identical across a batch.
fn directive(worker_id: u64, batch: u64, task: &str) -> String {
    format!(
        "WORKER {worker_id} (batch {batch}) — one task, one turn.\n\nTASK:\n{task}\n\n\
         Complete the task NOW, in this single turn. Your final text is the deliverable."
    )
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

/// Extract the task prompts from `task_fanout` args — item = string | {prompt}.
/// Errors are conductor-facing strings (the tool result), not panics.
fn parse_tasks(args: &serde_json::Value) -> Result<Vec<String>, String> {
    let items = args["tasks"].as_array().ok_or("tasks must be an array of task strings or {prompt} objects")?;
    if items.is_empty() { return Err("tasks is empty — nothing to fan out".into()); }
    if items.len() > MAX_BATCH_TASKS {
        return Err(format!("{} tasks exceeds the {MAX_BATCH_TASKS}-per-batch ceiling — split into sequential batches", items.len()));
    }
    let mut tasks = Vec::with_capacity(items.len());
    for (i, item) in items.iter().enumerate() {
        let prompt = item.as_str().or_else(|| item["prompt"].as_str()).unwrap_or("").trim();
        if prompt.is_empty() { return Err(format!("task {} has no prompt", i + 1)); }
        tasks.push(prompt.to_string());
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

/// A worker occupies a thermal slot while Running or Blocked (mid-turn either
/// way — an approval-parked turn is still resident). Parked/Queued/terminal
/// hold no slot.
fn holds_slot(state: WorkerState) -> bool {
    matches!(state, WorkerState::Running | WorkerState::Blocked)
}

fn slots_used(workers: &HashMap<u64, Worker>) -> usize {
    workers.values().filter(|w| holds_slot(w.state)).count()
}

/// The FIFO: the lowest-id Queued worker is next (ids are mint-ordered).
fn next_queued(workers: &HashMap<u64, Worker>) -> Option<u64> {
    workers.iter().filter(|(_, w)| w.state == WorkerState::Queued).map(|(id, _)| *id).min()
}

// ── Persistence (atomic — restarts are a first-class path here) ─────────────

fn save_workers(workers: &HashMap<u64, Worker>, path: &PathBuf) {
    let mut snapshot: Vec<PersistedWorker> = workers.iter().map(|(id, w)| PersistedWorker {
        id: *id, batch: w.batch, parent: w.parent, session: w.session,
        task: w.task.clone(), state: w.state,
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
    }).await;
}

// ── The driver ───────────────────────────────────────────────────────────────

/// Spawn the worker driver: `task_fanout`/`list_workers` arrive on `req_rx`
/// (supervisor-routed, deferred ack), worker turns complete via the bus
/// subscription, stalls fail on a 30s tick. Owns every counter and the map —
/// nothing else touches worker state.
pub fn spawn_worker_driver(
    bus:          BusHandle,
    mut bcast_rx: broadcast::Receiver<Event>,
    mut req_rx:   mpsc::Receiver<(SessionId, ActionId, String, serde_json::Value)>,
    workers_path: PathBuf,
    cap:          usize,
) {
    tokio::spawn(async move {
        let mut workers: HashMap<u64, Worker> = HashMap::new();
        // Driver-private counters. Workers persist, so the reload MUST re-seed
        // all three past what's on disk (the next_goal_id discipline) — never
        // blind-reset like the spawn counter (safe there only because spawns
        // never persist).
        let mut next_worker_id:  u64 = 1;
        let mut next_batch_id:   u64 = 1;
        let mut next_worker_sid: u64 = apexos_core::WORKER_SESSION_BASE;

        reload_workers(&mut workers, &bus, &workers_path,
                       &mut next_worker_id, &mut next_batch_id, &mut next_worker_sid).await;

        let step_timeout = step_timeout_from_env();
        let mut tick = tokio::time::interval(Duration::from_secs(30));
        loop {
            tokio::select! {
                Some((session, call_id, tool, args)) = req_rx.recv() => {
                    match tool.as_str() {
                        "task_fanout" => {
                            fanout(&mut workers, &bus, cap, session, call_id, args,
                                   &mut next_worker_id, &mut next_batch_id, &mut next_worker_sid).await;
                            save_workers(&workers, &workers_path);
                        }
                        "list_workers" => handle_list_workers(&workers, &bus, cap, session, call_id).await,
                        _ => {}
                    }
                }
                ev = bcast_rx.recv() => {
                    match ev {
                        Ok(Event::TurnComplete { session }) if apexos_core::is_worker_session(session.0) => {
                            if finish_done(&mut workers, &bus, session.0).await {
                                admit_queued(&mut workers, &bus, cap).await;
                                save_workers(&workers, &workers_path);
                            }
                        }
                        // A worker's turn hit an ask-gated tool: the turn is suspended on
                        // the approval, NOT dead — a human can grant it from the board and
                        // the turn proceeds. Mark Blocked (stall-exempt) so the lane tells
                        // the truth; the slot stays held (the turn is still resident).
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
                        _ => {}
                    }
                }
                _ = tick.tick() => {
                    if fail_stalled(&mut workers, &bus, step_timeout).await {
                        admit_queued(&mut workers, &bus, cap).await;
                        save_workers(&workers, &workers_path);
                    }
                }
            }
        }
    });
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
            task: pw.task, state, started: Instant::now(),
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
    workers: &mut HashMap<u64, Worker>, bus: &BusHandle, cap: usize,
    call_session: SessionId, call_id: ActionId, args: serde_json::Value,
    next_worker_id: &mut u64, next_batch_id: &mut u64, next_worker_sid: &mut u64,
) {
    let refuse = |msg: String| Event::ToolResult {
        session: call_session, call: call_id,
        output: ToolOutput { ok: false, content: serde_json::json!(msg) },
    };
    if apexos_core::is_worker_session(call_session.0) {
        bus.emit(refuse("workers cannot fan out further work (depth-1 by design; sub-conducting arrives with M-tier vouchers)".into())).await;
        return;
    }
    match args["mode"].as_str() {
        None | Some("async") => {}
        Some(other) => {
            bus.emit(refuse(format!("mode \"{other}\" is not available in this slice — only \"async\" (inline lands in W1d)"))).await;
            return;
        }
    }
    let tasks = match parse_tasks(&args) {
        Ok(t) => t,
        Err(e) => { bus.emit(refuse(e)).await; return; }
    };

    let batch = *next_batch_id;
    *next_batch_id += 1;
    let mut minted: Vec<(u64, u64)> = Vec::with_capacity(tasks.len()); // (worker_id, session)
    for task in tasks {
        let wid = *next_worker_id;  *next_worker_id  += 1;
        let sid = *next_worker_sid; *next_worker_sid += 1;
        workers.insert(wid, Worker {
            batch, parent: call_session.0, session: sid,
            task, state: WorkerState::Queued, started: Instant::now(),
        });
        minted.push((wid, sid));
    }

    // Ack first (the goal_create shape): ids now, work proceeds in background.
    let n = minted.len();
    let admitted_now = (cap.saturating_sub(slots_used(workers))).min(n);
    bus.emit(Event::ToolResult {
        session: call_session, call: call_id,
        output: ToolOutput { ok: true, content: serde_json::json!({
            "batch": batch,
            "workers": minted.iter().map(|(w, s)| serde_json::json!({ "worker": w, "session": s })).collect::<Vec<_>>(),
            "count": n, "cap": cap,
            "admitted": admitted_now, "queued": n - admitted_now,
            "status": "fanned",
        }) },
    }).await;

    // Cards for the whole batch (Queued), then the admission pass flips up to
    // `cap` of them Running and sends their work orders.
    for (wid, _) in &minted {
        let w = &workers[wid];
        emit_state(bus, *wid, w, "queued").await;
    }
    admit_queued(workers, bus, cap).await;
    eprintln!("[worker] batch {batch} fanned: {n} tasks from session {} (cap {cap})", call_session.0);
}

/// Admit Queued workers (FIFO by id) while slots remain: Queued → Running,
/// stall clock armed, the work order goes out as an ordinary gated UserPrompt
/// on the worker's own session.
async fn admit_queued(workers: &mut HashMap<u64, Worker>, bus: &BusHandle, cap: usize) {
    while slots_used(workers) < cap {
        let Some(id) = next_queued(workers) else { break };
        let (session, text) = {
            let w = workers.get_mut(&id).unwrap();
            w.state = WorkerState::Running;
            w.started = Instant::now();
            (w.session, directive(id, w.batch, &w.task))
        };
        let w = &workers[&id];
        emit_state(bus, id, w, "").await;
        bus.emit(Event::UserPrompt { session: SessionId(session), text, images: vec![] }).await;
        eprintln!("[worker] {id} admitted → session {session}");
    }
}

/// A worker session's turn completed → Done (W1a: single-turn workers; the
/// final text in the session JSONL is the deliverable). Frees the slot.
async fn finish_done(workers: &mut HashMap<u64, Worker>, bus: &BusHandle, session: u64) -> bool {
    let hit = workers.iter_mut()
        .find(|(_, w)| w.session == session && matches!(w.state, WorkerState::Running | WorkerState::Blocked))
        .map(|(id, w)| { w.state = WorkerState::Done; *id });
    if let Some(id) = hit {
        let w = &workers[&id];
        emit_state(bus, id, w, "").await;
        eprintln!("[worker] {id} done");
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

/// Fail any Running worker whose turn has stalled past the timeout (errored/
/// aborted turns emit no TurnComplete). Blocked is exempt — an approval wait
/// runs on the human's clock, not the stall clock.
async fn fail_stalled(workers: &mut HashMap<u64, Worker>, bus: &BusHandle, step_timeout: Duration) -> bool {
    let stalled: Vec<u64> = workers.iter_mut()
        .filter(|(_, w)| w.state == WorkerState::Running && w.started.elapsed() > step_timeout)
        .map(|(id, w)| { w.state = WorkerState::Failed; *id })
        .collect();
    let changed = !stalled.is_empty();
    for id in stalled {
        let w = &workers[&id];
        emit_state(bus, id, w, "step stalled — no completion").await;
        eprintln!("[worker] {id} failed (stalled > {}s)", step_timeout.as_secs());
    }
    changed
}

/// Conductor visibility: a snapshot of all workers plus the admission picture.
async fn handle_list_workers(workers: &HashMap<u64, Worker>, bus: &BusHandle, cap: usize, call_session: SessionId, call_id: ActionId) {
    let mut rows: Vec<(u64, serde_json::Value)> = workers.iter().map(|(id, w)| (*id, serde_json::json!({
        "worker": id, "batch": w.batch, "parent": w.parent, "session": w.session,
        "state": format!("{:?}", w.state).to_lowercase(),
        "task": w.task.chars().take(100).collect::<String>(),
    }))).collect();
    rows.sort_by_key(|(id, _)| *id);
    let list: Vec<serde_json::Value> = rows.into_iter().map(|(_, j)| j).collect();
    bus.emit(Event::ToolResult { session: call_session, call: call_id,
        output: ToolOutput { ok: true, content: serde_json::json!({
            "workers": list, "count": list.len(),
            "cap": cap, "slots_used": slots_used(workers),
        }) } }).await;
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
    fn parse_tasks_accepts_strings_and_objects() {
        let args = serde_json::json!({ "tasks": ["write a haiku", { "prompt": "count to three" }] });
        assert_eq!(parse_tasks(&args).unwrap(), vec!["write a haiku", "count to three"]);
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

    #[test]
    fn slot_accounting_counts_running_and_blocked_only() {
        let mk = |state| Worker { batch: 1, parent: 0, session: apexos_core::WORKER_SESSION_BASE,
                                  task: "t".into(), state, started: Instant::now() };
        let mut m = HashMap::new();
        m.insert(1, mk(WorkerState::Running));
        m.insert(2, mk(WorkerState::Blocked));
        m.insert(3, mk(WorkerState::Queued));
        m.insert(4, mk(WorkerState::Parked));
        m.insert(5, mk(WorkerState::Done));
        assert_eq!(slots_used(&m), 2);
        assert_eq!(next_queued(&m), Some(3));
    }

    #[test]
    fn fifo_picks_the_lowest_queued_id() {
        let mk = |state| Worker { batch: 1, parent: 0, session: apexos_core::WORKER_SESSION_BASE,
                                  task: "t".into(), state, started: Instant::now() };
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
        };
        let back: PersistedWorker = serde_json::from_str(&serde_json::to_string(&pw).unwrap()).unwrap();
        assert_eq!(back.id, 7);
        assert_eq!(back.batch, 2);
        assert_eq!(back.session, apexos_core::WORKER_SESSION_BASE + 6);
        assert_eq!(back.state, WorkerState::Running);
        assert_eq!(back.task, "refactor the parser");
    }

    #[test]
    fn charter_and_directive_carry_the_contract() {
        let sys = worker_system("APEX");
        assert!(sys.contains("final text IS the work product"));
        assert!(sys.contains("depth-1"), "the no-fanout law must ride the charter");
        assert!(sys.contains("Skip orientation"));
        let d = directive(3, 1, "write the tests");
        assert!(d.contains("WORKER 3 (batch 1)"));
        assert!(d.contains("write the tests"));
        assert!(d.contains("one turn"));
    }

    #[test]
    fn load_workers_missing_file_is_empty() {
        assert!(load_workers(&PathBuf::from("/nonexistent/apexos-workers-xyz.json")).is_empty());
    }
}
