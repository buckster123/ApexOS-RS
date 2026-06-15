//! The node's bound agent identity — single source of truth for "who is acting".
//!
//! See [docs/agent-identity.md]. agentd stamps this onto the model's Cerebro tool
//! calls (so routing/isolation can't depend on what the model typed), and uses it
//! for its own internal Cerebro writes (council summaries, the rollback store) so
//! everything lands in one agent space — no more `APEX`/`CLAUDE-APEX` drift.
//!
//! Today every session resolves to this one node identity; per-session
//! identities (the multi-agent boot flow) layer on top in a later slice.

/// Default agent identity when `AGENTD_AGENT_ID` is unset or blank.
pub const DEFAULT_AGENT_ID: &str = "APEX";

/// The node's agent identity: `$AGENTD_AGENT_ID`, else [`DEFAULT_AGENT_ID`].
pub fn node_agent_id() -> String {
    std::env::var("AGENTD_AGENT_ID")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_AGENT_ID.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    // AGENTD_AGENT_ID is process-global; serialize the env-mutating tests.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn defaults_to_apex_when_unset() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("AGENTD_AGENT_ID");
        assert_eq!(node_agent_id(), "APEX");
        assert_eq!(node_agent_id(), DEFAULT_AGENT_ID);
    }

    #[test]
    fn env_overrides_and_blank_falls_back() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("AGENTD_AGENT_ID", "LUMA");
        assert_eq!(node_agent_id(), "LUMA");
        // Blank/whitespace is treated as unset → default.
        std::env::set_var("AGENTD_AGENT_ID", "   ");
        assert_eq!(node_agent_id(), DEFAULT_AGENT_ID);
        std::env::remove_var("AGENTD_AGENT_ID");
    }
}
