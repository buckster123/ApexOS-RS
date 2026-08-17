//! Process-global mesh hub (ApexNET P5d).
//!
//! The router lives in this crate; the real lanes (`WifiLan`, `BleGossip`,
//! `Courier`) are assembled by agentd and [`install`]ed once at boot. Tools
//! in `apexos-plugins` and HTTP handlers in the gateway then ask the hub
//! which lane to use instead of each talking HTTP on their own.
//!
//! A missing hub (unit tests, a raw `cargo test` of a crate) is not an
//! error: [`route`] falls back to "WifiLan only", which is today's mesh.

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::OnceLock;

use tokio::sync::Mutex;

use apexos_mesh_proto::{MeshClass, MeshFrame};

use crate::mesh_router::{RouteOutcome, Router, TransportHealth, TransportId};

static HUB: OnceLock<Mutex<Router>> = OnceLock::new();

/// 0 = Down, 1 = Flaky, 2 = Up. Boot Up so an absent watcher cannot hide
/// the LAN (same no-regression rule as [`crate::connectivity`]).
static WIFI: AtomicU8 = AtomicU8::new(2);

pub fn install(router: Router) {
    if HUB.set(Mutex::new(router)).is_err() {
        eprintln!("[mesh-lanes] hub already installed — second install ignored");
    }
}

pub fn installed() -> bool {
    HUB.get().is_some()
}

pub fn set_wifi_health(h: TransportHealth) {
    let v = match h {
        TransportHealth::Down => 0,
        TransportHealth::Flaky => 1,
        TransportHealth::Up => 2,
    };
    WIFI.store(v, Ordering::Relaxed);
}

pub fn wifi_health() -> TransportHealth {
    match WIFI.load(Ordering::Relaxed) {
        0 => TransportHealth::Down,
        1 => TransportHealth::Flaky,
        _ => TransportHealth::Up,
    }
}

pub async fn route(class: MeshClass, frame_len: usize) -> Vec<TransportId> {
    match HUB.get() {
        Some(hub) => hub.lock().await.route(class, frame_len),
        None => vec![TransportId::WifiLan],
    }
}

pub async fn send(class: MeshClass, frame: &MeshFrame) -> RouteOutcome {
    match HUB.get() {
        Some(hub) => hub.lock().await.send(class, frame).await,
        None => RouteOutcome {
            sent: Vec::new(),
            failed: vec![(
                TransportId::WifiLan,
                crate::mesh_router::SendError::Unavailable,
            )],
        },
    }
}

pub async fn health() -> Vec<(TransportId, TransportHealth)> {
    match HUB.get() {
        Some(hub) => hub.lock().await.health(),
        None => vec![(TransportId::WifiLan, wifi_health())],
    }
}
