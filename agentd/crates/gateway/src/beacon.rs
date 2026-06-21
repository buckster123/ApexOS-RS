//! Mesh downtime beacon — colony-mesh spine, slice 1 (APEX's pick over NATS).
//!
//! Active liveness: every `MESH_BEACON_INTERVAL_SECS` each registered peer
//! (peers.toml) is HTTP-probed. A peer that misses `MESH_BEACON_STALE_MISSES`
//! consecutive probes crosses to **dark**; one success brings it back. Each
//! up↔down EDGE emits a global `MeshNodeStatus` event (→ board notification) and —
//! unless `MESH_BEACON_NOTIFY_AGENT=0` — injects a root-session prompt so the agent
//! is *told* a node went silent instead of a human having to notice the board went
//! grey. The point (APEX): a sensor-head node going dark mid-thermal-alert must not
//! look identical to "everything fine".
//!
//! Liveness "up" = the peer answered the HTTP layer AT ALL (even a 401) — only a
//! transport error/timeout is a miss. So a peer with no stored a2a token still
//! reports alive (it responded), and we never false-dark on an auth quirk.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use apexos_core::{BusHandle, Event, SessionId};
use tokio::sync::RwLock;

use crate::PeerRegistry;

const DEFAULT_INTERVAL_SECS: u64 = 30;
const DEFAULT_STALE_MISSES:  u32 = 3;
const PROBE_TIMEOUT_SECS:    u64 = 5;
const MIN_INTERVAL_SECS:     u64 = 10; // floor so a typo can't hammer the LAN

/// Per-peer liveness, held in the shared `LivenessMap`. `last_ok` is monotonic
/// (Instant) — the HTTP handler reports `last_ok.elapsed()` as seconds-since-seen.
#[derive(Debug, Clone, Default)]
pub struct PeerLiveness {
    pub dark:    bool,
    pub misses:  u32,
    pub last_ok: Option<Instant>,
}

/// node_id → liveness. Written by the beacon loop, read by `GET /api/mesh/peers`.
pub type LivenessMap = Arc<RwLock<HashMap<String, PeerLiveness>>>;

/// An up↔down transition the beacon should announce (only on the edge, never every poll).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BeaconEdge { WentDark, Recovered }

/// Pure liveness state-machine step (unit-tested): given the current miss-streak,
/// dark flag, this probe's outcome, and the dark threshold, return the new streak,
/// the new dark flag, and the edge (if any) crossed THIS step. Code controls the
/// rule; the loop is just IO around this. A miss only goes dark once it reaches the
/// threshold; a success recovers only if it was dark — so flapping below threshold
/// or repeated misses while already dark emit nothing.
pub fn beacon_step(misses: u32, dark: bool, ok: bool, threshold: u32) -> (u32, bool, Option<BeaconEdge>) {
    if ok {
        let edge = if dark { Some(BeaconEdge::Recovered) } else { None };
        (0, false, edge)
    } else {
        let m = misses.saturating_add(1);
        if !dark && m >= threshold {
            (m, true, Some(BeaconEdge::WentDark))
        } else {
            (m, dark, None)
        }
    }
}

fn env_flag_on(key: &str) -> bool {
    match std::env::var(key) {
        Ok(v) => { let v = v.to_lowercase(); v != "0" && v != "false" && v != "off" }
        Err(_) => true, // default ON
    }
}

/// Probe one peer for liveness. Returns true if the node answered the HTTP layer at
/// all (any status — even 401), false only on a transport error/timeout. Mirrors
/// `supervisor::fetch_peer_capabilities`'s ws→http derivation + bearer.
async fn probe_peer(ws_url: &str, token: Option<&str>) -> bool {
    let http_base = ws_url.replacen("ws://", "http://", 1).replacen("wss://", "https://", 1);
    let mut req = reqwest::Client::new()
        .get(format!("{http_base}/api/capabilities"))
        .timeout(Duration::from_secs(PROBE_TIMEOUT_SECS));
    if let Some(t) = token {
        req = req.bearer_auth(t);
    }
    req.send().await.is_ok()
}

/// Spawn the downtime beacon. No-op (logs and returns) if `MESH_BEACON=0`.
pub fn spawn_beacon_loop(peers: Arc<RwLock<PeerRegistry>>, bus: BusHandle, liveness: LivenessMap) {
    if !env_flag_on("MESH_BEACON") {
        eprintln!("[beacon] disabled (MESH_BEACON=0)");
        return;
    }
    let interval_secs = std::env::var("MESH_BEACON_INTERVAL_SECS")
        .ok().and_then(|s| s.parse::<u64>().ok()).unwrap_or(DEFAULT_INTERVAL_SECS)
        .max(MIN_INTERVAL_SECS);
    let threshold = std::env::var("MESH_BEACON_STALE_MISSES")
        .ok().and_then(|s| s.parse::<u32>().ok()).unwrap_or(DEFAULT_STALE_MISSES)
        .max(1);
    let notify_agent = env_flag_on("MESH_BEACON_NOTIFY_AGENT");

    eprintln!(
        "[beacon] downtime beacon — interval {interval_secs}s, dark after {threshold} misses (~{}s), notify_agent={notify_agent}",
        interval_secs * threshold as u64
    );

    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_secs(interval_secs));
        ticker.tick().await; // consume the immediate first tick
        ticker.tick().await; // let startup settle one interval before the first probe

        loop {
            // Snapshot targets under a short read lock — never hold it across a probe/await.
            let targets: Vec<(String, String, Option<String>)> = {
                let reg = peers.read().await;
                reg.peers.iter().map(|p| (p.node_id.clone(), p.ws_url.clone(), p.token.clone())).collect()
            };

            for (node_id, ws_url, token) in targets {
                let ok = probe_peer(&ws_url, token.as_deref()).await;
                let (edge, last_seen_secs) = {
                    let mut map = liveness.write().await;
                    let e = map.entry(node_id.clone()).or_default();
                    let (m, dark, edge) = beacon_step(e.misses, e.dark, ok, threshold);
                    e.misses = m;
                    e.dark = dark;
                    if ok { e.last_ok = Some(Instant::now()); }
                    let secs = e.last_ok.map(|t| t.elapsed().as_secs()).unwrap_or(0);
                    (edge, secs)
                };

                match edge {
                    Some(BeaconEdge::WentDark) => {
                        eprintln!("[beacon] {node_id} went DARK (last seen {last_seen_secs}s ago)");
                        bus.emit(Event::MeshNodeStatus {
                            node_id: node_id.clone(), status: "dark".into(), last_seen_secs,
                        }).await;
                        if notify_agent {
                            let text = format!(
                                "⚠️ Mesh node **{node_id}** has gone DARK — no heartbeat for ~{last_seen_secs}s \
                                 (missed {threshold} probes). It may have lost power, crashed, or dropped off the \
                                 LAN. If it's a sensor node, that's a monitoring blind spot right now — assess and \
                                 flag André if it matters."
                            );
                            bus.emit(Event::UserPrompt { session: SessionId(0), text, images: vec![] }).await;
                        }
                    }
                    Some(BeaconEdge::Recovered) => {
                        eprintln!("[beacon] {node_id} RECOVERED");
                        bus.emit(Event::MeshNodeStatus {
                            node_id: node_id.clone(), status: "alive".into(), last_seen_secs: 0,
                        }).await;
                        if notify_agent {
                            let text = format!("✓ Mesh node **{node_id}** is back ONLINE — heartbeat restored.");
                            bus.emit(Event::UserPrompt { session: SessionId(0), text, images: vec![] }).await;
                        }
                    }
                    None => {}
                }
            }

            ticker.tick().await;
        }
    });
}

/// The liveness fields a peer-listing handler folds in: `("alive"|"dark", secs)`.
/// Unknown (never probed) reports alive/0 — a freshly-added peer isn't "dark" until
/// the beacon has actually missed it.
pub async fn peer_liveness(liveness: &LivenessMap, node_id: &str) -> (&'static str, u64) {
    let map = liveness.read().await;
    match map.get(node_id) {
        Some(l) if l.dark => ("dark", l.last_ok.map(|t| t.elapsed().as_secs()).unwrap_or(0)),
        Some(l)           => ("alive", l.last_ok.map(|t| t.elapsed().as_secs()).unwrap_or(0)),
        None              => ("alive", 0),
    }
}

/// Convenience for boot: a fresh empty liveness map.
pub fn new_liveness_map() -> LivenessMap {
    Arc::new(RwLock::new(HashMap::new()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ok_while_healthy_is_quiet() {
        assert_eq!(beacon_step(0, false, true, 3), (0, false, None));
    }

    #[test]
    fn misses_below_threshold_dont_alert() {
        assert_eq!(beacon_step(0, false, false, 3), (1, false, None));
        assert_eq!(beacon_step(1, false, false, 3), (2, false, None));
    }

    #[test]
    fn threshold_miss_goes_dark_once() {
        assert_eq!(beacon_step(2, false, false, 3), (3, true, Some(BeaconEdge::WentDark)));
        // already dark → keep counting, but no repeated edge
        assert_eq!(beacon_step(3, true, false, 3), (4, true, None));
    }

    #[test]
    fn success_while_dark_recovers_once() {
        assert_eq!(beacon_step(5, true, true, 3), (0, false, Some(BeaconEdge::Recovered)));
        // and a healthy success after recovery is quiet
        assert_eq!(beacon_step(0, false, true, 3), (0, false, None));
    }

    #[test]
    fn threshold_of_one_darks_on_first_miss() {
        assert_eq!(beacon_step(0, false, false, 1), (1, true, Some(BeaconEdge::WentDark)));
    }
}
