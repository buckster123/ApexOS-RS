//! The autonomous Goal driver — Phase 2a (docs/ideas/goal-driver-design.md).
//!
//! A goal is a bounded multi-turn run: a dedicated session, driven step by step
//! through the EXISTING `TurnGate` by emitting `UserPrompt` on the bus (the
//! scheduler generalized). The control-plane here is deterministic — the loop, the
//! `max_steps` ceiling, and a per-step stall timeout are enforced in code; the LLM
//! just does the work each step. **P2a has no `goal_step` tool yet:** each step
//! re-prompts "continue", and the run ends at `max_steps` (Done) or a stalled step
//! (Failed). The `goal_step` hook (early done / blocked) + the real failure breaker
//! land in P2b / P2c.
//!
//! Pattern: like `scheduler.rs`, a virtual tool forwards `(session, call_id, args)`
//! over an mpsc; unlike it, the driver is reactive — it also subscribes to the bus
//! to observe each goal session's `TurnComplete` and advance.

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

struct Goal {
    objective:    String,
    session:      u64,
    state:        GoalState,
    step:         u32,
    max_steps:    u32,
    step_started: Instant,
}

type Goals = Arc<Mutex<HashMap<u64, Goal>>>;

/// The `goal_create` virtual-tool spec.
pub fn goal_create_spec() -> ToolSpec {
    ToolSpec {
        name: "goal_create".into(),
        description: "Start an autonomous GOAL: a bounded, self-driving multi-turn run that works \
                      toward `objective` on its own dedicated session — one gated turn per step — \
                      until the objective is met or the step budget runs out. Progress shows live on \
                      the Work Board (🗂). Returns immediately with the goal_id; the run proceeds in \
                      the background.".into(),
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

fn directive(objective: &str, step: u32, max_steps: u32) -> String {
    if step == 0 {
        format!(
            "You are running an autonomous GOAL (step 1/{max_steps}).\n\nOBJECTIVE:\n{objective}\n\n\
             Make concrete progress now. You'll be re-prompted to continue each step until the \
             objective is met or the step budget is spent."
        )
    } else {
        format!(
            "Continue the GOAL (step {}/{}).\nOBJECTIVE: {}\n\nKeep making concrete progress toward \
             completion.",
            step + 1, max_steps, objective
        )
    }
}

async fn emit_state(bus: &BusHandle, id: u64, objective: &str, state: GoalState, step: u32, max_steps: u32) {
    bus.emit(Event::GoalStateChanged {
        goal: GoalId(id), objective: objective.into(), state, step, max_steps,
    }).await;
}

/// Spawn the goal driver: creates goals from `req_rx`, drives each through the gate,
/// advances on the goal session's `TurnComplete`, fails stalled steps on a 30s tick.
pub fn spawn_goal_driver(
    bus:             BusHandle,
    mut bcast_rx:    broadcast::Receiver<Event>,
    mut req_rx:      mpsc::Receiver<(SessionId, ActionId, serde_json::Value)>,
    next_session_id: Arc<AtomicU64>,
    next_goal_id:    Arc<AtomicU64>,
) {
    tokio::spawn(async move {
        let goals: Goals = Arc::new(Mutex::new(HashMap::new()));
        let mut tick = tokio::time::interval(Duration::from_secs(30));
        loop {
            tokio::select! {
                Some((call_session, call_id, args)) = req_rx.recv() => {
                    create_goal(&goals, &bus, &next_session_id, &next_goal_id,
                                call_session, call_id, args).await;
                }
                ev = bcast_rx.recv() => {
                    if let Ok(Event::TurnComplete { session }) = ev {
                        advance(&goals, &bus, session.0).await;
                    }
                }
                _ = tick.tick() => {
                    fail_stalled(&goals, &bus).await;
                }
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
        step: 0, max_steps, step_started: Instant::now(),
    });

    // Deferred ack — the tool result carries the goal id.
    bus.emit(Event::ToolResult { session: call_session, call: call_id,
        output: ToolOutput { ok: true, content: serde_json::json!({
            "goal_id": gid, "session": sid, "max_steps": max_steps, "status": "started",
        }) } }).await;

    // Announce + fire the first step.
    emit_state(bus, gid, &objective, GoalState::Acting, 0, max_steps).await;
    bus.emit(Event::UserPrompt {
        session: SessionId(sid), text: directive(&objective, 0, max_steps), images: vec![],
    }).await;
    eprintln!("[goal] {gid} started → session {sid} (max_steps {max_steps})");
}

/// A goal session's turn completed → advance: bump step, then re-prompt (still
/// Acting) or close (Done at the ceiling). No-op if the session isn't a goal's.
async fn advance(goals: &Goals, bus: &BusHandle, session: u64) {
    let advanced = {
        let mut g = goals.lock().await;
        match g.iter_mut().find(|(_, go)| go.session == session && go.state == GoalState::Acting) {
            Some((gid, goal)) => {
                goal.step += 1;
                let done = goal.step >= goal.max_steps;
                if done { goal.state = GoalState::Done; } else { goal.step_started = Instant::now(); }
                Some((*gid, goal.objective.clone(), goal.state, goal.step, goal.max_steps, done))
            }
            None => None,
        }
    };
    if let Some((gid, objective, state, step, max_steps, done)) = advanced {
        emit_state(bus, gid, &objective, state, step, max_steps).await;
        if done {
            eprintln!("[goal] {gid} done (reached step budget {max_steps})");
        } else {
            bus.emit(Event::UserPrompt {
                session: SessionId(session), text: directive(&objective, step, max_steps), images: vec![],
            }).await;
        }
    }
}

/// Fail any Acting goal whose current step has stalled past STEP_TIMEOUT (no
/// TurnComplete arrived — the turn errored/aborted). Prevents a hung goal.
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
        eprintln!("[goal] {gid} failed (step stalled > {}s)", STEP_TIMEOUT.as_secs());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn directive_step0_has_objective_and_budget() {
        let d = directive("ship the lander", 0, 5);
        assert!(d.contains("ship the lander"));
        assert!(d.contains("1/5"));
    }

    #[test]
    fn directive_continue_increments_visible_step() {
        // step index 2 → human-readable "3/5" (step+1).
        let d = directive("x", 2, 5);
        assert!(d.contains("3/5"), "got: {d}");
    }
}
