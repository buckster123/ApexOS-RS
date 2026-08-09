//! The bridge daemon: own the UART to the brainstem, keep the link alive
//! through every fault (reconnect FSM, v2 §4.2), and expose the MUST-6
//! counters. Inbound frames are logged compactly for now — the mesh router
//! (P5) becomes their consumer; the crypto envelope opens there, never here.
//!
//! Env:
//!   MESH_BRIDGE_DEV         serial device (e.g. /dev/ttyUSB0, or a PTY) — required
//!   MESH_BRIDGE_BAUD        default 115200
//!   MESH_BRIDGE_STATS_SECS  stats JSON line to stderr every N seconds (default 30)
//!   AGENTD_MESH_WS          cortex link, e.g. ws://127.0.0.1:8787/mesh-bridge
//!                           (unset = standalone: frames are logged, not forwarded)
//!   MESH_BRIDGE_TOKEN       bearer token for that socket
//!
//! ## The cortex link (P5c)
//!
//! The bridge **dials agentd**, mirroring `apex-sensor-bridge`. agentd never
//! holds the serial port, so the bridge can be restarted or replaced under a
//! running daemon, and a node with no radio hardware simply never has one
//! connect — which reads as "lane down" rather than an error.
//!
//! Frames cross that socket as raw datagram bytes in binary messages: both
//! ends link the same wire crate, and wrapping them in a second format would
//! be two contracts where one will do.

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

    // Cortex link. Absent (AGENTD_MESH_WS unset) the bridge stays useful on
    // its own: frames are logged, which is how the P3 bench worked and how a
    // board is debugged without a daemon.
    let cortex_ws = std::env::var("AGENTD_MESH_WS").ok().filter(|s| !s.is_empty());
    // Cortex → radio. Broadcast so each reconnected link can subscribe
    // afresh without the cortex needing to know a link was replaced.
    let (to_radio, _) = tokio::sync::broadcast::channel::<apexos_mesh_proto::MeshFrame>(64);

    // Radio → cortex.
    let (inbound_tx, mut inbound_rx) = mpsc::channel(256);
    {
        let cortex_ws = cortex_ws.clone();
        let to_radio = to_radio.clone();
        tokio::spawn(async move {
            match cortex_ws {
                Some(url) => cortex_link(url, &mut inbound_rx, to_radio).await,
                None => {
                    while let Some(frame) = inbound_rx.recv().await {
                        eprintln!(
                            "[apexos-mesh-bridge] rx ver={} class={:?} sender={} ctr={} ct={}B",
                            frame.ver, frame.class, frame.sender, frame.ctr, frame.ct.len()
                        );
                    }
                }
            }
        });
    }

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
        // Pump the cortex's outbound frames into this link. A fresh pair per
        // link keeps `run_link` unchanged (law: a fresh deframer and a fresh
        // channel per link — a reconnect must inherit nothing).
        let (tx_handle, tx_rx) = mpsc::channel::<apexos_mesh_proto::MeshFrame>(64);
        let mut from_cortex = to_radio.subscribe();
        let pump = tokio::spawn(async move {
            while let Ok(frame) = from_cortex.recv().await {
                if tx_handle.send(frame).await.is_err() {
                    break;
                }
            }
        });
        let started = std::time::Instant::now();
        let rx_before = stats.rx_frames.load(std::sync::atomic::Ordering::Relaxed);
        let exit = run_link(stream, tx_rx, inbound_tx.clone(), stats.clone()).await;
        pump.abort();
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

/// Keep a socket to agentd up, forwarding frames both ways.
///
/// Reconnects forever with the same backoff ladder as the serial link: a
/// cortex that is restarting, updating, or simply not there yet is a normal
/// condition, not a reason for the bridge to give up and leave the radio
/// unattended.
async fn cortex_link(
    url: String,
    inbound: &mut mpsc::Receiver<apexos_mesh_proto::MeshFrame>,
    to_radio: tokio::sync::broadcast::Sender<apexos_mesh_proto::MeshFrame>,
) {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    use tokio_tungstenite::tungstenite::Message;

    let token = std::env::var("MESH_BRIDGE_TOKEN").unwrap_or_default();
    let mut attempt: u32 = 0;

    loop {
        let mut request = match url.as_str().into_client_request() {
            Ok(r) => r,
            Err(e) => {
                eprintln!("[apexos-mesh-bridge] bad AGENTD_MESH_WS {url}: {e}");
                return;
            }
        };
        if !token.is_empty() {
            // Header, not a query param: the token stays out of URLs and
            // therefore out of logs (the lesson from #194).
            if let Ok(v) = format!("Bearer {token}").parse() {
                request.headers_mut().insert("Authorization", v);
            }
        }

        match tokio_tungstenite::connect_async(request).await {
            Ok((socket, _)) => {
                eprintln!("[apexos-mesh-bridge] cortex link up: {url}");
                attempt = 0;
                let (mut sink, mut stream) = socket.split();
                loop {
                    tokio::select! {
                        // Radio → cortex.
                        frame = inbound.recv() => {
                            let Some(frame) = frame else { return };
                            let Ok(bytes) = apexos_mesh_proto::encode_datagram(&frame) else { continue };
                            if sink.send(Message::Binary(bytes.into())).await.is_err() {
                                break;
                            }
                        }
                        // Cortex → radio.
                        msg = stream.next() => {
                            match msg {
                                Some(Ok(Message::Binary(bytes))) => {
                                    match apexos_mesh_proto::decode_datagram(&bytes) {
                                        // A closed broadcast means no link is
                                        // currently up; the frame is dropped,
                                        // which is what a down radio means.
                                        Ok(frame) => { let _ = to_radio.send(frame); }
                                        Err(e) => eprintln!("[apexos-mesh-bridge] cortex sent an undecodable frame: {e}"),
                                    }
                                }
                                Some(Ok(_)) => {}
                                _ => break,
                            }
                        }
                    }
                }
                eprintln!("[apexos-mesh-bridge] cortex link down");
            }
            Err(e) => {
                let wait = backoff_ms(attempt);
                eprintln!("[apexos-mesh-bridge] cortex link {url}: {e} — retry in {wait} ms");
                attempt = attempt.saturating_add(1);
                tokio::time::sleep(Duration::from_millis(wait)).await;
            }
        }
    }
}
