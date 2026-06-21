//! The autonomous Goal driver — Phase 2a + 2b (docs/ideas/goal-driver-design.md).
//!
//! A goal is a bounded multi-turn run: a dedicated session, driven step by step
//! through the EXISTING `TurnGate` by emitting `UserPrompt` on the bus (the
//! scheduler generalized). The control-plane is deterministic — the loop, the
//! `max_steps` ceiling, and a per-step stall timeout are enforced in code; the LLM
//! does the work each step **and proposes the next move** via the `goal_step` tool.
//! Code disposes: `goal_step{done}` is honoured but the budget/guards are the hard
//! stop (LLM-proposes / code-disposes, like the evolution applier + council).
//!
//! P2b: the `goal_step{continue|done|blocked}` hook lets a goal finish early (no
//! more burning the whole budget) or park gracefully. `step` is the **in-flight**
//! step (1-indexed), so the board card tracks what the agent is actually doing
//! (1/N … N/N → DONE), not the completed-count (the P2a off-by-one APEX caught live).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use apexos_core::{ActionId, BusHandle, Event, GoalId, GoalState, SessionId, ToolOutput, ToolSpec};
use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, mpsc, Mutex};

const DEFAULT_MAX_STEPS: u32 = 12;
const MAX_STEPS_CEIL:    u32 = 100;
/// A step that produces no `TurnComplete` within this window is treated as stalled
/// (turn errored/aborted → no completion event) → the goal Fails instead of
/// hanging. The richer failure breaker (consecutive `ok:false` steps) lands in P2c.
const STEP_TIMEOUT: Duration = Duration::from_secs(900);

/// The agent's reported outcome for the in-flight step (via `goal_step`).
enum Verdict {
    Continue(Option<String>), // optional steer for the next step
    Done,
    Blocked(String),          // reason
}

struct Goal {
    objective:    String,
    session:      u64,
    state:        GoalState,
    step:         u32,            // IN-FLIGHT step, 1-indexed (1 = first step running)
    max_steps:    u32,
    step_started: Instant,
    pending:      Option<Verdict>, // the agent's goal_step verdict, applied on TurnComplete
}

type Goals = Arc<Mutex<HashMap<u64, Goal>>>;

/// The on-disk form of a goal (the transient `step_started`/`pending` are dropped).
/// Persisted to `goals.json` so an in-flight goal survives a daemon restart — most
/// importantly the nightly self-update binary swap, which would otherwise evaporate
/// any running goal. (P2d, docs/ideas/goal-driver-design.md)
#[derive(Serialize, Deserialize)]
struct PersistedGoal {
    id:        u64,
    objective: String,
    session:   u64,
    state:     GoalState,
    step:      u32,
    max_steps: u32,
}

async fn save_goals(goals: &Goals, path: &PathBuf) {
    let snapshot: Vec<PersistedGoal> = {
        let g = goals.lock().await;
        g.iter().map(|(id, go)| PersistedGoal {
            id: *id, objective: go.objective.clone(), session: go.session,
            state: go.state, step: go.step, max_steps: go.max_steps,
        }).collect()
    };
    if let Some(parent) = path.parent() { let _ = std::fs::create_dir_all(parent); }
    if let Ok(json) = serde_json::to_string_pretty(&snapshot) {
        let _ = std::fs::write(path, json);
    }
}

fn load_goals(path: &PathBuf) -> Vec<PersistedGoal> {
    std::fs::read_to_string(path).ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

/// What to do after a step resolves — computed under the lock, performed after release.
enum Outcome {
    Finished { gid: u64, objective: String, state: GoalState, step: u32, max_steps: u32, detail: String },
    Next     { gid: u64, objective: String, step: u32, max_steps: u32, directive: String },
}

pub fn goal_create_spec() -> ToolSpec {
    ToolSpec {
        name: "goal_create".into(),
        description: "Start an autonomous GOAL: a bounded, self-driving multi-turn run that works \
                      toward `objective` on its own dedicated session — one gated turn per step — \
                      until you call goal_step{done} or the step budget runs out. Progress shows \
                      live on the Work Board (🗂). Returns immediately with the goal_id; the run \
                      proceeds in the background.".into(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "objective": { "type": "string",  "description": "What the goal should accomplish." },
                "max_steps": { "type": "integer", "description": "Hard ceiling on turns (default 12, max 100)." }
            },
            "required": ["objective"]
        }),
    }
}

pub fn goal_step_spec() -> ToolSpec {
    ToolSpec {
        name: "goal_step".into(),
        description: "Report the outcome of the CURRENT goal step — only meaningful while running a \
                      goal. `done`: the objective is fully met, finish now (don't waste the rest of \
                      the budget). `blocked`: an unresolvable dependency — park the goal with a \
                      `reason`. `continue` (also the default if you DON'T call this): keep going, \
                      optionally steering the next step via `next`. The driver applies your verdict \
                      when this turn completes.".into(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "status": { "type": "string", "enum": ["continue", "done", "blocked"],
                            "description": "done = objective met; blocked = parked; continue = keep going." },
                "next":   { "type": "string", "description": "Optional steer for the next step (status=continue)." },
                "reason": { "type": "string", "description": "Why it's parked (status=blocked)." }
            },
            "required": ["status"]
        }),
    }
}

pub fn list_goals_spec() -> ToolSpec {
    ToolSpec {
        name: "list_goals".into(),
        description: "List the autonomous goals on this node and their live state (id, state \
                      acting/done/blocked/failed, step/max_steps, objective) — check on a running \
                      goal from anywhere, without the Work Board open.".into(),
        input_schema: serde_json::json!({ "type": "object", "properties": {} }),
    }
}

pub fn goal_resume_spec() -> ToolSpec {
    ToolSpec {
        name: "goal_resume".into(),
        description: "Resume a Blocked or Failed goal by id — e.g. one interrupted by a daemon \
                      restart (it reappears Blocked: 'interrupted by daemon restart'), or parked via \
                      goal_step{blocked}. It re-enters Acting at its last step and picks the objective \
                      back up. Use list_goals to find resumable goals.".into(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": { "goal_id": { "type": "integer", "description": "The goal to resume." } },
            "required": ["goal_id"]
        }),
    }
}

fn directive_first(objective: &str, max_steps: u32) -> String {
    format!(
        "You are running an autonomous GOAL (step 1/{max_steps}).\n\nOBJECTIVE:\n{objective}\n\n\
         Make concrete progress now. When the objective is FULLY met, call \
         `goal_step{{status:\"done\"}}` to finish early — don't burn the rest of the budget. If you \
         hit an unresolvable blocker, call `goal_step{{status:\"blocked\", reason:\"…\"}}`. Otherwise \
         just keep working; you'll be re-prompted to continue until done or the budget runs out."
    )
}

fn directive_continue(objective: &str, step: u32, max_steps: u32, steer: Option<&str>) -> String {
    let head = format!("Continue the GOAL (step {step}/{max_steps}). OBJECTIVE: {objective}");
    match steer {
        Some(s) => format!("{head}\n\nFocus this step on: {s}\n\nCall `goal_step{{status:\"done\"}}` when fully complete."),
        None    => format!("{head}\n\nKeep making concrete progress. Call `goal_step{{status:\"done\"}}` when fully complete."),
    }
}

fn parse_verdict(args: &serde_json::Value) -> Verdict {
    match args["status"].as_str() {
        Some("done")    => Verdict::Done,
        Some("blocked") => Verdict::Blocked(args["reason"].as_str().unwrap_or("blocked").to_string()),
        _               => Verdict::Continue(args["next"].as_str().map(str::to_owned)),
    }
}

async fn emit_state(bus: &BusHandle, id: u64, objective: &str, state: GoalState, step: u32, max_steps: u32, detail: &str) {
    bus.emit(Event::GoalStateChanged {
        goal: GoalId(id), objective: objective.into(), state, step, max_steps, detail: detail.into(),
    }).await;
}

/// Spawn the goal driver: creates goals + records `goal_step` verdicts from `req_rx`,
/// drives each through the gate, advances on the goal session's `TurnComplete`, and
/// fails stalled steps on a 30s tick.
pub fn spawn_goal_driver(
    bus:             BusHandle,
    mut bcast_rx:    broadcast::Receiver<Event>,
    mut req_rx:      mpsc::Receiver<(SessionId, ActionId, String, serde_json::Value)>,
    next_session_id: Arc<AtomicU64>,
    next_goal_id:    Arc<AtomicU64>,
    goals_path:      PathBuf,
) {
    tokio::spawn(async move {
        let goals: Goals = Arc::new(Mutex::new(HashMap::new()));
        reload_goals(&goals, &bus, &next_goal_id, &goals_path).await;
        let mut tick = tokio::time::interval(Duration::from_secs(30));
        loop {
            tokio::select! {
                Some((session, call_id, tool, args)) = req_rx.recv() => {
                    match tool.as_str() {
                        "goal_create" => { create_goal(&goals, &bus, &next_session_id, &next_goal_id, session, call_id, args).await; save_goals(&goals, &goals_path).await; }
                        "goal_step"   => record_step(&goals, &bus, session, call_id, args).await,
                        "goal_resume" => { resume_goal(&goals, &bus, session, call_id, args).await; save_goals(&goals, &goals_path).await; }
                        "list_goals"  => handle_list_goals(&goals, &bus, session, call_id).await,
                        _ => {}
                    }
                }
                ev = bcast_rx.recv() => {
                    if let Ok(Event::TurnComplete { session }) = ev {
                        if advance(&goals, &bus, session.0).await { save_goals(&goals, &goals_path).await; }
                    }
                }
                _ = tick.tick() => {
                    if fail_stalled(&goals, &bus).await { save_goals(&goals, &goals_path).await; }
                }
            }
        }
    });
}

/// Boot: reload persisted goals. A goal that was mid-flight (Acting) when the daemon
/// stopped is re-entered as Blocked ("interrupted by restart") — never silently lost
/// — and resumes via goal_resume. Re-seeds the goal-id counter past every loaded id.
async fn reload_goals(goals: &Goals, bus: &BusHandle, next_goal_id: &Arc<AtomicU64>, path: &PathBuf) {
    let loaded = load_goals(path);
    if loaded.is_empty() { return; }
    let mut max_id = 0u64;
    let mut announce: Vec<(u64, String, GoalState, u32, u32)> = Vec::new();
    {
        let mut g = goals.lock().await;
        for pg in loaded {
            max_id = max_id.max(pg.id);
            let state = if pg.state == GoalState::Acting { GoalState::Blocked } else { pg.state };
            g.insert(pg.id, Goal {
                objective: pg.objective.clone(), session: pg.session, state,
                step: pg.step, max_steps: pg.max_steps, step_started: Instant::now(), pending: None,
            });
            announce.push((pg.id, pg.objective, state, pg.step, pg.max_steps));
        }
    }
    next_goal_id.fetch_max(max_id + 1, Ordering::SeqCst);
    for (id, objective, state, step, max_steps) in announce {
        let detail = if state == GoalState::Blocked { "interrupted by daemon restart — goal_resume to continue" } else { "" };
        emit_state(bus, id, &objective, state, step, max_steps, detail).await;
    }
    eprintln!("[goal] reloaded goals from {} (in-flight ones marked blocked)", path.display());
}

/// goal_resume{goal_id}: re-activate a Blocked/Failed goal — back to Acting at its
/// last step, re-emitting the continue directive so the agent picks the objective up.
async fn resume_goal(goals: &Goals, bus: &BusHandle, call_session: SessionId, call_id: ActionId, args: serde_json::Value) {
    let resumed = {
        let mut g = goals.lock().await;
        match args["goal_id"].as_u64().and_then(|id| g.get_mut(&id).map(|go| (id, go))) {
            Some((id, go)) if matches!(go.state, GoalState::Blocked | GoalState::Failed) => {
                go.state = GoalState::Acting;
                go.step_started = Instant::now();
                go.pending = None;
                Some((id, go.objective.clone(), go.session, go.step, go.max_steps))
            }
            _ => None,
        }
    };
    match resumed {
        Some((id, objective, session, step, max_steps)) => {
            emit_state(bus, id, &objective, GoalState::Acting, step, max_steps, "resumed").await;
            bus.emit(Event::UserPrompt {
                session: SessionId(session),
                text: directive_continue(&objective, step, max_steps, None),
                images: vec![],
            }).await;
            bus.emit(Event::ToolResult { session: call_session, call: call_id,
                output: ToolOutput { ok: true, content: serde_json::json!({ "goal_id": id, "status": "resumed", "step": step }) } }).await;
            eprintln!("[goal] {id} resumed at step {step}");
        }
        None => {
            bus.emit(Event::ToolResult { session: call_session, call: call_id,
                output: ToolOutput { ok: false, content: serde_json::json!("no resumable (blocked/failed) goal with that id") } }).await;
        }
    }
}

async fn create_goal(
    goals: &Goals, bus: &BusHandle,
    next_session_id: &Arc<AtomicU64>, next_goal_id: &Arc<AtomicU64>,
    call_session: SessionId, call_id: ActionId, args: serde_json::Value,
) {
    let objective = match args["objective"].as_str() {
        Some(o) if !o.trim().is_empty() => o.to_string(),
        _ => {
            bus.emit(Event::ToolResult { session: call_session, call: call_id,
                output: ToolOutput { ok: false, content: serde_json::json!("objective is required") } }).await;
            return;
        }
    };
    let max_steps = args["max_steps"].as_u64()
        .map(|n| (n as u32).clamp(1, MAX_STEPS_CEIL))
        .unwrap_or(DEFAULT_MAX_STEPS);

    let gid = next_goal_id.fetch_add(1, Ordering::SeqCst);
    let sid = next_session_id.fetch_add(1, Ordering::SeqCst);

    goals.lock().await.insert(gid, Goal {
        objective: objective.clone(), session: sid, state: GoalState::Acting,
        step: 1, max_steps, step_started: Instant::now(), pending: None,
    });

    bus.emit(Event::ToolResult { session: call_session, call: call_id,
        output: ToolOutput { ok: true, content: serde_json::json!({
            "goal_id": gid, "session": sid, "max_steps": max_steps, "status": "started",
        }) } }).await;

    emit_state(bus, gid, &objective, GoalState::Acting, 1, max_steps, "").await;
    bus.emit(Event::UserPrompt {
        session: SessionId(sid), text: directive_first(&objective, max_steps), images: vec![],
    }).await;
    eprintln!("[goal] {gid} started → session {sid} (max_steps {max_steps})");
}

/// Root-session visibility: return a snapshot of all goals (APEX's P2c ask — "is
/// goal N still running?" without the board open).
async fn handle_list_goals(goals: &Goals, bus: &BusHandle, call_session: SessionId, call_id: ActionId) {
    let list: Vec<serde_json::Value> = {
        let g = goals.lock().await;
        let mut v: Vec<(u64, serde_json::Value)> = g.iter().map(|(gid, go)| (*gid, serde_json::json!({
            "goal_id": gid, "state": format!("{:?}", go.state).to_lowercase(),
            "step": go.step, "max_steps": go.max_steps, "objective": go.objective,
        }))).collect();
        v.sort_by_key(|(gid, _)| *gid);
        v.into_iter().map(|(_, j)| j).collect()
    };
    bus.emit(Event::ToolResult { session: call_session, call: call_id,
        output: ToolOutput { ok: true, content: serde_json::json!({ "goals": list, "count": list.len() }) } }).await;
}

/// The agent called `goal_step` from within a goal's turn — record the verdict for
/// the in-flight step (applied on the upcoming TurnComplete) and ack the tool now.
async fn record_step(goals: &Goals, bus: &BusHandle, call_session: SessionId, call_id: ActionId, args: serde_json::Value) {
    let status = args["status"].as_str().unwrap_or("continue").to_string();
    let recorded = {
        let mut g = goals.lock().await;
        match g.iter_mut().find(|(_, go)| go.session == call_session.0 && go.state == GoalState::Acting) {
            Some((_, goal)) => { goal.pending = Some(parse_verdict(&args)); true }
            None => false,
        }
    };
    let content = if recorded {
        serde_json::json!({ "recorded": status, "note": "applied when this step completes" })
    } else {
        serde_json::json!("goal_step has no effect outside a running goal session")
    };
    bus.emit(Event::ToolResult { session: call_session, call: call_id,
        output: ToolOutput { ok: recorded, content } }).await;
}

/// A goal session's turn completed → apply the in-flight step's verdict: done (early),
/// blocked (park), or continue (next step, or close at the budget ceiling).
async fn advance(goals: &Goals, bus: &BusHandle, session: u64) -> bool {
    let outcome = {
        let mut g = goals.lock().await;
        match g.iter_mut().find(|(_, go)| go.session == session && go.state == GoalState::Acting) {
            None => None,
            Some((gid, goal)) => {
                let gid = *gid;
                let fin = |goal: &Goal, gid: u64, state: GoalState, detail: String| Outcome::Finished {
                    gid, objective: goal.objective.clone(), state, step: goal.step, max_steps: goal.max_steps, detail,
                };
                match goal.pending.take().unwrap_or(Verdict::Continue(None)) {
                    Verdict::Done => {
                        goal.state = GoalState::Done;
                        Some(fin(goal, gid, GoalState::Done, String::new()))
                    }
                    Verdict::Blocked(reason) => {
                        goal.state = GoalState::Blocked;
                        eprintln!("[goal] {gid} blocked at step {}: {reason}", goal.step);
                        Some(fin(goal, gid, GoalState::Blocked, reason))
                    }
                    Verdict::Continue(steer) => {
                        if goal.step >= goal.max_steps {
                            goal.state = GoalState::Done; // budget reached
                            Some(fin(goal, gid, GoalState::Done, "step budget reached".into()))
                        } else {
                            goal.step += 1;
                            goal.step_started = Instant::now();
                            let directive = directive_continue(&goal.objective, goal.step, goal.max_steps, steer.as_deref());
                            Some(Outcome::Next { gid, objective: goal.objective.clone(), step: goal.step, max_steps: goal.max_steps, directive })
                        }
                    }
                }
            }
        }
    };
    let changed = outcome.is_some();
    match outcome {
        Some(Outcome::Finished { gid, objective, state, step, max_steps, detail }) => {
            emit_state(bus, gid, &objective, state, step, max_steps, &detail).await;
            eprintln!("[goal] {gid} {state:?} at step {step}/{max_steps}");
        }
        Some(Outcome::Next { gid, objective, step, max_steps, directive }) => {
            emit_state(bus, gid, &objective, GoalState::Acting, step, max_steps, "").await;
            bus.emit(Event::UserPrompt { session: SessionId(session), text: directive, images: vec![] }).await;
        }
        None => {}
    }
    changed
}

/// Fail any Acting goal whose current step has stalled past STEP_TIMEOUT.
async fn fail_stalled(goals: &Goals, bus: &BusHandle) -> bool {
    let failed: Vec<(u64, String, u32, u32)> = {
        let mut g = goals.lock().await;
        g.iter_mut()
            .filter(|(_, go)| go.state == GoalState::Acting && go.step_started.elapsed() > STEP_TIMEOUT)
            .map(|(gid, go)| { go.state = GoalState::Failed; (*gid, go.objective.clone(), go.step, go.max_steps) })
            .collect()
    };
    let changed = !failed.is_empty();
    for (gid, objective, step, max_steps) in failed {
        emit_state(bus, gid, &objective, GoalState::Failed, step, max_steps, "step stalled — no completion").await;
        eprintln!("[goal] {gid} failed (step {step} stalled > {}s)", STEP_TIMEOUT.as_secs());
    }
    changed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_directive_is_1_indexed_and_names_goal_step() {
        let d = directive_first("ship the lander", 5);
        assert!(d.contains("step 1/5"));
        assert!(d.contains("ship the lander"));
        assert!(d.contains("goal_step"));
    }

    #[test]
    fn continue_directive_tracks_inflight_step_and_steer() {
        let d = directive_continue("x", 3, 5, Some("write the tests"));
        assert!(d.contains("step 3/5"), "got: {d}");
        assert!(d.contains("write the tests"));
    }

    #[test]
    fn persisted_goal_round_trips_json() {
        let pg = PersistedGoal {
            id: 3, objective: "ship it".into(), session: 44,
            state: GoalState::Acting, step: 2, max_steps: 5,
        };
        let back: PersistedGoal = serde_json::from_str(&serde_json::to_string(&pg).unwrap()).unwrap();
        assert_eq!(back.id, 3);
        assert_eq!(back.session, 44);
        assert_eq!(back.step, 2);
        assert_eq!(back.state, GoalState::Acting);
        assert_eq!(back.objective, "ship it");
    }

    #[test]
    fn load_goals_missing_file_is_empty() {
        assert!(load_goals(&std::path::PathBuf::from("/nonexistent/apexos-goals-xyz.json")).is_empty());
    }

    #[test]
    fn parse_verdict_maps_status() {
        assert!(matches!(parse_verdict(&serde_json::json!({"status":"done"})), Verdict::Done));
        assert!(matches!(parse_verdict(&serde_json::json!({"status":"blocked","reason":"no key"}), ), Verdict::Blocked(r) if r == "no key"));
        assert!(matches!(parse_verdict(&serde_json::json!({"status":"continue","next":"step 2"})), Verdict::Continue(Some(s)) if s == "step 2"));
        // Absent/unknown status defaults to continue.
        assert!(matches!(parse_verdict(&serde_json::json!({})), Verdict::Continue(None)));
    }
}
