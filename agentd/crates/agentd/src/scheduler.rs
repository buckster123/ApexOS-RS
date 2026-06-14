use apexos_core::{ActionId, BusHandle, Event, SessionId, ToolOutput};
use chrono::{TimeZone, Utc};
use cron::Schedule;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScheduledTask {
    pub id:         String,
    pub cron:       String,
    pub prompt:     String,
    pub session_id: Option<u64>,
    pub created_at: u64,
    pub last_run:   Option<u64>,
}

pub type SchedulerState = Arc<Mutex<Vec<ScheduledTask>>>;

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn unique_id() -> String {
    format!("sched_{:x}", now_secs() ^ (std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_nanos() as u64))
}

pub fn load_schedules(path: &PathBuf) -> Vec<ScheduledTask> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(_) => return vec![],
    };
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

pub fn save_schedules(path: &PathBuf, tasks: &[ScheduledTask]) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let content: String = tasks.iter()
        .filter_map(|t| serde_json::to_string(t).ok())
        .map(|s| s + "\n")
        .collect();
    let _ = std::fs::write(path, content);
}

/// Background task: polls every 60s, fires UserPrompt when a cron expression matches.
pub async fn run_scheduler(
    state:      SchedulerState,
    bus:        BusHandle,
    path:       PathBuf,
    root_session: SessionId,
) {
    loop {
        tokio::time::sleep(tokio::time::Duration::from_secs(60)).await;

        let now = now_secs();
        let now_dt = Utc.timestamp_opt(now as i64, 0).single()
            .unwrap_or_else(Utc::now);

        let mut tasks = state.lock().await;
        let mut changed = false;

        for task in tasks.iter_mut() {
            let schedule = match Schedule::from_str(&task.cron) {
                Ok(s) => s,
                Err(_) => continue,
            };

            let last = task.last_run.unwrap_or(task.created_at);
            let last_dt = Utc.timestamp_opt(last as i64, 0).single()
                .unwrap_or_else(|| Utc::now() - chrono::Duration::seconds(120));

            let should_fire = schedule.after(&last_dt)
                .next()
                .map(|next| next <= now_dt)
                .unwrap_or(false);

            if should_fire {
                let session = task.session_id
                    .map(SessionId)
                    .unwrap_or(root_session);

                eprintln!("[scheduler] firing '{}' → session {:?}", task.id, session);
                bus.emit(Event::UserPrompt {
                    session,
                    text: task.prompt.clone(),
                    images: vec![],
                }).await;

                task.last_run = Some(now);
                changed = true;
            }
        }

        if changed {
            save_schedules(&path, &tasks);
        }
    }
}

/// Op handler: receives (session, call_id, tool_name, args) from supervisor, processes, emits ToolResult.
pub fn spawn_scheduler_handler(
    state:  SchedulerState,
    path:   PathBuf,
    bus:    BusHandle,
    mut rx: mpsc::Receiver<(SessionId, ActionId, String, serde_json::Value)>,
) {
    tokio::spawn(async move {
        while let Some((session, call_id, tool, args)) = rx.recv().await {
            let output = match tool.as_str() {
                "schedule_task" => handle_schedule_task(&state, &path, &args).await,
                "list_schedules" => handle_list_schedules(&state).await,
                "cancel_schedule" => handle_cancel_schedule(&state, &path, &args).await,
                _ => ToolOutput { ok: false, content: serde_json::json!("unknown scheduler tool") },
            };
            bus.emit(Event::ToolResult { session, call: call_id, output }).await;
        }
    });
}

async fn handle_schedule_task(
    state: &SchedulerState,
    path:  &PathBuf,
    args:  &serde_json::Value,
) -> ToolOutput {
    let cron_expr = match args["cron"].as_str() {
        Some(c) => c.to_string(),
        None => return ToolOutput { ok: false, content: serde_json::json!("cron is required") },
    };
    let prompt = match args["prompt"].as_str() {
        Some(p) => p.to_string(),
        None => return ToolOutput { ok: false, content: serde_json::json!("prompt is required") },
    };

    // Validate the cron expression before storing
    if Schedule::from_str(&cron_expr).is_err() {
        return ToolOutput {
            ok: false,
            content: serde_json::json!(format!("invalid cron expression: '{}'", cron_expr)),
        };
    }

    let session_id = args["session_id"].as_u64();
    let task = ScheduledTask {
        id:         unique_id(),
        cron:       cron_expr,
        prompt,
        session_id,
        created_at: now_secs(),
        last_run:   None,
    };
    let id = task.id.clone();
    let mut tasks = state.lock().await;
    tasks.push(task);
    save_schedules(path, &tasks);

    ToolOutput {
        ok:      true,
        content: serde_json::json!({ "schedule_id": id, "status": "scheduled" }),
    }
}

async fn handle_list_schedules(state: &SchedulerState) -> ToolOutput {
    let tasks = state.lock().await;
    let list: Vec<serde_json::Value> = tasks.iter().map(|t| serde_json::json!({
        "id":         t.id,
        "cron":       t.cron,
        "prompt":     t.prompt,
        "session_id": t.session_id,
        "created_at": t.created_at,
        "last_run":   t.last_run,
    })).collect();
    ToolOutput {
        ok:      true,
        content: serde_json::json!(list),
    }
}

async fn handle_cancel_schedule(
    state: &SchedulerState,
    path:  &PathBuf,
    args:  &serde_json::Value,
) -> ToolOutput {
    let schedule_id = match args["schedule_id"].as_str() {
        Some(id) => id.to_string(),
        None => return ToolOutput { ok: false, content: serde_json::json!("schedule_id is required") },
    };
    let mut tasks = state.lock().await;
    let before = tasks.len();
    tasks.retain(|t| t.id != schedule_id);
    if tasks.len() == before {
        return ToolOutput {
            ok:      false,
            content: serde_json::json!(format!("schedule '{}' not found", schedule_id)),
        };
    }
    save_schedules(path, &tasks);
    ToolOutput {
        ok:      true,
        content: serde_json::json!({ "cancelled": schedule_id }),
    }
}
