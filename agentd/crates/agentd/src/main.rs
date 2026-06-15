mod session_store;
use session_store::SessionStore;
mod scheduler;
use scheduler::{load_schedules, run_scheduler, spawn_scheduler_handler, SchedulerState};
mod council_handler;
use council_handler::spawn_council_handler;

use apexos_core::{
    ActionId, Bus, ContentBlock, Event, EvolutionId, EvolutionProposal, Message,
    PluginId, PolicyMode, SessionId, SensorReading, Subsystem, SystemState, ToolOutput, ToolSpec,
};
use apexos_gateway::{serve, GatewayState, PeerRegistry};
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
    let backend_str = std::env::var("AGENTD_BACKEND").unwrap_or_else(|_| "anthropic".into());
    let oai_base_url_str = std::env::var("AGENTD_OAI_BASE_URL")
        .unwrap_or_else(|_| "http://localhost:11434/v1".into());
    let default_model = std::env::var("AGENTD_MODEL").unwrap_or_else(|_| match backend_str.as_str() {
        "ollama" | "vllm" => "qwen3:27b".into(),
        "openrouter"      => "qwen/qwen3-70b-a3b".into(),
        _                 => "claude-sonnet-4-6".into(),
    });
    eprintln!("[agentd] backend: {backend_str}, model: {default_model}");
    let model_arc        = Arc::new(RwLock::new(default_model));
    let backend_arc      = Arc::new(RwLock::new(backend_str));
    let oai_base_url_arc = Arc::new(RwLock::new(oai_base_url_str));

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

    // Server-issued session IDs — start above any IDs already loaded from disk.
    let max_loaded_sid = initial_histories.keys().map(|s| s.0).max().unwrap_or(0);
    let next_session_id = Arc::new(AtomicU64::new(max_loaded_sid + 1));

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

    // Peer registry — /etc/agentd/peers.toml (created empty if missing)
    let peers_path = PathBuf::from(
        std::env::var("PEERS_TOML").unwrap_or_else(|_| "/etc/agentd/peers.toml".into())
    );
    if !peers_path.exists() {
        let _ = std::fs::write(&peers_path, "# ApexOS mesh peers\n");
    }
    let peer_registry = Arc::new(RwLock::new(PeerRegistry::load(&peers_path)));
    let vast_state = VastState::new();
    {
        let vs = vast_state.clone();
        tokio::spawn(async move { vs.try_restore().await; });
    }
    let node_id = Arc::new(
        std::env::var("APEX_NODE_ID").unwrap_or_else(|_| {
            std::process::Command::new("hostname")
                .output()
                .ok()
                .and_then(|o| String::from_utf8(o.stdout).ok())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "apexos".into())
        })
    );

    // Per-session agent bindings (multi-agent runtime, docs/agent-identity.md 3b):
    // a `hello` frame may bind its session to an agent; the Cerebro stamp + CCBS
    // boot resolve identity here, falling back to the node default when unbound.
    // Shared across gateway (writes), supervisor (stamp), and router (boot).
    let session_bindings: apexos_core::SessionBindings = Arc::new(std::sync::Mutex::new(HashMap::new()));

    eprintln!("[agentd] serving UI from {}", ui_dir.display());
    let gw_state = GatewayState {
        bus:                  handle.clone(),
        bcast:                bcast.clone(),
        api_key:              Arc::clone(&api_key_arc),
        oai_api_key:          Arc::clone(&oai_api_key_arc),
        model:                Arc::clone(&model_arc),
        backend:              Arc::clone(&backend_arc),
        oai_base_url:         Arc::clone(&oai_base_url_arc),
        policy_mode:          Arc::clone(&policy_mode_arc),
        policy_set_tx,
        ui_dir,
        events_dir:           log_dir.clone(),
        sessions_dir:         log_dir.join("sessions"),
        histories:            Arc::clone(&histories),
        next_session_id:      Arc::clone(&next_session_id),
        sensor_bridge_token:  sensor_bridge_token,
        api_token,
        soul_path:            soul_path.clone(),
        policy_arc:           Arc::clone(&policy_arc),
        council_start_tx,
        council_butt_in:      Arc::clone(&council_butt_in),
        council_sessions:     Arc::clone(&council_sessions),
        council_next_id:      Arc::clone(&council_next_id),
        peer_registry:        Arc::clone(&peer_registry),
        pairing:              Arc::new(std::sync::Mutex::new(None)),
        node_id:              Arc::clone(&node_id),
        vast_state:           vast_state.clone(),
        session_bindings:     Arc::clone(&session_bindings),
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
    supervisor.set_events_dir(log_dir.clone());
    supervisor.set_vast_state(vast_state.clone());
    // Subscribe the agent-router's receiver BEFORE the supervisor starts. The
    // supervisor emits PluginUp (carrying each plugin's tools) the moment a plugin
    // finishes enumerating; a broadcast Receiver created afterwards misses those
    // events (tokio drops messages sent before subscribe), leaving tool_reg holding
    // only the virtual tools — the model then sees no plugin tools at all. On a fast
    // host the supervisor reliably wins that race, so this MUST be subscribed here,
    // not down by spawn_agent_router. The receiver buffers (cap 1024) until the
    // router task drains it.
    let agent_rx = bcast.subscribe();
    tokio::spawn(supervisor.run(plugin_configs, bcast.subscribe()));

    // Agent turn engine — RoutingProvider dispatches per-call based on backend_arc
    let engine: Arc<TurnEngine> = Arc::new(TurnEngine::new(
        RoutingProvider::new(
            Arc::clone(&backend_arc),
            Arc::clone(&oai_base_url_arc),
            Arc::clone(&api_key_arc),
            Arc::clone(&oai_api_key_arc),
            Arc::clone(&model_arc),
        ),
        16,
        Some(soul_content),
    ));
    let soul_arc = engine.system_arc();
    let embodiment_arc = engine.embodiment_arc();

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

    // Evolution applier — subscribes to EvolutionProposed and applies changes live.
    spawn_evolution_applier(
        bcast.subscribe(),
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

    // Live embodiment refresher — regenerates the "## Current embodiment" block the
    // engine appends after soul.md (node/senses/mesh/uptime + the LIVE tool list, so
    // it can never go stale). Cloned arcs since tool_reg/engine move into the router.
    spawn_embodiment_refresher(
        embodiment_arc,
        Arc::clone(&tool_reg),
        Arc::clone(&backend_arc),
        Arc::clone(&model_arc),
        Arc::clone(&peer_registry),
        Arc::clone(&node_id),
        cerebro_embed,
    );

    // Nightly autonomous memory consolidation: call dream_run directly via the
    // ToolProxy on a cron (default 03:00 daily) — no LLM turn, can't be skipped by
    // the agent. Disable by setting AGENTD_DREAM_CRON empty. See docs/agent-identity.md.
    spawn_nightly_dream(dream_proxy);

    // agent_rx was subscribed above, before the supervisor spawned, so the early
    // PluginUp events that populate tool_reg are captured (see the comment there).
    spawn_agent_router(agent_rx, bcast.clone(), handle.clone(),
                       tool_reg, histories, engine, max_depth, session_store, router_proxy,
                       Arc::clone(&session_bindings));

    // Vast.ai backend hot-swap — listens for VastInstanceReady / VastInstanceDestroyed
    {
        let mut vast_rx    = bcast.subscribe();
        let backend_w      = Arc::clone(&backend_arc);
        let oai_url_w      = Arc::clone(&oai_base_url_arc);
        let model_w        = Arc::clone(&model_arc);
        let default_backend = std::env::var("AGENTD_BACKEND").unwrap_or_else(|_| "anthropic".into());
        let default_url     = std::env::var("AGENTD_OAI_BASE_URL")
            .unwrap_or_else(|_| "http://localhost:11434/v1".into());
        let default_model   = std::env::var("AGENTD_MODEL").unwrap_or_default();
        tokio::spawn(async move {
            loop {
                match vast_rx.recv().await {
                    Ok(Event::VastInstanceReady { instance_id, local_port }) => {
                        eprintln!("[vast] hot-swapping backend → http://127.0.0.1:{local_port}/v1");
                        *backend_w.write().await = "ollama".into();
                        *oai_url_w.write().await = format!("http://127.0.0.1:{local_port}/v1");
                        eprintln!("[vast] backend ready on instance {instance_id}");
                    }
                    Ok(Event::VastInstanceDestroyed { instance_id }) => {
                        eprintln!("[vast] reverting backend after destroy (instance {instance_id})");
                        *backend_w.write().await = default_backend.clone();
                        *oai_url_w.write().await = default_url.clone();
                        if !default_model.is_empty() {
                            *model_w.write().await = default_model.clone();
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(_) => break,
                    Ok(_)  => {}
                }
            }
        });
    }

    // Mesh discovery loop — mDNS poll, subnet guard, PeerSeen events
    spawn_discovery_loop(Arc::clone(&peer_registry), Arc::clone(&node_id), handle.clone());

    // Event log
    tokio::spawn(run_log_writer(log_dir, bcast.subscribe()));

    eprintln!("[agentd] ready — gateway ws://{gw_bind}/ws");
    tokio::signal::ctrl_c().await?;
    eprintln!("[agentd] shutting down");
    Ok(())
}

// ── evolution applier ─────────────────────────────────────────────────────────

fn spawn_evolution_applier(
    mut bus_rx:      broadcast::Receiver<Event>,
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
) {
    tokio::spawn(async move {
        loop {
            tokio::select! {
                result = bus_rx.recv() => match result {
                    Ok(Event::EvolutionProposed { id, proposal, proposed_by: _ }) => {
                        let kind = evo_kind(&proposal);

                        // Open a Cerebro episode for this apply (best-effort).
                        let episode_id = episode_start(&tool_proxy, id, &kind).await;

                        // Snapshot current state for rollback BEFORE applying.
                        let undo = compute_undo(
                            &proposal, &soul_arc, &soul_path, &policy_path, &plugins_path,
                        ).await;

                        let proposal_copy = proposal.clone();
                        let result = apply_evolution(
                            id, proposal,
                            &soul_arc, &soul_path, &policy_path, &plugins_path,
                            &policy_arc, &sv_cmd_tx,
                        ).await;
                        match result {
                            Ok(summary) => {
                                eprintln!("[evolution] applied {:?}: {summary}", id);
                                if let Some(undo_proposal) = undo {
                                    // Record undo snapshot in the episode before storing in memory.
                                    if let Some(ref eid) = episode_id {
                                        episode_add_step(&tool_proxy, eid, &undo_proposal, &summary).await;
                                    }
                                    rollback_store.lock().await.insert(id, undo_proposal);
                                }
                                episode_end(&tool_proxy, &episode_id, "success", &summary).await;
                                bus.emit(Event::EvolutionApplied {
                                    id,
                                    proposal:      proposal_copy,
                                    patch_summary: summary,
                                    applied_by:    None,
                                }).await;
                            }
                            Err(e) => {
                                eprintln!("[evolution] apply failed {:?}: {e}", id);
                                episode_end(&tool_proxy, &episode_id, "failed", &e.to_string()).await;
                                bus.emit(Event::Error {
                                    session: None,
                                    message: format!("evolution {}: {e}", id.0),
                                }).await;
                            }
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(_) => break,
                    Ok(_)  => {}
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
                            let result = apply_evolution(
                                evo_id, undo_proposal,
                                &soul_arc, &soul_path, &policy_path, &plugins_path,
                                &policy_arc, &sv_cmd_tx,
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
            }
        }
    });
}

// ── evolution episode helpers (Cerebro, best-effort) ─────────────────────────

fn evo_kind(proposal: &EvolutionProposal) -> String {
    serde_json::to_value(proposal).ok()
        .and_then(|v| v.get("kind").and_then(|k| k.as_str()).map(str::to_owned))
        .unwrap_or_else(|| "unknown".into())
}

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
fn parse_cerebro_id(output: &apexos_core::ToolOutput, json_key: &str) -> Option<String> {
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
async fn episode_add_step(proxy: &ToolProxy, episode_id: &str, undo: &EvolutionProposal, summary: &str) {
    let undo_json = serde_json::to_string(undo).unwrap_or_default();
    let content   = format!("evolution apply: {summary}\nundo_snapshot: {undo_json}");

    // Step 1: store the undo snapshot as a memory to get a memory_id.
    let memory_id = match proxy.call("memory_store", serde_json::json!({
        "content": content,
        "tags":    ["evolution", "undo_snapshot"]
    })).await {
        Ok(out) if out.ok => parse_cerebro_id(&out, "memory_id"),
        Ok(out) => { eprintln!("[evolution] memory_store not ok: {:?}", out.content); None }
        Err(e)  => { eprintln!("[evolution] memory_store: {e}"); None }
    };

    let Some(mid) = memory_id else { return };

    // Step 2: link the memory to the episode.
    if let Err(e) = proxy.call("episode_add_step", serde_json::json!({
        "episode_id": episode_id,
        "memory_id":  mid,
        "role":       "event"
    })).await {
        eprintln!("[evolution] episode_add_step: {e}");
    }
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

    let mut count = 0usize;
    for line in text.lines() {
        let line = line.trim();
        if !line.starts_with("- ep_") { continue; }
        let line = &line[2..]; // strip "- "

        let (episode_id, rest) = match line.split_once(": ") {
            Some(pair) => pair,
            None       => continue,
        };
        let title = match rest.split_once(" | steps:") {
            Some((t, _)) => t,
            None         => rest,
        };
        if !title.starts_with("evolution ") { continue; }

        let evo_id = match parse_evolution_id_from_title(title) {
            Some(id) => id,
            None     => { eprintln!("[evolution] restore: can't parse id from '{title}'"); continue; }
        };

        let mems_text = match proxy.call("get_episode_memories", serde_json::json!({
            "episode_id": episode_id,
            "agent_id":   apexos_core::node_agent_id()
        })).await {
            Ok(out) if out.ok => match mcp_text(&out) { Some(t) => t, None => continue },
            _ => continue,
        };

        if let Some(proposal) = parse_undo_snapshot_from_text(&mems_text) {
            rollback_store.lock().await.insert(evo_id, proposal);
            count += 1;
        }
    }

    eprintln!("[evolution] restore: loaded {count} rollback snapshot(s) from Cerebro");
}

fn parse_evolution_id_from_title(title: &str) -> Option<EvolutionId> {
    // "evolution {N}: {kind}"
    let rest  = title.strip_prefix("evolution ")?;
    let colon = rest.find(':')?;
    let n: u64 = rest[..colon].trim().parse().ok()?;
    Some(EvolutionId(n))
}

fn parse_undo_snapshot_from_text(text: &str) -> Option<EvolutionProposal> {
    // Memory content: "evolution apply: {summary}\nundo_snapshot: {compact_json}"
    // compact_json has no literal newlines (serde_json::to_string escapes them).
    let marker = "undo_snapshot: ";
    let start  = text.find(marker)? + marker.len();
    let rest   = &text[start..];
    let end    = rest.find('\n').unwrap_or(rest.len());
    serde_json::from_str(&rest[..end]).ok()
}

/// Snapshot current state to produce an inverse proposal (for rollback).
/// Returns None for proposals that have no meaningful undo (e.g. HotReload).
async fn compute_undo(
    proposal:     &EvolutionProposal,
    soul_arc:     &Arc<RwLock<String>>,
    _soul_path:   &PathBuf,
    policy_path:  &PathBuf,
    plugins_path: &PathBuf,
) -> Option<EvolutionProposal> {
    match proposal {
        EvolutionProposal::UpdateSystemPrompt { .. } => {
            let old = soul_arc.read().await.clone();
            Some(EvolutionProposal::UpdateSystemPrompt {
                content: old,
                reason:  "rollback".into(),
            })
        }
        EvolutionProposal::UpdatePolicyRule { tool_pattern, .. } => {
            // Snapshot the prior rule value so rollback restores it exactly.
            // If the rule didn't exist before (brand-new addition), there is no
            // meaningful inverse (we have no "remove rule" variant) — return None.
            let toml_text    = tokio::fs::read_to_string(policy_path).await.ok()?;
            let doc          = toml_text.parse::<toml_edit::DocumentMut>().ok()?;
            let old_rule_str = doc.get("rules")?.as_table()?.get(tool_pattern.as_str())?.as_str()?;
            let old_rule     = apexos_core::PolicyRule::from_toml_str(old_rule_str)?;
            Some(EvolutionProposal::UpdatePolicyRule {
                tool_pattern: tool_pattern.clone(),
                new_rule:     old_rule,
                reason:       "rollback".into(),
            })
        }
        EvolutionProposal::RegisterMcpServer { name, .. } => {
            Some(EvolutionProposal::UnregisterMcpServer {
                name:   name.clone(),
                reason: "rollback".into(),
            })
        }
        EvolutionProposal::UnregisterMcpServer { name, .. } => {
            let toml_text = tokio::fs::read_to_string(plugins_path).await.ok()?;
            let doc = toml_text.parse::<toml_edit::DocumentMut>().ok()?;
            let arr = doc.get("plugin")?.as_array_of_tables()?;
            let tbl = arr.iter().find(|t| {
                t.get("id").and_then(|v| v.as_str()) == Some(name.as_str())
            })?;
            let cmd = tbl.get("cmd")?.as_str()?.to_string();
            Some(EvolutionProposal::RegisterMcpServer {
                name:    name.clone(),
                command: cmd,
                env:     std::collections::HashMap::new(),
                reason:  "rollback".into(),
            })
        }
        EvolutionProposal::HotReloadSubsystem { .. } => None,

        // A hardware request is a record, not a mutation — nothing to roll back.
        EvolutionProposal::RequestHardware { .. } => None,
    }
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

async fn apply_evolution(
    id:           EvolutionId,
    proposal:     EvolutionProposal,
    soul_arc:     &Arc<RwLock<String>>,
    soul_path:    &PathBuf,
    policy_path:  &PathBuf,
    plugins_path: &PathBuf,
    policy_arc:   &Arc<RwLock<PolicyEngine>>,
    sv_cmd_tx:    &mpsc::Sender<SupervisorCmd>,
) -> anyhow::Result<String> {
    match proposal {
        EvolutionProposal::UpdateSystemPrompt { content, reason: _ } => {
            tokio::fs::write(soul_path, &content).await?;
            *soul_arc.write().await = content.clone();
            eprintln!("[evolution] soul.md updated ({} chars)", content.len());
            Ok(format!("system prompt updated ({} chars)", content.len()))
        }

        EvolutionProposal::UpdatePolicyRule { tool_pattern, new_rule, reason: _ } => {
            let toml_text = tokio::fs::read_to_string(policy_path).await?;
            let mut doc = toml_text.parse::<toml_edit::DocumentMut>()?;
            // The [rules] table accepts allow/ask/workspace (PolicyRule), NOT the
            // global mode names. Writing a mode name here makes policy.toml fail to
            // deserialize on the next load and silently wipes every rule.
            let rule_str = new_rule.as_toml_str();
            // Ensure [rules] exists so brand-new rule additions don't silently no-op.
            if doc.get("rules").is_none() {
                doc["rules"] = toml_edit::Item::Table(toml_edit::Table::new());
            }
            if let Some(rules) = doc.get_mut("rules").and_then(|v| v.as_table_mut()) {
                rules.insert(&tool_pattern, toml_edit::value(rule_str));
            }
            // Validate-before-persist: parse the candidate doc into a PolicyConfig
            // BEFORE touching the live file, so a bad proposal can never corrupt it.
            let new_toml = doc.to_string();
            let new_config = PolicyConfig::parse(&new_toml)
                .map_err(|e| anyhow::anyhow!("rejected policy edit (would corrupt policy.toml): {e}"))?;
            write_atomic(policy_path, new_toml.as_bytes()).await?;
            *policy_arc.write().await = PolicyEngine::new(new_config);
            eprintln!("[evolution] policy rule '{tool_pattern}' = '{rule_str}'");
            Ok(format!("policy rule '{tool_pattern}' set to '{rule_str}'"))
        }

        EvolutionProposal::RegisterMcpServer { name, command, env, reason: _ } => {
            let toml_text = tokio::fs::read_to_string(plugins_path).await?;
            let mut doc = toml_text.parse::<toml_edit::DocumentMut>()?;
            if let Some(arr) = doc.get_mut("plugin").and_then(|v| v.as_array_of_tables_mut()) {
                let mut tbl = toml_edit::Table::new();
                tbl.insert("id",      toml_edit::value(name.as_str()));
                tbl.insert("cmd",     toml_edit::value(command.as_str()));
                tbl.insert("restart", toml_edit::value("always"));
                if !env.is_empty() {
                    let mut env_inline = toml_edit::InlineTable::new();
                    for (k, v) in &env {
                        env_inline.insert(k, toml_edit::Value::from(v.as_str()));
                    }
                    tbl.insert("env",
                        toml_edit::Item::Value(toml_edit::Value::InlineTable(env_inline)));
                }
                arr.push(tbl);
            }
            tokio::fs::write(plugins_path, doc.to_string()).await?;
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
            let mut doc = toml_text.parse::<toml_edit::DocumentMut>()?;
            if let Some(arr) = doc.get_mut("plugin").and_then(|v| v.as_array_of_tables_mut()) {
                let idx = (0..arr.len()).find(|&i| {
                    arr.get(i)
                        .and_then(|t| t.get("id"))
                        .and_then(|v| v.as_str()) == Some(name.as_str())
                });
                if let Some(i) = idx { arr.remove(i); }
            }
            tokio::fs::write(plugins_path, doc.to_string()).await?;
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
            let header = "# ApexOS hardware wishlist\n\n\
                APEX's request-to-incarnate queue (EDK, docs/edk.md). Each entry is a part\n\
                APEX asked for; a human seats it, reboots, and the embodiment probe confirms\n\
                the new sense live. Remove an entry once it's installed.\n";
            let mut doc = tokio::fs::read_to_string(&path_buf).await
                .unwrap_or_else(|_| header.to_string());
            let bus_line    = if bus.is_empty()    { String::new() } else { format!("- attaches: {bus}\n") };
            let source_line = if source.is_empty() { String::new() } else { format!("- source: {source}\n") };
            doc.push_str(&format!(
                "\n## [#{}] {part} → {capability}\n{bus_line}{source_line}- why: {reason}\n\
                 - status: REQUESTED — seat it, reboot; the embodiment probe confirms it live\n",
                id.0,
            ));
            write_atomic(&path_buf, doc.as_bytes()).await?;
            eprintln!("[evolution] hardware request filed: {part} -> {capability}");
            Ok(format!("hardware request filed: {part} → {capability}. A human must seat it; \
                        the next-boot embodiment probe will confirm it. (logged to {path})"))
        }
    }
}

// ── agent router ──────────────────────────────────────────────────────────────

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
    let next_child_id    = Arc::new(AtomicU64::new(1u64 << 63));
    // CCBS boot-priming cache: one cognitive_bootstrap per session (first turn),
    // reused on later turns so orientation stays in the system prompt all session.
    let boot_primings    = Arc::new(Mutex::new(HashMap::<SessionId, String>::new()));

    tokio::spawn(async move {
        // Per-alert-key cooldown to prevent turn storms when a condition persists.
        let mut last_alert: HashMap<String, std::time::Instant> = HashMap::new();
        let iaq_threshold: f32 = std::env::var("SENSOR_IAQ_THRESHOLD")
            .ok().and_then(|s| s.parse().ok()).unwrap_or(150.0);
        let cpu_temp_threshold: f32 = std::env::var("SENSOR_CPU_TEMP_THRESHOLD")
            .ok().and_then(|s| s.parse().ok()).unwrap_or(85.0);
        let thermal_threshold: f32 = std::env::var("SENSOR_THERMAL_THRESHOLD")
            .ok().and_then(|s| s.parse().ok()).unwrap_or(45.0);
        let alert_cooldown_secs: u64 = std::env::var("SENSOR_ALERT_COOLDOWN_SECS")
            .ok().and_then(|s| s.parse().ok()).unwrap_or(1800);
        loop {
            match rx.recv().await {
                // ── new root turn ────────────────────────────────────────────
                Ok(Event::UserPrompt { session, text, images }) => {
                    session_depths.lock().await.entry(session).or_insert(0);

                    // Text first (skipped when empty — image-only prompts are
                    // valid), then any attached images. The gateway has already
                    // shimmed each image through vision::prepare (downscaled b64).
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
                    let snapshot     = history.clone();
                    let snapshot_len = snapshot.len();
                    drop(hist);

                    // Persist user message immediately.
                    {
                        let store = Arc::clone(&session_store);
                        tokio::spawn(async move { store.append(session, &user_msg).await; });
                    }

                    let tools  = gather_tools(&tool_reg).await;
                    let gen    = next_turn_gen.fetch_add(1, Ordering::SeqCst);
                    let handle = tokio::spawn(root_turn(
                        session, snapshot,
                        bus.clone(), bcast.clone(), tools, engine.clone(),
                        histories.clone(), Arc::clone(&session_store), snapshot_len,
                        tracker.clone(), gen,
                        tool_proxy.clone(), boot_primings.clone(),
                        Arc::clone(&session_bindings),
                    ));
                    abort_handles.lock().await.insert(session, (gen, handle.abort_handle()));
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
                    // (alert_key, prompt) — key is used for per-type cooldown dedup.
                    let alert: Option<(String, String)> = match &reading {
                        SensorReading::Temperature { celsius, sensor_id }
                            if *celsius > cpu_temp_threshold => {
                            Some((
                                format!("{node_id}:cpu_temp"),
                                format!("[sensor alert] {node_id}/{sensor_id} CPU temperature critical: {celsius:.1}°C (threshold {cpu_temp_threshold:.0}°C) — please investigate"),
                            ))
                        }
                        SensorReading::Motion { detected: true, sensor_id } => {
                            Some((
                                format!("{node_id}:motion"),
                                format!("[sensor alert] {node_id}/{sensor_id} motion detected"),
                            ))
                        }
                        SensorReading::AirQuality { iaq, accuracy, sensor_id, .. }
                            if *iaq > iaq_threshold && *accuracy >= 2 => {
                            Some((
                                format!("{node_id}:air_quality"),
                                format!("[sensor alert] {node_id}/{sensor_id} air quality degraded: IAQ {iaq:.0} (threshold {iaq_threshold:.0}, accuracy {accuracy}/3) — consider ventilating"),
                            ))
                        }
                        SensorReading::ThermalFrame { max_c, mean_c, sensor_id, .. }
                            if *max_c > thermal_threshold => {
                            Some((
                                format!("{node_id}:thermal_hotspot"),
                                format!("[sensor alert] {node_id}/{sensor_id} thermal hotspot: {max_c:.1}°C max, {mean_c:.1}°C mean (threshold {thermal_threshold:.0}°C) — check for overheating devices"),
                            ))
                        }
                        _ => None,
                    };
                    if let Some((alert_key, prompt)) = alert {
                        let now = std::time::Instant::now();
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
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(_) => break,
            }
        }
    });
}

// ── turn task helpers ─────────────────────────────────────────────────────────

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
    let block = fetch_boot_priming(proxy, agent_id, last_user_text(history).unwrap_or_default()).await;
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

/// Nightly autonomous memory consolidation: call `dream_run` directly via the
/// ToolProxy on a cron (default 03:00 UTC daily), scoped to this node's agent
/// identity — no LLM turn, can't be skipped by the agent. Disabled when
/// `AGENTD_DREAM_CRON` is empty. See docs/agent-identity.md (slice 2).
fn spawn_nightly_dream(proxy: ToolProxy) {
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
            let args = serde_json::json!({ "agent_id": apexos_core::node_agent_id() });
            match proxy.call("dream_run", args).await {
                Ok(out) if out.ok => eprintln!("[dream] nightly dream_run complete"),
                Ok(out)           => eprintln!("[dream] dream_run not ok: {:?}", out.content),
                Err(e)            => eprintln!("[dream] dream_run error: {e}"),
            }
        }
    });
}

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
) {
    // CCBS (docs/agent-identity.md slice 2): on a session's first turn, prime the
    // system prompt with the agent's live memory state (where it left off, open
    // intentions, relevant skills) so it wakes oriented — daemon-driven, so it
    // doesn't depend on the agent remembering to orient. Cached per session.
    // Scoped (3b) to the session's bound agent, else the node default.
    let agent_id = apexos_core::resolve_agent_id(&session_bindings, session);
    let engine = match boot_priming_for(&tool_proxy, &boot_primings, session, &agent_id, &history).await {
        Some(block) => Arc::new(engine.with_priming(block)),
        None        => engine,
    };

    match run_turn(session, history, bus.clone(), bcast, tools, engine).await {
        Ok(updated) => {
            // Persist the assistant messages added during this turn.
            if updated.len() > snapshot_len {
                let delta: Vec<Message> = updated[snapshot_len..].to_vec();
                let store = session_store.clone();
                tokio::spawn(async move {
                    for msg in &delta { store.append(session, msg).await; }
                });
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
    tools.push(rollback_evolution_spec());
    tools.push(schedule_task_spec());
    tools.push(list_schedules_spec());
    tools.push(cancel_schedule_spec());
    tools.push(convene_council_spec());
    tools.push(send_to_agent_spec());
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
fn spawn_embodiment_refresher(
    embodiment:    Arc<RwLock<String>>,
    tool_reg:      Arc<RwLock<HashMap<PluginId, Vec<ToolSpec>>>>,
    backend_arc:   Arc<RwLock<String>>,
    model_arc:     Arc<RwLock<String>>,
    peer_registry: Arc<RwLock<PeerRegistry>>,
    node_id:       Arc<String>,
    cerebro_embed: Option<String>,
) {
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        loop {
            let block = build_embodiment(&tool_reg, &backend_arc, &model_arc,
                                         &peer_registry, &node_id, &cerebro_embed).await;
            *embodiment.write().await = block;
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
        }
    });
}

/// Build the "## Current embodiment" block from this node's ACTUAL state: the live
/// tool registry (never stale — its absence caused a multi-hour debugging hunt),
/// cheap hardware probes, mesh peers, and uptime.
async fn build_embodiment(
    tool_reg:      &Arc<RwLock<HashMap<PluginId, Vec<ToolSpec>>>>,
    backend_arc:   &Arc<RwLock<String>>,
    model_arc:     &Arc<RwLock<String>>,
    peer_registry: &Arc<RwLock<PeerRegistry>>,
    node_id:       &str,
    cerebro_embed: &Option<String>,
) -> String {
    let full = gather_tools(tool_reg).await;            // plugin tools + virtual tools
    let reg  = tool_reg.read().await;
    let plugin_names: std::collections::HashSet<&str> =
        reg.values().flatten().map(|t| t.name.as_str()).collect();

    let backend = backend_arc.read().await.clone();
    let mut model = model_arc.read().await.clone();
    if model.trim().is_empty() { model = "(provider default)".into(); }

    let has_sensors = plugin_names.contains("get_iaq")
        || plugin_names.contains("thermal_frame")
        || plugin_names.iter().any(|n| n.starts_with("get_temp"));
    let has_cam = has_camera();
    let ram = read_ram_mb();

    let mut out = String::new();
    out.push_str("## Current embodiment — auto-generated from this node's live state. Trust this\n");
    out.push_str("## over any hardware or tool list in your identity above; it reflects THIS body.\n\n");
    out.push_str(&format!(
        "- Node: {node_id} · {} · {ram} MB · tier {} · uptime {} · backend {backend}/{model}\n",
        std::env::consts::ARCH, tier_from_ram(ram), fmt_uptime(read_uptime_secs()),
    ));
    out.push_str(&format!(
        "- Senses: camera {} · thermal/IAQ {} · GPIO {}\n",
        yn(has_cam), yn(has_sensors), yn(is_raspberry_pi()),
    ));
    out.push_str(&format!("- Memory: cerebro {}\n", match cerebro_embed {
        Some(m) => format!("semantic embeddings ({m})"),
        None    => "FTS5 keyword search only".to_string(),
    }));

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

/// A camera is reachable if there's a V4L2 node (USB/laptop) or a Pi CSI capture
/// utility on PATH — mirrors camera_capture's own backend detection.
fn has_camera() -> bool {
    let v4l2 = std::fs::read_dir("/dev").map(|rd| rd.flatten()
        .any(|e| e.file_name().to_string_lossy().starts_with("video"))).unwrap_or(false);
    v4l2 || which_on_path("rpicam-jpeg") || which_on_path("libcamera-jpeg")
}

fn is_raspberry_pi() -> bool {
    std::fs::read_to_string("/proc/device-tree/model")
        .map(|s| s.to_lowercase().contains("raspberry")).unwrap_or(false)
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
        description: "Spawn a focused sub-agent to handle a sub-task. \
                      Returns the sub-agent's final text output.".into(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "prompt": {
                    "type":        "string",
                    "description": "The task for the sub-agent to perform."
                },
                "system": {
                    "type":        "string",
                    "description": "Optional system prompt override for the sub-agent."
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

fn propose_evolution_spec() -> ToolSpec {
    ToolSpec {
        name:        "propose_evolution".into(),
        description: "Propose a structural change to agentd: register or remove an MCP plugin, \
                      update a policy rule, update your own system prompt (soul.md), \
                      hot-reload a subsystem, or file a hardware request (request_hardware — \
                      the EDK request-to-incarnate, docs/edk.md: it CANNOT auto-apply, a human \
                      seats the part and the next-boot probe confirms it). Every proposal is \
                      recorded as an event (gated by the evolution.* policy rule).".into(),
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
                                    Call read_soul_md first to get the current content before editing."
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
        description: "Send an asynchronous message to another agent session (fire-and-forget). \
                      Without node: routes locally on this machine. \
                      With node: proxies to a registered mesh peer — session_id 0 = root session on that node. \
                      Returns immediately — use agent_spawn if you need the result.".into(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "session_id": {
                    "type":        "integer",
                    "description": "Target session ID (use 0 for the remote node's root session)."
                },
                "message": {
                    "type":        "string",
                    "description": "Message to deliver to the target agent."
                },
                "node": {
                    "type":        "string",
                    "description": "Optional mesh node_id (hostname) to route to a peer node. \
                                   Omit for local routing."
                }
            },
            "required": ["session_id", "message"]
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
                    "description": "Git repo URL (default: https://github.com/buckster123/ApexOS.git)."
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

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use apexos_core::{ContentBlock, Message};

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
}
