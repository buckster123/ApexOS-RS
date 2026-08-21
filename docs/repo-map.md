# Repo Map — ApexOS-RS

> Developer navigation guide for the ApexOS-RS monorepo. Find the crate, file, or
> change-point you need without grepping the whole tree.
>
> See also: [architecture.md](architecture.md) · [build-roadmap.md](build-roadmap.md) · [slint-notes.md](slint-notes.md)

ApexOS-RS is one Cargo workspace (`resolver = 2`, release `lto=thin strip`) producing
**7 binaries**: the agent daemon plus the Cerebro memory stack, the system tool plugins,
and the native Slint UI — plus **2 workspace-EXCLUDED voice sidecars** (`apex-tts`, `apex-stt`:
their own workspaces/lockfiles, built separately by install.sh) and the **`web/` browser/PWA**
(static assets, no build step). One `cargo build --release --workspace`. One `install.sh`.

---

## At a glance

```
ApexOS-RS/
├── apexos-protocol/                # shared wire protocol — Event enum + WS/a2a contract (serde-only)
├── apexos-mesh-proto/              # ApexNET radio/UART wire contract — framing, crypto envelope, chunker (no_std+alloc)
├── apexos-confine/                 # path-confinement primitives — the FS-sandbox algorithm (std-only)
│
├── agentd/crates/                  # the agent daemon — event bus, gateway, plugins, turn engine
│   ├── core         apexos-core      runtime shared state: Bus, SystemState, identity/persona/history,
│   │                                 vision shim; re-exports apexos-protocol as `types`
│   ├── gateway      apexos-gateway   axum HTTP+WS server: /ws, /sensor-bridge, /terminal-ws, /api/*,
│   │                                 mesh + beacon + session-auth + voice plans + web static
│   ├── plugins      apexos-plugins   MCP plugin host + PolicyEngine (approval) + vast.ai recipes
│   │                                 + the Tier-4 courier lane (courier.rs — outbox, sealed manifests)
│   ├── agent        apexos-agent     turn engine: LLM stream → tool round-trips → council; cache + usage
│   ├── store        apexos-store     append-only JSONL event-log writer
│   └── agentd       agentd  (bin)    main daemon: wires bus+gateway+supervisor+turn+scheduler+goals
│                                     +workers/mandalas+evolution+self-update+dream-digest+rehearse
│
├── cerebro/crates/                 # cognitive-memory stack (agent FORGE's brain)
│   ├── cerebro      cerebro          engine lib: SQLite+vec, petgraph, ACT-R/FSRS, brain engines, vision
│   ├── cerebro-mcp  cerebro-mcp (bin) MCP-over-stdio server, 67 advertised memory tools (agentd spawns this)
│   ├── cerebro-api  cerebro-api (bin) axum REST API + dashboard over the engine
│   └── cerebro-cli  cerebro     (bin) clap CLI over the engine
│
├── tools/crates/                   # system tool plugins + voice sidecars
│   ├── apexos-tools      apexos-tools      (bin) MCP stdio: shell/file/git/http/sysinfo/audio/GPIO
│   ├── apex-sensor-bridge apex-sensor-bridge (bin) WS client: CPU temp / SensorHead → /sensor-bridge
│   ├── apexos-mesh-bridge apexos-mesh-bridge (bin) ApexNET UART link daemon + apexos-brainstem-sim (bin)
│   │                       + apexos-brainstem-provision (bin) — the commissioning one-shot
│
├── firmware/brainstem/             # ESP32 brainstem firmware — workspace-EXCLUDED (Xtensa/`esp`
│                                   # toolchain, own Cargo.lock); see firmware/README.md
│   ├── apex-tts          apex-tts (bin, EXCLUDED) Kokoro TTS sidecar — own workspace/lock, :8770
│   └── apex-stt          apex-stt (bin, EXCLUDED) Whisper STT sidecar — own workspace/lock, :8771
│
├── ui-slint/                       # the unique contribution — native Slint KMS/DRM UI
│   └── ui-slint  (bin → apexos-rs-ui)  chat/tools/dashboard/sensor/council/terminal + GL face
│
├── web/                            # browser + mobile-PWA frontend (vanilla JS, no build;
│                                   # install.sh → /var/lib/agentd/ui on every node)
├── config/                         # default plugins.toml, policy.toml, soul.md, parts/inventory.toml
├── deploy/                         # systemd units + avahi/udev/usb helpers, fonts, self-update scripts
└── install.sh                      # one-shot installer (deps → build → install → systemd → policy sync)
```

---

## Per-crate reference

| Crate | Path | Role | Key files | Depends on |
|-------|------|------|-----------|------------|
| **apexos-protocol** | `apexos-protocol` (repo root) | Shared wire-protocol types — the `Event` enum + ToolCall/ContentBlock/Message + ID newtypes. serde-only so frontends depend on it without the daemon stack; both WS ends share these types. **no_std-capable** (default `std`; `--no-default-features --features alloc` is a second build gate — consumed bare-metal by ApexOS-RV). | `src/lib.rs` (828 lines — `Event` :306, `ToolCall` :523, `ContentBlock` :581, PolicyMode/EvolutionProposal/SensorReading, the worker/batch events (`TaskBatchDone` :483, `WorkerStateChanged` :493), the `Map<K,V>` HashMap⇄BTreeMap alias) | serde, serde_json (default-features off for the no_std gate) |
| **apexos-confine** | `apexos-confine` (repo root) | Path-confinement primitives — the FS-sandbox *algorithm* (policy via `confine_fs`, IO via `openat2` `Beneath`, Landlock allowlist for the same-uid tools worker). Policy *values* live in apexos-tools `tools.rs::confine`. | `src/lib.rs` + `src/openat.rs` + `src/landlock.rs` | libc (linux) |
| **apexos-core** | `agentd/crates/core` | Runtime shared state + the in-process event Bus (mpsc inbox → broadcast out). Re-exports `apexos-protocol` as `types` (so `apexos_core::Event` still resolves). | `src/lib.rs` (re-export glue) · `src/bus.rs` (`Bus::new` → BusHandle emit + broadcast subscribe; `Bus::run` applies state then rebroadcasts) · `src/state.rs` (`SystemState::apply`) · `src/identity.rs` (identity registry, SessionBindings, workspace roots, `SPAWN_SESSION_BASE`/`is_spawn_session` — the shared ephemeral-spawn-session constant) · `src/persona.rs` (`persona_style`) · `src/history.rs` (`trim_history`) · `src/vision.rs` (image downscale shim) · `src/connectivity.rs` (ApexNET P5a — the latched process-global `ConnectivityState`, `Latch`/`derive_state`, and the `connectivity.toml` gating rules every consumer shares) | apexos-protocol, serde, serde_json, tokio, anyhow, image, base64, toml, sha2, subtle, rand |
| **apexos-gateway** | `agentd/crates/gateway` | axum HTTP+WS server — the entire external surface of agentd. | `src/lib.rs` (6376 lines — `GatewayState` :102, `require_token` :263, `router()` :318, `ws_handler` :444 / `handle_socket` :525, `handle_sensor_bridge` :769, `static_handler` :799 — the web/ filename whitelist, voice `stt_plan` :2696 / `tts_plan` :3124, `handle_terminal_ws` :3473, `serve()` :5983) · `src/mesh.rs` (PeerRegistry, avahi discovery, pairing) · `src/beacon.rs` (downtime-beacon state machine) · `src/session_auth.rs` (login session tokens, `gate_agent_bind`) · `src/backend_config.rs` (`resolve_boot` — persisted > env > backend-aware defaults) · `src/history_config.rs` (persisted history-window budget — the `/api/history` Settings choice wins over the `AGENTD_HISTORY_TOKEN_BUDGET` env seed; mirrors `backend_config`'s load/persist/resolve shape) · `src/compute.rs` (LAN compute scan → `/api/compute/discover`) | apexos-core, apexos-plugins, apexos-agent (CacheConfig), axum, tokio, futures-util, reqwest, libc, toml, chrono, tiny-skia, subtle, percent-encoding |
| **apexos-plugins** | `agentd/crates/plugins` | MCP plugin host: spawn/supervise stdio plugins, route tool calls, enforce approval policy, vast.ai recipes. | `src/supervisor.rs` (`Supervisor::run` :367, `ToolProxy::call`, `SupervisorCmd`, `dispatch_tool` :558 virtual-tool chain — `task_fanout`/`worker_*`/`mandala_*` route to the worker driver via `set_worker_tx` (:785) — `resolve_spawn_system` :112 — spawn task-charter vs `inherit_soul` — + the `spawn-derived` cerebro-tag stamp for spawn sessions — 3008 lines) · `src/mcp.rs` (`McpClient` over child stdio; `request()` bounded by `AGENTD_TOOL_RESULT_TIMEOUT_SECS`, default 1800s) · `src/policy.rs` (`PolicyEngine`, `Rule`, `Decision`) · `src/config.rs` (plugins.toml loader, RestartPolicy) · `src/vast.rs` · `src/courier.rs` (ApexNET Tier-4 courier lane — marker v2 `stick_id`, PSK-sealed manifest/receipts, blake3 cargo, `outbox.jsonl`, heard-gossip ledger, `process_plug`, queue-time dest resolution (`resolve_dest`) + the `courier_queue`/`courier_cancel`/`courier_status` virtual tools) | apexos-core, apexos-confine, apexos-mesh-proto, tokio, serde, serde_json, anyhow, toml, chrono, reqwest, rand, base64 |
| **apexos-agent** | `agentd/crates/agent` | Agent turn engine: stream from LLM providers, drive tool round-trips over the Bus, run councils. | `src/turn.rs` (`run_turn`: stream → AgentText, emit ToolRequested, await ToolResult, loop; `compose_system`, `vision_rewrite`, `inject_ambient`) · `src/provider.rs` (`Provider` trait + Chunk stream) · `src/routing.rs` (`RoutingProvider`, live backend swap) · `src/anthropic.rs` / `src/oai.rs` · `src/council.rs` (`run_council`) · `src/cache.rs` (`CacheConfig` — runtime prompt-cache knobs) · `src/usage.rs` (process-global token/cost accumulator → `/api/usage`) | apexos-core, tokio, reqwest, async-trait, async-stream, futures-util, bytes, serde, serde_json |
| **apexos-store** | `agentd/crates/store` | Append-only event-log writer; subscribes the broadcast bus, persists JSONL (date-rolling). | `src/lib.rs` (`run_log_writer` — single pub async fn) | apexos-core, serde_json, tokio, anyhow, chrono |
| **agentd** | `agentd/crates/agentd` | Main daemon binary: wires everything, owns the agent-router, evolution-applier, goal-driver and self-update loops. | `src/main.rs` (4645 lines — Bus wiring, `spawn_evolution_applier` :888, `spawn_agent_router` :1588 routing UserPrompt→root_turn, `spawn_nightly_dream` :2246, `gather_tools` :2456, `build_embodiment` :2644) · `src/scheduler.rs` (cron) · `src/council_handler.rs` · `src/session_store.rs` · `src/goal.rs` (autonomous goal driver, `spawn_goal_driver` :256) · `src/worker.rs` (worker/mandala driver, `spawn_worker_driver` :1044 — a bus subscriber + the supervisor's worker-tool channel; fan-out incl. W2/M2 remote rows and cross-node rings, barriers, the review tick, torus epochs — 4793 lines) · `src/remote.rs` (pure W2 mesh-worker seam: wire payloads, tolerant state mapping, mirror docs, poll cadence) · `src/mandala.rs` (pure mandala core: forms, addresses, budgets, the on-disk tree) · `src/review.rs` (pure review procedure: posture × word → one remediation, golden offsets, fib backoff) · `src/evolution.rs` (pure evolution state machine: kind/invert, undo codec, TOML edits) · `src/pac_lint.rs` (pure PAC-2 Dense structural lint gate on `UpdateSystemPrompt` payloads) · `src/self_update.rs` + `src/health.rs` (self-update loop + health contract) · `src/consolidate.rs` (session→Cerebro distiller worker) · `src/rehearse.rs` (`soul_rehearse` fitting-room worker — ephemeral tool-less probes on a candidate soul, 6-probe default battery / ≤6 custom + `compare_to` A/B) · `src/dream_digest.rs` (post-dream federation push, echo-guarded) · `src/sensor_config.rs` (alert-profile persistence) | apexos-core, apexos-gateway, apexos-plugins, apexos-agent, apexos-store, tokio, toml_edit, cron, chrono, anyhow |
| **cerebro** | `cerebro/crates/cerebro` | Cognitive-memory engine lib: SQLite+vec storage, petgraph graph, ACT-R/FSRS activation, fastembed, brain-region engines, VLM/CLIP vision. | `src/cortex.rs` (`CerebroCortex` facade) · `src/engines/` (hippocampus/neocortex/amygdala/prefrontal/dream/…) · `src/storage/` (sqlite.rs, vector.rs, graph.rs) · `src/activation/` (actr.rs, fsrs.rs, spreading.rs) · `src/ingest.rs` (the `ingest_file` pipeline — extension-routed format handlers, chunks stored through the normal `remember()` gauntlet; images via the vision backend, PDF via lopdf — 822 lines) · `src/vision.rs` (`describe_image` VLM backends + CLIP towers) · `src/models/` (memory/episode/link/agent structs) · `src/types.rs` · `src/config.rs` (`Config::from_env`) | rusqlite, sqlite-vec, petgraph, fastembed, tokio, reqwest, uuid, chrono, notify, dirs-next, serde, tracing |
| **cerebro-mcp** | `cerebro/crates/cerebro-mcp` | MCP-over-stdio server exposing **67 advertised** Cerebro tools, all functional (count asserted by the `tools.len()` test in dispatch.rs); the plugin agentd spawns for agent memory. | `src/main.rs` (initialize handshake + read/dispatch/write loop) · `src/dispatch.rs` (`route(name,args,brain)` + the audit write chokepoint — every successful mutating call logs one row via the `audit_action` whitelist → `log_audit_event` — 2735 lines) · `src/tools.rs` (schema registry) · `src/transport.rs` (`StdioTransport`) | cerebro, tokio, serde, serde_json, anyhow, tracing, uuid |
| **cerebro-api** | `cerebro/crates/cerebro-api` | axum REST API + dashboard over the engine (~40 routes); AGENTD_TOKEN bearer middleware. | `src/main.rs` (1040 lines — all handlers + router) | cerebro, axum, tower, tokio, serde, serde_json, chrono, uuid, tracing |
| **cerebro-cli** | `cerebro/crates/cerebro-cli` | clap CLI over the engine (binary named `cerebro`). | `src/main.rs` (Cli/Command/Subcommand tree, 832 lines) | cerebro, clap, tokio, serde, serde_json, chrono, uuid, tracing |
| **apexos-tools** | `tools/crates/apexos-tools` | MCP-over-stdio system tool plugin: shell/file/git/http/sysinfo/audio/GPIO/display/media + the `ui_*` adaptive-UI staging verbs (open/close/focus/query/arrange/theme/reflex — validate+echo, applied by ui-slint), with a command denylist. | `src/main.rs` (stdio JSON-RPC loop) · `src/tools.rs` (`list()`/`call()` + 51 tool impls + `denylist_check` + `confine` policy values — 3838 lines) | serde, serde_json, reqwest (blocking), apexos-confine |
| **apex-sensor-bridge** | `tools/crates/apex-sensor-bridge` | Standalone WS client: polls CPU temp / SensorHead (BME688, MLX90640), pushes SensorReading to `/sensor-bridge`. | `src/main.rs` (299 lines — `read_cpu_temp`, SensorHead HTTP poll, tungstenite WS push loop + per-iteration inbound drain, reconnect backoff) | serde, serde_json, tungstenite, reqwest (blocking) |
| **apexos-gateway** (mesh_link) | `agentd/crates/gateway/src/mesh_link.rs` | ApexNET P5c: the radio lane's seam — `/mesh-bridge` socket state (the bridge dials in), inbound dedup + `BrainstemStatus` absorption, and `BleGossipTransport` (health = socket connected, MTU = one extended advertisement). | `mesh_link.rs` (`MeshLink`, `BrainstemView`, `BleGossipTransport`) | apexos-mesh-proto, apexos-core, async-trait |
| **apexos-core** (mesh_router) | `agentd/crates/core/src/mesh_router.rs` | ApexNET P5b: the policy router — `MeshTransport` trait, class→lane policy (Critical fans out, Bulk refuses narrow lanes, MTU excludes rather than truncates), the bounded seen-cache that makes fan-out safe, and the `PeerLost` ruling (= unreachable on every transport). Testable with mock transports, zero hardware. | `mesh_router.rs` (`Router`, `SeenCache`, `PeerReachability`) | apexos-mesh-proto, async-trait |
| **apexos-mesh-bridge** | `tools/crates/apexos-mesh-bridge` | ApexNET P3: the Pi↔brainstem UART bridge — link FSM (250 ms→5 s backoff, EOF = link-down), the frozen frame codec via `apexos-mesh-proto`, MUST-6 counters; **PSK-free by design** (ct is opaque here — crypto opens at the P5 router). Ships `apexos-brainstem-sim` (fake brainstem with fault injection: garbage/bitflip/truncate/flood/giant) for the hardware-free bench, and `apexos-brainstem-provision` — the P4b commissioning one-shot, which is the ONLY thing that reads the colony PSK (a separate binary precisely so the long-running daemon stays PSK-free). Not deployed by install.sh until P4c. | `src/lib.rs` (`run_link` — generic over AsyncRead+Write so the harness drives it, `LinkStats`, `backoff_ms`) · `src/main.rs` (reconnect FSM daemon) · `src/bin/brainstem_sim.rs` · `tests/harness.rs` (the §4.3 MUSTs incl. a real-PTY garbage-injection end-to-end) | apexos-mesh-proto, tokio, tokio-serial, serde_json, rand, postcard |
| **apex-tts** | `tools/crates/apex-tts` | **Workspace-EXCLUDED** Kokoro-82M TTS sidecar — its own workspace/Cargo.lock so the `ort =2.0.0-rc.11` pin can't fight cerebro's fastembed `ort`. `tiny_http` server on loopback `:8770` (`POST /synth` → WAV); gateway reaches it via `APEX_TTS_URL`. Built by install.sh when voice is on. | `src/main.rs` · own `Cargo.lock` | tts-rs (kokoro), ort =rc.11, tiny_http, hound |
| **apex-stt** | `tools/crates/apex-stt` | **Workspace-EXCLUDED** Whisper STT sidecar (build isolation — the whisper.cpp C++ build stays off workspace builds; no `ort`). Loads a ggml model once; `tiny_http` on loopback `:8771` (`POST /transcribe` ← 16 kHz mono WAV). Built by install.sh when voice is on. | `src/main.rs` · own `Cargo.lock` | whisper-rs, tiny_http, hound |
| **ui-slint** | `ui-slint` | Native Slint KMS/DRM (or winit) UI binary `apexos-rs-ui`. | `src/main.rs` (11333 lines — tokio bootstrap, WS connect+reconnect, HTTP polling, event→model mapping, window manager, persona/toast/inbox subsystems, geometry persistence (`geometry.json` :867) + the reflex engine (`REFLEX_TRIGGERS` :937, `reflexes.json` :967) + the Mandalas window rows (`build_mandala_rows` :1369)) · `src/face_gl.rs` (raymarched-SDF GL face overlay, 487 lines) · `src/ui/appwindow.slint` (root) · `src/ui/components/` (35 views: chat_view, tool_card, dashboard, sensor_view, council_view, terminal_view, explorer_view, mesh_view, settings_view, face_view, taskbar, work_board, mandala_view, …) · `src/ui/types.slint` (shared structs) · `build.rs` (`slint_build::compile`) | apexos-protocol, slint (backend-linuxkms-noseat + backend-winit), tokio, tokio-tungstenite, futures-util, serde, serde_json, reqwest, chrono, slint-build |

Not a crate but part of the shipped surface: **`web/`** — the browser + mobile-PWA frontend
(`index.html` · `app.js` · `style.css` · `sw.js` · `manifest.json` · `icon.svg`; vanilla JS, no build
step). install.sh copies it to `/var/lib/agentd/ui` on **every** node (headless included); the gateway
serves it via the filename-whitelisted `static_handler` (lib.rs:799) — a new asset filename must be
added there. See [web-ui.md](web-ui.md).

---

## How a message flows

### Chat request (UI → gateway → engine → plugins/cerebro → back)

The **core Bus** is the hub. Everything is fan-out via a `broadcast::Sender<Event>`
(capacity 1024) and point-to-point via mpsc command channels. A WS frame that fails
`Event` deserialization in the gateway is **silently dropped**.

```
  ui-slint (main.rs)                              agentd
  ┌──────────────┐   {type:user_prompt}   ┌──────────────────────────────┐
  │ WS send      │ ─────────────────────► │ gateway /ws handle_socket     │
  └──────────────┘                        │  (lib.rs:525; read task)       │
         ▲                                │  inject session, deser Event   │
         │                                │  bus.emit (lib.rs:718)         │
         │                                └───────────────┬────────────────┘
         │                                                ▼
         │                                  core Bus::run (bus.rs)
         │                                  apply SystemState → rebroadcast
         │                                                │
         │             ┌──────────────────────────────────┼───────────────────────────┐
         │             ▼                                   ▼                            ▼
         │   spawn_agent_router (main.rs:1588)     apexos-store              (other subscribers)
         │     match UserPrompt → root_turn        run_log_writer (JSONL)
         │             │
         │             ▼
         │   apexos-agent run_turn (turn.rs)
         │     stream from RoutingProvider (Anthropic/OAI)
         │     emit Event::AgentText deltas ───────────────┐
         └─────────────────────────────────────────────────┘  (broadcast → UI appends text)

  TOOL ROUND-TRIP
  run_turn emits Event::ToolRequested (turn.rs:332) ──► broadcast
        ├──► UI renders tool_card
        └──► apexos-plugins Supervisor::run (supervisor.rs:367 → dispatch_tool :558) consumes it
                 PolicyEngine.check(tool,path)  →  Allow | Ask
                   Ask → emit ApprovalPending{nonce} ──► UI buttons ──► {type:user_approval, action:<id>, nonce}
                   Allow → dispatch_tool:
                       virtual tool → channel (rollback_tx/schedule_tx/council_tx) or async work
                       real tool    → ToolProxy/McpClient.call_tool over stdio
                                        ├─ apexos-tools  (shell/file/http/…)
                                        └─ cerebro-mcp   (memory)  → cerebro::CerebroCortex
                                                                       → SQLite/vector/graph + fastembed
                 Supervisor emits Event::ToolResult ──► broadcast
        run_turn awaits matching ToolResult(s) (collect_tool_results, turn.rs:396 — approval-phase-aware:
          a missing result names the true blocker, awaiting-approval / declined / stalled), loops … → Event::TurnComplete
```

Note the **id shapes** on the wire: tool fields nest under `call` (a `ToolCall`); `ActionId`/`SessionId`
serialize as **bare numbers**. Approval frames use `action: <numeric ToolCall.id>` and `granted` — not `call_id`/`approved`.
Both ends share these types via the **`apexos-protocol`** crate — ui-slint deserializes outbound frames
into the typed `Event` (and logs any undecodable frame) instead of string-matching fields.

### Sensor / event-bus path

```
  apex-sensor-bridge (separate process)
    read CPU temp (sysfs) / SensorHead HTTP (BME688, MLX90640)
        │  WS push, SENSOR_BRIDGE_TOKEN auth
        ▼
  gateway /sensor-bridge handle_sensor_bridge (own token + SensorIngress only)
        │  emit Event::SensorReading
        ▼
  core Bus  ──broadcast──┬──► UI /ws subscribers (sensor_view, dashboard)
                         └──► apexos-store run_log_writer (persist)
```

**Event-bus backbone:** mpsc inbox (`BusHandle::emit`) feeds `Bus::run`, which calls
`SystemState::apply` then fans out via `broadcast::Sender`. Subscribers: gateway WS write
tasks (all UIs/browsers), Supervisor (tools), agent-router (turns), goal driver, worker/mandala
driver (`spawn_worker_driver`, worker.rs:1044 — watches worker-session TurnComplete /
ApprovalPending / UserPrompt / Error edges off the broadcast; its tool *commands* arrive on the
supervisor's worker channel), evolution-applier, store writer, scheduler/council handlers.

---

## Where do I change X?

| I want to… | Go to |
|------------|-------|
| Add / edit a **system tool** (shell, file, http, GPIO, audio) | `tools/crates/apexos-tools/src/tools.rs` (`list()` schema + `call()` dispatch); shell guards in `denylist_check` |
| Add / edit a **memory tool** (Cerebro) | `cerebro/crates/cerebro-mcp/src/dispatch.rs` (route) + `cerebro/crates/cerebro-mcp/src/tools.rs` (schema); engine logic in `cerebro/crates/cerebro/src/` |
| Add a **virtual tool** (propose_evolution, schedule_*, convene_council, agent_spawn, soul_rehearse, vast_*, task_fanout/worker_*/mandala_*) | declare ToolSpec in `agentd/crates/agentd/src/main.rs` `gather_tools`; intercept in `agentd/crates/plugins/src/supervisor.rs` `dispatch_tool` |
| Change **approval policy** behaviour | rules in `config/policy.toml`; engine logic in `agentd/crates/plugins/src/policy.rs` (`PolicyEngine::check`, workspace_decision) |
| Change which **plugins** agentd spawns | `config/plugins.toml`; loader in `agentd/crates/plugins/src/config.rs` |
| Add / change an **HTTP/WS route** or `/api/*` endpoint | `agentd/crates/gateway/src/lib.rs` (`router()` :318, handlers below it) |
| Change **auth / bind policy** (token, loopback) | bind/auth in `agentd/crates/agentd/src/main.rs:467-471`; `require_token` middleware in `gateway/src/lib.rs:263`; human-login session tokens in `gateway/src/session_auth.rs` |
| Change the **wire protocol** (Event/ToolCall shape) | `apexos-protocol/src/lib.rs` (re-exported as `apexos_core::Event` / `apexos_core::types::*`; ui-slint consumes the crate directly) — then `SystemState::apply` in `core/src/state.rs` + every consumer |
| Change **FS confinement** | new confinement *logic* → `apexos-confine/src/lib.rs` (with a test); policy *values* (workspace root, read allowlist, git roots, secret denylist) → `tools/crates/apexos-tools/src/tools.rs::confine` |
| Change **LLM provider / turn loop** logic | `agentd/crates/agent/src/turn.rs`; provider impls in `anthropic.rs` / `oai.rs`; dispatch in `routing.rs` |
| Give a **tool image output** (let APEX see a PNG) | tool returns the **vision sentinel** `{"vision":{"path"\|"b64","media_type"?},"text"?}`; `turn.rs::vision_rewrite` runs it through `apexos_core::vision` (the downscale shim, `VISION_MAX_EDGE`) → multimodal content block. Providers gate on `vision::contains_image_block`. Reference impl: `apexos-tools` `sketch_snapshot` |
| Add / change **multi-agent council** | `agentd/crates/agent/src/council.rs` + `agentd/crates/agentd/src/council_handler.rs` |
| Change **scheduled / cron tasks** | `agentd/crates/agentd/src/scheduler.rs` |
| Change **goal driver** behaviour (autonomous goals, yolo scope) | `agentd/crates/agentd/src/goal.rs` |
| Change **worker / mandala** behaviour (task_fanout, admission, review, rings) | driver + tool specs in `agentd/crates/agentd/src/worker.rs`; pure mandala core (forms, addresses, budgets, the on-disk tree) in `agentd/crates/agentd/src/mandala.rs`; W2 mesh seam in `remote.rs`; review procedure in `review.rs` — charter in `docs/fabrica.md` |
| Change **evolution apply/undo** logic | pure state machine in `agentd/crates/agentd/src/evolution.rs` (with a test); the applier loop in `main.rs` is IO-thin glue |
| Change **self-update / health** | `agentd/crates/agentd/src/self_update.rs` + `health.rs`; watchdog/rollback scripts + units in `deploy/apexos-self-update.*`, `deploy/apexos-rollback.*` |
| Change **mesh discovery / pairing / beacon / federation** | `agentd/crates/gateway/src/mesh.rs` (PeerRegistry, avahi, pairing) · `gateway/src/beacon.rs` (liveness) · federation import/recall handlers in `gateway/src/lib.rs` · dream-digest push in `agentd/src/dream_digest.rs` |
| Change **voice / TTS / STT** | backend plans + handlers in `agentd/crates/gateway/src/lib.rs` (`stt_plan` :2696, `tts_plan` :3124, `speak_handler`, `transcribe_wav`); local engines in the excluded sidecars `tools/crates/apex-tts` / `apex-stt` |
| Edit the **browser / PWA UI** | `web/` (app.js, index.html, style.css, sw.js) — a NEW asset filename must also be added to the gateway `static_handler` whitelist (`gateway/src/lib.rs:799`) |
| Change **session history persistence** | `agentd/crates/agentd/src/session_store.rs` (root sessions) |
| Change **history trimming / the window budget** | trim algorithm in `agentd/crates/core/src/history.rs` (`trim_history` :100); the persisted budget knob (`/api/history`, Settings) in `agentd/crates/gateway/src/history_config.rs` |
| Change **session transcript search** (`session_search` / `session_list`) | pure matcher + identity gate in `agentd/crates/core/src/transcript.rs`; specs in `agentd/src/main.rs`; intercept in `supervisor.rs`; charter `docs/session-rag.md` |
| Change the **event log** format | `agentd/crates/store/src/lib.rs` (`run_log_writer`) |
| Edit the **chat view** | `ui-slint/src/ui/components/chat_view.slint` |
| Edit a **tool card / approval UI** | `ui-slint/src/ui/components/tool_card.slint` |
| Edit the **dashboard / sensor / council / terminal** view | `ui-slint/src/ui/components/{dashboard,sensor_view,council_view,terminal_view}.slint` |
| Edit the **root window / shell modes / dock** | `ui-slint/src/ui/appwindow.slint` |
| Add a **shared Slint struct** | `ui-slint/src/ui/types.slint` (must mirror Rust models in `main.rs`) |
| Change **UI ↔ agentd wiring** (WS, event→model, window manager, personas) | `ui-slint/src/main.rs` |
| Change **adaptive-UI verbs / reflexes** (`ui_open`/`ui_close`/`ui_focus`/`ui_query`/`ui_arrange`/`ui_theme`/`ui_reflex`) | `tools/crates/apexos-tools/src/tools.rs` (`UI_APPS` :2995 / `UI_REFLEX_TRIGGERS` :3150) — must literal-mirror `ui-slint/src/main.rs` (`APP_TABLE` :593 / `REFLEX_TRIGGERS` :937); contract in `docs/adaptive-ui.md` |
| Change a **theme / persona** | Slint globals `Palette` / `Personas` in `ui-slint/src/ui/` |
| Change **sensor polling** (CPU temp, SensorHead) | `tools/crates/apex-sensor-bridge/src/main.rs` |
| Change **install / hardware detection / systemd** | `install.sh`; units + helpers in `deploy/` (agentd.service, apexos-rs-ui.service, apex-tts/apex-stt.service, avahi/, udev/, usb/, systemd/, fonts/) |
| Change the **default soul / persona prompt** | `config/soul.md` (agent-mutable at runtime via Settings / propose_evolution) |

---

## Build / run / deploy entry points

**Build everything** (workspace root `Cargo.toml`):
```bash
cargo build --release --workspace
```
On a headless dev machine, exclude the UI (needs fontconfig): `--exclude ui-slint`.

**Binaries produced** (7 `[[bin]]` targets → `target/release/`, plus 2 workspace-excluded sidecars built separately):

| Binary | Source | Run as |
|--------|--------|--------|
| `agentd` | `agentd/crates/agentd/src/main.rs` | daemon (systemd); loads soul + keys, binds gateway (default `127.0.0.1:8787`) |
| `ui-slint` (`apexos-rs-ui`) | `ui-slint/src/main.rs` | kiosk UI (systemd, `User=apexos-ui`, token-only `ui.env`) or `cargo run` on desktop |
| `cerebro-mcp` | `cerebro/crates/cerebro-mcp/src/main.rs` | spawned by agentd as stdio MCP plugin (not run directly) |
| `apexos-tools` | `tools/crates/apexos-tools/src/main.rs` | spawned by agentd as stdio MCP plugin (not run directly) |
| `cerebro-api` | `cerebro/crates/cerebro-api/src/main.rs` | optional REST/dashboard service |
| `cerebro` | `cerebro/crates/cerebro-cli/src/main.rs` | CLI for humans/scripts |
| `apex-sensor-bridge` | `tools/crates/apex-sensor-bridge/src/main.rs` | own process, pushes to `/sensor-bridge` |
| `apex-tts` (excluded workspace) | `tools/crates/apex-tts/src/main.rs` | voice sidecar (systemd `apex-tts.service`, loopback `:8770`) — separate `--manifest-path` build by install.sh when voice is on |
| `apex-stt` (excluded workspace) | `tools/crates/apex-stt/src/main.rs` | voice sidecar (systemd `apex-stt.service`, loopback `:8771`) — separate `--manifest-path` build by install.sh when voice is on |

**Run agentd (dev):** `agentd` builds a manual multi-thread tokio runtime, reads soul
(`AGENTD_SOUL` or `config/soul.md`) and API keys (`ANTHROPIC_API_KEY` / `OAI_API_KEY` or key files),
binds the gateway (`AGENTD_BIND`, default loopback — non-loopback bind requires `AGENTD_TOKEN`),
and serves `ws://localhost:8787/ws`.

**Run the UI (dev):**
```bash
AGENTD_WS=ws://localhost:8787/ws SLINT_BACKEND=linuxkms ./target/release/ui-slint
```
`SLINT_BACKEND` auto-detects `winit` on desktop (when `DISPLAY`/`WAYLAND_DISPLAY` is set).
`build.rs` compiles `ui-slint/src/ui/appwindow.slint` at build time — if a `.slint` change
doesn't recompile, `touch ui-slint/build.rs`.

**Deploy:** `install.sh` — apt deps → rustup → `cargo build --release --workspace`
(+ separate `--manifest-path` builds of the voice sidecars and the sibling Occipital-RS when enabled) →
`install -m 755` of release binaries into `/usr/local/bin` → **seed-if-absent** `/etc/agentd/{plugins,policy,peers}.toml` + `soul.md`
+ **additive policy-rule sync** (`sync_policy_rules` — new default rules reach already-deployed nodes without clobbering
self-evolved ones) + a `600 root:root` env file with a generated `AGENTD_TOKEN` → copy `web/` → `/var/lib/agentd/ui`
(every node) → install + enable systemd units (`deploy/`) → fastembed prewarm → health-check.
agentd runs as a jailed `agentd` system user; `apexos-rs-ui` runs as `apexos-ui` (token-only env; DRM-master root only via `APEXOS_UI_AS_ROOT`).
