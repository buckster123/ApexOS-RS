use crate::{models::MemoryNode, types::EmotionalValence};

const POSITIVE_MARKERS: &[&str] = &[
    "amazing", "breakthrough", "excellent", "great", "perfect",
    "solved", "success", "works", "love", "beautiful", "happy",
    "excited", "wonderful", "fantastic",
];
const NEGATIVE_MARKERS: &[&str] = &[
    "bug", "broken", "crash", "error", "fail", "frustrat",
    "terrible", "wrong", "hate", "awful", "disappoint", "stuck",
    "confused", "impossible", "nightmare",
];
const HIGH_AROUSAL_MARKERS: &[&str] = &[
    "!", "urgent", "critical", "panic", "incredible", "shocking",
    "breakthrough", "eureka", "finally", "nightmare", "disaster",
];

/// AffectEngine — amygdala.
/// Computes emotional valence, arousal, and salience modulation.
/// Mirrors Python engines/amygdala.py AffectEngine.
pub struct AffectEngine;

impl Default for AffectEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl AffectEngine {
    pub fn new() -> Self { Self }

    /// Analyze emotional content of text.
    ///
    /// Returns `(valence, arousal, salience_adjustment)`.
    /// - arousal: 0.0 (calm) → 1.0 (intense)
    /// - salience_adjustment: delta to add to existing salience
    pub fn analyze_emotion(&self, content: &str) -> (EmotionalValence, f32, f32) {
        let lower = content.to_lowercase();

        let pos_count     = POSITIVE_MARKERS.iter().filter(|&&m| lower.contains(m)).count();
        let neg_count     = NEGATIVE_MARKERS.iter().filter(|&&m| lower.contains(m)).count();
        // Check raw content for "!" (case-sensitive), lowercase for word markers
        let arousal_count = HIGH_AROUSAL_MARKERS.iter()
            .filter(|&&m| if m == "!" { content.contains(m) } else { lower.contains(m) })
            .count();

        let valence = if pos_count > 0 && neg_count > 0 {
            EmotionalValence::Mixed
        } else if pos_count > neg_count {
            EmotionalValence::Positive
        } else if neg_count > pos_count {
            EmotionalValence::Negative
        } else {
            EmotionalValence::Neutral
        };

        let emotion_intensity = pos_count + neg_count;
        let arousal = (0.3 + emotion_intensity as f32 * 0.15 + arousal_count as f32 * 0.1)
            .min(1.0);

        let mut salience_adj = 0.0_f32;
        if neg_count > 0 { salience_adj += (neg_count as f32 * 0.1).min(0.3); }
        if pos_count > 0 { salience_adj += (pos_count as f32 * 0.05).min(0.15); }
        if arousal_count > 0 { salience_adj += (arousal_count as f32 * 0.05).min(0.1); }

        (valence, arousal, salience_adj)
    }

    /// Apply emotional analysis to a memory node.
    ///
    /// Updates valence, arousal (taking the higher value), and salience.
    pub fn apply_emotion(&self, mut node: MemoryNode) -> MemoryNode {
        let (valence, arousal, salience_adj) = self.analyze_emotion(&node.content);
        node.emotional_valence   = Some(valence);
        node.emotional_intensity = node.emotional_intensity.max(arousal);
        node.salience            = (node.salience + salience_adj).clamp(0.1, 1.0);
        node
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::MemoryType;

    fn engine() -> AffectEngine { AffectEngine::new() }

    #[test]
    fn positive_valence() {
        let (v, _, _) = engine().analyze_emotion("amazing breakthrough, everything works perfectly");
        assert_eq!(v, EmotionalValence::Positive);
    }

    #[test]
    fn negative_valence() {
        let (v, _, _) = engine().analyze_emotion("terrible crash error, totally stuck");
        assert_eq!(v, EmotionalValence::Negative);
    }

    #[test]
    fn mixed_valence() {
        let (v, _, _) = engine().analyze_emotion("amazing result but there was a bug in the fail path");
        assert_eq!(v, EmotionalValence::Mixed);
    }

    #[test]
    fn neutral_valence() {
        let (v, _, _) = engine().analyze_emotion("the graph database stores nodes and links");
        assert_eq!(v, EmotionalValence::Neutral);
    }

    #[test]
    fn arousal_above_baseline() {
        let (_, arousal, _) = engine().analyze_emotion("critical panic! disaster unfolding");
        assert!(arousal > 0.3, "arousal should exceed baseline 0.3, got {arousal}");
    }

    #[test]
    fn negative_salience_adj_larger_than_positive() {
        let (_, _, neg_adj) = engine().analyze_emotion("bug crash error terrible");
        let (_, _, pos_adj) = engine().analyze_emotion("amazing excellent perfect wonderful");
        assert!(neg_adj > pos_adj, "negative outcomes should get bigger salience boost");
    }

    #[test]
    fn apply_emotion_updates_node() {
        let node = MemoryNode::new("amazing breakthrough solved everything!", MemoryType::Semantic);
        let original_salience = node.salience;
        let updated = engine().apply_emotion(node);
        assert!(updated.emotional_valence.is_some());
        assert!(updated.salience >= original_salience, "salience should increase for positive content");
        assert!(updated.emotional_intensity > 0.0);
    }
}
