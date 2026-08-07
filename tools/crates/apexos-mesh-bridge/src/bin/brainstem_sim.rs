//! apexos-brainstem-sim — a fake ESP32 brainstem for the hardware-free bench
//! (ApexNET P3). Speaks the real wire (apexos-mesh-proto frames) over any
//! serial device or PTY, with optional fault injection so the bridge's
//! resilience can be watched live.
//!
//!   # a PTY pair, no hardware:
//!   socat -d -d pty,raw,echo=0 pty,raw,echo=0
//!   MESH_BRIDGE_DEV=/dev/pts/A apexos-mesh-bridge &
//!   apexos-brainstem-sim /dev/pts/B --interval-ms=500 --fault=garbage
//!
//! Faults (--fault=): none | garbage (random bytes between frames) |
//! bitflip (corrupt ~1 in 4 frames) | truncate (cut ~1 in 4 frames) |
//! flood (bursts of 0x00 delimiters) | giant (periodic >4 KB delimiter-free
//! blobs). Every fault mode still emits good frames — the bridge must keep
//! decoding them through the noise; watch its stats line tell the story.

use std::time::Duration;

use apexos_mesh_proto::{encode_frame, Deframer, MeshClass, MeshFrame, Payload, PlainPacket};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_serial::SerialPortBuilderExt;

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
}

fn heartbeat(ctr: u64, uptime_s: u32) -> MeshFrame {
    // The sim seals nothing — the bridge treats ct as opaque, and the P4
    // brainstem owns real crypto. postcard(PlainPacket) UNSEALED stands in.
    let packet = PlainPacket {
        target: apexos_mesh_proto::BROADCAST,
        hop_limit: 1,
        flags: 0,
        payload: Payload::Heartbeat {
            uptime_s,
            cortex_up: false,
            conn: 3,
        },
    };
    MeshFrame {
        ver: apexos_mesh_proto::WIRE_VERSION,
        class: MeshClass::Gossip,
        sender: 999,
        ctr,
        ct: postcard::to_allocvec(&packet).unwrap_or_default(),
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let mut args = std::env::args().skip(1);
    let dev = match args.next() {
        Some(d) => d,
        None => {
            eprintln!("usage: apexos-brainstem-sim <serial-or-pty> [--interval-ms=N] [--fault=MODE] [--count=N]");
            std::process::exit(2);
        }
    };
    let mut interval_ms: u64 = 1000;
    let mut fault = "none".to_string();
    let mut count: u64 = 0; // 0 = forever
    for a in args {
        if let Some(v) = a.strip_prefix("--interval-ms=") {
            interval_ms = v.parse().unwrap_or(1000);
        } else if let Some(v) = a.strip_prefix("--fault=") {
            fault = v.to_string();
        } else if let Some(v) = a.strip_prefix("--count=") {
            count = v.parse().unwrap_or(0);
        }
    }

    let mut port = match tokio_serial::new(&dev, 115_200).open_native_async() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("[brainstem-sim] open {dev}: {e}");
            std::process::exit(1);
        }
    };
    eprintln!("[brainstem-sim] up on {dev} — fault={fault}, interval={interval_ms}ms");

    let mut rng = Rng(0xB0A7_5EED_0000_0001);
    let mut deframer = Deframer::new();
    let mut buf = [0u8; 1024];
    let mut ctr: u64 = 0;
    let started = std::time::Instant::now();

    loop {
        ctr += 1;
        if count > 0 && ctr > count {
            eprintln!(
                "[brainstem-sim] done ({count} frames) — rx stats {:?}",
                deframer.stats
            );
            return;
        }
        let mut wire = Vec::new();

        // Fault chaos BEFORE the good frame — always delimiter-terminated, so
        // corruption costs itself, never the neighbor (the COBS resync law).
        match fault.as_str() {
            "garbage" => {
                let n = (rng.next() % 120) as usize;
                for _ in 0..n {
                    wire.push((rng.next() % 256) as u8);
                }
                wire.push(0x00);
            }
            "flood" => {
                wire.extend(std::iter::repeat_n(0x00, (rng.next() % 32) as usize));
            }
            "giant" if ctr.is_multiple_of(10) => {
                wire.extend(std::iter::repeat_n(0x55, 6000)); // > MAX_WIRE_FRAME, no delimiter
                wire.push(0x00);
            }
            _ => {}
        }

        let mut frame_wire = encode_frame(&heartbeat(ctr, started.elapsed().as_secs() as u32))
            .expect("heartbeat encodes");
        match fault.as_str() {
            "bitflip" if ctr.is_multiple_of(4) => {
                let pos = (rng.next() as usize) % (frame_wire.len() - 1);
                frame_wire[pos] ^= 1 << (rng.next() % 8);
            }
            "truncate" if ctr.is_multiple_of(4) => {
                let cut = frame_wire.len() / 2;
                frame_wire.truncate(cut);
                frame_wire.push(0x00);
            }
            _ => {}
        }
        wire.extend_from_slice(&frame_wire);

        if let Err(e) = port.write_all(&wire).await {
            eprintln!("[brainstem-sim] write: {e} — exiting");
            return;
        }
        let _ = port.flush().await;

        // Drain anything the bridge sends us (its TX path), briefly.
        match tokio::time::timeout(Duration::from_millis(interval_ms), port.read(&mut buf)).await {
            Ok(Ok(0)) => {
                eprintln!("[brainstem-sim] peer EOF — exiting");
                return;
            }
            Ok(Ok(n)) => {
                for f in deframer.push(&buf[..n]) {
                    eprintln!(
                        "[brainstem-sim] rx from bridge: class={:?} sender={} ctr={}",
                        f.class, f.sender, f.ctr
                    );
                }
            }
            Ok(Err(e)) => {
                eprintln!("[brainstem-sim] read: {e} — exiting");
                return;
            }
            Err(_) => {} // timeout = the pacing sleep
        }
    }
}
