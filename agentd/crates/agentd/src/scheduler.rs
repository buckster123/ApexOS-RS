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
    /// One-shot fire time (epoch secs). `Some` = fire once when `now >= at`, then
    /// keep the entry (last_run set) for daily-cap accounting until pruned.
    /// `None` = the original recurring-cron behaviour. Serde-default so old
    /// schedules.jsonl lines load unchanged.
    #[serde(default)]
    pub at:         Option<u64>,
    /// Agent-scheduled continuity note (`schedule_wakeup`): fires into the ROOT
    /// session with self-provenance framing, counted against the wakeup caps.
    #[serde(default)]
    pub wakeup:     bool,
}

pub type SchedulerState = Arc<Mutex<Vec<ScheduledTask>>>;

// ── Wakeups — the agent's own alarm clock ──────────────────────────────────────
// A wakeup is a one-shot note-to-future-self: the agent decides when it next runs
// and why, instead of existing only when poked (user prompt / sensor / crons set by
// the operator). Bounds keep a self-perpetuating chain harmless: a floor on delay,
// a horizon, a pending cap, and a per-UTC-day fire cap enforced at SCHEDULE time
// (each fire raises fired-today, so a schedule-on-every-wake chain self-limits).

const WAKEUP_MIN_DELAY_SECS: u64 = 60;
const WAKEUP_MAX_HORIZON_SECS: u64 = 90 * 86_400; // 90 days
/// Fired wakeups are retained (for cap accounting + list_wakeups visibility) this
/// long, then pruned from schedules.jsonl.
const WAKEUP_FIRED_RETENTION_SECS: u64 = 48 * 3_600;

fn wakeup_enabled() -> bool {
    !matches!(
        std::env::var("AGENTD_WAKEUP").unwrap_or_default().to_lowercase().as_str(),
        "0" | "false" | "off"
    )
}

fn wakeup_max_pending() -> usize {
    std::env::var("AGENTD_WAKEUP_MAX_PENDING").ok().and_then(|s| s.parse().ok()).unwrap_or(16)
}

fn wakeup_daily_cap() -> usize {
    std::env::var("AGENTD_WAKEUP_DAILY_CAP").ok().and_then(|s| s.parse().ok()).unwrap_or(24)
}

fn utc_day(t: u64) -> u64 {
    t / 86_400
}

fn rfc3339(t: u64) -> String {
    Utc.timestamp_opt(t as i64, 0)
        .single()
        .map(|dt| dt.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
        .unwrap_or_else(|| t.to_string())
}

/// Resolve the requested fire time from `delay_secs` (number) XOR `at` (RFC3339
/// string or epoch-seconds number). Pure — unit-tested.
pub fn resolve_fire_at(args: &serde_json::Value, now: u64) -> Result<u64, String> {
    let delay = args["delay_secs"].as_u64();
    let at_raw = &args["at"];
    let at: Option<u64> = if let Some(n) = at_raw.as_u64() {
        Some(n)
    } else if let Some(s) = at_raw.as_str() {
        match chrono::DateTime::parse_from_rfc3339(s) {
            Ok(dt) => Some(dt.timestamp().max(0) as u64),
            Err(e) => return Err(format!("could not parse 'at' as RFC3339 ('{s}'): {e}")),
        }
    } else {
        None
    };
    let fire = match (delay, at) {
        (Some(d), None) => now.saturating_add(d),
        (None, Some(t)) => t,
        (Some(_), Some(_)) => return Err("pass exactly one of delay_secs / at, not both".into()),
        (None, None) => return Err("pass exactly one of delay_secs (seconds from now) or at (RFC3339 UTC)".into()),
    };
    if fire < now.saturating_add(WAKEUP_MIN_DELAY_SECS) {
        return Err(format!("wakeup must be at least {WAKEUP_MIN_DELAY_SECS}s in the future"));
    }
    if fire > now.saturating_add(WAKEUP_MAX_HORIZON_SECS) {
        return Err("wakeup horizon is 90 days — store a cerebro intention for anything further out".into());
    }
    Ok(fire)
}

/// Wakeups that haven't fired yet.
pub fn pending_wakeups(tasks: &[ScheduledTask]) -> usize {
    tasks.iter().filter(|t| t.wakeup && t.last_run.is_none()).count()
}

/// Wakeups fired in the current UTC day (the schedule-time cap input).
pub fn fired_today(tasks: &[ScheduledTask], now: u64) -> usize {
    tasks
        .iter()
        .filter(|t| t.wakeup && t.last_run.map(|r| utc_day(r) == utc_day(now)).unwrap_or(false))
        .count()
}

/// Drop fired wakeups past the retention window. Returns true when anything changed.
pub fn prune_fired_wakeups(tasks: &mut Vec<ScheduledTask>, now: u64) -> bool {
    let before = tasks.len();
    tasks.retain(|t| {
        !(t.wakeup
            && t.last_run.map(|r| now.saturating_sub(r) > WAKEUP_FIRED_RETENTION_SECS).unwrap_or(false))
    });
    tasks.len() != before
}

/// The prompt a fired wakeup injects: the note wrapped in self-provenance, so the
/// agent knows it is its own past self talking (and how late the alarm rang).
pub fn wakeup_frame(task: &ScheduledTask, now: u64) -> String {
    let due = task.at.unwrap_or(now);
    let late = now.saturating_sub(due);
    let late_note = if late > 120 {
        format!(" — fired {}m late (the daemon was down or busy)", late / 60)
    } else {
        String::new()
    };
    format!(
        "[wakeup {} — a note you scheduled for yourself at {}{}]: {}",
        task.id,
        rfc3339(task.created_at),
        late_note,
        task.prompt
    )
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn unique_id() -> String {
    // A monotonic per-process counter guarantees uniqueness within a run (the old
    // secs^subsec_nanos XOR collided on same-second creates, and `cancel` then
    // removed both); the full-nanosecond timestamp disambiguates across restarts
    // (where the counter resets to 0).
    static SCHED_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let seq = SCHED_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    format!("sched_{nanos:x}_{seq:x}")
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
            // One-shot (wakeup) branch: fire once when due, keep the entry marked
            // fired for cap accounting / list visibility. An entry overdue at boot
            // (daemon was down) fires on the first tick — commitments run late,
            // they don't evaporate; the frame says how late.
            if task.at.is_some() {
                let due = task.at.unwrap_or(0);
                // AGENTD_WAKEUP=0 is a full kill switch: new schedules are refused
                // AND already-pending wakeups hold (they fire late if re-enabled).
                if task.wakeup && !wakeup_enabled() {
                    continue;
                }
                if task.last_run.is_none() && now >= due {
                    let session = if task.wakeup {
                        root_session // a wakeup is the node agent's own thread, always
                    } else {
                        task.session_id.map(SessionId).unwrap_or(root_session)
                    };
                    let text = if task.wakeup {
                        wakeup_frame(task, now)
                    } else {
                        task.prompt.clone()
                    };
                    eprintln!("[scheduler] firing wakeup '{}' → session {:?}", task.id, session);
                    bus.emit(Event::UserPrompt { session, text, images: vec![] }).await;
                    task.last_run = Some(now);
                    changed = true;
                }
                continue;
            }

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

        let pruned = prune_fired_wakeups(&mut tasks, now);
        if changed || pruned {
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
                "schedule_wakeup" => handle_schedule_wakeup(&state, &path, &args).await,
                "list_wakeups" => handle_list_wakeups(&state).await,
                "cancel_wakeup" => handle_cancel_wakeup(&state, &path, &args).await,
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
        at:         None,
        wakeup:     false,
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

async fn handle_schedule_wakeup(
    state: &SchedulerState,
    path:  &PathBuf,
    args:  &serde_json::Value,
) -> ToolOutput {
    if !wakeup_enabled() {
        return ToolOutput { ok: false, content: serde_json::json!("wakeups are disabled on this node (AGENTD_WAKEUP=0)") };
    }
    let note = match args["note"].as_str().map(str::trim).filter(|n| !n.is_empty()) {
        Some(n) => n.to_string(),
        None => return ToolOutput { ok: false, content: serde_json::json!("note is required — write it to your future self: context, why it matters, the next concrete action") },
    };
    if note.len() > 4000 {
        return ToolOutput { ok: false, content: serde_json::json!("note too long (>4000 chars) — put the detail in cerebro and reference it") };
    }
    let now = now_secs();
    let fire = match resolve_fire_at(args, now) {
        Ok(f) => f,
        Err(e) => return ToolOutput { ok: false, content: serde_json::json!(e) },
    };

    let mut tasks = state.lock().await;
    let pending = pending_wakeups(&tasks);
    let today = fired_today(&tasks, now);
    if pending >= wakeup_max_pending() {
        return ToolOutput {
            ok: false,
            content: serde_json::json!(format!(
                "pending-wakeup cap reached ({pending}/{}) — cancel one (list_wakeups → cancel_wakeup) or let one fire first",
                wakeup_max_pending()
            )),
        };
    }
    if today >= wakeup_daily_cap() {
        return ToolOutput {
            ok: false,
            content: serde_json::json!(format!(
                "daily wakeup cap reached ({today}/{}) — resumes at the next UTC day; store an intention for anything that must survive",
                wakeup_daily_cap()
            )),
        };
    }

    let task = ScheduledTask {
        id:         unique_id(),
        cron:       String::new(),
        prompt:     note,
        session_id: None, // wakeups always fire on the root session — the node agent's thread
        created_at: now,
        last_run:   None,
        at:         Some(fire),
        wakeup:     true,
    };
    let id = task.id.clone();
    tasks.push(task);
    save_schedules(path, &tasks);

    ToolOutput {
        ok:      true,
        content: serde_json::json!({
            "wakeup_id":     id,
            "fires_at":      rfc3339(fire),
            "fires_in_secs": fire.saturating_sub(now),
            "pending":       pending + 1,
            "fired_today":   today,
            "daily_cap":     wakeup_daily_cap(),
        }),
    }
}

async fn handle_list_wakeups(state: &SchedulerState) -> ToolOutput {
    let tasks = state.lock().await;
    let now = now_secs();
    let pending: Vec<serde_json::Value> = tasks
        .iter()
        .filter(|t| t.wakeup && t.last_run.is_none())
        .map(|t| serde_json::json!({
            "id":            t.id,
            "note":          t.prompt,
            "fires_at":      t.at.map(rfc3339),
            "fires_in_secs": t.at.map(|a| a.saturating_sub(now)),
            "scheduled_at":  rfc3339(t.created_at),
        }))
        .collect();
    let fired: Vec<serde_json::Value> = tasks
        .iter()
        .filter(|t| t.wakeup && t.last_run.is_some())
        .map(|t| serde_json::json!({
            "id":       t.id,
            "note":     t.prompt,
            "fired_at": t.last_run.map(rfc3339),
        }))
        .collect();
    ToolOutput {
        ok:      true,
        content: serde_json::json!({
            "pending":       pending,
            "recently_fired": fired,
            "fired_today":   fired_today(&tasks, now),
            "daily_cap":     wakeup_daily_cap(),
            "pending_cap":   wakeup_max_pending(),
        }),
    }
}

async fn handle_cancel_wakeup(
    state: &SchedulerState,
    path:  &PathBuf,
    args:  &serde_json::Value,
) -> ToolOutput {
    let id = match args["wakeup_id"].as_str() {
        Some(i) => i.to_string(),
        None => return ToolOutput { ok: false, content: serde_json::json!("wakeup_id is required") },
    };
    let mut tasks = state.lock().await;
    let before = tasks.len();
    // Only pending wakeups are cancellable — fired entries are the cap ledger, and
    // recurring crons belong to cancel_schedule.
    tasks.retain(|t| !(t.wakeup && t.last_run.is_none() && t.id == id));
    if tasks.len() == before {
        return ToolOutput {
            ok:      false,
            content: serde_json::json!(format!("no pending wakeup '{id}' (already fired, cancelled, or a cron schedule — see list_wakeups)")),
        };
    }
    save_schedules(path, &tasks);
    ToolOutput { ok: true, content: serde_json::json!({ "cancelled": id }) }
}

#[cfg(test)]
mod wakeup_tests {
    use super::*;

    fn wk(id: &str, at: Option<u64>, last_run: Option<u64>, wakeup: bool) -> ScheduledTask {
        ScheduledTask {
            id: id.into(),
            cron: String::new(),
            prompt: "note".into(),
            session_id: None,
            created_at: 1_000,
            last_run,
            at,
            wakeup,
        }
    }

    #[test]
    fn resolve_fire_at_delay_and_rfc3339_and_epoch() {
        let now = 1_751_800_000u64; // 2025-07-06-ish
        assert_eq!(resolve_fire_at(&serde_json::json!({"delay_secs": 3600}), now), Ok(now + 3600));
        assert_eq!(resolve_fire_at(&serde_json::json!({"at": now + 7200}), now), Ok(now + 7200));
        let rfc = rfc3339(now + 7200);
        assert_eq!(resolve_fire_at(&serde_json::json!({"at": rfc}), now), Ok(now + 7200));
    }

    #[test]
    fn resolve_fire_at_rejects_bad_shapes() {
        let now = 1_751_800_000u64;
        assert!(resolve_fire_at(&serde_json::json!({}), now).is_err());
        assert!(resolve_fire_at(&serde_json::json!({"delay_secs": 60, "at": now + 900}), now).is_err());
        assert!(resolve_fire_at(&serde_json::json!({"at": "next tuesday"}), now).is_err());
        // Below the 60s floor and beyond the 90-day horizon both reject.
        assert!(resolve_fire_at(&serde_json::json!({"delay_secs": 5}), now).is_err());
        assert!(resolve_fire_at(&serde_json::json!({"delay_secs": 91 * 86_400}), now).is_err());
    }

    #[test]
    fn caps_count_pending_and_fired_today() {
        let now = 10 * 86_400 + 3_600; // day 10, 01:00 UTC
        let tasks = vec![
            wk("a", Some(now + 60), None, true),               // pending
            wk("b", Some(now - 60), Some(now - 30), true),     // fired today
            wk("c", Some(now - 90_000), Some(now - 90_000), true), // fired yesterday
            wk("d", None, Some(now - 30), false),              // cron task — never counted
        ];
        assert_eq!(pending_wakeups(&tasks), 1);
        assert_eq!(fired_today(&tasks, now), 1);
    }

    #[test]
    fn prune_drops_only_stale_fired_wakeups() {
        let now = 1_000_000u64;
        let mut tasks = vec![
            wk("fresh", Some(now - 10), Some(now - 5), true),
            wk("stale", Some(now - 200_000), Some(now - 180_000), true), // > 48h
            wk("pending", Some(now + 500), None, true),
            wk("cron", None, Some(now - 180_000), false), // cron history untouched
        ];
        assert!(prune_fired_wakeups(&mut tasks, now));
        let ids: Vec<&str> = tasks.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(ids, vec!["fresh", "pending", "cron"]);
    }

    #[test]
    fn frame_carries_provenance_and_late_marker() {
        let now = 2_000_000u64;
        let on_time = wk("w1", Some(now - 30), None, true);
        let framed = wakeup_frame(&on_time, now);
        assert!(framed.starts_with("[wakeup w1 — a note you scheduled for yourself at "));
        assert!(framed.ends_with("]: note"));
        assert!(!framed.contains("late"));

        let overdue = wk("w2", Some(now - 3_600), None, true);
        assert!(wakeup_frame(&overdue, now).contains("fired 60m late"));
    }

    #[test]
    fn old_jsonl_lines_load_without_new_fields() {
        // Additive serde: a pre-wakeup schedules.jsonl line must deserialize.
        let old = r#"{"id":"sched_1","cron":"0 0 8 * * *","prompt":"p","session_id":null,"created_at":1,"last_run":null}"#;
        let t: ScheduledTask = serde_json::from_str(old).unwrap();
        assert_eq!(t.at, None);
        assert!(!t.wakeup);
    }
}

#[cfg(test)]
mod tests {
    use super::unique_id;
    use std::collections::HashSet;

    #[test]
    fn unique_id_does_not_collide_within_a_burst() {
        // The old secs^subsec_nanos XOR collided on same-second creates; the
        // monotonic counter must make a tight burst of ids all distinct.
        let ids: HashSet<String> = (0..1000).map(|_| unique_id()).collect();
        assert_eq!(ids.len(), 1000, "all generated schedule ids must be unique");
        assert!(ids.iter().all(|id| id.starts_with("sched_")));
    }
}
