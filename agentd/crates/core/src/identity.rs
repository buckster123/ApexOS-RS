//! The node's bound agent identity — single source of truth for "who is acting".
//!
//! See [docs/agent-identity.md]. agentd stamps this onto the model's Cerebro tool
//! calls (so routing/isolation can't depend on what the model typed), and uses it
//! for its own internal Cerebro writes (council summaries, the rollback store) so
//! everything lands in one agent space — no more `APEX`/`CLAUDE-APEX` drift.
//!
//! Today every session resolves to this one node identity; per-session
//! identities (the multi-agent boot flow) layer on top in a later slice.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::Path;

/// Default agent identity when `AGENTD_AGENT_ID` is unset or blank.
pub const DEFAULT_AGENT_ID: &str = "APEX";

/// Default owner user id seeded on a fresh node (owns the built-in APEX agent).
pub const DEFAULT_USER_ID: &str = "owner";

/// The node's agent identity: `$AGENTD_AGENT_ID`, else [`DEFAULT_AGENT_ID`].
pub fn node_agent_id() -> String {
    std::env::var("AGENTD_AGENT_ID")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_AGENT_ID.to_string())
}

// ── Identity records (persisted in identities.toml) ─────────────────────────
// The agreed data model (docs/agent-identity.md): a `user` is a human profile
// (optional PIN); an `agent` is a distinct being with its own Cerebro memory
// space (`id` == agent_id), soul file, and default skin, owned by a user. APEX
// is the built-in default agent. This module is the pure data layer — the HTTP
// API and the per-session runtime binding land in later sub-slices.

/// A human profile on the device. Owns one or more agents; may set an optional
/// PIN that the boot flow gates the profile's agents/memory behind.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct User {
    pub id:   String,
    pub name: String,
    /// Salted hash of the PIN (hex sha256(salt||pin)); None = open profile.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pin_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pin_salt: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_agent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_skin: Option<String>,
}

impl User {
    pub fn has_pin(&self) -> bool { self.pin_hash.is_some() }

    /// Set (or replace) the PIN with a fresh random salt.
    pub fn set_pin(&mut self, pin: &str) {
        let salt = gen_salt();
        self.pin_hash = Some(hash_pin(pin, &salt));
        self.pin_salt = Some(salt);
    }

    /// Clear the PIN (profile becomes open).
    pub fn clear_pin(&mut self) {
        self.pin_hash = None;
        self.pin_salt = None;
    }

    /// Verify a PIN (constant-time). An open profile (no PIN) always verifies.
    pub fn verify_pin(&self, pin: &str) -> bool {
        match (&self.pin_hash, &self.pin_salt) {
            (Some(hash), Some(salt)) => {
                use subtle::ConstantTimeEq;
                hash_pin(pin, salt).as_bytes().ct_eq(hash.as_bytes()).into()
            }
            _ => true,
        }
    }
}

/// An agent identity: a distinct being with its own Cerebro space (`id` ==
/// agent_id), soul, and default skin, owned by a user.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AgentRecord {
    pub id:        String,
    pub name:      String,
    pub owner:     String,
    pub soul_file: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_skin: Option<String>,
}

/// The on-disk identity registry (identities.toml): `[[user]]` + `[[agent]]`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Identities {
    #[serde(default, rename = "user")]
    pub users:  Vec<User>,
    #[serde(default, rename = "agent")]
    pub agents: Vec<AgentRecord>,
}

impl Identities {
    /// Path to identities.toml: `$AGENTD_IDENTITIES` else `/etc/agentd/identities.toml`.
    pub fn default_path() -> std::path::PathBuf {
        std::env::var("AGENTD_IDENTITIES")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|_| std::path::PathBuf::from("/etc/agentd/identities.toml"))
    }

    /// Load from `path`; a missing or unparseable file yields an empty registry
    /// (caller seeds defaults) — never panics on bad config.
    pub fn load(path: &Path) -> Self {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|s| toml::from_str(&s).ok())
            .unwrap_or_default()
    }

    /// Persist to `path` as pretty TOML.
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        let body = toml::to_string_pretty(self)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        std::fs::write(path, body)
    }

    pub fn user(&self, id: &str) -> Option<&User> { self.users.iter().find(|u| u.id == id) }
    pub fn user_mut(&mut self, id: &str) -> Option<&mut User> { self.users.iter_mut().find(|u| u.id == id) }
    pub fn agent(&self, id: &str) -> Option<&AgentRecord> { self.agents.iter().find(|a| a.id == id) }
    pub fn agents_for<'a>(&'a self, owner: &str) -> Vec<&'a AgentRecord> {
        self.agents.iter().filter(|a| a.owner == owner).collect()
    }

    /// Ensure the default owner user + the built-in APEX agent exist (idempotent).
    /// APEX's soul is the existing soul.md (`apex_soul_file`). Returns true if
    /// anything was added, so the caller knows to persist.
    pub fn seed_defaults(&mut self, apex_soul_file: &str) -> bool {
        let mut changed = false;
        if self.user(DEFAULT_USER_ID).is_none() {
            self.users.push(User {
                id: DEFAULT_USER_ID.to_string(),
                name: "Owner".to_string(),
                ..Default::default()
            });
            changed = true;
        }
        if self.agent(DEFAULT_AGENT_ID).is_none() {
            self.agents.push(AgentRecord {
                id:        DEFAULT_AGENT_ID.to_string(),
                name:      DEFAULT_AGENT_ID.to_string(),
                owner:     DEFAULT_USER_ID.to_string(),
                soul_file: apex_soul_file.to_string(),
                default_skin: None,
            });
            changed = true;
        }
        changed
    }
}

// ── PIN hashing ─────────────────────────────────────────────────────────────
// A 4–6 digit PIN is inherently low-entropy; its real protection is the API-side
// guess lockout (a later sub-slice), not hash strength — the salted hash just
// avoids storing the PIN in plaintext at rest.

fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Random 16-byte salt, hex-encoded.
pub fn gen_salt() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut bytes);
    to_hex(&bytes)
}

/// Salted PIN hash: hex(sha256(salt || pin)).
pub fn hash_pin(pin: &str, salt_hex: &str) -> String {
    let mut h = Sha256::new();
    h.update(salt_hex.as_bytes());
    h.update(pin.as_bytes());
    to_hex(&h.finalize())
}

// ── Per-session identity binding (multi-agent runtime) ──────────────────────

/// Process-wide map of session → bound `agent_id`. A `std::sync::Mutex` (not
/// tokio) so the synchronous tool-dispatch path can resolve without `.await`;
/// keep the critical section tiny (lock → clone → drop) and never hold it across
/// an await.
pub type SessionBindings =
    std::sync::Arc<std::sync::Mutex<std::collections::HashMap<apexos_protocol::SessionId, String>>>;

/// The agent identity bound to `session`, or the node default ([`node_agent_id`])
/// when the session is unbound (legacy / pre-selection) — so single-agent nodes
/// behave exactly as before.
pub fn resolve_agent_id(
    bindings: &std::sync::Mutex<std::collections::HashMap<apexos_protocol::SessionId, String>>,
    session: apexos_protocol::SessionId,
) -> String {
    bindings
        .lock()
        .ok()
        .and_then(|m| m.get(&session).cloned())
        .unwrap_or_else(node_agent_id)
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

    #[test]
    fn pin_hash_verify_and_salting() {
        let mut u = User { id: "andre".into(), name: "Andre".into(), ..Default::default() };
        // Open profile (no PIN) always verifies.
        assert!(!u.has_pin());
        assert!(u.verify_pin("anything"));

        u.set_pin("1337");
        assert!(u.has_pin());
        assert!(u.verify_pin("1337"));
        assert!(!u.verify_pin("0000"));

        // Re-setting the same PIN yields a different stored hash (fresh salt).
        let first = u.pin_hash.clone();
        u.set_pin("1337");
        assert_ne!(first, u.pin_hash);
        assert!(u.verify_pin("1337"));

        u.clear_pin();
        assert!(!u.has_pin());
        assert!(u.verify_pin("whatever"));
    }

    #[test]
    fn seed_defaults_is_idempotent() {
        let mut ids = Identities::default();
        assert!(ids.seed_defaults("/etc/agentd/soul.md"));
        assert!(!ids.seed_defaults("/etc/agentd/soul.md")); // nothing added second time
        assert_eq!(ids.users.len(), 1);
        assert_eq!(ids.agents.len(), 1);
        let apex = ids.agent(DEFAULT_AGENT_ID).expect("APEX seeded");
        assert_eq!(apex.owner, DEFAULT_USER_ID);
        assert_eq!(apex.soul_file, "/etc/agentd/soul.md");
        assert_eq!(ids.agents_for(DEFAULT_USER_ID).len(), 1);
    }

    #[test]
    fn resolve_agent_id_binds_or_falls_back() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("AGENTD_AGENT_ID");
        use apexos_protocol::SessionId;
        let map = std::sync::Mutex::new(std::collections::HashMap::new());
        // Unbound session → node default (APEX).
        assert_eq!(resolve_agent_id(&map, SessionId(7)), "APEX");
        // Bound session → its agent.
        map.lock().unwrap().insert(SessionId(7), "LUMA".to_string());
        assert_eq!(resolve_agent_id(&map, SessionId(7)), "LUMA");
        // A different session stays unbound → default.
        assert_eq!(resolve_agent_id(&map, SessionId(9)), "APEX");
    }

    #[test]
    fn identities_toml_roundtrips_with_pin() {
        let mut ids = Identities::default();
        ids.seed_defaults("/etc/agentd/soul.md");
        ids.user_mut(DEFAULT_USER_ID).unwrap().set_pin("4242");
        let toml = toml::to_string_pretty(&ids).unwrap();
        // `[[user]]` / `[[agent]]` table arrays, not "users"/"agents".
        assert!(toml.contains("[[user]]"));
        assert!(toml.contains("[[agent]]"));
        let back: Identities = toml::from_str(&toml).unwrap();
        assert_eq!(ids, back);
        assert!(back.user(DEFAULT_USER_ID).unwrap().verify_pin("4242"));
    }
}
