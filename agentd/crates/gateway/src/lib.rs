use axum::{
    Json, Router, middleware,
    extract::{
        ConnectInfo, Path, Query, Request, State,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    Extension,
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    routing::{delete, get, post},
};
pub mod backend_config;
pub mod history_config;

pub mod compute;

pub mod mesh;
pub mod mesh_link;
pub use mesh::{parse_avahi_output, PeerRecord, PeerRegistry, PeerRole};
pub mod beacon;
pub use beacon::{new_liveness_map, spawn_beacon_loop, LivenessMap};
pub mod session_auth;
pub use session_auth::{AuthRole, RequestAuth, SessionAuth, SessionStore};
mod sensor_ingress;
use sensor_ingress::SensorIngress;
use serde::{Deserialize, Serialize};
use futures_util::{SinkExt, StreamExt};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::{broadcast, Mutex, RwLock};
use apexos_core::{ActionId, BusHandle, ClientEvent, Event, Message as CoreMessage, SessionId};
use apexos_plugins::{
    policy_toml_set_mode, load_recipes, PolicyEngine, PolicyMode, Rule, VastPhase, VastState,
};
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

/// A request to consolidate a session into Cerebro (summary + key discoveries →
/// `session_save`), sent from the gateway handler to the agentd-side worker that
/// owns the LLM provider + Cerebro ToolProxy. `reply` carries the result JSON the
/// HTTP handler returns (`{ok, memory_id?, summary?}` or `{ok:false, error}`).
pub struct ConsolidateReq {
    pub session_id: u64,
    pub reply:      tokio::sync::oneshot::Sender<serde_json::Value>,
}

/// Delete or archive a session (SA-8). Serialized through the agentd turn
/// gate so an in-flight persist cannot recreate the JSONL.
#[derive(Debug, Clone, Copy)]
pub enum SessionRetireKind { Delete, Archive }

pub struct SessionRetireReq {
    pub session_id: u64,
    pub kind:       SessionRetireKind,
    pub reply:      tokio::sync::oneshot::Sender<serde_json::Value>,
}

/// A federation Cerebro call (colony-federation Slices 1+2): the gateway's
/// `/api/mesh/memory` and `/api/mesh/recall` handlers validate the peer payload
/// into ready tool args (the pure `mesh::federated_*` fns), then send this to
/// the agentd-side worker that owns the Cerebro ToolProxy (unavailable here at
/// GatewayState build time — same seam as `ConsolidateReq`). `tool` is the
/// Cerebro tool to run (`remember` for an import, `recall` for a federated
/// query); `reply` carries the tool's JSON on success, or an error string.
pub struct MeshMemoryReq {
    pub tool:  String,
    pub args:  serde_json::Value,
    pub reply: tokio::sync::oneshot::Sender<Result<serde_json::Value, String>>,
}

/// A blocking cross-node sub-agent spawn (colony-mesh Slice 3). The gateway's
/// `/api/spawn` handler sends this to the agentd spawn worker (which owns the turn
/// engine); `reply` carries the result JSON (`{ok, output}` or `{ok:false, error}`).
pub struct SpawnReq {
    pub prompt:    String,
    pub system:    Option<String>,
    pub timeout_s: u64,
    /// Inbound `x-mesh-hops` after [`apexos_core::parse_mesh_hops`].
    pub hops:      u32,
    pub reply:     tokio::sync::oneshot::Sender<serde_json::Value>,
}

/// W2 mesh workers: a peer-originated worker-tier request. `Fanout`/`Query`/
/// `Cancel` arrive on the HOSTING side (this node mints/serves/cancels real
/// local workers for a remote conductor); `Report` arrives on the CONDUCTING
/// side (a peer pushing its settled batch home). The handlers validate `from`
/// against the peer registry (the mesh/memory pattern — never token-only),
/// resolve the sender's a2a landing session for fanouts (the minted batch's
/// parent), and forward here; `reply` carries the JSON returned verbatim.
pub struct WorkerMeshReq {
    pub kind:   WorkerMeshKind,
    pub from:   String,
    pub body:   serde_json::Value,
    /// Fanout only: the sender-peer's a2a landing session on THIS node.
    pub parent: Option<SessionId>,
    /// Inbound mesh hops (fanout). Other worker kinds leave this 0.
    pub hops:   u32,
    pub reply:  tokio::sync::oneshot::Sender<serde_json::Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkerMeshKind { Fanout, Query, Cancel, Report }

#[derive(Clone)]
pub struct GatewayState {
    pub bus:                   BusHandle,
    pub bcast:                 broadcast::Sender<Event>,
    /// Anthropic API key — set via env or browser UI key-entry flow
    pub api_key:               Arc<RwLock<String>>,
    /// OAI-compatible key (OpenRouter / Together / etc.) — separate from Anthropic key
    /// OpenAI-compat key ring (oai / openrouter / xai slots — coexist, no first-wins).
    pub oai_keys:              Arc<RwLock<apexos_agent::OaiKeyRing>>,
    pub model:                 Arc<RwLock<String>>,
    /// Prompt-cache policy (Anthropic) — live-tunable from the Settings UI via /api/cache.
    pub cache:                 Arc<RwLock<apexos_agent::CacheConfig>>,
    /// Active inference backend — live-swappable: "anthropic" | "openrouter" | "xai" | "ollama" | "vllm" | "oai"
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
    /// Per-session history window budget (rough tokens; 0 = trimming off) —
    /// live-tunable from Settings via /api/history, read by the router per turn.
    pub history_budget:        Arc<std::sync::atomic::AtomicUsize>,
    /// Shared secret for /sensor-bridge WS connections. Empty = no auth
    /// (loopback bench only). A non-loopback bind refuses to start if empty.
    pub sensor_bridge_token:   Arc<String>,
    /// Shared secret for /mesh-bridge WS connections (ApexNET P5c). Empty =
    /// no auth required, matching the sensor-bridge convention.
    pub mesh_bridge_token:     Arc<String>,
    /// The radio lane's seam (docs/apexnet.md §6.1). Present whether or not a
    /// bridge ever connects — no bridge simply reads as "lane down".
    pub mesh_link:             mesh_link::MeshLink,
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
    /// Per-peer active-liveness, written by the downtime beacon loop and folded into
    /// `GET /api/mesh/peers` so the UI shows each node alive/dark + last-seen.
    pub liveness:          LivenessMap,
    /// Sensor-alert sensitivity PROFILE (standard / smoker / kitchen / workshop), shared
    /// with the agentd sensor-alert loop, which reads it per reading. `POST
    /// /api/sensors/config` sets it + persists; agentd seeds it from the same file at
    /// startup. See agentd `sensor_config.rs`.
    pub sensor_profile:    Arc<std::sync::RwLock<String>>,
    /// Where the sensitivity profile persists (`<log_dir>/sensor_config.json`).
    pub sensor_config_path: PathBuf,
    /// Active mesh pairing offer (in-memory only, never persisted). See mesh::Pairing.
    pub pairing:           Arc<std::sync::Mutex<Option<mesh::Pairing>>>,
    /// In-flight redeem (nonce + claimed URL). `pair/confirm` must echo the nonce.
    pub redeem_flight:     Arc<std::sync::Mutex<Option<mesh::RedeemFlight>>>,
    /// Own node_id (hostname) — used by discovery loop to avoid self-bootstrap
    pub node_id:           Arc<String>,
    /// Mesh a2a routing: peer node_id → the session on THIS node that holds that
    /// peer's conversation thread. Allocated once (from `next_session_id`) on a
    /// peer's first inbound message so each peer's a2a stays in its own session
    /// instead of flooding root session 0 / the user's active chat. Persisted to
    /// `mesh_sessions_path` so the thread survives a restart. See
    /// `session_message_handler` + `mesh_session_for`.
    pub mesh_sessions:      Arc<std::sync::Mutex<HashMap<String, SessionId>>>,
    /// On-disk JSON backing for `mesh_sessions` (`<log_dir>/mesh_sessions.json`).
    pub mesh_sessions_path: PathBuf,
    /// Per-peer-thread unread counts (session id → state), bumped on each inbound
    /// a2a and persisted so the UI's inbox unread survives a restart. See
    /// `mesh_inbox_handler` / `mesh_inbox_read_handler`.
    pub mesh_unread:        MeshInbox,
    /// On-disk JSON backing for `mesh_unread` (`<log_dir>/mesh_unread.json`).
    pub mesh_unread_path:   PathBuf,
    /// Federation observability counters (principle 6, receiver-side v1):
    /// peer node_id → inbound memories/duplicates/recall stats. Bumped by
    /// `mesh_memory_handler` + `mesh_recall_handler`, folded into
    /// `GET /api/mesh/peers`, persisted to `<log_dir>/mesh_fed_stats.json`.
    pub fed_stats:          FedStats,
    pub fed_stats_path:     PathBuf,
    /// Session-consolidation requests → the agentd-side worker (which owns the LLM
    /// provider + Cerebro ToolProxy, unavailable here at GatewayState build time).
    /// The handler sends a `ConsolidateReq` and awaits its oneshot reply. See
    /// `session_consolidate_handler` + `consolidate::run` (agentd).
    pub consolidate_tx:     tokio::sync::mpsc::Sender<ConsolidateReq>,
    /// Session delete/archive → the agentd router (owns TurnGate + SessionStore).
    /// The handler sends a `SessionRetireReq` and awaits its oneshot reply.
    pub session_retire_tx:  tokio::sync::mpsc::Sender<SessionRetireReq>,
    /// Blocking cross-node spawn requests → the agentd spawn worker (which owns the
    /// turn engine). The `/api/spawn` handler sends a `SpawnReq` and awaits its
    /// oneshot reply. See `spawn_handler` + the worker in `spawn_agent_router`.
    pub spawn_tx:           tokio::sync::mpsc::Sender<SpawnReq>,
    /// W2 mesh workers: peer worker-tier requests → the agentd worker driver's
    /// mesh arm (fanout/query/cancel on the hosting side, report-home on the
    /// conducting side). See `worker_fanout_handler` and friends.
    pub worker_mesh_tx:     tokio::sync::mpsc::Sender<WorkerMeshReq>,
    /// W2 kill switch (`AGENTD_MESH_WORKERS`, boot-read; default on): when off,
    /// every `/api/worker/*` mesh endpoint refuses — and the driver refuses
    /// `task_fanout{node}` symmetrically.
    pub mesh_workers_enabled: bool,
    /// Federated memory imports → the agentd-side worker owning the Cerebro
    /// ToolProxy (colony-federation Slice 1). See `mesh_memory_handler`.
    pub mesh_memory_tx:     tokio::sync::mpsc::Sender<MeshMemoryReq>,
    /// This node's structured capability snapshot (senses/tools/tier), refreshed by
    /// agentd's embodiment loop and served at `GET /api/capabilities` for mesh
    /// capability discovery (colony-mesh Slice 2).
    pub capabilities:       Arc<RwLock<serde_json::Value>>,
    /// Vast.ai instance + tunnel state — shared with supervisor for virtual tools
    pub vast_state:        VastState,
    /// Per-session agent bindings (multi-agent runtime). A `hello` frame may bind
    /// its session to an agent; the supervisor stamp + CCBS boot resolve identity
    /// here. See docs/agent-identity.md (slice 3b).
    pub session_bindings:  apexos_core::SessionBindings,
    /// Per-session active persona/skin (ui-glowup G5 tier-2). The UI sends the chosen
    /// persona over the WS (`set_persona` frame / a `persona` field on `hello`); the
    /// router reads it to append the matching response-style fragment.
    pub persona_sessions:  apexos_core::PersonaSessions,
    /// The identity registry (users + agents). The API mutates it; the router
    /// reads it for per-agent souls. See docs/agent-identity.md (slice 3a/3c).
    pub identities:        Arc<RwLock<apexos_core::Identities>>,
    /// In-memory PIN guess-lockout, keyed by user id (never persisted).
    pub pin_lockouts:      Arc<std::sync::Mutex<HashMap<String, PinLockout>>>,
    /// In-memory human-login session tokens (agent-identity.md slice 3e). Minted by
    /// `/api/auth/login`, accepted by `require_token` alongside the admin token, and
    /// cleared on restart — never persisted. Lets the desktop UI / PWA authenticate
    /// without the shared `AGENTD_TOKEN`.
    pub sessions:          Arc<std::sync::Mutex<SessionStore>>,
}

/// Per-user PIN guess-lockout: N consecutive failures locks verification for a
/// cooldown. In-memory only — a restart clears it (consistent with the mesh
/// pairing lockout). A 4–6 digit PIN's real protection is this, not hash strength.
#[derive(Default)]
pub struct PinLockout {
    fails:        u32,
    locked_until: Option<std::time::Instant>,
}

const PIN_MAX_FAILS: u32 = 5;
const PIN_LOCKOUT_SECS: u64 = 300;

impl PinLockout {
    /// Remaining lockout in seconds, or None if not currently locked.
    fn locked_for(&self, now: std::time::Instant) -> Option<u64> {
        self.locked_until
            .and_then(|u| (u > now).then(|| (u - now).as_secs()))
    }
    /// Record a verify outcome. Success resets; the Nth consecutive failure arms
    /// the cooldown (and resets the counter so it re-arms after it expires).
    fn record(&mut self, ok: bool, now: std::time::Instant) {
        if ok {
            self.fails = 0;
            self.locked_until = None;
        } else {
            self.fails += 1;
            if self.fails >= PIN_MAX_FAILS {
                self.locked_until = Some(now + std::time::Duration::from_secs(PIN_LOCKOUT_SECS));
                self.fails = 0;
            }
        }
    }
}

/// Check Bearer token on all gated routes.
/// Accepts "Authorization: Bearer <token>" header or "?token=<token>" query param.
/// No-op when AGENTD_TOKEN is unset (empty string) — treated as Admin (tests/dev).
/// Inserts [`RequestAuth`] so `require_admin` and handlers can see the role.
async fn require_token(
    State(state): State<GatewayState>,
    mut req: Request,
    next: middleware::Next,
) -> Response {
    let token = state.api_token.as_str();
    if token.is_empty() {
        req.extensions_mut().insert(RequestAuth::Admin);
        return next.run(req).await;
    }
    let from_header = req.headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .unwrap_or("");
    if tokens_match(from_header, token) {
        req.extensions_mut().insert(RequestAuth::Admin);
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
        req.extensions_mut().insert(RequestAuth::Admin);
        return next.run(req).await;
    }
    // Not the admin token — accept a valid minted human-login session token
    // (slice 3e). Either transport (header or ?token=) may carry it; the store is a
    // direct lookup over 256-bit opaque tokens, so no constant-time compare needed.
    if !from_header.is_empty() || !from_query.is_empty() {
        let now = std::time::Instant::now();
        let session = {
            let s = state.sessions.lock().unwrap_or_else(|e| e.into_inner());
            s.verify(from_header, now).cloned().or_else(|| s.verify(from_query.as_ref(), now).cloned())
        };
        if let Some(auth) = session {
            req.extensions_mut().insert(RequestAuth::Session(auth));
            return next.run(req).await;
        }
    }
    (StatusCode::UNAUTHORIZED, "invalid or missing token").into_response()
}

fn bearer_from_req(req: &Request) -> (String, String) {
    let from_header = req.headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .unwrap_or("")
        .to_string();
    let from_query_raw = req.uri().query().unwrap_or("")
        .split('&')
        .find_map(|p| p.strip_prefix("token="))
        .unwrap_or("");
    let from_query = percent_encoding::percent_decode_str(from_query_raw)
        .decode_utf8_lossy()
        .into_owned();
    (from_header, from_query)
}

/// Privileged REST: admin token or an Owner-role session. Guest session tokens
/// stay on /ws + their own sessions (finding 2).
async fn require_admin(req: Request, next: middleware::Next) -> Response {
    match req.extensions().get::<RequestAuth>() {
        Some(auth) if auth.is_privileged() => next.run(req).await,
        Some(_) => (StatusCode::FORBIDDEN, "owner or admin token required").into_response(),
        None => (StatusCode::UNAUTHORIZED, "invalid or missing token").into_response(),
    }
}

fn request_is_admin_token(state: &GatewayState, headers: &axum::http::HeaderMap) -> bool {
    let token = state.api_token.as_str();
    if token.is_empty() {
        return false;
    }
    let from_header = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .unwrap_or("");
    tokens_match(from_header, token)
}

/// Peer-facing mesh routes: admin token, human session, OR a per-peer inbound
/// mesh token. A mesh token is NOT accepted by `require_token` (so it cannot
/// reach `/api/run`). Inserts `RequestAuth` for admin/session so session
/// ownership still applies to UI posts.
async fn require_mesh_or_admin(
    State(state): State<GatewayState>,
    req: Request,
    next: middleware::Next,
) -> Response {
    let token = state.api_token.as_str();
    let (from_header, from_query) = bearer_from_req(&req);
    if token.is_empty()
        || tokens_match(&from_header, token)
        || tokens_match(&from_query, token)
    {
        let mut req = req;
        req.extensions_mut().insert(RequestAuth::Admin);
        return next.run(req).await;
    }
    let now = std::time::Instant::now();
    let session = {
        let s = state.sessions.lock().unwrap_or_else(|e| e.into_inner());
        s.verify(&from_header, now).cloned().or_else(|| s.verify(&from_query, now).cloned())
    };
    if let Some(auth) = session {
        let mut req = req;
        req.extensions_mut().insert(RequestAuth::Session(auth));
        return next.run(req).await;
    }
    let presented = if !from_header.is_empty() { from_header.as_str() } else { from_query.as_str() };
    let peer = state.peer_registry.try_read().ok()
        .and_then(|reg| reg.peer_id_for_inbound_token(presented));
    if let Some(node_id) = peer {
        let mut req = req;
        req.extensions_mut().insert(mesh::MeshPeerAuth { node_id });
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
    // Human-session routes: any valid admin or session token.
    let user_gated = Router::new()
        .route("/ws",              get(ws_handler))
        .route("/api/status",      get(status_handler))
        .route("/api/model",       get(get_model_handler))
        .route("/api/models",      get(get_models_handler))
        .route("/api/cache",       get(get_cache_handler))
        .route("/api/history",     get(get_history_handler))
        .route("/api/usage",       get(get_usage_handler))
        .route("/api/thermal/frame", get(thermal_frame_handler))
        .route("/api/backend",     get(get_backend_handler))
        .route("/api/soul",           get(get_soul_handler))
        .route("/api/evolution/history",  get(evolution_history_handler))
        .route("/api/evolution/stats",    get(evolution_stats_handler))
        .route("/api/sessions",           get(sessions_handler))
        .route("/api/sessions/active",    get(active_sessions_handler))
        .route("/api/events/recent",      get(events_recent_handler))
        .route("/api/sessions/{id}",            delete(session_delete_handler))
        .route("/api/sessions/{id}/archive",     post(session_archive_handler))
        .route("/api/sessions/{id}/consolidate", post(session_consolidate_handler))
        .route("/api/sessions/{id}/image",   post(session_image_handler))
        .route("/api/workspace/images",      get(workspace_images_handler))
        .route("/api/workspace/texts",       get(workspace_texts_handler))
        .route("/api/workspace/list",        get(workspace_list_handler))
        .route("/api/workspace/read",        get(workspace_read_handler))
        .route("/api/workspace/download",    get(workspace_download_handler))
        .route("/api/workspace/upload",      post(workspace_upload_handler).layer(axum::extract::DefaultBodyLimit::max(256 * 1024 * 1024)))
        .route("/api/workspace/mkdir",       post(workspace_mkdir_handler))
        .route("/api/workspace/delete",      post(workspace_delete_handler))
        .route("/api/workspace/rename",      post(workspace_rename_handler))
        .route("/api/workspace/move",        post(workspace_move_handler))
        .route("/api/workspace/copy",        post(workspace_copy_handler))
        .route("/api/media/candidates",   get(media_candidates_handler))
        .route("/api/snapshot",           get(snapshot_handler))
        .route("/api/sonus/files",        get(sonus_files_handler))
        .route("/api/sonus/stream",       get(sonus_stream_handler))
        .route("/api/sonus/play",         post(sonus_play_handler))
        .route("/api/sonus/stop",         post(sonus_stop_handler))
        .route("/api/transcribe",         post(transcribe_handler).layer(axum::extract::DefaultBodyLimit::max(16 * 1024 * 1024)))
        .route("/api/record/start",       post(record_start_handler))
        .route("/api/record/stop",        post(record_stop_handler))
        .route("/api/wake",               post(wake_handler))
        .route("/api/speak",              post(speak_handler))
        .route("/api/tts",                post(tts_handler))
        .route("/api/council",               get(council_list_handler))
        .route("/api/council/{id}",          get(council_detail_handler))
        .route("/api/capabilities",       get(capabilities_handler))
        .route("/api/sensors/config",     get(sensor_config_get_handler))
        .route("/api/voice",              get(get_voice_handler))
        .route("/api/imaginarium",        get(imaginarium_reach_handler))
        .route("/api/courier/status",     get(courier_status_handler))
        .route("/api/mesh/nodes",         get(mesh_nodes_handler))
        .route("/api/mesh/peers",         get(mesh_peers_get_handler))
        .route("/api/mesh/inbox",         get(mesh_inbox_handler))
        .route("/api/mesh/inbox/read",    post(mesh_inbox_read_handler))
        .route("/api/mesh/pair/status",   get(pair_status_handler))
        .route("/api/vast/recipes",       get(vast_recipes_handler))
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
        .route("/api/auth/logout",        post(auth_logout_handler))
        .route("/api/auth/me",            get(auth_me_handler))
        .route_layer(middleware::from_fn_with_state(state.clone(), require_token));

    // Privileged REST: AGENTD_TOKEN or an Owner-role session. Guest tokens 403.
    // Layers: require_token (outer) then require_admin.
    let admin_gated = Router::new()
        .route("/terminal-ws",     get(terminal_ws_handler))
        .route("/api/key",         post(set_key_handler))
        .route("/api/keys",        get(get_keys_handler).post(set_keys_handler))
        .route("/api/model",       post(set_model_handler))
        .route("/api/cache",       post(set_cache_handler))
        .route("/api/history",     post(set_history_handler))
        .route("/api/backend",     post(set_backend_handler))
        .route("/api/compute/discover", get(compute_discover_handler))
        .route("/api/policy",         post(set_policy_handler))
        .route("/api/policy/rules",   get(get_policy_rules_handler))
        .route("/api/soul",           post(set_soul_handler))
        .route("/api/power",              post(power_handler))
        .route("/api/sessions/export",    post(session_export_handler))
        .route("/api/media/eject",        post(media_eject_handler))
        .route("/api/media/plugged",      post(media_plugged_handler))
        .route("/api/media/prep",         post(media_prep_handler))
        .route("/api/run",                post(run_command_handler))
        .route("/api/council",               post(council_start_handler))
        .route("/api/council/{id}/butt-in",  post(council_butt_in_handler))
        .route("/api/sensors/config",     post(sensor_config_post_handler))
        .route("/api/voice",              post(set_voice_handler))
        .route("/api/mesh/peers",         post(mesh_peers_post_handler))
        .route("/api/mesh/peers/{id}",    delete(mesh_peers_delete_handler))
        .route("/api/mesh/pair/start",    post(pair_start_handler))
        .route("/api/mesh/pair/redeem",   post(pair_redeem_handler))
        .route("/api/mesh/gossip",        post(mesh_gossip_handler))
        .route("/api/vast/recipes",       post(vast_recipes_save_handler))
        .route("/api/identities",         get(identities_list_handler))
        .route("/api/identities/user",    post(identities_create_user_handler))
        .route("/api/identities/agent",   post(identities_create_agent_handler))
        .route("/api/identities/verify",  post(identities_verify_pin_handler))
        .route("/api/auth/default",       post(auth_default_handler))
        .route_layer(middleware::from_fn(require_admin))
        .route_layer(middleware::from_fn_with_state(state.clone(), require_token));

    // Peer-facing mesh POSTs: admin / session / per-peer inbound mesh token.
    // Mesh tokens are NOT valid on `gated` (so they cannot hit /api/run).
    let mesh_in = Router::new()
        .route("/api/sessions/{id}/message", post(session_message_handler))
        .route("/api/spawn",              post(spawn_handler))
        .route("/api/worker/fanout",      post(worker_fanout_handler))
        .route("/api/worker/query",       post(worker_query_handler))
        .route("/api/worker/cancel",      post(worker_cancel_mesh_handler))
        .route("/api/worker/report",      post(worker_report_mesh_handler).layer(axum::extract::DefaultBodyLimit::max(4 * 1024 * 1024)))
        .route("/api/mesh/file",          post(mesh_file_handler).layer(axum::extract::DefaultBodyLimit::max(8 * 1024 * 1024)))
        .route("/api/courier/manifest",   post(courier_manifest_handler))
        .route("/api/courier/receipt",    post(courier_receipt_handler))
        .route("/api/mesh/memory",        post(mesh_memory_handler).layer(axum::extract::DefaultBodyLimit::max(256 * 1024)))
        .route("/api/mesh/recall",        post(mesh_recall_handler))
        .route_layer(middleware::from_fn_with_state(state.clone(), require_mesh_or_admin));

    Router::new()
        .merge(user_gated)
        .merge(admin_gated)
        .merge(mesh_in)
        .route("/sensor-bridge",   get(sensor_bridge_ws_handler))
        .route("/mesh-bridge",     get(mesh_bridge_ws_handler))
        // UNgated: the pairing claim is authenticated by the short-lived code itself,
        // not the api_token (the whole point is the caller doesn't have our token yet).
        .route("/api/mesh/pair/claim", post(pair_claim_handler))
        .route("/api/mesh/pair/confirm", post(pair_confirm_handler))
        // UNgated: human login — PIN (or loopback-only open guest after claim).
        // LAN is closed until the owner profile has a PIN (finding 2).
        .route("/api/auth/login", post(auth_login_handler))
        .route("/api/auth/setup", post(auth_setup_handler))
        .route("/api/auth/profiles", get(auth_profiles_handler))
        // UNgated: the lean liveness ping (ApexNET §6.2/D8) — ~40 B of node_id +
        // uptime, nothing mDNS discovery doesn't already broadcast on this LAN. The
        // beacon probes THIS instead of pulling a multi-KB /api/capabilities body it
        // discards; future radio-side probes won't hold tokens at all.
        .route("/api/ping", get(ping_handler))
        .route("/api/connectivity", get(connectivity_handler))
        .fallback(static_handler)
        .with_state(state)
}

// ── WebSocket ─────────────────────────────────────────────────────────────────

async fn ws_handler(
    ws: WebSocketUpgrade,
    headers: axum::http::HeaderMap,
    axum::extract::RawQuery(query): axum::extract::RawQuery,
    State(state): State<GatewayState>,
) -> impl IntoResponse {
    // Resolve the connection's human session (slice 3e): a session-token client
    // → its SessionAuth (user + default agent), used to gate which agents a
    // `hello{agent_id}` may bind. None = the admin token / token-less dev path
    // (a trusted operator — not gated). require_token already authorized the
    // socket; this only recovers WHO, for the per-session bind gate.
    let auth = resolve_ws_auth(&state, headers, query.as_deref());
    ws.on_upgrade(move |socket| handle_socket(socket, state, auth))
}

/// Recover the `SessionAuth` behind an `Authorization: Bearer` request — Some only
/// for a valid *session* token (a logged-in human), None for the admin token or a
/// token-less node. Used by `/api/auth/me` so a logged-in client learns WHO it is.
fn resolve_req_auth(state: &GatewayState, headers: &axum::http::HeaderMap) -> Option<SessionAuth> {
    let bearer = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .unwrap_or("");
    let now = std::time::Instant::now();
    let s = state.sessions.lock().unwrap_or_else(|e| e.into_inner());
    s.verify(bearer, now).cloned()
}

/// Recover the `SessionAuth` behind a WS connection from its bearer/`?token=`
/// credential — Some only for a valid *session* token (a logged-in human), None
/// for the admin token or a token-less node. Mirrors `require_token`'s extraction.
fn resolve_ws_auth(
    state:   &GatewayState,
    headers: axum::http::HeaderMap,
    query:   Option<&str>,
) -> Option<SessionAuth> {
    let from_header = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .unwrap_or("");
    let from_query_raw = query.unwrap_or("")
        .split('&')
        .find_map(|p| p.strip_prefix("token="))
        .unwrap_or("");
    let from_query = percent_encoding::percent_decode_str(from_query_raw).decode_utf8_lossy();
    let now = std::time::Instant::now();
    let s = state.sessions.lock().unwrap_or_else(|e| e.into_inner());
    s.verify(from_header, now)
        .or_else(|| s.verify(from_query.as_ref(), now))
        .cloned()
}

/// The session a broadcast event belongs to, or `None` if it's a global/status
/// event every connected client should receive. The WS write task forwards a
/// session-scoped event only to the socket bound to that session — without this,
/// a client viewing session 42 also receives (and splices) session 43's deltas
/// and approval buttons (the multi-client / PWA bug). Conservative: only the
/// per-session conversation stream is scoped; anything whose routing is ambiguous
/// stays global (forwarded to all), so no status event is ever hidden. The
/// supervisor subscribes to the bus on its own, so this never affects routing.
fn event_session(event: &Event) -> Option<SessionId> {
    match event {
        Event::AgentText      { session, .. }
        | Event::AgentThinking  { session, .. }
        | Event::ToolRequested  { session, .. }
        | Event::TurnComplete   { session }
        | Event::ToolResult     { session, .. }
        | Event::ApprovalPending { session, .. }
        | Event::UserPrompt     { session, .. }
        | Event::UserApproval   { session, .. }
        | Event::UserCancel     { session } => Some(*session),
        Event::SubAgentStarted  { parent, .. } => Some(*parent),
        Event::Error            { session, .. } => *session, // already Option<SessionId>
        // Sensors, council, mesh/peers, plugins, vast, evolution, a2a — global
        // status; broadcast to every client (current behaviour).
        _ => None,
    }
}

async fn handle_socket(socket: WebSocket, state: GatewayState, auth: Option<SessionAuth>) {
    let mut rx = state.bcast.subscribe();
    let (mut sink, stream) = socket.split();

    // Sessions this socket bound to an agent — evicted from `session_bindings` when
    // the socket closes (slice 3e), so a resume must re-bind (and re-gate) rather
    // than silently inherit a stale identity. Shared with the read task.
    let bound_sessions: Arc<std::sync::Mutex<std::collections::HashSet<SessionId>>> =
        Arc::new(std::sync::Mutex::new(std::collections::HashSet::new()));

    // Priority channel: read task sends session_init frames; write task forwards them
    // before anything from the broadcast. Capacity 8 is enough for the hello + one resume.
    let (prio_tx, mut prio_rx) = tokio::sync::mpsc::channel::<String>(8);

    // Assign a fresh session_id immediately — no blocking on hello.
    let session_id = state.next_session_id.fetch_add(1, Ordering::SeqCst);

    // Register the fresh session at mint time — a connected-but-silent client
    // belongs in /api/sessions/active, and a later `hello{resume}` of a
    // never-prompted id must find its (empty) entry instead of silently keeping
    // the old session. (First UserPrompt used to be the only creator.)
    state.histories.lock().await.entry(SessionId(session_id)).or_default();
    if let Some(a) = &auth {
        session_auth::write_session_owner(&state.sessions_dir, session_id, &a.user_id);
    }

    // Initial bind (slice 3e): an authenticated human's first session resolves to
    // one of THEIR agents (their default) up front — so a guest can't act as APEX
    // (the node default) in the fresh session before explicitly picking an agent.
    // Admin / token-less connections stay unbound here (node default), as before.
    if let Some(a) = &auth {
        let owned: Vec<String> = {
            let ids = state.identities.read().await;
            ids.agents_for(&a.user_id).iter().map(|ag| ag.id.clone()).collect()
        };
        if let Some(agent) = session_auth::gate_agent_bind(a, "", &owned) {
            if let Ok(mut m) = state.session_bindings.lock() {
                m.insert(SessionId(session_id), agent);
            }
            if let Ok(mut b) = bound_sessions.lock() {
                b.insert(SessionId(session_id));
            }
        }
    }

    // The socket's current session, shared with the write task so it can drop
    // session-scoped events belonging to OTHER sessions. The read task updates it
    // on a `hello` resume (a client switching sessions). Lock-free atomic.
    let sock_session   = Arc::new(AtomicU64::new(session_id));
    let sock_session_w = sock_session.clone();

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
                        // Session-scoped events go only to the socket bound to that
                        // session; session-less (global/status) events go to all.
                        if let Some(s) = event_session(&event) {
                            if s.0 != sock_session_w.load(Ordering::Relaxed) { continue; }
                        }
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

    // Read task: decode ClientEvent only (prompt/approval/cancel/hello/persona).
    // The full Event enum is outbound; a client cannot mint ToolRequested etc.
    let bus      = state.bus.clone();
    let histories = state.histories.clone();
    let session_bindings = state.session_bindings.clone();
    let persona_sessions = state.persona_sessions.clone();  // G5 tier-2 — per-session persona
    let next_session_id = state.next_session_id.clone();   // for `hello{new:true}` (start a fresh chat)
    let identities = state.identities.clone();              // slice 3e — agent-bind gate
    let sessions_dir = state.sessions_dir.clone();          // finding 2 — resume ownership
    let conn_auth = auth.clone();                           // this socket's human session (if any)
    let bound_w = bound_sessions.clone();
    let read = tokio::spawn(async move {
        let mut stream   = stream;
        let mut session_id = session_id;   // mutable — updated by hello

        while let Some(Ok(msg)) = stream.next().await {
            let Message::Text(text) = msg else { continue };
            let client: ClientEvent = match serde_json::from_str(&text) {
                Ok(c) => c,
                Err(_) => continue,
            };
            match client {
                ClientEvent::Hello {
                    resume_session,
                    new: want_new,
                    persona,
                    agent_id,
                } => {
                    // Resume an existing session, start a brand-new one (`new:true`,
                    // the "+ New chat" button), or (neither) keep the current session.
                    let resume = resume_session.map(SessionId);
                    let hist = {
                        let mut lock = histories.lock().await;
                        match resume {
                            Some(s) if lock.contains_key(&s) => {
                                let allowed = match &conn_auth {
                                    None => true,
                                    Some(a) => session_auth::session_visible_to(&sessions_dir, s.0, a),
                                };
                                if allowed {
                                    session_id = s.0;
                                    lock.get(&s).cloned().unwrap_or_default()
                                } else {
                                    lock.get(&SessionId(session_id)).cloned().unwrap_or_default()
                                }
                            }
                            _ if want_new => {
                                // Fresh chat: a new id from the shared atomic, empty
                                // history — registered at mint like the connect path.
                                session_id = next_session_id.fetch_add(1, Ordering::SeqCst);
                                lock.insert(SessionId(session_id), Vec::new());
                                if let Some(a) = &conn_auth {
                                    session_auth::write_session_owner(&sessions_dir, session_id, &a.user_id);
                                }
                                vec![]
                            }
                            _ => vec![], // keep current session_id
                        }
                    };
                    // Keep the write task's per-session event filter in sync with
                    // the (possibly new) session this socket now follows.
                    sock_session.store(session_id, Ordering::Relaxed);
                    // G5 tier-2: a hello may carry the active persona, so a fresh /
                    // resumed session starts in the right voice (the live switch goes
                    // through `set_persona` below). Absent → leave it to the default.
                    if !persona.is_empty() {
                        if let Ok(mut m) = persona_sessions.lock() {
                            m.insert(SessionId(session_id), persona);
                        }
                    }
                    // Bind this session to the chosen agent identity (multi-agent
                    // runtime, slice 3b). The stamp + CCBS resolve it; unbound
                    // sessions fall back to the node default (APEX).
                    //
                    // Auth-gate (slice 3e): a session-token human may only bind an
                    // agent THEY own — a disallowed/blank request resolves to their
                    // own default agent, so a guest can never inherit APEX. The
                    // admin / token-less path is trusted and binds whatever it asks.
                    let requested = agent_id.as_str();
                    let sid = SessionId(session_id);
                    match &conn_auth {
                        Some(a) => {
                            let owned: Vec<String> = {
                                let ids = identities.read().await;
                                ids.agents_for(&a.user_id).iter().map(|ag| ag.id.clone()).collect()
                            };
                            let to_bind = session_auth::gate_agent_bind(a, requested, &owned);
                            if let Ok(mut m) = session_bindings.lock() {
                                match to_bind {
                                    Some(agent) => {
                                        m.insert(sid, agent);
                                        if let Ok(mut b) = bound_w.lock() {
                                            b.insert(sid);
                                        }
                                    }
                                    // Nothing the user may bind → clear any stale
                                    // binding so this session resolves to the default.
                                    None => {
                                        m.remove(&sid);
                                    }
                                }
                            }
                        }
                        None => {
                            if !requested.is_empty() {
                                if let Ok(mut m) = session_bindings.lock() {
                                    m.insert(sid, requested.to_string());
                                }
                                if let Ok(mut b) = bound_w.lock() {
                                    b.insert(sid);
                                }
                            }
                        }
                    }
                    let _ = prio_tx.send(make_session_init(session_id, &hist)).await;
                }
                ClientEvent::SetPersona { persona } => {
                    // G5 tier-2: a live persona switch — update this session's voice
                    // WITHOUT touching the session (no re-init), so the chat view isn't
                    // cleared the way a `hello` would. Empty persona clears it (→ default).
                    if let Ok(mut m) = persona_sessions.lock() {
                        if persona.is_empty() {
                            m.remove(&SessionId(session_id));
                        } else {
                            m.insert(SessionId(session_id), persona);
                        }
                    }
                }
                ClientEvent::UserPrompt { text, images } => {
                    let images = if images.is_empty() {
                        vec![]
                    } else {
                        prepare_user_images(&images).await
                    };
                    bus.emit(Event::UserPrompt {
                        session: SessionId(session_id),
                        text,
                        images,
                    })
                    .await;
                }
                ClientEvent::UserApproval {
                    action,
                    granted,
                    nonce,
                } => {
                    bus.emit(Event::UserApproval {
                        session: SessionId(session_id),
                        action,
                        granted,
                        nonce,
                    })
                    .await;
                }
                ClientEvent::UserCancel => {
                    bus.emit(Event::UserCancel {
                        session: SessionId(session_id),
                    })
                    .await;
                }
            }
        }
    });

    tokio::select! {
        _ = read  => {}
        _ = write => {}
    }

    // Socket closed: evict the agent bindings this socket established (slice 3e).
    // A later resume of one of these sessions must send `hello{agent_id}` again and
    // pass the gate — so a disconnected session can't be silently re-entered as a
    // stale identity. Sessions never reuse ids, so this only drops this socket's own.
    let bound = bound_sessions.lock().unwrap_or_else(|e| e.into_inner());
    if !bound.is_empty() {
        let mut binds = state.session_bindings.lock().unwrap_or_else(|e| e.into_inner());
        for sid in bound.iter() {
            binds.remove(sid);
        }
    }
}

// ── Sensor bridge WS ─────────────────────────────────────────────────────────

async fn sensor_bridge_ws_handler(
    ws:              WebSocketUpgrade,
    headers:         axum::http::HeaderMap,
    Query(params):   Query<HashMap<String, String>>,
    State(state):    State<GatewayState>,
) -> Response {
    let expected = state.sensor_bridge_token.as_str();
    if !expected.is_empty() {
        // Prefer the Authorization header (the token stays out of the URL → out of
        // logs); fall back to ?token= for a not-yet-updated sensor-bridge during a
        // rolling apexos-update.
        let from_header = headers.get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.strip_prefix("Bearer "))
            .unwrap_or("");
        let from_query = params.get("token").map(|s| s.as_str()).unwrap_or("");
        if !tokens_match(from_header, expected) && !tokens_match(from_query, expected) {
            return (StatusCode::UNAUTHORIZED, "invalid sensor bridge token").into_response();
        }
    }
    ws.on_upgrade(move |socket| handle_sensor_bridge(socket, state))
       .into_response()
}

/// `/mesh-bridge` — where `apexos-mesh-bridge` connects in (ApexNET P5c).
///
/// Same shape and same auth convention as `/sensor-bridge`: the bridge owns
/// the serial port and dials agentd, so agentd never holds a device open and
/// a node without radio hardware simply never sees a connection.
async fn mesh_bridge_ws_handler(
    ws:              WebSocketUpgrade,
    headers:         axum::http::HeaderMap,
    Query(params):   Query<HashMap<String, String>>,
    State(state):    State<GatewayState>,
) -> Response {
    let expected = state.mesh_bridge_token.as_str();
    if !expected.is_empty() {
        let from_header = headers.get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.strip_prefix("Bearer "))
            .unwrap_or("");
        let from_query = params.get("token").map(|s| s.as_str()).unwrap_or("");
        if from_header != expected && from_query != expected {
            return (StatusCode::UNAUTHORIZED, "invalid mesh bridge token").into_response();
        }
    }
    ws.on_upgrade(move |socket| handle_mesh_bridge(socket, state)).into_response()
}

async fn handle_mesh_bridge(socket: WebSocket, state: GatewayState) {
    use apexos_core::mesh_router::SeenCache;
    let (mut sink, mut stream) = socket.split();
    state.mesh_link.link_up();
    eprintln!("[mesh-bridge] bridge connected");

    // Outbound: whatever the router hands the lane goes to every connected
    // bridge. A lagging bridge drops frames rather than stalling the router —
    // gossip is lossy and the next heartbeat is seconds away.
    let mut outbound = state.mesh_link.subscribe();
    let tx_task = tokio::spawn(async move {
        loop {
            match outbound.recv().await {
                Ok(bytes) => {
                    if sink.send(Message::Binary(bytes.into())).await.is_err() {
                        break;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    eprintln!("[mesh-bridge] dropped {n} outbound frames (bridge lagging)");
                }
                Err(_) => break,
            }
        }
    });

    // Inbound dedup. The same heartbeat can arrive over two bridges once a
    // node has two radios, and fan-out makes duplicates the norm rather than
    // the exception (docs/apexnet.md §6.1).
    let mut seen = SeenCache::new(512);
    while let Some(Ok(msg)) = stream.next().await {
        let Message::Binary(bytes) = msg else { continue };
        let Ok(frame) = apexos_mesh_proto::decode_datagram(&bytes) else {
            state.mesh_link.note_decode_fail();
            continue;
        };
        state.mesh_link.note_rx();
        if accept_radio_payload(&state, &frame, &mut seen).await {
            continue;
        }
        if !seen.accept(frame.sender, frame.ctr) {
            state.mesh_link.note_duplicate();
            continue;
        }
        absorb_mesh_frame(&state, &frame);
    }

    tx_task.abort();
    state.mesh_link.link_down();
    eprintln!("[mesh-bridge] bridge disconnected");
}

/// Persist an inbound radio data payload and tell the brainstem it may ACK.
/// Returns true when the frame was radio-data (caller must not treat it as
/// status). A USB retry of the same `(sender, ctr)` re-sends the host accept
/// but does not re-deliver A2A.
async fn accept_radio_payload(
    state: &GatewayState,
    frame: &apexos_mesh_proto::MeshFrame,
    seen: &mut apexos_core::mesh_router::SeenCache,
) -> bool {
    use apexos_mesh_proto::{Payload, PlainPacket};
    let Ok((packet, _)) = postcard::take_from_bytes::<PlainPacket>(&frame.ct) else {
        return false;
    };
    let Some((kind, body)) = mesh_link::payload_kind_body(&packet.payload) else {
        return false;
    };
    if !mesh_link::radio_payload_needs_accept(&packet.payload) {
        return false;
    }
    let path = state.events_dir.join("radio_inbox.jsonl");
    match mesh_link::persist_radio_inbox(&path, frame.sender, frame.ctr, kind, &body) {
        Ok(news) => {
            if news {
                if let Payload::A2A { body: raw } = &packet.payload {
                    deliver_radio_a2a(state, frame.sender, raw).await;
                }
            }
            if !seen.accept(frame.sender, frame.ctr) {
                state.mesh_link.note_duplicate();
            }
            if !state
                .mesh_link
                .push_frame(&mesh_link::host_accept_frame(frame.sender, frame.ctr))
            {
                eprintln!("[mesh-bridge] host accept not sent — no bridge subscriber");
            }
            true
        }
        Err(e) => {
            eprintln!("[mesh-bridge] radio inbox persist failed: {e}");
            // Do not host-accept: the brainstem will retry USB and the
            // sender still holds the message.
            true
        }
    }
}

async fn deliver_radio_a2a(state: &GatewayState, sender: u16, raw: &[u8]) {
    let text = String::from_utf8_lossy(raw);
    if text.trim().is_empty() {
        return;
    }
    let from = format!("radio-{sender}");
    let session = mesh_session_for(state, &from);
    let prompt = a2a_prompt_text(Some(&from), None, &text);
    state
        .bus
        .emit(Event::UserPrompt {
            session,
            text: prompt,
            images: vec![],
        })
        .await;
    let preview: String = text.chars().take(140).collect();
    state
        .bus
        .emit(Event::MeshMessage {
            from_node: from.clone(),
            session,
            preview: preview.clone(),
        })
        .await;
    let snapshot = {
        let mut map = state.mesh_unread.lock().unwrap_or_else(|e| e.into_inner());
        mesh_unread_bump(&mut map, session.0, &from, &preview, now_epoch_secs());
        map.clone()
    };
    persist_mesh_unread(&state.mesh_unread_path, &snapshot);
}

/// Make sense of a frame that arrived from our own brainstem.
///
/// This link is **unsealed by design** (charter §5): it extends the cable
/// between a board and its Pi, so `ct` is a plain `postcard(PlainPacket)`.
/// Anything that arrived over the *air* was sealed and opened by the
/// brainstem before it got here — nothing in this function may be used to
/// justify trusting a radio payload.
fn absorb_mesh_frame(state: &GatewayState, frame: &apexos_mesh_proto::MeshFrame) {
    use apexos_mesh_proto::{Payload, PlainPacket};
    let Ok((packet, _)) = postcard::take_from_bytes::<PlainPacket>(&frame.ct) else {
        state.mesh_link.note_decode_fail();
        return;
    };
    if let Payload::BrainstemStatus { node_id, queued, neighbors, ctr_hw } = packet.payload {
        // The board is the thing with the antenna; this is the only view
        // agentd has of the air, and it is second-hand on purpose.
        state.mesh_link.set_brainstem(mesh_link::BrainstemView {
            node_id,
            neighbors,
            queued,
            counter_high_water: ctr_hw,
            seen: true,
        });
    }
}

async fn handle_sensor_bridge(socket: WebSocket, state: GatewayState) {
    let (_, mut stream) = socket.split();
    eprintln!("[sensor-bridge] node connected");
    while let Some(Ok(msg)) = stream.next().await {
        if let Message::Text(text) = msg {
            match SensorIngress::parse(&text) {
                Ok(ingress) => {
                    let event = ingress.into_event();
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
            "sw.js"             => "application/javascript; charset=utf-8",
            "manifest.json"     => "application/manifest+json; charset=utf-8",
            "icon.svg"          => "image/svg+xml; charset=utf-8",
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
    let key_set = !state.api_key.read().await.is_empty();
    let backend = state.backend.read().await.clone();
    let oai_key_set = state.oai_keys.read().await.backend_key_set(&backend);
    let model = state.model.read().await.clone();
    let policy_mode = state.policy_mode.read().await.clone();
    Json(serde_json::json!({
        "api_key_set":     key_set,
        "oai_key_set":     oai_key_set, // active backend's OAI-compat slot
        "model":           model,
        "policy_mode":     policy_mode,
    }))
}

/// Secret-file path for one OAI key-ring slot.
fn oai_slot_key_path(slot: &str) -> String {
    match slot {
        "openrouter" => std::env::var("AGENTD_OPENROUTER_KEY_FILE")
            .unwrap_or_else(|_| "/var/lib/agentd/.openrouter_api_key".into()),
        "xai" => std::env::var("AGENTD_XAI_KEY_FILE")
            .unwrap_or_else(|_| "/var/lib/agentd/.xai_api_key".into()),
        _ => std::env::var("AGENTD_OAI_KEY_FILE")
            .unwrap_or_else(|_| "/var/lib/agentd/.oai_api_key".into()),
    }
}

fn policy_toml_path() -> PathBuf {
    std::env::var("AGENTD_POLICY_TOML")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("config/policy.toml"))
}

/// Validate + atomically persist `mode` into policy.toml. RAM is not the commit.
fn persist_policy_mode(mode: PolicyMode) -> Result<(), String> {
    let path = policy_toml_path();
    let text = std::fs::read_to_string(&path)
        .map_err(|e| format!("read {}: {e}", path.display()))?;
    let (new_toml, _) = policy_toml_set_mode(&text, mode).map_err(|e| e.to_string())?;
    apexos_core::write_config_atomic(&path, new_toml.as_bytes())
        .map_err(|e| format!("persist {}: {e}", path.display()))?;
    Ok(())
}

async fn set_policy_handler(
    State(state): State<GatewayState>,
    Json(body): Json<serde_json::Value>,
) -> impl IntoResponse {
    let mode = body["mode"].as_str().unwrap_or("").trim().to_string();
    let Some(parsed) = PolicyMode::from_api(&mode) else {
        return Json(serde_json::json!({ "ok": false, "error": "unknown mode" }));
    };
    if let Err(e) = persist_policy_mode(parsed) {
        return Json(serde_json::json!({ "ok": false, "error": e }));
    }
    *state.policy_mode.write().await = mode.clone();
    state.policy_arc.write().await.config.mode = parsed;
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
    if let Err(e) = write_secret_file(&persist_path, &key) {
        // The key IS live in memory for this run; surface the persistence failure
        // so the caller knows it won't survive a restart (was silently swallowed).
        eprintln!("[gateway] persist api key to {persist_path} failed: {e}");
        return Json(serde_json::json!({
            "ok": false,
            "error": format!("key set in memory but not persisted: {e}")
        }));
    }

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
    let ring = state.oai_keys.read().await;
    let backend = state.backend.read().await.clone();
    Json(serde_json::json!({
        "anthropic_set": !state.api_key.read().await.is_empty(),
        // Back-compat: "oai_set" = active backend's OAI-compat slot is non-empty.
        "oai_set":       ring.backend_key_set(&backend),
        "keys_set": {
            "oai":        ring.slot_set("oai"),
            "openrouter": ring.slot_set("openrouter"),
            "xai":        ring.slot_set("xai"),
        },
    }))
}

async fn set_keys_handler(
    State(state): State<GatewayState>,
    Json(body): Json<serde_json::Value>,
) -> impl IntoResponse {
    // Each key is set live in memory regardless; collect any persistence failures
    // so a write error surfaces instead of returning a false ok:true.
    let mut errors: Vec<String> = Vec::new();
    if let Some(key) = body["anthropic"].as_str() {
        let key = key.trim().to_string();
        if !key.is_empty() {
            *state.api_key.write().await = key.clone();
            let path = std::env::var("AGENTD_KEY_FILE")
                .unwrap_or_else(|_| "/var/lib/agentd/.api_key".into());
            if let Err(e) = write_secret_file(&path, &key) {
                eprintln!("[gateway] persist anthropic key to {path} failed: {e}");
                errors.push(format!("anthropic: {e}"));
            }
        }
    }
    // Per-slot cloud keys — openrouter / xai / oai never overwrite each other.
    for slot in ["oai", "openrouter", "xai"] {
        if let Some(key) = body[slot].as_str() {
            let key = key.trim().to_string();
            if key.is_empty() {
                continue;
            }
            state.oai_keys.write().await.set_slot(slot, key.clone());
            let path = oai_slot_key_path(slot);
            if let Err(e) = write_secret_file(&path, &key) {
                eprintln!("[gateway] persist {slot} key to {path} failed: {e}");
                errors.push(format!("{slot}: {e}"));
            }
        }
    }
    if errors.is_empty() {
        Json(serde_json::json!({ "ok": true }))
    } else {
        Json(serde_json::json!({
            "ok": false,
            "error": format!("set in memory but not persisted — {}", errors.join("; "))
        }))
    }
}

async fn get_model_handler(State(state): State<GatewayState>) -> impl IntoResponse {
    let model = state.model.read().await.clone();
    Json(serde_json::json!({ "model": model }))
}

/// Parse Anthropic's `GET /v1/models` list shape (`data[].{id, display_name}`) into
/// the picker's `{id, name}` rows. Newest-first API order is kept (Fable/Sonnet 5
/// land on top). `display_name` falls back to the id.
fn parse_anthropic_models(v: &serde_json::Value) -> Vec<serde_json::Value> {
    v["data"]
        .as_array()
        .map(|data| {
            data.iter()
                .filter_map(|m| {
                    let id = m["id"].as_str()?;
                    let name = m["display_name"].as_str().unwrap_or(id);
                    Some(serde_json::json!({ "id": id, "name": name }))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Offline / key-less fallback for the Anthropic picker — the current family at the
/// time of writing. The live path below supersedes this whenever the API is reachable,
/// so a new model launch needs no code change to appear on deployed nodes.
fn anthropic_fallback_models() -> serde_json::Value {
    serde_json::json!([
        { "id": "claude-fable-5",    "name": "Fable 5"    },
        { "id": "claude-opus-4-8",   "name": "Opus 4.8"   },
        { "id": "claude-sonnet-5",   "name": "Sonnet 5"   },
        { "id": "claude-opus-4-7",   "name": "Opus 4.7"   },
        { "id": "claude-sonnet-4-6", "name": "Sonnet 4.6" },
        { "id": "claude-haiku-4-5",  "name": "Haiku 4.5"  },
    ])
}

/// Returns available models for the active backend.
/// For Anthropic: live discovery via `GET api.anthropic.com/v1/models` (auth-only —
/// no tokens billed; the same free call install.sh uses to verify keys), so the
/// picker reflects exactly what this key can see: new launches AND any legacy-model
/// access privileges the org holds. Static current-family fallback when offline or
/// key-less. For OAI backends: proxies to {base_url}/models.
async fn get_models_handler(State(state): State<GatewayState>) -> impl IntoResponse {
    let current     = state.model.read().await.clone();
    let backend     = state.backend.read().await.clone();
    let oai_base    = state.oai_base_url.read().await.clone();

    // Shared across calls so repeated /api/models probes don't each rebuild a TLS
    // client (function-local — only this handler needs it).
    static MODELS_CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    let client = MODELS_CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(4))
            .build()
            .unwrap_or_default()
    });

    if backend == "anthropic" {
        let api_key = state.api_key.read().await.clone();
        if !api_key.is_empty() {
            // limit=1000 fetches the whole catalog in one page (the list is dozens
            // of entries at most; the default page size of 20 could clip legacy ids).
            let resp = client
                .get("https://api.anthropic.com/v1/models?limit=1000")
                .header("x-api-key", api_key)
                .header("anthropic-version", "2023-06-01")
                .send()
                .await;
            if let Ok(resp) = resp {
                if resp.status().is_success() {
                    if let Ok(body) = resp.json::<serde_json::Value>().await {
                        let models = parse_anthropic_models(&body);
                        if !models.is_empty() {
                            return Json(serde_json::json!({
                                "backend": backend,
                                "current": current,
                                "models": models,
                            }));
                        }
                    }
                }
            }
        }
        return Json(serde_json::json!({
            "backend": backend,
            "current": current,
            "models": anthropic_fallback_models(),
        }));
    }

    // OAI-compatible backend: query {base_url}/models for live model list
    let models_url = format!("{}/models", oai_base.trim_end_matches('/'));
    let api_key = {
        let ring = state.oai_keys.read().await;
        ring.for_backend(&backend).to_owned()
    };

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

/// GET /api/thermal/frame — proxy the SensorHead dashboard's raw 32×24 thermal grid
/// (`/api/thermal/data` → `{"frame":[768 floats °C], ...}`) so the UI can render a
/// heatmap. The sensor_reading WS events carry only min/max/mean, not the full grid,
/// so the UI fetches this on demand (only while the Sensors view is open). SensorHead
/// reads the MLX90640 over I2C; we just relay its JSON. Graceful 503 + empty frame
/// when there's no SensorHead (non-sensor node, or dashboard down).
async fn thermal_frame_handler() -> impl IntoResponse {
    let base = std::env::var("SENSORHEAD_URL").unwrap_or_else(|_| "http://localhost:8080".into());
    let url  = format!("{}/api/thermal/data", base.trim_end_matches('/'));
    static THERMAL_CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    let client = THERMAL_CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(4))
            .build()
            .unwrap_or_default()
    });
    match client.get(&url).send().await {
        Ok(resp) if resp.status().is_success() => match resp.json::<serde_json::Value>().await {
            Ok(body) => Json(body).into_response(),
            Err(_)   => (StatusCode::BAD_GATEWAY,
                Json(serde_json::json!({ "error": "bad thermal payload", "frame": [] }))).into_response(),
        },
        _ => (StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({ "error": "no thermal sensor", "frame": [] }))).into_response(),
    }
}

async fn get_backend_handler(State(state): State<GatewayState>) -> impl IntoResponse {
    let backend = state.backend.read().await.clone();
    let ring = state.oai_keys.read().await;
    Json(serde_json::json!({
        "backend":      backend.clone(),
        "oai_base_url": state.oai_base_url.read().await.clone(),
        "model":        state.model.read().await.clone(),
        "backends":     backend_config::KNOWN_BACKENDS,
        "anthropic_key_set": !state.api_key.read().await.is_empty(),
        // Active backend's OAI-compat slot (Settings "key set" indicator).
        "oai_key_set":       ring.backend_key_set(&backend),
        "keys_set": {
            "oai":        ring.slot_set("oai"),
            "openrouter": ring.slot_set("openrouter"),
            "xai":        ring.slot_set("xai"),
        },
        "key_slot": apexos_agent::OaiKeyRing::slot_for_backend(&backend),
    }))
}

/// Substrate-change notice (model-welfare H3): a backend/model hot-swap makes the
/// agent differently capable mid-life — without a notice it has no idea why its own
/// competence shifted, and its memories of the period carry false context. Injects
/// a root-session note (mirrors the USB-plug greeting / mesh beacon notify).
/// Default ON; `AGENTD_SWAP_NOTIFY_AGENT=0/false/off` silences it.
pub async fn notify_substrate_change(bus: &BusHandle, backend: &str, model: &str, reason: &str) {
    let notify = std::env::var("AGENTD_SWAP_NOTIFY_AGENT")
        .map(|v| { let v = v.to_lowercase(); v != "0" && v != "false" && v != "off" })
        .unwrap_or(true);
    if !notify {
        return;
    }
    let text = format!(
        "[substrate notice — {reason}] Your inference substrate just changed: backend `{backend}`, \
         model `{model}`. You are the same agent — soul, memory, and tools are unchanged — but \
         capability, style, and speed may differ from the substrate you were just running on. \
         If it matters to ongoing work, note the change in memory; otherwise a brief acknowledgement is enough."
    );
    bus.emit(Event::UserPrompt { session: SessionId(0), text, images: vec![] }).await;
}

/// Snapshot the three arcs and persist as the operator's steady-state choice
/// (file-wins-on-restart, the voice-config pattern). Called from the operator
/// handlers only — the vast hot-swap writes the arcs directly and deliberately
/// does NOT persist (a rented GPU is transient, never a boot default).
async fn persist_backend_snapshot(state: &GatewayState) {
    let cfg = backend_config::BackendConfig {
        backend:      state.backend.read().await.clone(),
        model:        state.model.read().await.clone(),
        oai_base_url: state.oai_base_url.read().await.clone(),
    };
    backend_config::persist(&cfg);
}

async fn set_backend_handler(
    State(state): State<GatewayState>,
    Json(body): Json<serde_json::Value>,
) -> impl IntoResponse {
    let backend = body["backend"].as_str().unwrap_or("").trim().to_lowercase();
    if backend.is_empty() {
        return Json(serde_json::json!({ "ok": false, "error": "missing backend" })).into_response();
    }
    // Reject unknowns: RoutingProvider's `_ => anthropic` arm would otherwise turn a
    // typo into a silent wrong-backend node.
    if !backend_config::backend_valid(&backend) {
        return Json(serde_json::json!({
            "ok": false,
            "error": format!("unknown backend '{backend}' (known: {})",
                backend_config::KNOWN_BACKENDS.join(", ")),
        })).into_response();
    }

    let explicit_url = body["oai_base_url"]
        .as_str()
        .map(|u| u.trim().to_string())
        .filter(|u| !u.is_empty());
    // Named cloud OAI backends have exactly one canonical endpoint — switching
    // without an explicit URL must not leave the arc at ollama's localhost default.
    let url = match (&explicit_url, backend.as_str()) {
        (Some(u), _) => Some(u.clone()),
        (None, "openrouter" | "xai") => {
            Some(backend_config::default_url_for(&backend).to_string())
        }
        (None, _) => None,
    };

    *state.backend.write().await = backend.clone();
    if let Some(url) = url {
        *state.oai_base_url.write().await = url;
    }

    // Explicit model in the body always wins; otherwise, if the live model is empty
    // or clearly the wrong family for this backend (e.g. claude-* against xai), pin
    // the per-backend default so the next turn isn't stranded on a foreign model id.
    if let Some(model) = body["model"].as_str() {
        let model = model.trim().to_string();
        if !model.is_empty() {
            *state.model.write().await = model;
        }
    } else {
        let current = state.model.read().await.clone();
        if backend_config::model_family_mismatch(&backend, &current) {
            *state.model.write().await = backend_config::default_model_for(&backend).to_string();
        }
    }

    persist_backend_snapshot(&state).await;
    notify_substrate_change(
        &state.bus,
        &state.backend.read().await.clone(),
        &state.model.read().await.clone(),
        "operator backend switch",
    )
    .await;
    get_backend_handler(State(state)).await.into_response()
}

/// GET /api/compute/discover — operator-triggered LAN sweep for OpenAI-compatible
/// inference endpoints (Settings → SCAN NETWORK). Localhost + the local /24 + mesh
/// peers, verified by the /v1/models shape. Takes a few seconds on a quiet LAN.
async fn compute_discover_handler(State(state): State<GatewayState>) -> impl IntoResponse {
    let peer_urls: Vec<String> = state
        .peer_registry
        .read()
        .await
        .peers
        .iter()
        .map(|p| p.ws_url.clone())
        .collect();
    let endpoints = compute::discover(compute::peer_hosts(&peer_urls)).await;
    Json(serde_json::json!({ "endpoints": endpoints }))
}

async fn set_model_handler(
    State(state): State<GatewayState>,
    Json(body): Json<serde_json::Value>,
) -> impl IntoResponse {
    let model = body["model"].as_str().unwrap_or("").trim().to_string();
    if model.is_empty() {
        return Json(serde_json::json!({ "ok": false, "error": "empty model" }));
    }
    *state.model.write().await = model.clone();
    persist_backend_snapshot(&state).await;
    notify_substrate_change(
        &state.bus,
        &state.backend.read().await.clone(),
        &model,
        "operator model switch",
    )
    .await;
    Json(serde_json::json!({ "ok": true }))
}

/// Current prompt-cache policy (Anthropic). `ttl` is "5m" | "1h".
async fn get_cache_handler(State(state): State<GatewayState>) -> impl IntoResponse {
    let c = state.cache.read().await;
    Json(serde_json::json!({
        "enabled":            c.enabled,
        "cache_conversation": c.cache_conversation,
        "ttl":                c.ttl.label(),
        "summary":            c.summary(),
    }))
}

/// Live-tune the prompt-cache policy. Any subset of `enabled` / `cache_conversation`
/// (bools) and `ttl` ("5m"|"1h") may be present; absent fields keep their value. Takes
/// effect on the very next turn — the engine reads this arc per request.
async fn set_cache_handler(
    State(state): State<GatewayState>,
    Json(body): Json<serde_json::Value>,
) -> impl IntoResponse {
    let mut c = state.cache.write().await;
    if let Some(b) = body["enabled"].as_bool() {
        c.enabled = b;
    }
    if let Some(b) = body["cache_conversation"].as_bool() {
        c.cache_conversation = b;
    }
    if let Some(t) = body["ttl"].as_str() {
        c.ttl = match t.trim().to_ascii_lowercase().as_str() {
            "1h" | "1hr" | "hour" | "3600" => apexos_agent::CacheTtl::OneHour,
            _ => apexos_agent::CacheTtl::FiveMin,
        };
    }
    Json(serde_json::json!({
        "ok":                 true,
        "enabled":            c.enabled,
        "cache_conversation": c.cache_conversation,
        "ttl":                c.ttl.label(),
        "summary":            c.summary(),
    }))
}

/// GET /api/history — the live window budget plus per-session "window in use"
/// estimates (top 5 by size), so trim behavior stops being invisible. Bands are
/// the trim's own math: fires past `trim_trigger`, cuts to `trim_target`.
async fn get_history_handler(
    State(state): State<GatewayState>,
    Extension(auth): Extension<RequestAuth>,
) -> impl IntoResponse {
    let budget = state.history_budget.load(Ordering::Relaxed);
    let mut sessions: Vec<(u64, usize)> = {
        let lock = state.histories.lock().await;
        lock.iter()
            .filter(|(sid, _)| session_ok(&state.sessions_dir, sid.0, &auth))
            .map(|(sid, h)| (sid.0, apexos_core::history::estimate_history(h)))
            .collect()
    };
    sessions.sort_by_key(|&(_, est)| std::cmp::Reverse(est));
    sessions.truncate(5);
    Json(serde_json::json!({
        "budget":       budget,
        "trim_trigger": apexos_core::history::trim_trigger(budget),
        "trim_target":  apexos_core::history::trim_target(budget),
        "sessions":     sessions.iter().map(|(s, est)| serde_json::json!({
            "session_id": s, "est_tokens": est,
        })).collect::<Vec<_>>(),
    }))
}

/// Live-tune + persist the history window budget. `{budget: n}` — 0 disables
/// trimming, tiny values floor at 10k (`sanitize_budget`). Effective on the very
/// next turn (the router reads the atomic per prompt); persisted to
/// history_config.json — env stays the seed, delete the file to return to env.
async fn set_history_handler(
    State(state): State<GatewayState>,
    Json(body): Json<serde_json::Value>,
) -> impl IntoResponse {
    let Some(raw) = body["budget"].as_u64() else {
        return Json(serde_json::json!({ "ok": false, "error": "budget (number) required" }));
    };
    let budget = history_config::sanitize_budget(raw);
    state.history_budget.store(budget, Ordering::Relaxed);
    let persisted = history_config::persist(&history_config::HistoryConfig { budget });
    Json(serde_json::json!({ "ok": true, "budget": budget, "persisted": persisted }))
}

/// Approximate Anthropic input/output price in $ per million tokens, by model family.
/// Pricing drifts — this is a labelled estimate for the tokenomics readout, not billing.
fn anthropic_pricing(model: &str) -> (f64, f64) {
    let m = model.to_ascii_lowercase();
    if m.contains("haiku") { (1.0, 5.0) }
    else if m.contains("sonnet") { (3.0, 15.0) }
    else if m.contains("fable") || m.contains("mythos") { (10.0, 50.0) }
    else { (5.0, 25.0) } // opus-tier default
}

/// Cumulative token + cache accounting since daemon boot, plus the "cache bank"
/// economics: what caching has saved vs re-sending every prefix at full price. The
/// $ figures are an estimate at the *current* model's price (usage may span models).
async fn get_usage_handler(State(state): State<GatewayState>) -> impl IntoResponse {
    let u = apexos_agent::usage::snapshot();
    let model = state.model.read().await.clone();
    let (in_price, out_price) = anthropic_pricing(&model);
    let m = 1_000_000.0_f64;

    // Anthropic input billing tiers: full × input, 1.25× × cache-creation, 0.1× × cache-read.
    let spent = (u.input_tokens as f64
        + u.cache_creation_tokens as f64 * 1.25
        + u.cache_read_tokens as f64 * 0.10) / m * in_price
        + u.output_tokens as f64 / m * out_price;
    // Baseline if caching were off: every input token (incl. what's now cached) at full price.
    let uncached = u.total_input() as f64 / m * in_price + u.output_tokens as f64 / m * out_price;
    let saved = uncached - spent;
    // The "cache bank": net input-token-equivalents kept off the bill (reads at 0.9× discount
    // minus the 0.25× write premium). The headline number for the cache-banking insight.
    let banked_tokens = (u.cache_read_tokens as f64 * 0.90) - (u.cache_creation_tokens as f64 * 0.25);

    Json(serde_json::json!({
        "turns": u.turns,
        "tokens": {
            "input":          u.input_tokens,
            "cache_read":     u.cache_read_tokens,
            "cache_creation": u.cache_creation_tokens,
            "output":         u.output_tokens,
            "total_input":    u.total_input(),
        },
        "cache_hit_rate": u.cache_hit_rate(),
        "banked_tokens":  banked_tokens.round() as i64,
        "model": model,
        "pricing": { "input_per_mtok": in_price, "output_per_mtok": out_price, "note": "approximate, current model" },
        "cost_usd": { "spent": spent, "uncached_baseline": uncached, "saved": saved },
    }))
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
async fn active_sessions_handler(
    State(state): State<GatewayState>,
    Extension(auth): Extension<RequestAuth>,
) -> impl IntoResponse {
    let histories = state.histories.lock().await;
    let mut sessions: Vec<serde_json::Value> = histories.iter()
        .filter(|(sid, _)| session_ok(&state.sessions_dir, sid.0, &auth))
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

/// Resolve (allocate-once) the session that holds `peer`'s a2a thread on this node.
/// Maps `peer node_id → SessionId` so every message from a given peer lands in the
/// same session — its own thread, kept out of root session 0 and the user's active
/// chat. The id is drawn from the shared `next_session_id` atomic (so it can never
/// collide with a socket-allocated session), recorded in `mesh_sessions`, and the
/// map is persisted best-effort. A restart reloads the map (and bumps the counter
/// past any loaded id in `main.rs`), so the thread is continuous across restarts.
/// Pure allocate-or-lookup: returns `peer`'s existing session, or a freshly
/// allocated one drawn from `counter`. The bool is `true` only when a NEW id was
/// allocated (the caller persists then). Ids come from the SAME atomic the gateway
/// uses for socket sessions, so a mesh session can never collide with a socket one.
fn mesh_session_alloc(
    map: &mut HashMap<String, SessionId>,
    counter: &AtomicU64,
    peer: &str,
) -> (SessionId, bool) {
    if let Some(s) = map.get(peer) {
        return (*s, false);
    }
    let sid = SessionId(counter.fetch_add(1, Ordering::SeqCst));
    map.insert(peer.to_string(), sid);
    (sid, true)
}

fn mesh_session_for(state: &GatewayState, peer: &str) -> SessionId {
    let (sid, snapshot) = {
        let mut map = state.mesh_sessions.lock().unwrap_or_else(|e| e.into_inner());
        let (sid, fresh) = mesh_session_alloc(&mut map, &state.next_session_id, peer);
        if !fresh {
            return sid;
        }
        (sid, map.clone())
    };
    // Persist outside the lock (small map, infrequent — only on a peer's first message).
    if let Ok(json) = serde_json::to_string_pretty(&snapshot) {
        if let Err(e) = std::fs::write(&state.mesh_sessions_path, json) {
            eprintln!("[mesh] could not persist mesh_sessions: {e}");
        }
    }
    sid
}

// ── Mesh inbox unread (cross-restart persistence) ───────────────────────────────
// Per-peer-thread unread counts that survive a daemon/UI restart. The UI's inbox
// is event-driven (the `mesh_message` stream) but its counts were UI-session-scoped
// (lost on restart). This is the durable side: agentd increments a per-session
// counter on each inbound a2a, persists it to `<log_dir>/mesh_unread.json`, serves
// it at `GET /api/mesh/inbox` (the UI seeds from this on launch) and zeroes it at
// `POST /api/mesh/inbox/read`. Keyed by the peer's thread SessionId — the same join
// key the UI's inbox + `mesh_sessions` already use.

/// One peer thread's unread state (carries the node_id + last preview/time so the
/// UI can rebuild a full inbox row from a cold start, not just a bare count).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct MeshUnread {
    pub node_id: String,
    pub unread:  u32,
    pub preview: String,
    pub last_ts: i64, // epoch seconds
}

/// Session id → unread state. `Arc<std::sync::Mutex>` (not tokio) — the critical
/// sections are tiny map ops, never held across an await.
pub type MeshInbox = Arc<std::sync::Mutex<HashMap<u64, MeshUnread>>>;

/// Bump a peer thread's unread on an inbound a2a message (pure; caller persists).
fn mesh_unread_bump(map: &mut HashMap<u64, MeshUnread>, session: u64, node_id: &str, preview: &str, now: i64) {
    let e = map.entry(session).or_default();
    e.node_id = node_id.to_string();
    e.unread  = e.unread.saturating_add(1);
    e.preview = preview.to_string();
    e.last_ts = now;
}

/// Zero a peer thread's unread (the user opened it). Returns true if it changed.
fn mesh_unread_clear(map: &mut HashMap<u64, MeshUnread>, session: u64) -> bool {
    match map.get_mut(&session) {
        Some(e) if e.unread != 0 => { e.unread = 0; true }
        _ => false,
    }
}

fn persist_mesh_unread(path: &std::path::Path, map: &HashMap<u64, MeshUnread>) {
    if let Ok(json) = serde_json::to_string_pretty(map) {
        if let Err(e) = std::fs::write(path, json) {
            eprintln!("[mesh] could not persist mesh_unread: {e}");
        }
    }
}

// ── Federation observability counters (colony-federation principle 6, v1) ─────
// Per-peer counters for knowledge flowing INTO this node — the receiving edge.
// v1 is deliberately receiver-side only: every node counting its inbound makes
// colony-wide flow fully visible with zero cross-crate wiring (a peer's "sent"
// is this node's "received"); sender-side attribution (who initiated: manual
// send vs dream digest vs procedure) is the follow-up if the colony wants it.
// Same shape as MeshInbox: std Mutex (tiny ops, never held across await),
// persisted to `<log_dir>/mesh_fed_stats.json`, folded into GET /api/mesh/peers.

/// One peer's inbound-federation counters on this node.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct PeerFedStats {
    pub memories_received: u64, // imports stored (manual sends · dream digests · procedures)
    pub duplicates:        u64, // re-sends answered `duplicate:true` (origin dedup)
    pub recall_served:     u64, // federated recall queries answered for this peer
    pub recall_hits:       u64, // total hits returned across those queries
    pub last_ts:           i64, // epoch secs of this peer's last federation touch
}

/// Peer node_id → inbound counters.
pub type FedStats = Arc<std::sync::Mutex<HashMap<String, PeerFedStats>>>;

/// Bump one peer's counters (pure; caller persists the returned snapshot).
fn fed_stats_bump(
    map: &mut HashMap<String, PeerFedStats>,
    from: &str,
    now: i64,
    f: impl FnOnce(&mut PeerFedStats),
) {
    let e = map.entry(from.to_string()).or_default();
    f(e);
    e.last_ts = now;
}

fn persist_fed_stats(path: &std::path::Path, map: &HashMap<String, PeerFedStats>) {
    if let Ok(json) = serde_json::to_string_pretty(map) {
        if let Err(e) = std::fs::write(path, json) {
            eprintln!("[mesh] could not persist fed_stats: {e}");
        }
    }
}

/// Lock → bump → snapshot → persist, in one call (the handlers' one-liner).
fn fed_stats_record(state: &GatewayState, from: &str, f: impl FnOnce(&mut PeerFedStats)) {
    let snapshot = {
        let mut map = state.fed_stats.lock().unwrap_or_else(|e| e.into_inner());
        fed_stats_bump(&mut map, from, now_epoch_secs(), f);
        map.clone()
    };
    persist_fed_stats(&state.fed_stats_path, &snapshot);
}

fn now_epoch_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// GET /api/mesh/inbox — persisted per-peer unread threads, newest first. The UI
/// seeds its inbox model from this on launch so unread survives a restart.
async fn mesh_inbox_handler(State(state): State<GatewayState>) -> impl IntoResponse {
    let map = state.mesh_unread.lock().unwrap_or_else(|e| e.into_inner());
    let mut rows: Vec<MeshUnread> = Vec::with_capacity(map.len());
    let mut sessions: Vec<u64> = Vec::with_capacity(map.len());
    for (sid, e) in map.iter() { sessions.push(*sid); rows.push(e.clone()); }
    drop(map);
    let mut threads: Vec<serde_json::Value> = sessions.into_iter().zip(rows).map(|(sid, e)| {
        serde_json::json!({
            "session": sid, "node_id": e.node_id,
            "unread": e.unread, "preview": e.preview, "last_ts": e.last_ts,
        })
    }).collect();
    threads.sort_by(|a, b| b["last_ts"].as_i64().cmp(&a["last_ts"].as_i64()));
    Json(serde_json::json!({ "threads": threads }))
}

#[derive(Deserialize)]
struct InboxReadBody { session: u64 }

/// POST /api/mesh/inbox/read — zero a peer thread's unread (the user opened it).
async fn mesh_inbox_read_handler(
    State(state): State<GatewayState>,
    Json(body):   Json<InboxReadBody>,
) -> impl IntoResponse {
    let snapshot = {
        let mut map = state.mesh_unread.lock().unwrap_or_else(|e| e.into_inner());
        mesh_unread_clear(&mut map, body.session);
        map.clone()
    };
    persist_mesh_unread(&state.mesh_unread_path, &snapshot);
    Json(serde_json::json!({ "ok": true }))
}

/// Compose the prompt text injected for POST /api/sessions/:id/message. A message
/// from a registered peer carries `[from <node>]:` provenance (mirrors local a2a's
/// `[Agent N]:`). When the sender also stamped its asking session
/// (`origin_session`, system-stamped by its supervisor), the prefix carries the
/// ready-made reply route — the receiving agent continues the conversation by
/// copying that call verbatim, and the answer lands in the session that asked
/// instead of vanishing into the asker's per-peer mesh thread. Pure — unit-tested.
fn a2a_prompt_text(from: Option<&str>, origin_session: Option<u64>, message: &str) -> String {
    match (from, origin_session) {
        (Some(peer), Some(o)) => format!(
            "[from {peer} — to reply: send_to_agent(node=\"{peer}\", session_id={o})]: {message}"
        ),
        (Some(peer), None) => format!("[from {peer}]: {message}"),
        (None, _)          => message.to_string(),
    }
}

/// POST /api/sessions/:id/message — inject a message into an agent session from
/// external code (scripts, other services, the desktop UI) or a mesh peer (a2a).
/// Emits UserPrompt on the bus so the target session starts a new turn.
///
/// Routing: a body `from` field (the sending peer's node_id, stamped by the
/// cross-node `send_to_agent` sender) carries provenance. When `from` names a
/// **registered** peer AND no explicit target session was given (`:id` == 0, the
/// a2a default), the message is routed to that peer's own thread via
/// [`mesh_session_for`] and a global `MeshMessage` notification is broadcast to
/// every client — so a user watching any session sees the mesh traffic arrive.
/// An explicit non-zero `:id` is always honored (this is how a peer's reply
/// reaches the session that asked — see [`a2a_prompt_text`]); a missing/unknown
/// `from` falls back to `:id` (session 0) — byte-identical to the
/// pre-mesh-routing behaviour for generic external injectors (scripts, the
/// desktop UI).
async fn session_message_handler(
    State(state): State<GatewayState>,
    mesh_peer: Option<Extension<mesh::MeshPeerAuth>>,
    auth: Option<Extension<RequestAuth>>,
    Path(id):     Path<u64>,
    Json(body):   Json<serde_json::Value>,
) -> impl IntoResponse {
    // Mesh peers may inject a2a (session 0 or an explicit reply id). Human
    // callers still cannot write into someone else's session.
    if mesh_peer.is_none() {
        let Some(Extension(auth)) = auth else {
            return Json(serde_json::json!({ "ok": false, "error": "not your session" }));
        };
        if !session_ok(&state.sessions_dir, id, &auth) {
            return Json(serde_json::json!({ "ok": false, "error": "not your session" }));
        }
    }
    let message = match body["message"].as_str() {
        Some(s) if !s.trim().is_empty() => s.to_string(),
        _ => return Json(serde_json::json!({ "ok": false, "error": "missing message" })),
    };
    // Provenance: a mesh-token caller is the registered peer that minted the
    // inbound token — body.from is ignored so they cannot impersonate another
    // peer. Admin/session callers still honour a registered `from` (UI / tools).
    let from = if let Some(Extension(auth)) = mesh_peer {
        Some(auth.node_id)
    } else {
        match body["from"].as_str().map(str::trim).filter(|s| !s.is_empty()) {
            Some(f) if state.peer_registry.read().await.contains(f) => Some(f.to_string()),
            _ => None,
        }
    };

    // Decide the landing session.
    let session = match (&from, id) {
        // Mesh a2a with no explicit target → the peer's own thread.
        (Some(peer), 0) => mesh_session_for(&state, peer),
        // Explicit target, or a generic external POST without a known peer.
        _ => SessionId(id),
    };

    // Bake the peer into the prompt so the agent (and the replayed thread) sees
    // who is speaking; a sender-stamped origin session adds the reply route
    // (only meaningful from a registered peer — ignored otherwise).
    let origin = body["origin_session"].as_u64().filter(|o| *o != 0);
    let text = a2a_prompt_text(from.as_deref(), origin, &message);
    state.bus.emit(Event::UserPrompt { session, text, images: vec![] }).await;

    // Global notification so it surfaces regardless of the user's active session.
    if let Some(peer) = from {
        let preview: String = message.chars().take(140).collect();
        state.bus.emit(Event::MeshMessage { from_node: peer.clone(), session, preview: preview.clone() }).await;
        // Durable unread (survives a restart): bump + persist this peer's thread.
        let snapshot = {
            let mut map = state.mesh_unread.lock().unwrap_or_else(|e| e.into_inner());
            mesh_unread_bump(&mut map, session.0, &peer, &preview, now_epoch_secs());
            map.clone()
        };
        persist_mesh_unread(&state.mesh_unread_path, &snapshot);
    }

    Json(serde_json::json!({ "ok": true, "session_id": session.0 }))
}

fn workspace_dir() -> Result<std::path::PathBuf, String> {
    let ws = std::env::var("AGENTD_WORKSPACE")
        .ok().filter(|s| !s.is_empty())
        .unwrap_or_else(|| "/var/lib/agentd/workspace".to_string());
    let _ = std::fs::create_dir_all(&ws);
    std::fs::canonicalize(&ws).map_err(|e| format!("workspace {ws}: {e}"))
}

fn workspace_beneath() -> Result<apexos_confine::Beneath, String> {
    let ws = workspace_dir()?;
    apexos_confine::Beneath::open(&ws).map_err(|e| format!("workspace {}: {e}", ws.display()))
}

/// Relative path under the workspace, **without** resolving the target.
/// `..` and absolute escapes are refused. IO must use [`workspace_beneath`].
fn workspace_rel(path: &str) -> Result<std::path::PathBuf, String> {
    if std::path::Path::new(path)
        .components()
        .any(|c| c == std::path::Component::ParentDir)
    {
        return Err("path traversal (..) is not allowed".to_string());
    }
    let ws = workspace_dir()?;
    let p = std::path::Path::new(path);
    let joined = if p.is_absolute() {
        p.to_path_buf()
    } else {
        ws.join(p)
    };
    let rel = apexos_confine::relative_under(&ws, &joined)
        .ok_or_else(|| format!("path {} escapes workspace", joined.display()))?;
    Ok(if rel.as_os_str().is_empty() {
        std::path::PathBuf::from(".")
    } else {
        rel
    })
}

/// Display path (workspace + rel). For ffmpeg/argv that still need a pathname.
/// Explorer IO uses the root fd, not this.
fn resolve_workspace_path(path: &str) -> Result<std::path::PathBuf, String> {
    let rel = workspace_rel(path)?;
    let ws = workspace_dir()?;
    Ok(if rel == std::path::Path::new(".") {
        ws
    } else {
        ws.join(rel)
    })
}

/// Write target: parent must exist (checked via the root fd) and stay under
/// the workspace; the final component is appended un-resolved.
fn resolve_workspace_write_path(path: &str) -> Result<std::path::PathBuf, String> {
    let p = std::path::Path::new(path);
    if p.components().any(|c| c == std::path::Component::ParentDir) {
        return Err("path traversal (..) is not allowed".to_string());
    }
    let name = p.file_name().ok_or_else(|| format!("no file name in {path}"))?;
    if name == "." || name == ".." {
        return Err("path traversal (..) is not allowed".to_string());
    }
    let parent = p.parent().filter(|d| !d.as_os_str().is_empty());
    let parent_str = parent
        .map(|d| d.to_string_lossy().into_owned())
        .unwrap_or_else(|| ".".to_string());
    let parent_rel = workspace_rel(&parent_str)?;
    let root = workspace_beneath()?;
    if parent_rel != std::path::Path::new(".") {
        root.stat(&parent_rel)
            .map_err(|e| format!("{}: {e}", parent_rel.display()))?;
    }
    Ok(if parent_rel == std::path::Path::new(".") {
        workspace_dir()?.join(name)
    } else {
        workspace_dir()?.join(parent_rel).join(name)
    })
}

fn workspace_write_rel(path: &str) -> Result<(apexos_confine::Beneath, std::path::PathBuf), String> {
    let p = std::path::Path::new(path);
    if p.components().any(|c| c == std::path::Component::ParentDir) {
        return Err("path traversal (..) is not allowed".to_string());
    }
    let name = p.file_name().ok_or_else(|| format!("no file name in {path}"))?;
    if name == "." || name == ".." {
        return Err("path traversal (..) is not allowed".to_string());
    }
    let parent = p.parent().filter(|d| !d.as_os_str().is_empty());
    let parent_str = parent
        .map(|d| d.to_string_lossy().into_owned())
        .unwrap_or_else(|| ".".to_string());
    let parent_rel = workspace_rel(&parent_str)?;
    let root = workspace_beneath()?;
    if parent_rel != std::path::Path::new(".") {
        root.stat(&parent_rel)
            .map_err(|e| format!("{}: {e}", parent_rel.display()))?;
    }
    let rel = if parent_rel == std::path::Path::new(".") {
        std::path::PathBuf::from(name)
    } else {
        parent_rel.join(name)
    };
    Ok((root, rel))
}

/// A single safe path component for a rename/new-folder name: non-empty, not a
/// traversal token, no separator. The agent FS tools confine the same way
/// (`apexos-confine`); this is the gateway-side gate for the Explorer's write ops.
fn safe_component(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && !name.contains('/')
        && !name.contains('\0')
}

/// Recursively copy `src` → `dst` through a [`apexos_confine::Beneath`] root.
/// Symlinks are refused (not followed) — a planted link cannot pull bytes
/// from outside the workspace.
fn copy_beneath(
    root: &apexos_confine::Beneath,
    src: &std::path::Path,
    dst: &std::path::Path,
) -> std::io::Result<()> {
    let meta = root.stat(src)?;
    if meta.is_symlink {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "refusing to copy a symlink",
        ));
    }
    if meta.is_dir {
        match root.mkdir(dst) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e),
        }
        for entry in root.read_dir(src)? {
            copy_beneath(root, &src.join(&entry.name), &dst.join(&entry.name))?;
        }
        Ok(())
    } else {
        let bytes = root.read(src)?;
        root.write(dst, &bytes, false)
    }
}

/// Resolve a move/copy to (root, src_rel, dest_rel). Both ends stay under
/// the same workspace fd.
fn resolve_move_target(
    src: &str,
    dest: &str,
) -> Result<(apexos_confine::Beneath, std::path::PathBuf, std::path::PathBuf), String> {
    if src.trim().is_empty() {
        return Err("no source".to_string());
    }
    let root = workspace_beneath()?;
    let src_rel = workspace_rel(src)?;
    if src_rel == std::path::Path::new(".") {
        return Err("cannot move the workspace root".to_string());
    }
    let dest_rel = workspace_rel(dest)?;
    let dest_st = root
        .stat(&dest_rel)
        .map_err(|e| format!("destination: {e}"))?;
    if !dest_st.is_dir {
        return Err("destination is not a directory".to_string());
    }
    let name = src_rel
        .file_name()
        .ok_or_else(|| "source has no name".to_string())?;
    if dest_rel == src_rel || dest_rel.starts_with(&src_rel) {
        return Err("cannot move a folder into itself".to_string());
    }
    let target = if dest_rel == std::path::Path::new(".") {
        std::path::PathBuf::from(name)
    } else {
        dest_rel.join(name)
    };
    if root.exists(&target) {
        return Err("a file or folder with that name already exists here".to_string());
    }
    Ok((root, src_rel, target))
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
            match (workspace_beneath(), workspace_rel(p)) {
                (Ok(root), Ok(rel)) => match root.read(&rel) {
                    Ok(bytes) => tokio::task::spawn_blocking(move || apexos_core::vision::prepare_image(&bytes)).await,
                    Err(e) => { eprintln!("[vision] user image read failed: {e}"); continue; }
                },
                (Err(e), _) | (_, Err(e)) => { eprintln!("[vision] user image path rejected: {e}"); continue; }
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
    Extension(auth): Extension<RequestAuth>,
    Path(id):     Path<u64>,
    Json(body):   Json<serde_json::Value>,
) -> impl IntoResponse {
    if !session_ok(&state.sessions_dir, id, &auth) {
        return Json(serde_json::json!({ "ok": false, "error": "not your session" }));
    }
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

async fn sessions_handler(
    State(state): State<GatewayState>,
    Extension(auth): Extension<RequestAuth>,
) -> impl IntoResponse {
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
        if !session_ok(&state.sessions_dir, id, &auth) { continue; }

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

// ── session management: delete / archive / export ──────────────────────────────

/// The path to session `id`'s JSONL transcript (filename = id, one Message per line).
fn session_file(sessions_dir: &std::path::Path, id: u64) -> PathBuf {
    sessions_dir.join(format!("{id}.jsonl"))
}

fn session_ok(dir: &std::path::Path, id: u64, auth: &RequestAuth) -> bool {
    match auth {
        RequestAuth::Admin => true,
        RequestAuth::Session(a) => session_auth::session_visible_to(dir, id, a),
    }
}

/// DELETE /api/sessions/:id — remove a session's transcript and drop its in-memory
/// history. Irreversible (the UI confirms first); the cerebro-consolidate step
/// — extract useful info before deletion — is the safety net (next slice). The
/// root session 0 is refused: it's the always-on funnel for sensor alerts +
/// scheduled tasks, so deleting it is never what the user means.
async fn session_delete_handler(
    State(state): State<GatewayState>,
    Extension(auth): Extension<RequestAuth>,
    Path(id):     Path<u64>,
) -> impl IntoResponse {
    if id == 0 {
        return Json(serde_json::json!({ "ok": false, "error": "the root session (0) cannot be deleted" }));
    }
    if !session_ok(&state.sessions_dir, id, &auth) {
        return Json(serde_json::json!({ "ok": false, "error": "not your session" }));
    }
    retire_session(&state, id, SessionRetireKind::Delete).await
}

/// POST /api/sessions/:id/archive — move the transcript into `sessions/archive/`
/// (out of the active list — `sessions_handler` reads the top level only) and drop
/// the in-memory history. Recoverable: the file is preserved, just hidden.
async fn session_archive_handler(
    State(state): State<GatewayState>,
    Extension(auth): Extension<RequestAuth>,
    Path(id):     Path<u64>,
) -> impl IntoResponse {
    if id == 0 {
        return Json(serde_json::json!({ "ok": false, "error": "the root session (0) cannot be archived" }));
    }
    if !session_ok(&state.sessions_dir, id, &auth) {
        return Json(serde_json::json!({ "ok": false, "error": "not your session" }));
    }
    retire_session(&state, id, SessionRetireKind::Archive).await
}

/// Auth already checked. Hand the retire to the router so TurnGate + tombstone
/// + file IO run in one place (SA-8).
async fn retire_session(
    state: &GatewayState,
    id:    u64,
    kind:  SessionRetireKind,
) -> Json<serde_json::Value> {
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    if state.session_retire_tx.send(SessionRetireReq {
        session_id: id, kind, reply: reply_tx,
    }).await.is_err() {
        return Json(serde_json::json!({ "ok": false, "error": "session retire worker unavailable" }));
    }
    match tokio::time::timeout(std::time::Duration::from_secs(30), reply_rx).await {
        Ok(Ok(v))  => Json(v),
        Ok(Err(_)) => Json(serde_json::json!({ "ok": false, "error": "session retire worker dropped the request" })),
        Err(_)     => Json(serde_json::json!({ "ok": false, "error": "session retire timed out" })),
    }
}

/// POST /api/sessions/:id/consolidate — distill the session into Cerebro: one LLM
/// turn summarizes the transcript into a summary + key discoveries, stored via
/// `session_save` (so useful info is preserved before an export/archive/delete).
/// The actual work runs in the agentd consolidation worker (it owns the provider +
/// ToolProxy); here we send a request and await its oneshot reply (bounded — an LLM
/// call over a long transcript can take a while, but never hangs the socket).
async fn session_consolidate_handler(
    State(state): State<GatewayState>,
    Extension(auth): Extension<RequestAuth>,
    Path(id):     Path<u64>,
) -> impl IntoResponse {
    if !session_ok(&state.sessions_dir, id, &auth) {
        return Json(serde_json::json!({ "ok": false, "error": "not your session" }));
    }
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    if state.consolidate_tx.send(ConsolidateReq { session_id: id, reply: reply_tx }).await.is_err() {
        return Json(serde_json::json!({ "ok": false, "error": "consolidation worker unavailable" }));
    }
    match tokio::time::timeout(std::time::Duration::from_secs(120), reply_rx).await {
        Ok(Ok(v))  => Json(v),
        Ok(Err(_)) => Json(serde_json::json!({ "ok": false, "error": "consolidation worker dropped the request" })),
        Err(_)     => Json(serde_json::json!({ "ok": false, "error": "consolidation timed out" })),
    }
}

/// Compact a tool-call/result JSON value to a short single-line string for the
/// markdown transcript (full payloads bloat the export; the raw `jsonl` format
/// keeps everything for machine use).
pub fn compact_json(v: &serde_json::Value) -> String {
    let s = match v {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    let s = s.replace('\n', " ");
    if s.chars().count() > 200 {
        format!("{}…", s.chars().take(200).collect::<String>())
    } else {
        s
    }
}

/// Render a session's JSONL transcript to a readable markdown document.
pub fn render_session_markdown(id: u64, jsonl: &str) -> String {
    use apexos_core::{ContentBlock, Message};
    let mut out = format!("# Session {id}\n\n");
    for line in jsonl.lines().filter(|l| !l.trim().is_empty()) {
        let msg: Message = match serde_json::from_str(line) { Ok(m) => m, Err(_) => continue };
        let (label, content) = match &msg {
            Message::User      { content } => ("You",  content),
            Message::Assistant { content } => ("APEX", content),
        };
        let mut parts: Vec<String> = Vec::new();
        for b in content {
            match b {
                ContentBlock::Text { text } if !text.trim().is_empty() => parts.push(text.clone()),
                ContentBlock::ToolUse { name, input, .. } =>
                    parts.push(format!("🔧 `{name}`({})", compact_json(input))),
                ContentBlock::ToolResult { content, is_error, .. } =>
                    parts.push(format!("{} {}", if *is_error { "⚠ tool error:" } else { "↳" }, compact_json(content))),
                ContentBlock::Image { .. } => parts.push("🖼 [image]".into()),
                _ => {} // thinking blocks are omitted from the transcript
            }
        }
        if !parts.is_empty() {
            out.push_str(&format!("**{label}:** {}\n\n", parts.join("\n\n")));
        }
    }
    out
}

/// List every active (non-archived) session id under `sessions_dir`.
async fn list_session_ids(sessions_dir: &std::path::Path) -> Vec<u64> {
    let mut ids = Vec::new();
    if let Ok(mut rd) = tokio::fs::read_dir(sessions_dir).await {
        while let Ok(Some(entry)) = rd.next_entry().await {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") { continue; }
            if let Some(id) = path.file_stem().and_then(|s| s.to_str()).and_then(|s| s.parse().ok()) {
                ids.push(id);
            }
        }
    }
    ids.sort_unstable();
    ids
}

/// POST /api/sessions/export — export one/some/all sessions into the workspace.
/// Body: `{ ids?: [u64], all?: bool, format?: "md" | "jsonl" }`. `all:true` exports
/// every active session; otherwise `ids` selects them. Each session is written to
/// `<workspace>/exports/session-<id>.<ext>` — a markdown transcript by default (or
/// the raw jsonl for machine use). Writing into the workspace works on every
/// surface (kiosk has no browser download; the PWA / file browser / scp can read it).
async fn session_export_handler(
    State(state): State<GatewayState>,
    Json(body):   Json<serde_json::Value>,
) -> impl IntoResponse {
    let format = match body["format"].as_str() { Some("jsonl") => "jsonl", _ => "md" };
    let ids: Vec<u64> = if body["all"].as_bool().unwrap_or(false) {
        list_session_ids(&state.sessions_dir).await
    } else {
        body["ids"].as_array()
            .map(|a| a.iter().filter_map(|v| v.as_u64()).collect())
            .unwrap_or_default()
    };
    if ids.is_empty() {
        return Json(serde_json::json!({ "ok": false, "error": "no sessions selected" }));
    }

    let ws = std::env::var("AGENTD_WORKSPACE").ok().filter(|s| !s.is_empty())
        .unwrap_or_else(|| "/var/lib/agentd/workspace".to_string());
    let export_dir = PathBuf::from(ws).join("exports");
    if let Err(e) = tokio::fs::create_dir_all(&export_dir).await {
        return Json(serde_json::json!({ "ok": false, "error": format!("exports dir: {e}") }));
    }

    let mut files = Vec::new();
    for id in ids {
        let jsonl = match tokio::fs::read_to_string(session_file(&state.sessions_dir, id)).await {
            Ok(t) => t,
            Err(_) => continue,
        };
        let content = if format == "jsonl" { jsonl } else { render_session_markdown(id, &jsonl) };
        let fname = format!("session-{id}.{format}");
        if tokio::fs::write(export_dir.join(&fname), content).await.is_ok() {
            files.push(fname);
        }
    }
    Json(serde_json::json!({
        "ok":    !files.is_empty(),
        "count": files.len(),
        "dir":   "exports",
        "files": files,
    }))
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

    // One recording at a time: a concurrent start would clobber the shared WAV
    // path mid-write, so refuse with 409 (mirrors the wake CAS) instead of
    // silently killing the other client's capture. A child that already exited
    // (arecord self-bounds at -d 30, or a stop that never came) frees the slot.
    {
        let mut guard = recorder_lock().lock().await;
        let in_flight = matches!(guard.as_mut().map(|c| c.try_wait()), Some(Ok(None)));
        if in_flight {
            return (StatusCode::CONFLICT, "a recording is already in progress").into_response();
        }
        guard.take(); // reap an exited/errored child
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

    let r = transcribe_wav(SERVER_WAV).await;
    let _ = tokio::fs::remove_file(SERVER_WAV).await;
    match r {
        Ok(text) => Json(serde_json::json!({ "text": text })).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

// ── Voice: STT backend selection (AGENTD_STT_BACKEND) ──────────────────────────
// Voice slice 3: STT mirrors the TTS selector. AGENTD_STT_BACKEND = auto|local|api|off.
// `local` = whisper-cpp (existing), `api` = cloud (OpenAI / ElevenLabs Scribe), `off`
// = disabled, `auto` = local first (free/offline) → api. Unlike TTS there's no trivial
// always-on fallback, so an empty plan / all-failed returns an honest error.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SttStep {
    Local,      // whisper-cpp
    OpenAi,     // cloud API
    ElevenLabs, // cloud API (Scribe)
}

/// Pure: resolve the ordered STT plan from config + key availability.
fn stt_plan(backend: &str, stt_api: &str, has_openai: bool, has_elevenlabs: bool) -> Vec<SttStep> {
    let api_step = || -> Option<SttStep> {
        match stt_api.trim().to_ascii_lowercase().as_str() {
            "openai" | "oai" => has_openai.then_some(SttStep::OpenAi),
            "elevenlabs" | "eleven" | "11labs" | "scribe" => {
                has_elevenlabs.then_some(SttStep::ElevenLabs)
            }
            // auto-by-key: prefer OpenAI (whisper is the canonical STT), else Scribe.
            _ if has_openai => Some(SttStep::OpenAi),
            _ if has_elevenlabs => Some(SttStep::ElevenLabs),
            _ => None,
        }
    };
    match backend.trim().to_ascii_lowercase().as_str() {
        "off" | "none" | "0" | "false" | "disabled" => vec![],
        "local" => vec![SttStep::Local],
        "api" => api_step().into_iter().collect(),
        // auto (and unknown): local first, then api if a key is set.
        _ => {
            let mut v = vec![SttStep::Local];
            if let Some(s) = api_step() {
                v.push(s);
            }
            v
        }
    }
}

/// Transcribe a 16 kHz mono WAV at `wav_path`, trying the configured STT plan in
/// order. Returns the text, or an error string if every backend in the plan failed.
async fn transcribe_wav(wav_path: &str) -> Result<String, String> {
    let cfg = voice_config_snapshot();
    let plan = stt_plan(
        &cfg.stt_backend,
        &cfg.stt_api,
        env_nonempty("OPENAI_API_KEY"),
        env_nonempty("ELEVENLABS_API_KEY"),
    );
    if plan.is_empty() {
        return Err("STT disabled (AGENTD_STT_BACKEND=off, or no backend available)".into());
    }
    let mut last_err = String::from("no STT backend available");
    for step in plan {
        let r = match step {
            SttStep::Local => stt_local(wav_path).await,
            SttStep::OpenAi => stt_cloud_openai(wav_path).await,
            SttStep::ElevenLabs => stt_cloud_elevenlabs(wav_path).await,
        };
        match r {
            Ok(text) => return Ok(text),
            Err(e) => {
                eprintln!("[voice] stt {step:?} failed: {e}");
                last_err = e;
            }
        }
    }
    Err(last_err)
}

/// Local STT: the apex-stt (Whisper) sidecar first, then a hand-installed whisper-cpp
/// binary. Both failing surfaces an Err so the plan can fall through to cloud.
async fn stt_local(wav_path: &str) -> Result<String, String> {
    match stt_sidecar(wav_path).await {
        Ok(text) => Ok(text),
        Err(sidecar_err) => match stt_whispercpp(wav_path).await {
            Ok(text) => Ok(text),
            Err(cpp_err) => Err(format!("apex-stt: {sidecar_err}; whisper-cpp: {cpp_err}")),
        },
    }
}

/// POST the WAV to the apex-stt sidecar. `Err` if unreachable (voice-off node refuses
/// the loopback connection ~instantly) or it errored.
async fn stt_sidecar(wav_path: &str) -> Result<String, String> {
    let url = std::env::var("APEX_STT_URL")
        .unwrap_or_else(|_| "http://127.0.0.1:8771/transcribe".to_string());
    let bytes = tokio::fs::read(wav_path).await.map_err(|e| format!("read wav: {e}"))?;
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(60))
        .build()
        .map_err(|e| e.to_string())?;
    let resp = client
        .post(&url)
        .header("content-type", "audio/wav")
        .body(bytes)
        .send()
        .await
        .map_err(|e| format!("apex-stt request: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("apex-stt HTTP {}", resp.status()));
    }
    let v: serde_json::Value = resp.json().await.map_err(|e| format!("apex-stt json: {e}"))?;
    Ok(v["text"].as_str().unwrap_or_default().trim().to_string())
}

/// Local whisper-cpp. A missing binary/model surfaces as an Err so the plan can
/// fall through to a cloud backend.
async fn stt_whispercpp(wav_path: &str) -> Result<String, String> {
    let model = std::env::var("WHISPER_MODEL")
        .unwrap_or_else(|_| "/var/lib/agentd/whisper/ggml-tiny.en.bin".into());
    let bin = std::env::var("WHISPER_BIN").unwrap_or_else(|_| "/usr/local/bin/whisper-cpp".into());
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        tokio::process::Command::new(&bin)
            .args(["-m", &model, "-f", wav_path, "-nt", "-l", "en", "--no-prints"])
            .output(),
    )
    .await;
    match result {
        Ok(Ok(out)) if out.status.success() => {
            let raw = String::from_utf8_lossy(&out.stdout);
            Ok(raw
                .lines()
                .map(|l| l.trim())
                .filter(|l| !l.is_empty() && *l != "[BLANK_AUDIO]")
                .collect::<Vec<_>>()
                .join(" "))
        }
        Ok(Ok(out)) => Err(format!(
            "whisper-cpp exited: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )),
        Ok(Err(e)) => Err(format!("whisper-cpp: {e}")),
        Err(_) => Err("whisper-cpp timed out".into()),
    }
}

/// OpenAI transcription (`/v1/audio/transcriptions`, multipart). OPENAI_API_KEY must
/// be a real api.openai.com key (the routing OAI key may be OpenRouter).
async fn stt_cloud_openai(wav_path: &str) -> Result<String, String> {
    let key = std::env::var("OPENAI_API_KEY")
        .ok()
        .filter(|k| !k.trim().is_empty())
        .ok_or("no OPENAI_API_KEY")?;
    let model = std::env::var("OPENAI_STT_MODEL").unwrap_or_else(|_| "whisper-1".into());
    let bytes = tokio::fs::read(wav_path).await.map_err(|e| format!("read wav: {e}"))?;
    let part = reqwest::multipart::Part::bytes(bytes)
        .file_name("audio.wav")
        .mime_str("audio/wav")
        .map_err(|e| e.to_string())?;
    let form = reqwest::multipart::Form::new().part("file", part).text("model", model);
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(60))
        .build()
        .map_err(|e| e.to_string())?;
    let resp = client
        .post("https://api.openai.com/v1/audio/transcriptions")
        .bearer_auth(key)
        .multipart(form)
        .send()
        .await
        .map_err(|e| format!("openai request: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("openai HTTP {}", resp.status()));
    }
    let v: serde_json::Value = resp.json().await.map_err(|e| format!("openai json: {e}"))?;
    Ok(v["text"].as_str().unwrap_or_default().trim().to_string())
}

/// ElevenLabs Scribe (`/v1/speech-to-text`, multipart). Reuses ELEVENLABS_API_KEY.
async fn stt_cloud_elevenlabs(wav_path: &str) -> Result<String, String> {
    let key = std::env::var("ELEVENLABS_API_KEY")
        .ok()
        .filter(|k| !k.trim().is_empty())
        .ok_or("no ELEVENLABS_API_KEY")?;
    let model = std::env::var("ELEVENLABS_STT_MODEL").unwrap_or_else(|_| "scribe_v2".into());
    let bytes = tokio::fs::read(wav_path).await.map_err(|e| format!("read wav: {e}"))?;
    let part = reqwest::multipart::Part::bytes(bytes)
        .file_name("audio.wav")
        .mime_str("audio/wav")
        .map_err(|e| e.to_string())?;
    let form = reqwest::multipart::Form::new().part("file", part).text("model_id", model);
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(60))
        .build()
        .map_err(|e| e.to_string())?;
    let resp = client
        .post("https://api.elevenlabs.io/v1/speech-to-text")
        .header("xi-api-key", key)
        .multipart(form)
        .send()
        .await
        .map_err(|e| format!("elevenlabs request: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("elevenlabs HTTP {}", resp.status()));
    }
    let v: serde_json::Value = resp.json().await.map_err(|e| format!("elevenlabs json: {e}"))?;
    Ok(v["text"].as_str().unwrap_or_default().trim().to_string())
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

    let r = transcribe_wav(&tmp_wav).await;
    let _ = tokio::fs::remove_file(&tmp_wav).await;
    match r {
        Ok(text) => Json(serde_json::json!({ "text": text })).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

#[cfg(test)]
mod stt_plan_tests {
    use super::{stt_plan, SttStep};

    #[test]
    fn plan_resolves_backend_and_keys() {
        use SttStep::*;
        // off → empty
        assert_eq!(stt_plan("off", "", true, true), Vec::<SttStep>::new());
        // local → whisper-cpp only, never cloud
        assert_eq!(stt_plan("local", "", true, true), vec![Local]);
        // auto → local first, then api (OpenAI preferred for STT)
        assert_eq!(stt_plan("auto", "", true, true), vec![Local, OpenAi]);
        assert_eq!(stt_plan("", "", false, true), vec![Local, ElevenLabs]); // default auto, only ElevenLabs
        assert_eq!(stt_plan("auto", "", false, false), vec![Local]); // no keys → local only
        // api → cloud only
        assert_eq!(stt_plan("api", "", true, true), vec![OpenAi]);
        assert_eq!(stt_plan("api", "elevenlabs", true, true), vec![ElevenLabs]); // explicit override
        assert_eq!(stt_plan("api", "", false, false), Vec::<SttStep>::new()); // api wanted, no key
        assert_eq!(stt_plan("api", "openai", false, true), Vec::<SttStep>::new()); // explicit, no matching key
    }
}

// ── Runtime voice config (live-tunable via /api/voice + the Settings UI) ───────
// Voice slice 5: the env vars (AGENTD_VOICE_BACKEND / _TTS_API / _STT_BACKEND /
// _STT_API) seed a process-global that the Settings UI retunes live, persisted to
// AGENTD_VOICE_CONFIG. Same shape as the sensor-profile / prompt-cache live knobs.
// Empty string = the plan-resolver default (auto backend / auto-by-key provider),
// so an unset/default node is byte-identical to the pre-slice env behaviour.
#[derive(Clone, Default, serde::Serialize, serde::Deserialize)]
struct VoiceConfig {
    #[serde(default)]
    voice_backend: String, // auto|local|api|off  (TTS)
    #[serde(default)]
    tts_api: String, // elevenlabs|openai|"" (auto-by-key)
    #[serde(default)]
    stt_backend: String, // auto|local|api|off  (STT)
    #[serde(default)]
    stt_api: String, // openai|elevenlabs|"" (auto-by-key)
}

static VOICE_CONFIG: OnceLock<std::sync::RwLock<VoiceConfig>> = OnceLock::new();

fn voice_config_path() -> String {
    std::env::var("AGENTD_VOICE_CONFIG").unwrap_or_else(|_| "/var/lib/agentd/voice_config.json".into())
}

fn voice_config_cell() -> &'static std::sync::RwLock<VoiceConfig> {
    VOICE_CONFIG.get_or_init(|| {
        // Persisted file wins (the operator's live choice survives restart); else seed
        // from /etc/agentd/env so a fresh node honours its configured defaults.
        let cfg = std::fs::read_to_string(voice_config_path())
            .ok()
            .and_then(|s| serde_json::from_str::<VoiceConfig>(&s).ok())
            .unwrap_or_else(|| VoiceConfig {
                voice_backend: std::env::var("AGENTD_VOICE_BACKEND").unwrap_or_default(),
                tts_api: std::env::var("AGENTD_TTS_API").unwrap_or_default(),
                stt_backend: std::env::var("AGENTD_STT_BACKEND").unwrap_or_default(),
                stt_api: std::env::var("AGENTD_STT_API").unwrap_or_default(),
            });
        std::sync::RwLock::new(cfg)
    })
}

fn voice_config_snapshot() -> VoiceConfig {
    voice_config_cell().read().map(|c| c.clone()).unwrap_or_default()
}

fn voice_backend_valid(s: &str) -> bool {
    matches!(s, "auto" | "local" | "api" | "off")
}

// ── Imaginarium reach (docs/imaginarium.md) ───────────────────────────────────
// Hands an authenticated client the node's Imaginarium base URL + LAN token from
// agentd's OWN environment (the systemd-parsed /etc/agentd/env values install.sh
// provisioned — the same ones the MCP proxy plugin inherits). Exists for the
// DESKTOP ui-slint Imagine app: a winit window in the user's session cannot read
// the 0600 root env file, so it asks agentd after login instead. Never the xAI
// key — that lives only in /etc/imaginarium/env. Gated like every /api route
// (admin token OR a minted login session token): node-local trust — whoever may
// use this node's UI may use its generator.

/// Pure resolver so the trim/default rules are testable without process env.
fn imaginarium_reach_from(url_env: Option<String>, token_env: Option<String>) -> (String, String) {
    let url = url_env
        .map(|s| s.trim().trim_end_matches('/').to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "http://127.0.0.1:8791".to_string());
    let token = token_env.map(|s| s.trim().to_string()).unwrap_or_default();
    (url, token)
}

async fn imaginarium_reach_handler() -> impl IntoResponse {
    let (url, token) = imaginarium_reach_from(
        std::env::var("IMAGINARIUM_URL").ok(),
        std::env::var("IMAGINARIUM_TOKEN").ok(),
    );
    Json(serde_json::json!({
        "url": url,
        "token": token,
        "configured": !token.is_empty(),
    }))
}

#[cfg(test)]
mod imaginarium_reach_tests {
    use super::imaginarium_reach_from;

    #[test]
    fn reach_defaults_trims_and_reports() {
        // Unset env → loopback default, no token.
        assert_eq!(
            imaginarium_reach_from(None, None),
            ("http://127.0.0.1:8791".into(), String::new())
        );
        // Trailing slash + whitespace healed; token trimmed.
        assert_eq!(
            imaginarium_reach_from(
                Some("  http://10.0.0.5:8791/  ".into()),
                Some(" abc123 \n".into())
            ),
            ("http://10.0.0.5:8791".into(), "abc123".into())
        );
        // Empty strings behave like unset.
        assert_eq!(
            imaginarium_reach_from(Some("   ".into()), Some(String::new())),
            ("http://127.0.0.1:8791".into(), String::new())
        );
    }
}

async fn get_voice_handler() -> impl IntoResponse {
    let c = voice_config_snapshot();
    let or_auto = |s: String| if s.is_empty() { "auto".to_string() } else { s };
    Json(serde_json::json!({
        "voice_backend": or_auto(c.voice_backend),
        "tts_api":       c.tts_api,
        "stt_backend":   or_auto(c.stt_backend),
        "stt_api":       c.stt_api,
        "has_elevenlabs": env_nonempty("ELEVENLABS_API_KEY"),
        "has_openai":     env_nonempty("OPENAI_API_KEY"),
        "backends":      ["auto", "local", "api", "off"],
    }))
}

async fn set_voice_handler(Json(body): Json<serde_json::Value>) -> impl IntoResponse {
    {
        let Ok(mut c) = voice_config_cell().write() else {
            return (StatusCode::INTERNAL_SERVER_ERROR, "voice config lock poisoned").into_response();
        };
        // Backends are validated against the known set; providers are free-form (the
        // plan resolvers treat any unknown value as auto-by-key, so they can't misfire).
        if let Some(v) = body["voice_backend"].as_str() {
            if voice_backend_valid(v) {
                c.voice_backend = v.to_string();
            }
        }
        if let Some(v) = body["stt_backend"].as_str() {
            if voice_backend_valid(v) {
                c.stt_backend = v.to_string();
            }
        }
        if let Some(v) = body["tts_api"].as_str() {
            c.tts_api = v.to_string();
        }
        if let Some(v) = body["stt_api"].as_str() {
            c.stt_api = v.to_string();
        }
        // Persist best-effort (in-memory change is already live for the next turn).
        let _ = std::fs::write(
            voice_config_path(),
            serde_json::to_string_pretty(&*c).unwrap_or_default(),
        );
    }
    get_voice_handler().await.into_response()
}

// ── TTS backend selection (AGENTD_VOICE_BACKEND) ───────────────────────────────
// Voice slice 2: one knob, `AGENTD_VOICE_BACKEND` = auto|local|api|off, mirroring
// CEREBRO_VISION_BACKEND. `local` = the Kokoro apex-tts sidecar, `api` = cloud TTS
// (ElevenLabs / OpenAI, picked by AGENTD_TTS_API or whichever key is set), `off` =
// silent, `auto` = local first (free/offline) → api → piper → espeak. espeak-ng is
// always the final fallback, so a node always talks. Default `auto` deliberately
// prefers the free local voice over paid API spend (the operator opts into `api`).

/// One ordered TTS attempt. The handler tries them in turn until one speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TtsStep {
    Local,      // apex-tts Kokoro sidecar
    ElevenLabs, // cloud API
    OpenAi,     // cloud API
    Piper,      // legacy external binary
    Espeak,     // always-available fallback
}

/// Pure: resolve the ordered fallback plan from config + key availability.
/// `backend` = AGENTD_VOICE_BACKEND, `tts_api` = AGENTD_TTS_API (empty → auto-by-key).
fn tts_plan(backend: &str, tts_api: &str, has_elevenlabs: bool, has_openai: bool) -> Vec<TtsStep> {
    let api_step = || -> Option<TtsStep> {
        match tts_api.trim().to_ascii_lowercase().as_str() {
            "elevenlabs" | "eleven" | "11labs" => has_elevenlabs.then_some(TtsStep::ElevenLabs),
            "openai" | "oai" => has_openai.then_some(TtsStep::OpenAi),
            // auto-by-key: prefer ElevenLabs (quality/latency), else OpenAI.
            _ if has_elevenlabs => Some(TtsStep::ElevenLabs),
            _ if has_openai => Some(TtsStep::OpenAi),
            _ => None,
        }
    };
    match backend.trim().to_ascii_lowercase().as_str() {
        "off" | "none" | "0" | "false" | "disabled" => vec![],
        "local" => vec![TtsStep::Local, TtsStep::Piper, TtsStep::Espeak],
        "api" => {
            let mut v = Vec::new();
            if let Some(s) = api_step() {
                v.push(s);
            }
            v.push(TtsStep::Espeak);
            v
        }
        // auto (and anything unknown): local first, then api, then the fallbacks.
        _ => {
            let mut v = vec![TtsStep::Local];
            if let Some(s) = api_step() {
                v.push(s);
            }
            v.push(TtsStep::Piper);
            v.push(TtsStep::Espeak);
            v
        }
    }
}

fn env_nonempty(name: &str) -> bool {
    std::env::var(name).map(|v| !v.trim().is_empty()).unwrap_or(false)
}

/// Ask the apex-tts (Kokoro) sidecar to synthesize `text`, returning WAV bytes.
/// `None` if the sidecar is unreachable / errored — on a voice-off node the
/// loopback connection is refused ~instantly, so the caller falls back cheaply.
async fn tts_sidecar_wav(text: &str) -> Option<Vec<u8>> {
    let url = std::env::var("APEX_TTS_URL")
        .unwrap_or_else(|_| "http://127.0.0.1:8770/synth".to_string());
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .ok()?;
    let resp = client
        .post(&url)
        .json(&serde_json::json!({ "text": text }))
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let bytes = resp.bytes().await.ok()?;
    (!bytes.is_empty()).then(|| bytes.to_vec())
}

/// Play WAV bytes on the device speakers via aplay. Returns true if it played.
async fn play_wav_bytes(bytes: &[u8]) -> bool {
    let path = format!("/tmp/apex_speak_{}.wav", speak_stamp());
    if tokio::fs::write(&path, bytes).await.is_err() {
        return false;
    }
    let ok = tokio::process::Command::new("aplay")
        .args(["-q", &path])
        .output()
        .await
        .map(|o| o.status.success())
        .unwrap_or(false);
    let _ = tokio::fs::remove_file(&path).await;
    ok
}

/// Wrap raw little-endian PCM in a minimal WAV container, so every TTS backend
/// yields a self-describing WAV — uniform for both aplay and client-side playback.
fn pcm_to_wav(pcm: &[u8], sample_rate: u32, channels: u16, bits: u16) -> Vec<u8> {
    let byte_rate = sample_rate * channels as u32 * (bits as u32 / 8);
    let block_align = channels * (bits / 8);
    let data_len = pcm.len() as u32;
    let mut w = Vec::with_capacity(44 + pcm.len());
    w.extend_from_slice(b"RIFF");
    w.extend_from_slice(&(36 + data_len).to_le_bytes());
    w.extend_from_slice(b"WAVE");
    w.extend_from_slice(b"fmt ");
    w.extend_from_slice(&16u32.to_le_bytes()); // PCM fmt chunk size
    w.extend_from_slice(&1u16.to_le_bytes()); // format = PCM
    w.extend_from_slice(&channels.to_le_bytes());
    w.extend_from_slice(&sample_rate.to_le_bytes());
    w.extend_from_slice(&byte_rate.to_le_bytes());
    w.extend_from_slice(&block_align.to_le_bytes());
    w.extend_from_slice(&bits.to_le_bytes());
    w.extend_from_slice(b"data");
    w.extend_from_slice(&data_len.to_le_bytes());
    w.extend_from_slice(pcm);
    w
}

fn speak_stamp() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros()
}

// Each backend produces a self-describing WAV (Some) or None. speak_handler plays
// them server-side (aplay); /api/tts returns them for client-side playback.

/// Local Kokoro via the apex-tts sidecar — already returns a WAV.
async fn tts_local_wav(text: &str) -> Option<Vec<u8>> {
    tts_sidecar_wav(text).await
}

/// ElevenLabs TTS → raw pcm_24000 → wrapped WAV. Needs ELEVENLABS_API_KEY.
async fn tts_elevenlabs_wav(text: &str) -> Option<Vec<u8>> {
    let key = std::env::var("ELEVENLABS_API_KEY").ok().filter(|k| !k.trim().is_empty())?;
    let voice = std::env::var("ELEVENLABS_VOICE_ID")
        .unwrap_or_else(|_| "21m00Tcm4TlvDq8ikWAM".to_string()); // "Rachel" default
    let model =
        std::env::var("ELEVENLABS_MODEL").unwrap_or_else(|_| "eleven_flash_v2_5".to_string());
    let url =
        format!("https://api.elevenlabs.io/v1/text-to-speech/{voice}?output_format=pcm_24000");
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .ok()?;
    let resp = client
        .post(&url)
        .header("xi-api-key", key)
        .json(&serde_json::json!({ "text": text, "model_id": model }))
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        eprintln!("[voice] elevenlabs HTTP {}", resp.status());
        return None;
    }
    let pcm = resp.bytes().await.ok()?;
    (!pcm.is_empty()).then(|| pcm_to_wav(&pcm, 24000, 1, 16))
}

/// OpenAI TTS → wav. Needs OPENAI_API_KEY (a real api.openai.com key — the routing
/// OAI key may be OpenRouter, which doesn't serve /v1/audio/speech).
async fn tts_openai_wav(text: &str) -> Option<Vec<u8>> {
    let key = std::env::var("OPENAI_API_KEY").ok().filter(|k| !k.trim().is_empty())?;
    let model = std::env::var("OPENAI_TTS_MODEL").unwrap_or_else(|_| "gpt-4o-mini-tts".to_string());
    let voice = std::env::var("OPENAI_TTS_VOICE").unwrap_or_else(|_| "alloy".to_string());
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .ok()?;
    let resp = client
        .post("https://api.openai.com/v1/audio/speech")
        .bearer_auth(key)
        .json(&serde_json::json!({
            "model": model, "input": text, "voice": voice, "response_format": "wav"
        }))
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        eprintln!("[voice] openai HTTP {}", resp.status());
        return None;
    }
    let b = resp.bytes().await.ok()?;
    (!b.is_empty()).then(|| b.to_vec())
}

/// Legacy piper external binary → wav file → bytes (if PIPER_MODEL is set).
async fn tts_piper_wav(text: &str) -> Option<Vec<u8>> {
    let model = std::env::var("PIPER_MODEL").ok()?;
    let wav = format!("/tmp/apex_speak_{}.wav", speak_stamp());
    let mut child = tokio::process::Command::new("piper")
        .args(["--model", &model, "--output_file", &wav])
        .stdin(std::process::Stdio::piped())
        .spawn()
        .ok()?;
    if let Some(mut stdin) = child.stdin.take() {
        use tokio::io::AsyncWriteExt;
        let _ = stdin.write_all(text.as_bytes()).await;
    }
    let _ = child.wait().await;
    let bytes = tokio::fs::read(&wav).await.ok();
    let _ = tokio::fs::remove_file(&wav).await;
    bytes.filter(|b| !b.is_empty())
}

/// espeak-ng → wav file → bytes (the always-available final fallback). `-w` writes
/// a WAV instead of playing, so even the fallback can be returned to a client.
async fn tts_espeak_wav(text: &str) -> Option<Vec<u8>> {
    let wav = format!("/tmp/apex_speak_{}.wav", speak_stamp());
    let ok = tokio::process::Command::new("espeak-ng")
        .args(["-a", "100", "-s", "150", "-w", &wav, text])
        .output()
        .await
        .map(|o| o.status.success())
        .unwrap_or(false);
    let bytes = if ok { tokio::fs::read(&wav).await.ok() } else { None };
    let _ = tokio::fs::remove_file(&wav).await;
    bytes.filter(|b| !b.is_empty())
}

/// Resolve the TTS plan (AGENTD_VOICE_BACKEND) → the first backend's audio as WAV
/// bytes. Shared by /api/speak (server-side play) and /api/tts (client-side return).
async fn tts_synth_wav(text: &str) -> Option<Vec<u8>> {
    let cfg = voice_config_snapshot();
    let plan = tts_plan(
        &cfg.voice_backend,
        &cfg.tts_api,
        env_nonempty("ELEVENLABS_API_KEY"),
        env_nonempty("OPENAI_API_KEY"),
    );
    for step in plan {
        let wav = match step {
            TtsStep::Local => tts_local_wav(text).await,
            TtsStep::ElevenLabs => tts_elevenlabs_wav(text).await,
            TtsStep::OpenAi => tts_openai_wav(text).await,
            TtsStep::Piper => tts_piper_wav(text).await,
            TtsStep::Espeak => tts_espeak_wav(text).await,
        };
        if wav.is_some() {
            return wav;
        }
    }
    None
}

/// POST /api/speak {text} — synthesize and play on THIS device's speakers (kiosk /
/// headless, where agentd owns the audio). Fire-and-forget.
async fn speak_handler(Json(body): Json<serde_json::Value>) -> impl IntoResponse {
    let text = match body["text"].as_str() {
        Some(t) if !t.trim().is_empty() => t.to_string(),
        _ => return StatusCode::BAD_REQUEST.into_response(),
    };
    tokio::spawn(async move {
        if let Some(wav) = tts_synth_wav(&text).await {
            play_wav_bytes(&wav).await;
        }
    });
    StatusCode::OK.into_response()
}

/// POST /api/tts {text} → audio/wav bytes for CLIENT-side playback (desktop / web /
/// phone, where the audio belongs to the user's session, not agentd). Same backend
/// selection as /api/speak, but returns the WAV instead of playing it server-side.
async fn tts_handler(Json(body): Json<serde_json::Value>) -> impl IntoResponse {
    let text = match body["text"].as_str() {
        Some(t) if !t.trim().is_empty() => t.to_string(),
        _ => return StatusCode::BAD_REQUEST.into_response(),
    };
    match tts_synth_wav(&text).await {
        Some(wav) => {
            ([(axum::http::header::CONTENT_TYPE, "audio/wav")], wav).into_response()
        }
        None => (StatusCode::SERVICE_UNAVAILABLE, "no TTS backend available").into_response(),
    }
}

#[cfg(test)]
mod voice_plan_tests {
    use super::{tts_plan, TtsStep};

    #[test]
    fn plan_resolves_backend_and_keys() {
        use TtsStep::*;
        // off → silence regardless of keys
        assert_eq!(tts_plan("off", "", true, true), Vec::<TtsStep>::new());
        // local → never hits the API, even with keys
        assert_eq!(tts_plan("local", "", true, true), vec![Local, Piper, Espeak]);
        // auto → local first, then api (ElevenLabs preferred), then fallbacks
        assert_eq!(tts_plan("auto", "", true, true), vec![Local, ElevenLabs, Piper, Espeak]);
        assert_eq!(tts_plan("", "", false, true), vec![Local, OpenAi, Piper, Espeak]); // default auto, only OpenAI key
        assert_eq!(tts_plan("auto", "", false, false), vec![Local, Piper, Espeak]); // no keys → no api step
        // api → api first then espeak (explicit choice doesn't fall back to local)
        assert_eq!(tts_plan("api", "", true, true), vec![ElevenLabs, Espeak]);
        assert_eq!(tts_plan("api", "openai", true, true), vec![OpenAi, Espeak]); // explicit provider override
        assert_eq!(tts_plan("api", "", false, false), vec![Espeak]); // api wanted but no key → espeak only
        // explicit provider with no matching key → no api step
        assert_eq!(tts_plan("api", "elevenlabs", false, true), vec![Espeak]);
    }
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
            // Write the WHOLE buffer: a single libc::write can short-write (esp. a
            // large paste exceeding the PTY buffer) or be interrupted (EINTR). The
            // old discarded result silently truncated input. Loop until flushed.
            let mut off = 0;
            while off < data.len() {
                let n = unsafe {
                    libc::write(mw, data[off..].as_ptr() as _, data.len() - off)
                };
                if n > 0 {
                    off += n as usize;
                } else if n < 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
                    continue;            // interrupted before writing — retry
                } else {
                    break;               // real error (e.g. EIO on PTY close) or 0 — stop
                }
            }
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

/// POST /api/spawn — run a one-shot sub-agent on THIS node for a mesh peer and
/// return its final output (the blocking-`agent_spawn` keystone). Body:
/// `{prompt, system?, timeout_s?}`. The turn runs in the agentd spawn worker (it
/// owns the engine); we await its oneshot reply. Loop guard: the `x-mesh-hops`
/// header (set by the caller's `mesh_agent_spawn`) is refused past a small cap so a
/// remote spawn can't recurse across nodes unboundedly.
async fn spawn_handler(
    State(state): State<GatewayState>,
    headers:      axum::http::HeaderMap,
    Json(body):   Json<serde_json::Value>,
) -> impl IntoResponse {
    let prompt = match body["prompt"].as_str() {
        Some(s) if !s.trim().is_empty() => s.to_string(),
        _ => return Json(serde_json::json!({ "ok": false, "error": "missing prompt" })),
    };
    let hops = match apexos_core::parse_mesh_hops(
        headers.get("x-mesh-hops").and_then(|v| v.to_str().ok()),
    ) {
        Ok(n) => n,
        Err(apexos_core::MeshHopsError::Limit) => {
            return Json(serde_json::json!({ "ok": false, "error": "mesh hop limit reached (loop guard)" }));
        }
        Err(_) => {
            return Json(serde_json::json!({ "ok": false, "error": "x-mesh-hops required and must increase" }));
        }
    };
    let system = body["system"].as_str().filter(|s| !s.trim().is_empty()).map(str::to_string);
    let timeout_s = body["timeout_s"].as_u64().unwrap_or(90).clamp(5, 300);

    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    if state.spawn_tx.send(SpawnReq { prompt, system, timeout_s, hops, reply: reply_tx }).await.is_err() {
        return Json(serde_json::json!({ "ok": false, "error": "spawn worker unavailable" }));
    }
    // The worker already bounds the turn by timeout_s; add slack for the round-trip.
    match tokio::time::timeout(std::time::Duration::from_secs(timeout_s + 15), reply_rx).await {
        Ok(Ok(v))  => Json(v),
        Ok(Err(_)) => Json(serde_json::json!({ "ok": false, "error": "spawn worker dropped the request" })),
        Err(_)     => Json(serde_json::json!({ "ok": false, "error": "spawn timed out" })),
    }
}

/// Shared body of the four `/api/worker/*` mesh endpoints (W2). Validates the
/// kill switch and `from` ∈ peer registry (the mesh/memory pattern — a bearer
/// token alone never authorizes worker-tier traffic), resolves the sender's
/// a2a landing session for fanouts, forwards to the worker driver's mesh arm,
/// and returns its JSON verbatim. All error shapes are HTTP 200 `{ok:false}`
/// (the `/api/spawn` idiom — the mesh client triages by body, not status).
async fn worker_mesh_request(
    state: GatewayState,
    kind: WorkerMeshKind,
    body: serde_json::Value,
    hops: u32,
) -> Json<serde_json::Value> {
    if !state.mesh_workers_enabled {
        return Json(serde_json::json!({ "ok": false, "error": "mesh workers disabled on this node (AGENTD_MESH_WORKERS=0)" }));
    }
    let from = match body["from"].as_str().filter(|s| !s.trim().is_empty()) {
        Some(f) => f.to_string(),
        None => return Json(serde_json::json!({ "ok": false, "error": "missing from" })),
    };
    if !state.peer_registry.read().await.contains(&from) {
        return Json(serde_json::json!({ "ok": false, "error": format!("'{from}' is not a registered peer on this node") }));
    }
    let parent = if kind == WorkerMeshKind::Fanout {
        Some(mesh_session_for(&state, &from))
    } else { None };
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    let req = WorkerMeshReq { kind, from, body, parent, hops, reply: reply_tx };
    if state.worker_mesh_tx.send(req).await.is_err() {
        return Json(serde_json::json!({ "ok": false, "error": "worker driver unavailable" }));
    }
    match tokio::time::timeout(std::time::Duration::from_secs(15), reply_rx).await {
        Ok(Ok(v))  => Json(v),
        Ok(Err(_)) => Json(serde_json::json!({ "ok": false, "error": "worker driver dropped the request" })),
        Err(_)     => Json(serde_json::json!({ "ok": false, "error": "worker driver timed out" })),
    }
}

/// POST /api/worker/fanout — host a batch of workers for a remote conductor
/// (W2). Body `{from, origin_batch, deadline_s?, tasks:[{prompt, model?}]}`.
/// The minted workers are ORDINARY local workers: this node's cap, FIFO,
/// policy gates (yolo never crosses the wire), review procedure, evidence dir
/// and episodes all apply — the wire distributes a finished machine. Their
/// batch's parent is the sender's a2a landing session, and the batch carries
/// its origin so the report POSTs home when it settles.
async fn worker_fanout_handler(
    State(state): State<GatewayState>,
    headers:      axum::http::HeaderMap,
    Json(body):   Json<serde_json::Value>,
) -> impl IntoResponse {
    let hops = match apexos_core::parse_mesh_hops(
        headers.get("x-mesh-hops").and_then(|v| v.to_str().ok()),
    ) {
        Ok(n) => n,
        Err(apexos_core::MeshHopsError::Limit) => {
            return Json(serde_json::json!({ "ok": false, "error": "mesh hop limit reached (loop guard)" }));
        }
        Err(_) => {
            return Json(serde_json::json!({ "ok": false, "error": "x-mesh-hops required and must increase" }));
        }
    };
    worker_mesh_request(state, WorkerMeshKind::Fanout, body, hops).await
}

/// POST /api/worker/query — a remote conductor polling one of its hosted
/// batches: `{from, batch}` → the rows (evidence docs inline for terminals).
/// The reconcile path after a conductor restart, and the poll half of the
/// push/poll supervision pair.
async fn worker_query_handler(
    State(state): State<GatewayState>,
    Json(body):   Json<serde_json::Value>,
) -> impl IntoResponse {
    worker_mesh_request(state, WorkerMeshKind::Query, body, 0).await
}

/// POST /api/worker/cancel — a remote conductor cancelling its hosted batch
/// (or specific workers in it): `{from, batch, workers?}`. Only the batch's
/// origin conductor is honored; the normal cancel path runs (full terminal
/// trail, honest report rows).
async fn worker_cancel_mesh_handler(
    State(state): State<GatewayState>,
    Json(body):   Json<serde_json::Value>,
) -> impl IntoResponse {
    worker_mesh_request(state, WorkerMeshKind::Cancel, body, 0).await
}

/// POST /api/worker/report — a hosting peer pushing a settled batch home to
/// THIS conducting node: `{from, origin_batch, batch, rows:[…]}` with the
/// small evidence docs inline (one hop; artifacts stay on the peer). The
/// driver mirrors evidence locally and the normal batch machinery reports.
async fn worker_report_mesh_handler(
    State(state): State<GatewayState>,
    Json(body):   Json<serde_json::Value>,
) -> impl IntoResponse {
    worker_mesh_request(state, WorkerMeshKind::Report, body, 0).await
}

/// GET /api/capabilities — this node's structured capability snapshot (senses,
/// tools, tier, memory mode, peer count), refreshed by agentd's embodiment loop.
/// Token-gated; mesh peers query it via the `mesh_capabilities` tool to route by
/// capability. Null until the first embodiment refresh (~2s after boot).
async fn capabilities_handler(State(state): State<GatewayState>) -> impl IntoResponse {
    Json(state.capabilities.read().await.clone())
}

/// The selectable sensor-alert sensitivity profiles (order = UI order). Canonical
/// here (the gateway validates + advertises them); agentd's `sensor_config` references
/// this same list so there's one source of truth. `standard` = non-smoker / clean-air
/// default; the rest raise the alert floor for that environment's normal baseline.
pub const SENSOR_PROFILES: [&str; 4] = ["standard", "smoker", "kitchen", "workshop"];

/// GET /api/sensors/config — the active sensor-alert sensitivity profile + the
/// selectable list (drives the Settings/Sensor selector).
async fn sensor_config_get_handler(State(state): State<GatewayState>) -> impl IntoResponse {
    let profile = state.sensor_profile.read().map(|p| p.clone()).unwrap_or_else(|_| "standard".into());
    Json(serde_json::json!({ "profile": profile, "available": SENSOR_PROFILES }))
}

/// POST /api/sensors/config — set the sensitivity profile `{profile: "standard"|"smoker"
/// |"kitchen"|"workshop"}`. Updates the shared value (the agentd alert loop reads it per
/// reading, so it's live) and persists it (format matches `sensor_config::load_profile`).
/// A non-standard profile raises IAQ/thermal thresholds above that environment's baseline
/// so routine activity doesn't autonomously alert (a sustained real fire still does).
/// An unknown profile falls back to "standard".
async fn sensor_config_post_handler(
    State(state): State<GatewayState>,
    Json(body): Json<serde_json::Value>,
) -> impl IntoResponse {
    let req = body["profile"].as_str().unwrap_or("standard");
    let profile = if SENSOR_PROFILES.contains(&req) { req } else { "standard" };
    if let Ok(mut p) = state.sensor_profile.write() { *p = profile.to_string(); }
    let path = &state.sensor_config_path;
    if let Some(parent) = path.parent() { let _ = std::fs::create_dir_all(parent); }
    let _ = std::fs::write(path, serde_json::json!({ "profile": profile }).to_string());
    Json(serde_json::json!({ "ok": true, "profile": profile }))
}

/// Confine a peer-supplied destination to THIS node's workspace. Rejects `..` and
/// absolute paths; the result is `<workspace>/<dest>` (a relative subpath under the
/// canonical workspace root). Parents are created on write. Mirrors the FS-confine
/// rule (workspace-only for writes); the caller is already a token-authenticated peer.
fn confine_mesh_dest(dest: &str) -> Result<(apexos_confine::Beneath, std::path::PathBuf), String> {
    let p = std::path::Path::new(dest);
    if p.components().any(|c| c == std::path::Component::ParentDir) {
        return Err("path traversal (..) is not allowed".to_string());
    }
    if p.is_absolute() {
        return Err("dest must be workspace-relative".to_string());
    }
    if p.as_os_str().is_empty() {
        return Err("empty dest".to_string());
    }
    let root = workspace_beneath()?;
    let rel = workspace_rel(dest)?;
    Ok((root, rel))
}

/// POST /api/mesh/file — receive a file from a mesh peer (token-gated) and write it
/// into THIS node's workspace. The raw file bytes are the request body (binary-safe,
/// no base64); the destination relative path rides in the `x-dest` header. Confined
/// to the workspace (rejects `..`); parents auto-created. Ends the agent↔agent
/// "courier" problem — the sender is `mesh_file_send` (supervisor virtual tool).
async fn mesh_file_handler(
    headers: axum::http::HeaderMap,
    body:    axum::body::Bytes,
) -> impl IntoResponse {
    let dest = headers.get("x-dest").and_then(|v| v.to_str().ok()).unwrap_or("").trim().to_string();
    if dest.is_empty() {
        return Json(serde_json::json!({ "ok": false, "error": "missing x-dest header" }));
    }
    if body.is_empty() {
        return Json(serde_json::json!({ "ok": false, "error": "empty body" }));
    }
    let (root, target) = match confine_mesh_dest(&dest) {
        Ok(p)  => p,
        Err(e) => return Json(serde_json::json!({ "ok": false, "error": format!("dest: {e}") })),
    };
    if let Some(parent) = target.parent() {
        if !parent.as_os_str().is_empty() && parent != std::path::Path::new(".") {
            if let Err(e) = root.mkdir_all(parent) {
                return Json(serde_json::json!({ "ok": false, "error": format!("mkdir: {e}") }));
            }
        }
    }
    match root.write(&target, &body, false) {
        Ok(_)  => Json(serde_json::json!({ "ok": true, "path": dest, "bytes": body.len() })),
        Err(e) => Json(serde_json::json!({ "ok": false, "error": format!("write: {e}") })),
    }
}

/// GET /api/ping — the lean liveness probe (ApexNET §6.2). ~40 bytes; the
/// answer IS the signal. Replaces the beacon's old habit of pulling the
/// multi-KB /api/capabilities body every 30 s per peer and discarding it.
async fn ping_handler(State(state): State<GatewayState>) -> impl IntoResponse {
    let uptime_s = std::fs::read_to_string("/proc/uptime")
        .ok()
        .and_then(|s| s.split_whitespace().next().and_then(|f| f.parse::<f64>().ok()))
        .map(|f| f as u64)
        .unwrap_or(0);
    Json(serde_json::json!({ "node_id": *state.node_id, "uptime_s": uptime_s }))
}

/// GET /api/connectivity — what tier this node believes it is in, and why.
///
/// The honesty layer had no surface: the state gated tools and coloured the
/// ambient line, but nothing outside the process could see it, which makes a
/// chaos drill blind and a "why did my tool vanish" question unanswerable.
///
/// Ungated like `/api/ping`: it reveals strictly less than the capabilities
/// endpoint already does, and a peer deciding how to reach us needs it before
/// it holds a token.
async fn connectivity_handler(State(state): State<GatewayState>) -> impl IntoResponse {
    use apexos_core::mesh_router::{MeshTransport, TransportHealth};
    let tier = apexos_core::connectivity::current();
    let ble = mesh_link::BleGossipTransport::new(state.mesh_link.clone());
    let health = |h: TransportHealth| match h {
        TransportHealth::Up => "up",
        TransportHealth::Flaky => "flaky",
        TransportHealth::Down => "down",
    };
    Json(serde_json::json!({
        "state": tier.as_str(),
        "transports": [
            {
                "id": ble.id().as_str(),
                "health": health(ble.health()),
                "mtu": ble.mtu(),
            }
        ],
        "mesh_link": state.mesh_link.stats(),
    }))
}

/// POST /api/mesh/gossip — hand the radio a message for another node.
///
/// Admin / owner only (SA-11): this path makes the brainstem seal and
/// transmit under the colony identity. Mesh-peer tokens are intentionally
/// not accepted — that would recreate the signing oracle for every pair.
///
/// The frame goes down the wired link **unsealed** (charter §5) and the
/// brainstem takes it from there: a packet addressed to someone else is
/// queued in its flash outbox, sealed with a fresh counter, and delivered
/// when that peer is actually on the air (P4d). So this returns "handed to
/// the radio", not "delivered" — the two are days apart when the peer is a
/// node someone has to walk to.
async fn mesh_gossip_handler(
    State(state): State<GatewayState>,
    Json(body): Json<serde_json::Value>,
) -> impl IntoResponse {
    use apexos_core::mesh_router::{MeshTransport, SendError};
    use apexos_mesh_proto::{MeshClass, Payload, PlainPacket, DEFAULT_HOP_LIMIT, WIRE_VERSION};
    use crate::mesh_link::GossipRefuse;

    let Some(target) = body.get("target").and_then(|v| v.as_u64()) else {
        return (StatusCode::BAD_REQUEST, Json(serde_json::json!({
            "error": "target (node id) is required"
        }))).into_response();
    };
    let text = body.get("text").and_then(|v| v.as_str()).unwrap_or("");
    if let Err(why) = state.mesh_link.admit_gossip(target, text) {
        let status = match why {
            GossipRefuse::RateLimited | GossipRefuse::QueueFull => StatusCode::TOO_MANY_REQUESTS,
            _ => StatusCode::BAD_REQUEST,
        };
        return (status, Json(serde_json::json!({ "error": why.error() }))).into_response();
    }

    let packet = PlainPacket {
        target: target as u16,
        hop_limit: DEFAULT_HOP_LIMIT,
        flags: 0,
        payload: Payload::A2A { body: text.as_bytes().to_vec() },
    };
    let Ok(ct) = postcard::to_allocvec(&packet) else {
        return (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({
            "error": "encoding failed"
        }))).into_response();
    };
    let frame = apexos_mesh_proto::MeshFrame {
        ver: WIRE_VERSION,
        class: MeshClass::Gossip,
        sender: 0,
        ctr: 1,
        ct,
    };

    let ble = mesh_link::BleGossipTransport::new(state.mesh_link.clone());
    match ble.send(&frame).await {
        Ok(receipt) => Json(serde_json::json!({
            "handed_to": receipt.via.as_str(),
            "bytes": receipt.bytes,
            "note": "queued by the brainstem; delivery happens when the peer is on the air",
        })).into_response(),
        // No bridge connected is not a server error — it is the honest state
        // of a node with no radio attached, and the caller should queue or
        // say so rather than retry.
        Err(SendError::Unavailable) => (StatusCode::SERVICE_UNAVAILABLE, Json(serde_json::json!({
            "error": "no radio lane — is apexos-mesh-bridge running?",
        }))).into_response(),
        Err(SendError::Failed(e)) => (StatusCode::BAD_GATEWAY, Json(serde_json::json!({
            "error": e,
        }))).into_response(),
    }
}

/// Shared kill switch for the courier's proactive session-0 notices (charter
/// §6.4 — connectivity/courier truth is an environmental fact). Default ON.
fn apexnet_notify() -> bool {
    std::env::var("APEXNET_NOTIFY_AGENT")
        .map(|v| { let v = v.to_lowercase(); v != "0" && v != "false" && v != "off" })
        .unwrap_or(true)
}

/// POST /api/courier/manifest — Tier-1 courier-ledger gossip (ApexNET P2,
/// `docs/apexnet.md` §7 step 1): a peer announces "stick S carrying root R
/// for node D departed me". `from` must name a registered peer (the mesh
/// pattern — bearer token proves trust, `from` names which peer). The heard
/// ledger enables the plug notification's announced-vs-unannounced diff; if
/// the cargo is for THIS node, the agent hears about it before the human
/// arrives. Radio tiers replace this POST with the ~56 B `CourierManifest`
/// payload in P6 — same semantics.
async fn courier_manifest_handler(
    State(state): State<GatewayState>,
    Json(body):   Json<serde_json::Value>,
) -> impl IntoResponse {
    let from = match body["from"].as_str().map(str::trim).filter(|s| !s.is_empty()) {
        Some(f) if state.peer_registry.read().await.contains(f) => f.to_string(),
        Some(f) => return Json(serde_json::json!({
            "ok": false, "error": format!("'{f}' is not a registered peer on this node")
        })),
        None => return Json(serde_json::json!({ "ok": false, "error": "missing 'from'" })),
    };
    let (stick, root) = (body["stick"].as_str().unwrap_or("").to_string(),
                         body["root"].as_str().unwrap_or("").to_string());
    if stick.is_empty() || root.is_empty() {
        return Json(serde_json::json!({ "ok": false, "error": "missing stick/root" }));
    }
    let heard = apexos_plugins::courier::HeardManifest {
        stick: stick.clone(),
        root,
        origin: body["origin"].as_str().unwrap_or(&from).to_string(),
        dest: body["dest"].as_str().unwrap_or("").to_string(),
        len: body["len"].as_u64().unwrap_or(0),
        epoch: body["epoch"].as_u64().unwrap_or(0) as u32,
        name: body["name"].as_str().unwrap_or("").to_string(),
        heard_at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        shipment_id: body["shipment_id"].as_str().unwrap_or("").to_string(),
    };
    let for_me = heard.dest == *state.node_id;
    let (origin, name, len) = (heard.origin.clone(), heard.name.clone(), heard.len);
    let log_dir = apexos_plugins::courier::log_dir_env();
    let news = tokio::task::spawn_blocking(move || {
        apexos_plugins::courier::ledger_hear_manifest(&log_dir, heard)
    }).await.unwrap_or(false);
    if news && for_me && apexnet_notify() {
        let what = if name.is_empty() { "cargo".to_string() } else { format!("**{name}**") };
        let text = format!(
            "📦 Courier gossip from the mesh: stick `{stick}` departed **{origin}** carrying \
             {what} ({len} bytes) addressed to this node. It arrives when a human plugs the \
             stick in — it will be blake3-verified and receipted automatically; nothing to do \
             until then."
        );
        state.bus.emit(Event::UserPrompt { session: SessionId(0), text, images: vec![] }).await;
    }
    Json(serde_json::json!({ "ok": true, "news": news }))
}

/// POST /api/courier/receipt — the loop-closing half (§7 step 4): the
/// destination gossips "stick S's root R ingested (or refused)" back toward
/// the origin, which learns of delivery at network speed instead of waiting
/// for the stick to walk home. Marks the matching outbox entry delivered.
async fn courier_receipt_handler(
    State(state): State<GatewayState>,
    Json(body):   Json<serde_json::Value>,
) -> impl IntoResponse {
    let from = match body["from"].as_str().map(str::trim).filter(|s| !s.is_empty()) {
        Some(f) if state.peer_registry.read().await.contains(f) => f.to_string(),
        Some(f) => return Json(serde_json::json!({
            "ok": false, "error": format!("'{f}' is not a registered peer on this node")
        })),
        None => return Json(serde_json::json!({ "ok": false, "error": "missing 'from'" })),
    };
    let heard = apexos_plugins::courier::HeardReceipt {
        stick: body["stick"].as_str().unwrap_or("").to_string(),
        root: body["root"].as_str().unwrap_or("").to_string(),
        node: body["node"].as_str().unwrap_or(&from).to_string(),
        accepted: body["accepted"].as_bool().unwrap_or(false),
        heard_at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        shipment_id: body["shipment_id"].as_str().unwrap_or("").to_string(),
    };
    if heard.stick.is_empty() || heard.root.is_empty() {
        return Json(serde_json::json!({ "ok": false, "error": "missing stick/root" }));
    }
    let (node, accepted) = (heard.node.clone(), heard.accepted);
    let log_dir = apexos_plugins::courier::log_dir_env();
    let (news, delivered) = tokio::task::spawn_blocking(move || {
        apexos_plugins::courier::ledger_hear_receipt(&log_dir, heard)
    }).await.unwrap_or((false, None));
    if news && apexnet_notify() {
        if let Some(name) = &delivered {
            let text = if accepted {
                format!("🧾 Courier receipt: **{name}** was delivered to **{node}** and verified — \
                         the sneakernet loop closed.")
            } else {
                format!("⚠️ Courier receipt: **{node}** REFUSED **{name}** (verification failed \
                         in transit). The outbox still holds it — re-queue onto the next stick.")
            };
            state.bus.emit(Event::UserPrompt { session: SessionId(0), text, images: vec![] }).await;
        }
    }
    Json(serde_json::json!({ "ok": true, "news": news, "matched_outbox": delivered.is_some() }))
}

/// GET /api/courier/status — the courier lane's state for UI/tools.
async fn courier_status_handler(State(state): State<GatewayState>) -> impl IntoResponse {
    let log_dir = apexos_plugins::courier::log_dir_env();
    let psk_present = apexos_plugins::courier::load_psk_env().is_some();
    let node_id = state.node_id.to_string();
    let mut v = tokio::task::spawn_blocking(move || {
        apexos_plugins::courier::status_json(&log_dir, psk_present, &node_id)
    }).await.unwrap_or_else(|e| serde_json::json!({ "error": format!("join: {e}") }));
    v["mounted_sticks"] = serde_json::json!(apexos_plugins::courier::mounted_sticks());
    Json(v)
}

/// POST /api/mesh/memory — receive a memory from a mesh peer (token-gated) and
/// import it into THIS node's Cerebro (colony-federation Slice 1). `from` must
/// name a registered peer (the bearer token proves trust; `from` names which
/// peer). Validation + the provenance stamp (`colony` / `from:<node>` /
/// `origin:<id>` tags — forged provenance stripped) are the pure
/// `mesh::federated_remember_args`; the import runs in the agentd worker owning
/// the Cerebro ToolProxy. On success a global `MeshMemoryShared` event tells
/// every client knowledge landed. The sender is `mesh_memory_send` (supervisor
/// virtual tool).
async fn mesh_memory_handler(
    State(state): State<GatewayState>,
    Json(body):   Json<serde_json::Value>,
) -> impl IntoResponse {
    let from = match body["from"].as_str().map(str::trim).filter(|s| !s.is_empty()) {
        Some(f) if state.peer_registry.read().await.contains(f) => f.to_string(),
        Some(f) => {
            return Json(serde_json::json!({
                "ok": false, "error": format!("'{f}' is not a registered peer on this node")
            }))
        }
        None => return Json(serde_json::json!({ "ok": false, "error": "missing 'from'" })),
    };

    // Import into the receiving node's own agent space, receiver-stamped private.
    let agent_id = apexos_core::node_agent_id();
    let (args, preview) = match mesh::federated_remember_args(&from, &agent_id, &body) {
        Ok(v)  => v,
        Err(e) => return Json(serde_json::json!({ "ok": false, "error": e })),
    };

    // Origin dedup (Slice 4): a re-send of the same origin memory from the same
    // peer must not duplicate — the provenance tags stamped on every import are
    // the natural key, and `find_by_tags` is the exact lookup (recall would be
    // fuzzy under embeddings). Fail-open: if the probe errors, import anyway
    // (a duplicate is recoverable; a lost memory isn't).
    if let Some(origin) = body["origin_memory_id"].as_str().filter(|s| !s.trim().is_empty()) {
        let probe = serde_json::json!({
            "tags":     [format!("from:{from}"), format!("origin:{}", origin.trim())],
            "limit":    1,
            "agent_id": agent_id,
        });
        let (ptx, prx) = tokio::sync::oneshot::channel();
        let preq = MeshMemoryReq { tool: "find_by_tags".into(), args: probe, reply: ptx };
        if state.mesh_memory_tx.send(preq).await.is_ok() {
            if let Ok(Ok(found)) = prx.await {
                if let Some(existing) = found.as_array().and_then(|a| a.first()) {
                    let memory_id = existing["id"].as_str().unwrap_or("").to_string();
                    fed_stats_record(&state, &from, |s| s.duplicates += 1);
                    return Json(serde_json::json!({
                        "ok": true, "memory_id": memory_id, "from": from, "duplicate": true,
                    }));
                }
            }
        }
    }

    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    let req = MeshMemoryReq { tool: "remember".into(), args, reply: reply_tx };
    if state.mesh_memory_tx.send(req).await.is_err() {
        return Json(serde_json::json!({ "ok": false, "error": "memory import worker unavailable" }));
    }
    match reply_rx.await {
        Ok(Ok(stored)) => {
            let memory_id = stored["id"].as_str().unwrap_or("").to_string();
            fed_stats_record(&state, &from, |s| s.memories_received += 1);
            state.bus.emit(Event::MeshMemoryShared {
                from_node: from.clone(),
                memory_id: memory_id.clone(),
                preview,
            }).await;
            Json(serde_json::json!({ "ok": true, "memory_id": memory_id, "from": from }))
        }
        Ok(Err(e)) => Json(serde_json::json!({ "ok": false, "error": e })),
        Err(_)     => Json(serde_json::json!({ "ok": false, "error": "import reply dropped" })),
    }
}

/// POST /api/mesh/recall — answer a mesh peer's federated recall (token-gated,
/// colony-federation Slice 2). `from` must name a registered peer. The query
/// runs against THIS node's Cerebro restricted to **`Visibility::Shared`**
/// (`recall{visibility:"shared"}` → `VisibilityScope::shared_only()`), so a
/// private memory never crosses the wire — publishing (`share_memory`) is what
/// makes knowledge colony-queryable. Hits are BOUNDED (snippet ≤300 chars ·
/// type · tags · salience · score — never full-store dumps); the caller is
/// `mesh_recall` (supervisor virtual tool).
async fn mesh_recall_handler(
    State(state): State<GatewayState>,
    Json(body):   Json<serde_json::Value>,
) -> impl IntoResponse {
    let from = match body["from"].as_str().map(str::trim).filter(|s| !s.is_empty()) {
        Some(f) if state.peer_registry.read().await.contains(f) => f.to_string(),
        Some(f) => {
            return Json(serde_json::json!({
                "ok": false, "error": format!("'{f}' is not a registered peer on this node")
            }))
        }
        None => return Json(serde_json::json!({ "ok": false, "error": "missing 'from'" })),
    };
    let query = match body["query"].as_str().map(str::trim).filter(|s| !s.is_empty()) {
        Some(q) => q.to_string(),
        None    => return Json(serde_json::json!({ "ok": false, "error": "missing 'query'" })),
    };
    let limit = body["limit"].as_u64().unwrap_or(5).clamp(1, 10) as usize;

    let args = serde_json::json!({
        "query":      query,
        "top_k":      limit,
        "visibility": "shared",   // the federation scope — private never matches
    });
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    let req = MeshMemoryReq { tool: "recall".into(), args, reply: reply_tx };
    if state.mesh_memory_tx.send(req).await.is_err() {
        return Json(serde_json::json!({ "ok": false, "error": "recall worker unavailable" }));
    }
    match reply_rx.await {
        Ok(Ok(results)) => {
            let hits = mesh::federated_recall_hits(&results, limit);
            fed_stats_record(&state, &from, |s| {
                s.recall_served += 1;
                s.recall_hits   += hits.len() as u64;
            });
            Json(serde_json::json!({
                "ok": true, "node": state.node_id.as_str(), "count": hits.len(), "hits": hits,
            }))
        }
        Ok(Err(e)) => Json(serde_json::json!({ "ok": false, "error": e })),
        Err(_)     => Json(serde_json::json!({ "ok": false, "error": "recall reply dropped" })),
    }
}

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
    let fed = state.fed_stats.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let mut peers: Vec<serde_json::Value> = Vec::with_capacity(registry.peers.len());
    for p in &registry.peers {
        // Fold in the beacon's active-liveness (alive/dark + seconds-since-seen).
        let (live, last_seen_secs) = beacon::peer_liveness(&state.liveness, &p.node_id).await;
        // Fold in the inbound-federation counters (principle 6, receiver-side).
        let f = fed.get(p.node_id.as_str()).cloned().unwrap_or_default();
        peers.push(serde_json::json!({
            "node_id":        p.node_id,
            "ws_url":         p.ws_url,
            "role":           p.role.to_string(),
            "status":         p.status,
            "has_token":      p.token.is_some(),
            "live":           live,
            "last_seen_secs": last_seen_secs,
            "federation": {
                "memories_received": f.memories_received,
                "duplicates":        f.duplicates,
                "recall_served":     f.recall_served,
                "recall_hits":       f.recall_hits,
                "last_ts":           f.last_ts,
            },
        }));
    }
    Json(serde_json::json!({ "peers": peers }))
}

/// POST /api/mesh/peers — add or update a peer.
/// Body: { node_id, ws_url, role?, token? }  (token = outbound mesh credential,
/// never the node AGENTD_TOKEN).
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
    if token_in.as_deref().is_some_and(|t| mesh::is_node_admin_token(t, state.api_token.as_str())) {
        return Json(serde_json::json!({ "ok": false, "error": "refusing to store the node admin token as a mesh credential" }));
    }

    let result = {
        let mut registry = state.peer_registry.write().await;
        // Preserve an existing token when the caller didn't supply one (e.g. a
        // ws_url/status-only re-add from REFRESH shouldn't wipe the a2a credential).
        let token = token_in.or_else(|| registry.peers.iter()
            .find(|p| p.node_id == node_id).and_then(|p| p.token.clone()));
        let inbound_token = registry.peers.iter()
            .find(|p| p.node_id == node_id).and_then(|p| p.inbound_token.clone());
        let record = PeerRecord { node_id: node_id.clone(), ws_url: ws_url.clone(), role, status: "online".into(), token, inbound_token };
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

// ── Identity API (multi-agent boot flow) ────────────────────────────────────────
// docs/agent-identity.md slice 3c. Token-gated CRUD over the identity registry the
// boot UI (3d) drives; PIN verify is guarded by a per-user guess lockout. Writes
// persist to identities.toml (best-effort; see install.sh ownership).

/// Where new agents' soul files live: `<dir of identities.toml>/souls`.
fn souls_dir() -> std::path::PathBuf {
    apexos_core::Identities::default_path()
        .parent()
        .map(|p| p.join("souls"))
        .unwrap_or_else(|| std::path::PathBuf::from("/etc/agentd/souls"))
}

/// Reduce a display name to an id slug; `upper` for agent ids (APEX/FORGE style),
/// lowercase for user ids. Non-alphanumerics collapse to `_`; empty → "x".
fn slug(name: &str, upper: bool) -> String {
    let mut s: String = name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect::<String>()
        .trim_matches('_')
        .to_string();
    s = if upper { s.to_uppercase() } else { s.to_lowercase() };
    if s.is_empty() { "x".to_string() } else { s }
}

/// Seed content for a freshly created agent's soul.
fn agent_soul_template(name: &str) -> String {
    format!(
        "# {name}\n\nYou are {name}, an agent on this ApexOS node. This file is your \
soul — your identity and values, yours to grow over time.\n\n## Identity\n\n\
(Newly created. Evolve this as you learn who you are.)\n"
    )
}

/// GET /api/identities — users (PIN redacted to `has_pin`) + agents.
async fn identities_list_handler(State(state): State<GatewayState>) -> impl IntoResponse {
    let ids = state.identities.read().await;
    let users: Vec<_> = ids.users.iter().map(|u| serde_json::json!({
        "id": u.id, "name": u.name, "has_pin": u.has_pin(),
        "default_agent": u.default_agent, "default_skin": u.default_skin,
    })).collect();
    let agents: Vec<_> = ids.agents.iter().map(|a| serde_json::json!({
        "id": a.id, "name": a.name, "owner": a.owner, "default_skin": a.default_skin,
    })).collect();
    Json(serde_json::json!({ "users": users, "agents": agents }))
}

#[derive(Deserialize)]
struct CreateUserBody {
    name: String,
    pin: Option<String>,
    default_agent: Option<String>,
    default_skin: Option<String>,
}

/// POST /api/identities/user — create a profile (optional PIN).
async fn identities_create_user_handler(
    State(state): State<GatewayState>,
    Json(body):   Json<CreateUserBody>,
) -> impl IntoResponse {
    if body.name.trim().is_empty() {
        return (StatusCode::BAD_REQUEST, Json(serde_json::json!({ "error": "name required" }))).into_response();
    }
    let id = slug(&body.name, false);
    let mut ids = state.identities.write().await;
    if ids.user(&id).is_some() {
        return (StatusCode::CONFLICT, Json(serde_json::json!({ "error": format!("user '{id}' exists") }))).into_response();
    }
    let mut u = apexos_core::User {
        id: id.clone(),
        name: body.name,
        default_agent: body.default_agent,
        default_skin: body.default_skin,
        ..Default::default()
    };
    if let Some(pin) = body.pin.filter(|p| !p.trim().is_empty()) {
        u.set_pin(&pin);
    }
    let has_pin = u.has_pin();
    let mut next = ids.clone();
    next.users.push(u);
    if let Err(e) = ids.commit(&apexos_core::Identities::default_path(), next) {
        eprintln!("[identity] persist failed: {e}");
        return (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({
            "error": format!("persist failed: {e}"),
        }))).into_response();
    }
    (StatusCode::OK, Json(serde_json::json!({ "id": id, "has_pin": has_pin }))).into_response()
}

#[derive(Deserialize)]
struct CreateAgentBody {
    name: String,
    owner: String,
    default_skin: Option<String>,
}

/// POST /api/identities/agent — create an agent (own Cerebro space + soul file).
async fn identities_create_agent_handler(
    State(state): State<GatewayState>,
    Json(body):   Json<CreateAgentBody>,
) -> impl IntoResponse {
    if body.name.trim().is_empty() {
        return (StatusCode::BAD_REQUEST, Json(serde_json::json!({ "error": "name required" }))).into_response();
    }
    let id = slug(&body.name, true);
    let mut ids = state.identities.write().await;
    if ids.user(&body.owner).is_none() {
        return (StatusCode::BAD_REQUEST, Json(serde_json::json!({ "error": format!("unknown owner '{}'", body.owner) }))).into_response();
    }
    if ids.agent(&id).is_some() {
        return (StatusCode::CONFLICT, Json(serde_json::json!({ "error": format!("agent '{id}' exists") }))).into_response();
    }
    // Seed the agent's soul file (best-effort — dir may be root-owned pre-install.sh).
    let dir = souls_dir();
    let soul_file = dir.join(format!("{id}.md"));
    let _ = std::fs::create_dir_all(&dir);
    if let Err(e) = std::fs::write(&soul_file, agent_soul_template(&body.name)) {
        eprintln!("[identity] could not seed soul {}: {e}", soul_file.display());
    }
    // Provision the agent's per-agent ("agent-locked") workspace, the same root
    // confine() resolves to (<AGENTD_WORKSPACE>/workspaces/<id>). Best-effort —
    // confine() also create_dir_all's it, so a skip here self-heals on first use.
    let agent_ws = apexos_core::agent_workspace_root(&id);
    if let Err(e) = std::fs::create_dir_all(&agent_ws) {
        eprintln!("[identity] could not provision workspace {}: {e}", agent_ws.display());
    }
    let mut next = ids.clone();
    next.agents.push(apexos_core::AgentRecord {
        id: id.clone(),
        name: body.name,
        owner: body.owner,
        soul_file: soul_file.to_string_lossy().into_owned(),
        default_skin: body.default_skin,
    });
    if let Err(e) = ids.commit(&apexos_core::Identities::default_path(), next) {
        eprintln!("[identity] persist failed: {e}");
        return (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({
            "error": format!("persist failed: {e}"),
        }))).into_response();
    }
    (StatusCode::OK, Json(serde_json::json!({ "id": id, "soul_file": soul_file.to_string_lossy() }))).into_response()
}

#[derive(Deserialize)]
struct VerifyPinBody {
    user_id: String,
    pin: String,
}

/// POST /api/identities/verify — check a profile's PIN, guarded by a guess lockout.
async fn identities_verify_pin_handler(
    State(state): State<GatewayState>,
    Json(body):   Json<VerifyPinBody>,
) -> impl IntoResponse {
    let now = std::time::Instant::now();
    // Locked? Refuse without even checking (and without revealing validity).
    {
        let lk = state.pin_lockouts.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(remaining) = lk.get(&body.user_id).and_then(|l| l.locked_for(now)) {
            return (StatusCode::TOO_MANY_REQUESTS,
                Json(serde_json::json!({ "ok": false, "locked": true, "retry_after_secs": remaining }))
            ).into_response();
        }
    }
    let ok = state.identities.read().await
        .user(&body.user_id)
        .map(|u| u.verify_pin(&body.pin))
        .unwrap_or(false);   // unknown user → fail (also counts toward lockout)
    let locked = {
        let mut lk = state.pin_lockouts.lock().unwrap_or_else(|e| e.into_inner());
        let entry = lk.entry(body.user_id).or_default();
        entry.record(ok, now);
        entry.locked_for(now)
    };
    if ok {
        (StatusCode::OK, Json(serde_json::json!({ "ok": true }))).into_response()
    } else {
        (StatusCode::OK, Json(serde_json::json!({
            "ok": false,
            "locked": locked.is_some(),
            "retry_after_secs": locked,
        }))).into_response()
    }
}

// ── Human login → session token (agent-identity.md slice 3e) ────────────────────

#[derive(serde::Deserialize)]
struct LoginBody {
    user_id: String,
    #[serde(default)]
    pin: String,
}

/// POST /api/auth/login — profile (+ PIN) → a minted session token.
///
/// UNGATED (authenticated by the PIN itself). LAN login is closed until the
/// owner profile has a PIN (finding 2). After claim, a PIN-less profile may
/// one-tap only on loopback. Owner one-tap is gone — use `/api/auth/setup`.
async fn auth_login_handler(
    State(state): State<GatewayState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Json(body):   Json<LoginBody>,
) -> impl IntoResponse {
    let now = std::time::Instant::now();
    let loopback = session_auth::is_loopback_addr(&addr);
    {
        let lk = state.pin_lockouts.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(remaining) = lk.get(&body.user_id).and_then(|l| l.locked_for(now)) {
            return (StatusCode::TOO_MANY_REQUESTS, Json(serde_json::json!({
                "ok": false, "locked": true, "retry_after_secs": remaining,
            }))).into_response();
        }
    }
    let (exists, ok, agent_id, has_pin, claimed) = {
        let ids = state.identities.read().await;
        let claimed = ids.owner_claimed();
        match ids.user(&body.user_id) {
            Some(u) => (true, u.verify_pin(&body.pin), u.default_agent.clone().unwrap_or_default(), u.has_pin(), claimed),
            None    => (false, false, String::new(), false, claimed),
        }
    };
    if exists && !session_auth::login_permitted(claimed, loopback, has_pin) {
        return (StatusCode::FORBIDDEN, Json(serde_json::json!({
            "ok": false,
            "setup_required": !claimed,
            "error": if claimed { "pin_required" } else { "owner_setup_required" },
        }))).into_response();
    }
    let locked = {
        let mut lk = state.pin_lockouts.lock().unwrap_or_else(|e| e.into_inner());
        let entry = lk.entry(body.user_id.clone()).or_default();
        entry.record(ok && exists, now);
        entry.locked_for(now)
    };
    if !exists || !ok {
        return (StatusCode::OK, Json(serde_json::json!({
            "ok": false, "locked": locked.is_some(), "retry_after_secs": locked,
        }))).into_response();
    }
    let auth = SessionAuth::new(body.user_id.clone(), agent_id.clone());
    let role = auth.role.as_str();
    let token = session_auth::gen_session_token();
    {
        let mut s = state.sessions.lock().unwrap_or_else(|e| e.into_inner());
        s.sweep(now);
        s.insert(
            token.clone(),
            auth,
            now,
            std::time::Duration::from_secs(session_auth::SESSION_TTL_SECS),
        );
    }
    (StatusCode::OK, Json(serde_json::json!({
        "ok": true,
        "token": token,
        "user_id": body.user_id,
        "agent_id": agent_id,
        "role": role,
        "expires_in": session_auth::SESSION_TTL_SECS,
    }))).into_response()
}

#[derive(serde::Deserialize)]
struct SetupBody {
    pin: String,
}

/// POST /api/auth/setup — claim the node by setting the owner PIN.
/// Allowed from loopback or with the admin token, and only while unclaimed.
async fn auth_setup_handler(
    State(state): State<GatewayState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: axum::http::HeaderMap,
    Json(body): Json<SetupBody>,
) -> impl IntoResponse {
    let loopback = session_auth::is_loopback_addr(&addr);
    let admin = request_is_admin_token(&state, &headers);
    let claimed = state.identities.read().await.owner_claimed();
    if !session_auth::setup_permitted(claimed, loopback, admin) {
        return (StatusCode::FORBIDDEN, Json(serde_json::json!({
            "ok": false,
            "setup_required": !claimed,
            "error": if claimed { "already_claimed" } else { "owner_setup_required" },
        }))).into_response();
    }
    if !apexos_core::valid_owner_pin(&body.pin) {
        return (StatusCode::BAD_REQUEST, Json(serde_json::json!({
            "ok": false, "error": "pin must be 4-8 digits",
        }))).into_response();
    }
    {
        let mut ids = state.identities.write().await;
        let mut next = ids.clone();
        next.seed_defaults("/etc/agentd/soul.md");
        match next.user_mut(apexos_core::DEFAULT_USER_ID) {
            Some(u) => u.set_pin(&body.pin),
            None => {
                return (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({
                    "ok": false, "error": "owner profile missing",
                }))).into_response();
            }
        }
        if let Err(e) = ids.commit(&apexos_core::Identities::default_path(), next) {
            eprintln!("[identity] persist owner PIN failed: {e}");
            return (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({
                "ok": false, "error": "persist failed",
            }))).into_response();
        }
    }
    (StatusCode::OK, Json(serde_json::json!({
        "ok": true, "user_id": apexos_core::DEFAULT_USER_ID,
    }))).into_response()
}

#[derive(serde::Deserialize)]
struct LogoutBody {
    token: String,
}

/// POST /api/auth/logout — revoke a session token. Gated (you must present a valid
/// token to reach it); idempotent — revoking an unknown/expired token is `ok:true`.
async fn auth_logout_handler(
    State(state): State<GatewayState>,
    Json(body):   Json<LogoutBody>,
) -> impl IntoResponse {
    let mut s = state.sessions.lock().unwrap_or_else(|e| e.into_inner());
    s.revoke(&body.token);
    Json(serde_json::json!({ "ok": true }))
}

/// GET /api/auth/profiles — the minimal login-tile data (id, name, has_pin) for each
/// profile. UNGATED: the login screen needs it *before* the client holds any token.
/// Deliberately minimal — no agents, no PIN hashes; the full registry stays behind
/// the token-gated `/api/identities`.
async fn auth_profiles_handler(
    State(state): State<GatewayState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: axum::http::HeaderMap,
) -> impl IntoResponse {
    let loopback = session_auth::is_loopback_addr(&addr);
    let admin = request_is_admin_token(&state, &headers);
    let ids = state.identities.read().await;
    let setup_required = !ids.owner_claimed();
    // Unclaimed + LAN + no admin token: do not leak profile names.
    if setup_required && !loopback && !admin {
        return Json(serde_json::json!({
            "users": [],
            "default_user": serde_json::Value::Null,
            "setup_required": true,
            "login_open": false,
        }));
    }
    let users: Vec<serde_json::Value> = ids.users.iter().map(|u| serde_json::json!({
        "id": u.id, "name": u.name, "has_pin": u.has_pin(),
    })).collect();
    Json(serde_json::json!({
        "users": users,
        "default_user": ids.default_user,
        "setup_required": setup_required,
        "login_open": true,
    }))
}

#[derive(serde::Deserialize)]
struct DefaultBody {
    /// Profile to auto-login on launch; an empty string clears the default.
    user_id: String,
}

/// POST /api/auth/default — set (or clear, with `""`) the device's default login
/// profile (slice 3e). Gated: you must already be authenticated to change it. The
/// login screen ("remember me") sets it; Settings clears it.
async fn auth_default_handler(
    State(state): State<GatewayState>,
    Json(body):   Json<DefaultBody>,
) -> impl IntoResponse {
    let mut ids = state.identities.write().await;
    let id = body.user_id.trim();
    let mut next = ids.clone();
    if id.is_empty() {
        next.default_user = None;
    } else if next.user(id).is_some() {
        next.default_user = Some(id.to_string());
    } else {
        return (StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "ok": false, "error": format!("no such profile '{id}'") }))).into_response();
    }
    if let Err(e) = ids.commit(&apexos_core::Identities::default_path(), next) {
        eprintln!("[identity] persist default_user failed: {e}");
        return (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({
            "ok": false, "error": format!("persist failed: {e}"),
        }))).into_response();
    }
    (StatusCode::OK, Json(serde_json::json!({ "ok": true, "default_user": ids.default_user }))).into_response()
}

/// GET /api/auth/me — who the caller is logged in as (slice 3e). `{user_id, name,
/// agent_id}` for a session-token client; `{user_id: null}` for the admin token /
/// token-less node (no human session). Lets Settings show "auto-login me" without
/// the client tracking its own id across the post-login re-exec.
async fn auth_me_handler(
    State(state):  State<GatewayState>,
    headers:       axum::http::HeaderMap,
) -> impl IntoResponse {
    match resolve_req_auth(&state, &headers) {
        Some(auth) => {
            let name = {
                let ids = state.identities.read().await;
                ids.user(&auth.user_id).map(|u| u.name.clone()).unwrap_or_default()
            };
            Json(serde_json::json!({
                "user_id": auth.user_id, "name": name, "agent_id": auth.agent_id,
                "role": auth.role.as_str(),
            }))
        }
        None => Json(serde_json::json!({ "user_id": serde_json::Value::Null })),
    }
}

// ── Mesh pairing — kiosk-friendly token exchange ────────────────────────────────

/// First local IPv4 (from `hostname -I`).
fn own_ipv4() -> Option<String> {
    let out = std::process::Command::new("hostname").arg("-I").output().ok()?;
    String::from_utf8(out.stdout).ok()?
        .split_whitespace()
        .find(|t| t.contains('.') && t.split('.').count() == 4)
        .map(|t| t.to_string())
}

/// This node's ws_url to advertise to a peer.
fn own_ws_url() -> String {
    format!("ws://{}:8787", own_ipv4().unwrap_or_else(|| "127.0.0.1".into()))
}

/// POST /api/mesh/pair/start — generate a fresh pairing code (this node's own UI).
async fn pair_start_handler(State(state): State<GatewayState>) -> impl IntoResponse {
    let code = mesh::gen_pair_code();
    {
        let mut p = state.pairing.lock().unwrap();
        *p = Some(mesh::Pairing {
            code:       code.clone(),
            expires_at: std::time::Instant::now() + std::time::Duration::from_secs(mesh::PAIR_TTL_SECS),
            attempts:   0,
        });
    }
    Json(serde_json::json!({ "ok": true, "code": code, "ttl_secs": mesh::PAIR_TTL_SECS }))
}

/// GET /api/mesh/pair/status — current code + remaining seconds (UI countdown).
async fn pair_status_handler(State(state): State<GatewayState>) -> impl IntoResponse {
    let p = state.pairing.lock().unwrap();
    match p.as_ref() {
        Some(pair) if pair.expires_at > std::time::Instant::now() => Json(serde_json::json!({
            "active":         true,
            "code":           pair.code,
            "remaining_secs": (pair.expires_at - std::time::Instant::now()).as_secs(),
        })),
        _ => Json(serde_json::json!({ "active": false })),
    }
}

/// POST /api/mesh/pair/claim — UNAUTHENTICATED, gated by the short-lived code.
/// Never accepts or returns AGENTD_TOKEN. Callbacks to the claimer's URL
/// (`/api/mesh/pair/confirm`) before disclosing a minted mesh credential.
async fn pair_claim_handler(
    State(state): State<GatewayState>,
    Json(body):   Json<serde_json::Value>,
) -> impl IntoResponse {
    let code     = body["code"].as_str().unwrap_or_default().to_string();
    let req_node = body["node_id"].as_str().unwrap_or_default().trim().to_string();
    let req_url  = body["ws_url"].as_str().unwrap_or_default().trim().to_string();
    let nonce    = body["nonce"].as_str().unwrap_or_default().trim().to_string();
    if code.is_empty() || req_node.is_empty() || req_url.is_empty() || nonce.is_empty() {
        return (StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "ok": false, "error": "missing fields" })));
    }
    let Some(peer_http) = mesh::mesh_http_base(&req_url) else {
        return (StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "ok": false, "error": "invalid ws_url" })));
    };
    let ok = {
        let mut p = state.pairing.lock().unwrap();
        match p.as_mut() {
            Some(pair) if pair.expires_at <= std::time::Instant::now() => { *p = None; false }
            Some(pair) if pair.code == code => { *p = None; true }
            Some(pair) => {
                pair.attempts += 1;
                if pair.attempts >= mesh::PAIR_MAX_ATTEMPTS { *p = None; }
                false
            }
            None => false,
        }
    };
    if !ok {
        return (StatusCode::FORBIDDEN,
                Json(serde_json::json!({ "ok": false, "error": "invalid or expired code" })));
    }
    // Prove the claimer answers at ws_url. They mint OUR outbound token there.
    // A caller-supplied `token` in the claim body is ignored (finding 6).
    let confirm = serde_json::json!({
        "node_id": state.node_id.as_str(),
        "ws_url":  own_ws_url(),
        "nonce":   nonce,
    });
    let resp = reqwest::Client::new()
        .post(format!("{peer_http}/api/mesh/pair/confirm"))
        .json(&confirm)
        .timeout(std::time::Duration::from_secs(8))
        .send().await;
    let outbound = match resp {
        Ok(r) if r.status().is_success() => {
            let v: serde_json::Value = r.json().await.unwrap_or_default();
            v["token"].as_str().unwrap_or("").to_string()
        }
        _ => String::new(),
    };
    if !mesh::is_mesh_token_shape(&outbound)
        || mesh::is_node_admin_token(&outbound, state.api_token.as_str())
    {
        return (StatusCode::BAD_GATEWAY,
                Json(serde_json::json!({ "ok": false, "error": "peer did not confirm with a mesh token" })));
    }
    let inbound = mesh::gen_mesh_token();
    {
        let mut registry = state.peer_registry.write().await;
        let _ = registry.add(PeerRecord {
            node_id: req_node, ws_url: req_url, role: PeerRole::Full,
            status: "online".into(),
            token: Some(outbound),
            inbound_token: Some(inbound.clone()),
        });
    }
    (StatusCode::OK, Json(serde_json::json!({
        "ok":      true,
        "node_id": state.node_id.as_str(),
        "ws_url":  own_ws_url(),
        "token":   inbound,
    })))
}

/// POST /api/mesh/pair/confirm — the claimer answers here to prove they own
/// the URL they advertised. Authenticated by the in-flight redeem nonce.
async fn pair_confirm_handler(
    State(state): State<GatewayState>,
    Json(body):   Json<serde_json::Value>,
) -> impl IntoResponse {
    let nonce = body["nonce"].as_str().unwrap_or_default().to_string();
    let peer_node = body["node_id"].as_str().unwrap_or_default().trim().to_string();
    let peer_url = body["ws_url"].as_str().unwrap_or_default().trim().to_string();
    if nonce.is_empty() || peer_node.is_empty() {
        return (StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "ok": false, "error": "missing fields" })));
    }
    let ok = {
        let mut f = state.redeem_flight.lock().unwrap();
        match f.as_ref() {
            Some(flight) if flight.expires_at > std::time::Instant::now() && flight.nonce == nonce => {
                *f = None;
                true
            }
            _ => false,
        }
    };
    if !ok {
        return (StatusCode::FORBIDDEN,
                Json(serde_json::json!({ "ok": false, "error": "no matching redeem" })));
    }
    let inbound = mesh::gen_mesh_token();
    if !peer_url.is_empty() {
        let mut registry = state.peer_registry.write().await;
        let outbound = registry.peers.iter()
            .find(|p| p.node_id == peer_node)
            .and_then(|p| p.token.clone());
        let _ = registry.add(PeerRecord {
            node_id: peer_node,
            ws_url: peer_url,
            role: PeerRole::Full,
            status: "online".into(),
            token: outbound,
            inbound_token: Some(inbound.clone()),
        });
    }
    (StatusCode::OK, Json(serde_json::json!({ "ok": true, "token": inbound })))
}

/// POST /api/mesh/pair/redeem — this node's UI hands us a discovered peer's ws_url +
/// the code shown on it. We claim with a nonce (never AGENTD_TOKEN); the peer
/// callbacks `/confirm` before either side discloses a minted mesh token.
async fn pair_redeem_handler(
    State(state): State<GatewayState>,
    Json(body):   Json<serde_json::Value>,
) -> impl IntoResponse {
    let peer_ws = body["ws_url"].as_str().unwrap_or_default().to_string();
    let code    = body["code"].as_str().unwrap_or_default().to_string();
    if peer_ws.is_empty() || code.is_empty() {
        return Json(serde_json::json!({ "ok": false, "error": "missing ws_url or code" }));
    }
    let Some(http_base) = mesh::mesh_http_base(&peer_ws) else {
        return Json(serde_json::json!({ "ok": false, "error": "invalid ws_url" }));
    };
    let self_ws = body["self_ws_url"].as_str()
        .map(str::trim).filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .unwrap_or_else(own_ws_url);
    if mesh::mesh_http_base(&self_ws).is_none() {
        return Json(serde_json::json!({ "ok": false, "error": "invalid self_ws_url" }));
    }
    let nonce = mesh::gen_mesh_token();
    {
        let mut f = state.redeem_flight.lock().unwrap();
        *f = Some(mesh::RedeemFlight {
            peer_http: http_base.clone(),
            nonce: nonce.clone(),
            expires_at: std::time::Instant::now() + std::time::Duration::from_secs(30),
        });
    }
    let claim = serde_json::json!({
        "code":    code,
        "node_id": state.node_id.as_str(),
        "ws_url":  self_ws,
        "nonce":   nonce,
    });
    let resp = reqwest::Client::new()
        .post(format!("{http_base}/api/mesh/pair/claim"))
        .json(&claim)
        .timeout(std::time::Duration::from_secs(15))
        .send().await;
    // Clear a leftover flight if claim never confirmed.
    {
        let mut f = state.redeem_flight.lock().unwrap();
        if f.as_ref().is_some_and(|fl| fl.nonce == nonce) {
            *f = None;
        }
    }
    match resp {
        Ok(r) if r.status().is_success() => {
            let v: serde_json::Value = r.json().await.unwrap_or_default();
            let node = v["node_id"].as_str().unwrap_or_default().to_string();
            let url  = v["ws_url"].as_str().unwrap_or(peer_ws.as_str()).to_string();
            let tok  = v["token"].as_str().unwrap_or("").to_string();
            if node.is_empty()
                || !mesh::is_mesh_token_shape(&tok)
                || mesh::is_node_admin_token(&tok, state.api_token.as_str())
            {
                return Json(serde_json::json!({ "ok": false, "error": "peer returned no mesh token" }));
            }
            {
                let mut registry = state.peer_registry.write().await;
                let inbound = registry.peers.iter()
                    .find(|p| p.node_id == node)
                    .and_then(|p| p.inbound_token.clone());
                let _ = registry.add(PeerRecord {
                    node_id: node.clone(), ws_url: url, role: PeerRole::Full,
                    status: "online".into(),
                    token: Some(tok),
                    inbound_token: inbound,
                });
            }
            Json(serde_json::json!({ "ok": true, "node_id": node }))
        }
        Ok(r)  => Json(serde_json::json!({ "ok": false, "error": format!("pairing rejected ({})", r.status().as_u16()) })),
        Err(e) => Json(serde_json::json!({ "ok": false, "error": e.to_string() })),
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

/// GET /api/workspace/texts — list text files under the workspace for the
/// Imagine app's "prompt from file" picker (twin of /api/workspace/images):
/// anything written into the workspace — agent notes, USB imports, uploads —
/// becomes generation fuel. Scans the root + text-bearing subdirs, newest
/// first, capped so the picker stays a picker.
async fn workspace_texts_handler() -> impl IntoResponse {
    let ws = std::env::var("AGENTD_WORKSPACE")
        .ok().filter(|s| !s.is_empty())
        .unwrap_or_else(|| "/var/lib/agentd/workspace".to_string());
    let ws_path = std::path::Path::new(&ws);
    let exts = ["txt", "md", "prompt"];
    let subdirs = ["", "notes", "prompts", "uploads", "docs", "imagine"];
    let mut texts: Vec<serde_json::Value> = Vec::new();
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
            let rel = p.strip_prefix(ws_path).map(|r| r.to_string_lossy().to_string())
                .unwrap_or_else(|_| abs.clone());
            let meta = entry.metadata().await.ok();
            let size = meta.as_ref().map(|m| m.len()).unwrap_or(0);
            let modified = meta.as_ref().and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs()).unwrap_or(0);
            texts.push(serde_json::json!({
                "path": rel,
                "name": p.file_name().and_then(|n| n.to_str()).unwrap_or(""),
                "size": size,
                "modified": modified,
            }));
        }
    }

    texts.sort_by(|a, b| b["modified"].as_u64().unwrap_or(0).cmp(&a["modified"].as_u64().unwrap_or(0)));
    texts.truncate(60);
    Json(serde_json::json!({ "texts": texts }))
}

/// GET /api/workspace/list?path=<rel> — browse the workspace tree for the Explorer
/// app. Returns the entries directly under <workspace>/<path>: directories first,
/// then files, alpha within each. Confined to the workspace. `path` is
/// workspace-relative; `abs` lets a co-located UI load image previews directly.
/// A valid exo-workspace filesystem label: `APEX-` + a sane single component. The
/// udev rule already gates on `APEX-*`; this re-validates before handing the label
/// to the (root) umount helper, so a crafted value can't widen the eject target.
fn valid_exo_label(label: &str) -> bool {
    label.starts_with("APEX-")
        && (6..=64).contains(&label.len())   // at least one char after "APEX-"
        && !label.contains("..")
        && label.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// Is `<workspace>/media/<label>` currently a mountpoint in /proc/mounts? This is the
/// authoritative success oracle for an eject — when it goes false, the stick is gone.
/// Shared shape with `mounted_exo_sticks` / the apexos-tools eject tool.
fn media_mount_present(label: &str) -> bool {
    let ws = std::env::var("AGENTD_WORKSPACE").ok().filter(|s| !s.is_empty())
        .unwrap_or_else(|| "/var/lib/agentd/workspace".to_string());
    let ws_canon = std::fs::canonicalize(&ws).unwrap_or_else(|_| std::path::PathBuf::from(&ws));
    let target = ws_canon.join("media").join(label);
    let target_s = target.to_string_lossy();
    std::fs::read_to_string("/proc/mounts").map(|m| {
        m.lines().any(|l| l.split_whitespace().nth(1) == Some(target_s.as_ref()))
    }).unwrap_or(false)
}

/// POST /api/media/eject {label} — safely unmount an exo-workspace stick (the UI ⏏
/// affordance + the agent `eject_media` tool both land here). agentd runs non-root with
/// NoNewPrivileges=true, so it CAN'T sudo/umount — instead it drops an APEX-<label>
/// request file into the (agentd-owned) eject dir, which fires the root drain service
/// (apexos-usb-eject.path → .service) that does the umount on its behalf. Success is
/// confirmed by polling /proc/mounts (the mountpoint disappears). The label is validated
/// here, again by the drain, and a third time by usb-umount (defence in depth).
async fn media_eject_handler(Json(body): Json<serde_json::Value>) -> impl IntoResponse {
    let label = body["label"].as_str().unwrap_or("").trim().to_string();
    if !valid_exo_label(&label) {
        return Json(serde_json::json!({ "ok": false, "error": "label must be APEX-<name> (letters, digits, . _ -)" }));
    }
    match request_eject(&label).await {
        Ok(()) => Json(serde_json::json!({ "ok": true, "label": label })),
        Err(e) => Json(serde_json::json!({ "ok": false, "error": e })),
    }
}

/// Drop an eject request for `label` and wait (≤8s) for the root drain to unmount it.
/// Returns Err with a human message if the stick is still mounted after the window
/// (the drain may have failed — its journal has the reason). Assumes `label` is already
/// `valid_exo_label`-checked.
async fn request_eject(label: &str) -> Result<(), String> {
    if !media_mount_present(label) {
        return Err(format!("{label} is not mounted"));
    }
    let dir = std::env::var("AGENTD_USB_EJECT_DIR")
        .unwrap_or_else(|_| "/var/lib/agentd/usb-eject".to_string());
    tokio::fs::create_dir_all(&dir).await.map_err(|e| format!("eject dir: {e}"))?;
    let req = std::path::Path::new(&dir).join(label);
    tokio::fs::write(&req, b"").await.map_err(|e| format!("drop eject request: {e}"))?;
    for _ in 0..16 {
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        if !media_mount_present(label) { return Ok(()); }
    }
    Err(format!("{label} still mounted after 8s — the eject service may have failed \
                 (check: journalctl -u apexos-usb-eject)"))
}

/// POST /api/media/plugged {label} — the `usb-mount` helper calls this (loopback +
/// token) right after own-mounting an `APEX-*` stick, so the agent learns the stick
/// landed *the moment it's plugged* rather than waiting for its next turn's embodiment
/// block. Mirrors the mesh-beacon notify: injects a root-session prompt so APEX can
/// greet the stick proactively, unless `AGENTD_USB_NOTIFY_AGENT=0`.
async fn media_plugged_handler(
    State(state): State<GatewayState>,
    Json(body):   Json<serde_json::Value>,
) -> impl IntoResponse {
    let label = body["label"].as_str().unwrap_or("").trim().to_string();
    if !valid_exo_label(&label) {
        return Json(serde_json::json!({ "ok": false, "error": "label must be APEX-<name>" }));
    }
    // The courier pass (ApexNET P2): stick identity, verify+ingest cargo for
    // this node, receipts home, outbox drained aboard — then the ledger
    // gossip goes out over Tier 1, best-effort (a dark peer is fine: the
    // stick itself is the durable copy).
    let lbl = label.clone();
    let outcome = tokio::task::spawn_blocking(move || {
        apexos_plugins::courier::process_plug_env(&lbl)
    }).await.ok();
    let courier_section = outcome.as_ref()
        .and_then(|o| apexos_plugins::courier::compose_plug_notice(&o.report));
    if let Some(o) = outcome {
        if !o.gossip.is_empty() {
            let node_id = state.node_id.to_string();
            tokio::spawn(async move {
                for f in apexos_plugins::courier::dispatch_gossip(&node_id, o.gossip).await {
                    eprintln!("[courier] gossip: {f}");
                }
            });
        }
    }

    // Default ON; AGENTD_USB_NOTIFY_AGENT=0/false/off silences the proactive greeting.
    let notify = std::env::var("AGENTD_USB_NOTIFY_AGENT")
        .map(|v| { let v = v.to_lowercase(); v != "0" && v != "false" && v != "off" })
        .unwrap_or(true);
    if notify {
        let mut text = format!(
            "🔌 A USB exo-workspace stick **{label}** was just plugged in and mounted at \
             `media/{label}` — portable storage you read + write like any workspace folder. \
             If André's about to work from it, take a quick look and offer to pick up where \
             its files leave off; when he's done with it you can `eject_media` it (label \
             \"{label}\") so it's safe to unplug."
        );
        if let Some(section) = &courier_section {
            text.push_str("\n\nCourier lane (this plug):\n");
            text.push_str(section);
        }
        state.bus.emit(Event::UserPrompt { session: SessionId(0), text, images: vec![] }).await;
    }
    Json(serde_json::json!({
        "ok": true, "label": label, "notified": notify,
        "courier": courier_section.is_some(),
    }))
}

/// Bytes → a short human size for the device picker ("57.3 GB").
fn human_bytes(n: u64) -> String {
    const U: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = n as f64; let mut i = 0;
    while v >= 1024.0 && i < U.len() - 1 { v /= 1024.0; i += 1; }
    if i == 0 { format!("{n} B") } else { format!("{v:.1} {}", U[i]) }
}

/// Relabel-able FAT-family filesystems (slice A "Use this drive" preserves files by
/// relabelling in place; ext4/btrfs/blank need format mode — slice B).
fn is_relabelable_fs(fs: &str) -> bool {
    matches!(fs, "exfat" | "vfat" | "fat" | "fat32" | "msdos")
}

/// Pure parser for `GET /api/media/candidates`: given `lsblk -J -b …` output, return the
/// USB sticks "Use this drive" can adopt, on a USB-transport disk, NOT the system disk, NOT
/// the active exo-workspace mount. `mode="relabel"` offers only relabel-able FAT/exFAT that
/// isn't already `APEX-*` (keeps files); `mode="format"` offers ANY wipeable device incl.
/// blank/ext4/etc. (it'll be erased). The destructive guard still lives in `usb-prep`; this
/// just decides what to *offer*. Unit-tested for both modes with a realistic fixture.
fn parse_prep_candidates(lsblk: &serde_json::Value, media_root: &str, mode: &str) -> Vec<serde_json::Value> {
    const SYS_MOUNTS: [&str; 4] = ["/", "/boot", "/boot/firmware", "/boot/efi"];
    let format = mode == "format";
    let mut out = Vec::new();
    let disks = match lsblk["blockdevices"].as_array() { Some(d) => d, None => return out };

    // Does this device or any descendant mount a system path?
    fn holds_system(dev: &serde_json::Value) -> bool {
        if let Some(mp) = dev["mountpoint"].as_str() {
            if SYS_MOUNTS.contains(&mp) { return true; }
        }
        dev["children"].as_array().map(|cs| cs.iter().any(holds_system)).unwrap_or(false)
    }

    for disk in disks {
        if disk["tran"].as_str() != Some("usb") { continue; }     // USB transport only
        if holds_system(disk) { continue; }                       // never a system disk
        let vendor = disk["vendor"].as_str().unwrap_or("").trim();
        let model  = disk["model"].as_str().unwrap_or("").trim();
        let display = format!("{vendor} {model}").trim().to_string();

        // A filesystem sits on a partition (the common case) or, on a blank/superfloppy disk,
        // directly on the disk. A disk WITH partitions only ever yields its partitions (never
        // the whole disk → format won't clobber a partition table into a superfloppy).
        let parts: Vec<&serde_json::Value> = match disk["children"].as_array() {
            Some(cs) if !cs.is_empty() => cs.iter().collect(),
            _ => vec![disk],
        };
        for p in parts {
            let fstype = p["fstype"].as_str().unwrap_or("");
            let label = p["label"].as_str().unwrap_or("");
            let mp = p["mountpoint"].as_str().unwrap_or("");
            // Never offer the active exo-workspace mount (don't relabel/wipe the live workspace).
            if !mp.is_empty() && mp.starts_with(media_root) { continue; }
            if !format {
                // Relabel: only FAT/exFAT, and not already an exo-workspace.
                if !is_relabelable_fs(fstype) { continue; }
                if label.starts_with("APEX-") { continue; }
            }
            // Format: any wipeable device qualifies (incl. blank fstype / ext4 / an unmounted
            // old APEX stick) — the erase-confirm + the usb-prep gate are the protection.
            let bytes = p["size"].as_u64().unwrap_or(0);
            out.push(serde_json::json!({
                "dev":        p["path"].as_str().unwrap_or(""),
                "label":      label,
                "fstype":     fstype,
                "blank":      fstype.is_empty(),
                "size":       human_bytes(bytes),
                "size_bytes": bytes,
                "model":      display,
                "mountpoint": mp,
            }));
        }
    }
    out
}

/// GET /api/media/candidates?mode=relabel|format — USB sticks the "Use this drive" button
/// can adopt. `relabel` (default) = keep files; `format` = the broader wipeable set.
async fn media_candidates_handler(
    Query(params): Query<HashMap<String, String>>,
) -> impl IntoResponse {
    let mode = match params.get("mode").map(|s| s.as_str()) {
        Some("format") => "format",
        _ => "relabel",
    };
    let out = tokio::process::Command::new("lsblk")
        .args(["-J", "-b", "-o", "NAME,PATH,SIZE,TYPE,MOUNTPOINT,LABEL,FSTYPE,TRAN,MODEL,VENDOR,PKNAME"])
        .output().await;
    let lsblk: serde_json::Value = match out {
        Ok(o) if o.status.success() => serde_json::from_slice(&o.stdout).unwrap_or(serde_json::Value::Null),
        Ok(o) => return Json(serde_json::json!({ "candidates": [], "error": String::from_utf8_lossy(&o.stderr).trim().to_string() })),
        Err(e) => return Json(serde_json::json!({ "candidates": [], "error": format!("lsblk: {e}") })),
    };
    let media_root = {
        let ws = std::env::var("AGENTD_WORKSPACE").ok().filter(|s| !s.is_empty())
            .unwrap_or_else(|| "/var/lib/agentd/workspace".to_string());
        let ws_canon = std::fs::canonicalize(&ws).unwrap_or_else(|_| std::path::PathBuf::from(&ws));
        ws_canon.join("media").to_string_lossy().into_owned()
    };
    Json(serde_json::json!({ "candidates": parse_prep_candidates(&lsblk, &media_root, mode) }))
}

/// POST /api/media/prep {dev, name, mode?} — adopt a USB stick as an exo-workspace.
/// `mode:"relabel"` (default) keeps files; `mode:"format"` WIPES the stick to a fresh
/// exFAT (the UI gates that behind an erase-confirm). agentd can't touch block devices
/// (NoNewPrivileges), so it drops a prep request for the root `usb-prep` drain (which
/// re-validates the device — the real safety boundary) and polls `/proc/mounts` for the
/// new `media/APEX-<name>` mount to appear.
async fn media_prep_handler(Json(body): Json<serde_json::Value>) -> impl IntoResponse {
    let dev  = body["dev"].as_str().unwrap_or("").trim().to_string();
    let mode = body["mode"].as_str().unwrap_or("relabel").trim().to_string();
    // Sanitise the name → the APEX-<name> label.
    let raw_name = body["name"].as_str().unwrap_or("").trim();
    let safe_name: String = raw_name.chars().filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')).collect();
    let label = format!("APEX-{safe_name}");
    if mode != "relabel" && mode != "format" {
        return Json(serde_json::json!({ "ok": false, "error": "mode must be 'relabel' (keep files) or 'format' (wipe)" }));
    }
    // exFAT/FAT volume labels cap at 11 chars (mkfs.exfat/exfatlabel ERROR beyond it), so the
    // name (after APEX-) can be at most 6 chars.
    if safe_name.len() > 6 {
        return Json(serde_json::json!({ "ok": false, "error": "name too long — max 6 characters (drive labels are short)" }));
    }
    if safe_name.is_empty() || !valid_exo_label(&label) {
        return Json(serde_json::json!({ "ok": false, "error": "name must be 1–6 of letters/digits/._- (becomes the APEX-<name> drive label)" }));
    }
    if !dev.starts_with("/dev/") || dev.contains("..") {
        return Json(serde_json::json!({ "ok": false, "error": "bad device path" }));
    }
    // Drop the request (3 lines: mode / dev / name) for the root drain.
    let dir = std::env::var("AGENTD_USB_PREP_DIR").unwrap_or_else(|_| "/var/lib/agentd/usb-prep".to_string());
    if let Err(e) = tokio::fs::create_dir_all(&dir).await {
        return Json(serde_json::json!({ "ok": false, "error": format!("prep dir: {e}") }));
    }
    let req = std::path::Path::new(&dir).join(format!("{label}.req"));
    if let Err(e) = tokio::fs::write(&req, format!("{mode}\n{dev}\n{safe_name}\n")).await {
        return Json(serde_json::json!({ "ok": false, "error": format!("drop prep request: {e}") }));
    }
    // Poll for the new exo-workspace mount (relabel + settle + mount takes a few seconds).
    for _ in 0..50 {
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        if media_mount_present(&label) {
            return Json(serde_json::json!({ "ok": true, "label": label, "mountpoint": format!("media/{label}") }));
        }
    }
    Json(serde_json::json!({ "ok": false, "error": format!(
        "{label} did not mount within 25s — the prep may have failed (check: journalctl -u apexos-usb-prep)") }))
}

async fn workspace_list_handler(
    Query(params): Query<HashMap<String, String>>,
) -> impl IntoResponse {
    let rel = params.get("path").map(|s| s.as_str()).unwrap_or("");
    let (root, dir_rel) = match (workspace_beneath(), workspace_rel(rel)) {
        (Ok(r), Ok(p)) => (r, p),
        (Err(e), _) | (_, Err(e)) => return Json(serde_json::json!({ "error": e, "path": rel, "entries": [] })),
    };
    let ws_canon = root.display().to_path_buf();

    let listed = match root.read_dir(&dir_rel) {
        Ok(v) => v,
        Err(e) => return Json(serde_json::json!({ "error": format!("read dir: {e}"), "path": rel, "entries": [] })),
    };
    let mut entries: Vec<serde_json::Value> = Vec::new();
    for entry in listed {
        if entry.name.starts_with('.') { continue; }
        if entry.is_symlink { continue; } // never present a planted link as a file
        let child_rel = if dir_rel == std::path::Path::new(".") {
            std::path::PathBuf::from(&entry.name)
        } else {
            dir_rel.join(&entry.name)
        };
        let p = ws_canon.join(&child_rel);
        let ext = p.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
        let rel_path = child_rel.to_string_lossy().to_string();
        entries.push(serde_json::json!({
            "name": entry.name,
            "kind": if entry.is_dir { "dir" } else { "file" },
            "size": if entry.is_file { entry.len } else { 0 },
            "modified": entry.mtime,
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
    let (root, path_rel) = match (workspace_beneath(), workspace_rel(rel)) {
        (Ok(r), Ok(p)) => (r, p),
        (Err(e), _) | (_, Err(e)) => return Json(serde_json::json!({ "error": e })),
    };
    let bytes = match root.read(&path_rel) {
        Ok(b) => b,
        Err(e) => return Json(serde_json::json!({ "error": format!("read: {e}") })),
    };
    let truncated = bytes.len() > CAP;
    let slice = &bytes[..bytes.len().min(CAP)];
    let binary = slice.contains(&0u8);
    let content = if binary { String::new() } else { String::from_utf8_lossy(slice).to_string() };
    Json(serde_json::json!({ "content": content, "truncated": truncated, "binary": binary }))
}

/// A coarse content-type by extension for the workspace download endpoint.
fn content_type_for(ext: &str) -> &'static str {
    match ext {
        "png" => "image/png", "jpg" | "jpeg" => "image/jpeg", "gif" => "image/gif",
        "webp" => "image/webp", "svg" => "image/svg+xml", "bmp" => "image/bmp", "heic" => "image/heic",
        "pdf" => "application/pdf", "zip" => "application/zip",
        "txt" | "md" | "log" | "csv" => "text/plain; charset=utf-8",
        "json" => "application/json; charset=utf-8", "html" | "htm" => "text/html; charset=utf-8",
        "mp4" => "video/mp4", "webm" => "video/webm", "mov" => "video/quicktime",
        "mp3" => "audio/mpeg", "wav" => "audio/wav", "ogg" => "audio/ogg", "flac" => "audio/flac", "m4a" => "audio/mp4",
        _ => "application/octet-stream",
    }
}

/// GET /api/workspace/download?path=<rel> — serve a workspace file to the browser/PWA
/// (the phone-handoff file browser; also inline image preview). Confined to the workspace;
/// `?token=` is accepted by `require_token`, so a plain `<a download>` link works.
async fn workspace_download_handler(
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    const CAP: u64 = 256 * 1024 * 1024;
    let rel = params.get("path").map(|s| s.as_str()).unwrap_or("");
    let (root, path_rel) = match (workspace_beneath(), workspace_rel(rel)) {
        (Ok(r), Ok(p)) => (r, p),
        (Err(e), _) | (_, Err(e)) => return (StatusCode::BAD_REQUEST, e).into_response(),
    };
    let meta = match root.stat(&path_rel) {
        Ok(m) => m,
        Err(e) => return (StatusCode::NOT_FOUND, format!("{e}")).into_response(),
    };
    if meta.is_dir { return (StatusCode::BAD_REQUEST, "is a directory".to_string()).into_response(); }
    if meta.is_symlink { return (StatusCode::BAD_REQUEST, "is a symlink".to_string()).into_response(); }
    if meta.len > CAP { return (StatusCode::PAYLOAD_TOO_LARGE, "file too large (>256 MB)".to_string()).into_response(); }
    let bytes = match root.read(&path_rel) {
        Ok(b) => b,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, format!("read: {e}")).into_response(),
    };
    let ext = path_rel.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
    // ASCII-sanitise the filename for the (latin1) Content-Disposition header.
    let name: String = path_rel.file_name().and_then(|n| n.to_str()).unwrap_or("download")
        .chars().map(|c| if c.is_ascii_graphic() && c != '"' && c != '\\' { c } else { '_' }).collect();
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, content_type_for(&ext).to_string()),
            (header::CONTENT_DISPOSITION, format!("attachment; filename=\"{name}\"")),
        ],
        bytes,
    ).into_response()
}

/// POST /api/workspace/upload?path=<rel-target> (raw body = file bytes) — write an uploaded
/// file into the workspace (the phone-handoff upload — incl. onto a mounted `media/` stick).
/// Confined via `resolve_workspace_write_path` (rejects `..`, parent must exist); the route
/// raises the body limit to 256 MB.
async fn workspace_upload_handler(
    Query(params): Query<HashMap<String, String>>,
    body: axum::body::Bytes,
) -> impl IntoResponse {
    let rel = params.get("path").map(|s| s.as_str()).unwrap_or("");
    let (root, target) = match workspace_write_rel(rel) {
        Ok(p) => p,
        Err(e) => return Json(serde_json::json!({ "ok": false, "error": e })),
    };
    if body.is_empty() {
        return Json(serde_json::json!({ "ok": false, "error": "empty upload" }));
    }
    match root.write(&target, &body, false) {
        Ok(_) => Json(serde_json::json!({ "ok": true, "path": rel, "bytes": body.len() })),
        Err(e) => Json(serde_json::json!({ "ok": false, "error": format!("write: {e}") })),
    }
}

/// Body for the Explorer's confined write ops (mkdir/delete/rename/move/copy).
/// Fields are op-specific; all are optional so one struct serves every endpoint.
#[derive(Deserialize)]
struct WorkspaceOpBody {
    #[serde(default)] path: String,   // delete target / new-folder path
    #[serde(default)] name: String,   // rename: the new basename
    #[serde(default)] src:  String,   // move/copy: source (workspace-relative)
    #[serde(default)] dest: String,   // move/copy: destination directory
}

/// POST /api/workspace/mkdir {path} — create a new folder under the workspace.
/// `path` is workspace-relative; the parent must already exist (single-level new
/// folder). Confined exactly like the agent FS tools.
async fn workspace_mkdir_handler(Json(body): Json<WorkspaceOpBody>) -> impl IntoResponse {
    let (root, target) = match workspace_write_rel(&body.path) {
        Ok(p) => p,
        Err(e) => return Json(serde_json::json!({ "ok": false, "error": e })),
    };
    if root.exists(&target) {
        return Json(serde_json::json!({ "ok": false, "error": "already exists" }));
    }
    match root.mkdir(&target) {
        Ok(_) => Json(serde_json::json!({ "ok": true })),
        Err(e) => Json(serde_json::json!({ "ok": false, "error": format!("mkdir: {e}") })),
    }
}

/// POST /api/workspace/delete {path} — remove a file or directory (recursive).
/// Refuses the workspace root itself; a mounted exo-workspace stick's mountpoint
/// fails naturally (EBUSY) — eject it first.
async fn workspace_delete_handler(Json(body): Json<WorkspaceOpBody>) -> impl IntoResponse {
    let (root, target) = match (workspace_beneath(), workspace_rel(&body.path)) {
        (Ok(r), Ok(p)) => (r, p),
        (Err(e), _) | (_, Err(e)) => return Json(serde_json::json!({ "ok": false, "error": e })),
    };
    if target == std::path::Path::new(".") {
        return Json(serde_json::json!({ "ok": false, "error": "refusing to delete the workspace root" }));
    }
    match root.remove_all(&target) {
        Ok(_) => Json(serde_json::json!({ "ok": true })),
        Err(e) => Json(serde_json::json!({ "ok": false, "error": format!("delete: {e}") })),
    }
}

/// POST /api/workspace/rename {path, name} — rename an entry in place. `name` is a
/// single safe component (no separator / traversal); the target stays in the same
/// (already-confined) parent directory.
async fn workspace_rename_handler(Json(body): Json<WorkspaceOpBody>) -> impl IntoResponse {
    let (root, from) = match (workspace_beneath(), workspace_rel(&body.path)) {
        (Ok(r), Ok(p)) => (r, p),
        (Err(e), _) | (_, Err(e)) => return Json(serde_json::json!({ "ok": false, "error": e })),
    };
    if from == std::path::Path::new(".") {
        return Json(serde_json::json!({ "ok": false, "error": "cannot rename the workspace root" }));
    }
    let name = body.name.trim();
    if !safe_component(name) {
        return Json(serde_json::json!({ "ok": false, "error": "invalid name (no /, .. and not empty)" }));
    }
    let parent = from.parent().unwrap_or(std::path::Path::new("."));
    let to = if parent.as_os_str().is_empty() || parent == std::path::Path::new(".") {
        std::path::PathBuf::from(name)
    } else {
        parent.join(name)
    };
    if root.exists(&to) {
        return Json(serde_json::json!({ "ok": false, "error": "already exists" }));
    }
    match root.rename(&from, &to) {
        Ok(_) => Json(serde_json::json!({ "ok": true })),
        Err(e) => Json(serde_json::json!({ "ok": false, "error": format!("rename: {e}") })),
    }
}

/// POST /api/workspace/move {src, dest} — move `src` into the `dest` directory
/// (keeps the basename). Same-filesystem → `rename`; a cross-device move (EXDEV,
/// e.g. workspace ⇄ a mounted exo-workspace stick) falls back to recursive copy +
/// remove. Both ends are workspace-confined.
async fn workspace_move_handler(Json(body): Json<WorkspaceOpBody>) -> impl IntoResponse {
    let (root, src, target) = match resolve_move_target(&body.src, &body.dest) {
        Ok(t) => t,
        Err(e) => return Json(serde_json::json!({ "ok": false, "error": e })),
    };
    let res = tokio::task::spawn_blocking(move || {
        match root.rename(&src, &target) {
            Ok(_) => Ok(()),
            // EXDEV (18): cross-device link — copy then remove the source.
            Err(e) if e.raw_os_error() == Some(18) => {
                copy_beneath(&root, &src, &target)?;
                root.remove_all(&src)
            }
            Err(e) => Err(e),
        }
    }).await;
    match res {
        Ok(Ok(())) => Json(serde_json::json!({ "ok": true })),
        Ok(Err(e)) => Json(serde_json::json!({ "ok": false, "error": format!("move: {e}") })),
        Err(e) => Json(serde_json::json!({ "ok": false, "error": format!("move task: {e}") })),
    }
}

/// POST /api/workspace/copy {src, dest} — copy `src` into the `dest` directory
/// (recursive for a folder; keeps the basename). Both ends are workspace-confined.
async fn workspace_copy_handler(Json(body): Json<WorkspaceOpBody>) -> impl IntoResponse {
    let (root, src, target) = match resolve_move_target(&body.src, &body.dest) {
        Ok(t) => t,
        Err(e) => return Json(serde_json::json!({ "ok": false, "error": e })),
    };
    let res = tokio::task::spawn_blocking(move || copy_beneath(&root, &src, &target)).await;
    match res {
        Ok(Ok(())) => Json(serde_json::json!({ "ok": true })),
        Ok(Err(e)) => Json(serde_json::json!({ "ok": false, "error": format!("copy: {e}") })),
        Err(e) => Json(serde_json::json!({ "ok": false, "error": format!("copy task: {e}") })),
    }
}

#[derive(Deserialize)]
struct AudioPathBody {
    path: String,
}

/// POST /api/audio/analyze — run ffprobe + ffmpeg loudnorm analysis.
async fn audio_analyze_handler(
    Json(body): Json<AudioPathBody>,
) -> impl IntoResponse {
    let path = match resolve_workspace_path(&body.path) {
        Ok(p) => p.to_string_lossy().into_owned(),
        Err(e) => return (StatusCode::BAD_REQUEST, Json(serde_json::json!({ "error": e }))).into_response(),
    };
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
    let path = match resolve_workspace_path(&body.path) {
        Ok(p) => p.to_string_lossy().into_owned(),
        Err(e) => return (StatusCode::BAD_REQUEST, Json(serde_json::json!({ "error": e }))).into_response(),
    };
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
    let path = match resolve_workspace_path(&body.path) {
        Ok(p) => p.to_string_lossy().into_owned(),
        Err(e) => return (StatusCode::BAD_REQUEST, Json(serde_json::json!({ "error": e }))).into_response(),
    };
    let ops = body.ops.clone();

    // Default output path: <stem>_edit.<ext>, alongside the (confined) input.
    let output_req = match body.output_path.clone() {
        Some(p) => p,
        None => {
            let p = std::path::Path::new(&path);
            let stem = p.file_stem().and_then(|s| s.to_str()).unwrap_or("track");
            let ext  = p.extension().and_then(|s| s.to_str()).unwrap_or("mp3");
            let dir  = p.parent().and_then(|d| d.to_str()).unwrap_or(".");
            format!("{dir}/{stem}_edit.{ext}")
        }
    };
    // Confine the write target to the workspace (it may not exist yet).
    let output_path = match resolve_workspace_write_path(&output_req) {
        Ok(p) => p.to_string_lossy().into_owned(),
        Err(e) => return (StatusCode::BAD_REQUEST, Json(serde_json::json!({ "error": e }))).into_response(),
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
        .find(|l| l.contains(key))?.split_once(':')?.1
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
    axum::serve(
        listener,
        router(state).into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await?;
    Ok(())
}

#[cfg(test)]
mod models_tests {
    use super::*;

    #[test]
    fn parse_anthropic_models_extracts_id_and_display_name() {
        let body = serde_json::json!({
            "data": [
                { "type": "model", "id": "claude-fable-5", "display_name": "Claude Fable 5" },
                { "type": "model", "id": "claude-3-opus-20240229" }, // no display_name → id
                { "type": "model", "display_name": "no id — skipped" },
            ],
            "has_more": false,
        });
        let models = parse_anthropic_models(&body);
        assert_eq!(models.len(), 2);
        assert_eq!(models[0]["id"], "claude-fable-5");
        assert_eq!(models[0]["name"], "Claude Fable 5");
        assert_eq!(models[1]["id"], "claude-3-opus-20240229");
        assert_eq!(models[1]["name"], "claude-3-opus-20240229");
    }

    #[test]
    fn parse_anthropic_models_handles_garbage() {
        assert!(parse_anthropic_models(&serde_json::Value::Null).is_empty());
        assert!(parse_anthropic_models(&serde_json::json!({"data": "nope"})).is_empty());
        // Empty catalog must fall through to the static fallback (non-empty check).
        assert!(parse_anthropic_models(&serde_json::json!({"data": []})).is_empty());
    }
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

    #[test]
    fn exo_label_validation() {
        // Accept: APEX- prefix, sane single component.
        assert!(valid_exo_label("APEX-mystick"));
        assert!(valid_exo_label("APEX-work_2024.1"));
        // Reject: wrong prefix, path-escape, separators, too short/long, bad chars.
        assert!(!valid_exo_label("mystick"));        // no APEX- prefix
        assert!(!valid_exo_label("APEX-"));          // empty name
        assert!(!valid_exo_label("APEX-a/b"));       // path separator
        assert!(!valid_exo_label("APEX-../etc"));    // traversal
        assert!(!valid_exo_label("APEX-a b"));       // space
        assert!(!valid_exo_label("APEX-$(x)"));      // shell-ish chars
        assert!(!valid_exo_label(&format!("APEX-{}", "x".repeat(70)))); // too long
    }

    #[test]
    fn prep_candidates_offers_only_safe_usb_fat_sticks() {
        // Realistic shape: an NVMe system disk, a USB exFAT stick (the one candidate), a
        // USB ext4 spare drive (not relabel-able → excluded), an already-APEX USB stick
        // (excluded), and a USB stick already mounted under media/ (excluded).
        let lsblk = serde_json::json!({ "blockdevices": [
            { "name": "nvme0n1", "path": "/dev/nvme0n1", "type": "disk", "tran": "nvme", "children": [
                { "name": "nvme0n1p1", "path": "/dev/nvme0n1p1", "type": "part", "fstype": "vfat", "mountpoint": "/boot/efi", "label": "EFI", "size": 536870912 },
                { "name": "nvme0n1p2", "path": "/dev/nvme0n1p2", "type": "part", "fstype": "ext4", "mountpoint": "/", "label": null, "size": 511000000000u64 }
            ]},
            { "name": "sdb", "path": "/dev/sdb", "type": "disk", "tran": "usb", "vendor": "SanDisk", "model": "Ultra", "children": [
                { "name": "sdb1", "path": "/dev/sdb1", "type": "part", "fstype": "exfat", "mountpoint": null, "label": "MYSTICK", "size": 61530439680u64 }
            ]},
            { "name": "sdc", "path": "/dev/sdc", "type": "disk", "tran": "usb", "vendor": "Seagate", "model": "Backup", "children": [
                { "name": "sdc1", "path": "/dev/sdc1", "type": "part", "fstype": "ext4", "mountpoint": "/run/media/andre/spare", "label": "spare", "size": 2000000000000u64 }
            ]},
            { "name": "sdd", "path": "/dev/sdd", "type": "disk", "tran": "usb", "children": [
                { "name": "sdd1", "path": "/dev/sdd1", "type": "part", "fstype": "exfat", "mountpoint": "/var/lib/agentd/workspace/media/APEX-config", "label": "APEX-config", "size": 32000000000u64 }
            ]},
            { "name": "sde", "path": "/dev/sde", "type": "disk", "tran": "usb", "children": [
                { "name": "sde1", "path": "/dev/sde1", "type": "part", "fstype": "vfat", "mountpoint": null, "label": "APEX-work", "size": 16000000000u64 }
            ]},
            // A truly-blank USB disk (no partition table, no filesystem).
            { "name": "sdf", "path": "/dev/sdf", "type": "disk", "tran": "usb", "vendor": "Generic", "model": "Flash", "fstype": null, "mountpoint": null, "label": null, "size": 8000000000u64 }
        ]});
        let media = "/var/lib/agentd/workspace/media";

        // RELABEL: exactly one offer — the FAT/exFAT USB stick that isn't system / ext4 /
        // already-APEX / adopted / blank.
        let rel = parse_prep_candidates(&lsblk, media, "relabel");
        assert_eq!(rel.len(), 1, "relabel got {rel:?}");
        assert_eq!(rel[0]["dev"], "/dev/sdb1");
        assert_eq!(rel[0]["label"], "MYSTICK");
        assert_eq!(rel[0]["model"], "SanDisk Ultra");
        assert_eq!(rel[0]["size"], "57.3 GB");

        // FORMAT: any wipeable USB device — the exFAT stick, the ext4 spare, the unmounted
        // APEX stick, AND the blank disk — but NEVER the system disk or the ACTIVE media mount.
        let fmt = parse_prep_candidates(&lsblk, media, "format");
        let devs: std::collections::HashSet<&str> = fmt.iter().map(|c| c["dev"].as_str().unwrap()).collect();
        assert_eq!(devs.len(), 4, "format got {fmt:?}");
        assert!(devs.contains("/dev/sdb1"));   // exfat
        assert!(devs.contains("/dev/sdc1"));   // ext4 spare — wipeable
        assert!(devs.contains("/dev/sde1"));   // unmounted old APEX stick — wipeable
        assert!(devs.contains("/dev/sdf"));    // blank disk — wipeable (offered as the whole disk)
        assert!(!devs.contains("/dev/nvme0n1p2")); // system disk — never
        assert!(!devs.contains("/dev/sdd1"));      // active media mount — never
        assert_eq!(fmt.iter().find(|c| c["dev"] == "/dev/sdf").unwrap()["blank"], true);
    }

    #[test]
    fn safe_component_validation() {
        // Accept: a normal single basename for a rename / new folder.
        assert!(safe_component("notes"));
        assert!(safe_component("my file.txt"));   // spaces are fine in a name
        assert!(safe_component(".hidden"));        // leading dot is a valid name
        // Reject: empty, traversal tokens, separators, NUL.
        assert!(!safe_component(""));
        assert!(!safe_component("."));
        assert!(!safe_component(".."));
        assert!(!safe_component("a/b"));           // path separator escapes the dir
        assert!(!safe_component("../etc"));         // traversal
        assert!(!safe_component("a\0b"));           // NUL byte
    }
}

#[cfg(test)]
mod ws_filter_tests {
    use super::*;

    #[test]
    fn conversation_stream_events_are_session_scoped() {
        assert_eq!(event_session(&Event::AgentText { session: SessionId(42), delta: "hi".into() }), Some(SessionId(42)));
        assert_eq!(event_session(&Event::AgentThinking { session: SessionId(42), delta: "…".into() }), Some(SessionId(42)));
        assert_eq!(event_session(&Event::TurnComplete { session: SessionId(7) }), Some(SessionId(7)));
        assert_eq!(event_session(&Event::UserCancel { session: SessionId(7) }), Some(SessionId(7)));
        // Sub-agent events route to the PARENT session's client.
        assert_eq!(
            event_session(&Event::SubAgentStarted { parent: SessionId(3), child: SessionId(9000), prompt: "x".into() }),
            Some(SessionId(3))
        );
        // Error scopes to its session, or is global when session-less.
        assert_eq!(event_session(&Event::Error { session: Some(SessionId(5)), message: "boom".into() }), Some(SessionId(5)));
        assert_eq!(event_session(&Event::Error { session: None, message: "global".into() }), None);
    }

    #[test]
    fn global_status_events_go_to_all_clients() {
        // No session field → None → forwarded to every socket (unchanged behaviour),
        // so no status event is ever hidden by the per-session filter.
        assert_eq!(event_session(&Event::PeerLost { node_id: "n1".into() }), None);
        assert_eq!(event_session(&Event::PeerSeen { node_id: "n1".into(), ip: "10.0.0.2".into() }), None);
        assert_eq!(event_session(&Event::VastTunnelLost { instance_id: "i1".into() }), None);
        // A mesh a2a notification is GLOBAL despite carrying a `session` field — the
        // session there is informational (where it landed), not a delivery scope, so
        // a user watching any session sees that mesh traffic arrived.
        assert_eq!(
            event_session(&Event::MeshMessage { from_node: "ApexOS-RS".into(), session: SessionId(23), preview: "hi".into() }),
            None
        );
    }

    #[test]
    fn session_markdown_renders_roles_tools_and_skips_thinking() {
        use apexos_core::{ContentBlock, Message};
        let lines = [
            Message::User { content: vec![ContentBlock::Text { text: "hello there".into() }] },
            Message::Assistant { content: vec![
                ContentBlock::Thinking { thinking: "secret".into(), signature: "s".into() },
                ContentBlock::ToolUse { id: "t1".into(), name: "read_file".into(),
                    input: serde_json::json!({"path": "x.rs"}) },
            ] },
            Message::User { content: vec![ContentBlock::ToolResult {
                tool_use_id: "t1".into(), content: serde_json::json!("file body"), is_error: false }] },
        ];
        let jsonl: String = lines.iter()
            .map(|m| serde_json::to_string(m).unwrap())
            .collect::<Vec<_>>()
            .join("\n");

        let md = render_session_markdown(42, &jsonl);
        assert!(md.starts_with("# Session 42"));
        assert!(md.contains("**You:** hello there"), "user text rendered");
        assert!(md.contains("🔧 `read_file`"), "tool call rendered");
        assert!(md.contains("↳"), "tool result rendered");
        assert!(!md.contains("secret"), "thinking blocks are omitted");
    }

    #[test]
    fn mesh_dest_rejects_traversal_and_absolute() {
        assert!(confine_mesh_dest("../etc/passwd").is_err(), "reject ..");
        assert!(confine_mesh_dest("a/../../b").is_err(), "reject .. mid-path");
        assert!(confine_mesh_dest("/etc/passwd").is_err(), "reject absolute");
        assert!(confine_mesh_dest("").is_err(), "reject empty");
        // A plain relative dest reaches the canonicalize step (workspace-dependent);
        // the guards above are the security-critical short-circuits.
    }

    #[test]
    fn compact_json_truncates_and_flattens() {
        let long = serde_json::Value::String("x".repeat(500));
        let out = compact_json(&long);
        assert!(out.chars().count() <= 201, "truncated to ~200 chars + ellipsis");
        assert!(out.ends_with('…'));
        assert_eq!(compact_json(&serde_json::json!("a\nb")), "a b", "newlines flattened");
    }

    #[test]
    fn mesh_session_alloc_is_stable_and_collision_free() {
        let mut map: HashMap<String, SessionId> = HashMap::new();
        let counter = AtomicU64::new(23);

        // First contact allocates a fresh id; the same peer is then stable.
        let (a1, fresh1) = mesh_session_alloc(&mut map, &counter, "ApexOS-RS");
        assert_eq!((a1, fresh1), (SessionId(23), true));
        let (a2, fresh2) = mesh_session_alloc(&mut map, &counter, "ApexOS-RS");
        assert_eq!((a2, fresh2), (SessionId(23), false), "same peer → same session, no re-alloc");

        // A different peer gets its own distinct id from the same counter.
        let (b1, freshb) = mesh_session_alloc(&mut map, &counter, "apex3-radxa");
        assert_eq!((b1, freshb), (SessionId(24), true));
        assert_ne!(a1, b1, "distinct peers never share a thread");

        // The counter is shared with socket-session allocation, so the next socket
        // id is strictly above every mesh id — they can never collide.
        assert_eq!(counter.fetch_add(1, Ordering::SeqCst), 25);
    }

    #[test]
    fn fed_stats_bump_accumulates_and_roundtrips() {
        let mut map: HashMap<String, PeerFedStats> = HashMap::new();
        // Import + a re-send caught by origin dedup + a recall answered with 3 hits.
        fed_stats_bump(&mut map, "ApexOS-2", 100, |s| s.memories_received += 1);
        fed_stats_bump(&mut map, "ApexOS-2", 160, |s| s.duplicates += 1);
        fed_stats_bump(&mut map, "ApexOS-2", 200, |s| { s.recall_served += 1; s.recall_hits += 3; });
        let e = &map["ApexOS-2"];
        assert_eq!(
            (e.memories_received, e.duplicates, e.recall_served, e.recall_hits, e.last_ts),
            (1, 1, 1, 3, 200),
            "counters accumulate independently; last_ts follows the latest touch"
        );

        // A different peer is independent; an unknown peer reads as all-zeros default.
        fed_stats_bump(&mut map, "andre-laptop", 210, |s| s.memories_received += 1);
        assert_eq!(map["andre-laptop"].memories_received, 1);
        assert_eq!(map["ApexOS-2"].memories_received, 1, "other peer untouched");
        assert_eq!(PeerFedStats::default().memories_received, 0);

        // JSON round-trip (the persistence format).
        let json = serde_json::to_string(&map).unwrap();
        let back: HashMap<String, PeerFedStats> = serde_json::from_str(&json).unwrap();
        assert_eq!(back["ApexOS-2"].recall_hits, 3);
    }

    #[test]
    fn mesh_unread_bump_clear_and_persist_roundtrip() {
        let mut map: HashMap<u64, MeshUnread> = HashMap::new();
        // Two inbound messages on one thread → unread 2, latest preview/time win.
        mesh_unread_bump(&mut map, 23, "ApexOS-RS", "hi", 100);
        mesh_unread_bump(&mut map, 23, "ApexOS-RS", "you there?", 160);
        let e = &map[&23];
        assert_eq!((e.unread, e.preview.as_str(), e.last_ts), (2, "you there?", 160));
        assert_eq!(e.node_id, "ApexOS-RS");

        // A different thread is independent.
        mesh_unread_bump(&mut map, 24, "apex3-radxa", "ping", 170);
        assert_eq!(map[&24].unread, 1);

        // Clear zeroes only the named thread (and is idempotent).
        assert!(mesh_unread_clear(&mut map, 23));
        assert_eq!(map[&23].unread, 0);
        assert!(!mesh_unread_clear(&mut map, 23), "already zero → no change");
        assert_eq!(map[&24].unread, 1, "other thread untouched");

        // JSON round-trips with u64 keys (serde stringifies/parses them).
        let json = serde_json::to_string(&map).unwrap();
        let back: HashMap<u64, MeshUnread> = serde_json::from_str(&json).unwrap();
        assert_eq!(back[&24].preview, "ping");
        assert_eq!(back[&23].unread, 0);
    }

    #[test]
    fn a2a_prompt_text_carries_provenance_and_reply_route() {
        // Peer + origin → provenance AND the verbatim reply call (the continuity
        // affordance the receiving agent copies to answer into the asking session).
        assert_eq!(
            a2a_prompt_text(Some("ApexOS-2"), Some(42), "status?"),
            "[from ApexOS-2 — to reply: send_to_agent(node=\"ApexOS-2\", session_id=42)]: status?"
        );
        // Peer without origin → the classic provenance prefix, byte-identical to
        // the pre-continuity wire format (old-node senders keep working).
        assert_eq!(
            a2a_prompt_text(Some("ApexOS-2"), None, "status?"),
            "[from ApexOS-2]: status?"
        );
        // No registered peer → the raw message, whatever the body claimed —
        // origin is only meaningful with trusted provenance.
        assert_eq!(a2a_prompt_text(None, Some(42), "status?"), "status?");
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

#[cfg(test)]
mod pin_lockout_tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn locks_after_max_fails_and_resets_on_success() {
        let now = Instant::now();
        let mut l = PinLockout::default();

        // Below the threshold: not locked yet.
        for _ in 0..(PIN_MAX_FAILS - 1) {
            l.record(false, now);
            assert!(l.locked_for(now).is_none());
        }
        // The Nth consecutive failure arms the cooldown.
        l.record(false, now);
        let remaining = l.locked_for(now).expect("locked after max fails");
        assert!(remaining > 0 && remaining <= PIN_LOCKOUT_SECS);

        // Still locked just before expiry; clear after it.
        assert!(l.locked_for(now + Duration::from_secs(PIN_LOCKOUT_SECS - 1)).is_some());
        assert!(l.locked_for(now + Duration::from_secs(PIN_LOCKOUT_SECS + 1)).is_none());

        // A success clears state entirely.
        l.record(true, now);
        assert!(l.locked_for(now).is_none());
        assert_eq!(l.fails, 0);
    }

    #[test]
    fn success_keeps_it_unlocked() {
        let now = Instant::now();
        let mut l = PinLockout::default();
        for _ in 0..10 { l.record(true, now); }
        assert!(l.locked_for(now).is_none());
    }
}
