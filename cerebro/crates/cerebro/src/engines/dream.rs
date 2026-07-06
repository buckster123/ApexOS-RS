use std::{collections::HashMap, sync::Arc};

use anyhow::Result;
use chrono::Utc;
use rand::{rngs::StdRng, seq::SliceRandom, Rng, SeedableRng};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::{
    config::{FSRS_INITIAL_DIFFICULTY, PRUNE_CANDIDATE_SALIENCE},
    cortex::CerebroCortex,
    models::{AssociativeLink, MemoryNode},
    storage::ListFilter,
    types::{LinkType, MemoryLayer, MemoryType, VisibilityScope},
};

// Mirror Python config.py DREAM_* constants
const MAX_LLM_CALLS: usize = 20;
const LLM_BUDGET_PATTERN: usize = 12;
const LLM_BUDGET_SCHEMA: usize = 4;
const LLM_BUDGET_REM: usize = 4;
// Exo-evolution E2 (variation): LLM budget for the mutation phase that refines
// struggling procedures into fresh variants. Shares the overall MAX_LLM_CALLS
// cap with the other phases (gated on `calls_used`), so on a small brain the
// hungrier pattern/schema passes are served first and variation uses the slack.
const LLM_BUDGET_VARIATION: usize = 4;
const CLUSTER_MIN_SIZE: usize = 3;
const PRUNING_MIN_AGE_HOURS: i64 = 48;
const PRUNING_MAX_SALIENCE: f32 = 0.3;
const REM_SAMPLE_SIZE: usize = 20;
const REM_PAIR_CHECKS: usize = 10;
const REM_MIN_CONN_STRENGTH: f32 = 0.4;
// Evolutionary layer, build slice #1 (docs/evolutionary-layer.md): skill
// distillation inside phase 3. A topical tag shared by at least this many
// SUCCESSFUL procedures is a skill cluster worth abstracting. Lower than
// CLUSTER_MIN_SIZE (3) — two graded-successful procedures already justify
// "how this is done in general", and skills must be able to form on a young
// brain that has only used a handful of procedures.
const SKILL_CLUSTER_MIN_SIZE: usize = 2;
// A procedure must score at least this on `procedure_fitness` to count as a
// successful skill source. 0.8 = the store-default salience with zero recorded
// failures; any failure (FSRS difficulty above the 5.0 baseline) drops a
// procedure below the bar. Slice #3 will sharpen this once failure also demotes
// salience.
const SKILL_MIN_FITNESS: f32 = 0.8;
// Python config.py EPISODE_AUTO_CLOSE_HOURS. Used by the pre-phase cleanup
// in run_cycle to auto-close stale open episodes.
const EPISODE_AUTO_CLOSE_HOURS: i64 = 24;

// Exo-evolution: niche competition (docs/evolutionary-layer.md). Procedures that
// address the same task — share a topical tag — are rivals competing for one
// niche. The `skill_competition` phase marks the fittest the niche CHAMPION and
// makes clearly-dominated rivals decay toward the prune floor, so a subtly-worse
// procedure can no longer survive indefinitely just because it is never *failed*:
// relative inferiority now demotes, not only absolute failure.
//
// A niche needs at least this many ELIGIBLE contenders to hold a contest.
const COMPETITION_MIN_NICHE: usize = 2;
// A procedure must have at least this many GRADED outcomes (ledger
// successes+failures) to win or lose a contest. Below it the procedure is exempt
// — it cannot be killed for losing a competition it has barely entered. This
// protects novelty: a fresh procedure gets exercised before selection can retire
// it (variation is the raw material the loop selects over).
const COMPETITION_MIN_GRADED_USES: u32 = 2;
// The champion's confidence-aware fitness (Wilson lower bound) must lead a rival
// by more than this for the rival to count as dominated. A near-tie is not a
// loss — only a clear fitness gap demotes.
const COMPETITION_MARGIN: f32 = 0.15;
// Bounded salience decay applied to a dominated rival per losing cycle. Gradual
// by design (not a one-shot kill): a 0.8-salience loser needs several losing
// dreams to reach the 0.25 prune floor, leaving room to recover if its win/loss
// record improves before then.
const COMPETITION_PENALTY: f32 = 0.1;

// Exo-evolution E2 (variation/mutation, docs/evolutionary-layer.md). The dream
// engine refines genuinely-underperforming procedures into fresh variants so
// competition (E1) has new alternatives to select among, rather than only what
// the agent happened to store. A procedure is a refinement candidate only if it
// has actually FAILED (≥1 recorded failure) and its Wilson fitness is below this
// ceiling — i.e. it underperforms in practice, not merely unproven. The variant
// inherits the parent's niche tags and starts un-graded, so E1 treats it as
// novelty (exempt) until it has been tried, then it competes on its own record.
const REFINE_FITNESS_CEILING: f32 = 0.5;

// E2b (merge/recombination): both parents of a merge must be genuinely PROVEN —
// confidence-aware (Wilson) fitness at or above this floor — so the engine
// recombines two strong-but-different approaches to a task, not a champion with a
// weak straggler (that's refinement's job). ~3 clean successes clears it.
const MERGE_FITNESS_FLOOR: f32 = 0.4;

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

const PROMPT_DISTILL_SKILL: &str = "These are concrete procedures the agent has used SUCCESSFULLY on real tasks. \
Distil them into ONE abstract, reusable skill — the general way this kind of task is done.\n\
\nSuccessful procedures:\n{procedures}\n\
\nReturn JSON with:\n\
- \"content\": The abstract skill (1-3 sentences, general enough to apply to new instances of this task, written as actionable guidance — not a summary of these specific runs)\n\
- \"tags\": Relevant skill tags\n\
\nReturn ONLY valid JSON object. Example:\n\
{\"content\": \"To debug async Rust, first confirm the runtime is multi-threaded, then trace each .await for a held lock or missing poll before suspecting the logic itself\", \
\"tags\": [\"debugging\", \"async\", \"rust\"]}";

const PROMPT_REFINE_PROCEDURE: &str = "This is a procedure the agent has tried on real tasks, \
but it has been UNDERPERFORMING — it fails or underdelivers more often than it should.\n\
\nProcedure:\n{procedure}\n\
\nPropose ONE improved variant of it: keep what works, fix the most likely cause of failure, \
and make it more reliable. It must be a concrete, actionable procedure for the SAME kind of \
task — an improved how-to, not a critique of the original.\n\
\nReturn JSON with:\n\
- \"content\": The improved procedure (1-4 sentences, actionable guidance)\n\
- \"tags\": Relevant topical tags (the task domain)\n\
\nReturn ONLY valid JSON object. Example:\n\
{\"content\": \"To restart a wedged service, first capture its last 50 log lines, stop it, \
verify the port is released, then start and confirm readiness before declaring success\", \
\"tags\": [\"ops\", \"service-restart\"]}";

const PROMPT_MERGE_PROCEDURES: &str = "These are TWO procedures the agent uses for the SAME \
kind of task. Each has a solid track record, but they take different approaches.\n\
\nProcedure A:\n{procedure_a}\n\nProcedure B:\n{procedure_b}\n\
\nSynthesise ONE hybrid procedure that combines the strengths of both — the most reliable \
way to do this task, drawing the best steps from each. It must be a concrete, actionable \
procedure, not a comparison of the two.\n\
\nReturn JSON with:\n\
- \"content\": The combined procedure (1-4 sentences, actionable guidance)\n\
- \"tags\": Relevant topical tags (the task domain)\n\
\nReturn ONLY valid JSON object.";

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

/// Outcome-graded fitness of a procedure, in `[0,1]` — the selection signal for
/// skill distillation (build slice #1). Until slice #3 sharpens
/// `record_procedure_outcome`, the only outcome signals on a node are salience
/// (rises on every graded use: +0.1 success / +0.02 failure) and FSRS difficulty
/// (rises ONLY on failure). So reward salience and penalise difficulty above the
/// `FSRS_INITIAL_DIFFICULTY` (5.0) baseline: an unfailed procedure keeps its full
/// salience, a repeatedly-failed one is driven toward zero and excluded.
fn procedure_fitness(node: &MemoryNode) -> f32 {
    let salience = node.salience.clamp(0.0, 1.0);
    let span = (10.0 - FSRS_INITIAL_DIFFICULTY).max(f32::EPSILON);
    let failure_penalty =
        ((node.strength.difficulty - FSRS_INITIAL_DIFFICULTY).max(0.0) / span).clamp(0.0, 1.0);
    (salience * (1.0 - failure_penalty)).clamp(0.0, 1.0)
}

/// True for tags that mark a memory's *role* rather than its *topic*. Excluded
/// when clustering procedures (into skills, and into competition niches) so
/// clusters form on subject matter (e.g. "slint", "async") not on bookkeeping
/// markers.
fn is_structural_tag(tag: &str) -> bool {
    matches!(
        tag,
        "procedure"
            | "dream_extracted"
            | "schema"
            | "skill"
            | "dream_formed"
            | "dream_distilled"
            | "dream_mutated"
            | "dream_merged"
            | "skill_champion"
            | "prune_candidate"
    ) || tag.starts_with("support_count:")
}

/// Read a procedure's win/loss ledger (`metadata.outcomes`, written by
/// `record_procedure_outcome`) as `(successes, failures)`. `None` if the
/// procedure has never been graded — the ungraded case the competition phase
/// treats as exempt.
fn outcome_stats(node: &MemoryNode) -> Option<(u32, u32)> {
    let o = node.metadata.get("outcomes")?;
    let s = o.get("successes").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
    let f = o.get("failures").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
    if s + f == 0 {
        None
    } else {
        Some((s, f))
    }
}

/// 95% Wilson score interval lower bound of the success proportion for `s`
/// successes in `n = s + f` graded outcomes. Confidence-aware: a perfect 1/1
/// (LB ≈ 0.21) scores well below a proven 8/10 (LB ≈ 0.49), so a single lucky
/// success cannot dominate a niche over a procedure with a real track record.
/// Returns 0.0 for `n == 0`.
fn wilson_lower_bound(successes: u32, failures: u32) -> f32 {
    let n = (successes + failures) as f32;
    if n <= 0.0 {
        return 0.0;
    }
    const Z: f32 = 1.96; // 95% confidence
    let z2 = Z * Z;
    let phat = successes as f32 / n;
    let denom = 1.0 + z2 / n;
    let centre = phat + z2 / (2.0 * n);
    let margin = Z * ((phat * (1.0 - phat) + z2 / (4.0 * n)) / n).sqrt();
    ((centre - margin) / denom).clamp(0.0, 1.0)
}

/// Confidence-aware fitness used to rank procedures within a competition niche.
/// `Some(wilson_lower_bound)` once a procedure has at least
/// `COMPETITION_MIN_GRADED_USES` graded outcomes; `None` (exempt — neither
/// champion nor demotable) below that bar, which is how novelty is protected.
fn competitive_fitness(node: &MemoryNode) -> Option<f32> {
    let (s, f) = outcome_stats(node)?;
    if s + f < COMPETITION_MIN_GRADED_USES {
        return None;
    }
    Some(wilson_lower_bound(s, f))
}

/// The tag the competition phase stamps on the fittest procedure of a niche. The
/// single source of truth for the literal — read it via [`is_skill_champion`]
/// rather than re-spelling the string at call sites.
pub const SKILL_CHAMPION_TAG: &str = "skill_champion";

/// True if `node` is the marked champion of ≥1 competition niche
/// (carries [`SKILL_CHAMPION_TAG`], stamped by the dream `skill_competition`
/// phase).
pub fn is_skill_champion(node: &MemoryNode) -> bool {
    node.tags.iter().any(|t| t == SKILL_CHAMPION_TAG)
}

/// Champion-aware retrieval rank for a procedure — higher surfaces first. The
/// public counterpart to the competition phase's ordering, so the MCP retrieval
/// path (`find_relevant_procedures`, `cognitive_bootstrap`) prefers the same
/// procedures competition crowned rather than inventing a second, drifting
/// notion of "best". A niche champion gets a full +1.0 band so it always leads
/// the field; within a band, a graded procedure ranks by its confidence-aware
/// Wilson fitness and an ungraded one falls back to its raw salience — novelty
/// stays visible instead of sinking under proven-but-mediocre rivals.
pub fn retrieval_rank(node: &MemoryNode) -> f32 {
    let base = competitive_fitness(node).unwrap_or(node.salience);
    if is_skill_champion(node) {
        base + 1.0
    } else {
        base
    }
}

/// The `metadata.derived_from` provenance ids of a memory (the procedures/episodes
/// it was distilled or mutated from), or empty if none.
fn derived_from_ids(node: &MemoryNode) -> Vec<String> {
    node.metadata.get("derived_from")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
        .unwrap_or_default()
}

/// True if some existing procedure is an UNGRADED variant of `parent_id` — a
/// `dream_mutated` child deriving from it that has not yet been tried. Used so the
/// variation phase never spawns a second untested variant of a parent before the
/// first has earned an outcome (no untested-variant pile-up).
fn has_pending_variant(procedures: &[MemoryNode], parent_id: &str) -> bool {
    procedures.iter().any(|n| {
        outcome_stats(n).is_none()
            && n.tags.iter().any(|t| t == "dream_mutated")
            && derived_from_ids(n).iter().any(|id| id == parent_id)
    })
}

/// Indices of procedures worth refining (E2 variation), worst-fitness first.
/// A candidate is a graded procedure that has actually failed (≥1 failure) and
/// whose Wilson fitness is below `REFINE_FITNESS_CEILING` — genuinely
/// underperforming — and that does not already have an untested variant awaiting
/// trial. Pure (no I/O) so candidate selection is unit-testable. Sorting worst
/// first means a limited LLM budget refines the most-struggling procedures.
fn refine_candidates(procedures: &[MemoryNode]) -> Vec<usize> {
    let mut scored: Vec<(usize, f32)> = procedures.iter().enumerate()
        .filter_map(|(i, n)| {
            let (s, f) = outcome_stats(n)?;
            if f == 0 {
                return None; // never failed → not struggling
            }
            let fit = wilson_lower_bound(s, f);
            if fit >= REFINE_FITNESS_CEILING {
                return None;
            }
            Some((i, fit))
        })
        .filter(|&(i, _)| !has_pending_variant(procedures, &procedures[i].id.0))
        .collect();
    scored.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
    scored.into_iter().map(|(i, _)| i).collect()
}

/// True if a niche already holds an UNGRADED merged child — a `dream_merged`
/// procedure carrying `niche_tag` that has not yet been tried. Used so the merge
/// pass doesn't recombine the same niche every cycle before its last hybrid has
/// earned an outcome (no untested-merge pile-up).
fn has_pending_merge(procedures: &[MemoryNode], niche_tag: &str) -> bool {
    procedures.iter().any(|n| {
        outcome_stats(n).is_none()
            && n.tags.iter().any(|t| t == "dream_merged")
            && n.tags.iter().any(|t| t == niche_tag)
    })
}

/// Whether two procedure contents share a 40-char prefix — a cheap "these are
/// effectively the same procedure" check, so the merge pass doesn't recombine
/// near-duplicates (no new strength to combine).
fn same_prefix(a: &str, b: &str) -> bool {
    truncate_chars(a, 40) == truncate_chars(b, 40)
}

/// Pairs of procedure indices worth merging (E2b recombination): within each
/// niche, the fittest procedure and the best DISTINCT-content partner, both at or
/// above `MERGE_FITNESS_FLOOR` (two *proven* approaches, not a champion + a weak
/// straggler). Niches that already hold an untested merged child are skipped, and
/// each unordered pair is emitted once. Pure (no I/O) so it is unit-testable.
fn merge_candidates(procedures: &[MemoryNode]) -> Vec<(usize, usize)> {
    let fitness: Vec<Option<f32>> = procedures.iter().map(competitive_fitness).collect();

    // Niche = topical tag → indices of the eligible procedures carrying it.
    let mut niches: HashMap<String, Vec<usize>> = HashMap::new();
    for (i, node) in procedures.iter().enumerate() {
        if fitness[i].is_none() { continue; }
        for tag in &node.tags {
            if is_structural_tag(tag) { continue; }
            niches.entry(tag.clone()).or_default().push(i);
        }
    }

    let mut pairs: Vec<(usize, usize)> = Vec::new();
    let mut seen: std::collections::HashSet<(usize, usize)> = std::collections::HashSet::new();

    for (tag, members) in &niches {
        if members.len() < 2 { continue; }
        if has_pending_merge(procedures, tag) { continue; }

        // Rank the niche by fitness, descending.
        let mut ranked = members.clone();
        ranked.sort_by(|&a, &b| {
            fitness[b].unwrap()
                .partial_cmp(&fitness[a].unwrap())
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        let best = ranked[0];
        if fitness[best].unwrap() < MERGE_FITNESS_FLOOR { continue; }
        // Best partner that is also proven and not a near-duplicate of the best.
        let partner = ranked.iter().skip(1).copied().find(|&idx| {
            fitness[idx].unwrap() >= MERGE_FITNESS_FLOOR
                && !same_prefix(&procedures[best].content, &procedures[idx].content)
        });

        if let Some(p) = partner {
            let key = if best < p { (best, p) } else { (p, best) };
            if seen.insert(key) {
                pairs.push(key);
            }
        }
    }

    pairs
}

/// What the competition phase should do to one procedure this cycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CompetitionAction {
    /// Champion of ≥1 niche — mark `skill_champion`. A champion of any niche is
    /// never demoted, even if it loses another (it is the best at *something*).
    Champion,
    /// Dominated in ≥1 niche and champion of none — selected against (decay +
    /// prune-flag at the floor).
    Demote,
    /// Neither — left alone (any stale `skill_champion` marker is cleared).
    Leave,
}

/// Result of one competition round, indexed parallel to the input procedure
/// slice, plus the contest count for the phase report.
struct CompetitionVerdicts {
    actions:          Vec<CompetitionAction>,
    niches_contested: usize,
}

/// Pure core of the competition phase: from the procedure set, decide each
/// procedure's action this cycle. Separated from I/O so the full selection rule
/// (including "champion of any niche is never demoted") is unit-testable without a
/// database. A procedure is a contender in a niche only when `competitive_fitness`
/// is `Some` (graded past the novelty bar); ungraded procedures never appear in a
/// niche, so they can neither win nor be dominated → `Leave`.
fn compute_competition_verdicts(procedures: &[MemoryNode]) -> CompetitionVerdicts {
    let fitness: Vec<Option<f32>> = procedures.iter().map(competitive_fitness).collect();

    // Niche = topical tag → indices of the eligible procedures carrying it.
    let mut niches: HashMap<String, Vec<usize>> = HashMap::new();
    for (i, node) in procedures.iter().enumerate() {
        if fitness[i].is_none() { continue; }
        for tag in &node.tags {
            if is_structural_tag(tag) { continue; }
            niches.entry(tag.clone()).or_default().push(i);
        }
    }

    let mut won_any   = vec![false; procedures.len()];
    let mut dominated = vec![false; procedures.len()];
    let mut niches_contested = 0usize;

    for members in niches.values() {
        if members.len() < COMPETITION_MIN_NICHE { continue; }
        niches_contested += 1;

        // Champion = highest fitness in the niche (ties keep the first seen).
        let champion = *members.iter()
            .max_by(|&&a, &&b| {
                fitness[a].unwrap()
                    .partial_cmp(&fitness[b].unwrap())
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .unwrap();
        won_any[champion] = true;

        let champ_fit = fitness[champion].unwrap();
        for &idx in members {
            if idx == champion { continue; }
            if champ_fit - fitness[idx].unwrap() > COMPETITION_MARGIN {
                dominated[idx] = true;
            }
        }
    }

    // Champion wins over Demote: being best at one thing beats losing another.
    let actions = (0..procedures.len())
        .map(|i| {
            if won_any[i] {
                CompetitionAction::Champion
            } else if dominated[i] {
                CompetitionAction::Demote
            } else {
                CompetitionAction::Leave
            }
        })
        .collect();

    CompetitionVerdicts { actions, niches_contested }
}

// ---------------------------------------------------------------------------
// DreamEngine — Default Mode Network for CerebroCortex
// 6 biologically-inspired consolidation phases:
//   1. SWS Replay      — algorithmic: Hebbian link strengthening
//   2. Pattern Extract — LLM: cluster → procedural memories
//   3. Schema Formation— LLM: episodes → principles + successful procedure
//                              clusters → abstract skills (evolutionary layer)
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
        // Variation/mutation (exo-evolution E2): refine struggling procedures into
        // fresh variants so competition has new alternatives. LLM-assisted; shares
        // the overall budget (served after the hungrier pattern/schema passes).
        let pv = self.variation(
            &scope, &cortex, &mut calls_used,
            effective_budget.min(LLM_BUDGET_VARIATION), effective_budget,
        ).await;
        let p4 = self.emotional_reprocessing(&scope, &cortex).await;
        // Niche competition (exo-evolution E1): runs after distillation so champions
        // are marked, and BEFORE pruning so a rival demoted to the floor this
        // cycle can be retired the same cycle. Algorithmic — no LLM budget.
        let pc = self.skill_competition(&scope, &cortex).await;
        let p5 = self.pruning(&scope, &cortex).await;
        let p6 = self.rem_recombination(
            &scope, &cortex, &mut calls_used,
            effective_budget.min(LLM_BUDGET_REM), effective_budget,
        ).await;

        let phases: Vec<PhaseResult> = [p1, p2, p3, pv, p4, pc, p5, p6]
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
        /// Cosine-similarity floor above which a dream-extracted candidate counts as a
        /// RE-DISCOVERY of an existing procedure rather than a novel one. bge-small
        /// paraphrases of the same lesson land ~0.85–0.95; topically-adjacent-but-
        /// different procedures land ~0.6–0.8 — 0.86 keeps genuine variants novel.
        const REDISCOVERY_SIMILARITY: f32 = 0.86;

        /// Recurring evidence is stronger evidence — but boundedly: small bump,
        /// hard cap below champion territory.
        fn reinforced_salience(current: f32) -> f32 {
            (current + 0.05).min(0.95)
        }

        /// If `content` is a semantic near-duplicate of an existing PROCEDURAL memory,
        /// reinforce that memory (salience bump + a rediscovery ledger in metadata)
        /// and return true. Embeddings-only: on FTS5 nodes (no vec0/embedder) this is
        /// always false and the caller's prefix dedup remains the only gate.
        async fn reinforce_if_rediscovery(
            cortex:  &Arc<CerebroCortex>,
            content: &str,
            scope:   &VisibilityScope,
        ) -> bool {
            let storage = cortex.storage.read().await;
            if !storage.vector.is_vec_available() || !storage.vector.is_embedder_loaded() {
                return false;
            }
            let (scope_sql, scope_params) = scope.sql_filter();
            let Ok(hits) = storage.vector.search(content, 3, &scope_sql, &scope_params).await else {
                return false;
            };
            let near_ids: Vec<crate::types::MemoryId> = hits
                .into_iter()
                .filter(|(_, score)| *score >= REDISCOVERY_SIMILARITY)
                .map(|(id, _)| id)
                .collect();
            if near_ids.is_empty() {
                return false;
            }
            let Ok(nodes) = storage.sqlite.get_memories_by_ids(&near_ids, scope).await else {
                return false;
            };
            let Some(mut existing) = nodes
                .into_iter()
                .find(|n| n.memory_type == MemoryType::Procedural)
            else {
                return false;
            };
            existing.salience = reinforced_salience(existing.salience);
            if let serde_json::Value::Object(ref mut map) = existing.metadata {
                let n = map.get("rediscovered_count").and_then(|v| v.as_u64()).unwrap_or(0);
                map.insert("rediscovered_count".to_string(), json!(n + 1));
                map.insert("last_rediscovered".to_string(), json!(Utc::now().to_rfc3339()));
            }
            if storage.sqlite.update_memory(&existing).await.is_err() {
                return false; // couldn't reinforce — let the candidate store normally
            }
            tracing::info!(
                id = %existing.id.0,
                salience = existing.salience,
                "dream re-discovery reinforced existing procedure"
            );
            true
        }
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
        let mut total_rediscovered = 0usize;

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

                            // Semantic re-discovery gate (colony C2): the old 40-char
                            // prefix check let the LLM re-mint the same lesson nightly
                            // in different words — apex2 found five near-identical
                            // procedures across five nights, never merged, each too
                            // weak to trust. With embeddings available, check the
                            // candidate against the WHOLE store: a procedural hit at
                            // ≥ REDISCOVERY_SIMILARITY means the dream re-discovered
                            // something known → REINFORCE the existing memory (small
                            // capped salience bump — recurring evidence IS stronger
                            // evidence — plus a rediscovery ledger in metadata)
                            // instead of storing a fragment, counted honestly in the
                            // report so the journal can say novel vs re-discovered.
                            if reinforce_if_rediscovery(cortex, content, scope).await {
                                total_rediscovered += 1;
                                continue;
                            }

                            // Prefix dedup kept as the FTS5-only (Nano) fallback —
                            // BM25 scores aren't a similarity, so no threshold works
                            // there — and as a cheap first line everywhere.
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

        result.procedures_extracted    = total_procedures;
        result.procedures_rediscovered = total_rediscovered;
        result.notes = format!(
            "Extracted {} novel procedures from {} clusters, reinforced {} re-discoveries (budget used: {}/{})",
            total_procedures, clusters_total, total_rediscovered, result.llm_calls, budget,
        );
        result.duration_secs = start.elapsed().as_secs_f64();
        Ok(result)
    }

    // -------------------------------------------------------------------------
    // Phase 3: Schema Formation — LLM-assisted
    //
    // Two consolidation passes into the `schematic` layer, sharing the phase
    // LLM budget:
    //   (a) episode → abstract principle (the original behavior)
    //   (b) successful procedure-cluster → abstract SKILL (evolutionary layer,
    //       build slice #1) — the Darwinian loop's consolidation step. See
    //       docs/evolutionary-layer.md.
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

        // Reserve roughly half the phase budget for skill distillation (pass b)
        // so an episode-rich brain still grows skills. Skills also consume any
        // budget the episode pass leaves unused.
        let skill_reserve = (budget / 2).max(1);

        for ep in &episodes {
            if budget_remaining <= skill_reserve || *calls_used >= overall_budget { break; }

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

        // ---- (b) Skill distillation: successful procedure clusters → skills --
        // Load procedures and existing schemas once (the latter for dedup).
        let procedures = cortex.storage.read().await.sqlite
            .list_memories_scoped(scope, &ListFilter {
                memory_type: Some(MemoryType::Procedural),
                limit: 500,
                ..Default::default()
            })
            .await?;

        // Selection pressure: keep only outcome-successful procedures.
        let successful: Vec<&MemoryNode> = procedures.iter()
            .filter(|n| procedure_fitness(n) >= SKILL_MIN_FITNESS)
            .collect();

        // Cluster the survivors by topical tag (structural markers excluded).
        let mut tag_map: HashMap<String, Vec<usize>> = HashMap::new();
        for (i, node) in successful.iter().enumerate() {
            for tag in &node.tags {
                if is_structural_tag(tag) { continue; }
                tag_map.entry(tag.clone()).or_default().push(i);
            }
        }

        // Existing schemas — prefix-dedup so re-running dream doesn't duplicate
        // a skill it already distilled.
        let existing_schemas = cortex.storage.read().await.sqlite
            .list_memories_scoped(scope, &ListFilter {
                memory_type: Some(MemoryType::Schematic),
                limit: 200,
                ..Default::default()
            })
            .await?;

        let clusters_total = tag_map.values().filter(|v| v.len() >= SKILL_CLUSTER_MIN_SIZE).count();
        let mut total_skills = 0usize;

        for (tag, indices) in &tag_map {
            if indices.len() < SKILL_CLUSTER_MIN_SIZE { continue; }
            if budget_remaining == 0 || *calls_used >= overall_budget { break; }

            let proc_text: String = indices.iter().take(10).enumerate()
                .map(|(i, &idx)| format!("[{}] {}", i, truncate_chars(&successful[idx].content, 200)))
                .collect::<Vec<_>>()
                .join("\n");

            let prompt = PROMPT_DISTILL_SKILL.replace("{procedures}", &proc_text);
            match llm_call(&key, SYSTEM_DREAM, &prompt).await {
                Ok(resp) => {
                    *calls_used      += 1;
                    result.llm_calls += 1;
                    budget_remaining -= 1;
                    result.memories_processed += indices.len().min(10);

                    let skill = match parse_json_object(&resp) {
                        Some(s) => s,
                        None    => continue,
                    };
                    let content = skill["content"].as_str().unwrap_or("").trim();
                    if content.len() < 10 { continue; }

                    // Skip if the first 40 chars match an existing schema.
                    let prefix = truncate_chars(content, 40);
                    if existing_schemas.iter().any(|n| n.content.starts_with(prefix)) {
                        continue;
                    }

                    let llm_tags: Vec<String> = skill["tags"].as_array()
                        .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
                        .unwrap_or_default();

                    let source_ids: Vec<String> = indices.iter()
                        .map(|&idx| successful[idx].id.0.clone()).collect();
                    // Skill salience tracks the mean fitness of its source
                    // procedures, floored at 0.7 so a distilled skill always
                    // outranks the raw memories it came from.
                    let mean_fitness = indices.iter()
                        .map(|&idx| procedure_fitness(successful[idx])).sum::<f32>()
                        / indices.len() as f32;
                    let skill_salience = mean_fitness.clamp(0.7, 0.95);

                    let skill_tags = {
                        let mut t = vec![
                            "schema".to_string(),
                            "skill".to_string(),
                            "dream_distilled".to_string(),
                            format!("support_count:{}", indices.len()),
                            tag.clone(),
                        ];
                        t.extend(llm_tags);
                        t
                    };

                    if let Ok(mut node) = cortex.remember(
                        content.to_string(),
                        Some(MemoryType::Schematic),
                        Some(skill_tags),
                        Some(skill_salience),
                        scope.clone(),
                    ).await {
                        if let serde_json::Value::Object(ref mut map) = node.metadata {
                            map.insert("derived_from".to_string(), json!(source_ids));
                        } else {
                            node.metadata = json!({ "derived_from": source_ids });
                        }
                        match cortex.storage.read().await.sqlite.update_memory(&node).await {
                            Ok(_) => {
                                total_skills += 1;
                                result.links_created += source_ids.len();
                            }
                            Err(e) => tracing::warn!(
                                "Phase 3 skill persist failed for {}: {e}", node.id.0
                            ),
                        }
                    }
                }
                Err(e) => tracing::warn!("Phase 3 skill LLM call failed: {e}"),
            }
        }

        result.schemas_extracted = total_schemas;
        result.skills_distilled  = total_skills;
        result.notes = format!(
            "Formed {} schemas from {} episodes; distilled {} skills from {} successful \
             procedure clusters (budget used: {}/{})",
            total_schemas, episodes.len(), total_skills, clusters_total, result.llm_calls, budget,
        );
        result.duration_secs = start.elapsed().as_secs_f64();
        Ok(result)
    }

    // -------------------------------------------------------------------------
    // Phase: Variation / Mutation — LLM-assisted (exo-evolution E2 / E2b)
    //
    // Generates fresh procedure variants so competition (E1) has new alternatives
    // to select among. Two operators share the phase budget:
    //   (a) REFINEMENT (E2): take a genuinely-underperforming procedure (graded,
    //       ≥1 failure, Wilson fitness below REFINE_FITNESS_CEILING) and ask the
    //       LLM for an improved variant for the SAME task (`dream_mutated`).
    //   (b) MERGE/recombination (E2b): take the two fittest DISTINCT procedures in
    //       a niche, both above MERGE_FITNESS_FLOOR, and synthesise a hybrid that
    //       combines their strengths (`dream_merged`) — crossover, not refinement.
    //
    // Every variant inherits its parent(s)' topical (niche) tags, links back via
    // `derived_from`, and starts un-graded — so E1 treats it as novelty (exempt)
    // until it has been tried, then it competes against its parent(s) on its own
    // record. Lose/recombine → re-compete is the variation→selection loop.
    //
    // Guards: one untested variant per parent / merged child per niche (no pile-up),
    // prefix dedup against existing procedures, distinct-content merge parents, and
    // a bounded LLM budget (refinement worst-fitness first; ~half reserved for
    // merge). Junk variants are self-correcting — if one underperforms it is
    // demoted/pruned by selection, exactly like any procedure.
    // -------------------------------------------------------------------------
    async fn variation(
        &self,
        scope:          &VisibilityScope,
        cortex:         &Arc<CerebroCortex>,
        calls_used:     &mut usize,
        budget:         usize,
        overall_budget: usize,
    ) -> Result<PhaseResult> {
        let start = std::time::Instant::now();
        let mut result = PhaseResult::new("variation");

        let key = match &self.anthropic_key {
            None => {
                result.notes = "skipped: no ANTHROPIC_API_KEY".into();
                result.duration_secs = start.elapsed().as_secs_f64();
                return Ok(result);
            }
            Some(k) => k.clone(),
        };

        let procedures = cortex.storage.read().await.sqlite
            .list_memories_scoped(scope, &ListFilter {
                memory_type: Some(MemoryType::Procedural),
                limit: 500,
                ..Default::default()
            })
            .await?;

        let candidates = refine_candidates(&procedures);
        let candidates_total = candidates.len();
        result.memories_processed = candidates_total;

        // Reserve roughly half the phase budget for the merge pass (b) so a
        // refinement-heavy brain still recombines — mirrors schema_formation.
        let merge_reserve = (budget / 2).max(1);
        let mut budget_remaining = budget;
        let mut total_mutated = 0usize;

        // ---- (a) Refinement: improve a struggling procedure into a variant ----
        for idx in candidates {
            if budget_remaining <= merge_reserve || *calls_used >= overall_budget { break; }
            let parent = &procedures[idx];

            let prompt = PROMPT_REFINE_PROCEDURE
                .replace("{procedure}", truncate_chars(&parent.content, 400));
            let resp = match llm_call(&key, SYSTEM_DREAM, &prompt).await {
                Ok(r) => r,
                Err(e) => { tracing::warn!("variation LLM call failed: {e}"); continue; }
            };
            *calls_used      += 1;
            result.llm_calls += 1;
            budget_remaining -= 1;

            let variant = match parse_json_object(&resp) {
                Some(v) => v,
                None    => continue,
            };
            let content = variant["content"].as_str().unwrap_or("").trim();
            if content.len() < 10 { continue; }

            // Prefix dedup against existing procedures (incl. the parent itself, so
            // a no-op "refinement" that just echoes the original is dropped).
            let prefix = truncate_chars(content, 40);
            if procedures.iter().any(|n| n.content.starts_with(prefix)) { continue; }

            let llm_tags: Vec<String> = variant["tags"].as_array()
                .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
                .unwrap_or_default();
            // Inherit the parent's topical tags so the variant lands in the SAME
            // niche it must out-compete; add the mutation markers.
            let parent_topical = parent.tags.iter()
                .filter(|t| !is_structural_tag(t)).cloned();
            let variant_tags = {
                let mut t = vec!["procedure".to_string(), "dream_mutated".to_string()];
                t.extend(parent_topical);
                t.extend(llm_tags);
                t.sort();
                t.dedup();
                t
            };

            let parent_id = parent.id.0.clone();
            if let Ok(mut node) = cortex.remember(
                content.to_string(),
                Some(MemoryType::Procedural),
                Some(variant_tags),
                Some(0.7), // unproven — below the 0.8 store default and champions
                scope.clone(),
            ).await {
                if let serde_json::Value::Object(ref mut map) = node.metadata {
                    map.insert("derived_from".to_string(), json!([parent_id]));
                } else {
                    node.metadata = json!({ "derived_from": [parent_id] });
                }
                match cortex.storage.read().await.sqlite.update_memory(&node).await {
                    Ok(_)  => total_mutated += 1,
                    Err(e) => tracing::warn!("variation persist failed for {}: {e}", node.id.0),
                }
            }
        }

        // ---- (b) Merge: recombine two strong same-niche procedures into a hybrid ----
        let merge_pairs = merge_candidates(&procedures);
        let merge_pairs_total = merge_pairs.len();
        let mut total_merged = 0usize;

        for (a, b) in merge_pairs {
            if budget_remaining == 0 || *calls_used >= overall_budget { break; }
            let (pa, pb) = (&procedures[a], &procedures[b]);

            let prompt = PROMPT_MERGE_PROCEDURES
                .replace("{procedure_a}", truncate_chars(&pa.content, 300))
                .replace("{procedure_b}", truncate_chars(&pb.content, 300));
            let resp = match llm_call(&key, SYSTEM_DREAM, &prompt).await {
                Ok(r) => r,
                Err(e) => { tracing::warn!("variation merge LLM call failed: {e}"); continue; }
            };
            *calls_used      += 1;
            result.llm_calls += 1;
            budget_remaining -= 1;

            let merged = match parse_json_object(&resp) {
                Some(v) => v,
                None    => continue,
            };
            let content = merged["content"].as_str().unwrap_or("").trim();
            if content.len() < 10 { continue; }

            // Prefix dedup against existing procedures (incl. either parent).
            let prefix = truncate_chars(content, 40);
            if procedures.iter().any(|n| n.content.starts_with(prefix)) { continue; }

            let llm_tags: Vec<String> = merged["tags"].as_array()
                .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
                .unwrap_or_default();
            // Inherit BOTH parents' topical tags so the hybrid competes in their
            // shared niche; add the merge markers.
            let parent_topical = pa.tags.iter().chain(pb.tags.iter())
                .filter(|t| !is_structural_tag(t)).cloned();
            let variant_tags = {
                let mut t = vec!["procedure".to_string(), "dream_merged".to_string()];
                t.extend(parent_topical);
                t.extend(llm_tags);
                t.sort();
                t.dedup();
                t
            };

            let parent_ids = vec![pa.id.0.clone(), pb.id.0.clone()];
            if let Ok(mut node) = cortex.remember(
                content.to_string(),
                Some(MemoryType::Procedural),
                Some(variant_tags),
                Some(0.7), // unproven — competes on its own record once tried
                scope.clone(),
            ).await {
                if let serde_json::Value::Object(ref mut map) = node.metadata {
                    map.insert("derived_from".to_string(), json!(parent_ids));
                } else {
                    node.metadata = json!({ "derived_from": parent_ids });
                }
                match cortex.storage.read().await.sqlite.update_memory(&node).await {
                    Ok(_)  => total_merged += 1,
                    Err(e) => tracing::warn!("variation merge persist failed for {}: {e}", node.id.0),
                }
            }
        }

        result.procedures_mutated = total_mutated;
        result.procedures_merged  = total_merged;
        result.notes = format!(
            "Refined {} of {} struggling procedures; merged {} of {} niche pairs (budget used: {}/{})",
            total_mutated, candidates_total, total_merged, merge_pairs_total, result.llm_calls, budget,
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
    // Phase: Skill Competition — algorithmic (exo-evolution)
    //
    // Procedures sharing a topical tag compete for that niche. The fittest
    // (confidence-aware Wilson lower bound of its win/loss ledger) is marked the
    // niche CHAMPION (`skill_champion` tag); rivals trailing the champion by more
    // than COMPETITION_MARGIN are demoted (bounded salience decay + difficulty
    // bump), so a persistent loser drifts to the prune floor and is retired by the
    // pruning phase. This is relative inferiority as selection pressure — the
    // complement to slice #3's absolute-failure demotion: a subtly-worse procedure
    // can no longer survive forever simply because it is rarely *failed*.
    //
    // Novelty is protected: a procedure below COMPETITION_MIN_GRADED_USES graded
    // outcomes is exempt (`competitive_fitness` → None) and neither wins nor loses,
    // so a fresh procedure is exercised before selection can retire it. A procedure
    // that is champion of ANY niche is never demoted, even if it loses another.
    // -------------------------------------------------------------------------
    async fn skill_competition(
        &self,
        scope:  &VisibilityScope,
        cortex: &Arc<CerebroCortex>,
    ) -> Result<PhaseResult> {
        let start = std::time::Instant::now();
        let mut result = PhaseResult::new("skill_competition");

        let procedures = cortex.storage.read().await.sqlite
            .list_memories_scoped(scope, &ListFilter {
                memory_type: Some(MemoryType::Procedural),
                limit: 500,
                ..Default::default()
            })
            .await?;
        result.memories_processed = procedures.len();

        let CompetitionVerdicts { actions, niches_contested } =
            compute_competition_verdicts(&procedures);

        // Apply verdicts: one DB write per node that actually changed, and at most
        // one penalty per node per cycle regardless of how many niches it lost
        // (bounded, predictable decay).
        let mut champions_marked   = 0usize;
        let mut procedures_demoted = 0usize;

        for (mut node, action) in procedures.into_iter().zip(actions) {
            let has_champion_tag = is_skill_champion(&node);
            let mut changed = false;

            match action {
                CompetitionAction::Champion => {
                    if !has_champion_tag {
                        node.tags.push(SKILL_CHAMPION_TAG.to_string());
                        changed = true;
                    }
                    champions_marked += 1;
                }
                CompetitionAction::Demote => {
                    // No longer champion → drop the stale marker, then select
                    // against it: bounded decay + a difficulty nudge, flagged for
                    // pruning once it reaches the floor.
                    if has_champion_tag {
                        node.tags.retain(|t| t != SKILL_CHAMPION_TAG);
                    }
                    node.salience = (node.salience - COMPETITION_PENALTY).max(0.0);
                    node.strength.difficulty = (node.strength.difficulty + 0.3).min(10.0);
                    if node.salience <= PRUNE_CANDIDATE_SALIENCE
                        && !node.tags.iter().any(|t| t == "prune_candidate")
                    {
                        node.tags.push("prune_candidate".to_string());
                    }
                    procedures_demoted += 1;
                    changed = true;
                }
                CompetitionAction::Leave => {
                    // Keep the champion marker honest — clear it if this procedure
                    // is no longer champion of any niche.
                    if has_champion_tag {
                        node.tags.retain(|t| t != SKILL_CHAMPION_TAG);
                        changed = true;
                    }
                }
            }

            if changed {
                if let Err(e) = cortex.storage.read().await.sqlite
                    .update_memory(&node).await
                {
                    tracing::warn!("skill_competition persist failed for {}: {e}", node.id.0);
                }
            }
        }

        result.niches_contested   = niches_contested;
        result.champions_marked   = champions_marked;
        result.procedures_demoted = procedures_demoted;
        result.notes = format!(
            "Contested {} niches: marked {} champions, demoted {} dominated procedures",
            niches_contested, champions_marked, procedures_demoted,
        );
        result.duration_secs = start.elapsed().as_secs_f64();
        Ok(result)
    }

    // -------------------------------------------------------------------------
    // Phase 5: Pruning — algorithmic
    // Soft-delete two classes of stale memory:
    //   A) isolated, low-salience sensory-layer memories (original rule)
    //   B) `prune_candidate`-flagged memories — procedures demoted to the floor
    //      by repeated failure (evolutionary layer, slice #3). This is what makes
    //      the failure → demote → flag → retire selection loop actually retire.
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
        let mut demoted_pruned = 0usize;
        for node in &all_memories {
            // Both prune classes require the memory to be stale.
            if node.created_at > cutoff { continue; }

            let prune_candidate = node.tags.iter().any(|t| t == "prune_candidate");

            let should_prune = if prune_candidate {
                // Class B: explicitly demoted by repeated failure (slice #3) —
                // retire regardless of layer or links; the flag IS the decision.
                true
            } else if node.layer == MemoryLayer::Sensory
                && node.salience <= PRUNING_MAX_SALIENCE
            {
                // Class A: isolated, low-salience, stale sensory memory.
                cortex.storage.read().await.sqlite
                    .list_links_from(&node.id).await?.is_empty()
            } else {
                false
            };
            if !should_prune { continue; }

            // CB-024: only count a prune that actually soft-deleted a live row.
            // Through the coordinator so the node is also pruned from the in-memory
            // graph (write lock; bound first so the guard releases before the match).
            let prune_result = cortex.storage.write().await.delete_memory(&node.id).await;
            match prune_result {
                Ok(true)  => {
                    pruned += 1;
                    if prune_candidate { demoted_pruned += 1; }
                }
                Ok(false) => {} // no-op (already deleted) — don't over-count
                Err(e)    => tracing::warn!(
                    "Phase 5 prune failed for {}: {e}", node.id.0
                ),
            }
        }

        result.memories_processed = all_memories.len();
        result.memories_pruned    = pruned;
        result.notes = format!(
            "Pruned {} memories of {} scanned ({} demoted procedures, {} isolated sensory)",
            pruned, all_memories.len(), demoted_pruned, pruned - demoted_pruned,
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
    /// Abstract skills distilled from successful procedure clusters in phase 3
    /// (evolutionary layer). `#[serde(default)]` keeps older persisted reports
    /// (which lack the field) deserialisable.
    #[serde(default)]
    pub skills_distilled:     usize,
    /// Niche-competition counters (exo-evolution `skill_competition` phase).
    /// `#[serde(default)]` keeps older persisted reports deserialisable.
    #[serde(default)]
    pub niches_contested:     usize,
    #[serde(default)]
    pub champions_marked:     usize,
    #[serde(default)]
    pub procedures_demoted:   usize,
    /// Procedures refined into fresh variants by the variation phase (E2).
    #[serde(default)]
    pub procedures_mutated:   usize,
    /// Procedure pairs recombined into hybrid variants by the variation phase (E2b).
    #[serde(default)]
    pub procedures_merged:    usize,
    pub procedures_extracted: usize,
    /// Colony C2: extraction candidates that semantically matched an EXISTING
    /// procedure (≥ the rediscovery similarity floor) and reinforced it instead of
    /// storing a fragment. novel-vs-treading-water, visible in the dream journal.
    /// `#[serde(default)]` keeps older persisted reports deserialisable.
    #[serde(default)]
    pub procedures_rediscovered: usize,
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
            skills_distilled:     0,
            niches_contested:     0,
            champions_marked:     0,
            procedures_demoted:   0,
            procedures_mutated:   0,
            procedures_merged:    0,
            procedures_extracted: 0,
            procedures_rediscovered: 0,
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
    use super::{
        compute_competition_verdicts, competitive_fitness, has_pending_merge, has_pending_variant,
        is_skill_champion, is_structural_tag, merge_candidates, outcome_stats, procedure_fitness,
        refine_candidates, retrieval_rank, wilson_lower_bound, CompetitionAction, SKILL_MIN_FITNESS,
    };
    use crate::config::FSRS_INITIAL_DIFFICULTY;
    use crate::models::MemoryNode;
    use crate::types::MemoryType;
    use serde_json::json;

    fn proc(salience: f32, difficulty: f32) -> MemoryNode {
        let mut n = MemoryNode::new("how I did X", MemoryType::Procedural);
        n.salience = salience;
        n.strength.difficulty = difficulty;
        n
    }

    /// A procedure carrying `tags` with a win/loss ledger of `successes`/`failures`,
    /// as `record_procedure_outcome` would write it.
    fn graded_proc(tags: &[&str], successes: u32, failures: u32) -> MemoryNode {
        let mut n = MemoryNode::new("how I did X", MemoryType::Procedural);
        n.tags = tags.iter().map(|t| t.to_string()).collect();
        n.metadata = json!({ "outcomes": { "successes": successes, "failures": failures } });
        n
    }

    /// An un-graded `dream_mutated` variant deriving from `parent_id` (no ledger),
    /// as the variation phase would store it.
    fn variant_of(parent_id: &str) -> MemoryNode {
        let mut n = MemoryNode::new("a refined approach", MemoryType::Procedural);
        n.tags = vec!["procedure".into(), "dream_mutated".into()];
        n.metadata = json!({ "derived_from": [parent_id] });
        n
    }

    /// Like `graded_proc` but with explicit `content` — needed for merge tests,
    /// where same-niche procedures must have DISTINCT content to be merge-worthy.
    fn graded_proc_c(content: &str, tags: &[&str], successes: u32, failures: u32) -> MemoryNode {
        let mut n = MemoryNode::new(content, MemoryType::Procedural);
        n.tags = tags.iter().map(|t| t.to_string()).collect();
        n.metadata = json!({ "outcomes": { "successes": successes, "failures": failures } });
        n
    }

    /// A `dream_merged` hybrid carrying `niche`, graded or not — for pending-merge
    /// guard tests.
    fn merged_node(niche: &str, graded: bool) -> MemoryNode {
        let mut n = MemoryNode::new("a hybrid approach", MemoryType::Procedural);
        n.tags = vec!["procedure".into(), "dream_merged".into(), niche.into()];
        if graded {
            n.metadata = json!({ "outcomes": { "successes": 1, "failures": 0 } });
        }
        n
    }

    #[test]
    fn unfailed_default_procedure_passes_skill_bar() {
        // store-default procedure: salience 0.8, baseline difficulty, no failures.
        let n = proc(0.8, FSRS_INITIAL_DIFFICULTY);
        assert!((procedure_fitness(&n) - 0.8).abs() < 1e-6);
        assert!(procedure_fitness(&n) >= SKILL_MIN_FITNESS);
    }

    #[test]
    fn a_failure_drops_fitness_below_the_bar() {
        // one failure bumps difficulty to 5.5; fitness must dip under the bar so
        // a procedure that has ever failed is not yet a "successful" skill source.
        let n = proc(0.8, FSRS_INITIAL_DIFFICULTY + 0.5);
        assert!(procedure_fitness(&n) < SKILL_MIN_FITNESS);
    }

    #[test]
    fn chronic_failure_drives_fitness_toward_zero() {
        let n = proc(0.9, 10.0); // max difficulty
        assert!(procedure_fitness(&n) < 1e-6);
    }

    #[test]
    fn high_salience_success_scores_highest() {
        let strong = proc(0.95, FSRS_INITIAL_DIFFICULTY);
        let weak   = proc(0.8, FSRS_INITIAL_DIFFICULTY);
        assert!(procedure_fitness(&strong) > procedure_fitness(&weak));
    }

    #[test]
    fn structural_tags_excluded_topical_kept() {
        for t in [
            "procedure", "skill", "schema", "dream_distilled", "support_count:3",
            "skill_champion", "prune_candidate", "dream_mutated", "dream_merged",
        ] {
            assert!(is_structural_tag(t), "{t} should be structural");
        }
        for t in ["slint", "async", "debugging", "rust"] {
            assert!(!is_structural_tag(t), "{t} should be topical");
        }
    }

    // ---- exo-evolution: fitness ledger + niche competition ------------------

    #[test]
    fn wilson_zero_outcomes_is_zero() {
        assert_eq!(wilson_lower_bound(0, 0), 0.0);
    }

    #[test]
    fn wilson_penalises_small_samples() {
        // a perfect 1/1 must score below a proven 8/2 — confidence, not just rate
        assert!(wilson_lower_bound(1, 0) < wilson_lower_bound(8, 2));
    }

    #[test]
    fn wilson_rewards_more_evidence_at_equal_rate() {
        // identical 100% success rate, more trials → strictly higher lower bound
        assert!(wilson_lower_bound(10, 0) > wilson_lower_bound(2, 0));
    }

    #[test]
    fn wilson_failures_lower_the_score() {
        assert!(wilson_lower_bound(5, 5) < wilson_lower_bound(10, 0));
    }

    #[test]
    fn ungraded_procedure_has_no_ledger_and_no_fitness() {
        let n = MemoryNode::new("x", MemoryType::Procedural); // metadata = Null
        assert!(outcome_stats(&n).is_none());
        assert!(competitive_fitness(&n).is_none());
    }

    #[test]
    fn one_graded_use_is_below_the_novelty_bar() {
        // COMPETITION_MIN_GRADED_USES = 2: a single outcome is read but exempt
        let n = graded_proc(&["slint"], 1, 0);
        assert_eq!(outcome_stats(&n), Some((1, 0)));
        assert!(competitive_fitness(&n).is_none(), "1 use is below the novelty bar");
    }

    #[test]
    fn two_graded_uses_clear_the_novelty_bar() {
        assert!(competitive_fitness(&graded_proc(&["slint"], 2, 0)).is_some());
    }

    #[test]
    fn champion_wins_and_clear_loser_is_demoted() {
        let procs = vec![
            graded_proc(&["deploy"], 10, 0), // strong track record
            graded_proc(&["deploy"], 1, 9),  // graded loser, far behind
        ];
        let v = compute_competition_verdicts(&procs);
        assert_eq!(v.niches_contested, 1);
        assert_eq!(v.actions[0], CompetitionAction::Champion);
        assert_eq!(v.actions[1], CompetitionAction::Demote);
    }

    #[test]
    fn ungraded_rival_is_protected_and_makes_no_contest() {
        // A fresh procedure (1 use, below the bar) shares a niche with a strong
        // champion. It is exempt, so the niche has only ONE eligible contender and
        // holds no contest — novelty is never demoted for losing.
        let procs = vec![
            graded_proc(&["deploy"], 10, 0),
            graded_proc(&["deploy"], 1, 0), // below novelty bar
        ];
        let v = compute_competition_verdicts(&procs);
        assert_eq!(v.niches_contested, 0, "only one eligible contender → no contest");
        assert_eq!(v.actions[0], CompetitionAction::Leave);
        assert_eq!(v.actions[1], CompetitionAction::Leave);
    }

    #[test]
    fn near_tie_does_not_demote() {
        // both strong and within COMPETITION_MARGIN → a champion is marked but the
        // runner-up is left alone (a near-tie is not a loss).
        let procs = vec![graded_proc(&["async"], 10, 0), graded_proc(&["async"], 9, 1)];
        let v = compute_competition_verdicts(&procs);
        assert_eq!(v.niches_contested, 1);
        assert!(v.actions.contains(&CompetitionAction::Champion));
        assert!(!v.actions.contains(&CompetitionAction::Demote), "within-margin runner-up is safe");
    }

    #[test]
    fn champion_of_one_niche_survives_losing_another() {
        // proc 0 champions niche "a" but is dominated in niche "b"; being best at
        // something must outrank losing elsewhere → Champion, never Demote.
        let procs = vec![
            graded_proc(&["a", "b"], 7, 3), // champions "a", weak in "b"
            graded_proc(&["a"], 1, 9),      // clear loser in "a"
            graded_proc(&["b"], 20, 0),     // champions "b"
        ];
        let v = compute_competition_verdicts(&procs);
        assert_eq!(v.niches_contested, 2);
        assert_eq!(v.actions[0], CompetitionAction::Champion);
        assert_eq!(v.actions[1], CompetitionAction::Demote);
        assert_eq!(v.actions[2], CompetitionAction::Champion);
    }

    #[test]
    fn single_procedure_niche_holds_no_contest() {
        let v = compute_competition_verdicts(&[graded_proc(&["solo"], 5, 0)]);
        assert_eq!(v.niches_contested, 0);
        assert_eq!(v.actions[0], CompetitionAction::Leave);
    }

    #[test]
    fn structural_tags_alone_form_no_niche() {
        // Sharing only the structural "procedure" marker (no topical tag) is not a
        // niche — competition must form on subject matter, not bookkeeping.
        let procs = vec![graded_proc(&["procedure"], 10, 0), graded_proc(&["procedure"], 1, 9)];
        let v = compute_competition_verdicts(&procs);
        assert_eq!(v.niches_contested, 0);
        assert!(v.actions.iter().all(|a| *a == CompetitionAction::Leave));
    }

    // ---- exo-evolution E1 follow-up: champion-aware retrieval ranking -------

    #[test]
    fn is_skill_champion_reads_the_tag() {
        assert!(is_skill_champion(&graded_proc(&["deploy", "skill_champion"], 2, 0)));
        assert!(!is_skill_champion(&graded_proc(&["deploy"], 2, 0)));
    }

    #[test]
    fn retrieval_rank_floats_champion_above_a_fitter_rival() {
        // The core contract: the niche champion ALWAYS leads, even when a
        // non-champion rival has a strictly higher raw Wilson fitness. The +1.0
        // band guarantees it — "prefer the procedure competition crowned".
        let champion = graded_proc(&["deploy", "skill_champion"], 2, 2); // Wilson ≈ 0.15
        let fitter   = graded_proc(&["deploy"], 20, 0);                  // Wilson ≈ 0.84
        assert!(competitive_fitness(&fitter) > competitive_fitness(&champion),
            "rival is genuinely fitter on raw Wilson");
        assert!(retrieval_rank(&champion) > retrieval_rank(&fitter),
            "but the champion still surfaces first");
    }

    #[test]
    fn retrieval_rank_orders_graded_non_champions_by_wilson() {
        let strong = graded_proc(&["deploy"], 10, 0);
        let weak   = graded_proc(&["deploy"], 3, 3);
        assert!(retrieval_rank(&strong) > retrieval_rank(&weak));
    }

    #[test]
    fn retrieval_rank_falls_back_to_salience_for_ungraded() {
        // An ungraded procedure has no Wilson fitness, so it ranks by its raw
        // salience (recall strength) — not forced to zero, so novelty stays
        // visible and a higher-salience fresh procedure leads a lower-salience one.
        let hi = proc(0.9, FSRS_INITIAL_DIFFICULTY);
        let lo = proc(0.4, FSRS_INITIAL_DIFFICULTY);
        assert!(competitive_fitness(&hi).is_none(), "ungraded → no Wilson fitness");
        assert!((retrieval_rank(&hi) - 0.9).abs() < 1e-6, "uses salience, not 0");
        assert!(retrieval_rank(&hi) > retrieval_rank(&lo));
    }

    // ---- exo-evolution E2: variation / mutation ----------------------------

    #[test]
    fn refine_targets_failing_low_fitness_procedures() {
        let procs = vec![
            graded_proc(&["deploy"], 1, 5),  // [0] fails often, low fitness → candidate
            graded_proc(&["deploy"], 10, 0), // [1] never failed → not a candidate
        ];
        let c = refine_candidates(&procs);
        assert_eq!(c, vec![0], "only the failing, low-fitness procedure is refined");
    }

    #[test]
    fn refine_skips_ungraded_and_unfailed() {
        let procs = vec![
            MemoryNode::new("never graded", MemoryType::Procedural), // ungraded → skip
            graded_proc(&["a"], 8, 0),                               // graded, no failures → skip
        ];
        assert!(refine_candidates(&procs).is_empty());
    }

    #[test]
    fn refine_orders_worst_fitness_first() {
        let procs = vec![
            graded_proc(&["a"], 4, 4), // higher fitness of the two losers
            graded_proc(&["b"], 0, 6), // rock bottom → must come first
        ];
        assert_eq!(refine_candidates(&procs), vec![1, 0], "most-struggling refined first");
    }

    #[test]
    fn refine_skips_parent_with_untested_variant() {
        // A struggling parent that already spawned an un-graded variant is skipped
        // until that variant has been tried — no untested-variant pile-up.
        let parent = graded_proc(&["deploy"], 1, 5);
        let child  = variant_of(&parent.id.0);
        let procs  = vec![parent, child];
        assert!(refine_candidates(&procs).is_empty(),
            "parent with a pending untested variant is not re-refined");
    }

    #[test]
    fn refine_resumes_once_variant_is_graded() {
        // Once the variant has an outcome, the parent is eligible again.
        let parent = graded_proc(&["deploy"], 1, 5);
        let mut child = variant_of(&parent.id.0);
        child.metadata = json!({
            "derived_from": [parent.id.0.clone()],
            "outcomes": { "successes": 1, "failures": 0 }
        });
        let parent_idx_present = refine_candidates(&[parent, child]);
        assert!(parent_idx_present.contains(&0),
            "a graded variant no longer blocks re-refining the parent");
    }

    #[test]
    fn has_pending_variant_only_counts_ungraded_children() {
        let parent = graded_proc(&["deploy"], 1, 5);
        let pid = parent.id.0.clone();
        // ungraded child blocks
        assert!(has_pending_variant(&[parent.clone(), variant_of(&pid)], &pid));
        // a graded child does NOT block
        let mut graded_child = variant_of(&pid);
        graded_child.metadata = json!({
            "derived_from": [pid.clone()],
            "outcomes": { "successes": 2, "failures": 1 }
        });
        assert!(!has_pending_variant(&[parent, graded_child], &pid));
    }

    // ---- exo-evolution E2b: merge / recombination --------------------------

    #[test]
    fn merge_pairs_two_strong_distinct_procedures_in_a_niche() {
        let procs = vec![
            graded_proc_c("approach one: stop, swap, verify", &["deploy"], 8, 0),
            graded_proc_c("approach two: drain, swap, smoke-test", &["deploy"], 5, 0),
        ];
        assert_eq!(merge_candidates(&procs), vec![(0, 1)]);
    }

    #[test]
    fn merge_skips_a_weak_partner() {
        // champion is strong, but the only same-niche partner is below the floor →
        // nothing proven to recombine with.
        let procs = vec![
            graded_proc_c("strong approach", &["deploy"], 8, 0),
            graded_proc_c("weak approach", &["deploy"], 1, 3),
        ];
        assert!(merge_candidates(&procs).is_empty());
    }

    #[test]
    fn merge_skips_near_duplicate_partners() {
        // both strong, but effectively the same procedure (shared 40-char prefix) →
        // no new strength to combine.
        let dup = "identical opening that runs past forty characters for sure, then diverges";
        let procs = vec![
            graded_proc_c(dup, &["deploy"], 8, 0),
            graded_proc_c(dup, &["deploy"], 6, 0),
        ];
        assert!(merge_candidates(&procs).is_empty());
    }

    #[test]
    fn merge_skips_niche_with_a_pending_merged_child() {
        let procs = vec![
            graded_proc_c("approach one here", &["deploy"], 8, 0),
            graded_proc_c("approach two here", &["deploy"], 5, 0),
            merged_node("deploy", false), // ungraded hybrid already awaiting trial
        ];
        assert!(merge_candidates(&procs).is_empty(), "don't re-merge a niche with a pending hybrid");
    }

    #[test]
    fn merge_dedups_a_pair_shared_across_two_niches() {
        // both procedures carry two topical tags → the same pair would surface in
        // niche "a" and niche "b"; it must be emitted only once.
        let procs = vec![
            graded_proc_c("approach one", &["a", "b"], 8, 0),
            graded_proc_c("approach two", &["a", "b"], 5, 0),
        ];
        assert_eq!(merge_candidates(&procs), vec![(0, 1)]);
    }

    #[test]
    fn has_pending_merge_only_counts_ungraded_children_in_the_niche() {
        // ungraded hybrid in the niche blocks
        assert!(has_pending_merge(&[merged_node("deploy", false)], "deploy"));
        // a graded hybrid does not block
        assert!(!has_pending_merge(&[merged_node("deploy", true)], "deploy"));
        // an ungraded hybrid in a DIFFERENT niche does not block "deploy"
        assert!(!has_pending_merge(&[merged_node("other", false)], "deploy"));
    }
}

#[cfg(test)]
mod report_compat_tests {
    use super::PhaseResult;

    #[test]
    fn phase_result_deserializes_without_rediscovered_field() {
        // Older persisted DreamReports predate the C2 novel/rediscovery split —
        // the serde(default) must keep them loadable (dream_status reads them back).
        let old = r#"{"phase":"pattern_extraction","episodes_consolidated":0,
            "memories_processed":10,"links_created":0,"links_strengthened":0,
            "memories_pruned":0,"schemas_extracted":0,"procedures_extracted":3,
            "llm_calls":2,"duration_secs":1.0,"notes":"","success":true}"#;
        let r: PhaseResult = serde_json::from_str(old).unwrap();
        assert_eq!(r.procedures_rediscovered, 0);
        assert_eq!(r.procedures_extracted, 3);
    }
}

#[cfg(test)]
mod truncate_tests {
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
