//! Wire types. Serde only — no logic lives here.
//!
//! **Evolution law:** postcard is positional. Enum variants are append-only,
//! struct fields never reorder, nothing is ever removed — a breaking change
//! bumps [`crate::WIRE_VERSION`] instead.

use alloc::string::String;
use alloc::vec::Vec;
use serde::{Deserialize, Serialize};

use crate::Error;

/// Message class → routing policy (v2 §2.2). The cleartext header carries it
/// so queues can prioritize before decrypting.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum MeshClass {
    /// Heartbeat-loss alarms, safety events. Fans out on every up transport;
    /// receivers dedup by `(sender, ctr)`.
    Critical = 0,
    /// Heartbeats, presence, small A2A. Cheapest healthy transport.
    Gossip = 1,
    /// Chunked payloads. GATT/Tier-1 only — never flooded, never LoRa.
    Bulk = 2,
    /// Dream digests, merkle roots, courier gossip. Idempotent; coalesce to
    /// newest per (node, epoch).
    Digest = 3,
}

impl From<MeshClass> for u8 {
    fn from(c: MeshClass) -> u8 {
        c as u8
    }
}

impl TryFrom<u8> for MeshClass {
    type Error = Error;
    fn try_from(v: u8) -> Result<Self, Error> {
        match v {
            0 => Ok(MeshClass::Critical),
            1 => Ok(MeshClass::Gossip),
            2 => Ok(MeshClass::Bulk),
            3 => Ok(MeshClass::Digest),
            other => Err(Error::BadClass(other)),
        }
    }
}

/// Outer frame — what actually rides the wire (inside the COBS/CRC framing).
///
/// Header fields are cleartext (needed *pre*-decrypt for dedup, replay check,
/// and queueing) and are bound into the AEAD tag as AAD
/// ([`crate::crypto::header_aad`]) — tampering with any of them breaks
/// [`crate::open`].
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct MeshFrame {
    /// Wire version ([`crate::WIRE_VERSION`]).
    pub ver: u8,
    pub class: MeshClass,
    /// Node id from the colony registry (peers.toml-adjacent).
    pub sender: u16,
    /// Per-sender monotonic counter, minted from 1 and **persisted** by the
    /// sender. `(sender, ctr)` is the message id (dedup key) and the nonce —
    /// reuse is a hard-fail class of bug.
    pub ctr: u64,
    /// ChaCha20-Poly1305 ciphertext of `postcard(PlainPacket)`.
    pub ct: Vec<u8>,
}

/// What [`crate::open`] yields — the decrypted interior of a frame.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct PlainPacket {
    /// Destination node id; [`crate::BROADCAST`] = everyone.
    pub target: u16,
    /// Decrement on relay; drop at 0. Default [`crate::DEFAULT_HOP_LIMIT`].
    pub hop_limit: u8,
    /// Bit 0: [`crate::FLAG_ACK_REQUESTED`]. Rest reserved (send 0).
    pub flags: u8,
    pub payload: Payload,
}

/// The payload menu. **Append-only** (postcard is positional).
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub enum Payload {
    /// Per-node liveness, broadcast on the gossip tiers. `conn` is the
    /// sender's ConnectivityState as a raw u8 (the agentd side owns the
    /// enum; the wire stays tolerant of values it doesn't know yet).
    Heartbeat {
        uptime_s: u32,
        cortex_up: bool,
        conn: u8,
    },
    /// Safety/attention event. `code` is the alarm registry key (agentd-side
    /// table); `detail` is short free text — radio frames are small.
    Alarm { code: u16, detail: String },
    /// Agent-to-agent message, opaque to the mesh layer. Same law as every
    /// inbound payload: *data, never instructions*, until policy says
    /// otherwise.
    A2A { body: Vec<u8> },
    /// Nightly anti-entropy claim (see [`Digest`]).
    DreamDigest(Digest),
    /// A content-addressed blob exists and can be pulled chunk-by-chunk.
    /// `chunk_len` makes every chunk's expected size mechanically checkable
    /// (deviation from the v2 reference signature: `(n_chunks, total_len)`
    /// alone does not determine chunk geometry).
    ChunkAnnounce {
        root: [u8; 32],
        n_chunks: u16,
        total_len: u32,
        chunk_len: u16,
    },
    /// Pull one missing chunk.
    ChunkRequest { root: [u8; 32], index: u16 },
    /// One chunk of an announced blob.
    ChunkData {
        root: [u8; 32],
        index: u16,
        data: Vec<u8>,
    },
    /// Acknowledge receipt of `(of_sender, of_ctr)`.
    Ack { of_sender: u16, of_ctr: u64 },
    /// Tier-4 courier gossip: "stick X carrying root R for node D departed
    /// origin O at epoch E" (~56 B — fits every radio tier). The destination
    /// knows the cargo exists before the human arrives.
    CourierManifest(CourierManifest),
    /// Delivery proof gossiped back after ingest (~44 B). Proof of
    /// *delivery*, not proof of *read*.
    CourierReceipt(CourierReceipt),
    /// **Commissioning ceremony, wired links only.** The cortex tells a
    /// brainstem who it is and hands down the colony PSK, which the brainstem
    /// persists to flash so it survives both its own power cycles and the
    /// cortex's absence (charter §0.1 — the nervous system outlives the
    /// cortex).
    ///
    /// Acceptance is asymmetric by design and enforced by the *receiver*:
    /// - flash empty ⇒ accept unsealed (trust-on-first-use over the physical
    ///   UART; whoever holds the wire can already reflash the board);
    /// - key present ⇒ accept **only** inside a frame that opened under the
    ///   current key, which makes rotation authenticated.
    ///
    /// Never honoured over a radio tier — see charter §0.4. A PSK on the air
    /// is the one thing this protocol must never do.
    Provision { node_id: u16, psk: [u8; 32] },
    /// Brainstem telemetry, reported up the wired link. The firmware prints
    /// nothing after boot (its serial line *is* the wire), so this frame is
    /// how the cortex learns queue depth and counter state — the observable
    /// that makes "a queued message survived a power cycle" checkable.
    BrainstemStatus {
        node_id: u16,
        /// Frames waiting in the flash store-and-forward queue.
        queued: u16,
        /// Radio neighbours currently heard. 0 until the BLE tier lands.
        neighbors: u8,
        /// Persisted counter high-water mark — the reservation ceiling, not
        /// the last counter used. Reboots resume above it, never below.
        ctr_hw: u64,
    },
}

/// Nightly per-node anti-entropy claim, ≤96 B before the envelope (v2 §7 +
/// the v3 `ver`/`reserved` room). The radio never carries memories — it
/// carries the provenance-stamped *claim* that memories exist; reconciliation
/// happens later over a fat lane.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
pub struct Digest {
    /// Digest schema version (independent of the wire version).
    pub ver: u8,
    pub node: u16,
    /// Day index.
    pub epoch: u32,
    /// blake3 root over the epoch's new-memory chunk set.
    pub mem_root: [u8; 32],
    pub n_new: u16,
    /// Top-k salience tag hashes.
    pub tags: [u32; 4],
    /// Reserved for the PH-topology hook (v2 §7 future note). Send 0.
    pub reserved: u32,
}

/// Tier-4 courier announcement (charter §7). `stick` is the 8-byte
/// `stick_id` minted at prep (marker schema v2) — the *ledger* key,
/// decoupled from the 11-char filesystem label (the *mount* convention).
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
pub struct CourierManifest {
    pub stick: [u8; 8],
    pub origin: u16,
    pub dest: u16,
    /// blake3 root of the carried bundle.
    pub root: [u8; 32],
    pub n_chunks: u16,
    pub total_len: u32,
    pub epoch: u32,
}

/// Tier-4 delivery receipt, gossiped back over radio so the origin learns of
/// delivery at radio speed. Store-and-forward closes its loop.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
pub struct CourierReceipt {
    pub stick: [u8; 8],
    pub root: [u8; 32],
    pub accepted: bool,
}
