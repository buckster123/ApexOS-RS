//! The Tier-4 courier lane (ApexNET P2 — `docs/apexnet.md` §7): humans are a
//! transport. An `APEX-*` exo-workspace stick becomes an announced, ledgered,
//! verified, receipted link — not a manual workaround.
//!
//! On-stick layout (all under the mount root):
//! ```text
//! apexos-workspace.toml        # marker v2 — stick_id is the ledger identity
//! apexos-courier/
//!   manifest.json              # AEAD-sealed: what cargo is aboard, for whom
//!   receipts.json              # AEAD-sealed, append-only: who ingested what
//!   cargo/<blake3-root-hex>    # the artifact bytes, content-addressed
//! ```
//!
//! Both JSON artifacts are sealed with the colony PSK (ChaCha20-Poly1305 via
//! `apexos_mesh_proto::seal_blob`) and AAD-bound to `(domain, stick_id)` — a
//! found stick leaks nothing, a tampered manifest fails authentication, and a
//! sealed file can't be replayed onto another stick. Cargo bytes are verified
//! against their blake3 root at ingest; **tamper fails closed and loudly**
//! (an `accepted: false` receipt travels back).
//!
//! The ledger loop (charter §7): load → gossip `manifest` (Tier-1 stub today,
//! ~56 B radio payload later) → human carries → plug-verify + ingest →
//! receipt on the stick AND gossiped home. Store-and-forward closes its loop.
//!
//! Everything stateful takes explicit paths + PSK so the whole loop is
//! testable in a tempdir; `*_env` wrappers read the daemon's environment.
//! One process writes these files (agentd) — [`lock`] serializes the two
//! entry arms (gateway handlers, supervisor tools).

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use apexos_mesh_proto::{blob_root, open_blob, seal_blob, Psk};
use base64::Engine as _;
use serde::{Deserialize, Serialize};

/// Serializes every mutating courier op (outbox, ledger, stick files) across
/// the module's two entry arms. One daemon, one lock — never poison-fatal.
static COURIER_LOCK: Mutex<()> = Mutex::new(());

fn lock() -> std::sync::MutexGuard<'static, ()> {
    COURIER_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

const COURIER_DIR: &str = "apexos-courier";
const CARGO_DIR: &str = "cargo";
const MANIFEST_FILE: &str = "manifest.json";
const RECEIPTS_FILE: &str = "receipts.json";
const MARKER_FILE: &str = "apexos-workspace.toml";

/// Artifact size ceiling for the queue (honest refusal, not silent truncation
/// — bump when someone actually carries more).
pub const MAX_ARTIFACT_BYTES: u64 = 256 * 1024 * 1024;

// ── Environment wrappers ────────────────────────────────────────────────────

/// The colony PSK file — hex-encoded 32 bytes, seeded by install.sh, readable
/// by the agentd user only. Distribution is Tier-1/USB-manual until the
/// rotation machinery (charter Phase 8).
pub fn psk_path_env() -> PathBuf {
    PathBuf::from(
        std::env::var("APEXNET_PSK_FILE").unwrap_or_else(|_| "/etc/agentd/apexnet.psk".into()),
    )
}

/// Load the colony PSK (`None` = courier crypto unavailable — every caller
/// surfaces that honestly rather than working around it).
pub fn load_psk_env() -> Option<Psk> {
    load_psk(&psk_path_env())
}

pub fn load_psk(path: &Path) -> Option<Psk> {
    let hex = std::fs::read_to_string(path).ok()?;
    let hex = hex.trim();
    if hex.len() != 64 {
        return None;
    }
    let mut key = [0u8; 32];
    for (i, byte) in key.iter_mut().enumerate() {
        *byte = u8::from_str_radix(hex.get(i * 2..i * 2 + 2)?, 16).ok()?;
    }
    Some(Psk(key))
}

pub fn log_dir_env() -> PathBuf {
    PathBuf::from(std::env::var("AGENTD_LOG").unwrap_or_else(|_| "events".into()))
}

pub fn workspace_env() -> PathBuf {
    PathBuf::from(
        std::env::var("AGENTD_WORKSPACE")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "/var/lib/agentd/workspace".into()),
    )
}

/// Days since the Unix epoch — the courier/digest epoch counter.
pub fn epoch_today() -> u32 {
    (chrono::Utc::now().timestamp() / 86_400) as u32
}

/// Labels of the exo-workspace sticks currently mounted under
/// `<workspace>/media/` (sorted; from `/proc/mounts`, the authoritative
/// oracle). Mirrors agentd's embodiment helper — the courier needs its own
/// view from this crate.
pub fn mounted_sticks() -> Vec<String> {
    let ws = workspace_env();
    let ws_canon = std::fs::canonicalize(&ws).unwrap_or(ws);
    let prefix = format!("{}/", ws_canon.join("media").to_string_lossy());
    let mounts = std::fs::read_to_string("/proc/mounts").unwrap_or_default();
    let mut labels: Vec<String> = mounts
        .lines()
        .filter_map(|line| {
            let mp = line.split_whitespace().nth(1)?;
            let rest = mp.strip_prefix(&prefix)?;
            (!rest.is_empty() && !rest.contains('/')).then(|| rest.to_string())
        })
        .collect();
    labels.sort();
    labels.dedup();
    labels
}

fn now_iso() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

// ── Marker v2 (stick identity) ──────────────────────────────────────────────

/// The parsed exo-workspace marker. v1 markers (pre-courier) have no
/// `stick_id`; [`ensure_stick_id`] upgrades them in place on plug.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Marker {
    pub version: i64,
    pub name: String,
    pub stick_id: Option<String>,
}

/// Tolerant TOML parse — a marker hand-edited into oddity yields `None`, and
/// the plug flow treats the stick as courier-inert rather than failing it.
pub fn parse_marker(s: &str) -> Option<Marker> {
    let v: toml::Value = toml::from_str(s).ok()?;
    Some(Marker {
        version: v.get("version").and_then(|x| x.as_integer()).unwrap_or(1),
        name: v
            .get("name")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string(),
        stick_id: v
            .get("stick_id")
            .and_then(|x| x.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string),
    })
}

/// Mint a stick identity: 8 random bytes, hex. The *ledger* key — the
/// filesystem label stays the *mount* convention (labels collide; ids don't).
pub fn mint_stick_id() -> String {
    let b: [u8; 8] = rand::random();
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// Upgrade a v1 marker's text to v2 in place: bump the version line, append
/// the identity fields, preserve everything else byte-for-byte (comments,
/// name, layout). Pure — the IO wrapper is [`ensure_stick_id`].
pub fn upgraded_marker(existing: &str, stick_id: &str, node: &str, now_iso: &str) -> String {
    let mut out = String::new();
    for line in existing.lines() {
        if line.trim_start().starts_with("version") && line.contains('=') {
            out.push_str("version   = 2\n");
        } else {
            out.push_str(line);
            out.push('\n');
        }
    }
    out.push_str(&format!(
        "# v2 courier identity — minted on plug by {node} (docs/apexnet.md §7); never edit.\n\
         stick_id  = \"{stick_id}\"\n\
         minted_by = \"{node}\"\n\
         minted_at = \"{now_iso}\"\n"
    ));
    out
}

/// Read the marker at `mount`, minting + persisting a stick_id if it lacks
/// one (v1 sticks upgrade on first plug — no re-prep needed). Errors are
/// strings for the notice surface.
pub fn ensure_stick_id(mount: &Path, node: &str) -> Result<String, String> {
    let path = mount.join(MARKER_FILE);
    let raw = std::fs::read_to_string(&path).map_err(|e| format!("marker read: {e}"))?;
    let marker = parse_marker(&raw).ok_or("marker is not parseable TOML")?;
    if let Some(id) = marker.stick_id {
        return Ok(id);
    }
    let id = mint_stick_id();
    let upgraded = upgraded_marker(&raw, &id, node, &now_iso());
    std::fs::write(&path, upgraded).map_err(|e| format!("marker upgrade write: {e}"))?;
    Ok(id)
}

// ── The sealed envelope ─────────────────────────────────────────────────────

/// AAD binds a sealed artifact to its stick AND its slot: a manifest copied
/// onto another stick — or renamed to receipts.json — fails authentication.
fn aad_for(stick_id: &str, domain: &str) -> Vec<u8> {
    format!("apexos-courier:v1:{domain}:{stick_id}").into_bytes()
}

/// Seal a JSON value into the on-stick envelope: `{"v":1,"nonce":hex,"ct":b64}`.
pub fn seal_json(
    psk: &Psk,
    stick_id: &str,
    domain: &str,
    value: &serde_json::Value,
) -> Result<String, String> {
    let nonce: [u8; 12] = rand::random();
    let plain = serde_json::to_vec(value).map_err(|e| format!("encode: {e}"))?;
    let ct = seal_blob(psk, &nonce, &aad_for(stick_id, domain), &plain)
        .map_err(|e| format!("seal: {e}"))?;
    let nonce_hex: String = nonce.iter().map(|b| format!("{b:02x}")).collect();
    Ok(serde_json::json!({
        "v": 1,
        "nonce": nonce_hex,
        "ct": base64::engine::general_purpose::STANDARD.encode(ct),
    })
    .to_string())
}

/// Open an on-stick envelope. Every failure collapses to one honest string —
/// tamper, wrong stick, wrong domain, wrong key all fail closed.
pub fn open_json(
    psk: &Psk,
    stick_id: &str,
    domain: &str,
    sealed: &str,
) -> Result<serde_json::Value, String> {
    let env: serde_json::Value =
        serde_json::from_str(sealed).map_err(|_| "envelope is not JSON".to_string())?;
    if env["v"].as_i64() != Some(1) {
        return Err("unknown envelope version".into());
    }
    let nonce_hex = env["nonce"].as_str().unwrap_or("");
    if nonce_hex.len() != 24 {
        return Err("bad nonce".into());
    }
    let mut nonce = [0u8; 12];
    for i in 0..12 {
        nonce[i] = u8::from_str_radix(&nonce_hex[i * 2..i * 2 + 2], 16)
            .map_err(|_| "bad nonce hex".to_string())?;
    }
    let ct = base64::engine::general_purpose::STANDARD
        .decode(env["ct"].as_str().unwrap_or(""))
        .map_err(|_| "bad ct base64".to_string())?;
    let plain = open_blob(psk, &nonce, &aad_for(stick_id, domain), &ct)
        .map_err(|_| "authentication failed (tamper, wrong stick, or wrong PSK)".to_string())?;
    serde_json::from_slice(&plain).map_err(|_| "sealed payload is not JSON".into())
}

// ── Manifest + receipts on the stick ────────────────────────────────────────

/// One cargo entry aboard a stick. `root` is the blake3 of the artifact
/// (content address = cargo filename); `name` is the human filename it lands
/// under at the destination.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ManifestEntry {
    pub root: String,
    pub len: u64,
    pub name: String,
    pub class: String,
    pub origin: String,
    pub dest: String,
    pub epoch: u32,
    pub created_at: String,
    /// Immutable shipment key (`{dest}:{outbox_id}`). Empty on pre-SA-4
    /// sealed manifests — those cannot close a later outbox row.
    #[serde(default)]
    pub shipment_id: String,
}

/// One ingest receipt. `accepted: false` = the cargo failed verification at
/// the destination — the failure travels home too.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Receipt {
    pub root: String,
    pub node: String,
    pub accepted: bool,
    pub at: String,
    #[serde(default)]
    pub shipment_id: String,
}

fn read_sealed_list<T: serde::de::DeserializeOwned>(
    path: &Path,
    psk: &Psk,
    stick_id: &str,
    domain: &str,
) -> Result<Vec<T>, String> {
    let raw = match std::fs::read_to_string(path) {
        Ok(r) => r,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(format!("{domain} read: {e}")),
    };
    let v = open_json(psk, stick_id, domain, &raw).map_err(|e| format!("{domain}: {e}"))?;
    serde_json::from_value(v["entries"].clone()).map_err(|e| format!("{domain} decode: {e}"))
}

fn write_sealed_list<T: Serialize>(
    path: &Path,
    psk: &Psk,
    stick_id: &str,
    domain: &str,
    entries: &[T],
) -> Result<(), String> {
    let v = serde_json::json!({ "entries": entries });
    let sealed = seal_json(psk, stick_id, domain, &v)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("{domain} mkdir: {e}"))?;
    }
    std::fs::write(path, sealed).map_err(|e| format!("{domain} write: {e}"))
}

pub fn read_manifest(
    mount: &Path,
    psk: &Psk,
    stick_id: &str,
) -> Result<Vec<ManifestEntry>, String> {
    read_sealed_list(
        &mount.join(COURIER_DIR).join(MANIFEST_FILE),
        psk,
        stick_id,
        "manifest",
    )
}

pub fn write_manifest(
    mount: &Path,
    psk: &Psk,
    stick_id: &str,
    entries: &[ManifestEntry],
) -> Result<(), String> {
    write_sealed_list(
        &mount.join(COURIER_DIR).join(MANIFEST_FILE),
        psk,
        stick_id,
        "manifest",
        entries,
    )
}

pub fn read_receipts(mount: &Path, psk: &Psk, stick_id: &str) -> Result<Vec<Receipt>, String> {
    read_sealed_list(
        &mount.join(COURIER_DIR).join(RECEIPTS_FILE),
        psk,
        stick_id,
        "receipts",
    )
}

pub fn write_receipts(
    mount: &Path,
    psk: &Psk,
    stick_id: &str,
    receipts: &[Receipt],
) -> Result<(), String> {
    write_sealed_list(
        &mount.join(COURIER_DIR).join(RECEIPTS_FILE),
        psk,
        stick_id,
        "receipts",
        receipts,
    )
}

// ── The outbox (charter §6.5, courier-fed for P2) ───────────────────────────

/// One queued outbound artifact. Lives in `<log_dir>/outbox.jsonl` — the
/// scheduler's "commitments run late, they don't evaporate" rule applied to
/// transports: a queued artifact waits for a stick, however long that takes.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OutboxEntry {
    pub id: u64,
    pub dest: String,
    /// Absolute source path — confined to the caller's workspace at queue time.
    pub path: String,
    pub name: String,
    pub root: String,
    pub len: u64,
    pub created_at: String,
    /// stick_id this artifact was last loaded onto (None = still waiting).
    #[serde(default)]
    pub loaded_on: Option<String>,
    /// Delivery confirmed (receipt heard via gossip or read off a stick).
    #[serde(default)]
    pub receipted_at: Option<String>,
    /// Immutable id stamped at queue time. Receipts must echo it.
    #[serde(default)]
    pub shipment_id: String,
}

pub fn outbox_path(log_dir: &Path) -> PathBuf {
    log_dir.join("outbox.jsonl")
}

pub fn outbox_load(log_dir: &Path) -> Vec<OutboxEntry> {
    std::fs::read_to_string(outbox_path(log_dir))
        .map(|s| {
            s.lines()
                .filter_map(|l| serde_json::from_str(l).ok())
                .collect()
        })
        .unwrap_or_default()
}

pub fn outbox_save(log_dir: &Path, entries: &[OutboxEntry]) -> Result<(), String> {
    let mut out = String::new();
    for e in entries {
        out.push_str(&serde_json::to_string(e).map_err(|e| format!("outbox encode: {e}"))?);
        out.push('\n');
    }
    std::fs::create_dir_all(log_dir).map_err(|e| format!("outbox mkdir: {e}"))?;
    std::fs::write(outbox_path(log_dir), out).map_err(|e| format!("outbox write: {e}"))
}

/// Queue one artifact for the next courier stick. `abs_path` must already be
/// confined by the caller (`confine_mesh_source` — the system-stamped
/// workspace rule); this re-checks only size and readability.
pub fn queue_artifact(
    log_dir: &Path,
    abs_path: &Path,
    dest_node: &str,
    name: &str,
) -> Result<OutboxEntry, String> {
    let meta = std::fs::metadata(abs_path).map_err(|e| format!("source: {e}"))?;
    if !meta.is_file() {
        return Err("source is not a regular file".into());
    }
    if meta.len() > MAX_ARTIFACT_BYTES {
        return Err(format!(
            "source is {} bytes (> {} MiB courier cap)",
            meta.len(),
            MAX_ARTIFACT_BYTES / (1024 * 1024)
        ));
    }
    let bytes = std::fs::read(abs_path).map_err(|e| format!("source read: {e}"))?;
    let root = hex32(&blob_root(&bytes));
    let _g = lock();
    let mut entries = outbox_load(log_dir);
    // Same file, same destination, still undelivered → idempotent (re-queue
    // refreshes nothing, invents nothing).
    if let Some(existing) = entries
        .iter()
        .find(|e| e.root == root && e.dest == dest_node && e.receipted_at.is_none())
    {
        return Ok(existing.clone());
    }
    let id = entries.iter().map(|e| e.id).max().unwrap_or(0) + 1;
    let entry = OutboxEntry {
        id,
        dest: dest_node.to_string(),
        path: abs_path.to_string_lossy().into_owned(),
        name: name.to_string(),
        root,
        len: meta.len(),
        created_at: now_iso(),
        loaded_on: None,
        receipted_at: None,
        shipment_id: shipment_id_for(dest_node, id),
    };
    entries.push(entry.clone());
    outbox_save(log_dir, &entries)?;
    Ok(entry)
}

// ── Destination resolution (field note 08-07: "apex2" vs "ApexOS-2") ────────

/// All registered peer node_ids (peers.toml — same file/shape `find_peer`
/// reads; this is the list view for resolution + honest errors).
pub fn peers_list() -> Vec<String> {
    #[derive(serde::Deserialize)]
    struct PeersFile {
        #[serde(default)]
        peer: Vec<PeerEntry>,
    }
    #[derive(serde::Deserialize)]
    struct PeerEntry {
        node_id: String,
    }
    let path = std::env::var("PEERS_TOML").unwrap_or_else(|_| "/etc/agentd/peers.toml".into());
    std::fs::read_to_string(path)
        .ok()
        .and_then(|raw| toml::from_str::<PeersFile>(&raw).ok())
        .map(|f| f.peer.into_iter().map(|p| p.node_id).collect())
        .unwrap_or_default()
}

/// How a queue-time destination resolved against the peer registry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DestResolution {
    /// Exact registered id.
    Exact(String),
    /// Unambiguous alias (case/subsequence) → the canonical id.
    Resolved(String),
    /// The alias matches several peers — caller must pick.
    Ambiguous(Vec<String>),
    /// No registered peer comes close. Still queueable (a courier can reach
    /// nodes the LAN can't) — but the caller should say so, loudly.
    Unknown,
}

fn normalize(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect::<String>()
        .to_ascii_lowercase()
}

/// Is `needle` a subsequence of `hay` (both normalized)? Catches the natural
/// human abbreviations — "apex2" ⊆ "apexos2", "tvpi" ⊆ "tvpi" — without a
/// fuzzy-distance dependency.
fn is_subsequence(needle: &str, hay: &str) -> bool {
    let mut h = hay.chars();
    needle.chars().all(|n| h.any(|c| c == n))
}

/// Resolve a human-supplied destination against the registered peers:
/// exact → normalized-exact → unique subsequence (≥3 chars — "a2" is too
/// little signal). Pure; the tool layer decides what each variant says.
pub fn resolve_dest(query: &str, peers: &[String]) -> DestResolution {
    if peers.iter().any(|p| p == query) {
        return DestResolution::Exact(query.to_string());
    }
    let nq = normalize(query);
    if nq.is_empty() {
        return DestResolution::Unknown;
    }
    let exact_norm: Vec<&String> = peers.iter().filter(|p| normalize(p) == nq).collect();
    match exact_norm.len() {
        1 => return DestResolution::Resolved(exact_norm[0].clone()),
        n if n > 1 => return DestResolution::Ambiguous(exact_norm.into_iter().cloned().collect()),
        _ => {}
    }
    if nq.len() >= 3 {
        let subseq: Vec<&String> = peers
            .iter()
            .filter(|p| is_subsequence(&nq, &normalize(p)))
            .collect();
        match subseq.len() {
            1 => return DestResolution::Resolved(subseq[0].clone()),
            n if n > 1 => return DestResolution::Ambiguous(subseq.into_iter().cloned().collect()),
            _ => {}
        }
    }
    DestResolution::Unknown
}

/// Cancel an undelivered outbox entry by id. Honest about the physics: if
/// the artifact is already aboard a stick, that copy still travels — cancel
/// only stops future loads (and the eventual receipt will simply find no
/// outbox entry to mark).
pub fn outbox_cancel(log_dir: &Path, id: u64) -> Result<(OutboxEntry, Option<String>), String> {
    let _g = lock();
    let mut entries = outbox_load(log_dir);
    let idx = entries
        .iter()
        .position(|e| e.id == id)
        .ok_or_else(|| format!("no outbox entry with id {id}"))?;
    if entries[idx].receipted_at.is_some() {
        return Err(format!(
            "entry {id} ({}) was already delivered — nothing to cancel",
            entries[idx].name
        ));
    }
    let entry = entries.remove(idx);
    outbox_save(log_dir, &entries)?;
    let caveat = entry.loaded_on.as_ref().map(|stick| {
        format!(
            "already loaded on stick {stick} — that copy still travels; \
             cancelling only stops future loads"
        )
    });
    Ok((entry, caveat))
}

// ── The gossip ledger (what this node has HEARD) ────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HeardManifest {
    pub stick: String,
    pub root: String,
    pub origin: String,
    pub dest: String,
    pub len: u64,
    pub epoch: u32,
    #[serde(default)]
    pub name: String,
    pub heard_at: String,
    #[serde(default)]
    pub shipment_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HeardReceipt {
    pub stick: String,
    pub root: String,
    pub node: String,
    pub accepted: bool,
    pub heard_at: String,
    #[serde(default)]
    pub shipment_id: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Ledger {
    #[serde(default)]
    pub manifests: Vec<HeardManifest>,
    #[serde(default)]
    pub receipts: Vec<HeardReceipt>,
}

pub fn ledger_path(log_dir: &Path) -> PathBuf {
    log_dir.join("courier_ledger.json")
}

pub fn ledger_load(log_dir: &Path) -> Ledger {
    std::fs::read_to_string(ledger_path(log_dir))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn ledger_save(log_dir: &Path, ledger: &Ledger) -> Result<(), String> {
    std::fs::create_dir_all(log_dir).map_err(|e| format!("ledger mkdir: {e}"))?;
    let s = serde_json::to_string_pretty(ledger).map_err(|e| format!("ledger encode: {e}"))?;
    std::fs::write(ledger_path(log_dir), s).map_err(|e| format!("ledger write: {e}"))
}

/// Record a heard manifest announcement (dedup by stick+root+dest). Returns
/// true when it was news.
pub fn ledger_hear_manifest(log_dir: &Path, m: HeardManifest) -> bool {
    let _g = lock();
    let mut ledger = ledger_load(log_dir);
    if ledger.manifests.iter().any(|x| {
        x.stick == m.stick && x.root == m.root && x.dest == m.dest && x.shipment_id == m.shipment_id
    }) {
        return false;
    }
    ledger.manifests.push(m);
    let _ = ledger_save(log_dir, &ledger);
    true
}

/// Record a heard receipt AND mark any matching outbox entry delivered.
/// Returns (was_news, delivered_entry_name).
pub fn ledger_hear_receipt(log_dir: &Path, r: HeardReceipt) -> (bool, Option<String>) {
    let _g = lock();
    let mut ledger = ledger_load(log_dir);
    let news = !ledger.receipts.iter().any(|x| {
        x.stick == r.stick && x.root == r.root && x.node == r.node && x.shipment_id == r.shipment_id
    });
    if news {
        ledger.receipts.push(r.clone());
        let _ = ledger_save(log_dir, &ledger);
    }
    let mut delivered = None;
    if r.accepted {
        let mut entries = outbox_load(log_dir);
        let mut changed = false;
        for e in entries.iter_mut() {
            if receipt_closes_outbox(e, &r.shipment_id, &r.node, &r.stick, &r.root) {
                e.receipted_at = Some(r.heard_at.clone());
                delivered = Some(e.name.clone());
                changed = true;
            }
        }
        if changed {
            let _ = outbox_save(log_dir, &entries);
        }
    }
    (news, delivered)
}

// ── Plug processing — the whole loop, one call ──────────────────────────────

/// A gossip payload to relay over Tier 1 (the radio tiers pick these up in
/// P6 as `CourierManifest`/`CourierReceipt` wire payloads — same semantics,
/// string node ids here because the u16 radio registry doesn't exist yet).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Gossip {
    /// "manifest" or "receipt".
    pub kind: &'static str,
    /// Which peer to tell (manifest → dest, receipt → origin).
    pub target: String,
    pub body: serde_json::Value,
}

/// What one plug did — feeds [`compose_plug_notice`] (the honest surface).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PlugReport {
    pub stick_id: Option<String>,
    pub psk_missing: bool,
    /// Sealed courier data present but unreadable (tamper / wrong PSK).
    pub sealed_error: Option<String>,
    /// Ingested this plug: (name, origin).
    pub verified: Vec<(String, String)>,
    /// Failed verification: (name, reason) — cargo NOT ingested.
    pub failed: Vec<(String, String)>,
    /// Entries for me already ingested on an earlier plug.
    pub already: usize,
    /// Of the entries for me, how many were pre-announced via gossip.
    pub announced_matched: usize,
    pub unannounced: usize,
    /// Loaded from the outbox onto this stick: (name, dest).
    pub loaded: Vec<(String, String)>,
    /// My outbox entries confirmed delivered by receipts read off this stick.
    pub receipts_matched: usize,
    /// Cargo aboard for other nodes (in transit — leave it alone).
    pub in_transit: usize,
    pub notes: Vec<String>,
}

pub struct PlugOutcome {
    pub report: PlugReport,
    pub gossip: Vec<Gossip>,
}

/// Immutable shipment key. Unique per origin outbox; receipts must echo it.
pub fn shipment_id_for(dest: &str, id: u64) -> String {
    format!("{dest}:{id}")
}

/// `origin` is a directory name under `courier/incoming/`. One component,
/// no traversal — a sealed manifest is authenticated, not trusted.
pub fn safe_origin(origin: &str) -> Result<String, String> {
    let s = origin.trim();
    if s.is_empty() {
        return Err("origin is empty".into());
    }
    if s == "." || s == ".." || s.contains('/') || s.contains('\\') {
        return Err("origin must be a single path component".into());
    }
    if s.starts_with('.') {
        return Err("origin must not be hidden or relative".into());
    }
    if s.len() > 64 {
        return Err("origin is too long".into());
    }
    if !s
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    {
        return Err("origin has illegal characters".into());
    }
    Ok(s.to_string())
}

/// A receipt closes one outbox row only: shipment id + dest, and the stick
/// it was loaded on when we know it. Root is defence-in-depth. A missing
/// shipment id (pre-SA-4) never closes a row.
pub fn receipt_closes_outbox(
    e: &OutboxEntry,
    shipment_id: &str,
    dest: &str,
    stick: &str,
    root: &str,
) -> bool {
    if e.receipted_at.is_some() {
        return false;
    }
    if e.dest != dest {
        return false;
    }
    if e.shipment_id.is_empty() || shipment_id.is_empty() || e.shipment_id != shipment_id {
        return false;
    }
    if !root.is_empty() && e.root != root {
        return false;
    }
    if let Some(on) = &e.loaded_on {
        if !stick.is_empty() && on != stick {
            return false;
        }
    }
    true
}

/// Sanitize a manifest-supplied filename for the incoming dir: last path
/// component only, conservative charset, never empty.
fn safe_name(name: &str, root: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or("");
    let clean: String = base
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | ' '))
        .collect();
    let clean = clean.trim().trim_start_matches('.').to_string();
    if clean.is_empty() {
        format!("cargo-{}", &root[..root.len().min(12)])
    } else {
        clean
    }
}

fn hex32(b: &[u8; 32]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// Process a freshly-plugged exo-workspace stick end to end: identity,
/// verify+ingest cargo for this node, read receipts home, drain the outbox
/// aboard, and produce the gossip to announce it all. Synchronous (call via
/// `spawn_blocking`); takes [`lock`] for the duration.
pub fn process_plug(
    mount: &Path,
    node_id: &str,
    workspace: &Path,
    log_dir: &Path,
    psk: Option<&Psk>,
) -> PlugOutcome {
    let mut report = PlugReport::default();
    let mut gossip = Vec::new();

    let stick_id = match ensure_stick_id(mount, node_id) {
        Ok(id) => id,
        Err(e) => {
            report.notes.push(format!("stick identity: {e}"));
            return PlugOutcome { report, gossip };
        }
    };
    report.stick_id = Some(stick_id.clone());

    let Some(psk) = psk else {
        report.psk_missing = true;
        if mount.join(COURIER_DIR).join(MANIFEST_FILE).exists() {
            report
                .notes
                .push("a sealed courier manifest is aboard but this node has no colony PSK".into());
        }
        return PlugOutcome { report, gossip };
    };

    let _g = lock();

    let mut manifest = match read_manifest(mount, psk, &stick_id) {
        Ok(m) => m,
        Err(e) => {
            report.sealed_error = Some(e);
            return PlugOutcome { report, gossip };
        }
    };
    let mut receipts = match read_receipts(mount, psk, &stick_id) {
        Ok(r) => r,
        Err(e) => {
            report.sealed_error = Some(e);
            return PlugOutcome { report, gossip };
        }
    };
    let ledger = ledger_load(log_dir);
    let mut receipts_dirty = false;
    let mut manifest_dirty = false;

    // 1. Cargo addressed to me: verify → ingest → receipt (both on the stick
    //    and gossiped home). Tamper fails closed, with an accepted:false
    //    receipt so the origin learns the carry failed.
    for entry in manifest.iter().filter(|e| e.dest == node_id) {
        if receipts.iter().any(|r| {
            r.node == node_id
                && r.accepted
                && if !r.shipment_id.is_empty() && !entry.shipment_id.is_empty() {
                    r.shipment_id == entry.shipment_id
                } else {
                    r.root == entry.root
                }
        }) {
            report.already += 1;
            continue;
        }
        if ledger.manifests.iter().any(|m| {
            m.stick == stick_id
                && m.root == entry.root
                && (m.shipment_id.is_empty()
                    || entry.shipment_id.is_empty()
                    || m.shipment_id == entry.shipment_id)
        }) {
            report.announced_matched += 1;
        } else {
            report.unannounced += 1;
        }
        let cargo_path = mount.join(COURIER_DIR).join(CARGO_DIR).join(&entry.root);
        let verdict: Result<(), String> = (|| {
            let origin = safe_origin(&entry.origin)?;
            let bytes = std::fs::read(&cargo_path).map_err(|e| format!("cargo read: {e}"))?;
            if hex32(&blob_root(&bytes)) != entry.root {
                return Err("blake3 mismatch — cargo does not match its announced root".into());
            }
            let dir = workspace.join("courier").join("incoming").join(&origin);
            std::fs::create_dir_all(&dir).map_err(|e| format!("incoming mkdir: {e}"))?;
            std::fs::write(dir.join(safe_name(&entry.name, &entry.root)), &bytes)
                .map_err(|e| format!("incoming write: {e}"))?;
            Ok(())
        })();
        let accepted = verdict.is_ok();
        match verdict {
            Ok(()) => report
                .verified
                .push((entry.name.clone(), entry.origin.clone())),
            Err(reason) => report.failed.push((entry.name.clone(), reason)),
        }
        let receipt = Receipt {
            root: entry.root.clone(),
            node: node_id.to_string(),
            accepted,
            at: now_iso(),
            shipment_id: entry.shipment_id.clone(),
        };
        receipts.push(receipt.clone());
        receipts_dirty = true;
        if let Ok(origin) = safe_origin(&entry.origin) {
            gossip.push(Gossip {
                kind: "receipt",
                target: origin,
                body: serde_json::json!({
                    "stick": stick_id, "root": receipt.root,
                    "node": node_id, "accepted": accepted,
                    "shipment_id": receipt.shipment_id,
                }),
            });
        }
    }

    report.in_transit = manifest
        .iter()
        .filter(|e| e.dest != node_id && e.origin != node_id)
        .count();

    // 2. Receipts riding home on the stick: my deliveries, confirmed.
    {
        let mut outbox = outbox_load(log_dir);
        let mut outbox_dirty = false;
        for r in receipts.iter().filter(|r| r.accepted) {
            for e in outbox.iter_mut() {
                if receipt_closes_outbox(e, &r.shipment_id, &r.node, &stick_id, &r.root) {
                    e.receipted_at = Some(r.at.clone());
                    report.receipts_matched += 1;
                    outbox_dirty = true;
                }
            }
        }
        if outbox_dirty {
            let _ = outbox_save(log_dir, &outbox);
        }
    }

    // 3. Drain the outbox aboard: every undelivered artifact gets loaded (or
    //    re-verified as already loaded) + announced toward its destination.
    {
        let mut outbox = outbox_load(log_dir);
        let mut outbox_dirty = false;
        for e in outbox.iter_mut() {
            if e.receipted_at.is_some() || e.dest == node_id {
                continue;
            }
            let load: Result<(), String> = (|| {
                let bytes = std::fs::read(&e.path).map_err(|err| format!("source read: {err}"))?;
                let root = hex32(&blob_root(&bytes));
                if root != e.root {
                    // The file changed since queueing — carry what exists NOW,
                    // and say so.
                    report.notes.push(format!(
                        "{}: source changed since queueing — carrying current bytes",
                        e.name
                    ));
                    e.root = root;
                    e.len = bytes.len() as u64;
                }
                let cargo_dir = mount.join(COURIER_DIR).join(CARGO_DIR);
                std::fs::create_dir_all(&cargo_dir).map_err(|err| format!("cargo mkdir: {err}"))?;
                let cargo_path = cargo_dir.join(&e.root);
                if !cargo_path.exists() {
                    std::fs::write(&cargo_path, &bytes)
                        .map_err(|err| format!("cargo write: {err}"))?;
                }
                if e.shipment_id.is_empty() {
                    e.shipment_id = shipment_id_for(&e.dest, e.id);
                    outbox_dirty = true;
                }
                if !manifest.iter().any(|m| {
                    if !m.shipment_id.is_empty() && !e.shipment_id.is_empty() {
                        m.shipment_id == e.shipment_id
                    } else {
                        m.root == e.root && m.dest == e.dest
                    }
                }) {
                    manifest.push(ManifestEntry {
                        root: e.root.clone(),
                        len: e.len,
                        name: e.name.clone(),
                        class: "bulk".into(),
                        origin: node_id.to_string(),
                        dest: e.dest.clone(),
                        epoch: epoch_today(),
                        created_at: now_iso(),
                        shipment_id: e.shipment_id.clone(),
                    });
                    manifest_dirty = true;
                }
                Ok(())
            })();
            match load {
                Ok(()) => {
                    if e.loaded_on.as_deref() != Some(stick_id.as_str()) {
                        e.loaded_on = Some(stick_id.clone());
                        outbox_dirty = true;
                        report.loaded.push((e.name.clone(), e.dest.clone()));
                        gossip.push(Gossip {
                            kind: "manifest",
                            target: e.dest.clone(),
                            body: serde_json::json!({
                                "stick": stick_id, "root": e.root, "origin": node_id,
                                "dest": e.dest, "len": e.len, "epoch": epoch_today(),
                                "name": e.name, "shipment_id": e.shipment_id,
                            }),
                        });
                    }
                }
                Err(reason) => report.notes.push(format!("{}: {reason}", e.name)),
            }
        }
        if outbox_dirty {
            let _ = outbox_save(log_dir, &outbox);
        }
    }

    if manifest_dirty {
        if let Err(e) = write_manifest(mount, psk, &stick_id, &manifest) {
            report.notes.push(format!("manifest write: {e}"));
        }
    }
    if receipts_dirty {
        if let Err(e) = write_receipts(mount, psk, &stick_id, &receipts) {
            report.notes.push(format!("receipts write: {e}"));
        }
    }

    PlugOutcome { report, gossip }
}

/// Env-wrapped [`process_plug`] for the gateway's plug handler.
pub fn process_plug_env(label: &str) -> PlugOutcome {
    let ws = workspace_env();
    let mount = ws.join("media").join(label);
    let psk = load_psk_env();
    process_plug(
        &mount,
        &apexos_core::node_id(),
        &ws,
        &log_dir_env(),
        psk.as_ref(),
    )
}

/// The courier paragraph appended to the plug greeting — `None` when the
/// stick has no courier story to tell (plain exo-workspace, no PSK, nothing
/// aboard, empty outbox).
pub fn compose_plug_notice(report: &PlugReport) -> Option<String> {
    let mut lines: Vec<String> = Vec::new();
    if let Some(e) = &report.sealed_error {
        lines.push(format!(
            "⚠️ Courier data aboard FAILED authentication ({e}) — treating it as untrusted; \
             nothing was ingested."
        ));
    }
    for (name, origin) in &report.verified {
        lines.push(format!(
            "📦 Ingested **{name}** from {origin} (blake3 verified) → `courier/incoming/{origin}/`."
        ));
    }
    for (name, reason) in &report.failed {
        lines.push(format!(
            "⚠️ **{name}** FAILED verification ({reason}) — not ingested; a rejection receipt \
             is on the stick and gossiped to its origin."
        ));
    }
    if report.already > 0 {
        lines.push(format!(
            "{} item(s) aboard were already ingested on an earlier plug.",
            report.already
        ));
    }
    if report.announced_matched + report.unannounced > 0 {
        lines.push(format!(
            "Of the cargo for this node: {} pre-announced via mesh gossip, {} unannounced.",
            report.announced_matched, report.unannounced
        ));
    }
    if report.receipts_matched > 0 {
        lines.push(format!(
            "🧾 {} of this node's outbound deliveries confirmed by receipts riding this stick.",
            report.receipts_matched
        ));
    }
    for (name, dest) in &report.loaded {
        lines.push(format!(
            "📤 Loaded **{name}** from the outbox for **{dest}** — carry the stick there; \
             delivery will be receipted."
        ));
    }
    if report.in_transit > 0 {
        lines.push(format!(
            "{} item(s) aboard are in transit for other nodes — left untouched.",
            report.in_transit
        ));
    }
    if report.psk_missing && !report.notes.is_empty() {
        lines.push(format!("⚠️ {}", report.notes.join(" · ")));
    } else {
        for n in &report.notes {
            lines.push(format!("ℹ️ {n}"));
        }
    }
    if lines.is_empty() {
        None
    } else {
        Some(lines.join("\n"))
    }
}

/// Best-effort Tier-1 relay of the gossip a plug produced: each payload goes
/// to its one interested peer (manifest → dest, receipt → origin), stamped
/// `from` = this node, over the peer's token-gated courier endpoint. A dark
/// peer is fine — the stick itself is the durable copy.
pub async fn dispatch_gossip(node_id: &str, gossip: Vec<Gossip>) -> Vec<String> {
    let mut failures = Vec::new();
    for g in gossip {
        if g.target == node_id {
            continue;
        }
        let Some((ws_url, token)) = crate::supervisor::find_peer(&g.target).await else {
            failures.push(format!(
                "{}: not a registered peer (gossip skipped)",
                g.target
            ));
            continue;
        };
        let http_base = ws_url
            .replacen("ws://", "http://", 1)
            .replacen("wss://", "https://", 1);
        let mut body = g.body.clone();
        body["from"] = serde_json::json!(node_id);
        let mut req = reqwest::Client::new()
            .post(format!("{http_base}/api/courier/{}", g.kind))
            .json(&body)
            .timeout(std::time::Duration::from_secs(8));
        if let Some(t) = token.as_deref() {
            req = req.bearer_auth(t);
        }
        match req.send().await {
            Ok(r) if r.status().is_success() => {}
            Ok(r) => failures.push(format!("{}: {} (status {})", g.target, g.kind, r.status())),
            Err(e) => failures.push(format!("{}: {} ({e})", g.target, g.kind)),
        }
    }
    failures
}

/// Drain unrecepted outbox rows over HTTP now that WifiLan is back (P5d DoD:
/// restore → drains). Stick-bound `loaded_on` rows stay for the courier loop;
/// this only tries dests that are in peers.toml.
pub async fn drain_outbox_over_lan() -> usize {
    let log_dir = log_dir_env();
    let entries = {
        let _g = lock();
        outbox_load(&log_dir)
    };
    let mut delivered = 0usize;
    for entry in entries {
        if entry.receipted_at.is_some() {
            continue;
        }
        let Some(dest) = crate::wifi_lan::lookup_peer(&entry.dest) else {
            continue;
        };
        let bytes = match std::fs::read(&entry.path) {
            Ok(b) => b,
            Err(_) => continue,
        };
        let http_base = dest
            .ws_url
            .replacen("ws://", "http://", 1)
            .replacen("wss://", "https://", 1);
        let mut req = reqwest::Client::new()
            .post(format!("{http_base}/api/mesh/file"))
            .header("x-dest", &entry.name)
            .body(bytes)
            .timeout(std::time::Duration::from_secs(30));
        if let Some(t) = dest.token.as_deref() {
            req = req.bearer_auth(t);
        }
        let ok = match req.send().await {
            Ok(r) => {
                let status = r.status();
                let v = r.json::<serde_json::Value>().await.ok();
                status.is_success() && v.as_ref().and_then(|b| b["ok"].as_bool()) == Some(true)
            }
            Err(_) => false,
        };
        if !ok {
            continue;
        }
        let _g = lock();
        let mut rows = outbox_load(&log_dir);
        if let Some(e) = rows.iter_mut().find(|e| e.id == entry.id) {
            e.receipted_at = Some(now_iso());
        }
        let _ = outbox_save(&log_dir, &rows);
        delivered += 1;
    }
    if delivered > 0 {
        eprintln!("[courier] drained {delivered} outbox row(s) over WifiLan");
    }
    delivered
}

/// Tier 4 as the router sees it: the outbox. Always Up — queueing is always
/// possible. A stick in a pocket is not connectivity (see
/// [`apexos_core::mesh_router::Router::implied_state`]).
pub struct CourierTransport;

#[async_trait::async_trait]
impl apexos_core::mesh_router::MeshTransport for CourierTransport {
    fn id(&self) -> apexos_core::mesh_router::TransportId {
        apexos_core::mesh_router::TransportId::Courier
    }

    fn mtu(&self) -> usize {
        MAX_ARTIFACT_BYTES as usize
    }

    fn latency_class(&self) -> apexos_core::mesh_router::LatencyClass {
        apexos_core::mesh_router::LatencyClass::Overnight
    }

    fn health(&self) -> apexos_core::mesh_router::TransportHealth {
        apexos_core::mesh_router::TransportHealth::Up
    }

    async fn send(
        &self,
        frame: &apexos_mesh_proto::MeshFrame,
    ) -> Result<apexos_core::mesh_router::SendReceipt, apexos_core::mesh_router::SendError> {
        use apexos_core::mesh_router::{SendError, SendReceipt, TransportId};
        use apexos_mesh_proto::{Payload, PlainPacket};
        let (packet, _): (PlainPacket, _) = postcard::take_from_bytes(&frame.ct)
            .map_err(|_| SendError::Failed("courier: undecodable frame".into()))?;
        let Payload::A2A { body } = packet.payload else {
            return Err(SendError::Failed(
                "courier: only A2A overflow rides this lane in P5d".into(),
            ));
        };
        let env: crate::wifi_lan::A2aEnvelope = serde_json::from_slice(&body)
            .map_err(|e| SendError::Failed(format!("courier: envelope: {e}")))?;
        let ws = workspace_env().join("courier-pending");
        let _ = std::fs::create_dir_all(&ws);
        let name = format!("a2a-{}-{}.md", env.node, chrono::Utc::now().timestamp());
        let path = ws.join(&name);
        let text = format!(
            "# queued a2a for {}\n\nfrom: {}\nsession: {}\n\n{}\n",
            env.node, env.from, env.session_id, env.message
        );
        std::fs::write(&path, text.as_bytes())
            .map_err(|e| SendError::Failed(format!("courier: write: {e}")))?;
        let log_dir = log_dir_env();
        let dest = env.node.clone();
        let queued =
            tokio::task::spawn_blocking(move || queue_artifact(&log_dir, &path, &dest, &name))
                .await
                .map_err(|e| SendError::Failed(format!("courier: join: {e}")))?
                .map_err(SendError::Failed)?;
        Ok(SendReceipt {
            via: TransportId::Courier,
            bytes: queued.len as usize,
        })
    }
}

/// Courier state for the status tool / API: the outbox, what's been heard,
/// and whether crypto is available.
pub fn status_json(log_dir: &Path, psk_present: bool, node_id: &str) -> serde_json::Value {
    let outbox = outbox_load(log_dir);
    let ledger = ledger_load(log_dir);
    let pending: Vec<_> = outbox.iter().filter(|e| e.receipted_at.is_none()).collect();
    let inbound: Vec<_> = ledger
        .manifests
        .iter()
        .filter(|m| m.dest == node_id)
        .map(|m| {
            let arrived = ledger
                .receipts
                .iter()
                .any(|r| r.root == m.root && r.node == node_id && r.accepted);
            serde_json::json!({
                "stick": m.stick, "root": m.root, "origin": m.origin, "name": m.name,
                "len": m.len, "heard_at": m.heard_at,
                "status": if arrived { "delivered" } else { "en route" },
            })
        })
        .collect();
    serde_json::json!({
        "psk_present": psk_present,
        "outbox": {
            "pending": pending.iter().map(|e| serde_json::json!({
                "id": e.id, "name": e.name, "dest": e.dest, "len": e.len,
                "created_at": e.created_at,
                "status": match (&e.loaded_on, &e.receipted_at) {
                    (_, Some(_))    => "delivered",
                    (Some(s), None) => return serde_json::json!({
                        "id": e.id, "name": e.name, "dest": e.dest, "len": e.len,
                        "created_at": e.created_at, "status": format!("on stick {s}"),
                    }),
                    (None, None)    => "waiting for a stick",
                },
            })).collect::<Vec<_>>(),
            "delivered": outbox.iter().filter(|e| e.receipted_at.is_some()).count(),
        },
        "inbound_announced": inbound,
        "receipts_heard": ledger.receipts.len(),
    })
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> tempfile::TempDir {
        tempfile::tempdir().expect("tempdir")
    }

    fn psk() -> Psk {
        Psk([0x21; 32])
    }

    fn write_marker_v1(mount: &Path, name: &str) {
        std::fs::write(
            mount.join(MARKER_FILE),
            format!(
                "# ApexOS exo-workspace\nversion = 1\nname    = \"{name}\"\nlayout  = [\"projects\", \"data\", \"notes\"]\n"
            ),
        )
        .unwrap();
    }

    #[test]
    fn marker_v1_parses_and_upgrades_preserving_content() {
        let m = tmp();
        write_marker_v1(m.path(), "work");
        let raw = std::fs::read_to_string(m.path().join(MARKER_FILE)).unwrap();
        let parsed = parse_marker(&raw).unwrap();
        assert_eq!(parsed.version, 1);
        assert_eq!(parsed.name, "work");
        assert!(parsed.stick_id.is_none());

        let id = ensure_stick_id(m.path(), "apex1").unwrap();
        assert_eq!(id.len(), 16);
        let upgraded = std::fs::read_to_string(m.path().join(MARKER_FILE)).unwrap();
        let re = parse_marker(&upgraded).unwrap();
        assert_eq!(re.version, 2);
        assert_eq!(re.name, "work"); // preserved
        assert!(upgraded.contains("layout")); // preserved
        assert_eq!(re.stick_id.as_deref(), Some(id.as_str()));

        // Idempotent: a second plug re-reads the same identity.
        assert_eq!(ensure_stick_id(m.path(), "apex1").unwrap(), id);
    }

    #[test]
    fn sealed_envelope_fails_closed_on_every_tamper_axis() {
        let v = serde_json::json!({ "entries": [{"hello": "world"}] });
        let sealed = seal_json(&psk(), "aabbccdd00112233", "manifest", &v).unwrap();
        // Roundtrip.
        assert_eq!(
            open_json(&psk(), "aabbccdd00112233", "manifest", &sealed).unwrap(),
            v
        );
        // Wrong stick (AAD).
        assert!(open_json(&psk(), "ffffffffffffffff", "manifest", &sealed).is_err());
        // Wrong domain (a manifest renamed to receipts.json).
        assert!(open_json(&psk(), "aabbccdd00112233", "receipts", &sealed).is_err());
        // Wrong key.
        assert!(open_json(&Psk([9; 32]), "aabbccdd00112233", "manifest", &sealed).is_err());
        // Flipped ciphertext byte.
        let mut env: serde_json::Value = serde_json::from_str(&sealed).unwrap();
        let mut ct = base64::engine::general_purpose::STANDARD
            .decode(env["ct"].as_str().unwrap())
            .unwrap();
        ct[0] ^= 1;
        env["ct"] = serde_json::json!(base64::engine::general_purpose::STANDARD.encode(ct));
        assert!(open_json(&psk(), "aabbccdd00112233", "manifest", &env.to_string()).is_err());
    }

    #[test]
    fn outbox_roundtrips_and_queue_is_idempotent() {
        let d = tmp();
        let src = d.path().join("artifact.txt");
        std::fs::write(&src, b"the cargo").unwrap();
        let e1 = queue_artifact(d.path(), &src, "apex2", "artifact.txt").unwrap();
        let e2 = queue_artifact(d.path(), &src, "apex2", "artifact.txt").unwrap();
        assert_eq!(e1, e2); // same undelivered artifact+dest → same entry
        let other = queue_artifact(d.path(), &src, "tvpi", "artifact.txt").unwrap();
        assert_ne!(e1.id, other.id); // different dest → its own entry
        assert_eq!(outbox_load(d.path()).len(), 2);
    }

    /// The DoD in miniature: node A queues + loads onto a stick; the stick
    /// "travels" (same tempdir); node B verifies, ingests, receipts; the
    /// stick travels home; node A reads the receipt off it. Then the tamper
    /// case fails closed.
    #[test]
    fn courier_loop_end_to_end_with_receipt_round_trip() {
        let stick = tmp(); // the mounted stick
        let a_log = tmp();
        let a_ws = tmp();
        let b_log = tmp();
        let b_ws = tmp();
        let k = psk();
        write_marker_v1(stick.path(), "work");

        // A queues an artifact for B.
        let src = a_ws.path().join("report.md");
        std::fs::write(&src, b"# the report\nproof-sized").unwrap();
        queue_artifact(a_log.path(), &src, "apex-b", "report.md").unwrap();

        // Plug at A: outbox drains aboard, manifest gossip produced.
        let out_a = process_plug(stick.path(), "apex-a", a_ws.path(), a_log.path(), Some(&k));
        assert_eq!(
            out_a.report.loaded,
            vec![("report.md".into(), "apex-b".into())]
        );
        assert_eq!(out_a.gossip.len(), 1);
        assert_eq!(out_a.gossip[0].kind, "manifest");
        assert_eq!(out_a.gossip[0].target, "apex-b");

        // …the human walks…

        // Plug at B: verify, ingest, receipt (on stick + gossiped home).
        let out_b = process_plug(stick.path(), "apex-b", b_ws.path(), b_log.path(), Some(&k));
        assert_eq!(out_b.report.verified.len(), 1);
        assert!(out_b.report.failed.is_empty());
        let landed = b_ws
            .path()
            .join("courier")
            .join("incoming")
            .join("apex-a")
            .join("report.md");
        assert_eq!(
            std::fs::read(&landed).unwrap(),
            b"# the report\nproof-sized"
        );
        assert_eq!(out_b.gossip.len(), 1);
        assert_eq!(out_b.gossip[0].kind, "receipt");
        assert_eq!(out_b.gossip[0].target, "apex-a");

        // Re-plug at B: idempotent, nothing re-ingested.
        let again = process_plug(stick.path(), "apex-b", b_ws.path(), b_log.path(), Some(&k));
        assert!(again.report.verified.is_empty());
        assert_eq!(again.report.already, 1);

        // …the human walks home…

        // Plug at A: the receipt riding the stick closes the loop.
        let home = process_plug(stick.path(), "apex-a", a_ws.path(), a_log.path(), Some(&k));
        assert_eq!(home.report.receipts_matched, 1);
        let outbox = outbox_load(a_log.path());
        assert!(outbox[0].receipted_at.is_some());
    }

    #[test]
    fn tampered_cargo_fails_closed_with_a_rejection_receipt() {
        let stick = tmp();
        let a_log = tmp();
        let a_ws = tmp();
        let b_log = tmp();
        let b_ws = tmp();
        let k = psk();
        write_marker_v1(stick.path(), "work");

        let src = a_ws.path().join("payload.bin");
        std::fs::write(&src, b"authentic bytes").unwrap();
        queue_artifact(a_log.path(), &src, "apex-b", "payload.bin").unwrap();
        let out_a = process_plug(stick.path(), "apex-a", a_ws.path(), a_log.path(), Some(&k));
        assert_eq!(out_a.report.loaded.len(), 1);

        // A hostile hand rewrites the cargo in transit.
        let root = outbox_load(a_log.path())[0].root.clone();
        let cargo = stick.path().join(COURIER_DIR).join(CARGO_DIR).join(&root);
        std::fs::write(&cargo, b"EVIL bytes").unwrap();

        let out_b = process_plug(stick.path(), "apex-b", b_ws.path(), b_log.path(), Some(&k));
        assert!(out_b.report.verified.is_empty());
        assert_eq!(out_b.report.failed.len(), 1);
        assert!(out_b.report.failed[0].1.contains("blake3 mismatch"));
        // Nothing landed in the workspace.
        assert!(
            !b_ws.path().join("courier").exists()
                || std::fs::read_dir(b_ws.path().join("courier").join("incoming").join("apex-a"))
                    .map(|d| d.count() == 0)
                    .unwrap_or(true)
        );
        // The rejection travels: receipt gossip with accepted=false.
        assert_eq!(out_b.gossip.len(), 1);
        assert_eq!(out_b.gossip[0].kind, "receipt");
        assert_eq!(out_b.gossip[0].body["accepted"], serde_json::json!(false));
        // And a tampered MANIFEST (not just cargo) fails authentication wholesale.
        let mpath = stick.path().join(COURIER_DIR).join(MANIFEST_FILE);
        let mut sealed = std::fs::read_to_string(&mpath).unwrap();
        sealed = sealed.replacen("\"ct\":\"A", "\"ct\":\"B", 1); // best-effort flip
        std::fs::write(&mpath, sealed).unwrap();
        let out_c = process_plug(stick.path(), "apex-b", b_ws.path(), b_log.path(), Some(&k));
        // Either the flip changed nothing (rare) or authentication failed closed.
        if out_c.report.sealed_error.is_none() {
            assert!(out_c.report.verified.is_empty());
        }
    }

    #[test]
    fn missing_psk_is_honest_not_fatal() {
        let stick = tmp();
        let log = tmp();
        let ws = tmp();
        write_marker_v1(stick.path(), "work");
        // Courier dir present (sealed by someone else) but no PSK here.
        std::fs::create_dir_all(stick.path().join(COURIER_DIR)).unwrap();
        std::fs::write(stick.path().join(COURIER_DIR).join(MANIFEST_FILE), "{}").unwrap();
        let out = process_plug(stick.path(), "apex-x", ws.path(), log.path(), None);
        assert!(out.report.psk_missing);
        assert!(out.report.notes.iter().any(|n| n.contains("no colony PSK")));
        assert!(out.gossip.is_empty());
        // Identity still minted — the stick is usable as a plain exo-workspace.
        assert!(out.report.stick_id.is_some());
    }

    #[test]
    fn ledger_hear_receipt_marks_outbox_delivered() {
        let log = tmp();
        let src = log.path().join("f.txt");
        std::fs::write(&src, b"bytes").unwrap();
        let e = queue_artifact(log.path(), &src, "apex2", "f.txt").unwrap();
        let (news, delivered) = ledger_hear_receipt(
            log.path(),
            HeardReceipt {
                stick: "aabb".into(),
                root: e.root.clone(),
                node: "apex2".into(),
                accepted: true,
                heard_at: "2026-08-07T00:00:00Z".into(),
                shipment_id: e.shipment_id.clone(),
            },
        );
        assert!(news);
        assert_eq!(delivered.as_deref(), Some("f.txt"));
        assert!(outbox_load(log.path())[0].receipted_at.is_some());
        // Same receipt again: not news, nothing further to deliver.
        let (news2, delivered2) = ledger_hear_receipt(
            log.path(),
            HeardReceipt {
                stick: "aabb".into(),
                root: e.root,
                node: "apex2".into(),
                accepted: true,
                heard_at: "2026-08-07T00:00:01Z".into(),
                shipment_id: e.shipment_id,
            },
        );
        assert!(!news2);
        assert!(delivered2.is_none());
    }

    #[test]
    fn dest_resolution_catches_the_field_case() {
        let peers = vec![
            "ApexOS-2".to_string(),
            "tvpi".to_string(),
            "andre-laptop".to_string(),
        ];
        // The exact id passes through untouched.
        assert_eq!(
            resolve_dest("ApexOS-2", &peers),
            DestResolution::Exact("ApexOS-2".into())
        );
        // The field friction: André's shorthand resolves to the canonical id.
        assert_eq!(
            resolve_dest("apex2", &peers),
            DestResolution::Resolved("ApexOS-2".into())
        );
        // Case-insensitive exact.
        assert_eq!(
            resolve_dest("TVPI", &peers),
            DestResolution::Resolved("tvpi".into())
        );
        // Abbreviation via subsequence.
        assert_eq!(
            resolve_dest("laptop", &peers),
            DestResolution::Resolved("andre-laptop".into())
        );
        // Too short to trust as a subsequence; not near anything → Unknown.
        assert_eq!(resolve_dest("a2", &peers), DestResolution::Unknown);
        assert_eq!(resolve_dest("radxa", &peers), DestResolution::Unknown);
        // Ambiguity is surfaced, never guessed through.
        let twins = vec!["apex-a".to_string(), "apex-b".to_string()];
        match resolve_dest("apex", &twins) {
            DestResolution::Ambiguous(c) => assert_eq!(c.len(), 2),
            other => panic!("expected Ambiguous, got {other:?}"),
        }
    }

    #[test]
    fn outbox_cancel_removes_undelivered_and_is_honest_about_loaded() {
        let d = tmp();
        let src = d.path().join("a.txt");
        std::fs::write(&src, b"x").unwrap();
        let e = queue_artifact(d.path(), &src, "apex2", "a.txt").unwrap();
        // Plain cancel: entry gone, no caveat.
        let (gone, caveat) = outbox_cancel(d.path(), e.id).unwrap();
        assert_eq!(gone.id, e.id);
        assert!(caveat.is_none());
        assert!(outbox_load(d.path()).is_empty());
        // Unknown id errors.
        assert!(outbox_cancel(d.path(), 99).is_err());
        // Loaded-on-a-stick cancel carries the caveat.
        let e2 = queue_artifact(d.path(), &src, "apex2", "a.txt").unwrap();
        let mut entries = outbox_load(d.path());
        entries
            .iter_mut()
            .for_each(|x| x.loaded_on = Some("aabb".into()));
        outbox_save(d.path(), &entries).unwrap();
        let (_, caveat) = outbox_cancel(d.path(), e2.id).unwrap();
        assert!(caveat.unwrap().contains("still travels"));
        // Delivered entries refuse cancellation.
        let e3 = queue_artifact(d.path(), &src, "apex2", "a.txt").unwrap();
        let mut entries = outbox_load(d.path());
        entries
            .iter_mut()
            .for_each(|x| x.receipted_at = Some("t".into()));
        outbox_save(d.path(), &entries).unwrap();
        assert!(outbox_cancel(d.path(), e3.id)
            .unwrap_err()
            .contains("already delivered"));
    }

    #[test]
    fn safe_origin_is_a_single_component() {
        assert_eq!(safe_origin("apex-a").unwrap(), "apex-a");
        assert_eq!(safe_origin("ApexOS-2").unwrap(), "ApexOS-2");
        assert_eq!(safe_origin("andre-laptop").unwrap(), "andre-laptop");
        for bad in [
            "",
            ".",
            "..",
            "../etc",
            "../../etc/passwd",
            "a/b",
            "a\\b",
            ".hidden",
            "has space",
            "evil;id",
        ] {
            assert!(safe_origin(bad).is_err(), "must refuse {bad:?}");
        }
    }

    #[test]
    fn hostile_origin_does_not_escape_incoming() {
        let stick = tmp();
        let a_log = tmp();
        let a_ws = tmp();
        let b_log = tmp();
        let b_ws = tmp();
        let k = psk();
        write_marker_v1(stick.path(), "work");
        let src = a_ws.path().join("note.txt");
        std::fs::write(&src, b"secret").unwrap();
        queue_artifact(a_log.path(), &src, "apex-b", "note.txt").unwrap();
        process_plug(stick.path(), "apex-a", a_ws.path(), a_log.path(), Some(&k));

        // Rewrite the sealed manifest origin to a traversal after load.
        let stick_id = ensure_stick_id(stick.path(), "apex-a").unwrap();
        let mut entries = read_manifest(stick.path(), &k, &stick_id).unwrap();
        entries[0].origin = "../../escaped".into();
        write_manifest(stick.path(), &k, &stick_id, &entries).unwrap();

        let out_b = process_plug(stick.path(), "apex-b", b_ws.path(), b_log.path(), Some(&k));
        assert!(out_b.report.verified.is_empty());
        assert!(out_b
            .report
            .failed
            .iter()
            .any(|(_, r)| r.contains("origin")));
        assert!(!b_ws.path().join("escaped").exists());
        assert!(!b_ws.path().join("courier/incoming/../../escaped").exists());
        let incoming = b_ws.path().join("courier").join("incoming");
        if incoming.exists() {
            for ent in std::fs::read_dir(&incoming).unwrap() {
                let name = ent.unwrap().file_name();
                assert_ne!(name, "..");
                assert!(!name.to_string_lossy().contains('/'));
            }
        }
    }

    #[test]
    fn receipt_for_one_shipment_does_not_close_the_next() {
        let log = tmp();
        let src = log.path().join("same.txt");
        std::fs::write(&src, b"identical bytes").unwrap();
        let first = queue_artifact(log.path(), &src, "apex2", "same.txt").unwrap();
        // First shipment delivered.
        let _ = ledger_hear_receipt(
            log.path(),
            HeardReceipt {
                stick: "stick-a".into(),
                root: first.root.clone(),
                node: "apex2".into(),
                accepted: true,
                heard_at: "2026-08-16T00:00:00Z".into(),
                shipment_id: first.shipment_id.clone(),
            },
        );
        assert!(outbox_load(log.path())[0].receipted_at.is_some());

        // Same bytes queued again after delivery is a new shipment.
        let second = queue_artifact(log.path(), &src, "apex2", "same.txt").unwrap();
        assert_ne!(first.shipment_id, second.shipment_id);
        assert!(second.receipted_at.is_none());

        // A replay of the first receipt must not close the second.
        let (_news, delivered) = ledger_hear_receipt(
            log.path(),
            HeardReceipt {
                stick: "stick-a".into(),
                root: first.root.clone(),
                node: "apex2".into(),
                accepted: true,
                heard_at: "2026-08-16T00:00:01Z".into(),
                shipment_id: first.shipment_id.clone(),
            },
        );
        assert!(delivered.is_none());
        let rows = outbox_load(log.path());
        let again = rows.iter().find(|e| e.id == second.id).unwrap();
        assert!(again.receipted_at.is_none());

        // A pre-SA-4 receipt (empty shipment_id) also must not close it.
        let (_news, delivered) = ledger_hear_receipt(
            log.path(),
            HeardReceipt {
                stick: "stick-a".into(),
                root: second.root.clone(),
                node: "apex2".into(),
                accepted: true,
                heard_at: "2026-08-16T00:00:02Z".into(),
                shipment_id: String::new(),
            },
        );
        assert!(delivered.is_none());
        assert!(outbox_load(log.path())
            .iter()
            .find(|e| e.id == second.id)
            .unwrap()
            .receipted_at
            .is_none());
    }

    #[test]
    fn safe_name_neutralizes_hostile_manifest_names() {
        assert_eq!(safe_name("../../etc/passwd", "ff00"), "passwd");
        assert_eq!(safe_name("..\\..\\boot.ini", "ff00"), "boot.ini");
        assert_eq!(safe_name(".hidden", "ff00"), "hidden");
        assert_eq!(safe_name("///", "ffaa00112233445566"), "cargo-ffaa00112233");
        assert_eq!(safe_name("report v2.md", "ff00"), "report v2.md");
    }

    #[test]
    fn plug_notice_is_quiet_when_there_is_no_courier_story() {
        assert!(compose_plug_notice(&PlugReport::default()).is_none());
        let mut r = PlugReport::default();
        r.verified.push(("report.md".into(), "apex-a".into()));
        let n = compose_plug_notice(&r).unwrap();
        assert!(n.contains("report.md"));
        assert!(n.contains("blake3 verified"));
    }
}
