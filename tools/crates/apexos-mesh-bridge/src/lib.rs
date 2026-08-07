//! # apexos-mesh-bridge
//!
//! ApexNET P3 (`docs/apexnet.md` §9): the Pi side of the UART to the ESP32
//! brainstem. Owns the serial link, runs the frozen frame codec
//! (`apexos-mesh-proto`), and survives every fault a hostile line can throw
//! (the v2 §4.3 MUSTs, each locked by a harness test).
//!
//! Mirrors the `apex-sensor-bridge` posture — a small standalone daemon —
//! but async: reads and writes race on one `select!` loop, and the link
//! engine is generic over `AsyncRead + AsyncWrite` so the fault harness
//! drives it over in-memory duplexes and PTY pairs with zero hardware.
//!
//! Deliberately **PSK-free**: the bridge treats `MeshFrame.ct` as opaque
//! bytes. The crypto envelope opens at the mesh router (P5) — a compromised
//! bridge process never holds the colony key.
//!
//! Not deployed by install.sh yet — that switch flips in P4 when a brainstem
//! exists to talk to. Until then: `apexos-brainstem-sim` + a PTY pair is the
//! whole bench (`SerialStream::pair()` in tests; `socat -d -d pty,raw,echo=0
//! pty,raw,echo=0` for a manual rig).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use apexos_mesh_proto::{encode_frame, Deframer, MeshFrame};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc;

/// Live link counters (the deframer's RX set + the bridge's TX/link set —
/// v2 §4.3 MUST-6). Atomics so the daemon's stats logger and the harness
/// read them while the link runs.
#[derive(Debug, Default)]
pub struct LinkStats {
    pub rx_frames: AtomicU64,
    pub tx_frames: AtomicU64,
    pub crc_fail: AtomicU64,
    pub decode_fail: AtomicU64,
    pub resyncs: AtomicU64,
    pub oversize_drops: AtomicU64,
    pub link_downs: AtomicU64,
}

impl LinkStats {
    pub fn snapshot(&self) -> serde_json::Value {
        serde_json::json!({
            "rx_frames": self.rx_frames.load(Ordering::Relaxed),
            "tx_frames": self.tx_frames.load(Ordering::Relaxed),
            "crc_fail": self.crc_fail.load(Ordering::Relaxed),
            "decode_fail": self.decode_fail.load(Ordering::Relaxed),
            "resyncs": self.resyncs.load(Ordering::Relaxed),
            "oversize_drops": self.oversize_drops.load(Ordering::Relaxed),
            "link_downs": self.link_downs.load(Ordering::Relaxed),
        })
    }

    /// Fold one link's deframer-counter DELTAS into the process-lifetime
    /// totals (a fresh deframer per link starts at zero — deltas keep the
    /// totals monotonic across reconnects).
    fn absorb_deframer_delta(
        &self,
        current: &apexos_mesh_proto::DeframerStats,
        last: &mut apexos_mesh_proto::DeframerStats,
    ) {
        self.crc_fail
            .fetch_add(current.crc_fail - last.crc_fail, Ordering::Relaxed);
        self.decode_fail
            .fetch_add(current.decode_fail - last.decode_fail, Ordering::Relaxed);
        self.resyncs
            .fetch_add(current.resyncs - last.resyncs, Ordering::Relaxed);
        self.oversize_drops.fetch_add(
            current.oversize_drops - last.oversize_drops,
            Ordering::Relaxed,
        );
        *last = *current;
    }
}

/// Why a link ended. `Eof` = the port/peer vanished (v2 §4.2: `read() == 0`
/// is a LINK-DOWN event, never "no data") — the FSM reconnects. `Io` =
/// driver/port error — also link-down. `Closed` = our own consumer/producer
/// went away (shutdown).
#[derive(Debug)]
pub enum LinkExit {
    Eof,
    Io(std::io::Error),
    Closed,
}

/// Drive one link until it dies: decode inbound bytes into frames (draining
/// EVERY complete frame per wakeup — MUST-4), write outbound frames from
/// `tx`. Generic so the harness runs it over duplexes and PTYs.
///
/// A fresh [`Deframer`] per link: a new physical connection never inherits a
/// half-parsed buffer from the old one.
pub async fn run_link<S>(
    stream: S,
    mut tx: mpsc::Receiver<MeshFrame>,
    inbound: mpsc::Sender<MeshFrame>,
    stats: Arc<LinkStats>,
) -> LinkExit
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let (mut rd, mut wr) = tokio::io::split(stream);
    let mut deframer = Deframer::new();
    let mut last_dstats = apexos_mesh_proto::DeframerStats::default();
    let mut buf = [0u8; 4096];

    loop {
        tokio::select! {
            read = rd.read(&mut buf) => {
                match read {
                    Ok(0) => return LinkExit::Eof, // MUST-3: EOF = link-down
                    Ok(n) => {
                        let frames = deframer.push(&buf[..n]);
                        stats.absorb_deframer_delta(&deframer.stats.clone(), &mut last_dstats);
                        for frame in frames {
                            stats.rx_frames.fetch_add(1, Ordering::Relaxed);
                            if inbound.send(frame).await.is_err() {
                                return LinkExit::Closed;
                            }
                        }
                    }
                    Err(e) => return LinkExit::Io(e),
                }
            }
            out = tx.recv() => {
                match out {
                    Some(frame) => {
                        let wire = match encode_frame(&frame) {
                            Ok(w) => w,
                            Err(e) => {
                                // An oversize/unencodable frame is the caller's
                                // bug — drop it loudly, never wedge the link.
                                eprintln!("[apexos-mesh-bridge] tx frame dropped: {e}");
                                continue;
                            }
                        };
                        if let Err(e) = wr.write_all(&wire).await {
                            return LinkExit::Io(e);
                        }
                        if let Err(e) = wr.flush().await {
                            return LinkExit::Io(e);
                        }
                        stats.tx_frames.fetch_add(1, Ordering::Relaxed);
                    }
                    None => return LinkExit::Closed,
                }
            }
        }
    }
}

/// Reconnect backoff (v2 §4.2): 250 ms doubling to a 5 s cap.
pub fn backoff_ms(attempt: u32) -> u64 {
    250u64.saturating_mul(1u64 << attempt.min(5)).min(5_000)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_walks_250ms_to_a_5s_cap() {
        assert_eq!(backoff_ms(0), 250);
        assert_eq!(backoff_ms(1), 500);
        assert_eq!(backoff_ms(2), 1000);
        assert_eq!(backoff_ms(3), 2000);
        assert_eq!(backoff_ms(4), 4000);
        assert_eq!(backoff_ms(5), 5000);
        assert_eq!(backoff_ms(6), 5000);
        assert_eq!(backoff_ms(u32::MAX), 5000);
    }
}
