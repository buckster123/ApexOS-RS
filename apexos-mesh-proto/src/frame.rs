//! The framing layer — the frozen recipe (crate docs) and the incremental
//! [`Deframer`]. This is the layer the fuzz target hammers: raw bytes in,
//! never panic, never unbounded memory, never stall.

use alloc::vec::Vec;

use crate::types::MeshFrame;
use crate::{Error, MAX_WIRE_FRAME};

/// Encode one frame for the wire: postcard → CRC32-LE trailer → COBS →
/// `0x00` delimiter. Errors with [`Error::TooLarge`] if the result would
/// exceed [`MAX_WIRE_FRAME`].
pub fn encode_frame(frame: &MeshFrame) -> Result<Vec<u8>, Error> {
    let mut body = postcard::to_allocvec(frame).map_err(|_| Error::Postcard)?;
    let crc = crc32fast::hash(&body);
    body.extend_from_slice(&crc.to_le_bytes());
    let mut wire = cobs::encode_vec(&body);
    wire.push(0x00);
    if wire.len() > MAX_WIRE_FRAME {
        return Err(Error::TooLarge);
    }
    Ok(wire)
}

/// Decode one delimiter-stripped wire frame (COBS bytes, no trailing `0x00`).
/// Exact-fit: trailing bytes after the postcard value are rejected — a frame
/// can't smuggle a rider.
pub fn decode_frame(wire: &[u8]) -> Result<MeshFrame, Error> {
    let body = cobs::decode_vec(wire).map_err(|_| Error::Cobs)?;
    if body.len() < 5 {
        return Err(Error::Truncated);
    }
    let (payload, trailer) = body.split_at(body.len() - 4);
    let want = u32::from_le_bytes([trailer[0], trailer[1], trailer[2], trailer[3]]);
    if crc32fast::hash(payload) != want {
        return Err(Error::CrcMismatch);
    }
    let (frame, rest) =
        postcard::take_from_bytes::<MeshFrame>(payload).map_err(|_| Error::Postcard)?;
    if !rest.is_empty() {
        return Err(Error::Postcard);
    }
    Ok(frame)
}

/// Hard ceiling on a datagram frame. A BLE extended advertisement carries at
/// most 254 B of AD, and the ApexNET AD structure spends 6 of them on its
/// header, so anything past this could never have gone out over Tier 2a.
pub const MAX_DATAGRAM_FRAME: usize = 248;

/// Encode one frame for a **datagram** link: plain `postcard`, nothing else.
///
/// Framing is a property of the *link*, not of the frame. A UART is a byte
/// stream with no boundaries and no integrity, so it needs COBS delimiting
/// and a CRC32 trailer ([`encode_frame`]). A BLE advertisement is already a
/// bounded packet the link layer CRC-checks and discards on error — adding
/// our own delimiter and checksum there would spend scarce advertising bytes
/// re-solving a solved problem.
///
/// The frame itself is identical on both; only the wrapper differs.
pub fn encode_datagram(frame: &MeshFrame) -> Result<Vec<u8>, Error> {
    let body = postcard::to_allocvec(frame).map_err(|_| Error::Postcard)?;
    if body.len() > MAX_DATAGRAM_FRAME {
        return Err(Error::TooLarge);
    }
    Ok(body)
}

/// Decode one datagram frame. Exact-fit, like every other decode here: a
/// frame cannot smuggle a rider in trailing bytes.
pub fn decode_datagram(bytes: &[u8]) -> Result<MeshFrame, Error> {
    if bytes.len() > MAX_DATAGRAM_FRAME {
        return Err(Error::TooLarge);
    }
    let (frame, rest) =
        postcard::take_from_bytes::<MeshFrame>(bytes).map_err(|_| Error::Postcard)?;
    if !rest.is_empty() {
        return Err(Error::Postcard);
    }
    Ok(frame)
}

/// Receive-side counters (the deframer's half of the v2 §4.3 MUST-6 set;
/// `tx_frames` / `link_downs` live in the bridge, which owns the port).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct DeframerStats {
    /// Frames decoded and yielded.
    pub rx_frames: u64,
    /// CRC trailer mismatches (noise that survived COBS).
    pub crc_fail: u64,
    /// COBS or postcard decode failures, and truncated bodies.
    pub decode_fail: u64,
    /// Times an in-progress buffer was abandoned to rescan for a delimiter
    /// (oversize overflow completing its skip).
    pub resyncs: u64,
    /// Frames dropped for growing past [`MAX_WIRE_FRAME`] undelimited.
    pub oversize_drops: u64,
}

/// Incremental stream deframer: feed arbitrary byte slices, get complete
/// frames. Guarantees, each locked by a test and hammered by the fuzz target:
///
/// - **Bounded memory** — the internal buffer never exceeds
///   [`MAX_WIRE_FRAME`]; past it, bytes are discarded until the next
///   delimiter (MUST-1: a corrupted stream can't buffer gigabytes).
/// - **Poison-frame advance** — a frame that fails COBS/CRC/postcard is
///   counted, dropped, and the scan moves on (MUST-2: never re-parsed
///   forever).
/// - **Trivial resync** — after arbitrary garbage, the next `0x00` restores
///   framing (MUST-5: the COBS property, tested with fault injection).
/// - Consecutive delimiters (idle keepalive / flood) are ignored silently.
#[derive(Debug, Default)]
pub struct Deframer {
    buf: Vec<u8>,
    skipping: bool,
    pub stats: DeframerStats,
}

impl Deframer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Bytes currently buffered awaiting a delimiter (bounded by
    /// [`MAX_WIRE_FRAME`] — asserted by the fuzz target).
    pub fn buffered_len(&self) -> usize {
        self.buf.len()
    }

    /// Feed a chunk of the stream; returns every frame completed by it
    /// (MUST-4: all of them, not just the first).
    pub fn push(&mut self, bytes: &[u8]) -> Vec<MeshFrame> {
        let mut out = Vec::new();
        for &b in bytes {
            if b == 0x00 {
                if self.skipping {
                    // The oversize skip ends here; framing is restored.
                    self.skipping = false;
                    self.stats.resyncs += 1;
                    continue;
                }
                if self.buf.is_empty() {
                    continue;
                }
                match decode_frame(&self.buf) {
                    Ok(frame) => {
                        self.stats.rx_frames += 1;
                        out.push(frame);
                    }
                    Err(Error::CrcMismatch) => self.stats.crc_fail += 1,
                    Err(_) => self.stats.decode_fail += 1,
                }
                self.buf.clear();
            } else if self.skipping {
                // Discard until the next delimiter.
            } else {
                self.buf.push(b);
                // The delimiter is part of the MAX_WIRE_FRAME budget, so a
                // legal frame body is at most MAX_WIRE_FRAME - 1 bytes.
                if self.buf.len() >= MAX_WIRE_FRAME {
                    self.stats.oversize_drops += 1;
                    self.buf.clear();
                    self.skipping = true;
                }
            }
        }
        out
    }
}
