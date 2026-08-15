//! The P3 fault harness (charter §9: "survives every fault injector over
//! PTYs"). Every v2 §4.3 MUST is a test here; the logic faults run over
//! in-memory duplexes, and the serial path itself is proven over REAL PTY
//! pairs (`SerialStream::pair()` — same tty machinery a socat rig uses, no
//! external tooling needed).

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use apexos_mesh_bridge::{run_link, LinkExit, LinkStats};
use apexos_mesh_proto::{encode_frame, Deframer, MeshClass, MeshFrame};
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;

fn frame(ctr: u64) -> MeshFrame {
    MeshFrame {
        ver: 1,
        class: MeshClass::Gossip,
        sender: 7,
        ctr,
        ct: vec![0xAB; 24],
    }
}

/// Spin up a link over an in-memory duplex. Returns (peer endpoint, tx to
/// the link, inbound from the link, stats, join handle).
#[allow(clippy::type_complexity)]
fn link_over_duplex() -> (
    tokio::io::DuplexStream,
    mpsc::Sender<MeshFrame>,
    mpsc::Receiver<MeshFrame>,
    Arc<LinkStats>,
    tokio::task::JoinHandle<LinkExit>,
) {
    let (ours, theirs) = tokio::io::duplex(64 * 1024);
    let (tx, tx_rx) = mpsc::channel(16);
    let (inbound_tx, inbound_rx) = mpsc::channel(256);
    let stats = Arc::new(LinkStats::default());
    let handle = tokio::spawn(run_link(ours, tx_rx, inbound_tx, stats.clone()));
    (theirs, tx, inbound_rx, stats, handle)
}

async fn recv_n(inbound: &mut mpsc::Receiver<MeshFrame>, n: usize) -> Vec<MeshFrame> {
    let mut out = Vec::new();
    for _ in 0..n {
        let f = tokio::time::timeout(Duration::from_secs(5), inbound.recv())
            .await
            .expect("timed out waiting for a frame")
            .expect("link closed early");
        out.push(f);
    }
    out
}

// ── MUST-4: drain every buffered frame per wakeup ───────────────────────────

#[tokio::test]
async fn burst_of_frames_in_one_write_all_arrive() {
    let (mut peer, _tx, mut inbound, stats, _h) = link_over_duplex();
    let mut burst = Vec::new();
    for ctr in 1..=5 {
        burst.extend_from_slice(&encode_frame(&frame(ctr)).unwrap());
    }
    peer.write_all(&burst).await.unwrap();
    let got = recv_n(&mut inbound, 5).await;
    assert_eq!(
        got.iter().map(|f| f.ctr).collect::<Vec<_>>(),
        vec![1, 2, 3, 4, 5]
    );
    assert_eq!(stats.rx_frames.load(Ordering::Relaxed), 5);
}

// ── MUST-2 + MUST-5: poison frames drop, the stream continues ───────────────

#[tokio::test]
async fn bit_flip_costs_one_frame_never_the_stream() {
    let (mut peer, _tx, mut inbound, stats, _h) = link_over_duplex();
    let good = encode_frame(&frame(1)).unwrap();
    let mut flipped = encode_frame(&frame(2)).unwrap();
    let mid = flipped.len() / 2;
    flipped[mid] ^= 0x10;
    let after = encode_frame(&frame(3)).unwrap();
    peer.write_all(&good).await.unwrap();
    peer.write_all(&flipped).await.unwrap();
    peer.write_all(&after).await.unwrap();
    let got = recv_n(&mut inbound, 2).await;
    assert_eq!(got.iter().map(|f| f.ctr).collect::<Vec<_>>(), vec![1, 3]);
    assert_eq!(
        stats.crc_fail.load(Ordering::Relaxed) + stats.decode_fail.load(Ordering::Relaxed),
        1
    );
}

#[tokio::test]
async fn truncated_frame_recovers_at_the_next_delimiter() {
    let (mut peer, _tx, mut inbound, stats, _h) = link_over_duplex();
    let wire = encode_frame(&frame(1)).unwrap();
    peer.write_all(&wire[..wire.len() / 2]).await.unwrap();
    peer.write_all(&[0x00]).await.unwrap(); // the cut ends here
    peer.write_all(&encode_frame(&frame(2)).unwrap())
        .await
        .unwrap();
    let got = recv_n(&mut inbound, 1).await;
    assert_eq!(got[0].ctr, 2);
    assert_eq!(
        stats.crc_fail.load(Ordering::Relaxed) + stats.decode_fail.load(Ordering::Relaxed),
        1
    );
}

// ── MUST-1: a giant delimiter-free blob can't buffer unbounded ──────────────

#[tokio::test]
async fn giant_blob_is_bounded_and_the_link_resyncs() {
    let (mut peer, _tx, mut inbound, stats, _h) = link_over_duplex();
    peer.write_all(&vec![0x55u8; 20_000]).await.unwrap(); // no delimiter anywhere
    peer.write_all(&[0x00]).await.unwrap(); // chaos ends
    peer.write_all(&encode_frame(&frame(9)).unwrap())
        .await
        .unwrap();
    let got = recv_n(&mut inbound, 1).await;
    assert_eq!(got[0].ctr, 9);
    assert!(stats.oversize_drops.load(Ordering::Relaxed) >= 1);
    assert!(stats.resyncs.load(Ordering::Relaxed) >= 1);
}

#[tokio::test]
async fn delimiter_floods_are_free() {
    let (mut peer, _tx, mut inbound, stats, _h) = link_over_duplex();
    peer.write_all(&vec![0x00u8; 2_000]).await.unwrap();
    peer.write_all(&encode_frame(&frame(1)).unwrap())
        .await
        .unwrap();
    let got = recv_n(&mut inbound, 1).await;
    assert_eq!(got[0].ctr, 1);
    assert_eq!(stats.crc_fail.load(Ordering::Relaxed), 0);
    assert_eq!(stats.decode_fail.load(Ordering::Relaxed), 0);
}

// ── MUST-3: EOF is a link-down event, never "no data" ───────────────────────

#[tokio::test]
async fn peer_disconnect_mid_frame_is_link_down() {
    let (mut peer, _tx, _inbound, _stats, h) = link_over_duplex();
    let wire = encode_frame(&frame(1)).unwrap();
    peer.write_all(&wire[..wire.len() / 3]).await.unwrap(); // mid-frame…
    drop(peer); // …the port vanishes
    let exit = tokio::time::timeout(Duration::from_secs(5), h)
        .await
        .expect("link did not notice the disconnect")
        .unwrap();
    assert!(matches!(exit, LinkExit::Eof), "expected Eof, got {exit:?}");
}

#[tokio::test]
async fn fresh_link_after_disconnect_starts_clean() {
    // A reconnect must never inherit the old link's half-parsed buffer.
    let (mut peer, _tx, _inbound, _stats, h) = link_over_duplex();
    let wire = encode_frame(&frame(1)).unwrap();
    peer.write_all(&wire[..wire.len() / 2]).await.unwrap();
    drop(peer);
    let _ = tokio::time::timeout(Duration::from_secs(5), h)
        .await
        .unwrap();

    // "Reconnect": a brand-new link (fresh deframer by construction).
    let (mut peer2, _tx2, mut inbound2, stats2, _h2) = link_over_duplex();
    peer2
        .write_all(&encode_frame(&frame(2)).unwrap())
        .await
        .unwrap();
    let got = recv_n(&mut inbound2, 1).await;
    assert_eq!(got[0].ctr, 2);
    assert_eq!(stats2.decode_fail.load(Ordering::Relaxed), 0); // no ghost of the old half-frame
}

// ── The TX path ─────────────────────────────────────────────────────────────

#[tokio::test]
async fn tx_frames_encode_and_the_peer_deframes_them() {
    let (peer, tx, _inbound, stats, _h) = link_over_duplex();
    tx.send(frame(41)).await.unwrap();
    tx.send(frame(42)).await.unwrap();
    let mut deframer = Deframer::new();
    let mut got = Vec::new();
    let mut peer = peer;
    let mut buf = [0u8; 4096];
    while got.len() < 2 {
        let n = tokio::time::timeout(
            Duration::from_secs(5),
            tokio::io::AsyncReadExt::read(&mut peer, &mut buf),
        )
        .await
        .expect("timed out reading tx")
        .unwrap();
        got.extend(deframer.push(&buf[..n]));
    }
    assert_eq!(got.iter().map(|f| f.ctr).collect::<Vec<_>>(), vec![41, 42]);
    assert_eq!(stats.tx_frames.load(Ordering::Relaxed), 2);
}

#[tokio::test]
async fn oversize_tx_frame_is_dropped_loudly_not_wedging() {
    let (mut peer, tx, mut inbound, stats, _h) = link_over_duplex();
    // Unencodable (too large) — must be dropped, and the link must live on.
    tx.send(MeshFrame {
        ver: 1,
        class: MeshClass::Bulk,
        sender: 1,
        ctr: 1,
        ct: vec![0xAA; apexos_mesh_proto::MAX_WIRE_FRAME],
    })
    .await
    .unwrap();
    tx.send(frame(2)).await.unwrap();
    // The good frame still crosses; the giant one never counted.
    let mut deframer = Deframer::new();
    let mut buf = [0u8; 4096];
    let mut seen = Vec::new();
    while seen.is_empty() {
        let n = tokio::time::timeout(
            Duration::from_secs(5),
            tokio::io::AsyncReadExt::read(&mut peer, &mut buf),
        )
        .await
        .expect("timed out")
        .unwrap();
        seen.extend(deframer.push(&buf[..n]));
    }
    assert_eq!(seen[0].ctr, 2);
    assert_eq!(stats.tx_frames.load(Ordering::Relaxed), 1);
    // And inbound still works after the drop.
    peer.write_all(&encode_frame(&frame(3)).unwrap())
        .await
        .unwrap();
    assert_eq!(recv_n(&mut inbound, 1).await[0].ctr, 3);
}

// ── The hardware golden vector (ApexNET P4a) ────────────────────────────────

/// A REAL heartbeat, captured off an ESP32-S3 running `firmware/brainstem`
/// (2026-08-07, board d0:cf:13:26:69:ec) straight from `/dev/ttyACM1` — one
/// COBS-delimited frame, exactly as the silicon emitted it.
///
/// This is the firmware↔bridge contract pinned to bytes that actually
/// crossed a wire: it runs in CI forever with no hardware, and it fails the
/// day either side's codec drifts. If the wire version bumps, RE-CAPTURE
/// from hardware — never hand-edit this vector.
const HW_HEARTBEAT_FRAME: &str = "0b0101e9071f09ffff030101021e0603c522a975";

fn unhex(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).expect("hex"))
        .collect()
}

#[tokio::test]
async fn hardware_captured_heartbeat_decodes_end_to_end() {
    use apexos_mesh_proto::{MeshClass, Payload, PlainPacket};

    let (mut peer, _tx, mut inbound, stats, _h) = link_over_duplex();
    let mut wire = unhex(HW_HEARTBEAT_FRAME);
    wire.push(0x00); // the delimiter the capture split on
    peer.write_all(&wire).await.unwrap();

    let frames = recv_n(&mut inbound, 1).await;
    let frame = &frames[0];
    // The header the silicon stamped.
    assert_eq!(frame.ver, apexos_mesh_proto::WIRE_VERSION);
    assert_eq!(frame.class, MeshClass::Gossip);
    assert_eq!(frame.sender, 1001, "brainstem NODE_ID");
    assert!(frame.ctr >= 1, "counters mint from 1");

    // The payload interior (unsealed on the USB link by design — the bridge
    // itself stays PSK-free and never looks; the test may).
    let (packet, rest) =
        postcard::take_from_bytes::<PlainPacket>(&frame.ct).expect("firmware packet decodes");
    assert!(rest.is_empty(), "exact-fit — no trailing bytes");
    assert_eq!(packet.target, apexos_mesh_proto::BROADCAST);
    assert_eq!(packet.hop_limit, 1);
    match packet.payload {
        Payload::Heartbeat {
            uptime_s,
            cortex_up,
            conn,
        } => {
            // Captured with no cortex writing to the board, and the firmware
            // reports Isolated (3) until a radio tier says otherwise.
            assert!(!cortex_up, "no cortex was writing during the capture");
            assert_eq!(conn, 3, "brainstem reports Isolated pre-radio");
            assert!(uptime_s < 86_400, "sane uptime, got {uptime_s}");
        }
        other => panic!("expected a Heartbeat from the brainstem, got {other:?}"),
    }
    assert_eq!(stats.rx_frames.load(Ordering::Relaxed), 1);
    assert_eq!(stats.crc_fail.load(Ordering::Relaxed), 0);
}

// ── The real serial path: a PTY pair, the same tty machinery as socat ───────

#[cfg(unix)]
#[tokio::test]
async fn pty_pair_end_to_end_with_garbage_injection() {
    let (bridge_side, mut sim_side) = tokio_serial::SerialStream::pair().expect("pty pair (unix)");
    let (_tx, tx_rx) = mpsc::channel(16);
    let (inbound_tx, mut inbound) = mpsc::channel(256);
    let stats = Arc::new(LinkStats::default());
    let _h = tokio::spawn(run_link(bridge_side, tx_rx, inbound_tx, stats.clone()));

    // Interleave garbage blocks (delimiter-terminated) with real frames —
    // the socat fault rig, in-process.
    let mut rng: u64 = 0x00C0_FFEE;
    for ctr in 1..=50u64 {
        let mut chunk = Vec::new();
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        let junk = (rng % 60) as usize;
        for i in 0..junk {
            chunk.push((rng.wrapping_add(i as u64) % 256) as u8);
        }
        chunk.push(0x00);
        chunk.extend_from_slice(&encode_frame(&frame(ctr)).unwrap());
        sim_side.write_all(&chunk).await.unwrap();
    }
    sim_side.flush().await.unwrap();

    let got = recv_n(&mut inbound, 50).await;
    assert_eq!(
        got.iter().map(|f| f.ctr).collect::<Vec<_>>(),
        (1..=50).collect::<Vec<_>>()
    );
    assert_eq!(stats.rx_frames.load(Ordering::Relaxed), 50);
}

/// The commissioning contract, both directions of the acceptance rule.
///
/// `apexos-brainstem-provision` builds these two frames and the firmware
/// decides on them; this pins the wire shape so the tool and the board cannot
/// drift apart without a test going red. (The *decision* lives in firmware and
/// is verified on hardware; what's checkable here is that each frame carries a
/// `Provision` the board can actually read, and — crucially — that the sealed
/// one does NOT parse as plain postcard, since that is exactly how the
/// firmware tells "authenticated" from "someone on the wire".)
#[test]
fn provision_frames_match_what_the_firmware_expects() {
    use apexos_mesh_proto::{seal, Payload, PlainPacket, Psk, BROADCAST, WIRE_VERSION};

    let psk = [0x5Au8; 32];
    let packet = PlainPacket {
        target: BROADCAST,
        hop_limit: 1,
        flags: 0,
        payload: Payload::Provision {
            node_id: 1001,
            psk,
        },
    };

    // First touch: plain postcard in `ct`, exact fit.
    let plain = MeshFrame {
        ver: WIRE_VERSION,
        class: MeshClass::Critical,
        sender: 1,
        ctr: 1,
        ct: postcard::to_allocvec(&packet).unwrap(),
    };
    let mut deframer = Deframer::new();
    let got = deframer.push(&encode_frame(&plain).unwrap());
    assert_eq!(got.len(), 1);
    let (decoded, rest) =
        postcard::take_from_bytes::<PlainPacket>(&got[0].ct).expect("plain provision decodes");
    assert!(rest.is_empty(), "exact fit — no trailing bytes to smuggle in");
    assert_eq!(decoded.payload, packet.payload);

    // Rotation: sealed under the current key. It must open under that key...
    // First sealed provision still uses ctr=1; later ones increment a
    // persisted file (`take_provision_ctr`) so the AEAD nonce never repeats.
    let sealed = seal(&Psk(psk), MeshClass::Critical, 1, 1, &packet).unwrap();
    let mut deframer = Deframer::new();
    let got = deframer.push(&encode_frame(&sealed).unwrap());
    assert_eq!(got.len(), 1);
    assert_eq!(
        apexos_mesh_proto::open(&Psk(psk), &got[0]).unwrap().payload,
        packet.payload
    );
    // ...and must NOT open under a different one.
    assert!(apexos_mesh_proto::open(&Psk([0x11; 32]), &got[0]).is_err());
    // ...and must not masquerade as an unsealed frame: ciphertext that parsed
    // cleanly as a PlainPacket would let an unauthenticated sender look
    // authenticated to the firmware's `decode_inbound`.
    let as_plain = postcard::take_from_bytes::<PlainPacket>(&got[0].ct);
    assert!(
        match as_plain {
            Err(_) => true,
            Ok((_, rest)) => !rest.is_empty(),
        },
        "sealed ct must not decode as an exact-fit plain packet"
    );
}
