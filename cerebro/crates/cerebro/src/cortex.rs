use std::{collections::HashMap, sync::Arc};

use anyhow::Result;
use chrono::Utc;
use petgraph::graph::NodeIndex;
use tokio::sync::RwLock;

use crate::{
    activation::spread,
    config::Config,
    engines::{
        AffectEngine, DreamEngine, EpisodicEngine, ExecutiveEngine, GatingEngine,
        LinkEngine, ProceduralEngine, SchemaEngine, SemanticEngine,
    },
    models::{AssociativeLink, MemoryNode},
    storage::StorageCoordinator,
    types::{MemoryId, MemoryType, Visibility, VisibilityScope},
};

/// CerebroCortex — top-level coordinator.
/// Owns all 9 engines + the storage coordinator.
/// This is what `cerebro-mcp` and `cerebro-api` construct and share via Arc.
pub struct CerebroCortex {
    pub storage:     Arc<RwLock<StorageCoordinator>>,
    // Engines
    pub thalamus:    GatingEngine,
    pub amygdala:    AffectEngine,
    pub temporal:    SemanticEngine,
    pub hippocampus: EpisodicEngine,
    pub association: LinkEngine,
    pub cerebellum:  ProceduralEngine,
    pub prefrontal:  ExecutiveEngine,
    pub neocortex:   SchemaEngine,
    pub dream:       DreamEngine,
}

impl CerebroCortex {
    pub async fn new(config: Config) -> Result<Self> {
        let storage = StorageCoordinator::new(&config).await?;
        Ok(Self {
            storage:     Arc::new(RwLock::new(storage)),
            thalamus:    GatingEngine::new(),
            amygdala:    AffectEngine::new(),
            temporal:    SemanticEngine::new(),
            hippocampus: EpisodicEngine::new(),
            association: LinkEngine::new(),
            cerebellum:  ProceduralEngine::new(),
            prefrontal:  ExecutiveEngine::new(),
            neocortex:   SchemaEngine::new(),
            dream:       DreamEngine::new(config.anthropic_key),
        })
    }

    // -----------------------------------------------------------------------
    // Core operations (build-order step 7)
    // -----------------------------------------------------------------------

    /// Store a new memory through the full cognitive pipeline.
    ///
    /// Pipeline: thalamus gate → amygdala emotion → temporal concepts
    ///           → SQLite insert → vector embed → graph node
    ///
    /// Returns Err if thalamus rejects the content (too short / filtered).
    pub async fn remember(
        &self,
        content: impl Into<String>,
        memory_type: Option<MemoryType>,
        tags:        Option<Vec<String>>,
        salience:    Option<f32>,
        scope:       VisibilityScope,
    ) -> Result<MemoryNode> {
        let content = content.into();

        // Thalamus: gate and initialize parameters
        let visibility = match &scope.agent_id {
            None    => Visibility::Shared,
            Some(_) => Visibility::Private,
        };
        let mut node = self.thalamus
            .evaluate_input(&content, memory_type, tags, salience, scope.agent_id.clone(), visibility)
            .ok_or_else(|| anyhow::anyhow!("content rejected by thalamus (too short or filtered)"))?;

        // Amygdala: emotional classification and salience modulation
        node = self.amygdala.apply_emotion(node);

        // Temporal: extract and store semantic concepts in metadata
        node = self.temporal.enrich_node(node);

        // Persist across all three storage backends
        let mut storage = self.storage.write().await;
        storage.sqlite.insert_memory(&node).await?;
        storage.vector.embed_and_store(&node.id, &node.content).await?;
        storage.graph.add_node(node.id.clone());

        tracing::info!(id = %node.id.0, memory_type = ?node.memory_type, salience = node.salience, "memory stored");
        Ok(node)
    }

    /// Recall memories matching a query string.
    ///
    /// Pipeline: vector/FTS5 search → spreading activation → bulk SQLite load
    ///           → prefrontal ranking → top-k return
    pub async fn recall(
        &self,
        query: &str,
        k:     usize,
        scope: VisibilityScope,
    ) -> Result<Vec<(MemoryNode, f32)>> {
        let storage = self.storage.read().await;
        let (scope_sql, scope_params) = scope.sql_filter();

        // 1. Vector / FTS5 candidates (over-fetch for spreading)
        let candidates = storage.vector
            .search(query, k * 5, scope_sql, &scope_params).await?;
        if candidates.is_empty() {
            return Ok(vec![]);
        }

        // 2. Spreading activation from vector-search seeds.
        //    Seeds carry their vector-similarity score (not a flat 1.0) so the
        //    spread is similarity-weighted, matching Python.
        let seeds: Vec<(NodeIndex, f32)> = candidates.iter()
            .filter_map(|(id, sim)| storage.graph.index.get(id).map(|&idx| (idx, *sim)))
            .collect();

        // Scope-visibility map (C-RS-003): a node participates in the spread only
        // if the caller can see it, so another agent's private/thread memories
        // can't shape the activations of nodes we *do* return. Global scope
        // (agent_id == None) short-circuits to all-visible, matching Python's
        // `agent_id is None` path in `_check_access`.
        let visible_nodes: HashMap<NodeIndex, bool> = if scope.agent_id.is_none() {
            storage.graph.index.values().map(|&idx| (idx, true)).collect()
        } else {
            let all_ids: Vec<MemoryId> = storage.graph.index.keys().cloned().collect();
            let vis_meta = storage.sqlite.get_visibility_meta(&all_ids).await?;
            storage.graph.index.iter()
                .map(|(id, &idx)| {
                    let visible = match vis_meta.get(id) {
                        Some((vis, owner)) => scope.can_access(*vis, owner.as_ref()),
                        None => true, // not in DB → final SQLite filter handles it
                    };
                    (idx, visible)
                })
                .collect()
        };
        let activated = spread(&storage.graph.graph, &seeds, &visible_nodes);

        // 3. Build score maps and union ID set
        let sims_map: HashMap<MemoryId, f32> = candidates.into_iter().collect();
        let assoc_map: HashMap<MemoryId, f32> = activated.into_iter()
            .filter_map(|(idx, score)| {
                storage.graph.graph.node_weight(idx).map(|id| (id.clone(), score))
            })
            .collect();

        let mut all_ids: Vec<MemoryId> = sims_map.keys().cloned().collect();
        for id in assoc_map.keys() {
            if !sims_map.contains_key(id) {
                all_ids.push(id.clone());
            }
        }

        // 4. Bulk-load full nodes
        let nodes = storage.sqlite.get_memories_by_ids(&all_ids, &scope).await?;

        // 5. Rank and return top-k
        let ranked = self.prefrontal.rank_results(&nodes, Some(&sims_map), Some(&assoc_map));
        let node_map: HashMap<MemoryId, MemoryNode> =
            nodes.into_iter().map(|n| (n.id.clone(), n)).collect();

        let mut results: Vec<(MemoryNode, f32)> = ranked.into_iter()
            .take(k)
            .filter_map(|(id, score)| node_map.get(&id).map(|n| (n.clone(), score)))
            .collect();

        // Reinforcement (ACT-R): a successful retrieval IS an access — record it
        // so base-level activation rises and the memory resurfaces more easily
        // next time ("recall sharpens memory"). Only the returned top-k are
        // reinforced (what the caller actually saw), and the strength is persisted
        // in one batched UPDATE so the hot path stays cheap. The returned nodes
        // carry the updated access history so the caller sees a consistent view.
        let now = Utc::now();
        for (node, _) in results.iter_mut() {
            node.record_access(now);        // ACT-R: a retrieval IS an access
            node.record_recall_review(now); // FSRS: successful review (stability/difficulty + last_review)
        }
        let reinforcements: Vec<(MemoryId, u32, String, f32, f32, Option<String>)> = results.iter()
            .map(|(n, _)| {
                let times = serde_json::to_string(&n.access_times)
                    .unwrap_or_else(|_| "[]".to_string());
                (
                    n.id.clone(),
                    n.access_count,
                    times,
                    n.strength.stability,
                    n.strength.difficulty,
                    n.strength.last_review.map(|dt| dt.to_rfc3339()),
                )
            })
            .collect();
        storage.sqlite.record_accesses(&reinforcements).await?;

        Ok(results)
    }

    /// Associate two existing memories with a typed link.
    ///
    /// Writes to SQLite first (source of truth), then mirrors into the graph.
    pub async fn associate(
        &self,
        _source: MemoryId,
        _target: MemoryId,
        link:    AssociativeLink,
    ) -> Result<()> {
        let mut storage = self.storage.write().await;

        // C-RS-010: validate both endpoints exist (and are live) BEFORE writing,
        // so a typo'd/nonexistent id can't leave a dangling orphan row in `links`
        // that the graph silently skips. The graph index is rebuilt from
        // non-deleted memories, so membership there == exists & not soft-deleted.
        if !storage.graph.index.contains_key(&link.source_id) {
            anyhow::bail!("associate: source memory does not exist: {}", link.source_id.0);
        }
        if !storage.graph.index.contains_key(&link.target_id) {
            anyhow::bail!("associate: target memory does not exist: {}", link.target_id.0);
        }

        storage.sqlite.insert_link(&link).await?;
        if let Err(e) = storage.graph.add_edge(link) {
            tracing::warn!("associate: graph edge not added — {e}");
        }
        Ok(())
    }
}
