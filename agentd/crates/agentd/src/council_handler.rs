use std::path::PathBuf;
use std::sync::Arc;
use tokio::io::AsyncWriteExt;
use tokio::sync::{broadcast, mpsc, RwLock};
use apexos_core::{BusHandle, CouncilAgentDef, Event, SessionId, ActionId, ToolOutput};
use apexos_agent::run_council;
use apexos_gateway::{CouncilButtInMap, CouncilRecord, CouncilSessionsMap};
use apexos_plugins::ToolProxy;

/// Message type: (calling session, tool call id, raw convene_council args)
pub type CouncilMsg = (SessionId, ActionId, serde_json::Value);

/// Spawn the council handler task.
///
/// Receives `convene_council` tool calls from the supervisor and direct API
/// starts from the gateway. Runs each council as an isolated tokio task.
pub fn spawn_council_handler(
    mut rx:          mpsc::Receiver<CouncilMsg>,
    bcast:           broadcast::Sender<Event>,
    bus:             BusHandle,
    anthropic_key:   Arc<RwLock<String>>,
    oai_api_key:     Arc<RwLock<String>>,
    oai_base_url:    Arc<RwLock<String>>,
    backend_arc:     Arc<RwLock<String>>,
    model_arc:       Arc<RwLock<String>>,
    butt_in_map:     CouncilButtInMap,
    sessions:        CouncilSessionsMap,
    council_log_dir: PathBuf,
    tool_proxy:      ToolProxy,
) {
    tokio::spawn(async move {
        let mut counter: u64 = 0;

        while let Some((session, call_id, args)) = rx.recv().await {
            // Respect a caller-supplied council_id (used by the gateway for direct API starts)
            // so the ID is known before the call is made. Fall back to internal counter.
            let council_id = if let Some(id) = args["council_id"].as_str() {
                id.to_owned()
            } else {
                counter += 1;
                format!("c{counter}")
            };

            let topic = args["topic"].as_str().unwrap_or("").to_owned();
            let max_rounds = args["max_rounds"].as_u64().unwrap_or(3) as u32;
            let consensus_threshold = args["consensus_threshold"].as_f64().unwrap_or(0.7) as f32;

            // Parse agents array — supports both string IDs and objects
            let agent_defs: Vec<CouncilAgentDef> = match args["agents"].as_array() {
                Some(arr) => arr.iter().filter_map(parse_agent_def).collect(),
                None => {
                    // Skip ToolResult for gateway-initiated calls (sentinel session)
                    if session.0 != u64::MAX {
                        let bus_c = bus.clone();
                        let call_id_c = call_id;
                        tokio::spawn(async move {
                            bus_c.emit(Event::ToolResult {
                                session,
                                call: call_id_c,
                                output: ToolOutput {
                                    ok: false,
                                    content: serde_json::json!("convene_council: 'agents' must be an array"),
                                },
                            }).await;
                        });
                    }
                    continue;
                }
            };

            if agent_defs.is_empty() {
                if session.0 != u64::MAX {
                    let bus_c = bus.clone();
                    let call_id_c = call_id;
                    tokio::spawn(async move {
                        bus_c.emit(Event::ToolResult {
                            session,
                            call: call_id_c,
                            output: ToolOutput {
                                ok: false,
                                content: serde_json::json!("convene_council: at least one agent required"),
                            },
                        }).await;
                    });
                }
                continue;
            }

            // Clone arcs for the spawned task
            let ant_key   = Arc::clone(&anthropic_key);
            let oai_key   = Arc::clone(&oai_api_key);
            let oai_url   = Arc::clone(&oai_base_url);
            let bus_c     = bus.clone();
            let bcast_c   = bcast.clone();

            // butt_in channel — gateway can inject human messages mid-council
            let (butt_in_tx, butt_in_rx) = mpsc::channel::<String>(4);

            // Register this council in the shared maps before spawning
            butt_in_map.lock().await.insert(council_id.clone(), butt_in_tx);
            let record = CouncilRecord {
                id:        council_id.clone(),
                topic:     topic.clone(),
                agents:    agent_defs.clone(),
                status:    "running".into(),
                rounds:    0,
                synthesis: String::new(),
            };
            sessions.lock().await.push(record);

            let default_backend = backend_arc.read().await.clone();
            let default_model   = model_arc.read().await.clone();

            let cid_done   = council_id.clone();
            let butt_map_c = Arc::clone(&butt_in_map);
            let sessions_c = Arc::clone(&sessions);
            let log_dir_c  = council_log_dir.clone();
            let proxy_c    = tool_proxy.clone();
            let topic_c    = topic.clone();
            let agents_c   = agent_defs.clone();

            tokio::spawn(async move {
                // Start JSONL log writer — self-terminates on CouncilComplete for this id
                let mut event_sub = bcast_c.subscribe();
                let cid_log  = cid_done.clone();
                let log_path = log_dir_c.join(format!("{cid_log}.jsonl"));
                tokio::fs::create_dir_all(&log_dir_c).await.ok();
                let log_task = tokio::spawn(async move {
                    let mut file = match tokio::fs::OpenOptions::new()
                        .create(true).append(true).open(&log_path).await
                    {
                        Ok(f)  => f,
                        Err(e) => { eprintln!("[council] log open: {e}"); return; }
                    };
                    loop {
                        match event_sub.recv().await {
                            Ok(ev) => {
                                let done = matches!(&ev, Event::CouncilComplete { council_id: cid, .. } if cid == &cid_log);
                                if let Some(line) = council_log_line(&cid_log, &ev) {
                                    let _ = file.write_all(format!("{line}\n").as_bytes()).await;
                                }
                                if done { break; }
                            }
                            // A transient lag (busy bus) must not truncate the
                            // council log — skip the dropped span and keep recording.
                            Err(broadcast::error::RecvError::Lagged(_)) => continue,
                            Err(broadcast::error::RecvError::Closed)    => break,
                        }
                    }
                });

                let synthesis = run_council(
                    council_id,
                    topic_c.clone(),
                    agent_defs,
                    max_rounds,
                    consensus_threshold,
                    ant_key,
                    oai_key,
                    oai_url,
                    default_backend,
                    default_model,
                    bus_c.clone(),
                    bcast_c,
                    butt_in_rx,
                ).await;

                // Wait for log writer to flush
                let _ = log_task.await;

                // Clean up butt-in entry
                butt_map_c.lock().await.remove(&cid_done);

                // Update session record to complete
                let mut sess = sessions_c.lock().await;
                if let Some(r) = sess.iter_mut().find(|r| r.id == cid_done) {
                    r.status    = "complete".into();
                    r.synthesis = synthesis.clone();
                    if r.rounds == 0 { r.rounds = 1; }
                }
                drop(sess);

                // Store council summary in Cerebro (best-effort)
                let agent_ids = agents_c.iter().map(|a| a.id.as_str()).collect::<Vec<_>>().join(", ");
                let content = format!(
                    "Council [{cid_done}] — Topic: {topic_c}\nAgents: {agent_ids}\nSynthesis: {synthesis}"
                );
                tokio::spawn(async move {
                    match proxy_c.call("memory_store", serde_json::json!({
                        "content": content,
                        "tags":    ["council", "apexos"],
                        "agent_id": apexos_core::node_agent_id()
                    })).await {
                        Ok(out) if out.ok => {}
                        Ok(out) => eprintln!("[council] cerebro store not ok: {:?}", out.content),
                        Err(e)  => eprintln!("[council] cerebro store: {e}"),
                    }
                });

                // Only emit ToolResult for agent-originated calls (not gateway sentinel)
                if session.0 != u64::MAX {
                    bus_c.emit(Event::ToolResult {
                        session,
                        call: call_id,
                        output: ToolOutput {
                            ok:      true,
                            content: serde_json::json!(synthesis),
                        },
                    }).await;
                }
            });
        }
    });
}

/// Serialise a council bus event to a JSONL line for the session log.
/// Returns None for non-council events and for CouncilAgentDelta (too noisy).
fn council_log_line(cid: &str, ev: &Event) -> Option<String> {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let v = match ev {
        Event::CouncilStarted { council_id, topic, agents } if council_id == cid =>
            serde_json::json!({
                "type": "start", "id": council_id, "topic": topic,
                "agents": agents.iter().map(|a| &a.id).collect::<Vec<_>>(), "ts": ts
            }),
        Event::CouncilRoundStart { council_id, round } if council_id == cid =>
            serde_json::json!({ "type": "round_start", "round": round, "ts": ts }),
        Event::CouncilAgentDone { council_id, round, agent_id, full_text } if council_id == cid =>
            serde_json::json!({
                "type": "agent_done", "round": round,
                "agent": agent_id, "text": full_text, "ts": ts
            }),
        Event::CouncilRoundDone { council_id, round, convergence, agreements } if council_id == cid =>
            serde_json::json!({
                "type": "round_done", "round": round,
                "convergence": convergence, "agreements": agreements, "ts": ts
            }),
        Event::CouncilComplete { council_id, rounds, reason, synthesis } if council_id == cid =>
            serde_json::json!({
                "type": "complete", "rounds": rounds,
                "reason": reason, "synthesis": synthesis, "ts": ts
            }),
        _ => return None,
    };
    serde_json::to_string(&v).ok()
}

fn parse_agent_def(v: &serde_json::Value) -> Option<CouncilAgentDef> {
    if let Some(id) = v.as_str() {
        return Some(CouncilAgentDef {
            id:      id.to_owned(),
            persona: String::new(),
            backend: None,
            model:   None,
            color:   None,
        });
    }
    if let Some(obj) = v.as_object() {
        let id = obj.get("id")?.as_str()?.to_owned();
        let persona = obj.get("persona")
            .and_then(|p| p.as_str())
            .unwrap_or("")
            .to_owned();
        return Some(CouncilAgentDef {
            id,
            persona,
            backend: obj.get("backend").and_then(|b| b.as_str()).map(str::to_owned),
            model:   obj.get("model").and_then(|m| m.as_str()).map(str::to_owned),
            color:   obj.get("color").and_then(|c| c.as_str()).map(str::to_owned),
        });
    }
    None
}
