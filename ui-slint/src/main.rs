// ApexOS-RS: Slint native UI
//
// Thread model:
//   main thread — Slint event loop (never use #[tokio::main])
//   tokio pool  — WebSocket I/O
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
// VecModel is !Send, so it must only be touched on the Slint main thread.
// invoke_from_event_loop closures run on the main thread and can access this.
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

// ── App state (shared with WS task via Arc<Mutex>) ───────────────────────────
#[derive(Default)]
struct AppState {
    session_id: Option<u64>,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;

    let ui = AppWindow::new()?;

    // Build message model and register it
    let messages: Rc<slint::VecModel<MessageItem>> = Rc::new(slint::VecModel::default());
    ui.set_messages(slint::ModelRc::from(messages.clone()));
    MESSAGES.with(|m| *m.borrow_mut() = Some(messages.clone()));

    let state = Arc::new(Mutex::new(AppState::default()));

    // Outbound WS channel
    let (tx, mut rx) = mpsc::unbounded_channel::<String>();

    // ── WS task ──────────────────────────────────────────────────────────────
    let ui_weak = ui.as_weak();
    let state_ws = state.clone();
    rt.spawn(async move {
        let url = std::env::var("AGENTD_WS")
            .unwrap_or_else(|_| "ws://localhost:8787/ws".to_string());

        eprintln!("[ui-slint] connecting to {url}");

        let (ws, _) = match connect_async(&url).await {
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
                // If the card is already in the list, mark it awaiting
                if let Some(row) = find_tool_row(&call_id) {
                    update_tool_row(row, |item| item.awaiting_approval = true);
                } else {
                    // Card not yet pushed (rare race) — create one
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

        _ => {}
    }
}
