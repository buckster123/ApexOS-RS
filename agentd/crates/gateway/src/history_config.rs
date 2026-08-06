//! Persisted history-window budget — the Settings choice for
//! `AGENTD_HISTORY_TOKEN_BUDGET` (env stays the seed, the file wins on
//! restart; delete the file to return to env control). Mirrors
//! `backend_config`'s load/persist/resolve shape.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryConfig {
    /// Rough-token per-session window budget. `0` disables trimming.
    pub budget: usize,
}

pub fn config_path() -> String {
    std::env::var("AGENTD_HISTORY_CONFIG")
        .unwrap_or_else(|_| "/var/lib/agentd/history_config.json".into())
}

/// The persisted operator choice, if any. `None` on a fresh node / unreadable file.
pub fn load_persisted() -> Option<HistoryConfig> {
    let raw = std::fs::read_to_string(config_path()).ok()?;
    serde_json::from_str::<HistoryConfig>(&raw).ok()
}

/// Best-effort persist — the in-memory atomic is already live for the next turn,
/// so a write failure only costs restart-durability. Returns whether it stuck.
pub fn persist(cfg: &HistoryConfig) -> bool {
    match serde_json::to_string_pretty(cfg) {
        Ok(s) => match std::fs::write(config_path(), s) {
            Ok(()) => true,
            Err(e) => {
                eprintln!("[gateway] persist history config to {} failed: {e}", config_path());
                false
            }
        },
        Err(_) => false,
    }
}

/// Sanitize an operator-supplied budget: `0` = trimming off (documented), anything
/// else floors at 10k — a fat-fingered tiny budget would shred every session's
/// window to the one-turn minimum on the next prompt.
pub fn sanitize_budget(raw: u64) -> usize {
    if raw == 0 { 0 } else { (raw as usize).max(10_000) }
}

/// Pure boot resolution: the persisted choice wins over the env seed.
pub fn resolve_boot(env_budget: usize, persisted: Option<&HistoryConfig>) -> usize {
    match persisted {
        Some(p) => sanitize_budget(p.budget as u64),
        None => env_budget,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_keeps_zero_floors_tiny_passes_sane() {
        assert_eq!(sanitize_budget(0), 0, "0 stays 0 — trimming off");
        assert_eq!(sanitize_budget(500), 10_000, "tiny budgets floor at 10k");
        assert_eq!(sanitize_budget(120_000), 120_000);
    }

    #[test]
    fn boot_resolution_file_wins_else_env() {
        assert_eq!(resolve_boot(120_000, None), 120_000);
        assert_eq!(resolve_boot(120_000, Some(&HistoryConfig { budget: 400_000 })), 400_000);
        assert_eq!(resolve_boot(120_000, Some(&HistoryConfig { budget: 0 })), 0);
    }
}
