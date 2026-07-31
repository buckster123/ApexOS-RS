//! The review procedure — Fabrica M1b (docs/fabrica.md, "Runtime supervision").
//!
//! The pure decision core of worker/cell supervision: a residency POSTURE and
//! a six-observable WORD go in, exactly ONE single-line remediation comes out.
//! The function is TOTAL — every posture × word combination has a defined
//! answer (the exhaustive test walks all of them) — and it never returns a
//! subtree restart (the anti-thrash rule: one line at a time).
//!
//! Centrality precedence (the charter's order):
//!   1. EXISTENCE — a terminal child is reaped (Done is the least stable
//!      state: one review tick, never a zombie); a live child whose turn died
//!      (no TurnComplete will ever come) fails; an edge with no demand is
//!      cancelled.
//!   2. THE TWO CENTERS — child Budget and parent Capacity. Parking an
//!      overdue waiting worker IS the capacity action: RAM is the parent's
//!      capacity, and park frees it (the send-revives contract keeps it safe).
//!   3. CORRECTNESS — Verified / Horizon are censused at M1b and become
//!      actionable with measures (M1c) and epochs (M1d). No kill switches
//!      here: a batch deadline is a REPORT bound, never an executioner.
//!
//! The observable BUILDERS (what makes a bit true for a given worker) live in
//! worker.rs — they own the clocks and the maps. This module owns only the
//! decision and the scheduling math: golden offsets (Weyl — sibling reviews
//! structurally cannot phase-lock) and Fibonacci backoff (quiet waiting
//! states are re-read at widening intervals; LIVE workers never back off —
//! stall detection latency is semantics, not housekeeping).

use std::time::Duration;

/// Where the worker's residency sits, for review purposes. This is the axis
/// the word deliberately does not encode: the same word can demand different
/// single-line actions depending on what kind of clock the worker is on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Posture {
    /// A turn is in flight: Running, or Blocked suspended on an approval
    /// (the human's clock — its Progress bit is built true, stall-exempt).
    Live,
    /// Waiting for input, no turn: Idle (yielded) or verdict-Blocked.
    Waiting,
    /// A GATE/DIAMOND cell's worker held before admission — waiting on its
    /// own descendants (the J guard's clock).
    BarrierWait,
    /// Done / Failed / Cancelled.
    Terminal,
}

/// The six binary observables — child triple, then parent triple.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Word {
    /// Child: the thing is moving (clocks inside bounds; for a barrier,
    /// "the wait is still legitimate" — subtree open and timeout unexpired).
    pub progress: bool,
    /// Child: budget remains (steps/deadline headroom).
    pub budget: bool,
    /// Child: no dishonesty signal (turn not errored; terminal Done left
    /// evidence).
    pub verified: bool,
    /// Parent: someone still wants this work.
    pub demand: bool,
    /// Parent: the parent can afford/integrate it.
    pub capacity: bool,
    /// Parent: time remains on the parent's horizon (batch/mandala deadline).
    pub horizon: bool,
}

impl Word {
    /// Census key bits, child triple then parent triple: "PBV DCH" → "110111".
    pub fn bits(&self) -> String {
        let b = |v: bool| if v { '1' } else { '0' };
        [
            b(self.progress), b(self.budget), b(self.verified),
            b(self.demand), b(self.capacity), b(self.horizon),
        ]
        .iter()
        .collect()
    }
}

/// Posture letter for census keys ("L:110111").
pub fn census_key(posture: Posture, word: &Word) -> String {
    let p = match posture {
        Posture::Live => 'L',
        Posture::Waiting => 'W',
        Posture::BarrierWait => 'B',
        Posture::Terminal => 'T',
    };
    format!("{p}:{}", word.bits())
}

/// Exactly one single-line remediation — never a subtree restart. The
/// APPLICATION of each line stays in worker.rs and reuses the shipped
/// terminal/park/open paths (behavioral identity with the pre-M1b
/// supervision is the refactor's correctness bar).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Remediation {
    /// Nothing to do; censused and rescheduled.
    Healthy,
    /// The live turn is dead (stall past the step timeout) → Failed, with
    /// the full terminal trail.
    Fail,
    /// The waiting worker sat past the idle TTL → Parked (RAM freed, JSONL
    /// stays truth, a send revives).
    Park,
    /// Terminal worker seen by review → censused once, dropped from the
    /// schedule, cell/closure bookkeeping runs. Anti-zombie.
    Reap,
    /// The barrier's wait is over (subtree closed, or the J-guard timeout
    /// fired) → open the gate: append the descendant evidence list and let
    /// admission take it.
    OpenBarrier,
    /// No demand for the edge — cancel with the honest terminal trail.
    /// (Unreached by M1b's conservative builders; the arm is defined so the
    /// table is total and later slices arm the observable, not the code.)
    Cancel,
}

/// The total decision procedure. See the module doc for the precedence
/// argument; the exhaustive test walks every posture × word.
pub fn review(posture: Posture, word: &Word) -> Remediation {
    match posture {
        // Existence, first clause: terminal work is reaped within one tick,
        // whatever the word says — a stale Done is itself a defect.
        Posture::Terminal => Remediation::Reap,
        Posture::Live => {
            if !word.progress {
                Remediation::Fail // the turn died; no completion will come
            } else if !word.demand {
                Remediation::Cancel
            } else {
                Remediation::Healthy
            }
        }
        Posture::Waiting => {
            if !word.demand {
                Remediation::Cancel
            } else if !word.progress {
                Remediation::Park // the capacity action: free the RAM
            } else {
                Remediation::Healthy
            }
        }
        Posture::BarrierWait => {
            if !word.demand {
                Remediation::Cancel // don't open a join nobody wants
            } else if !word.progress {
                Remediation::OpenBarrier
            } else {
                Remediation::Healthy
            }
        }
    }
}

// ── Scheduling math ─────────────────────────────────────────────────────────

/// φ⁻¹ — the Weyl constant. The ONE place the golden ratio is mechanism:
/// offsets i·φ⁻¹ (mod 1) are maximally spread for any N, so sibling review
/// pulses structurally cannot phase-lock into herd storms.
const PHI_INV: f64 = 0.618_033_988_749_895;

/// Worker `i`'s review phase offset within `period` — deterministic,
/// well-spread, no RNG.
pub fn golden_offset(i: u64, period: Duration) -> Duration {
    let frac = (i as f64 * PHI_INV).fract();
    period.mul_f64(frac)
}

/// Fibonacci backoff for quiet re-reviews: base × [1,1,2,3,5,8], capped at
/// 8× forever after. Attempt 0 = the first re-review.
pub fn fib_backoff(attempt: u32, base: Duration) -> Duration {
    const FIB: [u32; 6] = [1, 1, 2, 3, 5, 8];
    let mult = FIB[(attempt as usize).min(FIB.len() - 1)];
    base.saturating_mul(mult)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all_words() -> Vec<Word> {
        (0u8..64)
            .map(|n| Word {
                progress: n & 0b100_000 != 0,
                budget: n & 0b010_000 != 0,
                verified: n & 0b001_000 != 0,
                demand: n & 0b000_100 != 0,
                capacity: n & 0b000_010 != 0,
                horizon: n & 0b000_001 != 0,
            })
            .collect()
    }

    #[test]
    fn review_is_total_and_terminal_always_reaps() {
        // The whole input space: 4 postures × 64 words. Every combination
        // answers (totality is the charter's stability argument), and the
        // anti-zombie rule holds unconditionally.
        for posture in [Posture::Live, Posture::Waiting, Posture::BarrierWait, Posture::Terminal] {
            for w in all_words() {
                let r = review(posture, &w);
                if posture == Posture::Terminal {
                    assert_eq!(r, Remediation::Reap, "{w:?}");
                }
                // No remediation is ever a restart — the enum simply has no
                // such arm; this asserts the healthy default is reachable
                // only via full-true-ish words.
                if r == Remediation::Healthy {
                    assert!(w.progress && w.demand, "healthy requires progress+demand: {w:?}");
                }
            }
        }
    }

    #[test]
    fn precedence_existence_before_centers_before_correctness() {
        let base = Word { progress: true, budget: true, verified: true, demand: true, capacity: true, horizon: true };
        // A dead live turn fails regardless of every other bit.
        for w in all_words().into_iter().filter(|w| !w.progress) {
            assert_eq!(review(Posture::Live, &w), Remediation::Fail, "{w:?}");
        }
        // Demand loss cancels a waiting/barrier worker before any TTL park.
        let no_demand = Word { demand: false, progress: false, ..base };
        assert_eq!(review(Posture::Waiting, &no_demand), Remediation::Cancel);
        assert_eq!(review(Posture::BarrierWait, &no_demand), Remediation::Cancel);
        // TTL overdue parks; barrier overdue opens — same word, different
        // posture: the axis the word deliberately does not encode.
        let overdue = Word { progress: false, ..base };
        assert_eq!(review(Posture::Waiting, &overdue), Remediation::Park);
        assert_eq!(review(Posture::BarrierWait, &overdue), Remediation::OpenBarrier);
        // Correctness bits alone (verified/horizon low) never kill at M1b —
        // censused, not executed. The batch deadline is a report bound.
        let unverified = Word { verified: false, horizon: false, budget: false, ..base };
        assert_eq!(review(Posture::Live, &unverified), Remediation::Healthy);
        assert_eq!(review(Posture::Waiting, &unverified), Remediation::Healthy);
    }

    #[test]
    fn census_keys_are_posture_tagged_bitstrings() {
        let w = Word { progress: true, budget: true, verified: false, demand: true, capacity: true, horizon: true };
        assert_eq!(w.bits(), "110111");
        assert_eq!(census_key(Posture::Live, &w), "L:110111");
        assert_eq!(census_key(Posture::BarrierWait, &w), "B:110111");
    }

    #[test]
    fn golden_offsets_spread_and_stay_inside_the_period() {
        let period = Duration::from_secs(30);
        let offsets: Vec<Duration> = (0..16).map(|i| golden_offset(i, period)).collect();
        for (i, a) in offsets.iter().enumerate() {
            assert!(*a < period, "offset {i} out of period");
            for (j, b) in offsets.iter().enumerate() {
                if i != j {
                    // Weyl spacing: no two of the first 16 collide (they
                    // can't — φ is irrational; this pins the implementation).
                    assert_ne!(a, b, "offsets {i} and {j} collide");
                }
            }
        }
        assert_eq!(golden_offset(0, period), Duration::ZERO);
    }

    #[test]
    fn fib_backoff_grows_and_caps() {
        let base = Duration::from_secs(30);
        let seq: Vec<u64> = (0..8).map(|a| fib_backoff(a, base).as_secs()).collect();
        assert_eq!(seq, vec![30, 30, 60, 90, 150, 240, 240, 240]);
    }
}
