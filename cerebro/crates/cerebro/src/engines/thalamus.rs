use crate::{
    models::{MemoryNode, StrengthState},
    types::{AgentId, MemoryLayer, MemoryType, Visibility},
};

const MIN_CONTENT_LENGTH: usize = 10;

const HIGH_SALIENCE_KEYWORDS: &[&str] = &[
    "important", "critical", "bug", "fix", "error", "breakthrough",
    "discovery", "remember", "never", "always", "warning", "danger",
    "lesson", "learned", "insight",
];

/// GatingEngine — thalamus.
/// Filters incoming information and initializes memory parameters.
/// Mirrors Python engines/thalamus.py GatingEngine.
pub struct GatingEngine;

impl Default for GatingEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl GatingEngine {
    pub fn new() -> Self { Self }

    /// Evaluate incoming content and create a MemoryNode if it passes gating.
    ///
    /// Gate 1: minimum content length (10 chars).
    /// Gate 2: deduplication — handled in cortex.rs (needs DB).
    /// Returns None if gated out.
    pub fn evaluate_input(
        &self,
        content: &str,
        memory_type:  Option<MemoryType>,
        tags:         Option<Vec<String>>,
        salience:     Option<f32>,
        agent_id:     Option<AgentId>,
        visibility:   Visibility,
    ) -> Option<MemoryNode> {
        if content.trim().len() < MIN_CONTENT_LENGTH {
            return None;
        }

        let resolved_type     = memory_type.unwrap_or_else(|| self.classify_type(content));
        let tags              = tags.unwrap_or_default();
        let resolved_salience = salience.unwrap_or_else(|| self.estimate_salience(content, &tags));
        let initial_layer     = self.assign_layer(resolved_salience, resolved_type);
        let stability         = self.initial_stability(resolved_salience);

        let mut node = MemoryNode::new(content, resolved_type);
        node.tags       = tags;
        node.agent_id   = agent_id;
        node.visibility = visibility;
        node.layer      = initial_layer;
        node.salience   = resolved_salience;
        node.strength   = StrengthState { stability, ..StrengthState::default() };

        Some(node)
    }

    /// Heuristic memory type classification — keyword-based fast path.
    pub fn classify_type(&self, content: &str) -> MemoryType {
        let lower = content.to_lowercase();

        if ["step 1", "1)", "first,", "when you", "how to",
            "workflow", "procedure", "algorithm", "strategy"]
            .iter().any(|m| lower.contains(m))
        {
            return MemoryType::Procedural;
        }

        if ["felt", "feeling", "amazing", "frustrat", "excit",
            "disappoint", "breakthrough", "terrible", "love", "hate"]
            .iter().any(|m| lower.contains(m))
        {
            return MemoryType::Affective;
        }

        if ["need to", "should", "todo", "plan to", "will",
            "going to", "revisit", "later", "eventually"]
            .iter().any(|m| lower.contains(m))
        {
            return MemoryType::Prospective;
        }

        if ["then", "after", "before", "yesterday", "today",
            "session", "deployed", "tried", "encountered"]
            .iter().any(|m| lower.contains(m))
        {
            return MemoryType::Episodic;
        }

        MemoryType::Semantic
    }

    /// Estimate how memorable this content is — [0.1, 1.0].
    pub fn estimate_salience(&self, content: &str, tags: &[String]) -> f32 {
        let mut score = 0.5_f32;
        let lower = content.to_lowercase();

        let keyword_hits = HIGH_SALIENCE_KEYWORDS.iter()
            .filter(|&&kw| lower.contains(kw))
            .count();
        score += (keyword_hits as f32 * 0.1).min(0.3);

        if content.len() > 200 {
            score += 0.1;
        } else if content.len() < 30 {
            score -= 0.1;
        }

        if !tags.is_empty() {
            score += (tags.len() as f32 * 0.05).min(0.15);
        }

        if content.contains('?') { score += 0.05; }
        if content.contains('!') { score += 0.05; }

        score.clamp(0.1, 1.0)
    }

    /// Assign initial memory layer based on salience and type.
    pub fn assign_layer(&self, salience: f32, memory_type: MemoryType) -> MemoryLayer {
        if matches!(memory_type, MemoryType::Procedural | MemoryType::Schematic) {
            return MemoryLayer::Working;
        }
        if salience >= 0.4 { MemoryLayer::Working } else { MemoryLayer::Sensory }
    }

    /// Initial FSRS stability based on salience — range [0.5, 3.0] days.
    pub fn initial_stability(&self, salience: f32) -> f32 {
        0.5 + salience * 2.5
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn engine() -> GatingEngine { GatingEngine::new() }

    #[test]
    fn gate_short_content_returns_none() {
        assert!(engine().evaluate_input("too short", None, None, None, None, Visibility::Shared).is_none());
        assert!(engine().evaluate_input("", None, None, None, None, Visibility::Shared).is_none());
    }

    #[test]
    fn gate_valid_content_returns_node() {
        let node = engine().evaluate_input(
            "This is a valid memory about Rust programming.",
            None, None, None, None, Visibility::Shared,
        );
        assert!(node.is_some());
    }

    #[test]
    fn classify_type_procedural() {
        assert_eq!(engine().classify_type("step 1: open the file"), MemoryType::Procedural);
        assert_eq!(engine().classify_type("how to deploy the service"), MemoryType::Procedural);
    }

    #[test]
    fn classify_type_affective() {
        assert_eq!(engine().classify_type("I felt amazing after the breakthrough"), MemoryType::Affective);
    }

    #[test]
    fn classify_type_prospective() {
        assert_eq!(engine().classify_type("need to revisit this later"), MemoryType::Prospective);
    }

    #[test]
    fn classify_type_episodic() {
        assert_eq!(engine().classify_type("then after we deployed the service"), MemoryType::Episodic);
    }

    #[test]
    fn classify_type_default_semantic() {
        assert_eq!(engine().classify_type("the graph database stores nodes and edges"), MemoryType::Semantic);
    }

    #[test]
    fn salience_keyword_boosts() {
        let base = engine().estimate_salience("the graph has nodes", &[]);
        let boosted = engine().estimate_salience("critical bug error in the system", &[]);
        assert!(boosted > base, "keyword hits should increase salience");
    }

    #[test]
    fn salience_tags_boost() {
        let no_tags  = engine().estimate_salience("some content here in this file", &[]);
        let with_tags = engine().estimate_salience("some content here in this file", &["rust".into(), "perf".into()]);
        assert!(with_tags > no_tags);
    }

    #[test]
    fn assign_layer_procedural_always_working() {
        assert_eq!(engine().assign_layer(0.1, MemoryType::Procedural), MemoryLayer::Working);
        assert_eq!(engine().assign_layer(0.1, MemoryType::Schematic),  MemoryLayer::Working);
    }

    #[test]
    fn assign_layer_low_salience_is_sensory() {
        assert_eq!(engine().assign_layer(0.3, MemoryType::Semantic), MemoryLayer::Sensory);
    }

    #[test]
    fn initial_stability_formula() {
        let s = engine().initial_stability(0.5);
        assert!((s - 1.75).abs() < 1e-6, "0.5 + 0.5*2.5 = 1.75, got {s}");
        assert!((engine().initial_stability(0.0) - 0.5).abs() < 1e-6);
        assert!((engine().initial_stability(1.0) - 3.0).abs() < 1e-6);
    }
}
