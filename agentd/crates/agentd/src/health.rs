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
                },
            },
        );
        eprintln!(
            "[health] healthy (commit={}, plugins={plugins_loaded}, cognitive_ok={cognitive_ok})",
            build_commit()
        );
    });
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
