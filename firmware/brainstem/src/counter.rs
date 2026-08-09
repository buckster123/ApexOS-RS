//! The counter allocator: hands out `ctr` values that are guaranteed never to
//! repeat, across reboots and across power cuts.
//!
//! `(sender, ctr)` is both the mesh dedup key and the AEAD nonce. Repeating
//! one under the same key breaks ChaCha20-Poly1305 outright, so this module
//! has exactly one rule:
//!
//! > **Never hand out a counter above the ceiling that flash has already
//! > promised.**
//!
//! When the reserved block runs dry, [`try_next`] returns `None` and the
//! caller drops the frame. A dropped heartbeat is a nuisance; a repeated
//! nonce is a key compromise. The asymmetry decides the design.

use portable_atomic::{AtomicU64, Ordering};

/// Next counter to hand out.
static NEXT: AtomicU64 = AtomicU64::new(0);
/// Highest counter flash has promised. Handing out above this is forbidden.
static CEILING: AtomicU64 = AtomicU64::new(0);

/// Adopt the boot-time state: counters resume *above* the previous ceiling,
/// abandoning whatever the last boot had reserved but not spent. Wasting
/// counters is free; reusing them is not.
pub fn init(previous_high_water: u64, new_ceiling: u64) {
    NEXT.store(previous_high_water.saturating_add(1), Ordering::SeqCst);
    CEILING.store(new_ceiling, Ordering::SeqCst);
}

/// Raise the ceiling after a successful flash reservation.
pub fn raise_ceiling(new_ceiling: u64) {
    CEILING.fetch_max(new_ceiling, Ordering::SeqCst);
}

/// Take the next counter, or `None` when the reservation is exhausted.
///
/// Counters mint from 1 — `ctr == 0` is the replay window's "nothing seen"
/// floor and never goes on the wire.
pub fn try_next() -> Option<u64> {
    loop {
        let n = NEXT.load(Ordering::SeqCst);
        if n == 0 || n > CEILING.load(Ordering::SeqCst) {
            return None;
        }
        if NEXT
            .compare_exchange(n, n + 1, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
        {
            return Some(n);
        }
    }
}

/// Counters left in the current reservation — the top-up task's trigger.
pub fn remaining() -> u64 {
    CEILING
        .load(Ordering::SeqCst)
        .saturating_sub(NEXT.load(Ordering::SeqCst))
}

/// The current ceiling, for [`apexos_mesh_proto::Payload::BrainstemStatus`].
pub fn ceiling() -> u64 {
    CEILING.load(Ordering::SeqCst)
}
