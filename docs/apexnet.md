# ApexNET — the nervous system (v3 charter)

> **Status:** v3 **LOCKED** (André, 2026-08-03 — merged as #330) · **Date:** 2026-08-03
> **Supersedes:** `docs/ideas/apexnet/BT-LoRa-NERVOUS_SYSTEM-V2.md` (v2, locked 2026-07-27 — preserved as the input doc; its §3–§5 stand as written and are referenced, not repeated)
> **Built from:** the v2 doc + a three-scout code recon (2026-08-03) against `d3ab7cc` — v2 predates W2 mesh workers (#318), M2 cross-node rings (#321), the beacon, and the shipped dream-digest push, all of which change §6–§7.

The colony's radio substrate + courier lane: when Wi-Fi/LAN/internet dies, nodes
stay coherent — speed scales down with distance (Mbit → KB/s → bytes/s → a human
walking), but **comms and tasks keep flowing**, and humans carry what radios
can't. The subtle win is not "they have radios now"; it is that *total internet
failure degrades the colony instead of ending it*.

---

## 0. Principles (v2's five, reaffirmed + one new)

1. **The nervous system survives the cortex.** The ESP32 brainstem stays up
   through Pi reboots/crashes/self-evolution; own 5 V rail; flash-backed queue.
2. **Radio carries proofs, not data.** Hashes, digests, alarms, assignments;
   the bytes move when a high-bandwidth window opens (Tier 1 / GATT bulk /
   **Tier 4 courier**). Anti-entropy, not sync.
3. **Honesty is mechanical.** Connectivity state gates which tools are
   *exposed*, not which ones fail. Degraded means absent, not broken.
4. **The airwaves are hostile.** Every inbound radio payload is authenticated,
   replay-protected, and is *data, never instructions* until it clears the same
   policy layer as everything else. Blocks yolo until Phase H ships.
5. **Pure Rust, end to end.** One shared `no_std` wire crate on both sides of
   the UART. Both protocol gates (default + `--no-default-features --features
   alloc`) run in CI, exactly like `apexos-protocol`.
6. **NEW — Humans are a transport.** The exo-workspace stick is Tier 4: highest
   bandwidth, highest latency, zero infrastructure. The mesh treats a courier
   like a link — announced, ledgered, verified, receipted — not like a manual
   workaround.

---

## 1. Decision log

D1–D4 (v2, stand unchanged): postcard+COBS+CRC32 shared `no_std` crate · custom
BLE adv-flood + GATT (SIG Mesh dropped) · native lora-rs (`lora-phy` v3) + own
flood MAC · Bitchat = pinned stretch adapter.

New in v3 (each grounded in the 2026-08-03 recon):

| # | Decision | Rationale / code truth |
|---|----------|------------------------|
| D5 | **Tier 4 courier lane is first-class**, shipped as its own hardware-free phase | The USB exo-workspace machinery is live (marker-gated mount, plug notification, privilege-separated eject) but has **no identity beyond the 11-char filesystem label and no manifest/ledger** — `apexos-workspace.toml` is written by `deploy/usb/apexos-workspace-init` and read by *nothing* in Rust, so the schema is ours to extend (`version = 1` → 2) with zero parsers to migrate |
| D6 | **Tool gating rides a policy-style side table, not ToolSpec fields** — `[connectivity]` rows keyed by tool name in a config file, synced additively like `sync_policy_rules` | `ToolSpec` is 3 fields in the protocol crate; `mcp.rs::list_tools` hard-selects 3 keys (any MCP metadata dies there); a side table needs **no protocol change**, inherits the proven additive-sync distribution story, and unknown/unlisted tools default to *always available* (today's behavior, no regression). Listed rows name the minimum `ConnectivityState` |
| D7 | **ConnectivityState is coarse and LATCHED** (hysteresis-damped; changes only on real transitions) | Tools+system cache as one prefix (`anthropic.rs:130-160`); a flapping filter would nuke the cached prefix per flap. Template: the 180 s sensor-freshness window. A state *transition* is rare and worth the one-time prefix rebuild (same cost class as a plugin registering) |
| D8 | **The beacon becomes transport-aware; the radio heartbeat feeds the same liveness map** | Today `beacon.rs` is a per-peer 2-state binary driven by pulling `GET /api/capabilities` (a multi-KB body discarded every 30 s). A radio-only peer would read permanently *dark* → fanout refuses it, polls skip it, and the agent gets a false "lost power" prompt every ~90 s. v3: per-peer per-transport last-seen; `dark` = unreachable on ALL transports; push heartbeats (radio) and pull probes (LAN) land in one `LivenessMap`; the LAN probe moves to a lean `/api/ping` |
| D9 | **Fabrica crosses the radio as proofs** — W2 assignments go as-is; reports demote `evidence_doc` to a blake3 root + byte size, pulled later over a fat lane | Recon sizing: `query` (~50 B) fits a frame; `fanout` is 0.3–10 KB (chunkable); but a settled batch report inlines whole evidence docs (20–100 KB) — the inline exists to save a hop on LAN, the exactly wrong trade on radio. The **tolerant string-state wire is already radio-shaped**: unknown state → non-terminal → the deadline is the net; no changes needed there |
| D10 | **§7 digest = evolution of `dream_digest.rs`, not a parallel system** | As-built the digest push is the *opposite* model (up to 5 whole memories, ≤60 k chars each, eager HTTP fan-out, no reconciliation). Keep its proven parts — `digest_candidates` (pure epoch-set selection), the echo-guard (`colony`/`dream-digest`/`from:*` exclusion), receiver-side unforgeable provenance stamping, the `from:`+`origin:` dedup key — and add the missing ones: blake3 root, epoch counter, per-peer have-state, a pull path. Tier 1 keeps eager push (it's cheap there); radio tiers send only the claim |
| D11 | **The outbox is a durable JSONL ledger, drained on windows** | Zero outbox exists (grep-clean); every mesh send today is synchronous-HTTP-or-lose-it. The semantic precedent is the scheduler's overdue-at-boot rule ("commitments run late, they don't evaporate") — the outbox is that rule applied to transports instead of clocks |

---

## 2. Network architecture

Tiers 1–3 as v2 §2.1. New row:

| Tier | Medium | Practical MTU / throughput | Carries |
|------|--------|---------------------------|---------|
| 4 | **Human courier** (APEX-labelled exo-workspace stick) | GBs per trip; latency = walking/driving time | Everything too big for radio: artifacts, evidence bundles, cerebro reconciliation chunks, code, key rotation. The "ADSL" lane — enormous bandwidth one way, time the other |

Message classes (v2 §2.2) stand; `Bulk` gains Tier 4 as an allowed transport.
Token/inference streaming stays blocked on all radio tiers.

**Absorbed:** the parked hotspot-mode item (Top-10 #8) is Tier 1's no-router
story — a node flips itself into the WiFi AP that carries the colony's fat
tier; the old BACKLOG sketch (hostapd+dnsmasq, captive portal, token gate)
remains its implementation notes.

---

## 3–5. Wire crate · Pi bridge · brainstem firmware

**v2 §3, §4, §5 stand as written** — `apexos-mesh-proto` (workspace root,
`no_std`+alloc, postcard/COBS/CRC32, ChaCha20-Poly1305 envelope, chunker),
`tools/crates/apexos-mesh-bridge` (the apex-sensor-bridge pattern; the six
codec MUSTs, each a test), `firmware/brainstem/` (esp-hal 1.x + embassy +
trouble-host + lora-phy v3; excluded from the main workspace).

v3 annotations:

- The sensor-bridge pattern was field-hardened *after* v2: #327 added the
  inbound drain (50 ms read timeout, drain-to-WouldBlock, EOF = link-down) —
  the bridge inherits that discipline from day one; v2's MUST-3/MUST-4 are
  already proven in-repo idiom.
- One new `Payload` variant beside v2 §3.2's set:
  `CourierManifest { stick: [u8; 8], origin: u16, dest: u16, root: [u8; 32], n_chunks: u16, total_len: u32, epoch: u32 }`
  (~56 B — fits every radio tier) and
  `CourierReceipt { stick: [u8; 8], root: [u8; 32], accepted: bool }` (~44 B).
- `Digest` gains `ver: u8` + one reserved `u32` (v2 §7's PH-topology hook, room
  is cheap now, breaking later isn't).
- Crate versions in v2 §3.5/§5.2 were index-verified 2026-07-27; **re-verify at
  implementation** (v2 §11 checklist stands verbatim).

---

## 6. agentd integration (rewritten against `d3ab7cc`)

### 6.1 Transport abstraction + policy router

v2 §6.1's `MeshTransport` trait stands. Transport 0 (`WifiLan`) **wraps the
existing HTTP mesh paths** (a2a `POST /api/sessions/{id}/message`, the four
`/api/worker/*` routes, `/api/mesh/*`) — today's mesh becomes one transport
among several, unchanged in behavior when it's healthy. The policy router owns
class→transport mapping, the agentd-side seen-cache (dedup by `(sender, ctr)` —
mandatory, Critical fans out everywhere), and idempotent A2A dispatch. Mock
transports make it fully testable with zero hardware.

### 6.2 ConnectivityState (greenfield — nothing to extend, verified)

`Full / Degraded / Minimal / Isolated` per v2 §6.2, derived from per-transport
health, **latched with hysteresis (D7)**. Interplay with what exists:

- **Beacon** (`gateway/src/beacon.rs`): per-PEER reachability; ConnectivityState
  is per-NODE transport tier — different axes, one truth source. Per D8 the
  `LivenessMap` gains per-transport last-seen; `beacon_step` stays the pure
  edge machine; radio heartbeats (push) mark peers alive on their tier.
  The false-"dark" prompt for radio-only peers becomes "reachable via LoRa
  (minimal)" — a different, honest message.
- **`PeerLost` — RULED (P5b): claimed, meaning "unreachable on every
  transport".** It stays in the protocol and gains a definition it did not
  have. Deleting it would be a wire change for no gain, and the fact it names
  did not exist until a node had more than one lane. The ruling has teeth:
  a peer that is merely off the LAN is **not** lost — it is reachable on a
  slower tier, and calling that "lost" is precisely the false alarm this
  section warns about. `PeerReachability::is_lost()` is the predicate;
  emission belongs to whoever owns multi-transport liveness (P5c).
- **Per-peer `status` string in `PeerRecord`** is dead (always `"online"`,
  `set_status` has zero callers) — do NOT overload it; liveness stays in the
  beacon map, transport tier in the new state.
- New lean probe: `GET /api/ping` → `{node_id, uptime_s}` (~40 B). The beacon
  stops pulling the multi-KB `/api/capabilities` body it discards.

### 6.3 Mechanical tool gating (D6)

- `config/connectivity.toml` (seed in `config/`, deployed `/etc/agentd/`,
  additive-synced like policy): `[tools]` rows `tool_name = "full" | "degraded"
  | "minimal"` — the *minimum* state at which the tool appears. Unlisted =
  always available. First seeding covers the obvious WAN/LAN set:
  `http_fetch`, `bootstrap_node`, `vast_*`, `mesh_file_send`,
  `mesh_memory_send`, `mesh_procedure_send`, `mesh_recall`,
  `mesh_capabilities`, `agent_spawn` (cross-node arm), `task_fanout` (node-
  targeted tasks degrade to local), `apply_daemon_update`, `imaginarium_*`,
  occipital's `web_*` family.
- `gather_tools` gains the filter (signature change, 5 call sites:
  root turn, both spawn arms, `gather_capabilities`, `build_embodiment`) —
  the embodiment's tool-registry prose inherits consistency by construction.
- The supervisor keeps a call-time backstop: a call to a hidden tool returns an
  honest "unavailable in <state> — queued paths: outbox / courier" error (the
  model shouldn't ever see it, but frames race state transitions).

### 6.4 Substrate notices (both surfaces, per the recon)

- **Edges** (state transitions): synthetic `UserPrompt` into session 0 —
  the established idiom (beacon dark/recover, USB plug, substrate swap all use
  it) — kill switch `APEXNET_NOTIFY_AGENT=0`, wording per
  `docs/model-welfare.md` (connectivity truth is an *environmental fact*, not
  persona surgery; no consent cycle).
- **State** (while degraded): composes into the existing `inject_ambient`
  seam — currently a single-producer `Arc<RwLock<String>>` (the clock); it
  becomes clock + connectivity line. Ephemeral, never persisted, rides after
  the cached prefix at zero breakpoint cost, rate-gated as today.

### 6.5 The outbox (D11 — greenfield)

`<log_dir>/outbox.jsonl`: `{id, class, target, payload_ref, created_at,
announced: bool, sent_via: Option<TransportId>, receipt: Option<...>}`.
Heavy sends in degraded states land here instead of erroring; existence is
announced via `Digest`/`ChunkAnnounce` on cheap tiers; drained when a bulk
window opens (Tier 1 back, GATT neighbor, or a Tier-4 stick with room).
Surfaced in `mesh_status` and on the board (the MeshInbox pattern, outbound).

### 6.6 Fabrica over the mesh (D9 — the marriage)

The M2 ruling extends verbatim: *the cell stays on the bindu, the body
travels* — over radio, only the proof-sized parts travel at all.

- **Assignments** (`RemoteTaskItem {prompt, model?, steps?}`): cross as-is;
  a fanout body chunks over GATT when >MTU; over LoRa only single small tasks
  make sense (the router refuses `Bulk`-class fanouts on Tier 3, honestly).
- **Reports**: `WireRow` stays, but on radio transports `evidence_doc` is
  demoted to `{root: blake3, len: u32}`; the doc itself is `ChunkAnnounce`d
  and pulled over a fat lane, or rides the next courier stick. The conductor's
  gate reads mirrors when they arrive — barriers already tolerate late
  evidence (deadline is the net).
- **State wire**: unchanged — `parse_state_tolerant`'s unknown→non-terminal
  rule and the deadline net were built for exactly this level of link chaos.
- **Steers/revives**: `send_to_agent` bodies are small free text — they ride
  `Gossip` class fine. A parked worker on a radio-only peer is revivable.

### 6.7 Dream digest / anti-entropy (D10)

Keep from `dream_digest.rs`: `digest_candidates` (pure epoch selection),
`digest_excluded` (echo-guard), the receiver's `federated_remember_args`
(unforgeable provenance re-stamping), the `from:`+`origin:` dedup key.
Add: per-epoch blake3 root over the candidate set, the ≤96 B `Digest` payload
broadcast on 2a/3, per-peer `(node, epoch, root)` have-state, and pull-based
reconciliation over GATT/Tier 1/Tier 4 using the §3.4 chunker. Tier 1 keeps
today's eager push (cheap there); radio sends claims only.

---

## 7. Tier 4 — the courier lane (new in v3)

**Identity.** Marker schema v2 (`apexos-workspace.toml` — no existing Rust
parser, clean bump):

```toml
version  = 2
name     = "work"
layout   = ["projects", "data", "notes"]
stick_id = "a3f9c2e11b7d4402"     # minted once at prep, 8 random bytes hex
minted_by = "apex1"
minted_at = "2026-08-03T20:00:00Z"
```

`stick_id` decouples identity from the 11-char label (which is neither unique
nor collision-safe — two same-label sticks silently contend for one
mountpoint today). The label stays the *mount* convention; the id is the
*ledger* key.

**Cargo.** `apexos-courier/` directory on the stick: `manifest.json` (entries:
`{root, len, n_chunks, class, origin, dest, epoch, created_at}`, AEAD-sealed
with the colony PSK so a found stick leaks nothing and a tampered manifest
fails authentication) + the chunk files themselves + `receipts.json` (append-
only: which node ingested which root, when).

**The ledger loop** (the low-bandwidth trick):

1. Node A loads outbox bundles onto a plugged stick → appends manifest entries
   → gossips `CourierManifest` (~56 B) over 2a/3: *"stick a3f9… carrying root
   R for node B departed A at epoch E."* B knows the cargo exists — and what
   it is — before the human arrives.
2. Human carries the stick (the ADSL lane: all bandwidth, all latency).
3. On plug at B: the existing udev→mount→`/api/media/plugged` path fires; the
   plug notification (already a session-0 prompt) is enriched with the
   manifest diff vs the gossiped ledger — *"stick a3f9… arrived; carrying 2
   announced roots for you + 1 unannounced; verifying."* Chunks verify against
   blake3 roots (same machinery as §3.4 — a courier is a very slow transport,
   not a special case).
4. B appends a receipt on the stick AND gossips `CourierReceipt` (~44 B) back
   over radio — A learns of delivery at radio speed. Store-and-forward closes
   its loop.

**Security posture:** sticks are untrusted media. Manifests/chunks are
authenticated (PSK AEAD) and are *data until policy says otherwise* — same law
as radio (§0.4). Nothing on a stick auto-executes; ingest is explicit or
policy-gated. Key rotation may itself ride a courier (it's Tier-1-or-USB by
the v2 §8 rule — a stick IS the USB path).

---

## 8. Security model

v2 §8 stands (PSK + ChaCha20-Poly1305, strict nonce discipline, per-sender
replay windows everywhere including the brainstem, AAD-bound headers, rotation
with dual-accept, X25519/Noise unicast roadmap). v3 additions: the courier
manifest/receipt payloads use the same envelope; `CourierReceipt` is proof of
*delivery*, not proof of *read*; and the ApexHub C2C work (ideas intake #5)
should consume this roadmap's identity layer rather than invent its own.

---

## 9. Phases (v3 — reordered so hardware never blocks)

| Phase | Scope | Hardware? | Done when |
|-------|-------|-----------|-----------|
| **P0 — Procurement** | André orders 2–3 boards (Heltec V3 / LilyGo T3-S3 default; one ESP32-C6 if the RISC-V itch wins) | — | Boards on the bench; blink flashes |
| **P1 — Wire crate** | `apexos-mesh-proto`: frame pipeline, chunker, crypto envelope, courier payloads; both no_std gates in CI; proptest roundtrips | No | 24 h fuzz clean on the deframer |
| **P2 — Courier lane** | Marker v2 + stick_id mint in prep; manifest/receipt read-write; plug-verification + enriched notification; outbox JSONL + drain-to-stick; ledger gossip *stubbed to Tier 1* | **No** | Two LAN nodes exchange an artifact via a physically-carried stick with manifest verification + receipt round trip; tamper test fails closed |
| **P3 — Pi bridge** | `apexos-mesh-bridge` + PTY test harness + brainstem-simulator binary; the six MUSTs as tests | No | Survives every fault injector over socat PTYs |
| **P4a — Brainstem on the wire** ✅ | esp-hal + embassy firmware, heartbeat + ack over USB-Serial-JTAG, same wire crate both ends | **Yes** | Real board drives the real bridge at 1 Hz, `crc_fail 0`; hardware golden vector in CI |
| **P4b — Identity, key, memory** ✅ | Dedicated `apexnet` flash partition; `Provision` payload + `apexos-brainstem-provision`; reserved counter high-water; `BrainstemStatus` telemetry | **Yes** | First-touch commissioning accepted, unsealed re-provision refused, sealed accepted, wrong key refused; identity + counter survive a reset |
| **P4c — BLE gossip** ✅ | Raw-HCI Tier 2a driver (ext adv + ext scan), sealed heartbeats, neighbour table with per-sender replay windows | **Yes** | Two commissioned boards see each other as neighbours with no cortex attached; two boards on DIFFERENT colony keys, in range, see nothing |
| **P4d — Store-and-forward** ✅ | Flash outbox on the `apexnet` partition; seal-at-drain, deliver on neighbour appearance, retire on ack | **Yes** | A message queued for an absent peer survived a restart and was delivered, acknowledged and retired when the peer returned |
| **P5a — Honesty** ✅ | Latched `ConnectivityState`, tool gating, notices, lean `/api/ping` | No | WAN drops → tools vanish + notice; restore → recovers |
| **P5b — Router** ✅ | `MeshTransport` trait, policy router (class→lane, fan-out, MTU refusal), seen-cache, `PeerLost` ruling, `/api/connectivity`, chaos drill script | Sim only | Router policy proven against mock transports incl. flaky/down/undersized lanes; drill script verifies the degrade+recover edge on a live node |
| **P5c — The radio lane** ✅ | `/mesh-bridge` socket (the bridge dials agentd, sensor-bridge pattern), `BleGossipTransport`, `/api/connectivity` reports real lane health + the brainstem's view, `POST /api/mesh/gossip` (admin/owner only; unicast + bound + quota) | **Yes** | agentd → bridge → UART → brainstem outbox → sealed radio → peer, proven end to end; no bridge = honest 503, not an error |
| **P5d — The remaining lanes** ✅ | `WifiLan` (wrapping today's HTTP mesh paths) and `Courier` (the outbox) registered with the router; a2a routed by class rather than by tool; `apexos-mesh-bridge` systemd unit + install.sh | Sim + LAN | **Outbound field-proven 2026-08-17** (apex1 blackhole of apex2, WAN kept so the API LLM still drove): `send_to_agent` `via=["ble-gossip"]`, `mesh_file_send` `via=courier`, restore + HTTP drain. Residual: inbound envelope → named session; auto-drain when WifiLan recovers without a tier flip |
| **P6 — LoRa + digests** | lora-phy + duty-cycle governor (mocked-clock tested) + digest claims + chunk reconciliation + courier gossip goes real | Yes | Overnight digest exchange with Wi-Fi off; morning reconciliation converges; governor provably blocks over-budget TX |
| **P7 — Fabrica over radio** | D9: evidence demotion on radio transports, router class rules for fanout/report, cross-tier revive | Yes | A W2 batch conducted over BLE-only completes with evidence pulled over a later Tier-1 window |
| **P8 — Hardening** | Full crypto E2E, replay/forge/nonce suites, backpressure, metrics, 4-node chaos drill, docs | Yes | Suite green → colony may enable under yolo |
| **P9 — Stretch** | Bitchat adapter (`apexos-bitchat-proxy`, spec pinned at build time) | Yes | A phone running Bitchat sees colony presence |

P1–P3 (and P2 especially) ship **before any board arrives**. P2 alone already
delivers André's worst-case story: no radios at all, pure sneakernet with
cryptographic verification and a paper trail.

---

## 10. Test matrix, open questions

v2 §10 (test matrix) and §11 (implementer-judgment list + the
anti-confabulation version checklist) stand, plus: courier manifest round-trip
+ tamper corpus; same-label two-stick collision behavior decided (mount by
stick_id subdir?— implementer's call, but decided, not inherited); gating
config validate-before-persist (the policy-sync lesson, #274).

### Answered: extended advertising (v2 §11 open question 4)

v2 asked whether `trouble-host` on the chosen chip supports BLE 5 extended
advertising, since the ~200 B gossip MTU depends on it and a sealed heartbeat
(~37-45 B) does not fit legacy's 31 B payload.

**Answered on hardware, 2026-08-09** (bare ESP32-S3, `esp-radio 0.18` +
`trouble-host 0.6`, `Advertisement::ExtNonconnectableNonscannableUndirected`):
extended advertising **works** — `advertise_ext` accepted and enabled a 52-byte
payload. Tier 2a keeps its real MTU; the chunked-legacy fallback is not needed.

Two things to carry into P4c:

- `Peripheral::advertise` is **legacy-only** (it rejects extended props
  outright); extended needs `advertise_ext`. `update_adv_data_ext` refreshes
  the payload without redoing params — the right shape for a periodic
  heartbeat.
- The `Advertiser` and `ScanSession` handles **stop the radio when dropped**.
  Both must be held for the lifetime of the advertising/scan.

### Resolved: the radio was never broken — the host stack was

Bench findings, bare ESP32-S3, 2026-08-09. Two separate things had to be
peeled off before the picture was honest.

**First, a missing antenna.** The devkits ship with an external 2.4 GHz patch
antenna; without it every HCI command still succeeds and essentially nothing
crosses the air. Fitted, a phone sees the board advertising by name. *A missing
antenna is indistinguishable from a broken stack* — confirm RF hardware with an
external witness before suspecting software.

**Then, with a raw-HCI spike** (no `trouble-host` at all — hand-built HCI
straight onto `esp-radio`'s byte pipe), the controller turned out to do
**everything** correctly:

| Path | Result |
|---|---|
| Legacy advertising | works (phone-witnessed) |
| Extended advertising | works, 52 B payload |
| Legacy scanning | **131 LE Meta events** |
| Extended scanning | **174 LE Meta events** (subevent `0x0D`) |

The reports decode to the exact devices a phone sees in the same room.

**Root cause of "no reports": the duplicate filter.** Isolated by changing one
variable at a time on the raw path:

| Scan interval | `filter_duplicates` | LE Meta events |
|---|---|---|
| 160 (100 ms, spec-correct) | off | **131** |
| 10 (6.25 ms) | off | **108** |
| 160 (100 ms, spec-correct) | **on** | **2** |
| 10 (6.25 ms) | **on** | **2** |

`trouble-host`'s legacy `scan()` hardcodes `LeSetScanEnable(.., filter_duplicates = true)`,
and `esp-radio`'s BLE `Config` defaults `scan_duplicate_refresh_period: 0` — a
duplicate list that **never refreshes**. Together: every device is reported
exactly once and then silenced forever. Two individually reasonable defaults
combining into a radio that looks dead.

**A second, real defect (not the cause):** `bt-hci` 0.8.1 declares
`le_scan_interval` / `le_scan_window` as `Duration<10_000>` (10 ms units); the
Bluetooth spec defines them in **0.625 ms** units, so every value goes on the
wire **16x too small**. A requested 100 ms arrives as 6.25 ms. Worth reporting
upstream; harmless here only because interval == window keeps the duty cycle at
100% either way.

**Still open (smaller):** `trouble-host`'s `scan_ext` *does* pass
`FilterDuplicates::Disabled`, yet still returned zero reports. Untested
candidates: the 16x unit bug, the own-address kind (it sets RANDOM; the raw
spike used PUBLIC), or its PHY-params construction.

### The design consequence: Tier 2a needs no BLE host stack

Gossip is **connectionless** — advertise and scan, no GATT, no connections, no
L2CAP. That is a few HCI commands and an event loop, which the raw spike
already demonstrates end to end. Driving HCI directly for Tier 2a is *less*
code than working around a host stack's defaults, keeps unit conversions under
our own tests, and removes an entire dependency from the radio path. `trouble-host`
stays available if a later phase genuinely needs GATT (the v2 §2.1 bulk lane).

**Tier 2a's physical layer is PROVEN, bidirectionally** (2026-08-09, two bare
ESP32-S3s, raw HCI on both sides, ~20 s window):

| Direction | Receptions | RSSI |
|---|---|---|
| board A hears board B | **199** | -54 dBm |
| board B hears board A | **184** | -52 dBm |

```text
04 3e 1a 02 01 03 00 │ ed 69 26 13 cf d2 │ 0e │ 0d 09 "APEXNET-EC69" │ ca
LE Meta, adv report     advertiser address  len  complete local name   RSSI
                 ^^ 0x03 = ADV_NONCONN_IND
```

Two colony brainstems gossiping over the air, continuously, at healthy signal
strength. What remains for P4c is the ApexNET payload and the flash queue —
the radio itself is done.

### The third cause: connectable advertising is a trap for gossip

Before the raw-HCI advertiser, boards would transmit for a while and then go
permanently silent while still receiving perfectly — surviving power cycles,
untouched, on both boards independently. It looked exactly like failing
hardware, and cost an antenna hunt it had nothing to do with.

Cause: the bench beacon advertised **connectable** (to be easy to spot on a
phone). A connectable advertiser **stops advertising the moment anything
connects to it**, and nothing restarts it. On a desk within reach of a phone
and a laptop that are both probing the band, a connectable named device is an
invitation; one connection silences it forever.

**Law: gossip advertises non-connectable (`ADV_NONCONN_IND`), always.** It is
also what the tier wants on its merits — gossip is connectionless, a
connection is a side channel nobody asked for, and any peer able to connect is
a peer able to occupy the radio. If a future phase needs connectable
advertising for the GATT bulk lane, it must own an explicit re-arm on
disconnect, and that re-arm needs a test.

Corollary for diagnosis: **"transmits for a while, then never again, while
still receiving" is a connection, not a fault.** Check the advertising type
before suspecting the antenna.

### Traps already paid for (carry into P4c)

- **Extended advertising works** — `advertise_ext` enabled a 52 B payload, so
  Tier 2a keeps its ~200 B MTU and the chunked-legacy fallback is unnecessary.
  (Whether it can be *received* is the open question above.)
- `Peripheral::advertise` is **legacy-only** — it rejects extended props
  outright; extended needs `advertise_ext`. `update_adv_data_ext` refreshes the
  payload without redoing params: the right shape for a periodic heartbeat.
- **`Advertiser` and `ScanSession` stop the radio when dropped.** Both must be
  held for the lifetime of the advertising/scan.
- `trouble-host` needs feature **`scan`**, or the report callbacks compile out
  silently.
- **Legacy scanning hardcodes `filter_duplicates = ON`** (`scan_ext` disables
  it): each device is reported *once*, so a quiet room is indistinguishable
  from a dead radio. Do not diagnose reception on legacy-scan counts alone.
- **A missing antenna is indistinguishable from a broken stack**, and cost
  most of a session here. Confirm RF hardware with an external witness (a
  phone) before suspecting software.

---

*v3 compiled by FORGE from the v2 charter + the 2026-08-03 three-scout recon
(mesh surface · digest/notice seams · USB/tool-registry seams), the same week
the W2/M2 machinery it now rides on was field-proven. The peripheral nervous
system, meeting the body it grew for.*
