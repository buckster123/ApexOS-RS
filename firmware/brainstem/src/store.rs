//! Persistent brainstem state: who this board is, the colony key it was
//! handed, and how far its counter has ever been reserved.
//!
//! Lives in the dedicated `apexnet` flash partition (see `partitions.csv`),
//! wear-levelled by `sequential-storage` so the hot record (the counter
//! high-water) can move across 32 sectors for the life of the board.
//!
//! ## Why any of this is persistent
//!
//! Charter §0.1: *the nervous system survives the cortex*. A brainstem that
//! forgets its identity whenever its Pi is unplugged is not a nervous system,
//! it is a peripheral. Identity, key and counter therefore outlive both the
//! cortex and the brainstem's own power rail.
//!
//! ## The counter is the dangerous one
//!
//! `(sender, ctr)` is the AEAD nonce ([`apexos_mesh_proto::crypto`]). Reusing
//! a counter under the same key is a catastrophic, silent break of
//! ChaCha20-Poly1305 — not a dropped message, a leaked keystream. A naive
//! "persist every counter" costs a flash write per heartbeat; a naive "start
//! from 0 each boot" reuses every counter it ever used.
//!
//! So counters are **reserved in blocks**: the high-water written to flash is
//! a *ceiling we promise never to exceed*, not the last value used. A reboot
//! resumes above the old ceiling, skipping whatever the previous boot had
//! left unspent. Cost: one flash write per [`CTR_RESERVATION`] frames.
//! Benefit: a counter can never be handed out twice, including across a power
//! cut mid-write.

use embedded_storage::nor_flash::{
    ErrorType, MultiwriteNorFlash as BlockingMultiwrite, NorFlash as BlockingNorFlash,
    ReadNorFlash as BlockingReadNorFlash,
};
use embedded_storage_async::nor_flash::{
    MultiwriteNorFlash as AsyncMultiwrite, NorFlash as AsyncNorFlash,
    ReadNorFlash as AsyncReadNorFlash,
};
use sequential_storage::cache::Cache;
use sequential_storage::map::{MapConfig, MapStorage};
use sequential_storage::queue::{QueueConfig, QueueStorage};

/// Size of the `apexnet` partition, mirrored from `partitions.csv`. The
/// firmware checks the table against this at boot and refuses to run on a
/// mismatch rather than quietly writing records into the wrong sectors.
pub const APEXNET_PARTITION_LEN: u32 = 0x20000;

/// The partition is split in two, because `sequential-storage`'s map and
/// queue each own their range exclusively and would corrupt each other
/// sharing one.
///
/// Identity is three tiny records plus a counter high-water that moves once
/// per 1024 frames; the outbox is whole messages waiting for a peer. 32 KiB
/// is luxurious for the former and the rest goes to the latter.
///
/// **Changing either boundary invalidates existing boards.** Records written
/// under the old split can land outside the new range, where the store will
/// not find them — a board would silently look un-commissioned. On a change,
/// factory-reset (`espflash erase-region`) and re-commission.
const MAP_RANGE: core::ops::Range<u32> = 0..0x8000;
const QUEUE_RANGE: core::ops::Range<u32> = 0x8000..APEXNET_PARTITION_LEN;

/// How many counters one flash write buys. At the 1 Hz wired heartbeat that
/// is a write every ~17 minutes; the partition wear-levels across 32 sectors,
/// which puts the flash's endurance far beyond the board's service life.
pub const CTR_RESERVATION: u64 = 1024;

const KEY_NODE_ID: u8 = 0;
const KEY_PSK: u8 = 1;
const KEY_CTR_HW: u8 = 2;
/// Replay windows occupy keys `[KEY_REPLAY_BASE, KEY_REPLAY_BASE + MAX)`.
const KEY_REPLAY_BASE: u8 = 16;
/// Radio inbox (SA-2) occupies `[KEY_INBOX_BASE, KEY_INBOX_BASE + MAX_INBOX)`.
const KEY_INBOX_BASE: u8 = 48;

/// Big enough for the largest record ([`KEY_PSK`], 32 B) plus key and header,
/// rounded well past flash word alignment.
/// Scratch for map store/fetch. Must fit an inbox slot (268 B) plus the
/// sequential-storage header — replay slots are 18 B and still fit.
const BUF_LEN: usize = 384;

/// Ceiling on one queued message. A radio frame cannot exceed one extended
/// advertisement anyway, so anything larger could never be delivered on this
/// tier and is refused at the door rather than stored forever.
pub const MAX_QUEUED_MESSAGE: usize = 256;

/// What the board knows about itself at boot.
#[derive(Clone, Copy, Default)]
pub struct Identity {
    pub node_id: Option<u16>,
    pub psk: Option<[u8; 32]>,
}

impl Identity {
    /// A board is commissioned once it has both a name and a key. Until then
    /// it may accept an unsealed [`apexos_mesh_proto::Payload::Provision`]
    /// over the wired link (trust on first use); afterwards, only a sealed
    /// rotation is honoured.
    pub fn is_commissioned(&self) -> bool {
        self.node_id.is_some() && self.psk.is_some()
    }
}


/// Bridges the blocking flash to `sequential-storage`'s async traits, through
/// a mutable borrow.
///
/// Two things forced this rather than `BlockingAsync`: that adapter does not
/// implement `MultiwriteNorFlash` (which `queue::pop` requires), and the
/// async traits have no blanket `&mut T` impl for it either — so the map and
/// the queue could never take turns on one flash handle. Both storages want
/// their flash by value; this hands each a borrow that ends when its view is
/// dropped.
///
/// Flash access is genuinely blocking on this chip, so nothing is lost by not
/// being truly async: these futures simply never yield.
struct FlashRef<'a, F>(&'a mut F);

impl<F: ErrorType> ErrorType for FlashRef<'_, F> {
    type Error = F::Error;
}

impl<F: BlockingReadNorFlash> AsyncReadNorFlash for FlashRef<'_, F> {
    const READ_SIZE: usize = F::READ_SIZE;

    async fn read(&mut self, offset: u32, bytes: &mut [u8]) -> Result<(), Self::Error> {
        self.0.read(offset, bytes)
    }

    fn capacity(&self) -> usize {
        self.0.capacity()
    }
}

impl<F: BlockingNorFlash> AsyncNorFlash for FlashRef<'_, F> {
    const WRITE_SIZE: usize = F::WRITE_SIZE;
    const ERASE_SIZE: usize = F::ERASE_SIZE;

    async fn erase(&mut self, from: u32, to: u32) -> Result<(), Self::Error> {
        self.0.erase(from, to)
    }

    async fn write(&mut self, offset: u32, bytes: &[u8]) -> Result<(), Self::Error> {
        self.0.write(offset, bytes)
    }
}

impl<F: BlockingMultiwrite> AsyncMultiwrite for FlashRef<'_, F> {}

/// The persistent store, parameterised over the blocking flash region so the
/// partition plumbing stays in `main`.
///
/// Owns the flash and lends it to a map view or a queue view per call; the
/// two never exist at once, which is what keeps their ranges honest.
pub struct Store<S: BlockingMultiwrite> {
    flash: S,
    buf: [u8; BUF_LEN],
    /// Scratch for queue operations. A field rather than a local because
    /// `Store` lives in a static, while a local would be duplicated into the
    /// future of every task that awaits an outbox call.
    qbuf: [u8; MAX_QUEUED_MESSAGE],
}

impl<S: BlockingMultiwrite> Store<S> {
    pub fn new(flash: S) -> Self {
        Self {
            flash,
            buf: [0u8; BUF_LEN],
            qbuf: [0u8; MAX_QUEUED_MESSAGE],
        }
    }


    /// Read identity from flash. A read failure is reported as "not
    /// commissioned" rather than a panic: a board with an unreadable store
    /// must still boot and still beat — it just cannot talk on the radio,
    /// which is exactly the honest degradation the charter asks for.
    pub async fn identity(&mut self) -> Identity {
        // Destructured so the flash and the scratch buffer are disjoint
        // borrows; `&mut self` as a whole cannot lend both at once.
        let Self { flash, buf, .. } = self;
        let mut map = MapStorage::new(
            FlashRef(flash),
            MapConfig::new(MAP_RANGE),
            Cache::new_uncached(),
        );
        let node_id = map
            .fetch_item::<u16>(buf, &KEY_NODE_ID)
            .await
            .ok()
            .flatten();
        let psk = map
            .fetch_item::<&[u8]>(buf, &KEY_PSK)
            .await
            .ok()
            .flatten()
            .and_then(|bytes| <[u8; 32]>::try_from(bytes).ok());
        Identity { node_id, psk }
    }

    /// Commission (or rotate). Caller enforces the acceptance rule — this is
    /// the storage layer, not the policy layer.
    ///
    /// The key is written *before* the node id, so a power cut between the
    /// two leaves a board that is still un-commissioned (no id ⇒ not
    /// commissioned ⇒ still accepts a first-touch provision) rather than one
    /// that believes it has an identity it cannot authenticate.
    pub async fn commission(&mut self, node_id: u16, psk: &[u8; 32]) -> Result<(), ()> {
        let Self { flash, buf, .. } = self;
        let mut map = MapStorage::new(
            FlashRef(flash),
            MapConfig::new(MAP_RANGE),
            Cache::new_uncached(),
        );
        map.store_item(buf, &KEY_PSK, &psk.as_slice())
            .await
            .map_err(|_| ())?;
        map.store_item(buf, &KEY_NODE_ID, &node_id)
            .await
            .map_err(|_| ())
    }

    /// Raise the persisted counter ceiling by [`CTR_RESERVATION`].
    /// Returns `(previous, new_ceiling)`. A flash *read* error is `Err`,
    /// never treated as high-water zero (SA-1).
    pub async fn reserve_counters(&mut self) -> Result<(u64, u64), ()> {
        let Self { flash, buf, .. } = self;
        let mut map = MapStorage::new(
            FlashRef(flash),
            MapConfig::new(MAP_RANGE),
            Cache::new_uncached(),
        );
        let fetched = map
            .fetch_item::<u64>(buf, &KEY_CTR_HW)
            .await
            .map_err(|_| ());
        let (previous, next) =
            apexos_mesh_proto::reserve_from_stored(fetched, CTR_RESERVATION)?;
        map.store_item(buf, &KEY_CTR_HW, &next)
            .await
            .map_err(|_| ())?;
        Ok((previous, next))
    }

    /// The persisted ceiling, without reserving more. `Err` on a torn read.
    pub async fn counter_high_water(&mut self) -> Result<u64, ()> {
        let Self { flash, buf, .. } = self;
        let mut map = MapStorage::new(
            FlashRef(flash),
            MapConfig::new(MAP_RANGE),
            Cache::new_uncached(),
        );
        match map.fetch_item::<u64>(buf, &KEY_CTR_HW).await {
            Ok(Some(v)) => Ok(v),
            Ok(None) => Ok(0),
            Err(_) => Err(()),
        }
    }

    /// Load persisted replay windows. Missing keys → empty table.
    /// A torn record fails closed (caller must not accept radio frames).
    pub async fn load_replay(&mut self) -> Result<apexos_mesh_proto::ReplayTable, ()> {
        use apexos_mesh_proto::{ReplayTable, MAX_REPLAY_SENDERS, REPLAY_SLOT_BYTES};
        let Self { flash, buf, .. } = self;
        let mut map = MapStorage::new(
            FlashRef(flash),
            MapConfig::new(MAP_RANGE),
            Cache::new_uncached(),
        );
        let mut table = ReplayTable::new();
        for i in 0..MAX_REPLAY_SENDERS {
            let key = KEY_REPLAY_BASE + i as u8;
            match map.fetch_item::<&[u8]>(buf, &key).await {
                Ok(None) => {}
                Ok(Some(bytes)) if bytes.len() == REPLAY_SLOT_BYTES => {
                    if !table.load_slot(bytes) {
                        return Err(());
                    }
                }
                Ok(Some(_)) => return Err(()),
                Err(_) => return Err(()),
            }
        }
        Ok(table)
    }

    pub async fn load_inbox(&mut self) -> Result<apexos_mesh_proto::InboxTable, ()> {
        use apexos_mesh_proto::{InboxTable, MAX_INBOX};
        let Self { flash, buf, .. } = self;
        let mut map = MapStorage::new(
            FlashRef(flash),
            MapConfig::new(MAP_RANGE),
            Cache::new_uncached(),
        );
        let mut table = InboxTable::new();
        for i in 0..MAX_INBOX {
            let key = KEY_INBOX_BASE + i as u8;
            match map.fetch_item::<&[u8]>(buf, &key).await {
                Ok(None) => {}
                Ok(Some(bytes)) => {
                    if !table.load_slot(bytes) {
                        return Err(());
                    }
                }
                Err(_) => return Err(()),
            }
        }
        Ok(table)
    }

    pub async fn save_inbox(&mut self, table: &apexos_mesh_proto::InboxTable) -> Result<(), ()> {
        use apexos_mesh_proto::MAX_INBOX;
        let Self { flash, buf, .. } = self;
        let mut map = MapStorage::new(
            FlashRef(flash),
            MapConfig::new(MAP_RANGE),
            Cache::new_uncached(),
        );
        for i in 0..MAX_INBOX {
            let key = KEY_INBOX_BASE + i as u8;
            if let Some(bytes) = table.encode_slot(i) {
                let slice: &[u8] = &bytes;
                map.store_item(buf, &key, &slice).await.map_err(|_| ())?;
            }
        }
        Ok(())
    }

    pub async fn save_replay(&mut self, table: &apexos_mesh_proto::ReplayTable) -> Result<(), ()> {
        use apexos_mesh_proto::MAX_REPLAY_SENDERS;
        let Self { flash, buf, .. } = self;
        let mut map = MapStorage::new(
            FlashRef(flash),
            MapConfig::new(MAP_RANGE),
            Cache::new_uncached(),
        );
        for i in 0..MAX_REPLAY_SENDERS {
            let key = KEY_REPLAY_BASE + i as u8;
            if let Some(bytes) = table.encode_slot(i) {
                let slice: &[u8] = &bytes;
                map.store_item(buf, &key, &slice).await.map_err(|_| ())?;
            }
        }
        Ok(())
    }

    /// Queue a message for a peer that is not currently reachable.
    ///
    /// The **plaintext** packet is stored, not a sealed frame. Sealing binds
    /// a counter, and a frame that sits in flash for a day would surface with
    /// a counter far below everything sent since — which every receiver would
    /// correctly reject as a replay. Seal at drain time, with a fresh counter.
    ///
    /// (The plaintext shares a partition with the colony key, so anyone who
    /// can read one can read the other; storing it sealed would buy nothing.)
    pub async fn outbox_push(&mut self, packet: &[u8]) -> Result<(), ()> {
        let mut queue = QueueStorage::new(
            FlashRef(&mut self.flash),
            QueueConfig::new(QUEUE_RANGE),
            Cache::new_uncached(),
        );
        // Never overwrite old data: a full outbox refuses new messages rather
        // than silently dropping ones already promised delivery.
        queue.push(packet, false).await.map_err(|_| ())
    }

    /// The oldest queued message, left in place. Returns its length.
    pub async fn outbox_peek(&mut self, out: &mut [u8]) -> Option<usize> {
        let mut queue = QueueStorage::new(
            FlashRef(&mut self.flash),
            QueueConfig::new(QUEUE_RANGE),
            Cache::new_uncached(),
        );
        queue.peek(out).await.ok().flatten().map(|d| d.len())
    }

    /// Drop the oldest queued message — called only once delivery is
    /// acknowledged, so an unheard message stays queued.
    pub async fn outbox_pop(&mut self) -> bool {
        let Self { flash, qbuf, .. } = self;
        let mut queue = QueueStorage::new(
            FlashRef(flash),
            QueueConfig::new(QUEUE_RANGE),
            Cache::new_uncached(),
        );
        matches!(queue.pop(qbuf).await, Ok(Some(_)))
    }

    /// How many messages are waiting. Walks the queue, so it is called at
    /// boot and then tracked in RAM rather than polled.
    pub async fn outbox_len(&mut self) -> u16 {
        let Self { flash, qbuf, .. } = self;
        let mut queue = QueueStorage::new(
            FlashRef(flash),
            QueueConfig::new(QUEUE_RANGE),
            Cache::new_uncached(),
        );
        let Ok(mut iter) = queue.iter().await else {
            return 0;
        };
        let mut n = 0u16;
        while let Ok(Some(_)) = iter.next(qbuf).await {
            n = n.saturating_add(1);
        }
        n
    }
}
