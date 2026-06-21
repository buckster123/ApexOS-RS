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
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use apexos_core::{ActionId, BusHandle, Event, GoalId, GoalState, SessionId, ToolOutput, ToolSpec};
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

/// What to do after a step resolves — computed under the lock, performed after release.
enum Outcome {
    Finished { gid: u64, objective: String, state: GoalState, step: u32, max_steps: u32 },
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

async fn emit_state(bus: &BusHandle, id: u64, objective: &str, state: GoalState, step: u32, max_steps: u32) {
    bus.emit(Event::GoalStateChanged {
        goal: GoalId(id), objective: objective.into(), state, step, max_steps,
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
) {
    tokio::spawn(async move {
        let goals: Goals = Arc::new(Mutex::new(HashMap::new()));
        let mut tick = tokio::time::interval(Duration::from_secs(30));
        loop {
            tokio::select! {
                Some((session, call_id, tool, args)) = req_rx.recv() => {
                    match tool.as_str() {
                        "goal_create" => create_goal(&goals, &bus, &next_session_id, &next_goal_id, session, call_id, args).await,
                        "goal_step"   => record_step(&goals, &bus, session, call_id, args).await,
                        _ => {}
                    }
                }
                ev = bcast_rx.recv() => {
                    if let Ok(Event::TurnComplete { session }) = ev {
                        advance(&goals, &bus, session.0).await;
                    }
                }
                _ = tick.tick() => { fail_stalled(&goals, &bus).await; }
            }
        }
    });
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

    emit_state(bus, gid, &objective, GoalState::Acting, 1, max_steps).await;
    bus.emit(Event::UserPrompt {
        session: SessionId(sid), text: directive_first(&objective, max_steps), images: vec![],
    }).await;
    eprintln!("[goal] {gid} started → session {sid} (max_steps {max_steps})");
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
async fn advance(goals: &Goals, bus: &BusHandle, session: u64) {
    let outcome = {
        let mut g = goals.lock().await;
        match g.iter_mut().find(|(_, go)| go.session == session && go.state == GoalState::Acting) {
            None => None,
            Some((gid, goal)) => {
                let gid = *gid;
                match goal.pending.take().unwrap_or(Verdict::Continue(None)) {
                    Verdict::Done => {
                        goal.state = GoalState::Done;
                        Some(Outcome::Finished { gid, objective: goal.objective.clone(), state: GoalState::Done, step: goal.step, max_steps: goal.max_steps })
                    }
                    Verdict::Blocked(reason) => {
                        goal.state = GoalState::Blocked;
                        eprintln!("[goal] {gid} blocked at step {}: {reason}", goal.step);
                        Some(Outcome::Finished { gid, objective: goal.objective.clone(), state: GoalState::Blocked, step: goal.step, max_steps: goal.max_steps })
                    }
                    Verdict::Continue(steer) => {
                        if goal.step >= goal.max_steps {
                            goal.state = GoalState::Done; // budget reached
                            Some(Outcome::Finished { gid, objective: goal.objective.clone(), state: GoalState::Done, step: goal.step, max_steps: goal.max_steps })
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
    match outcome {
        Some(Outcome::Finished { gid, objective, state, step, max_steps }) => {
            emit_state(bus, gid, &objective, state, step, max_steps).await;
            eprintln!("[goal] {gid} {state:?} at step {step}/{max_steps}");
        }
        Some(Outcome::Next { gid, objective, step, max_steps, directive }) => {
            emit_state(bus, gid, &objective, GoalState::Acting, step, max_steps).await;
            bus.emit(Event::UserPrompt { session: SessionId(session), text: directive, images: vec![] }).await;
        }
        None => {}
    }
}

/// Fail any Acting goal whose current step has stalled past STEP_TIMEOUT.
async fn fail_stalled(goals: &Goals, bus: &BusHandle) {
    let failed: Vec<(u64, String, u32, u32)> = {
        let mut g = goals.lock().await;
        g.iter_mut()
            .filter(|(_, go)| go.state == GoalState::Acting && go.step_started.elapsed() > STEP_TIMEOUT)
            .map(|(gid, go)| { go.state = GoalState::Failed; (*gid, go.objective.clone(), go.step, go.max_steps) })
            .collect()
    };
    for (gid, objective, step, max_steps) in failed {
        emit_state(bus, gid, &objective, GoalState::Failed, step, max_steps).await;
        eprintln!("[goal] {gid} failed (step {step} stalled > {}s)", STEP_TIMEOUT.as_secs());
    }
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
    fn parse_verdict_maps_status() {
        assert!(matches!(parse_verdict(&serde_json::json!({"status":"done"})), Verdict::Done));
        assert!(matches!(parse_verdict(&serde_json::json!({"status":"blocked","reason":"no key"}), ), Verdict::Blocked(r) if r == "no key"));
        assert!(matches!(parse_verdict(&serde_json::json!({"status":"continue","next":"step 2"})), Verdict::Continue(Some(s)) if s == "step 2"));
        // Absent/unknown status defaults to continue.
        assert!(matches!(parse_verdict(&serde_json::json!({})), Verdict::Continue(None)));
    }
}
