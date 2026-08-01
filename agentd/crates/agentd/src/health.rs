//! Boot-health marker for the daemon self-update loop (docs/self-update.md, slice 1).
//!
//! On every boot agentd writes `<update_dir>/health.json` once a staged set of
//! checks pass. The root watchdog (slice 2) polls this file to decide whether a
//! freshly-swapped binary is healthy or must be rolled back. The marker carries
//! the `build.rs`-embedded commit so the watchdog can prove *which* binary booted
//! (`commit == target ∧ booted_at ≥ swap_ts ∧ status == "healthy"`).
//!
//! Gates (mirrors the doc's health contract):
//! 1. listeners bound — hard (loopback TCP probe of the gateway port).
//! 2. all restart=always plugins up — hard (folded from PluginUp/PluginDown).
//! 3. Cerebro reachable — soft: a bounded probe; a memory blip never blocks
//!    "healthy", we just flag `cognitive_ok:false`.

use std::collections::HashSet;
use std::net::{Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use apexos_core::{Event, PluginId};
use apexos_plugins::ToolProxy;
use serde::Serialize;
use tokio::sync::broadcast;

/// Hard cap on how long the marker task waits for the gates before giving up and
/// writing a `degraded` marker. Set above the watchdog's default probe TIMEOUT
/// (120s) so that, in production, the watchdog is the one that decides to roll
/// back; this deadline only matters when running standalone (dev / no watchdog).
const GATE_DEADLINE: Duration = Duration::from_secs(180);

/// The git commit this binary was built from (embedded by `build.rs`).
pub fn build_commit() -> &'static str {
    option_env!("GIT_COMMIT").unwrap_or("unknown")
}

/// Directory holding the self-update control + marker files. agentd has it as a
/// `ReadWritePaths` (`/var/lib/agentd`); the root watchdog reads/writes here too.
/// Overridable via `AGENTD_UPDATE_DIR` (dev / tests).
pub fn update_dir() -> PathBuf {
    std::env::var("AGENTD_UPDATE_DIR")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/var/lib/agentd/update"))
}

#[derive(Debug, Clone, Serialize)]
pub struct HealthChecks {
    pub listeners_bound: bool,
    pub plugins_loaded: usize,
    pub cognitive_ok: bool,
    /// Dual-tree integrity (Fabrica M1c): the worker ledger and the mandala
    /// trees agree — closed mandalas hold no open cells, open cells' workers
    /// exist, terminal workers left no cell open (the reap rule as a boot
    /// question). Informational like `cognitive_ok`: false until evaluated,
    /// NEVER a gate — a violation is an operator flag, not a rollback.
    pub mandala_coherent: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct HealthMarker {
    pub commit: String,
    /// `"booting"` → `"healthy"` (gates met) or `"degraded"` (deadline hit).
    pub status: String,
    pub booted_at: u64,
    pub pid: u32,
    pub checks: HealthChecks,
}

/// `"healthy"` requires BOTH hard gates; cognitive is informational only. Pure so
/// the gate logic is unit-testable without a running daemon.
pub fn decide_status(listeners_bound: bool, expected_plugins_up: bool) -> &'static str {
    if listeners_bound && expected_plugins_up {
        "healthy"
    } else {
        "booting"
    }
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Write the marker (temp + atomic rename; in-place fallback if dir-write is
/// unavailable). Best-effort — a failed marker write logs and returns, never
/// panics: the marker is a signal, not a critical path for serving.
fn write_marker(dir: &Path, marker: &HealthMarker) {
    if let Err(e) = std::fs::create_dir_all(dir) {
        eprintln!("[health] cannot create {}: {e}", dir.display());
        return;
    }
    let path = dir.join("health.json");
    let json = match serde_json::to_string_pretty(marker) {
        Ok(j) => j,
        Err(e) => {
            eprintln!("[health] serialize: {e}");
            return;
        }
    };
    let tmp = dir.join("health.json.tmp");
    let atomic = std::fs::write(&tmp, &json).and_then(|_| std::fs::rename(&tmp, &path));
    if let Err(e) = atomic {
        if let Err(e2) = std::fs::write(&path, &json) {
            eprintln!("[health] write {}: {e} / {e2}", path.display());
        }
    }
}

/// Loopback TCP probe of the gateway port. Probes 127.0.0.1 regardless of the
/// configured bind address (a `0.0.0.0` bind is not itself a connect target, but
/// the listener still accepts on loopback).
async fn probe_listener(addr: SocketAddr) -> bool {
    matches!(
        tokio::time::timeout(Duration::from_secs(2), tokio::net::TcpStream::connect(addr)).await,
        Ok(Ok(_))
    )
}

/// Bounded Cerebro reachability probe. `cortex_stats` is a cheap read — far
/// lighter than re-running `cognitive_bootstrap` (which the first turn already
/// does); the health gate only needs "is memory reachable", not the full block.
/// `ToolProxy::call` carries its own 10s timeout, so this can't wedge the boot.
async fn probe_cognitive(proxy: &ToolProxy, agent_id: &str) -> bool {
    let args = serde_json::json!({ "agent_id": agent_id });
    matches!(proxy.call("cortex_stats", args).await, Ok(out) if out.ok)
}

/// Spawn the boot-health marker task. Call it LAST in `main` so the gates it waits
/// on (gateway listener, plugin supervisor) are already being brought up. The
/// `events` receiver MUST be subscribed *before* the supervisor spawns, or early
/// `PluginUp` events are missed (same race the agent router guards against).
pub fn spawn_health_marker(
    gw_addr: SocketAddr,
    expected_plugins: Vec<PluginId>,
    mut events: broadcast::Receiver<Event>,
    proxy: ToolProxy,
    agent_id: String,
    log_dir: PathBuf,
) {
    let dir = update_dir();
    let commit = build_commit().to_string();
    let pid = std::process::id();
    let probe_addr = SocketAddr::new(Ipv4Addr::LOCALHOST.into(), gw_addr.port());
    let expected: HashSet<PluginId> = expected_plugins.into_iter().collect();

    tokio::spawn(async move {
        // 1. Stamp an immediate "booting" marker (fresh booted_at + correct commit)
        //    so a stale "healthy" left by the previous binary can't be read as this
        //    boot. (The watchdog also guards on commit + booted_at; this keeps the
        //    file honest from the first instant regardless.)
        let booted_at = now_unix();
        write_marker(
            &dir,
            &HealthMarker {
                commit: commit.clone(),
                status: "booting".into(),
                booted_at,
                pid,
                checks: HealthChecks {
                    listeners_bound: false,
                    plugins_loaded: 0,
                    cognitive_ok: false,
                    mandala_coherent: false,
                },
            },
        );

        // 2. Wait for the hard gates: listeners bound + every restart=always plugin up.
        let mut up: HashSet<PluginId> = HashSet::new();
        let mut listeners_bound = false;
        let deadline = tokio::time::Instant::now() + GATE_DEADLINE;
        let mut tick = tokio::time::interval(Duration::from_secs(2));
        loop {
            if !listeners_bound {
                listeners_bound = probe_listener(probe_addr).await;
            }
            if decide_status(listeners_bound, expected.is_subset(&up)) == "healthy" {
                break;
            }
            if tokio::time::Instant::now() >= deadline {
                eprintln!(
                    "[health] gates not met within {}s (listeners={listeners_bound}, \
                     plugins {}/{}) — writing degraded marker",
                    GATE_DEADLINE.as_secs(),
                    up.intersection(&expected).count(),
                    expected.len()
                );
                write_marker(
                    &dir,
                    &HealthMarker {
                        commit: commit.clone(),
                        status: "degraded".into(),
                        booted_at,
                        pid,
                        checks: HealthChecks {
                            listeners_bound,
                            plugins_loaded: up.len(),
                            cognitive_ok: false,
                            mandala_coherent: false,
                        },
                    },
                );
                return;
            }
            tokio::select! {
                ev = events.recv() => match ev {
                    Ok(Event::PluginUp   { plugin, .. }) => { up.insert(plugin); }
                    Ok(Event::PluginDown { plugin, .. }) => { up.remove(&plugin); }
                    Ok(_) => {}
                    Err(broadcast::error::RecvError::Lagged(_)) => {}
                    Err(broadcast::error::RecvError::Closed)    => return,
                },
                _ = tick.tick() => {}
            }
        }

        // 3. Cognitive reachability — bounded, NON-FATAL (don't punish a good daemon
        //    for a brief memory blip; just record the flag).
        let cognitive_ok = probe_cognitive(&proxy, &agent_id).await;

        // 3b. Dual-tree integrity (Fabrica M1c) — a file-based join over the
        //     worker/mandala truth this boot just reloaded. Informational:
        //     a cold boot with no trees is vacuously coherent, and a
        //     violation flags the operator without touching `status`.
        let (mandala_coherent, violations) = probe_mandala_coherence(&log_dir);
        for v in violations.iter().take(8) {
            eprintln!("[health] mandala integrity: {v}");
        }

        // 4. Healthy.
        let plugins_loaded = up.len();
        write_marker(
            &dir,
            &HealthMarker {
                commit,
                status: "healthy".into(),
                booted_at,
                pid,
                checks: HealthChecks {
                    listeners_bound: true,
                    plugins_loaded,
                    cognitive_ok,
                    mandala_coherent,
                },
            },
        );
        eprintln!(
            "[health] healthy (commit={}, plugins={plugins_loaded}, cognitive_ok={cognitive_ok}, mandala_coherent={mandala_coherent})",
            build_commit()
        );
    });
}

/// Dual-tree integrity (Fabrica M1c) — a pure join over the driver's on-disk
/// truth: mandalas.json × worktrees/<id>/*.json × workers.json ×
/// remote_workers.json. Returns the violations (empty = coherent). Three
/// laws checked:
///   1. a CLOSED mandala holds no open cells;
///   2. an open non-root cell's bound worker EXISTS in its ledger — the
///      LOCAL ledger for local-bodied cells, the REMOTE mirror ledger for
///      cells with `node` (M2 smoke find: the probe flagged healthy open
///      remote cells as "worker gone" because their wids live in
///      remote_workers.json — a false incoherence for the whole lifetime of
///      any cross-node ring);
///   3. a terminal worker leaves no cell open (reap lag — a crash between a
///      worker's terminal transition and its cell sync).
pub fn mandala_violations(
    mandalas: &[serde_json::Value],
    trees: &[(u64, Vec<serde_json::Value>)],
    workers: &[serde_json::Value],
    remotes: &[serde_json::Value],
) -> Vec<String> {
    let worker_state: std::collections::HashMap<u64, String> = workers
        .iter()
        .filter_map(|w| Some((w["id"].as_u64()?, w["state"].as_str()?.to_string())))
        .collect();
    // Remote rows persist state as the raw wire string (`state_raw`) — the
    // three terminal strings are exact; anything else (assigning, running,
    // cancel requested, a newer peer's word) reads open, the skew law.
    let remote_state: std::collections::HashMap<u64, String> = remotes
        .iter()
        .filter_map(|r| Some((r["id"].as_u64()?, r["state_raw"].as_str().unwrap_or("").to_string())))
        .collect();
    let open = |st: &str| !matches!(st, "done" | "failed" | "cancelled");
    let mut out = Vec::new();
    for m in mandalas {
        let (Some(id), Some(mstate)) = (m["id"].as_u64(), m["state"].as_str()) else { continue };
        let Some((_, cells)) = trees.iter().find(|(tid, _)| *tid == id) else { continue };
        for c in cells {
            let addr = c["addr"].as_str().unwrap_or("?");
            let cstate = c["state"].as_str().unwrap_or("open");
            if mstate == "closed" && open(cstate) {
                out.push(format!("mandala {id} is closed but cell {addr} is {cstate}"));
            }
            if addr == "0" {
                continue; // the root is the conductor's own cell — no worker binds it
            }
            if open(cstate) {
                let remote_bodied = c["node"].as_str().is_some();
                match c["worker"].as_u64() {
                    None => out.push(format!("mandala {id} cell {addr} is open with no worker bound")),
                    Some(w) if remote_bodied => match remote_state.get(&w).map(String::as_str) {
                        None => out.push(format!("mandala {id} cell {addr} is open but remote worker {w} is gone from the remote ledger")),
                        Some(ws) if !open(ws) => out.push(format!(
                            "mandala {id} cell {addr} is open but remote worker {w} is {ws} (reap lag)")),
                        _ => {}
                    },
                    Some(w) => match worker_state.get(&w).map(String::as_str) {
                        None => out.push(format!("mandala {id} cell {addr} is open but worker {w} is gone from the ledger")),
                        Some(ws) if !open(ws) => out.push(format!(
                            "mandala {id} cell {addr} is open but worker {w} is {ws} (reap lag)")),
                        _ => {}
                    },
                }
            }
        }
    }
    out
}

/// Load the driver's files and run the join. Missing/unparseable files read
/// as empty — a cold boot is vacuously coherent (the normal case is green).
pub fn probe_mandala_coherence(log_dir: &Path) -> (bool, Vec<String>) {
    let read_list = |p: PathBuf| -> Vec<serde_json::Value> {
        std::fs::read_to_string(p)
            .ok()
            .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
            .and_then(|v| v.as_array().cloned())
            .unwrap_or_default()
    };
    let mandalas = read_list(log_dir.join("mandalas.json"));
    let workers = read_list(log_dir.join("workers.json"));
    let remotes = read_list(log_dir.join("remote_workers.json"));
    let mut trees = Vec::new();
    for m in &mandalas {
        let Some(id) = m["id"].as_u64() else { continue };
        let dir = log_dir.join("worktrees").join(id.to_string());
        let mut cells = Vec::new();
        if let Ok(rd) = std::fs::read_dir(&dir) {
            for e in rd.flatten() {
                if e.path().extension().and_then(|x| x.to_str()) == Some("json") {
                    if let Some(v) = std::fs::read_to_string(e.path())
                        .ok()
                        .and_then(|t| serde_json::from_str(&t).ok())
                    {
                        cells.push(v);
                    }
                }
            }
        }
        trees.push((id, cells));
    }
    let violations = mandala_violations(&mandalas, &trees, &workers, &remotes);
    (violations.is_empty(), violations)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decide_status_truth_table() {
        assert_eq!(decide_status(true, true), "healthy");
        assert_eq!(decide_status(true, false), "booting");
        assert_eq!(decide_status(false, true), "booting");
        assert_eq!(decide_status(false, false), "booting");
    }

    #[test]
    fn update_dir_default_and_override() {
        // Default when unset.
        std::env::remove_var("AGENTD_UPDATE_DIR");
        assert_eq!(update_dir(), PathBuf::from("/var/lib/agentd/update"));
        // Honors an override.
        std::env::set_var("AGENTD_UPDATE_DIR", "/tmp/apex-update-test");
        assert_eq!(update_dir(), PathBuf::from("/tmp/apex-update-test"));
        // Blank falls back to default.
        std::env::set_var("AGENTD_UPDATE_DIR", "   ");
        assert_eq!(update_dir(), PathBuf::from("/var/lib/agentd/update"));
        std::env::remove_var("AGENTD_UPDATE_DIR");
    }

    #[test]
    fn marker_serializes_to_the_documented_schema() {
        let m = HealthMarker {
            commit: "abc123".into(),
            status: "healthy".into(),
            booted_at: 1_700_000_000,
            pid: 4242,
            checks: HealthChecks {
                listeners_bound: true,
                plugins_loaded: 3,
                cognitive_ok: true,
                mandala_coherent: true,
            },
        };
        let v: serde_json::Value = serde_json::to_value(&m).unwrap();
        assert_eq!(v["commit"], "abc123");
        assert_eq!(v["status"], "healthy");
        assert_eq!(v["booted_at"], 1_700_000_000u64);
        assert_eq!(v["pid"], 4242);
        assert_eq!(v["checks"]["listeners_bound"], true);
        assert_eq!(v["checks"]["plugins_loaded"], 3);
        assert_eq!(v["checks"]["cognitive_ok"], true);
        assert_eq!(v["checks"]["mandala_coherent"], true);
    }

    #[test]
    fn mandala_violations_catch_the_three_laws() {
        let mandalas = vec![
            serde_json::json!({ "id": 1, "state": "closed" }),
            serde_json::json!({ "id": 2, "state": "open" }),
        ];
        let trees = vec![
            (1u64, vec![
                serde_json::json!({ "addr": "0", "state": "open" }),          // closed mandala, open ROOT
                serde_json::json!({ "addr": "0.0", "state": "done", "worker": 5 }),
            ]),
            (2u64, vec![
                serde_json::json!({ "addr": "0", "state": "open" }),          // open mandala root — fine
                serde_json::json!({ "addr": "0.1", "state": "open", "worker": 9 }),   // worker gone
                serde_json::json!({ "addr": "0.2", "state": "open", "worker": 6 }),   // reap lag
                serde_json::json!({ "addr": "0.3", "state": "open" }),                // no worker bound
                serde_json::json!({ "addr": "0.4", "state": "open", "worker": 7 }),   // healthy
                // M2 — remote-bodied cells check the REMOTE ledger:
                serde_json::json!({ "addr": "0.5", "state": "open", "worker": 40, "node": "andre-laptop" }), // healthy remote (running)
                serde_json::json!({ "addr": "0.6", "state": "open", "worker": 41, "node": "andre-laptop" }), // remote gone
                serde_json::json!({ "addr": "0.7", "state": "open", "worker": 42, "node": "tvpi" }),         // remote reap lag
                serde_json::json!({ "addr": "0.8", "state": "open", "worker": 43, "node": "tvpi" }),         // healthy remote (skew word)
            ]),
        ];
        let workers = vec![
            serde_json::json!({ "id": 5, "state": "done" }),
            serde_json::json!({ "id": 6, "state": "done" }),    // terminal, cell 0.2 open
            serde_json::json!({ "id": 7, "state": "running" }),
        ];
        let remotes = vec![
            serde_json::json!({ "id": 40, "state_raw": "running" }),
            serde_json::json!({ "id": 42, "state_raw": "done" }),        // terminal, cell 0.7 open
            serde_json::json!({ "id": 43, "state_raw": "hibernating" }), // newer peer's word = open (skew law)
        ];
        let v = mandala_violations(&mandalas, &trees, &workers, &remotes);
        assert_eq!(v.len(), 6, "{v:?}");
        assert!(v.iter().any(|s| s.contains("mandala 1 is closed but cell 0")));
        assert!(v.iter().any(|s| s.contains("cell 0.1 is open but worker 9 is gone")));
        assert!(v.iter().any(|s| s.contains("cell 0.2 is open but worker 6 is done (reap lag)")));
        assert!(v.iter().any(|s| s.contains("cell 0.3 is open with no worker bound")));
        // The M2 smoke find: an open remote-bodied cell whose mirror row is
        // alive is COHERENT (0.5, 0.8 — no false "worker gone"); a vanished
        // or terminal mirror still flags through the remote ledger.
        assert!(v.iter().any(|s| s.contains("cell 0.6 is open but remote worker 41 is gone from the remote ledger")));
        assert!(v.iter().any(|s| s.contains("cell 0.7 is open but remote worker 42 is done (reap lag)")));
        assert!(!v.iter().any(|s| s.contains("0.5")), "a live remote body is coherent");
        assert!(!v.iter().any(|s| s.contains("0.8")), "an unknown wire word reads open — coherent");
        // A coherent world — and the cold-boot vacuum — are both green.
        assert!(mandala_violations(&[], &[], &[], &[]).is_empty());
        let (ok, list) = probe_mandala_coherence(Path::new("/nonexistent/apexos-health-xyz"));
        assert!(ok && list.is_empty(), "a cold boot is vacuously coherent");
    }

    #[test]
    fn write_marker_roundtrips_through_a_temp_dir() {
        let dir = std::env::temp_dir().join("apex-health-test-rs");
        let _ = std::fs::remove_dir_all(&dir);
        let m = HealthMarker {
            commit: "deadbeef".into(),
            status: "booting".into(),
            booted_at: 42,
            pid: 7,
            checks: HealthChecks {
                listeners_bound: false,
                plugins_loaded: 0,
                cognitive_ok: false,
                mandala_coherent: false,
            },
        };
        write_marker(&dir, &m);
        let txt = std::fs::read_to_string(dir.join("health.json")).unwrap();
        let v: serde_json::Value = serde_json::from_str(&txt).unwrap();
        assert_eq!(v["commit"], "deadbeef");
        assert_eq!(v["status"], "booting");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
