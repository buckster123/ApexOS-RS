//! # ApexNET brainstem — real silicon on the wire
//!
//! The ESP32-S3 side of the UART to the Pi cortex (`docs/apexnet.md` §5).
//! This slice replaces `apexos-brainstem-sim` with actual hardware speaking
//! the actual wire: the firmware links **`apexos-mesh-proto`** — the same
//! crate the Pi bridge and agentd use — so a frame the brainstem emits
//! cannot drift from what the bridge parses. One codec, both ends of the
//! link, enforced by the compiler.
//!
//! Five embassy tasks over the USB-Serial-JTAG peripheral (split rx/tx):
//! - **heartbeat**: a `Payload::Heartbeat` every `HEARTBEAT_MS`, carrying
//!   uptime, `cortex_up` (do we hear the Pi?), and the brainstem's own
//!   connectivity byte.
//! - **inbound**: deframes everything the cortex sends with the same
//!   `Deframer` the bridge runs — bounded buffer, poison-frame advance,
//!   COBS resync — answers `flags & FLAG_ACK_REQUESTED` with an `Ack`, and
//!   applies the provisioning rule below. Any inbound frame marks the cortex
//!   up; silence past `CORTEX_TIMEOUT_MS` marks it down again (the brainstem
//!   outlives the cortex — principle 1).
//! - **tx**: the single owner of the TX half.
//! - **status**: periodic `BrainstemStatus` — counter and queue state.
//! - **counter**: keeps the flash counter reservation ahead of consumption.
//!
//! **Unsealed on this link, by design.** `MeshFrame.ct` carries a plain
//! `postcard(PlainPacket)` here: this is a physical wire between a board and
//! its own Pi, the bridge is PSK-free (it treats `ct` as opaque), and the
//! crypto envelope belongs to the radio tiers + the router. Charter §0.4's
//! "every inbound RADIO payload is authenticated" is not weakened — no
//! radio is involved yet.
//!
//! ## Commissioning (the identity this board keeps)
//!
//! The board boots anonymous. A `Payload::Provision` down the wired link
//! gives it a `node_id` and the colony PSK, both persisted to the `apexnet`
//! flash partition so they survive its own power cycles *and* the cortex's
//! absence (charter §0.1). Acceptance is asymmetric and enforced here:
//!
//! - **Un-commissioned** ⇒ an unsealed provision is honoured. Trust on first
//!   use over a physical wire: whoever holds the UART can already reflash
//!   the board, so refusing them buys nothing.
//! - **Commissioned** ⇒ only a provision that arrived *sealed under the
//!   current key* is honoured, which makes rotation authenticated and makes
//!   a stranger on the wire unable to re-key a live board.
//!
//! A provision is **never** honoured from a radio tier. A PSK on the air is
//! the one thing this protocol must not do.
//!
//! Counters come from [`brainstem::counter`], which refuses to hand out a
//! value flash has not already promised — see that module for why a dropped
//! frame beats a repeated nonce.
//!
//! Not in the workspace: `cargo build --release` here needs the `esp`
//! toolchain (`. ~/export-esp.sh`); see `firmware/README.md`.

#![no_std]
#![no_main]
#![deny(
    clippy::mem_forget,
    reason = "mem::forget is generally not safe to do with esp_hal types, especially those \
    holding buffers for the duration of a data transfer."
)]
#![deny(clippy::large_stack_frames)]

use embassy_executor::Spawner;
use embassy_futures::select::{select, Either};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Channel;
use embassy_sync::mutex::Mutex;
use embassy_time::{Duration, Instant, Timer};
use esp_backtrace as _;
use esp_bootloader_esp_idf::partitions::{self, PARTITION_TABLE_MAX_LEN};
use esp_hal::clock::CpuClock;
use esp_hal::timer::timg::TimerGroup;
use esp_hal::usb_serial_jtag::{UsbSerialJtag, UsbSerialJtagRx, UsbSerialJtagTx};
use esp_hal::Async;
use esp_storage::FlashStorage;
use static_cell::StaticCell;

use apexos_mesh_proto::{
    decide_radio_inbound, encode_frame, Deframer, InboxTable, MeshClass, MeshFrame, Payload,
    PlainPacket, RadioInbound, BROADCAST, DEFAULT_HOP_LIMIT, FLAG_ACK_REQUESTED, WIRE_VERSION,
};
use brainstem::{counter, neighbors::Neighbors, radio::Radio, store};

extern crate alloc;

esp_bootloader_esp_idf::esp_app_desc!();

/// Node id used before the cortex has ever commissioned this board. It is
/// deliberately [`BROADCAST`]-adjacent nonsense: an un-commissioned brainstem
/// still beats (so a human can see it is alive) but announces that it has no
/// name yet, rather than impersonating a real colony node.
const UNPROVISIONED_NODE_ID: u16 = 0;

const HEARTBEAT_MS: u64 = 1000;
/// How often the brainstem reports queue/counter state up the wired link.
const STATUS_MS: u64 = 5_000;
/// No inbound frame for this long ⇒ the cortex is not listening. The
/// brainstem keeps beating regardless — it survives the cortex.
const CORTEX_TIMEOUT_MS: u64 = 5_000;
/// Top up the counter reservation once fewer than this many remain, so the
/// flash write happens well before [`counter::try_next`] would start
/// refusing.
const CTR_LOW_WATER: u64 = 128;
/// How often the radio's advertised heartbeat is refreshed. Slower than the
/// wired beat: the controller repeats the payload every advertising interval
/// on its own, so this only sets how fresh `uptime_s` is.
const RADIO_BEAT_MS: u64 = 2_000;

/// Shared across tasks: last inbound frame time and this board's id.
/// Single-core executor, so plain atomics are enough; `AtomicU64` is
/// available on Xtensa via `portable-atomic`'s critical-section support.
use portable_atomic::{AtomicU16, AtomicU64, AtomicU8, Ordering};
static LAST_INBOUND_MS: AtomicU64 = AtomicU64::new(0);
static NODE_ID: AtomicU16 = AtomicU16::new(UNPROVISIONED_NODE_ID);

fn now_ms() -> u64 {
    Instant::now().as_millis()
}

/// Wrap a payload in the wire's frame shape. Unsealed on this link (see the
/// module docs): `ct` is `postcard(PlainPacket)`, which the PSK-free bridge
/// passes through untouched.
///
/// Returns `None` when no counter is available — see [`counter::try_next`].
/// Dropping the frame is the correct failure: emitting one with a reused
/// counter would be a nonce collision.
fn frame_for(payload: Payload, class: MeshClass, target: u16, flags: u8) -> Option<MeshFrame> {
    let packet = PlainPacket {
        target,
        hop_limit: 1,
        flags,
        payload,
    };
    let ct = postcard::to_allocvec(&packet).ok()?;
    Some(MeshFrame {
        ver: WIRE_VERSION,
        class,
        sender: NODE_ID.load(Ordering::Relaxed),
        ctr: counter::try_next()?,
        ct,
    })
}

/// Outbound frames funnel through ONE owner of the TX half (the peripheral
/// can't be aliased). Bounded — a wedged host drops frames instead of
/// growing a queue: gossip is lossy by design, the next heartbeat is 1 s
/// away, and the brainstem must never stall on a cortex that isn't reading.
static TX_QUEUE: Channel<CriticalSectionRawMutex, MeshFrame, 8> = Channel::new();

/// Queue a frame for transmission; drops it (rather than blocking) when the
/// queue is full.
fn enqueue(frame: MeshFrame) {
    let _ = TX_QUEUE.try_send(frame);
}

/// Host accepted `(radio_sender, radio_ctr)` — radio task ACKs on the air.
static HOST_ACCEPT: Channel<CriticalSectionRawMutex, (u16, u64), 8> = Channel::new();

/// Forward an inbound radio packet up the USB with its original provenance
/// so the cortex can accept that exact pair.
fn enqueue_up(sender: u16, ctr: u64, ct: alloc::vec::Vec<u8>) {
    enqueue(MeshFrame {
        ver: WIRE_VERSION,
        class: MeshClass::Gossip,
        sender,
        ctr,
        ct,
    });
}

/// The persistent store, shared by the tasks that write it (provisioning and
/// counter reservation). Both are rare, so a single mutex is cheaper than a
/// dedicated owner task.
type StoreMutex =
    Mutex<CriticalSectionRawMutex, store::Store<partitions::FlashRegion<'static, FlashStorage<'static>>>>;

/// Identity is read by the radio (to seal) and written by the wired link (on
/// provisioning), so it outgrew being a task-local.
type IdentityMutex = Mutex<CriticalSectionRawMutex, store::Identity>;
static IDENTITY: StaticCell<IdentityMutex> = StaticCell::new();

/// Radio neighbours currently alive — the heartbeat's connectivity byte and
/// `BrainstemStatus` both report it, so it lives where both can see it.
static NEIGHBOR_COUNT: AtomicU8 = AtomicU8::new(0);

/// Messages waiting in the flash outbox. Tracked in RAM because counting them
/// means walking flash; seeded once at boot from the real queue.
static QUEUE_DEPTH: AtomicU16 = AtomicU16::new(0);

/// How long to keep a queued message on the air before re-sending it. Gossip
/// is lossy and unacknowledged sends are cheap, so this is a re-send timer,
/// not a failure timer.
const OUTBOX_RETRY_MS: u64 = 4_000;

static FLASH: StaticCell<FlashStorage<'static>> = StaticCell::new();
static PT_BUF: StaticCell<[u8; PARTITION_TABLE_MAX_LEN]> = StaticCell::new();
static STORE: StaticCell<StoreMutex> = StaticCell::new();

#[embassy_executor::task]
async fn tx_task(mut tx: UsbSerialJtagTx<'static, Async>) {
    use embedded_io_async::Write;
    loop {
        let frame = TX_QUEUE.receive().await;
        if let Ok(wire) = encode_frame(&frame) {
            // A blocked/absent host must never wedge the brainstem: bounded
            // writes, and a timeout just drops this frame.
            let _ =
                embassy_time::with_timeout(Duration::from_millis(500), tx.write_all(&wire)).await;
            let _ = embassy_time::with_timeout(Duration::from_millis(200), tx.flush()).await;
        }
    }
}

#[embassy_executor::task]
async fn heartbeat_task() {
    loop {
        let uptime_s = (now_ms() / 1000) as u32;
        let cortex_up = now_ms().saturating_sub(LAST_INBOUND_MS.load(Ordering::Relaxed))
            < CORTEX_TIMEOUT_MS
            && LAST_INBOUND_MS.load(Ordering::Relaxed) != 0;
        if let Some(frame) = frame_for(
            Payload::Heartbeat {
                uptime_s,
                cortex_up,
                // The brainstem's own view, now that it has a radio to form
                // one: neighbours on the air means it is not alone, even with
                // the cortex gone. Matches agentd's ConnectivityState values
                // (2 = Minimal, 3 = Isolated).
                conn: if NEIGHBOR_COUNT.load(Ordering::Relaxed) > 0 {
                    2
                } else {
                    3
                },
            },
            MeshClass::Gossip,
            BROADCAST,
            0,
        ) {
            enqueue(frame);
        }
        Timer::after(Duration::from_millis(HEARTBEAT_MS)).await;
    }
}

/// How an inbound frame authenticated. The provisioning rule turns on this
/// distinction, so it is a type rather than a bool nobody reads.
enum Inbound {
    /// Opened under the current colony PSK — the sender proved key custody.
    Sealed(PlainPacket),
    /// Plain `postcard` on the wired link. Carries no authentication at all.
    Plain(PlainPacket),
}

/// Prefer the authenticated reading: a commissioned board that can open a
/// frame under its key knows strictly more about it than one that merely
/// parsed it. Unsealed decoding demands an exact fit (no trailing bytes) so
/// ciphertext cannot masquerade as a plain packet.
fn decode_inbound(frame: &MeshFrame, psk: Option<&[u8; 32]>) -> Option<Inbound> {
    if let Some(key) = psk
        && let Ok(packet) = apexos_mesh_proto::open(&apexos_mesh_proto::Psk(*key), frame)
    {
        return Some(Inbound::Sealed(packet));
    }
    let (packet, rest) = postcard::take_from_bytes::<PlainPacket>(&frame.ct).ok()?;
    if !rest.is_empty() {
        return None;
    }
    Some(Inbound::Plain(packet))
}

#[embassy_executor::task]
async fn inbound_task(
    mut rx: UsbSerialJtagRx<'static, Async>,
    store: &'static StoreMutex,
    identity: &'static IdentityMutex,
) {
    use embedded_io_async::Read;
    // The SAME deframer the Pi bridge runs: bounded buffer, poison-frame
    // advance, COBS resync. Line noise costs one frame, never the stream.
    let mut deframer = Deframer::new();
    let mut buf = [0u8; 256];
    loop {
        // USB-Serial-JTAG reads are `Infallible` — the peripheral has no
        // error path (a host that vanishes simply stops delivering, which
        // shows up as silence, and `cortex_up` goes false on its own).
        let Ok(n) = rx.read(&mut buf).await;
        if n == 0 {
            continue;
        }
        LAST_INBOUND_MS.store(now_ms(), Ordering::Relaxed);
        for frame in deframer.push(&buf[..n]) {
            let current = *identity.lock().await;
            let Some(inbound) = decode_inbound(&frame, current.psk.as_ref()) else {
                continue;
            };
            let (packet, sealed) = match inbound {
                Inbound::Sealed(p) => (p, true),
                Inbound::Plain(p) => (p, false),
            };

            if let Payload::Provision { node_id, psk } = &packet.payload {
                // The acceptance rule, in one line: an un-commissioned board
                // trusts the wire; a commissioned one trusts only the key.
                let allowed = if current.is_commissioned() {
                    sealed
                } else {
                    true
                };
                if allowed {
                    let mut guard = store.lock().await;
                    if guard.commission(*node_id, psk).await.is_ok() {
                        let mut ident = identity.lock().await;
                        ident.node_id = Some(*node_id);
                        ident.psk = Some(*psk);
                        NODE_ID.store(*node_id, Ordering::Relaxed);
                    }
                }
                // Provisioning is acknowledged like any other frame below —
                // the cortex learns it landed from the following status
                // frame, which now carries the new node id.
            }

            // The cortex accepted a radio pair. Tell the radio task so it
            // can ACK on the air (SA-2). Any Ack on this cable is a host
            // accept — the radio path never writes Ack onto USB.
            if let Payload::Ack { of_sender, of_ctr } = &packet.payload {
                let _ = HOST_ACCEPT.try_send((*of_sender, *of_ctr));
                continue;
            }

            // A packet the cortex addressed to some OTHER node is ours to
            // carry, not to act on: queue it durably and let the radio deliver
            // it when that peer turns up. This is the whole point of a
            // brainstem outliving its cortex — the Pi can hand off a message
            // and go away.
            let me = NODE_ID.load(Ordering::Relaxed);
            if packet.target != BROADCAST && packet.target != me && me != UNPROVISIONED_NODE_ID {
                if let Ok(bytes) = postcard::to_allocvec(&packet)
                    && bytes.len() <= store::MAX_QUEUED_MESSAGE
                {
                    let mut guard = store.lock().await;
                    if guard.outbox_push(&bytes).await.is_ok() {
                        QUEUE_DEPTH.store(guard.outbox_len().await, Ordering::Relaxed);
                    }
                }
                continue;
            }

            if packet.flags & FLAG_ACK_REQUESTED != 0
                && let Some(ack) = frame_for(
                    Payload::Ack {
                        of_sender: frame.sender,
                        of_ctr: frame.ctr,
                    },
                    MeshClass::Gossip,
                    frame.sender,
                    0,
                )
            {
                enqueue(ack);
            }
        }
    }
}

/// Telemetry up the wired link. The firmware prints nothing after boot (the
/// serial line *is* the wire), so this frame is the only way the cortex sees
/// counter and queue state — including, after a power cycle, that the counter
/// resumed above its old ceiling instead of restarting.
#[embassy_executor::task]
async fn status_task() {
    loop {
        Timer::after(Duration::from_millis(STATUS_MS)).await;
        if let Some(frame) = frame_for(
            Payload::BrainstemStatus {
                node_id: NODE_ID.load(Ordering::Relaxed),
                // The flash store-and-forward queue lands with the radio
                // tier that gives it somewhere to forward to.
                queued: QUEUE_DEPTH.load(Ordering::Relaxed),
                neighbors: NEIGHBOR_COUNT.load(Ordering::Relaxed),
                ctr_hw: counter::ceiling(),
            },
            MeshClass::Gossip,
            BROADCAST,
            0,
        ) {
            enqueue(frame);
        }
    }
}

/// Keeps the counter reservation ahead of consumption. Runs well before the
/// allocator would start refusing, so a healthy board never drops a frame for
/// want of a counter — and an unhealthy one (flash failing) drops frames
/// instead of repeating nonces.
#[embassy_executor::task]
async fn counter_task(store: &'static StoreMutex) {
    loop {
        if counter::remaining() < CTR_LOW_WATER {
            let mut guard = store.lock().await;
            if let Ok((_, ceiling)) = guard.reserve_counters().await {
                counter::raise_ceiling(ceiling);
            }
        }
        Timer::after(Duration::from_secs(10)).await;
    }
}

/// Tier 2a gossip. Advertises a **sealed** heartbeat and listens for the
/// neighbours' — the first frames in this system that are authenticated,
/// because they are the first that ride hostile air (charter §0.4).
///
/// An **un-commissioned brainstem stays off the radio entirely**. It holds no
/// colony key, so it could neither authenticate what it says nor verify what
/// it hears; broadcasting anyway would put an unauthenticated claimant on the
/// air. Silence is the honest state, and the wired link is how it stops being
/// silent.
#[embassy_executor::task]
async fn radio_task(
    ble: esp_radio::ble::controller::BleConnector<'static>,
    identity: &'static IdentityMutex,
    store: &'static StoreMutex,
) {
    let Ok(mut radio) = Radio::new(ble).await else {
        // No radio is a degraded node, not a dead one: the wired link and the
        // heartbeat carry on without it.
        return;
    };
    let mut neighbors = Neighbors::new();
    let (mut replay, replay_blocked) = {
        let mut guard = store.lock().await;
        match guard.load_replay().await {
            Ok(t) => (t, false),
            Err(()) => (apexos_mesh_proto::ReplayTable::new(), true),
        }
    };
    let mut inbox = {
        let mut guard = store.lock().await;
        guard.load_inbox().await.unwrap_or_else(|_| InboxTable::new())
    };
    let mut replay_dirty = false;
    let mut inbox_dirty = false;
    let mut next_beat = Instant::now();
    // The counter the head-of-queue message was last sent under; an Ack must
    // match it to retire that message.
    let mut pending_ctr: Option<u64> = None;
    let mut next_drain = Instant::now();

    loop {
        // Cortex accepted a held pair — now (and only now) ACK on the air.
        while let Ok((of_sender, of_ctr)) = HOST_ACCEPT.try_receive() {
            let _ = inbox.take(of_sender, of_ctr);
            inbox_dirty = true;
            let ident = *identity.lock().await;
            if let (Some(node_id), Some(psk)) = (ident.node_id, ident.psk)
                && let Some(ctr) = counter::try_next()
                && let Ok(ack) = apexos_mesh_proto::seal(
                    &apexos_mesh_proto::Psk(psk),
                    MeshClass::Gossip,
                    node_id,
                    ctr,
                    &PlainPacket {
                        target: of_sender,
                        hop_limit: 1,
                        flags: 0,
                        payload: Payload::Ack {
                            of_sender,
                            of_ctr,
                        },
                    },
                )
            {
                let _ = radio.advertise(&ack).await;
            }
        }

        // Advertising is a standing state, so the timer only decides how often
        // we refresh the payload; between refreshes we are listening.
        let now = Instant::now();
        if now >= next_beat {
            next_beat = now + Duration::from_millis(RADIO_BEAT_MS);
            let ident = *identity.lock().await;
            if let (Some(node_id), Some(psk)) = (ident.node_id, ident.psk) {
                let uptime_s = (now_ms() / 1000) as u32;
                let cortex_up = now_ms().saturating_sub(LAST_INBOUND_MS.load(Ordering::Relaxed))
                    < CORTEX_TIMEOUT_MS
                    && LAST_INBOUND_MS.load(Ordering::Relaxed) != 0;
                let packet = PlainPacket {
                    target: BROADCAST,
                    hop_limit: DEFAULT_HOP_LIMIT,
                    flags: 0,
                    payload: Payload::Heartbeat {
                        uptime_s,
                        cortex_up,
                        conn: if NEIGHBOR_COUNT.load(Ordering::Relaxed) > 0 {
                            2
                        } else {
                            3
                        },
                    },
                };
                // A counter we cannot mint is a frame we must not send: the
                // counter IS the nonce (see brainstem::counter).
                if let Some(ctr) = counter::try_next()
                    && let Ok(frame) = apexos_mesh_proto::seal(
                        &apexos_mesh_proto::Psk(psk),
                        MeshClass::Gossip,
                        node_id,
                        ctr,
                        &packet,
                    )
                {
                    let _ = radio.advertise(&frame).await;
                }
            }
            if replay_dirty {
                let mut guard = store.lock().await;
                if guard.save_replay(&replay).await.is_ok() {
                    replay_dirty = false;
                }
            }
            if inbox_dirty {
                let mut guard = store.lock().await;
                if guard.save_inbox(&inbox).await.is_ok() {
                    inbox_dirty = false;
                }
            }
            // USB retry of anything still waiting on the host (SA-2).
            let cortex_up = LAST_INBOUND_MS.load(Ordering::Relaxed) != 0
                && now_ms().saturating_sub(LAST_INBOUND_MS.load(Ordering::Relaxed))
                    < CORTEX_TIMEOUT_MS;
            if cortex_up {
                for slot in inbox.iter() {
                    enqueue_up(slot.sender, slot.ctr, slot.packet().to_vec());
                }
            }
        }

        // Drain the outbox when its target is actually on the air. Sending
        // into the void would burn counters and prove nothing.
        if QUEUE_DEPTH.load(Ordering::Relaxed) > 0 && Instant::now() >= next_drain {
            let ident = *identity.lock().await;
            if let (Some(node_id), Some(psk)) = (ident.node_id, ident.psk) {
                let mut msg = [0u8; store::MAX_QUEUED_MESSAGE];
                let head = {
                    let mut guard = store.lock().await;
                    guard.outbox_peek(&mut msg).await
                };
                if let Some(len) = head
                    && let Ok((packet, _)) =
                        postcard::take_from_bytes::<PlainPacket>(&msg[..len])
                    && neighbors.is_alive(packet.target, now_ms())
                    && let Some(ctr) = counter::try_next()
                    && let Ok(frame) = apexos_mesh_proto::seal(
                        &apexos_mesh_proto::Psk(psk),
                        MeshClass::Gossip,
                        node_id,
                        ctr,
                        &packet,
                    )
                {
                    if radio.advertise(&frame).await.is_ok() {
                        pending_ctr = Some(ctr);
                    }
                    // Hold it on the air long enough to be heard, then let the
                    // heartbeat resume; re-send if no ack arrives.
                    next_drain = Instant::now() + Duration::from_millis(OUTBOX_RETRY_MS);
                    next_beat = Instant::now() + Duration::from_millis(1_500);
                }
            }
        }

        match select(
            Timer::at(next_beat.min(next_drain)),
            radio.next_frame(),
        )
        .await
        {
            Either::First(_) => {}
            Either::Second(Some(heard)) => {
                let ident = *identity.lock().await;
                let Some(psk) = ident.psk else { continue };
                // Ignore our own broadcasts, and anything that does not open
                // under the colony key — on the air, "unauthenticated" and
                // "not ours" are the same answer: drop it.
                if Some(heard.frame.sender) == ident.node_id {
                    continue;
                }
                if apexos_mesh_proto::open(&apexos_mesh_proto::Psk(psk), &heard.frame).is_err() {
                    continue;
                }
                // Replay check AFTER authentication: an unauthenticated frame
                // must never be able to advance a peer's window.
                let Ok(packet) =
                    apexos_mesh_proto::open(&apexos_mesh_proto::Psk(psk), &heard.frame)
                else {
                    continue;
                };
                if replay_blocked {
                    continue;
                }
                match replay.accept(heard.frame.sender, heard.frame.ctr) {
                    apexos_mesh_proto::ReplayAdmit::Fresh => {
                        neighbors.heard(heard.frame.sender, heard.rssi_dbm, now_ms());
                        replay_dirty = true;
                    }
                    _ => continue,
                }
                NEIGHBOR_COUNT.store(neighbors.alive(now_ms()) as u8, Ordering::Relaxed);

                match &packet.payload {
                    // Our queued message landed: it is safe to forget it.
                    Payload::Ack { of_sender, of_ctr }
                        if Some(*of_sender) == ident.node_id
                            && Some(*of_ctr) == pending_ctr =>
                    {
                        let mut guard = store.lock().await;
                        if guard.outbox_pop().await {
                            QUEUE_DEPTH.store(guard.outbox_len().await, Ordering::Relaxed);
                        }
                        pending_ctr = None;
                        next_drain = Instant::now();
                    }
                    // Addressed to us: hold it until the cortex durably
                    // accepts, then ACK (SA-2). A try_send drop is no
                    // longer an implicit delivery.
                    _ if packet.target == ident.node_id.unwrap_or(UNPROVISIONED_NODE_ID) => {
                        let Ok(ct) = postcard::to_allocvec(&packet) else {
                            continue;
                        };
                        match decide_radio_inbound(
                            &mut replay,
                            &mut inbox,
                            heard.frame.sender,
                            heard.frame.ctr,
                            &ct,
                        ) {
                            RadioInbound::Deliver => {
                                replay_dirty = true;
                                inbox_dirty = true;
                                enqueue_up(heard.frame.sender, heard.frame.ctr, ct);
                            }
                            RadioInbound::WaitHost => {}
                            RadioInbound::ReAck => {
                                if let Some(ctr) = counter::try_next()
                                    && let Ok(ack) = apexos_mesh_proto::seal(
                                        &apexos_mesh_proto::Psk(psk),
                                        MeshClass::Gossip,
                                        ident.node_id.unwrap_or(UNPROVISIONED_NODE_ID),
                                        ctr,
                                        &PlainPacket {
                                            target: heard.frame.sender,
                                            hop_limit: 1,
                                            flags: 0,
                                            payload: Payload::Ack {
                                                of_sender: heard.frame.sender,
                                                of_ctr: heard.frame.ctr,
                                            },
                                        },
                                    )
                                {
                                    let _ = radio.advertise(&ack).await;
                                }
                            }
                            RadioInbound::Drop => {}
                        }
                    }
                    _ => {}
                }
            }
            Either::Second(None) => {
                Timer::after(Duration::from_millis(100)).await;
            }
        }
        NEIGHBOR_COUNT.store(neighbors.alive(now_ms()) as u8, Ordering::Relaxed);
    }
}

#[allow(
    clippy::large_stack_frames,
    reason = "it's not unusual to allocate larger buffers etc. in main"
)]
#[esp_rtos::main]
async fn main(spawner: Spawner) -> ! {
    let config = esp_hal::Config::default().with_cpu_clock(CpuClock::max());
    let peripherals = esp_hal::init(config);

    esp_alloc::heap_allocator!(#[esp_hal::ram(reclaimed)] size: 73744);

    let timg0 = TimerGroup::new(peripherals.TIMG0);
    let sw_interrupt =
        esp_hal::interrupt::software::SoftwareInterruptControl::new(peripherals.SW_INTERRUPT);
    esp_rtos::start(timg0.timer0, sw_interrupt.software_interrupt0);

    // The USB-Serial-JTAG peripheral IS the link to the cortex. Split so the
    // heartbeat and inbound tasks own their halves. NOTE: this is also where
    // esp-println would print — so this firmware deliberately prints NOTHING
    // after boot. Its telemetry IS the heartbeat stream (uptime, cortex_up,
    // conn); stray log text would just be decode_fails on the bridge.
    let usb = UsbSerialJtag::new(peripherals.USB_DEVICE).into_async();
    let (rx, tx) = usb.split();

    // Persistent state lives in the dedicated `apexnet` partition. Everything
    // here is 'static because embassy tasks outlive `main`'s stack frame.
    let flash = FLASH.init(FlashStorage::new(peripherals.FLASH));
    let pt_buf = PT_BUF.init([0u8; PARTITION_TABLE_MAX_LEN]);
    let table = partitions::read_partition_table(flash, pt_buf).expect("partition table");
    let entry = table
        .find_partition(partitions::PartitionType::Data(
            partitions::DataPartitionSubType::Undefined,
        ))
        .expect("partition table readable")
        .expect(
            "no `apexnet` data partition — flash with \
             `espflash flash --partition-table partitions.csv` (see firmware/README.md)",
        );
    // A wrong-sized partition means the table on the board is not the one this
    // firmware was built against; writing records into it would corrupt
    // whatever else lives there. Refuse loudly instead.
    assert_eq!(
        entry.len(),
        store::APEXNET_PARTITION_LEN,
        "apexnet partition size does not match partitions.csv"
    );
    let region = entry.as_embedded_storage(flash);
    let store_ref: &'static StoreMutex = STORE.init(Mutex::new(store::Store::new(region)));

    // Boot-time counter discipline: resume ABOVE the previous ceiling, then
    // reserve a fresh block before a single frame goes out.
    let identity_ref: &'static IdentityMutex = IDENTITY.init(Mutex::new(store::Identity::default()));
    {
        let mut guard = store_ref.lock().await;
        let identity = guard.identity().await;
        *identity_ref.lock().await = identity;
        if let Some(id) = identity.node_id {
            NODE_ID.store(id, Ordering::Relaxed);
        }
        match guard.reserve_counters().await {
            Ok((previous, ceiling)) => counter::init(previous, ceiling),
            // Torn high-water: stay silent. NEXT=0/CEILING=0 refuses TX
            // rather than reminting ctr=1 under the colony key (SA-1).
            Err(()) => {}
        }
        // Whatever the outbox held before the power cut is still there.
        QUEUE_DEPTH.store(guard.outbox_len().await, Ordering::Relaxed);
    }

    spawner.spawn(tx_task(tx).expect("tx task pool"));
    spawner.spawn(heartbeat_task().expect("heartbeat task pool"));
    spawner.spawn(inbound_task(rx, store_ref, identity_ref).expect("inbound task pool"));
    spawner.spawn(status_task().expect("status task pool"));
    spawner.spawn(counter_task(store_ref).expect("counter task pool"));

    // Tier 2a. The BT peripheral is handed over wholesale — nothing else in
    // this firmware touches the radio.
    match esp_radio::ble::controller::BleConnector::new(peripherals.BT, Default::default()) {
        Ok(ble) => {
            spawner.spawn(radio_task(ble, identity_ref, store_ref).expect("radio task pool"));
        }
        Err(_) => {
            // A brainstem whose radio will not initialise still beats on the
            // wire. Degraded, not dead.
        }
    }

    loop {
        Timer::after(Duration::from_secs(60)).await;
    }
}
