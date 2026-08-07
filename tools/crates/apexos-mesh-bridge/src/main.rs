//! The bridge daemon: own the UART to the brainstem, keep the link alive
//! through every fault (reconnect FSM, v2 §4.2), and expose the MUST-6
//! counters. Inbound frames are logged compactly for now — the mesh router
//! (P5) becomes their consumer; the crypto envelope opens there, never here.
//!
//! Env:
//!   MESH_BRIDGE_DEV         serial device (e.g. /dev/ttyUSB0, or a PTY) — required
//!   MESH_BRIDGE_BAUD        default 115200
//!   MESH_BRIDGE_STATS_SECS  stats JSON line to stderr every N seconds (default 30)

use std::sync::Arc;
use std::time::Duration;

use apexos_mesh_bridge::{backoff_ms, run_link, LinkExit, LinkStats};
use tokio::sync::mpsc;
use tokio_serial::SerialPortBuilderExt;

fn main() {
    let dev = match std::env::var("MESH_BRIDGE_DEV") {
        Ok(d) if !d.is_empty() => d,
        _ => {
            eprintln!(
                "[apexos-mesh-bridge] MESH_BRIDGE_DEV is not set — nothing to bridge.\n\
                 Point it at the brainstem UART (P4) or a PTY from apexos-brainstem-sim / socat."
            );
            std::process::exit(2);
        }
    };
    let baud: u32 = std::env::var("MESH_BRIDGE_BAUD")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(115_200);
    let stats_secs: u64 = std::env::var("MESH_BRIDGE_STATS_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(30)
        .max(5);

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    rt.block_on(daemon(dev, baud, stats_secs));
}

async fn daemon(dev: String, baud: u32, stats_secs: u64) {
    let stats = Arc::new(LinkStats::default());

    // Periodic stats line — the P3 telemetry surface (P5 wires it further).
    {
        let stats = stats.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(stats_secs)).await;
                eprintln!("[apexos-mesh-bridge] stats {}", stats.snapshot());
            }
        });
    }

    // Inbound consumer: compact header log until the P5 router takes over.
    let (inbound_tx, mut inbound_rx) = mpsc::channel(256);
    tokio::spawn(async move {
        while let Some(f) = inbound_rx.recv().await {
            let frame: apexos_mesh_proto::MeshFrame = f;
            eprintln!(
                "[apexos-mesh-bridge] rx ver={} class={:?} sender={} ctr={} ct={}B",
                frame.ver,
                frame.class,
                frame.sender,
                frame.ctr,
                frame.ct.len()
            );
        }
    });

    // The reconnect FSM: open → run_link → classify exit → backoff → again.
    // A link that lived ≥5 s (or moved a frame) resets the backoff ladder so
    // a flapping port can't stay hot forever while a healthy one reconnects
    // instantly after a one-off.
    let mut attempt: u32 = 0;
    loop {
        let stream = match tokio_serial::new(&dev, baud).open_native_async() {
            Ok(s) => s,
            Err(e) => {
                let wait = backoff_ms(attempt);
                eprintln!("[apexos-mesh-bridge] open {dev}: {e} — retry in {wait} ms");
                attempt = attempt.saturating_add(1);
                tokio::time::sleep(Duration::from_millis(wait)).await;
                continue;
            }
        };
        eprintln!("[apexos-mesh-bridge] link up on {dev} @ {baud}");
        // The TX side has no producer until the P5 router — hold the sender so
        // the link doesn't see a closed channel and exit.
        let (_tx_handle, tx_rx) = mpsc::channel::<apexos_mesh_proto::MeshFrame>(64);
        let started = std::time::Instant::now();
        let rx_before = stats.rx_frames.load(std::sync::atomic::Ordering::Relaxed);
        let exit = run_link(stream, tx_rx, inbound_tx.clone(), stats.clone()).await;
        stats
            .link_downs
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let moved_frames = stats.rx_frames.load(std::sync::atomic::Ordering::Relaxed) > rx_before;
        if started.elapsed() >= Duration::from_secs(5) || moved_frames {
            attempt = 0;
        }
        let wait = backoff_ms(attempt);
        match exit {
            LinkExit::Eof => eprintln!(
                "[apexos-mesh-bridge] link down (EOF — port/peer vanished) — retry in {wait} ms"
            ),
            LinkExit::Io(e) => {
                eprintln!("[apexos-mesh-bridge] link error: {e} — retry in {wait} ms")
            }
            LinkExit::Closed => {
                eprintln!("[apexos-mesh-bridge] consumer closed — shutting down");
                return;
            }
        }
        attempt = attempt.saturating_add(1);
        tokio::time::sleep(Duration::from_millis(wait)).await;
    }
}
