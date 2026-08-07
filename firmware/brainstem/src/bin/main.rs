//! # ApexNET brainstem — P4a: real silicon on the wire
//!
//! The ESP32-S3 side of the UART to the Pi cortex (`docs/apexnet.md` §5).
//! This slice replaces `apexos-brainstem-sim` with actual hardware speaking
//! the actual wire: the firmware links **`apexos-mesh-proto`** — the same
//! crate the Pi bridge and agentd use — so a frame the brainstem emits
//! cannot drift from what the bridge parses. One codec, both ends of the
//! link, enforced by the compiler.
//!
//! Two embassy tasks over the USB-Serial-JTAG peripheral (split rx/tx):
//! - **heartbeat**: a `Payload::Heartbeat` every `HEARTBEAT_MS`, carrying
//!   uptime, `cortex_up` (do we hear the Pi?), and the brainstem's own
//!   connectivity byte. Monotonic `ctr` from 1 — the wire's dedup/replay
//!   key (persisting it across reboots lands with the crypto envelope).
//! - **inbound**: deframes everything the cortex sends with the same
//!   `Deframer` the bridge runs — bounded buffer, poison-frame advance,
//!   COBS resync — and answers `flags & FLAG_ACK_REQUESTED` with an `Ack`.
//!   Any inbound frame marks the cortex up; silence past `CORTEX_TIMEOUT_MS`
//!   marks it down again (the brainstem outlives the cortex — principle 1).
//!
//! **Unsealed on this link, by design.** `MeshFrame.ct` carries a plain
//! `postcard(PlainPacket)` here: this is a physical wire between a board and
//! its own Pi, the bridge is PSK-free (it treats `ct` as opaque), and the
//! crypto envelope belongs to the radio tiers + the router. Charter §0.4's
//! "every inbound RADIO payload is authenticated" is not weakened — no
//! radio is involved yet. Sealing arrives with the PSK-provisioning story
//! in the LoRa/BLE phases.
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
use embassy_time::{Duration, Instant, Timer};
use esp_backtrace as _;
use esp_hal::clock::CpuClock;
use esp_hal::timer::timg::TimerGroup;
use esp_hal::usb_serial_jtag::{UsbSerialJtag, UsbSerialJtagRx, UsbSerialJtagTx};
use esp_hal::Async;

use apexos_mesh_proto::{
    encode_frame, Deframer, MeshClass, MeshFrame, Payload, PlainPacket, BROADCAST,
    FLAG_ACK_REQUESTED, WIRE_VERSION,
};

extern crate alloc;

esp_bootloader_esp_idf::esp_app_desc!();

/// This brainstem's node id on the mesh wire. Provisioning (flash-stored,
/// or handed down by the cortex at link-up) lands with the neighbor table;
/// a fixed id is honest for a single-board bench.
const NODE_ID: u16 = 1001;

const HEARTBEAT_MS: u64 = 1000;
/// No inbound frame for this long ⇒ the cortex is not listening. The
/// brainstem keeps beating regardless — it survives the cortex.
const CORTEX_TIMEOUT_MS: u64 = 5_000;

/// Shared across the two tasks: last inbound frame time + the monotonic
/// counter. Single-core executor, so a `critical_section`-free atomic pair
/// is enough; `AtomicU64` is available on Xtensa via the HAL's portable
/// atomic support.
use portable_atomic::{AtomicU64, Ordering};
static LAST_INBOUND_MS: AtomicU64 = AtomicU64::new(0);
static TX_CTR: AtomicU64 = AtomicU64::new(0);

fn now_ms() -> u64 {
    Instant::now().as_millis()
}

/// Mint the next monotonic `(sender, ctr)` counter. Starts at 1 — `ctr == 0`
/// never goes on the wire (the replay window's "nothing seen" floor).
fn next_ctr() -> u64 {
    TX_CTR.fetch_add(1, Ordering::Relaxed) + 1
}

/// Wrap a payload in the wire's frame shape. Unsealed on this link (see the
/// module docs): `ct` is `postcard(PlainPacket)`, which the PSK-free bridge
/// passes through untouched.
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
        sender: NODE_ID,
        ctr: next_ctr(),
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

#[embassy_executor::task]
#[allow(
    clippy::large_stack_frames,
    reason = "an embassy task's 'frame' is its future, held in the static task \
    pool — not a call stack; ~1.2 KB for the rx buffer + deframer is fine on a \
    512 KB-SRAM S3"
)]
async fn inbound_task(mut rx: UsbSerialJtagRx<'static, Async>) {
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
            // Unsealed link: the interior is a plain postcard packet.
            let Ok((packet, _)) = postcard::take_from_bytes::<PlainPacket>(&frame.ct) else {
                continue;
            };
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

    spawner.spawn(tx_task(tx).expect("tx task pool"));
    spawner.spawn(heartbeat_task().expect("heartbeat task pool"));
    spawner.spawn(inbound_task(rx).expect("inbound task pool"));

    loop {
        Timer::after(Duration::from_secs(60)).await;
    }
}
