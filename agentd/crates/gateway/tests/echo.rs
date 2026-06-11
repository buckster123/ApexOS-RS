use apexos_core::{ActionId, Bus, Event, SessionId, SystemState};
use apexos_gateway::{router, GatewayState};
use apexos_plugins::VastState;
use futures_util::{SinkExt, StreamExt};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, atomic::AtomicU64};
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::sync::Mutex;
use tokio_tungstenite::connect_async;
use tungstenite::Message;

fn make_state(handle: apexos_core::BusHandle, bcast: tokio::sync::broadcast::Sender<Event>) -> GatewayState {
    use apexos_plugins::{PolicyConfig, PolicyEngine};
    GatewayState {
        bus:                  handle,
        bcast,
        api_key:              Arc::new(tokio::sync::RwLock::new(String::new())),
        oai_api_key:          Arc::new(tokio::sync::RwLock::new(String::new())),
        model:                Arc::new(tokio::sync::RwLock::new("claude-opus-4-8".into())),
        backend:              Arc::new(tokio::sync::RwLock::new("anthropic".into())),
        oai_base_url:         Arc::new(tokio::sync::RwLock::new("http://localhost:11434/v1".into())),
        policy_mode:          Arc::new(tokio::sync::RwLock::new("suggest".into())),
        policy_set_tx:        tokio::sync::mpsc::channel(1).0,
        ui_dir:               PathBuf::from("."),
        events_dir:           PathBuf::from("."),
        sessions_dir:         PathBuf::from("."),
        histories:            Arc::new(Mutex::new(HashMap::new())),
        next_session_id:      Arc::new(AtomicU64::new(1)),
        sensor_bridge_token:  Arc::new(String::new()),
        api_token:            Arc::new(String::new()),
        soul_path:            PathBuf::from("."),
        policy_arc:           Arc::new(tokio::sync::RwLock::new(PolicyEngine::new(PolicyConfig::default()))),
        council_start_tx:     tokio::sync::mpsc::channel::<(SessionId, ActionId, serde_json::Value)>(1).0,
        council_butt_in:      Arc::new(tokio::sync::Mutex::new(HashMap::new())),
        council_sessions:     Arc::new(tokio::sync::Mutex::new(Vec::new())),
        council_next_id:      Arc::new(AtomicU64::new(1)),
        peer_registry:        Arc::new(tokio::sync::RwLock::new(
            apexos_gateway::PeerRegistry::load(std::path::Path::new("/dev/null"))
        )),
        node_id:              Arc::new("test-node".into()),
        vast_state:           VastState::new(),
    }
}

/// Receive the next non-session_init Text frame from a WS stream.
async fn recv_event(ws: &mut tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>) -> String {
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            match ws.next().await.unwrap().unwrap() {
                Message::Text(json) => {
                    let val: serde_json::Value = serde_json::from_str(&json).unwrap_or_default();
                    if val["type"].as_str() == Some("session_init") { continue; }
                    break json.to_string();
                }
                _ => continue,
            }
        }
    })
    .await
    .expect("timed out waiting for event")
}

#[tokio::test]
async fn user_prompt_echoes_back() {
    let (bus_actor, handle, bcast) = Bus::new(SystemState::default());
    tokio::spawn(bus_actor.run());

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let state = make_state(handle, bcast);
    tokio::spawn(async move { axum::serve(listener, router(state)).await.unwrap() });

    let (mut ws, _) = connect_async(format!("ws://{}/ws", addr)).await.unwrap();

    // Yield so handle_socket can subscribe to broadcast before we fire events.
    tokio::time::sleep(Duration::from_millis(20)).await;

    // Server assigns session_id=1 (counter starts at 1). Send user_prompt — server
    // injects session_id=1 into the frame.
    ws.send(Message::Text(r#"{"type":"user_prompt","text":"hello"}"#.into())).await.unwrap();

    let response = recv_event(&mut ws).await;
    let event: Event = serde_json::from_str(&response).unwrap();
    assert!(
        matches!(event, Event::UserPrompt { session: SessionId(1), ref text } if text == "hello"),
        "unexpected event: {response}"
    );
}

#[tokio::test]
async fn multiple_clients_both_receive_broadcast() {
    let (bus_actor, handle, bcast) = Bus::new(SystemState::default());
    tokio::spawn(bus_actor.run());

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let state = make_state(handle, bcast);
    tokio::spawn(async move { axum::serve(listener, router(state)).await.unwrap() });

    let (mut ws1, _) = connect_async(format!("ws://{}/ws", addr)).await.unwrap();
    let (mut ws2, _) = connect_async(format!("ws://{}/ws", addr)).await.unwrap();

    // Yield so both write tasks subscribe to broadcast before ws1 fires.
    tokio::time::sleep(Duration::from_millis(20)).await;

    // ws1 gets session_id=1 (first connection). Send user_prompt — server injects 1.
    ws1.send(Message::Text(r#"{"type":"user_prompt","text":"broadcast"}"#.into())).await.unwrap();

    let (r1, r2) = tokio::join!(recv_event(&mut ws1), recv_event(&mut ws2));
    for r in [r1, r2] {
        let event: Event = serde_json::from_str(&r).unwrap();
        assert!(matches!(event, Event::UserPrompt { session: SessionId(1), .. }),
            "unexpected event: {r}");
    }
}
