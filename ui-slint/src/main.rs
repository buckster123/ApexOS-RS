// ApexOS-RS: Slint native UI
//
// Thread model:
//   main thread — Slint event loop (never use #[tokio::main])
//   tokio pool  — WebSocket I/O + HTTP polling
//
// Cross-thread bridge:
//   slint::invoke_from_event_loop() queues closures to the Slint thread.
//   VecModel mutations happen on the Slint thread via MESSAGES thread-local.
//   Outbound WS messages go through an unbounded mpsc channel.

slint::include_modules!();

use slint::Model; // row_count / row_data / set_row_data on VecModel
use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;
use tokio_tungstenite::{connect_async, tungstenite::Message};

// ── Thread-local model access ─────────────────────────────────────────────────
thread_local! {
    static MESSAGES: RefCell<Option<Rc<slint::VecModel<MessageItem>>>> =
        const { RefCell::new(None) };
}

fn push_message(item: MessageItem) {
    MESSAGES.with(|m| {
        if let Some(model) = m.borrow().as_ref() {
            model.push(item);
        }
    });
}

fn update_last_agent_message(delta: &str) {
    MESSAGES.with(|m| {
        if let Some(model) = m.borrow().as_ref() {
            let len = model.row_count();
            if len > 0 {
                let mut last = model.row_data(len - 1).unwrap();
                if last.role.as_str() == "agent" {
                    let new_text = last.text.as_str().to_string() + delta;
                    last.text = new_text.into();
                    model.set_row_data(len - 1, last);
                }
            }
        }
    });
}

fn finish_last_agent_message() {
    MESSAGES.with(|m| {
        if let Some(model) = m.borrow().as_ref() {
            let len = model.row_count();
            if len > 0 {
                let mut last = model.row_data(len - 1).unwrap();
                if last.role.as_str() == "agent" {
                    last.streaming = false;
                    model.set_row_data(len - 1, last);
                }
            }
        }
    });
}

fn find_tool_row(call_id: &str) -> Option<usize> {
    MESSAGES.with(|m| {
        if let Some(model) = m.borrow().as_ref() {
            for i in 0..model.row_count() {
                if let Some(item) = model.row_data(i) {
                    if item.role.as_str() == "tool" && item.call_id.as_str() == call_id {
                        return Some(i);
                    }
                }
            }
        }
        None
    })
}

fn update_tool_row(row: usize, f: impl FnOnce(&mut MessageItem)) {
    MESSAGES.with(|m| {
        if let Some(model) = m.borrow().as_ref() {
            if let Some(mut item) = model.row_data(row) {
                f(&mut item);
                model.set_row_data(row, item);
            }
        }
    });
}

// ── SysStats helpers ──────────────────────────────────────────────────────────

fn empty_sys_stats() -> SysStats {
    SysStats {
        cpu_pct:   0.0,
        ram_pct:   0.0,
        disk_pct:  0.0,
        iaq_score: 0.0,
        iaq_label: "—".into(),
        temp_c:    0.0,
        online:    false,
    }
}

fn iaq_label(score: f32) -> &'static str {
    match score as u32 {
        0..=50   => "Good",
        51..=100 => "Moderate",
        101..=150 => "Unhealthy (Sensitive)",
        151..=200 => "Unhealthy",
        201..=300 => "Very Unhealthy",
        _         => "Hazardous",
    }
}

// Derive HTTP base from WS URL: "ws://host:port/ws" → "http://host:port"
fn ws_to_http(ws_url: &str) -> String {
    ws_url
        .trim_end_matches("/ws")
        .replacen("ws://", "http://", 1)
        .replacen("wss://", "https://", 1)
}

// POST /api/run to fetch CPU / RAM / disk percentages from the server.
// Returns (cpu_pct, ram_pct, disk_pct) on success.
async fn fetch_sys_stats(client: &reqwest::Client, base_url: &str) -> Option<(f32, f32, f32)> {
    // One command: mem_pct on line 1, disk_pct on line 2, nproc on line 3, load_1m on line 4
    let cmd = concat!(
        "awk '/^MemTotal/{t=$2}/^MemAvailable/{a=$2}END{printf \"%.0f\\n\",100*(t-a)/t}' /proc/meminfo",
        " && df / | awk 'NR==2{gsub(/%/,\"\",$5);print $5}'",
        " && nproc",
        " && awk '{print $1}' /proc/loadavg",
    );
    let resp = client
        .post(format!("{base_url}/api/run"))
        .json(&serde_json::json!({"command": cmd}))
        .timeout(std::time::Duration::from_secs(8))
        .send()
        .await
        .ok()?;
    let body: Value = resp.json().await.ok()?;
    if body["ok"].as_bool() != Some(true) {
        return None;
    }
    let stdout = body["stdout"].as_str()?;
    let lines: Vec<&str> = stdout.lines().collect();
    let ram_pct:  f32 = lines.first()?.trim().parse().ok()?;
    let disk_pct: f32 = lines.get(1)?.trim().parse().ok()?;
    let nproc:    f32 = lines.get(2)?.trim().parse::<f32>().ok()?.max(1.0);
    let loadavg:  f32 = lines.get(3)?.trim().parse().ok()?;
    let cpu_pct = (loadavg / nproc * 100.0).min(100.0);
    Some((cpu_pct, ram_pct, disk_pct))
}

// ── App state ─────────────────────────────────────────────────────────────────
#[derive(Default)]
struct AppState {
    session_id: Option<u64>,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;

    let ui = AppWindow::new()?;

    // Message model
    let messages: Rc<slint::VecModel<MessageItem>> = Rc::new(slint::VecModel::default());
    ui.set_messages(slint::ModelRc::from(messages.clone()));
    MESSAGES.with(|m| *m.borrow_mut() = Some(messages.clone()));

    // Initial sys stats (all zeros, offline)
    ui.set_sys_stats(empty_sys_stats());

    let state = Arc::new(Mutex::new(AppState::default()));

    // Outbound WS channel
    let (tx, mut rx) = mpsc::unbounded_channel::<String>();

    let ws_url = std::env::var("AGENTD_WS")
        .unwrap_or_else(|_| "ws://localhost:8787/ws".to_string());
    let http_base = ws_to_http(&ws_url);

    // ── WS task ──────────────────────────────────────────────────────────────
    let ui_weak = ui.as_weak();
    let state_ws = state.clone();
    rt.spawn(async move {
        eprintln!("[ui-slint] connecting to {ws_url}");

        let (ws, _) = match connect_async(&ws_url).await {
            Ok(pair) => pair,
            Err(e) => {
                eprintln!("[ui-slint] WS connect failed: {e}");
                let w = ui_weak.clone();
                slint::invoke_from_event_loop(move || {
                    if let Some(ui) = w.upgrade() {
                        ui.set_status("Connection failed — is agentd running?".into());
                    }
                })
                .ok();
                return;
            }
        };

        let (mut write, mut read) = ws.split();

        let init = serde_json::json!({"type": "session_init"});
        write.send(Message::Text(init.to_string().into())).await.ok();

        {
            let w = ui_weak.clone();
            slint::invoke_from_event_loop(move || {
                if let Some(ui) = w.upgrade() {
                    ui.set_status("Connected".into());
                }
            })
            .ok();
        }

        loop {
            tokio::select! {
                msg = read.next() => {
                    match msg {
                        Some(Ok(Message::Text(text))) => {
                            if let Ok(ev) = serde_json::from_str::<Value>(&text) {
                                dispatch_event(ui_weak.clone(), ev, state_ws.clone());
                            }
                        }
                        Some(Ok(_)) => {}
                        _ => {
                            eprintln!("[ui-slint] WS disconnected");
                            let w = ui_weak.clone();
                            slint::invoke_from_event_loop(move || {
                                if let Some(ui) = w.upgrade() {
                                    ui.set_status("Disconnected".into());
                                }
                            })
                            .ok();
                            break;
                        }
                    }
                }
                out = rx.recv() => {
                    if let Some(text) = out {
                        write.send(Message::Text(text.into())).await.ok();
                    }
                }
            }
        }
    });

    // ── System stats polling (every 5 s) ─────────────────────────────────────
    let ui_weak_poll = ui.as_weak();
    rt.spawn(async move {
        let client = reqwest::Client::new();
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(5));
        loop {
            interval.tick().await;
            if let Some((cpu, ram, disk)) = fetch_sys_stats(&client, &http_base).await {
                let w = ui_weak_poll.clone();
                slint::invoke_from_event_loop(move || {
                    if let Some(ui) = w.upgrade() {
                        let mut s = ui.get_sys_stats();
                        s.cpu_pct  = cpu;
                        s.ram_pct  = ram;
                        s.disk_pct = disk;
                        s.online   = true;
                        ui.set_sys_stats(s);
                    }
                })
                .ok();
            }
        }
    });

    // ── approve / reject callbacks ────────────────────────────────────────────
    let tx_approve = tx.clone();
    ui.on_approve_tool(move |call_id| {
        if let Some(row) = find_tool_row(call_id.as_str()) {
            update_tool_row(row, |item| item.awaiting_approval = false);
        }
        let payload = serde_json::json!({
            "type": "user_approval",
            "call_id": call_id.as_str(),
            "approved": true
        })
        .to_string();
        tx_approve.send(payload).ok();
    });

    let tx_reject = tx.clone();
    ui.on_reject_tool(move |call_id| {
        if let Some(row) = find_tool_row(call_id.as_str()) {
            update_tool_row(row, |item| {
                item.awaiting_approval = false;
                item.tool_status = "error".into();
            });
        }
        let payload = serde_json::json!({
            "type": "user_approval",
            "call_id": call_id.as_str(),
            "approved": false
        })
        .to_string();
        tx_reject.send(payload).ok();
    });

    // ── send-message callback ─────────────────────────────────────────────────
    let tx_send = tx.clone();
    let messages_send = messages.clone();
    ui.on_send_message(move |text| {
        if text.is_empty() {
            return;
        }
        messages_send.push(MessageItem {
            role: "user".into(),
            text: text.clone(),
            streaming: false,
            call_id: "".into(),
            tool_name: "".into(),
            tool_args: "".into(),
            tool_output: "".into(),
            tool_status: "".into(),
            awaiting_approval: false,
        });
        let payload = serde_json::json!({"type": "user_prompt", "text": text.as_str()}).to_string();
        tx_send.send(payload).ok();
    });

    ui.run()?;
    Ok(())
}

/// Queue a UI update on the Slint main thread for the given agentd event.
fn dispatch_event(
    ui_weak: slint::Weak<AppWindow>,
    ev: Value,
    state: Arc<Mutex<AppState>>,
) {
    let ev_type = ev["type"].as_str().unwrap_or("").to_string();

    match ev_type.as_str() {
        "hello" => {
            let id = ev["session_id"].as_u64();
            let w = ui_weak.clone();
            slint::invoke_from_event_loop(move || {
                if let Some(ui) = w.upgrade() {
                    if let Some(id) = id {
                        state.lock().unwrap().session_id = Some(id);
                        ui.set_status(format!("Session {id}").into());
                    }
                }
            })
            .ok();
        }

        "turn_started" => {
            let w = ui_weak.clone();
            slint::invoke_from_event_loop(move || {
                if let Some(ui) = w.upgrade() {
                    ui.set_agent_busy(true);
                    push_message(MessageItem {
                        role: "agent".into(),
                        text: "".into(),
                        streaming: true,
                        call_id: "".into(),
                        tool_name: "".into(),
                        tool_args: "".into(),
                        tool_output: "".into(),
                        tool_status: "".into(),
                        awaiting_approval: false,
                    });
                    ui.invoke_scroll_to_bottom();
                }
            })
            .ok();
        }

        "agent_text" => {
            let delta = ev["delta"].as_str().unwrap_or("").to_string();
            if delta.is_empty() {
                return;
            }
            let w = ui_weak.clone();
            slint::invoke_from_event_loop(move || {
                if let Some(ui) = w.upgrade() {
                    update_last_agent_message(&delta);
                    ui.invoke_scroll_to_bottom();
                }
            })
            .ok();
        }

        "turn_complete" => {
            let w = ui_weak.clone();
            slint::invoke_from_event_loop(move || {
                if let Some(ui) = w.upgrade() {
                    finish_last_agent_message();
                    ui.set_agent_busy(false);
                }
            })
            .ok();
        }

        "tool_requested" => {
            let call_id   = ev["call_id"].as_str().unwrap_or("").to_string();
            let tool_name = ev["name"].as_str().unwrap_or("").to_string();
            let tool_args = ev["input"]
                .as_object()
                .map(|o| serde_json::to_string_pretty(o).unwrap_or_default())
                .unwrap_or_default();
            let w = ui_weak.clone();
            slint::invoke_from_event_loop(move || {
                if let Some(ui) = w.upgrade() {
                    push_message(MessageItem {
                        role: "tool".into(),
                        text: "".into(),
                        streaming: false,
                        call_id: call_id.into(),
                        tool_name: tool_name.into(),
                        tool_args: tool_args.into(),
                        tool_output: "".into(),
                        tool_status: "running".into(),
                        awaiting_approval: false,
                    });
                    ui.invoke_scroll_to_bottom();
                }
            })
            .ok();
        }

        "tool_result" => {
            let call_id = ev["call_id"].as_str().unwrap_or("").to_string();
            let output  = ev["output"].as_str().unwrap_or("").to_string();
            slint::invoke_from_event_loop(move || {
                if let Some(row) = find_tool_row(&call_id) {
                    update_tool_row(row, |item| {
                        item.tool_output = output.into();
                        item.tool_status = "done".into();
                    });
                }
            })
            .ok();
        }

        "approval_pending" => {
            let call_id   = ev["call_id"].as_str().unwrap_or("").to_string();
            let tool_name = ev["name"].as_str().unwrap_or("").to_string();
            slint::invoke_from_event_loop(move || {
                if let Some(row) = find_tool_row(&call_id) {
                    update_tool_row(row, |item| item.awaiting_approval = true);
                } else {
                    push_message(MessageItem {
                        role: "tool".into(),
                        text: "".into(),
                        streaming: false,
                        call_id: call_id.into(),
                        tool_name: tool_name.into(),
                        tool_args: "".into(),
                        tool_output: "".into(),
                        tool_status: "running".into(),
                        awaiting_approval: true,
                    });
                }
            })
            .ok();
        }

        // IAQ + temperature from BME688 sensor bridge
        "sensor_reading" => {
            let reading = &ev["reading"];
            if reading["kind"].as_str() == Some("air_quality") {
                let iaq  = reading["iaq"].as_f64().unwrap_or(0.0) as f32;
                let temp = reading["temperature_c"].as_f64().unwrap_or(0.0) as f32;
                let label = iaq_label(iaq).to_string();
                let w = ui_weak.clone();
                slint::invoke_from_event_loop(move || {
                    if let Some(ui) = w.upgrade() {
                        let mut s = ui.get_sys_stats();
                        s.iaq_score = iaq;
                        s.iaq_label = label.into();
                        s.temp_c    = temp;
                        ui.set_sys_stats(s);
                    }
                })
                .ok();
            }
        }

        _ => {}
    }
}
