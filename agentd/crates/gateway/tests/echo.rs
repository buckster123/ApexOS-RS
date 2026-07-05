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
        cache:                Arc::new(tokio::sync::RwLock::new(apexos_agent::CacheConfig::default())),
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
        liveness:             apexos_gateway::new_liveness_map(),
        sensor_profile:       Arc::new(std::sync::RwLock::new("standard".into())),
        sensor_config_path:   std::path::PathBuf::from("/dev/null"),
        pairing:              Arc::new(std::sync::Mutex::new(None)),
        node_id:              Arc::new("test-node".into()),
        mesh_sessions:        Arc::new(std::sync::Mutex::new(HashMap::new())),
        mesh_sessions_path:   PathBuf::from("."),
        mesh_unread:          Arc::new(std::sync::Mutex::new(HashMap::new())),
        mesh_unread_path:     PathBuf::from("."),
        fed_stats:            Arc::new(std::sync::Mutex::new(HashMap::new())),
        fed_stats_path:       PathBuf::from("."),
        consolidate_tx:       tokio::sync::mpsc::channel(1).0,
        spawn_tx:             tokio::sync::mpsc::channel(1).0,
        mesh_memory_tx:       tokio::sync::mpsc::channel(1).0,
        capabilities:         Arc::new(tokio::sync::RwLock::new(serde_json::Value::Null)),
        vast_state:           VastState::new(),
        session_bindings:     Arc::new(std::sync::Mutex::new(HashMap::new())),
        persona_sessions:     Arc::new(std::sync::Mutex::new(HashMap::new())),
        identities:           Arc::new(tokio::sync::RwLock::new(apexos_core::Identities::default())),
        pin_lockouts:         Arc::new(std::sync::Mutex::new(HashMap::new())),
        sessions:             Arc::new(std::sync::Mutex::new(apexos_gateway::SessionStore::default())),
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
        matches!(event, Event::UserPrompt { session: SessionId(1), ref text, .. } if text == "hello"),
        "unexpected event: {response}"
    );
}

#[tokio::test]
async fn user_prompt_with_image_is_shimmed_and_echoed() {
    // A valid 1×1 PNG — the gateway runs it through the real vision shim.
    const PNG_1X1_B64: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGP4z8DwHwAFAAH/iZk9HQAAAABJRU5ErkJggg==";

    let (bus_actor, handle, bcast) = Bus::new(SystemState::default());
    tokio::spawn(bus_actor.run());

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let state = make_state(handle, bcast);
    tokio::spawn(async move { axum::serve(listener, router(state)).await.unwrap() });

    let (mut ws, _) = connect_async(format!("ws://{}/ws", addr)).await.unwrap();
    tokio::time::sleep(Duration::from_millis(20)).await;

    // user_prompt carrying a raw b64 image ref → gateway shims it → the echoed
    // event carries a prepared {media_type,data} image block.
    let frame = serde_json::json!({
        "type": "user_prompt",
        "text": "what is this?",
        "images": [ { "b64": PNG_1X1_B64 } ],
    }).to_string();
    ws.send(Message::Text(frame.into())).await.unwrap();

    let response = recv_event(&mut ws).await;
    let val: serde_json::Value = serde_json::from_str(&response).unwrap();
    assert_eq!(val["type"], "user_prompt");
    assert_eq!(val["text"], "what is this?");
    let images = val["images"].as_array().expect("prepared images array");
    assert_eq!(images.len(), 1, "one image, shimmed");
    assert!(images[0]["media_type"].as_str().unwrap().starts_with("image/"));
    assert!(!images[0]["data"].as_str().unwrap().is_empty(), "carries prepared b64");
}

#[tokio::test]
async fn global_events_reach_all_clients() {
    let (bus_actor, handle, bcast) = Bus::new(SystemState::default());
    tokio::spawn(bus_actor.run());
    let bcast_tx = bcast.clone();

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let state = make_state(handle, bcast);
    tokio::spawn(async move { axum::serve(listener, router(state)).await.unwrap() });

    let (mut ws1, _) = connect_async(format!("ws://{}/ws", addr)).await.unwrap();
    let (mut ws2, _) = connect_async(format!("ws://{}/ws", addr)).await.unwrap();

    // Yield so both write tasks subscribe to broadcast before the event fires.
    tokio::time::sleep(Duration::from_millis(20)).await;

    // A session-less (global) status event must reach EVERY connected client.
    bcast_tx.send(Event::PeerSeen { node_id: "n1".into(), ip: "10.0.0.2".into() }).unwrap();

    let (r1, r2) = tokio::join!(recv_event(&mut ws1), recv_event(&mut ws2));
    for r in [r1, r2] {
        let event: Event = serde_json::from_str(&r).unwrap();
        assert!(matches!(event, Event::PeerSeen { .. }), "unexpected event: {r}");
    }
}

#[tokio::test]
async fn session_scoped_events_are_filtered_per_client() {
    // The fix for the multi-client splicing bug: a session-scoped event reaches
    // only the socket bound to that session; a global event still reaches all.
    let (bus_actor, handle, bcast) = Bus::new(SystemState::default());
    tokio::spawn(bus_actor.run());
    let bcast_tx = bcast.clone();

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let state = make_state(handle, bcast);
    tokio::spawn(async move { axum::serve(listener, router(state)).await.unwrap() });

    // ws1 → session 1, ws2 → session 2 (next_session_id starts at 1, connect order fixed).
    let (mut ws1, _) = connect_async(format!("ws://{}/ws", addr)).await.unwrap();
    let (mut ws2, _) = connect_async(format!("ws://{}/ws", addr)).await.unwrap();
    tokio::time::sleep(Duration::from_millis(20)).await;

    // A delta for session 1, then a global event. ws1 sees the delta first; ws2
    // must SKIP the session-1 delta and see the global event as its first frame.
    bcast_tx.send(Event::AgentText { session: SessionId(1), delta: "for-ws1".into() }).unwrap();
    bcast_tx.send(Event::PeerSeen { node_id: "n1".into(), ip: "10.0.0.2".into() }).unwrap();

    let r1: Event = serde_json::from_str(&recv_event(&mut ws1).await).unwrap();
    assert!(matches!(r1, Event::AgentText { session: SessionId(1), .. }),
        "ws1 should receive its own session's delta first, got: {r1:?}");

    let r2: Event = serde_json::from_str(&recv_event(&mut ws2).await).unwrap();
    assert!(matches!(r2, Event::PeerSeen { .. }),
        "ws2 must skip session 1's delta and receive only the global event, got: {r2:?}");
}

/// Spawn a gateway server on an ephemeral port; returns its http base.
async fn spawn_gateway() -> String {
    let (bus_actor, handle, bcast) = Bus::new(SystemState::default());
    tokio::spawn(bus_actor.run());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let state = make_state(handle, bcast);
    tokio::spawn(async move { axum::serve(listener, router(state)).await.unwrap() });
    tokio::time::sleep(Duration::from_millis(20)).await;
    format!("http://{addr}")
}

#[tokio::test]
async fn pairing_claim_exchanges_creds_and_is_single_use() {
    let base = spawn_gateway().await;
    let http = reqwest::Client::new();

    // start → a 6-digit code
    let started: serde_json::Value = http.post(format!("{base}/api/mesh/pair/start"))
        .send().await.unwrap().json().await.unwrap();
    let code = started["code"].as_str().unwrap().to_string();
    assert_eq!(code.len(), 6);

    // claim with the right code → 200 + our node creds, requester registered reciprocally
    let claim = serde_json::json!({
        "code": code, "node_id": "peer-x",
        "ws_url": "ws://10.0.0.9:8787", "token": "peer-token",
    });
    let resp = http.post(format!("{base}/api/mesh/pair/claim")).json(&claim).send().await.unwrap();
    assert_eq!(resp.status(), 200);
    let claimed: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(claimed["ok"], true);
    assert_eq!(claimed["node_id"], "test-node");

    // the requester is now a saved peer (token redacted → has_token:true)
    let peers: serde_json::Value = http.get(format!("{base}/api/mesh/peers"))
        .send().await.unwrap().json().await.unwrap();
    assert!(peers["peers"].as_array().unwrap().iter()
        .any(|p| p["node_id"] == "peer-x" && p["has_token"] == true),
        "requester should be registered with a token");

    // same code again → consumed (single-use) → 403
    let again = http.post(format!("{base}/api/mesh/pair/claim")).json(&claim).send().await.unwrap();
    assert_eq!(again.status(), 403, "code must be single-use");
}

#[tokio::test]
async fn pairing_claim_wrong_code_rejected() {
    let base = spawn_gateway().await;
    let http = reqwest::Client::new();
    http.post(format!("{base}/api/mesh/pair/start")).send().await.unwrap();
    // "BADCODE" can never equal a 6-digit numeric code → guaranteed rejection.
    let resp = http.post(format!("{base}/api/mesh/pair/claim"))
        .json(&serde_json::json!({ "code": "BADCODE", "node_id": "x", "ws_url": "ws://10.0.0.9:8787", "token": "t" }))
        .send().await.unwrap();
    assert_eq!(resp.status(), 403);
}
