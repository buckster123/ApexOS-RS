//! ConnectivityState — the honesty layer of ApexNET P5 (`docs/apexnet.md`
//! §6.2/§6.3). Coarse, LATCHED, and mechanical: the state gates which tools
//! are *exposed*, not which ones fail (charter principle 3 — degraded means
//! absent, not broken).
//!
//! The state is a **process-global atomic**: agentd, the gateway, and the
//! plugin supervisor are one process, and every consumer (tool gathering,
//! the ambient line, the call-time backstop) just reads [`current`]. Only
//! the watcher loop (agentd main) writes it, through a [`Latch`] so a
//! flapping link can't churn the tool list — and with it the prompt-cache
//! prefix — every probe (D7: a transition is rare and worth the one-time
//! rebuild; flapping is not).
//!
//! With one transport built (Wi-Fi/LAN — the radio tiers land P4/P6), the
//! derivation is: WAN up → `Full` · WAN down but mesh peers alive →
//! `Degraded` · nothing reachable → `Isolated`. `Minimal` (radio-only) is
//! declared now so the tool table and wire don't change shape later, and
//! becomes reachable when radio heartbeats feed the liveness map.

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::OnceLock;

/// The colony's connectivity tier, best (0) to worst (3). Ordering is part
/// of the contract: a tool's `min_state` means "available at this state or
/// better (numerically ≤)".
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
pub enum ConnectivityState {
    /// WAN + LAN normal — everything available.
    Full = 0,
    /// WAN gone, mesh/LAN alive — a2a and mesh tools still work.
    Degraded = 1,
    /// Radio tiers only (post-P4/P6) — proofs move, bytes queue.
    Minimal = 2,
    /// Nothing reachable — outbox and courier are the only lanes.
    Isolated = 3,
}

impl ConnectivityState {
    pub fn as_str(self) -> &'static str {
        match self {
            ConnectivityState::Full => "full",
            ConnectivityState::Degraded => "degraded",
            ConnectivityState::Minimal => "minimal",
            ConnectivityState::Isolated => "isolated",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "full" => Some(ConnectivityState::Full),
            "degraded" => Some(ConnectivityState::Degraded),
            "minimal" => Some(ConnectivityState::Minimal),
            "isolated" => Some(ConnectivityState::Isolated),
            _ => None,
        }
    }

    fn from_u8(v: u8) -> Self {
        match v {
            0 => ConnectivityState::Full,
            1 => ConnectivityState::Degraded,
            2 => ConnectivityState::Minimal,
            _ => ConnectivityState::Isolated,
        }
    }
}

/// Boot default is `Full` — no tool disappears before the first real
/// observation says otherwise (no-regression rule: absent machinery must
/// look exactly like today).
static CURRENT: AtomicU8 = AtomicU8::new(0);

/// The state every consumer reads.
pub fn current() -> ConnectivityState {
    ConnectivityState::from_u8(CURRENT.load(Ordering::Relaxed))
}

/// Watcher-only. Everyone else reads.
pub fn set_current(s: ConnectivityState) {
    CURRENT.store(s as u8, Ordering::Relaxed);
}

/// Derive the candidate state from this probe round's facts (pure).
/// `peers_total == 0` (a solo node) can never be "Degraded by mesh" — with
/// no WAN it is Isolated unless a radio lane is up (P5d: radio-only = Minimal).
pub fn derive_state(
    wan_ok: bool,
    peers_alive: usize,
    peers_total: usize,
    radio_up: bool,
) -> ConnectivityState {
    if wan_ok {
        ConnectivityState::Full
    } else if peers_total > 0 && peers_alive > 0 {
        ConnectivityState::Degraded
    } else if radio_up {
        ConnectivityState::Minimal
    } else {
        ConnectivityState::Isolated
    }
}

/// Hysteresis: a candidate state must repeat for `threshold` consecutive
/// observations before it becomes current (the beacon's miss-streak shape,
/// applied to state instead of liveness). Pure — the watcher loop is IO
/// around this.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Latch {
    pub current: ConnectivityState,
    candidate: ConnectivityState,
    streak: u32,
    threshold: u32,
}

impl Latch {
    pub fn new(initial: ConnectivityState, threshold: u32) -> Self {
        Self {
            current: initial,
            candidate: initial,
            streak: 0,
            threshold: threshold.max(1),
        }
    }

    /// Feed one observation; returns `Some((from, to))` exactly when the
    /// latch flips.
    pub fn observe(
        &mut self,
        observed: ConnectivityState,
    ) -> Option<(ConnectivityState, ConnectivityState)> {
        if observed == self.current {
            self.candidate = observed;
            self.streak = 0;
            return None;
        }
        if observed == self.candidate {
            self.streak += 1;
        } else {
            self.candidate = observed;
            self.streak = 1;
        }
        if self.streak >= self.threshold {
            let from = self.current;
            self.current = observed;
            self.streak = 0;
            Some((from, observed))
        } else {
            None
        }
    }
}

// ── The tool side table (charter D6 — policy-style, never ToolSpec fields) ──

/// tool name → the WORST state it remains available at. Unlisted tools are
/// always available (today's behavior — no regression by construction).
pub type ConnectivityRules = HashMap<String, ConnectivityState>;

/// Parse the `[tools]` table of a connectivity.toml. Unknown state strings
/// are SKIPPED loudly rather than failing the file — a newer repo's vocab
/// must not brick an older node (the tolerant-wire rule, config edition).
pub fn parse_rules(raw: &str) -> ConnectivityRules {
    let mut rules = HashMap::new();
    let Ok(v) = raw.parse::<toml::Value>() else {
        eprintln!(
            "[connectivity] config is not valid TOML — gating disabled (all tools available)"
        );
        return rules;
    };
    let Some(tools) = v.get("tools").and_then(|t| t.as_table()) else {
        return rules;
    };
    for (tool, val) in tools {
        match val.as_str().and_then(ConnectivityState::parse) {
            Some(min) => {
                rules.insert(tool.clone(), min);
            }
            None => eprintln!(
                "[connectivity] rule '{tool}' has unknown state {val:?} — skipped (tool stays available)"
            ),
        }
    }
    rules
}

/// Is `tool` available at `state` under `rules`? Listed = available while
/// the state is numerically ≤ its floor; unlisted = always.
pub fn tool_available(tool: &str, state: ConnectivityState, rules: &ConnectivityRules) -> bool {
    match rules.get(tool) {
        Some(min) => state <= *min,
        None => true,
    }
}

/// The deployed config path (`APEXNET_CONNECTIVITY_CONFIG` overrides; the
/// install seeds `/etc/agentd/connectivity.toml` and additively syncs it
/// like policy).
pub fn config_path() -> std::path::PathBuf {
    std::env::var("APEXNET_CONNECTIVITY_CONFIG")
        .unwrap_or_else(|_| "/etc/agentd/connectivity.toml".into())
        .into()
}

fn load_rules_from(path: &Path) -> ConnectivityRules {
    match std::fs::read_to_string(path) {
        Ok(raw) => parse_rules(&raw),
        Err(_) => HashMap::new(), // absent file = no gating (today's behavior)
    }
}

static RULES: OnceLock<ConnectivityRules> = OnceLock::new();

/// The process's gating rules, loaded once from [`config_path`]. Every
/// consumer (tool gathering, the supervisor backstop) shares this table.
pub fn rules() -> &'static ConnectivityRules {
    RULES.get_or_init(|| {
        let r = load_rules_from(&config_path());
        if !r.is_empty() {
            eprintln!("[connectivity] {} tool gating rule(s) loaded", r.len());
        }
        r
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derivation_maps_the_one_transport_world() {
        assert_eq!(derive_state(true, 0, 0, false), ConnectivityState::Full);
        assert_eq!(derive_state(true, 3, 3, false), ConnectivityState::Full);
        assert_eq!(
            derive_state(false, 2, 3, false),
            ConnectivityState::Degraded
        );
        assert_eq!(
            derive_state(false, 0, 3, false),
            ConnectivityState::Isolated
        );
        // Solo node, no WAN: nothing to degrade to.
        assert_eq!(
            derive_state(false, 0, 0, false),
            ConnectivityState::Isolated
        );
        // Radio-only (P5d): LAN gone, brainstem still on the air.
        assert_eq!(derive_state(false, 0, 3, true), ConnectivityState::Minimal);
        assert_eq!(derive_state(false, 0, 0, true), ConnectivityState::Minimal);
    }

    #[test]
    fn latch_needs_a_streak_and_flips_once() {
        let mut l = Latch::new(ConnectivityState::Full, 3);
        assert_eq!(l.observe(ConnectivityState::Degraded), None);
        assert_eq!(l.observe(ConnectivityState::Degraded), None);
        assert_eq!(
            l.observe(ConnectivityState::Degraded),
            Some((ConnectivityState::Full, ConnectivityState::Degraded))
        );
        // Stable in the new state: quiet.
        assert_eq!(l.observe(ConnectivityState::Degraded), None);
    }

    #[test]
    fn latch_flap_resets_the_streak() {
        let mut l = Latch::new(ConnectivityState::Full, 3);
        assert_eq!(l.observe(ConnectivityState::Isolated), None);
        assert_eq!(l.observe(ConnectivityState::Isolated), None);
        assert_eq!(l.observe(ConnectivityState::Full), None); // flap back — streak dies
        assert_eq!(l.observe(ConnectivityState::Isolated), None);
        assert_eq!(l.observe(ConnectivityState::Isolated), None);
        assert_eq!(
            l.observe(ConnectivityState::Isolated),
            Some((ConnectivityState::Full, ConnectivityState::Isolated))
        );
    }

    #[test]
    fn latch_candidate_switch_mid_streak_restarts() {
        let mut l = Latch::new(ConnectivityState::Full, 2);
        assert_eq!(l.observe(ConnectivityState::Degraded), None);
        assert_eq!(l.observe(ConnectivityState::Isolated), None); // new candidate, streak=1
        assert_eq!(
            l.observe(ConnectivityState::Isolated),
            Some((ConnectivityState::Full, ConnectivityState::Isolated))
        );
    }

    #[test]
    fn rules_parse_and_gate_with_order_semantics() {
        let rules = parse_rules(
            r#"
[tools]
http_fetch    = "full"
mesh_file_send = "degraded"
weird_tool    = "quantum"   # unknown → skipped, stays available
"#,
        );
        assert_eq!(rules.len(), 2);
        // full-only tool vanishes the moment we degrade.
        assert!(tool_available(
            "http_fetch",
            ConnectivityState::Full,
            &rules
        ));
        assert!(!tool_available(
            "http_fetch",
            ConnectivityState::Degraded,
            &rules
        ));
        // degraded tool survives Full + Degraded, dies below.
        assert!(tool_available(
            "mesh_file_send",
            ConnectivityState::Full,
            &rules
        ));
        assert!(tool_available(
            "mesh_file_send",
            ConnectivityState::Degraded,
            &rules
        ));
        assert!(!tool_available(
            "mesh_file_send",
            ConnectivityState::Minimal,
            &rules
        ));
        assert!(!tool_available(
            "mesh_file_send",
            ConnectivityState::Isolated,
            &rules
        ));
        // unlisted + unknown-state tools: always available.
        assert!(tool_available(
            "read_file",
            ConnectivityState::Isolated,
            &rules
        ));
        assert!(tool_available(
            "weird_tool",
            ConnectivityState::Isolated,
            &rules
        ));
    }

    #[test]
    fn garbage_config_disables_gating_not_the_node() {
        assert!(parse_rules("not toml at all [[[").is_empty());
        assert!(parse_rules("").is_empty());
    }

    #[test]
    fn global_roundtrips() {
        set_current(ConnectivityState::Degraded);
        assert_eq!(current(), ConnectivityState::Degraded);
        set_current(ConnectivityState::Full);
        assert_eq!(current(), ConnectivityState::Full);
    }
}
