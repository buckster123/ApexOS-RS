//! Property tests: the full frame pipeline, the crypto envelope, and the
//! chunker must all roundtrip losslessly — including through arbitrary
//! stream fragmentation and out-of-order chunk arrival.

use proptest::prelude::*;

use apexos_mesh_proto::{
    chunk_blob, decode_frame, encode_frame, open, seal, CourierManifest, CourierReceipt, Deframer,
    Digest, MeshClass, MeshFrame, Payload, PlainPacket, Psk, Reassembler,
};

fn arb_class() -> impl Strategy<Value = MeshClass> {
    prop_oneof![
        Just(MeshClass::Critical),
        Just(MeshClass::Gossip),
        Just(MeshClass::Bulk),
        Just(MeshClass::Digest),
    ]
}

fn arb_digest() -> impl Strategy<Value = Digest> {
    (
        any::<u8>(),
        any::<u16>(),
        any::<u32>(),
        any::<[u8; 32]>(),
        any::<u16>(),
        any::<[u32; 4]>(),
        any::<u32>(),
    )
        .prop_map(
            |(ver, node, epoch, mem_root, n_new, tags, reserved)| Digest {
                ver,
                node,
                epoch,
                mem_root,
                n_new,
                tags,
                reserved,
            },
        )
}

fn arb_payload() -> impl Strategy<Value = Payload> {
    prop_oneof![
        (any::<u32>(), any::<bool>(), any::<u8>()).prop_map(|(uptime_s, cortex_up, conn)| {
            Payload::Heartbeat {
                uptime_s,
                cortex_up,
                conn,
            }
        }),
        (any::<u16>(), ".{0,40}").prop_map(|(code, detail)| Payload::Alarm { code, detail }),
        proptest::collection::vec(any::<u8>(), 0..600).prop_map(|body| Payload::A2A { body }),
        arb_digest().prop_map(Payload::DreamDigest),
        (any::<[u8; 32]>(), any::<u16>(), any::<u32>(), any::<u16>()).prop_map(
            |(root, n_chunks, total_len, chunk_len)| Payload::ChunkAnnounce {
                root,
                n_chunks,
                total_len,
                chunk_len,
            }
        ),
        (any::<[u8; 32]>(), any::<u16>())
            .prop_map(|(root, index)| Payload::ChunkRequest { root, index }),
        (
            any::<[u8; 32]>(),
            any::<u16>(),
            proptest::collection::vec(any::<u8>(), 0..300)
        )
            .prop_map(|(root, index, data)| Payload::ChunkData { root, index, data }),
        (any::<u16>(), any::<u64>())
            .prop_map(|(of_sender, of_ctr)| Payload::Ack { of_sender, of_ctr }),
        (
            any::<[u8; 8]>(),
            any::<u16>(),
            any::<u16>(),
            any::<[u8; 32]>(),
            any::<u16>(),
            any::<u32>(),
            any::<u32>()
        )
            .prop_map(|(stick, origin, dest, root, n_chunks, total_len, epoch)| {
                Payload::CourierManifest(CourierManifest {
                    stick,
                    origin,
                    dest,
                    root,
                    n_chunks,
                    total_len,
                    epoch,
                })
            }),
        (any::<[u8; 8]>(), any::<[u8; 32]>(), any::<bool>()).prop_map(|(stick, root, accepted)| {
            Payload::CourierReceipt(CourierReceipt {
                stick,
                root,
                accepted,
            })
        }),
        (any::<u16>(), any::<[u8; 32]>())
            .prop_map(|(node_id, psk)| Payload::Provision { node_id, psk }),
        (any::<u16>(), any::<u16>(), any::<u8>(), any::<u64>()).prop_map(
            |(node_id, queued, neighbors, ctr_hw)| Payload::BrainstemStatus {
                node_id,
                queued,
                neighbors,
                ctr_hw,
            }
        ),
    ]
}

fn arb_packet() -> impl Strategy<Value = PlainPacket> {
    (any::<u16>(), any::<u8>(), any::<u8>(), arb_payload()).prop_map(
        |(target, hop_limit, flags, payload)| PlainPacket {
            target,
            hop_limit,
            flags,
            payload,
        },
    )
}

/// A raw frame (ct is arbitrary bytes — the framing layer doesn't care).
fn arb_frame() -> impl Strategy<Value = MeshFrame> {
    (
        any::<u8>(),
        arb_class(),
        any::<u16>(),
        any::<u64>(),
        proptest::collection::vec(any::<u8>(), 0..2048),
    )
        .prop_map(|(ver, class, sender, ctr, ct)| MeshFrame {
            ver,
            class,
            sender,
            ctr,
            ct,
        })
}

proptest! {
    /// Frames survive the wire pipeline through arbitrary fragmentation:
    /// encode several, concatenate, split at proptest-chosen boundaries,
    /// feed the pieces — every frame comes back, in order, bit-identical.
    #[test]
    fn frame_pipeline_roundtrips_through_fragmentation(
        frames in proptest::collection::vec(arb_frame(), 1..5),
        cuts in proptest::collection::vec(1usize..64, 0..40),
    ) {
        let mut stream = Vec::new();
        for f in &frames {
            stream.extend_from_slice(&encode_frame(f).unwrap());
        }
        let mut deframer = Deframer::new();
        let mut got = Vec::new();
        let mut rest = stream.as_slice();
        for cut in cuts {
            let take = cut.min(rest.len());
            let (piece, tail) = rest.split_at(take);
            got.extend(deframer.push(piece));
            rest = tail;
        }
        got.extend(deframer.push(rest));
        prop_assert_eq!(got, frames);
        prop_assert_eq!(deframer.stats.crc_fail, 0);
        prop_assert_eq!(deframer.stats.decode_fail, 0);
    }

    /// seal → open is lossless for every packet shape, and the sealed frame
    /// carries the headers it was sealed with.
    #[test]
    fn envelope_roundtrips(
        psk in any::<[u8; 32]>(),
        class in arb_class(),
        sender in any::<u16>(),
        ctr in 1u64..,
        packet in arb_packet(),
    ) {
        let psk = Psk(psk);
        let frame = seal(&psk, class, sender, ctr, &packet).unwrap();
        prop_assert_eq!(frame.class, class);
        prop_assert_eq!(frame.sender, sender);
        prop_assert_eq!(frame.ctr, ctr);
        prop_assert_eq!(open(&psk, &frame).unwrap(), packet);
    }

    /// The full stack: seal → frame → wire → deframe → open.
    #[test]
    fn full_stack_roundtrips(
        psk in any::<[u8; 32]>(),
        sender in any::<u16>(),
        ctr in 1u64..,
        packet in arb_packet(),
    ) {
        let psk = Psk(psk);
        let frame = seal(&psk, MeshClass::Gossip, sender, ctr, &packet).unwrap();
        let wire = encode_frame(&frame).unwrap();
        let mut deframer = Deframer::new();
        let got = deframer.push(&wire);
        prop_assert_eq!(got.len(), 1);
        prop_assert_eq!(open(&psk, &got[0]).unwrap(), packet);
    }

    /// Chunk → reassemble in shuffled order → identical blob, verified root.
    #[test]
    fn chunker_roundtrips_out_of_order(
        blob in proptest::collection::vec(any::<u8>(), 0..3000),
        chunk_len in 1usize..700,
        order in Just(()).prop_perturb(|_, mut rng| rng.random::<u64>()),
    ) {
        let set = chunk_blob(&blob, chunk_len).unwrap();
        // An announce built from a real ChunkSet is always geometry-consistent.
        let mut reasm = Reassembler::from_announce(&set.announce()).unwrap().unwrap();
        // Deterministic shuffle of the delivery order from the perturb seed.
        let mut indexes: Vec<u16> = (0..set.n_chunks()).collect();
        let mut s = order.wrapping_add(0x9E37_79B9_7F4A_7C15);
        for i in (1..indexes.len()).rev() {
            s ^= s << 13; s ^= s >> 7; s ^= s << 17;
            indexes.swap(i, (s % (i as u64 + 1)) as usize);
        }
        for idx in indexes {
            let chunk = &set.chunks[idx as usize];
            prop_assert!(reasm.accept(&set.root, idx, chunk).unwrap());
            // Idempotent duplicate:
            prop_assert!(!reasm.accept(&set.root, idx, chunk).unwrap());
        }
        prop_assert!(reasm.is_complete());
        prop_assert!(reasm.missing().is_empty());
        prop_assert_eq!(reasm.finish().unwrap(), blob);
    }
}

proptest! {
    /// Datagram framing carries exactly what stream framing carries — the
    /// frame is the contract, the wrapper is the link's business.
    #[test]
    fn datagram_roundtrips_and_matches_stream_framing(frame in arb_frame()) {
        use apexos_mesh_proto::{decode_datagram, encode_datagram};

        let wire = match encode_datagram(&frame) {
            Ok(w) => w,
            // Oversized frames belong to the chunker, not this layer.
            Err(_) => return Ok(()),
        };
        prop_assert_eq!(decode_datagram(&wire).unwrap(), frame.clone());

        // Same frame, both link shapes: the stream form is strictly bigger
        // (COBS + CRC32 + delimiter), which is exactly the overhead a radio
        // must not pay.
        if let Ok(stream) = encode_frame(&frame) {
            prop_assert!(stream.len() > wire.len());
            prop_assert_eq!(decode_frame(&stream[..stream.len() - 1]).unwrap(), frame);
        }
    }

    /// Trailing bytes are rejected, so a datagram cannot smuggle a rider past
    /// the decoder the way a naive length-prefixed parser would allow.
    #[test]
    fn datagram_rejects_trailing_bytes(frame in arb_frame(), rider in proptest::collection::vec(any::<u8>(), 1..8)) {
        use apexos_mesh_proto::{decode_datagram, encode_datagram};
        if let Ok(mut wire) = encode_datagram(&frame) {
            wire.extend_from_slice(&rider);
            if wire.len() <= apexos_mesh_proto::MAX_DATAGRAM_FRAME {
                prop_assert!(decode_datagram(&wire).is_err());
            }
        }
    }
}
