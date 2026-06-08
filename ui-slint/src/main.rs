// ApexOS-RS: Slint native UI
//
// Architecture: thin WS renderer. agentd is unchanged. This binary connects to
// ws://localhost:8787/ws (or AGENTD_WS env), subscribes to the Event stream,
// and renders it natively via Slint + KMS/DRM.
//
// Thread model:
//   main thread  — Slint event loop (required by Slint)
//   tokio pool   — WebSocket I/O, HTTP API calls
//
// Bridge: tokio tasks push updates via slint::invoke_from_event_loop().

slint::include_modules!();

use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use std::sync::{Arc, Mutex};
use tokio_tungstenite::{connect_async, tungstenite::Message};

#[derive(Default)]
struct AppState {
    session_id: Option<u64>,
    agent_text: String,
    connected: bool,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Tokio runtime on background threads — main thread stays for Slint.
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;

    let ui = AppWindow::new()?;
    let state = Arc::new(Mutex::new(AppState::default()));

    // ── WS client loop ────────────────────────────────────────────────
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
                return;
            }
        };

        let (mut write, mut read) = ws.split();

        // Send session_init (agentd assigns a session ID back)
        let init = serde_json::json!({"type": "session_init"});
        write.send(Message::Text(init.to_string().into())).await.ok();

        {
            let mut s = state_ws.lock().unwrap();
            s.connected = true;
        }

        let ui_weak2 = ui_weak.clone();
        slint::invoke_from_event_loop(move || {
            if let Some(ui) = ui_weak2.upgrade() {
                ui.set_status("Connected to agentd".into());
            }
        })
        .ok();

        while let Some(Ok(msg)) = read.next().await {
            if let Message::Text(text) = msg {
                if let Ok(ev) = serde_json::from_str::<Value>(&text) {
                    let ui_weak3 = ui_weak.clone();
                    let state_ev = state_ws.clone();
                    slint::invoke_from_event_loop(move || {
                        if let Some(ui) = ui_weak3.upgrade() {
                            handle_event(&ui, &ev, &state_ev);
                        }
                    })
                    .ok();
                }
            }
        }

        eprintln!("[ui-slint] WS disconnected");
    });

    ui.run()?;
    Ok(())
}

/// Dispatch an agentd Event to the Slint UI.
/// Called on the Slint main thread — safe to call any ui.set_*() here.
fn handle_event(ui: &AppWindow, ev: &Value, state: &Arc<Mutex<AppState>>) {
    let ev_type = ev["type"].as_str().unwrap_or("");
    match ev_type {
        "hello" => {
            if let Some(id) = ev["session_id"].as_u64() {
                state.lock().unwrap().session_id = Some(id);
                ui.set_status(format!("Session {id}").into());
            }
        }
        "agent_text" => {
            if let Some(delta) = ev["delta"].as_str() {
                let mut s = state.lock().unwrap();
                s.agent_text.push_str(delta);
                ui.set_agent_text(s.agent_text.clone().into());
            }
        }
        "turn_complete" => {
            ui.set_agent_busy(false);
        }
        "turn_started" => {
            state.lock().unwrap().agent_text.clear();
            ui.set_agent_text("".into());
            ui.set_agent_busy(true);
        }
        _ => {}
    }
}
