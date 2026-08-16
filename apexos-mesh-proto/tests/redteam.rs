//! The airwaves are hostile (charter §0.4). Tamper, forge, replay, and
//! line-noise suites — plus a seeded deterministic stress harness that runs
//! the deframer through corrupted-soup streams on every `cargo test` (the
//! cargo-fuzz target in `fuzz/` is the long-run version of the same law).

use apexos_mesh_proto::{
    chunk_blob, encode_frame, open, seal, Deframer, Error, MeshClass, MeshFrame, Payload,
    next_provision_ctr, reserve_from_stored, PlainPacket, Psk, Reassembler, ReplayAdmit,
    ReplayTable, ReplayWindow, MAX_REPLAY_SENDERS, MAX_WIRE_FRAME,
};

fn psk() -> Psk {
    Psk([0x42; 32])
}

fn packet() -> PlainPacket {
    PlainPacket {
        target: 3,
        hop_limit: 4,
        flags: 0,
        payload: Payload::Ack {
            of_sender: 9,
            of_ctr: 77,
        },
    }
}

// ── The envelope refuses forgery ────────────────────────────────────────────

#[test]
fn tampered_headers_break_the_tag() {
    let frame = seal(&psk(), MeshClass::Gossip, 5, 10, &packet()).unwrap();

    let mut f = frame.clone();
    f.class = MeshClass::Critical; // promote priority
    assert_eq!(open(&psk(), &f), Err(Error::Crypto));

    let mut f = frame.clone();
    f.sender = 6; // impersonate
    assert_eq!(open(&psk(), &f), Err(Error::Crypto));

    let mut f = frame.clone();
    f.ctr = 11; // dodge the replay window
    assert_eq!(open(&psk(), &f), Err(Error::Crypto));

    let mut f = frame;
    f.ver = 2; // lie about the wire version
    assert_eq!(open(&psk(), &f), Err(Error::Crypto));
}

#[test]
fn tampered_ciphertext_fails() {
    let mut frame = seal(&psk(), MeshClass::Gossip, 5, 10, &packet()).unwrap();
    frame.ct[0] ^= 0x01;
    assert_eq!(open(&psk(), &frame), Err(Error::Crypto));
}

#[test]
fn truncated_ciphertext_fails() {
    let mut frame = seal(&psk(), MeshClass::Gossip, 5, 10, &packet()).unwrap();
    frame.ct.pop();
    assert_eq!(open(&psk(), &frame), Err(Error::Crypto));
}

#[test]
fn wrong_psk_fails() {
    let frame = seal(&psk(), MeshClass::Gossip, 5, 10, &packet()).unwrap();
    assert_eq!(open(&Psk([0x43; 32]), &frame), Err(Error::Crypto));
}

#[test]
fn zero_counter_never_flies() {
    assert_eq!(
        seal(&psk(), MeshClass::Gossip, 5, 0, &packet()),
        Err(Error::ZeroCounter)
    );
    let mut frame = seal(&psk(), MeshClass::Gossip, 5, 1, &packet()).unwrap();
    frame.ctr = 0;
    assert_eq!(open(&psk(), &frame), Err(Error::ZeroCounter));
}

#[test]
fn nonces_differ_across_senders_and_counters() {
    use apexos_mesh_proto::crypto::nonce_for;
    // Same ctr, different senders — and same sender, different ctrs — must
    // never collide (the construction is injective by layout; spot-lock it).
    assert_ne!(nonce_for(1, 7), nonce_for(2, 7));
    assert_ne!(nonce_for(1, 7), nonce_for(1, 8));
    // The high sender byte can't bleed into the ctr field.
    assert_ne!(nonce_for(0x0100, 0), nonce_for(0, 1));
}

#[test]
fn blob_envelope_roundtrips_and_fails_closed() {
    use apexos_mesh_proto::{open_blob, seal_blob};
    let nonce = [7u8; 12];
    let aad = b"apexos-courier:v1:manifest:aabbccdd00112233";
    let ct = seal_blob(&psk(), &nonce, aad, b"the manifest").unwrap();
    assert_eq!(
        open_blob(&psk(), &nonce, aad, &ct).unwrap(),
        b"the manifest"
    );
    // Wrong AAD (another stick / another domain).
    assert_eq!(
        open_blob(
            &psk(),
            &nonce,
            b"apexos-courier:v1:receipts:aabbccdd00112233",
            &ct
        ),
        Err(Error::Crypto)
    );
    // Wrong nonce.
    assert_eq!(open_blob(&psk(), &[8u8; 12], aad, &ct), Err(Error::Crypto));
    // Wrong key.
    assert_eq!(
        open_blob(&Psk([0x43; 32]), &nonce, aad, &ct),
        Err(Error::Crypto)
    );
    // Flipped byte.
    let mut bad = ct.clone();
    bad[0] ^= 1;
    assert_eq!(open_blob(&psk(), &nonce, aad, &bad), Err(Error::Crypto));
}

// ── Replay windows ──────────────────────────────────────────────────────────

#[test]
fn replay_window_accepts_each_counter_exactly_once() {
    let mut w = ReplayWindow::new();
    for ctr in 1..=200u64 {
        assert!(w.check_and_accept(ctr), "fresh {ctr} rejected");
        assert!(!w.check_and_accept(ctr), "replay {ctr} accepted");
    }
    assert_eq!(w.highest(), 200);
}

#[test]
fn replay_window_backfills_within_reach_and_refuses_beyond() {
    let mut w = ReplayWindow::new();
    assert!(w.check_and_accept(100));
    // In-window out-of-order arrival backfills fine…
    assert!(w.check_and_accept(37)); // offset 63, the last reachable slot
    assert!(!w.check_and_accept(37)); // …once.
                                      // Older than the window reaches: refused, even though never seen.
    assert!(!w.check_and_accept(36));
    assert!(!w.check_and_accept(1));
}

#[test]
fn replay_window_survives_far_jumps() {
    let mut w = ReplayWindow::new();
    assert!(w.check_and_accept(1));
    assert!(w.check_and_accept(1_000_000));
    assert!(!w.check_and_accept(1_000_000));
    assert!(w.check_and_accept(999_999));
    assert!(!w.check_and_accept(1)); // long gone
    assert!(w.check_and_accept(u64::MAX));
    assert!(!w.check_and_accept(u64::MAX));
}

#[test]
fn replay_window_rejects_zero_always() {
    let mut w = ReplayWindow::new();
    assert!(!w.check_and_accept(0));
    assert!(w.check_and_accept(5));
    assert!(!w.check_and_accept(0));
}

#[test]
fn replay_table_never_evicts_and_survives_round_trip() {
    let mut t = ReplayTable::new();
    assert_eq!(t.accept(1, 10), ReplayAdmit::Fresh);
    assert_eq!(t.accept(1, 10), ReplayAdmit::Replay);
    for id in 2..=MAX_REPLAY_SENDERS as u16 {
        assert_eq!(t.accept(id, 1), ReplayAdmit::Fresh, "slot {id}");
    }
    assert_eq!(t.len(), MAX_REPLAY_SENDERS);
    // 17th sender is refused — we do not evict sender 1's window.
    assert_eq!(t.accept(99, 1), ReplayAdmit::TableFull);
    assert_eq!(t.accept(1, 10), ReplayAdmit::Replay);
    assert_eq!(t.accept(1, 11), ReplayAdmit::Fresh);

    let mut back = ReplayTable::new();
    for i in 0..MAX_REPLAY_SENDERS {
        if let Some(bytes) = t.encode_slot(i) {
            assert!(back.load_slot(&bytes));
        }
    }
    assert_eq!(back.accept(1, 11), ReplayAdmit::Replay);
    assert_eq!(back.accept(1, 12), ReplayAdmit::Fresh);
}

#[test]
fn reserve_from_stored_fails_closed_on_read_error() {
    assert_eq!(reserve_from_stored(Ok(None), 1024), Ok((0, 1024)));
    assert_eq!(reserve_from_stored(Ok(Some(4096)), 1024), Ok((4096, 5120)));
    assert_eq!(reserve_from_stored(Err(()), 1024), Err(()));
    assert_ne!(
        reserve_from_stored(Err(()), 1024),
        Ok((0, 1024)),
        "a torn read must not look like first boot"
    );
}

#[test]
fn inbox_holds_until_host_accept_and_does_not_ack_on_full() {
    use apexos_mesh_proto::{
        decide_radio_inbound, InboxTable, RadioInbound, ReplayTable, MAX_INBOX,
    };
    let mut replay = ReplayTable::new();
    let mut inbox = InboxTable::new();
    let pkt = b"hello-radio";

    assert_eq!(
        decide_radio_inbound(&mut replay, &mut inbox, 1001, 7, pkt),
        RadioInbound::Deliver
    );
    assert!(inbox.contains(1001, 7));
    // Same pair again (sender retry) — still waiting on the host.
    assert_eq!(
        decide_radio_inbound(&mut replay, &mut inbox, 1001, 7, pkt),
        RadioInbound::WaitHost
    );
    assert_eq!(inbox.len(), 1);

    // Host accept frees the slot; a later retry of the same pair is a re-ACK.
    assert!(inbox.take(1001, 7).is_some());
    assert_eq!(
        decide_radio_inbound(&mut replay, &mut inbox, 1001, 7, pkt),
        RadioInbound::ReAck
    );

    // Fill the inbox with other pairs; a new sender must not consume replay.
    for i in 0..MAX_INBOX as u64 {
        assert_eq!(
            decide_radio_inbound(&mut replay, &mut inbox, 2000, 10 + i, pkt),
            RadioInbound::Deliver
        );
    }
    assert!(inbox.is_full());
    let before = replay.len();
    assert_eq!(
        decide_radio_inbound(&mut replay, &mut inbox, 3000, 1, pkt),
        RadioInbound::Drop
    );
    assert_eq!(replay.len(), before, "a full inbox must not advance replay");

    // Round-trip a slot through the flash encoding.
    let mut back = InboxTable::new();
    for i in 0..MAX_INBOX {
        if let Some(bytes) = inbox.encode_slot(i) {
            assert!(back.load_slot(&bytes));
        }
    }
    assert_eq!(back.len(), inbox.len());
    assert!(back.contains(2000, 10));

    // A take must tombstone the flash slot so a reload cannot resurrect it.
    assert!(inbox.take(2000, 10).is_some());
    let mut after = InboxTable::new();
    for i in 0..MAX_INBOX {
        if let Some(bytes) = inbox.encode_slot(i) {
            assert!(after.load_slot(&bytes));
        }
    }
    assert!(!after.contains(2000, 10));
}

#[test]
fn next_provision_ctr_never_repeats_or_returns_zero() {
    assert_eq!(next_provision_ctr(None), 1);
    assert_eq!(next_provision_ctr(Some(0)), 1);
    assert_eq!(next_provision_ctr(Some(1)), 2);
    assert_eq!(next_provision_ctr(Some(99)), 100);
}

// ── The deframer under fire (the §4.3 MUSTs) ────────────────────────────────

fn raw_frame(ctr: u64) -> MeshFrame {
    MeshFrame {
        ver: 1,
        class: MeshClass::Gossip,
        sender: 7,
        ctr,
        ct: vec![0xAB; 24],
    }
}

#[test]
fn poison_frame_is_dropped_and_scanning_advances() {
    // MUST-2: a frame that fails decode is counted and passed, never wedged.
    let good = encode_frame(&raw_frame(1)).unwrap();
    let mut d = Deframer::new();
    let mut stream = Vec::new();
    stream.extend_from_slice(&good);
    stream.extend_from_slice(&[0x01, 0x02, 0x03, 0x00]); // valid COBS, garbage inside
    stream.extend_from_slice(&good);
    let got = d.push(&stream);
    assert_eq!(got.len(), 2);
    assert_eq!(d.stats.rx_frames, 2);
    assert_eq!(d.stats.crc_fail + d.stats.decode_fail, 1);
}

#[test]
fn flipped_bit_is_a_crc_fail_not_a_frame() {
    let mut wire = encode_frame(&raw_frame(1)).unwrap();
    // Flip a bit somewhere in the COBS body (not the delimiter).
    let mid = wire.len() / 2;
    wire[mid] ^= 0x10;
    let mut d = Deframer::new();
    let got = d.push(&wire);
    assert!(got.is_empty());
    assert_eq!(d.stats.crc_fail + d.stats.decode_fail, 1);
}

#[test]
fn oversize_flood_stays_bounded_and_resyncs() {
    // MUST-1: a corrupted stream without delimiters can never buffer
    // unbounded garbage.
    let mut d = Deframer::new();
    for _ in 0..10 {
        d.push(&[0x55; 4096]); // 40 KB of delimiter-free noise
        assert!(d.buffered_len() < MAX_WIRE_FRAME);
    }
    assert!(d.stats.oversize_drops >= 1);
    // The next delimiter restores framing (MUST-5)…
    let got = d.push(&[0x00]);
    assert!(got.is_empty());
    assert_eq!(d.stats.resyncs, 1);
    // …and a clean frame decodes.
    let wire = encode_frame(&raw_frame(9)).unwrap();
    assert_eq!(d.push(&wire), vec![raw_frame(9)]);
}

#[test]
fn delimiter_floods_are_silent() {
    let mut d = Deframer::new();
    let wire = encode_frame(&raw_frame(1)).unwrap();
    let mut stream = vec![0x00; 500];
    stream.extend_from_slice(&wire);
    stream.extend(vec![0x00; 500]);
    stream.extend_from_slice(&encode_frame(&raw_frame(2)).unwrap());
    let got = d.push(&stream);
    assert_eq!(got.len(), 2);
    assert_eq!(d.stats.crc_fail, 0);
    assert_eq!(d.stats.decode_fail, 0);
}

#[test]
fn truncated_frame_then_valid_frame_recovers() {
    let wire = encode_frame(&raw_frame(1)).unwrap();
    let mut d = Deframer::new();
    // First half of a frame, then a delimiter cuts it short…
    let mut stream = wire[..wire.len() / 2].to_vec();
    stream.push(0x00);
    // …then a whole valid frame.
    stream.extend_from_slice(&wire);
    let got = d.push(&stream);
    assert_eq!(got, vec![raw_frame(1)]);
    assert_eq!(d.stats.crc_fail + d.stats.decode_fail, 1);
}

#[test]
fn multiple_buffered_frames_drain_in_one_push() {
    // MUST-4: all complete frames per wakeup, not just the first.
    let mut stream = Vec::new();
    for ctr in 1..=5 {
        stream.extend_from_slice(&encode_frame(&raw_frame(ctr)).unwrap());
    }
    let mut d = Deframer::new();
    let got = d.push(&stream);
    assert_eq!(got.len(), 5);
    assert_eq!(
        got.iter().map(|f| f.ctr).collect::<Vec<_>>(),
        vec![1, 2, 3, 4, 5]
    );
}

// ── Seeded stress: the every-run mini-fuzzer ────────────────────────────────

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

/// 2000 intact frames swim through a soup of seeded garbage (random bytes,
/// embedded delimiters, bit flips, truncated frames). Laws: never panic, the
/// buffer stays bounded, and **every intact frame is delivered** — garbage
/// between delimiters can only cost itself, never a neighbor.
#[test]
fn deframer_survives_corrupted_soup_without_losing_intact_frames() {
    let mut rng = Rng(0xC0FF_EE15_600D_5EED);
    let mut stream = Vec::new();
    let n_frames: u64 = 2000;

    for ctr in 1..=n_frames {
        // A garbage block: random bytes, sometimes with delimiters inside,
        // sometimes a mutilated copy of a real frame. Always delimiter-
        // terminated so it can only poison itself (the COBS resync law).
        match rng.next() % 4 {
            0 => {
                let len = (rng.next() % 200) as usize;
                for _ in 0..len {
                    stream.push((rng.next() % 256) as u8);
                }
                stream.push(0x00);
            }
            1 => {
                let mut mangled = encode_frame(&raw_frame(ctr + 1_000_000)).unwrap();
                let cut = (rng.next() as usize) % mangled.len();
                mangled.truncate(cut);
                stream.extend_from_slice(&mangled);
                stream.push(0x00);
            }
            2 => {
                let mut flipped = encode_frame(&raw_frame(ctr + 2_000_000)).unwrap();
                let pos = (rng.next() as usize) % (flipped.len() - 1);
                flipped[pos] ^= 1 << (rng.next() % 8);
                // The flip may hit a body byte and turn it into 0x00 or turn
                // the delimiter into noise — both are legitimate line chaos.
                stream.extend_from_slice(&flipped);
                stream.push(0x00);
            }
            _ => {
                stream.extend(vec![0x00; (rng.next() % 8) as usize]);
            }
        }
        stream.extend_from_slice(&encode_frame(&raw_frame(ctr)).unwrap());
    }

    // Feed in randomly-sized slices, tracking the memory bound throughout.
    let mut d = Deframer::new();
    let mut got = Vec::new();
    let mut rest = stream.as_slice();
    while !rest.is_empty() {
        let take = (1 + rng.next() % 300).min(rest.len() as u64) as usize;
        let (piece, tail) = rest.split_at(take);
        got.extend(d.push(piece));
        assert!(d.buffered_len() < MAX_WIRE_FRAME);
        rest = tail;
    }

    // Every intact frame made it. (A bit-flipped decoy accidentally passing
    // CRC32 would only ADD frames with ctr > 1_000_000 — it can't remove
    // ours, and the intact set is asserted as a subsequence.)
    let intact: Vec<u64> = got
        .iter()
        .map(|f| f.ctr)
        .filter(|c| *c <= n_frames)
        .collect();
    assert_eq!(intact, (1..=n_frames).collect::<Vec<_>>());
    assert_eq!(d.stats.rx_frames as usize, got.len());
    assert!(d.stats.crc_fail + d.stats.decode_fail + d.stats.resyncs > 0);
}

// ── Chunker hostility ───────────────────────────────────────────────────────

#[test]
fn reassembler_rejects_inconsistent_announcements() {
    // Grid must tile total_len exactly.
    assert_eq!(
        Reassembler::new([0; 32], 2, 1000, 256).unwrap_err(),
        Error::ChunkSize
    ); // needs 4 chunks
    assert_eq!(
        Reassembler::new([0; 32], 0, 1, 256).unwrap_err(),
        Error::ChunkSize
    ); // one byte can't have zero chunks
    assert_eq!(
        Reassembler::new([0; 32], 1, 10, 0).unwrap_err(),
        Error::ChunkSize
    ); // zero-length chunks
    assert!(Reassembler::new([0; 32], 0, 0, 256).is_ok()); // empty blob is fine
}

#[test]
fn reassembler_rejects_wrong_geometry_chunks() {
    let blob = vec![7u8; 700];
    let set = chunk_blob(&blob, 256).unwrap(); // 3 chunks: 256, 256, 188
    let mut r = Reassembler::new(set.root, 3, 700, 256).unwrap();
    assert_eq!(
        r.accept(&[9; 32], 0, &set.chunks[0]),
        Err(Error::RootMismatch)
    );
    assert_eq!(r.accept(&set.root, 3, &[0; 256]), Err(Error::ChunkIndex));
    assert_eq!(r.accept(&set.root, 0, &[0; 188]), Err(Error::ChunkSize)); // last-chunk len at index 0
    assert_eq!(r.accept(&set.root, 2, &[0; 256]), Err(Error::ChunkSize)); // full len at last index
    assert!(r.accept(&set.root, 0, &set.chunks[0]).unwrap());
    assert_eq!(
        r.accept(&set.root, 0, &vec![0xEE; 256]),
        Err(Error::ChunkConflict)
    );
}

#[test]
fn reassembler_catches_content_forgery_at_finish() {
    // Right geometry, wrong bytes — only the blake3 root can tell.
    let blob = vec![7u8; 512];
    let set = chunk_blob(&blob, 256).unwrap();
    let mut r = Reassembler::new(set.root, 2, 512, 256).unwrap();
    assert!(r.accept(&set.root, 0, &set.chunks[0]).unwrap());
    assert!(r.accept(&set.root, 1, &vec![8u8; 256]).unwrap()); // forged chunk
    assert_eq!(r.finish(), Err(Error::RootMismatch));
}

#[test]
fn reassembler_refuses_to_finish_incomplete() {
    let blob = vec![7u8; 512];
    let set = chunk_blob(&blob, 256).unwrap();
    let mut r = Reassembler::new(set.root, 2, 512, 256).unwrap();
    assert!(r.accept(&set.root, 0, &set.chunks[0]).unwrap());
    assert_eq!(r.missing(), vec![1]);
    assert_eq!(r.finish(), Err(Error::Incomplete));
}
