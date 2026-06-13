# Repo Map — ApexOS-RS

> Developer navigation guide for the ApexOS-RS monorepo. Find the crate, file, or
> change-point you need without grepping the whole tree.
>
> See also: [architecture.md](architecture.md) · [build-roadmap.md](build-roadmap.md) · [slint-notes.md](slint-notes.md)

ApexOS-RS is one Cargo workspace (`resolver = 2`, release `lto=thin strip`) producing
**7 binaries**: the agent daemon plus the Cerebro memory stack, the system tool plugins,
and the native Slint UI. One `cargo build --release --workspace`. One `install.sh`.

---

## At a glance

```
ApexOS-RS/
├── agentd/crates/                  # the agent daemon — event bus, gateway, plugins, turn engine
│   ├── core         apexos-core      shared Event/Intent types, ID newtypes, SystemState, the Bus
│   ├── gateway      apexos-gateway   axum HTTP+WS server: /ws, /sensor-bridge, /terminal-ws, /api/*
│   ├── plugins      apexos-plugins   MCP plugin host + PolicyEngine (approval) + vast.ai recipes
│   ├── agent        apexos-agent     turn engine: LLM stream → tool round-trips → council
│   ├── store        apexos-store     append-only JSONL event-log writer
│   └── agentd       agentd  (bin)    main daemon: wires bus+gateway+supervisor+turn+scheduler
│
├── cerebro/crates/                 # cognitive-memory stack (agent FORGE's brain)
│   ├── cerebro      cerebro          engine lib: SQLite+vec, petgraph, ACT-R/FSRS, brain engines
│   ├── cerebro-mcp  cerebro-mcp (bin) MCP-over-stdio server, ~63 memory tools (agentd spawns this)
│   ├── cerebro-api  cerebro-api (bin) axum REST API + dashboard over the engine
│   └── cerebro-cli  cerebro     (bin) clap CLI over the engine
│
├── tools/crates/                   # system tool plugins
│   ├── apexos-tools      apexos-tools      (bin) MCP stdio: shell/file/http/sysinfo/audio/GPIO
│   └── apex-sensor-bridge apex-sensor-bridge (bin) WS client: CPU temp / SensorHead → /sensor-bridge
│
├── ui-slint/                       # the unique contribution — native Slint KMS/DRM UI
│   └── ui-slint  (bin → apexos-rs-ui)  chat/tools/dashboard/sensor/council/terminal
│
├── config/                         # default plugins.toml, policy.toml, soul.md
├── deploy/                         # systemd units: agentd.service, apexos-rs-ui.service
└── install.sh                      # one-shot installer (deps → build → install → systemd)
```

---

## Per-crate reference

| Crate | Path | Role | Key files | Depends on |
|-------|------|------|-----------|------------|
| **apexos-core** | `agentd/crates/core` | Shared types + the in-process event Bus (mpsc inbox → broadcast out). The wire protocol lives here. | `src/types.rs` (Event/Intent/ContentBlock/ToolCall/PolicyMode/EvolutionProposal) · `src/bus.rs` (`Bus::new` → BusHandle emit + broadcast subscribe; `Bus::run` applies state then rebroadcasts) · `src/state.rs` (`SystemState::apply`) · `src/lib.rs` | serde, serde_json, tokio |
| **apexos-gateway** | `agentd/crates/gateway` | axum HTTP+WS server — the entire external surface of agentd. | `src/lib.rs` (`router()` :116, `serve()` :2097, `ws_handler`/`handle_socket` :173-297, `GatewayState` :42, `handle_sensor_bridge`, `handle_terminal_ws`) · `src/mesh.rs` (PeerRegistry, avahi discovery) | apexos-core, apexos-plugins, axum, tokio, futures-util, reqwest, libc, toml, chrono |
| **apexos-plugins** | `agentd/crates/plugins` | MCP plugin host: spawn/supervise stdio plugins, route tool calls, enforce approval policy, vast.ai recipes. | `src/supervisor.rs` (`Supervisor::run`, `ToolProxy::call`, `SupervisorCmd`, `dispatch_tool` virtual-tool chain — 1558 lines) · `src/mcp.rs` (`McpClient` over child stdio, no request timeout) · `src/policy.rs` (`PolicyEngine`, `Rule`, `Decision`) · `src/config.rs` (plugins.toml loader, RestartPolicy) · `src/vast.rs` | apexos-core, tokio, serde, serde_json, anyhow, toml, chrono, reqwest |
| **apexos-agent** | `agentd/crates/agent` | Agent turn engine: stream from LLM providers, drive tool round-trips over the Bus, run councils. | `src/turn.rs` (`run_turn`: stream → AgentText, emit ToolRequested, await ToolResult, loop) · `src/provider.rs` (`Provider` trait + Chunk stream) · `src/routing.rs` (`RoutingProvider`, live backend swap) · `src/anthropic.rs` / `src/oai.rs` · `src/council.rs` (`run_council`) | apexos-core, tokio, reqwest, async-trait, async-stream, futures-util, bytes, serde, serde_json |
| **apexos-store** | `agentd/crates/store` | Append-only event-log writer; subscribes the broadcast bus, persists JSONL (date-rolling). | `src/lib.rs` (`run_log_writer` — single pub async fn) | apexos-core, serde_json, tokio, anyhow, chrono |
| **agentd** | `agentd/crates/agentd` | Main daemon binary: wires everything, owns the agent-router and evolution-applier loops. | `src/main.rs` (1877 lines — Bus wiring, `spawn_agent_router` :930 routing UserPrompt→root_turn, `serve()` :247, `spawn_evolution_applier`, `gather_tools`) · `src/scheduler.rs` (cron) · `src/council_handler.rs` · `src/session_store.rs` | apexos-core, apexos-gateway, apexos-plugins, apexos-agent, apexos-store, tokio, toml_edit, cron, chrono, anyhow |
| **cerebro** | `cerebro/crates/cerebro` | Cognitive-memory engine lib: SQLite+vec storage, petgraph graph, ACT-R/FSRS activation, fastembed, brain-region engines. | `src/cortex.rs` (`CerebroCortex` facade) · `src/engines/` (hippocampus/neocortex/amygdala/prefrontal/dream/…) · `src/storage/` (sqlite.rs, vector.rs, graph.rs) · `src/activation/` (actr.rs, fsrs.rs, spreading.rs) · `src/config.rs` (`Config::from_env`) | rusqlite, sqlite-vec, petgraph, fastembed, tokio, reqwest, uuid, chrono, notify, dirs-next, serde, tracing |
| **cerebro-mcp** | `cerebro/crates/cerebro-mcp` | MCP-over-stdio server exposing ~63 Cerebro tools; the plugin agentd spawns for agent memory. | `src/main.rs` (initialize handshake + read/dispatch/write loop) · `src/dispatch.rs` (`route(name,args,brain)` over ~61 tools — 1108 lines) · `src/tools.rs` (schema registry, 66 names) · `src/transport.rs` (`StdioTransport`) | cerebro, tokio, serde, serde_json, anyhow, tracing, uuid |
| **cerebro-api** | `cerebro/crates/cerebro-api` | axum REST API + dashboard over the engine (~40 routes); AGENTD_TOKEN bearer middleware. | `src/main.rs` (950 lines — all handlers + router) | cerebro, axum, tower, tokio, serde, serde_json, chrono, uuid, tracing |
| **cerebro-cli** | `cerebro/crates/cerebro-cli` | clap CLI over the engine (binary named `cerebro`). | `src/main.rs` (Cli/Command/Subcommand tree, 778 lines) | cerebro, clap, tokio, serde, serde_json, chrono, uuid, tracing |
| **apexos-tools** | `tools/crates/apexos-tools` | MCP-over-stdio system tool plugin: shell/file/http/sysinfo/audio/GPIO/display, with a command denylist. | `src/main.rs` (stdio JSON-RPC loop) · `src/tools.rs` (`list()`/`call()` + ~28 tool impls + `denylist_check` — 1684 lines) | serde, serde_json, reqwest (blocking) |
| **apex-sensor-bridge** | `tools/crates/apex-sensor-bridge` | Standalone WS client: polls CPU temp / SensorHead (BME688, MLX90640), pushes SensorReading to `/sensor-bridge`. | `src/main.rs` (257 lines — `read_cpu_temp`, SensorHead HTTP poll, tungstenite WS push loop, reconnect backoff) | serde, serde_json, tungstenite, reqwest (blocking) |
| **ui-slint** | `ui-slint` | Native Slint KMS/DRM (or winit) UI binary `apexos-rs-ui`. | `src/main.rs` (2135 lines — tokio bootstrap, WS connect+reconnect, HTTP polling, event→model mapping, window manager, persona/toast subsystems) · `src/ui/appwindow.slint` (root) · `src/ui/components/` (chat_view, tool_card, dashboard, sensor_view, council_view, terminal_view, taskbar, …) · `src/ui/types.slint` (shared structs) · `build.rs` (`slint_build::compile`) | slint (backend-linuxkms-noseat + backend-winit), tokio, tokio-tungstenite, futures-util, serde, serde_json, reqwest, chrono, slint-build |

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
  └──────────────┘                        │  (lib.rs:218 read task)        │
         ▲                                │  inject session, deser Event   │
         │                                │  bus.emit (lib.rs:288)         │
         │                                └───────────────┬────────────────┘
         │                                                ▼
         │                                  core Bus::run (bus.rs)
         │                                  apply SystemState → rebroadcast
         │                                                │
         │             ┌──────────────────────────────────┼───────────────────────────┐
         │             ▼                                   ▼                            ▼
         │   spawn_agent_router (main.rs:930)      apexos-store              (other subscribers)
         │     match UserPrompt → root_turn        run_log_writer (JSONL)
         │             │
         │             ▼
         │   apexos-agent run_turn (turn.rs)
         │     stream from RoutingProvider (Anthropic/OAI)
         │     emit Event::AgentText deltas ───────────────┐
         └─────────────────────────────────────────────────┘  (broadcast → UI appends text)

  TOOL ROUND-TRIP
  run_turn emits Event::ToolRequested (turn.rs:138) ──► broadcast
        ├──► UI renders tool_card
        └──► apexos-plugins Supervisor::run (supervisor.rs:270/1337) consumes it
                 PolicyEngine.check(tool,path)  →  Allow | Ask
                   Ask → emit ApprovalPending ──► UI buttons ──► {type:user_approval, action:<id>}
                   Allow → dispatch_tool:
                       virtual tool → channel (rollback_tx/schedule_tx/council_tx) or async work
                       real tool    → ToolProxy/McpClient.call_tool over stdio
                                        ├─ apexos-tools  (shell/file/http/…)
                                        └─ cerebro-mcp   (memory)  → cerebro::CerebroCortex
                                                                       → SQLite/vector/graph + fastembed
                 Supervisor emits Event::ToolResult ──► broadcast
        run_turn awaits matching ToolResult (turn.rs:164), loops … → Event::TurnComplete
```

Note the **id shapes** on the wire: tool fields nest under `call` (a `ToolCall`); `ActionId`/`SessionId`
serialize as **bare numbers**. Approval frames use `action: <numeric ToolCall.id>` and `granted` — not `call_id`/`approved`.

### Sensor / event-bus path

```
  apex-sensor-bridge (separate process)
    read CPU temp (sysfs) / SensorHead HTTP (BME688, MLX90640)
        │  WS push, SENSOR_BRIDGE_TOKEN auth
        ▼
  gateway /sensor-bridge handle_sensor_bridge (lib.rs:278, ungated route + own token check)
        │  emit Event::SensorReading
        ▼
  core Bus  ──broadcast──┬──► UI /ws subscribers (sensor_view, dashboard)
                         └──► apexos-store run_log_writer (persist)
```

**Event-bus backbone:** mpsc inbox (`BusHandle::emit`) feeds `Bus::run`, which calls
`SystemState::apply` then fans out via `broadcast::Sender`. Subscribers: gateway WS write
tasks (all UIs/browsers), Supervisor (tools), agent-router (turns), evolution-applier,
store writer, scheduler/council handlers.

---

## Where do I change X?

| I want to… | Go to |
|------------|-------|
| Add / edit a **system tool** (shell, file, http, GPIO, audio) | `tools/crates/apexos-tools/src/tools.rs` (`list()` schema + `call()` dispatch); shell guards in `denylist_check` |
| Add / edit a **memory tool** (Cerebro) | `cerebro/crates/cerebro-mcp/src/dispatch.rs` (route) + `cerebro/crates/cerebro-mcp/src/tools.rs` (schema); engine logic in `cerebro/crates/cerebro/src/` |
| Add a **virtual tool** (propose_evolution, schedule_*, convene_council, agent_spawn, vast_*) | declare ToolSpec in `agentd/crates/agentd/src/main.rs` `gather_tools`; intercept in `agentd/crates/plugins/src/supervisor.rs` `dispatch_tool` |
| Change **approval policy** behaviour | rules in `config/policy.toml`; engine logic in `agentd/crates/plugins/src/policy.rs` (`PolicyEngine::check`, workspace_decision) |
| Change which **plugins** agentd spawns | `config/plugins.toml`; loader in `agentd/crates/plugins/src/config.rs` |
| Add / change an **HTTP/WS route** or `/api/*` endpoint | `agentd/crates/gateway/src/lib.rs` (`router()` :116, handlers below it) |
| Change **auth / bind policy** (token, loopback) | bind/auth in `agentd/crates/agentd/src/main.rs:240-251`; `require_token` middleware in `gateway/src/lib.rs:89-114` |
| Change the **wire protocol** (Event/Intent/ToolCall shape) | `agentd/crates/core/src/types.rs` (then `SystemState::apply` in `state.rs` + every consumer) |
| Change **LLM provider / turn loop** logic | `agentd/crates/agent/src/turn.rs`; provider impls in `anthropic.rs` / `oai.rs`; dispatch in `routing.rs` |
| Add / change **multi-agent council** | `agentd/crates/agent/src/council.rs` + `agentd/crates/agentd/src/council_handler.rs` |
| Change **scheduled / cron tasks** | `agentd/crates/agentd/src/scheduler.rs` |
| Change **session history persistence** | `agentd/crates/agentd/src/session_store.rs` (root sessions) |
| Change the **event log** format | `agentd/crates/store/src/lib.rs` (`run_log_writer`) |
| Edit the **chat view** | `ui-slint/src/ui/components/chat_view.slint` |
| Edit a **tool card / approval UI** | `ui-slint/src/ui/components/tool_card.slint` |
| Edit the **dashboard / sensor / council / terminal** view | `ui-slint/src/ui/components/{dashboard,sensor_view,council_view,terminal_view}.slint` |
| Edit the **root window / shell modes / dock** | `ui-slint/src/ui/appwindow.slint` |
| Add a **shared Slint struct** | `ui-slint/src/ui/types.slint` (must mirror Rust models in `main.rs`) |
| Change **UI ↔ agentd wiring** (WS, event→model, window manager, personas) | `ui-slint/src/main.rs` |
| Change a **theme / persona** | Slint globals `Palette` / `Personas` in `ui-slint/src/ui/` |
| Change **sensor polling** (CPU temp, SensorHead) | `tools/crates/apex-sensor-bridge/src/main.rs` |
| Change **install / hardware detection / systemd** | `install.sh`; units in `deploy/agentd.service`, `deploy/apexos-rs-ui.service` |
| Change the **default soul / persona prompt** | `config/soul.md` (agent-mutable at runtime via Settings / propose_evolution) |

---

## Build / run / deploy entry points

**Build everything** (workspace root `Cargo.toml`):
```bash
cargo build --release --workspace
```
On a headless dev machine, exclude the UI (needs fontconfig): `--exclude ui-slint`.

**Binaries produced** (7 `[[bin]]` targets → `target/release/`):

| Binary | Source | Run as |
|--------|--------|--------|
| `agentd` | `agentd/crates/agentd/src/main.rs` | daemon (systemd); loads soul + keys, binds gateway (default `127.0.0.1:8787`) |
| `ui-slint` (`apexos-rs-ui`) | `ui-slint/src/main.rs` | kiosk UI (systemd, root for DRM master) or `cargo run` on desktop |
| `cerebro-mcp` | `cerebro/crates/cerebro-mcp/src/main.rs` | spawned by agentd as stdio MCP plugin (not run directly) |
| `apexos-tools` | `tools/crates/apexos-tools/src/main.rs` | spawned by agentd as stdio MCP plugin (not run directly) |
| `cerebro-api` | `cerebro/crates/cerebro-api/src/main.rs` | optional REST/dashboard service |
| `cerebro` | `cerebro/crates/cerebro-cli/src/main.rs` | CLI for humans/scripts |
| `apex-sensor-bridge` | `tools/crates/apex-sensor-bridge/src/main.rs` | own process, pushes to `/sensor-bridge` |

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

**Deploy:** `install.sh` — apt deps → rustup → `cargo build --release --workspace` →
`install -m 755` of release binaries into `/usr/local/bin` → write `/etc/agentd/{plugins,policy,soul,peers}.toml`
+ a `600 root:root` env file with a generated `AGENTD_TOKEN` → install + enable systemd units
(`deploy/agentd.service`, `deploy/apexos-rs-ui.service`) → fastembed prewarm → health-check.
agentd runs as a jailed `agentd` system user; `apexos-rs-ui` runs as root (DRM master on a seatless Pi).
