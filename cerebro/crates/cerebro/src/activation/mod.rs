pub mod actr;
pub mod fsrs;
pub mod spreading;

pub use actr::{actr_activation, base_level_activation};
pub use fsrs::{retrievability, update_difficulty_on_recall,
               update_stability_on_lapse, update_stability_on_recall};
pub use spreading::spread;

// ---------------------------------------------------------------------------
// ACT-R recall probability — sigmoid over activation
// ---------------------------------------------------------------------------

/// P(t) = sigmoid( (A(t) - τ) / s )
///
/// `threshold` (τ) and `noise` (s) from config: default 0.0 and 0.4.
/// Matches Python `recall_probability(activation, threshold, noise)`.
pub fn recall_probability(activation: f32, threshold: f32, noise: f32) -> f32 {
    if activation == f32::NEG_INFINITY { return 0.0; }
    if noise <= 0.0 {
        return if activation >= threshold { 1.0 } else { 0.0 };
    }
    let x = ((activation - threshold) / noise).clamp(-20.0, 20.0);
    1.0 / (1.0 + (-x).exp())
}

// ---------------------------------------------------------------------------
// Combined recall score — blends four signals into [0, 1]
// ---------------------------------------------------------------------------

/// Weighted blend of vector similarity, ACT-R activation, FSRS retrievability,
/// and emotional salience.
///
/// Matches Python `combined_recall_score()` exactly:
///   activation_score = recall_probability(base_level + associative)
///   score = w_v * sim + w_a * activation_score + w_r * fsrs + w_s * salience
pub fn recall_score(
    vector_sim: f32,
    base_level: f32,
    associative: f32,
    fsrs: f32,
    salience: f32,
) -> f32 {
    use crate::config::{
        ACTR_NOISE, ACTR_RETRIEVAL_THRESHOLD,
        SCORE_WEIGHT_ACTIVATION, SCORE_WEIGHT_RETRIEVABILITY,
        SCORE_WEIGHT_SALIENCE, SCORE_WEIGHT_VECTOR,
    };
    let activation_score = recall_probability(base_level + associative, ACTR_RETRIEVAL_THRESHOLD, ACTR_NOISE);
    let score = SCORE_WEIGHT_VECTOR        * vector_sim.clamp(0.0, 1.0)
              + SCORE_WEIGHT_ACTIVATION    * activation_score
              + SCORE_WEIGHT_RETRIEVABILITY * fsrs.clamp(0.0, 1.0)
              + SCORE_WEIGHT_SALIENCE       * salience.clamp(0.0, 1.0);
    score.clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recall_prob_at_threshold_is_half() {
        let p = recall_probability(0.0, 0.0, 0.4);
        assert!((p - 0.5).abs() < 1e-6, "got {p}");
    }

    #[test]
    fn recall_prob_above_threshold() {
        let p = recall_probability(1.0, 0.0, 0.4);
        let expected = 0.92414182_f32;
        assert!((p - expected).abs() < 1e-5, "got {p}");
    }

    #[test]
    fn recall_prob_neg_inf_is_zero() {
        assert_eq!(recall_probability(f32::NEG_INFINITY, 0.0, 0.4), 0.0);
    }

    #[test]
    fn recall_score_range() {
        let s = recall_score(0.8, -0.5, 0.1, 0.7, 0.6);
        assert!(s >= 0.0 && s <= 1.0, "out of range: {s}");
    }
}
