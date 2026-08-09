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

/// postcard writes an enum's variant *index*, not its name — so the order of
/// `Payload` IS the wire contract, shared with firmware that may be flashed
/// on a board nobody can reach. This pins every discriminant as a literal.
/// Appending a variant extends this list; changing an existing number is a
/// [`apexos_mesh_proto::WIRE_VERSION`] bump, never a quiet edit.
#[test]
fn payload_variant_indices_are_frozen() {
    fn tag(p: &Payload) -> u8 {
        postcard::to_allocvec(p).unwrap()[0]
    }

    assert_eq!(
        tag(&Payload::Heartbeat {
            uptime_s: 0,
            cortex_up: false,
            conn: 0
        }),
        0
    );
    assert_eq!(
        tag(&Payload::Alarm {
            code: 0,
            detail: String::new()
        }),
        1
    );
    assert_eq!(tag(&Payload::A2A { body: vec![] }), 2);
    assert_eq!(
        tag(&Payload::DreamDigest(Digest {
            ver: 0,
            node: 0,
            epoch: 0,
            mem_root: [0; 32],
            n_new: 0,
            tags: [0; 4],
            reserved: 0
        })),
        3
    );
    assert_eq!(
        tag(&Payload::ChunkAnnounce {
            root: [0; 32],
            n_chunks: 0,
            total_len: 0,
            chunk_len: 0
        }),
        4
    );
    assert_eq!(
        tag(&Payload::ChunkRequest {
            root: [0; 32],
            index: 0
        }),
        5
    );
    assert_eq!(
        tag(&Payload::ChunkData {
            root: [0; 32],
            index: 0,
            data: vec![]
        }),
        6
    );
    assert_eq!(
        tag(&Payload::Ack {
            of_sender: 0,
            of_ctr: 0
        }),
        7
    );
    assert_eq!(
        tag(&Payload::CourierManifest(CourierManifest {
            stick: [0; 8],
            origin: 0,
            dest: 0,
            root: [0; 32],
            n_chunks: 0,
            total_len: 0,
            epoch: 0
        })),
        8
    );
    assert_eq!(
        tag(&Payload::CourierReceipt(CourierReceipt {
            stick: [0; 8],
            root: [0; 32],
            accepted: false
        })),
        9
    );
    assert_eq!(
        tag(&Payload::Provision {
            node_id: 0,
            psk: [0; 32]
        }),
        10
    );
    assert_eq!(
        tag(&Payload::BrainstemStatus {
            node_id: 0,
            queued: 0,
            neighbors: 0,
            ctr_hw: 0
        }),
        11
    );
}

/// A provisioning frame must fit the brainstem's bounded RX path: it is the
/// largest thing the cortex ever sends down the wire, and the firmware's
/// deframer buffer is sized for it.
#[test]
fn provision_frame_fits_the_wired_link() {
    let psk = Psk([3; 32]);
    let packet = PlainPacket {
        target: 1001,
        hop_limit: 1,
        flags: 0,
        payload: Payload::Provision {
            node_id: u16::MAX,
            psk: [0xFF; 32],
        },
    };
    let frame = seal(&psk, MeshClass::Critical, 1, u64::MAX, &packet).unwrap();
    let wire = encode_frame(&frame).unwrap();
    assert!(wire.len() <= 128, "provision wire {} > 128", wire.len());
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
