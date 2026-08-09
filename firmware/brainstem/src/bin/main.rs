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
    encode_frame, Deframer, MeshClass, MeshFrame, Payload, PlainPacket, BROADCAST,
    FLAG_ACK_REQUESTED, WIRE_VERSION,
};
use brainstem::{counter, store};

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

/// Shared across tasks: last inbound frame time and this board's id.
/// Single-core executor, so plain atomics are enough; `AtomicU64` is
/// available on Xtensa via `portable-atomic`'s critical-section support.
use portable_atomic::{AtomicU16, AtomicU64, Ordering};
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

/// The persistent store, shared by the tasks that write it (provisioning and
/// counter reservation). Both are rare, so a single mutex is cheaper than a
/// dedicated owner task.
type StoreMutex =
    Mutex<CriticalSectionRawMutex, store::Store<partitions::FlashRegion<'static, FlashStorage<'static>>>>;

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
                // The brainstem's own view: 3 = Isolated until a radio tier
                // exists to say otherwise (P6). Honest, not optimistic.
                conn: 3,
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
    mut identity: store::Identity,
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
            let Some(inbound) = decode_inbound(&frame, identity.psk.as_ref()) else {
                continue;
            };
            let (packet, sealed) = match inbound {
                Inbound::Sealed(p) => (p, true),
                Inbound::Plain(p) => (p, false),
            };

            if let Payload::Provision { node_id, psk } = &packet.payload {
                // The acceptance rule, in one line: an un-commissioned board
                // trusts the wire; a commissioned one trusts only the key.
                let allowed = if identity.is_commissioned() {
                    sealed
                } else {
                    true
                };
                if allowed {
                    let mut guard = store.lock().await;
                    if guard.commission(*node_id, psk).await.is_ok() {
                        identity.node_id = Some(*node_id);
                        identity.psk = Some(*psk);
                        NODE_ID.store(*node_id, Ordering::Relaxed);
                    }
                }
                // Provisioning is acknowledged like any other frame below —
                // the cortex learns it landed from the following status
                // frame, which now carries the new node id.
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
                queued: 0,
                neighbors: 0,
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
            if let Ok(ceiling) = guard.reserve_counters().await {
                counter::raise_ceiling(ceiling);
            }
        }
        Timer::after(Duration::from_secs(10)).await;
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
    let identity = {
        let mut guard = store_ref.lock().await;
        let identity = guard.identity().await;
        if let Some(id) = identity.node_id {
            NODE_ID.store(id, Ordering::Relaxed);
        }
        let previous = guard.counter_high_water().await;
        let ceiling = guard.reserve_counters().await.unwrap_or(previous);
        counter::init(previous, ceiling);
        identity
    };

    spawner.spawn(tx_task(tx).expect("tx task pool"));
    spawner.spawn(heartbeat_task().expect("heartbeat task pool"));
    spawner.spawn(inbound_task(rx, store_ref, identity).expect("inbound task pool"));
    spawner.spawn(status_task().expect("status task pool"));
    spawner.spawn(counter_task(store_ref).expect("counter task pool"));

    loop {
        Timer::after(Duration::from_secs(60)).await;
    }
}
