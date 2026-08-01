//! W2 mesh workers — the pure core of the remote seam (docs/fabrica.md W2).
//!
//! THE OWNERSHIP RULING: the peer owns everything stateful — worker record,
//! session JSONL, thermal cap, policy, review procedure, evidence, episode.
//! The conductor owns only batch bookkeeping: a `RemoteWorker` mirror row per
//! remote task, the batch deadline, and the evidence MIRROR files integration
//! reads. The wire carries assignments out and reports home, never state —
//! one writing daemon per file, no sync protocol, no split brain.
//!
//! Everything here is pure (payload builders/parsers, state mapping, the
//! mirror-doc composer, poll cadence) so the laws are unit-tested before any
//! driver wiring. The wire is TOLERANT by design: worker states travel as
//! plain strings and an unknown state from a newer peer reads as non-terminal
//! — a rolling colony update must never break supervision (`WorkerState` has
//! no `#[serde(other)]`, so typed decode across skew would be a hard error).
//!
//! Remote rows are a SEPARATE struct and file (`remote_workers.json`), never
//! rows in `workers.json`: peer worker-session ids collide numerically across
//! nodes (`next_worker_sid` is node-local, both sides mint from `1<<62`), so
//! a remote row stored as a `PersistedWorker` would reload as a phantom LOCAL
//! parked worker. The local row id spends the same `next_worker_id` counter
//! as local workers — batch rows and evidence mirror names stay uniform.

use serde::{Deserialize, Serialize};
use std::time::Duration;

use apexos_core::WorkerState;

/// Report-home retry ladder (peer side): a lost conductor is reconciled by
/// its own polls, so pushes give up quickly — the conductor's batch deadline
/// is the final net, never a retry loop.
pub const REPORT_RETRY_DELAYS_S: [u64; 3] = [10, 30, 60];

/// Assign/report/query/cancel HTTP timeout — mesh calls are LAN-local; a
/// peer that can't answer in this window is the beacon's problem, not ours.
pub const MESH_HTTP_TIMEOUT_S: u64 = 20;

/// Conductor-side mirror states that are not peer states: minted-not-yet-
/// accepted, and the relayed-cancel window (non-terminal until confirmed —
/// the deadline nets a peer that never answers).
pub const STATE_ASSIGNING: &str = "assigning";
pub const STATE_CANCEL_REQUESTED: &str = "cancel requested";

// ── Wire payloads (plain JSON, hand-tolerant — never typed Event frames) ────

/// One remote task in an assignment: the prompt plus the optional model pin.
/// Model pins cross the wire (identity, not trust — the W1d distinction);
/// yolo NEVER crosses (it is not a field — the peer's policy is sovereign).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteTaskItem {
    pub prompt: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

/// Build the `POST /api/worker/fanout` body (conductor → peer).
pub fn build_fanout_body(from: &str, origin_batch: u64, deadline_s: u64, tasks: &[RemoteTaskItem]) -> serde_json::Value {
    serde_json::json!({
        "from": from,
        "origin_batch": origin_batch,
        "deadline_s": deadline_s,
        "tasks": tasks,
    })
}

/// One minted row in the peer's fanout accept: `index` maps back to the
/// request's task order — the conductor joins minted ids to its rows by it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcceptedRow {
    pub index: usize,
    pub worker: u64,
    pub session: u64,
}

/// Parse the peer's fanout response. `Ok` requires `ok:true` plus a workers
/// array; anything else surfaces the peer's error string (or a shape note) —
/// an assign refusal must reach the conductor's rows as an honest detail.
pub fn parse_fanout_accept(v: &serde_json::Value) -> Result<(u64, Vec<AcceptedRow>), String> {
    if v["ok"].as_bool() != Some(true) {
        return Err(v["error"].as_str().unwrap_or("peer refused the fan").to_string());
    }
    let batch = v["batch"].as_u64().ok_or("accept carries no batch id")?;
    let rows = v["workers"].as_array().ok_or("accept carries no workers array")?;
    let mut out = Vec::with_capacity(rows.len());
    for r in rows {
        let (Some(index), Some(worker), Some(session)) =
            (r["index"].as_u64(), r["worker"].as_u64(), r["session"].as_u64())
        else { return Err("accept row missing index/worker/session".into()) };
        out.push(AcceptedRow { index: index as usize, worker, session });
    }
    Ok((batch, out))
}

/// One worker's line in a report/query payload (peer → conductor). `state`
/// is a STRING on the wire — see the module doc's version-skew law. The
/// evidence DOC (the peer's small terminal JSON, never artifact payloads)
/// rides inline for terminal rows so mirroring costs one hop.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WireRow {
    pub worker: u64,
    pub session: u64,
    pub state: String,
    #[serde(default)]
    pub timed_out: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence_doc: Option<serde_json::Value>,
}

/// Build the report/query response body (peer side). Report-home POSTs and
/// query replies share this shape — one parser on the conductor covers both.
pub fn build_rows_body(from: &str, origin_batch: u64, peer_batch: u64, rows: &[WireRow]) -> serde_json::Value {
    serde_json::json!({
        "ok": true,
        "from": from,
        "origin_batch": origin_batch,
        "batch": peer_batch,
        "rows": rows,
    })
}

/// Parse a rows payload tolerantly: rows that don't parse are SKIPPED (a
/// half-broken payload still advances the batch; the poll loop retries the
/// rest), absent arrays read empty.
pub fn parse_rows(v: &serde_json::Value) -> Vec<WireRow> {
    v["rows"].as_array().map(|rows| {
        rows.iter()
            .filter_map(|r| serde_json::from_value::<WireRow>(r.clone()).ok())
            .collect()
    }).unwrap_or_default()
}

// ── State mapping (the tolerant boundary) ───────────────────────────────────

/// The wire form of a local WorkerState (snake_case, the workers.json form).
pub fn state_str(s: WorkerState) -> String {
    format!("{s:?}").to_lowercase()
}

/// Terminal-by-string: the three terminal states, exactly. An unknown string
/// from a newer peer is NON-terminal — supervision waits (deadline nets it),
/// never crashes and never invents completion.
pub fn is_terminal_state_str(s: &str) -> bool {
    matches!(s, "done" | "failed" | "cancelled")
}

/// Map a wire state string to the typed enum where known. Unknown → None;
/// callers that must produce a typed `BatchWorkerRow` fall back to `Queued`
/// (a white lie bounded to version skew — `timed_out`/the mirror doc carry
/// the raw truth; terminal math never uses this fallback).
pub fn parse_state_tolerant(s: &str) -> Option<WorkerState> {
    match s {
        "queued" => Some(WorkerState::Queued),
        "running" => Some(WorkerState::Running),
        "idle" => Some(WorkerState::Idle),
        "parked" => Some(WorkerState::Parked),
        "blocked" => Some(WorkerState::Blocked),
        "done" => Some(WorkerState::Done),
        "failed" => Some(WorkerState::Failed),
        "cancelled" => Some(WorkerState::Cancelled),
        _ => None,
    }
}

/// The typed state for a report row — parsed, else the bounded fallback.
pub fn row_state(raw: &str) -> WorkerState {
    parse_state_tolerant(raw).unwrap_or(WorkerState::Queued)
}

// ── The conductor's mirror row ───────────────────────────────────────────────

/// A remote worker as the conductor sees it: batch bookkeeping ONLY. No
/// local session exists (events ride the `SessionId(0)` sentinel + `node`);
/// no thermal slot is held; supervision is a poll, not a clock.
#[derive(Debug, Clone)]
pub struct RemoteWorker {
    pub batch: u64,
    /// Conductor session that fanned the batch (for event cards).
    pub parent: u64,
    pub node: String,
    pub task: String,
    pub model: Option<String>,
    /// Peer-side ids, filled by the assign accept.
    pub remote_batch: Option<u64>,
    pub remote_worker: Option<u64>,
    pub remote_session: Option<u64>,
    /// The state mirror — a raw wire string (`STATE_ASSIGNING` before accept).
    pub state_raw: String,
    pub summary: Option<String>,
    /// Local mirror evidence path, once terminal.
    pub evidence: Option<String>,
    pub assigned_epoch: u64,
}

impl RemoteWorker {
    pub fn is_terminal(&self) -> bool { is_terminal_state_str(&self.state_raw) }
}

/// On-disk form. New fields MUST carry `#[serde(default)]` (the
/// PersistedGoal/PersistedWorker discipline).
#[derive(Serialize, Deserialize)]
pub struct PersistedRemoteWorker {
    pub id: u64,
    pub batch: u64,
    pub parent: u64,
    pub node: String,
    pub task: String,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub remote_batch: Option<u64>,
    #[serde(default)]
    pub remote_worker: Option<u64>,
    #[serde(default)]
    pub remote_session: Option<u64>,
    #[serde(default)]
    pub state_raw: String,
    #[serde(default)]
    pub summary: Option<String>,
    #[serde(default)]
    pub evidence: Option<String>,
    #[serde(default)]
    pub assigned_epoch: u64,
}

pub fn save_remotes(remotes: &std::collections::HashMap<u64, RemoteWorker>, path: &std::path::Path) {
    let mut snapshot: Vec<PersistedRemoteWorker> = remotes.iter().map(|(id, r)| PersistedRemoteWorker {
        id: *id, batch: r.batch, parent: r.parent, node: r.node.clone(), task: r.task.clone(),
        model: r.model.clone(), remote_batch: r.remote_batch, remote_worker: r.remote_worker,
        remote_session: r.remote_session, state_raw: r.state_raw.clone(),
        summary: r.summary.clone(), evidence: r.evidence.clone(), assigned_epoch: r.assigned_epoch,
    }).collect();
    snapshot.sort_by_key(|r| r.id);
    if let Some(parent) = path.parent() { let _ = std::fs::create_dir_all(parent); }
    if let Ok(json) = serde_json::to_string_pretty(&snapshot) {
        let tmp = path.with_extension("json.tmp");
        if std::fs::write(&tmp, &json).is_ok() {
            let _ = std::fs::rename(&tmp, path);
        }
    }
}

pub fn load_remotes(path: &std::path::Path) -> Vec<PersistedRemoteWorker> {
    std::fs::read_to_string(path).ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

// ── The evidence mirror ──────────────────────────────────────────────────────

/// Compose the conductor-side mirror doc for one terminal remote row. Written
/// to `<log_dir>/agents/<local_wid>.json` — the SAME naming as local workers,
/// so batch rows and integration are uniform: reading the mirror IS the
/// integration step. Artifacts stay on the peer (their paths are the peer's
/// workspace); `mesh_file_send` is the colony's hand for bringing one home.
pub fn compose_mirror_doc(local_wid: u64, r: &RemoteWorker, completed_at: &str) -> serde_json::Value {
    serde_json::json!({
        "worker": local_wid,
        "batch": r.batch,
        "parent": r.parent,
        "node": r.node,
        "remote_worker": r.remote_worker,
        "remote_session": r.remote_session,
        "task": r.task,
        "state": r.state_raw,
        "summary": r.summary,
        "history": r.remote_session
            .map(|s| format!("remote:{}:sessions/{}.jsonl", r.node, s))
            .unwrap_or_else(|| format!("remote:{}:(never assigned)", r.node)),
        "completed_at": completed_at,
        "origin": "mesh-mirror",
    })
}

/// Fold the peer's own evidence doc into the mirror (artifacts + anything
/// else the peer recorded ride under `remote_evidence`; its artifact list is
/// ALSO lifted to the top level so integration reads one shape for local and
/// remote rows alike).
pub fn fold_remote_evidence(mut mirror: serde_json::Value, evidence_doc: Option<&serde_json::Value>) -> serde_json::Value {
    if let Some(doc) = evidence_doc {
        mirror["artifacts"] = doc.get("artifacts").cloned().unwrap_or(serde_json::json!([]));
        mirror["remote_evidence"] = doc.clone();
    } else {
        mirror["artifacts"] = serde_json::json!([]);
    }
    mirror
}

// ── Poll cadence (supervision across the wire = the review discipline) ──────

/// A fingerprint of a rows snapshot — the poll ladder widens only while the
/// picture is UNCHANGED (the review procedure's repeated-quiet-word rule).
pub fn rows_fingerprint(rows: &[WireRow]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    for r in rows {
        r.worker.hash(&mut h);
        r.state.hash(&mut h);
        r.timed_out.hash(&mut h);
        r.evidence_doc.is_some().hash(&mut h);
    }
    h.finish()
}

/// Next poll interval: changed snapshots hold the base period; repeated
/// identical snapshots climb the same Fibonacci ladder reviews use. Dark
/// peers are skipped upstream (the beacon owns dark detection — a poll is
/// never the liveness probe).
pub fn next_poll_in(attempt: u32, changed: bool, base: Duration) -> Duration {
    if changed { base } else { crate::review::fib_backoff(attempt, base) }
}

// ── The task provenance prefix (peer-side board honesty) ────────────────────

/// Remote-origin tasks carry a short provenance prefix on the peer so its
/// board tells the truth about who is using the node. Short by design — the
/// task text rides every directive.
pub fn provenance_prefix(from: &str, origin_batch: u64, prompt: &str) -> String {
    format!("[mesh: {from} batch {origin_batch}] {prompt}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(p: &str) -> RemoteTaskItem { RemoteTaskItem { prompt: p.into(), model: None } }

    #[test]
    fn fanout_body_and_accept_round_trip() {
        let body = build_fanout_body("apex1", 7, 1800, &[item("write tests"), RemoteTaskItem { prompt: "port docs".into(), model: Some("claude-haiku-4-5".into()) }]);
        assert_eq!(body["from"], "apex1");
        assert_eq!(body["origin_batch"], 7);
        assert_eq!(body["deadline_s"], 1800);
        assert_eq!(body["tasks"][1]["model"], "claude-haiku-4-5");
        assert!(body["tasks"][0].get("model").is_none(), "None model must not serialize");
        // yolo NEVER crosses the wire — the body has no such key, structurally.
        assert!(body.get("yolo").is_none());

        let accept = serde_json::json!({
            "ok": true, "batch": 3,
            "workers": [ {"index": 0, "worker": 11, "session": 4611686018427387904u64},
                         {"index": 1, "worker": 12, "session": 4611686018427387905u64} ],
            "cap": 8, "admitted": 2, "queued": 0
        });
        let (batch, rows) = parse_fanout_accept(&accept).unwrap();
        assert_eq!(batch, 3);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0], AcceptedRow { index: 0, worker: 11, session: 4611686018427387904 });
        // Refusals surface the peer's error string.
        let refused = serde_json::json!({ "ok": false, "error": "mesh workers disabled" });
        assert_eq!(parse_fanout_accept(&refused).unwrap_err(), "mesh workers disabled");
        // Shape breaks are named, not panics.
        assert!(parse_fanout_accept(&serde_json::json!({ "ok": true })).is_err());
    }

    #[test]
    fn rows_parse_tolerantly_and_state_strings_stay_honest() {
        let v = serde_json::json!({
            "ok": true, "from": "apex-3", "origin_batch": 7, "batch": 3,
            "rows": [
                { "worker": 11, "session": 4611686018427387904u64, "state": "done",
                  "summary": "shipped", "evidence_doc": { "artifacts": ["out/a.md"] } },
                { "worker": 12, "session": 4611686018427387905u64, "state": "hibernating" },
                { "not": "a row" }
            ]
        });
        let rows = parse_rows(&v);
        assert_eq!(rows.len(), 2, "unparseable rows are skipped, not fatal");
        assert!(is_terminal_state_str(&rows[0].state));
        // The version-skew law: an unknown state is NON-terminal and maps to
        // the bounded fallback for typed rows.
        assert!(!is_terminal_state_str(&rows[1].state));
        assert_eq!(parse_state_tolerant(&rows[1].state), None);
        assert_eq!(row_state(&rows[1].state), WorkerState::Queued);
        // Round trip every known state string.
        for s in [WorkerState::Queued, WorkerState::Running, WorkerState::Idle, WorkerState::Parked,
                  WorkerState::Blocked, WorkerState::Done, WorkerState::Failed, WorkerState::Cancelled] {
            assert_eq!(parse_state_tolerant(&state_str(s)), Some(s));
        }
        assert!(is_terminal_state_str("cancelled"));
        assert!(!is_terminal_state_str(STATE_ASSIGNING));
        assert!(!is_terminal_state_str(STATE_CANCEL_REQUESTED));
    }

    fn remote(state: &str) -> RemoteWorker {
        RemoteWorker {
            batch: 7, parent: 42, node: "apex-3".into(), task: "port the docs".into(),
            model: None, remote_batch: Some(3), remote_worker: Some(11),
            remote_session: Some(4611686018427387904), state_raw: state.into(),
            summary: Some("shipped".into()), evidence: None, assigned_epoch: 0,
        }
    }

    #[test]
    fn mirror_doc_carries_provenance_and_uniform_shape() {
        let m = compose_mirror_doc(31, &remote("done"), "2026-08-01T00:00:00Z");
        assert_eq!(m["worker"], 31, "mirror keys the LOCAL row id — batch rows stay uniform");
        assert_eq!(m["node"], "apex-3");
        assert_eq!(m["remote_worker"], 11);
        assert_eq!(m["state"], "done");
        assert_eq!(m["history"], "remote:apex-3:sessions/4611686018427387904.jsonl");
        assert_eq!(m["origin"], "mesh-mirror");
        // The peer's evidence doc folds in: artifacts lift to the top level.
        let folded = fold_remote_evidence(m.clone(), Some(&serde_json::json!({
            "artifacts": ["out/a.md"], "task": "port the docs", "episode": "ep_9"
        })));
        assert_eq!(folded["artifacts"][0], "out/a.md");
        assert_eq!(folded["remote_evidence"]["episode"], "ep_9");
        // No doc (assign-fail, straggler) → empty artifacts, shape intact.
        let bare = fold_remote_evidence(m, None);
        assert_eq!(bare["artifacts"], serde_json::json!([]));
        // A never-assigned row still composes an honest history line.
        let mut na = remote(STATE_ASSIGNING); na.remote_session = None;
        let m2 = compose_mirror_doc(32, &na, "t");
        assert_eq!(m2["history"], "remote:apex-3:(never assigned)");
    }

    #[test]
    fn persisted_remote_worker_round_trips_and_defaults() {
        let r = remote("running");
        let mut map = std::collections::HashMap::new();
        map.insert(31u64, r);
        let dir = std::env::temp_dir().join(format!("apexos-remotes-{}", std::process::id()));
        let path = dir.join("remote_workers.json");
        save_remotes(&map, &path);
        let back = load_remotes(&path);
        assert_eq!(back.len(), 1);
        assert_eq!(back[0].id, 31);
        assert_eq!(back[0].node, "apex-3");
        assert_eq!(back[0].remote_batch, Some(3));
        assert_eq!(back[0].state_raw, "running");
        // A minimal legacy row loads through the defaults.
        let legacy: PersistedRemoteWorker = serde_json::from_str(
            r#"{"id":1,"batch":2,"parent":0,"node":"tvpi","task":"x"}"#).unwrap();
        assert_eq!(legacy.state_raw, "");
        assert!(legacy.remote_worker.is_none());
        let _ = std::fs::remove_dir_all(&dir);
        // Missing file loads empty, never fatal.
        assert!(load_remotes(std::path::Path::new("/nonexistent/apexos-remotes-x.json")).is_empty());
    }

    #[test]
    fn poll_ladder_holds_on_change_and_climbs_when_quiet() {
        let base = Duration::from_secs(30);
        assert_eq!(next_poll_in(4, true, base), base, "a changed snapshot resets to the base period");
        let a0 = next_poll_in(0, false, base);
        let a3 = next_poll_in(3, false, base);
        assert!(a3 > a0, "repeated identical snapshots widen the interval");
        // Fingerprints: state changes flip it; identical snapshots don't.
        let rows1 = parse_rows(&serde_json::json!({ "rows": [ {"worker":1,"session":9,"state":"running"} ] }));
        let rows2 = parse_rows(&serde_json::json!({ "rows": [ {"worker":1,"session":9,"state":"running"} ] }));
        let rows3 = parse_rows(&serde_json::json!({ "rows": [ {"worker":1,"session":9,"state":"done"} ] }));
        assert_eq!(rows_fingerprint(&rows1), rows_fingerprint(&rows2));
        assert_ne!(rows_fingerprint(&rows1), rows_fingerprint(&rows3));
    }

    #[test]
    fn report_body_and_provenance_prefix_carry_the_contract() {
        let rows = vec![WireRow {
            worker: 11, session: 4611686018427387904, state: "done".into(),
            timed_out: false, summary: Some("shipped".into()),
            evidence_doc: Some(serde_json::json!({"artifacts": []})),
        }];
        let body = build_rows_body("apex-3", 7, 3, &rows);
        assert_eq!(body["ok"], true);
        assert_eq!(body["origin_batch"], 7);
        assert_eq!(body["batch"], 3);
        assert_eq!(body["rows"][0]["state"], "done");
        let p = provenance_prefix("apex1", 7, "write the tests");
        assert!(p.starts_with("[mesh: apex1 batch 7] "));
        assert!(p.contains("write the tests"));
    }
}
