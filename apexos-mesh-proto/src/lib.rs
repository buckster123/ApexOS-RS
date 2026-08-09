//! # apexos-mesh-proto
//!
//! The ApexNET wire contract — the radio/UART frame format shared by the Pi
//! bridge (`apexos-mesh-bridge`), the ESP32 brainstem firmware, and agentd's
//! mesh router. Sibling of `apexos-protocol` (which stays the WS/a2a
//! contract); this crate is the *radio* side of the nervous system.
//! Charter: `docs/apexnet.md` (v3, LOCKED) + v2 §3 (`docs/ideas/apexnet/`).
//!
//! `#![no_std]` + `alloc` on every target. Both build gates must stay green,
//! exactly like `apexos-protocol`:
//!
//! ```text
//! cargo test -p apexos-mesh-proto
//! cargo test -p apexos-mesh-proto --no-default-features --features alloc
//! ```
//!
//! ## The frozen framing recipe (v2 §3.1 / §11.3 — pick one, freeze it)
//!
//! ```text
//! TX: MeshFrame --postcard--> bytes --append CRC32(LE)--> bytes --COBS--> wire ++ 0x00
//! RX: split on 0x00 --COBS decode--> verify+strip CRC32 --postcard--> MeshFrame
//! ```
//!
//! **Explicit layering** — plain postcard, the `cobs` crate, `crc32fast` —
//! NOT postcard's `use-crc` flavor or its built-in COBS helpers. Each layer is
//! independently testable and fuzzable, and the recipe is identical on both
//! ends of the UART by construction (same crate). Changing any layer is a wire
//! version bump, never a silent swap.
//!
//! ## Wire evolution rules
//!
//! postcard is not self-describing: **enum variant order and struct field
//! order ARE the wire contract**. Variants are append-only; fields never
//! reorder. Breaking changes bump [`WIRE_VERSION`] (checked before decrypt —
//! the header is AAD-bound, so a lying version fails the AEAD tag).
//!
//! ## Layer map
//!
//! - [`types`] — `MeshFrame`, `PlainPacket`, `Payload`, `Digest`, courier
//!   payloads. Serde types only, no logic.
//! - [`frame`] — encode/decode pipeline + the incremental [`Deframer`]
//!   (bounded memory, poison-frame advance, COBS resync; the fuzz target),
//!   plus [`encode_datagram`]/[`decode_datagram`] for packet links (BLE) that
//!   already bound and CRC their own payloads.
//! - [`crypto`] — ChaCha20-Poly1305 envelope: [`seal`]/[`open`], the nonce
//!   and AAD constructions, and the per-sender [`ReplayWindow`].
//! - [`chunk`] — content-addressed chunker for the Bulk lane: blake3 root,
//!   [`chunk_blob`], [`Reassembler`].

#![cfg_attr(not(feature = "std"), no_std)]
#![forbid(unsafe_code)]

extern crate alloc;

use core::fmt;

pub mod chunk;
pub mod crypto;
pub mod frame;
pub mod types;

pub use chunk::{blob_root, chunk_blob, ChunkSet, Reassembler, DEFAULT_CHUNK_SIZE};
pub use crypto::{open, open_blob, seal, seal_blob, Psk, ReplayWindow};
pub use frame::{
    decode_datagram, decode_frame, encode_datagram, encode_frame, Deframer, DeframerStats,
    MAX_DATAGRAM_FRAME,
};
pub use types::{
    CourierManifest, CourierReceipt, Digest, MeshClass, MeshFrame, Payload, PlainPacket,
};

/// Current wire version. Bump on any breaking change to framing, types, or
/// the crypto envelope. Receivers reject other versions *after* the AEAD tag
/// check (the header is AAD, so a tampered version can't forge its way in).
pub const WIRE_VERSION: u8 = 1;

/// Hard ceiling on one COBS-encoded wire frame **including** the `0x00`
/// delimiter (UART sizing). Radio MTUs are far smaller — the chunker splits
/// *above* the frame layer, never the framing itself. The deframer enforces
/// this bound while scanning, so a corrupted stream can never make a parser
/// buffer unbounded garbage (v1 bug class).
pub const MAX_WIRE_FRAME: usize = 4096;

/// Broadcast target id.
pub const BROADCAST: u16 = 0xFFFF;

/// Default relay hop limit for [`PlainPacket::hop_limit`].
pub const DEFAULT_HOP_LIMIT: u8 = 4;

/// `PlainPacket::flags` bit 0: sender requests an [`Payload::Ack`].
pub const FLAG_ACK_REQUESTED: u8 = 0b0000_0001;

/// Everything that can go wrong at the wire layer. Deliberately coarse —
/// radio-side code branches on *class* of failure, and detail strings don't
/// exist in `no_std`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// postcard serialize/deserialize failed (or trailing bytes after a
    /// complete decode — frames are exact-fit, no smuggling).
    Postcard,
    /// Encoded frame would exceed [`MAX_WIRE_FRAME`], or a blob exceeds the
    /// `u32` byte-length the wire carries.
    TooLarge,
    /// Frame body shorter than the CRC32 trailer.
    Truncated,
    /// CRC32 trailer mismatch (line noise that survived COBS).
    CrcMismatch,
    /// COBS decode failed (corrupt stuffing).
    Cobs,
    /// AEAD seal/open failed — on open this means tamper, wrong PSK, or a
    /// forged header (headers are AAD-bound).
    Crypto,
    /// Counters mint from 1; `ctr == 0` never goes on the wire (it is the
    /// replay window's "nothing seen" floor).
    ZeroCounter,
    /// Unknown [`MeshClass`] discriminant.
    BadClass(u8),
    /// Chunk index out of range for the announced set.
    ChunkIndex,
    /// Chunk length inconsistent with the announced geometry.
    ChunkSize,
    /// Two different payloads arrived for the same chunk index.
    ChunkConflict,
    /// Reassembled blob does not hash to the announced blake3 root.
    RootMismatch,
    /// Reassembly finished with chunks still missing.
    Incomplete,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Postcard => write!(f, "postcard encode/decode failed"),
            Error::TooLarge => write!(f, "exceeds wire size bound"),
            Error::Truncated => write!(f, "frame shorter than CRC trailer"),
            Error::CrcMismatch => write!(f, "CRC32 mismatch"),
            Error::Cobs => write!(f, "COBS decode failed"),
            Error::Crypto => write!(f, "AEAD failure (tamper, wrong key, or forged header)"),
            Error::ZeroCounter => write!(f, "counter 0 is never valid on the wire"),
            Error::BadClass(c) => write!(f, "unknown mesh class {c}"),
            Error::ChunkIndex => write!(f, "chunk index out of range"),
            Error::ChunkSize => write!(f, "chunk length inconsistent with announced geometry"),
            Error::ChunkConflict => write!(f, "conflicting data for same chunk index"),
            Error::RootMismatch => write!(f, "reassembled blob does not match blake3 root"),
            Error::Incomplete => write!(f, "chunks still missing"),
        }
    }
}

impl core::error::Error for Error {}
