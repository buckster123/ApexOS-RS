# ApexOS-RS — Architecture

> System layout, crate inventory, data flows, the cognitive-memory loop, and the
> install/deploy + hardening model. For Slint-specific patterns see
> [slint-notes.md](slint-notes.md); for the desktop shell / persona story see
> [ui-glowup.md](ui-glowup.md); for the runtime cognitive architecture see
> [symbiosis.md](symbiosis.md).

## The shape of the thing

ApexOS-RS is a **single Cargo workspace** containing the full pure-Rust stack: the
agent daemon, the cognitive-memory engine, the system-tool plugins, and the native
Slint UI. One `cargo build --release --workspace`, one `install.sh`, one mesh node.

The original ApexOS treated the display as *"a thin stateless renderer"* over agentd's
WebSocket — and that contract still holds: `ui-slint` connects to the same
`ws://localhost:8787/ws` a browser does, consuming the same `Event` JSON and sending the
same `Intent` JSON. **What changed is the perimeter.** This repo is no longer a renderer
*against* an external agentd — it *is* the daemon too. The Chromium kiosk is gone; the
agentd that the UI talks to lives in this workspace (`agentd/crates/`), as does the
cognitive memory it depends on (`cerebro/crates/`) and the tool plugins it spawns
(`tools/crates/`).

```
┌─────────────────────────── ApexOS-RS workspace ─────────────────────────────┐
│                                                                               │
│   agentd ─────── ws://localhost:8787/ws ──────┬──→ Browser / PWA (remote)    │
│   (daemon)       /sensor-bridge  /terminal-ws │                               │
│      │           REST /api/*                  └──→ ui-slint (local display)   │
│      │                                              Slint + KMS/DRM           │
│      │ spawns stdio MCP plugins                                               │
│      ├──→ cerebro-mcp   (agent memory — agent FORGE)                          │
│      └──→ apexos-tools  (shell/file/http/system/audio/GPIO)                   │
│                                                                               │
│   apex-sensor-bridge ── WS push ──→ /sensor-bridge   (separate process)       │
│                                                                               │
└───────────────────────────────────────────────────────────────────────────────┘
```

The UI still has **no Cargo dependency** on agentd — the coupling is the stable JSON
wire protocol, not Rust types. (Vendoring `apexos-core` for shared `Event` types is a
documented post-v1 option; today both sides hand-match JSON.)

---

## Crate inventory

Seven binaries come out of `cargo build --release --workspace` (resolver 2, release
profile `lto=thin`, `strip`). Library crates are listed alongside the binaries that use
them.

### `agentd/crates/` — the agent daemon

| Crate | Kind | Role |
|-------|------|------|
| `apexos-core` | lib | Shared types and the event bus. `types.rs` is the wire protocol — `Event` / `Intent` / `ContentBlock` / `ToolCall` / `PolicyMode` / `EvolutionProposal`, with `ActionId`/`SessionId` newtypes that serialize as **bare numbers**. `bus.rs` is the in-process hub: `Bus::new` returns a `BusHandle` (mpsc inbox `emit`) and a `broadcast::Sender` (capacity 1024) to subscribe; `Bus::run` applies `SystemState::apply` (`state.rs`) then rebroadcasts. |
| `apexos-gateway` | lib | The entire external surface. One axum `Router` (`router()`): `/ws` (UI/browser), `/sensor-bridge` (sensor ingest), `/terminal-ws` (PTY), REST `/api/*` (run/speak/record/transcribe/council/session/policy/model), plus mesh peer discovery (`mesh.rs`, avahi). `GatewayState` is a fat clone-of-Arcs struct holding the `BusHandle`, the broadcast sender, live-swappable model/backend/keys/policy `RwLock`s, the in-memory session `histories` map (shared `Arc<Mutex<HashMap>>` with the agent router), and the two auth tokens. |
| `apexos-plugins` | lib | MCP plugin host + policy engine. `Supervisor::run` (`supervisor.rs`) multiplexes bus events (`ToolRequested` → `PolicyEngine.check` → dispatch or `ApprovalPending`; `UserApproval` resolves pending) and `SupervisorCmd` messages, spawns/supervises stdio MCP child processes (`mcp.rs`, newline-delimited JSON-RPC), and intercepts ~a dozen *virtual tools* (propose/rollback_evolution, read_soul_md, schedule_*, convene_council, agent_spawn, vast_*) before falling through to a real plugin. `policy.rs` is pure rule eval (Allow/Ask, Yolo short-circuit, exact-then-`prefix.*` wildcard, Workspace rule). `config.rs` loads `plugins.toml`; `vast.rs` wraps vast.ai GPU recipes. |
| `apexos-agent` | lib | Turn engine. `turn.rs` `run_turn` streams from the provider, emits `AgentText` deltas + `ToolRequested`, awaits matching `ToolResult` (bounded timeout synthesizes an error so a turn never wedges), loops to `TurnComplete`. `provider.rs` = `Provider` trait + `Chunk` stream; `anthropic.rs`/`oai.rs` are the two impls; `routing.rs` `RoutingProvider` dispatches per-call off a live-swappable backend `Arc<RwLock<String>>` so hot-swaps land on the next turn with no restart; `council.rs` runs parallel multi-agent deliberation. |
| `apexos-store` | lib | `run_log_writer` — subscribes to the broadcast bus and appends every `Event` as date-rolling JSONL. |
| `agentd` | **bin** | The daemon. Manual multi-thread tokio runtime (never `#[tokio::main]`). Wires `Bus::new`/`run` + gateway + supervisor + turn engine + scheduler + council; loads soul (`AGENTD_SOUL` or `config/soul.md`) and keys. `spawn_agent_router` is the central dispatcher (consumes broadcast `Event`s, owns abort handles / session children / depths, routes `UserPrompt`→`root_turn`, handles `SpawnAgent`/`AgentMessage`/`UserCancel`/sensor-alert→prompt). `spawn_evolution_applier` consumes `EvolutionProposed`, snapshots undo state, applies via `apply_evolution` (writes `soul.md`/`policy.toml`/`plugins.toml`, hot-reloads policy/agent), and journals undo into Cerebro episodes. `scheduler.rs` (cron, polls every 60s), `council_handler.rs`, `session_store.rs` (per-root-session history JSONL) round it out. |

### `cerebro/crates/` — cognitive memory

| Crate | Kind | Role |
|-------|------|------|
| `cerebro` | lib | The engine. `cortex.rs` `CerebroCortex` is the public facade owning storage + the cognitive engines (`engines/` — hippocampus, neocortex, amygdala, prefrontal, dream, …). `storage/` = `sqlite.rs` (source of truth), `vector.rs` (sqlite-vec vec0 with FTS5 fallback), `graph.rs` (in-memory petgraph rebuilt at startup). `activation/` (`actr.rs`, `fsrs.rs`, `spreading.rs`) is pure math. `dream.rs` runs a 6-phase consolidation against claude-haiku. `config.rs` `Config::from_env`. |
| `cerebro-mcp` | **bin** | MCP-over-stdio server — the plugin `agentd` spawns for agent memory (agent `FORGE`). `dispatch.rs` routes ~66 tools over `Arc<CerebroCortex>`; `tools.rs` is the static schema registry; `transport.rs` is the stdio JSON-RPC loop. `agent_scope(args)` maps an optional `agent_id` to a `VisibilityScope` — the single scoping primitive. |
| `cerebro-api` | **bin** | axum REST + dashboard over the same engine (memory/episode/graph/tag endpoints, ~40 routes). Enforces the shared `AGENTD_TOKEN` bearer secret and refuses a non-loopback bind when it is unset. Binds **:8765**. |
| `cerebro-cli` | **bin** (`cerebro`) | clap CLI over the engine for human/script use. |

### `tools/crates/` — system tools and sensors

| Crate | Kind | Role |
|-------|------|------|
| `apexos-tools` | **bin** | MCP-over-stdio system-tool plugin spawned by the supervisor: shell (`run_command`), file (read/write/list/create/delete), `http_fetch`, system telemetry (cpu_temp/disk/memory/uptime via `/proc`+`df`), notify (jsonl/notify-send/TTS/ntfy/telegram), audio (ffmpeg), GPIO/PWM/servo (sysfs + libgpiod), display_face. **Tools are largely unconfined**; the agentd `PolicyEngine` plus the systemd sandbox are the real enforcement layers (see Security). |
| `apex-sensor-bridge` | **bin** | Standalone write-only WS client. Reads CPU temp from sysfs and optionally polls a SensorHead HTTP service (BME688 env + MLX90640 thermal), pushing `sensor_reading` frames to `/sensor-bridge` on an interval with a fixed-backoff reconnect loop. |

### `ui-slint/` — the native UI (the unique contribution)

| Crate | Kind | Role |
|-------|------|------|
| `ui-slint` | **bin** (`apexos-rs-ui`) | Slint declarative UI compiled to native GL, rendering via KMS/DRM on Pi (`SLINT_BACKEND=linuxkms`) or winit on desktop. `main.rs` holds all Rust logic: tokio bootstrap, WS client + reconnect, REST polling, event dispatch, the Rust-owned window manager, persona system, toasts/notifs, and the terminal/council PTY streams. `src/ui/appwindow.slint` is the root tree (compiled by `build.rs`); `src/ui/components/` are the views. |

---

## Thread model (the load-bearing invariant)

Slint **requires** the main thread for its event loop; `#[tokio::main]` would hijack it.
So the daemon and the UI both build the tokio runtime manually:

```rust
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    let ui = AppWindow::new()?;             // Slint init on main thread
    rt.spawn(async move { /* WS + HTTP */ }); // async work in background
    ui.run()?;                              // blocks main thread — Slint owns it
    Ok(())
}
```

All UI mutation is marshalled through `slint::invoke_from_event_loop`. The dynamic
collections — `messages`, `sessions`, `models`, `toasts`, the notif log, the window set,
the council roster — are `Rc<VecModel<…>>` held in thread-locals and only ever touched on
the Slint thread. **Chat is a `VecModel<MessageItem>` with per-row streaming bubbles**;
there is no single `agent_text` property. (The old `set_agent_text("hello")` sketch from
early scaffolding is gone.)

---

## Data flows

The **broadcast bus is the backbone**. `core::Bus` is the hub: an mpsc inbox
(`BusHandle::emit`) feeds `Bus::run`, which applies `SystemState::apply` and fans out via a
`broadcast::Sender`. Subscribers: gateway WS write tasks (every connected UI/browser), the
supervisor (tools), the agent router (turns), the evolution applier, the store log writer,
and the scheduler/council handlers. **A frame that fails `Event` deserialization in the
gateway is silently dropped** — wrong field names produce no error, just nothing.

**Chat turn.** UI sends `{type:user_prompt}` → gateway `/ws` read task injects `session`,
deserializes to `Event::UserPrompt`, `bus.emit` → `Bus::run` applies state and rebroadcasts
→ `spawn_agent_router` matches `UserPrompt`, appends to session history, spawns `root_turn`
→ `run_turn` streams from the `RoutingProvider`, emitting `Event::AgentText` deltas back
onto the bus → gateway relays to all sockets.

**Tool round-trip.** `run_turn` emits `Event::ToolRequested` → gateway forwards to the UI
(renders a `tool_card`) **and** the supervisor consumes it; `PolicyEngine.check` decides
allow/ask. On Ask it emits `ApprovalPending` (UI shows approve/reject, sends
`{type:user_approval, action:<id>}`). On approval the supervisor calls the stdio MCP plugin
(`apexos-tools` or `cerebro-mcp`) and emits `Event::ToolResult` → `run_turn` awaits the
matching `ToolResult`, loops, then `TurnComplete`.

**Sensors.** `apex-sensor-bridge` (separate process) → WS push to `/sensor-bridge`
(`SENSOR_BRIDGE_TOKEN` auth, the one ungated route) → `Event::SensorReading` on the bus →
broadcast to UI sensor view / dashboard **and** persisted by the store log writer.

**Cerebro memory.** `agentd` spawns `cerebro-mcp` as an MCP plugin (`plugins.toml`); agent
memory tool calls route Supervisor → `McpClient` stdio → `cerebro-mcp` `dispatch.rs` →
`CerebroCortex` → SQLite/vector/graph + fastembed. `agentd` itself also drives Cerebro
directly (via `ToolProxy`) for the evolution rollback journal — `episode_start` /
`memory_store` / `episode_add_step` / `list_episodes` / `get_episode_memories`. `cerebro-api`
and `cerebro-cli` are independent binaries over the same engine library.

### Multi-client caveat (read before building a frontend)

The gateway broadcasts **every session's events to every connected socket** with no
server-side session filter. The same `session` field the gateway injects on inbound frames
appears on outbound frames — **clients MUST filter on it** or they will render another
session's output. The single-display kiosk does not hit this; a multi-client browser/PWA
deployment does.

---

## Protocol (agentd WebSocket)

The gateway sends the raw `Event` enum (`serde_json::to_string(&event)`, no reshaping). Tool
fields nest under `call` (a `ToolCall`); `action`/`session` ids are bare numbers, not
strings. Read `call.id` (number), stringify it for the row key.

Send on connect:
```json
{"type": "session_init"}
```
agentd replies:
```json
{"type": "hello", "session_id": 42}
```
Replay a prior session with `{"type": "session_init", "session_id": 42}`.

| Inbound event | Fields | Action |
|---------------|--------|--------|
| `agent_text` | `delta: string` | append to that row's bubble; **also the only signal that drives busy** |
| `turn_complete` | — | clear busy; TTS if enabled |
| `tool_requested` | `call: {id, tool, args, needs_approval}` | push tool card (running) |
| `tool_result` | `call: <id>, output: {ok, content}` | update card by id |
| `approval_pending` | `call: {id, tool, args}` | show approve/reject |
| `sensor_reading` | `reading: {kind, …}` | update IAQ / thermal |
| `wake_triggered` | — | flash wake indicator |

> **`turn_started` is Python-agentd-only.** The Rust daemon never emits it (see
> `main.rs` comments). Busy state is therefore driven solely by `agent_text` — which is why
> a tool-first turn (no leading text) does not set busy until text arrives. Do not wait on
> `turn_started`.

Send a prompt: `{"type": "user_prompt", "text": "hello"}`
Send approval: `{"type": "user_approval", "action": 5, "granted": true}` (the numeric
`ToolCall.id` — **not** `call_id`/`approved`).
Cancel: `{"type": "user_cancel"}`. `cascade_cancel` aborts the turn but emits **no**
`TurnComplete`, so the UI must clear its own busy + pending tool cards. (Partial assistant
output is discarded and persisted history can be left inconsistent.)

Full event list: `agentd/crates/core/src/types.rs` — `Event` enum.

---

## The cognitive loop (symbiosis)

ApexOS-RS is not just a chat client over a daemon; it is meant to run a continuous
**APEX ⇄ agentd ⇄ Cerebro** loop where the agent recalls context on wake, acts, and
consolidates memory on sleep. Two mechanisms make this more than a stateless turn engine:

**Self-evolution.** The agent can `propose_evolution`; the applier snapshots an undo state,
writes the targeted config (`soul.md` via plain `tokio::fs::write`, `policy.toml` via
`write_atomic`, `plugins.toml`/`peers.toml` via `tokio::fs::write`), hot-reloads policy and
the agent, and journals the undo into a Cerebro episode + an in-memory rollback store
(rebuilt on cold start). `rollback_evolution` replays the snapshot.

**Memory protocol.** Sessions begin with `session_recall` (prior summaries, unfinished
business, stored procedures — instant hotstart after compaction/reboot) and end with
`session_save`. Procedures, intentions, and episodes layer on top. This is the durable
counterpart to CLAUDE.md (static blueprint) and git (code truth).

### Known gaps (do not assume these work)

- **CCBS / `cognitive_bootstrap` is stubbed (not implemented).** `dispatch.rs` has a
  deliberate route arm for it that returns a *success* `not_yet_implemented` stub (kept as
  a success so APEX's soul-boot step-0 doesn't hard-fail), but it injects **zero** priming
  content. The two-tier soul / 12-slot bootstrap is **not load-bearing** in the Rust port
  yet; a Wake-loop calling `cognitive_bootstrap` as "step 0" silently primes nothing
  (audit finding CB-001).
- **Reinforcement is inert.** `recall` does not update FSRS/ACT-R activation; the memory-tier
  reinforcement story in CLAUDE.md is aspirational, not wired.
- **`ingest_file` / `describe_image` / `search_vision` are unimplemented** — they return an
  honest `-32601` not-implemented error (C-RS-007), and their advertised schemas are still
  placeholders (Step 9 schema work). (Spreading activation **does** enforce scope as of
  C-RS-003 — the earlier "ignores scope" claim is no longer true.)

See [symbiosis.md](symbiosis.md) for the full loop and the Sleep-loop gap.

---

## KMS/DRM (no Wayland, no cage)

Set `SLINT_BACKEND=linuxkms` at runtime. Slint renders directly via `/dev/dri/card0` (DRM
modesetting → HDMI) and `/dev/dri/renderD128` (GPU, GLES/Vulkan). This replaces the old
`cage` + `seatd` + kiosk stack entirely — no compositor, no seat manager.

- **Pi 5 (BCM2712 / VideoCore VII):** Debian trixie ships the `v3d` driver; `linuxkms` works
  out of the box. Verify `ls /dev/dri/` shows `card0` + `renderD128`.
- **Pi Zero 2W (BCM2837 / vc4):** no `v3d` — use `SLINT_BACKEND=linuxkms-femtovg` (software
  renderer, no GPU).
- **Desktop (x86):** `SLINT_BACKEND` auto-selects `winit` when `DISPLAY`/`WAYLAND_DISPLAY` is
  set. No special config.

The `slint` dependency needs the `backend-linuxkms-noseat` feature (default only compiles
winit); the UI declares `features = ["backend-linuxkms-noseat", "backend-winit"]`. KMS link
deps (`libgbm`, `libegl`, `libudev`, `libinput`, `libxkbcommon`, `libfontconfig1`,
`libssl`) are compiled in even on desktop, so `cargo build` (not just `check`) needs them
installed.

> **No key auto-repeat on linuxkms** — the backend dispatches one press/release per libinput
> event with no repeat synthesis. Holding a key = one hop on the Pi (works on desktop/winit).
> Backend limitation, not fixable in app code without forking the backend.

---

## Install, deploy, and the hardening model

`install.sh` is the single entry point for the whole distro:

1. Bootstrap apt deps + rustup.
2. Auto-detect hardware tier / deployment mode; optional whiptail TUI (routed through
   `/dev/tty` wrappers so it works under `curl | bash`).
3. Optional boot-file key provisioning (`find_key_file` probes mounted *and* unmounted
   removable partitions read-only).
4. Clone/pull `/opt/ApexOS-RS` as the unprivileged `BUILD_USER` (the rustup toolchain owner;
   falls back to the clone owner when run as bare root, e.g. the in-UI Update button).
5. `cargo build --release --workspace`; `install -m 755` the binaries into `/usr/local/bin`.
6. Write `/etc/agentd/{plugins,policy,soul,peers}.toml` and a `600 root:root` `env` file
   carrying a generated `AGENTD_TOKEN`.
7. Install + enable the systemd units from `deploy/`; drop an `apexos-update` helper; prewarm
   fastembed; start + health-check.

**Security model.**

- `agentd` runs as a dedicated `agentd` **system user**, jailed by the systemd sandbox in
  `deploy/agentd.service`: `NoNewPrivileges`, `ProtectSystem=strict`, `ProtectHome`,
  `PrivateTmp`, `ReadWritePaths=/var/lib/agentd /etc/agentd`,
  `WorkingDirectory=/var/lib/agentd/workspace`. This sandbox — not the tool layer — is the
  real confinement for `apexos-tools`, whose tools are otherwise unconfined.
- The agent-mutable config files (`soul.md`/`policy.toml`/`plugins.toml`/`peers.toml`) are
  individually `chown`ed to `agentd` so self-evolution can write them, while `/etc/agentd`
  itself stays root-owned to protect the token file. Because the directory is root-owned,
  temp+rename into it fails — so `write_atomic` falls back to an **in-place** write
  (in-place needs only file-write perm). Re-run `install.sh` to fix ownership on an
  already-deployed Pi.
- The **single genuinely solid network control** is the bind/auth gate in `main.rs`: the
  gateway defaults to `127.0.0.1:8787`, `AGENTD_BIND` overrides, and it **hard-bails** on a
  non-loopback bind if `AGENTD_TOKEN` is unset. The token gates `/ws`, `/terminal-ws`, and
  all `/api/*` (Bearer header or `?token=`); `/sensor-bridge` is ungated but does its own
  `SENSOR_BRIDGE_TOKEN` query check; static files are an ungated whitelist.
- `apexos-rs-ui` runs as **root** with a device allowlist — `drmSetMaster` +
  `drmModePageFlip` require DRM master, and on a seatless Pi only root wins reliably. The
  service uses `User=root`, `PAMName=login`, `TTYPath=/dev/tty7`,
  `WantedBy=multi-user.target` (Pi boots to `multi-user.target`, not `graphical.target`).
- **Policy posture.** `config/policy.toml` defaults `mode=suggest`: read-only verbs allowed,
  write/delete/`run_command`/`http_fetch` gated, wake-loop boot verbs explicitly allowed so
  startup orient never blocks on approval. Caveat: "read-only" is **not** "safe" — there is
  no workspace rooting on `read_file`/`list_dir`, so the agent can read any file the
  `agentd` user can see (including `/etc/agentd/env`). And `apexos-tools`' `run_command`
  denylist is a soft substring heuristic, trivially bypassable; treat the systemd sandbox,
  not the denylist, as the boundary.

### Deploy / hot-swap workflow

```bash
# On the Pi — always build on-device (Cortex-A76 arm64), never cross-compile
cd ~/ApexOS-RS && git pull
cargo build --release --workspace

# Hot-swap one binary (must stop the service first — a running binary is "text file busy")
sudo systemctl stop agentd
sudo cp target/release/cerebro-mcp /usr/local/bin/cerebro-mcp
sudo systemctl start agentd

sudo systemctl stop apexos-rs-ui
sudo cp target/release/ui-slint /usr/local/bin/apexos-rs-ui
sudo systemctl start apexos-rs-ui
```

In UI development, run the binary directly — no service needed:
```bash
AGENTD_WS=ws://localhost:8787/ws SLINT_BACKEND=linuxkms ./target/release/ui-slint
```
For LAN dev against a remote Pi, set `AGENTD_BIND=0.0.0.0:8787` in the Pi's
`/etc/agentd/env` and pass the token:
```bash
AGENTD_TOKEN=$(ssh apex1@192.168.0.158 'sudo grep -oP "(?<=AGENTD_TOKEN=).*" /etc/agentd/env') \
AGENTD_WS=ws://192.168.0.158:8787/ws cargo run
```
