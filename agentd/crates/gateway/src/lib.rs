use axum::{
    Json, Router, middleware,
    extract::{
        Path, Query, Request, State,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    routing::{delete, get, post},
};
pub mod mesh;
pub use mesh::{parse_avahi_output, PeerRecord, PeerRegistry, PeerRole};
use serde::{Deserialize, Serialize};
use futures_util::{SinkExt, StreamExt};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::{broadcast, Mutex, RwLock};
use apexos_core::{ActionId, BusHandle, Event, Message as CoreMessage, SessionId};
use apexos_plugins::{PolicyEngine, Rule, VastState, VastPhase, load_recipes};
use tokio::sync::mpsc;

/// Lightweight record of a council session, served by `GET /api/council[/:id]`.
#[derive(Clone, Serialize, Deserialize)]
pub struct CouncilRecord {
    pub id:        String,
    pub topic:     String,
    pub agents:    Vec<apexos_core::CouncilAgentDef>,
    pub status:    String,   // "running" | "complete"
    pub rounds:    u32,
    pub synthesis: String,
}

/// Map council_id → live butt-in sender. Entry removed when council completes.
pub type CouncilButtInMap  = Arc<Mutex<HashMap<String, mpsc::Sender<String>>>>;
/// Ordered list of all sessions (running + complete) for this daemon run.
pub type CouncilSessionsMap = Arc<Mutex<Vec<CouncilRecord>>>;

#[derive(Clone)]
pub struct GatewayState {
    pub bus:                   BusHandle,
    pub bcast:                 broadcast::Sender<Event>,
    /// Anthropic API key — set via env or browser UI key-entry flow
    pub api_key:               Arc<RwLock<String>>,
    /// OAI-compatible key (OpenRouter / Together / etc.) — separate from Anthropic key
    pub oai_api_key:           Arc<RwLock<String>>,
    pub model:                 Arc<RwLock<String>>,
    /// Active inference backend — live-swappable: "anthropic" | "ollama" | "vllm" | "openrouter"
    pub backend:               Arc<RwLock<String>>,
    /// Base URL for OAI-compatible backends — live-swappable
    pub oai_base_url:          Arc<RwLock<String>>,
    pub policy_mode:           Arc<RwLock<String>>,
    /// Send a mode string ("suggest" | "auto-edit" | "yolo") to live-update the PolicyEngine.
    pub policy_set_tx:         mpsc::Sender<String>,
    pub ui_dir:                PathBuf,
    pub events_dir:            PathBuf,
    pub sessions_dir:          PathBuf,
    pub histories:             Arc<Mutex<HashMap<SessionId, Vec<CoreMessage>>>>,
    pub next_session_id:       Arc<AtomicU64>,
    /// Shared secret for /sensor-bridge WS connections. Empty = no auth required.
    pub sensor_bridge_token:   Arc<String>,
    /// Bearer token for all other API + WS routes. Empty = auth disabled.
    /// Set via AGENTD_TOKEN env var; clients pass as "Authorization: Bearer <token>"
    /// or as "?token=<token>" query param (for WebSocket upgrades).
    pub api_token:             Arc<String>,
    pub soul_path:             PathBuf,
    pub policy_arc:            Arc<RwLock<PolicyEngine>>,
    /// Council: start a new council session (shared with supervisor for agent-tool calls)
    pub council_start_tx:  mpsc::Sender<(SessionId, ActionId, serde_json::Value)>,
    /// Council: live butt-in senders, keyed by council_id
    pub council_butt_in:   CouncilButtInMap,
    /// Council: session records for listing/detail
    pub council_sessions:  CouncilSessionsMap,
    /// Council: counter for gateway-initiated council IDs (prefix "gw")
    pub council_next_id:   Arc<std::sync::atomic::AtomicU64>,
    /// Mesh peer registry — peers.toml backed, hot-reloadable
    pub peer_registry:     Arc<RwLock<PeerRegistry>>,
    /// Own node_id (hostname) — used by discovery loop to avoid self-bootstrap
    pub node_id:           Arc<String>,
    /// Vast.ai instance + tunnel state — shared with supervisor for virtual tools
    pub vast_state:        VastState,
}

/// Check Bearer token on all gated routes.
/// Accepts "Authorization: Bearer <token>" header or "?token=<token>" query param.
/// No-op when AGENTD_TOKEN is unset (empty string).
async fn require_token(
    State(state): State<GatewayState>,
    req: Request,
    next: middleware::Next,
) -> Response {
    let token = state.api_token.as_str();
    if token.is_empty() {
        return next.run(req).await;
    }
    let from_header = req.headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .unwrap_or("");
    if tokens_match(from_header, token) {
        return next.run(req).await;
    }
    // URL-decode the ?token= value so percent-encoded tokens compare correctly.
    let from_query_raw = req.uri().query().unwrap_or("")
        .split('&')
        .find_map(|p| p.strip_prefix("token="))
        .unwrap_or("");
    let from_query = percent_encoding::percent_decode_str(from_query_raw)
        .decode_utf8_lossy();
    if tokens_match(&from_query, token) {
        return next.run(req).await;
    }
    (StatusCode::UNAUTHORIZED, "invalid or missing token").into_response()
}

/// Constant-time token comparison. Length is checked first (lengths are not
/// secret); equal-length byte slices are then compared with `ConstantTimeEq`
/// so a timing side-channel cannot leak how many leading bytes matched.
fn tokens_match(provided: &str, expected: &str) -> bool {
    use subtle::ConstantTimeEq;
    let (p, e) = (provided.as_bytes(), expected.as_bytes());
    if p.len() != e.len() {
        return false;
    }
    p.ct_eq(e).into()
}

pub fn router(state: GatewayState) -> Router {
    // All API + WS routes are gated by the bearer token middleware.
    // /sensor-bridge has its own SENSOR_BRIDGE_TOKEN scheme — kept outside.
    // Static fallback (dashboard HTML/JS) is public — no secrets in those files.
    let gated = Router::new()
        .route("/ws",              get(ws_handler))
        .route("/terminal-ws",     get(terminal_ws_handler))
        .route("/api/status",      get(status_handler))
        .route("/api/key",         post(set_key_handler))
        .route("/api/keys",        get(get_keys_handler).post(set_keys_handler))
        .route("/api/model",       get(get_model_handler).post(set_model_handler))
        .route("/api/models",      get(get_models_handler))
        .route("/api/backend",     get(get_backend_handler).post(set_backend_handler))
        .route("/api/policy",         post(set_policy_handler))
        .route("/api/policy/rules",   get(get_policy_rules_handler))
        .route("/api/soul",           get(get_soul_handler).post(set_soul_handler))
        .route("/api/power",              post(power_handler))
        .route("/api/evolution/history",  get(evolution_history_handler))
        .route("/api/evolution/stats",    get(evolution_stats_handler))
        .route("/api/sessions",           get(sessions_handler))
        .route("/api/sessions/active",    get(active_sessions_handler))
        .route("/api/events/recent",      get(events_recent_handler))
        .route("/api/sessions/{id}/message", post(session_message_handler))
        .route("/api/sessions/{id}/image",   post(session_image_handler))
        .route("/api/workspace/images",      get(workspace_images_handler))
        .route("/api/workspace/list",        get(workspace_list_handler))
        .route("/api/workspace/read",        get(workspace_read_handler))
        .route("/api/run",                post(run_command_handler))
        .route("/api/snapshot",           get(snapshot_handler))
        .route("/api/sonus/files",        get(sonus_files_handler))
        .route("/api/sonus/stream",       get(sonus_stream_handler))
        .route("/api/sonus/play",         post(sonus_play_handler))
        .route("/api/sonus/stop",         post(sonus_stop_handler))
        .route("/api/transcribe",         post(transcribe_handler))
        .route("/api/record/start",       post(record_start_handler))
        .route("/api/record/stop",        post(record_stop_handler))
        .route("/api/wake",               post(wake_handler))
        .route("/api/speak",              post(speak_handler))
        .route("/api/council",               get(council_list_handler).post(council_start_handler))
        .route("/api/council/{id}",          get(council_detail_handler))
        .route("/api/council/{id}/butt-in",  post(council_butt_in_handler))
        .route("/api/mesh/nodes",         get(mesh_nodes_handler))
        .route("/api/mesh/peers",         get(mesh_peers_get_handler).post(mesh_peers_post_handler))
        .route("/api/mesh/peers/{id}",    delete(mesh_peers_delete_handler))
        .route("/api/vast/recipes",       get(vast_recipes_handler).post(vast_recipes_save_handler))
        .route("/api/vast/status",        get(vast_status_handler))
        .route("/api/vast/offers",        get(vast_offers_handler))
        .route("/api/vast/hf-search",     get(vast_hf_search_handler))
        .route("/api/audio/files",        get(audio_files_handler))
        .route("/api/audio/analyze",      post(audio_analyze_handler))
        .route("/api/audio/waveform",     post(audio_waveform_handler))
        .route("/api/audio/process",      post(audio_process_handler))
        .route("/api/notes",              get(notes_list_handler))
        .route("/api/notes/read",         post(notes_read_handler))
        .route("/api/notes/write",        post(notes_write_handler))
        .route("/api/sketch",             post(sketch_save_handler))
        .route("/api/sketch/latest",      get(sketch_latest_handler))
        .route_layer(middleware::from_fn_with_state(state.clone(), require_token));

    Router::new()
        .merge(gated)
        .route("/sensor-bridge",   get(sensor_bridge_ws_handler))
        .fallback(static_handler)
        .with_state(state)
}

// ── WebSocket ─────────────────────────────────────────────────────────────────

async fn ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<GatewayState>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_socket(socket, state))
}

async fn handle_socket(socket: WebSocket, state: GatewayState) {
    let mut rx = state.bcast.subscribe();
    let (mut sink, stream) = socket.split();

    // Priority channel: read task sends session_init frames; write task forwards them
    // before anything from the broadcast. Capacity 8 is enough for the hello + one resume.
    let (prio_tx, mut prio_rx) = tokio::sync::mpsc::channel::<String>(8);

    // Assign a fresh session_id immediately — no blocking on hello.
    let session_id = state.next_session_id.fetch_add(1, Ordering::SeqCst);

    // Send initial session_init (empty history — new session) before write task starts.
    let _ = prio_tx.send(make_session_init(session_id, &[])).await;

    // Write task: drain priority channel first (biased), then relay broadcast events.
    let write = tokio::spawn(async move {
        loop {
            tokio::select! {
                biased;
                Some(msg) = prio_rx.recv() => {
                    if sink.send(Message::Text(msg.into())).await.is_err() { break; }
                }
                result = rx.recv() => match result {
                    Ok(event) => {
                        if let Ok(json) = serde_json::to_string(&event) {
                            if sink.send(Message::Text(json.into())).await.is_err() { break; }
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(_) => break,
                }
            }
        }
    });

    // Read task: handle hello frames (session resume) and relay everything else as Events.
    let bus      = state.bus.clone();
    let histories = state.histories.clone();
    let read = tokio::spawn(async move {
        let mut stream   = stream;
        let mut session_id = session_id;   // mutable — updated by hello

        while let Some(Ok(msg)) = stream.next().await {
            if let Message::Text(text) = msg {
                let val: serde_json::Value = match serde_json::from_str(&text) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                if val["type"].as_str() == Some("hello") {
                    // Client wants to resume an existing session.
                    let resume = val["resume_session"].as_u64().map(SessionId);
                    let hist = {
                        let lock = histories.lock().await;
                        match resume {
                            Some(s) if lock.contains_key(&s) => {
                                session_id = s.0;
                                lock.get(&s).cloned().unwrap_or_default()
                            }
                            _ => vec![],  // keep current session_id
                        }
                    };
                    let _ = prio_tx.send(make_session_init(session_id, &hist)).await;
                } else {
                    // Regular frame — inject WS-bound session_id and emit as Event.
                    let mut frame = val;
                    frame["session"] = serde_json::json!(session_id);
                    // A user_prompt may carry raw image refs (path|b64). Shim them
                    // through the vision downscaler here so UserPrompt.images is the
                    // prepared {media_type,data} form the event deserializes into.
                    if frame.get("type").and_then(|v| v.as_str()) == Some("user_prompt") {
                        if let Some(raw) = frame.get("images").and_then(|v| v.as_array()).cloned() {
                            if !raw.is_empty() {
                                let prepared = prepare_user_images(&raw).await;
                                frame["images"] = serde_json::to_value(prepared).unwrap_or_default();
                            }
                        }
                    }
                    if let Ok(event) = serde_json::from_value::<Event>(frame) {
                        bus.emit(event).await;
                    }
                }
            }
        }
    });

    tokio::select! {
        _ = read  => {}
        _ = write => {}
    }
}

// ── Sensor bridge WS ─────────────────────────────────────────────────────────

async fn sensor_bridge_ws_handler(
    ws:              WebSocketUpgrade,
    Query(params):   Query<HashMap<String, String>>,
    State(state):    State<GatewayState>,
) -> Response {
    let expected = state.sensor_bridge_token.as_str();
    if !expected.is_empty() {
        let provided = params.get("token").map(|s| s.as_str()).unwrap_or("");
        if provided != expected {
            return (StatusCode::UNAUTHORIZED, "invalid sensor bridge token").into_response();
        }
    }
    ws.on_upgrade(move |socket| handle_sensor_bridge(socket, state))
       .into_response()
}

async fn handle_sensor_bridge(socket: WebSocket, state: GatewayState) {
    let (_, mut stream) = socket.split();
    eprintln!("[sensor-bridge] node connected");
    while let Some(Ok(msg)) = stream.next().await {
        if let Message::Text(text) = msg {
            match serde_json::from_str::<Event>(&text) {
                Ok(event) => {
                    if let Event::SensorReading { ref node_id, ref reading, .. } = event {
                        eprintln!("[sensor-bridge] {node_id}: {reading:?}");
                    }
                    state.bus.emit(event).await;
                }
                Err(e) => eprintln!("[sensor-bridge] parse error: {e} — raw: {text}"),
            }
        }
    }
    eprintln!("[sensor-bridge] node disconnected");
}

fn make_session_init(session_id: u64, history: &[CoreMessage]) -> String {
    serde_json::to_string(&serde_json::json!({
        "type":       "session_init",
        "session_id": session_id,
        "history":    history,
    }))
    .unwrap_or_default()
}

// ── Static file handler ───────────────────────────────────────────────────────

async fn static_handler(
    State(state): State<GatewayState>,
    uri: axum::http::Uri,
) -> Response {
    let path = uri.path().trim_start_matches('/');
    let file_name = match path {
        "" => "index.html",
        "mobile" => "mobile.html",
        other => other,
    };

    // Block path traversal
    if file_name.contains("..") {
        return StatusCode::NOT_FOUND.into_response();
    }

    let content_type: &'static str = if file_name.starts_with("lib/") {
        if file_name.ends_with(".js")  { "application/javascript; charset=utf-8" }
        else if file_name.ends_with(".css") { "text/css; charset=utf-8" }
        else { return StatusCode::NOT_FOUND.into_response(); }
    } else {
        match file_name {
            "index.html"        => "text/html; charset=utf-8",
            "desktop.html"      => "text/html; charset=utf-8",
            "mobile.html"       => "text/html; charset=utf-8",
            "style.css"         => "text/css; charset=utf-8",
            "desktop-style.css" => "text/css; charset=utf-8",
            "app.js"            => "application/javascript; charset=utf-8",
            "desktop-app.js"    => "application/javascript; charset=utf-8",
            "manifest.json"     => "application/manifest+json; charset=utf-8",
            _                   => return StatusCode::NOT_FOUND.into_response(),
        }
    };

    let full_path = state.ui_dir.join(file_name);
    match tokio::fs::read(&full_path).await {
        Ok(bytes) => (
            StatusCode::OK,
            [(header::CONTENT_TYPE, content_type)],
            bytes,
        ).into_response(),
        Err(e) => {
            eprintln!("[gateway] static {file_name}: {e}");
            StatusCode::NOT_FOUND.into_response()
        }
    }
}

// ── API routes ────────────────────────────────────────────────────────────────

async fn status_handler(State(state): State<GatewayState>) -> impl IntoResponse {
    let key_set     = !state.api_key.read().await.is_empty();
    let oai_key_set = !state.oai_api_key.read().await.is_empty();
    let model       = state.model.read().await.clone();
    let policy_mode = state.policy_mode.read().await.clone();
    Json(serde_json::json!({
        "api_key_set":     key_set,
        "oai_key_set":     oai_key_set,
        "model":           model,
        "policy_mode":     policy_mode,
    }))
}

async fn set_policy_handler(
    State(state): State<GatewayState>,
    Json(body): Json<serde_json::Value>,
) -> impl IntoResponse {
    let mode = body["mode"].as_str().unwrap_or("").trim().to_string();
    if !matches!(mode.as_str(), "suggest" | "auto-edit" | "yolo") {
        return Json(serde_json::json!({ "ok": false, "error": "unknown mode" }));
    }
    *state.policy_mode.write().await = mode.clone();
    let _ = state.policy_set_tx.send(mode).await;
    Json(serde_json::json!({ "ok": true }))
}

async fn get_soul_handler(State(state): State<GatewayState>) -> impl IntoResponse {
    match tokio::fs::read_to_string(&state.soul_path).await {
        Ok(text) => Json(serde_json::json!({ "ok": true, "content": text })),
        Err(e)   => Json(serde_json::json!({ "ok": false, "error": e.to_string() })),
    }
}

async fn set_soul_handler(
    State(state): State<GatewayState>,
    Json(body): Json<serde_json::Value>,
) -> impl IntoResponse {
    let content = match body["content"].as_str() {
        Some(s) => s.to_string(),
        None    => return Json(serde_json::json!({ "ok": false, "error": "missing content" })),
    };
    match tokio::fs::write(&state.soul_path, content).await {
        Ok(_)  => Json(serde_json::json!({ "ok": true })),
        Err(e) => Json(serde_json::json!({ "ok": false, "error": e.to_string() })),
    }
}

async fn set_key_handler(
    State(state): State<GatewayState>,
    Json(body): Json<serde_json::Value>,
) -> impl IntoResponse {
    let key = body["key"].as_str().unwrap_or("").trim().to_string();
    if key.is_empty() {
        return Json(serde_json::json!({ "ok": false, "error": "empty key" }));
    }
    *state.api_key.write().await = key.clone();

    let persist_path = std::env::var("AGENTD_KEY_FILE")
        .unwrap_or_else(|_| "/var/lib/agentd/.api_key".into());
    let _ = write_secret_file(&persist_path, &key);

    Json(serde_json::json!({ "ok": true }))
}

/// Write a secret (API key) to `path` with mode 0600, so it is not world- or
/// group-readable. Truncates any existing file. Synchronous std I/O — key
/// files are tiny and writes are infrequent (settings save only).
fn write_secret_file(path: &str, contents: &str) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    f.write_all(contents.as_bytes())?;
    // .mode() only applies on create; enforce 0600 on a pre-existing file too.
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    Ok(())
}

async fn get_keys_handler(State(state): State<GatewayState>) -> impl IntoResponse {
    Json(serde_json::json!({
        "anthropic_set": !state.api_key.read().await.is_empty(),
        "oai_set":       !state.oai_api_key.read().await.is_empty(),
    }))
}

async fn set_keys_handler(
    State(state): State<GatewayState>,
    Json(body): Json<serde_json::Value>,
) -> impl IntoResponse {
    if let Some(key) = body["anthropic"].as_str() {
        let key = key.trim().to_string();
        if !key.is_empty() {
            *state.api_key.write().await = key.clone();
            let path = std::env::var("AGENTD_KEY_FILE")
                .unwrap_or_else(|_| "/var/lib/agentd/.api_key".into());
            let _ = write_secret_file(&path, &key);
        }
    }
    if let Some(key) = body["oai"].as_str() {
        let key = key.trim().to_string();
        if !key.is_empty() {
            *state.oai_api_key.write().await = key.clone();
            let path = std::env::var("AGENTD_OAI_KEY_FILE")
                .unwrap_or_else(|_| "/var/lib/agentd/.oai_api_key".into());
            let _ = write_secret_file(&path, &key);
        }
    }
    Json(serde_json::json!({ "ok": true }))
}

async fn get_model_handler(State(state): State<GatewayState>) -> impl IntoResponse {
    let model = state.model.read().await.clone();
    Json(serde_json::json!({ "model": model }))
}

/// Returns available models for the active backend.
/// For Anthropic: static list. For OAI backends: proxies to {base_url}/models.
async fn get_models_handler(State(state): State<GatewayState>) -> impl IntoResponse {
    let current     = state.model.read().await.clone();
    let backend     = state.backend.read().await.clone();
    let oai_base    = state.oai_base_url.read().await.clone();

    if backend == "anthropic" {
        return Json(serde_json::json!({
            "backend": backend,
            "current": current,
            "models": [
                { "id": "claude-sonnet-4-6", "name": "Sonnet 4.6" },
                { "id": "claude-opus-4-8",   "name": "Opus 4.8"   },
                { "id": "claude-opus-4-7",   "name": "Opus 4.7"   },
                { "id": "claude-haiku-4-5",  "name": "Haiku 4.5"  },
            ]
        }));
    }

    // OAI-compatible backend: query {base_url}/models for live model list
    let models_url = format!("{}/models", oai_base.trim_end_matches('/'));
    let api_key = state.oai_api_key.read().await.clone();
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(3))
        .build()
        .unwrap_or_default();

    let mut req = client.get(&models_url);
    if !api_key.is_empty() {
        req = req.header("authorization", format!("Bearer {api_key}"));
    }

    match req.send().await {
        Ok(resp) if resp.status().is_success() => {
            if let Ok(body) = resp.json::<serde_json::Value>().await {
                let models: Vec<serde_json::Value> = body["data"]
                    .as_array()
                    .unwrap_or(&vec![])
                    .iter()
                    .filter_map(|m| m["id"].as_str())
                    .map(|id| serde_json::json!({ "id": id, "name": id }))
                    .collect();
                return Json(serde_json::json!({
                    "backend": backend,
                    "oai_base_url": oai_base,
                    "current": current,
                    "models":  models,
                }));
            }
        }
        _ => {}
    }

    // Fallback: return just the current model
    Json(serde_json::json!({
        "backend": backend,
        "oai_base_url": oai_base,
        "current": current,
        "models": [{ "id": current, "name": current }],
    }))
}

async fn get_backend_handler(State(state): State<GatewayState>) -> impl IntoResponse {
    Json(serde_json::json!({
        "backend":     state.backend.read().await.clone(),
        "oai_base_url": state.oai_base_url.read().await.clone(),
    }))
}

async fn set_backend_handler(
    State(state): State<GatewayState>,
    Json(body): Json<serde_json::Value>,
) -> impl IntoResponse {
    let backend = body["backend"].as_str().unwrap_or("").trim().to_lowercase();
    if backend.is_empty() {
        return Json(serde_json::json!({ "ok": false, "error": "missing backend" }));
    }
    *state.backend.write().await = backend;

    if let Some(url) = body["oai_base_url"].as_str() {
        let url = url.trim().to_string();
        if !url.is_empty() {
            *state.oai_base_url.write().await = url;
        }
    }

    // Optionally update the model when switching backends
    if let Some(model) = body["model"].as_str() {
        let model = model.trim().to_string();
        if !model.is_empty() {
            *state.model.write().await = model;
        }
    }

    Json(serde_json::json!({ "ok": true }))
}

async fn set_model_handler(
    State(state): State<GatewayState>,
    Json(body): Json<serde_json::Value>,
) -> impl IntoResponse {
    let model = body["model"].as_str().unwrap_or("").trim().to_string();
    if model.is_empty() {
        return Json(serde_json::json!({ "ok": false, "error": "empty model" }));
    }
    *state.model.write().await = model;
    Json(serde_json::json!({ "ok": true }))
}

async fn power_handler(
    State(_): State<GatewayState>,
    Json(body): Json<serde_json::Value>,
) -> impl IntoResponse {
    let action = body["action"].as_str().unwrap_or("");
    let cmd = match action {
        "reboot"   => "reboot",
        "shutdown" => "poweroff",
        _ => return Json(serde_json::json!({ "ok": false, "error": "unknown action" })),
    };
    // Call systemctl directly — NOT via sudo. agentd runs with
    // NoNewPrivileges=true, which blocks sudo's setuid escalation entirely.
    // `systemctl reboot/poweroff` routes through logind + polkit; the agentd
    // user is authorized by /etc/polkit-1/rules.d/49-agentd-power.rules.
    match tokio::process::Command::new("systemctl")
        .arg(cmd)
        .output()
        .await
    {
        Ok(o) if o.status.success() => Json(serde_json::json!({ "ok": true })),
        Ok(o) => {
            let err = String::from_utf8_lossy(&o.stderr).to_string();
            eprintln!("[gateway] power/{cmd}: {err}");
            Json(serde_json::json!({ "ok": false, "error": err }))
        }
        Err(e) => Json(serde_json::json!({ "ok": false, "error": e.to_string() })),
    }
}

async fn evolution_history_handler(State(state): State<GatewayState>) -> impl IntoResponse {
    let mut entries: Vec<serde_json::Value> = Vec::new();

    let Ok(mut dir) = tokio::fs::read_dir(&state.events_dir).await else {
        return Json(serde_json::json!([]));
    };

    // Collect matching filenames first so we can sort them.
    let mut files: Vec<String> = Vec::new();
    while let Ok(Some(entry)) = dir.next_entry().await {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with("events-") && name.ends_with(".jsonl") {
            files.push(entry.path().to_string_lossy().to_string());
        }
    }
    files.sort();

    for path in files {
        let Ok(text) = tokio::fs::read_to_string(&path).await else { continue };
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() { continue }
            let Ok(val) = serde_json::from_str::<serde_json::Value>(line) else { continue };
            if val.get("type").and_then(|t| t.as_str()) == Some("evolution_applied") {
                entries.push(val);
            }
        }
    }

    Json(serde_json::json!(entries))
}

async fn evolution_stats_handler(State(state): State<GatewayState>) -> impl IntoResponse {
    let mut applied_total:  u64 = 0;
    let mut rolledback_total: u64 = 0;
    let mut by_kind: std::collections::HashMap<String, u64> = std::collections::HashMap::new();

    let Ok(mut dir) = tokio::fs::read_dir(&state.events_dir).await else {
        return Json(serde_json::json!({
            "applied_total": 0, "rolledback_total": 0,
            "rollback_rate": 0.0, "by_kind": {}
        }));
    };

    let mut files: Vec<String> = Vec::new();
    while let Ok(Some(entry)) = dir.next_entry().await {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with("events-") && name.ends_with(".jsonl") {
            files.push(entry.path().to_string_lossy().to_string());
        }
    }
    files.sort();

    for path in files {
        let Ok(text) = tokio::fs::read_to_string(&path).await else { continue };
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() { continue }
            let Ok(val) = serde_json::from_str::<serde_json::Value>(line) else { continue };
            match val.get("type").and_then(|t| t.as_str()) {
                Some("evolution_applied") => {
                    applied_total += 1;
                    let kind = val.get("proposal")
                        .and_then(|p| p.get("kind"))
                        .and_then(|k| k.as_str())
                        .unwrap_or("unknown")
                        .to_string();
                    *by_kind.entry(kind).or_insert(0) += 1;
                }
                Some("evolution_rolled_back") => {
                    rolledback_total += 1;
                }
                _ => {}
            }
        }
    }

    let rollback_rate = if applied_total > 0 {
        (rolledback_total as f64 / applied_total as f64 * 100.0 * 10.0).round() / 10.0
    } else {
        0.0
    };

    Json(serde_json::json!({
        "applied_total":    applied_total,
        "rolledback_total": rolledback_total,
        "rollback_rate":    rollback_rate,
        "by_kind":          by_kind,
    }))
}

// ── sessions ──────────────────────────────────────────────────────────────────

/// GET /api/sessions/active — sessions currently loaded in memory (this daemon run).
/// Returns session_id + message_count so agents can choose a target for send_to_agent.
async fn active_sessions_handler(State(state): State<GatewayState>) -> impl IntoResponse {
    let histories = state.histories.lock().await;
    let mut sessions: Vec<serde_json::Value> = histories.iter()
        .map(|(sid, hist)| serde_json::json!({
            "session_id":    sid.0,
            "message_count": hist.len(),
        }))
        .collect();
    drop(histories);
    sessions.sort_by(|a, b| {
        b["session_id"].as_u64().unwrap_or(0)
            .cmp(&a["session_id"].as_u64().unwrap_or(0))
    });
    Json(serde_json::json!(sessions))
}

/// POST /api/sessions/:id/message — inject a message into an agent session from
/// external code (scripts, other services, the desktop UI). Same path as A2A:
/// emits UserPrompt on the bus so the target session starts a new turn.
async fn session_message_handler(
    State(state): State<GatewayState>,
    Path(id):     Path<u64>,
    Json(body):   Json<serde_json::Value>,
) -> impl IntoResponse {
    let message = match body["message"].as_str() {
        Some(s) if !s.trim().is_empty() => s.to_string(),
        _ => return Json(serde_json::json!({ "ok": false, "error": "missing message" })),
    };
    state.bus.emit(Event::UserPrompt { session: SessionId(id), text: message, images: vec![] }).await;
    Json(serde_json::json!({ "ok": true, "session_id": id }))
}

/// Resolve a user-supplied workspace path. Relative paths join `AGENTD_WORKSPACE`;
/// the canonical result must stay inside the workspace (defeats `../` + absolute
/// escapes) — a frontend must never reach a file outside the workspace through any
/// of these routes (image attach, explorer list/read).
fn resolve_workspace_path(path: &str) -> Result<std::path::PathBuf, String> {
    let ws = std::env::var("AGENTD_WORKSPACE")
        .ok().filter(|s| !s.is_empty())
        .unwrap_or_else(|| "/var/lib/agentd/workspace".to_string());
    let ws_canon = std::fs::canonicalize(&ws).map_err(|e| format!("workspace {ws}: {e}"))?;
    let p = std::path::Path::new(path);
    let joined = if p.is_absolute() { p.to_path_buf() } else { ws_canon.join(p) };
    let canon = std::fs::canonicalize(&joined).map_err(|e| format!("{}: {e}", joined.display()))?;
    if !canon.starts_with(&ws_canon) {
        return Err(format!("path {} escapes workspace", canon.display()));
    }
    Ok(canon)
}

/// Run raw user-attached image refs through the vision shim, returning prepared
/// images ready to drop into `Event::UserPrompt.images`. Each ref is either
/// `{ "path": "<workspace file>" }` or `{ "b64": "<base64>", "media_type": ... }`.
/// Every image is decoded → downscaled (≤ `VISION_MAX_EDGE`) → re-encoded (the same
/// token-bomb guard as the SensorHead path). A bad or unsafe ref is logged and
/// skipped so one bad image never sinks the whole prompt. CPU-bound decode runs on
/// a blocking thread.
async fn prepare_user_images(raw: &[serde_json::Value]) -> Vec<apexos_core::ImageSource> {
    let mut out = Vec::new();
    for item in raw {
        let prepared = if let Some(p) = item.get("path").and_then(|v| v.as_str()) {
            match resolve_workspace_path(p) {
                Ok(path) => tokio::task::spawn_blocking(move || apexos_core::vision::load_and_prepare(&path)).await,
                Err(e) => { eprintln!("[vision] user image path rejected: {e}"); continue; }
            }
        } else if let Some(b64) = item.get("b64").and_then(|v| v.as_str()) {
            let b64 = b64.to_string();
            tokio::task::spawn_blocking(move || apexos_core::vision::prepare_b64(&b64)).await
        } else {
            continue;
        };
        match prepared {
            Ok(Ok(p)) => {
                eprintln!("[vision] user image prepared {}x{} ~{} tokens", p.width, p.height, p.est_tokens);
                out.push(apexos_core::ImageSource { media_type: p.media_type, data: p.b64 });
            }
            Ok(Err(e)) => eprintln!("[vision] user image prepare failed: {e}"),
            Err(e)      => eprintln!("[vision] user image task join error: {e}"),
        }
    }
    out
}

/// POST /api/sessions/:id/image — inject a user message carrying attached image(s).
/// Body: `{ "text": "<optional caption>", "images": [ {"path":...} | {"b64":...,"media_type":...} ] }`,
/// or a single inline `{"b64":...}` / `{"path":...}` shorthand. The PWA / a phone
/// camera upload / curl all use this; images run through the vision shim first.
async fn session_image_handler(
    State(state): State<GatewayState>,
    Path(id):     Path<u64>,
    Json(body):   Json<serde_json::Value>,
) -> impl IntoResponse {
    let text = body["text"].as_str().unwrap_or("").to_string();
    let raw: Vec<serde_json::Value> = if let Some(arr) = body["images"].as_array() {
        arr.clone()
    } else if body.get("b64").is_some() || body.get("path").is_some() {
        vec![body.clone()]
    } else {
        vec![]
    };
    let images = prepare_user_images(&raw).await;
    if images.is_empty() {
        return Json(serde_json::json!({ "ok": false, "error": "no usable image (need path|b64)" }));
    }
    let n = images.len();
    state.bus.emit(Event::UserPrompt { session: SessionId(id), text, images }).await;
    Json(serde_json::json!({ "ok": true, "session_id": id, "images": n }))
}

async fn sessions_handler(State(state): State<GatewayState>) -> impl IntoResponse {
    use apexos_core::{ContentBlock, Message};
    use tokio::fs;

    let mut sessions = Vec::new();
    let mut rd = match fs::read_dir(&state.sessions_dir).await {
        Ok(r) => r,
        Err(_) => return Json(serde_json::json!([])),
    };

    while let Ok(Some(entry)) = rd.next_entry().await {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") { continue; }
        let id: u64 = match path.file_stem().and_then(|s| s.to_str())
            .and_then(|s| s.parse().ok()) { Some(n) => n, None => continue };

        let last_active = entry.metadata().await.ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);

        let text = match fs::read_to_string(&path).await { Ok(t) => t, Err(_) => continue };
        let message_count = text.lines().filter(|l| !l.trim().is_empty()).count();
        if message_count == 0 { continue; }

        let preview: String = text.lines()
            .filter_map(|line| serde_json::from_str::<Message>(line).ok())
            .find_map(|msg| {
                if let Message::User { content } = msg {
                    content.into_iter().find_map(|b| {
                        if let ContentBlock::Text { text } = b { Some(text) } else { None }
                    })
                } else {
                    None
                }
            })
            .unwrap_or_default();
        let preview: String = preview.chars().take(80).collect();

        sessions.push(serde_json::json!({
            "session_id":    id,
            "last_active":   last_active,
            "message_count": message_count,
            "preview":       preview,
        }));
    }

    sessions.sort_by(|a, b| {
        let ta = a["last_active"].as_u64().unwrap_or(0);
        let tb = b["last_active"].as_u64().unwrap_or(0);
        tb.cmp(&ta)
    });

    Json(serde_json::json!(sessions))
}

// ── event log ─────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct EventsQuery {
    hours: Option<u64>,
    types: Option<String>,
    max:   Option<usize>,
}

/// GET /api/events/recent — filtered view of the JSONL event log.
/// Returns a JSON array of raw event objects. Noisy streaming events
/// (agent_text, tool_result, turn_complete) are excluded by default.
async fn events_recent_handler(
    State(state):  State<GatewayState>,
    Query(params): Query<EventsQuery>,
) -> impl IntoResponse {
    const NOISE: &[&str] = &["agent_text", "agent_thinking", "tool_result", "turn_complete"];

    let hours      = params.hours.unwrap_or(24).min(168);
    let max_events = params.max.unwrap_or(500).min(2000);
    let type_filter: Option<std::collections::HashSet<String>> =
        params.types.as_deref().map(|s| s.split(',').map(|t| t.trim().to_owned()).collect());

    let days_back = ((hours as f64) / 24.0).ceil() as i64 + 1;
    let today = chrono::Local::now().date_naive();
    let mut date_files: Vec<std::path::PathBuf> = Vec::new();
    for d in 0..days_back {
        let date = today - chrono::Duration::days(d);
        let path = state.events_dir.join(format!("events-{}.jsonl", date.format("%Y-%m-%d")));
        if tokio::fs::metadata(&path).await.is_ok() {
            date_files.push(path);
        }
    }
    date_files.reverse();

    let mut events: Vec<serde_json::Value> = Vec::new();
    for path in &date_files {
        let Ok(text) = tokio::fs::read_to_string(path).await else { continue };
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() { continue }
            let Ok(val) = serde_json::from_str::<serde_json::Value>(line) else { continue };
            let ev_type = val["type"].as_str().unwrap_or("");
            if NOISE.contains(&ev_type) { continue }
            if let Some(ref filter) = type_filter {
                if !filter.contains(ev_type) { continue }
            }
            events.push(val);
        }
    }

    if events.len() > max_events {
        let skip = events.len() - max_events;
        events.drain(0..skip);
    }

    Json(serde_json::json!(events))
}

// ── shell passthrough ─────────────────────────────────────────────────────────

async fn run_command_handler(
    Json(body): Json<serde_json::Value>,
) -> impl IntoResponse {
    let command = match body["command"].as_str() {
        Some(s) if !s.trim().is_empty() => s.trim().to_string(),
        _ => return Json(serde_json::json!({ "ok": false, "error": "missing command" })),
    };

    // Block obviously destructive patterns
    const DENY: &[&str] = &["rm -rf /", "mkfs", "dd if=/dev/zero", ":(){ :|:& };:"];
    for pat in DENY {
        if command.contains(pat) {
            return Json(serde_json::json!({ "ok": false, "error": "command denied" }));
        }
    }

    let result = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        tokio::process::Command::new("sh").arg("-c").arg(&command).output(),
    ).await;

    match result {
        Ok(Ok(o)) => Json(serde_json::json!({
            "ok":        true,
            "stdout":    String::from_utf8_lossy(&o.stdout).to_string(),
            "stderr":    String::from_utf8_lossy(&o.stderr).to_string(),
            "exit_code": o.status.code().unwrap_or(-1),
        })),
        Ok(Err(e)) => Json(serde_json::json!({ "ok": false, "error": e.to_string() })),
        Err(_)     => Json(serde_json::json!({ "ok": false, "error": "timed out (30s)" })),
    }
}

// ── camera snapshot ───────────────────────────────────────────────────────────

async fn snapshot_handler(
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let night = params.get("night").map(|v| v == "true" || v == "1").unwrap_or(false);
    match capture_camera_jpeg(night).await {
        Ok(bytes) => {
            (StatusCode::OK, [(header::CONTENT_TYPE, "image/jpeg")], bytes).into_response()
        }
        Err(e) => {
            eprintln!("[snapshot] {e}");
            (StatusCode::INTERNAL_SERVER_ERROR, e).into_response()
        }
    }
}

/// Sorted `/dev/video*` capture nodes (video0, video1, …). A USB cam often exposes
/// several nodes; the extras are metadata-only and just fail to capture, so we try
/// them in order until one yields a frame.
fn video_nodes() -> Vec<String> {
    let mut nodes: Vec<(u32, String)> = std::fs::read_dir("/dev")
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().into_string().ok()?;
            let n: u32 = name.strip_prefix("video")?.parse().ok()?;
            Some((n, format!("/dev/{name}")))
        })
        .collect();
    nodes.sort_by_key(|(n, _)| *n);
    nodes.into_iter().map(|(_, p)| p).collect()
}

/// Capture one JPEG frame from whatever camera this device has — the device-agnostic
/// backend pick (the capture half of HW-tier detection): Pi CSI camera (rpicam-jpeg,
/// honoring `night`) first, then a USB / laptop webcam over V4L2 (ffmpeg), then
/// fswebcam. Each backend gets a 10s timeout; a >1KB output file counts as a frame.
/// Returns the JPEG bytes, or an error string if no camera produced one.
async fn capture_camera_jpeg(night: bool) -> Result<Vec<u8>, String> {
    use tokio::process::Command;
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_micros();
    let out = format!("/tmp/apex_snapshot_{stamp}.jpg");
    let dur = std::time::Duration::from_secs(10);

    // Run one capture command, return Some(bytes) only on a real (>1KB) frame.
    async fn grab(mut cmd: Command, out: &str, dur: std::time::Duration) -> Option<Vec<u8>> {
        match tokio::time::timeout(dur, cmd.output()).await {
            Ok(Ok(o)) if o.status.success() => match tokio::fs::read(out).await {
                Ok(bytes) if bytes.len() > 1024 => {
                    let _ = tokio::fs::remove_file(out).await;
                    Some(bytes)
                }
                _ => None,
            },
            _ => None,
        }
    }

    // 1) Pi CSI camera (rpicam-jpeg). `--timeout 3000` = ~3s AE/AWB warmup.
    let mut cmd = Command::new("rpicam-jpeg");
    cmd.args(["--output", &out, "--timeout", "3000",
              "--width", "1280", "--height", "720",
              "--nopreview", "--camera", "0", "-q", "85"]);
    if night {
        cmd.args(["--ev", "2", "--awb", "fluorescent", "--shutter", "100000"]);
    }
    if let Some(bytes) = grab(cmd, &out, dur).await {
        return Ok(bytes);
    }

    // 2) USB / laptop webcam over V4L2 (ffmpeg), then fswebcam, per node.
    for dev in video_nodes() {
        let mut cmd = Command::new("ffmpeg");
        cmd.args(["-hide_banner", "-loglevel", "error", "-y",
                  "-f", "v4l2", "-i", &dev,
                  "-frames:v", "5", "-update", "1", &out]);
        if let Some(bytes) = grab(cmd, &out, dur).await {
            return Ok(bytes);
        }
        let mut cmd = Command::new("fswebcam");
        cmd.args(["-d", &dev, "-S", "8", "--no-banner", "-q", &out]);
        if let Some(bytes) = grab(cmd, &out, dur).await {
            return Ok(bytes);
        }
    }

    let _ = tokio::fs::remove_file(&out).await;
    Err("no camera available (no Pi CSI camera and no working /dev/video* webcam)".into())
}

// ── Sonus / media ────────────────────────────────────────────────────────────

fn sonus_dir() -> std::path::PathBuf {
    std::env::var("SUNO_DOWNLOAD_DIR")
        .unwrap_or_else(|_| "/var/lib/agentd/workspace/sonus".into())
        .into()
}

// Server-side Sonus playback (kiosk speakers). A single current-player child,
// held in a process-global so play/stop work without threading state through
// GatewayState. We decode + render with `ffmpeg -f alsa <device>` (ffmpeg is
// already required by the Audio Editor) rather than ffplay: ffplay routes
// through SDL → the ALSA `default` PCM, which on a Pi 5 points at a nonexistent
// card 0 (no analog jack — HDMI only). ffmpeg's alsa muxer lets us target a real
// device explicitly via SONUS_AUDIO_DEVICE (e.g. `plughw:1,0` for HDMI-0); it
// paces to real time and exits at end-of-track. agentd must be in the `audio`
// group to open the device.
fn sonus_player() -> &'static std::sync::Mutex<Option<std::process::Child>> {
    static PLAYER: std::sync::OnceLock<std::sync::Mutex<Option<std::process::Child>>> =
        std::sync::OnceLock::new();
    PLAYER.get_or_init(|| std::sync::Mutex::new(None))
}

// Kill any current playback (best-effort). Returns true if something was stopped.
fn sonus_stop_current() -> bool {
    if let Ok(mut guard) = sonus_player().lock() {
        if let Some(mut child) = guard.take() {
            let _ = child.kill();
            let _ = child.wait();
            return true;
        }
    }
    false
}

/// POST /api/sonus/play — play a downloaded track on the device's own speakers.
/// Body: { name }. Replaces any current playback.
async fn sonus_play_handler(Json(body): Json<serde_json::Value>) -> impl IntoResponse {
    let name = match body["name"].as_str().map(|s| s.trim().to_string()) {
        Some(n) if !n.is_empty() => n,
        _ => return (StatusCode::BAD_REQUEST, Json(serde_json::json!({"ok": false, "error": "missing name"}))).into_response(),
    };
    // Same path-traversal guard as the stream handler.
    if name.contains('/') || name.contains("..") || name.contains('\\') {
        return (StatusCode::BAD_REQUEST, Json(serde_json::json!({"ok": false, "error": "invalid name"}))).into_response();
    }
    let path = sonus_dir().join(&name);
    if tokio::fs::metadata(&path).await.is_err() {
        return (StatusCode::NOT_FOUND, Json(serde_json::json!({"ok": false, "error": "not found"}))).into_response();
    }

    sonus_stop_current();

    // ALSA output device — overridable per-deployment; `default` works where a
    // standard sink exists, but Pi 5 needs an explicit HDMI card (SONUS_AUDIO_DEVICE).
    let device = std::env::var("SONUS_AUDIO_DEVICE")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "default".to_string());

    let spawned = std::process::Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-nostdin", "-i"])
        .arg(&path)
        .args(["-f", "alsa"])
        .arg(&device)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();

    match spawned {
        Ok(child) => {
            if let Ok(mut guard) = sonus_player().lock() {
                *guard = Some(child);
            }
            (StatusCode::OK, Json(serde_json::json!({"ok": true, "playing": name}))).into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({
            "ok": false, "error": format!("ffmpeg failed to start: {e}")
        }))).into_response(),
    }
}

/// POST /api/sonus/stop — stop current playback.
async fn sonus_stop_handler() -> impl IntoResponse {
    let stopped = sonus_stop_current();
    (StatusCode::OK, Json(serde_json::json!({"ok": true, "stopped": stopped})))
}

async fn sonus_files_handler() -> impl IntoResponse {
    const AUDIO_EXTS: &[&str] = &["mp3", "wav", "ogg", "webm", "flac", "aac", "m4a", "opus"];
    let dir = sonus_dir();
    let mut entries: Vec<serde_json::Value> = Vec::new();

    if let Ok(mut rd) = tokio::fs::read_dir(&dir).await {
        while let Ok(Some(entry)) = rd.next_entry().await {
            let name = entry.file_name().to_string_lossy().to_string();
            let ext  = name.rsplit('.').next().unwrap_or("").to_lowercase();
            if !AUDIO_EXTS.contains(&ext.as_str()) { continue; }
            let size = entry.metadata().await.map(|m| m.len()).unwrap_or(0);
            let url  = format!("/api/sonus/stream?name={}", urlencoding_simple(&name));
            entries.push(serde_json::json!({ "name": name, "size": size, "url": url }));
        }
    }

    entries.sort_by(|a, b| {
        a["name"].as_str().unwrap_or("").cmp(b["name"].as_str().unwrap_or(""))
    });

    Json(serde_json::json!(entries))
}

fn urlencoding_simple(s: &str) -> String {
    s.chars().map(|c| match c {
        'A'..='Z' | 'a'..='z' | '0'..='9' | '-' | '_' | '.' | '~' => c.to_string(),
        ' ' => "+".to_string(),
        _ => format!("%{:02X}", c as u32),
    }).collect()
}

async fn sonus_stream_handler(
    Query(params):   Query<HashMap<String, String>>,
    req_headers:     axum::http::HeaderMap,
) -> Response {
    let name = match params.get("name").map(|s| s.trim().to_string()) {
        Some(n) if !n.is_empty() => n,
        _ => return (StatusCode::BAD_REQUEST, "missing name").into_response(),
    };
    if name.contains('/') || name.contains("..") || name.contains('\\') {
        return (StatusCode::BAD_REQUEST, "invalid name").into_response();
    }

    let ct = match name.rsplit('.').next().unwrap_or("").to_lowercase().as_str() {
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "ogg" | "opus" => "audio/ogg",
        "webm" => "audio/webm",
        "flac" => "audio/flac",
        "aac" | "m4a" => "audio/mp4",
        _ => "application/octet-stream",
    };

    let path = sonus_dir().join(&name);
    let bytes = match tokio::fs::read(&path).await {
        Ok(b) => b,
        Err(_) => return StatusCode::NOT_FOUND.into_response(),
    };
    let total = bytes.len();

    if let Some(range_hdr) = req_headers.get(header::RANGE) {
        if let Ok(range_str) = range_hdr.to_str() {
            if let Some(rest) = range_str.strip_prefix("bytes=") {
                let mut parts = rest.splitn(2, '-');
                let start = parts.next().and_then(|s| s.parse::<usize>().ok()).unwrap_or(0);
                let end   = parts.next()
                    .and_then(|s| if s.is_empty() { None } else { s.parse::<usize>().ok() })
                    .unwrap_or(total.saturating_sub(1))
                    .min(total.saturating_sub(1));
                if start < total && start <= end {
                    let body  = bytes[start..=end].to_vec();
                    let len   = body.len();
                    return axum::http::Response::builder()
                        .status(StatusCode::PARTIAL_CONTENT)
                        .header(header::CONTENT_TYPE, ct)
                        .header(header::ACCEPT_RANGES, "bytes")
                        .header(header::CONTENT_RANGE, format!("bytes {start}-{end}/{total}"))
                        .header(header::CONTENT_LENGTH, len)
                        .body(axum::body::Body::from(body))
                        .unwrap();
                }
            }
        }
    }

    axum::http::Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, ct)
        .header(header::ACCEPT_RANGES, "bytes")
        .header(header::CONTENT_LENGTH, total)
        .body(axum::body::Body::from(bytes))
        .unwrap()
}

// ── policy rules ─────────────────────────────────────────────────────────────

async fn get_policy_rules_handler(State(state): State<GatewayState>) -> impl IntoResponse {
    let engine = state.policy_arc.read().await;
    let rules: HashMap<String, &'static str> = engine.config.rules.iter()
        .map(|(k, v)| (k.clone(), match v {
            Rule::Allow     => "allow",
            Rule::Ask       => "ask",
            Rule::Workspace => "workspace",
        }))
        .collect();
    Json(serde_json::json!({ "rules": rules }))
}

// ── Wake word trigger ─────────────────────────────────────────────────────────

static WAKE_ACTIVE: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

async fn wake_handler(State(state): State<GatewayState>) -> impl IntoResponse {
    // One wake sequence at a time
    if WAKE_ACTIVE.compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst).is_err() {
        return StatusCode::CONFLICT.into_response();
    }

    tokio::spawn(async move {
        // 1. Piper "yes?" — wait for it to finish so mic captures after the ding
        let model = std::env::var("PIPER_MODEL").unwrap_or_default();
        if !model.is_empty() {
            let stamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_micros();
            let wav = format!("/tmp/apex_wake_ding_{stamp}.wav");
            let wav = wav.as_str();
            if let Ok(mut child) = tokio::process::Command::new("piper")
                .args(["--model", &model, "--output_file", wav])
                .stdin(std::process::Stdio::piped())
                .spawn()
            {
                if let Some(mut stdin) = child.stdin.take() {
                    use tokio::io::AsyncWriteExt;
                    let _ = stdin.write_all(b"yes?").await;
                }
                let _ = child.wait().await;
                let _ = tokio::process::Command::new("aplay")
                    .args(["-q", wav])
                    .output().await;
                let _ = tokio::fs::remove_file(wav).await;
            }
        }

        // 2. Signal the frontend to start recording
        let _ = state.bcast.send(apexos_core::Event::WakeTriggered);

        WAKE_ACTIVE.store(false, Ordering::SeqCst);
    });

    StatusCode::OK.into_response()
}

// ── Server-side mic recording (ALSA → whisper, no browser getUserMedia needed) ─

const SERVER_WAV: &str = "/tmp/apex_stt_server.wav";

static SERVER_RECORDER: OnceLock<tokio::sync::Mutex<Option<tokio::process::Child>>> = OnceLock::new();

fn recorder_lock() -> &'static tokio::sync::Mutex<Option<tokio::process::Child>> {
    SERVER_RECORDER.get_or_init(|| tokio::sync::Mutex::new(None))
}

async fn record_start_handler() -> impl IntoResponse {
    let device = std::env::var("ALSA_CAPTURE_DEVICE")
        .unwrap_or_else(|_| "plughw:2,0".into());

    // Kill any in-flight recording
    {
        let mut guard = recorder_lock().lock().await;
        if let Some(mut c) = guard.take() { let _ = c.kill().await; }
    }
    let _ = tokio::fs::remove_file(SERVER_WAV).await;

    match tokio::process::Command::new("arecord")
        .args(["-D", &device, "-f", "S16_LE", "-r", "16000", "-c", "1", "-d", "30", SERVER_WAV])
        .spawn()
    {
        Ok(child) => {
            *recorder_lock().lock().await = Some(child);
            StatusCode::OK.into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("arecord: {e}")).into_response(),
    }
}

async fn record_stop_handler() -> impl IntoResponse {
    // Stop the recorder
    {
        let mut guard = recorder_lock().lock().await;
        if let Some(mut c) = guard.take() { let _ = c.kill().await; }
    }
    // Small yield so arecord flushes its WAV header
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;

    let model = std::env::var("WHISPER_MODEL")
        .unwrap_or_else(|_| "/var/lib/agentd/whisper/ggml-tiny.en.bin".into());
    let bin = std::env::var("WHISPER_BIN")
        .unwrap_or_else(|_| "/usr/local/bin/whisper-cpp".into());

    let result = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        tokio::process::Command::new(&bin)
            .args(["-m", &model, "-f", SERVER_WAV, "-nt", "-l", "en", "--no-prints"])
            .output(),
    ).await;
    let _ = tokio::fs::remove_file(SERVER_WAV).await;

    match result {
        Ok(Ok(out)) => {
            let raw = String::from_utf8_lossy(&out.stdout);
            let text = raw.lines()
                .map(|l| l.trim())
                .filter(|l| !l.is_empty() && *l != "[BLANK_AUDIO]")
                .collect::<Vec<_>>()
                .join(" ");
            Json(serde_json::json!({ "text": text })).into_response()
        }
        Ok(Err(e)) => (StatusCode::INTERNAL_SERVER_ERROR, format!("whisper: {e}")).into_response(),
        Err(_)     => (StatusCode::GATEWAY_TIMEOUT, "whisper timed out").into_response(),
    }
}

// ── Voice: STT + TTS ─────────────────────────────────────────────────────────

async fn transcribe_handler(body: axum::body::Bytes) -> impl IntoResponse {
    if body.is_empty() {
        return (StatusCode::BAD_REQUEST, "empty audio").into_response();
    }

    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros();
    let tmp_in  = format!("/tmp/apex_stt_{stamp}.webm");
    let tmp_wav = format!("/tmp/apex_stt_{stamp}.wav");

    if let Err(e) = tokio::fs::write(&tmp_in, &body).await {
        return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response();
    }

    // Convert to 16kHz mono WAV
    let ff = tokio::process::Command::new("ffmpeg")
        .args(["-y", "-i", &tmp_in, "-ar", "16000", "-ac", "1", &tmp_wav])
        .output().await;
    let _ = tokio::fs::remove_file(&tmp_in).await;
    if let Err(e) = ff {
        return (StatusCode::INTERNAL_SERVER_ERROR, format!("ffmpeg: {e}")).into_response();
    }
    let ff_out = ff.unwrap();
    if !ff_out.status.success() {
        let _ = tokio::fs::remove_file(&tmp_wav).await;
        let stderr = String::from_utf8_lossy(&ff_out.stderr).to_string();
        return (StatusCode::INTERNAL_SERVER_ERROR, format!("ffmpeg failed: {stderr}")).into_response();
    }

    let model = std::env::var("WHISPER_MODEL")
        .unwrap_or_else(|_| "/var/lib/agentd/whisper/ggml-tiny.en.bin".into());
    let bin = std::env::var("WHISPER_BIN")
        .unwrap_or_else(|_| "/usr/local/bin/whisper-cpp".into());

    let result = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        tokio::process::Command::new(&bin)
            .args(["-m", &model, "-f", &tmp_wav, "-nt", "-l", "en", "--no-prints"])
            .output(),
    ).await;
    let _ = tokio::fs::remove_file(&tmp_wav).await;

    match result {
        Ok(Ok(out)) => {
            let raw = String::from_utf8_lossy(&out.stdout);
            let text = raw.lines()
                .map(|l| l.trim())
                .filter(|l| !l.is_empty())
                .collect::<Vec<_>>()
                .join(" ");
            Json(serde_json::json!({ "text": text })).into_response()
        }
        Ok(Err(e)) => (StatusCode::INTERNAL_SERVER_ERROR, format!("whisper: {e}")).into_response(),
        Err(_)     => (StatusCode::GATEWAY_TIMEOUT, "whisper timed out (30s)").into_response(),
    }
}

async fn speak_handler(Json(body): Json<serde_json::Value>) -> impl IntoResponse {
    let text = match body["text"].as_str() {
        Some(t) if !t.trim().is_empty() => t.to_string(),
        _ => return StatusCode::BAD_REQUEST.into_response(),
    };

    tokio::spawn(async move {
        if let Ok(model) = std::env::var("PIPER_MODEL") {
            let stamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_micros();
            let wav = format!("/tmp/apex_speak_{stamp}.wav");
            if let Ok(mut child) = tokio::process::Command::new("piper")
                .args(["--model", &model, "--output_file", &wav])
                .stdin(std::process::Stdio::piped())
                .spawn()
            {
                if let Some(mut stdin) = child.stdin.take() {
                    use tokio::io::AsyncWriteExt;
                    let _ = stdin.write_all(text.as_bytes()).await;
                }
                let _ = child.wait().await;
                let _ = tokio::process::Command::new("aplay")
                    .args(["-q", &wav])
                    .output().await;
                let _ = tokio::fs::remove_file(&wav).await;
            }
        } else {
            let _ = tokio::process::Command::new("espeak-ng")
                .args(["-a", "100", "-s", "150", &text])
                .output().await;
        }
    });

    StatusCode::OK.into_response()
}

// ── PTY terminal ─────────────────────────────────────────────────────────────

async fn terminal_ws_handler(ws: WebSocketUpgrade) -> impl IntoResponse {
    ws.on_upgrade(handle_terminal_ws)
}

unsafe fn open_pty_session() -> Option<(i32, i32, std::process::Child)> {
    use std::os::unix::io::FromRawFd;
    use std::os::unix::process::CommandExt;

    let mut master_fd: libc::c_int = -1;
    let mut slave_fd:  libc::c_int = -1;
    let ws = libc::winsize { ws_row: 24, ws_col: 80, ws_xpixel: 0, ws_ypixel: 0 };
    if libc::openpty(&mut master_fd, &mut slave_fd,
                     std::ptr::null_mut(), std::ptr::null(), &ws) != 0 {
        eprintln!("[terminal] openpty: {}", std::io::Error::last_os_error());
        return None;
    }

    let slave_out = libc::dup(slave_fd);
    let slave_err = libc::dup(slave_fd);
    if slave_out < 0 || slave_err < 0 {
        libc::close(master_fd); libc::close(slave_fd);
        if slave_out >= 0 { libc::close(slave_out); }
        return None;
    }

    let mut cmd = std::process::Command::new("/bin/bash");
    cmd.env("TERM", "xterm-256color")
       .env("HOME", std::env::var("HOME").unwrap_or_else(|_| "/root".into()))
       .stdin(std::process::Stdio::from_raw_fd(slave_fd))
       .stdout(std::process::Stdio::from_raw_fd(slave_out))
       .stderr(std::process::Stdio::from_raw_fd(slave_err));

    // post-fork pre-exec: new session + controlling terminal via fd 0 (stdin = slave)
    cmd.pre_exec(|| unsafe {
        libc::setsid();
        libc::ioctl(0, libc::TIOCSCTTY as _, 0i32);
        Ok(())
    });

    let child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => { eprintln!("[terminal] spawn: {e}"); libc::close(master_fd); return None; }
    };

    let mr = libc::dup(master_fd);
    let mw = libc::dup(master_fd);
    libc::close(master_fd);
    if mr < 0 || mw < 0 {
        // dup failed: reap the bash child and close whichever fd did succeed,
        // so we don't leak a zombie process or a file descriptor.
        if mr >= 0 { libc::close(mr); }
        if mw >= 0 { libc::close(mw); }
        let mut child = child;
        let _ = child.kill();
        let _ = child.wait();
        return None;
    }

    Some((mr, mw, child))
}

async fn handle_terminal_ws(socket: WebSocket) {
    let (mr, mw, mut child) = match unsafe { open_pty_session() } {
        Some(t) => t,
        None    => return,
    };

    // Separate fd for resize ioctls so mw can be moved into the writer thread
    let mw_resize = unsafe { libc::dup(mw) };

    let (from_pty_tx, mut from_pty_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
    let (to_pty_tx,   to_pty_rx)       = std::sync::mpsc::channel::<Vec<u8>>();

    std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            let n = unsafe { libc::read(mr, buf.as_mut_ptr() as _, buf.len()) };
            if n <= 0 { break; }
            if from_pty_tx.blocking_send(buf[..n as usize].to_vec()).is_err() { break; }
        }
        unsafe { libc::close(mr); }
    });

    std::thread::spawn(move || {
        for data in to_pty_rx {
            unsafe { libc::write(mw, data.as_ptr() as _, data.len()); }
        }
        unsafe { libc::close(mw); }
    });

    let (mut sink, mut stream) = socket.split();

    let mut ws_write = tokio::spawn(async move {
        while let Some(data) = from_pty_rx.recv().await {
            if sink.send(Message::Binary(data.into())).await.is_err() { break; }
        }
    });

    let mut ws_read = tokio::spawn(async move {
        while let Some(Ok(msg)) = stream.next().await {
            match msg {
                Message::Text(text) => {
                    if text.starts_with('{') {
                        if let Ok(val) = serde_json::from_str::<serde_json::Value>(&text) {
                            if val["type"].as_str() == Some("resize") {
                                let cols = val["cols"].as_u64().unwrap_or(80) as libc::c_ushort;
                                let rows = val["rows"].as_u64().unwrap_or(24) as libc::c_ushort;
                                unsafe {
                                    let ws = libc::winsize {
                                        ws_col: cols, ws_row: rows,
                                        ws_xpixel: 0, ws_ypixel: 0,
                                    };
                                    libc::ioctl(mw_resize, libc::TIOCSWINSZ as _, &ws);
                                }
                                continue;
                            }
                        }
                    }
                    let _ = to_pty_tx.send(text.as_bytes().to_vec());
                }
                Message::Binary(data) => { let _ = to_pty_tx.send(data.to_vec()); }
                Message::Close(_) => break,
                _ => {}
            }
        }
        unsafe { libc::close(mw_resize); }
        drop(to_pty_tx);
    });

    tokio::select! {
        _ = &mut ws_write => { ws_read.abort(); }
        _ = &mut ws_read  => { ws_write.abort(); }
    }
    let _ = child.kill();
    // Reap the child so it doesn't become a zombie process.
    let _ = tokio::task::spawn_blocking(move || child.wait()).await;
    eprintln!("[terminal] session closed");
}

// ── Council ───────────────────────────────────────────────────────────────────

/// POST /api/council — start a new council session from the UI.
/// Body: { topic, agents, max_rounds?, consensus_threshold? }
async fn council_start_handler(
    State(state): State<GatewayState>,
    Json(mut body): Json<serde_json::Value>,
) -> impl IntoResponse {
    let id = format!("gw{}", state.council_next_id.fetch_add(1, std::sync::atomic::Ordering::SeqCst));
    body["council_id"] = serde_json::json!(id);
    // Use sentinel session/call so no spurious ToolResult lands on an agent turn
    let session = apexos_core::SessionId(u64::MAX);
    let call_id = apexos_core::ActionId(u64::MAX);
    if state.council_start_tx.send((session, call_id, body)).await.is_err() {
        return (StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": "council handler unavailable"}))).into_response();
    }
    Json(serde_json::json!({"council_id": id})).into_response()
}

/// GET /api/council — list all council sessions (running + complete).
async fn council_list_handler(
    State(state): State<GatewayState>,
) -> impl IntoResponse {
    let sessions = state.council_sessions.lock().await;
    Json(sessions.clone()).into_response()
}

/// GET /api/council/:id — detail for a single council session.
async fn council_detail_handler(
    State(state): State<GatewayState>,
    Path(id):     Path<String>,
) -> impl IntoResponse {
    let sessions = state.council_sessions.lock().await;
    match sessions.iter().find(|r| r.id == id) {
        Some(r) => Json(r.clone()).into_response(),
        None    => (StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "council not found"}))).into_response(),
    }
}

/// POST /api/council/:id/butt-in — inject a human message into a running council.
/// Body: { message: "..." }
async fn council_butt_in_handler(
    State(state): State<GatewayState>,
    Path(id):     Path<String>,
    Json(body):   Json<serde_json::Value>,
) -> impl IntoResponse {
    let msg = body["message"].as_str().unwrap_or("").to_owned();
    if msg.is_empty() {
        return (StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "message required"}))).into_response();
    }
    let map = state.council_butt_in.lock().await;
    match map.get(&id) {
        Some(tx) => {
            if tx.send(msg).await.is_ok() {
                Json(serde_json::json!({"ok": true})).into_response()
            } else {
                (StatusCode::GONE,
                    Json(serde_json::json!({"error": "council channel closed"}))).into_response()
            }
        }
        None => (StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "council not active or not found"}))).into_response(),
    }
}

// ── Mesh ──────────────────────────────────────────────────────────────────────

/// GET /api/mesh/nodes — run avahi-browse and return discovered _apexos._tcp nodes.
/// Each entry includes whether the node is already in peers.toml ("known").
async fn mesh_nodes_handler(State(state): State<GatewayState>) -> impl IntoResponse {
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        tokio::process::Command::new("avahi-browse")
            .args(["-rpt", "_apexos._tcp", "--no-db-lookup"])
            .output(),
    ).await;

    let raw = match result {
        Ok(Ok(o)) => String::from_utf8_lossy(&o.stdout).to_string(),
        _ => String::new(),
    };

    let discovered = mesh::parse_avahi_output(&raw);
    let registry   = state.peer_registry.read().await;
    let my_node_id = state.node_id.as_str();

    let nodes: Vec<serde_json::Value> = discovered.into_iter()
        .filter(|(node_id, _)| node_id != my_node_id)  // don't list self
        .map(|(node_id, ip)| {
            let known   = registry.contains(&node_id);
            let ws_url  = format!("ws://{}:8787", ip);
            serde_json::json!({
                "node_id": node_id,
                "ip":      ip,
                "port":    8787,
                "ws_url":  ws_url,
                "known":   known,
            })
        })
        .collect();

    Json(serde_json::json!({ "nodes": nodes }))
}

/// GET /api/mesh/peers — list peers.toml contents (tokens REDACTED).
async fn mesh_peers_get_handler(State(state): State<GatewayState>) -> impl IntoResponse {
    let registry = state.peer_registry.read().await;
    // Never serialize the per-peer token: it's the peer's secret credential.
    // Clients only need to know whether one is set (drives the a2a-ready dot).
    let peers: Vec<serde_json::Value> = registry.peers.iter().map(|p| serde_json::json!({
        "node_id":   p.node_id,
        "ws_url":    p.ws_url,
        "role":      p.role.to_string(),
        "status":    p.status,
        "has_token": p.token.is_some(),
    })).collect();
    Json(serde_json::json!({ "peers": peers }))
}

/// POST /api/mesh/peers — add or update a peer.
/// Body: { node_id, ws_url, role?, token? }  (token = the peer's AGENTD_TOKEN, for a2a)
async fn mesh_peers_post_handler(
    State(state): State<GatewayState>,
    Json(body):   Json<serde_json::Value>,
) -> impl IntoResponse {
    let node_id = match body["node_id"].as_str().filter(|s| !s.is_empty()) {
        Some(s) => s.to_string(),
        None    => return Json(serde_json::json!({ "ok": false, "error": "missing node_id" })),
    };
    let ws_url = match body["ws_url"].as_str().filter(|s| !s.is_empty()) {
        Some(s) => s.to_string(),
        None    => return Json(serde_json::json!({ "ok": false, "error": "missing ws_url" })),
    };
    let role = match body["role"].as_str().unwrap_or("full") {
        "sensor" => PeerRole::Sensor,
        "thin"   => PeerRole::Thin,
        _        => PeerRole::Full,
    };
    let token_in = body["token"].as_str().filter(|s| !s.is_empty()).map(str::to_string);

    let result = {
        let mut registry = state.peer_registry.write().await;
        // Preserve an existing token when the caller didn't supply one (e.g. a
        // ws_url/status-only re-add from REFRESH shouldn't wipe the a2a credential).
        let token = token_in.or_else(|| registry.peers.iter()
            .find(|p| p.node_id == node_id).and_then(|p| p.token.clone()));
        let record = PeerRecord { node_id: node_id.clone(), ws_url: ws_url.clone(), role, status: "online".into(), token };
        registry.add(record)
    };

    match result {
        Ok(_) => {
            state.bus.emit(apexos_core::Event::PeerRegistered {
                node_id, ws_url, role: "full".into(),
            }).await;
            Json(serde_json::json!({ "ok": true }))
        }
        Err(e) => Json(serde_json::json!({ "ok": false, "error": e.to_string() })),
    }
}

/// DELETE /api/mesh/peers/:id — remove a peer by node_id.
async fn mesh_peers_delete_handler(
    State(state): State<GatewayState>,
    Path(id):     Path<String>,
) -> impl IntoResponse {
    let result = {
        let mut registry = state.peer_registry.write().await;
        registry.remove(&id)
    };
    match result {
        Ok(true)  => Json(serde_json::json!({ "ok": true })),
        Ok(false) => Json(serde_json::json!({ "ok": false, "error": "peer not found" })),
        Err(e)    => Json(serde_json::json!({ "ok": false, "error": e.to_string() })),
    }
}

// ── Vast.ai API handlers ──────────────────────────────────────────────────────

async fn vast_recipes_handler(
    State(_state): State<GatewayState>,
) -> impl IntoResponse {
    match load_recipes() {
        Ok(rf) => {
            let out = serde_json::json!({
                "docker":    rf.docker,
                "gpu_tiers": rf.gpu_tiers,
                "recipes":   rf.recipes,
            });
            (StatusCode::OK, Json(out))
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        ),
    }
}

#[derive(Deserialize)]
struct RecipeSaveBody {
    content: String,
}

async fn vast_recipes_save_handler(
    State(_): State<GatewayState>,
    Json(body): Json<RecipeSaveBody>,
) -> impl IntoResponse {
    let path = apexos_plugins::vast::recipes_path();
    match tokio::fs::write(&path, &body.content).await {
        Ok(_)  => (StatusCode::OK, Json(serde_json::json!({ "ok": true }))),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        ),
    }
}

async fn vast_status_handler(
    State(state): State<GatewayState>,
) -> impl IntoResponse {
    let vs    = &state.vast_state;
    let inst  = vs.instance.read().await.clone();
    let phase = vs.phase.read().await.clone();
    let status = match &phase {
        VastPhase::Idle            => "idle",
        VastPhase::Launching { .. } => "launching",
        VastPhase::Ready            => "ready",
        VastPhase::Destroying       => "destroying",
    };
    let mut val = serde_json::json!({ "status": status });
    if let VastPhase::Launching { phase: p } = &phase {
        val["launch_phase"] = serde_json::json!(p);
    }
    if let Some(i) = inst {
        val["instance"] = serde_json::to_value(&i).unwrap_or_default();
    }
    Json(val)
}

#[derive(Deserialize)]
struct VastOffersQuery {
    gpu: Option<String>,
    geo: Option<String>,
}

async fn vast_offers_handler(
    State(_state): State<GatewayState>,
    Query(q): Query<VastOffersQuery>,
) -> impl IntoResponse {
    let api_key = match std::env::var("VAST_API_KEY") {
        Ok(k) if !k.is_empty() => k,
        _ => return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "VAST_API_KEY not set" })),
        ),
    };

    // Build GPU filter from tier or raw name
    let gpu_filter = if let Some(gpu) = &q.gpu {
        if let Ok(rf) = load_recipes() {
            if let Some(tier) = rf.gpu_tiers.get(gpu.as_str()) {
                tier.vast_names.iter().map(|n| format!("gpu_name={n}")).collect::<Vec<_>>().join(" | ")
            } else {
                format!("gpu_name={gpu}")
            }
        } else {
            format!("gpu_name={gpu}")
        }
    } else {
        "".into()
    };

    let query = if gpu_filter.is_empty() {
        "reliability>0.99 inet_down>300 rentable=true".into()
    } else {
        format!("({gpu_filter}) reliability>0.99 inet_down>300 rentable=true")
    };

    let out = tokio::process::Command::new("vastai")
        .args(["search", "offers", &query, "--order", "dph_total", "--raw"])
        .env("VAST_API_KEY", &api_key)
        .output()
        .await;

    match out {
        Ok(o) if o.status.success() => {
            let text = String::from_utf8_lossy(&o.stdout);
            let mut offers: Vec<serde_json::Value> = serde_json::from_str(&text).unwrap_or_default();

            // Apply geo filter if requested
            if let Some(geo) = &q.geo {
                let codes: Vec<&str> = match geo.as_str() {
                    "EU_NORDIC" => vec!["SE", "NO", "FI", "DK", "IS"],
                    "EU"        => vec!["SE", "NO", "FI", "DK", "IS", "DE", "NL", "FR", "GB", "PL"],
                    "US"        => vec!["US"],
                    _           => vec![],
                };
                if !codes.is_empty() {
                    offers.retain(|o| {
                        let loc = o["geolocation"].as_str().unwrap_or("");
                        codes.iter().any(|c| loc.contains(c))
                    });
                }
            }

            // Return slim fields
            let slim: Vec<serde_json::Value> = offers.iter().map(|o| serde_json::json!({
                "id":           o["id"],
                "gpu_name":     o["gpu_name"],
                "dph_total":    o["dph_total"],
                "vram_mb":      o["gpu_ram"],
                "geolocation":  o["geolocation"],
                "reliability":  o["reliability2"],
                "inet_down":    o["inet_down"],
            })).collect();

            (StatusCode::OK, Json(serde_json::json!(slim)))
        }
        Ok(o) => {
            let err = String::from_utf8_lossy(&o.stderr);
            (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({ "error": err.trim() })))
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": format!("vastai not found: {e}") })),
        ),
    }
}

#[derive(Deserialize)]
struct HfSearchQuery {
    q: String,
}

async fn vast_hf_search_handler(
    State(_state): State<GatewayState>,
    Query(q): Query<HfSearchQuery>,
) -> impl IntoResponse {
    // Proxy HuggingFace API for GGUF model search
    let url = format!(
        "https://huggingface.co/api/models?search={}&filter=gguf&sort=downloads&limit=20",
        urlencoding(&q.q)
    );
    let out = tokio::process::Command::new("curl")
        .args(["-s", "--max-time", "10", &url])
        .output()
        .await;
    match out {
        Ok(o) if o.status.success() => {
            let text  = String::from_utf8_lossy(&o.stdout);
            let models: Vec<serde_json::Value> = serde_json::from_str(&text).unwrap_or_default();
            let slim: Vec<serde_json::Value> = models.iter().take(20).map(|m| serde_json::json!({
                "id":        m["id"],
                "downloads": m["downloads"],
                "likes":     m["likes"],
            })).collect();
            (StatusCode::OK, Json(serde_json::json!(slim)))
        }
        _ => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": "HF search failed" })),
        ),
    }
}

fn urlencoding(s: &str) -> String {
    s.chars().map(|c| match c {
        'A'..='Z' | 'a'..='z' | '0'..='9' | '-' | '_' | '.' | '~' => c.to_string(),
        ' ' => "+".into(),
        c   => format!("%{:02X}", c as u32),
    }).collect()
}

// ── Notes API handlers ──────────────────────────────────────────────────────
// Plain-text notebook shared with APEX: notes are `.md` files under
// <workspace>/notes. The UI lists/reads/writes them here; APEX reads/appends
// the same files via the notes_* tools (apexos-tools). One flat dir, no
// subfolders — keep it dead simple.

/// The notes directory: <AGENTD_WORKSPACE or /var/lib/agentd/workspace>/notes.
fn notes_dir() -> std::path::PathBuf {
    let ws = std::env::var("AGENTD_WORKSPACE")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "/var/lib/agentd/workspace".to_string());
    std::path::Path::new(&ws).join("notes")
}

/// Reduce an arbitrary name to a safe `.md` filename inside the notes dir:
/// strip any path components (defeats `../` traversal), default a blank stem,
/// and force a `.md` extension. Returns None if nothing usable remains.
fn sanitize_note_name(name: &str) -> Option<String> {
    let stem = std::path::Path::new(name.trim())
        .file_name()
        .and_then(|s| s.to_str())?
        .trim();
    if stem.is_empty() || stem == "." || stem == ".." {
        return None;
    }
    let stem = stem.strip_suffix(".md").unwrap_or(stem);
    if stem.is_empty() { return None; }
    Some(format!("{stem}.md"))
}

/// GET /api/notes — list note files in the workspace notes dir.
async fn notes_list_handler() -> impl IntoResponse {
    let dir = notes_dir();
    let mut files: Vec<serde_json::Value> = Vec::new();

    if let Ok(mut rd) = tokio::fs::read_dir(&dir).await {
        while let Ok(Some(entry)) = rd.next_entry().await {
            let p = entry.path();
            let ext = p.extension().and_then(|e| e.to_str()).unwrap_or("");
            if !matches!(ext, "md" | "markdown" | "txt") { continue; }
            let meta = entry.metadata().await.ok();
            let size = meta.as_ref().map(|m| m.len()).unwrap_or(0);
            files.push(serde_json::json!({
                "name": p.file_name().and_then(|n| n.to_str()).unwrap_or(""),
                "size": size,
            }));
        }
    }

    files.sort_by(|a, b| {
        let an = a["name"].as_str().unwrap_or("");
        let bn = b["name"].as_str().unwrap_or("");
        an.cmp(bn)
    });

    Json(serde_json::json!({ "files": files }))
}

#[derive(Deserialize)]
struct NoteReadBody {
    name: String,
}

/// POST /api/notes/read — return the content of one note. Body: { name }.
async fn notes_read_handler(
    Json(body): Json<NoteReadBody>,
) -> impl IntoResponse {
    let Some(name) = sanitize_note_name(&body.name) else {
        return (StatusCode::BAD_REQUEST, Json(serde_json::json!({ "error": "invalid note name" }))).into_response();
    };
    let path = notes_dir().join(&name);
    match tokio::fs::read_to_string(&path).await {
        Ok(content) => Json(serde_json::json!({ "name": name, "content": content })).into_response(),
        Err(e) => (StatusCode::NOT_FOUND, Json(serde_json::json!({ "error": e.to_string() }))).into_response(),
    }
}

#[derive(Deserialize)]
struct NoteWriteBody {
    name: String,
    content: String,
}

/// POST /api/notes/write — create or overwrite a note. Body: { name, content }.
async fn notes_write_handler(
    Json(body): Json<NoteWriteBody>,
) -> impl IntoResponse {
    let Some(name) = sanitize_note_name(&body.name) else {
        return (StatusCode::BAD_REQUEST, Json(serde_json::json!({ "error": "invalid note name" }))).into_response();
    };
    let dir = notes_dir();
    if let Err(e) = tokio::fs::create_dir_all(&dir).await {
        return (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({ "error": e.to_string() }))).into_response();
    }
    let path = dir.join(&name);
    match tokio::fs::write(&path, body.content.as_bytes()).await {
        Ok(()) => Json(serde_json::json!({ "ok": true, "name": name })).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({ "error": e.to_string() }))).into_response(),
    }
}

// ── Sketch API handlers ─────────────────────────────────────────────────────
// The Sketchpad app posts its strokes as JSON; we rasterise them to a PNG with
// tiny-skia (server-side keeps the UI binary lean) under <workspace>/sketches.
// APEX views the result via the sketch_snapshot tool → describe_image/read_file.

/// The sketches directory: <AGENTD_WORKSPACE or default>/sketches.
fn sketches_dir() -> std::path::PathBuf {
    let ws = std::env::var("AGENTD_WORKSPACE")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "/var/lib/agentd/workspace".to_string());
    std::path::Path::new(&ws).join("sketches")
}

#[derive(Deserialize)]
struct SketchPoint { x: f32, y: f32 }

#[derive(Deserialize)]
struct SketchStroke {
    color: String,          // "#rrggbb"
    width: f32,
    points: Vec<SketchPoint>,
}

#[derive(Deserialize)]
struct SketchBody {
    width: u32,
    height: u32,
    #[serde(default)]
    bg: Option<String>,     // "#rrggbb", default dark slate
    strokes: Vec<SketchStroke>,
}

/// Parse "#rrggbb" (or "rrggbb") → (r,g,b). Falls back to the given default.
fn parse_hex_rgb(s: &str, default: (u8, u8, u8)) -> (u8, u8, u8) {
    let h = s.trim().trim_start_matches('#');
    if h.len() == 6 {
        if let Ok(v) = u32::from_str_radix(h, 16) {
            return (((v >> 16) & 0xff) as u8, ((v >> 8) & 0xff) as u8, (v & 0xff) as u8);
        }
    }
    default
}

/// POST /api/sketch — rasterise posted strokes to a PNG and save it.
async fn sketch_save_handler(
    Json(body): Json<SketchBody>,
) -> impl IntoResponse {
    let w = body.width.clamp(16, 4096);
    let h = body.height.clamp(16, 4096);

    let png = match tokio::task::spawn_blocking(move || rasterise_sketch(w, h, &body)).await {
        Ok(Ok(bytes)) => bytes,
        Ok(Err(e)) => return (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({ "error": e }))).into_response(),
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({ "error": e.to_string() }))).into_response(),
    };

    let dir = sketches_dir();
    if let Err(e) = tokio::fs::create_dir_all(&dir).await {
        return (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({ "error": e.to_string() }))).into_response();
    }
    let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S").to_string();
    let stamped = dir.join(format!("sketch-{stamp}.png"));
    let latest  = dir.join("latest.png");
    if let Err(e) = tokio::fs::write(&stamped, &png).await {
        return (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({ "error": e.to_string() }))).into_response();
    }
    let _ = tokio::fs::write(&latest, &png).await;

    Json(serde_json::json!({
        "ok": true,
        "path": stamped.to_string_lossy(),
        "latest": latest.to_string_lossy(),
    })).into_response()
}

/// Draw the strokes onto a Pixmap and PNG-encode it. Runs on a blocking thread.
fn rasterise_sketch(w: u32, h: u32, body: &SketchBody) -> Result<Vec<u8>, String> {
    use tiny_skia::{Pixmap, Paint, PathBuilder, Stroke, Transform, Color, LineCap, LineJoin};

    let mut pixmap = Pixmap::new(w, h).ok_or("invalid sketch dimensions")?;
    let (br, bg_, bb) = parse_hex_rgb(body.bg.as_deref().unwrap_or("#0d0f18"), (13, 15, 24));
    pixmap.fill(Color::from_rgba8(br, bg_, bb, 255));

    let stroke_style = |width: f32| Stroke {
        width: width.max(0.5),
        line_cap: LineCap::Round,
        line_join: LineJoin::Round,
        ..Default::default()
    };

    for s in &body.strokes {
        if s.points.is_empty() { continue; }
        let (r, g, b) = parse_hex_rgb(&s.color, (230, 230, 235));
        let mut paint = Paint::default();
        paint.set_color_rgba8(r, g, b, 255);
        paint.anti_alias = true;

        let mut pb = PathBuilder::new();
        if s.points.len() == 1 {
            // A tap = a dot: round-capped zero-length segment renders a filled circle.
            let p = &s.points[0];
            pb.move_to(p.x, p.y);
            pb.line_to(p.x + 0.01, p.y);
        } else {
            pb.move_to(s.points[0].x, s.points[0].y);
            for p in &s.points[1..] {
                pb.line_to(p.x, p.y);
            }
        }
        if let Some(path) = pb.finish() {
            pixmap.stroke_path(&path, &paint, &stroke_style(s.width), Transform::identity(), None);
        }
    }

    pixmap.encode_png().map_err(|e| e.to_string())
}

/// GET /api/sketch/latest — path to the most recent saved sketch (if any).
async fn sketch_latest_handler() -> impl IntoResponse {
    let latest = sketches_dir().join("latest.png");
    let exists = tokio::fs::metadata(&latest).await.is_ok();
    Json(serde_json::json!({
        "exists": exists,
        "path": if exists { latest.to_string_lossy().to_string() } else { String::new() },
    }))
}

// ── Audio API handlers ────────────────────────────────────────────────────────

/// GET /api/audio/files — list audio files in workspace dirs.
async fn audio_files_handler() -> impl IntoResponse {
    let search_dirs = vec![
        "/var/lib/agentd/workspace/sonus",
        "/var/lib/agentd/workspace",
    ];
    let exts = ["mp3", "wav", "flac", "ogg", "m4a", "aac"];
    let mut files: Vec<serde_json::Value> = Vec::new();

    for dir in &search_dirs {
        let mut rd = match tokio::fs::read_dir(dir).await {
            Ok(r) => r,
            Err(_) => continue,
        };
        while let Ok(Some(entry)) = rd.next_entry().await {
            let p = entry.path();
            let ext = p.extension().and_then(|e| e.to_str()).unwrap_or("");
            if !exts.contains(&ext) { continue; }
            let meta = entry.metadata().await.ok();
            let size = meta.as_ref().map(|m| m.len()).unwrap_or(0);
            files.push(serde_json::json!({
                "path": p.to_string_lossy(),
                "name": p.file_name().and_then(|n| n.to_str()).unwrap_or(""),
                "size": size,
            }));
        }
    }

    files.sort_by(|a, b| {
        let an = a["name"].as_str().unwrap_or("");
        let bn = b["name"].as_str().unwrap_or("");
        an.cmp(bn)
    });

    Json(serde_json::json!({ "files": files }))
}

/// GET /api/workspace/images — list image files under the workspace for the
/// native UI's attach picker (the seed of a workspace file-explorer). Scans the
/// workspace root and the image-bearing subdirs (screenshots/, sketches/,
/// uploads/, images/), newest first. Paths are workspace-relative so they round-
/// trip cleanly through the `user_prompt` `images:[{path}]` (workspace-confined).
async fn workspace_images_handler() -> impl IntoResponse {
    let ws = std::env::var("AGENTD_WORKSPACE")
        .ok().filter(|s| !s.is_empty())
        .unwrap_or_else(|| "/var/lib/agentd/workspace".to_string());
    let ws_path = std::path::Path::new(&ws);
    let exts = ["png", "jpg", "jpeg", "gif", "webp", "bmp"];
    let subdirs = ["", "screenshots", "sketches", "uploads", "images"];
    let mut images: Vec<serde_json::Value> = Vec::new();
    let mut seen = std::collections::HashSet::new();

    for sub in subdirs {
        let dir = if sub.is_empty() { ws_path.to_path_buf() } else { ws_path.join(sub) };
        let mut rd = match tokio::fs::read_dir(&dir).await {
            Ok(r) => r,
            Err(_) => continue,
        };
        while let Ok(Some(entry)) = rd.next_entry().await {
            let p = entry.path();
            let ext = p.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
            if !exts.contains(&ext.as_str()) { continue; }
            let abs = p.to_string_lossy().to_string();
            if !seen.insert(abs.clone()) { continue; }
            // Workspace-relative path (falls back to absolute if not under ws).
            let rel = p.strip_prefix(ws_path).map(|r| r.to_string_lossy().to_string())
                .unwrap_or_else(|_| abs.clone());
            let meta = entry.metadata().await.ok();
            let size = meta.as_ref().map(|m| m.len()).unwrap_or(0);
            let modified = meta.as_ref().and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs()).unwrap_or(0);
            images.push(serde_json::json!({
                "path": rel,
                "name": p.file_name().and_then(|n| n.to_str()).unwrap_or(""),
                "size": size,
                "modified": modified,
            }));
        }
    }

    // Newest first — most useful for "the screenshot I just took".
    images.sort_by(|a, b| b["modified"].as_u64().unwrap_or(0).cmp(&a["modified"].as_u64().unwrap_or(0)));
    Json(serde_json::json!({ "images": images }))
}

/// GET /api/workspace/list?path=<rel> — browse the workspace tree for the Explorer
/// app. Returns the entries directly under <workspace>/<path>: directories first,
/// then files, alpha within each. Confined to the workspace. `path` is
/// workspace-relative; `abs` lets a co-located UI load image previews directly.
async fn workspace_list_handler(
    Query(params): Query<HashMap<String, String>>,
) -> impl IntoResponse {
    let rel = params.get("path").map(|s| s.as_str()).unwrap_or("");
    let dir = match resolve_workspace_path(rel) {
        Ok(d) => d,
        Err(e) => return Json(serde_json::json!({ "error": e, "path": rel, "entries": [] })),
    };
    let ws = std::env::var("AGENTD_WORKSPACE").ok().filter(|s| !s.is_empty())
        .unwrap_or_else(|| "/var/lib/agentd/workspace".to_string());
    let ws_canon = std::fs::canonicalize(&ws).unwrap_or_else(|_| std::path::PathBuf::from(&ws));

    let mut entries: Vec<serde_json::Value> = Vec::new();
    let mut rd = match tokio::fs::read_dir(&dir).await {
        Ok(r) => r,
        Err(e) => return Json(serde_json::json!({ "error": format!("read dir: {e}"), "path": rel, "entries": [] })),
    };
    while let Ok(Some(entry)) = rd.next_entry().await {
        let p = entry.path();
        let name = match p.file_name().and_then(|n| n.to_str()) { Some(n) => n.to_string(), None => continue };
        if name.starts_with('.') { continue; } // skip dotfiles
        let meta = entry.metadata().await.ok();
        let is_dir = meta.as_ref().map(|m| m.is_dir()).unwrap_or(false);
        let size = meta.as_ref().filter(|m| m.is_file()).map(|m| m.len()).unwrap_or(0);
        let modified = meta.as_ref().and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs()).unwrap_or(0);
        let ext = p.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
        let rel_path = p.strip_prefix(&ws_canon).map(|r| r.to_string_lossy().to_string())
            .unwrap_or_else(|_| name.clone());
        entries.push(serde_json::json!({
            "name": name,
            "kind": if is_dir { "dir" } else { "file" },
            "size": size,
            "modified": modified,
            "ext": ext,
            "path": rel_path,
            "abs": p.to_string_lossy(),
        }));
    }
    // Dirs first, then files; alpha (case-insensitive) within each group.
    entries.sort_by(|a, b| {
        let ad = a["kind"] == "dir"; let bd = b["kind"] == "dir";
        bd.cmp(&ad).then_with(|| {
            a["name"].as_str().unwrap_or("").to_ascii_lowercase()
                .cmp(&b["name"].as_str().unwrap_or("").to_ascii_lowercase())
        })
    });
    Json(serde_json::json!({ "path": rel, "entries": entries }))
}

/// GET /api/workspace/read?path=<rel> — read a workspace text file for the Explorer
/// preview pane. Capped at 256 KB; a binary file (NUL byte) reports binary:true
/// with no content.
async fn workspace_read_handler(
    Query(params): Query<HashMap<String, String>>,
) -> impl IntoResponse {
    const CAP: usize = 256 * 1024;
    let rel = params.get("path").map(|s| s.as_str()).unwrap_or("");
    let path = match resolve_workspace_path(rel) {
        Ok(p) => p,
        Err(e) => return Json(serde_json::json!({ "error": e })),
    };
    let bytes = match tokio::fs::read(&path).await {
        Ok(b) => b,
        Err(e) => return Json(serde_json::json!({ "error": format!("read: {e}") })),
    };
    let truncated = bytes.len() > CAP;
    let slice = &bytes[..bytes.len().min(CAP)];
    let binary = slice.contains(&0u8);
    let content = if binary { String::new() } else { String::from_utf8_lossy(slice).to_string() };
    Json(serde_json::json!({ "content": content, "truncated": truncated, "binary": binary }))
}

#[derive(Deserialize)]
struct AudioPathBody {
    path: String,
}

/// POST /api/audio/analyze — run ffprobe + ffmpeg loudnorm analysis.
async fn audio_analyze_handler(
    Json(body): Json<AudioPathBody>,
) -> impl IntoResponse {
    let path = body.path.clone();
    let result = tokio::task::spawn_blocking(move || {
        audio_analyze_inner_gw(&path)
    }).await;

    match result {
        Ok(Ok(stats)) => (StatusCode::OK, Json(stats)).into_response(),
        Ok(Err(e)) => (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({ "error": e }))).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({ "error": e.to_string() }))).into_response(),
    }
}

/// POST /api/audio/waveform — extract amplitude envelope for canvas rendering.
/// Body: { path, samples? } — returns { samples: [f32], duration_s: f64 }
#[derive(Deserialize)]
struct WaveformBody {
    path: String,
    samples: Option<usize>,
}

async fn audio_waveform_handler(
    Json(body): Json<WaveformBody>,
) -> impl IntoResponse {
    let path = body.path.clone();
    let n = body.samples.unwrap_or(1200).min(4096);

    let result = tokio::task::spawn_blocking(move || {
        // Get duration first via ffprobe
        let probe = std::process::Command::new("ffprobe")
            .args(["-v", "quiet", "-print_format", "json", "-show_format", &path])
            .output()
            .map_err(|e| e.to_string())?;
        let info: serde_json::Value = serde_json::from_slice(&probe.stdout)
            .unwrap_or_default();
        let duration_s: f64 = info["format"]["duration"].as_str()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0.0);

        // Sample at 4000 Hz mono → compute max-envelope bins
        let out = std::process::Command::new("ffmpeg")
            .args(["-i", &path, "-ac", "1", "-ar", "4000", "-f", "f32le", "pipe:1"])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .output()
            .map_err(|e| e.to_string())?;

        let bytes = out.stdout;
        let total_samples = bytes.len() / 4;
        if total_samples == 0 {
            return Err("no PCM output from ffmpeg".to_string());
        }

        let raw: Vec<f32> = bytes.chunks_exact(4)
            .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
            .collect();

        // Max-envelope into n bins
        let bin_size = (total_samples / n).max(1);
        let envelope: Vec<f32> = (0..n)
            .map(|i| {
                let start = i * bin_size;
                let end = ((i + 1) * bin_size).min(raw.len());
                if start >= raw.len() { return 0.0f32; }
                raw[start..end].iter().map(|s| s.abs()).fold(0.0f32, f32::max)
            })
            .collect();

        Ok((envelope, duration_s))
    }).await;

    match result {
        Ok(Ok((samples, duration_s))) => (StatusCode::OK, Json(serde_json::json!({
            "samples": samples,
            "duration_s": duration_s,
        }))).into_response(),
        Ok(Err(e)) => (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({ "error": e }))).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({ "error": e.to_string() }))).into_response(),
    }
}

/// POST /api/audio/process — apply an op chain to an audio file.
/// Body: { path, ops: [{type, ...params}], output_path? }
#[derive(Deserialize)]
struct ProcessBody {
    path: String,
    ops: Vec<serde_json::Value>,
    output_path: Option<String>,
}

async fn audio_process_handler(
    Json(body): Json<ProcessBody>,
) -> impl IntoResponse {
    let path = body.path.clone();
    let ops = body.ops.clone();

    // Default output path: <stem>_edit.<ext>
    let output_path = match body.output_path.clone() {
        Some(p) => p,
        None => {
            let p = std::path::Path::new(&path);
            let stem = p.file_stem().and_then(|s| s.to_str()).unwrap_or("track");
            let ext  = p.extension().and_then(|s| s.to_str()).unwrap_or("mp3");
            let dir  = p.parent().and_then(|d| d.to_str()).unwrap_or(".");
            format!("{dir}/{stem}_edit.{ext}")
        }
    };

    let out = output_path.clone();
    let result = tokio::task::spawn_blocking(move || {
        apply_audio_ops(&path, &ops, &output_path)
    }).await;

    match result {
        Ok(Ok(())) => (StatusCode::OK, Json(serde_json::json!({ "output_path": out }))).into_response(),
        Ok(Err(e)) => (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({ "error": e }))).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({ "error": e.to_string() }))).into_response(),
    }
}

/// Build and run an ffmpeg command from an op list.
fn apply_audio_ops(path: &str, ops: &[serde_json::Value], out: &str) -> Result<(), String> {
    let mut af_filters: Vec<String> = Vec::new();
    let mut start_s: Option<f64> = None;
    let mut end_s: Option<f64>   = None;

    for op in ops {
        match op["type"].as_str().unwrap_or("") {
            "trim" => {
                start_s = op["start_s"].as_f64();
                end_s   = op["end_s"].as_f64();
            }
            "normalize" => {
                let target = op["target_lufs"].as_f64().unwrap_or(-14.0);
                let tp     = op["true_peak"].as_f64().unwrap_or(-2.0);
                af_filters.push(format!("loudnorm=I={target}:TP={tp}:LRA=11"));
            }
            "peak_limit" => {
                let limit_db = op["limit_db"].as_f64().unwrap_or(-1.0);
                let linear   = 10f64.powf(limit_db / 20.0);
                af_filters.push(format!("alimiter=limit={linear:.4}:level_in=1:level_out=1:attack=5:release=50:asc=1"));
            }
            "trim_silence" => {
                let thresh = op["threshold_db"].as_f64().unwrap_or(-50.0);
                af_filters.push(format!(
                    "silenceremove=stop_periods=-1:stop_threshold={thresh}dB:stop_duration=0.5"
                ));
            }
            "fade_in" => {
                let d = op["duration_s"].as_f64().unwrap_or(1.0);
                af_filters.push(format!("afade=t=in:st=0:d={d}"));
            }
            "fade_out" => {
                let d = op["duration_s"].as_f64().unwrap_or(2.0);
                // Compute start from trim end or use 0 as placeholder (ffmpeg will clamp)
                let start = end_s.unwrap_or(0.0) - d;
                let start = start.max(0.0);
                af_filters.push(format!("afade=t=out:st={start:.3}:d={d}"));
            }
            "gain" => {
                let gain_db = op["gain_db"].as_f64().unwrap_or(0.0);
                if gain_db != 0.0 {
                    let linear = 10f64.powf(gain_db / 20.0);
                    af_filters.push(format!("volume={linear:.4}"));
                }
            }
            _ => {}
        }
    }

    // Build ffmpeg args
    let mut args: Vec<String> = vec!["-y".into(), "-i".into(), path.to_string()];
    if let Some(s) = start_s { args.extend(["-ss".into(), format!("{s:.3}")]); }
    if let Some(e) = end_s   { args.extend(["-to".into(), format!("{e:.3}")]); }
    if !af_filters.is_empty() {
        args.extend(["-af".into(), af_filters.join(",")]);
    }

    // Use stream copy only if no filters and trim requested (fast path)
    if af_filters.is_empty() && (start_s.is_some() || end_s.is_some()) {
        args.extend(["-c".into(), "copy".into()]);
    }

    args.push(out.to_string());

    let result = std::process::Command::new("ffmpeg")
        .args(&args)
        .output()
        .map_err(|e| e.to_string())?;

    if result.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&result.stderr);
        Err(stderr.lines().last().unwrap_or("ffmpeg error").to_string())
    }
}

/// Synchronous audio analysis for spawn_blocking contexts (mirrors apexos-tools logic).
fn audio_analyze_inner_gw(path: &str) -> Result<serde_json::Value, String> {
    // ffprobe
    let probe = std::process::Command::new("ffprobe")
        .args(["-v", "quiet", "-print_format", "json", "-show_streams", "-show_format", path])
        .output()
        .map_err(|e| e.to_string())?;
    let info: serde_json::Value = serde_json::from_slice(&probe.stdout)
        .map_err(|e| e.to_string())?;

    let format = info["format"]["format_name"].as_str().unwrap_or("").split(',').next().unwrap_or("").to_string();
    let duration_s: f64 = info["format"]["duration"].as_str()
        .and_then(|s| s.parse().ok()).unwrap_or(0.0);
    let bit_rate: u64 = info["format"]["bit_rate"].as_str()
        .and_then(|s| s.parse().ok()).unwrap_or(0);
    let stream0 = &info["streams"][0];
    let sample_rate: u32 = stream0["sample_rate"].as_str()
        .and_then(|s| s.parse().ok()).unwrap_or(0);
    let channels: u32 = stream0["channels"].as_u64().unwrap_or(0) as u32;

    // loudnorm
    let ln_out = std::process::Command::new("ffmpeg")
        .args(["-i", path, "-af", "loudnorm=print_format=json", "-f", "null", "-"])
        .output().map_err(|e| e.to_string())?;
    let ln_stderr = String::from_utf8_lossy(&ln_out.stderr).to_string();
    let ln = gw_extract_json(&ln_stderr).unwrap_or_default();
    let lufs_integrated: f64 = ln["input_i"].as_str()
        .and_then(|s| s.parse().ok()).unwrap_or(-99.0);

    // volumedetect
    let vd_out = std::process::Command::new("ffmpeg")
        .args(["-i", path, "-af", "volumedetect", "-f", "null", "-"])
        .output().map_err(|e| e.to_string())?;
    let vd_stderr = String::from_utf8_lossy(&vd_out.stderr).to_string();
    let peak_db = gw_parse_af_val(&vd_stderr, "max_volume").unwrap_or(-99.0);
    let rms_db  = gw_parse_af_val(&vd_stderr, "mean_volume").unwrap_or(-99.0);

    // silencedetect
    let sd_out = std::process::Command::new("ffmpeg")
        .args(["-i", path, "-af", "silencedetect=noise=-50dB:d=0.5", "-f", "null", "-"])
        .output().map_err(|e| e.to_string())?;
    let sd_stderr = String::from_utf8_lossy(&sd_out.stderr).to_string();
    let (silence_start_s, silence_end_s) = gw_parse_silence(&sd_stderr, duration_s);

    Ok(serde_json::json!({
        "duration_s":      duration_s,
        "sample_rate":     sample_rate,
        "channels":        channels,
        "format":          format,
        "bit_rate":        bit_rate,
        "peak_db":         peak_db,
        "rms_db":          rms_db,
        "lufs_integrated": lufs_integrated,
        "silence_start_s": silence_start_s,
        "silence_end_s":   silence_end_s,
        "has_clipping":    peak_db > -0.1,
        "dc_offset":       0.0,
    }))
}

fn gw_extract_json(text: &str) -> Option<serde_json::Value> {
    let start = text.rfind('{')?;
    let mut depth = 0usize;
    let mut end = start;
    for (i, c) in text[start..].char_indices() {
        match c {
            '{' => depth += 1,
            '}' => { depth -= 1; if depth == 0 { end = start + i + 1; break; } }
            _ => {}
        }
    }
    if depth != 0 { return None; }
    serde_json::from_str(&text[start..end]).ok()
}

fn gw_parse_af_val(text: &str, key: &str) -> Option<f64> {
    text.lines()
        .find(|l| l.contains(key))?
        .splitn(2, ':').nth(1)?
        .split_whitespace().next()?
        .parse().ok()
}

fn gw_parse_silence(text: &str, duration_s: f64) -> (f64, f64) {
    let mut first_end: Option<f64> = None;
    let mut last_start: Option<f64> = None;
    for line in text.lines() {
        if line.contains("silence_start:") {
            if let Some(v) = line.split("silence_start:").nth(1)
                .and_then(|s| s.split_whitespace().next())
                .and_then(|s| s.parse().ok()) {
                last_start = Some(v);
            }
        }
        if line.contains("silence_end:") {
            if let Some(v) = line.split("silence_end:").nth(1)
                .and_then(|s| s.split_whitespace().next())
                .and_then(|s| s.parse().ok()) {
                if first_end.is_none() { first_end = Some(v); }
            }
        }
    }
    let silence_start_s = first_end.unwrap_or(0.0);
    let silence_end_s   = last_start.map(|s| (duration_s - s).max(0.0)).unwrap_or(0.0);
    (silence_start_s, silence_end_s)
}

// ── serve ─────────────────────────────────────────────────────────────────────

pub async fn serve(state: GatewayState, addr: SocketAddr) -> anyhow::Result<()> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, router(state)).await?;
    Ok(())
}

#[cfg(test)]
mod auth_tests {
    use super::*;

    #[test]
    fn tokens_match_equal() {
        assert!(tokens_match("s3cret-token", "s3cret-token"));
    }

    #[test]
    fn tokens_match_rejects_mismatch_and_length() {
        assert!(!tokens_match("s3cret-token", "wrong-token!"));
        assert!(!tokens_match("short", "longer-token"));
        assert!(!tokens_match("", "nonempty"));
    }

    #[test]
    fn percent_encoded_query_token_decodes() {
        // A token containing reserved chars arrives percent-encoded in ?token=.
        let expected = "a b+c/d";
        let encoded  = "a%20b%2Bc%2Fd";
        let decoded  = percent_encoding::percent_decode_str(encoded).decode_utf8_lossy();
        assert!(tokens_match(&decoded, expected));
    }
}

#[cfg(test)]
mod image_input_tests {
    use super::*;

    // A valid 1×1 transparent PNG — exercises the real vision shim end-to-end.
    const PNG_1X1_B64: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGP4z8DwHwAFAAH/iZk9HQAAAABJRU5ErkJggg==";

    #[tokio::test]
    async fn prepare_user_images_shims_a_b64_png() {
        let raw = vec![serde_json::json!({ "b64": PNG_1X1_B64 })];
        let prepared = prepare_user_images(&raw).await;
        assert_eq!(prepared.len(), 1);
        assert!(prepared[0].media_type.starts_with("image/"));
        assert!(!prepared[0].data.is_empty());
    }

    #[tokio::test]
    async fn prepare_user_images_skips_garbage_and_missing_refs() {
        // A non-image b64 and a ref with neither path nor b64 are both dropped —
        // one bad image must never sink the whole prompt.
        let raw = vec![
            serde_json::json!({ "b64": "bm90LWFuLWltYWdl" }), // "not-an-image"
            serde_json::json!({ "note": "neither path nor b64" }),
        ];
        assert!(prepare_user_images(&raw).await.is_empty());
    }

    #[test]
    fn workspace_path_confinement_rejects_escape() {
        std::env::set_var("AGENTD_WORKSPACE", "/tmp");
        // An absolute system file outside the workspace is rejected …
        assert!(resolve_workspace_path("/etc/passwd").is_err());
        // … as is a `../` traversal escape.
        assert!(resolve_workspace_path("../etc/passwd").is_err());
        std::env::remove_var("AGENTD_WORKSPACE");
    }
}
