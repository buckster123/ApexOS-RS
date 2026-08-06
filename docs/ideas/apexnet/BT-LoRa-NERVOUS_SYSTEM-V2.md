# ApexOS-RS Nervous System — Offline Resilience Network (v2)

**Status:** Locked for implementation · **Date:** 2026-07-27 · **Supersedes:** `BT-LoRa-NERVOUS_SYSTEM.md` (v1 draft)
**Audience:** FORGE / Claude Code, working in `buckster123/ApexOS-RS`
**Dependency versions in this doc were verified against the live crates.io index on 2026-07-27** — they are real, current releases, not training-data guesses. Re-verify at implementation time anyway (checklist in §11).

---

## 0. Purpose & design principles

Extend the existing ApexOS-RS colony mesh (avahi discovery + `peers.toml`, `send_to_agent`, WS on 8787) with an always-on, routerless radio substrate so the colony stays coherent when Wi-Fi/LAN is down.

Five principles drive every decision below:

1. **The nervous system survives the cortex.** The ESP32 co-processor ("brainstem") stays up while a Pi reboots, crashes, or self-evolves agentd. It buffers heartbeats, store-and-forwards A2A messages, and answers "node alive, cortex restarting" on the mesh. This is a hard requirement, not a nice-to-have — it drives the flash-backed queue, independent power, and firmware autonomy requirements.
2. **Radio carries proofs, not data.** Low-bandwidth tiers announce *what exists* (hashes, digests, alarms); the bytes themselves move when a high-bandwidth window (Wi-Fi / USB Exo-Workspace / GATT bulk lane) opens. Anti-entropy, not sync.
3. **Honesty is mechanical.** Connectivity state is not just prose injected into a prompt — it gates which MCP tools are *exposed* to the agent. Degraded mode means heavy tools are absent, not present-but-failing.
4. **The airwaves are hostile.** BLE advertisements and LoRa are an open mic, and these nodes are unattended, self-modifying agents. Every inbound radio payload is authenticated, replay-protected, and treated as untrusted *data* (never instructions) until it passes the same policy layer everything else does. Non-negotiable before yolo mode touches any of this.
5. **Pure Rust, end to end.** Shared `no_std` wire crate compiled into both the Pi bridge and the ESP32 firmware. One source of truth for the packet format. No C firmware.

---

## 1. Decision log (locked in review round, 2026-07-27)

| # | Decision | Rationale |
|---|----------|-----------|
| D1 | Wire format = **postcard + COBS + CRC32** in a shared `no_std` crate | 3–5× smaller than JSON on radio links; COBS gives unambiguous framing (kills the STX/ETX false-lock resync problem from v1); same types compile on Pi and ESP32 |
| D2 | **Bluetooth SIG Mesh dropped.** Tier 2 = custom Rust BLE advertisement-flood (gossip) + point-to-point GATT (bulk lane) | SIG Mesh costs a C stack (no mature pure-Rust impl), a provisioning ceremony, and caps at ~384 B segmented with very low effective throughput. Custom adv-flood is what Bitchat does anyway, and keeps the stack Rust |
| D3 | Tier 3 LoRa = **native Rust via lora-rs (`lora-phy` v3)** with our own minimal flood MAC | Purity and control over the Meshtastic-sidecar shortcut. Duty-cycle governor is ours to enforce in firmware |
| D4 | **Bitchat interop = stretch-goal adapter** (Phase 6), pinned to a specific spec version, behind `apexos-bitchat-proxy` | The spec has been a moving target; core A2A must not depend on it. Verify current spec from the official repo before building (github.com is in the container allowlist) |

**Changes from v1:** JSON→postcard; STX/ETX length-prefix framing→COBS; SIG Mesh→adv-flood + GATT split; security model added (v1 had none); dream digest reframed from "small diff" to "proof + later reconciliation"; ESP-IDF C firmware→esp-hal/embassy Rust; six concrete codec bugs from the v1 sketch turned into MUST requirements (§4.3).

---

## 2. Network architecture

### 2.1 Tiers

| Tier | Medium | Practical MTU / throughput | Carries |
|------|--------|---------------------------|---------|
| 1 | Wi-Fi AP / LAN (existing) | MBs, ~10–100+ Mbit/s | Inference API, git sync, Exo-Workspace, full cerebro reconciliation. Unchanged — existing agentd mesh |
| 2a | BLE extended advertising flood (gossip) | ≤ ~200 B/packet, hundreds of B/s effective | Heartbeats, alarms, presence, digest announcements, tiny A2A |
| 2b | BLE GATT connection (bulk lane) | ~512 B/notification, tens of KB/s | Chunked soul.md diffs, memory chunks, code patches — point-to-point between radio neighbors |
| 3 | LoRa EU868, SF7–SF12 | ≤ ~51–222 B/packet (SF-dependent), **duty-cycle limited** | Long-range pings, alarms, digest headers, cluster-cohesion beacons |

### 2.2 Message classes → routing policy

| Class | Allowed transports | Policy on send | Queue policy under pressure |
|-------|--------------------|----------------|------------------------------|
| `Critical` (heartbeat-loss alarms, safety events) | ALL available (fan-out) | Send on every up transport simultaneously; receivers dedup by `msg_id` | Never dropped; preempts everything; may use the 10 % LoRa sub-band (§5.4) |
| `Gossip` (heartbeats, presence, small A2A) | 2a, 3, 1 | Cheapest healthy transport; escalate down-tier if unacked past deadline | Drop-oldest beyond TTL |
| `Bulk` (soul.md diffs, chunks, code) | 2b, 1 only — **never flooded, never LoRa** | Queue for next bulk window; announce existence via `Digest` | Drop first when brainstem flash queue fills |
| `Digest` (dream digests, merkle roots) | 2a, 3, 1 | Nightly broadcast; idempotent | Coalesce — only newest per (node, epoch) kept |

Token/inference streaming remains **blocked on all radio tiers** (v1 got this right). If Tier 1 is down, inference is local-only (Standard/Pro tiers) or queued.

---

## 3. Wire protocol — new crate `apexos-mesh-proto`

**Placement:** workspace root, sibling to the existing `apexos-protocol` (that one stays the WS protocol; this one is the radio/UART wire format). `#![no_std]` + `alloc`. Compiled by: `apexos-mesh-bridge` (Pi), brainstem firmware (ESP32), and agentd's router (for types).

### 3.1 Frame pipeline

```
TX:  MeshFrame --postcard--> bytes --append CRC32(LE)--> bytes --COBS encode--> wire ++ 0x00 delimiter
RX:  split on 0x00 --COBS decode--> verify+strip CRC32 --postcard--> MeshFrame
```

- `MAX_WIRE_FRAME = 4096` bytes (UART). Radio MTUs are far smaller — the **chunker** (§3.4) splits above the frame layer, never the framing itself.
- COBS makes resync trivial and unambiguous: scan to next `0x00`. No false-lock on payload bytes — this was a real failure mode of the v1 STX/LEN/ETX scheme.
- postcard's COBS helpers (`to_allocvec_cobs` / `from_bytes_cobs`) are built in; explicit `crc32fast` trailer computed over the postcard bytes. (Alternative: postcard's `use-crc` flavor — implementer's choice, but pick ONE and it's automatically identical on both ends since it's the same crate.)

### 3.2 Types (reference signatures — implementer owns final shape)

```rust
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum MeshClass { Critical = 0, Gossip = 1, Bulk = 2, Digest = 3 }

/// Outer frame. Header fields are cleartext (needed pre-decrypt for
/// dedup, replay check, and queueing) and are bound as AEAD AAD.
#[derive(Serialize, Deserialize, Clone)]
pub struct MeshFrame {
    pub ver: u8,            // wire version, start at 1
    pub class: MeshClass,
    pub sender: u16,        // node id from colony registry (peers.toml-adjacent)
    pub ctr: u64,           // per-sender monotonic counter; (sender, ctr) == msg_id
    pub ct: Vec<u8>,        // ChaCha20-Poly1305 ciphertext of postcard(PlainPacket)
}

#[derive(Serialize, Deserialize, Clone)]
pub struct PlainPacket {
    pub target: u16,        // 0xFFFF = broadcast
    pub hop_limit: u8,      // default 4; decrement on relay; drop at 0
    pub flags: u8,          // bit0: ack-requested; rest reserved
    pub payload: Payload,
}

#[derive(Serialize, Deserialize, Clone)]
pub enum Payload {
    Heartbeat { uptime_s: u32, cortex_up: bool, conn: u8 /* ConnectivityState */ },
    Alarm { code: u16, detail: heapless-or-vec-string },
    A2A { body: Vec<u8> },                       // opaque to the mesh layer
    DreamDigest(Digest),                          // §7
    ChunkAnnounce { root: [u8; 32], n_chunks: u16, total_len: u32 },
    ChunkRequest  { root: [u8; 32], index: u16 },
    ChunkData     { root: [u8; 32], index: u16, data: Vec<u8> },
    Ack { of_sender: u16, of_ctr: u64 },
}
```

### 3.3 Crypto envelope (details in §8)

- AEAD: **ChaCha20-Poly1305**, colony-wide PSK (32 B) from colony config, distributed over Tier 1 / USB only.
- Nonce (12 B) = `sender_le(2) ++ ctr_le(8) ++ [0, 0]`. Counter is monotonic per sender and **persisted** (agentd: on disk; brainstem: flash high-water every N increments to bound wear). Nonce reuse is a hard-fail class of bug — test for it.
- AAD = the serialized cleartext header (`ver, class, sender, ctr`) so header tampering breaks the tag.
- Replay: per-sender sliding window (64-entry bitmap over `ctr`) on every receiver, including the brainstem.

### 3.4 Chunker (Bulk lane)

Content-addressed: blob → blake3 root + fixed-size chunks (start at 256 B for GATT, tune later). `ChunkAnnounce` advertises; receivers pull missing `index`es; resume-by-hash is free. Same mechanism serves soul.md diffs, memory chunk reconciliation (§7), and code distribution.

### 3.5 Cargo.toml (versions verified live 2026-07-27)

```toml
[package]
name = "apexos-mesh-proto"
edition = "2021"

[dependencies]
postcard = { version = "1.1.3", default-features = false, features = ["alloc"] }
serde = { version = "1", default-features = false, features = ["derive", "alloc"] }
crc32fast = { version = "1.5.0", default-features = false }
chacha20poly1305 = { version = "0.11.0", default-features = false, features = ["alloc"] }
blake3 = { version = "1.8.5", default-features = false }
heapless = { version = "0.9.3", default-features = false }
```

---

## 4. Pi side — `tools/crates/apexos-mesh-bridge`

### 4.1 Role & integration pattern

Mirror the proven **`apex-sensor-bridge` pattern**: a small daemon that (a) owns the UART link to the brainstem, (b) pushes inbound mesh events to agentd over the existing WS event stream, and (c) exposes MCP tools for outbound control:

- `mesh_send { class, target, payload }`
- `mesh_status` → link state, brainstem uptime, queue depths, LoRa airtime budget remaining
- `mesh_neighbors` → neighbor table (node id, RSSI, last-seen, via which tier)
- `mesh_budget` → duty-cycle accounting (§5.4)

### 4.2 Link management

- `tokio-serial` **5.5.0** (verified current; v1's `MIN_CORE_BAUD_RATE` import does not exist — delete on sight).
- `read() == Ok(0)` on the serial stream = port died (unplug/driver reset). That is a **link-down event**: emit health event, enter reconnect FSM (backoff 250 ms → 5 s cap), resume. Never swallow it as "no data".
- Event loop: `select!` over {serial readable, outbound queue, shutdown}. On serial wake: read once into the ring buffer, then **drain ALL complete frames** before awaiting again. (v1 parsed at most one frame per read — a frame could sit decoded-but-undelivered until unrelated bytes arrived.)

### 4.3 Codec MUST requirements (each is a test)

1. **MUST** enforce `MAX_WIRE_FRAME` — a corrupted length can never make the parser wait on gigabytes (v1 bug).
2. **MUST** advance past a frame that fails postcard/CRC decode — a poison frame is dropped and logged, never re-parsed forever (v1 bug).
3. **MUST** treat EOF as link-down (v1 bug, see 4.2).
4. **MUST** drain multiple buffered frames per wakeup (v1 bug, see 4.2).
5. **MUST** resynchronize after arbitrary garbage injection within one delimiter (`0x00`) — property of COBS, but test it explicitly with fault injection.
6. **MUST** count and expose: `rx_frames, tx_frames, crc_fail, decode_fail, resyncs, link_downs` into existing telemetry.

### 4.4 Test harness (no hardware required)

- `proptest`: `MeshFrame` roundtrip through the full pipeline (postcard→CRC→COBS→back).
- `cargo-fuzz` target on the deframer: raw bytes in, must never panic, never OOM, never stall. Acceptance: 24 h clean.
- Integration: `socat -d -d pty,raw,echo=0 pty,raw,echo=0` pair — bridge on one end, a test brainstem-simulator binary on the other. Fault injectors: truncation, bit-flips, giant frames, mid-frame disconnect, delimiter floods.

---

## 5. Brainstem firmware — `firmware/brainstem/` (new top-level dir, excluded from the main workspace; cross-compiled)

### 5.1 Hardware (pick per node; both stay supported)

- **Option A (default): Heltec WiFi LoRa 32 V3** or **LilyGo T3-S3** — ESP32-S3 + SX1262 on one board. Covers Tier 2 + 3 with a single co-processor, one UART to the Pi. Xtensa target → `espup`-installed toolchain.
- **Option B (RISC-V purist path): ESP32-C6 devkit + SX1262 SPI breakout.** C6 is RISC-V → builds on **mainline rustc**, no forked toolchain — philosophically aligned with the coming node-5 direction. Cost: hand wiring + two parts.

Own 5 V supply (not the Pi's rail) so the brainstem rides through Pi power events. That's the point.

### 5.2 Crate set (versions verified live 2026-07-27)

| Crate | Version | Note |
|---|---|---|
| `esp-hal` | **1.1.1** | The 1.x stable line — post-1.0 API, differs substantially from older tutorials |
| `esp-radio` | **1.0.0-beta.0** | Successor to `esp-wifi` (0.15.1 still exists). Pick the coherent set per esp-hal's compatibility matrix — do not mix eras |
| `trouble-host` | **0.7.0** | Pure-Rust BLE host over the esp radio controller; central+peripheral |
| `embassy-executor` / `embassy-time` / `embassy-sync` / `esp-hal-embassy` | 0.10.0 / 0.5.1 / 0.8.0 / 0.9.1 | Async task runtime |
| `lora-phy` (+ `lora-modulation`) | **3.0.1** / 0.1.5 | lora-rs async SX126x driver — v3 API, review current docs, it moved past older examples |
| `sequential-storage` | **8.0.1** | Flash-backed queue/map for store-and-forward + counter high-water |
| `apexos-mesh-proto` | path dep | The whole point |

### 5.3 Embassy task layout

| Task | Responsibility |
|---|---|
| `uart_link` | Frame codec (shared crate), same MUSTs as §4.3, link supervision toward the Pi |
| `ble_gossip` | Extended-advertising TX/RX flood; seen-cache (LRU ≥256 entries, TTL ~5 min) keyed by `(sender, ctr)`; `hop_limit` decrement; relay policy |
| `gatt_bulk` | Custom GATT service: chunk-write characteristic + credit-based flow control via notifications; serves/pulls `Chunk*` payloads with neighbors |
| `lora_task` | SX1262 TX/RX; **duty-cycle governor is enforced HERE** (§5.4) — firmware physically cannot be talked into violating it |
| `store_forward` | `sequential-storage` queue (≥256 KB flash partition). Inbound-for-cortex while Pi is down; outbound while transports are down. Drop policy per §2.2 (Bulk first, Critical never). Drains on link-up |
| `status` | Own heartbeat (`cortex_up: false` when UART silent > threshold), LED, watchdog |

### 5.4 LoRa duty-cycle governor (EU868 — legal hard constraint)

- Default sub-bands: **1 % duty cycle ≈ 36 s airtime/hour**. The 869.4–869.65 MHz sub-band allows **10 % ≈ 360 s/hour** — reserve it for `Critical` only.
- Per-band token bucket; airtime computed per TX from SF/BW/payload length before keying the radio; hard-block + upstream report when exhausted (`mesh_budget`).
- Rough airtime anchors: ~51 B payload ≈ 110 ms @ SF7/125 kHz, ≈ 2.8 s @ SF12/125 kHz. Confirm the calculator against ETSI EN 300 220 at impl time.
- Unit-test the governor with a mocked clock. This is a compliance feature, not an optimization.

---

## 6. agentd integration (`agentd/crates/`)

### 6.1 Transport abstraction (replaces v1's if/else fallback)

```rust
#[async_trait]
pub trait MeshTransport: Send + Sync {
    fn id(&self) -> TransportId;                 // WifiLan, BleGossip, BleBulk, Lora
    fn mtu(&self) -> usize;
    fn latency_class(&self) -> LatencyClass;     // Interactive / Background / Overnight
    fn cost(&self) -> TransportCost;             // airtime/power signal for the router
    fn health(&self) -> TransportHealth;         // Up / Flaky / Down (+ metrics)
    async fn send(&self, frame: MeshFrame) -> Result<SendReceipt>;
}
```

A **policy router** maps §2.2 classes onto healthy transports: `Critical` fans out everywhere; `Gossip` picks cheapest-healthy and escalates down-tier on ack timeout; `Bulk` waits for a bulk-capable window; `Digest` coalesces. Router owns the agentd-side seen-cache (dedup — mandatory, since Critical fan-out means duplicates by design) and idempotent A2A dispatch keyed by `(sender, ctr)`. Mock transports make the entire router testable with zero hardware.

### 6.2 ConnectivityState machine

```
Full      — Tier 1 up                         → everything available
Degraded  — Tier 1 down, BLE up               → gossip + bulk lane, no WAN, local inference only
Minimal   — only LoRa reachable               → pings, alarms, digests
Isolated  — no transports                     → store-and-forward only
```

Derived from `TransportHealth`; published as an event on the existing WS bus (UI can show it; other nodes hear it via heartbeats).

### 6.3 Substrate notice + mechanical tool gating

- Each MCP tool entry gains `required_connectivity: ConnectivityState`. The registry **filters the tool list** exposed to the model by current state — in `Degraded`, WAN-dependent tools are absent, not failing. Heavy outbound artifacts go to a visible **outbox** with pending status rather than erroring.
- Truthful notice templates injected per state (wording lives in `docs/model-welfare.md`), e.g. Degraded: *"Substrate notice: this node is currently on a low-bandwidth radio mesh. Wide-area tools are unavailable and hidden; large transfers are queued in the outbox until Wi-Fi or the USB Exo-Workspace returns."*
- **Welfare-law note:** connectivity truth and capability gating are *environmental facts* (the network genuinely is down), not persona surgery — no evolution-tool consent cycle required. Document the reasoning in `model-welfare.md`; nodes may of course evolve their own *responses* to degraded states.

---

## 7. Dream digest / anti-entropy (adapts `dream_run`)

Nightly, per node, ≤ ~96 B before the crypto envelope:

```rust
pub struct Digest {
    pub epoch: u32,          // day index
    pub node: u16,
    pub mem_root: [u8; 32],  // blake3 root over the epoch's new-memory chunk set
    pub n_new: u16,
    pub tags: [u32; 4],      // top-k salience tag hashes
}
```

Flow: broadcast on 2a and 3 → peers record `(node, epoch, mem_root)` and diff against local state → **reconciliation happens later** over the GATT bulk lane or Tier 1 using the §3.4 chunk protocol, driven by the existing `dream_run` delegation (the GPU node consolidating for the cluster already fits this shape). The radio never carries memories — it carries the provenance-stamped *claim* that memories exist, which is exactly what the overnight budget can afford. Provenance = blake3 roots + AEAD sender authentication, by construction.

**Future hook (post-PH landing in cerebro):** `tags` could carry compact topological summary deltas — e.g. Betti-number changes — a few bytes describing the *shape* of what changed in memory. Deliberately out of scope for v2; noted so the struct gets a `ver`/reserved room if cheap.

---

## 8. Security model

- **Baseline (ships in Phase 5, blocks yolo until then):** colony PSK (32 B) in colony config, distributed over Tier 1/USB only; per-packet ChaCha20-Poly1305 as in §3.3; strict nonce discipline; per-sender replay windows everywhere (including brainstem); MAC-covered headers via AAD.
- **Key rotation:** new PSK distributed over Tier 1; dual-accept window (old+new) for 24 h; radio tiers never carry keys.
- **Threat model:** radio is untrusted input into agents that can self-modify. All inbound mesh payloads are data until they clear the same policy/approval layer as everything else ("reversible→automate, irreversible→approve" applies to mesh-originated actions too). A2A bodies from radio get the same adversarial-review gate as agentd evolutions when they would trigger privileged behavior.
- **Roadmap (post-v2):** per-node X25519 identities + Noise handshake for unicast lanes; PSK remains for flood/broadcast classes.
- No secrets, ever, in `Digest` or `Heartbeat` payloads.

---

## 9. Phased plan (each phase has a definition of done)

**Phase 0 — Procurement & spikes** *(days)*
Order 2–3 boards (Option A default; one Option B if the RISC-V itch demands). Toolchain spike: build+flash blink via esp-hal 1.1 on the chosen target; confirm the coherent esp-radio/trouble version set; skim lora-phy v3 examples.
✅ *Done when:* hello-world firmware flashes and prints over UART on real hardware.

**Phase 1 — Wire crate + Pi bridge** *(~1.5 k LOC)*
`apexos-mesh-proto` + `apexos-mesh-bridge` with §4.3 MUSTs, PTY test harness, proptest, fuzz target.
✅ *Done when:* 24 h fuzz clean; bridge survives every §4.4 fault injector against the simulator over socat PTYs; metrics visible in telemetry.

**Phase 2 — Brainstem MVP** *(~2 k LOC firmware)*
UART echo → heartbeat gossip flood → neighbor table w/ RSSI → flash store-and-forward.
✅ *Done when:* two boards exchange authenticated heartbeats **with both Pis powered off**, and a queued message survives a brainstem power cycle and delivers on cortex link-up.

**Phase 3 — agentd router + honesty machinery** *(~1 k LOC)*
`MeshTransport` trait, policy router + dedup, ConnectivityState, substrate notices, MCP tool gating, outbox.
✅ *Done when:* kill Wi-Fi mid-session → A2A continues over BLE within seconds, prompt shows the degraded notice, WAN tools are absent from the tool list, heavy artifact lands in outbox; restore Wi-Fi → outbox drains, state returns to Full. (Chaos-drill script checked into `deploy/`.)

**Phase 4 — LoRa tier + dream digests** *(~1.5 k LOC)*
lora-phy integration, duty-cycle governor, digest broadcast + chunk reconciliation.
✅ *Done when:* overnight digest exchange between two nodes with Wi-Fi disabled; next morning on Wi-Fi, cerebro reconciliation converges (roots match); governor provably blocks TX beyond budget in a mocked-clock test.

**Phase 5 — Hardening** *(~1 k LOC + tests)*
Crypto envelope end-to-end, nonce-persistence, replay-attack tests, backpressure policies, metrics dashboards, docs (`docs/nervous-system.md`, `model-welfare.md` update), full chaos drill on the 4-node colony.
✅ *Done when:* replay/forge/nonce-reuse test suite green; colony data-month can enable the mesh under yolo.

**Phase 6 — Stretch: Bitchat adapter** in `apexos-bitchat-proxy`, pinned to the then-current spec version (fetch spec from the official GitHub repo; it's in the container allowlist). Interop demo: a phone running Bitchat sees colony presence. Explicitly optional.

---

## 10. Test matrix & CI

- Unit: codec MUSTs, governor (mock clock), replay windows, router policy table.
- Property: proptest roundtrips (frame pipeline, chunker reassembly).
- Fuzz: deframer (`cargo-fuzz`), CI runs a short budget per PR, nightly long run.
- Integration: PTY harness + brainstem-simulator binary (also used by agentd router tests via mock transports).
- Firmware CI: `cargo check`/build for the ESP target on every PR (no flash); HIL job optional/manual.
- Chaos: scripted Wi-Fi kill/restore drill as a `deploy/` script, run before beta cut.

---

## 11. Open questions for FORGE + verify-at-impl checklist

Left deliberately to implementer judgment: GATT service/characteristic UUID scheme; flash partition layout & wear budget; chunk size tuning; exact bridge process model (standalone daemon vs. thread in an existing tools binary — sensor-bridge precedent suggests standalone); ack/escalation timeout constants.

**Verify before writing code** (the anti-confabulation checklist — versions above were index-verified 2026-07-27, but APIs move):

1. esp-hal 1.1.x ↔ esp-radio 1.0.0-beta ↔ trouble-host 0.7 ↔ esp-hal-embassy 0.9 compatibility matrix (esp-rs book / crate READMEs). Do not mix with pre-1.0 esp-wifi-era examples.
2. lora-phy **v3** API shape (it changed vs. widely-circulated v1/v2 examples).
3. postcard COBS helper names under the `alloc` feature (`to_allocvec_cobs` / `from_bytes_cobs`) vs. the `use-crc` flavor — pick one framing recipe and freeze it in `apexos-mesh-proto` docs.
4. Extended-advertising support/limits in trouble-host on the chosen chip (S3 vs C6) — gossip MTU assumption (~200 B) depends on it; fall back to chunked legacy adv if needed.
5. ETSI EN 300 220 current sub-band/duty-cycle table for the governor constants.
6. Bitchat spec current version + license (Phase 6 only).

---

## Appendix — EU868 quick reference (sanity anchors, confirm per §11.5)

| Sub-band | Duty cycle | Budget | Use here |
|---|---|---|---|
| 863–868 MHz (g) | 1 % | ~36 s airtime/h | Gossip beacons, digests |
| 868.0–868.6 (g1) | 1 % | ~36 s/h | Default TX band |
| 869.4–869.65 (g3) | 10 % (+500 mW ERP) | ~360 s/h | `Critical` alarms only |

Airtime anchors @125 kHz BW: ~51 B ≈ 110 ms (SF7) / ≈ 2.8 s (SF12). Overnight digest broadcast (≤96 B + envelope, SF9) costs well under one second of airtime — the budget is a non-issue *if and only if* nothing else abuses Tier 3, which the governor enforces.

---

*v2 compiled from the v1 draft + review round (chat-Claude, 2026-07-27). Carried across the airgap with the usual ceremony. Hei FORGE — the PH work sounds like it's landing beautifully; this one's the peripheral nervous system to go with it.*
