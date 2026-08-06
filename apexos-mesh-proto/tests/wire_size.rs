//! Wire-size locks. The charter budgets specific byte counts against radio
//! MTUs (LoRa SF12 ≈ 51 B payload, BLE adv ≈ 200 B) — these tests turn those
//! claims into compile-adjacent facts. If a type change trips one, the charter
//! numbers (docs/apexnet.md §3/§7) need updating in the same PR.

use apexos_mesh_proto::{
    chunk_blob, encode_frame, seal, CourierManifest, CourierReceipt, Digest, MeshClass, MeshFrame,
    Payload, PlainPacket, Psk, DEFAULT_CHUNK_SIZE, DEFAULT_HOP_LIMIT, MAX_WIRE_FRAME,
};

fn wire_len<T: serde::Serialize>(v: &T) -> usize {
    postcard::to_allocvec(v).unwrap().len()
}

#[test]
fn digest_fits_the_96_byte_claim_even_worst_case() {
    let worst = Digest {
        ver: u8::MAX,
        node: u16::MAX,
        epoch: u32::MAX,
        mem_root: [0xFF; 32],
        n_new: u16::MAX,
        tags: [u32::MAX; 4],
        reserved: u32::MAX,
    };
    assert!(wire_len(&worst) <= 96, "digest {} > 96", wire_len(&worst));
}

#[test]
fn courier_manifest_fits_its_claim() {
    // Charter §3 claims ~56 B for realistic values; worst-case varints stay
    // under 64. A LoRa SF7 frame (~222 B) carries either with the envelope.
    let typical = CourierManifest {
        stick: *b"\xa3\xf9\xc2\xe1\x1b\x7d\x44\x02",
        origin: 1,
        dest: 3,
        root: [0xAB; 32],
        n_chunks: 200,
        total_len: 48_000,
        epoch: 1200,
    };
    assert!(
        wire_len(&typical) <= 56,
        "typical manifest {} > 56",
        wire_len(&typical)
    );

    let worst = CourierManifest {
        stick: [0xFF; 8],
        origin: u16::MAX,
        dest: u16::MAX,
        root: [0xFF; 32],
        n_chunks: u16::MAX,
        total_len: u32::MAX,
        epoch: u32::MAX,
    };
    assert!(
        wire_len(&worst) <= 64,
        "worst manifest {} > 64",
        wire_len(&worst)
    );
}

#[test]
fn courier_receipt_fits_its_claim() {
    let worst = CourierReceipt {
        stick: [0xFF; 8],
        root: [0xFF; 32],
        accepted: true,
    };
    assert!(wire_len(&worst) <= 44, "receipt {} > 44", wire_len(&worst));
}

#[test]
fn sealed_heartbeat_fits_a_ble_advertisement() {
    // The gossip tier assumes ~200 B/packet; a full sealed+framed heartbeat
    // must leave comfortable headroom.
    let psk = Psk([7; 32]);
    let packet = PlainPacket {
        target: apexos_mesh_proto::BROADCAST,
        hop_limit: DEFAULT_HOP_LIMIT,
        flags: 0,
        payload: Payload::Heartbeat {
            uptime_s: 863_000,
            cortex_up: true,
            conn: 2,
        },
    };
    let frame = seal(&psk, MeshClass::Gossip, 42, 123_456, &packet).unwrap();
    let wire = encode_frame(&frame).unwrap();
    assert!(wire.len() <= 128, "heartbeat wire {} > 128", wire.len());
}

#[test]
fn default_chunk_rides_one_wire_frame() {
    let blob = vec![0x5A; DEFAULT_CHUNK_SIZE * 3];
    let set = chunk_blob(&blob, DEFAULT_CHUNK_SIZE).unwrap();
    let psk = Psk([9; 32]);
    let packet = PlainPacket {
        target: 2,
        hop_limit: 1,
        flags: 0,
        payload: set.data(1).unwrap(),
    };
    let frame = seal(&psk, MeshClass::Bulk, 1, 1, &packet).unwrap();
    let wire = encode_frame(&frame).unwrap();
    assert!(wire.len() <= 512, "chunk wire {} > 512", wire.len());
}

#[test]
fn oversized_frames_are_refused_at_encode() {
    let frame = MeshFrame {
        ver: 1,
        class: MeshClass::Bulk,
        sender: 1,
        ctr: 1,
        ct: vec![0xAA; MAX_WIRE_FRAME],
    };
    assert_eq!(
        encode_frame(&frame),
        Err(apexos_mesh_proto::Error::TooLarge)
    );
}
