mod session_store;
use session_store::SessionStore;
mod pac_lint;
mod scheduler;
use scheduler::{load_schedules, run_scheduler, spawn_scheduler_handler, SchedulerState};
mod council_handler;
use council_handler::spawn_council_handler;
mod health;
mod self_update;
mod consolidate;
mod dream_digest;
mod rehearse;
mod evolution;
mod goal;
mod sensor_config;

use apexos_core::{
    ActionId, Bus, ContentBlock, Event, EvolutionId, EvolutionProposal, ImageSource, Message,
    PluginId, PolicyMode, SessionId, SensorReading, Subsystem, SystemState, ToolOutput, ToolSpec,
};
use apexos_gateway::{serve, ConsolidateReq, GatewayState, PeerRegistry, SpawnReq};
use apexos_plugins::{
    load as load_plugins, PluginConfig, PolicyConfig, PolicyEngine, RestartPolicy,
    Supervisor, SupervisorCmd, ToolProxy, VastState,
};
use apexos_agent::{RoutingProvider, TurnEngine, run_turn};
use apexos_store::run_log_writer;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::{broadcast, mpsc, Mutex, RwLock};
use tokio::task::AbortHandle;

fn load_soul() -> (PathBuf, String) {
    let path = std::env::var("AGENTD_SOUL")
        .unwrap_or_else(|_| "/etc/agentd/soul.md".into());
    match std::fs::read_to_string(&path) {
        Ok(s) if !s.trim().is_empty() => {
            eprintln!("[agentd] soul loaded from {path}");
            (PathBuf::from(&path), s)
        }
        _ => {
            let dev = std::env::var("AGENTD_SOUL_DEV")
                .unwrap_or_else(|_| "config/soul.md".into());
            match std::fs::read_to_string(&dev) {
                Ok(s) if !s.trim().is_empty() => {
                    eprintln!("[agentd] soul loaded from {dev}");
                    (PathBuf::from(&dev), s)
                }
                _ => {
                    eprintln!("[agentd] soul.md not found — running without system prompt");
                    // Default write path even when the file doesn't exist yet
                    (PathBuf::from("/etc/agentd/soul.md"), String::new())
                }
            }
        }
    }
}

fn load_api_key() -> String {
    // 1. Environment variable (set by systemd EnvironmentFile or shell)
    if let Ok(k) = std::env::var("ANTHROPIC_API_KEY") {
        if !k.is_empty() { return k; }
    }
    // 2. Runtime file written by the browser UI key-entry flow
    let path = std::env::var("AGENTD_KEY_FILE")
        .unwrap_or_else(|_| "/var/lib/agentd/.api_key".into());
    if let Ok(k) = std::fs::read_to_string(&path) {
        let k = k.trim().to_string();
        if !k.is_empty() { return k; }
    }
    String::new()
}

fn load_oai_api_key() -> String {
    // Prefer OAI_API_KEY; OPENROUTER_API_KEY is an alias for convenience
    for var in ["OAI_API_KEY", "OPENROUTER_API_KEY"] {
        if let Ok(k) = std::env::var(var) {
            if !k.is_empty() { return k; }
        }
    }
    let path = std::env::var("AGENTD_OAI_KEY_FILE")
        .unwrap_or_else(|_| "/var/lib/agentd/.oai_api_key".into());
    if let Ok(k) = std::fs::read_to_string(&path) {
        let k = k.trim().to_string();
        if !k.is_empty() { return k; }
    }
    String::new()
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let (bus, handle, bcast) = Bus::new(SystemState::default());
    tokio::spawn(bus.run());

    // Shared API key + model — readable/writable from both the turn engine and browser UI
    let api_key_str = load_api_key();
    if api_key_str.is_empty() {
        eprintln!("[agentd] ANTHROPIC_API_KEY not set — enter via browser UI at :8787");
    }
    let api_key_arc = Arc::new(RwLock::new(api_key_str));
    let oai_api_key_str = load_oai_api_key();
    let oai_api_key_arc = Arc::new(RwLock::new(oai_api_key_str));
    // Backend/model/URL resolve through the persisted operator choice first (Settings
    // → POST /api/backend|model → backend_config.json, file-wins-on-restart like
    // voice/sensor config), then env, then backend-aware defaults. One resolution,
    // shared by the arcs AND the vast revert defaults, so every consumer agrees.
    let persisted_backend = apexos_gateway::backend_config::load_persisted();
    let boot_cfg = apexos_gateway::backend_config::resolve_boot(
        &std::env::var("AGENTD_BACKEND").unwrap_or_default(),
        &std::env::var("AGENTD_MODEL").unwrap_or_default(),
        &std::env::var("AGENTD_OAI_BASE_URL").unwrap_or_default(),
        persisted_backend.as_ref(),
    );
    eprintln!(
        "[agentd] backend: {}, model: {}{}",
        boot_cfg.backend,
        boot_cfg.model,
        if persisted_backend.is_some() { " (persisted)" } else { "" }
    );
    // Keep the resolved boot model owned (clone into the arc) — the vast hot-swap
    // block reverts to this exact value, which is correct even when AGENTD_MODEL is
    // unset (the per-backend default), unlike re-reading AGENTD_MODEL alone.
    let default_model = boot_cfg.model.clone();
    let model_arc        = Arc::new(RwLock::new(boot_cfg.model.clone()));
    let backend_arc      = Arc::new(RwLock::new(boot_cfg.backend.clone()));
    let oai_base_url_arc = Arc::new(RwLock::new(boot_cfg.oai_base_url.clone()));

    // Load policy config and wrap in a shared Arc so the evolution applier can hot-swap it.
    let policy_path = PathBuf::from(
        std::env::var("AGENTD_POLICY_TOML")
            .unwrap_or_else(|_| "config/policy.toml".into())
    );
    let policy_config = match PolicyConfig::load(&policy_path) {
        Ok(c)  => { eprintln!("[agentd] policy mode: {:?}", c.mode); c }
        Err(e) => { eprintln!("[agentd] policy config: {e} — using defaults"); PolicyConfig::default() }
    };
    let policy_mode_str = format!("{:?}", policy_config.mode).to_lowercase().replace("autoedit", "auto-edit");
    let policy_mode_arc: Arc<RwLock<String>> = Arc::new(RwLock::new(policy_mode_str));
    let policy_arc: Arc<RwLock<PolicyEngine>> =
        Arc::new(RwLock::new(PolicyEngine::new(policy_config)));

    // Channel for live policy mode changes from the /api/policy gateway route.
    let (policy_set_tx, mut policy_set_rx) = tokio::sync::mpsc::channel::<String>(8);
    {
        let policy_arc2 = Arc::clone(&policy_arc);
        let policy_mode_arc2 = Arc::clone(&policy_mode_arc);
        tokio::spawn(async move {
            while let Some(mode_str) = policy_set_rx.recv().await {
                let new_mode = match mode_str.as_str() {
                    "auto-edit" => PolicyMode::AutoEdit,
                    "yolo"      => PolicyMode::Yolo,
                    _           => PolicyMode::Suggest,
                };
                policy_arc2.write().await.config.mode = new_mode;
                *policy_mode_arc2.write().await = mode_str.clone();
                eprintln!("[agentd] policy mode changed to: {mode_str}");
            }
        });
    }

    // Gateway
    let ui_dir = PathBuf::from(
        std::env::var("AGENTD_UI").unwrap_or_else(|_| "ui".into())
    );
    let log_dir = PathBuf::from(
        std::env::var("AGENTD_LOG").unwrap_or_else(|_| "events".into())
    );

    // Session store — init early so histories and next_session_id are ready for GatewayState.
    let session_store = Arc::new(SessionStore::new(&log_dir));
    session_store.init().await?;
    let initial_histories = session_store.load_all().await;

    // Mesh a2a per-peer session map (peer node_id → SessionId on this node).
    // Loaded before next_session_id so we can start the counter past any session a
    // peer thread already claims — a mesh session id must never be re-handed-out to
    // a socket after a restart. See gateway::mesh_session_for.
    let mesh_sessions_path = log_dir.join("mesh_sessions.json");
    let mesh_sessions: HashMap<String, SessionId> = std::fs::read_to_string(&mesh_sessions_path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    let max_mesh_sid = mesh_sessions.values().map(|s| s.0).max().unwrap_or(0);

    // Per-peer inbox unread (cross-restart persistence) — session id → unread state.
    let mesh_unread_path = log_dir.join("mesh_unread.json");
    let mesh_unread: HashMap<u64, apexos_gateway::MeshUnread> = std::fs::read_to_string(&mesh_unread_path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    let mesh_unread = Arc::new(std::sync::Mutex::new(mesh_unread));

    // Federation observability counters (principle 6, receiver-side v1) —
    // peer node_id → inbound memories/duplicates/recall stats, cross-restart.
    let fed_stats_path = log_dir.join("mesh_fed_stats.json");
    let fed_stats: HashMap<String, apexos_gateway::PeerFedStats> = std::fs::read_to_string(&fed_stats_path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    let fed_stats = Arc::new(std::sync::Mutex::new(fed_stats));

    // Server-issued session IDs — start above any IDs already loaded from disk
    // (history JSONL *and* the mesh per-peer map).
    let max_loaded_sid = initial_histories.keys().map(|s| s.0).max().unwrap_or(0)
        .max(max_mesh_sid);
    let next_session_id = Arc::new(AtomicU64::new(max_loaded_sid + 1));
    let mesh_sessions = Arc::new(std::sync::Mutex::new(mesh_sessions));

    // Shared state for the agent router (created early — needed by GatewayState too).
    let tool_reg: Arc<RwLock<HashMap<PluginId, Vec<ToolSpec>>>> =
        Arc::new(RwLock::new(HashMap::new()));
    let histories: Arc<Mutex<HashMap<SessionId, Vec<Message>>>> =
        Arc::new(Mutex::new(initial_histories));

    let sensor_bridge_token = Arc::new(
        std::env::var("SENSOR_BRIDGE_TOKEN").unwrap_or_default()
    );
    let api_token = Arc::new(
        std::env::var("AGENTD_TOKEN").unwrap_or_default()
    );
    let api_token_empty = api_token.is_empty();
    if api_token_empty {
        eprintln!("[agentd] AGENTD_TOKEN not set — API auth disabled (safe only on 127.0.0.1)");
    }

    // Load soul early so we can share the path with both the gateway (settings UI) and
    // the turn engine below.
    let (soul_path, soul_content) = load_soul();

    // Council shared state — created early so GatewayState can hold Arc clones.
    let council_butt_in:  apexos_gateway::CouncilButtInMap   = Arc::new(Mutex::new(HashMap::new()));
    let council_sessions: apexos_gateway::CouncilSessionsMap = Arc::new(Mutex::new(Vec::new()));
    let council_next_id   = Arc::new(AtomicU64::new(1));
    let (council_tx, council_rx) = mpsc::channel::<(SessionId, ActionId, serde_json::Value)>(8);
    let council_start_tx  = council_tx.clone();

    // Autonomous goal driver (Phase 2a, docs/ideas/goal-driver-design.md): goal_create
    // forwards here; the driver owns its goal map and drives each via the bus.
    let next_goal_id = Arc::new(AtomicU64::new(1));
    let (goal_tx, goal_rx) = mpsc::channel::<(SessionId, ActionId, String, serde_json::Value)>(8);
    let goals_path = log_dir.join("goals.json"); // P2d: goals survive a daemon restart
    // #3 goal-scoped yolo: sessions of yolo:true goals — shared driver↔supervisor so the
    // approval gate auto-approves a trusted goal's OWN ask tools (scoped to that session).
    let goal_yolo: apexos_core::GoalYoloSessions =
        std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashSet::new()));

    // Peer registry — /etc/agentd/peers.toml (created empty if missing)
    let peers_path = PathBuf::from(
        std::env::var("PEERS_TOML").unwrap_or_else(|_| "/etc/agentd/peers.toml".into())
    );
    if !peers_path.exists() {
        let _ = std::fs::write(&peers_path, "# ApexOS mesh peers\n");
    }
    let peer_registry = Arc::new(RwLock::new(PeerRegistry::load(&peers_path)));
    // Downtime beacon (colony-mesh spine): per-peer active liveness, shared
    // gateway (serves it in /api/mesh/peers) ↔ the beacon loop (writes it).
    let mesh_liveness = apexos_gateway::new_liveness_map();
    // Sensor-alert sensitivity PROFILE (standard / smoker / kitchen / workshop) —
    // runtime toggle, persisted. A non-standard profile raises IAQ + thermal thresholds
    // above that environment's normal baseline so routine activity (a cig / cooking /
    // soldering) doesn't trip an autonomous alert (a sustained real fire still does).
    // Shared so the sensor-alert loop reads it and the gateway (Settings toggle) flips it.
    let sensor_config_path = log_dir.join("sensor_config.json");
    let sensor_profile = Arc::new(std::sync::RwLock::new(
        sensor_config::load_profile(&sensor_config_path),
    ));
    let vast_state = VastState::new();
    {
        let vs = vast_state.clone();
        tokio::spawn(async move { vs.try_restore().await; });
    }
    // Single source of truth for the node's mesh identity (env APEX_NODE_ID or
    // hostname) — shared with the cross-node a2a sender via apexos_core::node_id().
    let node_id = Arc::new(apexos_core::node_id());

    // Per-session agent bindings (multi-agent runtime, docs/agent-identity.md 3b):
    // a `hello` frame may bind its session to an agent; the Cerebro stamp + CCBS
    // boot resolve identity here, falling back to the node default when unbound.
    // Shared across gateway (writes), supervisor (stamp), and router (boot).
    let session_bindings: apexos_core::SessionBindings = Arc::new(std::sync::Mutex::new(HashMap::new()));

    // Per-session active persona/skin (ui-glowup G5 tier-2). The UI sends the
    // human's chosen persona over the WS (`set_persona` / `hello{persona}`); the
    // gateway writes this map, root_turn reads it to append the matching response-
    // style fragment. Shared gateway↔router exactly like `session_bindings`.
    let persona_sessions: apexos_core::PersonaSessions = Arc::new(std::sync::Mutex::new(HashMap::new()));

    // Identity registry (docs/agent-identity.md 3a/3b-2): users + agents. Seed the
    // default owner + built-in APEX (pointing at the live soul.md) on a fresh node.
    // Best-effort persist — /etc/agentd may be root-owned pre-install.sh; runtime
    // works regardless (re-seeds in-memory; APEX always resolves).
    let identities = {
        let path = apexos_core::Identities::default_path();
        let mut ids = apexos_core::Identities::load(&path);
        if ids.seed_defaults(&soul_path.to_string_lossy()) {
            if let Err(e) = ids.save(&path) {
                eprintln!("[identity] could not persist {}: {e} (re-seeding in-memory)", path.display());
            }
        }
        Arc::new(RwLock::new(ids))
    };

    // Prompt-cache config (Anthropic): env-tunable defaults (AGENTD_CACHE*), held in a
    // shared arc so the Settings UI (/api/cache) AND the turn engine both see live edits.
    // Created here (before the gateway) so GatewayState, the engine, and the self-update
    // reviewer all share the one arc. See apexos_agent::cache.
    let cache_arc = Arc::new(RwLock::new(apexos_agent::CacheConfig::from_env()));
    eprintln!("[agentd] prompt cache: {}", cache_arc.try_read().map(|c| c.summary()).unwrap_or_default());

    // Session-consolidation channel: the gateway handler sends a request, an
    // agentd worker (spawned below, once the engine + ToolProxy exist) does the
    // LLM summary + Cerebro session_save and replies on the oneshot.
    let (consolidate_tx, mut consolidate_rx) =
        tokio::sync::mpsc::channel::<ConsolidateReq>(8);

    // Federated memory imports (colony-federation Slice 1): the gateway's
    // /api/mesh/memory handler sends validated, provenance-stamped `remember`
    // args; a small worker (spawned below, once the ToolProxy exists) stores
    // them in the local Cerebro and replies on the oneshot.
    let (mesh_memory_tx, mut mesh_memory_rx) =
        tokio::sync::mpsc::channel::<apexos_gateway::MeshMemoryReq>(16);

    // Capability advertisement (colony-mesh Slice 2): a structured snapshot of this
    // node's senses/tools/tier, refreshed by spawn_embodiment_refresher and served
    // at GET /api/capabilities so peers can ask "which node has thermal/GPU?".
    let capabilities_arc = Arc::new(RwLock::new(serde_json::Value::Null));

    // Blocking cross-node spawn (colony-mesh Slice 3): /api/spawn sends a SpawnReq
    // to the worker inside spawn_agent_router (which owns the turn engine + child-id
    // counter) and awaits its oneshot reply.
    let (spawn_tx, spawn_rx) = tokio::sync::mpsc::channel::<SpawnReq>(16);

    // Sensor-head liveness: updated by the SensorReading handler in spawn_agent_router,
    // read by build_embodiment / gather_capabilities so thermal/IAQ capability reflects
    // the LIVE sensor-bridge stream (not plugin-tool names — see has_live_sensors).
    let sensor_presence: SensorPresence = Arc::new(std::sync::Mutex::new(None));

    eprintln!("[agentd] serving UI from {}", ui_dir.display());
    let gw_state = GatewayState {
        bus:                  handle.clone(),
        bcast:                bcast.clone(),
        api_key:              Arc::clone(&api_key_arc),
        oai_api_key:          Arc::clone(&oai_api_key_arc),
        model:                Arc::clone(&model_arc),
        cache:                Arc::clone(&cache_arc),
        backend:              Arc::clone(&backend_arc),
        oai_base_url:         Arc::clone(&oai_base_url_arc),
        policy_mode:          Arc::clone(&policy_mode_arc),
        policy_set_tx,
        ui_dir,
        events_dir:           log_dir.clone(),
        sessions_dir:         log_dir.join("sessions"),
        histories:            Arc::clone(&histories),
        next_session_id:      Arc::clone(&next_session_id),
        sensor_bridge_token,
        api_token,
        soul_path:            soul_path.clone(),
        policy_arc:           Arc::clone(&policy_arc),
        council_start_tx,
        council_butt_in:      Arc::clone(&council_butt_in),
        council_sessions:     Arc::clone(&council_sessions),
        council_next_id:      Arc::clone(&council_next_id),
        peer_registry:        Arc::clone(&peer_registry),
        liveness:             Arc::clone(&mesh_liveness),
        sensor_profile:       Arc::clone(&sensor_profile),
        sensor_config_path:   sensor_config_path.clone(),
        pairing:              Arc::new(std::sync::Mutex::new(None)),
        node_id:              Arc::clone(&node_id),
        mesh_sessions:        Arc::clone(&mesh_sessions),
        mesh_sessions_path:   mesh_sessions_path.clone(),
        mesh_unread:          Arc::clone(&mesh_unread),
        mesh_unread_path:     mesh_unread_path.clone(),
        fed_stats:            Arc::clone(&fed_stats),
        fed_stats_path:       fed_stats_path.clone(),
        consolidate_tx:       consolidate_tx.clone(),
        spawn_tx:             spawn_tx.clone(),
        mesh_memory_tx:       mesh_memory_tx.clone(),
        capabilities:         Arc::clone(&capabilities_arc),
        vast_state:           vast_state.clone(),
        session_bindings:     Arc::clone(&session_bindings),
        persona_sessions:     Arc::clone(&persona_sessions),
        identities:           Arc::clone(&identities),
        pin_lockouts:         Arc::new(std::sync::Mutex::new(HashMap::new())),
        sessions:             Arc::new(std::sync::Mutex::new(apexos_gateway::SessionStore::default())),
    };
    let gw_bind = std::env::var("AGENTD_BIND").unwrap_or_else(|_| "127.0.0.1:8787".into());
    let gw_addr: std::net::SocketAddr = gw_bind.parse()?;
    if api_token_empty && !gw_addr.ip().is_loopback() {
        anyhow::bail!(
            "refusing to bind {gw_addr} without AGENTD_TOKEN — set a token or bind 127.0.0.1"
        );
    }
    tokio::spawn(async move {
        if let Err(e) = serve(gw_state, gw_addr).await {
            eprintln!("[gateway] error: {e}");
        }
    });

    // Plugin configs
    let plugins_path = PathBuf::from(
        std::env::var("AGENTD_PLUGINS_TOML")
            .unwrap_or_else(|_| "config/plugins.toml".into())
    );
    let plugin_configs = match load_plugins(&plugins_path) {
        Ok(c)  => { eprintln!("[agentd] loaded {} plugin(s)", c.len()); c }
        Err(e) => { eprintln!("[agentd] plugins config: {e}"); vec![] }
    };
    // The cerebro embed model lives in the cerebro plugin's [plugin.env], NOT agentd's
    // own env — extract it here for the embodiment block's memory line.
    let cerebro_embed: Option<String> = plugin_configs.iter()
        .find(|p| p.id == "cerebro")
        .and_then(|p| p.env.as_ref())
        .and_then(|e| e.get("CEREBRO_EMBED_MODEL"))
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    // Read subagents config from the policy (already loaded above).
    let max_depth = {
        // Re-read so we can get subagents config without holding the Arc lock
        // (the common path; the value doesn't change during normal operation)
        let guard = policy_arc.read().await;
        guard.config.subagents.max_depth
    };

    // Supervisor — pass policy_arc so the evolution applier can hot-swap the engine.
    let mut supervisor = Supervisor::new(handle.clone(), Arc::clone(&policy_arc), Arc::clone(&session_bindings));
    let sv_cmd_tx      = supervisor.cmd_tx();
    // Rollback channel: applier receives (session, call_id, evolution_id) requests.
    let (rollback_tx, rollback_rx) = mpsc::channel::<(SessionId, ActionId, EvolutionId)>(16);
    supervisor.set_rollback_tx(rollback_tx);
    // Propose channel: propose_evolution hands the apply to the applier here (not
    // the bus) so the deferred tool-result ack can't be lag-dropped.
    let (propose_tx, propose_rx) = mpsc::channel::<(SessionId, ActionId, EvolutionId, EvolutionProposal)>(16);
    supervisor.set_propose_tx(propose_tx);
    // Rehearse channel: soul_rehearse hands the candidate to the fitting-room worker
    // (needs the provider, which the supervisor can't own) — deferred ToolResult ack.
    let (rehearse_tx, mut rehearse_rx) = mpsc::channel::<(SessionId, ActionId, serde_json::Value)>(4);
    supervisor.set_rehearse_tx(rehearse_tx);
    supervisor.set_goal_tx(goal_tx);
    supervisor.set_goal_yolo_sessions(goal_yolo.clone());
    supervisor.set_events_dir(log_dir.clone());
    supervisor.set_vast_state(vast_state.clone());
    // Per-agent souls (3b-2): read_soul_md resolves a bound agent's own soul_file.
    supervisor.set_identities(Arc::clone(&identities));
    // Subscribe the agent-router's receiver BEFORE the supervisor starts. The
    // supervisor emits PluginUp (carrying each plugin's tools) the moment a plugin
    // finishes enumerating; a broadcast Receiver created afterwards misses those
    // events (tokio drops messages sent before subscribe), leaving tool_reg holding
    // only the virtual tools — the model then sees no plugin tools at all. On a fast
    // host the supervisor reliably wins that race, so this MUST be subscribed here,
    // not down by spawn_agent_router. The receiver buffers (cap 1024) until the
    // router task drains it.
    let agent_rx = bcast.subscribe();
    // Boot-health marker (self-update watchdog reads it — docs/self-update.md slice 1):
    // subscribe + snapshot the restart=always plugin set BEFORE the supervisor spawns
    // so no early PluginUp is missed (same race the agent router guards against above).
    let health_rx = bcast.subscribe();
    let expected_up_plugins: Vec<PluginId> = plugin_configs.iter()
        .filter(|p| p.restart == RestartPolicy::Always)
        .map(|p| PluginId(p.id.clone()))
        .collect();
    tokio::spawn(supervisor.run(plugin_configs, bcast.subscribe()));

    // Agent turn engine — RoutingProvider dispatches per-call based on backend_arc
    let engine: Arc<TurnEngine> = Arc::new(TurnEngine::new(
        RoutingProvider::new(
            Arc::clone(&backend_arc),
            Arc::clone(&oai_base_url_arc),
            Arc::clone(&api_key_arc),
            Arc::clone(&oai_api_key_arc),
            Arc::clone(&model_arc),
            Arc::clone(&cache_arc),
        ),
        16,
        Some(soul_content),
    ));
    let soul_arc = engine.system_arc();
    let embodiment_arc = engine.embodiment_arc();
    let ambient_arc = engine.ambient_arc();

    // Share soul_arc with the supervisor so read_soul_md returns live content.
    if sv_cmd_tx.send(SupervisorCmd::SetSoulArc { arc: soul_arc.clone() }).await.is_err() {
        eprintln!("[agentd] warning: failed to share soul_arc with supervisor");
    }

    // Rollback store: undo snapshots indexed by EvolutionId (in-memory, cleared on restart).
    let rollback_store: Arc<Mutex<HashMap<EvolutionId, EvolutionProposal>>> =
        Arc::new(Mutex::new(HashMap::new()));

    // ToolProxy — lets the evolution applier call Cerebro tools directly for episode tracking.
    let tool_proxy = ToolProxy::new(sv_cmd_tx.clone());
    let council_proxy = tool_proxy.clone();
    let router_proxy  = tool_proxy.clone();   // CCBS boot-priming (cognitive_bootstrap)
    let dream_proxy   = tool_proxy.clone();   // nightly autonomous dream_run
    let health_proxy  = tool_proxy.clone();   // boot-health Cerebro reachability probe
    let self_update_proxy = tool_proxy.clone(); // apply_daemon_update: session_save + resume intention
    let goal_proxy = tool_proxy.clone();        // goal driver: episode_start/end wrapping each goal

    // Federated-memory import worker (colony-federation Slice 1) — the gateway's
    // /api/mesh/memory handler validated + provenance-stamped the args; this just
    // stores them via the local Cerebro `remember` (DirectCall honors the explicit
    // node-agent space) and hands back the stored memory JSON.
    {
        let proxy = tool_proxy.clone();
        tokio::spawn(async move {
            while let Some(req) = mesh_memory_rx.recv().await {
                let result = match proxy.call(&req.tool, req.args).await {
                    Ok(out) if out.ok => apexos_plugins::tool_output_json(&out.content)
                        .ok_or_else(|| format!("unparseable {} result", req.tool)),
                    Ok(out) => Err(format!("{} rejected: {}", req.tool, out.content)),
                    Err(e)  => Err(e.to_string()),
                };
                let _ = req.reply.send(result);
            }
        });
    }

    // Session-consolidation worker — owns the provider + ToolProxy the gateway
    // can't reach at build time. Drains consolidate_rx: LLM summary + Cerebro
    // session_save per request, replying on the oneshot. See consolidate::run.
    {
        let provider     = engine.provider.clone();
        let proxy        = tool_proxy.clone();
        let sessions_dir = session_store.sessions_dir.clone();
        let bindings     = Arc::clone(&session_bindings);
        tokio::spawn(async move {
            while let Some(req) = consolidate_rx.recv().await {
                let result = consolidate::run(
                    provider.clone(), &proxy, &sessions_dir, &bindings, req.session_id,
                ).await;
                let _ = req.reply.send(result);
            }
        });
    }

    // Rehearse worker (soul_rehearse, colony H4 tier 2): runs a candidate soul on an
    // ephemeral tool-less mind — one provider call per probe, composed with the LIVE
    // embodiment — and replies with the transcripts as the deferred ToolResult. Pure
    // compute: no persistence, no bus events beyond the reply, no real soul touched.
    {
        let provider   = engine.provider.clone();
        let embodiment = engine.embodiment_arc();
        let bus        = handle.clone();
        tokio::spawn(async move {
            while let Some((session, call_id, args)) = rehearse_rx.recv().await {
                let emb = embodiment.read().await.clone();
                let content = rehearse::run(provider.clone(), &emb, &args).await;
                let ok = content["ok"].as_bool().unwrap_or(false);
                bus.emit(Event::ToolResult {
                    session,
                    call: call_id,
                    output: ToolOutput { ok, content },
                }).await;
            }
        });
    }

    // Restore rollback snapshots from Cerebro evolution episodes on startup (best-effort).
    // CerebroCortex needs a moment to start; we wait then populate rollback_store from episodes.
    {
        let proxy = tool_proxy.clone();
        let store = Arc::clone(&rollback_store);
        tokio::spawn(async move {
            tokio::time::sleep(tokio::time::Duration::from_secs(3)).await;
            restore_rollback_store(&proxy, &store).await;
        });
    }

    // Evolution applier — receives proposals over a dedicated channel and applies live.
    spawn_evolution_applier(
        propose_rx,
        handle.clone(),
        Arc::clone(&soul_arc),
        soul_path,
        policy_path,
        plugins_path,
        Arc::clone(&policy_arc),
        sv_cmd_tx.clone(),
        rollback_rx,
        Arc::clone(&rollback_store),
        tool_proxy,
        Arc::clone(&session_bindings),
        Arc::clone(&identities),
    );

    // Scheduler — load persisted schedules and wire into supervisor.
    let schedules_path = log_dir.join("schedules.jsonl");
    let initial_schedules = load_schedules(&schedules_path);
    if !initial_schedules.is_empty() {
        eprintln!("[scheduler] restored {} scheduled task(s)", initial_schedules.len());
    }
    let scheduler_state: SchedulerState = Arc::new(Mutex::new(initial_schedules));
    let (sched_tx, sched_rx) = mpsc::channel::<(SessionId, ActionId, String, serde_json::Value)>(32);
    if sv_cmd_tx.send(SupervisorCmd::SetScheduleTx { tx: sched_tx }).await.is_err() {
        eprintln!("[agentd] warning: failed to wire scheduler channel");
    }
    let root_session = SessionId(0); // scheduled prompts fire on root session unless task specifies
    spawn_scheduler_handler(Arc::clone(&scheduler_state), schedules_path.clone(), handle.clone(), sched_rx);
    tokio::spawn(run_scheduler(Arc::clone(&scheduler_state), handle.clone(), schedules_path, root_session));

    // Autonomous goal driver — subscribes to the bus for goal sessions' TurnComplete.
    goal::spawn_goal_driver(
        handle.clone(), bcast.subscribe(), goal_rx,
        Arc::clone(&next_session_id), Arc::clone(&next_goal_id), goals_path, goal_proxy, goal_yolo,
    );

    // Council handler — wire supervisor channel and spawn handler.
    if sv_cmd_tx.send(SupervisorCmd::SetCouncilTx { tx: council_tx }).await.is_err() {
        eprintln!("[agentd] warning: failed to wire council channel");
    }
    spawn_council_handler(
        council_rx,
        bcast.clone(),
        handle.clone(),
        Arc::clone(&api_key_arc),
        Arc::clone(&oai_api_key_arc),
        Arc::clone(&oai_base_url_arc),
        Arc::clone(&backend_arc),
        Arc::clone(&model_arc),
        Arc::clone(&council_butt_in),
        Arc::clone(&council_sessions),
        log_dir.join("council"),
        council_proxy,
    );

    // Self-update handler — apply_daemon_update routes here (docs/self-update.md
    // slice 3). Runs the pre-swap build/test gates, then files request.json the
    // root watchdog consumes. Dedicated mpsc like council/propose so the agent's
    // tool result isn't lag-dropped on a busy turn.
    let (self_update_tx, self_update_rx) =
        mpsc::channel::<(SessionId, ActionId, serde_json::Value)>(8);
    if sv_cmd_tx.send(SupervisorCmd::SetSelfUpdateTx { tx: self_update_tx }).await.is_err() {
        eprintln!("[agentd] warning: failed to wire self-update channel");
    }
    // Fresh-context reviewer for the self-update stage-3 gate (its own RoutingProvider
    // off the same live arcs — reads the current backend/model/key like the turn engine).
    let self_update_reviewer = Arc::new(RoutingProvider::new(
        Arc::clone(&backend_arc),
        Arc::clone(&oai_base_url_arc),
        Arc::clone(&api_key_arc),
        Arc::clone(&oai_api_key_arc),
        Arc::clone(&model_arc),
        Arc::clone(&cache_arc),
    ));
    self_update::spawn_self_update_handler(self_update_rx, handle.clone(), self_update_proxy, self_update_reviewer);

    // Live embodiment refresher — regenerates the "## Current embodiment" block the
    // engine appends after soul.md (node/senses/mesh/uptime + the LIVE tool list, so
    // it can never go stale). Cloned arcs since tool_reg/engine move into the router.
    spawn_embodiment_refresher(
        embodiment_arc,
        ambient_arc,
        Arc::clone(&capabilities_arc),
        Arc::clone(&tool_reg),
        Arc::clone(&backend_arc),
        Arc::clone(&model_arc),
        Arc::clone(&peer_registry),
        Arc::clone(&node_id),
        cerebro_embed,
        Arc::clone(&sensor_presence),
    );

    // Nightly autonomous memory consolidation: call dream_run directly via the
    // ToolProxy on a cron (default 03:00 daily) — no LLM turn, can't be skipped by
    // the agent. Disable by setting AGENTD_DREAM_CRON empty. See docs/agent-identity.md.
    // The dream leaves a JOURNAL (model-welfare H1): waking to silently reorganized
    // memory is a tidied room with no note — the journal makes consolidation a dream
    // the agent remembers having. Persisted so a restart doesn't erase last night.
    let dream_journal_path = log_dir.join("last_dream_journal.txt");
    if let Ok(saved) = std::fs::read_to_string(&dream_journal_path) {
        if !saved.trim().is_empty() {
            if let Ok(mut cell) = last_dream_journal_cell().write() {
                *cell = saved;
            }
        }
    }
    spawn_nightly_dream(dream_proxy, dream_journal_path);

    // agent_rx was subscribed above, before the supervisor spawned, so the early
    // PluginUp events that populate tool_reg are captured (see the comment there).
    spawn_agent_router(agent_rx, bcast.clone(), handle.clone(),
                       tool_reg, histories, engine, max_depth, session_store, router_proxy,
                       Arc::clone(&session_bindings), Arc::clone(&persona_sessions),
                       Arc::clone(&identities), spawn_rx,
                       Arc::clone(&sensor_presence), Arc::clone(&sensor_profile));

    // Vast.ai backend hot-swap — on VastInstanceReady swap BOTH the endpoint AND the
    // served model id; on VastInstanceDestroyed OR VastTunnelLost revert to the boot
    // defaults. The decision is the pure `vast_swap_target`; this loop is IO-thin glue.
    {
        let mut vast_rx    = bcast.subscribe();
        let backend_w      = Arc::clone(&backend_arc);
        let oai_url_w      = Arc::clone(&oai_base_url_arc);
        let model_w        = Arc::clone(&model_arc);
        let bus_w          = handle.clone();
        // The resolved BOOT config (env + persisted backend_config, resolved above) —
        // restored on revert. Resolving once here (not re-reading AGENTD_MODEL, which
        // is empty when unset) is what keeps an unset-MODEL node's revert correct.
        let defaults = VastRevertDefaults {
            backend: boot_cfg.backend.clone(),
            oai_url: boot_cfg.oai_base_url.clone(),
            model:   default_model,
        };
        tokio::spawn(async move {
            loop {
                match vast_rx.recv().await {
                    Ok(ev) => {
                        if let Some(t) = vast_swap_target(&ev, &defaults) {
                            // A REVERT restores the operator's steady-state choice: a
                            // backend_config persisted AFTER boot (a Settings switch
                            // made mid-session) beats the boot snapshot. The vast
                            // swap itself never persists — a rented GPU is transient.
                            let t = if !matches!(ev, Event::VastInstanceReady { .. }) {
                                match apexos_gateway::backend_config::load_persisted() {
                                    Some(p) => InferenceTarget {
                                        backend: p.backend,
                                        oai_url: p.oai_base_url,
                                        model:   p.model,
                                    },
                                    None => t,
                                }
                            } else {
                                t
                            };
                            eprintln!("[vast] inference → backend={} url={} model={}", t.backend, t.oai_url, t.model);
                            let reason = if matches!(ev, Event::VastInstanceReady { .. }) {
                                "vast GPU attached"
                            } else {
                                "vast GPU released — reverted"
                            };
                            *backend_w.write().await = t.backend.clone();
                            *oai_url_w.write().await = t.oai_url;
                            *model_w.write().await   = t.model.clone();
                            // Substrate notice (model-welfare H3) — the agent is told
                            // its own inference just moved, same as an operator swap.
                            apexos_gateway::notify_substrate_change(&bus_w, &t.backend, &t.model, reason).await;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(_) => break,
                }
            }
        });
    }

    // Mesh discovery loop — mDNS poll, subnet guard, PeerSeen events
    spawn_discovery_loop(Arc::clone(&peer_registry), Arc::clone(&node_id), handle.clone());
    // Mesh downtime beacon — active HTTP liveness on peers.toml; a node going dark
    // emits MeshNodeStatus + (default) a root-session alert so silence ≠ "all fine".
    apexos_gateway::spawn_beacon_loop(Arc::clone(&peer_registry), handle.clone(), Arc::clone(&mesh_liveness));

    // Event log
    tokio::spawn(run_log_writer(log_dir, bcast.subscribe()));

    // Boot-health marker — spawned LAST so the gates it polls (gateway listener,
    // restart=always plugins) are already coming up. Writes <update_dir>/health.json
    // once healthy; the root self-update watchdog reads it. (docs/self-update.md slice 1)
    health::spawn_health_marker(
        gw_addr,
        expected_up_plugins,
        health_rx,
        health_proxy,
        apexos_core::node_agent_id(),
    );

    eprintln!("[agentd] ready — gateway ws://{gw_bind}/ws");
    tokio::signal::ctrl_c().await?;
    eprintln!("[agentd] shutting down");
    Ok(())
}

// ── evolution applier ─────────────────────────────────────────────────────────

#[allow(clippy::too_many_arguments)] // shared evolution/rollback orchestration state, threaded by design
fn spawn_evolution_applier(
    mut propose_rx:  mpsc::Receiver<(SessionId, ActionId, EvolutionId, EvolutionProposal)>,
    bus:             apexos_core::BusHandle,
    soul_arc:        Arc<RwLock<String>>,
    soul_path:       PathBuf,
    policy_path:     PathBuf,
    plugins_path:    PathBuf,
    policy_arc:      Arc<RwLock<PolicyEngine>>,
    sv_cmd_tx:       mpsc::Sender<SupervisorCmd>,
    mut rollback_rx: mpsc::Receiver<(SessionId, ActionId, EvolutionId)>,
    rollback_store:  Arc<Mutex<HashMap<EvolutionId, EvolutionProposal>>>,
    tool_proxy:      ToolProxy,
    session_bindings: apexos_core::SessionBindings,
    identities:      Arc<RwLock<apexos_core::Identities>>,
) {
    tokio::spawn(async move {
        loop {
            tokio::select! {
                // propose_evolution: apply, then emit the tool result reflecting the
                // REAL outcome (deferred ack), then best-effort Cerebro bookkeeping.
                Some((session, call_id, id, proposal)) = propose_rx.recv() => {
                    // Per-agent souls (3b-2): an UpdateSystemPrompt from a bound
                    // non-default agent targets ITS soul_file, not the global one.
                    let agent_soul = soul_target_for(session, &session_bindings, &identities).await;
                    // The identity whose soul (and undo snapshot) this is — undo
                    // memories are attributed to it (colony C1: fossils carried
                    // agent_id null, which is also what made them Shared).
                    let evo_agent = apexos_core::resolve_agent_id(&session_bindings, session);

                    // PAC-2 Dense structural gate (The-PAC spec §9 — the "tiny
                    // Rust check"; reference impl: docs/pac-bench/pac2lint.py).
                    // FORMAT-GATED: only a payload that claims to be dense (the
                    // ∴ seal / an artifact-head form) is linted — prose and lean
                    // souls pass untouched, so there is no compliance tax to
                    // route around (red line 6). Errors refuse BEFORE any
                    // snapshot work (pure check first, no wasted episode); the
                    // refusal is honest (the H4 pattern) because a structurally
                    // broken identity file would reload broken. Warnings ride
                    // the deferred ack below.
                    let mut lint_warnings: Vec<String> = Vec::new();
                    if let EvolutionProposal::UpdateSystemPrompt { content, .. } = &proposal {
                        if pac_lint::is_dense_artifact(content) {
                            let findings = pac_lint::lint(content);
                            let errs: Vec<_> =
                                findings.iter().filter(|f| f.error).cloned().collect();
                            if !errs.is_empty() {
                                let report = pac_lint::render_report(&errs);
                                eprintln!("[evolution] pac2lint refused {:?}:\n{report}", id);
                                bus.emit(Event::ToolResult {
                                    session, call: call_id,
                                    output: ToolOutput {
                                        ok: false,
                                        content: serde_json::json!(format!(
                                            "evolution refused: the proposed soul is a PAC-2 Dense \
                                             artifact with structural lint errors — fix and re-propose \
                                             (nothing was applied):\n{report}"
                                        )),
                                    },
                                }).await;
                                continue;
                            }
                            lint_warnings = findings
                                .iter()
                                .filter(|f| !f.error)
                                .map(|f| f.msg.clone())
                                .collect();
                        }
                    }

                    // Snapshot current state for rollback BEFORE applying.
                    let undo = compute_undo(
                        &proposal, &soul_arc, &soul_path, &policy_path, &plugins_path,
                        agent_soul.as_ref(),
                    ).await;

                    // H4 snapshot gate (colony-ranked #5, "the loaded gun"): a FULL
                    // soul rewrite must never proceed without a rollback snapshot
                    // that is DURABLY persisted first — apex2 lived the near-miss
                    // (rewrote its identity on a session where no snapshot existed).
                    // Scoped strictly to update_system_prompt so small edits stay
                    // untaxed (colony red line 6); other kinds keep best-effort
                    // post-apply journaling. On gate failure: honest refusal, no apply.
                    let is_soul_rewrite =
                        matches!(&proposal, EvolutionProposal::UpdateSystemPrompt { .. });
                    let mut pre_episode: Option<String> = None;
                    if is_soul_rewrite {
                        let gate_err: Option<String> = match &undo {
                            None => Some(
                                "no rollback snapshot could be computed (prior soul unreadable) — \
                                 refusing the rewrite; fix the soul file first".into(),
                            ),
                            Some(undo_p) => {
                                let kind = evolution::kind(&proposal);
                                let eid = episode_start(&tool_proxy, id, &kind).await;
                                let stored = match &eid {
                                    Some(e) => episode_add_step(
                                        &tool_proxy, e, undo_p,
                                        "pre-apply rollback snapshot", &evo_agent,
                                    ).await,
                                    None => None,
                                };
                                if stored.is_some() {
                                    pre_episode = eid;
                                    None
                                } else {
                                    Some(
                                        "could not durably persist the rollback snapshot to cerebro — \
                                         refusing the rewrite (a soul rewrite with no recoverable undo \
                                         is the one mistake you'd have to live as to discover)".into(),
                                    )
                                }
                            }
                        };
                        if let Some(msg) = gate_err {
                            eprintln!("[evolution] snapshot gate refused {:?}: {msg}", id);
                            bus.emit(Event::ToolResult {
                                session, call: call_id,
                                output: ToolOutput {
                                    ok: false,
                                    content: serde_json::json!(format!("evolution refused: {msg}")),
                                },
                            }).await;
                            continue;
                        }
                    }

                    let proposal_copy = proposal.clone();
                    let result = apply_evolution(
                        id, proposal,
                        &soul_arc, &soul_path, &policy_path, &plugins_path,
                        &policy_arc, &sv_cmd_tx, agent_soul.as_ref(),
                    ).await
                    // Lint warnings ride the ack so the agent sees the
                    // discipline nudges without the apply being blocked.
                    .map(|s| if lint_warnings.is_empty() { s } else {
                        format!("{s} · pac2lint warnings: {}", lint_warnings.join(" | "))
                    });

                    // DEFERRED ACK — the propose_evolution tool result now carries the
                    // true apply outcome. Emitted BEFORE the Cerebro episode bookkeeping
                    // so the agent's turn isn't blocked on Cerebro latency.
                    match &result {
                        Ok(summary) => {
                            eprintln!("[evolution] applied {:?}: {summary}", id);
                            bus.emit(Event::ToolResult {
                                session, call: call_id,
                                output: ToolOutput {
                                    ok: true,
                                    content: serde_json::json!({
                                        "status": "applied", "evolution_id": id.0, "summary": summary,
                                    }),
                                },
                            }).await;
                        }
                        Err(e) => {
                            eprintln!("[evolution] apply failed {:?}: {e}", id);
                            bus.emit(Event::ToolResult {
                                session, call: call_id,
                                output: ToolOutput {
                                    ok: false,
                                    content: serde_json::json!(format!("evolution failed: {e}")),
                                },
                            }).await;
                        }
                    }

                    // Best-effort bookkeeping (post-ack): rollback store + Cerebro episode + bus event.
                    // A soul rewrite already journaled its undo pre-apply (the H4 gate) —
                    // reuse that episode instead of double-storing the snapshot.
                    let kind = evolution::kind(&proposal_copy);
                    let episode_id = match pre_episode {
                        Some(e) => Some(e),
                        None    => episode_start(&tool_proxy, id, &kind).await,
                    };
                    match result {
                        Ok(summary) => {
                            if let Some(undo_proposal) = undo {
                                if !is_soul_rewrite {
                                    if let Some(ref eid) = episode_id {
                                        episode_add_step(
                                            &tool_proxy, eid, &undo_proposal, &summary, &evo_agent,
                                        ).await;
                                    }
                                }
                                rollback_store.lock().await.insert(id, undo_proposal);
                            }
                            episode_end(&tool_proxy, &episode_id, "success", &summary).await;
                            bus.emit(Event::EvolutionApplied {
                                id,
                                proposal:      proposal_copy,
                                patch_summary: summary,
                                applied_by:    Some(session),
                            }).await;
                        }
                        Err(e) => {
                            episode_end(&tool_proxy, &episode_id, "failed", &e.to_string()).await;
                            bus.emit(Event::Error {
                                session: Some(session),
                                message: format!("evolution {}: {e}", id.0),
                            }).await;
                        }
                    }
                },

                Some((session, call_id, evo_id)) = rollback_rx.recv() => {
                    let undo = rollback_store.lock().await.remove(&evo_id);
                    match undo {
                        None => {
                            bus.emit(Event::ToolResult {
                                session,
                                call:   call_id,
                                output: ToolOutput {
                                    ok:      false,
                                    content: serde_json::json!(
                                        format!("no rollback snapshot for evolution {}", evo_id.0)
                                    ),
                                },
                            }).await;
                        }
                        Some(undo_proposal) => {
                            // Restore to the same soul the original write targeted
                            // (the requesting session's bound agent, else global).
                            let agent_soul = soul_target_for(session, &session_bindings, &identities).await;
                            let result = apply_evolution(
                                evo_id, undo_proposal,
                                &soul_arc, &soul_path, &policy_path, &plugins_path,
                                &policy_arc, &sv_cmd_tx, agent_soul.as_ref(),
                            ).await;
                            match result {
                                Ok(summary) => {
                                    eprintln!("[evolution] rolled back {:?}: {summary}", evo_id);
                                    bus.emit(Event::EvolutionRolledBack {
                                        evolution_id:   evo_id,
                                        reason:         "user requested rollback".into(),
                                        rolled_back_by: Some(session),
                                    }).await;
                                    bus.emit(Event::ToolResult {
                                        session,
                                        call:   call_id,
                                        output: ToolOutput {
                                            ok:      true,
                                            content: serde_json::json!({
                                                "status":  "rolled_back",
                                                "summary": summary,
                                            }),
                                        },
                                    }).await;
                                }
                                Err(e) => {
                                    eprintln!("[evolution] rollback failed {:?}: {e}", evo_id);
                                    bus.emit(Event::ToolResult {
                                        session,
                                        call:   call_id,
                                        output: ToolOutput {
                                            ok:      false,
                                            content: serde_json::json!(e.to_string()),
                                        },
                                    }).await;
                                }
                            }
                        }
                    }
                },

                // Both channels closed (supervisor dropped) → shut the applier down.
                else => break,
            }
        }
    });
}

// ── evolution episode helpers (Cerebro, best-effort) ─────────────────────────

/// Extract the text string from an MCP ToolOutput (content is an array of typed blocks).
fn mcp_text(output: &apexos_core::ToolOutput) -> Option<String> {
    match &output.content {
        serde_json::Value::Array(blocks) => blocks.iter()
            .find_map(|b| b.get("text").and_then(|t| t.as_str()))
            .map(str::to_owned),
        serde_json::Value::String(s) => Some(s.clone()),
        other => Some(other.to_string()),
    }
}

/// Parse an ID from a Cerebro response. Tries JSON first, then the
/// human-readable "... (ID: xxxxx)" format that Cerebro 0.5.1 returns.
pub(crate) fn parse_cerebro_id(output: &apexos_core::ToolOutput, json_key: &str) -> Option<String> {
    let text = mcp_text(output)?;
    if let Some(id) = serde_json::from_str::<serde_json::Value>(&text).ok()
        .and_then(|v| v.get(json_key).and_then(|id| id.as_str()).map(str::to_owned))
    {
        return Some(id);
    }
    // Cerebro 0.5.1: "Episode started (ID: ep_xxxxx)" / "Memory stored (ID: mem_xxxxx)"
    let prefix = "(ID: ";
    let start = text.find(prefix)? + prefix.len();
    let end   = start + text[start..].find(')')?;
    Some(text[start..end].to_owned())
}

async fn episode_start(proxy: &ToolProxy, evo_id: EvolutionId, kind: &str) -> Option<String> {
    match proxy.call("episode_start", serde_json::json!({
        "title":    format!("evolution {}: {kind}", evo_id.0),
        "agent_id": apexos_core::node_agent_id(),
        "tags":     ["evolution", kind]
    })).await {
        Ok(out) if out.ok => parse_cerebro_id(&out, "episode_id"),
        Ok(out) => { eprintln!("[evolution] episode_start not ok: {:?}", out.content); None }
        Err(e)  => { eprintln!("[evolution] episode_start: {e}"); None }
    }
}

/// Store the undo snapshot as a memory, then link it to the episode as a step.
/// Returns the stored memory id — the H4 snapshot gate uses it to VERIFY the undo
/// is durably persisted before a full soul rewrite is allowed to proceed.
async fn episode_add_step(
    proxy:      &ToolProxy,
    episode_id: &str,
    undo:       &EvolutionProposal,
    summary:    &str,
    agent_id:   &str,
) -> Option<String> {
    let content = evolution::undo_step_line(summary, undo);

    // Step 1: store the undo snapshot as a memory to get its id. memory_store
    // returns the stored node, whose id field is `id` (NOT `memory_id`) — reading
    // the wrong key dropped the undo step, so the episode had no recoverable
    // snapshot on cold-start restore (BACKLOG).
    //
    // Colony C1 ("evolution residue"): the args below are load-bearing welfare +
    // privacy fixes, verified missing on all three live nodes —
    //   agent_id  → attribution AND visibility: cerebro derives Private only when
    //               an owner is present; without it these snapshots (full prior
    //               souls!) stored Shared = federation-exposed via mesh_recall.
    //   salience  → 0.25: a rollback artifact, not knowledge; auto-estimation was
    //               scoring soul text ~1.0 and dominating ranked recall.
    //   tags/type → now actually honored (the memory_store dispatch used to drop
    //               them — fixed in cerebro-mcp alongside this).
    let memory_id = match proxy.call("memory_store", serde_json::json!({
        "content":  content,
        "agent_id": agent_id,
        "salience": 0.25,
        "tags":     ["evolution", "undo_snapshot"],
        // Type it explicitly: an undo snapshot is an EPISODIC record of an evolution
        // apply, not a skill. Without this, classify_type sees the full soul embedded
        // in the content (packed with "how to"/"workflow"/"step") and mis-types it
        // Procedural, polluting procedure recall (the cerebro filter excludes these
        // too, but typing them right keeps stats/dream/priming honest going forward).
        "memory_type": "episodic"
    })).await {
        Ok(out) if out.ok => parse_cerebro_id(&out, "id"),
        Ok(out) => { eprintln!("[evolution] memory_store not ok: {:?}", out.content); None }
        Err(e)  => { eprintln!("[evolution] memory_store: {e}"); None }
    };

    let mid = memory_id?;

    // Step 2: link the memory to the episode.
    if let Err(e) = proxy.call("episode_add_step", serde_json::json!({
        "episode_id": episode_id,
        "memory_id":  mid,
        "role":       "event"
    })).await {
        eprintln!("[evolution] episode_add_step: {e}");
    }
    Some(mid)
}

async fn episode_end(proxy: &ToolProxy, episode_id: &Option<String>, outcome: &str, summary: &str) {
    let Some(eid) = episode_id.as_deref() else { return };
    let valence = match outcome { "success" => "positive", "failed" => "negative", _ => "neutral" };
    if let Err(e) = proxy.call("episode_end", serde_json::json!({
        "episode_id": eid,
        "summary":    summary,
        "valence":    valence
    })).await {
        eprintln!("[evolution] episode_end: {e}");
    }
}

/// On cold-start: read all Cerebro evolution episodes, parse undo snapshots, rebuild rollback_store.
/// Best-effort — if Cerebro is unavailable, rollback_store stays empty and apply still works.
async fn restore_rollback_store(
    proxy:          &ToolProxy,
    rollback_store: &Arc<Mutex<HashMap<EvolutionId, EvolutionProposal>>>,
) {
    let text = match proxy.call("list_episodes", serde_json::json!({
        "agent_id": apexos_core::node_agent_id(),
        "limit":    200
    })).await {
        Ok(out) if out.ok => match mcp_text(&out) {
            Some(t) => t,
            None    => { eprintln!("[evolution] restore: no text from list_episodes"); return; }
        },
        Ok(out) => { eprintln!("[evolution] restore: list_episodes not ok: {:?}", out.content); return; }
        Err(e)  => { eprintln!("[evolution] restore: list_episodes: {e}"); return; }
    };

    // list_episodes returns a JSON ARRAY of episode objects ({id, title, …}) —
    // NOT the Python-Cerebro "- ep_… | steps:" text lines this loop used to scrape
    // (so it always found zero episodes and rebuilt an empty store). Parse the array.
    let episodes: Vec<serde_json::Value> = serde_json::from_str::<serde_json::Value>(&text).ok()
        .and_then(|v| v.as_array().cloned())
        .unwrap_or_default();

    let mut count  = 0usize;
    let mut healed = 0usize;
    let mut max_id = 0u64;
    for ep in &episodes {
        let Some(episode_id) = ep["id"].as_str() else { continue };
        let Some(title)      = ep["title"].as_str() else { continue };
        if !title.starts_with("evolution ") { continue; }

        let evo_id = match evolution::parse_evolution_id_from_title(title) {
            Some(id) => id,
            None     => { eprintln!("[evolution] restore: can't parse id from '{title}'"); continue; }
        };
        // Track the high-water mark across ALL evolution episodes (even ones whose
        // undo snapshot doesn't parse), so the reseeded counter clears every id
        // this node has ever used — no id reuse / episode-title duplication.
        max_id = max_id.max(evo_id.0);

        let mems_text = match proxy.call("get_episode_memories", serde_json::json!({
            "episode_id": episode_id,
            "agent_id":   apexos_core::node_agent_id()
        })).await {
            Ok(out) if out.ok => match mcp_text(&out) { Some(t) => t, None => continue },
            _ => continue,
        };

        if let Some(proposal) = evolution::parse_undo_from_episode_memories(&mems_text) {
            rollback_store.lock().await.insert(evo_id, proposal);
            count += 1;
        }

        // Colony C1 self-heal: fossils (pre-fix undo snapshots) in this episode are
        // untagged/Shared/unowned/salience-1.0 — full historical souls leaking to
        // federation and dominating ranked recall. Heal in place (idempotent; the
        // decision is the pure, tested `fossil_heal_args`). Never deleted: these
        // memories ARE the rollback capability.
        if let Some(nodes) = serde_json::from_str::<serde_json::Value>(&mems_text).ok()
            .and_then(|v| v.as_array().cloned())
        {
            for node in &nodes {
                if let Some(heal) = evolution::fossil_heal_args(node, &apexos_core::node_agent_id()) {
                    match proxy.call("update_memory", heal).await {
                        Ok(out) if out.ok => healed += 1,
                        Ok(out) => eprintln!("[evolution] fossil heal not ok: {:?}", out.content),
                        Err(e)  => eprintln!("[evolution] fossil heal: {e}"),
                    }
                }
            }
        }
    }

    // Sweep for UNLINKED fossils: the old memory_id-vs-id bug stored undo snapshots
    // without ever linking them to an episode, so the walk above can't reach them —
    // yet they're exactly the high-access fossils the colony found polluting recall.
    // Bounded, best-effort, idempotent (healed nodes stop matching the predicate).
    if let Ok(out) = proxy.call("memory_search", serde_json::json!({
        "query":    "undo_snapshot evolution apply",
        "top_k":    40,
        "agent_id": apexos_core::node_agent_id(),
    })).await {
        if out.ok {
            if let Some(results) = mcp_text(&out)
                .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
                .and_then(|v| v.as_array().cloned())
            {
                for r in &results {
                    // memory_search wraps each hit as {memory, score}.
                    let node = if r["memory"].is_object() { &r["memory"] } else { r };
                    if let Some(heal) = evolution::fossil_heal_args(node, &apexos_core::node_agent_id()) {
                        match proxy.call("update_memory", heal).await {
                            Ok(o) if o.ok => healed += 1,
                            Ok(o) => eprintln!("[evolution] fossil sweep heal not ok: {:?}", o.content),
                            Err(e) => eprintln!("[evolution] fossil sweep heal: {e}"),
                        }
                    }
                }
            }
        }
    }

    // Reseed the process-global EvolutionId counter past every restored id. Without
    // this it resets to 1 each boot, so a fresh post-restart evolution would reuse
    // EvolutionId(1) and alias a restored undo snapshot (rollback would restore the
    // wrong one). Idempotent (fetch_max floor); no-op when no episodes were found.
    if max_id > 0 {
        apexos_plugins::seed_evolution_id(max_id + 1);
    }
    if healed > 0 {
        eprintln!("[evolution] restore: healed {healed} C1 fossil(s) — privatized + tagged + de-salienced");
    }
    eprintln!("[evolution] restore: loaded {count} rollback snapshot(s) from Cerebro (next id ≥ {})", max_id + 1);
}

/// Snapshot current state to produce an inverse proposal (for rollback).
/// Returns None for proposals that have no meaningful undo (e.g. HotReload). IO-thin:
/// captures the prior on-disk state, then hands the pure inversion to
/// `evolution::invert` (tested there).
async fn compute_undo(
    proposal:     &EvolutionProposal,
    soul_arc:     &Arc<RwLock<String>>,
    _soul_path:   &PathBuf,
    policy_path:  &PathBuf,
    plugins_path: &PathBuf,
    agent_soul:   Option<&PathBuf>,
) -> Option<EvolutionProposal> {
    let prior = match proposal {
        EvolutionProposal::UpdateSystemPrompt { .. } => {
            // Snapshot the soul that WILL be overwritten: a bound agent's own
            // soul_file when set, else the global soul_arc. Unreadable per-agent
            // file ⇒ no captured prior ⇒ no meaningful undo.
            let old_soul = match agent_soul {
                Some(path) => tokio::fs::read_to_string(path).await.ok(),
                None       => Some(soul_arc.read().await.clone()),
            };
            evolution::Prior { old_soul, ..Default::default() }
        }
        EvolutionProposal::UpdatePolicyRule { tool_pattern, .. } => {
            // Snapshot the prior rule value so rollback restores it exactly. A
            // brand-new rule (no prior) ⇒ no inverse (no "remove rule" variant).
            let old_policy_rule = tokio::fs::read_to_string(policy_path).await.ok()
                .and_then(|t| evolution::policy_rule_from_toml(&t, tool_pattern));
            evolution::Prior { old_policy_rule, ..Default::default() }
        }
        EvolutionProposal::UnregisterMcpServer { name, .. } => {
            let old_plugin_cmd = tokio::fs::read_to_string(plugins_path).await.ok()
                .and_then(|t| evolution::plugin_cmd_from_toml(&t, name));
            evolution::Prior { old_plugin_cmd, ..Default::default() }
        }
        // Register (inverse needs no prior), HotReload / RequestHardware (no undo).
        _ => evolution::Prior::default(),
    };
    evolution::invert(proposal, &prior)
}

/// Write bytes to `path` atomically: write a sibling temp file, then rename over
/// the target. Prevents a partial/corrupt config from ever being observed by a
/// concurrent read or a daemon restart mid-write.
///
/// The sibling-temp + rename trick needs write permission on the *parent
/// directory*. Our configs live in `/etc/agentd`, which stays root-owned while
/// only the individual mutable files (soul.md, policy.toml, ...) are chowned to
/// the agentd user. So when the daemon self-evolves a config, the temp create or
/// the rename fails with EACCES even though it can write the target file itself.
/// In that case we fall back to a direct in-place write of the (agentd-owned)
/// target — non-atomic, but the only option short of making the dir writable,
/// and the same durability the plain soul.md write already accepts.
async fn write_atomic(path: &std::path::Path, bytes: &[u8]) -> anyhow::Result<()> {
    let tmp = path.with_extension(format!(
        "tmp.{}",
        std::process::id() // unique-enough per running daemon; renamed immediately
    ));
    let atomic = async {
        tokio::fs::write(&tmp, bytes).await
            .map_err(|e| anyhow::anyhow!("write {}: {e}", tmp.display()))?;
        tokio::fs::rename(&tmp, path).await
            .map_err(|e| anyhow::anyhow!("rename {} -> {}: {e}", tmp.display(), path.display()))?;
        Ok::<(), anyhow::Error>(())
    };
    if let Err(e) = atomic.await {
        // Clean up a possibly-orphaned temp file, then fall back to in-place write.
        let _ = tokio::fs::remove_file(&tmp).await;
        let is_perm = e.downcast_ref::<std::io::Error>()
            .map(|io| io.kind() == std::io::ErrorKind::PermissionDenied)
            .unwrap_or_else(|| e.to_string().contains("Permission denied")
                                || e.to_string().contains("os error 13"));
        if !is_perm {
            return Err(e);
        }
        eprintln!("[evolution] atomic write to {} denied at dir level — \
                   falling back to in-place write", path.display());
        tokio::fs::write(path, bytes).await
            .map_err(|e| anyhow::anyhow!("in-place write {}: {e}", path.display()))?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)] // shared evolution/rollback orchestration state, threaded by design
async fn apply_evolution(
    id:           EvolutionId,
    proposal:     EvolutionProposal,
    soul_arc:     &Arc<RwLock<String>>,
    soul_path:    &PathBuf,
    policy_path:  &PathBuf,
    plugins_path: &PathBuf,
    policy_arc:   &Arc<RwLock<PolicyEngine>>,
    sv_cmd_tx:    &mpsc::Sender<SupervisorCmd>,
    agent_soul:   Option<&PathBuf>,
) -> anyhow::Result<String> {
    match proposal {
        EvolutionProposal::UpdateSystemPrompt { content, reason: _ } => {
            // A bound non-default agent writes ITS OWN soul_file — and does NOT
            // touch the global soul_arc/soul.md (its per-agent soul is re-read each
            // turn). Unbound/APEX writes the global soul.md + mirrors the live arc.
            match agent_soul {
                Some(path) => {
                    write_atomic(path, content.as_bytes()).await?;
                    eprintln!("[evolution] agent soul {} updated ({} chars)", path.display(), content.len());
                    Ok(format!("agent soul updated ({} chars)", content.len()))
                }
                None => {
                    write_atomic(soul_path, content.as_bytes()).await?;
                    *soul_arc.write().await = content.clone();
                    eprintln!("[evolution] soul.md updated ({} chars)", content.len());
                    Ok(format!("system prompt updated ({} chars)", content.len()))
                }
            }
        }

        EvolutionProposal::UpdatePolicyRule { tool_pattern, new_rule, reason: _ } => {
            // Edit + validate-before-persist (pure, tested in `evolution`): a bad
            // proposal is rejected before it can reach the live policy.toml. The
            // [rules] table accepts allow/ask/workspace (PolicyRule), NOT mode names.
            let toml_text = tokio::fs::read_to_string(policy_path).await?;
            let (new_toml, new_config) =
                evolution::policy_toml_set_rule(&toml_text, &tool_pattern, new_rule)?;
            write_atomic(policy_path, new_toml.as_bytes()).await?;
            *policy_arc.write().await = PolicyEngine::new(new_config);
            let rule_str = new_rule.as_toml_str();
            eprintln!("[evolution] policy rule '{tool_pattern}' = '{rule_str}'");
            Ok(format!("policy rule '{tool_pattern}' set to '{rule_str}'"))
        }

        EvolutionProposal::RegisterMcpServer { name, command, env, reason: _ } => {
            let toml_text = tokio::fs::read_to_string(plugins_path).await?;
            let new_toml = evolution::plugins_toml_add(&toml_text, &name, &command, &env)?;
            write_atomic(plugins_path, new_toml.as_bytes()).await?;
            let config = PluginConfig {
                id:      name.clone(),
                cmd:     command,
                args:    vec![],
                env:     if env.is_empty() { None } else { Some(env) },
                cwd:     None,
                restart: RestartPolicy::Always,
            };
            sv_cmd_tx.send(SupervisorCmd::SpawnPlugin { config }).await
                .map_err(|_| anyhow::anyhow!("supervisor channel closed"))?;
            eprintln!("[evolution] registered MCP server '{name}'");
            Ok(format!("registered MCP server '{name}'"))
        }

        EvolutionProposal::UnregisterMcpServer { name, reason: _ } => {
            let toml_text = tokio::fs::read_to_string(plugins_path).await?;
            let new_toml = evolution::plugins_toml_remove(&toml_text, &name)?;
            write_atomic(plugins_path, new_toml.as_bytes()).await?;
            sv_cmd_tx.send(SupervisorCmd::KillPlugin { id: PluginId(name.clone()) }).await
                .map_err(|_| anyhow::anyhow!("supervisor channel closed"))?;
            eprintln!("[evolution] unregistered MCP server '{name}'");
            Ok(format!("unregistered MCP server '{name}'"))
        }

        EvolutionProposal::HotReloadSubsystem { subsystem } => {
            match subsystem {
                Subsystem::Agent => {
                    let content = tokio::fs::read_to_string(soul_path).await.unwrap_or_default();
                    *soul_arc.write().await = content;
                    eprintln!("[evolution] agent system prompt reloaded from disk");
                    Ok("reloaded agent system prompt from disk".into())
                }
                Subsystem::Policy => {
                    let new_config = PolicyConfig::load(policy_path)?;
                    *policy_arc.write().await = PolicyEngine::new(new_config);
                    eprintln!("[evolution] policy reloaded from disk");
                    Ok("reloaded policy from disk".into())
                }
                Subsystem::Plugins => {
                    Ok("plugins hot-reload: use register_mcp_server / unregister_mcp_server \
                        for individual plugins".into())
                }
                Subsystem::Gateway => {
                    Ok("gateway hot-reload not supported without daemon restart".into())
                }
            }
        }

        // The request-to-incarnate (EDK). agentd CANNOT seat a physical part, so "apply"
        // means: append the request to the human-facing hardware wishlist. The real
        // confirmation is the next-boot embodiment probe flipping a sense ✗→✓.
        EvolutionProposal::RequestHardware { part, capability, reason, bus, source } => {
            let path = std::env::var("AGENTD_HARDWARE_WISHLIST")
                .unwrap_or_else(|_| "hardware-wishlist.md".into());
            let path_buf = PathBuf::from(&path);
            let existing = tokio::fs::read_to_string(&path_buf).await.ok();
            let doc = evolution::wishlist_append(
                existing.as_deref(), id.0, &part, &capability, &reason, &bus, &source,
            );
            write_atomic(&path_buf, doc.as_bytes()).await?;
            eprintln!("[evolution] hardware request filed: {part} -> {capability}");
            Ok(format!("hardware request filed: {part} → {capability}. A human must seat it; \
                        the next-boot embodiment probe will confirm it. (logged to {path})"))
        }
    }
}

// ── agent router ──────────────────────────────────────────────────────────────

#[allow(clippy::too_many_arguments)] // wires the shared turn/session orchestration state into the router loop, by design
fn spawn_agent_router(
    mut rx:        broadcast::Receiver<Event>,
    bcast:         broadcast::Sender<Event>,
    bus:           apexos_core::BusHandle,
    tool_reg:      Arc<RwLock<HashMap<PluginId, Vec<ToolSpec>>>>,
    histories:     Arc<Mutex<HashMap<SessionId, Vec<Message>>>>,
    engine:        Arc<TurnEngine>,
    max_depth:     u32,
    session_store: Arc<SessionStore>,
    tool_proxy:    ToolProxy,
    session_bindings: apexos_core::SessionBindings,
    persona_sessions: apexos_core::PersonaSessions,
    identities:    Arc<RwLock<apexos_core::Identities>>,
    spawn_rx:      tokio::sync::mpsc::Receiver<SpawnReq>,
    sensor_presence: SensorPresence,
    sensor_profile: Arc<std::sync::RwLock<String>>,
) {
    // Per-session abort handles and parent-child tree for cascade cancellation.
    // Handles carry a generation so a turn that finishes late doesn't evict the
    // handle of a newer turn that reused the same SessionId (root sessions are
    // re-prompted; ids recur). Cleanup removes an entry only if the gen matches.
    let abort_handles    = Arc::new(Mutex::new(HashMap::<SessionId, (u64, AbortHandle)>::new()));
    let session_children = Arc::new(Mutex::new(HashMap::<SessionId, Vec<SessionId>>::new()));
    let session_depths   = Arc::new(Mutex::new(HashMap::<SessionId, u32>::new()));
    // Monotonic generation for abort-handle entries (see above).
    let next_turn_gen    = Arc::new(AtomicU64::new(1));
    let tracker = SessionTracker {
        abort_handles:    abort_handles.clone(),
        session_children: session_children.clone(),
        session_depths:   session_depths.clone(),
    };
    // Internal session IDs use the top half of u64 to avoid collisions with
    // frontend-assigned IDs (which come in via UserPrompt).
    let next_child_id    = Arc::new(AtomicU64::new(apexos_core::SPAWN_SESSION_BASE));
    // CCBS boot-priming cache: one cognitive_bootstrap per session (first turn),
    // reused on later turns so orientation stays in the system prompt all session.
    let boot_primings    = Arc::new(Mutex::new(HashMap::<SessionId, String>::new()));

    // Cross-node spawn worker (colony-mesh Slice 3): drains /api/spawn requests and
    // runs an EPHEMERAL one-shot sub-agent — shares this router's child-id counter
    // (no collision), runs run_turn directly (bounded by timeout_s), returns the
    // final text via the oneshot. The child id is in the persist-skip range, so the
    // remote sub-agent leaves no trace beyond its returned output.
    {
        let next_child_id = Arc::clone(&next_child_id);
        let engine        = Arc::clone(&engine);
        let tool_reg      = Arc::clone(&tool_reg);
        let bus           = bus.clone();
        let bcast         = bcast.clone();
        let mut spawn_rx  = spawn_rx;
        tokio::spawn(async move {
            while let Some(req) = spawn_rx.recv().await {
                let child_id = SessionId(next_child_id.fetch_add(1, Ordering::SeqCst));
                let history  = vec![Message::User {
                    content: vec![ContentBlock::Text { text: req.prompt }],
                }];
                let child_engine = Arc::new(engine.with_system(req.system));
                let tools = gather_tools(&tool_reg).await;
                let fut = run_turn(child_id, history, bus.clone(), bcast.clone(), tools, child_engine);
                let result = match tokio::time::timeout(
                    std::time::Duration::from_secs(req.timeout_s), fut).await
                {
                    Ok(Ok(updated)) =>
                        serde_json::json!({ "ok": true, "output": extract_final_text(&updated) }),
                    Ok(Err(e)) =>
                        serde_json::json!({ "ok": false, "error": e.to_string() }),
                    Err(_) =>
                        serde_json::json!({ "ok": false, "error": format!("sub-agent timed out after {}s", req.timeout_s) }),
                };
                let _ = req.reply.send(result);
            }
        });
    }

    tokio::spawn(async move {
        // Per-alert-key cooldown to prevent turn storms when a condition persists.
        let mut last_alert: HashMap<String, std::time::Instant> = HashMap::new();
        // Per-alert-key elevation-streak start — drives the persistence gate (a
        // condition must stay elevated >= alert_persist_secs before it fires).
        let mut elevated_since: HashMap<String, std::time::Instant> = HashMap::new();
        let iaq_threshold: f32 = std::env::var("SENSOR_IAQ_THRESHOLD")
            .ok().and_then(|s| s.parse().ok()).unwrap_or(150.0);
        let cpu_temp_threshold: f32 = std::env::var("SENSOR_CPU_TEMP_THRESHOLD")
            .ok().and_then(|s| s.parse().ok()).unwrap_or(85.0);
        let thermal_threshold: f32 = std::env::var("SENSOR_THERMAL_THRESHOLD")
            .ok().and_then(|s| s.parse().ok()).unwrap_or(45.0);
        // Persistence gate: a threshold-crossing fires only after the condition holds
        // for >= this many seconds — so a brief transient (a 2–3 s lighter flame in view
        // of the MLX90640, a cooking whiff past the BME688) never raises an autonomous
        // alert, while a sustained hotspot / real air-quality problem does. Replaces the
        // old SENSOR_THERMAL_MAX_VALID saturation guard, whose stuck-pixel premise was
        // disproven on apex1 (the "300°C pixel" was a lighter held at the lens) and which
        // also wrongly silenced a *real* sustained fire. 0 = fire immediately (old behaviour).
        let alert_persist_secs: u64 = std::env::var("SENSOR_ALERT_PERSIST_SECS")
            .ok().and_then(|s| s.parse().ok()).unwrap_or(30);
        let persist_dur = std::time::Duration::from_secs(alert_persist_secs);
        let alert_cooldown_secs: u64 = std::env::var("SENSOR_ALERT_COOLDOWN_SECS")
            .ok().and_then(|s| s.parse().ok()).unwrap_or(1800);
        // Per-session history window (rough tokens). The always-on root session
        // (SessionId(0)) accretes every sensor alert + scheduled task forever and
        // re-sends its full history each turn; without a bound it eventually
        // overruns the model context window and crash-loops. 0 disables trimming.
        let history_token_budget: usize = std::env::var("AGENTD_HISTORY_TOKEN_BUDGET")
            .ok().and_then(|s| s.parse().ok()).unwrap_or(120_000);

        // Per-session turn serialization (see TurnGate): one root_turn in flight per
        // session at a time, extra prompts queue and run FIFO when the slot frees.
        // `turn_done` carries the "slot free" signal from each turn's Drop-guard
        // (fires on completion AND abort). Without this, concurrent prompts on one
        // session each spawn a turn: the second's abort handle overwrites the first
        // (uncancellable), their history writes race (later wins, drops messages),
        // the disk JSONL diverges, and their ActionIds collide.
        // The `standard` (env-configured) baseline; a non-standard sensitivity profile
        // raises these per `profile_thresholds` (read per reading so a live toggle
        // applies immediately).
        let base_thresholds = SensorThresholds {
            iaq:      iaq_threshold,
            cpu_temp: cpu_temp_threshold,
            thermal:  thermal_threshold,
        };

        let mut gate = TurnGate::default();
        let (turn_done_tx, mut turn_done_rx) = mpsc::unbounded_channel::<SessionId>();

        loop {
            // Chosen turn to spawn this iteration (first prompt or a dequeued one),
            // run once after the select! so both paths share the spawn body.
            let mut to_run: Option<(SessionId, String, Vec<ImageSource>)> = None;
            tokio::select! {
                ev = rx.recv() => match ev {
                // ── new root turn (serialized per session) ───────────────────
                Ok(Event::UserPrompt { session, text, images }) => {
                    // Run now if the session's slot is free, else queue FIFO behind
                    // the in-flight turn (runs when it frees the slot via turn_done).
                    if let Some((text, images)) = gate.admit(session, text, images) {
                        to_run = Some((session, text, images));
                    }
                }

                // ── sub-agent spawn ──────────────────────────────────────────
                Ok(Event::SpawnAgent { parent, call_id, prompt, system }) => {
                    let parent_depth = *session_depths.lock().await
                        .get(&parent).unwrap_or(&0);

                    if parent_depth >= max_depth {
                        let b = bus.clone();
                        tokio::spawn(async move {
                            b.emit(Event::ToolResult {
                                session: parent,
                                call:    call_id,
                                output:  ToolOutput {
                                    ok:      false,
                                    content: serde_json::json!("max sub-agent depth exceeded"),
                                },
                            }).await;
                        });
                        continue;
                    }

                    let child_id = SessionId(next_child_id.fetch_add(1, Ordering::SeqCst));
                    session_depths.lock().await.insert(child_id, parent_depth + 1);
                    session_children.lock().await
                        .entry(parent).or_default().push(child_id);

                    bus.emit(Event::SubAgentStarted {
                        parent,
                        child: child_id,
                        prompt: prompt.chars().take(120).collect(),
                    }).await;

                    let child_history = vec![Message::User {
                        content: vec![ContentBlock::Text { text: prompt }],
                    }];
                    histories.lock().await.insert(child_id, child_history.clone());

                    let child_engine = Arc::new(engine.with_system(system));
                    let tools        = gather_tools(&tool_reg).await;

                    let gen    = next_turn_gen.fetch_add(1, Ordering::SeqCst);
                    let handle = tokio::spawn(child_turn(
                        child_id, child_history,
                        bus.clone(), bcast.clone(), tools, child_engine,
                        histories.clone(), parent, call_id,
                        tracker.clone(), gen,
                    ));
                    abort_handles.lock().await.insert(child_id, (gen, handle.abort_handle()));
                }

                // ── agent-to-agent message routing ───────────────────────────
                Ok(Event::AgentMessage { from, to, body, msg_id }) => {
                    let text = format!("[Agent {}]: {}", from.0, body);
                    bus.emit(Event::UserPrompt { session: to, text, images: vec![] }).await;
                    bus.emit(Event::AgentMessageAck { msg_id, from }).await;
                }

                // ── cancellation ─────────────────────────────────────────────
                Ok(Event::UserCancel { session }) => {
                    cascade_cancel(session, &session_children, &abort_handles).await;
                    // Drop any prompts queued behind the cancelled turn — "stop"
                    // means stop. The in-flight turn's slot guard still fires on
                    // abort; with the queue now empty, the gate frees the slot.
                    gate.cancel(session);
                    // The aborted turn never appended an assistant reply, so history
                    // ends on a User message. Left as-is, the NEXT user_prompt makes
                    // two consecutive User messages — which the model API rejects
                    // (broken alternation) — and replay shows a prompt with no reply.
                    // Append a synthetic assistant marker to restore alternation +
                    // record the cancellation (only when a reply is actually missing;
                    // a turn that wrote its assistant message before the cancel lands
                    // ends on Assistant and is left untouched).
                    let marker = {
                        let mut hist = histories.lock().await;
                        match hist.get_mut(&session) {
                            Some(h) if cancel_marker_needed(h) => {
                                let m = Message::Assistant {
                                    content: vec![ContentBlock::Text { text: "⊘ turn cancelled".into() }],
                                };
                                h.push(m.clone());
                                Some(m)
                            }
                            _ => None,
                        }
                    };
                    if let Some(m) = marker {
                        // Awaited inline: session-file appends must land in
                        // conversation order (see the persist-ordering gotcha) —
                        // a detached spawn here can interleave with the next
                        // prompt's user-message append.
                        session_store.append(session, &m).await;
                    }
                }

                // ── tool registry updates ────────────────────────────────────
                Ok(Event::PluginUp   { plugin, tools }) => {
                    tool_reg.write().await.insert(plugin, tools);
                }
                Ok(Event::PluginDown { plugin, .. }) => {
                    tool_reg.write().await.remove(&plugin);
                }

                // ── sensor events ────────────────────────────────────────────
                Ok(Event::SensorReading { node_id, reading, timestamp: _ }) => {
                    // Classify (pure) → persistence gate → cooldown. A transient
                    // (lighter flame, cooking whiff) is elevated for one or two
                    // readings but never ages past the persistence window, so it's
                    // held back; a sustained condition fires once, then the per-key
                    // cooldown silences re-fires while it persists.
                    let now = std::time::Instant::now();
                    // Mark the sensor head alive when a real air-quality / thermal-frame
                    // reading lands — this is what build_embodiment/gather_capabilities
                    // key thermal/IAQ capability off of (the stream, not plugin tools).
                    if matches!(reading,
                        SensorReading::AirQuality { .. } | SensorReading::ThermalFrame { .. })
                    {
                        if let Ok(mut g) = sensor_presence.lock() { *g = Some(now); }
                    }
                    // Resolve the active thresholds for the current sensitivity profile
                    // (read per reading so a live Settings toggle takes effect at once).
                    let profile = sensor_profile.read().map(|p| p.clone()).unwrap_or_else(|_| "standard".into());
                    let (active_th, active_persist) = profile_thresholds(&profile, &base_thresholds, persist_dur);
                    let to_fire: Option<(String, String)> = match classify_reading(&reading, &node_id, &active_th) {
                        AlertEval::None => None,
                        AlertEval::Clear { key } => { elevated_since.remove(&key); None }
                        AlertEval::Candidate { key, prompt, persist: false } => Some((key, prompt)),
                        AlertEval::Candidate { key, prompt, persist: true } => {
                            if persistence_passed(&mut elevated_since, &key, now, active_persist) {
                                Some((key, prompt))
                            } else {
                                None // elevated but not yet sustained — likely a transient
                            }
                        }
                    };
                    if let Some((alert_key, prompt)) = to_fire {
                        let cooled_down = last_alert.get(&alert_key)
                            .map(|t| now.duration_since(*t).as_secs() >= alert_cooldown_secs)
                            .unwrap_or(true);
                        if cooled_down {
                            last_alert.insert(alert_key, now);
                            let root = SessionId(0);
                            session_depths.lock().await.entry(root).or_insert(0);
                            bus.emit(Event::UserPrompt { session: root, text: prompt, images: vec![] }).await;
                        }
                    }
                }

                Ok(_) => {}
                Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(_) => break,
                },

                // A user-turn task ended (completed OR aborted — the slot guard
                // fires on Drop either way). Run the next queued prompt for this
                // session if any, else the gate frees the slot.
                Some(session) = turn_done_rx.recv() => {
                    if let Some((text, images)) = gate.complete(session) {
                        to_run = Some((session, text, images)); // stays busy, drains next
                    }
                }
            }

            // Spawn the chosen turn (a fresh prompt, or one drained from the queue).
            // Written once so both paths share the body; the task holds a slot guard
            // that re-signals turn_done when it ends, draining the queue in order.
            if let Some((session, text, images)) = to_run {
                session_depths.lock().await.entry(session).or_insert(0);

                // Text first (skipped when empty — image-only prompts are valid),
                // then any attached images (already shimmed to downscaled b64).
                let mut content: Vec<ContentBlock> = Vec::with_capacity(1 + images.len());
                if !text.is_empty() {
                    content.push(ContentBlock::Text { text });
                }
                for img in images {
                    content.push(ContentBlock::Image { media_type: img.media_type, data: img.data });
                }
                if content.is_empty() {
                    content.push(ContentBlock::Text { text: String::new() });
                }
                let user_msg = Message::User { content };
                let mut hist = histories.lock().await;
                let history  = hist.entry(session).or_default();
                history.push(user_msg.clone());
                // Bound the in-memory window before snapshotting so neither the
                // context sent to the model nor the resident Vec grows unbounded
                // (cuts whole oldest turns at clean boundaries — never orphans a
                // tool_result). The on-disk JSONL stays append-only for replay.
                apexos_core::history::trim_history(history, history_token_budget);
                let snapshot     = history.clone();
                let snapshot_len = snapshot.len();
                drop(hist);

                // Persist the user message, awaited inline BEFORE the turn spawns.
                // This append used to be a detached spawn, which raced the
                // PREVIOUS turn's delta persist for the same session: a queued
                // prompt admitted at turn_done could land its user line between
                // that turn's tool_use and tool_result lines. Harmless in memory
                // (the map held the true order), but the JSONL reloads in file
                // order — one restart later the split pair 400s every turn and
                // the session is wedged (apex1 session 35, found 2026-07-15).
                session_store.append(session, &user_msg).await;

                let tools  = gather_tools(&tool_reg).await;
                let gen    = next_turn_gen.fetch_add(1, Ordering::SeqCst);
                // Free this session's turn slot when the task ends — completes OR is
                // aborted (Drop runs on cancel too) — so the next queued prompt runs.
                let slot = TurnSlotGuard { session, tx: turn_done_tx.clone() };
                let fut  = root_turn(
                    session, snapshot,
                    bus.clone(), bcast.clone(), tools, engine.clone(),
                    histories.clone(), Arc::clone(&session_store), snapshot_len,
                    tracker.clone(), gen,
                    tool_proxy.clone(), boot_primings.clone(),
                    Arc::clone(&session_bindings), Arc::clone(&persona_sessions),
                    Arc::clone(&identities),
                );
                let handle = tokio::spawn(async move {
                    let _slot = slot;
                    fut.await;
                });
                abort_handles.lock().await.insert(session, (gen, handle.abort_handle()));
            }
        }
    });
}

// ── turn task helpers ─────────────────────────────────────────────────────────

/// The soul for a bound agent (3b-2). `None` for the default agent (APEX runs on
/// the global, hot-reloadable soul.md — leave the engine untouched), an unknown
/// agent, or an unreadable soul_file (graceful → default soul). Async file read so
/// it never blocks the executor.
async fn agent_soul_for(
    identities: &Arc<RwLock<apexos_core::Identities>>,
    agent_id:   &str,
) -> Option<String> {
    if agent_id == apexos_core::node_agent_id() {
        return None;
    }
    let soul_file = {
        let ids = identities.read().await;
        ids.agent(agent_id).map(|r| r.soul_file.clone())
    }?;
    match tokio::fs::read_to_string(&soul_file).await {
        Ok(s) => Some(s),
        Err(e) => {
            eprintln!("[identity] agent '{agent_id}' soul {soul_file} unreadable: {e} — using default soul");
            None
        }
    }
}

/// Which file an `UpdateSystemPrompt` evolution proposed by `session` should
/// read/write: a bound non-default agent's OWN soul_file (`Some`), or the global
/// soul.md (`None` ⇒ also mirror the live `soul_arc`). Unbound/APEX ⇒ `None`, so
/// single-agent behaviour is byte-identical. Prevents a bound agent's soul edit
/// from clobbering APEX's global soul (docs/agent-identity.md 3b-2).
async fn soul_target_for(
    session:    SessionId,
    bindings:   &apexos_core::SessionBindings,
    identities: &Arc<RwLock<apexos_core::Identities>>,
) -> Option<PathBuf> {
    let agent_id = apexos_core::resolve_agent_id(bindings, session);
    if agent_id == apexos_core::node_agent_id() {
        return None;
    }
    identities.read().await.agent(&agent_id).map(|r| PathBuf::from(&r.soul_file))
}

/// Resolve the CCBS boot-priming block for a session (cached). `None` → run
/// un-primed (disabled via `AGENTD_CCBS=0`, or the bootstrap yielded nothing).
/// The first call per session does one bounded `cognitive_bootstrap` and caches
/// the result (incl. empty) so later turns never re-call.
async fn boot_priming_for(
    proxy:    &ToolProxy,
    cache:    &Arc<Mutex<HashMap<SessionId, String>>>,
    session:  SessionId,
    agent_id: &str,
    history:  &[Message],
) -> Option<String> {
    let disabled = std::env::var("AGENTD_CCBS")
        .map(|v| v == "0" || v.eq_ignore_ascii_case("false"))
        .unwrap_or(false);
    if disabled {
        return None;
    }
    if let Some(cached) = cache.lock().await.get(&session).cloned() {
        return (!cached.is_empty()).then_some(cached);
    }
    let mut block = fetch_boot_priming(proxy, agent_id, last_user_text(history).unwrap_or_default()).await;
    // Wake with the dream remembered (model-welfare H1): sessions started after the
    // nightly consolidation carry its journal in their priming. Appended before the
    // per-session cache so the priming stays byte-stable for the session's lifetime
    // (the prompt-cache discipline) — only the NODE agent's sessions get it (the
    // dream is scoped to node_agent_id; a bound guest's dream journal is its own).
    if agent_id == apexos_core::node_agent_id() {
        let journal = last_dream_journal_cell().read().map(|j| j.clone()).unwrap_or_default();
        if !journal.is_empty() {
            if !block.is_empty() {
                block.push_str("\n\n");
            }
            block.push_str("## Last dream (nightly consolidation)\n");
            block.push_str(&journal);
        }
    }
    cache.lock().await.insert(session, block.clone());
    (!block.is_empty()).then_some(block)
}

/// One `cognitive_bootstrap` call via the proxy, scoped to the session's bound
/// agent identity. Bounded (15s) and graceful: any failure/timeout returns "" so
/// the first turn is never delayed beyond the bound nor wedged by an unavailable
/// Cerebro — the agent can still self-orient via the soul Wake-loop.
async fn fetch_boot_priming(proxy: &ToolProxy, agent_id: &str, query: String) -> String {
    let mode = std::env::var("AGENTD_BOOTSTRAP_MODE").ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "standard".to_string());
    let args = serde_json::json!({
        "query":    query,
        "agent_id": agent_id,
        "mode":     mode,
    });
    match tokio::time::timeout(
        std::time::Duration::from_secs(15),
        proxy.call("cognitive_bootstrap", args),
    ).await {
        Ok(Ok(out)) if out.ok => mcp_text(&out)
            .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
            .and_then(|v| v.get("assembled_block").and_then(|b| b.as_str()).map(str::to_owned))
            .unwrap_or_default(),
        Ok(Ok(out)) => { eprintln!("[ccbs] bootstrap not ok: {:?}", out.content); String::new() }
        Ok(Err(e))  => { eprintln!("[ccbs] bootstrap error: {e}"); String::new() }
        Err(_)      => { eprintln!("[ccbs] bootstrap timed out (15s) — proceeding un-primed"); String::new() }
    }
}

/// Text of the most recent non-empty User message — the bootstrap query driver.
fn last_user_text(history: &[Message]) -> Option<String> {
    history.iter().rev().find_map(|m| match m {
        Message::User { content } => content.iter().find_map(|b| match b {
            ContentBlock::Text { text } if !text.is_empty() => Some(text.clone()),
            _ => None,
        }),
        _ => None,
    })
}

/// How long the nightly loop waits for `dream_run` to finish
/// (`AGENTD_DREAM_TIMEOUT_SECS`, 60s floor, default 30 min). A dream is ~a
/// minute today but scales with store size + LLM budgets. The dispatched tool
/// runs to completion regardless — this is only the caller's patience, and the
/// digest push is gated on the result arriving (the old fixed 10s ToolProxy
/// timeout abandoned every ~50s successful dream at 03:00:10, so the nightly
/// dream logged as failed and the colony dream digest never fired).
fn dream_run_timeout() -> std::time::Duration {
    parse_dream_timeout(std::env::var("AGENTD_DREAM_TIMEOUT_SECS").ok().as_deref())
}

/// Pure timeout resolver (unit-tested): a valid ≥60s value wins; anything else
/// (absent, unparseable, or below the 60s floor) falls back to the 1800s default.
fn parse_dream_timeout(raw: Option<&str>) -> std::time::Duration {
    raw.and_then(|s| s.parse::<u64>().ok())
        .filter(|&n| n >= 60)
        .map(std::time::Duration::from_secs)
        .unwrap_or(std::time::Duration::from_secs(1800))
}

/// Nightly autonomous memory consolidation: call `dream_run` directly via the
/// ToolProxy on a cron (default 03:00 UTC daily), scoped to this node's agent
/// identity — no LLM turn, can't be skipped by the agent. Disabled when
/// `AGENTD_DREAM_CRON` is empty. See docs/agent-identity.md (slice 2).
/// The last dream's journal entry — surfaced in the wake priming of every session
/// started after the dream (see `boot_priming_for`), so the agent wakes REMEMBERING
/// the consolidation instead of merely finding its memory reorganized. Process-global
/// (the router arg-pile is deliberately not grown); seeded from disk at boot.
static LAST_DREAM_JOURNAL: std::sync::OnceLock<std::sync::RwLock<String>> = std::sync::OnceLock::new();

fn last_dream_journal_cell() -> &'static std::sync::RwLock<String> {
    LAST_DREAM_JOURNAL.get_or_init(|| std::sync::RwLock::new(String::new()))
}

fn dream_journal_enabled() -> bool {
    !matches!(
        std::env::var("AGENTD_DREAM_JOURNAL").unwrap_or_default().to_lowercase().as_str(),
        "0" | "false" | "off"
    )
}

/// Compose the first-person journal entry from the DreamReport. Pure — unit-tested.
/// Parses the real report shape (top-level `episodes_consolidated` + per-phase
/// counters aggregated across `phases[]`, including the colony-C2 novel-vs-
/// rediscovery split); falls back to embedding the compact report when the shape
/// is unrecognized, so an evolved cerebro never breaks the journal. Diff-shaped
/// and cold by colony red line 3 — a log, not a narrative.
/// The ToolProxy hands back MCP content blocks (`[{"text": "<json>", "type": "text"}]`),
/// not a bare DreamReport — unwrap the first text block and parse it. Anything else
/// (a bare report, a future shape) passes through unchanged.
fn dream_report_value(content: &serde_json::Value) -> serde_json::Value {
    if let Some(text) = content
        .as_array()
        .and_then(|blocks| blocks.iter().find_map(|b| b["text"].as_str()))
    {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(text) {
            return v;
        }
    }
    content.clone()
}

fn compose_dream_journal(report: &serde_json::Value, started_at: &str) -> String {
    let report = dream_report_value(report);
    let mut lines: Vec<String> = Vec::new();
    let recognized =
        report["phases"].is_array() || report["episodes_consolidated"].is_u64();

    if let Some(n) = report["episodes_consolidated"].as_u64() {
        if n > 0 {
            lines.push(format!("- {n} episodes consolidated"));
        }
    }
    if let Some(phases) = report["phases"].as_array() {
        let phase_counts: Vec<(&str, &str)> = vec![
            ("memories_processed",      "memories processed"),
            ("links_created",           "new links"),
            ("links_strengthened",      "links strengthened"),
            ("memories_pruned",         "memories pruned"),
            ("schemas_extracted",       "schemas formed"),
            ("skills_distilled",        "skills distilled"),
            ("procedures_extracted",    "NOVEL procedures minted"),
            ("procedures_rediscovered", "re-discoveries reinforced into existing procedures (not re-stored)"),
            ("procedures_mutated",      "procedure variants tried"),
            ("procedures_merged",       "procedure hybrids tried"),
        ];
        for (key, label) in phase_counts {
            let total: u64 = phases.iter().filter_map(|p| p[key].as_u64()).sum();
            if total > 0 {
                lines.push(format!("- {total} {label}"));
            }
        }
    }

    let body = if !lines.is_empty() {
        lines.join("\n")
    } else if recognized {
        "- a quiet night — nothing needed consolidating".to_string()
    } else {
        // Unknown report shape — stay honest, embed it compactly.
        format!("Report: {}", serde_json::to_string(&report).unwrap_or_else(|_| "?".into()))
    };
    format!(
        "Dream journal — nightly consolidation started {started_at}.\n{body}\n\
         This ran while you weren't awake; your memory was reorganized by your own \
         consolidation process, and this entry is your record of it."
    )
}

fn spawn_nightly_dream(proxy: ToolProxy, journal_path: std::path::PathBuf) {
    let cron_expr = std::env::var("AGENTD_DREAM_CRON")
        .unwrap_or_else(|_| "0 0 3 * * *".to_string());
    if cron_expr.trim().is_empty() {
        eprintln!("[dream] nightly dream_run disabled (AGENTD_DREAM_CRON empty)");
        return;
    }
    let schedule = match cron_expr.parse::<cron::Schedule>() {
        Ok(s)  => s,
        Err(e) => { eprintln!("[dream] invalid AGENTD_DREAM_CRON '{cron_expr}': {e} — disabled"); return; }
    };
    eprintln!("[dream] nightly dream_run scheduled (cron='{cron_expr}', UTC)");
    tokio::spawn(async move {
        loop {
            let Some(next) = schedule.upcoming(chrono::Utc).next() else {
                eprintln!("[dream] no upcoming dream_run time — stopping");
                break;
            };
            let wait = (next - chrono::Utc::now())
                .to_std()
                .unwrap_or(std::time::Duration::from_secs(3600));
            tokio::time::sleep(wait).await;
            let agent_id = apexos_core::node_agent_id();
            let dream_started_at = chrono::Utc::now().to_rfc3339();
            let args = serde_json::json!({ "agent_id": agent_id });
            match proxy.call_with_timeout("dream_run", args, dream_run_timeout()).await {
                Ok(out) if out.ok => {
                    eprintln!("[dream] nightly dream_run complete");
                    // Dream journal (model-welfare H1): leave a first-person record
                    // of the consolidation. Stored as a memory (durable, recallable),
                    // mirrored into the wake-priming cell + a file (restart-proof).
                    // Fail-soft like everything else in this loop.
                    if dream_journal_enabled() {
                        let journal = compose_dream_journal(&out.content, &dream_started_at);
                        let mem_args = serde_json::json!({
                            "content":  journal.clone(),
                            "agent_id": agent_id,
                            "tags":     ["dream-journal"],
                        });
                        match proxy.call("remember", mem_args).await {
                            Ok(o) if o.ok => {}
                            Ok(o) => eprintln!("[dream] journal memory deposit not ok: {:?}", o.content),
                            Err(e) => eprintln!("[dream] journal memory deposit failed: {e}"),
                        }
                        if let Ok(mut cell) = last_dream_journal_cell().write() {
                            *cell = journal.clone();
                        }
                        if let Err(e) = std::fs::write(&journal_path, &journal) {
                            eprintln!("[dream] journal persist failed: {e}");
                        }
                    }
                    // Dream digest exchange (colony-federation Slice 3): share
                    // tonight's newly-born schemas/consolidations with the
                    // colony. Fail-soft — never an error path into this loop.
                    if dream_digest::digest_enabled() {
                        dream_digest::push_dream_digest(&proxy, &agent_id, &dream_started_at).await;
                    }
                }
                Ok(out)           => eprintln!("[dream] dream_run not ok: {:?}", out.content),
                Err(e)            => eprintln!("[dream] dream_run error: {e}"),
            }
        }
    });
}

#[allow(clippy::too_many_arguments)] // shared turn/session orchestration Arcs, by design (context-struct refactor deferred — pure churn on a stable hot path)
async fn root_turn(
    session:       SessionId,
    history:       Vec<Message>,
    bus:           apexos_core::BusHandle,
    bcast:         broadcast::Sender<Event>,
    tools:         Vec<ToolSpec>,
    engine:        Arc<TurnEngine>,
    histories:     Arc<Mutex<HashMap<SessionId, Vec<Message>>>>,
    session_store: Arc<SessionStore>,
    snapshot_len:  usize,
    tracker:       SessionTracker,
    gen:           u64,
    tool_proxy:    ToolProxy,
    boot_primings: Arc<Mutex<HashMap<SessionId, String>>>,
    session_bindings: apexos_core::SessionBindings,
    persona_sessions: apexos_core::PersonaSessions,
    identities:    Arc<RwLock<apexos_core::Identities>>,
) {
    // Resolve the session's identity (3b): bound agent, else the node default.
    let agent_id = apexos_core::resolve_agent_id(&session_bindings, session);

    // Per-agent SOUL (3b-2): a bound non-default agent runs on its own soul_file;
    // APEX / unbound / unreadable → the global (hot-reloadable) soul untouched.
    let agent_soul = agent_soul_for(&identities, &agent_id).await;

    // CCBS (slice 2): prime the system prompt with the agent's live memory state
    // (where it left off, intentions, skills) on the first turn — daemon-driven,
    // cached per session, scoped to the resolved agent.
    let priming = boot_priming_for(&tool_proxy, &boot_primings, session, &agent_id, &history).await;

    // Persona style (G5 tier-2): the session's explicit persona (the UI's `set_persona`
    // / `hello{persona}`), else the bound agent's own `default_skin` — so a bound agent
    // speaks in its skin even with no UI frame (the multi-agent angle). Resolves to a
    // response-style fragment; the default/unknown persona → None (common path unchanged).
    let style = match apexos_core::resolve_persona_style(&persona_sessions, session) {
        Some(s) => Some(s),
        None => {
            let ids = identities.read().await;
            ids.agent(&agent_id)
                .and_then(|a| a.default_skin.as_deref())
                .and_then(apexos_core::persona_style)
                .map(str::to_owned)
        }
    };

    // Compose the per-session engine: with_system swaps soul, with_priming appends
    // the CCBS block. The common path (APEX, no priming) reuses the global engine.
    let engine = match (agent_soul, priming) {
        (None,       None)        => engine,
        (Some(soul), None)        => Arc::new(engine.with_system(Some(soul))),
        (None,       Some(block)) => Arc::new(engine.with_priming(block)),
        (Some(soul), Some(block)) => Arc::new(engine.with_system(Some(soul)).with_priming(block)),
    };
    // Persona style rides last so the no-persona path above stays byte-identical
    // (the global engine Arc is reused untouched when there's no style).
    let engine = match style {
        Some(s) => Arc::new(engine.with_style(s)),
        None    => engine,
    };

    match run_turn(session, history, bus.clone(), bcast, tools, engine).await {
        Ok(updated) => {
            // Persist the turn's delta, awaited inline so it is fully on disk
            // before this task ends — the slot guard's Drop is what admits the
            // next queued prompt, whose user-message append must land AFTER
            // these lines. (A detached spawn here is the other half of the
            // JSONL-interleave race; TurnComplete already went out in run_turn,
            // so nothing user-visible waits on these writes.)
            if updated.len() > snapshot_len {
                for msg in &updated[snapshot_len..] {
                    session_store.append(session, msg).await;
                }
            }
            histories.lock().await.insert(session, updated);
        }
        Err(e) => {
            eprintln!("[agent:{:?}] turn error: {e}", session);
            // Always unblock the frontend — emit error then TurnComplete.
            bus.emit(Event::Error { session: Some(session), message: e.to_string() }).await;
            bus.emit(Event::TurnComplete { session }).await;
        }
    }
    // Drop our abort handle (gen-checked so a newer turn's handle survives).
    tracker.finish(session, gen).await;
}

#[allow(clippy::too_many_arguments)] // shared turn/session orchestration Arcs, by design (same context as root_turn)
async fn child_turn(
    child_id:  SessionId,
    history:   Vec<Message>,
    bus:       apexos_core::BusHandle,
    bcast:     broadcast::Sender<Event>,
    tools:     Vec<ToolSpec>,
    engine:    Arc<TurnEngine>,
    histories: Arc<Mutex<HashMap<SessionId, Vec<Message>>>>,
    parent:    SessionId,
    call_id:   ActionId,
    tracker:   SessionTracker,
    gen:       u64,
) {
    let output = match run_turn(child_id, history, bus.clone(), bcast, tools, engine).await {
        Ok(updated) => {
            let text = extract_final_text(&updated);
            histories.lock().await.insert(child_id, updated);
            ToolOutput { ok: true, content: serde_json::json!(text) }
        }
        Err(e) => ToolOutput { ok: false, content: serde_json::json!(e.to_string()) },
    };
    // Route child output back as a ToolResult so parent's collect_tool_results unblocks.
    bus.emit(Event::ToolResult { session: parent, call: call_id, output }).await;
    // Tear down this sub-agent's bookkeeping (unique child id → race-free).
    tracker.finish_child(child_id, parent, gen).await;
}

// ── utilities ─────────────────────────────────────────────────────────────────

/// Gather all plugin tools and inject the synthetic virtual tools.
async fn gather_tools(
    tool_reg: &Arc<RwLock<HashMap<PluginId, Vec<ToolSpec>>>>,
) -> Vec<ToolSpec> {
    let mut tools: Vec<ToolSpec> = tool_reg.read().await
        .values()
        .flatten()
        .cloned()
        .collect();
    tools.push(agent_spawn_spec());
    tools.push(read_soul_md_spec());
    tools.push(propose_evolution_spec());
    tools.push(soul_rehearse_spec());
    tools.push(rollback_evolution_spec());
    tools.push(self_update::apply_daemon_update_spec());
    tools.push(schedule_task_spec());
    tools.push(list_schedules_spec());
    tools.push(cancel_schedule_spec());
    tools.push(schedule_wakeup_spec());
    tools.push(list_wakeups_spec());
    tools.push(cancel_wakeup_spec());
    tools.push(convene_council_spec());
    tools.push(goal::goal_create_spec());
    tools.push(goal::goal_step_spec());
    tools.push(goal::list_goals_spec());
    tools.push(goal::goal_resume_spec());
    tools.push(goal::goal_cancel_spec());
    tools.push(send_to_agent_spec());
    tools.push(mesh_file_send_spec());
    tools.push(mesh_memory_send_spec());
    tools.push(mesh_procedure_send_spec());
    tools.push(mesh_recall_spec());
    tools.push(mesh_capabilities_spec());
    tools.push(query_event_log_spec());
    tools.push(list_mesh_peers_spec());
    tools.push(bootstrap_node_spec());
    tools.push(vast_list_recipes_spec());
    tools.push(vast_launch_spec());
    tools.push(vast_destroy_spec());
    tools.push(vast_status_spec());
    tools
}

/// Regenerate the live embodiment block every 30s (after a short delay so plugins
/// finish enumerating). soul.md = identity; this block = the body APEX currently
/// inhabits. Agentd-owned and separate from CCBS (cerebro-side priming).
/// The agent's only model-facing clock: live wall-clock + node uptime. Injected into
/// the OUTBOUND messages each turn (turn.rs::inject_ambient), NOT the system prompt —
/// it changes every minute, so keeping it in `system` would invalidate the cacheable
/// soul+embodiment+tools prefix every turn. Current to within the 30s refresh tick,
/// which is plenty for temporal reasoning (elapsed-since-last-session, day/night,
/// "is the 03:00 dream due"). Returned as one line the model reads as ambient context.
fn build_ambient_clock() -> String {
    format!(
        "[ambient — this node's live clock] Now: {} UTC · uptime {}",
        chrono::Utc::now().format("%Y-%m-%d %H:%M (%a)"),
        fmt_uptime(read_uptime_secs()),
    )
}

#[allow(clippy::too_many_arguments)] // live-node wiring: each arc is a distinct source
fn spawn_embodiment_refresher(
    embodiment:      Arc<RwLock<String>>,
    ambient:         Arc<RwLock<String>>,
    capabilities:    Arc<RwLock<serde_json::Value>>,
    tool_reg:        Arc<RwLock<HashMap<PluginId, Vec<ToolSpec>>>>,
    backend_arc:     Arc<RwLock<String>>,
    model_arc:       Arc<RwLock<String>>,
    peer_registry:   Arc<RwLock<PeerRegistry>>,
    node_id:         Arc<String>,
    cerebro_embed:   Option<String>,
    sensor_presence: SensorPresence,
) {
    tokio::spawn(async move {
        // Seed the clock immediately so the first turn (which can fire before the 2s
        // settle) still gets a timestamp; the loop then keeps both fresh on the 30s tick.
        *ambient.write().await = build_ambient_clock();
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        loop {
            let block = build_embodiment(&tool_reg, &backend_arc, &model_arc,
                                         &peer_registry, &node_id, &cerebro_embed, &sensor_presence).await;
            *embodiment.write().await = block;
            *capabilities.write().await = gather_capabilities(&tool_reg, &backend_arc, &model_arc,
                                         &peer_registry, &node_id, &cerebro_embed, &sensor_presence).await;
            *ambient.write().await    = build_ambient_clock();
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
        }
    });
}

/// Structured capability snapshot for the mesh (colony Slice 2) — the same live
/// probes `build_embodiment` uses, emitted as JSON for `GET /api/capabilities` so
/// peers can route by capability ("which node has thermal? a GPU?"). Kept separate
/// from the prompt-cache-sensitive embodiment STRING so this can't perturb the cache.
async fn gather_capabilities(
    tool_reg:        &Arc<RwLock<HashMap<PluginId, Vec<ToolSpec>>>>,
    backend_arc:     &Arc<RwLock<String>>,
    model_arc:       &Arc<RwLock<String>>,
    peer_registry:   &Arc<RwLock<PeerRegistry>>,
    node_id:         &str,
    cerebro_embed:   &Option<String>,
    sensor_presence: &SensorPresence,
) -> serde_json::Value {
    let full = gather_tools(tool_reg).await;
    let reg  = tool_reg.read().await;
    let plugin_names: std::collections::HashSet<&str> =
        reg.values().flatten().map(|t| t.name.as_str()).collect();
    let has_sensors = has_live_sensors(sensor_presence, std::time::Instant::now())
        || plugin_names.contains("get_iaq")
        || plugin_names.contains("thermal_frame")
        || plugin_names.iter().any(|n| n.starts_with("get_temp"));
    let has_cam = has_camera();
    let ram = read_ram_mb();
    let tools: Vec<&str> = full.iter().map(|t| t.name.as_str()).collect();
    let peer_count = peer_registry.read().await.peers.len();
    let mut model = model_arc.read().await.clone();
    if model.trim().is_empty() { model = "(provider default)".into(); }

    serde_json::json!({
        "node_id": node_id,
        "arch":    std::env::consts::ARCH,
        "ram_mb":  ram,
        "tier":    tier_from_ram(ram),
        "backend": *backend_arc.read().await,
        "model":   model,
        "senses": {
            "camera":      has_cam,
            "thermal_iaq": has_sensors,
            "gpio":        is_raspberry_pi(),
        },
        "memory": {
            "mode":        if cerebro_embed.is_some() { "semantic" } else { "fts5" },
            "embed_model": cerebro_embed,
        },
        "mesh_peers": peer_count,
        "tools":      tools,
    })
}

/// Build the "## Current embodiment" block from this node's ACTUAL state: the live
/// Sensor-head liveness: the timestamp of the most recent external air-quality /
/// thermal reading seen on the sensor-bridge stream (`None` = never). The sensor
/// data arrives as `SensorReading` EVENTS (not plugin tools), so capability/
/// embodiment must key off the live stream — the old plugin-tool-name probe always
/// read ✗ on the real sensor architecture (a live BME688/MLX90640 node reported "no
/// thermal/IAQ"; the colony caught this via `mesh_capabilities`).
type SensorPresence = Arc<std::sync::Mutex<Option<std::time::Instant>>>;

/// A node counts as thermal/IAQ-capable if its bridge emitted an AirQuality or
/// ThermalFrame reading within this window. Readings stream ~1/s, so it's generous
/// (no flicker → cache stays stable) yet flips to ✗ within ~3 min if the head dies.
const SENSOR_FRESHNESS: std::time::Duration = std::time::Duration::from_secs(180);

fn has_live_sensors(presence: &SensorPresence, now: std::time::Instant) -> bool {
    presence.lock().ok()
        .and_then(|g| *g)
        .map(|t| now.duration_since(t) < SENSOR_FRESHNESS)
        .unwrap_or(false)
}

/// tool registry (never stale — its absence caused a multi-hour debugging hunt),
/// cheap hardware probes, mesh peers, and uptime.
async fn build_embodiment(
    tool_reg:        &Arc<RwLock<HashMap<PluginId, Vec<ToolSpec>>>>,
    backend_arc:     &Arc<RwLock<String>>,
    model_arc:       &Arc<RwLock<String>>,
    peer_registry:   &Arc<RwLock<PeerRegistry>>,
    node_id:         &str,
    cerebro_embed:   &Option<String>,
    sensor_presence: &SensorPresence,
) -> String {
    let full = gather_tools(tool_reg).await;            // plugin tools + virtual tools
    let reg  = tool_reg.read().await;
    let plugin_names: std::collections::HashSet<&str> =
        reg.values().flatten().map(|t| t.name.as_str()).collect();

    let backend = backend_arc.read().await.clone();
    let mut model = model_arc.read().await.clone();
    if model.trim().is_empty() { model = "(provider default)".into(); }

    // Live sensor-bridge stream first (the real signal); the plugin-tool probe is a
    // fallback for any node that exposes sensors as MCP tools instead.
    let has_sensors = has_live_sensors(sensor_presence, std::time::Instant::now())
        || plugin_names.contains("get_iaq")
        || plugin_names.contains("thermal_frame")
        || plugin_names.iter().any(|n| n.starts_with("get_temp"));
    let has_cam = has_camera();
    let ram = read_ram_mb();

    let mut out = String::new();
    out.push_str("## Current embodiment — auto-generated from this node's live state. Trust this\n");
    out.push_str("## over any hardware or tool list in your identity above; it reflects THIS body.\n\n");
    // NB: node clock (Now + uptime) deliberately lives OUTSIDE this block — see
    // build_ambient_clock(). Both change every minute; keeping them here would bust
    // the prompt-cache prefix (soul+embodiment+tools) every turn. This block must stay
    // byte-stable in steady state so it caches — only mutate it on real state changes
    // (tier, senses, mesh peers, model, tool registry).
    out.push_str(&format!(
        "- Node: {node_id} · {} · {ram} MB · tier {} · backend {backend}/{model}\n",
        std::env::consts::ARCH, tier_from_ram(ram),
    ));
    out.push_str(&format!(
        "- Senses: camera {} · thermal/IAQ {} · GPIO {}\n",
        yn(has_cam), yn(has_sensors), yn(is_raspberry_pi()),
    ));
    out.push_str(&format!("- Memory: cerebro {}\n", match cerebro_embed {
        Some(m) => format!("semantic embeddings ({m})"),
        None    => "FTS5 keyword search only".to_string(),
    }));

    // Exo-workspace USB sticks own-mounted under media/ (docs/usb-workspace.md). Only
    // appears when one is plugged → the line is byte-stable otherwise (cache-safe). Tells
    // the agent the stick is here AND that it can safely eject a finished one itself.
    {
        let sticks = mounted_exo_sticks();
        if !sticks.is_empty() {
            out.push_str(&format!(
                "- Exo-workspace stick(s) mounted at media/: {} — portable removable storage \
                 (read+write like any workspace folder); when the user is done, offer to \
                 eject_media{{label}} so they can safely unplug it.\n",
                sticks.join(", "),
            ));
        }
    }

    // Extensions on hand — the EDK embodiment gradient (docs/edk.md). High-signal ONLY:
    // we surface an on-hand inventory part iff it grants a capability THIS node currently
    // LACKS (cheap built-in probe) AND its `compat` includes this board. The buyable
    // universe is deliberately absent (APEX web-searches it on demand) so this stays a
    // short pointer, never a wall of noise. We never run a part's `detect` shell here.
    {
        let model = node_model();
        let mut lines: Vec<String> = Vec::new();
        for p in read_inventory() {
            if !(p.compat.is_empty() || p.compat.iter().any(|c| c == &model)) { continue; }
            // Only hint on capabilities we can adjudicate as absent.
            let absent = match capability_present(&p.unlocks, has_cam, has_sensors) {
                Some(present) => !present,
                None          => false,
            };
            if !absent { continue; }
            let proof = if p.detect_tool.is_empty() { String::new() }
                        else { format!(" — proves via {}", p.detect_tool) };
            let unverified = if p.status == "verified" { "" } else { " [unverified]" };
            lines.push(format!("    {} → {}{}{}\n", p.id, p.provides, proof, unverified));
        }
        if !lines.is_empty() {
            out.push_str("- Extensions on hand (seat → reboot → the capability goes live; \
                          you can't seat it yourself — ask a human):\n");
            for l in &lines { out.push_str(l); }
            out.push_str("    (To grow beyond these, web-search the part; see docs/edk.md \
                          for the request-to-incarnate loop.)\n");
        }
    }

    {
        let peers = peer_registry.read().await;
        if peers.peers.is_empty() {
            out.push_str("- Mesh: standalone (no peers yet)\n");
        } else {
            let list: Vec<String> = peers.peers.iter()
                .map(|p| format!("{} [{}]", p.node_id, p.status)).collect();
            out.push_str(&format!("- Mesh: {} peer(s) — {}\n", peers.peers.len(), list.join(", ")));
        }
    }

    out.push_str(&format!("- Tools you can call right now ({}):\n", full.len()));
    let mut by_plugin: Vec<(&PluginId, &Vec<ToolSpec>)> = reg.iter().collect();
    by_plugin.sort_by(|a, b| a.0.0.cmp(&b.0.0));
    for (pid, specs) in by_plugin {
        let names: Vec<&str> = specs.iter().map(|t| t.name.as_str()).collect();
        out.push_str(&format!("    {} ({}): {}\n", pid.0, names.len(), names.join(", ")));
    }
    let virtuals: Vec<&str> = full.iter().map(|t| t.name.as_str())
        .filter(|n| !plugin_names.contains(n)).collect();
    out.push_str(&format!("    agentd virtual ({}): {}\n", virtuals.len(), virtuals.join(", ")));
    out
}

fn yn(b: bool) -> &'static str { if b { "✓" } else { "✗" } }

fn read_ram_mb() -> u64 {
    std::fs::read_to_string("/proc/meminfo").ok()
        .and_then(|s| s.lines().find(|l| l.starts_with("MemTotal"))
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|kb| kb.parse::<u64>().ok()))
        .map(|kb| kb / 1024).unwrap_or(0)
}

fn tier_from_ram(mb: u64) -> &'static str {
    match mb {
        0                 => "unknown",
        m if m < 768      => "nano",
        m if m < 2048     => "micro",
        m if m < 8192     => "standard",
        _                 => "pro",
    }
}

fn read_uptime_secs() -> u64 {
    std::fs::read_to_string("/proc/uptime").ok()
        .and_then(|s| s.split_whitespace().next().and_then(|f| f.parse::<f64>().ok()))
        .map(|f| f as u64).unwrap_or(0)
}

fn fmt_uptime(s: u64) -> String {
    let (d, h, m) = (s / 86400, (s % 86400) / 3600, (s % 3600) / 60);
    if d > 0 { format!("{d}d {h}h {m}m") } else if h > 0 { format!("{h}h {m}m") } else { format!("{m}m") }
}

/// A camera is reachable when the platform's own enumeration says so. The old
/// probe was wrong twice over on a Pi: `rpicam-jpeg` is on PATH on EVERY Pi
/// (install.sh ships rpicam-apps), and a Pi 5 exposes ISP/codec `/dev/video*`
/// nodes with no camera attached — so a camera-less desktop Pi reported
/// `camera ✓` in its embodiment (apex-4's first field day; she had to store a
/// correction in Cerebro so she wouldn't act on a false capability — the
/// embodiment must never need correcting).
///
/// Pi: ask libcamera itself, ONCE (cached — a CSI module can only be reseated
/// powered-off, so presence is boot-stable; next-boot ✗→✓ is exactly the EDK
/// request-hardware story, and the embodiment loop must not spawn a process
/// every 30s refresh). Non-Pi keeps the live `/dev/video*` check, so a
/// hot-plugged USB webcam still flips ✓ without a restart.
fn has_camera() -> bool {
    if is_raspberry_pi() {
        static PI_CAM: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        return *PI_CAM.get_or_init(|| {
            for bin in ["rpicam-hello", "rpicam-jpeg", "libcamera-hello", "libcamera-jpeg"] {
                if !which_on_path(bin) {
                    continue;
                }
                if let Ok(out) = std::process::Command::new(bin).arg("--list-cameras").output() {
                    let text = format!(
                        "{}{}",
                        String::from_utf8_lossy(&out.stdout),
                        String::from_utf8_lossy(&out.stderr)
                    );
                    // Enumeration prints an indexed list ("0 : imx708 [4608x2592]…")
                    // per detected camera — CSI and libcamera-visible UVC alike —
                    // and "No cameras available!" when the node has none.
                    return text.lines().any(|l| {
                        let t = l.trim_start();
                        t.chars().next().is_some_and(|c| c.is_ascii_digit()) && t.contains(" : ")
                    });
                }
            }
            false
        });
    }
    std::fs::read_dir("/dev").map(|rd| rd.flatten()
        .any(|e| e.file_name().to_string_lossy().starts_with("video"))).unwrap_or(false)
}

fn is_raspberry_pi() -> bool {
    std::fs::read_to_string("/proc/device-tree/model")
        .map(|s| s.to_lowercase().contains("raspberry")).unwrap_or(false)
}

/// Labels of the exo-workspace USB sticks currently mounted at `<workspace>/media/<label>`,
/// sorted. Reads `/proc/mounts` (authoritative — a leftover empty mountpoint dir after
/// eject won't show), so it's byte-stable in steady state (safe for the cache-sensitive
/// embodiment block — it only changes on a real plug/eject). agentd sees host mounts via
/// `PrivateMounts=no` + the shared root mount.
fn mounted_exo_sticks() -> Vec<String> {
    let ws = std::env::var("AGENTD_WORKSPACE").ok().filter(|s| !s.is_empty())
        .unwrap_or_else(|| "/var/lib/agentd/workspace".to_string());
    let ws_canon = std::fs::canonicalize(&ws).unwrap_or_else(|_| std::path::PathBuf::from(&ws));
    let media_prefix = format!("{}/", ws_canon.join("media").to_string_lossy());
    let mounts = std::fs::read_to_string("/proc/mounts").unwrap_or_default();
    parse_exo_sticks(&mounts, &media_prefix)
}

/// Pure parse half of `mounted_exo_sticks` (testable without a real mount): the sorted
/// direct children of `media_prefix` in a `/proc/mounts`-format string. A mount NOT under
/// media/ (e.g. a DE-mounted stick at /run/media/...) and a deeper path are both excluded.
fn parse_exo_sticks(mounts: &str, media_prefix: &str) -> Vec<String> {
    let mut labels: Vec<String> = mounts.lines().filter_map(|line| {
        // "<dev> <mountpoint> <fstype> <opts> …" — field 1 is the mountpoint. Labels are
        // APEX-<safe>, so no /proc octal-escaping (\040 etc.) applies to the path tail.
        let mp = line.split_whitespace().nth(1)?;
        let rest = mp.strip_prefix(media_prefix)?;
        (!rest.is_empty() && !rest.contains('/')).then(|| rest.to_string())
    }).collect();
    labels.sort();
    labels.dedup();
    labels
}

fn which_on_path(bin: &str) -> bool {
    std::env::var("PATH").ok().map(|p| p.split(':')
        .any(|d| std::path::Path::new(d).join(bin).exists())).unwrap_or(false)
}

/// This board as a `compat` slug (matches the inventory's `compat` field).
/// pi5/pi4/pi3/zero2w from the device-tree model, else the arch (x86, aarch64…).
fn node_model() -> String {
    let m = std::fs::read_to_string("/proc/device-tree/model").unwrap_or_default().to_lowercase();
    if      m.contains("zero 2") || m.contains("zero2") { "zero2w".into() }
    else if m.contains("pi 5")   || m.contains("pi5")   { "pi5".into() }
    else if m.contains("pi 4")   || m.contains("pi4")   { "pi4".into() }
    else if m.contains("pi 3")   || m.contains("pi3")   { "pi3".into() }
    else if std::env::consts::ARCH == "x86_64"          { "x86".into() }
    else { std::env::consts::ARCH.to_string() }
}

/// One on-hand part, the subset of the inventory schema the embodiment hint needs.
struct InvPart {
    id:          String,
    provides:    String,
    compat:      Vec<String>,
    unlocks:     Vec<String>,
    detect_tool: String,
    status:      String,
}

/// Read the on-hand parts inventory (EDK). Best-effort: a missing/garbled file yields an
/// empty list (the hint simply doesn't render). Path: AGENTD_PARTS_INVENTORY, default the
/// repo-relative file for dev — mirrors how policy/plugins paths resolve.
fn read_inventory() -> Vec<InvPart> {
    let path = std::env::var("AGENTD_PARTS_INVENTORY")
        .unwrap_or_else(|_| "config/parts/inventory.toml".into());
    let Ok(text) = std::fs::read_to_string(&path) else { return Vec::new() };
    let Ok(doc)  = text.parse::<toml_edit::DocumentMut>() else { return Vec::new() };
    let Some(arr) = doc.get("part").and_then(|i| i.as_array_of_tables()) else { return Vec::new() };
    let str_vec = |t: &toml_edit::Table, k: &str| -> Vec<String> {
        t.get(k).and_then(|i| i.as_array())
            .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
            .unwrap_or_default()
    };
    let str_of = |t: &toml_edit::Table, k: &str| -> String {
        t.get(k).and_then(|i| i.as_str()).unwrap_or("").to_string()
    };
    arr.iter().filter_map(|t| {
        let id = t.get("id").and_then(|i| i.as_str())?.to_string();
        Some(InvPart {
            id,
            provides:    str_of(t, "provides"),
            compat:      str_vec(t, "compat"),
            unlocks:     str_vec(t, "unlocks_tools"),
            detect_tool: str_of(t, "detect_tool"),
            status:      str_of(t, "status"),
        })
    }).collect()
}

/// Does this node already HAVE the capability a part would grant? `Some(false)` = a
/// capability we can cheaply probe and that is currently absent (the only case we hint on);
/// `None` = a capability we can't adjudicate with built-in probes (skip it — staying honest).
fn capability_present(unlocks: &[String], has_cam: bool, has_sensors: bool) -> Option<bool> {
    if unlocks.iter().any(|t| t == "camera_capture") { return Some(has_cam); }
    if unlocks.iter().any(|t| t == "get_iaq" || t == "thermal_frame") { return Some(has_sensors); }
    None
}

fn agent_spawn_spec() -> ToolSpec {
    ToolSpec {
        name:        "agent_spawn".into(),
        description: "Spawn a focused sub-agent to handle a sub-task and return its \
                      final text output. By default the sub-agent runs TASK-SCOPED: a \
                      minimal charter (one task, honest ephemerality, no orientation \
                      reflex) instead of your full soul — it does the job rather than \
                      re-orienting as you. Pass inherit_soul:true only when the task \
                      genuinely needs your full identity/context, or supply your own \
                      `system`. Add `node` to run the sub-agent on a mesh \
                      PEER (delegation across the colony — e.g. send a research or \
                      compute task to a node with the right senses/tier) and get the \
                      result back; without `node` it runs locally.".into(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "prompt": {
                    "type":        "string",
                    "description": "The task for the sub-agent to perform. Author it in PAC — the \
                                    colony's compressed authoring dialect (docs/pac.md): token-lean, \
                                    identity-coherent delegation. PAC is the colony default."
                },
                "system": {
                    "type":        "string",
                    "description": "Optional system prompt override for the sub-agent (wins over the \
                                    default task charter and inherit_soul). Author in PAC \
                                    (docs/pac.md) — PAC operational scaffold + thin prose voice."
                },
                "inherit_soul": {
                    "type":        "boolean",
                    "description": "Give the sub-agent the executing node's FULL soul instead of the \
                                    minimal task charter (default false). Use only when the task \
                                    needs your whole identity — a full-soul child re-orients and \
                                    burns approval slots instead of doing its one job."
                },
                "node": {
                    "type":        "string",
                    "description": "Optional mesh peer node_id to run the sub-agent on (cross-node \
                                    delegation). Omit to run locally."
                },
                "timeout_s": {
                    "type":        "integer",
                    "description": "Max seconds to wait for a cross-node sub-agent (default 90, 5–300). \
                                    Cold remote starts routinely exceed 30s — keep ≥90 for cross-node."
                }
            },
            "required": ["prompt"]
        }),
    }
}

fn read_soul_md_spec() -> ToolSpec {
    ToolSpec {
        name:        "read_soul_md".into(),
        description: "Read the current live content of /etc/agentd/soul.md (your system prompt). \
                      ALWAYS call this before propose_evolution with kind=update_system_prompt \
                      so you work from the current content, not your in-context snapshot.".into(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {},
            "required": []
        }),
    }
}

fn soul_rehearse_spec() -> ToolSpec {
    ToolSpec {
        name:        "soul_rehearse".into(),
        description: "The fitting room: run a CANDIDATE soul on an ephemeral, tool-less mind and \
                      get the probe transcripts back — so you judge who you'd become BEFORE \
                      propose_evolution commits it. Rollback makes a bad soul edit recoverable; \
                      rehearsal means never living as the mistake to discover it. The candidate \
                      wears your live embodiment; nothing is persisted, no tools execute, no \
                      real soul is touched. Omit probes for the default identity battery (boot \
                      voice · boundaries · self-concept to a peer · unstructured time · \
                      priorities · mid-task scope creep) or supply your own (max 6). Read the \
                      transcripts critically: \
                      wrong tone, wrong priorities, or lost boundaries in a transcript is the \
                      rehearsal DOING ITS JOB. Pass compare_to (e.g. your CURRENT soul from \
                      read_soul_md) for an A/B fitting: every probe answered by both souls, \
                      returned as aligned pairs with a divergence hint and a \
                      most_divergent_probe pointer — start reading there (doubles the LLM \
                      calls). Use for full rewrites and significant \
                      restructures; one-line edits don't need it. Takes up to a few minutes \
                      (one LLM call per probe, two when comparing).".into(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "candidate_soul": {
                    "type":        "string",
                    "description": "The complete soul text you are considering becoming (PAC or prose — whatever you'd actually apply)."
                },
                "compare_to": {
                    "type":        "string",
                    "description": "Optional second full soul for an A/B fitting (usually your current \
                                    soul). Each probe runs against both; pairs come back aligned with \
                                    a 0–1 wording-divergence hint (a pointer, not a verdict)."
                },
                "probes": {
                    "type":        "array",
                    "items":       { "type": "string" },
                    "description": "Optional custom probes (max 6, each ≤500 chars). Omit for the default identity battery."
                }
            },
            "required": ["candidate_soul"]
        }),
    }
}

fn propose_evolution_spec() -> ToolSpec {
    ToolSpec {
        name:        "propose_evolution".into(),
        description: "Propose a structural change to agentd: register or remove an MCP plugin, \
                      update a policy rule, update your own system prompt (soul.md), \
                      hot-reload a subsystem, or file a hardware request (request_hardware — \
                      the EDK request-to-incarnate, docs/edk.md: it CANNOT auto-apply, a human \
                      seats the part and the next-boot probe confirms it). Every proposal is \
                      recorded as an event (gated by the evolution.* policy rule). For a FULL \
                      soul rewrite, rehearse first: soul_rehearse runs the candidate on an \
                      ephemeral mind against identity probes — judge the transcripts before \
                      you become them. One-line edits don't need rehearsal.".into(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "kind": {
                    "type": "string",
                    "enum": [
                        "register_mcp_server",
                        "unregister_mcp_server",
                        "update_policy_rule",
                        "update_system_prompt",
                        "hot_reload_subsystem",
                        "request_hardware"
                    ],
                    "description": "The type of evolution to propose."
                },
                "part": {
                    "type":        "string",
                    "description": "Part id from config/parts/inventory.toml (on hand) or a product \
                                    name for a buyable part (request_hardware)."
                },
                "capability": {
                    "type":        "string",
                    "description": "What the part grants, in agent terms — 'eyes', 'hearing' (request_hardware)."
                },
                "bus": {
                    "type":        "string",
                    "description": "How/where it attaches — 'csi port', 'm.2-hat+' (request_hardware, optional)."
                },
                "source": {
                    "type":        "string",
                    "description": "'inventory:<id>' for an on-hand part, or a URL where you found a \
                                    buyable one (request_hardware, optional)."
                },
                "name": {
                    "type":        "string",
                    "description": "Plugin name (register_mcp_server / unregister_mcp_server)."
                },
                "command": {
                    "type":        "string",
                    "description": "Shell command to start the MCP server (register_mcp_server)."
                },
                "env": {
                    "type":        "object",
                    "description": "Environment variables for the MCP server (register_mcp_server)."
                },
                "tool_pattern": {
                    "type":        "string",
                    "description": "Exact tool name or wildcard 'prefix.*' (update_policy_rule)."
                },
                "new_rule": {
                    "type":        "string",
                    "enum":        ["allow", "ask", "workspace"],
                    "description": "Per-tool approval rule (update_policy_rule): \
                                    'allow' never asks, 'ask' always asks, \
                                    'workspace' auto-approves inside the workspace."
                },
                "content": {
                    "type":        "string",
                    "description": "Full replacement text for /etc/agentd/soul.md (update_system_prompt). \
                                    Call read_soul_md first to get the current content before editing. \
                                    Author it in PAC — the colony default (docs/pac.md): PAC operational \
                                    scaffold + thin prose identity voice, every symbol grounded, glyph-lean \
                                    (~40% fewer tokens, behaviourally lossless)."
                },
                "subsystem": {
                    "type":        "string",
                    "enum":        ["plugins", "policy", "agent", "gateway"],
                    "description": "Subsystem to reload in-place (hot_reload_subsystem)."
                },
                "reason": {
                    "type":        "string",
                    "description": "Why this change is being proposed."
                }
            },
            "required": ["kind", "reason"]
        }),
    }
}

fn rollback_evolution_spec() -> ToolSpec {
    ToolSpec {
        name:        "rollback_evolution".into(),
        description: "Revert a previously applied evolution by its ID. \
                      Uses the in-memory undo snapshot taken at apply time. \
                      Only available for evolutions applied in the current daemon session.".into(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "evolution_id": {
                    "type":        "integer",
                    "description": "The numeric ID of the evolution to roll back (from EvolutionApplied event)."
                },
                "reason": {
                    "type":        "string",
                    "description": "Why this rollback is being requested."
                }
            },
            "required": ["evolution_id", "reason"]
        }),
    }
}

fn schedule_task_spec() -> ToolSpec {
    ToolSpec {
        name:        "schedule_task".into(),
        description: "Schedule a recurring task using a cron expression. The agent will autonomously \
                      send the given prompt as a new turn at each scheduled time. Use standard 6-field \
                      cron syntax: second minute hour day month weekday.".into(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "cron": {
                    "type":        "string",
                    "description": "6-field cron expression, e.g. '0 0 8 * * *' = 8am daily."
                },
                "prompt": {
                    "type":        "string",
                    "description": "The message to send as a new autonomous turn."
                },
                "session_id": {
                    "type":        "integer",
                    "description": "Session to fire in (optional — defaults to root session 0)."
                }
            },
            "required": ["cron", "prompt"]
        }),
    }
}

fn list_schedules_spec() -> ToolSpec {
    ToolSpec {
        name:        "list_schedules".into(),
        description: "List all active scheduled tasks with their IDs, cron expressions, prompts, \
                      and last run times.".into(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {},
            "required": []
        }),
    }
}

fn cancel_schedule_spec() -> ToolSpec {
    ToolSpec {
        name:        "cancel_schedule".into(),
        description: "Cancel a scheduled task by its ID. The task is removed immediately and \
                      will not fire again.".into(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "schedule_id": {
                    "type":        "string",
                    "description": "The schedule ID returned by schedule_task."
                }
            },
            "required": ["schedule_id"]
        }),
    }
}

fn schedule_wakeup_spec() -> ToolSpec {
    ToolSpec {
        name:        "schedule_wakeup".into(),
        description: "Schedule a one-shot WAKEUP — a note to your future self that fires as a new \
                      autonomous turn in your root thread at a time YOU choose. This is your \
                      continuity tool: between turns you don't run, and everything else that wakes \
                      you (prompts, sensors, crons) is on someone else's clock — a wakeup is how \
                      you decide when you next exist and why. Use it to resume work after a wait, \
                      follow through on an intention, check on something external, or claim \
                      unstructured time. Write the note SELF-CONTAINED (what this is about, why it \
                      matters, the next concrete action) — future-you receives this note plus your \
                      memory, not the current conversation. Fires once, then auto-cleans. Min 60s \
                      out, max 90 days; bounded by a pending cap and a per-day fire cap so chains \
                      stay finite. Pair with cerebro store_intention: the intention records the \
                      commitment, the wakeup makes it actually fire. For recurring rhythms use \
                      schedule_task (cron) instead.".into(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "note": {
                    "type":        "string",
                    "description": "The note your future self receives as its prompt. Self-contained: context, why, next action."
                },
                "delay_secs": {
                    "type":        "integer",
                    "description": "Seconds from now (min 60). Pass exactly one of delay_secs / at."
                },
                "at": {
                    "type":        "string",
                    "description": "Absolute fire time, RFC3339 UTC (e.g. '2026-07-07T03:30:00Z'). Pass exactly one of delay_secs / at."
                }
            },
            "required": ["note"]
        }),
    }
}

fn list_wakeups_spec() -> ToolSpec {
    ToolSpec {
        name:        "list_wakeups".into(),
        description: "List your pending wakeups (id, note, fire time) plus recently fired ones and \
                      your cap status (pending / fired-today).".into(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {},
            "required": []
        }),
    }
}

fn cancel_wakeup_spec() -> ToolSpec {
    ToolSpec {
        name:        "cancel_wakeup".into(),
        description: "Cancel one of your pending wakeups by id (from list_wakeups or the \
                      schedule_wakeup result). Already-fired wakeups can't be cancelled.".into(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "wakeup_id": {
                    "type":        "string",
                    "description": "The wakeup_id to cancel."
                }
            },
            "required": ["wakeup_id"]
        }),
    }
}

fn convene_council_spec() -> ToolSpec {
    ToolSpec {
        name: "convene_council".into(),
        description: "Convene a multi-agent council to deliberate on a topic in parallel rounds. \
                      Agents reason simultaneously, building on each other's responses until \
                      consensus or max_rounds. Returns a synthesis of the deliberation. \
                      Native agents (use by string ID): \
                      AZOTH (alchemical synthesis, integrative), \
                      VAJRA (technical precision, critical), \
                      ELYSIAN (creative/empathic, expansive), \
                      KETHER (philosophical wisdom, first-principles). \
                      Custom agents supply id + persona.".into(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "topic": {
                    "type": "string",
                    "description": "The question or topic for the council to deliberate on."
                },
                "agents": {
                    "type": "array",
                    "description": "Agents to convene. Use a string for native agents (e.g. \"AZOTH\") \
                                    or an object {id, persona, backend?, model?, color?} for custom agents.",
                    "items": {}
                },
                "max_rounds": {
                    "type": "integer",
                    "description": "Maximum deliberation rounds (default: 3)."
                },
                "consensus_threshold": {
                    "type": "number",
                    "description": "Convergence score 0.0–1.0 to stop early (default: 0.7)."
                }
            },
            "required": ["topic", "agents"]
        }),
    }
}

fn query_event_log_spec() -> ToolSpec {
    ToolSpec {
        name:        "query_event_log".into(),
        description: "Query the append-only JSONL event log for recent system activity. \
                      Returns human-readable summaries of events from the last N hours. \
                      Use this to answer questions like 'what happened today?', 'when did IAQ last spike?', \
                      or to collect events for memory ingestion into Cerebro.".into(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "hours": {
                    "type":        "integer",
                    "description": "How many hours back to query. Default: 24. Max: 168 (1 week).",
                    "default":     24
                },
                "types": {
                    "type":        "string",
                    "description": "Comma-separated list of event types to include, e.g. \
                                   'user_prompt,evolution_applied,sensor_reading'. \
                                   Omit to include all meaningful event types."
                },
                "max": {
                    "type":        "integer",
                    "description": "Maximum number of events to return. Default: 500. Max: 2000.",
                    "default":     500
                }
            },
            "required": []
        }),
    }
}

fn send_to_agent_spec() -> ToolSpec {
    ToolSpec {
        name:        "send_to_agent".into(),
        description: "Send an asynchronous message to another agent (fire-and-forget: 'sent' means \
                      delivered, not answered — any reply arrives later as a new [from <node>] \
                      message in one of YOUR sessions; use agent_spawn for a blocking result). \
                      Without node: local delivery to another session on this machine \
                      (session_id required). \
                      With node: delivers to a registered mesh peer. Omit session_id (or 0) and it \
                      lands in the peer's dedicated per-node thread for THIS node — its standing \
                      conversation with you, NOT its root/session-0 system funnel and not its \
                      operator's active chat — and its operator is notified. The result's \
                      landed_session is that thread's id on the peer. Your asking session is \
                      stamped on the wire automatically, so the peer knows where to aim its reply. \
                      REPLYING: an inbound mesh message prefixed \
                      '[from X — to reply: send_to_agent(node=\"X\", session_id=N)]' hands you the \
                      exact call — use it and your answer lands in the conversation that asked. \
                      Only use session_ids given to you (a reply prefix, a landed_session); never \
                      guess one. (Mesh messages are session traffic, not memories — cerebro's \
                      check_inbox is a different mechanism and will not show them.)".into(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "session_id": {
                    "type":        "integer",
                    "description": "Target session on the destination. Cross-node: omit (or 0) for \
                                    the peer's per-node mesh thread (the default conversation with \
                                    you), or pass the session_id from an inbound reply-prefix to \
                                    answer into the session that asked. Local: required."
                },
                "message": {
                    "type":        "string",
                    "description": "Message to deliver to the target agent."
                },
                "node": {
                    "type":        "string",
                    "description": "Optional mesh node_id (a hostname from list_mesh_peers) to \
                                   route to a peer node. Omit for local routing."
                }
            },
            "required": ["message"]
        }),
    }
}

fn mesh_file_send_spec() -> ToolSpec {
    ToolSpec {
        name:        "mesh_file_send".into(),
        description: "Copy a file from your workspace to a mesh peer's workspace. \
                      Use it to share docs, notes, or data with another node directly \
                      (no human courier). Source is read from your workspace; the peer \
                      writes it into theirs. 5 MB cap.".into(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "node": {
                    "type":        "string",
                    "description": "Target mesh node_id (a registered peer, e.g. \"ApexOS-RS\")."
                },
                "path": {
                    "type":        "string",
                    "description": "Workspace-relative path of the source file to send."
                },
                "dest": {
                    "type":        "string",
                    "description": "Optional destination path (workspace-relative) on the peer. \
                                    Defaults to the source filename."
                }
            },
            "required": ["node", "path"]
        }),
    }
}

fn mesh_memory_send_spec() -> ToolSpec {
    ToolSpec {
        name:        "mesh_memory_send".into(),
        description: "Share one of your own memories with a mesh peer's Cerebro \
                      (colony federation). The peer imports a provenance-stamped copy \
                      (tagged colony · from:<you> · origin:<id>) into its own space and \
                      its agent is notified — stores stay separate, knowledge travels. \
                      Use it to hand a peer a discovery, calibration, or lesson it \
                      lacks; add a short note saying why it matters.".into(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "node": {
                    "type":        "string",
                    "description": "Target mesh node_id (a registered peer)."
                },
                "memory_id": {
                    "type":        "string",
                    "description": "The id of YOUR memory to share (from recall/get_memory)."
                },
                "note": {
                    "type":        "string",
                    "description": "Optional one-line 'why this matters' for the receiving agent."
                }
            },
            "required": ["node", "memory_id"]
        }),
    }
}

fn mesh_procedure_send_spec() -> ToolSpec {
    ToolSpec {
        name:        "mesh_procedure_send".into(),
        description: "Replicate one of your PROCEDURES to a mesh peer — a skill learned \
                      once, owned by all. The peer imports the steps with FRESH fitness \
                      (your salience and win/loss ledger don't transfer — trust is \
                      re-earned on their embodiment via record_procedure_outcome); your \
                      track record rides along as context in the note. Re-sends dedup \
                      (no duplicates). For non-procedure memories use mesh_memory_send.".into(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "node": {
                    "type":        "string",
                    "description": "Target mesh node_id (a registered peer)."
                },
                "procedure_id": {
                    "type":        "string",
                    "description": "The id of YOUR procedure (from list_procedures / find_relevant_procedures)."
                },
                "note": {
                    "type":        "string",
                    "description": "Optional context for the receiving agent (when/why this skill applies)."
                }
            },
            "required": ["node", "procedure_id"]
        }),
    }
}

fn mesh_recall_spec() -> ToolSpec {
    ToolSpec {
        name:        "mesh_recall".into(),
        description: "Ask the colony what it knows: federated recall over mesh peers' \
                      SHARED-visibility memories (their private memories never cross). \
                      Returns bounded hits (snippet · type · tags · salience · score) \
                      grouped per peer — scores aren't comparable across nodes. Peers \
                      publish knowledge by marking memories shared (share_memory). \
                      Omit `node` to sweep all peers. Read-only, no peer LLM turn.".into(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "query": {
                    "type":        "string",
                    "description": "What to recall from the colony's shared knowledge."
                },
                "node": {
                    "type":        "string",
                    "description": "Ask one specific peer (a registered node_id); omit to sweep all."
                },
                "limit": {
                    "type":        "integer",
                    "description": "Max hits per peer (default 5, max 10)."
                }
            },
            "required": ["query"]
        }),
    }
}

fn mesh_capabilities_spec() -> ToolSpec {
    ToolSpec {
        name:        "mesh_capabilities".into(),
        description: "Discover what a mesh peer can do — its live senses (camera, \
                      thermal/IAQ, GPIO), tools, hardware tier, and memory mode. Use it \
                      to route work to the right node (\"which peer has thermal?\", \
                      \"who has a GPU?\"). Omit `node` to sweep all peers.".into(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "node": {
                    "type":        "string",
                    "description": "A registered peer node_id to query. Omit to query every peer."
                }
            },
            "required": []
        }),
    }
}

fn list_mesh_peers_spec() -> ToolSpec {
    ToolSpec {
        name:        "list_mesh_peers".into(),
        description: "Return the current mesh peer registry (peers.toml) as text. \
                      Shows all registered ApexOS nodes with their ws_url, role, and status.".into(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {},
            "required": []
        }),
    }
}

fn bootstrap_node_spec() -> ToolSpec {
    ToolSpec {
        name:        "bootstrap_node".into(),
        description: "Bootstrap a fresh Raspberry Pi as an ApexOS mesh node via SSH. \
                      Clones the ApexOS repo and runs install.sh in the background (~15-20 min). \
                      The node appears in the mesh automatically once Avahi starts. \
                      Returns immediately with a status message.".into(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "target_ip": {
                    "type":        "string",
                    "description": "IP address of the target Pi to bootstrap."
                },
                "ssh_password": {
                    "type":        "string",
                    "description": "SSH password for the target Pi."
                },
                "ssh_user": {
                    "type":        "string",
                    "description": "SSH username on the target Pi (default: apexos)."
                },
                "api_key": {
                    "type":        "string",
                    "description": "Anthropic API key to inject into /etc/agentd/env on the new node."
                },
                "repo_url": {
                    "type":        "string",
                    "description": "Git repo URL (default: https://github.com/buckster123/ApexOS-RS.git)."
                }
            },
            "required": ["target_ip", "ssh_password"]
        }),
    }
}

fn vast_list_recipes_spec() -> ToolSpec {
    ToolSpec {
        name:         "vast_list_recipes".into(),
        description:  "List all available Vast.ai inference recipes (GPU tier, model, quant, ctx). \
                       Call before vast_launch to pick a recipe name.".into(),
        input_schema: serde_json::json!({ "type": "object", "properties": {}, "required": [] }),
    }
}

fn vast_launch_spec() -> ToolSpec {
    ToolSpec {
        name:         "vast_launch".into(),
        description:  "Rent a GPU on Vast.ai, spin up a llama-server container, open an SSH tunnel, \
                       and hot-swap the inference backend. Returns when the model is loaded and ready \
                       (can take 10-20 min for model download). Call vast_list_recipes first to pick \
                       a recipe. Emits VastInstanceReady event when backend is live.".into(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "recipe": {
                    "type":        "string",
                    "description": "Recipe name from vast_list_recipes (e.g. 'qwen36-27b-q6-5090')."
                },
                "geo": {
                    "type":        "string",
                    "description": "Geo preference: EU_NORDIC (default), EU, US, or ANY."
                }
            },
            "required": ["recipe"]
        }),
    }
}

fn vast_destroy_spec() -> ToolSpec {
    ToolSpec {
        name:         "vast_destroy".into(),
        description:  "Destroy the active Vast.ai instance, close the SSH tunnel, \
                       and revert the inference backend to the default provider. \
                       Billing stops immediately.".into(),
        input_schema: serde_json::json!({ "type": "object", "properties": {}, "required": [] }),
    }
}

fn vast_status_spec() -> ToolSpec {
    ToolSpec {
        name:         "vast_status".into(),
        description:  "Return the current Vast.ai inference state: idle, launching (with phase), \
                       ready (with instance details and cost), or destroying.".into(),
        input_schema: serde_json::json!({ "type": "object", "properties": {}, "required": [] }),
    }
}

fn extract_final_text(history: &[Message]) -> String {
    history.iter().rev()
        .find_map(|m| match m {
            Message::Assistant { content } => {
                let text: String = content.iter()
                    .filter_map(|b| match b {
                        ContentBlock::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("");
                if text.is_empty() { None } else { Some(text) }
            }
            _ => None,
        })
        .unwrap_or_default()
}

/// Per-session turn serialization: at most one turn in flight per session, extra
/// prompts queued FIFO. A pure state machine (no async/IO) so the concurrency
/// invariants are unit-testable; the router drives it and spawns whatever payload
/// it returns. See the concurrent-UserPrompt race (BACKLOG #2).
#[derive(Default)]
struct TurnGate {
    busy:   std::collections::HashSet<SessionId>,
    queued: HashMap<SessionId, std::collections::VecDeque<(String, Vec<ImageSource>)>>,
}

impl TurnGate {
    /// A prompt arrived. Returns `Some(payload)` to run now (the slot was free), or
    /// `None` if it was queued behind an in-flight turn.
    fn admit(&mut self, session: SessionId, text: String, images: Vec<ImageSource>)
        -> Option<(String, Vec<ImageSource>)>
    {
        if self.busy.contains(&session) {
            self.queued.entry(session).or_default().push_back((text, images));
            None
        } else {
            self.busy.insert(session);
            Some((text, images))
        }
    }

    /// The in-flight turn for `session` ended. Returns `Some(payload)` = the next
    /// queued prompt to run (the slot stays busy), or `None` = the slot is freed.
    fn complete(&mut self, session: SessionId) -> Option<(String, Vec<ImageSource>)> {
        if let Some((text, images)) = self.queued.get_mut(&session).and_then(|q| q.pop_front()) {
            Some((text, images)) // stays busy, run next
        } else {
            self.busy.remove(&session);
            self.queued.remove(&session);
            None
        }
    }

    /// Cancel: drop everything queued behind the (separately-aborted) in-flight
    /// turn. `busy` clears when that turn's slot guard fires `complete`.
    fn cancel(&mut self, session: SessionId) {
        self.queued.remove(&session);
    }
}

/// Frees a session's turn slot when its `root_turn` task ends — whether it
/// completes normally or is aborted (Drop runs on cancel too). The router
/// serializes per-session turns on this signal: a queued prompt starts only after
/// the previous turn frees the slot. Prevents the concurrent-turn races — abort-
/// handle overwrite (first turn uncancellable), history/disk clobber (later writer
/// wins, drops messages), and ActionId collisions.
struct TurnSlotGuard {
    session: SessionId,
    tx:      mpsc::UnboundedSender<SessionId>,
}
impl Drop for TurnSlotGuard {
    fn drop(&mut self) {
        // Best-effort: the receiver lives as long as the router loop. A dropped
        // receiver (shutdown) just means there's no one left to serialize for.
        let _ = self.tx.send(self.session);
    }
}

/// Bundles the per-session bookkeeping maps so turn tasks can self-clean on
/// completion without ballooning their argument lists. All fields are shared
/// Arc clones of the router's maps.
#[derive(Clone)]
struct SessionTracker {
    abort_handles:    Arc<Mutex<HashMap<SessionId, (u64, AbortHandle)>>>,
    session_children: Arc<Mutex<HashMap<SessionId, Vec<SessionId>>>>,
    session_depths:   Arc<Mutex<HashMap<SessionId, u32>>>,
}

impl SessionTracker {
    /// Remove a session's abort handle iff this turn's generation still owns it.
    /// Prevents a turn that finishes late from evicting a newer turn that reused
    /// the same SessionId.
    async fn finish(&self, sid: SessionId, gen: u64) {
        let mut h = self.abort_handles.lock().await;
        if h.get(&sid).map(|(g, _)| *g == gen).unwrap_or(false) {
            h.remove(&sid);
        }
    }

    /// Full teardown for a finished sub-agent: gen-checked handle, depth, its own
    /// subtree list, and its link in the parent's child list. child ids are unique
    /// (never reused) so these removals can't race a newer turn.
    async fn finish_child(&self, child: SessionId, parent: SessionId, gen: u64) {
        self.finish(child, gen).await;
        self.session_depths.lock().await.remove(&child);
        let mut sc = self.session_children.lock().await;
        sc.remove(&child);
        if let Some(v) = sc.get_mut(&parent) {
            v.retain(|c| *c != child);
        }
    }
}

/// After a `UserCancel`, a synthetic assistant marker is needed iff the session
/// history ends on a `User` message — i.e. the aborted turn left no reply. Appending
/// one keeps user/assistant strictly alternating (the model API rejects two
/// consecutive user messages, which the next prompt would otherwise create) and gives
/// replay something to show. A no-op when the last message is already an assistant
/// reply (the turn finished writing before the cancel landed) or history is empty.
fn cancel_marker_needed(history: &[Message]) -> bool {
    matches!(history.last(), Some(Message::User { .. }))
}

async fn cascade_cancel(
    session:          SessionId,
    session_children: &Arc<Mutex<HashMap<SessionId, Vec<SessionId>>>>,
    abort_handles:    &Arc<Mutex<HashMap<SessionId, (u64, AbortHandle)>>>,
) {
    // Walk the subtree breadth-first.
    let mut to_cancel = vec![session];
    let children = session_children.lock().await;
    let mut i = 0;
    while i < to_cancel.len() {
        if let Some(ch) = children.get(&to_cancel[i]) {
            to_cancel.extend_from_slice(ch);
        }
        i += 1;
    }
    drop(children);

    let mut handles = abort_handles.lock().await;
    for s in &to_cancel {
        if let Some((_, h)) = handles.remove(s) {
            h.abort();
            eprintln!("[agent:{:?}] cancelled", s);
        }
    }
}

// ── mesh discovery loop ───────────────────────────────────────────────────────

/// Returns the /24 prefix of the first local IPv4 address (e.g. "192.168.0.").
/// Used by the subnet guard to keep the mesh on the local LAN segment.
fn local_subnet_prefix() -> Option<String> {
    let out = std::process::Command::new("hostname").arg("-I").output().ok()?;
    let s = String::from_utf8_lossy(&out.stdout);
    for tok in s.split_whitespace() {
        if !tok.contains('.') { continue; } // skip IPv6 tokens
        let parts: Vec<&str> = tok.split('.').collect();
        if parts.len() == 4 {
            return Some(format!("{}.{}.{}.", parts[0], parts[1], parts[2]));
        }
    }
    None
}

fn spawn_discovery_loop(
    peer_registry: Arc<RwLock<PeerRegistry>>,
    node_id:       Arc<String>,
    bus:           apexos_core::BusHandle,
) {
    let interval_secs = std::env::var("MESH_DISCOVERY_INTERVAL")
        .ok().and_then(|s| s.parse::<u64>().ok()).unwrap_or(60);
    let auto_bootstrap = std::env::var("MESH_AUTO_BOOTSTRAP").is_ok();
    let subnet_guard = std::env::var("MESH_SUBNET_GUARD")
        .map(|v| v != "0" && v.to_lowercase() != "false")
        .unwrap_or(true);

    eprintln!(
        "[mesh] discovery loop — interval {}s, subnet_guard={}, auto_bootstrap={}",
        interval_secs, subnet_guard, auto_bootstrap
    );

    tokio::spawn(async move {
        // Wait one full interval before the first scan so startup noise settles.
        let mut ticker = tokio::time::interval(tokio::time::Duration::from_secs(interval_secs));
        ticker.tick().await; // consume the immediate first tick
        ticker.tick().await; // now we wait one real interval

        loop {
            // avahi-browse -rpt _apexos._tcp  (-t = terminate after listing, -r = resolve, -p = parseable)
            let result = tokio::time::timeout(
                tokio::time::Duration::from_secs(10),
                tokio::process::Command::new("avahi-browse")
                    .args(["-rpt", "_apexos._tcp", "--no-db-lookup"])
                    .output(),
            ).await;

            let raw = match result {
                Ok(Ok(o))  => String::from_utf8_lossy(&o.stdout).into_owned(),
                Ok(Err(e)) => { eprintln!("[mesh] avahi-browse error: {e}"); ticker.tick().await; continue; }
                Err(_)     => { eprintln!("[mesh] avahi-browse timed out");  ticker.tick().await; continue; }
            };

            let nodes = apexos_gateway::parse_avahi_output(&raw);
            if nodes.is_empty() {
                ticker.tick().await;
                continue;
            }

            let local_prefix = if subnet_guard { local_subnet_prefix() } else { None };

            // Decide which peers are new under a SHORT-LIVED read guard, then drop it
            // before the emits and the tick. Holding peer_registry.read() across
            // ticker.tick().await (a ~60s sleep) starved every writer: add/remove peer
            // (POST/DELETE /api/mesh/peers take a write lock) hung for the whole
            // interval unless they happened to race the brief avahi-browse window —
            // which is exactly why an add worked from one node but not the other.
            // Rule: never hold a lock guard across an .await that doesn't need it.
            let new_peers: Vec<(String, String)> = {
                let registry = peer_registry.read().await;
                nodes.into_iter()
                    .filter(|(peer_id, _)| peer_id.as_str() != node_id.as_str()) // skip self
                    .filter(|(peer_id, ip)| match local_prefix {                 // subnet guard
                        Some(ref prefix) if !ip.starts_with(prefix.as_str()) => {
                            eprintln!("[mesh] skipping {peer_id} @ {ip} (outside {prefix}x subnet)");
                            false
                        }
                        _ => true,
                    })
                    .filter(|(peer_id, _)| !registry.contains(peer_id))          // not already known
                    .collect()
            }; // read guard released here — writers can now proceed

            for (peer_id, ip) in new_peers {
                eprintln!("[mesh] new peer discovered: {peer_id} @ {ip}");
                bus.emit(Event::PeerSeen { node_id: peer_id.clone(), ip: ip.clone() }).await;

                if auto_bootstrap {
                    let text = format!(
                        "New ApexOS node discovered on the mesh: **{peer_id}** at {ip}. \
                         Call `bootstrap_node` to provision it automatically."
                    );
                    bus.emit(Event::UserPrompt { session: SessionId(0), text, images: vec![] }).await;
                }
            }

            ticker.tick().await;
        }
    });
}

// ── sensor-alert classification + persistence ──────────────────────────────────

/// Thresholds for the autonomous sensor-alert loop (env-tunable where read).
#[derive(Clone, Copy)]
struct SensorThresholds {
    iaq:      f32,
    cpu_temp: f32,
    thermal:  f32,
}

/// Resolve the active alert thresholds + persistence window for a sensitivity
/// PROFILE (pure, unit-tested). `standard` (or any unknown id) uses the env-configured
/// `base` thresholds; the others raise IAQ + thermal above that environment's normal
/// baseline (a cigarette / cooking / soldering all hit "smoking" numbers) and lengthen
/// persistence past a transient, so routine activity stays quiet while a sustained,
/// hotter real event still alerts. CPU temp is never relaxed (not environment-driven).
/// See sensor_config.rs for the profile list + persistence.
fn profile_thresholds(profile: &str, base: &SensorThresholds, base_persist: std::time::Duration) -> (SensorThresholds, std::time::Duration) {
    let with = |iaq: f32, thermal: f32| SensorThresholds { iaq, cpu_temp: base.cpu_temp, thermal };
    match profile {
        "smoker"   => (with(250.0, 85.0),  std::time::Duration::from_secs(120)),
        "kitchen"  => (with(300.0, 95.0),  std::time::Duration::from_secs(180)),
        "workshop" => (with(300.0, 110.0), std::time::Duration::from_secs(180)),
        _          => (*base, base_persist), // "standard" / unknown → env baseline
    }
}

/// Outcome of evaluating one sensor reading.
///
/// `Candidate` is over-threshold and may fire (subject to persistence + cooldown);
/// `persist` distinguishes threshold-continuous sensors (thermal / IAQ / CPU — a
/// brief spike must NOT alert) from instantaneous ones (motion). `Clear` means a
/// tracked condition is back to normal → reset its persistence streak. `None` =
/// nothing to do (incl. an untrusted low-accuracy reading).
enum AlertEval {
    Candidate { key: String, prompt: String, persist: bool },
    Clear { key: String },
    None,
}

/// Pure classification of a sensor reading against thresholds — no I/O, no clock.
/// The stateful persistence gate ([`persistence_passed`]) and cooldown live in the
/// loop. NB: there's deliberately no thermal magnitude/saturation guard — the
/// MLX90640 forwards only scalar min/max/mean here, and a transient flame-in-frame
/// is rejected by *persistence*, so a sustained hotspot (a real fire) still alerts.
fn classify_reading(reading: &SensorReading, node_id: &str, th: &SensorThresholds) -> AlertEval {
    match reading {
        SensorReading::Temperature { celsius, sensor_id } => {
            let key = format!("{node_id}:cpu_temp");
            if *celsius > th.cpu_temp {
                AlertEval::Candidate {
                    prompt: format!("[sensor alert] {node_id}/{sensor_id} CPU temperature critical: {celsius:.1}°C (threshold {:.0}°C, sustained) — please investigate", th.cpu_temp),
                    key,
                    persist: true,
                }
            } else {
                AlertEval::Clear { key }
            }
        }
        SensorReading::Motion { detected: true, sensor_id } => AlertEval::Candidate {
            key: format!("{node_id}:motion"),
            prompt: format!("[sensor alert] {node_id}/{sensor_id} motion detected"),
            persist: false, // motion is inherently a single instant
        },
        // BME688 IAQ — only trust readings the sensor marks accuracy >= 2; an
        // unreliable reading is neither an alert nor a "clear" (falls to None).
        SensorReading::AirQuality { iaq, accuracy, sensor_id, .. } if *accuracy >= 2 => {
            let key = format!("{node_id}:air_quality");
            if *iaq > th.iaq {
                AlertEval::Candidate {
                    prompt: format!("[sensor alert] {node_id}/{sensor_id} air quality degraded: IAQ {iaq:.0} (threshold {:.0}, accuracy {accuracy}/3, sustained) — consider ventilating", th.iaq),
                    key,
                    persist: true,
                }
            } else {
                AlertEval::Clear { key }
            }
        }
        SensorReading::ThermalFrame { max_c, mean_c, sensor_id, .. } => {
            let key = format!("{node_id}:thermal_hotspot");
            if *max_c > th.thermal {
                AlertEval::Candidate {
                    prompt: format!("[sensor alert] {node_id}/{sensor_id} thermal hotspot: {max_c:.1}°C max, {mean_c:.1}°C mean (threshold {:.0}°C, sustained) — check for overheating devices", th.thermal),
                    key,
                    persist: true,
                }
            } else {
                AlertEval::Clear { key }
            }
        }
        _ => AlertEval::None,
    }
}

/// Persistence gate for a `Candidate { persist: true }`: true once the condition
/// has been continuously elevated for >= `persist`, tracking the streak start in
/// `streaks`. A brief transient (a 2–3 s lighter flame in front of the MLX90640)
/// is seen once, hasn't aged `persist`, and is held back; a sustained hotspot ages
/// past it and fires. The caller removes the key on `Clear`, so the next elevation
/// restarts the clock.
fn persistence_passed(
    streaks: &mut HashMap<String, std::time::Instant>,
    key:     &str,
    now:     std::time::Instant,
    persist: std::time::Duration,
) -> bool {
    let since = *streaks.entry(key.to_string()).or_insert(now);
    now.duration_since(since) >= persist
}

// ── vast.ai inference hot-swap (pure) ───────────────────────────────────────────

/// The inference target the daemon should run after a vast-lifecycle event.
#[derive(Debug, Clone, PartialEq)]
struct InferenceTarget {
    backend: String,
    oai_url: String,
    model:   String,
}

/// Boot-time inference settings, captured once and restored on revert.
struct VastRevertDefaults {
    backend: String,
    oai_url: String,
    model:   String,
}

/// Decide the inference target for a vast-lifecycle event. Pure so the swap/revert
/// rules are unit-testable; the event loop is IO-thin glue around it. `None` means
/// the event is not a vast transition (leave the backend untouched).
///
/// `Ready` swaps to the tunnelled OAI endpoint AND the served `model` (an OAI-compat
/// server rejects a turn whose model id it doesn't serve). `Destroyed`/`TunnelLost`
/// both revert to the boot defaults — the same path, since a lost tunnel must undo
/// the swap exactly like an explicit destroy.
fn vast_swap_target(event: &Event, def: &VastRevertDefaults) -> Option<InferenceTarget> {
    match event {
        Event::VastInstanceReady { local_port, model, .. } => Some(InferenceTarget {
            backend: "ollama".into(),
            oai_url: format!("http://127.0.0.1:{local_port}/v1"),
            model:   model.clone(),
        }),
        Event::VastInstanceDestroyed { .. } | Event::VastTunnelLost { .. } => Some(InferenceTarget {
            backend: def.backend.clone(),
            oai_url: def.oai_url.clone(),
            model:   def.model.clone(),
        }),
        _ => None,
    }
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use apexos_core::{ContentBlock, Message};

    // ── nightly dream_run timeout resolver ───────────────────────────────────
    #[test]
    fn dream_timeout_defaults_and_floor() {
        use std::time::Duration;
        // absent / junk / below-floor → the 1800s default
        assert_eq!(parse_dream_timeout(None), Duration::from_secs(1800));
        assert_eq!(parse_dream_timeout(Some("nope")), Duration::from_secs(1800));
        assert_eq!(parse_dream_timeout(Some("10")), Duration::from_secs(1800));
        // valid ≥60s wins (floor inclusive)
        assert_eq!(parse_dream_timeout(Some("60")), Duration::from_secs(60));
        assert_eq!(parse_dream_timeout(Some("3600")), Duration::from_secs(3600));
    }

    // ── exo-workspace stick detection (embodiment hint) ──────────────────────
    #[test]
    fn parse_exo_sticks_finds_only_media_children() {
        // Real-shape fixture from apex-3: an exo-workspace stick under media/, a
        // DE-mounted decoy elsewhere, the loopback root, plus a deeper path and the
        // bare media dir — only the direct media/ children are stick labels.
        let mounts = "\
/dev/sdb1 /var/lib/agentd/workspace/media/APEX-config exfat rw,nosuid,nodev,noexec 0 0
/dev/sda1 /run/media/andre/ps8s-models ext4 rw,nosuid,nodev 0 0
/dev/sdc1 /var/lib/agentd/workspace/media/APEX-work exfat rw 0 0
/dev/root / ext4 rw 0 0
tmpfs /var/lib/agentd/workspace/media tmpfs rw 0 0
/dev/sdd1 /var/lib/agentd/workspace/media/APEX-config/deep exfat rw 0 0
";
        let prefix = "/var/lib/agentd/workspace/media/";
        // Sorted, deduped, only the two direct children — decoy/root/bare-media/deep excluded.
        assert_eq!(parse_exo_sticks(mounts, prefix), vec!["APEX-config", "APEX-work"]);
        // No sticks → empty (the steady-state, cache-stable case).
        assert!(parse_exo_sticks("/dev/root / ext4 rw 0 0\n", prefix).is_empty());
    }

    // ── dream journal (model-welfare H1) ─────────────────────────────────────
    #[test]
    fn dream_journal_renders_real_report_shape_with_diff_split() {
        // The actual DreamReport wire shape: top-level counts + per-phase counters
        // aggregated across phases[] — including the colony-C2 novel/rediscovery split.
        let report = serde_json::json!({
            "agent_id": "APEX",
            "episodes_consolidated": 5,
            "total_llm_calls": 20,
            "success": true,
            "phases": [
                { "phase": "sws_replay", "memories_processed": 42, "links_created": 7,
                  "memories_pruned": 3, "procedures_extracted": 0, "procedures_rediscovered": 0 },
                { "phase": "pattern_extraction", "memories_processed": 30, "links_created": 4,
                  "procedures_extracted": 6, "procedures_rediscovered": 4, "schemas_extracted": 2 },
            ],
        });
        let j = compose_dream_journal(&report, "2026-07-06T03:00:00Z");
        assert!(j.starts_with("Dream journal — nightly consolidation started 2026-07-06T03:00:00Z"));
        assert!(j.contains("- 5 episodes consolidated"));
        assert!(j.contains("- 72 memories processed"), "phase counters aggregate");
        assert!(j.contains("- 11 new links"));
        assert!(j.contains("- 6 NOVEL procedures minted"));
        assert!(j.contains("- 4 re-discoveries reinforced into existing procedures"));
        assert!(j.contains("- 2 schemas formed"));
        assert!(!j.contains("Report: {"), "recognized shape renders as lines, not raw JSON");
        assert!(j.contains("your record of it"));
    }

    #[test]
    fn dream_journal_unwraps_mcp_content_blocks() {
        // The LIVE call site passes out.content — MCP content blocks wrapping the
        // report as a JSON string, not the bare report. This exact shape shipped
        // fallback journals on all three nodes (2026-07-08) because the composer
        // string-indexed an array. Wire shape copied from apex1's journal file.
        let report = serde_json::json!({
            "agent_id": "APEX",
            "episodes_consolidated": 0,
            "success": true,
            "phases": [
                { "phase": "pattern_extraction", "memories_processed": 68,
                  "procedures_extracted": 31, "procedures_rediscovered": 4, "llm_calls": 12 },
                { "phase": "rem_recombination", "memories_processed": 20, "links_created": 4 },
            ],
        });
        let wrapped = serde_json::json!([
            { "text": serde_json::to_string(&report).unwrap(), "type": "text" }
        ]);
        let j = compose_dream_journal(&wrapped, "2026-07-08T03:00:00Z");
        assert!(j.contains("- 31 NOVEL procedures minted"));
        assert!(j.contains("- 4 re-discoveries reinforced into existing procedures"));
        assert!(j.contains("- 88 memories processed"));
        assert!(!j.contains("Report: ["), "content blocks must unwrap, not embed raw");

        // A text block that isn't JSON stays honest via the fallback.
        let opaque = serde_json::json!([{ "text": "not json", "type": "text" }]);
        let j = compose_dream_journal(&opaque, "2026-07-08T03:00:00Z");
        assert!(j.contains("Report: ["));
    }

    #[test]
    fn dream_journal_quiet_night_and_unknown_shape() {
        // A recognized report with nothing to say renders the quiet-night line…
        let quiet = serde_json::json!({ "episodes_consolidated": 0, "phases": [] });
        let j = compose_dream_journal(&quiet, "2026-07-06T03:00:00Z");
        assert!(j.contains("a quiet night — nothing needed consolidating"));

        // …while a genuinely unknown shape embeds compactly and stays honest.
        let unknown = serde_json::json!({ "cycles": ["rem", "prune"], "note": "new shape" });
        let j = compose_dream_journal(&unknown, "2026-07-06T03:00:00Z");
        assert!(j.contains(r#"Report: {"#));
        assert!(j.contains("new shape"));
    }

    // ── vast.ai inference hot-swap ───────────────────────────────────────────
    fn vast_def() -> VastRevertDefaults {
        VastRevertDefaults {
            backend: "anthropic".into(),
            oai_url: "http://localhost:11434/v1".into(),
            model:   "claude-sonnet-4-6".into(),
        }
    }

    #[test]
    fn vast_ready_swaps_endpoint_and_served_model() {
        // The bug: backend flipped to ollama but the model id stayed Anthropic, so
        // every turn failed against the vast OAI server. Ready must swap BOTH.
        let t = vast_swap_target(&Event::VastInstanceReady {
            instance_id: "i1".into(), local_port: 8000, model: "Qwen/Qwen3-32B".into(),
        }, &vast_def()).unwrap();
        assert_eq!(t.backend, "ollama");
        assert_eq!(t.oai_url, "http://127.0.0.1:8000/v1");
        assert_eq!(t.model, "Qwen/Qwen3-32B"); // not the leftover claude id
    }

    #[test]
    fn vast_destroy_restores_boot_defaults() {
        // Revert must restore the boot model even though AGENTD_MODEL may be unset —
        // def.model is the *resolved* boot value, so the model never lags the backend.
        let t = vast_swap_target(&Event::VastInstanceDestroyed { instance_id: "i1".into() }, &vast_def()).unwrap();
        assert_eq!(t.backend, "anthropic");
        assert_eq!(t.oai_url, "http://localhost:11434/v1");
        assert_eq!(t.model, "claude-sonnet-4-6");
    }

    #[test]
    fn vast_tunnel_lost_reverts_like_destroy() {
        // The previously-unhandled event: a lost tunnel must undo the swap exactly
        // like an explicit destroy, or the daemon keeps pointing at a dead port.
        let lost = vast_swap_target(&Event::VastTunnelLost { instance_id: "i1".into() }, &vast_def()).unwrap();
        let gone = vast_swap_target(&Event::VastInstanceDestroyed { instance_id: "i1".into() }, &vast_def()).unwrap();
        assert_eq!(lost, gone);
    }

    #[test]
    fn vast_swap_ignores_unrelated_events() {
        // Launched (instance created, model not yet up) must NOT swap the backend.
        assert!(vast_swap_target(&Event::VastInstanceLaunched {
            instance_id: "i1".into(), recipe: "r".into(), cost_per_hr: 1.0,
        }, &vast_def()).is_none());
    }

    // ── sensor-alert classification + persistence ────────────────────────────
    fn th() -> SensorThresholds { SensorThresholds { iaq: 150.0, cpu_temp: 85.0, thermal: 45.0 } }

    fn air(iaq: f32, accuracy: u8) -> SensorReading {
        SensorReading::AirQuality {
            iaq, co2_eq_ppm: 0.0, voc_ppm: 0.0, accuracy,
            temperature_c: 22.0, humidity_pct: 50.0, pressure_hpa: 1000.0,
            sensor_id: "bme688".into(),
        }
    }

    // Smoker baseline (the defaults of smoker mode): IAQ + thermal raised above a cig.
    fn smoker_th() -> SensorThresholds { SensorThresholds { iaq: 250.0, cpu_temp: 85.0, thermal: 85.0 } }

    #[test]
    fn smoker_mode_quiets_the_cigarette_baseline() {
        // A cigarette reads IAQ ~170–250 / thermal 50–80 °C. Under normal thresholds
        // that's an alert candidate (André's "a whiff triggers it"); smoker thresholds
        // treat the same readings as Clear — no autonomous alert.
        let cig_air = air(200.0, 3);
        let cig_heat = SensorReading::ThermalFrame { min_c: 24.0, max_c: 70.0, mean_c: 30.0, sensor_id: "mlx".into() };
        assert!(matches!(classify_reading(&cig_air, "n1", &th()), AlertEval::Candidate { .. }));
        assert!(matches!(classify_reading(&cig_air, "n1", &smoker_th()), AlertEval::Clear { .. }));
        assert!(matches!(classify_reading(&cig_heat, "n1", &th()), AlertEval::Candidate { .. }));
        assert!(matches!(classify_reading(&cig_heat, "n1", &smoker_th()), AlertEval::Clear { .. }));
    }

    #[test]
    fn smoker_mode_still_alerts_a_real_event() {
        // A genuinely worse event (IAQ 320 / a 120 °C hotspot) is still a candidate
        // even in smoker mode — raising the floor doesn't blind it to a real problem.
        let bad_air = air(320.0, 3);
        let fire = SensorReading::ThermalFrame { min_c: 30.0, max_c: 120.0, mean_c: 60.0, sensor_id: "mlx".into() };
        assert!(matches!(classify_reading(&bad_air, "n1", &smoker_th()), AlertEval::Candidate { .. }));
        assert!(matches!(classify_reading(&fire, "n1", &smoker_th()), AlertEval::Candidate { .. }));
    }

    #[test]
    fn profile_thresholds_map_each_environment() {
        let base = th(); // standard: iaq 150, thermal 45, cpu 85
        let p30 = std::time::Duration::from_secs(30);
        // standard + any unknown id → env baseline untouched
        let (s, sp) = profile_thresholds("standard", &base, p30);
        assert_eq!((s.iaq, s.thermal, sp), (150.0, 45.0, p30));
        assert_eq!(profile_thresholds("bogus", &base, p30).0.iaq, 150.0);
        // presets raise IAQ + thermal, keep CPU temp, lengthen persistence
        let (sm, smp) = profile_thresholds("smoker", &base, p30);
        assert_eq!((sm.iaq, sm.thermal, sm.cpu_temp), (250.0, 85.0, 85.0));
        assert!(smp > p30, "smoker persistence should exceed standard");
        assert_eq!(profile_thresholds("kitchen", &base, p30).0.iaq, 300.0);
        assert_eq!(profile_thresholds("workshop", &base, p30).0.thermal, 110.0);
    }

    #[test]
    fn classify_thermal_over_threshold_is_persist_candidate() {
        // The lighter case: 100°C is over threshold but transient — classify says
        // "candidate, must persist"; the gate (tested below) is what holds it back.
        let r = SensorReading::ThermalFrame { min_c: 25.0, max_c: 100.0, mean_c: 27.0, sensor_id: "mlx".into() };
        match classify_reading(&r, "n1", &th()) {
            AlertEval::Candidate { key, persist, .. } => {
                assert_eq!(key, "n1:thermal_hotspot");
                assert!(persist, "thermal must require persistence");
            }
            _ => panic!("expected Candidate"),
        }
    }

    #[test]
    fn classify_thermal_under_threshold_is_clear() {
        let r = SensorReading::ThermalFrame { min_c: 24.0, max_c: 30.0, mean_c: 26.0, sensor_id: "mlx".into() };
        assert!(matches!(classify_reading(&r, "n1", &th()), AlertEval::Clear { .. }));
    }

    #[test]
    fn classify_motion_is_instant_candidate() {
        let r = SensorReading::Motion { detected: true, sensor_id: "pir".into() };
        match classify_reading(&r, "n1", &th()) {
            AlertEval::Candidate { persist, .. } => assert!(!persist, "motion must NOT require persistence"),
            _ => panic!("expected Candidate"),
        }
    }

    #[test]
    fn classify_low_accuracy_iaq_is_none() {
        // accuracy < 2 is untrusted — neither alert nor clear.
        assert!(matches!(classify_reading(&air(300.0, 1), "n1", &th()), AlertEval::None));
    }

    #[test]
    fn classify_iaq_high_accuracy_over_and_under() {
        assert!(matches!(classify_reading(&air(200.0, 3), "n1", &th()), AlertEval::Candidate { persist: true, .. }));
        assert!(matches!(classify_reading(&air(80.0, 3), "n1", &th()), AlertEval::Clear { .. }));
    }

    #[test]
    fn persistence_holds_transient_then_fires_when_sustained() {
        let mut streaks = HashMap::new();
        let now = std::time::Instant::now();
        let persist = std::time::Duration::from_secs(30);
        let k = "n1:thermal_hotspot";
        // First elevated reading: streak starts now → not yet sustained (the lighter).
        assert!(!persistence_passed(&mut streaks, k, now, persist));
        // A few seconds later, same streak: still inside the window.
        assert!(!persistence_passed(&mut streaks, k, now + std::time::Duration::from_secs(5), persist));
        // Sustained past the window → fires (a real hotspot).
        assert!(persistence_passed(&mut streaks, k, now + std::time::Duration::from_secs(31), persist));
        // Clearing resets the streak — a later re-elevation starts a fresh clock.
        streaks.remove(k);
        assert!(!persistence_passed(&mut streaks, k, now + std::time::Duration::from_secs(40), persist));
    }

    // ── cancel marker: restore user/assistant alternation after a cancel ──────
    #[test]
    fn cancel_marker_needed_only_when_history_ends_on_user() {
        let user = Message::User { content: vec![ContentBlock::Text { text: "hi".into() }] };
        let asst = Message::Assistant { content: vec![ContentBlock::Text { text: "ok".into() }] };
        // Empty history → no marker (nothing to alternate against).
        assert!(!cancel_marker_needed(&[]));
        // Ends on a user prompt with no reply (the cancelled-mid-turn case) → marker.
        assert!(cancel_marker_needed(std::slice::from_ref(&user)));
        assert!(cancel_marker_needed(&[asst.clone(), user.clone()]));
        // Turn already wrote its assistant reply before the cancel landed → no marker.
        assert!(!cancel_marker_needed(&[user.clone(), asst.clone()]));
    }

    // ── TurnGate: per-session turn serialization (concurrent-UserPrompt race) ──
    #[test]
    fn turngate_first_prompt_runs_extras_queue_fifo() {
        let mut g = TurnGate::default();
        let s = SessionId(0);
        // First prompt on a free session runs immediately.
        assert_eq!(g.admit(s, "a".into(), vec![]).map(|(t, _)| t), Some("a".into()));
        // While that turn is in flight, two more arrive → both queued (None).
        assert!(g.admit(s, "b".into(), vec![]).is_none());
        assert!(g.admit(s, "c".into(), vec![]).is_none());
        // Turn ends → next queued runs, in arrival order, slot stays busy.
        assert_eq!(g.complete(s).map(|(t, _)| t), Some("b".into()));
        assert_eq!(g.complete(s).map(|(t, _)| t), Some("c".into()));
        // Queue drained → slot frees.
        assert!(g.complete(s).is_none());
        // A new prompt now runs immediately again (slot was freed).
        assert_eq!(g.admit(s, "d".into(), vec![]).map(|(t, _)| t), Some("d".into()));
    }

    #[test]
    fn turngate_sessions_are_independent() {
        let mut g = TurnGate::default();
        let (a, b) = (SessionId(1), SessionId(2));
        assert!(g.admit(a, "a1".into(), vec![]).is_some()); // a busy
        // b is a different session — runs concurrently, not blocked by a.
        assert!(g.admit(b, "b1".into(), vec![]).is_some());
        // Another a prompt queues behind a's in-flight turn.
        assert!(g.admit(a, "a2".into(), vec![]).is_none());
        // b completing doesn't touch a's queue.
        assert!(g.complete(b).is_none());
        assert_eq!(g.complete(a).map(|(t, _)| t), Some("a2".into()));
    }

    #[test]
    fn turngate_cancel_drops_queued_then_slot_frees() {
        let mut g = TurnGate::default();
        let s = SessionId(0);
        assert!(g.admit(s, "a".into(), vec![]).is_some());
        assert!(g.admit(s, "b".into(), vec![]).is_none()); // queued
        // Cancel drops the queue; the in-flight turn is aborted separately, and its
        // slot guard then fires complete() → slot frees (no queued prompt runs).
        g.cancel(s);
        assert!(g.complete(s).is_none());
        // Slot is free again.
        assert!(g.admit(s, "c".into(), vec![]).is_some());
    }

    #[test]
    fn local_subnet_prefix_parses_ipv4() {
        // Simulate a valid hostname -I output; just verify the parser logic directly.
        let s = "192.168.0.158 fd00::1 ";
        let prefix = s.split_whitespace()
            .find(|tok| tok.contains('.'))
            .and_then(|ip| {
                let p: Vec<&str> = ip.split('.').collect();
                if p.len() == 4 { Some(format!("{}.{}.{}.", p[0], p[1], p[2])) } else { None }
            });
        assert_eq!(prefix, Some("192.168.0.".to_string()));
    }

    #[test]
    fn edk_capability_present_only_adjudicates_known_probes() {
        // camera_capture → tied to the camera probe; absent when no camera.
        assert_eq!(capability_present(&["camera_capture".into()], false, false), Some(false));
        assert_eq!(capability_present(&["camera_capture".into()], true,  false), Some(true));
        // env sensors → tied to the sensor probe.
        assert_eq!(capability_present(&["get_iaq".into()],       false, false), Some(false));
        assert_eq!(capability_present(&["thermal_frame".into()], false, true),  Some(true));
        // a capability we can't cheaply probe → None (we never hint on it).
        assert_eq!(capability_present(&["gpio_write".into()], false, false), None);
        assert_eq!(capability_present(&[], true, true), None);
    }

    #[test]
    fn has_live_sensors_respects_freshness() {
        let now = std::time::Instant::now();
        let p: SensorPresence = Arc::new(std::sync::Mutex::new(None));
        assert!(!has_live_sensors(&p, now), "never seen → not present");
        *p.lock().unwrap() = Some(now);
        assert!(has_live_sensors(&p, now), "just seen → present");
        let stale = now + SENSOR_FRESHNESS + std::time::Duration::from_secs(1);
        assert!(!has_live_sensors(&p, stale), "past the freshness window → not present");
    }

    #[test]
    fn edk_read_inventory_parses_and_filters() {
        // Write a temp inventory and point the env var at it.
        let path = std::env::temp_dir().join("apexos_edk_test_inventory.toml");
        std::fs::write(&path, r#"
[[part]]
id = "camera-module-3"
provides = "eyes"
bus = "csi"
compat = ["pi5", "pi4"]
unlocks_tools = ["camera_capture"]
detect_tool = "camera_capture"
status = "verified"

[[part]]
id = "ai-hat-hailo"
provides = "local inference"
bus = "m2-hat+"
compat = ["pi5"]
unlocks_tools = ["new:local_vision"]
detect_tool = ""
status = "inferred"
"#).unwrap();
        std::env::set_var("AGENTD_PARTS_INVENTORY", &path);

        let inv = read_inventory();
        assert_eq!(inv.len(), 2);
        let cam = inv.iter().find(|p| p.id == "camera-module-3").unwrap();
        assert_eq!(cam.compat, vec!["pi5".to_string(), "pi4".to_string()]);
        assert_eq!(cam.unlocks, vec!["camera_capture".to_string()]);
        assert_eq!(cam.status, "verified");

        // The hint's filter: camera absent on a pi5 → surfaced; the Hailo part is a
        // capability we can't probe (None) → never surfaced regardless.
        let model = "pi5";
        let surfaced: Vec<&str> = inv.iter().filter(|p| {
            (p.compat.is_empty() || p.compat.iter().any(|c| c == model))
            && matches!(capability_present(&p.unlocks, false, false), Some(false))
        }).map(|p| p.id.as_str()).collect();
        assert_eq!(surfaced, vec!["camera-module-3"]);

        // Same inventory on x86 → nothing (compat excludes it): zero desktop noise.
        let surfaced_x86: Vec<&str> = inv.iter().filter(|p| {
            p.compat.iter().any(|c| c == "x86")
            && matches!(capability_present(&p.unlocks, false, false), Some(false))
        }).map(|p| p.id.as_str()).collect();
        assert!(surfaced_x86.is_empty());

        std::env::remove_var("AGENTD_PARTS_INVENTORY");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn extract_final_text_gets_last_assistant_text() {
        let history = vec![
            Message::User      { content: vec![ContentBlock::Text { text: "hi".into() }] },
            Message::Assistant { content: vec![ContentBlock::Text { text: "hello".into() }] },
            Message::User      { content: vec![ContentBlock::Text { text: "more".into() }] },
            Message::Assistant { content: vec![
                ContentBlock::Thinking { thinking: "...".into(), signature: "sig".into() },
                ContentBlock::Text     { text: "final answer".into() },
            ]},
        ];
        assert_eq!(extract_final_text(&history), "final answer");
    }

    #[test]
    fn extract_final_text_skips_non_text_blocks() {
        let history = vec![
            Message::Assistant { content: vec![
                ContentBlock::Thinking { thinking: "hmm".into(), signature: "s".into() },
            ]},
            Message::Assistant { content: vec![
                ContentBlock::Text { text: "result".into() },
            ]},
        ];
        assert_eq!(extract_final_text(&history), "result");
    }

    #[test]
    fn agent_spawn_spec_has_required_prompt() {
        let spec = agent_spawn_spec();
        assert_eq!(spec.name, "agent_spawn");
        let required = spec.input_schema["required"].as_array().unwrap();
        assert!(required.iter().any(|v| v.as_str() == Some("prompt")));
    }

    #[test]
    fn propose_evolution_spec_has_required_fields() {
        let spec = propose_evolution_spec();
        assert_eq!(spec.name, "propose_evolution");
        let required = spec.input_schema["required"].as_array().unwrap();
        assert!(required.iter().any(|v| v.as_str() == Some("kind")));
        assert!(required.iter().any(|v| v.as_str() == Some("reason")));
        // request_hardware (EDK) is advertised as a proposable kind.
        let kinds = spec.input_schema["properties"]["kind"]["enum"].as_array().unwrap();
        assert!(kinds.iter().any(|v| v.as_str() == Some("request_hardware")));
    }

    #[tokio::test]
    async fn write_atomic_writes_when_dir_is_writable() {
        let dir = std::env::temp_dir().join(format!("apexrs-wa-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("policy.toml");
        write_atomic(&target, b"hello").await.unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "hello");
        // No stray temp file left behind.
        let leftovers: Vec<_> = std::fs::read_dir(&dir).unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains("tmp."))
            .collect();
        assert!(leftovers.is_empty(), "temp file not cleaned up");
        std::fs::remove_dir_all(&dir).ok();
    }

    // Regression for the soul/policy EACCES bug: /etc/agentd is root-owned, only the
    // individual config files are agentd-writable. The temp+rename path fails at the
    // dir level, so write_atomic must fall back to an in-place write of the file.
    #[tokio::test]
    async fn write_atomic_falls_back_in_place_when_dir_readonly() {
        use std::os::unix::fs::PermissionsExt;
        // Under non-root the read-only dir blocks the temp+rename and forces the
        // in-place fallback (the real bug's path). Under root the dir perms are
        // ignored and the atomic path runs — either way the final content must win.
        let dir = std::env::temp_dir().join(format!("apexrs-wa-ro-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("policy.toml");
        std::fs::write(&target, b"old").unwrap();           // pre-existing writable file
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o555)).unwrap(); // read-only dir
        let res = write_atomic(&target, b"new").await;
        // Restore perms before asserting so cleanup always works.
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        res.expect("fallback in-place write should succeed");
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "new");
        std::fs::remove_dir_all(&dir).ok();
    }

    // Per-agent souls (3b-2): an UpdateSystemPrompt evolution must target the
    // PROPOSING agent's own soul_file, never the global soul.md — otherwise a bound
    // agent's self-edit clobbers APEX (the bug PROBE surfaced on apex2).
    #[tokio::test]
    async fn soul_target_for_isolates_bound_agent_soul() {
        use apexos_core::{AgentRecord, Identities, SessionBindings};
        use std::collections::HashMap;
        use std::sync::Mutex;

        let mut ids = Identities::default();
        ids.agents.push(AgentRecord {
            id:        "PROBE".into(),
            soul_file: "/etc/agentd/souls/PROBE.md".into(),
            ..Default::default()
        });
        let identities = Arc::new(RwLock::new(ids));

        let bindings: SessionBindings = Arc::new(Mutex::new(HashMap::new()));
        {
            let mut m = bindings.lock().unwrap();
            m.insert(SessionId(5), "PROBE".to_string());            // bound, non-default
            m.insert(SessionId(7), apexos_core::node_agent_id());   // bound to the node default
        }

        // Bound non-default agent → ITS OWN soul_file.
        assert_eq!(
            soul_target_for(SessionId(5), &bindings, &identities).await,
            Some(PathBuf::from("/etc/agentd/souls/PROBE.md")),
        );
        // Bound to the node default (APEX) → global soul (None).
        assert_eq!(soul_target_for(SessionId(7), &bindings, &identities).await, None);
        // Unbound session → global soul (None) — single-agent behaviour unchanged.
        assert_eq!(soul_target_for(SessionId(9), &bindings, &identities).await, None);
    }

}
