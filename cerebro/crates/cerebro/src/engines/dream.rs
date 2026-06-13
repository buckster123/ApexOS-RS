use std::{collections::HashMap, sync::Arc};

use anyhow::Result;
use chrono::Utc;
use rand::{rngs::StdRng, seq::SliceRandom, Rng, SeedableRng};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::{
    cortex::CerebroCortex,
    models::AssociativeLink,
    storage::ListFilter,
    types::{LinkType, MemoryLayer, MemoryType, VisibilityScope},
};

// Mirror Python config.py DREAM_* constants
const MAX_LLM_CALLS: usize = 20;
const LLM_BUDGET_PATTERN: usize = 12;
const LLM_BUDGET_SCHEMA: usize = 4;
const LLM_BUDGET_REM: usize = 4;
const CLUSTER_MIN_SIZE: usize = 3;
const PRUNING_MIN_AGE_HOURS: i64 = 48;
const PRUNING_MAX_SALIENCE: f32 = 0.3;
const REM_SAMPLE_SIZE: usize = 20;
const REM_PAIR_CHECKS: usize = 10;
const REM_MIN_CONN_STRENGTH: f32 = 0.4;
// Python config.py EPISODE_AUTO_CLOSE_HOURS. Used by the pre-phase cleanup
// in run_cycle to auto-close stale open episodes.
const EPISODE_AUTO_CLOSE_HOURS: i64 = 24;

const SYSTEM_DREAM: &str =
    "You are the Dream Engine of CerebroCortex, a brain-analogous AI memory system. \
     You process memories during consolidation, extracting patterns, creating schemas, \
     and finding unexpected connections. Respond in structured JSON only.";

const PROMPT_EXTRACT_PATTERNS: &str = "Analyze these memories and extract reusable patterns or procedures.\n\
\nMemories:\n{memories}\n\
\nReturn a JSON array of extracted patterns. Each pattern should have:\n\
- \"content\": A clear, actionable procedure or pattern (1-3 sentences)\n\
- \"source_indices\": Which memory indices (0-based) this pattern comes from\n\
- \"tags\": Relevant tags for the pattern\n\
\nReturn ONLY valid JSON array. Example:\n\
[{\"content\": \"When debugging async code, check the event loop first, then verify awaits\", \
\"source_indices\": [0, 2], \"tags\": [\"debugging\", \"async\"]}]";

const PROMPT_FORM_SCHEMA: &str = "Analyze these related memories and form an abstract schema (general principle).\n\
\nMemories:\n{memories}\n\
\nWhat general principle, pattern, or lesson connects these memories?\n\
\nReturn JSON with:\n\
- \"content\": The abstract principle (1-2 sentences, general enough to apply beyond these specific cases)\n\
- \"tags\": Relevant categorization tags\n\
\nReturn ONLY valid JSON object. Example:\n\
{\"content\": \"Iterative refinement with user feedback produces better results than upfront design\", \
\"tags\": [\"methodology\", \"development\"]}";

const PROMPT_REM_CONNECT: &str = "You are looking at two seemingly unrelated memories. \
Find an unexpected but meaningful connection.\n\
\nMemory A: {memory_a}\nMemory B: {memory_b}\n\
\nIs there a meaningful connection between these? If yes, describe it.\n\
\nReturn JSON with:\n\
- \"connected\": true/false\n\
- \"link_type\": One of: semantic, causal, supports, contradicts\n\
- \"reason\": Brief explanation of the connection (1 sentence)\n\
- \"weight\": Connection strength 0.0-1.0\n\
\nReturn ONLY valid JSON object.";

/// Truncate `s` to at most `max_chars` characters on a char boundary.
/// Byte-indexed slicing (`&s[..n]`) panics when `n` lands mid-multibyte-char
/// (emoji, CJK, smart quotes); this is panic-safe.
fn truncate_chars(s: &str, max_chars: usize) -> &str {
    match s.char_indices().nth(max_chars) {
        Some((idx, _)) => &s[..idx],
        None => s,
    }
}

// ---------------------------------------------------------------------------
// DreamEngine — Default Mode Network for CerebroCortex
// 6 biologically-inspired consolidation phases:
//   1. SWS Replay      — algorithmic: Hebbian link strengthening
//   2. Pattern Extract — LLM: cluster → procedural memories
//   3. Schema Formation— LLM: episodes → abstract principles
//   4. Emotional Reproc— algorithmic: re-apply amygdala scores
//   5. Pruning         — algorithmic: delete isolated stale sensory memories
//   6. REM Recombine   — LLM: random pair sampling → new semantic links
// ---------------------------------------------------------------------------
pub struct DreamEngine {
    anthropic_key: Option<String>,
}

impl DreamEngine {
    pub fn new(anthropic_key: Option<String>) -> Self {
        Self { anthropic_key }
    }

    /// Run a full 6-phase dream consolidation cycle.
    /// `max_llm_calls` caps total LLM API calls (capped at MAX_LLM_CALLS=20).
    pub async fn run_cycle(
        &self,
        scope:         VisibilityScope,
        cortex:        Arc<CerebroCortex>,
        max_llm_calls: usize,
    ) -> Result<DreamReport> {
        let cycle_start = std::time::Instant::now();
        let mut calls_used = 0usize;
        let effective_budget = max_llm_calls.min(MAX_LLM_CALLS);

        // Pre-phase cleanup (C-RS-004): auto-close stale open episodes so they
        // don't accumulate across cycles. Mirrors Python's pre-phase step.
        match cortex.storage.read().await.sqlite
            .close_stale_episodes(EPISODE_AUTO_CLOSE_HOURS).await {
            Ok(n) if n > 0 => tracing::info!("dream pre-phase: auto-closed {n} stale episodes"),
            Ok(_)          => {}
            Err(e)         => tracing::warn!("dream pre-phase: close_stale_episodes failed: {e}"),
        }

        let p1 = self.sws_replay(&scope, &cortex).await;
        let p2 = self.pattern_extraction(
            &scope, &cortex, &mut calls_used,
            effective_budget.min(LLM_BUDGET_PATTERN), effective_budget,
        ).await;
        let p3 = self.schema_formation(
            &scope, &cortex, &mut calls_used,
            effective_budget.min(LLM_BUDGET_SCHEMA), effective_budget,
        ).await;
        let p4 = self.emotional_reprocessing(&scope, &cortex).await;
        let p5 = self.pruning(&scope, &cortex).await;
        let p6 = self.rem_recombination(
            &scope, &cortex, &mut calls_used,
            effective_budget.min(LLM_BUDGET_REM), effective_budget,
        ).await;

        let phases: Vec<PhaseResult> = [p1, p2, p3, p4, p5, p6]
            .into_iter()
            .map(|r| r.unwrap_or_else(|e| PhaseResult::failed(&e.to_string())))
            .collect();

        // Episodes consolidated = those replayed in phase 1 (SWS) — no longer
        // hardcoded 0 (C-RS-004).
        let episodes_consolidated = phases.first().map(|p| p.episodes_consolidated).unwrap_or(0);

        let report = DreamReport {
            agent_id:              scope.agent_id.as_ref().map(|a| a.0.clone()),
            episodes_consolidated,
            total_llm_calls:       calls_used,
            total_duration_secs:   cycle_start.elapsed().as_secs_f64(),
            success:               phases.iter().all(|p| p.success),
            phases,
        };

        // Persist to dream_reports table
        let report_id = format!("dream_{}", uuid::Uuid::new_v4().simple());
        // CB-024: surface a failed report persist instead of silently dropping it.
        if let Err(e) = cortex.storage.read().await.sqlite
            .save_dream_report(
                &report_id,
                scope.agent_id.as_ref().map(|a| a.0.as_str()),
                &report,
            )
            .await
        {
            tracing::warn!("dream report persist failed ({report_id}): {e}");
        }

        Ok(report)
    }

    // -------------------------------------------------------------------------
    // Phase 1: SWS Replay — algorithmic
    // Strengthen temporal links between co-episode memories (Hebbian learning)
    // -------------------------------------------------------------------------
    async fn sws_replay(
        &self,
        scope:  &VisibilityScope,
        cortex: &Arc<CerebroCortex>,
    ) -> Result<PhaseResult> {
        let start = std::time::Instant::now();
        let mut result = PhaseResult::new("sws_replay");

        let agent_id_str = scope.agent_id.as_ref().map(|a| a.0.as_str());
        let episodes = cortex.storage.read().await.sqlite
            .list_episodes(agent_id_str, 100).await?;

        for ep in &episodes {
            let ep_id = ep["id"].as_str().unwrap_or("");
            let mem_ids = cortex.storage.read().await.sqlite
                .get_episode_memory_ids(ep_id).await?;

            if mem_ids.len() < 2 { continue; }
            result.memories_processed += mem_ids.len();
            result.episodes_consolidated += 1;

            for window in mem_ids.windows(2) {
                let (src, tgt) = (window[0].clone(), window[1].clone());
                let existing = cortex.storage.read().await.sqlite
                    .list_links_from(&src).await?;

                let link = if let Some(existing_link) = existing.iter().find(|l| l.target_id == tgt) {
                    result.links_strengthened += 1;
                    let mut l = existing_link.clone();
                    l.weight = (l.weight + 0.08).min(1.0);
                    l
                } else {
                    result.links_created += 1;
                    AssociativeLink {
                        source_id:       src.clone(),
                        target_id:       tgt.clone(),
                        link_type:       LinkType::Temporal,
                        weight:          0.1,
                        created_at:      Utc::now(),
                        last_traversed:  None,
                        traversal_count: 0,
                    }
                };
                cortex.associate(src, tgt, link).await?;
            }
        }

        result.notes = format!(
            "Replayed {} episodes, {} links strengthened, {} created",
            episodes.len(), result.links_strengthened, result.links_created,
        );
        result.duration_secs = start.elapsed().as_secs_f64();
        Ok(result)
    }

    // -------------------------------------------------------------------------
    // Phase 2: Pattern Extraction — LLM-assisted
    // Cluster memories by tag, ask LLM to extract reusable procedures
    // -------------------------------------------------------------------------
    async fn pattern_extraction(
        &self,
        scope:          &VisibilityScope,
        cortex:         &Arc<CerebroCortex>,
        calls_used:     &mut usize,
        budget:         usize,
        overall_budget: usize,
    ) -> Result<PhaseResult> {
        let start = std::time::Instant::now();
        let mut result = PhaseResult::new("pattern_extraction");

        let key = match &self.anthropic_key {
            None => {
                result.notes = "skipped: no ANTHROPIC_API_KEY".into();
                result.duration_secs = start.elapsed().as_secs_f64();
                return Ok(result);
            }
            Some(k) => k.clone(),
        };

        let memories = cortex.storage.read().await.sqlite
            .list_memories_scoped(scope, &ListFilter { limit: 500, ..Default::default() })
            .await?;

        // tag → indices into `memories`
        let mut tag_map: HashMap<String, Vec<usize>> = HashMap::new();
        for (i, node) in memories.iter().enumerate() {
            for tag in &node.tags {
                tag_map.entry(tag.clone()).or_default().push(i);
            }
        }

        let clusters_total = tag_map.values().filter(|v| v.len() >= CLUSTER_MIN_SIZE).count();
        let mut budget_remaining = budget;
        let mut total_procedures = 0usize;

        for (tag, indices) in &tag_map {
            if indices.len() < CLUSTER_MIN_SIZE { continue; }
            if budget_remaining == 0 || *calls_used >= overall_budget { break; }

            let mem_text: String = indices.iter().take(10).enumerate()
                .map(|(i, &idx)| {
                    let content = &memories[idx].content;
                    format!("[{}] {}", i, truncate_chars(content, 200))
                })
                .collect::<Vec<_>>()
                .join("\n");

            let prompt = PROMPT_EXTRACT_PATTERNS.replace("{memories}", &mem_text);
            match llm_call(&key, SYSTEM_DREAM, &prompt).await {
                Ok(resp) => {
                    *calls_used     += 1;
                    result.llm_calls += 1;
                    budget_remaining -= 1;
                    result.memories_processed += indices.len().min(10);

                    if let Some(patterns) = parse_json_array(&resp) {
                        for pattern in patterns {
                            let content = pattern["content"].as_str()
                                .unwrap_or("").trim();
                            if content.len() < 10 { continue; }

                            let tags: Vec<String> = pattern["tags"].as_array()
                                .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
                                .unwrap_or_else(|| vec![tag.clone()]);

                            // Basic dedup: skip if first 40 chars match any existing memory
                            let prefix = truncate_chars(content, 40);
                            if memories.iter().any(|n| n.content.starts_with(prefix)) {
                                continue;
                            }

                            let procedure_tags = {
                                let mut t = vec!["procedure".to_string(), "dream_extracted".to_string()];
                                t.extend(tags);
                                t
                            };
                            if cortex.remember(
                                content.to_string(),
                                Some(MemoryType::Procedural),
                                Some(procedure_tags),
                                Some(0.8),
                                scope.clone(),
                            ).await.is_ok() {
                                total_procedures += 1;
                            }
                        }
                    }
                }
                Err(e) => tracing::warn!("Phase 2 LLM call failed: {e}"),
            }
        }

        result.procedures_extracted = total_procedures;
        result.notes = format!(
            "Extracted {} procedures from {} clusters (budget used: {}/{})",
            total_procedures, clusters_total, result.llm_calls, budget,
        );
        result.duration_secs = start.elapsed().as_secs_f64();
        Ok(result)
    }

    // -------------------------------------------------------------------------
    // Phase 3: Schema Formation — LLM-assisted
    // For each episode, ask LLM to form an abstract principle
    // -------------------------------------------------------------------------
    async fn schema_formation(
        &self,
        scope:          &VisibilityScope,
        cortex:         &Arc<CerebroCortex>,
        calls_used:     &mut usize,
        budget:         usize,
        overall_budget: usize,
    ) -> Result<PhaseResult> {
        let start = std::time::Instant::now();
        let mut result = PhaseResult::new("schema_formation");

        let key = match &self.anthropic_key {
            None => {
                result.notes = "skipped: no ANTHROPIC_API_KEY".into();
                result.duration_secs = start.elapsed().as_secs_f64();
                return Ok(result);
            }
            Some(k) => k.clone(),
        };

        let agent_id_str = scope.agent_id.as_ref().map(|a| a.0.as_str());
        let episodes = cortex.storage.read().await.sqlite
            .list_episodes(agent_id_str, 50).await?;

        let mut budget_remaining = budget;
        let mut total_schemas = 0usize;

        for ep in &episodes {
            if budget_remaining == 0 || *calls_used >= overall_budget { break; }

            let ep_id = ep["id"].as_str().unwrap_or("");
            let mem_ids = cortex.storage.read().await.sqlite
                .get_episode_memory_ids(ep_id).await?;

            if mem_ids.len() < 2 { continue; }

            let nodes = cortex.storage.read().await.sqlite
                .get_memories_by_ids(&mem_ids, scope).await?;

            if nodes.is_empty() { continue; }

            let mem_text: String = nodes.iter().take(10).enumerate()
                .map(|(i, n)| {
                    let content = &n.content;
                    format!("[{}] {}", i, truncate_chars(content, 200))
                })
                .collect::<Vec<_>>()
                .join("\n");

            let prompt = PROMPT_FORM_SCHEMA.replace("{memories}", &mem_text);
            match llm_call(&key, SYSTEM_DREAM, &prompt).await {
                Ok(resp) => {
                    *calls_used     += 1;
                    result.llm_calls += 1;
                    budget_remaining -= 1;
                    result.memories_processed += nodes.len();

                    if let Some(schema_data) = parse_json_object(&resp) {
                        let content = schema_data["content"].as_str()
                            .unwrap_or("").trim();
                        if content.len() < 10 { continue; }

                        let tags: Vec<String> = schema_data["tags"].as_array()
                            .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
                            .unwrap_or_default();

                        let source_ids: Vec<String> = mem_ids.iter()
                            .map(|id| id.0.clone()).collect();

                        let schema_tags = {
                            let mut t = vec![
                                "schema".to_string(),
                                "support_count:0".to_string(),
                                "dream_formed".to_string(),
                            ];
                            t.extend(tags);
                            t
                        };

                        if let Ok(mut node) = cortex.remember(
                            content.to_string(),
                            Some(MemoryType::Schematic),
                            Some(schema_tags),
                            Some(0.7),
                            scope.clone(),
                        ).await {
                            if let serde_json::Value::Object(ref mut map) = node.metadata {
                                map.insert("derived_from".to_string(), json!(source_ids));
                            } else {
                                node.metadata = json!({ "derived_from": source_ids });
                            }
                            // CB-024: only count work that actually persisted.
                            match cortex.storage.read().await.sqlite
                                .update_memory(&node).await
                            {
                                Ok(_) => {
                                    total_schemas += 1;
                                    result.links_created += mem_ids.len();
                                }
                                Err(e) => tracing::warn!(
                                    "Phase 3 schema persist failed for {}: {e}", node.id.0
                                ),
                            }
                        }
                    }
                }
                Err(e) => tracing::warn!("Phase 3 LLM call failed: {e}"),
            }
        }

        result.schemas_extracted = total_schemas;
        result.notes = format!(
            "Formed {} schemas from {} episodes (budget used: {}/{})",
            total_schemas, episodes.len(), result.llm_calls, budget,
        );
        result.duration_secs = start.elapsed().as_secs_f64();
        Ok(result)
    }

    // -------------------------------------------------------------------------
    // Phase 4: Emotional Reprocessing — algorithmic
    // Re-apply amygdala scoring to all episode memories
    // -------------------------------------------------------------------------
    async fn emotional_reprocessing(
        &self,
        scope:  &VisibilityScope,
        cortex: &Arc<CerebroCortex>,
    ) -> Result<PhaseResult> {
        let start = std::time::Instant::now();
        let mut result = PhaseResult::new("emotional_reprocessing");

        let agent_id_str = scope.agent_id.as_ref().map(|a| a.0.as_str());
        let episodes = cortex.storage.read().await.sqlite
            .list_episodes(agent_id_str, 100).await?;

        for ep in &episodes {
            let ep_id = ep["id"].as_str().unwrap_or("");
            let mem_ids = cortex.storage.read().await.sqlite
                .get_episode_memory_ids(ep_id).await?;

            for mid in &mem_ids {
                if let Some(node) = cortex.storage.read().await.sqlite
                    .get_memory(mid, scope).await?
                {
                    let enriched = cortex.amygdala.apply_emotion(node);
                    // CB-024: only count memories whose re-scored state persisted.
                    match cortex.storage.read().await.sqlite
                        .update_memory(&enriched).await
                    {
                        Ok(_)  => result.memories_processed += 1,
                        Err(e) => tracing::warn!(
                            "Phase 4 emotional persist failed for {}: {e}", enriched.id.0
                        ),
                    }
                }
            }
        }

        result.notes = format!(
            "Reprocessed emotions for {} episode memories", result.memories_processed,
        );
        result.duration_secs = start.elapsed().as_secs_f64();
        Ok(result)
    }

    // -------------------------------------------------------------------------
    // Phase 5: Pruning — algorithmic
    // Soft-delete isolated, low-salience, stale sensory-layer memories
    // -------------------------------------------------------------------------
    async fn pruning(
        &self,
        scope:  &VisibilityScope,
        cortex: &Arc<CerebroCortex>,
    ) -> Result<PhaseResult> {
        let start = std::time::Instant::now();
        let mut result = PhaseResult::new("pruning");

        let cutoff = Utc::now() - chrono::Duration::hours(PRUNING_MIN_AGE_HOURS);

        let all_memories = cortex.storage.read().await.sqlite
            .list_memories_scoped(scope, &ListFilter {
                limit: 1000,
                ..Default::default()
            })
            .await?;

        let mut pruned = 0usize;
        for node in &all_memories {
            if node.layer    != MemoryLayer::Sensory       { continue; }
            if node.salience >  PRUNING_MAX_SALIENCE       { continue; }
            if node.created_at > cutoff                    { continue; }

            let links = cortex.storage.read().await.sqlite
                .list_links_from(&node.id).await?;
            if !links.is_empty() { continue; }

            // CB-024: only count a prune that actually soft-deleted a live row.
            match cortex.storage.read().await.sqlite
                .delete_memory(&node.id).await
            {
                Ok(true)  => pruned += 1,
                Ok(false) => {} // no-op (already deleted) — don't over-count
                Err(e)    => tracing::warn!(
                    "Phase 5 prune failed for {}: {e}", node.id.0
                ),
            }
        }

        result.memories_processed = all_memories.len();
        result.memories_pruned    = pruned;
        result.notes = format!(
            "Pruned {} isolated sensory memories (of {} scanned)",
            pruned, all_memories.len(),
        );
        result.duration_secs = start.elapsed().as_secs_f64();
        Ok(result)
    }

    // -------------------------------------------------------------------------
    // Phase 6: REM Recombination — LLM-assisted
    // Sample random memory pairs, ask LLM for unexpected connections
    // -------------------------------------------------------------------------
    async fn rem_recombination(
        &self,
        scope:          &VisibilityScope,
        cortex:         &Arc<CerebroCortex>,
        calls_used:     &mut usize,
        budget:         usize,
        overall_budget: usize,
    ) -> Result<PhaseResult> {
        let start = std::time::Instant::now();
        let mut result = PhaseResult::new("rem_recombination");

        let key = match &self.anthropic_key {
            None => {
                result.notes = "skipped: no ANTHROPIC_API_KEY".into();
                result.duration_secs = start.elapsed().as_secs_f64();
                return Ok(result);
            }
            Some(k) => k.clone(),
        };

        let all_ids = cortex.storage.read().await.sqlite
            .list_all_memory_ids().await?;

        if all_ids.len() < 4 {
            result.notes = "Not enough memories for REM recombination".into();
            result.duration_secs = start.elapsed().as_secs_f64();
            return Ok(result);
        }

        let mut rng = StdRng::from_entropy();
        let sample_count = all_ids.len().min(REM_SAMPLE_SIZE);
        let sample_ids: Vec<_> = all_ids
            .choose_multiple(&mut rng, sample_count)
            .cloned()
            .collect();

        let nodes = cortex.storage.read().await.sqlite
            .get_memories_by_ids(&sample_ids, scope).await?;

        if nodes.len() < 2 {
            result.notes = "Not enough accessible memories for REM recombination".into();
            result.duration_secs = start.elapsed().as_secs_f64();
            return Ok(result);
        }

        result.memories_processed = nodes.len();

        let mut budget_remaining = budget;
        let mut links_created = 0usize;
        let mut pairs_checked = 0usize;

        for _ in 0..REM_PAIR_CHECKS {
            if budget_remaining == 0 || *calls_used >= overall_budget { break; }
            if nodes.len() < 2 { break; }

            let i = rng.gen_range(0..nodes.len());
            let mut j = rng.gen_range(0..nodes.len());
            while j == i { j = rng.gen_range(0..nodes.len()); }

            let node_a = &nodes[i];
            let node_b = &nodes[j];

            // 70% skip same-type pairs
            if node_a.memory_type == node_b.memory_type && rng.gen::<f32>() > 0.3 {
                continue;
            }

            // Skip if already linked (either direction)
            if cortex.storage.read().await.sqlite
                .has_link_between(&node_a.id, &node_b.id).await?
            {
                continue;
            }

            let prompt = PROMPT_REM_CONNECT
                .replace("{memory_a}", truncate_chars(&node_a.content, 300))
                .replace("{memory_b}", truncate_chars(&node_b.content, 300));

            match llm_call(&key, SYSTEM_DREAM, &prompt).await {
                Ok(resp) => {
                    *calls_used     += 1;
                    result.llm_calls += 1;
                    budget_remaining -= 1;
                    pairs_checked    += 1;

                    if let Some(conn) = parse_json_object(&resp) {
                        if conn["connected"].as_bool().unwrap_or(false) {
                            let weight = (conn["weight"].as_f64().unwrap_or(0.4) as f32)
                                .clamp(0.1, 0.9);

                            if weight >= REM_MIN_CONN_STRENGTH {
                                let link_type = match conn["link_type"].as_str().unwrap_or("semantic") {
                                    "causal"     => LinkType::Causal,
                                    "supports"   => LinkType::Supports,
                                    "contradicts"=> LinkType::Contradicts,
                                    _            => LinkType::Semantic,
                                };
                                let link = AssociativeLink {
                                    source_id:       node_a.id.clone(),
                                    target_id:       node_b.id.clone(),
                                    link_type,
                                    weight,
                                    created_at:      Utc::now(),
                                    last_traversed:  None,
                                    traversal_count: 0,
                                };
                                if cortex.associate(
                                    node_a.id.clone(), node_b.id.clone(), link,
                                ).await.is_ok() {
                                    links_created += 1;
                                }
                            }
                        }
                    }
                }
                Err(e) => tracing::warn!("Phase 6 LLM call failed: {e}"),
            }
        }

        result.links_created = links_created;
        result.notes = format!(
            "Checked {} pairs, created {} new connections (budget used: {}/{})",
            pairs_checked, links_created, result.llm_calls, budget,
        );
        result.duration_secs = start.elapsed().as_secs_f64();
        Ok(result)
    }
}

// ---------------------------------------------------------------------------
// LLM client — Anthropic Messages API (claude-haiku: fast, cheap, good at JSON)
// ---------------------------------------------------------------------------
async fn llm_call(api_key: &str, system: &str, prompt: &str) -> Result<String> {
    let client = reqwest::Client::new();
    let body = json!({
        "model":       "claude-haiku-4-5-20251001",
        "max_tokens":  1024,
        "system":      system,
        "messages":    [{"role": "user", "content": prompt}]
    });
    let resp = client
        .post("https://api.anthropic.com/v1/messages")
        .header("x-api-key",           api_key)
        .header("anthropic-version",   "2023-06-01")
        .header("content-type",        "application/json")
        .json(&body)
        .send()
        .await?;
    let data: serde_json::Value = resp.json().await?;
    let text = data["content"][0]["text"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("unexpected Anthropic response: {}", data))?
        .to_string();
    Ok(text)
}

// ---------------------------------------------------------------------------
// JSON extraction helpers — handle markdown fences and preamble text
// ---------------------------------------------------------------------------
fn strip_fences(text: &str) -> String {
    let cleaned = text.trim();
    if cleaned.contains("```") {
        cleaned.lines()
            .filter(|l| !l.trim().starts_with("```"))
            .collect::<Vec<_>>()
            .join("\n")
            .trim()
            .to_string()
    } else {
        cleaned.to_string()
    }
}

fn parse_json_array(text: &str) -> Option<Vec<serde_json::Value>> {
    let cleaned = strip_fences(text);
    if let Ok(serde_json::Value::Array(arr)) = serde_json::from_str::<serde_json::Value>(&cleaned) {
        return Some(arr);
    }
    let start = cleaned.find('[')?;
    let end   = cleaned.rfind(']')?;
    if end <= start { return None; }
    serde_json::from_str(&cleaned[start..=end]).ok()
}

fn parse_json_object(text: &str) -> Option<serde_json::Value> {
    let cleaned = strip_fences(text);
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&cleaned) {
        if v.is_object() { return Some(v); }
    }
    let start = cleaned.find('{')?;
    let end   = cleaned.rfind('}')?;
    if end <= start { return None; }
    serde_json::from_str(&cleaned[start..=end]).ok()
}

// ---------------------------------------------------------------------------
// Report types — mirror Python DreamReport / PhaseReport
// ---------------------------------------------------------------------------
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DreamReport {
    pub agent_id:              Option<String>,
    pub episodes_consolidated: usize,
    pub total_llm_calls:       usize,
    pub total_duration_secs:   f64,
    pub success:               bool,
    pub phases:                Vec<PhaseResult>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PhaseResult {
    pub phase:                String,
    pub episodes_consolidated: usize,
    pub memories_processed:   usize,
    pub links_created:        usize,
    pub links_strengthened:   usize,
    pub memories_pruned:      usize,
    pub schemas_extracted:    usize,
    pub procedures_extracted: usize,
    pub llm_calls:            usize,
    pub duration_secs:        f64,
    pub notes:                String,
    pub success:              bool,
}

impl PhaseResult {
    fn new(phase: &str) -> Self {
        Self {
            phase:                phase.into(),
            episodes_consolidated: 0,
            memories_processed:   0,
            links_created:        0,
            links_strengthened:   0,
            memories_pruned:      0,
            schemas_extracted:    0,
            procedures_extracted: 0,
            llm_calls:            0,
            duration_secs:        0.0,
            notes:                String::new(),
            success:              true,
        }
    }

    fn failed(notes: &str) -> Self {
        let mut r = Self::new("unknown");
        r.success = false;
        r.notes   = notes.into();
        r
    }
}

#[cfg(test)]
mod tests {
    use super::truncate_chars;

    #[test]
    fn truncate_mid_emoji_does_not_panic() {
        // "a" + 4-byte emoji repeated. A byte slice at e.g. 3 would split the
        // emoji and panic; truncate_chars must cut on a char boundary instead.
        let s = "a🦀🦀🦀🦀";
        for n in 0..=10 {
            let out = truncate_chars(s, n);
            // valid UTF-8 prefix, never longer than the source
            assert!(s.starts_with(out));
        }
        // first char only
        assert_eq!(truncate_chars(s, 1), "a");
        // two chars = "a" + one crab (1 + 4 bytes)
        assert_eq!(truncate_chars(s, 2), "a🦀");
        // beyond length returns the whole string
        assert_eq!(truncate_chars(s, 100), s);
    }
}
