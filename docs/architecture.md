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
same intent frames (`user_prompt`/`user_approval`/`user_cancel`). **What changed is the perimeter.** This repo is no longer a renderer
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

The UI still has **no Cargo dependency** on agentd internals — the coupling is the
wire protocol, now expressed as shared Rust types: the lean serde-only
`apexos-protocol` root crate holds the `Event` enum (`apexos-core` re-exports it, so
`apexos_core::Event` is unchanged daemon-side), and `ui-slint` depends on it directly,
deserializing WS frames into the typed `Event` and **logging** any undecodable frame
instead of silently dropping it. (The old hand-matched-JSON era is over.)

---

## Crate inventory

Seven binaries come out of `cargo build --release --workspace` (resolver 2, release
profile `lto=thin`, `strip`). Library crates are listed alongside the binaries that use
them. Two shared **root crates** sit above the three directory groups:
`apexos-protocol` (the serde-only wire contract — the `Event` enum + id newtypes,
re-exported by `apexos-core`; **no_std-capable**: default `std`, with
`--no-default-features --features alloc` as a second build gate, consumed bare-metal by
ApexOS-RV) and `apexos-confine` (std-only path-confinement primitives —
`confine_fs`/`confine_to_roots`, used by `apexos-tools`). The two voice sidecars
(`tools/crates/apex-tts` Kokoro TTS, `tools/crates/apex-stt` Whisper STT) are
**workspace-excluded** — they pin their own `ort`/C++ deps and are built separately by
`install.sh`.

### `agentd/crates/` — the agent daemon

| Crate | Kind | Role |
|-------|------|------|
| `apexos-core` | lib | Shared types and the event bus. The wire protocol — `Event` / `ContentBlock` / `ToolCall` / `PolicyMode` / `EvolutionProposal`, with `ActionId`/`SessionId` newtypes that serialize as **bare numbers** — lives in the root `apexos-protocol` crate, re-exported here (`pub use apexos_protocol as types`). `bus.rs` is the in-process hub: `Bus::new` returns a `BusHandle` (mpsc inbox `emit`) and a `broadcast::Sender` (capacity 1024) to subscribe; `Bus::run` applies `SystemState::apply` (`state.rs`) then rebroadcasts. Also home to `history.rs` (the trim window + seam marker), `identity.rs` (agent identity, per-agent workspace roots, session bindings, `SPAWN_SESSION_BASE`/`is_spawn_session`), `persona.rs` (per-session style), and `vision.rs` (the image downscale shim). |
| `apexos-gateway` | lib | The entire external surface. One axum `Router` (`router()`): `/ws` (UI/browser), `/sensor-bridge` (sensor ingest), `/terminal-ws` (PTY), REST `/api/*` (run/speak/record/transcribe/council/sessions/policy/model/backend/voice/auth/identities/mesh/media/workspace/…), plus mesh peer discovery (`mesh.rs`, avahi). `GatewayState` is a fat clone-of-Arcs struct holding the `BusHandle`, the broadcast sender, live-swappable model/backend/keys/policy `RwLock`s, the in-memory session `histories` map (shared `Arc<Mutex<HashMap>>` with the agent router), the two shared tokens (`AGENTD_TOKEN` + sensor-bridge), and the in-memory human session-token store (`session_auth` — minted by `POST /api/auth/login`, accepted by `require_token` alongside the shared token). |
| `apexos-plugins` | lib | MCP plugin host + policy engine. `Supervisor::run` (`supervisor.rs`) multiplexes bus events (`ToolRequested` → `PolicyEngine.check` → dispatch or `ApprovalPending`; `UserApproval` resolves pending) and `SupervisorCmd` messages, spawns/supervises stdio MCP child processes (`mcp.rs`, newline-delimited JSON-RPC), and intercepts ~thirty *virtual tools* (propose/rollback_evolution, read_soul_md, soul_rehearse, schedule_* incl. the wakeup family, the goal_* family, convene_council, send_to_agent, the mesh_* family — file/memory/procedure relay, recall, capabilities — agent_spawn local + cross-node, query_event_log, apply_daemon_update, bootstrap_node, vast_*) before falling through to a real plugin. Dispatch also **system-stamps** plugin calls: `agent_id` on every cerebro call (`stamp_agent_id`), `__workspace` on every apexos-tools call, and a `spawn-derived` provenance tag on Cerebro mints from spawn sessions (`is_spawn_session`); an `agent_spawn` with no explicit `system` gets a minimal task charter (`resolve_spawn_system` — `inherit_soul:true` opts into full identity). `policy.rs` is pure rule eval (Allow/Ask, Yolo short-circuit, exact-then-`prefix.*` wildcard, Workspace rule). `config.rs` loads `plugins.toml`; `vast.rs` wraps vast.ai GPU recipes. |
| `apexos-agent` | lib | Turn engine. `turn.rs` `run_turn` streams from the provider, emits `AgentText` deltas + `ToolRequested`, awaits matching `ToolResult` (a bounded timeout synthesizes an **honest blocker** via the pure `missing_result_message` — still-awaiting-approval / approved-but-silent / dispatched-stalled / bus-lagged — so a turn never wedges and never misreads a pending approval as a decline), loops to `TurnComplete`. `provider.rs` = `Provider` trait + `Chunk` stream; `anthropic.rs`/`oai.rs` are the two impls; `routing.rs` `RoutingProvider` dispatches per-call off a live-swappable backend `Arc<RwLock<String>>` so hot-swaps land on the next turn with no restart; `council.rs` runs parallel multi-agent deliberation. |
| `apexos-store` | lib | `run_log_writer` — subscribes to the broadcast bus and appends every `Event` as date-rolling JSONL. |
| `agentd` | **bin** | The daemon. Manual multi-thread tokio runtime (never `#[tokio::main]`). Wires `Bus::new`/`run` + gateway + supervisor + turn engine + scheduler + council; loads soul (`AGENTD_SOUL` or `config/soul.md`) and keys. `spawn_agent_router` is the central dispatcher (consumes broadcast `Event`s, owns abort handles / session children / depths, routes `UserPrompt`→`root_turn`, handles `SpawnAgent`/`AgentMessage`/`UserCancel`/sensor-alert→prompt). `spawn_evolution_applier` consumes `EvolutionProposed`, snapshots undo state, applies via `apply_evolution` (writes `soul.md`/`policy.toml`/`plugins.toml`, hot-reloads policy/agent), and journals undo into Cerebro episodes; the pure evolution core (kind/invert, the undo codec, the TOML edits) lives in `evolution.rs`. The **consolidate/rehearse worker seam**: `consolidate.rs` (session → Cerebro distillation) and `rehearse.rs` (`soul_rehearse` — tool-less probes of a candidate soul) run as agentd-side workers that own the provider + `ToolProxy`; the gateway/supervisor forward requests over dedicated mpscs and await the reply (never the lag-droppable broadcast bus). `scheduler.rs` (cron, polls every 60s), `council_handler.rs`, `session_store.rs` (per-root-session history JSONL), `goal.rs` (the autonomous goal driver), `self_update.rs` + `health.rs` (the self-update loop + health contract), `sensor_config.rs` (alert-profile persistence), `pac_lint.rs` (the pure PAC-2 Dense lint gate on `UpdateSystemPrompt` payloads), and `dream_digest.rs` (post-dream mesh push) round it out. |

### `cerebro/crates/` — cognitive memory

| Crate | Kind | Role |
|-------|------|------|
| `cerebro` | lib | The engine. `cortex.rs` `CerebroCortex` is the public facade owning storage + the cognitive engines (`engines/` — hippocampus, neocortex, amygdala, prefrontal, dream, …). `storage/` = `sqlite.rs` (source of truth), `vector.rs` (sqlite-vec vec0 with FTS5 fallback), `graph.rs` (in-memory petgraph rebuilt at startup). `activation/` (`actr.rs`, `fsrs.rs`, `spreading.rs`) is pure math. `engines/dream.rs` runs the 6-phase consolidation (plus the exo-evolution variation/competition phases) against claude-haiku. `config.rs` `Config::from_env`. |
| `cerebro-mcp` | **bin** | MCP-over-stdio server — the plugin `agentd` spawns for agent memory (agent `FORGE`). `dispatch.rs` routes 67 tools (66 functional + the deferred `ingest_file` stub) over `Arc<CerebroCortex>`; `tools.rs` is the static schema registry; `transport.rs` is the stdio JSON-RPC loop. `agent_scope(args)` maps an optional `agent_id` to a `VisibilityScope` — the single scoping primitive. |
| `cerebro-api` | **bin** | axum REST + dashboard over the same engine (memory/episode/graph/tag endpoints, ~40 routes). Enforces the shared `AGENTD_TOKEN` bearer secret and refuses a non-loopback bind when it is unset. Binds **:8765**. |
| `cerebro-cli` | **bin** (`cerebro`) | clap CLI over the engine for human/script use. |

### `tools/crates/` — system tools and sensors

| Crate | Kind | Role |
|-------|------|------|
| `apexos-tools` | **bin** | MCP-over-stdio system-tool plugin spawned by the supervisor: shell (`run_command`), file (read/write/list/create/delete), git (`git_*`, argv-only — never `/bin/sh`), `http_fetch`, system telemetry (cpu_temp/disk/memory/uptime via `/proc`+`df`), notify (jsonl/notify-send/TTS/ntfy/telegram), audio (ffmpeg), GPIO/PWM/servo (sysfs + libgpiod), display_face/sketch_draw, camera_capture/screenshot_mirror, eject_media, and the `ui_*` adaptive-UI staging verbs (ui_open/close/focus/query/arrange/theme/reflex — validate+echo handlers, applied by ui-slint). **FS and git tools self-confine in-process** — `tools.rs::confine` (policy) over the std-only `apexos-confine` root crate (mechanism): writes are workspace-rooted, reads get the workspace + a small allowlist minus a secret denylist; the agentd `PolicyEngine` plus the systemd sandbox layer on top (see Security). |
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
(`SENSOR_BRIDGE_TOKEN` auth — its own gate, separate from the API token) →
`Event::SensorReading` on the bus →
broadcast to UI sensor view / dashboard **and** persisted by the store log writer.
A threshold-crossing that survives agentd's persistence filter additionally emits the
**global** `Event::SensorAlert` (node_id/kind/value/threshold/sensor_id) — the
machine-readable twin of the root-session alert prompt, fired at the same moment; it is
the trigger for `ui_reflex` rules.

**Cerebro memory.** `agentd` spawns `cerebro-mcp` as an MCP plugin (`plugins.toml`); agent
memory tool calls route Supervisor → `McpClient` stdio → `cerebro-mcp` `dispatch.rs` →
`CerebroCortex` → SQLite/vector/graph + fastembed. `agentd` itself also drives Cerebro
directly (via `ToolProxy`): the evolution rollback journal (`episode_start` /
`memory_store` / `episode_add_step` / `list_episodes` / `get_episode_memories`), the
per-session CCBS boot priming (`cognitive_bootstrap` on a session's first turn), the
nightly `dream_run` cron + dream-digest export, and session consolidation
(`session_save` from the consolidate worker). `cerebro-api`
and `cerebro-cli` are independent binaries over the same engine library.

### Multi-client routing (read before building a frontend)

The gateway write task filters outbound frames **per socket** (`event_session`,
`gateway/src/lib.rs`): a session-scoped event (the conversation stream —
`agent_text`/`tool_requested`/`turn_complete`/`approval_pending`/…) reaches only the
socket bound to that session; global/status events (sensors, mesh, council, vast,
evolution) go to every client. A frontend therefore receives **only its own session's
stream + globals** — clients don't (and shouldn't) filter outbound frames themselves.
Anything whose routing is ambiguous stays global, so no status event is ever hidden; the
supervisor subscribes to the bus separately, so the filter never affects tool routing.

---

## Protocol (agentd WebSocket)

The gateway sends the raw `Event` enum (`serde_json::to_string(&event)`, no reshaping). Tool
fields nest under `call` (a `ToolCall`); `action`/`session` ids are bare numbers, not
strings. Read `call.id` (number), stringify it for the row key.

On connect the gateway allocates a session and **pushes** (no client frame needed):
```json
{"type": "session_init", "session_id": 42, "history": []}
```
Replay a prior session with `{"type": "hello", "resume_session": 42}` — the gateway
replies with a fresh `session_init` carrying the replayed history; `{"type": "hello",
"new": true}` mints a fresh session on the live socket. A `hello` may also carry
`persona` (voice) and `agent_id` (identity bind); `{"type": "set_persona"}` switches the
voice live without re-initing the session.

| Inbound event | Fields | Action |
|---------------|--------|--------|
| `agent_text` | `delta: string` | append to that row's bubble; **also the only signal that drives busy** |
| `turn_complete` | — | clear busy; TTS if enabled |
| `tool_requested` | `call: {id, tool, args, needs_approval}` | push tool card (running) |
| `tool_result` | `call: <id>, output: {ok, content}` | update card by id |
| `approval_pending` | `call: {id, tool, args}` | show approve/reject |
| `sensor_reading` | `reading: {kind, …}` | update IAQ / thermal |
| `sensor_alert` | `node_id, kind, value, threshold, sensor_id` | global persistence-filtered alert (fires `ui_reflex` rules) |
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

Full event list: `apexos-protocol/src/lib.rs` — the `Event` enum (re-exported as
`apexos_core::Event`).

---

## The cognitive loop (symbiosis)

ApexOS-RS is not just a chat client over a daemon; it is meant to run a continuous
**APEX ⇄ agentd ⇄ Cerebro** loop where the agent recalls context on wake, acts, and
consolidates memory on sleep. Two mechanisms make this more than a stateless turn engine:

**Self-evolution.** The agent can `propose_evolution`; the applier snapshots an undo state,
writes the targeted config (policy.toml / plugins.toml / soul.md — the three evolution
targets — via `write_atomic`: temp+rename, falling back to an in-place write when
`/etc/agentd` is root-owned; peers.toml shares the helper but is written by the mesh
code, not the applier), hot-reloads policy and
the agent, and journals the undo into a Cerebro episode + an in-memory rollback store
(rebuilt on cold start; `restore_rollback_store` also heals off-spec fossil snapshots in
place). Undo snapshots are stored **private, attributed to the evolving agent, salience
0.25** (the colony C1 leak fix), and a full `update_system_prompt` rewrite refuses to
apply until its undo is durably persisted — verified by stored-memory id (the H4 snapshot
gate). The ack is deferred over a dedicated mpsc so the tool result carries the real apply
outcome. `rollback_evolution` replays the snapshot; `soul_rehearse` (the fitting room)
lets the current self probe a candidate soul on an ephemeral, tool-less mind — with an
optional `compare_to` A/B against a second soul — before proposing.

**Memory protocol.** Sessions begin with `session_recall` (prior summaries, unfinished
business, stored procedures — instant hotstart after compaction/reboot) and end with
`session_save`. Procedures, intentions, and episodes layer on top. This is the durable
counterpart to CLAUDE.md (static blueprint) and git (code truth).

### Known gaps (do not assume these work)

- **`cognitive_bootstrap` (CCBS) is implemented AND daemon-injected** — a live-state
  assembler pulls open intentions + query-relevant session summaries, procedures, and
  memories into a token-budgeted priming block (`dispatch.rs::assemble_bootstrap`, CB-001
  closed), and agentd now calls it itself on a session's **first turn** (`boot_priming_for`
  in `main.rs` — bounded 15s, graceful, cached per session) and appends it to the system
  prompt via `TurnEngine::with_priming`. The authored `# Module: X` skill-module layer
  remains a roadmap item.
- **Recall reinforcement is wired.** `recall()` records an access on the returned memories so
  ACT-R base-level activation rises ("recall sharpens memory"). FSRS *grading* still happens
  only via `record_procedure_outcome`, not on ordinary reads.
- **`ingest_file` is unimplemented** — it returns an honest `-32601` not-implemented error
  (C-RS-007). (`describe_image` and `search_vision` **are** implemented now —
  `cerebro::vision`: a tiered Ollama→Anthropic VLM caption tool, and CLIP visual recall
  over a `vision_embeddings` table.) (Spreading activation **does** enforce scope as of
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
6. Write the `/etc/agentd/` config (`plugins.toml`/`soul.md`/`peers.toml` seed-if-absent;
   `policy.toml` seed-or-additively-sync — new rule keys append, existing values never
   touched) and a `600 root:root` `env` file carrying a generated `AGENTD_TOKEN`.
7. Install + enable the systemd units from `deploy/`; drop an `apexos-update` helper; prewarm
   fastembed; start + health-check.

**Security model.**

- `agentd` runs as a dedicated `agentd` **system user**, jailed by the systemd sandbox in
  `deploy/agentd.service`: `NoNewPrivileges`, `ProtectSystem=strict`, `ProtectHome`,
  `PrivateTmp`, `ReadWritePaths=/var/lib/agentd /etc/agentd`,
  `WorkingDirectory=/var/lib/agentd/workspace`. The sandbox layers over the in-tool FS
  confinement (`tools.rs::confine` over `apexos-confine`) — the tool gate is per-agent and
  workspace-rooted; the sandbox bounds whatever the process itself can reach.
- The agent-mutable config files (`soul.md`/`policy.toml`/`plugins.toml`/`peers.toml`) are
  individually `chown`ed to `agentd` so self-evolution can write them, while `/etc/agentd`
  itself stays root-owned to protect the token file. Because the directory is root-owned,
  temp+rename into it fails — so `write_atomic` falls back to an **in-place** write
  (in-place needs only file-write perm). Re-run `install.sh` to fix ownership on an
  already-deployed Pi.
- The **single genuinely solid network control** is the bind/auth gate in `main.rs`: the
  gateway defaults to `127.0.0.1:8787`, `AGENTD_BIND` overrides, and it **hard-bails** on a
  non-loopback bind if `AGENTD_TOKEN` is unset. The token gates `/ws`, `/terminal-ws`, and
  all `/api/*` (Bearer header or `?token=`) — `require_token` also accepts a **minted human
  session token** (`POST /api/auth/login`, in-memory, 24 h); the login pair
  (`/api/auth/login` + `GET /api/auth/profiles`) and the mesh pairing claim are
  deliberately ungated (authenticated by the profile PIN / the short-lived code itself).
  `/sensor-bridge` is ungated but does its own `SENSOR_BRIDGE_TOKEN` query check; static
  files are an ungated whitelist.
- `apexos-rs-ui` runs as **root** with a device allowlist — `drmSetMaster` +
  `drmModePageFlip` require DRM master, and on a seatless Pi only root wins reliably. The
  service uses `User=root`, `PAMName=login`, `TTYPath=/dev/tty7`,
  `WantedBy=multi-user.target` (Pi boots to `multi-user.target`, not `graphical.target`).
- **Policy posture.** `config/policy.toml` defaults `mode=suggest`: read-only verbs allowed,
  write/delete/`run_command`/`http_fetch` gated, wake-loop boot verbs explicitly allowed so
  startup orient never blocks on approval. Because `read_file`/`list_dir` are `allow`, the
  **tool process is the only gate** for reads — `tools.rs::confine` roots them to the
  workspace + a small read allowlist (`/etc/agentd/parts`, `/sys`, `/proc/cpuinfo`/`meminfo`,
  `/var/lib/agentd/update`; `AGENTD_READ_ROOTS`-extensible) minus an always-blocked secret
  denylist (`/etc/agentd/env`, `/proc/*/environ`, `~/.ssh`, `/etc/shadow`, `*.api_key`).
  `apexos-tools`' `run_command` denylist remains a soft substring heuristic, trivially
  bypassable; treat the approval gate + systemd sandbox, not the denylist, as the boundary.

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
For LAN dev against a remote Pi: `install.sh` seeds `AGENTD_BIND=0.0.0.0:8787` in the
Pi's `/etc/agentd/env` (safe — a token is required for any non-loopback bind), so a
provisioned Pi is LAN-reachable out of the box; just pass the token:
```bash
AGENTD_TOKEN=$(ssh apex1@192.168.0.158 'sudo grep -oP "(?<=AGENTD_TOKEN=).*" /etc/agentd/env') \
AGENTD_WS=ws://192.168.0.158:8787/ws cargo run
```
