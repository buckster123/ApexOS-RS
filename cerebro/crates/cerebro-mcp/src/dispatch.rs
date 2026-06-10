use std::sync::Arc;

use cerebro::{
    models::AssociativeLink,
    storage::ListFilter,
    types::{AgentId, LinkType, MemoryId, MemoryType, VisibilityScope},
    CerebroCortex,
};
use serde_json::{json, Value};
use uuid::Uuid;

use crate::tools;

pub fn handle_initialize(req: &Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": req["id"],
        "result": {
            "protocolVersion": "2024-11-05",
            "capabilities": { "tools": {} },
            "serverInfo": {
                "name": "cerebro-mcp",
                "version": env!("CARGO_PKG_VERSION")
            }
        }
    })
}

pub fn tools_list(req: &Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": req["id"],
        "result": { "tools": tools::all_tool_schemas() }
    })
}

pub async fn dispatch_tool(msg: Value, brain: Arc<CerebroCortex>) -> Value {
    let id     = msg["id"].clone();
    let params = &msg["params"];
    let name   = params["name"].as_str().unwrap_or("");
    let args   = &params["arguments"];

    let result = route(name, args, brain).await;
    match result {
        Ok(v)  => json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": { "content": [{ "type": "text", "text": v.to_string() }] }
        }),
        Err(e) => json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": -32603, "message": e.to_string() }
        }),
    }
}

pub fn method_not_found(req: &Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": req["id"],
        "error": { "code": -32601, "message": "method not found" }
    })
}

// ---------------------------------------------------------------------------
// Tool routing
// ---------------------------------------------------------------------------

async fn route(name: &str, args: &Value, brain: Arc<CerebroCortex>) -> anyhow::Result<Value> {
    match name {
        "remember" => {
            let content = args["content"].as_str()
                .ok_or_else(|| anyhow::anyhow!("content is required"))?.to_string();
            let memory_type: Option<MemoryType> =
                serde_json::from_value(args["memory_type"].clone()).ok();
            let tags: Option<Vec<String>> = args["tags"].as_array().map(|arr| {
                arr.iter().filter_map(|v| v.as_str().map(String::from)).collect()
            });
            let salience = args["salience"].as_f64().map(|f| f as f32);
            let scope    = agent_scope(args);
            let node = brain.remember(content, memory_type, tags, salience, scope).await?;
            Ok(serde_json::to_value(&node)?)
        }

        "recall" => {
            let query = args["query"].as_str()
                .ok_or_else(|| anyhow::anyhow!("query is required"))?;
            let k     = args["top_k"].as_u64().unwrap_or(10) as usize;
            let scope = agent_scope(args);
            let results = brain.recall(query, k, scope).await?;
            let out: Vec<Value> = results.into_iter()
                .map(|(node, score)| json!({ "memory": node, "score": score }))
                .collect();
            Ok(json!(out))
        }

        "associate" => {
            let src = args["source_id"].as_str()
                .ok_or_else(|| anyhow::anyhow!("source_id is required"))?.to_string();
            let tgt = args["target_id"].as_str()
                .ok_or_else(|| anyhow::anyhow!("target_id is required"))?.to_string();
            let link_type: LinkType =
                serde_json::from_value(args["link_type"].clone()).unwrap_or(LinkType::Semantic);
            let weight = args["weight"].as_f64().unwrap_or(0.5) as f32;
            let link = AssociativeLink::new(
                MemoryId(src.clone()), MemoryId(tgt.clone()), link_type, weight,
            );
            brain.associate(MemoryId(src), MemoryId(tgt), link).await?;
            Ok(json!({ "status": "ok" }))
        }

        "get_memory" => {
            let id = args["memory_id"].as_str()
                .ok_or_else(|| anyhow::anyhow!("memory_id is required"))?;
            let scope = agent_scope(args);
            let node  = brain.storage.read().await
                .sqlite.get_memory(&MemoryId(id.to_string()), &scope).await?;
            match node {
                Some(n) => Ok(serde_json::to_value(&n)?),
                None    => Err(anyhow::anyhow!("memory not found: {id}")),
            }
        }

        "delete_memory" => {
            let id = args["memory_id"].as_str()
                .ok_or_else(|| anyhow::anyhow!("memory_id is required"))?;
            let deleted = brain.storage.read().await
                .sqlite.delete_memory(&MemoryId(id.to_string())).await?;
            Ok(json!({ "deleted": deleted }))
        }

        "update_memory" => {
            let id = args["memory_id"].as_str()
                .ok_or_else(|| anyhow::anyhow!("memory_id is required"))?;
            let scope = agent_scope(args);
            let storage = brain.storage.read().await;
            let mut node = storage.sqlite.get_memory(&MemoryId(id.to_string()), &scope).await?
                .ok_or_else(|| anyhow::anyhow!("memory not found: {id}"))?;
            drop(storage);

            let content_changed = args["content"].as_str().is_some();
            if let Some(c) = args["content"].as_str()  { node.content = c.to_string(); }
            if let Some(s) = args["salience"].as_f64()  { node.salience = s as f32; }
            if let Some(arr) = args["tags"].as_array() {
                node.tags = arr.iter().filter_map(|v| v.as_str().map(String::from)).collect();
            }

            let storage = brain.storage.read().await;
            storage.sqlite.update_memory(&node).await?;
            if content_changed {
                storage.vector.embed_and_store(&node.id, &node.content).await?;
            }
            Ok(serde_json::to_value(&node)?)
        }

        // Aliases — same underlying logic, different param names
        "memory_store" | "memory_search" => {
            if name == "memory_store" {
                let content = args["content"].as_str()
                    .ok_or_else(|| anyhow::anyhow!("content is required"))?.to_string();
                let scope = agent_scope(args);
                let node = brain.remember(content, None, None, None, scope).await?;
                Ok(serde_json::to_value(&node)?)
            } else {
                let query = args["query"].as_str()
                    .ok_or_else(|| anyhow::anyhow!("query is required"))?;
                let k     = args["top_k"].as_u64().unwrap_or(10) as usize;
                let scope = agent_scope(args);
                let results = brain.recall(query, k, scope).await?;
                let out: Vec<Value> = results.into_iter()
                    .map(|(node, score)| json!({ "memory": node, "score": score }))
                    .collect();
                Ok(json!(out))
            }
        }

        "memory_neighbors" => {
            let id    = args["memory_id"].as_str()
                .ok_or_else(|| anyhow::anyhow!("memory_id is required"))?;
            let scope = agent_scope(args);
            let storage = brain.storage.read().await;
            let neighbor_ids: Vec<MemoryId> = storage.graph
                .neighbors(&MemoryId(id.to_string()))
                .into_iter().map(|id| id.clone()).collect();
            let nodes = storage.sqlite.get_memories_by_ids(&neighbor_ids, &scope).await?;
            Ok(serde_json::to_value(&nodes)?)
        }

        "find_path" => {
            let src = args["source_id"].as_str()
                .ok_or_else(|| anyhow::anyhow!("source_id is required"))?;
            let tgt = args["target_id"].as_str()
                .ok_or_else(|| anyhow::anyhow!("target_id is required"))?;
            let storage = brain.storage.read().await;
            let path = brain.association.find_path(
                &storage.graph, &MemoryId(src.to_string()), &MemoryId(tgt.to_string()),
            );
            match path {
                Some(ids) => Ok(json!({ "found": true, "path": ids })),
                None      => Ok(json!({ "found": false, "path": [] })),
            }
        }

        "common_neighbors" => {
            let a = args["memory_id_a"].as_str()
                .ok_or_else(|| anyhow::anyhow!("memory_id_a is required"))?;
            let b = args["memory_id_b"].as_str()
                .ok_or_else(|| anyhow::anyhow!("memory_id_b is required"))?;
            let scope   = agent_scope(args);
            let storage = brain.storage.read().await;
            let common  = brain.association.get_common_neighbors(
                &storage.graph, &MemoryId(a.to_string()), &MemoryId(b.to_string()),
            );
            let ids: Vec<MemoryId> = common;
            let nodes = storage.sqlite.get_memories_by_ids(&ids, &scope).await?;
            Ok(serde_json::to_value(&nodes)?)
        }

        "cortex_stats" => {
            let stats = brain.storage.read().await.sqlite.memory_stats().await?;
            Ok(stats)
        }

        "memory_graph_stats" => {
            let storage = brain.storage.read().await;
            Ok(json!({
                "node_count": storage.graph.graph.node_count(),
                "edge_count": storage.graph.graph.edge_count(),
            }))
        }

        // ------------------------------------------------------------------ //
        // Session save / recall (FORGE-critical: tag-convention over episodic)
        // ------------------------------------------------------------------ //

        "session_save" => {
            let content = args["content"].as_str()
                .ok_or_else(|| anyhow::anyhow!("content is required"))?.to_string();
            let priority     = args["priority"].as_str().unwrap_or("medium");
            let session_type = args["session_type"].as_str().unwrap_or("general");
            let scope        = agent_scope(args);
            let mut tags = vec![
                "session_note".to_string(),
                format!("priority:{priority}"),
                format!("session_type:{session_type}"),
            ];
            if let Some(aid) = args["agent_id"].as_str().filter(|s| !s.is_empty()) {
                tags.push(format!("agent:{aid}"));
            }
            let node = brain.remember(
                content,
                Some(MemoryType::Episodic),
                Some(tags),
                args["salience"].as_f64().map(|f| f as f32),
                scope,
            ).await?;
            Ok(serde_json::to_value(&node)?)
        }

        "session_recall" => {
            let query  = args["query"].as_str()
                .ok_or_else(|| anyhow::anyhow!("query is required"))?;
            let k               = args["top_k"].as_u64().unwrap_or(10) as usize;
            let priority_filter = args["priority"].as_str();
            let type_filter     = args["session_type"].as_str();
            let scope           = agent_scope(args);
            // Over-fetch so filtering doesn't deplete results
            let results = brain.recall(query, k * 5, scope).await?;
            let out: Vec<Value> = results.into_iter()
                .filter(|(n, _)| n.tags.iter().any(|t| t == "session_note"))
                .filter(|(n, _)| priority_filter.map_or(true, |p|
                    n.tags.iter().any(|t| t == &format!("priority:{p}"))))
                .filter(|(n, _)| type_filter.map_or(true, |st|
                    n.tags.iter().any(|t| t == &format!("session_type:{st}"))))
                .take(k)
                .map(|(node, score)| json!({ "memory": node, "score": score }))
                .collect();
            Ok(json!(out))
        }

        // ------------------------------------------------------------------ //
        // CRUD — deleted-memory lifecycle
        // ------------------------------------------------------------------ //

        "list_deleted" => {
            let scope = agent_scope(args);
            let limit = args["limit"].as_u64().unwrap_or(50) as usize;
            let nodes = brain.storage.read().await
                .sqlite.list_deleted_memories(&scope, limit).await?;
            Ok(serde_json::to_value(&nodes)?)
        }

        "restore_memory" => {
            let id = args["memory_id"].as_str()
                .ok_or_else(|| anyhow::anyhow!("memory_id is required"))?;
            let restored = brain.storage.read().await
                .sqlite.restore_memory(&MemoryId(id.to_string())).await?;
            Ok(json!({ "restored": restored }))
        }

        "purge_memory" => {
            let id = args["memory_id"].as_str()
                .ok_or_else(|| anyhow::anyhow!("memory_id is required"))?;
            let purged = brain.storage.read().await
                .sqlite.purge_memory(&MemoryId(id.to_string())).await?;
            Ok(json!({ "purged": purged }))
        }

        "purge_all_deleted" => {
            let count = brain.storage.read().await
                .sqlite.purge_all_deleted().await?;
            Ok(json!({ "purged_count": count }))
        }

        "bulk_delete" => {
            let ids: Vec<MemoryId> = args["memory_ids"].as_array()
                .ok_or_else(|| anyhow::anyhow!("memory_ids (array) is required"))?
                .iter()
                .filter_map(|v| v.as_str().map(|s| MemoryId(s.to_string())))
                .collect();
            let count = brain.storage.read().await
                .sqlite.bulk_delete(&ids).await?;
            Ok(json!({ "deleted_count": count }))
        }

        "export_memories" => {
            let scope       = agent_scope(args);
            let memory_type = serde_json::from_value(args["memory_type"].clone()).ok();
            let limit       = args["limit"].as_u64().unwrap_or(1000) as usize;
            let nodes = brain.storage.read().await
                .sqlite.list_memories_scoped(&scope, &ListFilter {
                    memory_type,
                    limit,
                    ..Default::default()
                }).await?;
            Ok(serde_json::to_value(&nodes)?)
        }

        // ------------------------------------------------------------------ //
        // Agent registry
        // ------------------------------------------------------------------ //

        "register_agent" => {
            let id = args["agent_id"].as_str()
                .map(|s| s.to_string())
                .unwrap_or_else(|| Uuid::new_v4().to_string());
            let name = args["name"].as_str()
                .ok_or_else(|| anyhow::anyhow!("name is required"))?;
            let description = args["description"].as_str();
            let metadata = if args["metadata"].is_null() { json!(null) } else { args["metadata"].clone() };
            brain.storage.read().await
                .sqlite.register_agent(&id, name, description, &metadata).await?;
            Ok(json!({ "agent_id": id, "status": "registered" }))
        }

        "list_agents" => {
            let agents = brain.storage.read().await.sqlite.list_agents().await?;
            Ok(json!(agents))
        }

        "share_memory" => {
            let id      = args["memory_id"].as_str()
                .ok_or_else(|| anyhow::anyhow!("memory_id is required"))?;
            let target  = args["target_agent_id"].as_str();
            let updated = brain.storage.read().await
                .sqlite.share_memory(&MemoryId(id.to_string()), target).await?;
            Ok(json!({ "updated": updated }))
        }

        // ------------------------------------------------------------------ //
        // Messaging (tag-routed: "to:{agent}", "from:{agent}")
        // ------------------------------------------------------------------ //

        "send_message" => {
            let content  = args["content"].as_str()
                .ok_or_else(|| anyhow::anyhow!("content is required"))?.to_string();
            let to_agent = args["to_agent_id"].as_str()
                .ok_or_else(|| anyhow::anyhow!("to_agent_id is required"))?;
            let from_agent = args["from_agent_id"].as_str()
                .or_else(|| args["agent_id"].as_str())
                .unwrap_or("unknown");
            let thread_id = args["thread_id"].as_str().map(str::to_string);
            let scope     = agent_scope(args);
            let tags = vec![
                "message".to_string(),
                format!("to:{to_agent}"),
                format!("from:{from_agent}"),
            ];
            let mut node = brain.remember(
                content, Some(MemoryType::Affective), Some(tags), None, scope,
            ).await?;
            if let Some(tid) = thread_id {
                node.thread_id = Some(tid);
                brain.storage.read().await.sqlite.update_memory(&node).await?;
            }
            Ok(serde_json::to_value(&node)?)
        }

        "check_inbox" => {
            let agent_id = args["agent_id"].as_str()
                .ok_or_else(|| anyhow::anyhow!("agent_id is required"))?;
            let limit = args["limit"].as_u64().unwrap_or(20) as usize;
            let nodes = brain.storage.read().await
                .sqlite.check_inbox(agent_id, &VisibilityScope::global(), limit).await?;
            Ok(serde_json::to_value(&nodes)?)
        }

        // ------------------------------------------------------------------ //
        // Thread operations
        // ------------------------------------------------------------------ //

        "list_threads" => {
            let scope   = agent_scope(args);
            let threads = brain.storage.read().await.sqlite.list_threads(&scope).await?;
            Ok(json!(threads))
        }

        "get_thread_memories" => {
            let thread_id = args["thread_id"].as_str()
                .ok_or_else(|| anyhow::anyhow!("thread_id is required"))?;
            let scope = agent_scope(args);
            let nodes = brain.storage.read().await
                .sqlite.get_thread_memories(thread_id, &scope).await?;
            Ok(serde_json::to_value(&nodes)?)
        }

        "prune_thread" => {
            let thread_id = args["thread_id"].as_str()
                .ok_or_else(|| anyhow::anyhow!("thread_id is required"))?;
            let count = brain.storage.read().await.sqlite.prune_thread(thread_id).await?;
            Ok(json!({ "pruned_count": count }))
        }

        // ------------------------------------------------------------------ //
        // Tag operations
        // ------------------------------------------------------------------ //

        "list_tags" => {
            let scope = agent_scope(args);
            let tags  = brain.storage.read().await.sqlite.list_tags(&scope).await?;
            Ok(json!(tags))
        }

        "delete_tag" => {
            let tag   = args["tag"].as_str()
                .ok_or_else(|| anyhow::anyhow!("tag is required"))?;
            let count = brain.storage.read().await
                .sqlite.delete_tag_everywhere(tag).await?;
            Ok(json!({ "updated_memories": count }))
        }

        "rename_tag" => {
            let old_tag = args["old_tag"].as_str()
                .ok_or_else(|| anyhow::anyhow!("old_tag is required"))?;
            let new_tag = args["new_tag"].as_str()
                .ok_or_else(|| anyhow::anyhow!("new_tag is required"))?;
            let count = brain.storage.read().await
                .sqlite.rename_tag_everywhere(old_tag, new_tag).await?;
            Ok(json!({ "updated_memories": count }))
        }

        "merge_tags" => {
            // merge source_tag into target_tag = rename source → target everywhere
            let source_tag = args["source_tag"].as_str()
                .ok_or_else(|| anyhow::anyhow!("source_tag is required"))?;
            let target_tag = args["target_tag"].as_str()
                .ok_or_else(|| anyhow::anyhow!("target_tag is required"))?;
            let count = brain.storage.read().await
                .sqlite.rename_tag_everywhere(source_tag, target_tag).await?;
            Ok(json!({ "merged_memories": count, "merged_into": target_tag }))
        }

        // ------------------------------------------------------------------ //
        // Analytics
        // ------------------------------------------------------------------ //

        "emotional_summary" => {
            let scope = agent_scope(args);
            let summary = brain.storage.read().await.sqlite.emotional_summary(&scope).await?;
            Ok(summary)
        }

        "activation_at_risk" => {
            let threshold = args["threshold"].as_f64().unwrap_or(0.7) as f32;
            let limit     = args["limit"].as_u64().unwrap_or(20) as usize;
            let scope     = agent_scope(args);
            let at_risk   = brain.storage.read().await
                .sqlite.activation_at_risk(&scope, threshold, limit).await?;
            Ok(json!(at_risk))
        }

        "memory_health" => {
            let scope  = agent_scope(args);
            let health = brain.storage.read().await.sqlite.memory_health(&scope).await?;
            Ok(health)
        }

        "activation_curve" => {
            let id    = args["memory_id"].as_str()
                .ok_or_else(|| anyhow::anyhow!("memory_id is required"))?;
            let scope = agent_scope(args);
            let node  = brain.storage.read().await
                .sqlite.get_memory(&MemoryId(id.to_string()), &scope).await?
                .ok_or_else(|| anyhow::anyhow!("memory not found: {id}"))?;
            Ok(json!({
                "memory_id":        id,
                "access_count":     node.access_count,
                "access_times":     node.access_times,
                "fsrs_stability":   node.strength.stability,
                "fsrs_difficulty":  node.strength.difficulty,
                "fsrs_last_review": node.strength.last_review,
            }))
        }

        "activation_heatmap" => {
            let scope   = agent_scope(args);
            let heatmap = brain.storage.read().await
                .sqlite.activation_heatmap(&scope).await?;
            Ok(heatmap)
        }

        "check_near_duplicates" => {
            let threshold = args["threshold"].as_f64().unwrap_or(0.9) as f32;
            let limit     = args["limit"].as_u64().unwrap_or(50) as usize;
            let scope     = agent_scope(args);
            let (scope_sql, scope_params) = scope.sql_filter();

            let storage    = brain.storage.read().await;
            let candidates = storage.sqlite.list_memories_scoped(
                &scope, &ListFilter { limit, ..Default::default() },
            ).await?;

            let mut pairs: Vec<Value> = Vec::new();
            let mut seen: std::collections::HashSet<String> = Default::default();

            for node in &candidates {
                let results = storage.vector
                    .search(&node.content, 5, scope_sql, &scope_params).await?;
                for (result_id, sim) in results {
                    if result_id != node.id && sim >= threshold {
                        let (a, b) = if node.id.0 < result_id.0 {
                            (node.id.0.clone(), result_id.0.clone())
                        } else {
                            (result_id.0.clone(), node.id.0.clone())
                        };
                        if seen.insert(format!("{a}:{b}")) {
                            pairs.push(json!({
                                "memory_id_a": a, "memory_id_b": b, "similarity": sim,
                            }));
                        }
                    }
                }
            }
            Ok(json!({ "duplicates": pairs, "threshold": threshold }))
        }

        // ------------------------------------------------------------------ //
        // Episodes (tables already in schema)
        // ------------------------------------------------------------------ //

        "episode_start" => {
            let ep_id     = format!("ep_{}", Uuid::new_v4().simple());
            let title     = args["title"].as_str();
            let agent_id  = args["agent_id"].as_str();
            let thread_id = args["thread_id"].as_str();
            brain.storage.read().await
                .sqlite.create_episode(&ep_id, title, agent_id, thread_id).await?;
            Ok(json!({ "episode_id": ep_id, "status": "started" }))
        }

        "episode_add_step" => {
            let episode_id  = args["episode_id"].as_str()
                .ok_or_else(|| anyhow::anyhow!("episode_id is required"))?;
            let step_index  = args["step_index"].as_i64().unwrap_or(0);
            let description = args["description"].as_str()
                .ok_or_else(|| anyhow::anyhow!("description is required"))?;
            let memory_id   = args["memory_id"].as_str();
            brain.storage.read().await
                .sqlite.add_episode_step(episode_id, step_index, description, memory_id).await?;
            Ok(json!({ "status": "ok", "episode_id": episode_id, "step_index": step_index }))
        }

        "episode_end" => {
            let episode_id = args["episode_id"].as_str()
                .ok_or_else(|| anyhow::anyhow!("episode_id is required"))?;
            let summary = args["summary"].as_str();
            let ended   = brain.storage.read().await
                .sqlite.end_episode(episode_id, summary).await?;
            Ok(json!({ "ended": ended, "episode_id": episode_id }))
        }

        "get_episode" => {
            let episode_id = args["episode_id"].as_str()
                .ok_or_else(|| anyhow::anyhow!("episode_id is required"))?;
            let ep = brain.storage.read().await
                .sqlite.get_episode_raw(episode_id).await?;
            match ep {
                Some(v) => Ok(v),
                None    => Err(anyhow::anyhow!("episode not found: {episode_id}")),
            }
        }

        "list_episodes" => {
            let agent_id = args["agent_id"].as_str();
            let limit    = args["limit"].as_u64().unwrap_or(20) as usize;
            let episodes = brain.storage.read().await
                .sqlite.list_episodes(agent_id, limit).await?;
            Ok(json!(episodes))
        }

        "get_episode_memories" => {
            let episode_id = args["episode_id"].as_str()
                .ok_or_else(|| anyhow::anyhow!("episode_id is required"))?;
            let scope   = agent_scope(args);
            let storage = brain.storage.read().await;
            let ids     = storage.sqlite.get_episode_memory_ids(episode_id).await?;
            let nodes   = storage.sqlite.get_memories_by_ids(&ids, &scope).await?;
            Ok(serde_json::to_value(&nodes)?)
        }

        // ------------------------------------------------------------------ //
        // Audit log (table already in schema)
        // ------------------------------------------------------------------ //

        "audit_summary" => {
            let summary = brain.storage.read().await.sqlite.audit_summary().await?;
            Ok(summary)
        }

        "query_audit" => {
            let limit         = args["limit"].as_u64().unwrap_or(50) as usize;
            let agent_id_filt = args["agent_id"].as_str();
            let entries       = brain.storage.read().await
                .sqlite.query_audit(limit, agent_id_filt).await?;
            Ok(json!(entries))
        }

        // ------------------------------------------------------------------ //
        // Intentions — prospective memories (TODOs, reminders)
        // ------------------------------------------------------------------ //

        "store_intention" => {
            let content  = args["content"].as_str()
                .ok_or_else(|| anyhow::anyhow!("content required"))?;
            let salience = args["salience"].as_f64().unwrap_or(0.7) as f32;
            let scope    = agent_scope(args);
            let mut tags = vec!["intention".to_string()];
            if let Some(arr) = args["tags"].as_array() {
                tags.extend(arr.iter().filter_map(|v| v.as_str().map(String::from)));
            }
            let node = brain.remember(
                content, Some(MemoryType::Prospective), Some(tags), Some(salience), scope,
            ).await?;
            Ok(json!({ "id": node.id, "status": "ok", "salience": node.salience }))
        }

        "list_intentions" => {
            let min_salience = args["min_salience"].as_f64().unwrap_or(0.3) as f32;
            let limit        = args["limit"].as_u64().unwrap_or(50) as usize;
            let scope        = agent_scope(args);
            let filter       = ListFilter {
                memory_type: Some(MemoryType::Prospective),
                limit,
                ..Default::default()
            };
            let nodes = brain.storage.read().await.sqlite
                .list_memories_scoped(&scope, &filter).await?;
            let filtered: Vec<_> = nodes.into_iter()
                .filter(|n| n.salience >= min_salience)
                .collect();
            Ok(json!(filtered))
        }

        "resolve_intention" => {
            let memory_id = args["memory_id"].as_str()
                .ok_or_else(|| anyhow::anyhow!("memory_id required"))?;
            let scope   = agent_scope(args);
            let mid     = MemoryId(memory_id.to_string());
            let storage = brain.storage.read().await;
            let mut node = storage.sqlite.get_memory(&mid, &scope).await?
                .ok_or_else(|| anyhow::anyhow!("intention not found: {memory_id}"))?;
            node.salience = 0.1;
            node.tags.retain(|t| !t.starts_with("status:"));
            node.tags.push("status:resolved".to_string());
            storage.sqlite.update_memory(&node).await?;
            Ok(json!({ "status": "ok", "resolved": memory_id }))
        }

        // ------------------------------------------------------------------ //
        // Procedures — workflows, strategies, how-to guides
        // ------------------------------------------------------------------ //

        "store_procedure" => {
            let content  = args["content"].as_str()
                .ok_or_else(|| anyhow::anyhow!("content required"))?;
            let salience = args["salience"].as_f64().unwrap_or(0.8) as f32;
            let scope    = agent_scope(args);
            let mut tags = vec!["procedure".to_string()];
            if let Some(arr) = args["tags"].as_array() {
                tags.extend(arr.iter().filter_map(|v| v.as_str().map(String::from)));
            }
            let node = brain.remember(
                content, Some(MemoryType::Procedural), Some(tags), Some(salience), scope,
            ).await?;
            Ok(json!({ "id": node.id, "status": "ok" }))
        }

        "list_procedures" => {
            let min_salience = args["min_salience"].as_f64().unwrap_or(0.0) as f32;
            let limit        = args["limit"].as_u64().unwrap_or(50) as usize;
            let scope        = agent_scope(args);
            let filter       = ListFilter {
                memory_type: Some(MemoryType::Procedural),
                limit,
                ..Default::default()
            };
            let nodes = brain.storage.read().await.sqlite
                .list_memories_scoped(&scope, &filter).await?;
            let filtered: Vec<_> = nodes.into_iter()
                .filter(|n| n.salience >= min_salience)
                .collect();
            Ok(json!(filtered))
        }

        "find_relevant_procedures" => {
            let tags: Vec<String>     = args["tags"].as_array()
                .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
                .unwrap_or_default();
            let concepts: Vec<String> = args["concepts"].as_array()
                .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
                .unwrap_or_default();
            if tags.is_empty() && concepts.is_empty() {
                return Ok(json!([]));
            }
            let max_results = args["limit"].as_u64().unwrap_or(5) as usize;
            let scope       = agent_scope(args);
            let filter      = ListFilter {
                memory_type: Some(MemoryType::Procedural),
                limit: 200,
                ..Default::default()
            };
            let nodes = brain.storage.read().await.sqlite
                .list_memories_scoped(&scope, &filter).await?;
            let filtered: Vec<_> = nodes.into_iter()
                .filter(|n| {
                    let tag_hit = tags.iter().any(|t| n.tags.iter().any(|nt| nt == t));
                    let meta_str = n.metadata.to_string();
                    let concept_hit = concepts.iter().any(|c| meta_str.contains(c.as_str()));
                    tag_hit || concept_hit
                })
                .take(max_results)
                .collect();
            Ok(json!(filtered))
        }

        "record_procedure_outcome" => {
            let procedure_id = args["procedure_id"].as_str()
                .ok_or_else(|| anyhow::anyhow!("procedure_id required"))?;
            let success = args["success"].as_bool()
                .ok_or_else(|| anyhow::anyhow!("success (bool) required"))?;
            let scope   = agent_scope(args);
            let mid     = MemoryId(procedure_id.to_string());
            let storage = brain.storage.read().await;
            let mut node = storage.sqlite.get_memory(&mid, &scope).await?
                .ok_or_else(|| anyhow::anyhow!("procedure not found: {procedure_id}"))?;
            if success {
                node.salience = (node.salience + 0.1).min(1.0);
            } else {
                node.salience = (node.salience + 0.02).min(1.0);
                node.strength.difficulty = (node.strength.difficulty + 0.5).min(10.0);
            }
            storage.sqlite.update_memory(&node).await?;
            Ok(json!({
                "status":       "ok",
                "procedure_id": procedure_id,
                "success":      success,
                "new_salience": node.salience,
            }))
        }

        // ------------------------------------------------------------------ //
        // Schemas — abstract patterns derived from multiple memories
        // ------------------------------------------------------------------ //

        "create_schema" => {
            let content    = args["content"].as_str()
                .ok_or_else(|| anyhow::anyhow!("content required"))?;
            let source_ids: Vec<String> = args["source_ids"].as_array()
                .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
                .unwrap_or_default();
            let salience   = args["salience"].as_f64().unwrap_or(0.7) as f32;
            let scope      = agent_scope(args);
            let mut tags   = vec!["schema".to_string(), "support_count:0".to_string()];
            if let Some(arr) = args["tags"].as_array() {
                tags.extend(arr.iter().filter_map(|v| v.as_str().map(String::from)));
            }
            let mut node = brain.remember(
                content, Some(MemoryType::Schematic), Some(tags), Some(salience), scope,
            ).await?;
            if !source_ids.is_empty() {
                if let serde_json::Value::Object(ref mut map) = node.metadata {
                    map.insert("derived_from".to_string(), json!(source_ids));
                } else {
                    node.metadata = json!({ "derived_from": source_ids });
                }
                brain.storage.read().await.sqlite.update_memory(&node).await?;
            }
            Ok(json!({ "id": node.id, "status": "ok" }))
        }

        "list_schemas" => {
            let limit  = args["limit"].as_u64().unwrap_or(50) as usize;
            let scope  = agent_scope(args);
            let filter = ListFilter {
                memory_type: Some(MemoryType::Schematic),
                limit,
                ..Default::default()
            };
            let nodes = brain.storage.read().await.sqlite
                .list_memories_scoped(&scope, &filter).await?;
            Ok(json!(nodes))
        }

        "find_matching_schemas" => {
            let tags: Vec<String>     = args["tags"].as_array()
                .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
                .unwrap_or_default();
            let concepts: Vec<String> = args["concepts"].as_array()
                .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
                .unwrap_or_default();
            if tags.is_empty() && concepts.is_empty() {
                return Ok(json!([]));
            }
            let max_results = args["limit"].as_u64().unwrap_or(5) as usize;
            let scope       = agent_scope(args);
            let filter      = ListFilter {
                memory_type: Some(MemoryType::Schematic),
                limit: 200,
                ..Default::default()
            };
            let nodes = brain.storage.read().await.sqlite
                .list_memories_scoped(&scope, &filter).await?;
            let filtered: Vec<_> = nodes.into_iter()
                .filter(|n| {
                    let tag_hit = tags.iter().any(|t| n.tags.iter().any(|nt| nt == t));
                    let meta_str = n.metadata.to_string();
                    let concept_hit = concepts.iter().any(|c| meta_str.contains(c.as_str()));
                    tag_hit || concept_hit
                })
                .take(max_results)
                .collect();
            Ok(json!(filtered))
        }

        "get_schema_sources" => {
            let schema_id = args["schema_id"].as_str()
                .ok_or_else(|| anyhow::anyhow!("schema_id required"))?;
            let scope   = agent_scope(args);
            let mid     = MemoryId(schema_id.to_string());
            let storage = brain.storage.read().await;
            let node = storage.sqlite.get_memory(&mid, &scope).await?
                .ok_or_else(|| anyhow::anyhow!("schema not found: {schema_id}"))?;
            let sources: Vec<String> = node.metadata["derived_from"].as_array()
                .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
                .unwrap_or_default();
            Ok(json!({ "schema_id": schema_id, "source_ids": sources }))
        }

        // ------------------------------------------------------------------ //
        // Memory versions — content snapshots for undo / audit
        // ------------------------------------------------------------------ //

        "get_memory_versions" => {
            let memory_id = args["memory_id"].as_str()
                .ok_or_else(|| anyhow::anyhow!("memory_id required"))?;
            let limit    = args["limit"].as_u64().unwrap_or(10) as usize;
            let versions = brain.storage.read().await.sqlite
                .get_memory_versions_raw(memory_id, limit).await?;
            Ok(json!(versions))
        }

        "restore_version" => {
            let version_id = args["version_id"].as_i64()
                .ok_or_else(|| anyhow::anyhow!("version_id (integer) required"))?;
            let scope   = agent_scope(args);
            let storage = brain.storage.read().await;
            let ver = storage.sqlite.get_version_raw(version_id).await?
                .ok_or_else(|| anyhow::anyhow!("version {version_id} not found"))?;
            let memory_id_str = ver["memory_id"].as_str().unwrap_or("").to_string();
            let mid = MemoryId(memory_id_str.clone());
            let mut node = storage.sqlite.get_memory(&mid, &scope).await?
                .ok_or_else(|| anyhow::anyhow!("memory {memory_id_str} not found"))?;
            storage.sqlite.log_memory_version(
                &node, args["agent_id"].as_str(), Some("auto-snapshot before restore"),
            ).await?;
            node.content  = ver["content"].as_str().unwrap_or("").to_string();
            node.tags     = serde_json::from_str(ver["tags_json"].as_str().unwrap_or("[]"))
                .unwrap_or_default();
            node.salience = ver["salience"].as_f64().unwrap_or(0.5) as f32;
            storage.sqlite.update_memory(&node).await?;
            Ok(json!({
                "status":       "ok",
                "restored_to":  version_id,
                "memory_id":    memory_id_str,
            }))
        }

        "dream_run" => {
            let max_llm_calls = args["max_llm_calls"].as_u64().unwrap_or(20) as usize;
            let scope  = agent_scope(args);
            let report = brain.dream.run_cycle(scope, Arc::clone(&brain), max_llm_calls).await?;
            Ok(serde_json::to_value(&report)?)
        }

        "dream_status" => {
            let report = brain.storage.read().await.sqlite.get_last_dream_report().await?;
            Ok(report.unwrap_or(json!({ "status": "no_cycles_run" })))
        }

        _ => Ok(json!({ "status": "not_yet_implemented", "tool": name })),
    }
}

// ---------------------------------------------------------------------------
// Helper: build a VisibilityScope from an agent_id argument
// ---------------------------------------------------------------------------

fn agent_scope(args: &Value) -> VisibilityScope {
    match args["agent_id"].as_str() {
        Some(id) if !id.is_empty() => VisibilityScope::for_agent(AgentId(id.to_string())),
        _ => VisibilityScope::global(),
    }
}

// ---------------------------------------------------------------------------
// Tests — dispatch logic without stdio (no actual MCP session required)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use cerebro::config::Config;
    use tempfile::TempDir;

    async fn make_brain() -> (Arc<CerebroCortex>, TempDir) {
        let dir = TempDir::new().unwrap();
        let config = Config {
            db_path:       dir.path().join("test.db"),
            anthropic_key: None,
            embed_model:   "".into(),
        };
        let brain = Arc::new(CerebroCortex::new(config).await.unwrap());
        (brain, dir)
    }

    #[test]
    fn initialize_returns_capabilities_with_echoed_id() {
        let req  = json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}});
        let resp = handle_initialize(&req);
        assert_eq!(resp["id"], 1, "id must be echoed");
        assert!(resp["result"]["capabilities"]["tools"].is_object());
        assert_eq!(resp["result"]["protocolVersion"], "2024-11-05");
        assert_eq!(resp["result"]["serverInfo"]["name"], "cerebro-mcp");
    }

    #[test]
    fn tools_list_echoes_id_and_contains_63_tools() {
        let req  = json!({"jsonrpc":"2.0","id":42,"method":"tools/list","params":{}});
        let resp = tools_list(&req);
        assert_eq!(resp["id"], 42, "id must be echoed");
        let tools = resp["result"]["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 66);
        let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert!(names.contains(&"remember"));
        assert!(names.contains(&"recall"));
    }

    #[test]
    fn tools_list_remember_has_proper_schema() {
        let req  = json!({"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}});
        let resp = tools_list(&req);
        let tools = resp["result"]["tools"].as_array().unwrap();
        let remember = tools.iter().find(|t| t["name"] == "remember").unwrap();
        let schema = &remember["inputSchema"];
        assert_eq!(schema["type"], "object");
        assert!(schema["properties"]["content"].is_object(), "content property must exist");
        let required = schema["required"].as_array().unwrap();
        assert!(required.iter().any(|v| v == "content"), "content must be required");
    }

    #[tokio::test]
    async fn dispatch_remember_stores_and_returns_node() {
        let (brain, _dir) = make_brain().await;
        let msg = json!({
            "jsonrpc": "2.0", "id": 3, "method": "tools/call",
            "params": {
                "name": "remember",
                "arguments": { "content": "Rust is a memory-safe systems language" }
            }
        });
        let resp = dispatch_tool(msg, brain).await;
        assert!(resp["error"].is_null(), "unexpected error: {}", resp["error"]);
        let text = resp["result"]["content"][0]["text"].as_str().unwrap();
        let node: Value = serde_json::from_str(text).unwrap();
        assert!(node["id"].as_str().is_some_and(|s| !s.is_empty()));
        assert!(node["salience"].as_f64().is_some_and(|s| s > 0.0));
    }

    #[tokio::test]
    async fn dispatch_remember_rejects_short_content() {
        let (brain, _dir) = make_brain().await;
        let msg = json!({
            "jsonrpc":"2.0","id":4,"method":"tools/call",
            "params":{"name":"remember","arguments":{"content":"hi"}}
        });
        let resp = dispatch_tool(msg, brain).await;
        assert!(!resp["error"].is_null(), "short content should produce an error");
    }

    #[tokio::test]
    async fn dispatch_recall_returns_remembered_node_at_top() {
        let (brain, _dir) = make_brain().await;

        // Store first
        let store_msg = json!({
            "jsonrpc":"2.0","id":5,"method":"tools/call",
            "params":{"name":"remember","arguments":{
                "content": "sqlite fts5 full text search is the keyword fallback path"
            }}
        });
        let store_resp = dispatch_tool(store_msg, Arc::clone(&brain)).await;
        assert!(store_resp["error"].is_null());
        let text = store_resp["result"]["content"][0]["text"].as_str().unwrap();
        let stored: Value = serde_json::from_str(text).unwrap();
        let stored_id = stored["id"].as_str().unwrap();

        // Recall
        let recall_msg = json!({
            "jsonrpc":"2.0","id":6,"method":"tools/call",
            "params":{"name":"recall","arguments":{"query":"sqlite fts5 keyword search","top_k":5}}
        });
        let recall_resp = dispatch_tool(recall_msg, brain).await;
        assert!(recall_resp["error"].is_null());
        let text = recall_resp["result"]["content"][0]["text"].as_str().unwrap();
        let results: Vec<Value> = serde_json::from_str(text).unwrap();
        assert!(!results.is_empty(), "should return at least one result");
        assert_eq!(results[0]["memory"]["id"].as_str().unwrap(), stored_id,
            "stored memory should rank first");
    }

    #[tokio::test]
    async fn dispatch_associate_creates_link() {
        let (brain, _dir) = make_brain().await;

        let store = |content: &'static str, brain: Arc<CerebroCortex>| async move {
            let msg = json!({
                "jsonrpc":"2.0","id":0,"method":"tools/call",
                "params":{"name":"remember","arguments":{"content":content}}
            });
            let resp = dispatch_tool(msg, brain).await;
            let text = resp["result"]["content"][0]["text"].as_str().unwrap().to_string();
            let node: Value = serde_json::from_str(&text).unwrap();
            node["id"].as_str().unwrap().to_string()
        };

        let a_id = store("Rust ownership model prevents memory leaks at compile time", Arc::clone(&brain)).await;
        let b_id = store("C++ uses RAII for deterministic resource management patterns", Arc::clone(&brain)).await;

        let assoc_msg = json!({
            "jsonrpc":"2.0","id":7,"method":"tools/call",
            "params":{"name":"associate","arguments":{
                "source_id": a_id,
                "target_id": b_id,
                "link_type": "semantic",
                "weight": 0.8
            }}
        });
        let resp = dispatch_tool(assoc_msg, Arc::clone(&brain)).await;
        assert!(resp["error"].is_null(), "associate should not error: {}", resp["error"]);
        let text = resp["result"]["content"][0]["text"].as_str().unwrap();
        let result: Value = serde_json::from_str(text).unwrap();
        assert_eq!(result["status"], "ok");

        // Verify edge in graph
        let storage = brain.storage.read().await;
        let neighbors = storage.graph.neighbors(&MemoryId(a_id));
        assert_eq!(neighbors.len(), 1);
        assert_eq!(neighbors[0], &MemoryId(b_id));
    }

    #[tokio::test]
    async fn dispatch_dream_run_returns_report() {
        let (brain, _dir) = make_brain().await;
        let msg = json!({
            "jsonrpc":"2.0","id":8,"method":"tools/call",
            "params":{"name":"dream_run","arguments":{"max_llm_calls":0}}
        });
        let resp = dispatch_tool(msg, brain).await;
        assert!(resp["error"].is_null(), "dream_run should not produce a protocol error: {}", resp["error"]);
        let text = resp["result"]["content"][0]["text"].as_str().unwrap();
        let result: Value = serde_json::from_str(text).unwrap();
        // Report always has phases array (6 phases) and success field
        assert!(result["phases"].is_array(), "dream report should have phases: {result}");
        assert_eq!(result["phases"].as_array().unwrap().len(), 6);
        assert!(result["success"].is_boolean());
    }
}
