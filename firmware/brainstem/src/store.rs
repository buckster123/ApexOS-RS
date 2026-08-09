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

use embassy_embedded_hal::adapter::BlockingAsync;
use embedded_storage::nor_flash::NorFlash as BlockingNorFlash;
use sequential_storage::cache::Cache;
use sequential_storage::map::{MapConfig, MapStorage};

/// Size of the `apexnet` partition, mirrored from `partitions.csv`. The
/// firmware checks the table against this at boot and refuses to run on a
/// mismatch rather than quietly writing records into the wrong sectors.
pub const APEXNET_PARTITION_LEN: u32 = 0x20000;

/// How many counters one flash write buys. At the 1 Hz wired heartbeat that
/// is a write every ~17 minutes; the partition wear-levels across 32 sectors,
/// which puts the flash's endurance far beyond the board's service life.
pub const CTR_RESERVATION: u64 = 1024;

const KEY_NODE_ID: u8 = 0;
const KEY_PSK: u8 = 1;
const KEY_CTR_HW: u8 = 2;

/// Big enough for the largest record ([`KEY_PSK`], 32 B) plus key and header,
/// rounded well past flash word alignment.
const BUF_LEN: usize = 128;

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

/// Records are few and tiny, so the store runs uncached: a cache would have
/// to be kept exactly consistent with flash contents (the crate is explicit
/// that a stale one causes "undesirable things"), and it would buy nothing at
/// three records.
type Uncached = Cache<
    sequential_storage::cache::Uncached,
    sequential_storage::cache::Uncached,
    sequential_storage::cache::Uncached,
    u8,
>;

/// The persistent store, parameterised over the blocking flash region so the
/// partition plumbing stays in `main`.
pub struct Store<S: BlockingNorFlash> {
    map: MapStorage<u8, BlockingAsync<S>, Uncached>,
    buf: [u8; BUF_LEN],
}

impl<S: BlockingNorFlash> Store<S> {
    pub fn new(flash: S) -> Self {
        Self {
            map: MapStorage::new(
                BlockingAsync::new(flash),
                MapConfig::new(0..APEXNET_PARTITION_LEN),
                Cache::new_uncached(),
            ),
            buf: [0u8; BUF_LEN],
        }
    }

    /// Read identity from flash. A read failure is reported as "not
    /// commissioned" rather than a panic: a board with an unreadable store
    /// must still boot and still beat — it just cannot talk on the radio,
    /// which is exactly the honest degradation the charter asks for.
    pub async fn identity(&mut self) -> Identity {
        let node_id = self
            .map
            .fetch_item::<u16>(&mut self.buf, &KEY_NODE_ID)
            .await
            .ok()
            .flatten();
        let psk = self
            .map
            .fetch_item::<&[u8]>(&mut self.buf, &KEY_PSK)
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
        self.map
            .store_item(&mut self.buf, &KEY_PSK, &psk.as_slice())
            .await
            .map_err(|_| ())?;
        self.map
            .store_item(&mut self.buf, &KEY_NODE_ID, &node_id)
            .await
            .map_err(|_| ())
    }

    /// Raise the persisted counter ceiling by [`CTR_RESERVATION`] and return
    /// the new ceiling. The write lands *before* any counter in the new block
    /// is used, so a power cut can only ever waste counters, never repeat
    /// them.
    pub async fn reserve_counters(&mut self) -> Result<u64, ()> {
        let current = self
            .map
            .fetch_item::<u64>(&mut self.buf, &KEY_CTR_HW)
            .await
            .ok()
            .flatten()
            .unwrap_or(0);
        let next = current.saturating_add(CTR_RESERVATION);
        self.map
            .store_item(&mut self.buf, &KEY_CTR_HW, &next)
            .await
            .map_err(|_| ())?;
        Ok(next)
    }

    /// The persisted ceiling, without reserving more.
    pub async fn counter_high_water(&mut self) -> u64 {
        self.map
            .fetch_item::<u64>(&mut self.buf, &KEY_CTR_HW)
            .await
            .ok()
            .flatten()
            .unwrap_or(0)
    }
}
