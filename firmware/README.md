# `firmware/` — the ApexNET brainstem (ESP32)

The nervous system's peripheral half: firmware that keeps talking when the
cortex (the Pi) is rebooting, crashed, or mid-self-evolution. Charter:
`docs/apexnet.md` §5 · phases §9.

**Outside the Cargo workspace on purpose** (`exclude` in the root
`Cargo.toml`): it cross-compiles for Xtensa on the `esp` toolchain and keeps
its own `Cargo.lock`. `cargo build --workspace` on a Pi or laptop never
touches it; nothing here is installed by `install.sh`.

## What's here

| Crate | What it is |
|-------|-----------|
| `brainstem/` | The board-side daemon: heartbeats out, frames in, over USB-Serial-JTAG (LoRa/BLE tiers land in later phases) |

The firmware links **`apexos-mesh-proto`** — the very same crate the Pi
bridge and agentd use. One codec, both ends of the wire: a frame the
brainstem emits cannot drift from what the bridge parses, because the
compiler would have to accept the drift first.

## Toolchain (one-time)

```bash
cargo install espup espflash          # espflash also flashes + monitors
espup install --targets esp32s3       # the Xtensa Rust toolchain
```

`espup` writes `~/export-esp.sh`. **Source it in every shell that builds
firmware** — without it `cargo` uses the host toolchain and the build fails
confusingly:

```bash
. ~/export-esp.sh
```

## Build · flash · watch

```bash
cd firmware/brainstem
. ~/export-esp.sh
cargo build --release
espflash flash --port /dev/ttyACM0 target/xtensa-esp32s3-none-elf/release/brainstem
```

Serial access needs group membership (`sudo usermod -aG dialout $USER`, then
re-login). The board enumerates as `/dev/ttyACM*` (native USB-Serial-JTAG,
`303a:1001`).

**Do not `espflash monitor` a running brainstem** — the firmware speaks
binary frames on that port, not text. Watch it with the real consumer
instead:

```bash
MESH_BRIDGE_DEV=/dev/ttyACM0 MESH_BRIDGE_STATS_SECS=5 \
  cargo run -p apexos-mesh-bridge          # from the repo root
```

Expect one `rx ver=1 class=Gossip sender=1001` line per second and a stats
line with `crc_fail: 0`. A single `decode_fail` when you attach is normal —
that's the half-frame you joined mid-transmission, and the resync working.

## Design notes that are easy to get wrong

- **The firmware prints nothing after boot.** `esp-println` writes to the
  *same* USB-Serial-JTAG peripheral the frames use; log text would land in
  the bridge as `decode_fail`s. The heartbeat stream *is* the telemetry
  (uptime, `cortex_up`, connectivity byte).
- **One owner of the TX half.** The peripheral can't be aliased, so both
  producers (heartbeat, ack) push into a bounded channel and a single
  `tx_task` writes. Full queue drops frames — gossip is lossy by design and
  the brainstem must never stall on a cortex that stopped reading.
- **Unsealed on this link, deliberately.** `ct` carries a plain
  `postcard(PlainPacket)`: this is a physical wire between a board and its
  own Pi, and the bridge is PSK-free by design. Charter §0.4 ("every inbound
  *radio* payload is authenticated") is intact — no radio is involved yet.
  Sealing arrives with the radio tiers and their key-provisioning story.
- **The brainstem outlives the cortex** (principle 1): read errors and a
  silent host never stop the heartbeat; `cortex_up` simply goes false.

## Hardware

Developed on bare **ESP32-S3** devkits — the same MCU as the charter's radio
targets (Heltec WiFi LoRa 32 V3, LilyGo T3-S3), so this firmware carries to
them 1:1 with the radio driver added. Two boards are enough for the BLE
gossip tier; LoRa needs the SX1262 boards.

## Regression safety without hardware

`tools/crates/apexos-mesh-bridge/tests/harness.rs` carries a **hardware
golden vector**: real bytes captured off a running board, asserted to decode
into the expected `Heartbeat`. It runs in CI with no board attached and
fails the day either side's codec drifts. On a wire-version bump, **re-capture
from hardware** rather than hand-editing it.
