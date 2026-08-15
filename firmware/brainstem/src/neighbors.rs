//! The neighbour table: who is on the air, and which of their frames we have
//! already accepted.
//!
//! Liveness only — who is on the air right now.
//!
//! Replay state lives in [`apexos_mesh_proto::ReplayTable`] and is persisted
//! in flash. This table is allowed to evict the stalest neighbour because
//! forgetting "I heard you 30s ago" is not a security event. Forgetting a
//! replay window is (finding 12).

/// How many neighbours to track. Well past the number of nodes within radio
/// range of one another in any deployment this charter describes.
pub const MAX_NEIGHBORS: usize = 8;

/// Silence longer than this and a neighbour is presumed gone. Generous
/// against a 1 Hz heartbeat: gossip is lossy, and declaring a peer dead is
/// the kind of claim that should need real evidence.
pub const NEIGHBOR_TIMEOUT_MS: u64 = 30_000;

#[derive(Clone, Copy)]
struct Entry {
    node_id: u16,
    last_seen_ms: u64,
    rssi_dbm: i8,
}

/// A bounded set of radio neighbours with per-sender replay state.
pub struct Neighbors {
    entries: [Option<Entry>; MAX_NEIGHBORS],
}

impl Default for Neighbors {
    fn default() -> Self {
        Self::new()
    }
}

impl Neighbors {
    pub const fn new() -> Self {
        Self {
            entries: [None; MAX_NEIGHBORS],
        }
    }

    /// Note a *fresh* (already replay-checked) frame from this sender.
    pub fn heard(&mut self, node_id: u16, rssi_dbm: i8, now_ms: u64) {
        if let Some(slot) = self.slot_for(node_id, now_ms) {
            let entry = slot.get_or_insert(Entry {
                node_id,
                last_seen_ms: now_ms,
                rssi_dbm,
            });
            entry.last_seen_ms = now_ms;
            entry.rssi_dbm = rssi_dbm;
        }
    }

    /// Neighbours heard within [`NEIGHBOR_TIMEOUT_MS`].
    pub fn alive(&self, now_ms: u64) -> usize {
        self.entries
            .iter()
            .flatten()
            .filter(|e| now_ms.saturating_sub(e.last_seen_ms) < NEIGHBOR_TIMEOUT_MS)
            .count()
    }

    /// Is this specific peer on the air right now? The outbox asks before
    /// spending a counter on a message nobody is there to hear.
    pub fn is_alive(&self, node_id: u16, now_ms: u64) -> bool {
        self.entries.iter().flatten().any(|e| {
            e.node_id == node_id && now_ms.saturating_sub(e.last_seen_ms) < NEIGHBOR_TIMEOUT_MS
        })
    }

    /// Signal strength last heard from a neighbour, if it is still known.
    pub fn rssi(&self, node_id: u16) -> Option<i8> {
        self.entries
            .iter()
            .flatten()
            .find(|e| e.node_id == node_id)
            .map(|e| e.rssi_dbm)
    }

    /// Find this sender's slot, or claim one: first an existing entry, then a
    /// free slot, then the stalest entry. Never fails, so a node that meets
    /// more than `MAX_NEIGHBORS` peers keeps working — it just forgets the
    /// quietest one, which is the one least likely to matter.
    fn slot_for(&mut self, node_id: u16, now_ms: u64) -> Option<&mut Option<Entry>> {
        if let Some(i) = self
            .entries
            .iter()
            .position(|e| matches!(e, Some(x) if x.node_id == node_id))
        {
            return self.entries.get_mut(i);
        }
        if let Some(i) = self.entries.iter().position(|e| e.is_none()) {
            return self.entries.get_mut(i);
        }
        // Evict the stalest. `last_seen_ms` is monotonic, so the minimum is
        // the one we have heard from least recently.
        let (i, _) = self
            .entries
            .iter()
            .enumerate()
            .filter_map(|(i, e)| e.as_ref().map(|x| (i, x.last_seen_ms)))
            .min_by_key(|(_, seen)| *seen)?;
        let _ = now_ms;
        self.entries[i] = None;
        self.entries.get_mut(i)
    }
}
