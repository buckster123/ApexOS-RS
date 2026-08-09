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
espflash flash --port /dev/ttyACM0 --partition-table partitions.csv \
  target/xtensa-esp32s3-none-elf/release/brainstem
```

**`--partition-table partitions.csv` is not optional.** The board keeps its
identity, colony key and counter high-water in a dedicated `apexnet` data
partition, which the default table does not have. Flash without it and the
firmware panics at boot with a message saying exactly this — deliberately, in
preference to silently forgetting who it is on every power cycle.

## Commissioning a board

A freshly flashed brainstem is anonymous: it beats, but reports `sender=0`
and holds no key. Give it an identity from the Pi that owns it:

```bash
sudo systemctl stop apexos-mesh-bridge          # one reader per UART
apexos-brainstem-provision --port /dev/ttyACM0 --node-id 1001
# → confirmed: board reports node_id=1001, counter high-water 2048
```

The board persists both and confirms with its own telemetry — the tool trusts
the board's word, not its own successful write. Re-provisioning a board that
already holds the key needs `--sealed` (see the acceptance rule below).

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
- **The brainstem outlives the cortex** (principle 1): read errors and a
  silent host never stop the heartbeat; `cortex_up` simply goes false.
- **The provisioning rule is asymmetric, and the board enforces it.** No
  stored key ⇒ an unsealed `Provision` is honoured (trust on first use: whoever
  holds the UART can already reflash the board). Key present ⇒ only a
  provision that *opens under the current key* is honoured, which is what makes
  re-keying authenticated. A `Provision` is never honoured from a radio tier —
  a PSK on the air is the one thing this protocol must not do.
- **Counters are reserved, not recorded.** `(sender, ctr)` is the AEAD nonce,
  so a repeat is a key compromise, not a lost message. Flash holds a *ceiling
  we promise never to exceed*; a reboot resumes above it and abandons whatever
  the last boot left unspent. One flash write buys 1024 counters, and when the
  reservation runs dry the firmware **drops frames rather than reuse a
  counter**. Watch for it: a counter that jumps by ~1024 across a reboot is the
  system working, not a bug.

## The provisioning one-shot is a separate binary on purpose

`apexos-brainstem-provision` lives beside the bridge but is not part of it.
The bridge daemon is PSK-free by law — it runs forever parsing bytes off the
hostile edge, and a parser bug in a process holding the colony key would leak
the colony. The one-shot holds the key for the two seconds a commissioning
takes, then exits.

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
