use std::collections::HashMap;

use chrono::Utc;

use crate::{
    activation::{actr_activation, recall_score, retrievability},
    models::MemoryNode,
    types::MemoryId,
};

/// ExecutiveEngine — prefrontal cortex.
/// Ranks recall candidates using combined activation/FSRS/salience scores.
/// Prospective memory and layer promotion are wired to storage in cortex.rs (step 7).
/// Mirrors Python engines/prefrontal.py ExecutiveEngine.
pub struct ExecutiveEngine;

impl ExecutiveEngine {
    pub fn new() -> Self { Self }

    /// Rank a set of memory nodes by combined recall score.
    ///
    /// Blends four signals (mirror of Python `combined_recall_score`):
    /// - vector similarity (optional, per-ID)
    /// - ACT-R base-level activation
    /// - FSRS retrievability
    /// - salience
    ///
    /// Returns IDs sorted highest → lowest score.
    pub fn rank_results(
        &self,
        nodes:       &[MemoryNode],
        vector_sims: Option<&HashMap<MemoryId, f32>>,
        assoc_scores: Option<&HashMap<MemoryId, f32>>,
    ) -> Vec<(MemoryId, f32)> {
        let now = Utc::now();
        let mut scored: Vec<(MemoryId, f32)> = nodes.iter().map(|node| {
            let vector_sim = vector_sims
                .and_then(|m| m.get(&node.id))
                .copied()
                .unwrap_or(0.0);
            let assoc = assoc_scores
                .and_then(|m| m.get(&node.id))
                .copied()
                .unwrap_or(0.0);

            let base_level = actr_activation(&node.access_times, now);

            let elapsed_days = node.strength.last_review
                .map(|lr| (now - lr).num_seconds().max(0) as f32 / 86_400.0)
                .unwrap_or(0.0);
            let fsrs = retrievability(elapsed_days, node.strength.stability);

            let score = recall_score(vector_sim, base_level, assoc, fsrs, node.salience);
            (node.id.clone(), score)
        }).collect();

        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        scored
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::MemoryType;

    fn engine() -> ExecutiveEngine { ExecutiveEngine::new() }

    #[test]
    fn rank_results_empty() {
        let ranked = engine().rank_results(&[], None, None);
        assert!(ranked.is_empty());
    }

    #[test]
    fn rank_results_higher_salience_wins_when_equal_activation() {
        let mut high = MemoryNode::new("important critical finding", MemoryType::Semantic);
        let mut low  = MemoryNode::new("background trivia note", MemoryType::Semantic);
        high.salience = 0.9;
        low.salience  = 0.2;

        let ranked = engine().rank_results(&[low, high.clone()], None, None);
        assert_eq!(ranked[0].0, high.id, "high-salience memory should rank first");
    }

    #[test]
    fn rank_results_all_scores_in_range() {
        let nodes: Vec<MemoryNode> = (0..5).map(|i| {
            let mut n = MemoryNode::new(format!("memory {i}"), MemoryType::Semantic);
            n.salience = i as f32 * 0.2;
            n
        }).collect();
        for (_, score) in engine().rank_results(&nodes, None, None) {
            assert!(score >= 0.0 && score <= 1.0, "score out of range: {score}");
        }
    }

    #[test]
    fn rank_results_vector_sim_boosts_rank() {
        let a = MemoryNode::new("semantic memory", MemoryType::Semantic);
        let b = MemoryNode::new("unrelated content", MemoryType::Semantic);
        let a_id = a.id.clone();
        let b_id = b.id.clone();

        let mut sims = HashMap::new();
        sims.insert(a_id.clone(), 0.95_f32);
        sims.insert(b_id, 0.05_f32);

        let ranked = engine().rank_results(&[a, b], Some(&sims), None);
        assert_eq!(ranked[0].0, a_id, "high-similarity memory should rank first");
    }
}
