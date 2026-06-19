//! Token + cache accounting for the human-facing "tokenomics / cache bank" insight.
//!
//! Cumulative since daemon boot, across all sessions. The Anthropic SSE parser records
//! one turn's usage at `message_stop`; the gateway snapshots it for `GET /api/usage`.
//! A process-global (not a per-provider arc) because usage is inherently process-wide —
//! it sums every turn the daemon has run regardless of session or provider instance.
//! Anthropic-path only for now (that's where caching lives); the OpenAI/Ollama path can
//! feed the same recorder later.

use std::sync::Mutex;

/// Cumulative token counts. The three input tiers map to Anthropic's billing:
/// `input` at full price, `cache_read` at ~0.1×, `cache_creation` at ~1.25× (5m TTL).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct UsageStats {
    pub turns: u64,
    /// Uncached input tokens (full price).
    pub input_tokens: u64,
    /// Tokens served from cache (~0.1× input price) — the "cache bank" withdrawals.
    pub cache_read_tokens: u64,
    /// Tokens written to cache (~1.25× input price) — the deposits.
    pub cache_creation_tokens: u64,
    pub output_tokens: u64,
}

impl UsageStats {
    pub const fn zero() -> Self {
        Self {
            turns: 0,
            input_tokens: 0,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
            output_tokens: 0,
        }
    }

    /// All input tokens across the three tiers — the denominator for the hit rate.
    pub fn total_input(&self) -> u64 {
        self.input_tokens + self.cache_read_tokens + self.cache_creation_tokens
    }

    /// Fraction of input served from cache, 0.0–1.0. Zero when nothing has run.
    pub fn cache_hit_rate(&self) -> f64 {
        let t = self.total_input();
        if t == 0 { 0.0 } else { self.cache_read_tokens as f64 / t as f64 }
    }
}

static USAGE: Mutex<UsageStats> = Mutex::new(UsageStats::zero());

/// Record one completed turn's usage. Called by the Anthropic SSE parser at `message_stop`.
pub fn record_turn_usage(input: u64, cache_read: u64, cache_creation: u64, output: u64) {
    if let Ok(mut s) = USAGE.lock() {
        s.turns += 1;
        s.input_tokens += input;
        s.cache_read_tokens += cache_read;
        s.cache_creation_tokens += cache_creation;
        s.output_tokens += output;
    }
}

/// Snapshot the cumulative stats (for `GET /api/usage`).
pub fn snapshot() -> UsageStats {
    USAGE.lock().map(|s| *s).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn total_input_sums_all_three_tiers() {
        let u = UsageStats { input_tokens: 100, cache_read_tokens: 900, cache_creation_tokens: 50, ..UsageStats::zero() };
        assert_eq!(u.total_input(), 1050);
    }

    #[test]
    fn cache_hit_rate_is_read_over_total_input() {
        let u = UsageStats { input_tokens: 100, cache_read_tokens: 900, cache_creation_tokens: 0, ..UsageStats::zero() };
        assert!((u.cache_hit_rate() - 0.9).abs() < 1e-9);
        assert_eq!(UsageStats::zero().cache_hit_rate(), 0.0, "no divide-by-zero on a cold daemon");
    }
}
