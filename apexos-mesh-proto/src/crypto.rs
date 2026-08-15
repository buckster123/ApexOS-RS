//! The crypto envelope (v2 §3.3/§8): ChaCha20-Poly1305 under the colony PSK,
//! deterministic `(sender, ctr)` nonces, AAD-bound cleartext headers, and the
//! per-sender replay window.
//!
//! Invariants (each locked by a test):
//! - Nonce = `sender_le(2) ++ ctr_le(8) ++ [0, 0]` — unique iff the sender
//!   never reuses a counter, which is why counters are **persisted** by every
//!   sender (agentd: disk; brainstem: flash high-water).
//! - AAD = the 12-byte header encoding below — tampering with `ver`, `class`,
//!   `sender`, or `ctr` breaks the tag.
//! - Counters mint from 1. `ctr == 0` never goes on the wire; it is the
//!   replay window's "nothing seen" floor.

use alloc::vec::Vec;

use chacha20poly1305::aead::{Aead, Payload as AeadPayload};
use chacha20poly1305::{ChaCha20Poly1305, KeyInit};

use crate::types::{MeshClass, MeshFrame, PlainPacket};
use crate::{Error, WIRE_VERSION};

/// The colony-wide pre-shared key (32 B), distributed over Tier 1 / USB only
/// — never a radio tier. No `Debug` derive: keys don't print.
#[derive(Clone)]
pub struct Psk(pub [u8; 32]);

/// The 12-byte AEAD nonce: `sender_le(2) ++ ctr_le(8) ++ [0, 0]`.
pub fn nonce_for(sender: u16, ctr: u64) -> [u8; 12] {
    let mut n = [0u8; 12];
    n[0..2].copy_from_slice(&sender.to_le_bytes());
    n[2..10].copy_from_slice(&ctr.to_le_bytes());
    n
}

/// The 12-byte AAD: `ver ++ class ++ sender_le(2) ++ ctr_le(8)`. Hand-rolled
/// fixed layout — deliberately NOT a serde encoding, so the tag input can
/// never drift with a serializer change.
pub fn header_aad(ver: u8, class: MeshClass, sender: u16, ctr: u64) -> [u8; 12] {
    let mut a = [0u8; 12];
    a[0] = ver;
    a[1] = u8::from(class);
    a[2..4].copy_from_slice(&sender.to_le_bytes());
    a[4..12].copy_from_slice(&ctr.to_le_bytes());
    a
}

/// Seal a packet into a wire-ready [`MeshFrame`] (not yet framed — that's
/// [`crate::encode_frame`]). `ctr` must be freshly minted (from 1, monotonic,
/// persisted); passing 0 errors rather than emitting an illegal frame.
pub fn seal(
    psk: &Psk,
    class: MeshClass,
    sender: u16,
    ctr: u64,
    packet: &PlainPacket,
) -> Result<MeshFrame, Error> {
    if ctr == 0 {
        return Err(Error::ZeroCounter);
    }
    let plain = postcard::to_allocvec(packet).map_err(|_| Error::Postcard)?;
    let cipher = ChaCha20Poly1305::new_from_slice(&psk.0).map_err(|_| Error::Crypto)?;
    let nonce = nonce_for(sender, ctr);
    let ct: Vec<u8> = cipher
        .encrypt(
            (&nonce).into(),
            AeadPayload {
                msg: &plain,
                aad: &header_aad(WIRE_VERSION, class, sender, ctr),
            },
        )
        .map_err(|_| Error::Crypto)?;
    Ok(MeshFrame {
        ver: WIRE_VERSION,
        class,
        sender,
        ctr,
        ct,
    })
}

/// Open a frame: version check, AEAD verify+decrypt (headers as AAD), strict
/// exact-fit postcard decode. Every failure collapses to [`Error::Crypto`] /
/// [`Error::Postcard`] — receivers don't leak *why* a hostile frame died.
pub fn open(psk: &Psk, frame: &MeshFrame) -> Result<PlainPacket, Error> {
    if frame.ver != WIRE_VERSION {
        return Err(Error::Crypto);
    }
    if frame.ctr == 0 {
        return Err(Error::ZeroCounter);
    }
    let cipher = ChaCha20Poly1305::new_from_slice(&psk.0).map_err(|_| Error::Crypto)?;
    let nonce = nonce_for(frame.sender, frame.ctr);
    let plain = cipher
        .decrypt(
            (&nonce).into(),
            AeadPayload {
                msg: &frame.ct[..],
                aad: &header_aad(frame.ver, frame.class, frame.sender, frame.ctr),
            },
        )
        .map_err(|_| Error::Crypto)?;
    let (packet, rest) =
        postcard::take_from_bytes::<PlainPacket>(&plain).map_err(|_| Error::Postcard)?;
    if !rest.is_empty() {
        return Err(Error::Postcard);
    }
    Ok(packet)
}

/// Generic detached-nonce AEAD under the colony PSK — for courier artifacts
/// at rest (charter §7: the stick's `manifest.json`/`receipts.json`), where
/// the `(sender, ctr)` wire-nonce discipline doesn't apply. The caller
/// supplies the nonce (random 12 B per seal — file-at-rest volume makes
/// collision negligible; rotation lands in Phase 8) and the AAD (bind the
/// artifact to its stick + domain so a sealed file can't be replayed onto
/// another stick or another slot).
pub fn seal_blob(psk: &Psk, nonce: &[u8; 12], aad: &[u8], plain: &[u8]) -> Result<Vec<u8>, Error> {
    let cipher = ChaCha20Poly1305::new_from_slice(&psk.0).map_err(|_| Error::Crypto)?;
    cipher
        .encrypt(nonce.into(), AeadPayload { msg: plain, aad })
        .map_err(|_| Error::Crypto)
}

/// Open a [`seal_blob`] artifact. Any tamper — ciphertext, nonce, or AAD
/// (wrong stick, wrong domain, wrong key) — fails closed with
/// [`Error::Crypto`].
pub fn open_blob(psk: &Psk, nonce: &[u8; 12], aad: &[u8], ct: &[u8]) -> Result<Vec<u8>, Error> {
    let cipher = ChaCha20Poly1305::new_from_slice(&psk.0).map_err(|_| Error::Crypto)?;
    cipher
        .decrypt(nonce.into(), AeadPayload { msg: ct, aad })
        .map_err(|_| Error::Crypto)
}

/// Per-sender anti-replay: the classic 64-entry sliding-bitmap window over
/// `ctr`, run by **every** receiver including the brainstem. Accepts each
/// counter at most once; rejects anything older than `highest - 63`.
///
/// State is 16 bytes — cheap enough to keep one per known peer.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReplayWindow {
    highest: u64,
    /// Bit `i` set ⇔ `highest - i` has been seen.
    bitmap: u64,
}

impl ReplayWindow {
    pub const SIZE: u64 = 64;

    pub fn new() -> Self {
        Self::default()
    }

    /// Highest counter accepted so far (0 = nothing yet).
    pub fn highest(&self) -> u64 {
        self.highest
    }

    /// Persist form: `(highest, bitmap)`.
    pub fn parts(self) -> (u64, u64) {
        (self.highest, self.bitmap)
    }

    pub fn from_parts(highest: u64, bitmap: u64) -> Self {
        Self { highest, bitmap }
    }

    /// Check `ctr` against the window and mark it seen if fresh. Returns
    /// `true` exactly once per counter value.
    pub fn check_and_accept(&mut self, ctr: u64) -> bool {
        if ctr == 0 {
            return false; // counters mint from 1
        }
        if ctr > self.highest {
            let shift = ctr - self.highest;
            self.bitmap = if shift >= Self::SIZE {
                0
            } else {
                self.bitmap << shift
            };
            self.bitmap |= 1;
            self.highest = ctr;
            true
        } else {
            let offset = self.highest - ctr;
            if offset >= Self::SIZE {
                return false; // older than the window reaches
            }
            let bit = 1u64 << offset;
            if self.bitmap & bit != 0 {
                false // replay
            } else {
                self.bitmap |= bit;
                true
            }
        }
    }
}

/// How many senders a receiver will remember forever. Full ⇒ refuse the
/// *new* sender; never evict a window (finding 12). Liveness stays a
/// separate, evictable cache.
pub const MAX_REPLAY_SENDERS: usize = 16;

/// Packed flash/disk record: `node_id_le(2) ++ highest_le(8) ++ bitmap_le(8)`.
pub const REPLAY_SLOT_BYTES: usize = 18;

/// Outcome of offering a `(sender, ctr)` to the never-evict table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplayAdmit {
    Fresh,
    Replay,
    TableFull,
}

/// Persistent per-sender replay state. Distinct from the 8-slot RAM
/// neighbour liveness table: this one never forgets, and a full table
/// refuses unknown senders instead of evicting security state.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ReplayTable {
    slots: [Option<(u16, ReplayWindow)>; MAX_REPLAY_SENDERS],
}

impl ReplayTable {
    pub const fn new() -> Self {
        Self {
            slots: [None; MAX_REPLAY_SENDERS],
        }
    }

    pub fn accept(&mut self, node_id: u16, ctr: u64) -> ReplayAdmit {
        if let Some(slot) = self
            .slots
            .iter_mut()
            .find(|s| matches!(s, Some((id, _)) if *id == node_id))
        {
            return if slot.as_mut().unwrap().1.check_and_accept(ctr) {
                ReplayAdmit::Fresh
            } else {
                ReplayAdmit::Replay
            };
        }
        let Some(free) = self.slots.iter_mut().find(|s| s.is_none()) else {
            return ReplayAdmit::TableFull;
        };
        let mut w = ReplayWindow::new();
        if !w.check_and_accept(ctr) {
            return ReplayAdmit::Replay;
        }
        *free = Some((node_id, w));
        ReplayAdmit::Fresh
    }

    pub fn len(&self) -> usize {
        self.slots.iter().flatten().count()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn encode_slot(&self, i: usize) -> Option<[u8; REPLAY_SLOT_BYTES]> {
        let (id, w) = self.slots.get(i)?.as_ref()?;
        Some(encode_replay_slot(*id, *w))
    }

    /// Restore one persisted slot. Occupied slots are filled in order;
    /// returns false if the table is already full or the record is short.
    pub fn load_slot(&mut self, bytes: &[u8]) -> bool {
        let Some((id, w)) = decode_replay_slot(bytes) else {
            return false;
        };
        if self.slots.iter().any(|s| matches!(s, Some((n, _)) if *n == id)) {
            return true;
        }
        if let Some(free) = self.slots.iter_mut().find(|s| s.is_none()) {
            *free = Some((id, w));
            true
        } else {
            false
        }
    }
}

pub fn encode_replay_slot(node_id: u16, window: ReplayWindow) -> [u8; REPLAY_SLOT_BYTES] {
    let mut b = [0u8; REPLAY_SLOT_BYTES];
    b[0..2].copy_from_slice(&node_id.to_le_bytes());
    let (highest, bitmap) = window.parts();
    b[2..10].copy_from_slice(&highest.to_le_bytes());
    b[10..18].copy_from_slice(&bitmap.to_le_bytes());
    b
}

pub fn decode_replay_slot(bytes: &[u8]) -> Option<(u16, ReplayWindow)> {
    if bytes.len() < REPLAY_SLOT_BYTES {
        return None;
    }
    let node_id = u16::from_le_bytes(bytes[0..2].try_into().ok()?);
    let highest = u64::from_le_bytes(bytes[2..10].try_into().ok()?);
    let bitmap = u64::from_le_bytes(bytes[10..18].try_into().ok()?);
    Some((node_id, ReplayWindow::from_parts(highest, bitmap)))
}

/// Map a high-water flash read onto the next reservation.
///
/// `Ok(None)` is first boot (no record). `Err` is a torn/unreadable store
/// and **must not** be treated as zero (SA-1).
pub fn reserve_from_stored(
    stored: Result<Option<u64>, ()>,
    step: u64,
) -> Result<(u64, u64), ()> {
    match stored {
        Err(()) => Err(()),
        Ok(None) => Ok((0, step)),
        Ok(Some(v)) => Ok((v, v.saturating_add(step))),
    }
}

/// Next sealed-provision counter. Missing previous → 1; never returns 0.
pub fn next_provision_ctr(previous: Option<u64>) -> u64 {
    previous.unwrap_or(0).saturating_add(1).max(1)
}
