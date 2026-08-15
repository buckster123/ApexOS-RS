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
use std::collections::HashMap;
use std::path::{Path, PathBuf};

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

/// The node's mesh identity (the name peers know it by): `$APEX_NODE_ID`, else the
/// system hostname, else `"apexos"`. This is the *node* id (e.g. `ApexOS-RS`) —
/// distinct from [`node_agent_id`] (the *agent* identity, e.g. `APEX`). Cached: the
/// hostname is resolved at most once per process (it never changes at runtime), so
/// callers on the hot a2a-send path don't re-shell `hostname`. Single source of
/// truth shared by `main.rs` (the `GatewayState.node_id` Arc) and the cross-node
/// `send_to_agent` sender (which stamps it as `from` so the receiver can route the
/// message to that peer's own session and surface its provenance).
pub fn node_id() -> String {
    static NODE_ID: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    NODE_ID
        .get_or_init(|| {
            std::env::var("APEX_NODE_ID")
                .ok()
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| {
                    std::process::Command::new("hostname")
                        .output()
                        .ok()
                        .and_then(|o| String::from_utf8(o.stdout).ok())
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                        .unwrap_or_else(|| "apexos".into())
                })
        })
        .clone()
}

/// The node's workspace base: `$AGENTD_WORKSPACE`, else `/var/lib/agentd/workspace`.
pub fn workspace_base() -> PathBuf {
    let base = std::env::var("AGENTD_WORKSPACE")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "/var/lib/agentd/workspace".to_string());
    PathBuf::from(base)
}

/// The filesystem workspace root for `agent_id` — the single source of truth for
/// per-agent ("agent-locked") workspaces (the convergence of the identity arc and
/// the FS-confinement model; see CLAUDE.md + BACKLOG "Storage & workspaces").
///
/// APEX / the node identity (and any unbound session, which [`resolve_agent_id`]
/// maps to it) → the node base, **byte-identical** to the pre-per-agent single
/// workspace. A bound *non-default* agent → `<base>/workspaces/<agent_id>`.
///
/// The supervisor stamps this onto every apexos-tools call (`__workspace`, a
/// system-set arg the model can't spoof) so the shared, single tool process
/// confines each call to the *caller's* root; the gateway provisions the same
/// dir on agent-create. agent_id is registry-controlled (`slug()` → `[A-Z0-9_]`),
/// but the join is guarded anyway: a non-path-safe id (e.g. a hand-edited
/// identities.toml) falls back to the base so it can never escape via `/`/`..`.
pub fn agent_workspace_root(agent_id: &str) -> PathBuf {
    let base = workspace_base();
    let path_safe = !agent_id.is_empty()
        && agent_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    if agent_id == node_agent_id() || !path_safe {
        base
    } else {
        base.join("workspaces").join(agent_id)
    }
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
    pub id: String,
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
    pub fn has_pin(&self) -> bool {
        self.pin_hash.is_some()
    }

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

    /// Verify a PIN (constant-time). An open profile (no PIN) always verifies
    /// (used only after the LAN/loopback gate has already run).
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
    pub id: String,
    pub name: String,
    pub owner: String,
    pub soul_file: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_skin: Option<String>,
}

/// The on-disk identity registry (identities.toml): `[[user]]` + `[[agent]]`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Identities {
    /// The device's default login profile (user id). When set, the login screen
    /// auto-logs-in this profile (open → zero-tap; PIN → straight to the keypad),
    /// skipping the picker on a single-human device (agent-identity.md slice 3e).
    /// Declared FIRST so TOML serializes this scalar before the `[[user]]` arrays.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_user: Option<String>,
    #[serde(default, rename = "user")]
    pub users: Vec<User>,
    #[serde(default, rename = "agent")]
    pub agents: Vec<AgentRecord>,
    /// Set when boot refused a torn/unreadable file. Save/commit fail closed
    /// so a later API write cannot mint a fresh Owner/APEX over the wreckage.
    #[serde(skip)]
    pub persist_blocked: bool,
}

/// Why [`Identities::try_load`] failed. Missing is a fresh node; anything else
/// is *not* an empty registry (SA-10).
#[derive(Debug)]
pub enum IdentitiesLoadError {
    NotFound,
    Io(std::io::Error),
    Parse(String),
}

impl std::fmt::Display for IdentitiesLoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            IdentitiesLoadError::NotFound => write!(f, "identities.toml not found"),
            IdentitiesLoadError::Io(e) => write!(f, "read identities.toml: {e}"),
            IdentitiesLoadError::Parse(e) => write!(f, "parse identities.toml: {e}"),
        }
    }
}

/// Temp+rename with in-place fallback when the parent dir is not writable
/// (`/etc/agentd` is root-owned; the file is agentd-owned — see gotchas).
pub fn write_config_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
    let atomic = (|| {
        std::fs::write(&tmp, bytes)?;
        std::fs::rename(&tmp, path)?;
        Ok::<(), std::io::Error>(())
    })();
    if let Err(e) = atomic {
        let _ = std::fs::remove_file(&tmp);
        if e.kind() == std::io::ErrorKind::PermissionDenied
            || e.to_string().contains("os error 13")
        {
            return std::fs::write(path, bytes);
        }
        return Err(e);
    }
    Ok(())
}

impl Identities {
    /// Path to identities.toml: `$AGENTD_IDENTITIES` else `/etc/agentd/identities.toml`.
    pub fn default_path() -> std::path::PathBuf {
        std::env::var("AGENTD_IDENTITIES")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|_| std::path::PathBuf::from("/etc/agentd/identities.toml"))
    }

    /// Load a *valid* registry. Missing and parse/IO errors are distinct —
    /// never fold corruption into [`Default`].
    pub fn try_load(path: &Path) -> Result<Self, IdentitiesLoadError> {
        match std::fs::read_to_string(path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                Err(IdentitiesLoadError::NotFound)
            }
            Err(e) => Err(IdentitiesLoadError::Io(e)),
            Ok(s) => toml::from_str(&s).map_err(|e| IdentitiesLoadError::Parse(e.to_string())),
        }
    }

    /// Move a torn/unparseable registry aside so a later seed cannot overwrite it.
    pub fn quarantine(path: &Path) -> std::io::Result<PathBuf> {
        let dest = {
            let primary = path.with_extension("toml.corrupt");
            if primary.exists() {
                let ts = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                path.with_extension(format!("toml.corrupt.{ts}"))
            } else {
                primary
            }
        };
        std::fs::rename(path, &dest)?;
        Ok(dest)
    }

    /// Daemon-start load. Missing → seed Owner/APEX. Unparseable → quarantine
    /// and return an empty persist-blocked registry (do not treat as absence).
    pub fn boot_load(path: &Path, apex_soul_file: &str) -> Self {
        match Self::try_load(path) {
            Ok(mut ids) => {
                if ids.seed_defaults(apex_soul_file) {
                    if let Err(e) = ids.save(path) {
                        eprintln!(
                            "[identity] could not persist {}: {e} (re-seeding in-memory)",
                            path.display()
                        );
                    }
                }
                ids
            }
            Err(IdentitiesLoadError::NotFound) => {
                let mut ids = Self::default();
                ids.seed_defaults(apex_soul_file);
                if let Err(e) = ids.save(path) {
                    eprintln!(
                        "[identity] could not persist {}: {e} (re-seeding in-memory)",
                        path.display()
                    );
                }
                ids
            }
            Err(e) => {
                if matches!(e, IdentitiesLoadError::Parse(_)) {
                    match Self::quarantine(path) {
                        Ok(side) => eprintln!(
                            "[identity] quarantined unparseable {} → {} ({e}); persist blocked",
                            path.display(),
                            side.display()
                        ),
                        Err(qe) => eprintln!(
                            "[identity] unparseable {} ({e}); quarantine failed ({qe}); persist blocked",
                            path.display()
                        ),
                    }
                } else {
                    eprintln!("[identity] {e}; persist blocked (will not seed-overwrite)");
                }
                let mut ids = Self::default();
                ids.persist_blocked = true;
                ids
            }
        }
    }

    /// Persist to `path` as pretty TOML. Fails closed when [`persist_blocked`].
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if self.persist_blocked {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "identities registry corrupt; restore identities.toml.corrupt",
            ));
        }
        let body = toml::to_string_pretty(self)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        write_config_atomic(path, body.as_bytes())
    }

    /// Persist `next` then replace `self`. On persist failure `self` is unchanged.
    pub fn commit(&mut self, path: &Path, next: Self) -> std::io::Result<()> {
        next.save(path)?;
        *self = next;
        Ok(())
    }

    pub fn user(&self, id: &str) -> Option<&User> {
        self.users.iter().find(|u| u.id == id)
    }
    pub fn user_mut(&mut self, id: &str) -> Option<&mut User> {
        self.users.iter_mut().find(|u| u.id == id)
    }
    pub fn agent(&self, id: &str) -> Option<&AgentRecord> {
        self.agents.iter().find(|a| a.id == id)
    }
    pub fn agents_for<'a>(&'a self, owner: &str) -> Vec<&'a AgentRecord> {
        self.agents.iter().filter(|a| a.owner == owner).collect()
    }

    /// The node is claimed once the seeded owner profile has a PIN. Until then,
    /// LAN login is closed (finding 2).
    pub fn owner_claimed(&self) -> bool {
        self.user(DEFAULT_USER_ID).is_some_and(|u| u.has_pin())
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
                id: DEFAULT_AGENT_ID.to_string(),
                name: DEFAULT_AGENT_ID.to_string(),
                owner: DEFAULT_USER_ID.to_string(),
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

/// Owner-claim / login PIN: 4–8 ASCII digits. Low entropy is expected; lockout
/// is the real gate. Rejects empty / non-digit so setup cannot mint a no-op PIN.
pub fn valid_owner_pin(pin: &str) -> bool {
    let n = pin.len();
    (4..=8).contains(&n) && pin.bytes().all(|b| b.is_ascii_digit())
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

/// Per-session inbound mesh hop count (finding 13). Missing = 0 (local /
/// never arrived over the mesh). Outbound delegation sends [`next_mesh_hops`].
/// std Mutex so the tool-dispatch path can read without `.await`.
pub type MeshHops =
    std::sync::Arc<std::sync::Mutex<std::collections::HashMap<apexos_protocol::SessionId, u32>>>;

/// Inbound `x-mesh-hops` at this value or above is refused.
pub const MESH_HOP_LIMIT: u32 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MeshHopsError {
    Missing,
    Invalid,
    Limit,
}

/// Parse a peer `x-mesh-hops` header. Missing / zero / unparseable / at-limit
/// all fail — peer-only endpoints must carry a strictly positive count.
pub fn parse_mesh_hops(raw: Option<&str>) -> Result<u32, MeshHopsError> {
    let Some(s) = raw.map(str::trim).filter(|s| !s.is_empty()) else {
        return Err(MeshHopsError::Missing);
    };
    let n = s.parse::<u32>().map_err(|_| MeshHopsError::Invalid)?;
    if n == 0 {
        return Err(MeshHopsError::Invalid);
    }
    if n >= MESH_HOP_LIMIT {
        return Err(MeshHopsError::Limit);
    }
    Ok(n)
}

/// Next outbound hop from a session's stored inbound depth. Local sessions
/// (stored 0) send 1. A session that already received `LIMIT-1` cannot
/// delegate again.
pub fn next_mesh_hops(stored: u32) -> Result<u32, MeshHopsError> {
    let n = stored.saturating_add(1);
    if n >= MESH_HOP_LIMIT {
        return Err(MeshHopsError::Limit);
    }
    Ok(n)
}

pub fn mesh_hops_get(map: &MeshHops, session: apexos_protocol::SessionId) -> u32 {
    map.lock()
        .ok()
        .and_then(|m| m.get(&session).copied())
        .unwrap_or(0)
}

pub fn mesh_hops_set(map: &MeshHops, session: apexos_protocol::SessionId, hops: u32) {
    if let Ok(mut m) = map.lock() {
        m.insert(session, hops);
    }
}

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

// ── Session-id classes ───────────────────────────────────────────────────────
// SPAWN_SESSION_BASE / WORKER_SESSION_BASE / is_spawn_session / is_worker_session
// live in `apexos-protocol` (wire-relevant — frontends class sessions too) and
// re-export from this crate's root unchanged. The agentd-side laws that hang
// off the partition (spawn persist-skip + provenance stamping; worker boot-seed
// filter + no-hydration + counter reload discipline) are documented in
// docs/gotchas.md and enforced at their named sites.

// ── Per-worker model pins (Fabrica W1d) ──────────────────────────────────────

/// Sessions of workers fanned with an explicit `model` (`task_fanout{model?}`),
/// mapped to the pinned model string. Shared worker-driver↔`root_turn` (the
/// GoalYoloSessions pattern): the driver arms a session at admission/wake and
/// disarms at terminal/park; `root_turn` builds a pinned sibling provider for
/// armed sessions. std Mutex — lock, clone, drop; never held across an await.
pub type WorkerModels = std::sync::Arc<std::sync::Mutex<HashMap<u64, String>>>;

/// The pinned model for a worker session, if any. Poisoned lock → None (the
/// node default model — fails safe, never fails the turn).
pub fn worker_model_for(models: &WorkerModels, session_id: u64) -> Option<String> {
    models.lock().ok().and_then(|m| m.get(&session_id).cloned())
}

// ── Per-session goal autonomy (goal-scoped yolo) ────────────────────────────

/// Process-wide set of goal session ids running with **goal-scoped yolo**
/// (`goal_create{yolo:true}` after a human grant) — their OWN `ask`-gated tools
/// auto-approve. The goal driver inserts a session on create and removes it on a
/// terminal outcome; the supervisor's approval gate consults it so a *granted*
/// goal runs unattended **without** flipping global yolo — scoped strictly to
/// that one goal's session,
/// never root or another session. Co-located with [`SessionBindings`] as the other
/// process-wide per-session runtime map; a `std::sync::Mutex` (not tokio) so the
/// synchronous decision path checks it with a tiny lock→contains→drop.
pub type GoalYoloSessions = std::sync::Arc<std::sync::Mutex<std::collections::HashSet<u64>>>;

/// True iff `session` is a goal running with goal-scoped yolo. **Fails closed** — a
/// poisoned lock returns false, so a lock error can never silently auto-approve.
pub fn goal_session_is_yolo(
    set: &std::sync::Mutex<std::collections::HashSet<u64>>,
    session: u64,
) -> bool {
    set.lock().map(|s| s.contains(&session)).unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    // AGENTD_AGENT_ID is process-global; serialize the env-mutating tests.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    // (The session-id partition test moved to apexos-protocol with the
    // constants — the wire crate owns the class definitions now.)

    #[test]
    fn worker_model_pin_round_trips_and_fails_safe() {
        let models: WorkerModels = std::sync::Arc::new(std::sync::Mutex::new(HashMap::new()));
        assert_eq!(worker_model_for(&models, 7), None); // unpinned → node default
        models.lock().unwrap().insert(7, "claude-haiku-4-5".into());
        assert_eq!(
            worker_model_for(&models, 7).as_deref(),
            Some("claude-haiku-4-5")
        );
        assert_eq!(worker_model_for(&models, 8), None); // strictly per-session
    }

    #[test]
    fn mesh_hops_parse_and_increment() {
        assert_eq!(parse_mesh_hops(None), Err(MeshHopsError::Missing));
        assert_eq!(parse_mesh_hops(Some("")), Err(MeshHopsError::Missing));
        assert_eq!(parse_mesh_hops(Some("0")), Err(MeshHopsError::Invalid));
        assert_eq!(parse_mesh_hops(Some("nope")), Err(MeshHopsError::Invalid));
        assert_eq!(parse_mesh_hops(Some("3")), Err(MeshHopsError::Limit));
        assert_eq!(parse_mesh_hops(Some("1")), Ok(1));
        assert_eq!(parse_mesh_hops(Some("2")), Ok(2));
        assert_eq!(next_mesh_hops(0), Ok(1));
        assert_eq!(next_mesh_hops(1), Ok(2));
        assert_eq!(next_mesh_hops(2), Err(MeshHopsError::Limit));
        let map: MeshHops = std::sync::Arc::new(std::sync::Mutex::new(HashMap::new()));
        let sid = apexos_protocol::SessionId(42);
        assert_eq!(mesh_hops_get(&map, sid), 0);
        mesh_hops_set(&map, sid, 1);
        assert_eq!(mesh_hops_get(&map, sid), 1);
        assert_eq!(next_mesh_hops(mesh_hops_get(&map, sid)), Ok(2));
    }

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
    fn owner_pin_shape() {
        assert!(valid_owner_pin("1337"));
        assert!(valid_owner_pin("12345678"));
        assert!(!valid_owner_pin(""));
        assert!(!valid_owner_pin("12"));
        assert!(!valid_owner_pin("123456789"));
        assert!(!valid_owner_pin("12ab"));
        assert!(!valid_owner_pin("12 34"));
    }

    #[test]
    fn pin_hash_verify_and_salting() {
        let mut u = User {
            id: "andre".into(),
            name: "Andre".into(),
            ..Default::default()
        };
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
        assert!(!ids.owner_claimed(), "seeded owner has no PIN");
        ids.user_mut(DEFAULT_USER_ID).unwrap().set_pin("1337");
        assert!(ids.owner_claimed());
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
    fn agent_workspace_root_is_per_agent_but_byte_identical_for_node() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("AGENTD_AGENT_ID");
        std::env::set_var("AGENTD_WORKSPACE", "/srv/ws");
        // APEX / the node identity → the base, unchanged from pre-per-agent.
        assert_eq!(agent_workspace_root("APEX"), Path::new("/srv/ws"));
        // A bound non-default agent → its own subdir.
        assert_eq!(
            agent_workspace_root("LUMA"),
            Path::new("/srv/ws/workspaces/LUMA")
        );
        // A non-path-safe id (hand-edited registry) can't escape — falls back to base.
        assert_eq!(agent_workspace_root("../etc"), Path::new("/srv/ws"));
        assert_eq!(agent_workspace_root("a/b"), Path::new("/srv/ws"));
        // Empty workspace var → the documented default.
        std::env::remove_var("AGENTD_WORKSPACE");
        assert_eq!(
            agent_workspace_root("LUMA"),
            Path::new("/var/lib/agentd/workspace/workspaces/LUMA")
        );
    }

    #[test]
    fn default_user_roundtrips_before_tables() {
        let mut ids = Identities::default();
        ids.seed_defaults("/etc/agentd/soul.md");
        ids.default_user = Some(DEFAULT_USER_ID.to_string());
        let toml = toml::to_string_pretty(&ids).unwrap();
        // The scalar must serialize before the array-of-tables, or TOML reparses it
        // as a key of the last [[user]]/[[agent]] table.
        let du = toml.find("default_user").expect("default_user present");
        let tbl = toml.find("[[").expect("a table array present");
        assert!(du < tbl, "default_user must precede [[user]]/[[agent]]");
        let back: Identities = toml::from_str(&toml).unwrap();
        assert_eq!(back.default_user.as_deref(), Some(DEFAULT_USER_ID));
        // Absent in older files → None (migration-safe).
        let legacy: Identities = toml::from_str("[[user]]\nid='owner'\nname='Owner'\n").unwrap();
        assert_eq!(legacy.default_user, None);
    }

    fn tmp_ids(tag: &str) -> (std::path::PathBuf, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "apex-ids-{}-{}-{}",
            tag,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("identities.toml");
        (dir, path)
    }

    #[test]
    fn try_load_distinguishes_missing_from_corrupt() {
        let (dir, path) = tmp_ids("load");
        assert!(matches!(
            Identities::try_load(&path),
            Err(IdentitiesLoadError::NotFound)
        ));
        std::fs::write(&path, "[[user]]\nid = \"owner\"\nthis is not toml").unwrap();
        assert!(matches!(
            Identities::try_load(&path),
            Err(IdentitiesLoadError::Parse(_))
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn boot_load_quarantines_torn_file_and_blocks_persist() {
        let (dir, path) = tmp_ids("boot-torn");
        let torn = "[[user]]\nid = \"owner\"\nname = \"Owner\"\npin_hash = \"abc";
        std::fs::write(&path, torn).unwrap();
        let ids = Identities::boot_load(&path, "/etc/agentd/soul.md");
        assert!(ids.persist_blocked);
        assert!(ids.users.is_empty(), "must not seed over a torn registry");
        assert!(!path.exists(), "live path moved aside");
        let sidecar = dir.join("identities.toml.corrupt");
        assert_eq!(std::fs::read_to_string(&sidecar).unwrap(), torn);
        assert!(ids.save(&path).is_err(), "blocked save must not recreate live file");
        assert!(!path.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn boot_load_missing_seeds_defaults() {
        let (dir, path) = tmp_ids("boot-miss");
        let ids = Identities::boot_load(&path, "/etc/agentd/soul.md");
        assert!(!ids.persist_blocked);
        assert_eq!(ids.users.len(), 1);
        assert_eq!(ids.agents.len(), 1);
        let back = Identities::try_load(&path).expect("seeded file");
        assert_eq!(back.user(DEFAULT_USER_ID).unwrap().name, "Owner");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn commit_leaves_memory_untouched_when_save_fails() {
        let (dir, path) = tmp_ids("commit");
        let mut ids = Identities::default();
        ids.seed_defaults("/etc/agentd/soul.md");
        ids.save(&path).unwrap();
        let mut next = ids.clone();
        next.users.push(User {
            id: "guest".into(),
            name: "Guest".into(),
            ..Default::default()
        });
        // Path is a directory → write fails; RAM must stay at the pre-commit registry.
        let bad = dir.join("not-a-file");
        std::fs::create_dir(&bad).unwrap();
        assert!(ids.commit(&bad, next).is_err());
        assert!(ids.user("guest").is_none());
        let disk = Identities::try_load(&path).unwrap();
        assert!(disk.user("guest").is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_config_atomic_round_trips() {
        let (dir, path) = tmp_ids("atomic");
        write_config_atomic(&path, b"hello").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"hello");
        write_config_atomic(&path, b"world").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"world");
        let _ = std::fs::remove_dir_all(&dir);
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
