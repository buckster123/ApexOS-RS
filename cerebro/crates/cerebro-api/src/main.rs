use std::sync::Arc;

use anyhow::Result;
use axum::{
    extract::{Path, Query, Request, State},
    http::{header, StatusCode},
    middleware::Next,
    response::IntoResponse,
    routing::{delete, get, post},
    Json, Router,
};
use cerebro::{
    models::AssociativeLink,
    storage::ListFilter,
    types::{AgentId, LinkType, MemoryId, MemoryType, VisibilityScope},
    CerebroCortex,
};
use chrono::Utc;
use serde::Deserialize;
use serde_json::{json, Value};
use tracing::info;

// ---------------------------------------------------------------------------
// State alias
// ---------------------------------------------------------------------------
type Brain = Arc<CerebroCortex>;
type AppResult<T = Value> = Result<Json<T>, ApiError>;

/// Constant-time string equality for auth tokens. Guards on length first
/// (lengths are not secret), then compares bytes via `subtle::ConstantTimeEq`
/// so a mismatch does not leak the matching-prefix length through timing.
fn ct_eq(a: &str, b: &str) -> bool {
    use subtle::ConstantTimeEq;
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    a.ct_eq(b).into()
}

// ---------------------------------------------------------------------------
// Error helper — any anyhow error → 500 JSON
// ---------------------------------------------------------------------------
struct ApiError(anyhow::Error);

impl<E: Into<anyhow::Error>> From<E> for ApiError {
    fn from(e: E) -> Self { Self(e.into()) }
}

impl axum::response::IntoResponse for ApiError {
    fn into_response(self) -> axum::response::Response {
        let body = Json(json!({ "error": self.0.to_string() }));
        (StatusCode::INTERNAL_SERVER_ERROR, body).into_response()
    }
}

fn not_found(id: &str) -> ApiError {
    ApiError(anyhow::anyhow!("not found: {id}"))
}

/// CB-023: responder for `CatchPanicLayer`. Turns a caught handler panic into a
/// clean 500 JSON body shaped like `ApiError` (instead of an aborted connection
/// with no response), mirroring the MCP sibling's per-call panic isolation.
fn panic_response(err: Box<dyn std::any::Any + Send + 'static>) -> axum::response::Response {
    let msg = err
        .downcast_ref::<&str>().map(|s| s.to_string())
        .or_else(|| err.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "handler panicked".to_string());
    tracing::error!("cerebro-api: caught handler panic: {msg}");
    let body = Json(json!({ "error": format!("internal panic: {msg}") }));
    (StatusCode::INTERNAL_SERVER_ERROR, body).into_response()
}

fn scope_from(agent_id: Option<&str>) -> VisibilityScope {
    match agent_id {
        Some(a) if !a.is_empty() => VisibilityScope::for_agent(AgentId(a.to_string())),
        _ => VisibilityScope::global(),
    }
}

/// CB-012: canonicalize a session priority to uppercase, matching the MCP
/// `normalize_priority` (dispatch.rs) so a `priority:<p>` tag written here is
/// findable by an MCP `session_recall` priority filter (which compares against
/// the uppercased value). Keep this in lockstep with the MCP twin.
fn normalize_priority(p: &str) -> String {
    p.to_uppercase()
}

fn parse_link_type(s: &str) -> LinkType {
    match s {
        "causal"       => LinkType::Causal,
        "temporal"     => LinkType::Temporal,
        "supports"     => LinkType::Supports,
        "contradicts"  => LinkType::Contradicts,
        "affective"    => LinkType::Affective,
        "contextual"   => LinkType::Contextual,
        "derived_from" => LinkType::DerivedFrom,
        "part_of"      => LinkType::PartOf,
        _              => LinkType::Semantic,
    }
}

// ---------------------------------------------------------------------------
// Request bodies
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct RememberReq {
    content:     String,
    memory_type: Option<String>,
    tags:        Option<Vec<String>>,
    salience:    Option<f64>,
    agent_id:    Option<String>,
    #[allow(dead_code)]
    visibility:  Option<String>,
}

#[derive(Deserialize)]
struct RecallReq {
    query:    String,
    top_k:    Option<usize>,
    agent_id: Option<String>,
}

#[derive(Deserialize)]
struct AssociateReq {
    source_id: String,
    target_id: String,
    link_type: Option<String>,
    weight:    Option<f64>,
}

#[derive(Deserialize)]
struct UpdateMemoryReq {
    content:  Option<String>,
    tags:     Option<Vec<String>>,
    salience: Option<f64>,
}

#[derive(Deserialize)]
struct EpisodeStartReq {
    title:    String,
    agent_id: Option<String>,
}

#[derive(Deserialize)]
struct EpisodeStepReq {
    memory_id: String,
    role:      Option<String>,
}

#[derive(Deserialize)]
struct EpisodeEndReq {
    summary: Option<String>,
}

#[derive(Deserialize)]
struct SessionSaveReq {
    content:      String,
    priority:     Option<String>,
    session_type: Option<String>,
    agent_id:     Option<String>,
}

#[derive(Deserialize)]
struct RegisterAgentReq {
    agent_id:     String,
    display_name: String,
    symbol:       Option<String>,
    color:        Option<String>,
}

#[derive(Deserialize)]
struct IntentionReq {
    content:  String,
    tags:     Option<Vec<String>>,
    agent_id: Option<String>,
}

#[derive(Deserialize)]
struct CreateSchemaReq {
    content:    String,
    tags:       Option<Vec<String>>,
    source_ids: Option<Vec<String>>,
    agent_id:   Option<String>,
}

#[derive(Deserialize)]
struct StoreProcedureReq {
    content:  String,
    tags:     Option<Vec<String>>,
    agent_id: Option<String>,
}

#[derive(Deserialize)]
struct RenameTagReq {
    old_tag:  String,
    new_tag:  String,
}

#[derive(Deserialize)]
struct MergeTagsReq {
    source_tags: Vec<String>,
    target_tag:  String,
}

#[derive(Deserialize)]
struct BulkDeleteReq {
    ids: Vec<String>,
}

#[derive(Deserialize, Default)]
struct DreamRunQuery {
    agent_id:      Option<String>,
    #[serde(default = "default_max_llm_calls")]
    max_llm_calls: usize,
}
fn default_max_llm_calls() -> usize { 20 }

// ---------------------------------------------------------------------------
// Query param structs
// ---------------------------------------------------------------------------

#[derive(Deserialize, Default)]
struct AgentQuery { agent_id: Option<String> }

#[derive(Deserialize)]
struct LimitQuery {
    #[serde(default = "default_limit")]
    limit: usize,
    agent_id: Option<String>,
}
fn default_limit() -> usize { 50 }

#[derive(Deserialize)]
struct RecallQuery {
    query:    String,
    #[serde(default = "default_top_k")]
    top_k:    usize,
    agent_id: Option<String>,
    // CB-026: honor the same priority/session_type filters the MCP session_recall
    // twin applies, so the HTTP surface returns the same result set.
    priority:     Option<String>,
    session_type: Option<String>,
}
fn default_top_k() -> usize { 10 }

// ---------------------------------------------------------------------------
// Routes
// ---------------------------------------------------------------------------

async fn health() -> Json<Value> {
    Json(json!({ "status": "ok" }))
}

// stats is a global endpoint — memory_stats aggregates the whole store
// (C-RS-009: dropped the unused agent_id query param).
async fn stats(
    State(brain): State<Brain>,
) -> AppResult {
    let v = brain.storage.read().await.sqlite.memory_stats().await?;
    Ok(Json(v))
}

// GET /q/:query
async fn quick_search(
    Path(query): Path<String>,
    Query(q): Query<LimitQuery>,
    State(brain): State<Brain>,
) -> AppResult {
    let scope   = scope_from(q.agent_id.as_deref());
    let results = brain.recall(&query, q.limit, scope).await?;
    let arr: Vec<Value> = results.into_iter()
        .map(|(n, s)| json!({ "score": s, "memory": n }))
        .collect();
    Ok(Json(Value::Array(arr)))
}

// POST /remember
async fn remember(
    State(brain): State<Brain>,
    Json(req): Json<RememberReq>,
) -> AppResult {
    let mt: Option<MemoryType> = req.memory_type
        .and_then(|s| serde_json::from_value(Value::String(s)).ok());
    let scope = scope_from(req.agent_id.as_deref());
    let node  = brain.remember(
        req.content, mt, req.tags, req.salience.map(|f| f as f32), scope,
    ).await?;
    Ok(Json(serde_json::to_value(&node)?))
}

// POST /recall
async fn recall(
    State(brain): State<Brain>,
    Json(req): Json<RecallReq>,
) -> AppResult {
    let scope   = scope_from(req.agent_id.as_deref());
    let results = brain.recall(&req.query, req.top_k.unwrap_or(10), scope).await?;
    let arr: Vec<Value> = results.into_iter()
        .map(|(n, s)| json!({ "score": s, "memory": n }))
        .collect();
    Ok(Json(Value::Array(arr)))
}

// GET /memory/:id
async fn get_memory(
    Path(id): Path<String>,
    Query(q): Query<AgentQuery>,
    State(brain): State<Brain>,
) -> AppResult {
    let scope = scope_from(q.agent_id.as_deref());
    let node  = brain.storage.read().await.sqlite
        .get_memory(&MemoryId(id.clone()), &scope).await?
        .ok_or_else(|| not_found(&id))?;
    Ok(Json(serde_json::to_value(&node)?))
}

// PUT /memory/:id
async fn update_memory(
    Path(id): Path<String>,
    Query(q): Query<AgentQuery>,
    State(brain): State<Brain>,
    Json(req): Json<UpdateMemoryReq>,
) -> AppResult {
    let scope   = scope_from(q.agent_id.as_deref());
    let storage = brain.storage.read().await;
    let mut node = storage.sqlite
        .get_memory(&MemoryId(id.clone()), &scope).await?
        .ok_or_else(|| not_found(&id))?;
    let content_changed = req.content.is_some();
    if let Some(c) = req.content  { node.content  = c; }
    if let Some(t) = req.tags     { node.tags      = t; }
    if let Some(s) = req.salience { node.salience  = s as f32; }
    storage.sqlite.update_memory(&node).await?;
    // CB-006: mirror the MCP update path — re-embed when content changed so the
    // vector index does not point at the pre-edit text (sqlite.update_memory only
    // refreshes the content column + FTS5 trigger, never the embedding/vec0 row).
    if content_changed {
        storage.vector.embed_and_store(&node.id, &node.content).await?;
    }
    Ok(Json(serde_json::to_value(&node)?))
}

// DELETE /memory/:id
async fn delete_memory(
    Path(id): Path<String>,
    State(brain): State<Brain>,
) -> AppResult {
    let ok = brain.storage.read().await.sqlite
        .delete_memory(&MemoryId(id.clone())).await?;
    Ok(Json(json!({ "deleted": ok })))
}

// GET /memory/:id/versions
async fn get_memory_versions(
    Path(id): Path<String>,
    Query(q): Query<LimitQuery>,
    State(brain): State<Brain>,
) -> AppResult {
    let versions = brain.storage.read().await.sqlite
        .get_memory_versions_raw(&id, q.limit).await?;
    Ok(Json(Value::Array(versions)))
}

// POST /associate
async fn associate(
    State(brain): State<Brain>,
    Json(req): Json<AssociateReq>,
) -> AppResult {
    let src  = MemoryId(req.source_id.clone());
    let tgt  = MemoryId(req.target_id.clone());
    let link = AssociativeLink {
        source_id:       src.clone(),
        target_id:       tgt.clone(),
        link_type:       parse_link_type(req.link_type.as_deref().unwrap_or("semantic")),
        weight:          req.weight.unwrap_or(0.5) as f32,
        created_at:      Utc::now(),
        last_traversed:  None,
        traversal_count: 0,
    };
    brain.associate(src, tgt, link).await?;
    Ok(Json(json!({ "status": "ok" })))
}

// ---------------------------------------------------------------------------
// Episodes
// ---------------------------------------------------------------------------

async fn list_episodes(
    Query(q): Query<LimitQuery>,
    State(brain): State<Brain>,
) -> AppResult {
    let eps = brain.storage.read().await.sqlite
        .list_episodes(q.agent_id.as_deref(), q.limit).await?;
    Ok(Json(Value::Array(eps)))
}

async fn episode_start(
    State(brain): State<Brain>,
    Json(req): Json<EpisodeStartReq>,
) -> AppResult {
    let ep_id = format!("ep_{}", uuid::Uuid::new_v4().simple());
    brain.storage.read().await.sqlite
        .create_episode(&ep_id, Some(&req.title), req.agent_id.as_deref(), None).await?;
    Ok(Json(json!({ "episode_id": ep_id, "title": req.title })))
}

async fn episode_add_step(
    Path(episode_id): Path<String>,
    State(brain): State<Brain>,
    Json(req): Json<EpisodeStepReq>,
) -> AppResult {
    let step_index = {
        let ids = brain.storage.read().await.sqlite
            .get_episode_memory_ids(&episode_id).await?;
        ids.len() as i64
    };
    brain.storage.read().await.sqlite.add_episode_step(
        &episode_id,
        step_index,
        req.role.as_deref().unwrap_or("memory"),
        Some(&req.memory_id),
    ).await?;
    Ok(Json(json!({ "status": "ok" })))
}

async fn episode_end(
    Path(episode_id): Path<String>,
    State(brain): State<Brain>,
    Json(req): Json<EpisodeEndReq>,
) -> AppResult {
    let ok = brain.storage.read().await.sqlite
        .end_episode(&episode_id, req.summary.as_deref()).await?;
    Ok(Json(json!({ "ended": ok })))
}

async fn get_episode(
    Path(episode_id): Path<String>,
    State(brain): State<Brain>,
) -> AppResult {
    let ep = brain.storage.read().await.sqlite
        .get_episode_raw(&episode_id).await?
        .ok_or_else(|| not_found(&episode_id))?;
    Ok(Json(ep))
}

async fn get_episode_memories(
    Path(episode_id): Path<String>,
    Query(q): Query<AgentQuery>,
    State(brain): State<Brain>,
) -> AppResult {
    let scope  = scope_from(q.agent_id.as_deref());
    let ids    = brain.storage.read().await.sqlite
        .get_episode_memory_ids(&episode_id).await?;
    let nodes  = brain.storage.read().await.sqlite
        .get_memories_by_ids(&ids, &scope).await?;
    let arr: Vec<Value> = nodes.into_iter()
        .map(|n| serde_json::to_value(&n).unwrap_or_default())
        .collect();
    Ok(Json(Value::Array(arr)))
}

// ---------------------------------------------------------------------------
// Sessions
// ---------------------------------------------------------------------------

async fn session_save(
    State(brain): State<Brain>,
    Json(req): Json<SessionSaveReq>,
) -> AppResult {
    let priority     = normalize_priority(req.priority.as_deref().unwrap_or("MEDIUM"));
    let session_type = req.session_type.as_deref().unwrap_or("general");
    let mut tags = vec![
        "session_note".to_string(),
        format!("priority:{priority}"),
        format!("session_type:{session_type}"),
    ];
    if let Some(ref aid) = req.agent_id {
        if !aid.is_empty() { tags.push(format!("agent:{aid}")); }
    }
    let scope = scope_from(req.agent_id.as_deref());
    let node  = brain.remember(
        req.content, Some(MemoryType::Episodic), Some(tags), Some(0.8), scope,
    ).await?;
    Ok(Json(serde_json::to_value(&node)?))
}

async fn session_recall(
    Query(q): Query<RecallQuery>,
    State(brain): State<Brain>,
) -> AppResult {
    let scope   = scope_from(q.agent_id.as_deref());
    let priority_filter = q.priority.as_deref();
    let type_filter     = q.session_type.as_deref();
    // Over-fetch so the tag filters don't deplete results (matches MCP twin).
    let results = brain.recall(&q.query, q.top_k * 5, scope).await?;
    let arr: Vec<Value> = results.into_iter()
        .filter(|(n, _)| n.tags.iter().any(|t| t == "session_note"))
        .filter(|(n, _)| priority_filter.is_none_or(|p| {
            let want = format!("priority:{}", normalize_priority(p));
            n.tags.iter().any(|t| t == &want)
        }))
        .filter(|(n, _)| type_filter.is_none_or(|st|
            n.tags.iter().any(|t| t == &format!("session_type:{st}"))))
        .take(q.top_k)
        .map(|(n, s)| json!({ "score": s, "memory": n }))
        .collect();
    Ok(Json(Value::Array(arr)))
}

// ---------------------------------------------------------------------------
// Agents
// ---------------------------------------------------------------------------

async fn list_agents(State(brain): State<Brain>) -> AppResult {
    let agents = brain.storage.read().await.sqlite.list_agents().await?;
    Ok(Json(Value::Array(agents)))
}

async fn register_agent(
    State(brain): State<Brain>,
    Json(req): Json<RegisterAgentReq>,
) -> AppResult {
    let metadata = json!({
        "symbol": req.symbol,
        "color":  req.color,
    });
    brain.storage.read().await.sqlite.register_agent(
        &req.agent_id,
        &req.display_name,
        None,
        &metadata,
    ).await?;
    Ok(Json(json!({ "agent_id": req.agent_id, "status": "ok" })))
}

// ---------------------------------------------------------------------------
// Health / diagnostics
// ---------------------------------------------------------------------------

async fn memory_health(
    Query(q): Query<AgentQuery>,
    State(brain): State<Brain>,
) -> AppResult {
    let v = brain.storage.read().await.sqlite
        .memory_health(&scope_from(q.agent_id.as_deref())).await?;
    Ok(Json(v))
}

async fn emotional_summary(
    Query(q): Query<AgentQuery>,
    State(brain): State<Brain>,
) -> AppResult {
    let v = brain.storage.read().await.sqlite
        .emotional_summary(&scope_from(q.agent_id.as_deref())).await?;
    Ok(Json(v))
}

// ---------------------------------------------------------------------------
// Graph
// ---------------------------------------------------------------------------

async fn graph_stats(State(brain): State<Brain>) -> AppResult {
    let storage = brain.storage.read().await;
    let links   = storage.sqlite.list_all_links().await?;
    let ids     = storage.sqlite.list_all_memory_ids().await?;
    Ok(Json(json!({ "nodes": ids.len(), "edges": links.len() })))
}

async fn graph_neighbors(
    Path(memory_id): Path<String>,
    Query(q): Query<AgentQuery>,
    State(brain): State<Brain>,
) -> AppResult {
    // C-RS-009: honor agent scope — only return neighbors the caller can see,
    // consistent with the recall routes (was returning every neighbor id).
    let scope   = scope_from(q.agent_id.as_deref());
    let storage = brain.storage.read().await;
    let neighbor_ids: Vec<MemoryId> = storage.graph
        .neighbors(&MemoryId(memory_id))
        .into_iter().cloned().collect();
    let visible = storage.sqlite.get_memories_by_ids(&neighbor_ids, &scope).await?;
    let ids: Vec<Value> = visible.iter().map(|n| json!(n.id.0)).collect();
    Ok(Json(Value::Array(ids)))
}

async fn graph_path(
    Path((source_id, target_id)): Path<(String, String)>,
    State(brain): State<Brain>,
) -> AppResult {
    let storage = brain.storage.read().await;
    let path    = brain.association.find_path(
        &storage.graph,
        &MemoryId(source_id),
        &MemoryId(target_id),
    );
    let ids: Vec<Value> = path.as_ref()
        .map(|p| p.iter().map(|id| json!(id.0)).collect())
        .unwrap_or_default();
    Ok(Json(Value::Array(ids)))
}

async fn graph_common(
    Path((id_a, id_b)): Path<(String, String)>,
    State(brain): State<Brain>,
) -> AppResult {
    let storage = brain.storage.read().await;
    let common  = brain.association.get_common_neighbors(
        &storage.graph, &MemoryId(id_a), &MemoryId(id_b),
    );
    let ids: Vec<Value> = common.iter().map(|id| json!(id.0)).collect();
    Ok(Json(Value::Array(ids)))
}

// ---------------------------------------------------------------------------
// Tags
// ---------------------------------------------------------------------------

async fn list_tags(
    Query(q): Query<AgentQuery>,
    State(brain): State<Brain>,
) -> AppResult {
    let tags = brain.storage.read().await.sqlite
        .list_tags(&scope_from(q.agent_id.as_deref())).await?;
    Ok(Json(Value::Array(tags)))
}

async fn rename_tag(
    State(brain): State<Brain>,
    Json(req): Json<RenameTagReq>,
) -> AppResult {
    let count = brain.storage.read().await.sqlite
        .rename_tag_everywhere(&req.old_tag, &req.new_tag).await?;
    Ok(Json(json!({ "updated": count })))
}

async fn merge_tags(
    State(brain): State<Brain>,
    Json(req): Json<MergeTagsReq>,
) -> AppResult {
    let mut total = 0usize;
    let storage = brain.storage.read().await;
    for src in &req.source_tags {
        total += storage.sqlite.rename_tag_everywhere(src, &req.target_tag).await?;
    }
    Ok(Json(json!({ "updated": total })))
}

async fn delete_tag(
    Path(tag): Path<String>,
    State(brain): State<Brain>,
) -> AppResult {
    let count = brain.storage.read().await.sqlite
        .delete_tag_everywhere(&tag).await?;
    Ok(Json(json!({ "removed_from": count })))
}

// ---------------------------------------------------------------------------
// Intentions
// ---------------------------------------------------------------------------

async fn store_intention(
    State(brain): State<Brain>,
    Json(req): Json<IntentionReq>,
) -> AppResult {
    let scope = scope_from(req.agent_id.as_deref());
    let mut tags = vec!["intention".to_string()];
    if let Some(t) = req.tags { tags.extend(t); }
    let node = brain.remember(
        req.content, Some(MemoryType::Prospective), Some(tags), Some(0.7), scope,
    ).await?;
    Ok(Json(serde_json::to_value(&node)?))
}

async fn list_intentions(
    Query(q): Query<AgentQuery>,
    State(brain): State<Brain>,
) -> AppResult {
    let nodes = brain.storage.read().await.sqlite
        .list_memories_scoped(
            &scope_from(q.agent_id.as_deref()),
            &ListFilter { memory_type: Some(MemoryType::Prospective), limit: 100, ..Default::default() },
        ).await?;
    let active: Vec<Value> = nodes.into_iter()
        .filter(|n| !n.tags.iter().any(|t| t == "status:resolved"))
        .map(|n| serde_json::to_value(&n).unwrap_or_default())
        .collect();
    Ok(Json(Value::Array(active)))
}

async fn resolve_intention(
    Path(memory_id): Path<String>,
    Query(q): Query<AgentQuery>,
    State(brain): State<Brain>,
) -> AppResult {
    let mid  = MemoryId(memory_id.clone());
    let scope = scope_from(q.agent_id.as_deref());
    let storage = brain.storage.read().await;
    let mut node = storage.sqlite.get_memory(&mid, &scope).await?
        .ok_or_else(|| not_found(&memory_id))?;
    node.tags.retain(|t| !t.starts_with("status:"));
    node.tags.push("status:resolved".into());
    node.salience = 0.1;
    storage.sqlite.update_memory(&node).await?;
    Ok(Json(json!({ "status": "resolved", "id": memory_id })))
}

// ---------------------------------------------------------------------------
// Schemas
// ---------------------------------------------------------------------------

async fn create_schema(
    State(brain): State<Brain>,
    Json(req): Json<CreateSchemaReq>,
) -> AppResult {
    let scope = scope_from(req.agent_id.as_deref());
    let mut tags = vec!["schema".to_string(), "support_count:0".to_string()];
    if let Some(t) = req.tags { tags.extend(t); }
    let node = brain.remember(
        req.content, Some(MemoryType::Schematic), Some(tags), Some(0.7), scope,
    ).await?;
    if let Some(sources) = req.source_ids {
        if !sources.is_empty() {
            let mut n = node.clone();
            if let serde_json::Value::Object(ref mut map) = n.metadata {
                map.insert("derived_from".into(), json!(sources));
            } else {
                n.metadata = json!({ "derived_from": sources });
            }
            brain.storage.read().await.sqlite.update_memory(&n).await?;
        }
    }
    Ok(Json(serde_json::to_value(&node)?))
}

async fn list_schemas(
    Query(q): Query<AgentQuery>,
    State(brain): State<Brain>,
) -> AppResult {
    let nodes = brain.storage.read().await.sqlite
        .list_memories_scoped(
            &scope_from(q.agent_id.as_deref()),
            &ListFilter { memory_type: Some(MemoryType::Schematic), limit: 100, ..Default::default() },
        ).await?;
    let arr: Vec<Value> = nodes.into_iter()
        .map(|n| serde_json::to_value(&n).unwrap_or_default())
        .collect();
    Ok(Json(Value::Array(arr)))
}

// ---------------------------------------------------------------------------
// Procedures
// ---------------------------------------------------------------------------

async fn store_procedure(
    State(brain): State<Brain>,
    Json(req): Json<StoreProcedureReq>,
) -> AppResult {
    let scope = scope_from(req.agent_id.as_deref());
    let mut tags = vec!["procedure".to_string()];
    if let Some(t) = req.tags { tags.extend(t); }
    let node = brain.remember(
        req.content, Some(MemoryType::Procedural), Some(tags), Some(0.8), scope,
    ).await?;
    Ok(Json(serde_json::to_value(&node)?))
}

async fn list_procedures(
    Query(q): Query<AgentQuery>,
    State(brain): State<Brain>,
) -> AppResult {
    let nodes = brain.storage.read().await.sqlite
        .list_memories_scoped(
            &scope_from(q.agent_id.as_deref()),
            &ListFilter { memory_type: Some(MemoryType::Procedural), limit: 100, ..Default::default() },
        ).await?;
    let arr: Vec<Value> = nodes.into_iter()
        .map(|n| serde_json::to_value(&n).unwrap_or_default())
        .collect();
    Ok(Json(Value::Array(arr)))
}

// ---------------------------------------------------------------------------
// Trash
// ---------------------------------------------------------------------------

async fn list_trash(
    Query(q): Query<LimitQuery>,
    State(brain): State<Brain>,
) -> AppResult {
    let scope   = scope_from(q.agent_id.as_deref());
    let deleted = brain.storage.read().await.sqlite
        .list_deleted_memories(&scope, q.limit).await?;
    let arr: Vec<Value> = deleted.into_iter()
        .map(|n| serde_json::to_value(&n).unwrap_or_default())
        .collect();
    Ok(Json(Value::Array(arr)))
}

async fn restore_trash(
    Path(memory_id): Path<String>,
    State(brain): State<Brain>,
) -> AppResult {
    let ok = brain.storage.read().await.sqlite
        .restore_memory(&MemoryId(memory_id)).await?;
    Ok(Json(json!({ "restored": ok })))
}

async fn purge_trash(
    Path(memory_id): Path<String>,
    State(brain): State<Brain>,
) -> AppResult {
    let ok = brain.storage.read().await.sqlite
        .purge_memory(&MemoryId(memory_id)).await?;
    Ok(Json(json!({ "purged": ok })))
}

async fn purge_all_trash(
    State(brain): State<Brain>,
) -> AppResult {
    let count = brain.storage.read().await.sqlite.purge_all_deleted().await?;
    Ok(Json(json!({ "purged": count })))
}

async fn bulk_delete(
    State(brain): State<Brain>,
    Json(req): Json<BulkDeleteReq>,
) -> AppResult {
    let ids: Vec<MemoryId> = req.ids.into_iter().map(MemoryId).collect();
    let count = brain.storage.read().await.sqlite.bulk_delete(&ids).await?;
    Ok(Json(json!({ "deleted": count })))
}

// ---------------------------------------------------------------------------
// Threads
// ---------------------------------------------------------------------------

async fn list_threads(
    Query(q): Query<AgentQuery>,
    State(brain): State<Brain>,
) -> AppResult {
    let threads = brain.storage.read().await.sqlite
        .list_threads(&scope_from(q.agent_id.as_deref())).await?;
    Ok(Json(Value::Array(threads.into_iter().map(|s| json!(s)).collect())))
}

async fn get_thread_memories(
    Path(thread_id): Path<String>,
    Query(q): Query<LimitQuery>,
    State(brain): State<Brain>,
) -> AppResult {
    let scope = scope_from(q.agent_id.as_deref());
    let mems  = brain.storage.read().await.sqlite
        .get_thread_memories(&thread_id, &scope).await?;
    let arr: Vec<Value> = mems.into_iter()
        .map(|n| serde_json::to_value(&n).unwrap_or_default())
        .collect();
    Ok(Json(Value::Array(arr)))
}

async fn prune_thread(
    Path(thread_id): Path<String>,
    State(brain): State<Brain>,
) -> AppResult {
    let count = brain.storage.read().await.sqlite.prune_thread(&thread_id).await?;
    Ok(Json(json!({ "deleted": count })))
}

// ---------------------------------------------------------------------------
// Dream
// ---------------------------------------------------------------------------

async fn dream_run(
    Query(q): Query<DreamRunQuery>,
    State(brain): State<Brain>,
) -> AppResult {
    let scope     = scope_from(q.agent_id.as_deref());
    let brain_arc = Arc::clone(&brain);
    let report    = brain.dream.run_cycle(scope, brain_arc, q.max_llm_calls).await?;
    Ok(Json(serde_json::to_value(&report)?))
}

// dream_status is a global endpoint — the last dream report is not agent-scoped
// (C-RS-009: dropped the unused agent_id query param rather than pretending to
// honor it).
async fn dream_status(
    State(brain): State<Brain>,
) -> AppResult {
    let v = brain.storage.read().await.sqlite
        .get_last_dream_report().await?
        .unwrap_or(json!({ "status": "no_cycles_run" }));
    Ok(Json(v))
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

    let config = cerebro::config::Config::from_env()?;
    let brain: Brain = Arc::new(CerebroCortex::new(config).await?);

    let app = Router::new()
        // Core
        .route("/health",          get(health))
        .route("/stats",           get(stats))
        .route("/q/{query}",       get(quick_search))
        .route("/remember",        post(remember))
        .route("/recall",          post(recall))
        // Memory CRUD
        .route("/memory/{id}",              get(get_memory).put(update_memory).delete(delete_memory))
        .route("/memory/{id}/versions",     get(get_memory_versions))
        // Associate
        .route("/associate",       post(associate))
        // Episodes
        .route("/episodes",        get(list_episodes).post(episode_start))
        .route("/episodes/{episode_id}/step",      post(episode_add_step))
        .route("/episodes/{episode_id}/end",       post(episode_end))
        .route("/episodes/{episode_id}",           get(get_episode))
        .route("/episodes/{episode_id}/memories",  get(get_episode_memories))
        // Sessions
        .route("/sessions/save",   post(session_save))
        .route("/sessions",        get(session_recall))
        // Agents
        .route("/agents",          get(list_agents).post(register_agent))
        // Diagnostics
        .route("/memory/health",   get(memory_health))
        .route("/emotions",        get(emotional_summary))
        // Graph
        .route("/graph/stats",                         get(graph_stats))
        .route("/graph/neighbors/{memory_id}",         get(graph_neighbors))
        .route("/graph/path/{source_id}/{target_id}",  get(graph_path))
        .route("/graph/common/{id_a}/{id_b}",          get(graph_common))
        // Tags
        .route("/tags",            get(list_tags))
        .route("/tags/rename",     post(rename_tag))
        .route("/tags/merge",      post(merge_tags))
        .route("/tags/{tag}",      delete(delete_tag))
        // Intentions
        .route("/intentions",                      get(list_intentions).post(store_intention))
        .route("/intentions/{memory_id}/resolve",  post(resolve_intention))
        // Schemas
        .route("/schemas",         get(list_schemas).post(create_schema))
        // Procedures
        .route("/procedures",      get(list_procedures).post(store_procedure))
        // Trash / lifecycle
        .route("/trash",           get(list_trash))
        .route("/trash/{id}/restore", post(restore_trash))
        .route("/trash/{id}",      delete(purge_trash))
        .route("/trash/purge-all", post(purge_all_trash))
        .route("/bulk/delete",     post(bulk_delete))
        // Threads
        .route("/threads",                         get(list_threads))
        .route("/threads/{thread_id}/memories",    get(get_thread_memories))
        .route("/threads/{thread_id}",             delete(prune_thread))
        // Dream
        .route("/dream/run",       post(dream_run))
        .route("/dream/status",    get(dream_status))
        .with_state(brain);

    // Token auth — reads AGENTD_TOKEN (same shared secret as the agentd gateway).
    // Binds 127.0.0.1 by default; use CEREBRO_API_ADDR=0.0.0.0:8765 for LAN exposure.
    let api_token = Arc::new(std::env::var("AGENTD_TOKEN").unwrap_or_default());
    if api_token.is_empty() {
        info!("cerebro-api: AGENTD_TOKEN not set — auth disabled (127.0.0.1 only)");
    }
    let token_mw = api_token.clone();
    let app = app.layer(axum::middleware::from_fn(
        move |req: Request, next: Next| {
            let tok = token_mw.clone();
            async move {
                if tok.is_empty() { return next.run(req).await; }
                let from_header = req.headers()
                    .get(header::AUTHORIZATION)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|s| s.strip_prefix("Bearer "))
                    .unwrap_or("");
                if ct_eq(from_header, tok.as_str()) { return next.run(req).await; }
                let from_query = req.uri().query().unwrap_or("")
                    .split('&')
                    .find_map(|p| p.strip_prefix("token="))
                    .unwrap_or("");
                if ct_eq(from_query, tok.as_str()) { return next.run(req).await; }
                (StatusCode::UNAUTHORIZED, "invalid or missing token").into_response()
            }
        }
    ));

    // CB-023: outermost layer so a panic anywhere in a handler (or the auth
    // middleware) becomes a 500 JSON body rather than a dropped connection.
    let app = app.layer(
        tower_http::catch_panic::CatchPanicLayer::custom(panic_response),
    );

    let addr = std::env::var("CEREBRO_API_ADDR")
        .unwrap_or_else(|_| "127.0.0.1:8765".into());
    if api_token.is_empty() {
        if let Ok(sa) = addr.parse::<std::net::SocketAddr>() {
            if !sa.ip().is_loopback() {
                anyhow::bail!("refusing to bind {addr} without AGENTD_TOKEN");
            }
        }
    }
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    info!("cerebro-api listening on {addr}");
    if !api_token.is_empty() {
        info!("cerebro-api dashboard: http://{addr}/?token=<AGENTD_TOKEN>  (bearer token required)");
    }
    axum::serve(listener, app).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // CB-012: the HTTP priority normalization must match the MCP canonical
    // (uppercase) so a `priority:<p>` tag written here is matched by an MCP
    // session_recall priority filter that uppercases its argument.
    #[test]
    fn normalize_priority_uppercases() {
        assert_eq!(normalize_priority("medium"), "MEDIUM");
        assert_eq!(normalize_priority("High"), "HIGH");
        assert_eq!(normalize_priority("LOW"), "LOW");
    }

    // The session_save default ("MEDIUM") and an HTTP-supplied lowercase value
    // ("medium") must produce the identical canonical tag.
    #[test]
    fn normalize_priority_default_matches_lowercase_input() {
        assert_eq!(normalize_priority("MEDIUM"), normalize_priority("medium"));
    }
}
