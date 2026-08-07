//! The Bulk-lane chunker (v2 §3.4): content-addressed — blob → blake3 root +
//! fixed-size chunks. `ChunkAnnounce` advertises, receivers pull missing
//! indexes, resume-by-hash is free. The same mechanism serves soul.md diffs,
//! memory reconciliation, code distribution, and (very slowly) a courier
//! stick — a courier is a transport, not a special case.

use alloc::vec::Vec;

use crate::types::Payload;
use crate::Error;

/// Default chunk payload size — sized for a GATT notification with envelope
/// headroom. Tier-1 pulls may use much larger chunks (up to `u16::MAX`).
pub const DEFAULT_CHUNK_SIZE: usize = 256;

/// blake3 root of a blob — the content address everything keys on.
pub fn blob_root(data: &[u8]) -> [u8; 32] {
    *blake3::hash(data).as_bytes()
}

/// A chunked blob, ready to announce and serve.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChunkSet {
    pub root: [u8; 32],
    pub total_len: u32,
    pub chunk_len: u16,
    pub chunks: Vec<Vec<u8>>,
}

/// Split a blob into `chunk_len`-sized chunks (last one ragged). Bounds:
/// `chunk_len` in `1..=u16::MAX`, blob ≤ `u32::MAX` bytes, ≤ `u16::MAX`
/// chunks — the wire carries these as `u16`/`u32`.
pub fn chunk_blob(data: &[u8], chunk_len: usize) -> Result<ChunkSet, Error> {
    if chunk_len == 0 || chunk_len > u16::MAX as usize {
        return Err(Error::ChunkSize);
    }
    if data.len() > u32::MAX as usize {
        return Err(Error::TooLarge);
    }
    let chunks: Vec<Vec<u8>> = data.chunks(chunk_len).map(|c| c.to_vec()).collect();
    if chunks.len() > u16::MAX as usize {
        return Err(Error::ChunkIndex);
    }
    Ok(ChunkSet {
        root: blob_root(data),
        total_len: data.len() as u32,
        chunk_len: chunk_len as u16,
        chunks,
    })
}

impl ChunkSet {
    pub fn n_chunks(&self) -> u16 {
        self.chunks.len() as u16
    }

    /// The advertisement for this blob.
    pub fn announce(&self) -> Payload {
        Payload::ChunkAnnounce {
            root: self.root,
            n_chunks: self.n_chunks(),
            total_len: self.total_len,
            chunk_len: self.chunk_len,
        }
    }

    /// Serve one chunk (`None` if out of range).
    pub fn data(&self, index: u16) -> Option<Payload> {
        self.chunks.get(index as usize).map(|c| Payload::ChunkData {
            root: self.root,
            index,
            data: c.clone(),
        })
    }
}

/// Receiver-side reassembly for one announced blob. Every accepted chunk is
/// length-checked against the announced geometry (allocation stays bounded by
/// what actually arrives, never by what an attacker announces); the final
/// blob must hash to the announced root or [`Error::RootMismatch`].
#[derive(Debug, Clone)]
pub struct Reassembler {
    root: [u8; 32],
    total_len: u32,
    chunk_len: u16,
    slots: Vec<Option<Vec<u8>>>,
}

impl Reassembler {
    /// Start reassembly from announced geometry. Rejects inconsistent
    /// announcements up front: the chunk grid must tile `total_len` exactly
    /// (`n_chunks == ceil(total_len / chunk_len)`, zero-length blobs have
    /// zero chunks).
    pub fn new(
        root: [u8; 32],
        n_chunks: u16,
        total_len: u32,
        chunk_len: u16,
    ) -> Result<Self, Error> {
        if chunk_len == 0 {
            return Err(Error::ChunkSize);
        }
        let expect_chunks = (total_len as u64).div_ceil(chunk_len as u64);
        if expect_chunks != n_chunks as u64 {
            return Err(Error::ChunkSize);
        }
        let mut slots = Vec::new();
        slots.resize(n_chunks as usize, None);
        Ok(Self {
            root,
            total_len,
            chunk_len,
            slots,
        })
    }

    /// Start from a [`Payload::ChunkAnnounce`] (`None` for other variants).
    pub fn from_announce(p: &Payload) -> Option<Result<Self, Error>> {
        match p {
            Payload::ChunkAnnounce {
                root,
                n_chunks,
                total_len,
                chunk_len,
            } => Some(Self::new(*root, *n_chunks, *total_len, *chunk_len)),
            _ => None,
        }
    }

    pub fn root(&self) -> &[u8; 32] {
        &self.root
    }

    /// Expected byte length of chunk `index` under the announced geometry.
    fn expected_len(&self, index: u16) -> usize {
        let last = self.slots.len() - 1; // slots is never empty when called
        if index as usize == last {
            let full = (last as u64) * (self.chunk_len as u64);
            (self.total_len as u64 - full) as usize
        } else {
            self.chunk_len as usize
        }
    }

    /// Accept one chunk. `Ok(true)` = newly filled, `Ok(false)` = idempotent
    /// duplicate. Errors: wrong root ([`Error::RootMismatch`] — not our
    /// blob), bad index, wrong length for the grid, or conflicting data for
    /// an index already held.
    pub fn accept(&mut self, root: &[u8; 32], index: u16, data: &[u8]) -> Result<bool, Error> {
        if *root != self.root {
            return Err(Error::RootMismatch);
        }
        if index as usize >= self.slots.len() {
            return Err(Error::ChunkIndex);
        }
        if data.len() != self.expected_len(index) {
            return Err(Error::ChunkSize);
        }
        match &self.slots[index as usize] {
            Some(held) if held.as_slice() == data => Ok(false),
            Some(_) => Err(Error::ChunkConflict),
            None => {
                self.slots[index as usize] = Some(data.to_vec());
                Ok(true)
            }
        }
    }

    /// Indexes still missing — the pull list.
    pub fn missing(&self) -> Vec<u16> {
        self.slots
            .iter()
            .enumerate()
            .filter(|(_, s)| s.is_none())
            .map(|(i, _)| i as u16)
            .collect()
    }

    pub fn is_complete(&self) -> bool {
        self.slots.iter().all(|s| s.is_some())
    }

    /// Concatenate and verify against the announced root. Consumes the
    /// reassembler; [`Error::Incomplete`] if chunks are missing.
    pub fn finish(self) -> Result<Vec<u8>, Error> {
        if !self.is_complete() {
            return Err(Error::Incomplete);
        }
        let mut blob = Vec::with_capacity(self.total_len as usize);
        for slot in &self.slots {
            blob.extend_from_slice(slot.as_ref().expect("checked complete"));
        }
        if blob_root(&blob) != self.root {
            return Err(Error::RootMismatch);
        }
        Ok(blob)
    }
}
