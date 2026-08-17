/// Peer registry — reads/writes /etc/agentd/peers.toml.
/// Shared via Arc<RwLock<PeerRegistry>> between gateway routes and the
/// discovery loop in main. All writes are atomic (write tmp → rename).
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(rename_all = "lowercase")]
pub enum PeerRole {
    #[default]
    Full,
    Sensor,
    Thin,
}

impl std::fmt::Display for PeerRole {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PeerRole::Full   => write!(f, "full"),
            PeerRole::Sensor => write!(f, "sensor"),
            PeerRole::Thin   => write!(f, "thin"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerRecord {
    pub node_id: String,
    pub ws_url:  String,
    #[serde(default)]
    pub role:    PeerRole,
    #[serde(default = "online")]
    pub status:  String,
    /// Outbound mesh credential — what we present when calling this peer.
    /// Minted by the peer for us during pairing. Never the node AGENTD_TOKEN
    /// (finding 6). Persisted to peers.toml (0600); redacted from the JSON API.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token:   Option<String>,
    /// Inbound mesh credential — what this peer must present when calling us.
    /// We mint it; they store it as their outbound `token`. Distinct from
    /// `token` so a stolen outbound secret cannot impersonate us to ourselves.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inbound_token: Option<String>,
    /// ApexNET radio id of this peer's brainstem (P5d). Optional: BLE
    /// fallback for a2a needs it; WifiLan uses `node_id` + `ws_url`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub radio_id: Option<u16>,
}

fn online() -> String { "online".into() }

#[derive(Debug, Clone, Deserialize, Default)]
struct PeersFile {
    #[serde(default)]
    peer: Vec<PeerRecord>,
}

#[derive(Debug, Clone)]
pub struct PeerRegistry {
    pub peers: Vec<PeerRecord>,
    pub path:  PathBuf,
}

impl PeerRegistry {
    pub fn load(path: &Path) -> Self {
        let peers = std::fs::read_to_string(path)
            .ok()
            .and_then(|s| toml::from_str::<PeersFile>(&s).ok())
            .map(|f| f.peer)
            .unwrap_or_default();
        PeerRegistry { peers, path: path.to_path_buf() }
    }

    pub fn contains(&self, node_id: &str) -> bool {
        self.peers.iter().any(|p| p.node_id == node_id)
    }

    /// Which registered peer minted `token` as their inbound credential.
    /// Scans with constant-time compare so a timing side-channel cannot
    /// enumerate which peer a token belongs to.
    pub fn peer_id_for_inbound_token(&self, token: &str) -> Option<String> {
        if token.is_empty() {
            return None;
        }
        use subtle::ConstantTimeEq;
        let mut found = None;
        for p in &self.peers {
            let Some(t) = p.inbound_token.as_deref() else { continue };
            if t.len() == token.len() && t.as_bytes().ct_eq(token.as_bytes()).into() {
                found = Some(p.node_id.clone());
            }
        }
        found
    }

    pub fn add(&mut self, record: PeerRecord) -> std::io::Result<()> {
        if let Some(existing) = self.peers.iter_mut().find(|p| p.node_id == record.node_id) {
            *existing = record;
        } else {
            self.peers.push(record);
        }
        self.save()
    }

    pub fn remove(&mut self, node_id: &str) -> std::io::Result<bool> {
        let before = self.peers.len();
        self.peers.retain(|p| p.node_id != node_id);
        if self.peers.len() != before { self.save()?; Ok(true) } else { Ok(false) }
    }

    pub fn set_status(&mut self, node_id: &str, status: &str) -> std::io::Result<()> {
        if let Some(p) = self.peers.iter_mut().find(|p| p.node_id == node_id) {
            p.status = status.to_string();
        }
        self.save()
    }

    fn save(&self) -> std::io::Result<()> {
        let mut out = String::from("# ApexOS mesh peers — managed by agentd\n");
        for p in &self.peers {
            out.push_str(&format!(
                "\n[[peer]]\nnode_id = {:?}\nws_url  = {:?}\nrole    = {:?}\nstatus  = {:?}\n",
                p.node_id, p.ws_url, p.role.to_string(), p.status,
            ));
            // Secret — only written here (and to 0600 below), never to the JSON API.
            if let Some(ref tok) = p.token {
                out.push_str(&format!("token   = {:?}\n", tok));
            }
            if let Some(ref tok) = p.inbound_token {
                out.push_str(&format!("inbound_token = {:?}\n", tok));
            }
            if let Some(id) = p.radio_id {
                out.push_str(&format!("radio_id = {id}\n"));
            }
        }
        // Atomic write (temp + rename) when the dir is writable; fall back to an
        // in-place write when it isn't. /etc/agentd is root-owned (the auth-token
        // env file must stay 600 root:root), so the agentd user can write peers.toml
        // itself (install.sh chowns the file) but CANNOT create a sibling tempfile —
        // the temp+rename then fails with EPERM (os error 13), which silently broke
        // "add peer" from the mesh UI. Mirrors write_atomic() in agentd/main.rs.
        let tmp = self.path.with_extension("toml.tmp");
        let res = match std::fs::write(&tmp, &out).and_then(|()| std::fs::rename(&tmp, &self.path)) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                let _ = std::fs::remove_file(&tmp); // best-effort; may never have been created
                std::fs::write(&self.path, &out)
            }
            Err(e) => {
                let _ = std::fs::remove_file(&tmp);
                Err(e)
            }
        };
        // peers.toml now holds per-peer tokens — keep it owner-only (0600). Either
        // write path can land it at the umask default (0644), so clamp every time.
        if res.is_ok() {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&self.path, std::fs::Permissions::from_mode(0o600));
        }
        res
    }
}

// ── Mesh pairing (kiosk-friendly onboarding) ────────────────────────────────────
//
// To pair node A ↔ B without hand-typing a 64-char token (and without a phone),
// B shows a short code; A redeems it. Each side mints a 256-bit mesh credential
// for the other (never AGENTD_TOKEN). B callbacks A's advertised URL
// (`/api/mesh/pair/confirm`, nonce-bound) before disclosing a token. The offer
// lives in memory only — never persisted — and is single-use, expiring, and
// locks out after too many bad guesses.

/// One active pairing offer (one per node at a time).
pub struct Pairing {
    pub code:       String,
    pub expires_at: std::time::Instant,
    pub attempts:   u8,
}

/// Inserted on a request authenticated by a per-peer inbound mesh token.
#[derive(Clone, Debug)]
pub struct MeshPeerAuth {
    pub node_id: String,
}

/// In-flight redeem on the initiator — `pair/confirm` must echo this nonce
/// (proves the remote answered at the claimed URL) before we store them.
pub struct RedeemFlight {
    pub peer_http:  String,
    pub nonce:      String,
    pub expires_at: std::time::Instant,
}

/// 256-bit mesh credential (64 hex). Same source as session tokens.
pub fn gen_mesh_token() -> String {
    use std::io::Read;
    let mut buf = [0u8; 32];
    let _ = std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut buf));
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn is_mesh_token_shape(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Rewrite `ws://`/`wss://` to http(s). Rejects anything that is not a
/// host:port HTTP URL — redeem must never POST secrets at a caller-chosen
/// `file:` / empty / garbage target.
pub fn mesh_http_base(ws_url: &str) -> Option<String> {
    let s = ws_url.trim();
    let http = if let Some(rest) = s.strip_prefix("ws://") {
        format!("http://{rest}")
    } else if let Some(rest) = s.strip_prefix("wss://") {
        format!("https://{rest}")
    } else if s.starts_with("http://") || s.starts_with("https://") {
        s.to_string()
    } else {
        return None;
    };
    let rest = http
        .strip_prefix("https://")
        .or_else(|| http.strip_prefix("http://"))?;
    let host = rest.split('/').next().unwrap_or("");
    if host.is_empty() || host.contains('@') || host.contains(' ') {
        return None;
    }
    Some(http.trim_end_matches('/').to_string())
}

/// Refuse to persist the node admin token as a mesh credential.
pub fn is_node_admin_token(candidate: &str, admin: &str) -> bool {
    if candidate.is_empty() || admin.is_empty() || candidate.len() != admin.len() {
        return false;
    }
    use subtle::ConstantTimeEq;
    candidate.as_bytes().ct_eq(admin.as_bytes()).into()
}

/// Pairing-code lifetime and the bad-guess lockout (which invalidates the code).
pub const PAIR_TTL_SECS:     u64 = 300;
pub const PAIR_MAX_ATTEMPTS: u8  = 5;

/// A fresh 6-digit pairing code from the OS CSPRNG (/dev/urandom).
pub fn gen_pair_code() -> String {
    use std::io::Read;
    let mut buf = [0u8; 4];
    let n = std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut buf).map(|_| u32::from_le_bytes(buf)))
        .unwrap_or(0)
        % 1_000_000;
    format!("{n:06}")
}

/// Parse `avahi-browse -rpt _apexos._tcp --no-db-lookup` stdout into (node_id, ip) pairs.
/// Only processes fully-resolved lines (starting with `=`).
pub fn parse_avahi_output(raw: &str) -> Vec<(String, String)> {
    let mut results = Vec::new();
    for line in raw.lines() {
        if !line.starts_with('=') { continue; }
        // =;eth0;IPv4;ApexOS apex-kitchen;_apexos._tcp;local;apex-kitchen.local;192.168.0.201;8787;...
        let parts: Vec<&str> = line.split(';').collect();
        if parts.len() < 9 { continue; }
        let hostname = parts[6].trim_end_matches(".local");
        let ip = parts[7];
        // The mesh is IPv4:8787 throughout (ws://{ip}:8787, no bracket handling), and
        // avahi lists each node on BOTH an IPv4 and an IPv6 line. Skip IPv6: a
        // link-local fe80:: address makes a malformed, unusable ws_url and shows up as
        // a duplicate "already known" row in /api/mesh/nodes — which silently hid the
        // real IPv4 row from the UI's "+ ADD".
        if ip.contains(':') { continue; }
        if !ip.is_empty() && !hostname.is_empty() {
            results.push((hostname.to_string(), ip.to_string()));
        }
    }
    results
}

// ── federation memory relay (colony-federation Slice 1) ─────────────────────

/// Bounds on an inbound federated memory — a memory is distilled knowledge, not
/// a blob. Content over the cap is rejected (never silently truncated).
pub const FED_MEMORY_MAX_CHARS: usize = 60_000;
const FED_NOTE_MAX_CHARS: usize = 500;
const FED_MAX_TAGS: usize = 16;
const FED_TAG_MAX_CHARS: usize = 48;

/// Build the local-Cerebro `remember` args (and a board preview) for a memory
/// arriving from mesh peer `from_node`. Pure — all validation + the provenance
/// stamping happens here, unit-tested; the handler is IO glue.
///
/// Provenance is SYSTEM-stamped as tags (`colony` · `from:<node>` ·
/// `origin:<sender memory id>`) — the sender cannot forge or omit them, and a
/// peer's imports stay one tag-filter away from bulk cleanup. The memory lands
/// in `agent_id`'s space (the receiving node's own agent) with
/// `visibility:"private"` receiver-stamped (SA-3). A missing visibility would
/// default Shared (R-05) and leak through `shared_only` federated recall.
/// Sharing it onward is the receiving agent's `share_memory` call.
pub fn federated_remember_args(
    from_node: &str,
    agent_id: &str,
    body: &serde_json::Value,
) -> Result<(serde_json::Value, String), String> {
    let mem = &body["memory"];
    let content = mem["content"].as_str().map(str::trim).unwrap_or("");
    if content.is_empty() {
        return Err("memory.content (non-empty string) required".into());
    }
    if content.chars().count() > FED_MEMORY_MAX_CHARS {
        return Err(format!("memory.content exceeds the {FED_MEMORY_MAX_CHARS}-char cap"));
    }

    // Sender tags survive (bounded + cleaned) — but any provenance-SHAPED tag
    // the sender supplied is stripped outright, so the stamp appended below is
    // always ours: a peer can't forge `from:`/`origin:` provenance.
    let mut tags: Vec<String> = mem["tags"].as_array()
        .map(|a| a.iter()
            .filter_map(|t| t.as_str())
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty() && t.chars().count() <= FED_TAG_MAX_CHARS)
            .filter(|t| t != "colony" && !t.starts_with("from:") && !t.starts_with("origin:"))
            .take(FED_MAX_TAGS)
            .collect())
        .unwrap_or_default();
    tags.push("colony".into());
    tags.push(format!("from:{from_node}"));
    if let Some(origin) = body["origin_memory_id"].as_str().filter(|s| !s.trim().is_empty()) {
        tags.push(format!("origin:{}", origin.trim()));
    }

    // The sender's optional "why this matters" rides as an attributed suffix.
    let mut full = content.to_string();
    if let Some(note) = body["note"].as_str().map(str::trim).filter(|n| !n.is_empty()) {
        let note: String = note.chars().take(FED_NOTE_MAX_CHARS).collect();
        full.push_str(&format!("\n\n[note from {from_node}]: {note}"));
    }

    let mut args = serde_json::json!({
        "content":    full,
        "agent_id":   agent_id,
        "tags":       tags,
        // SA-3: never omit this — remember() defaults Shared and the import
        // would be immediately re-exportable via mesh_recall / shared_only.
        // Sender-supplied visibility is ignored; publish is share_memory.
        "visibility": "private",
    });
    // Type + salience are preserved when valid, else dropped (auto-classified).
    let mut memory_type = None;
    if let Some(t) = mem["memory_type"].as_str() {
        if ["episodic", "semantic", "procedural", "affective", "prospective", "schematic"]
            .contains(&t)
        {
            args["memory_type"] = serde_json::json!(t);
            memory_type = Some(t);
        }
    }
    // PROCEDURES re-earn their fitness locally (colony-federation Slice 4): the
    // sender's salience is part of its darwin signal, earned on a different
    // embodiment — so it is dropped and the local pipeline assigns fresh. The
    // origin track record rides in the note as *context*, never as stats.
    let is_procedure = memory_type == Some("procedural");
    if !is_procedure {
        if let Some(s) = mem["salience"].as_f64() {
            args["salience"] = serde_json::json!(s.clamp(0.0, 1.0));
        }
    }

    let preview: String = content.chars().take(120).collect();
    Ok((args, preview))
}

/// Max snippet length in a federated recall hit — a peer gets enough to judge
/// relevance, never the full store row (charter: bounded hits, no dumps).
const FED_HIT_SNIPPET_CHARS: usize = 300;

/// Shape a local `recall` result (an array of `{memory, score}`) into the
/// bounded hit list a federated query returns: memory_id · snippet ·
/// memory_type · tags · salience · score. Pure — unit-tested; anything
/// non-conforming in the input is skipped, and `cap` bounds the count.
pub fn federated_recall_hits(recall_out: &serde_json::Value, cap: usize) -> Vec<serde_json::Value> {
    let Some(arr) = recall_out.as_array() else { return Vec::new() };
    arr.iter()
        .filter_map(|entry| {
            let mem = entry.get("memory")?;
            let content = mem["content"].as_str()?;
            if content.trim().is_empty() {
                return None;
            }
            let snippet: String = content.chars().take(FED_HIT_SNIPPET_CHARS).collect();
            Some(serde_json::json!({
                "memory_id":   mem["id"],
                "snippet":     snippet,
                "memory_type": mem["memory_type"],
                "tags":        mem["tags"],
                "salience":    mem["salience"],
                "score":       entry["score"],
            }))
        })
        .take(cap)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_resolved_line() {
        let raw = "=;eth0;IPv4;ApexOS apex-kitchen;_apexos._tcp;local;apex-kitchen.local;192.168.0.201;8787;\n\
                   +;eth0;IPv4;ApexOS apex-garage;_apexos._tcp;local;;;;\n";
        let nodes = parse_avahi_output(raw);
        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0].0, "apex-kitchen");
        assert_eq!(nodes[0].1, "192.168.0.201");
    }

    #[test]
    fn parse_skips_ipv6_keeps_ipv4() {
        // avahi lists the same node on both an IPv6 (link-local) and an IPv4 line.
        // Only the IPv4 line should survive — the IPv6 one yields an unusable ws_url.
        let raw = "=;eth0;IPv6;ApexOS apex-kitchen;_apexos._tcp;local;apex-kitchen.local;fe80::2ecf:67ff:fe93:e90e;8787;\n\
                   =;eth0;IPv4;ApexOS apex-kitchen;_apexos._tcp;local;apex-kitchen.local;192.168.0.201;8787;\n";
        let nodes = parse_avahi_output(raw);
        assert_eq!(nodes.len(), 1, "IPv6 line must be skipped");
        assert_eq!(nodes[0], ("apex-kitchen".to_string(), "192.168.0.201".to_string()));
    }

    // The real "add peer fails" bug: /etc/agentd is root-owned, so the temp+rename
    // can't create a sibling tempfile and must fall back to an in-place write of the
    // (agentd-owned) peers.toml. Under non-root the read-only dir forces that path.
    #[test]
    fn save_falls_back_in_place_when_dir_readonly() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("apexrs-peers-ro-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("peers.toml");
        std::fs::write(&path, "# seed\n").unwrap();         // pre-existing writable file
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o555)).unwrap(); // read-only dir

        let mut reg = PeerRegistry { peers: vec![], path: path.clone() };
        let res = reg.add(PeerRecord {
            node_id: "apex-garage".into(),
            ws_url:  "ws://192.168.0.201:8787".into(),
            role:    PeerRole::Full,
            status:  "online".into(),
            token:   None,
            inbound_token: None,
            radio_id: None,
        });

        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap(); // restore for cleanup
        res.expect("add() must fall back to an in-place write when the dir is read-only");
        let written = std::fs::read_to_string(&path).unwrap();
        assert!(written.contains("apex-garage"), "peer should be persisted in place");
        assert!(!path.with_extension("toml.tmp").exists(), "temp file must not linger");
        std::fs::remove_dir_all(&dir).ok();
    }

    // Per-peer a2a token must survive a save()→load() round-trip, and peers.toml
    // must be owner-only (0600) since it now holds that secret credential.
    #[test]
    fn token_round_trips_and_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("apexrs-peers-tok-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("peers.toml");

        let mut reg = PeerRegistry { peers: vec![], path: path.clone() };
        let inbound = "aa".repeat(32);
        reg.add(PeerRecord {
            node_id: "ApexOS-RS".into(),
            ws_url:  "ws://192.168.0.158:8787".into(),
            role:    PeerRole::Full,
            status:  "online".into(),
            token:   Some("deadbeef-secret".into()),
            inbound_token: Some(inbound.clone()),
            radio_id: Some(7),
        }).unwrap();

        let loaded = PeerRegistry::load(&path);
        assert_eq!(loaded.peers.len(), 1);
        assert_eq!(loaded.peers[0].token.as_deref(), Some("deadbeef-secret"),
                   "token must round-trip through peers.toml");
        assert_eq!(loaded.peers[0].inbound_token.as_deref(), Some(inbound.as_str()),
                   "inbound_token must round-trip");
        assert_eq!(loaded.peer_id_for_inbound_token(&inbound).as_deref(), Some("ApexOS-RS"));
        assert_eq!(loaded.peer_id_for_inbound_token("nope"), None);
        assert_eq!(loaded.peers[0].radio_id, Some(7), "radio_id must round-trip");

        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "peers.toml holds secrets — must be owner-only");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn mesh_http_base_accepts_ws_and_rejects_garbage() {
        assert_eq!(mesh_http_base("ws://192.168.0.9:8787"), Some("http://192.168.0.9:8787".into()));
        assert_eq!(mesh_http_base("wss://n.example:443/"), Some("https://n.example:443".into()));
        assert_eq!(mesh_http_base("http://192.168.0.9:8787"), Some("http://192.168.0.9:8787".into()));
        assert_eq!(mesh_http_base("file:///etc/passwd"), None);
        assert_eq!(mesh_http_base("ws://user:pass@evil"), None);
        assert_eq!(mesh_http_base(""), None);
        assert_eq!(mesh_http_base("not-a-url"), None);
    }

    #[test]
    fn mesh_token_shape_and_admin_reject() {
        let t = gen_mesh_token();
        assert!(is_mesh_token_shape(&t));
        assert!(!is_mesh_token_shape("peer-token"));
        assert!(!is_mesh_token_shape(""));
        assert!(is_node_admin_token("secret", "secret"));
        assert!(!is_node_admin_token("secret", "other"));
        assert!(!is_node_admin_token("secret", ""));
    }

    #[test]
    fn pair_code_is_six_digits() {
        for _ in 0..50 {
            let c = gen_pair_code();
            assert_eq!(c.len(), 6, "code {c:?} must be 6 chars");
            assert!(c.chars().all(|ch| ch.is_ascii_digit()), "code {c:?} must be all digits");
        }
    }

    // ── federation memory relay ─────────────────────────────────────────────

    #[test]
    fn federated_import_stamps_provenance_and_preserves_fields() {
        let body = serde_json::json!({
            "from": "apex1", "origin_memory_id": "mem_abc",
            "memory": {
                "content": "BME688 gas baseline drifts for ~48h after power-on.",
                "memory_type": "semantic",
                "tags": ["sensors", "bme688"],
                "salience": 0.8,
            },
        });
        let (args, preview) = federated_remember_args("apex1", "APEX", &body).unwrap();
        assert_eq!(args["agent_id"], "APEX");
        assert_eq!(args["visibility"], "private", "SA-3: import is receiver-stamped private");
        assert_eq!(args["memory_type"], "semantic");
        assert_eq!(args["salience"], 0.8);
        let tags: Vec<&str> = args["tags"].as_array().unwrap().iter().map(|t| t.as_str().unwrap()).collect();
        assert!(tags.contains(&"sensors") && tags.contains(&"bme688"), "sender tags survive");
        assert!(tags.contains(&"colony") && tags.contains(&"from:apex1") && tags.contains(&"origin:mem_abc"),
            "provenance stamped: {tags:?}");
        assert!(preview.starts_with("BME688"));
    }

    #[test]
    fn federated_import_strips_forged_provenance_tags() {
        let body = serde_json::json!({
            "from": "apex2",
            "memory": {
                "content": "x",
                "tags": ["from:apex1", "origin:mem_fake", "colony", "real-tag"],
            },
        });
        let (args, _) = federated_remember_args("apex2", "APEX", &body).unwrap();
        let tags: Vec<&str> = args["tags"].as_array().unwrap().iter().map(|t| t.as_str().unwrap()).collect();
        assert!(tags.contains(&"from:apex2"), "our stamp present");
        assert!(!tags.contains(&"from:apex1"), "forged from: stripped");
        assert!(!tags.contains(&"origin:mem_fake"), "forged origin: stripped");
        assert_eq!(tags.iter().filter(|t| **t == "colony").count(), 1, "single colony tag");
        assert!(tags.contains(&"real-tag"));
    }

    #[test]
    fn federated_import_validates_and_bounds() {
        // Missing/empty content rejected.
        assert!(federated_remember_args("a", "APEX", &serde_json::json!({"memory": {}})).is_err());
        // Over-cap content rejected, not truncated.
        let huge = serde_json::json!({"memory": {"content": "x".repeat(FED_MEMORY_MAX_CHARS + 1)}});
        assert!(federated_remember_args("a", "APEX", &huge).is_err());
        // Invalid memory_type + out-of-range salience are dropped/clamped.
        let odd = serde_json::json!({"memory": {
            "content": "c", "memory_type": "not-a-type", "salience": 7.5 }});
        let (args, _) = federated_remember_args("a", "APEX", &odd).unwrap();
        assert!(args["memory_type"].is_null(), "bad type dropped (auto-classify)");
        assert_eq!(args["salience"], 1.0, "salience clamped");
        // The note rides as an attributed suffix.
        let noted = serde_json::json!({
            "from": "apex1", "note": "calibration context for your sensor work",
            "memory": {"content": "c"}});
        let (args, _) = federated_remember_args("apex1", "APEX", &noted).unwrap();
        assert!(args["content"].as_str().unwrap().contains("[note from apex1]: calibration"));
    }

    #[test]
    fn federated_procedure_import_drops_sender_salience() {
        // A procedure's fitness is re-earned per node (Slice 4): the sender's
        // salience never rides in; other types keep it (clamped).
        let proc = serde_json::json!({
            "from": "apex3",
            "memory": { "content": "1. do X  2. verify Y", "memory_type": "procedural",
                        "tags": ["procedure"], "salience": 0.95 },
        });
        let (args, _) = federated_remember_args("apex3", "APEX", &proc).unwrap();
        assert_eq!(args["memory_type"], "procedural");
        assert!(args["salience"].is_null(), "sender fitness dropped — re-earned locally");

        let sem = serde_json::json!({
            "from": "apex3",
            "memory": { "content": "c", "memory_type": "semantic", "salience": 0.95 },
        });
        let (args, _) = federated_remember_args("apex3", "APEX", &sem).unwrap();
        assert_eq!(args["salience"], 0.95, "non-procedures keep sender salience");
    }

    #[test]
    fn federated_import_ignores_sender_visibility() {
        // A peer that marks the payload Shared (or omits visibility) must still
        // land private. Publish is the receiver's share_memory, not the wire.
        let shared = serde_json::json!({
            "from": "apex1",
            "memory": {
                "content": "one-hop import that claimed to be shared",
                "visibility": "shared",
            },
        });
        let (args, _) = federated_remember_args("apex1", "APEX", &shared).unwrap();
        assert_eq!(args["visibility"], "private");
        let omitted = serde_json::json!({
            "from": "apex1",
            "memory": { "content": "one-hop import with no visibility field" },
        });
        let (args, _) = federated_remember_args("apex1", "APEX", &omitted).unwrap();
        assert_eq!(args["visibility"], "private");
    }

    #[test]
    fn federated_recall_hits_are_bounded_snippets() {
        let recall_out = serde_json::json!([
            { "memory": { "id": "mem_1", "content": "x".repeat(1000),
                          "memory_type": "semantic", "tags": ["colony"], "salience": 0.9 },
              "score": 0.83 },
            { "memory": { "id": "mem_2", "content": "short one",
                          "memory_type": "procedural", "tags": [], "salience": 0.5 },
              "score": 0.61 },
            { "not-a-memory": true },                       // skipped, not fatal
            { "memory": { "id": "mem_3", "content": "over the cap" }, "score": 0.4 },
        ]);
        let hits = federated_recall_hits(&recall_out, 2);
        assert_eq!(hits.len(), 2, "cap applied");
        assert_eq!(hits[0]["memory_id"], "mem_1");
        assert_eq!(hits[0]["snippet"].as_str().unwrap().chars().count(), 300, "snippet bounded");
        assert_eq!(hits[0]["score"], 0.83);
        assert_eq!(hits[1]["memory_id"], "mem_2");
        assert_eq!(hits[1]["snippet"], "short one");
        // Junk input degrades to empty, never panics.
        assert!(federated_recall_hits(&serde_json::json!("nope"), 5).is_empty());
    }
}
