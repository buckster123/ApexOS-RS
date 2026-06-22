//! Human↔node session authentication (agent-identity.md slice 3e).
//!
//! A login (profile + optional PIN) mints a short-lived bearer token the UI / PWA
//! uses for the WS + API, so a human client never needs the node's shared
//! `AGENTD_TOKEN` — that retreats to being the machine / mesh / admin secret
//! (node↔node a2a tokens, kiosk-as-root, operator curl/CI). The gate
//! (`require_token`) accepts EITHER the admin token OR a valid minted session
//! token.
//!
//! In-memory ONLY: a daemon restart clears every session (re-login), so a session
//! token never touches disk. This is the deliberate, safest default — the cost is
//! a re-login after a restart, which on the spare-device tier is fine.
//!
//! The pure store lives here (mint/verify/revoke/sweep), unit-tested with injected
//! `Instant`s; the IO-thin login/logout handlers + the `require_token` hook live in
//! `lib.rs`.

use std::collections::HashMap;
use std::time::{Duration, Instant};

/// Default session lifetime (24 h). Re-login after this, or after a daemon restart.
pub const SESSION_TTL_SECS: u64 = 24 * 60 * 60;

/// What a valid session token authorizes: the user profile that logged in and the
/// agent it resolved to (the user's `default_agent`, empty if none — the client
/// then picks an agent via the existing `hello{agent_id}` step).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionAuth {
    pub user_id:  String,
    pub agent_id: String,
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
        SessionAuth { user_id: "andre".into(), agent_id: "APEX".into() }
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
    fn generated_tokens_are_long_and_distinct() {
        let a = gen_session_token();
        let b = gen_session_token();
        assert_eq!(a.len(), 64); // 32 bytes → 64 hex chars
        assert_ne!(a, b);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
    }
}
