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
/// What to search the visual memory with (search_vision).
pub enum VisionQuery {
    /// A text description → CLIP text tower → ranks images by visual content.
    Text(String),
    /// Raw image bytes → CLIP image tower → finds visually-similar images.
    Image(Vec<u8>),
}

/// One search_vision result: the caption memory, its CLIP similarity, and the
/// source image path (present when CLIP-indexed; None on the caption/FTS fallback).
pub struct VisionHit {
    pub memory:     MemoryNode,
    pub score:      f32,
    pub image_path: Option<String>,
}

pub struct CerebroCortex {
    pub storage:     Arc<RwLock<StorageCoordinator>>,
    /// Whether CLIP visual embedding is active (tier-gated on text-embeddings, env
    /// `CEREBRO_VISION_EMBED` override). Off → search_vision uses caption/FTS recall.
    pub vision_embed: bool,
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
        let vision_embed = vision_embed_enabled(&config);
        let storage = StorageCoordinator::new(&config).await?;
        Ok(Self {
            storage:     Arc::new(RwLock::new(storage)),
            vision_embed,
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

        // Embed OUTSIDE any storage lock (CB-007): inference is CPU-bound and
        // needs only the embedder Arc — holding the write guard across it
        // serialized every concurrent reader/writer on the embed latency.
        // Failure is non-fatal by design (CB-009): the memory still lands in
        // sqlite + FTS5 + graph, just without a vector.
        let embedding = self.embed_lockfree(&node.content).await;

        // Persist across all three storage backends. Graph node BEFORE the
        // vector persist (CB-009): add_node is infallible, so a vector-store
        // error can no longer orphan the memory out of spreading activation
        // until the next restart.
        let mut storage = self.storage.write().await;
        storage.sqlite.insert_memory(&node).await?;
        storage.graph.add_node(node.id.clone());
        if let Some(vec) = embedding {
            if let Err(e) = storage.vector.store_raw_embedding(&node.id, &vec).await {
                tracing::warn!(id = %node.id.0,
                    "embedding persist failed — memory stored without a vector (FTS5 still finds it): {e}");
            }
        }

        tracing::info!(id = %node.id.0, memory_type = ?node.memory_type, salience = node.salience, "memory stored");
        Ok(node)
    }

    /// Compute an embedding with NO storage lock held (CB-007/CB-019): clone
    /// the embedder handle under a brief read guard, run inference lock-free.
    /// `None` = no embedder (Nano tier) or a failed embed — logged, non-fatal;
    /// callers degrade to FTS5/vector-less behaviour.
    async fn embed_lockfree(&self, text: &str) -> Option<Vec<f32>> {
        let embedder = self.storage.read().await.vector.embedder_handle()?;
        let owned = text.to_string();
        match tokio::task::spawn_blocking(move || {
            embedder.embed(vec![owned], None).map(|mut v| v.remove(0))
        }).await {
            Ok(Ok(v))  => Some(v),
            Ok(Err(e)) => { tracing::warn!("embedding failed (non-fatal): {e}"); None }
            Err(e)     => { tracing::warn!("embedding task join failed (non-fatal): {e}"); None }
        }
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
        // Embed the query BEFORE taking the read guard (CB-019): a held read
        // guard blocks writers, so query inference under it stalled every
        // concurrent remember/associate. A failed embed degrades to FTS5.
        let query_vec = self.embed_lockfree(query).await;

        let storage = self.storage.read().await;
        let (scope_sql, scope_params) = scope.sql_filter();

        // 1. Vector / FTS5 candidates (over-fetch for spreading)
        let candidates = storage.vector
            .search_seeded(query, k * 5, scope_sql, &scope_params, query_vec.as_deref()).await?;
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
        // `agent_id is None` path in `_check_access` — but NOT the shared-only
        // federation scope, where private nodes must not even influence the
        // spread (can_access below enforces it per node).
        let visible_nodes: HashMap<NodeIndex, bool> = if scope.agent_id.is_none() && !scope.shared_only {
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

    // -----------------------------------------------------------------------
    // Visual recall (search_vision) — CLIP image/text shared embedding space
    // -----------------------------------------------------------------------

    /// Index an image's CLIP embedding against its caption `memory_id`, so visual
    /// recall can rank it later. No-op (`Ok(false)`) when visual embedding is off
    /// (Nano tier / opt-out) or the CLIP tower can't load — describe_image still
    /// stored the caption, so the loop degrades, never breaks.
    pub async fn index_image(
        &self,
        memory_id:  &MemoryId,
        image_bytes: Vec<u8>,
        image_path:  Option<String>,
    ) -> Result<bool> {
        if !self.vision_embed {
            return Ok(false);
        }
        let vec = match crate::vision::clip_embed_image(image_bytes).await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!("index_image: CLIP embed failed ({e}) — caption stored, image not indexed");
                return Ok(false);
            }
        };
        let storage = self.storage.read().await;
        storage.vector.store_vision_embedding(memory_id, &vec, image_path.as_deref()).await?;
        Ok(true)
    }

    /// Visual recall: rank stored images by a text or image query in CLIP's shared
    /// space, scope-filtered. Falls back to caption/FTS recall over `vision`-tagged
    /// memories when visual embedding is off or no images are indexed yet (a text
    /// query only — an image query has no text to keyword-search).
    pub async fn search_vision(
        &self,
        query: VisionQuery,
        k:     usize,
        scope: VisibilityScope,
    ) -> Result<Vec<VisionHit>> {
        if self.vision_embed {
            let qvec = match &query {
                VisionQuery::Text(t)  => crate::vision::clip_embed_text(t.clone()).await,
                VisionQuery::Image(b) => crate::vision::clip_embed_image(b.clone()).await,
            };
            match qvec {
                Ok(qvec) => {
                    let storage = self.storage.read().await;
                    let cands = storage.vector.vision_search(&qvec, k * 3).await?;
                    if !cands.is_empty() {
                        let ids: Vec<MemoryId> = cands.iter().map(|(id, _, _)| id.clone()).collect();
                        let nodes = storage.sqlite.get_memories_by_ids(&ids, &scope).await?;
                        let node_map: HashMap<MemoryId, MemoryNode> =
                            nodes.into_iter().map(|n| (n.id.clone(), n)).collect();
                        // cands are already similarity-sorted; keep that order, drop
                        // out-of-scope ids, take k.
                        let mut hits: Vec<VisionHit> = cands.into_iter()
                            .filter_map(|(id, sim, path)| {
                                node_map.get(&id).map(|n| VisionHit {
                                    memory: n.clone(), score: sim, image_path: path,
                                })
                            })
                            .collect();
                        hits.truncate(k);
                        return Ok(hits);
                    }
                    // No images indexed yet → fall through to caption recall.
                }
                Err(e) => tracing::warn!("search_vision: CLIP embed failed ({e}) — caption fallback"),
            }
        }

        // Fallback: semantic recall over vision-tagged captions (text query only).
        let qtext = match query {
            VisionQuery::Text(t)  => t,
            VisionQuery::Image(_) => return Ok(vec![]),
        };
        let recalled = self.recall(&qtext, k * 2, scope).await?;
        let hits = recalled.into_iter()
            .filter(|(n, _)| n.tags.iter().any(|t| t == "vision"))
            .take(k)
            .map(|(n, score)| VisionHit { memory: n, score, image_path: None })
            .collect();
        Ok(hits)
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

/// Whether CLIP visual embedding is active. Default (env unset / `auto`): follow
/// text-embeddings — on for Micro+ (`CEREBRO_EMBED_MODEL` set), off for Nano
/// (empty), keeping the tier ladder automatic and CLIP's ~350 MB model off the
/// smallest boards. `CEREBRO_VISION_EMBED=off` force-disables; any other value
/// (`on` / a model name) force-enables. The model itself lazy-loads on first use,
/// so an enabled node that never does visual recall pays nothing.
fn vision_embed_enabled(config: &Config) -> bool {
    match std::env::var("CEREBRO_VISION_EMBED") {
        Err(_) => !config.embed_model.is_empty(),
        Ok(v) => match v.trim().to_lowercase().as_str() {
            "" | "off" | "0" | "false" | "no" => false,
            "auto" => !config.embed_model.is_empty(),
            _ => true,
        },
    }
}
