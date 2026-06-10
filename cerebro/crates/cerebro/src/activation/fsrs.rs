use crate::config::{FSRS_INITIAL_DIFFICULTY, FSRS_MAX_STABILITY, FSRS_MIN_STABILITY};

// ---------------------------------------------------------------------------
// FSRS forgetting curve
// ---------------------------------------------------------------------------

/// R(t, S) = (1 + t / (9 × S))^{-1}
///
/// Matches Python `retrievability(elapsed_days, stability)` exactly.
/// `elapsed_days` = 0 → returns 1.0. `stability` ≤ 0 → returns 0.0.
pub fn retrievability(elapsed_days: f32, stability: f32) -> f32 {
    if stability <= 0.0 { return 0.0; }
    if elapsed_days <= 0.0 { return 1.0; }
    (1.0 + elapsed_days / (9.0 * stability)).powi(-1)
}

// ---------------------------------------------------------------------------
// FSRS stability update
// ---------------------------------------------------------------------------

/// Update stability after a SUCCESSFUL recall.
///
/// FSRS SInc formula:
///   s_inc = exp(11 − d) × S^{−0.2} × (exp((1 − R) × 9) − 1)
///   new_S = S × (1 + s_inc)
///
/// Matches Python `update_stability_on_recall(stability, difficulty, current_retrievability)`.
pub fn update_stability_on_recall(stability: f32, difficulty: f32, current_r: f32) -> f32 {
    let s_inc = (11.0 - difficulty).exp()
        * stability.powf(-0.2)
        * ((1.0 - current_r) * 9.0).exp_m1(); // exp(x) - 1, avoids cancellation near 0
    let new_s = stability * (1.0 + s_inc);
    new_s.clamp(FSRS_MIN_STABILITY, FSRS_MAX_STABILITY)
}

/// Update stability after a LAPSE (memory not recalled when needed).
///
///   new_S = S × 0.3 × (11 − d)^0.2
///
/// Matches Python `update_stability_on_lapse(stability, difficulty)`.
pub fn update_stability_on_lapse(stability: f32, difficulty: f32) -> f32 {
    let new_s = stability * 0.3 * (11.0 - difficulty).powf(0.2);
    new_s.max(FSRS_MIN_STABILITY)
}

// ---------------------------------------------------------------------------
// FSRS difficulty update
// ---------------------------------------------------------------------------

/// Update difficulty after a recall.
///
///   delta   = −0.8 × (R − 0.5)
///   new_d   = 0.9 × (d + delta) + 0.1 × D_0
///
/// Easy recalls (high R) lower difficulty; hard recalls (low R) raise it.
/// Mean-reverts toward `FSRS_INITIAL_DIFFICULTY`.
///
/// Matches Python `update_difficulty_on_recall(difficulty, current_retrievability)`.
pub fn update_difficulty_on_recall(difficulty: f32, current_r: f32) -> f32 {
    let delta = -0.8 * (current_r - 0.5);
    let new_d = 0.9 * (difficulty + delta) + 0.1 * FSRS_INITIAL_DIFFICULTY;
    new_d.clamp(1.0, 10.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // retrievability
    // -----------------------------------------------------------------------

    #[test]
    fn just_accessed_is_one() {
        assert!((retrievability(0.0, 1.0) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn negative_elapsed_is_one() {
        assert!((retrievability(-1.0, 1.0) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn zero_stability_is_zero() {
        assert_eq!(retrievability(10.0, 0.0), 0.0);
    }

    #[test]
    fn one_day_s1() {
        // R(1, 1) = (1 + 1/9)^{-1} = 9/10 = 0.9
        let r = retrievability(1.0, 1.0);
        assert!((r - 0.9).abs() < 1e-6, "got {r}");
    }

    #[test]
    fn seven_days_s2() {
        // R(7, 2) = (1 + 7/18)^{-1} = 18/25 = 0.72
        let r = retrievability(7.0, 2.0);
        assert!((r - 0.72).abs() < 1e-6, "got {r}");
    }

    #[test]
    fn fsrs_halflife_point() {
        // At age = 9 × H, R = 0.5
        let r = retrievability(270.0, 30.0);
        assert!((r - 0.5).abs() < 1e-6, "got {r}");
    }

    // -----------------------------------------------------------------------
    // update_stability_on_recall
    // -----------------------------------------------------------------------

    #[test]
    fn recall_hits_ceiling() {
        // All standard cases hit FSRS_MAX_STABILITY — expected from fixtures
        let new_s = update_stability_on_recall(1.0, 5.0, 0.9);
        assert!((new_s - FSRS_MAX_STABILITY).abs() < 1e-4, "got {new_s}");
    }

    // -----------------------------------------------------------------------
    // update_stability_on_lapse
    // -----------------------------------------------------------------------

    #[test]
    fn lapse_s1_d5() {
        // Python: 1.0 * 0.3 * (6.0)^0.2 = 0.3 * 1.43097 ≈ 0.42929
        let new_s = update_stability_on_lapse(1.0, 5.0);
        assert!((new_s - 0.42929).abs() < 1e-4, "got {new_s}");
    }

    #[test]
    fn lapse_s5_d5() {
        let new_s = update_stability_on_lapse(5.0, 5.0);
        assert!((new_s - 2.14645).abs() < 1e-4, "got {new_s}");
    }

    #[test]
    fn lapse_cannot_go_below_min() {
        let new_s = update_stability_on_lapse(0.1, 10.0);
        assert!(new_s >= FSRS_MIN_STABILITY);
    }

    // -----------------------------------------------------------------------
    // update_difficulty_on_recall
    // -----------------------------------------------------------------------

    #[test]
    fn easy_recall_lowers_difficulty() {
        // R=0.9 → delta = -0.8*(0.9-0.5) = -0.32
        // new_d = 0.9*(5.0 - 0.32) + 0.1*5.0 = 0.9*4.68 + 0.5 = 4.212 + 0.5 = 4.712
        let new_d = update_difficulty_on_recall(5.0, 0.9);
        assert!((new_d - 4.712).abs() < 1e-4, "got {new_d}");
    }

    #[test]
    fn hard_recall_raises_difficulty() {
        // R=0.1 → delta = -0.8*(0.1-0.5) = 0.32 → new_d = 5.288
        let new_d = update_difficulty_on_recall(5.0, 0.1);
        assert!((new_d - 5.288).abs() < 1e-4, "got {new_d}");
    }

    #[test]
    fn neutral_recall_unchanged() {
        // R=0.5 → delta = 0 → new_d = 0.9*5.0 + 0.5 = 5.0
        let new_d = update_difficulty_on_recall(5.0, 0.5);
        assert!((new_d - 5.0).abs() < 1e-4, "got {new_d}");
    }
}
