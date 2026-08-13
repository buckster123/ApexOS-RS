//! Human↔node session authentication (agent-identity.md slice 3e).
//!
//! A login (profile + optional PIN) mints a short-lived bearer token the UI / PWA
//! uses for the WS + API, so a human client never needs the node's shared
//! `AGENTD_TOKEN` — that retreats to being the machine / mesh / admin secret
//! (node↔node a2a tokens, kiosk-as-root, operator curl/CI). The gate
//! (`require_token`) accepts EITHER the admin token OR a valid minted session
//! token. Privileged REST (`require_admin`) then requires the admin token **or**
//! an Owner-role session — a guest session token is not `/api/run`.
//!
//! In-memory ONLY: a daemon restart clears every session (re-login), so a session
//! token never touches disk. This is the deliberate, safest default — the cost is
//! a re-login after a restart, which on the spare-device tier is fine.
//!
//! The pure store lives here (mint/verify/revoke/sweep), unit-tested with injected
//! `Instant`s; the IO-thin login/logout handlers + the `require_token` hook live in
//! `lib.rs`.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Default session lifetime (24 h). Re-login after this, or after a daemon restart.
pub const SESSION_TTL_SECS: u64 = 24 * 60 * 60;

/// Capability claim carried on a minted human session (finding 2).
/// `Admin` is the `AGENTD_TOKEN` path — it is never stored in this enum.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuthRole {
    /// Seeded owner profile (`DEFAULT_USER_ID`). May use privileged REST.
    Owner,
    /// Any other human profile. Chat/WS + own sessions only.
    User,
}

impl AuthRole {
    pub fn for_user_id(user_id: &str) -> Self {
        if user_id == apexos_core::DEFAULT_USER_ID {
            Self::Owner
        } else {
            Self::User
        }
    }

    pub fn is_privileged(self) -> bool {
        matches!(self, Self::Owner)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Owner => "owner",
            Self::User => "user",
        }
    }
}

/// What authorized this HTTP/WS request after `require_token`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RequestAuth {
    /// Shared `AGENTD_TOKEN` (or a token-less node — tests / loopback-dev).
    Admin,
    /// Minted human-login session.
    Session(SessionAuth),
}

impl RequestAuth {
    pub fn is_privileged(&self) -> bool {
        match self {
            Self::Admin => true,
            Self::Session(a) => a.role.is_privileged(),
        }
    }

    pub fn session(&self) -> Option<&SessionAuth> {
        match self {
            Self::Session(a) => Some(a),
            Self::Admin => None,
        }
    }
}

/// What a valid session token authorizes: the user profile that logged in, the
/// agent it resolved to (the user's `default_agent`, empty if none — the client
/// then picks an agent via the existing `hello{agent_id}` step), and the role
/// derived from that profile (owner vs user).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionAuth {
    pub user_id:  String,
    pub agent_id: String,
    pub role:     AuthRole,
}

impl SessionAuth {
    pub fn new(user_id: impl Into<String>, agent_id: impl Into<String>) -> Self {
        let user_id = user_id.into();
        let role = AuthRole::for_user_id(&user_id);
        Self { user_id, agent_id: agent_id.into(), role }
    }
}

/// LAN login is closed until the owner profile has a PIN.
pub fn lan_login_open(owner_claimed: bool) -> bool {
    owner_claimed
}

/// `/api/auth/setup` may claim the node from loopback or with the admin token.
pub fn setup_permitted(owner_claimed: bool, loopback: bool, admin_token: bool) -> bool {
    !owner_claimed && (loopback || admin_token)
}

pub fn is_loopback_addr(addr: &SocketAddr) -> bool {
    addr.ip().is_loopback()
}

/// An unclaimed node refuses login (even on loopback — use `/api/auth/setup`).
/// After claim, a PIN-less profile may one-tap only on loopback.
pub fn login_permitted(owner_claimed: bool, loopback: bool, profile_has_pin: bool) -> bool {
    if !owner_claimed {
        return false;
    }
    profile_has_pin || loopback
}

pub fn session_owner_file(dir: &Path, id: u64) -> PathBuf {
    dir.join(format!("{id}.owner"))
}

/// Stamp the creating user onto a session once. Never overwrite.
pub fn write_session_owner(dir: &Path, id: u64, user_id: &str) {
    if user_id.is_empty() {
        return;
    }
    let p = session_owner_file(dir, id);
    if p.exists() {
        return;
    }
    let _ = std::fs::create_dir_all(dir);
    let _ = std::fs::write(p, user_id);
}

pub fn read_session_owner(dir: &Path, id: u64) -> Option<String> {
    std::fs::read_to_string(session_owner_file(dir, id))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Owner sees their own + unowned (legacy) sessions. User sees only their own.
/// Admin is handled by the caller (always visible).
pub fn session_visible_to(dir: &Path, id: u64, auth: &SessionAuth) -> bool {
    match read_session_owner(dir, id) {
        None => auth.role.is_privileged(),
        Some(owner) => owner == auth.user_id,
    }
}

struct Entry {
    auth:       SessionAuth,
    expires_at: Instant,
}

/// In-memory session-token store. Tokens are opaque random strings and ARE the map
/// key, so verification is a direct hashmap lookup (no constant-time compare loop
/// needed — the 256-bit space defeats guessing, unlike a low-entropy admin token).
#[derive(Default)]
pub struct SessionStore {
    sessions: HashMap<String, Entry>,
}

impl SessionStore {
    /// Insert a freshly-minted `token` valid for `ttl` from `now`.
    pub fn insert(&mut self, token: String, auth: SessionAuth, now: Instant, ttl: Duration) {
        self.sessions.insert(token, Entry { auth, expires_at: now + ttl });
    }

    /// The auth a token grants, iff it exists and hasn't expired at `now`.
    pub fn verify(&self, token: &str, now: Instant) -> Option<&SessionAuth> {
        if token.is_empty() {
            return None;
        }
        self.sessions.get(token).filter(|e| e.expires_at > now).map(|e| &e.auth)
    }

    /// Drop a token (logout). Returns whether it existed.
    pub fn revoke(&mut self, token: &str) -> bool {
        self.sessions.remove(token).is_some()
    }

    /// Evict all entries expired at `now` (called opportunistically on login so the
    /// map can't grow unboundedly from abandoned sessions).
    pub fn sweep(&mut self, now: Instant) {
        self.sessions.retain(|_, e| e.expires_at > now);
    }

    pub fn len(&self) -> usize { self.sessions.len() }
    pub fn is_empty(&self) -> bool { self.sessions.is_empty() }
}

/// Resolve which agent a **session-authenticated** connection may bind to
/// (agent-identity.md slice 3e — auth-gating the multi-agent `hello{agent_id}`).
///
/// A logged-in human may only act as an agent **they own**, so a guest can never
/// bind APEX (the node owner's agent) and inherit its Cerebro memory. `owned` is
/// the agent ids the session's user owns (`Identities::agents_for(user)`):
///
/// - requested id is owned → bind it;
/// - requested id is empty / not owned → **fall back to the user's own default
///   agent** (`auth.agent_id`, set from `default_agent` at login) if that's owned;
/// - nothing valid → `None` (leave unbound → the session resolves to the node
///   default; only reachable for a profile that owns no agents, a setup error).
///
/// The **admin / token-less** path is NOT gated (a trusted operator binds anything)
/// — that case is handled by the caller and never reaches here.
pub fn gate_agent_bind(auth: &SessionAuth, requested: &str, owned: &[String]) -> Option<String> {
    let owns = |a: &str| !a.is_empty() && owned.iter().any(|o| o == a);
    if owns(requested) {
        Some(requested.to_string())
    } else if owns(&auth.agent_id) {
        Some(auth.agent_id.clone())
    } else {
        None
    }
}

/// A fresh 256-bit session token: hex of 32 bytes from the OS CSPRNG
/// (`/dev/urandom`, same source as the mesh pairing code — no `rand` dependency).
pub fn gen_session_token() -> String {
    use std::io::Read;
    let mut buf = [0u8; 32];
    // A read failure leaves the buffer zeroed; paired with the empty/zero guards in
    // `verify`, a degenerate token still can't authorize anything it shouldn't, and
    // login surfaces no token. In practice /dev/urandom never fails on Linux.
    let _ = std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut buf));
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn auth() -> SessionAuth {
        SessionAuth::new("andre", "APEX")
    }

    #[test]
    fn verifies_within_ttl() {
        let mut s = SessionStore::default();
        let t0 = Instant::now();
        s.insert("tok".into(), auth(), t0, Duration::from_secs(100));
        assert_eq!(s.verify("tok", t0 + Duration::from_secs(50)), Some(&auth()));
    }

    #[test]
    fn rejects_expired() {
        let mut s = SessionStore::default();
        let t0 = Instant::now();
        s.insert("tok".into(), auth(), t0, Duration::from_secs(100));
        assert_eq!(s.verify("tok", t0 + Duration::from_secs(101)), None);
    }

    #[test]
    fn rejects_unknown_and_empty() {
        let s = SessionStore::default();
        assert_eq!(s.verify("nope", Instant::now()), None);
        assert_eq!(s.verify("", Instant::now()), None);
    }

    #[test]
    fn revoke_drops_token() {
        let mut s = SessionStore::default();
        let t0 = Instant::now();
        s.insert("tok".into(), auth(), t0, Duration::from_secs(100));
        assert!(s.revoke("tok"));
        assert_eq!(s.verify("tok", t0), None);
        assert!(!s.revoke("tok")); // second revoke is a no-op
    }

    #[test]
    fn sweep_evicts_only_expired() {
        let mut s = SessionStore::default();
        let t0 = Instant::now();
        s.insert("a".into(), auth(), t0, Duration::from_secs(10));
        s.insert("b".into(), auth(), t0, Duration::from_secs(100));
        s.sweep(t0 + Duration::from_secs(50));
        assert_eq!(s.len(), 1);
        assert!(s.verify("b", t0 + Duration::from_secs(50)).is_some());
        assert!(s.verify("a", t0 + Duration::from_secs(50)).is_none());
    }

    #[test]
    fn gate_binds_owned_else_falls_back_to_default() {
        let owned = vec!["LUMA".to_string(), "SAGE".to_string()];
        let auth = SessionAuth::new("andre", "LUMA");
        // Owned request → bound as asked.
        assert_eq!(gate_agent_bind(&auth, "SAGE", &owned), Some("SAGE".into()));
        // Disallowed request → falls back to the user's own default (LUMA).
        assert_eq!(gate_agent_bind(&auth, "APEX", &owned), Some("LUMA".into()));
        // No request → also falls back to the default.
        assert_eq!(gate_agent_bind(&auth, "", &owned), Some("LUMA".into()));
    }

    #[test]
    fn gate_returns_none_when_nothing_owned() {
        // A profile with no agents (and a default it doesn't own) binds nothing.
        let auth = SessionAuth::new("guest", "GONE");
        assert_eq!(gate_agent_bind(&auth, "APEX", &[]), None);
        // Default not in the owned set → no fallback either.
        let owned = vec!["MOTE".to_string()];
        assert_eq!(gate_agent_bind(&auth, "APEX", &owned), None);
    }

    #[test]
    fn generated_tokens_are_long_and_distinct() {
        let a = gen_session_token();
        let b = gen_session_token();
        assert_eq!(a.len(), 64); // 32 bytes → 64 hex chars
        assert_ne!(a, b);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn owner_user_id_is_privileged_guest_is_not() {
        let owner = SessionAuth::new(apexos_core::DEFAULT_USER_ID, "APEX");
        assert_eq!(owner.role, AuthRole::Owner);
        assert!(owner.role.is_privileged());
        let guest = SessionAuth::new("guest", "MOTE");
        assert_eq!(guest.role, AuthRole::User);
        assert!(!guest.role.is_privileged());
        assert!(RequestAuth::Admin.is_privileged());
        assert!(!RequestAuth::Session(guest).is_privileged());
    }

    #[test]
    fn login_closed_until_claimed_and_lan_needs_pin() {
        assert!(!login_permitted(false, true, false));
        assert!(!login_permitted(false, false, true));
        assert!(login_permitted(true, true, false)); // loopback open guest
        assert!(!login_permitted(true, false, false)); // LAN open profile
        assert!(login_permitted(true, false, true)); // LAN + PIN
    }

    #[test]
    fn setup_only_when_unclaimed_and_local_or_admin() {
        assert!(setup_permitted(false, true, false));
        assert!(setup_permitted(false, false, true));
        assert!(!setup_permitted(false, false, false));
        assert!(!setup_permitted(true, true, true));
    }

    #[test]
    fn session_visibility_legacy_unowned_is_owner_only() {
        let dir = std::env::temp_dir().join(format!("apex-own-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let owner = SessionAuth::new(apexos_core::DEFAULT_USER_ID, "APEX");
        let guest = SessionAuth::new("guest", "MOTE");
        assert!(session_visible_to(&dir, 7, &owner), "unowned legacy → owner");
        assert!(!session_visible_to(&dir, 7, &guest), "unowned legacy ↛ guest");
        write_session_owner(&dir, 7, "guest");
        write_session_owner(&dir, 7, "owner"); // must not overwrite
        assert_eq!(read_session_owner(&dir, 7).as_deref(), Some("guest"));
        assert!(session_visible_to(&dir, 7, &guest));
        assert!(!session_visible_to(&dir, 7, &owner));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
