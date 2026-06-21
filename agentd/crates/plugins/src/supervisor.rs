use std::{collections::HashMap, sync::Arc, time::Duration};
use std::path::PathBuf;
use tokio::sync::{broadcast, mpsc, oneshot, RwLock};
use tokio::process::Command;
use std::process::Stdio;
use apexos_core::{ActionId, BusHandle, Event, EvolutionId, EvolutionProposal, PluginId, SessionId, ToolCall, ToolOutput};
use crate::config::{PluginConfig, RestartPolicy};
use crate::mcp::McpClient;
use crate::policy::{Decision, PolicyEngine};
use crate::vast::{VastState, VastPhase, VastInstance, vastai, load_recipes};

/// Process-global monotonic source of `EvolutionId`s. Each proposed evolution
/// gets a unique id for the lifetime of the process, so the rollback-snapshot
/// map (keyed by `EvolutionId`) never collides across turns.
static NEXT_EVOLUTION_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// Seed the counter so the next allocated `EvolutionId` is at least `min_next`.
/// The counter otherwise resets to 1 each boot, while cold-start restore rebuilds
/// `rollback_store` keyed by the OLD ids parsed from Cerebro episodes — so a fresh
/// post-restart evolution would reuse `EvolutionId(1)` and alias a restored undo
/// snapshot (rollback restores the wrong one). agentd calls this after
/// `restore_rollback_store` with `max_restored_id + 1`. Uses `fetch_max`, so it is
/// idempotent and a stale/low value can never rewind the counter.
pub fn seed_evolution_id(min_next: u64) {
    NEXT_EVOLUTION_ID.fetch_max(min_next, std::sync::atomic::Ordering::Relaxed);
}

struct Plugin {
    client: Arc<McpClient>,
}

struct PendingApproval {
    session: SessionId,
    call:    ToolCall,
}

pub enum SupervisorCmd {
    /// Internal: process watcher detected the child exited. `success` = clean
    /// exit (status 0) — needed so a `RestartPolicy::OnFailure` plugin restarts
    /// on a crash but not on a clean shutdown (the exit status was discarded
    /// before, so `OnFailure` silently behaved like `Never`).
    PluginDied  { id: PluginId, success: bool },
    /// Start a brand-new plugin (appended to plugins.toml by evolution applier).
    SpawnPlugin { config: PluginConfig },
    /// Kill a plugin and remove it from the registry; does NOT restart.
    KillPlugin  { id: PluginId },
    /// Kill a plugin and restart it (in-place upgrade / config change).
    HotReload   { id: PluginId },
    /// Direct tool call bypassing policy — reply arrives on the oneshot sender.
    DirectCall  { tool: String, args: serde_json::Value, reply: oneshot::Sender<ToolOutput> },
    /// Wire the live soul.md Arc so read_soul_md returns current content.
    SetSoulArc  { arc: Arc<RwLock<String>> },
    /// Wire the scheduler op channel so schedule_* tools route to the scheduler task.
    SetScheduleTx { tx: mpsc::Sender<(SessionId, ActionId, String, serde_json::Value)> },
    /// Wire the council op channel so convene_council routes to the council handler.
    SetCouncilTx  { tx: mpsc::Sender<(SessionId, ActionId, serde_json::Value)> },
    /// Wire the VastState Arc so vast_* virtual tools can read/write instance state.
    SetVastState  { state: VastState },
    /// Wire the self-update channel so apply_daemon_update routes to its handler.
    SetSelfUpdateTx { tx: mpsc::Sender<(SessionId, ActionId, serde_json::Value)> },
}

/// Thin handle for calling plugin tools directly from non-agent code (e.g. the
/// evolution applier calling Cerebro for episode tracking).
#[derive(Clone)]
pub struct ToolProxy {
    tx: mpsc::Sender<SupervisorCmd>,
}

impl ToolProxy {
    pub fn new(tx: mpsc::Sender<SupervisorCmd>) -> Self { Self { tx } }

    pub async fn call(&self, tool: &str, args: serde_json::Value) -> anyhow::Result<ToolOutput> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx.send(SupervisorCmd::DirectCall {
            tool:  tool.to_string(),
            args,
            reply: reply_tx,
        }).await.map_err(|_| anyhow::anyhow!("supervisor channel closed"))?;
        tokio::time::timeout(Duration::from_secs(10), reply_rx).await
            .map_err(|_| anyhow::anyhow!("direct call timed out: {tool}"))?
            .map_err(|_| anyhow::anyhow!("reply dropped"))
    }
}

/// Stamp the caller's agent identity onto a Cerebro tool call's args, overriding
/// any model-supplied value. In every Cerebro tool `agent_id` is the *caller's*
/// space (storing/filter/scope); cross-agent targets use distinct params
/// (`target_agent_id`/`to_agent_id`), so this never redirects a cross-agent op.
/// Coerces a non-object/absent args into an object so the stamp always lands.
fn stamp_agent_id(args: &mut serde_json::Value, agent_id: &str) {
    if !args.is_object() {
        *args = serde_json::json!({});
    }
    if let Some(obj) = args.as_object_mut() {
        obj.insert("agent_id".to_string(), serde_json::Value::String(agent_id.to_string()));
    }
}

/// Stamp the caller's per-agent workspace root onto an apexos-tools call's args,
/// overriding any model-supplied value. apexos-tools is ONE process shared by
/// every agent, so its FS confinement can't key off a process-global env var —
/// the root travels per call as `__workspace`. Like [`stamp_agent_id`], the
/// insert overwrites whatever the model typed, so a model can never widen or
/// redirect its own confinement boundary. APEX/unbound resolves to the node base
/// (byte-identical to pre-per-agent); a bound agent gets `<base>/workspaces/<id>`.
fn stamp_workspace(args: &mut serde_json::Value, workspace: &str) {
    if !args.is_object() {
        *args = serde_json::json!({});
    }
    if let Some(obj) = args.as_object_mut() {
        obj.insert("__workspace".to_string(), serde_json::Value::String(workspace.to_string()));
    }
}

pub struct Supervisor {
    bus:               BusHandle,
    plugins:           HashMap<PluginId, Plugin>,
    tool_registry:     HashMap<String, PluginId>,
    configs:           HashMap<PluginId, PluginConfig>,
    /// Shared with the evolution applier; writing here updates policy live.
    policy:            Arc<RwLock<PolicyEngine>>,
    pending_approvals: HashMap<ActionId, PendingApproval>,
    sv_tx:             mpsc::Sender<SupervisorCmd>,
    sv_rx:             Option<mpsc::Receiver<SupervisorCmd>>,
    /// Set by main.rs so rollback_evolution can route to the applier task.
    rollback_tx:       Option<mpsc::Sender<(SessionId, ActionId, EvolutionId)>>,
    /// Set by main.rs so schedule_* tools route to the scheduler task.
    schedule_tx:       Option<mpsc::Sender<(SessionId, ActionId, String, serde_json::Value)>>,
    /// Set by main.rs so convene_council routes to the council handler.
    council_tx:        Option<mpsc::Sender<(SessionId, ActionId, serde_json::Value)>>,
    goal_tx:           Option<mpsc::Sender<(SessionId, ActionId, String, serde_json::Value)>>,
    /// Shared with engine so read_soul_md returns the live system prompt.
    soul_arc:          Option<Arc<RwLock<String>>>,
    /// Path to the events log directory so query_event_log can read JSONL files.
    events_dir:        Option<PathBuf>,
    /// Vast.ai instance/tunnel state — shared with gateway for API routes.
    vast_state:        Option<VastState>,
    /// Per-session agent bindings (multi-agent runtime). The Cerebro stamp
    /// resolves the calling session's identity here (bound agent → else the node
    /// default), so routing/isolation can't depend on what the model typed.
    /// See docs/agent-identity.md (slices 1 & 3b).
    session_bindings:  apexos_core::SessionBindings,
    /// Identity registry — lets `read_soul_md` return a bound agent's OWN
    /// soul_file (per-agent souls, docs/agent-identity.md 3b-2) rather than the
    /// global soul.md. `None` until wired → reads fall back to the global soul.
    identities:        Option<Arc<RwLock<apexos_core::Identities>>>,
    /// Dedicated channel to the evolution applier for `propose_evolution`. A
    /// dedicated mpsc (not the broadcast bus) guarantees delivery so the DEFERRED
    /// tool-result ack — emitted by the applier once the apply truly lands — can't
    /// be lag-dropped on a busy turn. `None` until wired.
    propose_tx:        Option<mpsc::Sender<(SessionId, ActionId, EvolutionId, EvolutionProposal)>>,
    /// Dedicated channel to the self-update handler for `apply_daemon_update`
    /// (docs/self-update.md slice 3) — forwards `(session, call_id, args)` to the
    /// handler in main.rs, which runs the build/test gates and files the swap
    /// request. `None` until wired.
    self_update_tx:    Option<mpsc::Sender<(SessionId, ActionId, serde_json::Value)>>,
}

impl Supervisor {
    pub fn new(
        bus: BusHandle,
        policy: Arc<RwLock<PolicyEngine>>,
        session_bindings: apexos_core::SessionBindings,
    ) -> Self {
        let (sv_tx, sv_rx) = mpsc::channel::<SupervisorCmd>(64);
        Self {
            bus,
            plugins:           HashMap::new(),
            tool_registry:     HashMap::new(),
            configs:           HashMap::new(),
            policy,
            pending_approvals: HashMap::new(),
            sv_tx,
            sv_rx: Some(sv_rx),
            rollback_tx:       None,
            soul_arc:          None,
            schedule_tx:       None,
            council_tx:        None,
            goal_tx:           None,
            events_dir:        None,
            vast_state:        None,
            session_bindings,
            identities:        None,
            propose_tx:        None,
            self_update_tx:    None,
        }
    }

    /// Returns a sender that main.rs can use to send hot-reload commands.
    pub fn cmd_tx(&self) -> mpsc::Sender<SupervisorCmd> {
        self.sv_tx.clone()
    }

    /// Wires the rollback channel so `rollback_evolution` can reach the applier.
    pub fn set_rollback_tx(&mut self, tx: mpsc::Sender<(SessionId, ActionId, EvolutionId)>) {
        self.rollback_tx = Some(tx);
    }

    /// Wires the propose channel so `propose_evolution` hands the apply to the
    /// applier over a guaranteed-delivery mpsc (deferred tool-result ack).
    pub fn set_propose_tx(&mut self, tx: mpsc::Sender<(SessionId, ActionId, EvolutionId, EvolutionProposal)>) {
        self.propose_tx = Some(tx);
    }

    /// Wires the self-update channel so `apply_daemon_update` reaches its handler.
    pub fn set_self_update_tx(&mut self, tx: mpsc::Sender<(SessionId, ActionId, serde_json::Value)>) {
        self.self_update_tx = Some(tx);
    }

    /// Wires the scheduler channel so schedule_* tools route to the scheduler task.
    pub fn set_schedule_tx(&mut self, tx: mpsc::Sender<(SessionId, ActionId, String, serde_json::Value)>) {
        self.schedule_tx = Some(tx);
    }

    /// Wires the council channel so convene_council routes to the council handler.
    pub fn set_council_tx(&mut self, tx: mpsc::Sender<(SessionId, ActionId, serde_json::Value)>) {
        self.council_tx = Some(tx);
    }

    pub fn set_goal_tx(&mut self, tx: mpsc::Sender<(SessionId, ActionId, String, serde_json::Value)>) {
        self.goal_tx = Some(tx);
    }

    /// Shares the live soul.md Arc so `read_soul_md` returns current content.
    pub fn set_soul_arc(&mut self, arc: Arc<RwLock<String>>) {
        self.soul_arc = Some(arc);
    }

    /// Shares the identity registry so `read_soul_md` can return a bound agent's
    /// OWN soul (per-agent souls); without it, reads fall back to the global soul.
    pub fn set_identities(&mut self, ids: Arc<RwLock<apexos_core::Identities>>) {
        self.identities = Some(ids);
    }

    pub fn set_events_dir(&mut self, dir: PathBuf) {
        self.events_dir = Some(dir);
    }

    pub fn set_vast_state(&mut self, state: VastState) {
        self.vast_state = Some(state);
    }

    /// Boot all plugins from config then run the dispatch/supervision loop.
    pub async fn run(
        mut self,
        plugin_configs: Vec<PluginConfig>,
        mut bus_rx: broadcast::Receiver<Event>,
    ) {
        let mut sv_rx = self.sv_rx.take().expect("run() called twice");

        for cfg in plugin_configs {
            let tx = self.sv_tx.clone();
            if let Err(e) = self.spawn_plugin(&cfg, tx).await {
                eprintln!("[supervisor] failed to start plugin '{}': {e}", cfg.id);
            }
        }

        loop {
            tokio::select! {
                result = bus_rx.recv() => match result {
                    Ok(Event::ToolRequested { session, call }) => {
                        // Inspect every path-typed arg, not just `path`, so a
                        // workspace-rule tool can't smuggle a write past the gate
                        // via `output_path`/`dest`/etc. Most-restrictive wins:
                        // Ask if ANY candidate path would Ask under the rule.
                        let path_keys = ["path", "output_path", "dest", "destination", "target", "to"];
                        let candidates: Vec<&str> = path_keys
                            .iter()
                            .filter_map(|k| call.args[*k].as_str())
                            .collect();
                        let decision = {
                            let pol = self.policy.read().await;
                            if candidates.is_empty() {
                                pol.check(&call.tool, None)
                            } else if candidates.iter().any(|p| pol.check(&call.tool, Some(p)) == Decision::Ask) {
                                Decision::Ask
                            } else {
                                Decision::Allow
                            }
                        };
                        match decision {
                            Decision::Allow => {
                                self.dispatch_tool(session, call);
                            }
                            Decision::Ask => {
                                eprintln!("[policy] approval required: '{}' (session {:?})",
                                    call.tool, session);
                                self.pending_approvals.insert(
                                    call.id,
                                    PendingApproval { session, call: call.clone() },
                                );
                                self.bus.emit(Event::ApprovalPending { session, call }).await;
                            }
                        }
                    }

                    Ok(Event::UserApproval { session, action, granted }) => {
                        if let Some(pending) = self.pending_approvals.remove(&action) {
                            if granted {
                                eprintln!("[policy] approved: '{}' (session {:?})",
                                    pending.call.tool, pending.session);
                                self.dispatch_tool(pending.session, pending.call);
                            } else {
                                eprintln!("[policy] denied: '{}' (session {:?})",
                                    pending.call.tool, pending.session);
                                let bus = self.bus.clone();
                                tokio::spawn(async move {
                                    bus.emit(Event::ToolResult {
                                        session,
                                        call: action,
                                        output: ToolOutput {
                                            ok:      false,
                                            content: serde_json::json!("denied by user"),
                                        },
                                    }).await;
                                });
                            }
                        }
                    }

                    Ok(_) => {}
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(_) => break,
                },

                Some(cmd) = sv_rx.recv() => {
                    let tx = self.sv_tx.clone();
                    match cmd {
                        SupervisorCmd::PluginDied { id, success } => {
                            self.handle_died(id, success, tx).await;
                        }
                        SupervisorCmd::SpawnPlugin { config } => {
                            if let Err(e) = self.spawn_plugin(&config, tx).await {
                                eprintln!("[supervisor] spawn failed for '{}': {e}", config.id);
                            }
                        }
                        SupervisorCmd::KillPlugin { id } => {
                            // Remove config first so handle_died (from dying process) won't restart.
                            self.configs.remove(&id);
                            self.tool_registry.retain(|_, owner| owner != &id);
                            let had_plugin = self.plugins.remove(&id).is_some();
                            if had_plugin {
                                self.bus.emit(Event::PluginDown {
                                    plugin: id,
                                    reason: "killed by evolution".into(),
                                }).await;
                            }
                        }
                        SupervisorCmd::HotReload { id } => {
                            // Kill the live instance (keep config so handle_died restarts it).
                            self.tool_registry.retain(|_, owner| owner != &id);
                            let had_plugin = self.plugins.remove(&id).is_some();
                            if had_plugin {
                                self.bus.emit(Event::PluginDown {
                                    plugin: id.clone(),
                                    reason: "hot-reload".into(),
                                }).await;
                            }
                            // For non-Always policies: the process won't self-restart, so force it.
                            if let Some(cfg) = self.configs.get(&id).cloned() {
                                if cfg.restart != RestartPolicy::Always {
                                    tokio::time::sleep(Duration::from_millis(300)).await;
                                    if let Err(e) = self.spawn_plugin(&cfg, tx).await {
                                        eprintln!("[supervisor] hot-reload '{}' failed: {e}", id.0);
                                    }
                                }
                                // else: child exits → PluginDied fires → handle_died restarts
                            }
                        }
                        SupervisorCmd::SetSoulArc { arc } => {
                            self.soul_arc = Some(arc);
                        }
                        SupervisorCmd::SetScheduleTx { tx } => {
                            self.schedule_tx = Some(tx);
                        }
                        SupervisorCmd::SetCouncilTx { tx } => {
                            self.council_tx = Some(tx);
                        }
                        SupervisorCmd::SetVastState { state } => {
                            self.vast_state = Some(state);
                        }
                        SupervisorCmd::SetSelfUpdateTx { tx } => {
                            self.self_update_tx = Some(tx);
                        }
                        SupervisorCmd::DirectCall { tool, args, reply } => {
                            if let Some(pid) = self.tool_registry.get(&tool).cloned() {
                                if let Some(plugin) = self.plugins.get(&pid) {
                                    let client = plugin.client.clone();
                                    tokio::spawn(async move {
                                        let out = match client.call_tool(&tool, &args).await {
                                            Ok(o)  => o,
                                            Err(e) => ToolOutput {
                                                ok:      false,
                                                content: serde_json::json!(e.to_string()),
                                            },
                                        };
                                        let _ = reply.send(out);
                                    });
                                } else {
                                    let _ = reply.send(ToolOutput {
                                        ok: false,
                                        content: serde_json::json!(format!("plugin for '{tool}' not live")),
                                    });
                                }
                            } else {
                                let _ = reply.send(ToolOutput {
                                    ok: false,
                                    content: serde_json::json!(format!("unknown tool: {tool}")),
                                });
                            }
                        }
                    }
                },
            }
        }
    }

    /// Dispatch a tool call immediately (policy already checked).
    fn dispatch_tool(&self, session: SessionId, call: ToolCall) {
        // Virtual tool: propose_evolution — emits EvolutionProposed (for the
        // event log/UI) and routes (session, call_id, evolution_id, proposal) to
        // the evolution applier in main.rs over the dedicated `propose_tx` mpsc.
        // The tool result is DEFERRED: the applier acks after the apply lands, so
        // the result carries the real outcome (no premature success ack).
        if call.tool == "propose_evolution" {
            // Process-global monotonic id — must NOT be derived from the per-turn
            // ActionId (call.id resets each turn, so successive evolutions would
            // collide and corrupt the rollback-snapshot map keyed by EvolutionId).
            let evolution_id = EvolutionId(NEXT_EVOLUTION_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed));
            let call_id      = call.id;
            let bus          = self.bus.clone();
            match serde_json::from_value::<EvolutionProposal>(call.args.clone()) {
                Ok(proposal) => {
                    // DEFERRED ACK: hand the apply to the applier over a DEDICATED
                    // mpsc (not the broadcast bus — a busy turn can lag-drop bus
                    // events, and the agent's tool result must not be lost). The
                    // applier emits the ToolResult only once the apply truly lands,
                    // so the agent learns the REAL outcome — a failed apply no longer
                    // looks like success. EvolutionProposed still goes on the bus for
                    // the UI / event-log / audit.
                    let propose_tx = self.propose_tx.clone();
                    tokio::spawn(async move {
                        bus.emit(Event::EvolutionProposed {
                            id: evolution_id,
                            proposal: proposal.clone(),
                            proposed_by: session,
                        }).await;
                        match propose_tx {
                            Some(tx) => {
                                if tx.send((session, call_id, evolution_id, proposal)).await.is_err() {
                                    bus.emit(Event::ToolResult {
                                        session, call: call_id,
                                        output: ToolOutput { ok: false, content: serde_json::json!("evolution applier unavailable") },
                                    }).await;
                                }
                            }
                            None => {
                                bus.emit(Event::ToolResult {
                                    session, call: call_id,
                                    output: ToolOutput { ok: false, content: serde_json::json!("evolution applier not wired") },
                                }).await;
                            }
                        }
                    });
                }
                Err(e) => {
                    let err = e.to_string();
                    let bus = self.bus.clone();
                    tokio::spawn(async move {
                        bus.emit(Event::ToolResult {
                            session,
                            call: call_id,
                            output: ToolOutput {
                                ok:      false,
                                content: serde_json::json!(
                                    format!("invalid evolution proposal: {err}")
                                ),
                            },
                        }).await;
                    });
                }
            }
            return;
        }

        // Virtual tool: rollback_evolution — routes to the applier via rollback channel.
        if call.tool == "rollback_evolution" {
            let evolution_id = call.args["evolution_id"].as_u64().map(EvolutionId);
            let call_id      = call.id;
            let bus          = self.bus.clone();
            match (evolution_id, self.rollback_tx.as_ref()) {
                (Some(eid), Some(tx)) => {
                    let tx = tx.clone();
                    tokio::spawn(async move {
                        if tx.send((session, call_id, eid)).await.is_err() {
                            bus.emit(Event::ToolResult {
                                session,
                                call: call_id,
                                output: ToolOutput {
                                    ok:      false,
                                    content: serde_json::json!("rollback channel closed"),
                                },
                            }).await;
                        }
                    });
                }
                (None, _) => {
                    tokio::spawn(async move {
                        bus.emit(Event::ToolResult {
                            session,
                            call: call_id,
                            output: ToolOutput {
                                ok:      false,
                                content: serde_json::json!("missing evolution_id"),
                            },
                        }).await;
                    });
                }
                (_, None) => {
                    tokio::spawn(async move {
                        bus.emit(Event::ToolResult {
                            session,
                            call: call_id,
                            output: ToolOutput {
                                ok:      false,
                                content: serde_json::json!("rollback not available"),
                            },
                        }).await;
                    });
                }
            }
            return;
        }

        // Virtual tool: read_soul_md — returns live soul.md content so the agent can
        // read the current system prompt before proposing update_system_prompt.
        if call.tool == "read_soul_md" {
            let call_id    = call.id;
            let bus        = self.bus.clone();
            let soul       = self.soul_arc.clone();
            let bindings   = self.session_bindings.clone();
            let identities = self.identities.clone();
            tokio::spawn(async move {
                // Per-agent souls (docs/agent-identity.md 3b-2): a bound non-default
                // agent reads ITS OWN soul_file, not the global soul.md — otherwise a
                // bound agent reads (and, via propose_evolution, would overwrite) APEX.
                // Unbound/APEX → the live global soul_arc, unchanged.
                let agent_id = apexos_core::resolve_agent_id(&bindings, session);
                let per_agent = if agent_id != apexos_core::node_agent_id() {
                    let soul_file = match &identities {
                        Some(ids) => ids.read().await.agent(&agent_id).map(|r| r.soul_file.clone()),
                        None      => None,
                    };
                    match soul_file {
                        Some(f) => Some(tokio::fs::read_to_string(&f).await
                            .unwrap_or_else(|_| format!("agent '{agent_id}' soul not initialized"))),
                        None => None,   // registry unavailable → fall back to global
                    }
                } else {
                    None
                };
                let content = match per_agent {
                    Some(c) => c,
                    None => match soul {
                        Some(arc) => arc.read().await.clone(),
                        None      => String::from("soul.md not yet initialized"),
                    },
                };
                bus.emit(Event::ToolResult {
                    session,
                    call: call_id,
                    output: ToolOutput {
                        ok:      true,
                        content: serde_json::json!(content),
                    },
                }).await;
            });
            return;
        }

        // Virtual tools: schedule_task / list_schedules / cancel_schedule — forwarded to scheduler task.
        if matches!(call.tool.as_str(), "schedule_task" | "list_schedules" | "cancel_schedule") {
            let call_id  = call.id;
            let tool     = call.tool.clone();
            let args     = call.args.clone();
            let bus      = self.bus.clone();
            match &self.schedule_tx {
                Some(tx) => {
                    let tx = tx.clone();
                    tokio::spawn(async move {
                        if tx.send((session, call_id, tool, args)).await.is_err() {
                            bus.emit(Event::ToolResult {
                                session,
                                call: call_id,
                                output: ToolOutput { ok: false, content: serde_json::json!("scheduler not available") },
                            }).await;
                        }
                    });
                }
                None => {
                    tokio::spawn(async move {
                        bus.emit(Event::ToolResult {
                            session,
                            call: call_id,
                            output: ToolOutput { ok: false, content: serde_json::json!("scheduler not initialized") },
                        }).await;
                    });
                }
            }
            return;
        }

        // Virtual tools: goal_create / goal_step — route to the autonomous goal driver (deferred ack).
        if matches!(call.tool.as_str(), "goal_create" | "goal_step") {
            let call_id = call.id;
            let tool    = call.tool.clone();
            let args    = call.args.clone();
            let bus     = self.bus.clone();
            match &self.goal_tx {
                Some(tx) => {
                    let tx = tx.clone();
                    tokio::spawn(async move {
                        if tx.send((session, call_id, tool, args)).await.is_err() {
                            bus.emit(Event::ToolResult {
                                session, call: call_id,
                                output: ToolOutput { ok: false, content: serde_json::json!("goal driver not available") },
                            }).await;
                        }
                    });
                }
                None => {
                    tokio::spawn(async move {
                        bus.emit(Event::ToolResult {
                            session, call: call_id,
                            output: ToolOutput { ok: false, content: serde_json::json!("goal driver not initialized") },
                        }).await;
                    });
                }
            }
            return;
        }

        // Virtual tool: convene_council — routes to the council handler task.
        if call.tool == "convene_council" {
            let call_id = call.id;
            let args    = call.args.clone();
            let bus     = self.bus.clone();
            match &self.council_tx {
                Some(tx) => {
                    let tx = tx.clone();
                    tokio::spawn(async move {
                        if tx.send((session, call_id, args)).await.is_err() {
                            bus.emit(Event::ToolResult {
                                session,
                                call: call_id,
                                output: ToolOutput { ok: false, content: serde_json::json!("council handler not available") },
                            }).await;
                        }
                    });
                }
                None => {
                    tokio::spawn(async move {
                        bus.emit(Event::ToolResult {
                            session,
                            call: call_id,
                            output: ToolOutput { ok: false, content: serde_json::json!("council not initialized") },
                        }).await;
                    });
                }
            }
            return;
        }

        // Virtual tool: apply_daemon_update — routes to the self-update handler
        // (docs/self-update.md slice 3). The handler runs the pre-swap build/test
        // gates and, on success, files the swap request the root watchdog consumes.
        if call.tool == "apply_daemon_update" {
            let call_id = call.id;
            let args    = call.args.clone();
            let bus     = self.bus.clone();
            match &self.self_update_tx {
                Some(tx) => {
                    let tx = tx.clone();
                    tokio::spawn(async move {
                        if tx.send((session, call_id, args)).await.is_err() {
                            bus.emit(Event::ToolResult {
                                session, call: call_id,
                                output: ToolOutput { ok: false, content: serde_json::json!("self-update handler not available") },
                            }).await;
                        }
                    });
                }
                None => {
                    tokio::spawn(async move {
                        bus.emit(Event::ToolResult {
                            session, call: call_id,
                            output: ToolOutput { ok: false, content: serde_json::json!("self-update handler not wired") },
                        }).await;
                    });
                }
            }
            return;
        }

        // Virtual tool: query_event_log — reads recent JSONL events and returns
        // human-readable summaries for agent analysis / Cerebro ingestion.
        if call.tool == "query_event_log" {
            let hours      = call.args["hours"].as_u64().unwrap_or(24).min(168);
            let types_arg  = call.args["types"].as_str().map(str::to_owned);
            let max_events = call.args["max"].as_u64().unwrap_or(500).min(2000) as usize;
            let events_dir = self.events_dir.clone();
            let bus        = self.bus.clone();
            let call_id    = call.id;
            tokio::spawn(async move {
                let Some(dir) = events_dir else {
                    bus.emit(Event::ToolResult {
                        session, call: call_id,
                        output: ToolOutput { ok: false, content: serde_json::json!("events_dir not configured") },
                    }).await;
                    return;
                };

                let type_filter: Option<std::collections::HashSet<String>> =
                    types_arg.as_deref().map(|s| s.split(',').map(|t| t.trim().to_owned()).collect());

                // Determine which date files to read based on hours window.
                let days_back = ((hours as f64) / 24.0).ceil() as u64 + 1;
                let today = chrono::Local::now().date_naive();
                let mut date_files: Vec<std::path::PathBuf> = Vec::new();
                for d in 0..days_back {
                    let date = today - chrono::Duration::days(d as i64);
                    let path = dir.join(format!("events-{}.jsonl", date.format("%Y-%m-%d")));
                    if tokio::fs::metadata(&path).await.is_ok() {
                        date_files.push(path);
                    }
                }
                date_files.reverse(); // oldest first

                let mut lines: Vec<String> = Vec::new();
                for path in &date_files {
                    let Ok(text) = tokio::fs::read_to_string(path).await else { continue };
                    for line in text.lines() {
                        let line = line.trim();
                        if line.is_empty() { continue }
                        let Ok(val) = serde_json::from_str::<serde_json::Value>(line) else { continue };
                        let ev_type = val["type"].as_str().unwrap_or("").to_owned();
                        if let Some(ref filter) = type_filter {
                            if !filter.contains(&ev_type) { continue }
                        }
                        if let Some(summary) = format_event_line(&val) {
                            lines.push(summary);
                        }
                    }
                }

                // Take the last max_events lines (most recent).
                let total = lines.len();
                if lines.len() > max_events {
                    lines = lines.split_off(lines.len() - max_events);
                }

                let text = if lines.is_empty() {
                    format!("No matching events found in the last {hours}h.")
                } else {
                    format!("Last {hours}h event log ({} events, showing {}):\n\n{}",
                        total, lines.len(), lines.join("\n"))
                };

                bus.emit(Event::ToolResult {
                    session, call: call_id,
                    output: ToolOutput { ok: true, content: serde_json::json!(text) },
                }).await;
            });
            return;
        }

        // Virtual tool: send_to_agent — local or cross-node A2A message.
        // With node: routes via HTTP to a registered peer's /api/sessions/{id}/message.
        if call.tool == "send_to_agent" {
            let to_id    = call.args["session_id"].as_u64().map(SessionId);
            let body     = call.args["message"].as_str().unwrap_or("").to_owned();
            let node_arg = call.args["node"].as_str().map(str::to_owned);
            let call_id  = call.id;
            let msg_id   = call.id.0;
            let bus      = self.bus.clone();

            // Cross-node: look up peer, proxy via HTTP.
            if let Some(node_id) = node_arg {
                let sid = to_id.unwrap_or(SessionId(0)).0;
                tokio::spawn(async move {
                    match find_peer(&node_id).await {
                        None => {
                            bus.emit(Event::ToolResult {
                                session, call: call_id,
                                output: ToolOutput {
                                    ok:      false,
                                    content: serde_json::json!(format!("send_to_agent: peer '{node_id}' not found in peers.toml")),
                                },
                            }).await;
                        }
                        Some((ws_url, token)) => {
                            let http_base = ws_url.replacen("ws://", "http://", 1)
                                                  .replacen("wss://", "https://", 1);
                            let url = format!("{http_base}/api/sessions/{sid}/message");
                            // reqwest (not curl) so the peer's token rides in an Authorization
                            // header rather than argv — never visible in `ps`. The peer's
                            // /api/sessions/{id}/message is token-gated; without the credential
                            // this 401s (the whole reason cross-node a2a needs the per-peer token).
                            // Stamp our node_id as `from` so the receiver can route
                            // this into our own per-peer thread on its side (not its
                            // root session 0) and surface the provenance. Absent on a
                            // generic external POST → the receiver falls back to s0.
                            let mut req = reqwest::Client::new()
                                .post(&url)
                                .json(&serde_json::json!({
                                    "message": body,
                                    "from":    apexos_core::node_id(),
                                }))
                                .timeout(std::time::Duration::from_secs(15));
                            if let Some(tok) = token.as_deref() {
                                req = req.bearer_auth(tok);
                            }
                            let resp = req.send().await;
                            // The handler replies 200 with {ok:bool,…}; a
                            // 200-with-{ok:false} (empty/rejected message) must
                            // NOT read as a delivery — check the body, not just
                            // the HTTP status. (Status-only is what let the old
                            // field mismatch fail silently as a false "sent".)
                            let (status, body_ok) = match resp {
                                Ok(r) => {
                                    let s = r.status();
                                    let b = r.json::<serde_json::Value>().await.ok()
                                        .and_then(|v| v["ok"].as_bool());
                                    (Some(s), b)
                                }
                                Err(_) => (None, None),
                            };
                            let ok = status.map(|s| s.is_success()).unwrap_or(false)
                                && body_ok != Some(false);
                            let detail = match (&token, status) {
                                (None, _)                       => "no token stored for peer — set one to reach a token-gated node",
                                (Some(_), Some(s)) if s == 401  => "peer rejected the token (401) — stale credential?",
                                _                               => if ok { "sent" } else { "delivery failed" },
                            };
                            bus.emit(Event::ToolResult {
                                session, call: call_id,
                                output: ToolOutput {
                                    ok,
                                    content: serde_json::json!({
                                        "status": if ok { "sent" } else { "error" },
                                        "detail": detail,
                                        "node": node_id,
                                        "target_session": sid,
                                    }),
                                },
                            }).await;
                        }
                    }
                });
                return;
            }

            // Local: emit AgentMessage on bus.
            match to_id {
                Some(to) => {
                    tokio::spawn(async move {
                        bus.emit(Event::AgentMessage { from: session, to, body, msg_id }).await;
                        bus.emit(Event::ToolResult {
                            session,
                            call: call_id,
                            output: ToolOutput {
                                ok:      true,
                                content: serde_json::json!({ "status": "sent", "msg_id": msg_id }),
                            },
                        }).await;
                    });
                }
                None => {
                    tokio::spawn(async move {
                        bus.emit(Event::ToolResult {
                            session,
                            call: call_id,
                            output: ToolOutput {
                                ok:      false,
                                content: serde_json::json!("send_to_agent: missing or invalid session_id"),
                            },
                        }).await;
                    });
                }
            }
            return;
        }

        // Virtual tool: mesh_file_send — copy a workspace file to a peer's workspace.
        // Reads a workspace-confined source, POSTs the raw bytes to the peer's
        // token-gated /api/mesh/file (x-dest header = remote relative path).
        if call.tool == "mesh_file_send" {
            let node    = call.args["node"].as_str().map(str::to_owned);
            let path    = call.args["path"].as_str().unwrap_or("").to_owned();
            let dest    = call.args["dest"].as_str().map(str::to_owned);
            let call_id = call.id;
            let bus     = self.bus.clone();
            // Resolve the caller's workspace from its bound identity (system-stamped,
            // not model-supplied) so the source read can't escape the agent's root.
            let agent_id = apexos_core::resolve_agent_id(&self.session_bindings, session);
            tokio::spawn(async move {
                let output = mesh_file_send(node.as_deref(), &agent_id, &path, dest.as_deref()).await;
                bus.emit(Event::ToolResult { session, call: call_id, output }).await;
            });
            return;
        }

        // Virtual tool: mesh_capabilities — query a peer's (or all peers') live
        // senses/tools/tier via their GET /api/capabilities. Capability discovery.
        if call.tool == "mesh_capabilities" {
            let node    = call.args["node"].as_str().map(str::to_owned);
            let call_id = call.id;
            let bus     = self.bus.clone();
            tokio::spawn(async move {
                let output = mesh_capabilities(node.as_deref()).await;
                bus.emit(Event::ToolResult { session, call: call_id, output }).await;
            });
            return;
        }

        // Virtual tool: list_mesh_peers — returns current peers.toml as JSON.
        if call.tool == "list_mesh_peers" {
            let call_id = call.id;
            let bus     = self.bus.clone();
            tokio::spawn(async move {
                let path = std::env::var("PEERS_TOML")
                    .unwrap_or_else(|_| "/etc/agentd/peers.toml".into());
                let content = tokio::fs::read_to_string(&path).await
                    .unwrap_or_else(|_| "# no peers registered\n".into());
                bus.emit(Event::ToolResult {
                    session, call: call_id,
                    output: ToolOutput { ok: true, content: serde_json::json!(content) },
                }).await;
            });
            return;
        }

        // Virtual tool: bootstrap_node — SSH to target, clone ApexOS repo, background install.sh.
        // Returns quickly; install takes 15-20 min and node appears in mesh via mDNS.
        if call.tool == "bootstrap_node" {
            let target_ip   = call.args["target_ip"].as_str().unwrap_or("").to_owned();
            let ssh_password = call.args["ssh_password"].as_str().unwrap_or("").to_owned();
            let ssh_user    = call.args["ssh_user"].as_str().unwrap_or("apexos").to_owned();
            let api_key     = call.args["api_key"].as_str().unwrap_or("").to_owned();
            let repo_url    = call.args["repo_url"].as_str()
                .unwrap_or("https://github.com/buckster123/ApexOS-RS.git").to_owned();
            let call_id     = call.id;
            let bus         = self.bus.clone();

            if target_ip.is_empty() || ssh_password.is_empty() {
                tokio::spawn(async move {
                    bus.emit(Event::ToolResult {
                        session, call: call_id,
                        output: ToolOutput {
                            ok:      false,
                            content: serde_json::json!("bootstrap_node: target_ip and ssh_password are required"),
                        },
                    }).await;
                });
                return;
            }

            // Reject metacharacter-bearing host/user. Both are interpolated into
            // root-run remote shell scripts AND the ssh destination, so a shell
            // metacharacter or leading `-` here is a root-RCE / ssh argument-
            // injection vector — validate rather than escape (ssh_password,
            // repo_url, api_key go through shell_single_quote; a host/user never
            // legitimately contains metacharacters).
            if !is_valid_host(&target_ip) || !is_valid_username(&ssh_user) {
                tokio::spawn(async move {
                    bus.emit(Event::ToolResult {
                        session, call: call_id,
                        output: ToolOutput {
                            ok:      false,
                            content: serde_json::json!(
                                "bootstrap_node: target_ip must be a plain IP/hostname and ssh_user a plain login name (letters, digits, '_', '-')"
                            ),
                        },
                    }).await;
                });
                return;
            }

            tokio::spawn(async move {
                let ssh_base = vec![
                    "sshpass".to_string(),
                    format!("-p{ssh_password}"),
                    "ssh".into(),
                    "-o".into(), "StrictHostKeyChecking=accept-new".into(),
                    "-o".into(), "ConnectTimeout=5".into(),
                    "--".into(), // end ssh option parsing — destination can't be read as a flag
                    format!("{ssh_user}@{target_ip}"),
                ];

                // Step 1: connectivity check
                let ok = tokio::process::Command::new(&ssh_base[0])
                    .args(&ssh_base[1..])
                    .arg("echo OK")
                    .output().await;
                match ok {
                    Err(e) => {
                        bus.emit(Event::ToolResult {
                            session, call: call_id,
                            output: ToolOutput {
                                ok:      false,
                                content: serde_json::json!(format!("SSH to {target_ip} failed: {e}")),
                            },
                        }).await;
                        return;
                    }
                    Ok(o) if !o.status.success() => {
                        let stderr = String::from_utf8_lossy(&o.stderr);
                        bus.emit(Event::ToolResult {
                            session, call: call_id,
                            output: ToolOutput {
                                ok:      false,
                                content: serde_json::json!(format!("SSH auth failed for {ssh_user}@{target_ip}: {stderr}")),
                            },
                        }).await;
                        return;
                    }
                    _ => {}
                }

                // Step 2: check if already an ApexOS node
                let check = tokio::process::Command::new(&ssh_base[0])
                    .args(&ssh_base[1..])
                    .arg("systemctl is-active agentd 2>/dev/null || echo inactive")
                    .output().await;
                if let Ok(o) = check {
                    let out = String::from_utf8_lossy(&o.stdout).trim().to_string();
                    if out == "active" {
                        bus.emit(Event::ToolResult {
                            session, call: call_id,
                            output: ToolOutput {
                                ok:      true,
                                content: serde_json::json!(format!(
                                    "{target_ip} is already running agentd. Register it manually with POST /api/mesh/peers."
                                )),
                            },
                        }).await;
                        return;
                    }
                }

                // Step 3: install git if needed, clone repo
                let repo_url_q = shell_single_quote(&repo_url);
                let prep_cmd = format!(
                    "apt-get install -y -q git 2>/dev/null; \
                     git clone {repo_url_q} /home/{ssh_user}/ApexOS-RS 2>/dev/null || \
                     git -C /home/{ssh_user}/ApexOS-RS pull"
                );
                let prep = tokio::time::timeout(
                    tokio::time::Duration::from_secs(60),
                    tokio::process::Command::new(&ssh_base[0])
                        .args(&ssh_base[1..])
                        .arg(format!(
                            "echo {} | sudo -S bash -c {}",
                            shell_single_quote(&ssh_password),
                            shell_single_quote(&prep_cmd),
                        ))
                        .output(),
                ).await;
                if prep.is_err() {
                    bus.emit(Event::ToolResult {
                        session, call: call_id,
                        output: ToolOutput {
                            ok:      false,
                            content: serde_json::json!("bootstrap_node: git clone timed out"),
                        },
                    }).await;
                    return;
                }

                // Step 4: inject API key (if provided) and background install.sh
                let api_key_export = if api_key.is_empty() {
                    String::new()
                } else {
                    format!("export ANTHROPIC_API_KEY={}; ", shell_single_quote(&api_key))
                };
                let install_cmd = format!(
                    "cd /home/{ssh_user}/ApexOS-RS && \
                     {api_key_export}\
                     nohup bash install.sh > /tmp/apex-install.log 2>&1 &\
                     echo $!"
                );
                let launch = tokio::process::Command::new(&ssh_base[0])
                    .args(&ssh_base[1..])
                    .arg(format!(
                        "echo {} | sudo -S bash -c {}",
                        shell_single_quote(&ssh_password),
                        shell_single_quote(&install_cmd),
                    ))
                    .output().await;

                let pid_line = launch.as_ref().ok()
                    .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                    .unwrap_or_default();

                let msg = format!(
                    "Bootstrap of {ssh_user}@{target_ip} started (PID {pid_line}). \
                     install.sh is running in background — takes 15-20 min. \
                     Monitor: ssh {ssh_user}@{target_ip} tail -f /tmp/apex-install.log. \
                     The node will appear in the mesh automatically once Avahi starts."
                );
                bus.emit(Event::ToolResult {
                    session, call: call_id,
                    output: ToolOutput { ok: true, content: serde_json::json!(msg) },
                }).await;
            });
            return;
        }

        // ── Vast.ai virtual tools ──────────────────────────────────────────────

        // vast_list_recipes — read recipes.toml, return JSON array (no Vast API).
        if call.tool == "vast_list_recipes" {
            let call_id = call.id;
            let bus     = self.bus.clone();
            tokio::spawn(async move {
                let result = load_recipes();
                match result {
                    Ok(rf) => {
                        let summary: Vec<serde_json::Value> = rf.recipes.iter().map(|r| {
                            let tier = rf.gpu_tiers.get(&r.gpu);
                            serde_json::json!({
                                "name":        r.name,
                                "label":       r.label,
                                "gpu":         r.gpu,
                                "gpu_label":   tier.map(|t| t.label.as_str()).unwrap_or(&r.gpu),
                                "model_repo":  r.model_repo,
                                "model_quant": r.model_quant,
                                "ctx":         r.ctx,
                                "parallel":    r.parallel,
                                "description": r.description,
                            })
                        }).collect();
                        bus.emit(Event::ToolResult {
                            session, call: call_id,
                            output: ToolOutput { ok: true, content: serde_json::json!(summary) },
                        }).await;
                    }
                    Err(e) => {
                        bus.emit(Event::ToolResult {
                            session, call: call_id,
                            output: ToolOutput { ok: false, content: serde_json::json!(e.to_string()) },
                        }).await;
                    }
                }
            });
            return;
        }

        // vast_status — return current VastState as JSON.
        if call.tool == "vast_status" {
            let call_id    = call.id;
            let bus        = self.bus.clone();
            let vast_state = self.vast_state.clone();
            tokio::spawn(async move {
                let Some(vs) = vast_state else {
                    bus.emit(Event::ToolResult {
                        session, call: call_id,
                        output: ToolOutput { ok: true, content: serde_json::json!({ "status": "idle", "note": "vast not configured" }) },
                    }).await;
                    return;
                };
                let inst  = vs.instance.read().await.clone();
                let phase = vs.phase.read().await.clone();
                let status = match &phase {
                    VastPhase::Idle       => "idle",
                    VastPhase::Launching { .. } => "launching",
                    VastPhase::Ready      => "ready",
                    VastPhase::Destroying => "destroying",
                };
                let mut val = serde_json::json!({ "status": status });
                if let VastPhase::Launching { phase: p } = &phase {
                    val["phase"] = serde_json::json!(p);
                }
                if let Some(i) = inst {
                    val["instance"] = serde_json::json!({
                        "id":          i.id,
                        "recipe":      i.recipe,
                        "local_port":  i.local_port,
                        "cost_per_hr": i.cost_per_hr,
                        "launched_at": i.launched_at,
                    });
                }
                bus.emit(Event::ToolResult {
                    session, call: call_id,
                    output: ToolOutput { ok: true, content: val },
                }).await;
            });
            return;
        }

        // vast_launch — full async lifecycle: find offer → create → tunnel → health → hot-swap.
        if call.tool == "vast_launch" {
            let recipe_name = call.args["recipe"].as_str().unwrap_or("qwen36-27b-q6-5090").to_owned();
            let geo         = call.args["geo"].as_str()
                .map(str::to_owned)
                .or_else(|| std::env::var("VAST_DEFAULT_GEO").ok())
                .unwrap_or_else(|| "EU_NORDIC".into());
            let call_id     = call.id;
            let bus         = self.bus.clone();
            let vast_state  = self.vast_state.clone();

            let Some(vs) = vast_state else {
                tokio::spawn(async move {
                    bus.emit(Event::ToolResult {
                        session, call: call_id,
                        output: ToolOutput { ok: false, content: serde_json::json!("vast not configured") },
                    }).await;
                });
                return;
            };

            tokio::spawn(async move {
                // Step 1: check for existing instance
                if vs.instance.read().await.is_some() {
                    let phase_str = match *vs.phase.read().await {
                        VastPhase::Launching { ref phase } => format!("launching ({})", phase),
                        VastPhase::Ready      => "ready".into(),
                        VastPhase::Destroying => "destroying".into(),
                        VastPhase::Idle       => "idle".into(),
                    };
                    bus.emit(Event::ToolResult {
                        session, call: call_id,
                        output: ToolOutput {
                            ok: false,
                            content: serde_json::json!(format!(
                                "vast_launch: instance already exists (status: {}). Call vast_destroy first.",
                                phase_str
                            )),
                        },
                    }).await;
                    return;
                }

                // Step 2: load recipe
                let rf = match load_recipes() {
                    Ok(r)  => r,
                    Err(e) => {
                        bus.emit(Event::ToolResult {
                            session, call: call_id,
                            output: ToolOutput { ok: false, content: serde_json::json!(e.to_string()) },
                        }).await;
                        return;
                    }
                };
                let recipe = match rf.recipes.iter().find(|r| r.name == recipe_name) {
                    Some(r) => r.clone(),
                    None => {
                        bus.emit(Event::ToolResult {
                            session, call: call_id,
                            output: ToolOutput {
                                ok: false,
                                content: serde_json::json!(format!("recipe '{}' not found", recipe_name)),
                            },
                        }).await;
                        return;
                    }
                };
                let tier = match rf.gpu_tiers.get(&recipe.gpu) {
                    Some(t) => t.clone(),
                    None => {
                        bus.emit(Event::ToolResult {
                            session, call: call_id,
                            output: ToolOutput {
                                ok: false,
                                content: serde_json::json!(format!("gpu tier '{}' not found", recipe.gpu)),
                            },
                        }).await;
                        return;
                    }
                };
                let docker_image = rf.docker.prebuilt.clone();
                let local_port: u16 = std::env::var("VAST_LOCAL_PORT")
                    .ok().and_then(|s| s.parse().ok()).unwrap_or(8000);

                *vs.phase.write().await = VastPhase::Launching { phase: "searching for offer".into() };
                eprintln!("[vast] launching recipe={} geo={}", recipe.name, geo);

                // Step 3: search offers
                let gpu_filter = tier.vast_names.iter()
                    .map(|n| format!("gpu_name={}", n))
                    .collect::<Vec<_>>()
                    .join(" | ");
                let query = format!(
                    "({gpu_filter}) reliability>0.99 inet_down>300 \
                     dph_total<{} disk_space>{}",
                    tier.max_price, tier.min_disk_gb
                );
                eprintln!("[vast] offer search: {query}");
                let offers_out = match vastai(&["search", "offers", &query, "--order", "dph_total", "--raw"]).await {
                    Ok(o) => o,
                    Err(e) => {
                        *vs.phase.write().await = VastPhase::Idle;
                        bus.emit(Event::ToolResult {
                            session, call: call_id,
                            output: ToolOutput { ok: false, content: serde_json::json!(format!("offer search failed: {e}")) },
                        }).await;
                        return;
                    }
                };

                // Parse JSON array from offers — filter by geo, take cheapest
                let offers: Vec<serde_json::Value> = serde_json::from_str(&offers_out)
                    .unwrap_or_default();
                let geo_re = match geo.as_str() {
                    "EU_NORDIC" => vec!["SE", "NO", "FI", "DK", "IS"],
                    "EU"        => vec!["SE", "NO", "FI", "DK", "IS", "DE", "NL", "FR", "GB", "PL"],
                    "US"        => vec!["US"],
                    _           => vec![],
                };
                let offer = offers.iter().find(|o| {
                    if geo_re.is_empty() { return true; }
                    let geo_str = o["geolocation"].as_str().unwrap_or("");
                    geo_re.iter().any(|code| geo_str.contains(code))
                }).or_else(|| offers.first());

                let offer_id = match offer.and_then(|o| o["id"].as_u64()) {
                    Some(id) => id.to_string(),
                    None => {
                        *vs.phase.write().await = VastPhase::Idle;
                        bus.emit(Event::ToolResult {
                            session, call: call_id,
                            output: ToolOutput { ok: false, content: serde_json::json!("no matching offers found") },
                        }).await;
                        return;
                    }
                };
                let cost_per_hr = offer
                    .and_then(|o| o["dph_total"].as_f64())
                    .unwrap_or(0.0);
                eprintln!("[vast] selected offer {offer_id} at ${cost_per_hr:.3}/hr");
                *vs.phase.write().await = VastPhase::Launching { phase: "creating instance".into() };

                // Step 4: create instance
                let onstart = format!(
                    "MODEL_REPO={} MODEL_QUANT={} CTX={} KV_TYPE={} MODE=thinking PARALLEL={} HOST=127.0.0.1 bash /app/launch.sh > /var/log/launch.log 2>&1 &",
                    recipe.model_repo, recipe.model_quant, recipe.ctx, recipe.kv_type, recipe.parallel
                );
                let env_str = format!(
                    "MODEL_REPO={} MODEL_QUANT={} CTX={} KV_TYPE=q8_0 MODE=thinking PARALLEL={} HOST=127.0.0.1",
                    recipe.model_repo, recipe.model_quant, recipe.ctx, recipe.parallel
                );
                let create_out = match vastai(&[
                    "create", "instance", &offer_id,
                    "--image", &docker_image,
                    "--disk",  &tier.min_disk_gb.to_string(),
                    "--env",   &env_str,
                    "--onstart-cmd", &onstart,
                    "--raw",
                ]).await {
                    Ok(o) => o,
                    Err(e) => {
                        *vs.phase.write().await = VastPhase::Idle;
                        bus.emit(Event::ToolResult {
                            session, call: call_id,
                            output: ToolOutput { ok: false, content: serde_json::json!(format!("create failed: {e}")) },
                        }).await;
                        return;
                    }
                };
                let create_json: serde_json::Value = serde_json::from_str(&create_out).unwrap_or_default();
                let instance_id = match create_json["new_contract"].as_u64()
                    .or_else(|| create_json["id"].as_u64())
                {
                    Some(id) => id.to_string(),
                    None => {
                        *vs.phase.write().await = VastPhase::Idle;
                        bus.emit(Event::ToolResult {
                            session, call: call_id,
                            output: ToolOutput { ok: false, content: serde_json::json!(format!("create returned unexpected JSON: {create_out}")) },
                        }).await;
                        return;
                    }
                };
                eprintln!("[vast] instance created: {instance_id}");
                bus.emit(Event::VastInstanceLaunched {
                    instance_id: instance_id.clone(),
                    recipe:      recipe.name.clone(),
                    cost_per_hr,
                }).await;
                *vs.phase.write().await = VastPhase::Launching { phase: "waiting for SSH".into() };

                // Step 5: poll until running + SSH available (5 min timeout)
                let (ssh_host, ssh_port) = {
                    let mut attempts = 0u32;
                    let mut found = None;
                    while attempts < 30 {
                        tokio::time::sleep(Duration::from_secs(10)).await;
                        attempts += 1;
                        let show = match vastai(&["show", "instance", &instance_id, "--raw"]).await {
                            Ok(o) => o,
                            Err(_) => continue,
                        };
                        let info: serde_json::Value = serde_json::from_str(&show).unwrap_or_default();
                        let arr = if info.is_array() { &info } else { &serde_json::json!([info]) };
                        if let Some(inst_info) = arr.as_array().and_then(|a| a.first()) {
                            let status = inst_info["actual_status"].as_str().unwrap_or("");
                            let host   = inst_info["ssh_host"].as_str().unwrap_or("");
                            let port   = inst_info["ssh_port"].as_u64().unwrap_or(0) as u16;
                            if status == "running" && !host.is_empty() && port > 0 {
                                found = Some((host.to_owned(), port));
                                break;
                            }
                            eprintln!("[vast] waiting for SSH: status={status} attempt={attempts}/30");
                        }
                    }
                    match found {
                        Some(v) => v,
                        None => {
                            // cleanup orphaned instance
                            let _ = vastai(&["destroy", "instance", &instance_id]).await;
                            *vs.phase.write().await = VastPhase::Idle;
                            bus.emit(Event::ToolResult {
                                session, call: call_id,
                                output: ToolOutput { ok: false, content: serde_json::json!("timed out waiting for instance to come up (5 min)") },
                            }).await;
                            return;
                        }
                    }
                };
                eprintln!("[vast] SSH ready: {ssh_host}:{ssh_port}");

                // Step 6: write instance state + persist
                let now = chrono::Utc::now().to_rfc3339();
                let inst = VastInstance {
                    id:          instance_id.clone(),
                    recipe:      recipe.name.clone(),
                    ssh_host:    ssh_host.clone(),
                    ssh_port,
                    local_port,
                    cost_per_hr,
                    launched_at: now,
                };
                *vs.instance.write().await = Some(inst.clone());
                vs.persist_instance().await;
                *vs.phase.write().await = VastPhase::Launching { phase: "opening tunnel".into() };

                // Step 7: open SSH tunnel
                let cm_path = format!("/tmp/apex-vast-cm-{instance_id}");
                let mut ssh = tokio::process::Command::new("ssh");
                ssh.args([
                    "-f", "-N",
                    "-o", "StrictHostKeyChecking=accept-new",
                    "-o", "ControlMaster=auto",
                    "-o", &format!("ControlPath={cm_path}"),
                    "-o", "ControlPersist=5m",
                    "-o", "ServerAliveInterval=30",
                    "-o", "ExitOnForwardFailure=yes",
                    "-L", &format!("{local_port}:127.0.0.1:8000"),
                    "-p", &ssh_port.to_string(),
                    &format!("root@{ssh_host}"),
                ])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null());
                let tunnel_child = match ssh.spawn() {
                    Ok(c) => c,
                    Err(e) => {
                        let _ = vastai(&["destroy", "instance", &instance_id]).await;
                        vs.clear_instance().await;
                        bus.emit(Event::ToolResult {
                            session, call: call_id,
                            output: ToolOutput { ok: false, content: serde_json::json!(format!("SSH tunnel spawn failed: {e}")) },
                        }).await;
                        return;
                    }
                };
                {
                    use crate::vast::TunnelHandle;
                    *vs.tunnel.lock().await = Some(TunnelHandle { child: tunnel_child, local_port });
                }
                // Give SSH a moment to establish
                tokio::time::sleep(Duration::from_secs(3)).await;
                *vs.phase.write().await = VastPhase::Launching { phase: "waiting for model load".into() };

                // Step 8: poll health until model ready (20 min timeout)
                let health_url = format!("http://127.0.0.1:{local_port}/health");
                let client = reqwest::Client::builder()
                    .timeout(Duration::from_secs(5))
                    .build()
                    .unwrap_or_default();
                let mut ready = false;
                for attempt in 0..80 {
                    tokio::time::sleep(Duration::from_secs(15)).await;
                    match client.get(&health_url).send().await {
                        Ok(r) if r.status().is_success() => {
                            ready = true;
                            break;
                        }
                        _ => {
                            if attempt % 4 == 0 {
                                eprintln!("[vast] waiting for model (attempt {}/80)", attempt + 1);
                            }
                        }
                    }
                }
                if !ready {
                    let _ = vastai(&["destroy", "instance", &instance_id]).await;
                    vs.clear_instance().await;
                    bus.emit(Event::ToolResult {
                        session, call: call_id,
                        output: ToolOutput { ok: false, content: serde_json::json!("model failed to load in 20 minutes") },
                    }).await;
                    return;
                }

                // Step 9: ready — emit event for main.rs to hot-swap backend
                *vs.phase.write().await = VastPhase::Ready;
                eprintln!("[vast] model ready on port {local_port}");
                bus.emit(Event::VastInstanceReady { instance_id: instance_id.clone(), local_port }).await;

                // Spawn keepalive loop
                let vs_ka  = vs.clone();
                let bus_ka = bus.clone();
                let id_ka  = instance_id.clone();
                tokio::spawn(async move {
                    let c = reqwest::Client::builder()
                        .timeout(Duration::from_secs(5))
                        .build()
                        .unwrap_or_default();
                    let url = format!("http://127.0.0.1:{local_port}/health");
                    let mut fails: u32 = 0;
                    loop {
                        tokio::time::sleep(Duration::from_secs(30)).await;
                        let alive = vs_ka.instance.read().await.is_some();
                        if !alive { break; }
                        match c.get(&url).send().await {
                            Ok(r) if r.status().is_success() => { fails = 0; }
                            _ => {
                                fails += 1;
                                eprintln!("[vast] keepalive fail {fails}/3");
                                if fails >= 3 {
                                    eprintln!("[vast] tunnel lost after 3 failures");
                                    bus_ka.emit(Event::VastTunnelLost { instance_id: id_ka.clone() }).await;
                                    break;
                                }
                            }
                        }
                    }
                });

                bus.emit(Event::ToolResult {
                    session, call: call_id,
                    output: ToolOutput {
                        ok: true,
                        content: serde_json::json!({
                            "status":      "ready",
                            "instance_id": instance_id,
                            "recipe":      recipe.name,
                            "model":       recipe.model_repo,
                            "quant":       recipe.model_quant,
                            "ctx":         recipe.ctx,
                            "parallel":    recipe.parallel,
                            "cost_per_hr": cost_per_hr,
                            "local_port":  local_port,
                            "message":     "Backend hot-swapped to Vast.ai instance. Agent will now use this model."
                        }),
                    },
                }).await;
            });
            return;
        }

        // vast_destroy — tear down instance + tunnel + revert backend.
        if call.tool == "vast_destroy" {
            let call_id    = call.id;
            let bus        = self.bus.clone();
            let vast_state = self.vast_state.clone();

            let Some(vs) = vast_state else {
                tokio::spawn(async move {
                    bus.emit(Event::ToolResult {
                        session, call: call_id,
                        output: ToolOutput { ok: true, content: serde_json::json!("no vast state") },
                    }).await;
                });
                return;
            };

            tokio::spawn(async move {
                let inst = vs.instance.read().await.clone();
                let Some(i) = inst else {
                    bus.emit(Event::ToolResult {
                        session, call: call_id,
                        output: ToolOutput { ok: true, content: serde_json::json!("no active instance") },
                    }).await;
                    return;
                };
                *vs.phase.write().await = VastPhase::Destroying;
                let instance_id = i.id.clone();

                // Kill tunnel
                {
                    let mut guard = vs.tunnel.lock().await;
                    if let Some(mut t) = guard.take() {
                        let _ = t.child.kill().await;
                        let _ = tokio::fs::remove_file(
                            format!("/tmp/apex-vast-cm-{instance_id}")
                        ).await;
                    }
                }

                // Destroy Vast instance
                match vastai(&["destroy", "instance", &instance_id]).await {
                    Ok(_)  => eprintln!("[vast] instance {instance_id} destroyed"),
                    Err(e) => eprintln!("[vast] destroy error (continuing): {e}"),
                }

                vs.clear_instance().await;
                bus.emit(Event::VastInstanceDestroyed { instance_id: instance_id.clone() }).await;

                bus.emit(Event::ToolResult {
                    session, call: call_id,
                    output: ToolOutput {
                        ok: true,
                        content: serde_json::json!({
                            "status":      "destroyed",
                            "instance_id": instance_id,
                            "message":     "Instance destroyed. Backend reverted to default."
                        }),
                    },
                }).await;
            });
            return;
        }

        // ── end Vast.ai virtual tools ──────────────────────────────────────────

        // Virtual tool: agent_spawn is handled by the async router, not an MCP plugin.
        if call.tool == "agent_spawn" {
            let prompt  = call.args["prompt"].as_str().unwrap_or("").to_owned();
            let system  = call.args["system"].as_str().map(str::to_owned);
            let node    = call.args["node"].as_str().filter(|s| !s.is_empty()).map(str::to_owned);
            let timeout_s = call.args["timeout_s"].as_u64().unwrap_or(30).clamp(5, 300);
            let bus     = self.bus.clone();
            let call_id = call.id;
            match node {
                // Cross-node: BLOCKING spawn on the peer (colony-mesh keystone). Run a
                // sub-agent there, get its output back over HTTP (timeout + breaker).
                Some(node_id) => {
                    tokio::spawn(async move {
                        let output = mesh_agent_spawn(&node_id, &prompt, system.as_deref(), timeout_s).await;
                        bus.emit(Event::ToolResult { session, call: call_id, output }).await;
                    });
                }
                // Local: the existing sub-agent flow (SpawnAgent → child_turn).
                None => {
                    tokio::spawn(async move {
                        bus.emit(Event::SpawnAgent { parent: session, call_id, prompt, system }).await;
                    });
                }
            }
            return;
        }

        let tool_name = call.tool.clone();
        if let Some(pid) = self.tool_registry.get(&tool_name).cloned() {
            if let Some(plugin) = self.plugins.get(&pid) {
                let client   = plugin.client.clone();
                let bus      = self.bus.clone();
                let mut call = call;
                // System-stamp the agent identity onto Cerebro calls: routing and
                // private/shared isolation must not depend on the agent_id the
                // model typed (it can forget, typo, or — multi-agent — spoof).
                // Resolved from the calling session's binding (else the node
                // default). See docs/agent-identity.md (slices 1 & 3b).
                if pid.0 == "cerebro" {
                    let agent_id = apexos_core::resolve_agent_id(&self.session_bindings, session);
                    stamp_agent_id(&mut call.args, &agent_id);
                } else if pid.0 == "apexos-tools" {
                    // System-stamp the caller's workspace so the shared (single)
                    // tool process confines this call's FS ops to the per-agent
                    // root. Always stamped (APEX → the node base), so a model
                    // can't redirect confinement by injecting `__workspace`.
                    let agent_id = apexos_core::resolve_agent_id(&self.session_bindings, session);
                    let ws = apexos_core::agent_workspace_root(&agent_id);
                    stamp_workspace(&mut call.args, &ws.to_string_lossy());
                }
                tokio::spawn(async move {
                    let output = match client.call_tool(&call.tool, &call.args).await {
                        Ok(o)  => o,
                        Err(e) => ToolOutput {
                            ok:      false,
                            content: serde_json::json!(e.to_string()),
                        },
                    };
                    bus.emit(Event::ToolResult { session, call: call.id, output }).await;
                });
                return;
            }
        }
        // Unknown tool or plugin not live → return error so the turn loop unblocks.
        let bus     = self.bus.clone();
        let call_id = call.id;
        tokio::spawn(async move {
            bus.emit(Event::ToolResult {
                session,
                call: call_id,
                output: ToolOutput {
                    ok:      false,
                    content: serde_json::json!(format!("unknown tool: {tool_name}")),
                },
            }).await;
        });
    }

    async fn spawn_plugin(
        &mut self,
        cfg: &PluginConfig,
        sv_tx: mpsc::Sender<SupervisorCmd>,
    ) -> anyhow::Result<()> {
        let plugin_id = PluginId(cfg.id.clone());

        let mut cmd = Command::new(&cfg.cmd);
        cmd.args(&cfg.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        if let Some(cwd) = &cfg.cwd {
            cmd.current_dir(cwd);
        }
        if let Some(env) = &cfg.env {
            for (k, v) in env {
                cmd.env(k, v);
            }
        }

        let mut child = cmd.spawn()?;

        if let Some(stderr) = child.stderr.take() {
            let id = plugin_id.clone();
            tokio::spawn(async move {
                use tokio::io::AsyncBufReadExt;
                let mut lines = tokio::io::BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    eprintln!("[plugin:{id}] {line}");
                }
            });
        }

        let client = Arc::new(McpClient::attach(&mut child).await?);
        client.initialize().await?;
        let tools = client.list_tools().await?;

        for spec in &tools {
            self.tool_registry.insert(spec.name.clone(), plugin_id.clone());
        }

        eprintln!("[supervisor] plugin '{}' up — {} tools", plugin_id.0, tools.len());
        self.bus.emit(Event::PluginUp { plugin: plugin_id.clone(), tools }).await;

        let id_w = plugin_id.clone();
        tokio::spawn(async move {
            // Clean exit (status 0) → success; a non-zero exit, a signal, or a
            // wait() error → failure (so OnFailure restarts on a crash only).
            let success = child.wait().await.map(|s| s.success()).unwrap_or(false);
            let _ = sv_tx.send(SupervisorCmd::PluginDied { id: id_w, success }).await;
        });

        self.configs.insert(plugin_id.clone(), cfg.clone());
        self.plugins.insert(plugin_id, Plugin { client });
        Ok(())
    }

    async fn handle_died(&mut self, id: PluginId, success: bool, sv_tx: mpsc::Sender<SupervisorCmd>) {
        self.tool_registry.retain(|_, owner| owner != &id);
        // Only emit PluginDown if the plugin was still in our live set.
        // HotReload removes it first, so this avoids a duplicate PluginDown event.
        let was_live = self.plugins.remove(&id).is_some();

        if was_live {
            eprintln!("[supervisor] plugin '{}' died", id.0);
            self.bus.emit(Event::PluginDown {
                plugin: id.clone(),
                reason: "process exited".into(),
            }).await;
        }

        let cfg = match self.configs.get(&id) {
            Some(c) => c.clone(),
            None    => return,
        };

        if should_restart(&cfg.restart, success) {
            eprintln!("[supervisor] restarting '{}' in 1s… (exited {})",
                id.0, if success { "cleanly" } else { "with failure" });
            tokio::time::sleep(Duration::from_secs(1)).await;
            if let Err(e) = self.spawn_plugin(&cfg, sv_tx).await {
                eprintln!("[supervisor] restart of '{}' failed: {e}", id.0);
            }
        }
    }
}

/// Whether a died plugin should be respawned, given its restart policy and
/// whether it exited cleanly (status 0). `OnFailure` restarts on a crash but not
/// a clean shutdown — which is why the watcher must carry the exit status through
/// `PluginDied` (it was discarded before, so `OnFailure` silently never restarted).
fn should_restart(policy: &RestartPolicy, exited_success: bool) -> bool {
    match policy {
        RestartPolicy::Always    => true,
        RestartPolicy::OnFailure => !exited_success,
        RestartPolicy::Never     => false,
    }
}

/// Convert a raw event JSON object into a concise human-readable sentence.
/// Returns None for high-frequency noise events (agent_text, tool_result, etc.)
fn format_event_line(v: &serde_json::Value) -> Option<String> {
    let t = v["type"].as_str()?;
    let line = match t {
        // Skip streaming noise
        "agent_text" | "agent_thinking" | "tool_result" | "turn_complete" => return None,

        "user_prompt" => {
            let session = v["session"].as_u64().unwrap_or(0);
            let text    = v["text"].as_str().unwrap_or("").chars().take(120).collect::<String>();
            format!("Session {session}: user said '{text}'")
        }
        "tool_requested" => {
            let session = v["session"].as_u64().unwrap_or(0);
            let tool    = v["call"]["tool"].as_str().unwrap_or("?");
            format!("Session {session}: tool '{tool}' called")
        }
        "approval_pending" => {
            let session = v["session"].as_u64().unwrap_or(0);
            let tool    = v["call"]["tool"].as_str().unwrap_or("?");
            format!("Session {session}: tool '{tool}' awaiting approval")
        }
        "user_approval" => {
            let session = v["session"].as_u64().unwrap_or(0);
            let granted = v["granted"].as_bool().unwrap_or(false);
            format!("Session {session}: approval {}", if granted { "granted" } else { "denied" })
        }
        "evolution_proposed" => {
            let kind   = v["proposal"]["kind"].as_str().unwrap_or("?");
            let reason = v["proposal"]["reason"].as_str().unwrap_or("");
            format!("Evolution proposed: {kind} — '{reason}'")
        }
        "evolution_applied" => {
            let kind   = v["proposal"]["kind"].as_str().unwrap_or("?");
            let reason = v["proposal"]["reason"].as_str().unwrap_or("");
            format!("Evolution applied: {kind} — '{reason}'")
        }
        "evolution_rolled_back" => {
            format!("Evolution rolled back (id={})", v["id"].as_u64().unwrap_or(0))
        }
        "plugin_up" => {
            let plugin = v["plugin"].as_str().unwrap_or("?");
            let n      = v["tools"].as_array().map(|a| a.len()).unwrap_or(0);
            format!("Plugin '{plugin}' started ({n} tools)")
        }
        "plugin_down" => {
            let plugin = v["plugin"].as_str().unwrap_or("?");
            format!("Plugin '{plugin}' stopped")
        }
        "wake_triggered"   => "Wake word triggered".into(),
        "spawn_agent"      => {
            let parent = v["parent"].as_u64().unwrap_or(0);
            let prompt = v["prompt"].as_str().unwrap_or("").chars().take(80).collect::<String>();
            format!("Session {parent}: spawned sub-agent — '{prompt}'")
        }
        "sub_agent_started" => {
            let child  = v["child"].as_u64().unwrap_or(0);
            let parent = v["parent"].as_u64().unwrap_or(0);
            format!("Sub-agent {child} started (parent: {parent})")
        }
        "sensor_reading" => {
            let node    = v["node_id"].as_str().unwrap_or("?");
            let reading = &v["reading"];
            if let Some(iaq) = reading["iaq"].as_f64() {
                let temp = reading["temperature"].as_f64().unwrap_or(0.0);
                let rh   = reading["humidity"].as_f64().unwrap_or(0.0);
                format!("Sensor {node}: IAQ={iaq:.0} Temp={temp:.1}°C RH={rh:.0}%")
            } else {
                format!("Sensor {node}: {reading}")
            }
        }
        "council_started" => {
            let id    = v["id"].as_str().unwrap_or("?");
            let topic = v["topic"].as_str().unwrap_or("");
            format!("Council '{id}' started: topic='{topic}'")
        }
        "council_complete" => {
            let id    = v["id"].as_str().unwrap_or("?");
            let synth = v["synthesis"].as_str().unwrap_or("").chars().take(100).collect::<String>();
            format!("Council '{id}' complete: '{synth}'")
        }
        "agent_message" => {
            let from = v["from"].as_u64().unwrap_or(0);
            let to   = v["to"].as_u64().unwrap_or(0);
            let body = v["body"].as_str().unwrap_or("").chars().take(80).collect::<String>();
            format!("Agent {from} → Agent {to}: '{body}'")
        }
        // Unknown event types: show the type name so they appear in results
        other => format!("[{other}]"),
    };
    Some(line)
}

/// Wrap `s` in single quotes, escaping any embedded single quote, so it is safe
/// to interpolate into a POSIX shell command. `'` becomes `'\''` (close quote,
/// escaped literal quote, reopen quote). Prevents command injection through
/// interpolated user-supplied values (ssh password, repo url, api key).
fn shell_single_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        if c == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(c);
        }
    }
    out.push('\'');
    out
}

/// A safe POSIX login name to interpolate into `bootstrap_node`'s root-run remote
/// shell scripts (`/home/<user>/ApexOS-RS`) and the ssh destination: starts with a
/// letter or `_`, then only letters/digits/`_`/`-`, ≤32 chars. No shell
/// metacharacters, no `/`, no leading `-` — so it can neither inject a command
/// into `sudo -S bash -c …` nor be parsed as an ssh option. Validated, not
/// escaped: a metacharacter here is never a legitimate username.
fn is_valid_username(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 32
        && s.chars().next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// A safe ssh destination host (IPv4 / bracketless IPv6 / DNS hostname): no shell
/// metacharacters and no leading `-` (which would let it be read as an ssh flag).
/// Conservative allowlist of the characters those forms actually use.
fn is_valid_host(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 255
        && !s.starts_with('-')
        && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | ':' | '-' | '_'))
}

// ── mesh helpers ──────────────────────────────────────────────────────────────

/// Look up a peer's ws_url by node_id in peers.toml. Async because it reads a file.
/// Look up a peer in peers.toml, returning its ws_url and (optional) a2a token.
async fn find_peer(node_id: &str) -> Option<(String, Option<String>)> {
    #[derive(serde::Deserialize)]
    struct PeersFile { #[serde(default)] peer: Vec<PeerEntry> }
    #[derive(serde::Deserialize)]
    struct PeerEntry { node_id: String, ws_url: String, #[serde(default)] token: Option<String> }

    let path = std::env::var("PEERS_TOML").unwrap_or_else(|_| "/etc/agentd/peers.toml".into());
    let raw  = tokio::fs::read_to_string(&path).await.ok()?;
    let file: PeersFile = toml::from_str(&raw).ok()?;
    file.peer.into_iter().find(|p| p.node_id == node_id).map(|p| (p.ws_url, p.token))
}

/// Confine a mesh-relay SOURCE path to `agent_id`'s workspace root. Rejects `..`
/// and requires the canonical result to stay inside the workspace — the read can't
/// escape the (system-stamped) per-agent root. Mirrors the FS-tool confine rule.
fn confine_mesh_source(agent_id: &str, rel: &str) -> Result<std::path::PathBuf, String> {
    let p = std::path::Path::new(rel);
    if p.components().any(|c| c == std::path::Component::ParentDir) {
        return Err("path traversal (..) is not allowed".to_string());
    }
    let root = apexos_core::agent_workspace_root(agent_id);
    let joined = if p.is_absolute() { p.to_path_buf() } else { root.join(p) };
    let canon = std::fs::canonicalize(&joined).map_err(|e| format!("{rel}: {e}"))?;
    let root_canon = std::fs::canonicalize(&root).map_err(|e| format!("workspace: {e}"))?;
    if !canon.starts_with(&root_canon) {
        return Err(format!("{rel} escapes the workspace"));
    }
    Ok(canon)
}

/// Send a workspace file to a registered peer's token-gated `/api/mesh/file`.
/// Raw bytes in the body (binary-safe), remote relative path in the `x-dest` header.
/// 5 MB cap. Confinement at BOTH ends (source here, dest on the receiver).
async fn mesh_file_send(node: Option<&str>, agent_id: &str, path: &str, dest: Option<&str>) -> ToolOutput {
    let err = |m: String| ToolOutput { ok: false, content: serde_json::json!(m) };
    let node = match node {
        Some(n) if !n.is_empty() => n,
        _ => return err("mesh_file_send: missing 'node'".into()),
    };
    if path.is_empty() {
        return err("mesh_file_send: missing 'path'".into());
    }
    let abs = match confine_mesh_source(agent_id, path) {
        Ok(p)  => p,
        Err(e) => return err(format!("mesh_file_send: {e}")),
    };
    let bytes = match tokio::fs::read(&abs).await {
        Ok(b)  => b,
        Err(e) => return err(format!("mesh_file_send: read {path}: {e}")),
    };
    let n = bytes.len();
    if n > 5 * 1024 * 1024 {
        return err(format!("mesh_file_send: {path} is {n} bytes (> 5 MB cap)"));
    }
    let filename = abs.file_name().and_then(|f| f.to_str()).unwrap_or("file").to_string();
    let remote = dest.filter(|d| !d.is_empty()).unwrap_or(&filename).to_string();

    let (ws_url, token) = match find_peer(node).await {
        Some(p) => p,
        None    => return err(format!("mesh_file_send: peer '{node}' not found in peers.toml")),
    };
    let http_base = ws_url.replacen("ws://", "http://", 1).replacen("wss://", "https://", 1);
    let mut req = reqwest::Client::new()
        .post(format!("{http_base}/api/mesh/file"))
        .header("x-dest", &remote)
        .body(bytes)
        .timeout(std::time::Duration::from_secs(30));
    if let Some(t) = token.as_deref() {
        req = req.bearer_auth(t);
    }
    match req.send().await {
        Ok(r) => {
            let status = r.status();
            let v = r.json::<serde_json::Value>().await.ok();
            let ok = status.is_success() && v.as_ref().and_then(|b| b["ok"].as_bool()) == Some(true);
            if ok {
                ToolOutput { ok: true, content: serde_json::json!({
                    "status": "sent", "node": node, "bytes": n, "remote_path": remote,
                }) }
            } else {
                let detail = v.as_ref().and_then(|b| b["error"].as_str())
                    .unwrap_or(if token.is_none() { "no token stored for peer" } else { "delivery failed" });
                err(format!("mesh_file_send: {detail} (status {status})"))
            }
        }
        Err(e) => err(format!("mesh_file_send: {e}")),
    }
}

/// All peer node_ids in peers.toml (for an all-peers capability sweep).
async fn list_peer_ids() -> Vec<String> {
    #[derive(serde::Deserialize)]
    struct PeersFile { #[serde(default)] peer: Vec<PeerEntry> }
    #[derive(serde::Deserialize)]
    struct PeerEntry { node_id: String }
    let path = std::env::var("PEERS_TOML").unwrap_or_else(|_| "/etc/agentd/peers.toml".into());
    tokio::fs::read_to_string(&path).await.ok()
        .and_then(|raw| toml::from_str::<PeersFile>(&raw).ok())
        .map(|f| f.peer.into_iter().map(|p| p.node_id).collect())
        .unwrap_or_default()
}

/// Fetch one peer's `GET /api/capabilities` (token-gated). Never errors hard — a
/// failure becomes `{node, error}` so an all-peers sweep returns partial results.
async fn fetch_peer_capabilities(node: &str) -> serde_json::Value {
    let (ws_url, token) = match find_peer(node).await {
        Some(p) => p,
        None    => return serde_json::json!({ "node": node, "error": "not in peers.toml" }),
    };
    let http_base = ws_url.replacen("ws://", "http://", 1).replacen("wss://", "https://", 1);
    let mut req = reqwest::Client::new()
        .get(format!("{http_base}/api/capabilities"))
        .timeout(std::time::Duration::from_secs(10));
    if let Some(t) = token.as_deref() {
        req = req.bearer_auth(t);
    }
    match req.send().await {
        Ok(r) if r.status().is_success() =>
            r.json::<serde_json::Value>().await
                .unwrap_or_else(|_| serde_json::json!({ "node": node, "error": "unparseable response" })),
        Ok(r)  => serde_json::json!({ "node": node, "error": format!("status {}", r.status()) }),
        Err(e) => serde_json::json!({ "node": node, "error": e.to_string() }),
    }
}

/// `mesh_capabilities(node?)`: one peer's capabilities, or an array over all peers.
async fn mesh_capabilities(node: Option<&str>) -> ToolOutput {
    match node {
        Some(n) if !n.is_empty() => ToolOutput { ok: true, content: fetch_peer_capabilities(n).await },
        _ => {
            let mut out = Vec::new();
            for id in list_peer_ids().await {
                out.push(fetch_peer_capabilities(&id).await);
            }
            ToolOutput { ok: true, content: serde_json::json!(out) }
        }
    }
}

// ── cross-node agent_spawn: blocking remote sub-agent (colony-mesh keystone) ─────

/// Per-peer circuit breaker for cross-node spawn: after `BREAKER_TRIP` consecutive
/// failures a peer is "open" (short-circuited) for `BREAKER_COOLDOWN`, so repeated
/// calls to a dead/slow peer fail fast instead of each waiting the full timeout.
struct Breaker { consecutive_failures: u32, open_until: Option<std::time::Instant> }
const BREAKER_TRIP: u32 = 3;
const BREAKER_COOLDOWN: std::time::Duration = std::time::Duration::from_secs(60);

fn breakers() -> &'static std::sync::Mutex<std::collections::HashMap<String, Breaker>> {
    static B: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<String, Breaker>>> =
        std::sync::OnceLock::new();
    B.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

/// Is the peer's breaker currently open (within its cooldown)?
fn breaker_open(node: &str, now: std::time::Instant) -> bool {
    let map = breakers().lock().unwrap_or_else(|e| e.into_inner());
    map.get(node).and_then(|b| b.open_until).map(|t| t > now).unwrap_or(false)
}

/// Record a spawn outcome: success resets the breaker; the `BREAKER_TRIP`-th
/// consecutive failure opens it for `BREAKER_COOLDOWN`.
fn breaker_record(node: &str, ok: bool, now: std::time::Instant) {
    let mut map = breakers().lock().unwrap_or_else(|e| e.into_inner());
    let b = map.entry(node.to_string()).or_insert(Breaker { consecutive_failures: 0, open_until: None });
    if ok {
        b.consecutive_failures = 0;
        b.open_until = None;
    } else {
        b.consecutive_failures += 1;
        if b.consecutive_failures >= BREAKER_TRIP {
            b.open_until = Some(now + BREAKER_COOLDOWN);
            b.consecutive_failures = 0;
        }
    }
}

/// Blocking cross-node spawn: run `prompt` as a sub-agent on `node` and return its
/// output. Bounded by the peer's own `timeout_s` plus HTTP slack; a per-peer circuit
/// breaker fails fast when a peer is repeatedly unreachable. `x-mesh-hops: 1` lets
/// the receiver refuse a runaway cross-node recursion.
async fn mesh_agent_spawn(node: &str, prompt: &str, system: Option<&str>, timeout_s: u64) -> ToolOutput {
    let err = |m: String| ToolOutput { ok: false, content: serde_json::json!(m) };
    let now = std::time::Instant::now();
    if breaker_open(node, now) {
        return err(format!("agent_spawn: peer '{node}' circuit open (recent failures) — retry shortly"));
    }
    let (ws_url, token) = match find_peer(node).await {
        Some(p) => p,
        None    => return err(format!("agent_spawn: peer '{node}' not found in peers.toml")),
    };
    let http_base = ws_url.replacen("ws://", "http://", 1).replacen("wss://", "https://", 1);
    let mut body = serde_json::json!({ "prompt": prompt, "timeout_s": timeout_s });
    if let Some(s) = system { body["system"] = serde_json::json!(s); }
    let mut req = reqwest::Client::new()
        .post(format!("{http_base}/api/spawn"))
        .header("x-mesh-hops", "1")
        .json(&body)
        .timeout(std::time::Duration::from_secs(timeout_s + 20));
    if let Some(t) = token.as_deref() {
        req = req.bearer_auth(t);
    }
    match req.send().await {
        Ok(r) => {
            let status = r.status();
            let v = r.json::<serde_json::Value>().await.ok();
            let ok = status.is_success() && v.as_ref().and_then(|b| b["ok"].as_bool()) == Some(true);
            breaker_record(node, ok, std::time::Instant::now());
            if ok {
                let out = v.as_ref().and_then(|b| b["output"].as_str()).unwrap_or("");
                ToolOutput { ok: true, content: serde_json::json!({ "node": node, "output": out }) }
            } else {
                let detail = v.as_ref().and_then(|b| b["error"].as_str())
                    .unwrap_or(if token.is_none() { "no token stored for peer" } else { "spawn failed" });
                err(format!("agent_spawn: {detail} (status {status})"))
            }
        }
        Err(e) => {
            breaker_record(node, false, std::time::Instant::now());
            err(format!("agent_spawn: {e}"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;

    #[test]
    fn circuit_breaker_trips_after_3_failures_and_resets_on_success() {
        let node = "test-breaker-node-xyz"; // unique key in the global map
        let t0 = std::time::Instant::now();
        assert!(!breaker_open(node, t0), "starts closed");
        // Two failures: still closed (trip is the 3rd).
        breaker_record(node, false, t0);
        breaker_record(node, false, t0);
        assert!(!breaker_open(node, t0), "2 failures < trip");
        // Third consecutive failure opens the breaker for the cooldown.
        breaker_record(node, false, t0);
        assert!(breaker_open(node, t0 + std::time::Duration::from_secs(30)), "open within cooldown");
        assert!(!breaker_open(node, t0 + std::time::Duration::from_secs(61)), "closed after cooldown");
        // A success clears it.
        let t1 = t0 + std::time::Duration::from_secs(5);
        breaker_record(node, false, t1);
        breaker_record(node, false, t1);
        breaker_record(node, true, t1);
        assert!(!breaker_open(node, t1), "success resets the failure streak");
    }

    #[test]
    fn mesh_source_rejects_traversal() {
        // The `..` guard short-circuits before any workspace canonicalize, so this
        // is testable without a real workspace on disk.
        let e = confine_mesh_source("APEX", "../../etc/passwd").unwrap_err();
        assert!(e.contains("traversal"), "got: {e}");
        let e = confine_mesh_source("APEX", "notes/../../x").unwrap_err();
        assert!(e.contains("traversal"), "got: {e}");
    }

    #[test]
    fn should_restart_honors_each_policy() {
        // Always: restart regardless of how it exited.
        assert!(should_restart(&RestartPolicy::Always, true));
        assert!(should_restart(&RestartPolicy::Always, false));
        // OnFailure: restart only on a non-clean exit (the bug — it used to never
        // restart because the exit status was discarded).
        assert!(!should_restart(&RestartPolicy::OnFailure, true));
        assert!(should_restart(&RestartPolicy::OnFailure, false));
        // Never: never restart.
        assert!(!should_restart(&RestartPolicy::Never, true));
        assert!(!should_restart(&RestartPolicy::Never, false));
    }

    // Serialize the tests that mutate the process-global NEXT_EVOLUTION_ID — Rust
    // runs tests in parallel, and an interleaved fetch_add/fetch_max would break
    // the monotonic-by-one assertion below.
    static EVO_ID_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn evolution_ids_are_distinct_across_proposals() {
        let _g = EVO_ID_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // Two evolutions in the same process must receive distinct ids, even
        // though the per-turn ActionId (call.id) resets each turn.
        let a = EvolutionId(NEXT_EVOLUTION_ID.fetch_add(1, Ordering::Relaxed));
        let b = EvolutionId(NEXT_EVOLUTION_ID.fetch_add(1, Ordering::Relaxed));
        assert_ne!(a.0, b.0, "successive evolutions must not collide");
        assert_eq!(b.0, a.0 + 1, "ids must be monotonic");
    }

    #[test]
    fn seed_evolution_id_advances_but_never_rewinds() {
        let _g = EVO_ID_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // Cold-start restore seeds the counter past the max restored id so a fresh
        // evolution can't reuse an id and alias a restored undo snapshot.
        seed_evolution_id(100_000);
        assert!(NEXT_EVOLUTION_ID.load(Ordering::Relaxed) >= 100_000);
        let next = NEXT_EVOLUTION_ID.fetch_add(1, Ordering::Relaxed); // >= 100_000
        // A lower seed (an older/smaller max) must NOT rewind the counter — fetch_max
        // floor — so subsequently allocated ids stay strictly ahead.
        seed_evolution_id(5);
        assert!(NEXT_EVOLUTION_ID.load(Ordering::Relaxed) > next,
            "seed must never rewind the counter");
    }

    #[test]
    fn stamp_agent_id_overrides_forges_and_coerces() {
        // Overrides whatever the model supplied (anti-spoof / anti-typo).
        let mut a = serde_json::json!({ "query": "x", "agent_id": "SOMEONE_ELSE" });
        stamp_agent_id(&mut a, "APEX");
        assert_eq!(a["agent_id"], "APEX");
        assert_eq!(a["query"], "x"); // other args untouched

        // Adds it when the model omitted it.
        let mut b = serde_json::json!({ "query": "x" });
        stamp_agent_id(&mut b, "APEX");
        assert_eq!(b["agent_id"], "APEX");

        // Coerces a non-object (null) args into an object carrying the stamp.
        let mut c = serde_json::Value::Null;
        stamp_agent_id(&mut c, "LUMA");
        assert_eq!(c["agent_id"], "LUMA");
    }

    #[test]
    fn stamp_workspace_overrides_model_supplied_root() {
        // Anti-spoof: a model can't widen its confinement by injecting __workspace.
        let mut a = serde_json::json!({ "path": "notes.txt", "__workspace": "/" });
        stamp_workspace(&mut a, "/var/lib/agentd/workspace/workspaces/LUMA");
        assert_eq!(a["__workspace"], "/var/lib/agentd/workspace/workspaces/LUMA");
        assert_eq!(a["path"], "notes.txt"); // other args untouched

        // Adds it when absent; coerces a non-object args into one carrying it.
        let mut b = serde_json::Value::Null;
        stamp_workspace(&mut b, "/var/lib/agentd/workspace");
        assert_eq!(b["__workspace"], "/var/lib/agentd/workspace");
    }

    #[test]
    fn shell_single_quote_escapes_injection() {
        // Normal input is just wrapped in quotes.
        assert_eq!(shell_single_quote("https://x/repo.git"), "'https://x/repo.git'");
        // Embedded single quote is neutralized: '\'' closes/reopens the quote.
        assert_eq!(
            shell_single_quote("a'; rm -rf / #"),
            "'a'\\''; rm -rf / #'"
        );
        // Shell metacharacters stay inert because they remain inside the quotes.
        let q = shell_single_quote("$(touch /tmp/pwned)");
        assert_eq!(q, "'$(touch /tmp/pwned)'");
    }

    #[test]
    fn bootstrap_node_rejects_injection_in_user_and_host() {
        // Legitimate values pass.
        assert!(is_valid_username("apexos"));
        assert!(is_valid_username("pi"));
        assert!(is_valid_username("apex_node-1"));
        assert!(is_valid_host("192.168.0.158"));
        assert!(is_valid_host("fe80::1"));
        assert!(is_valid_host("node.local"));

        // ssh_user injection into the root-run `sudo -S bash -c …` script — the
        // real hole the original finding missed — is rejected.
        assert!(!is_valid_username("x; rm -rf / #"));
        assert!(!is_valid_username("a$(touch /tmp/p)"));
        assert!(!is_valid_username("a`id`"));
        assert!(!is_valid_username("../etc"));   // no '/'
        assert!(!is_valid_username("-oProxyCommand=x")); // no leading '-'
        assert!(!is_valid_username(""));         // empty
        assert!(!is_valid_username("9pi"));      // must start alpha/_

        // ssh destination metacharacters / option-injection are rejected.
        assert!(!is_valid_host("-oProxyCommand=touch /tmp/x"));
        assert!(!is_valid_host("1.2.3.4; reboot"));
        assert!(!is_valid_host("$(id)"));
        assert!(!is_valid_host(""));
    }
}
