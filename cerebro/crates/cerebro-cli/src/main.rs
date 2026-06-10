use std::sync::Arc;

use anyhow::Result;
use cerebro::{
    models::AssociativeLink,
    storage::ListFilter,
    types::{AgentId, LinkType, MemoryId, MemoryType, VisibilityScope},
    CerebroCortex,
};
use chrono::Utc;
use clap::{Parser, Subcommand};
use serde_json::{json, Value};

// ---------------------------------------------------------------------------
// CLI root
// ---------------------------------------------------------------------------

/// cerebro — CerebroCortex command-line interface
#[derive(Parser)]
#[command(name = "cerebro", version, about)]
struct Cli {
    /// Agent ID scope (empty = global)
    #[arg(long, global = true, env = "CEREBRO_AGENT")]
    agent: Option<String>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Show system statistics
    Stats {
        #[arg(long)] json: bool,
    },
    /// Store a new memory
    Remember {
        content:      String,
        #[arg(long, default_value = "semantic")] memory_type: String,
        #[arg(long, value_delimiter = ',')] tags:     Vec<String>,
        #[arg(long)] salience: Option<f64>,
        #[arg(long)] json:     bool,
    },
    /// Search memories
    Recall {
        query: String,
        #[arg(short = 'n', long, default_value = "10")] top: usize,
        #[arg(long)] json: bool,
    },
    /// Get a memory by ID
    Get {
        id:         String,
        #[arg(long)] json: bool,
    },
    /// Soft-delete a memory
    Delete {
        id:           String,
        #[arg(long)] force: bool,
    },
    /// Update a memory's content / tags / salience
    Update {
        id:          String,
        #[arg(long)] content:  Option<String>,
        #[arg(long, value_delimiter = ',')] tags: Vec<String>,
        #[arg(long)] salience: Option<f64>,
        #[arg(long)] json:     bool,
    },
    /// Create an associative link between two memories
    Associate {
        source_id:   String,
        target_id:   String,
        #[arg(long, default_value = "semantic")] link_type: String,
        #[arg(long, default_value_t = 0.5)] weight: f64,
    },
    /// Show memory health diagnostics
    Health {
        #[arg(long)] json: bool,
    },
    /// Show emotional state summary
    Emotions {
        #[arg(long)] json: bool,
    },
    /// Episode management
    Episode {
        #[command(subcommand)] cmd: EpisodeCmd,
    },
    /// Session note management
    Session {
        #[command(subcommand)] cmd: SessionCmd,
    },
    /// Agent registry
    Agents {
        #[command(subcommand)] cmd: AgentsCmd,
    },
    /// Intention / goal tracking
    Intention {
        #[command(subcommand)] cmd: IntentionCmd,
    },
    /// Memory graph operations
    Graph {
        #[command(subcommand)] cmd: GraphCmd,
    },
    /// Schema (abstract principle) management
    Schema {
        #[command(subcommand)] cmd: SchemaCmd,
    },
    /// Procedural memory management
    Procedure {
        #[command(subcommand)] cmd: ProcedureCmd,
    },
    /// Dream consolidation
    Dream {
        #[command(subcommand)] cmd: DreamCmd,
    },
}

// ---------------------------------------------------------------------------
// Subcommand enums
// ---------------------------------------------------------------------------

#[derive(Subcommand)]
enum EpisodeCmd {
    /// Start a new episode
    Start {
        title: String,
        #[arg(long)] json: bool,
    },
    /// Add a memory to an episode
    Step {
        episode_id: String,
        memory_id:  String,
        #[arg(long, default_value = "memory")] role: String,
    },
    /// End an episode
    End {
        episode_id: String,
        #[arg(long)] summary: Option<String>,
    },
    /// List episodes
    List {
        #[arg(short = 'n', long, default_value = "20")] limit: usize,
        #[arg(long)] json: bool,
    },
    /// Get episode details
    Get {
        episode_id: String,
        #[arg(long)] json: bool,
    },
}

#[derive(Subcommand)]
enum SessionCmd {
    /// Save a session note
    Save {
        summary: String,
        #[arg(long, default_value = "medium")] priority: String,
        #[arg(long, default_value = "general")] session_type: String,
        #[arg(long)] json: bool,
    },
    /// Recall recent session notes
    Recall {
        query:   String,
        #[arg(short = 'n', long, default_value = "10")] top: usize,
        #[arg(long)] json: bool,
    },
}

#[derive(Subcommand)]
enum AgentsCmd {
    /// List registered agents
    List {
        #[arg(long)] json: bool,
    },
    /// Register a new agent
    Register {
        agent_id:     String,
        display_name: String,
        #[arg(long, default_value = "🤖")] symbol: String,
        #[arg(long, default_value = "#888888")] color: String,
        #[arg(long)] json: bool,
    },
}

#[derive(Subcommand)]
enum IntentionCmd {
    /// Add an intention / goal
    Add {
        content: String,
        #[arg(long, value_delimiter = ',')] tags: Vec<String>,
        #[arg(long)] json: bool,
    },
    /// List active intentions
    List {
        #[arg(long)] json: bool,
    },
    /// Mark an intention as resolved
    Resolve {
        id: String,
    },
}

#[derive(Subcommand)]
enum GraphCmd {
    /// Find shortest path between two memories
    Path {
        source_id: String,
        target_id: String,
        #[arg(long)] json: bool,
    },
    /// Find common neighbors of two memories
    Common {
        id_a: String,
        id_b: String,
        #[arg(long)] json: bool,
    },
    /// List graph stats
    Stats {
        #[arg(long)] json: bool,
    },
}

#[derive(Subcommand)]
enum SchemaCmd {
    /// Create a new schema (abstract principle)
    Create {
        content: String,
        #[arg(long, value_delimiter = ',')] tags:    Vec<String>,
        #[arg(long, value_delimiter = ',')] sources: Vec<String>,
        #[arg(long)] json: bool,
    },
    /// List schemas
    List {
        #[arg(long)] json: bool,
    },
}

#[derive(Subcommand)]
enum ProcedureCmd {
    /// Add a procedural memory
    Add {
        content: String,
        #[arg(long, value_delimiter = ',')] tags: Vec<String>,
        #[arg(long)] json: bool,
    },
    /// List procedures
    List {
        #[arg(long)] json: bool,
    },
    /// Record an outcome for a procedure
    Outcome {
        id:      String,
        outcome: String,
    },
}

#[derive(Subcommand)]
enum DreamCmd {
    /// Run a consolidation cycle
    Run {
        #[arg(long, default_value = "20")] max_llm_calls: usize,
        #[arg(long)] json: bool,
    },
    /// Show the last dream report
    Status {
        #[arg(long)] json: bool,
    },
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn scope(agent: Option<&str>) -> VisibilityScope {
    match agent {
        Some(a) if !a.is_empty() => VisibilityScope::for_agent(AgentId(a.to_string())),
        _ => VisibilityScope::global(),
    }
}

fn out(v: &Value, as_json: bool) {
    if as_json {
        println!("{}", serde_json::to_string_pretty(v).unwrap_or_default());
    } else {
        match v {
            Value::String(s) => println!("{s}"),
            Value::Array(a)  => a.iter().for_each(|x| out(x, false)),
            Value::Object(_) => {
                for (k, val) in v.as_object().unwrap() {
                    let display = match val {
                        Value::String(s)  => s.clone(),
                        Value::Number(n)  => n.to_string(),
                        Value::Bool(b)    => b.to_string(),
                        Value::Null       => "-".into(),
                        other             => serde_json::to_string(other).unwrap_or_default(),
                    };
                    println!("  {k:<22} {display}");
                }
            }
            other => println!("{other}"),
        }
    }
}

fn parse_link_type(s: &str) -> LinkType {
    match s {
        "causal"      => LinkType::Causal,
        "temporal"    => LinkType::Temporal,
        "supports"    => LinkType::Supports,
        "contradicts" => LinkType::Contradicts,
        "affective"   => LinkType::Affective,
        "contextual"  => LinkType::Contextual,
        "derived_from"=> LinkType::DerivedFrom,
        "part_of"     => LinkType::PartOf,
        _             => LinkType::Semantic,
    }
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();

    let cli    = Cli::parse();
    let config = cerebro::config::Config::from_env()?;
    let brain  = Arc::new(CerebroCortex::new(config).await?);
    let ag     = cli.agent.as_deref();

    match cli.command {
        // -----------------------------------------------------------------
        Command::Stats { json } => {
            let v = brain.storage.read().await.sqlite.memory_stats().await?;
            out(&v, json);
        }

        // -----------------------------------------------------------------
        Command::Remember { content, memory_type, tags, salience, json } => {
            let mt: Option<MemoryType> =
                serde_json::from_value(Value::String(memory_type)).ok();
            let t  = if tags.is_empty() { None } else { Some(tags) };
            let s  = salience.map(|f| f as f32);
            let node = brain.remember(content, mt, t, s, scope(ag)).await?;
            if json {
                out(&serde_json::to_value(&node)?, true);
            } else {
                println!("stored  {}", node.id.0);
                println!("type    {:?}", node.memory_type);
                println!("salience {:.3}", node.salience);
            }
        }

        // -----------------------------------------------------------------
        Command::Recall { query, top, json } => {
            let results = brain.recall(&query, top, scope(ag)).await?;
            if results.is_empty() {
                eprintln!("no results");
                return Ok(());
            }
            if json {
                let arr: Vec<Value> = results.iter()
                    .map(|(n, s)| json!({ "score": s, "memory": n }))
                    .collect();
                out(&Value::Array(arr), true);
            } else {
                for (node, score) in &results {
                    println!("[{score:.3}] {} — {}", node.id.0, node.content);
                }
            }
        }

        // -----------------------------------------------------------------
        Command::Get { id, json } => {
            let mid  = MemoryId(id.clone());
            let node = brain.storage.read().await.sqlite
                .get_memory(&mid, &scope(ag)).await?;
            match node {
                None    => eprintln!("not found: {id}"),
                Some(n) => out(&serde_json::to_value(&n)?, json),
            }
        }

        // -----------------------------------------------------------------
        Command::Delete { id, force } => {
            let mid = MemoryId(id.clone());
            if force {
                let ok = brain.storage.read().await.sqlite.purge_memory(&mid).await?;
                if ok { println!("purged {id}"); } else { eprintln!("not found: {id}"); }
            } else {
                let ok = brain.storage.read().await.sqlite.delete_memory(&mid).await?;
                if ok { println!("deleted {id}"); } else { eprintln!("not found: {id}"); }
            }
        }

        // -----------------------------------------------------------------
        Command::Update { id, content, tags, salience, json } => {
            let mid   = MemoryId(id.clone());
            let storage = brain.storage.read().await;
            let mut node = storage.sqlite.get_memory(&mid, &scope(ag)).await?
                .ok_or_else(|| anyhow::anyhow!("not found: {id}"))?;
            if let Some(c) = content { node.content = c; }
            if !tags.is_empty()      { node.tags    = tags; }
            if let Some(s) = salience { node.salience = s as f32; }
            storage.sqlite.update_memory(&node).await?;
            if json {
                out(&serde_json::to_value(&node)?, true);
            } else {
                println!("updated {id}");
            }
        }

        // -----------------------------------------------------------------
        Command::Associate { source_id, target_id, link_type, weight } => {
            let src = MemoryId(source_id.clone());
            let tgt = MemoryId(target_id.clone());
            let link = AssociativeLink {
                source_id:       src.clone(),
                target_id:       tgt.clone(),
                link_type:       parse_link_type(&link_type),
                weight:          weight as f32,
                created_at:      Utc::now(),
                last_traversed:  None,
                traversal_count: 0,
            };
            brain.associate(src, tgt, link).await?;
            println!("linked {} → {}", source_id, target_id);
        }

        // -----------------------------------------------------------------
        Command::Health { json } => {
            let v = brain.storage.read().await.sqlite
                .memory_health(&scope(ag)).await?;
            out(&v, json);
        }

        // -----------------------------------------------------------------
        Command::Emotions { json } => {
            let v = brain.storage.read().await.sqlite
                .emotional_summary(&scope(ag)).await?;
            out(&v, json);
        }

        // -----------------------------------------------------------------
        Command::Episode { cmd } => match cmd {
            EpisodeCmd::Start { title, json } => {
                let ep_id = format!("ep_{}", uuid::Uuid::new_v4().simple());
                brain.storage.read().await.sqlite
                    .create_episode(&ep_id, Some(&title), ag, None).await?;
                if json {
                    out(&json!({ "id": ep_id, "title": title }), true);
                } else {
                    println!("episode {ep_id}");
                }
            }
            EpisodeCmd::Step { episode_id, memory_id, role } => {
                let step_index = {
                    let ids = brain.storage.read().await.sqlite
                        .get_episode_memory_ids(&episode_id).await?;
                    ids.len() as i64
                };
                brain.storage.read().await.sqlite
                    .add_episode_step(&episode_id, step_index, &role, Some(&memory_id)).await?;
                println!("step added to {episode_id}");
            }
            EpisodeCmd::End { episode_id, summary } => {
                let ok = brain.storage.read().await.sqlite
                    .end_episode(&episode_id, summary.as_deref()).await?;
                if ok { println!("ended {episode_id}"); } else { eprintln!("not found: {episode_id}"); }
            }
            EpisodeCmd::List { limit, json } => {
                let eps = brain.storage.read().await.sqlite
                    .list_episodes(ag, limit).await?;
                if json {
                    out(&Value::Array(eps), true);
                } else {
                    for ep in &eps {
                        let id    = ep["id"].as_str().unwrap_or("?");
                        let title = ep["title"].as_str().unwrap_or("?");
                        println!("  {id}  {title}");
                    }
                }
            }
            EpisodeCmd::Get { episode_id, json } => {
                let ep = brain.storage.read().await.sqlite
                    .get_episode_raw(&episode_id).await?;
                match ep {
                    None    => eprintln!("not found: {episode_id}"),
                    Some(v) => out(&v, json),
                }
            }
        },

        // -----------------------------------------------------------------
        Command::Session { cmd } => match cmd {
            SessionCmd::Save { summary, priority, session_type, json } => {
                let tags = vec![
                    "session_note".to_string(),
                    format!("priority:{priority}"),
                    format!("session_type:{session_type}"),
                ];
                let node = brain.remember(
                    summary, Some(MemoryType::Episodic), Some(tags), Some(0.8), scope(ag),
                ).await?;
                if json {
                    out(&serde_json::to_value(&node)?, true);
                } else {
                    println!("session note {}", node.id.0);
                }
            }
            SessionCmd::Recall { query, top, json } => {
                let results = brain.recall(&query, top * 5, scope(ag)).await?;
                let filtered: Vec<_> = results.into_iter()
                    .filter(|(n, _)| n.tags.iter().any(|t| t == "session_note"))
                    .take(top)
                    .collect();
                if json {
                    let arr: Vec<Value> = filtered.iter()
                        .map(|(n, s)| json!({ "score": s, "memory": n }))
                        .collect();
                    out(&Value::Array(arr), true);
                } else {
                    for (node, score) in &filtered {
                        println!("[{score:.3}] {} — {}", node.id.0, node.content);
                    }
                }
            }
        },

        // -----------------------------------------------------------------
        Command::Agents { cmd } => match cmd {
            AgentsCmd::List { json } => {
                let agents = brain.storage.read().await.sqlite.list_agents().await?;
                if json {
                    out(&Value::Array(agents), true);
                } else {
                    for a in &agents {
                        let id   = a["agent_id"].as_str().unwrap_or("?");
                        let name = a["display_name"].as_str().unwrap_or("?");
                        let sym  = a["symbol"].as_str().unwrap_or("");
                        println!("  {sym} {id:<20} {name}");
                    }
                }
            }
            AgentsCmd::Register { agent_id, display_name, symbol, color, json } => {
                let metadata = serde_json::json!({ "symbol": symbol, "color": color });
                brain.storage.read().await.sqlite.register_agent(
                    &agent_id, &display_name, None, &metadata,
                ).await?;
                if json {
                    out(&serde_json::json!({ "agent_id": agent_id, "display_name": display_name }), true);
                } else {
                    println!("registered {agent_id}");
                }
            }
        },

        // -----------------------------------------------------------------
        Command::Intention { cmd } => match cmd {
            IntentionCmd::Add { content, tags, json } => {
                let mut t = vec!["intention".to_string()];
                t.extend(tags);
                let node = brain.remember(
                    content, Some(MemoryType::Prospective), Some(t), Some(0.7), scope(ag),
                ).await?;
                if json {
                    out(&serde_json::to_value(&node)?, true);
                } else {
                    println!("intention {}", node.id.0);
                }
            }
            IntentionCmd::List { json } => {
                let nodes = brain.storage.read().await.sqlite
                    .list_memories_scoped(&scope(ag), &ListFilter {
                        memory_type: Some(MemoryType::Prospective),
                        limit: 100,
                        ..Default::default()
                    })
                    .await?;
                let active: Vec<_> = nodes.into_iter()
                    .filter(|n| !n.tags.iter().any(|t| t == "status:resolved"))
                    .collect();
                if json {
                    let arr: Vec<Value> = active.iter()
                        .map(|n| serde_json::to_value(n).unwrap_or_default())
                        .collect();
                    out(&Value::Array(arr), true);
                } else {
                    for n in &active {
                        println!("  {} — {}", n.id.0, n.content);
                    }
                }
            }
            IntentionCmd::Resolve { id } => {
                let mid  = MemoryId(id.clone());
                let storage = brain.storage.read().await;
                if let Some(mut node) = storage.sqlite.get_memory(&mid, &scope(ag)).await? {
                    node.tags.retain(|t| !t.starts_with("status:"));
                    node.tags.push("status:resolved".into());
                    node.salience = 0.1;
                    storage.sqlite.update_memory(&node).await?;
                    println!("resolved {id}");
                } else {
                    eprintln!("not found: {id}");
                }
            }
        },

        // -----------------------------------------------------------------
        Command::Graph { cmd } => match cmd {
            GraphCmd::Path { source_id, target_id, json } => {
                let storage = brain.storage.read().await;
                let path = brain.association.find_path(
                    &storage.graph,
                    &MemoryId(source_id.clone()),
                    &MemoryId(target_id.clone()),
                );
                let ids: Vec<Value> = path.as_ref()
                    .map(|p| p.iter().map(|id| serde_json::json!(id.0)).collect())
                    .unwrap_or_default();
                if json {
                    out(&Value::Array(ids), true);
                } else if path.is_none() {
                    println!("no path found");
                } else {
                    println!("{}", ids.iter()
                        .map(|v| v.as_str().unwrap_or("?"))
                        .collect::<Vec<_>>()
                        .join(" → "));
                }
            }
            GraphCmd::Common { id_a, id_b, json } => {
                let storage = brain.storage.read().await;
                let common = brain.association.get_common_neighbors(
                    &storage.graph,
                    &MemoryId(id_a),
                    &MemoryId(id_b),
                );
                let ids: Vec<Value> = common.iter().map(|id| serde_json::json!(id.0)).collect();
                if json {
                    out(&Value::Array(ids), true);
                } else if ids.is_empty() {
                    println!("no common neighbors");
                } else {
                    for id in &ids { println!("  {}", id.as_str().unwrap_or("?")); }
                }
            }
            GraphCmd::Stats { json } => {
                let storage = brain.storage.read().await;
                let links   = storage.sqlite.list_all_links().await?;
                let ids     = storage.sqlite.list_all_memory_ids().await?;
                let v = json!({ "nodes": ids.len(), "edges": links.len() });
                out(&v, json);
            }
        },

        // -----------------------------------------------------------------
        Command::Schema { cmd } => match cmd {
            SchemaCmd::Create { content, tags, sources, json } => {
                let mut t = vec!["schema".to_string(), "support_count:0".to_string()];
                t.extend(tags);
                let node = brain.remember(
                    content, Some(MemoryType::Schematic), Some(t), Some(0.7), scope(ag),
                ).await?;
                if !sources.is_empty() {
                    let mut n = node.clone();
                    if let serde_json::Value::Object(ref mut map) = n.metadata {
                        map.insert("derived_from".into(), json!(sources));
                    } else {
                        n.metadata = json!({ "derived_from": sources });
                    }
                    brain.storage.read().await.sqlite.update_memory(&n).await?;
                }
                if json {
                    out(&serde_json::to_value(&node)?, true);
                } else {
                    println!("schema {}", node.id.0);
                }
            }
            SchemaCmd::List { json } => {
                let nodes = brain.storage.read().await.sqlite
                    .list_memories_scoped(&scope(ag), &ListFilter {
                        memory_type: Some(MemoryType::Schematic),
                        limit: 100,
                        ..Default::default()
                    })
                    .await?;
                if json {
                    let arr: Vec<Value> = nodes.iter()
                        .map(|n| serde_json::to_value(n).unwrap_or_default())
                        .collect();
                    out(&Value::Array(arr), true);
                } else {
                    for n in &nodes {
                        println!("  {} — {}", n.id.0, n.content);
                    }
                }
            }
        },

        // -----------------------------------------------------------------
        Command::Procedure { cmd } => match cmd {
            ProcedureCmd::Add { content, tags, json } => {
                let mut t = vec!["procedure".to_string()];
                t.extend(tags);
                let node = brain.remember(
                    content, Some(MemoryType::Procedural), Some(t), Some(0.8), scope(ag),
                ).await?;
                if json {
                    out(&serde_json::to_value(&node)?, true);
                } else {
                    println!("procedure {}", node.id.0);
                }
            }
            ProcedureCmd::List { json } => {
                let nodes = brain.storage.read().await.sqlite
                    .list_memories_scoped(&scope(ag), &ListFilter {
                        memory_type: Some(MemoryType::Procedural),
                        limit: 100,
                        ..Default::default()
                    })
                    .await?;
                if json {
                    let arr: Vec<Value> = nodes.iter()
                        .map(|n| serde_json::to_value(n).unwrap_or_default())
                        .collect();
                    out(&Value::Array(arr), true);
                } else {
                    for n in &nodes {
                        println!("  {} — {}", n.id.0, n.content);
                    }
                }
            }
            ProcedureCmd::Outcome { id, outcome } => {
                let mid  = MemoryId(id.clone());
                let storage = brain.storage.read().await;
                if let Some(mut node) = storage.sqlite.get_memory(&mid, &scope(ag)).await? {
                    node.tags.retain(|t| !t.starts_with("outcome:"));
                    node.tags.push(format!("outcome:{outcome}"));
                    storage.sqlite.update_memory(&node).await?;
                    println!("outcome recorded for {id}");
                } else {
                    eprintln!("not found: {id}");
                }
            }
        },

        // -----------------------------------------------------------------
        Command::Dream { cmd } => match cmd {
            DreamCmd::Run { max_llm_calls, json } => {
                let report = brain.dream.run_cycle(
                    scope(ag), Arc::clone(&brain), max_llm_calls,
                ).await?;
                let v = serde_json::to_value(&report)?;
                if json {
                    out(&v, true);
                } else {
                    println!("dream cycle complete");
                    println!("  phases     {}", report.phases.len());
                    println!("  llm_calls  {}", report.total_llm_calls);
                    println!("  duration   {:.2}s", report.total_duration_secs);
                    println!("  success    {}", report.success);
                }
            }
            DreamCmd::Status { json } => {
                let v = brain.storage.read().await.sqlite
                    .get_last_dream_report().await?
                    .unwrap_or(json!({ "status": "no_cycles_run" }));
                out(&v, json);
            }
        },
    }

    Ok(())
}
