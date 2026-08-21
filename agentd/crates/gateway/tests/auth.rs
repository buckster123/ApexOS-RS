//! Finding 2: owner-claim login + role-split REST.
use apexos_core::{Bus, Event, SessionId, SystemState};
static IDENTITIES_ENV: std::sync::Mutex<()> = std::sync::Mutex::new(());
use apexos_gateway::{router, GatewayState};
use apexos_plugins::VastState;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, atomic::AtomicU64};
use tokio::net::TcpListener;
use tokio::sync::Mutex;

fn make_state(
    handle: apexos_core::BusHandle,
    bcast: tokio::sync::broadcast::Sender<Event>,
    api_token: &str,
    identities: apexos_core::Identities,
    sessions_dir: PathBuf,
) -> GatewayState {
    use apexos_plugins::{PolicyConfig, PolicyEngine};
    GatewayState {
        bus:                  handle,
        bcast,
        api_key:              Arc::new(tokio::sync::RwLock::new(String::new())),
        oai_keys:             Arc::new(tokio::sync::RwLock::new(apexos_agent::OaiKeyRing::default())),
        model:                Arc::new(tokio::sync::RwLock::new("claude-opus-4-8".into())),
        cache:                Arc::new(tokio::sync::RwLock::new(apexos_agent::CacheConfig::default())),
        backend:              Arc::new(tokio::sync::RwLock::new("anthropic".into())),
        oai_base_url:         Arc::new(tokio::sync::RwLock::new("http://localhost:11434/v1".into())),
        policy_mode:          Arc::new(tokio::sync::RwLock::new("suggest".into())),
        policy_set_tx:        tokio::sync::mpsc::channel(1).0,
        ui_dir:               PathBuf::from("."),
        events_dir:           PathBuf::from("."),
        sessions_dir,
        histories:            Arc::new(Mutex::new(HashMap::new())),
        next_session_id:      Arc::new(AtomicU64::new(1)),
        history_budget:       Arc::new(std::sync::atomic::AtomicUsize::new(120_000)),
        session_idle_gzip_days: Arc::new(std::sync::atomic::AtomicU32::new(0)),
        session_never_delete:   Arc::new(std::sync::atomic::AtomicBool::new(true)),
        mesh_bridge_token: std::sync::Arc::new(String::new()),
        mesh_link: apexos_gateway::mesh_link::MeshLink::new(),
        sensor_bridge_token:  Arc::new(String::new()),
        api_token:            Arc::new(api_token.to_string()),
        soul_path:            PathBuf::from("."),
        policy_arc:           Arc::new(tokio::sync::RwLock::new(PolicyEngine::new(PolicyConfig::default()))),
        council_start_tx:     tokio::sync::mpsc::channel::<(SessionId, apexos_core::ActionId, serde_json::Value)>(1).0,
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
        redeem_flight:        Arc::new(std::sync::Mutex::new(None)),
        node_id:              Arc::new("test-node".into()),
        mesh_sessions:        Arc::new(std::sync::Mutex::new(HashMap::new())),
        mesh_sessions_path:   PathBuf::from("."),
        mesh_unread:          Arc::new(std::sync::Mutex::new(HashMap::new())),
        mesh_unread_path:     PathBuf::from("."),
        fed_stats:            Arc::new(std::sync::Mutex::new(HashMap::new())),
        fed_stats_path:       PathBuf::from("."),
        consolidate_tx:       tokio::sync::mpsc::channel(1).0,
        session_retire_tx:    tokio::sync::mpsc::channel(1).0,
        spawn_tx:             tokio::sync::mpsc::channel(1).0,
        worker_mesh_tx:       tokio::sync::mpsc::channel(1).0,
        mesh_workers_enabled: true,
        mesh_memory_tx:       tokio::sync::mpsc::channel(1).0,
        capabilities:         Arc::new(tokio::sync::RwLock::new(serde_json::Value::Null)),
        vast_state:           VastState::new(),
        session_bindings:     Arc::new(std::sync::Mutex::new(HashMap::new())),
        persona_sessions:     Arc::new(std::sync::Mutex::new(HashMap::new())),
        identities:           Arc::new(tokio::sync::RwLock::new(identities)),
        pin_lockouts:         Arc::new(std::sync::Mutex::new(HashMap::new())),
        sessions:             Arc::new(std::sync::Mutex::new(apexos_gateway::SessionStore::default())),
    }
}

async fn boot(api_token: &str, identities: apexos_core::Identities) -> (String, PathBuf) {
    let (bus_actor, handle, bcast) = Bus::new(SystemState::default());
    tokio::spawn(bus_actor.run());
    let tmp = std::env::temp_dir().join(format!(
        "apex-auth-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _ = std::fs::create_dir_all(&tmp);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let state = make_state(handle, bcast, api_token, identities, tmp.clone());
    tokio::spawn(async move {
        axum::serve(
            listener,
            router(state).into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .unwrap();
    });
    (format!("http://{addr}"), tmp)
}

async fn json_post(url: &str, token: Option<&str>, body: serde_json::Value) -> (u16, serde_json::Value) {
    let mut req = reqwest::Client::new().post(url).json(&body);
    if let Some(t) = token {
        req = req.bearer_auth(t);
    }
    let r = req.send().await.unwrap();
    let status = r.status().as_u16();
    let v = r.json::<serde_json::Value>().await.unwrap_or(serde_json::Value::Null);
    (status, v)
}

#[tokio::test]
async fn unclaimed_login_is_refused_even_on_loopback() {
    let mut ids = apexos_core::Identities::default();
    ids.seed_defaults("/tmp/soul.md");
    let (base, _tmp) = boot("admin-secret", ids).await;
    let (st, v) = json_post(
        &format!("{base}/api/auth/login"),
        None,
        serde_json::json!({ "user_id": "owner", "pin": "" }),
    )
    .await;
    assert_eq!(st, 403, "unclaimed login must 403: {v}");
    assert_eq!(v["error"], "owner_setup_required");
}

#[tokio::test]
async fn setup_then_owner_login_gets_privileged_run() {
    let _g = IDENTITIES_ENV.lock().unwrap();
    let tmp_ids = std::env::temp_dir().join(format!("apex-ids-{}", std::process::id()));
    let id_path = tmp_ids.join("identities.toml");
    let _ = std::fs::create_dir_all(&tmp_ids);
    std::env::set_var("AGENTD_IDENTITIES", id_path.to_str().unwrap());

    let mut ids = apexos_core::Identities::default();
    ids.seed_defaults("/tmp/soul.md");
    let (base, _tmp) = boot("admin-secret", ids).await;

    let (st, v) = json_post(
        &format!("{base}/api/auth/setup"),
        None,
        serde_json::json!({ "pin": "12" }),
    )
    .await;
    assert_eq!(st, 400, "short pin: {v}");

    let (st, v) = json_post(
        &format!("{base}/api/auth/setup"),
        None,
        serde_json::json!({ "pin": "1337" }),
    )
    .await;
    assert_eq!(st, 200, "setup: {v}");
    assert!(v["ok"].as_bool().unwrap());

    let (st, v) = json_post(
        &format!("{base}/api/auth/login"),
        None,
        serde_json::json!({ "user_id": "owner", "pin": "1337" }),
    )
    .await;
    assert_eq!(st, 200, "login: {v}");
    let token = v["token"].as_str().unwrap();
    assert_eq!(v["role"], "owner");

    let (st, v) = json_post(
        &format!("{base}/api/run"),
        Some(token),
        serde_json::json!({ "command": "true" }),
    )
    .await;
    assert_eq!(st, 200, "owner run: {v}");
    assert!(v["ok"].as_bool().unwrap(), "owner /api/run body: {v}");

    std::env::remove_var("AGENTD_IDENTITIES");
    let _ = std::fs::remove_dir_all(&tmp_ids);
}

#[tokio::test]
async fn guest_session_cannot_hit_run() {
    let _g = IDENTITIES_ENV.lock().unwrap();
    let tmp_ids = std::env::temp_dir().join(format!("apex-ids-g-{}", std::process::id()));
    let id_path = tmp_ids.join("identities.toml");
    let _ = std::fs::create_dir_all(&tmp_ids);
    std::env::set_var("AGENTD_IDENTITIES", id_path.to_str().unwrap());

    let mut ids = apexos_core::Identities::default();
    ids.seed_defaults("/tmp/soul.md");
    ids.user_mut("owner").unwrap().set_pin("1337");
    let mut guest = apexos_core::User {
        id: "guest".into(),
        name: "Guest".into(),
        ..Default::default()
    };
    guest.set_pin("4242");
    ids.users.push(guest);
    let (base, _tmp) = boot("admin-secret", ids).await;

    let (_, v) = json_post(
        &format!("{base}/api/auth/login"),
        None,
        serde_json::json!({ "user_id": "guest", "pin": "4242" }),
    )
    .await;
    let token = v["token"].as_str().expect("guest token");
    assert_eq!(v["role"], "user");

    let status = reqwest::Client::new()
        .get(format!("{base}/api/status"))
        .bearer_auth(token)
        .send()
        .await
        .unwrap()
        .status()
        .as_u16();
    assert_eq!(status, 200, "guest may read /api/status");

    let (st, _) = json_post(
        &format!("{base}/api/run"),
        Some(token),
        serde_json::json!({ "command": "id" }),
    )
    .await;
    assert_eq!(st, 403, "guest /api/run must 403");

    let (st, _) = json_post(
        &format!("{base}/api/run"),
        Some("admin-secret"),
        serde_json::json!({ "command": "true" }),
    )
    .await;
    assert_eq!(st, 200, "admin token still reaches /api/run");

    std::env::remove_var("AGENTD_IDENTITIES");
    let _ = std::fs::remove_dir_all(&tmp_ids);
}

#[tokio::test]
async fn create_user_refuses_when_persist_blocked() {
    let _g = IDENTITIES_ENV.lock().unwrap();
    let tmp_ids = std::env::temp_dir().join(format!("apex-ids-blk-{}", std::process::id()));
    let id_path = tmp_ids.join("identities.toml");
    let _ = std::fs::create_dir_all(&tmp_ids);
    std::env::set_var("AGENTD_IDENTITIES", id_path.to_str().unwrap());

    let mut ids = apexos_core::Identities::default();
    ids.seed_defaults("/tmp/soul.md");
    ids.persist_blocked = true;
    let (base, _tmp) = boot("admin-secret", ids).await;

    let (st, v) = json_post(
        &format!("{base}/api/identities/user"),
        Some("admin-secret"),
        serde_json::json!({ "name": "Guest" }),
    )
    .await;
    assert_eq!(st, 500, "blocked persist must fail closed: {v}");
    assert!(!id_path.exists(), "must not mint a fresh identities.toml");

    std::env::remove_var("AGENTD_IDENTITIES");
    let _ = std::fs::remove_dir_all(&tmp_ids);
}

#[tokio::test]
async fn policy_mode_persists_before_ok() {
    let _g = IDENTITIES_ENV.lock().unwrap();
    let tmp = std::env::temp_dir().join(format!(
        "apex-pol-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _ = std::fs::create_dir_all(&tmp);
    let pol = tmp.join("policy.toml");
    std::fs::write(&pol, "mode = \"suggest\"\n\n[rules]\n\"read_file\" = \"allow\"\n").unwrap();
    std::env::set_var("AGENTD_POLICY_TOML", pol.to_str().unwrap());

    let mut ids = apexos_core::Identities::default();
    ids.seed_defaults("/tmp/soul.md");
    let (base, _sess) = boot("admin-secret", ids).await;

    let (st, v) = json_post(
        &format!("{base}/api/policy"),
        Some("admin-secret"),
        serde_json::json!({ "mode": "yolo" }),
    )
    .await;
    assert_eq!(st, 200, "policy set: {v}");
    assert!(v["ok"].as_bool().unwrap(), "body: {v}");
    let on_disk = std::fs::read_to_string(&pol).unwrap();
    assert!(on_disk.contains("yolo"), "mode must hit disk: {on_disk}");
    assert!(on_disk.contains("read_file"), "rules must survive: {on_disk}");

    std::fs::write(&pol, "not valid toml [[[").unwrap();
    let before = std::fs::read_to_string(&pol).unwrap();
    let (st, v) = json_post(
        &format!("{base}/api/policy"),
        Some("admin-secret"),
        serde_json::json!({ "mode": "suggest" }),
    )
    .await;
    assert_eq!(st, 200, "handler still 200 with ok:false: {v}");
    assert!(!v["ok"].as_bool().unwrap(), "torn policy must not report success: {v}");
    assert_eq!(std::fs::read_to_string(&pol).unwrap(), before);

    std::env::remove_var("AGENTD_POLICY_TOML");
    let _ = std::fs::remove_dir_all(&tmp);
}

#[tokio::test]
async fn gossip_is_admin_only_and_rejects_broadcast() {
    let mut ids = apexos_core::Identities::default();
    ids.seed_defaults("/tmp/soul.md");
    ids.user_mut("owner").unwrap().set_pin("1337");
    let mut guest = apexos_core::User {
        id: "guest".into(),
        name: "Guest".into(),
        ..Default::default()
    };
    guest.set_pin("4242");
    ids.users.push(guest);
    let (base, _tmp) = boot("admin-secret", ids).await;

    let (st, v) = json_post(
        &format!("{base}/api/mesh/gossip"),
        None,
        serde_json::json!({ "target": 7, "text": "hi" }),
    )
    .await;
    assert_eq!(st, 401, "ungated gossip is the signing oracle: {v}");

    let (_, login) = json_post(
        &format!("{base}/api/auth/login"),
        None,
        serde_json::json!({ "user_id": "guest", "pin": "4242" }),
    )
    .await;
    let guest_tok = login["token"].as_str().expect("guest token");
    let (st, _) = json_post(
        &format!("{base}/api/mesh/gossip"),
        Some(guest_tok),
        serde_json::json!({ "target": 7, "text": "hi" }),
    )
    .await;
    assert_eq!(st, 403, "guest must not sign radio A2A");

    let (st, v) = json_post(
        &format!("{base}/api/mesh/gossip"),
        Some("admin-secret"),
        serde_json::json!({ "target": 65535, "text": "hi" }),
    )
    .await;
    assert_eq!(st, 400, "broadcast: {v}");
    assert!(v["error"].as_str().unwrap_or("").contains("broadcast"));

    let (st, v) = json_post(
        &format!("{base}/api/mesh/gossip"),
        Some("admin-secret"),
        serde_json::json!({ "target": 7, "text": "hello from admin" }),
    )
    .await;
    assert_eq!(st, 503, "admin may call; no radio is honest 503: {v}");
}
