use std::{collections::HashMap, sync::Arc};

use anyhow::Result;
use chrono::Utc;
use petgraph::graph::NodeIndex;
use tokio::sync::RwLock;

use crate::{
    activation::spread_traced,
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
    ///           → SQLite insert → vector embed → graph node → auto-link
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
        self.remember_with_visibility(content, memory_type, tags, salience, scope, None).await
    }

    /// `remember` with an explicit visibility. `None` defaults to **Shared**
    /// (Python parity — cortex.py's `visibility=Visibility.SHARED` default,
    /// regardless of agent scope; the port used to force agent-scoped stores
    /// Private, silently breaking cross-agent sharing — R-05). Private without
    /// an owner is refused, mirroring update_memory's orphan guard: such a row
    /// would be visible to no one.
    pub async fn remember_with_visibility(
        &self,
        content: impl Into<String>,
        memory_type: Option<MemoryType>,
        tags:        Option<Vec<String>>,
        salience:    Option<f32>,
        scope:       VisibilityScope,
        visibility:  Option<Visibility>,
    ) -> Result<MemoryNode> {
        let content = content.into();

        // Thalamus: gate and initialize parameters
        let visibility = visibility.unwrap_or(Visibility::Shared);
        if visibility == Visibility::Private && scope.agent_id.is_none() {
            anyhow::bail!(
                "refusing to store a private memory with no owner (it would be \
                 visible to no one) — pass agent_id alongside visibility:\"private\""
            );
        }
        let mut node = self.thalamus
            .evaluate_input(&content, memory_type, tags, salience, scope.agent_id.clone(), visibility)
            .ok_or_else(|| anyhow::anyhow!(
                "content rejected by thalamus (under 10 chars, or over the 64 KiB \
                 single-memory cap — one memory is a note, not a document dump; \
                 split it or ingest_file it)"
            ))?;

        // Thalamus gate 2 — exact-content dedup (the Python step the port
        // dropped; found via the colony's "install prewarm" fossil islands,
        // 2026-07-28). An identical body in the SAME owner space is a
        // re-encounter, not a new memory: reinforce the existing node's
        // access history and hand it back. Messages are exempt (Python:
        // "messages are always stored") — a repeated "ping" is two events.
        let is_message = node.tags.iter().any(|t| t == "message");
        if !is_message {
            let storage = self.storage.read().await;
            if let Some(id) = storage.sqlite
                .find_exact_content(&content, node.agent_id.as_ref().map(|a| a.0.as_str())).await?
            {
                if let Some(mut existing) = storage.sqlite.get_memory(&id, &scope).await? {
                    let now = Utc::now();
                    existing.record_access(now);
                    let times = serde_json::to_string(&existing.access_times)
                        .unwrap_or_else(|_| "[]".to_string());
                    storage.sqlite.record_accesses(&[(
                        existing.id.clone(),
                        existing.access_count,
                        times,
                        existing.strength.stability,
                        existing.strength.difficulty,
                        existing.strength.last_review.map(|dt| dt.to_rfc3339()),
                    )]).await?;
                    return Ok(existing);
                }
            }
        }

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

        // Python remember() step 5, restored: without it every new memory is
        // born isolated and spreading activation can never reach it (the topo
        // exploration found 23% of the store stranded this way). Non-fatal by
        // the same rule as the vector persist.
        let auto_links = match Self::auto_link_locked(&mut storage, &node).await {
            Ok(n) => n,
            Err(e) => {
                tracing::warn!("auto-link failed (non-fatal): {e}");
                0
            }
        };

        tracing::info!(id = %node.id.0, memory_type = ?node.memory_type,
            salience = node.salience, auto_links, "memory stored");
        Ok(node)
    }

    /// Encoding-time auto-link (own write guard) — the retrofit entry point
    /// for memories stored before the pass existed (`cerebro autolink`).
    pub async fn auto_link(&self, node: &MemoryNode) -> Result<usize> {
        let mut storage = self.storage.write().await;
        Self::auto_link_locked(&mut storage, node).await
    }

    /// Port of Python's encoding-time link passes (`auto_link_on_store` tag
    /// links + `create_semantic_links` concept links + `create_affective_links`),
    /// folded into one call under the caller's write guard. Scope is derived
    /// from the node's owner exactly like Python (`_scope_sql(agent_id)`), so
    /// another agent's private memories never become link partners.
    ///
    /// Deliberate deviations, both documented in CerebroCortex-RS FINDINGS-1:
    /// bookkeeping tags (`session_note`, `priority:*`, …) never link — Python
    /// linked on ALL tags, which is how its 269 session notes inter-linked
    /// into a hairball; and concept candidates come from one combined query
    /// taking the strongest overlaps instead of Python's per-concept LIMIT
    /// loop (same weight formula, deterministic). Weights are Python's exactly:
    /// tags min(0.3 + 0.1·overlap, 0.8); concepts min(0.3 + 0.15·overlap, 0.9);
    /// affect max(0.3, 0.7 − |Δintensity|·0.5), top-salience partners, ≤3.
    async fn auto_link_locked(
        storage: &mut StorageCoordinator,
        node: &MemoryNode,
    ) -> Result<usize> {
        use crate::engines::dream::is_structural_tag;

        let scope = match &node.agent_id {
            Some(a) => VisibilityScope::for_agent(a.clone()),
            None => VisibilityScope::global(),
        };
        let now = Utc::now();
        let mut created = 0usize;
        let mut semantic_partners: std::collections::HashSet<MemoryId> =
            std::collections::HashSet::new();

        let push_link = |storage_links: &mut Vec<AssociativeLink>,
                         target: MemoryId,
                         link_type: crate::types::LinkType,
                         weight: f32| {
            storage_links.push(AssociativeLink {
                source_id:       node.id.clone(),
                target_id:       target,
                link_type,
                weight,
                created_at:      now,
                last_traversed:  None,
                traversal_count: 0,
            });
        };
        let mut links: Vec<AssociativeLink> = Vec::new();

        // Pass 1 — shared topical tags → semantic (Python weight formula).
        let topical: Vec<String> = node
            .tags
            .iter()
            .filter(|t| !is_structural_tag(t))
            .cloned()
            .collect();
        if !topical.is_empty() {
            let tag_set: std::collections::HashSet<&str> =
                topical.iter().map(String::as_str).collect();
            for (id, other_tags) in storage
                .sqlite
                .find_by_any_tag(&node.id, &topical, &scope, 5)
                .await?
            {
                let overlap = other_tags
                    .iter()
                    .filter(|t| tag_set.contains(t.as_str()))
                    .count();
                if overlap > 0 && semantic_partners.insert(id.clone()) {
                    let weight = (0.3 + 0.1 * overlap as f32).min(0.8);
                    push_link(&mut links, id, crate::types::LinkType::Semantic, weight);
                }
            }
        }

        // Pass 2 — shared concepts → semantic; strongest overlaps win the
        // (bounded) slots, pairs already tag-linked are skipped (Python's
        // ensure_link first-wins).
        let concepts: Vec<String> = node
            .metadata
            .get("concepts")
            .and_then(|c| serde_json::from_value(c.clone()).ok())
            .unwrap_or_default();
        if !concepts.is_empty() {
            let concept_set: std::collections::HashSet<&str> =
                concepts.iter().map(String::as_str).collect();
            let mut scored: Vec<(MemoryId, usize)> = storage
                .sqlite
                .find_by_any_concept(&node.id, &concepts, &scope, 15)
                .await?
                .into_iter()
                .filter(|(id, _)| !semantic_partners.contains(id))
                .map(|(id, other)| {
                    let overlap = other
                        .iter()
                        .filter(|c| concept_set.contains(c.as_str()))
                        .count();
                    (id, overlap)
                })
                .filter(|(_, overlap)| *overlap > 0)
                .collect();
            scored.sort_by_key(|(_, overlap)| std::cmp::Reverse(*overlap));
            for (id, overlap) in scored.into_iter().take(5) {
                if semantic_partners.insert(id.clone()) {
                    let weight = (0.3 + 0.15 * overlap as f32).min(0.9);
                    push_link(&mut links, id, crate::types::LinkType::Semantic, weight);
                }
            }
        }

        // Pass 3 — same emotional valence → affective, top-salience partners.
        if let Some(valence) = &node.emotional_valence {
            let valence_str = serde_json::to_value(valence)?
                .as_str()
                .map(str::to_string)
                .unwrap_or_default();
            for (id, intensity) in storage
                .sqlite
                .find_same_valence(&node.id, &valence_str, &scope, 3)
                .await?
            {
                let weight = (0.7 - (node.emotional_intensity - intensity).abs() * 0.5).max(0.3);
                push_link(&mut links, id, crate::types::LinkType::Affective, weight);
            }
        }

        for link in links {
            storage.sqlite.insert_link(&link).await?;
            if let Err(e) = storage.graph.add_edge(link.clone()) {
                tracing::warn!("auto-link graph mirror failed (rebuild heals): {e}");
            }
            created += 1;
        }
        Ok(created)
    }

    /// Backfill vector embeddings for memories that have none (rows migrated
    /// from the Python store predate the Rust embedder and are FTS5-only at
    /// recall until embedded). Runs the same embed-and-persist path
    /// `remember()` uses, oldest first, up to `limit` rows. Returns
    /// `(embedded, missing_found)`.
    pub async fn backfill_embeddings(&self, limit: usize) -> Result<(usize, usize)> {
        let storage = self.storage.read().await;
        if !storage.vector.is_embedder_loaded() {
            anyhow::bail!(
                "no embedder loaded — backfill requires embeddings enabled \
                 (CEREBRO_EMBED_MODEL must not be empty)"
            );
        }
        let missing = storage.sqlite.list_missing_embeddings(limit).await?;
        let found = missing.len();
        let mut embedded = 0usize;
        for (id, content) in missing {
            match storage.vector.embed_and_store(&id, &content).await {
                Ok(v) if !v.is_empty() => {
                    embedded += 1;
                    if embedded.is_multiple_of(50) {
                        tracing::info!("backfill: {embedded}/{found} embedded");
                    }
                }
                Ok(_)  => tracing::warn!("backfill: no vector produced for {}", id.0),
                Err(e) => tracing::warn!("backfill: embed failed for {}: {e}", id.0),
            }
        }
        Ok((embedded, found))
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

    /// Refresh the in-memory graph if ANOTHER process committed to the shared
    /// database since it was last built (CB-003 — cerebro-mcp and cerebro-api
    /// each hold their own graph over one SQLite file). Two-phase: a cheap
    /// `PRAGMA data_version` check under the read guard; only when stale do we
    /// take the write guard and rebuild (which re-checks, so a racing caller
    /// that already refreshed makes it a no-op). Own-process writes never
    /// trigger this — the pragma only moves on foreign commits, and own writes
    /// maintain the graph incrementally.
    pub async fn refresh_graph_if_stale(&self) -> Result<()> {
        let stale = self.storage.read().await.graph_is_stale().await?;
        if stale {
            self.storage.write().await.refresh_graph().await?;
        }
        Ok(())
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
        // Cross-process graph freshness (CB-003): pick up any memories/links
        // the OTHER front-end committed, so spreading sees them and the
        // association hits below aren't silently missing. One PRAGMA row when
        // fresh; a rebuild only after a foreign commit.
        self.refresh_graph_if_stale().await?;

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

        // Scope-visibility map (C-RS-003), bounded to the reachable frontier
        // (CB-008): `spread` can only ever touch the seeds' undirected
        // ≤MAX_HOPS neighbourhood, so that is ALL the visibility we fetch —
        // previously this collected every graph id and ran one
        // one-placeholder-per-memory IN query per recall (O(live-store) on the
        // hot path, hard-failing past SQLite's ~32k parameter limit). Safe
        // because spread treats a node missing from the map as NOT visible:
        // under-collection could only weaken the spread, never leak. Global
        // scope (agent_id == None) still short-circuits to all-visible —
        // matching Python's `agent_id is None` path in `_check_access` — but
        // NOT the shared-only federation scope, where private nodes must not
        // even influence the spread (can_access below enforces it per node).
        let frontier = crate::activation::reachable_frontier(&storage.graph.graph, &seeds);
        let visible_nodes: HashMap<NodeIndex, bool> = if scope.agent_id.is_none() && !scope.shared_only {
            frontier.iter().map(|&idx| (idx, true)).collect()
        } else {
            let frontier_ids: Vec<MemoryId> = frontier.iter()
                .filter_map(|&idx| storage.graph.graph.node_weight(idx).cloned())
                .collect();
            let vis_meta = storage.sqlite.get_visibility_meta(&frontier_ids).await?;
            frontier.iter()
                .map(|&idx| {
                    let visible = match storage.graph.graph.node_weight(idx)
                        .and_then(|id| vis_meta.get(id))
                    {
                        Some((vis, owner)) => scope.can_access(*vis, owner.as_ref()),
                        None => true, // not in DB → final SQLite filter handles it
                    };
                    (idx, visible)
                })
                .collect()
        };
        let (activated, walked) = spread_traced(&storage.graph.graph, &seeds, &visible_nodes);

        // The walk is part of the memory act: stamp the links spreading
        // actually used, so link decay and the fragmentation watchdog's
        // never_traversed_links_pct measure real usage (the colony's 4/4
        // "exactly 100.0%" finding — the write half was never wired). SQLite
        // is the source of truth; the in-memory copy picks the stamps up on
        // its next rebuild (cross-process refresh / restart), so recall stays
        // on the read guard with no new lock contention.
        let walked_keys: Vec<(MemoryId, MemoryId, crate::types::LinkType)> = walked
            .iter()
            .filter_map(|&eidx| storage.graph.graph.edge_weight(eidx))
            .map(|l| (l.source_id.clone(), l.target_id.clone(), l.link_type))
            .collect();
        storage.sqlite.record_traversals(&walked_keys).await?;

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
        // Cross-process graph freshness (CB-003): without this, an id the
        // OTHER front-end just committed fails the existence guard below with
        // a false "memory does not exist". Already under the write guard, so
        // refresh directly (no-op when nothing foreign was committed).
        storage.refresh_graph().await?;

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
