# world-app — the apexos-world renderer (`apexos-world` binary)

The 3D world interface for ApexOS-RS: a navigable **Atrium** that is *another agentd
client*, peer to `ui-slint` and the browser PWA. Slint hosts the window + winit event
loop and draws all 2D chrome; the 3D scene (Bevy, deferred) renders to an offscreen
wgpu texture Slint draws into a full-window `Image` (**Pattern A** — DESIGN.md §5,
doc 03 §1). Standard/Pro tier only (real GPU).

Speaks agentd's real `Event`/Intent JSON via the `world-protocol` crate. New agent
powers (vision, world-state) come from MCP plugins (`world-vision`), never a core fork.

## Run (dev)

```bash
# default features (NO Bevy) — the buildable skeleton
cargo run -p world-app
# point at a live agentd (Pi over LAN needs a token — see root CLAUDE.md):
AGENTD_WS=ws://192.168.0.158:8787/ws cargo run -p world-app
```

Build deps (root CLAUDE.md): `libfontconfig1-dev libxkbcommon-dev` (+ the linuxkms
link deps the workspace pulls in). This crate is desktop/VR only — it does **not** use
`backend-linuxkms-femtovg` (software) and refuses Nano/Micro tiers (gate is stubbed).

## ⚠️ HONEST BUILD STATUS

`cargo check -p world-app` and `cargo test -p world-app` **pass** on this dev box
(default features, no Bevy). What that does and does NOT mean:

| Area | Status |
|------|--------|
| Crate compiles (default features) | ✅ **Verified** — `cargo check` clean, no warnings |
| Unit tests (station registry) | ✅ **Verified** — 4 tests pass |
| Slint `hud.slint` compiles + `WorldWindow`/`ChatLine` generated | ✅ **Verified** |
| Thread model wired (Slint main thread, tokio bg, channel bridge, frame timer) | ✅ **Wired** (structure real; WS body stubbed) |
| Station registry + catalog + lookup | ✅ **Real** (data + tests); binding machinery stubbed |
| HUD overlay + Mode-II activated-station panel placeholder | ✅ **Real UI** (placeholder surface body) |
| Picking → activate event seam | 🟡 **Stubbed** (`world.rs` `pick_system` wired but inert) |
| Live agentd WS connect (`world-protocol::WorldClient`) | 🟡 **Stubbed** — `ws_task_stub` logs "offline" and idles |
| Bevy 3D scene (`src/world.rs`, feature `viz`) | 🔴 **UNCOMPILED + UNVALIDATED** — see below |
| Slint↔wgpu shared-device handoff | 🔴 **Not wired** — Spike 1 |
| Tier gate (GPU probe / refuse) | 🔴 **Stubbed** — assumes Standard/Pro |
| Agent-vision snapshot, VR | 🔴 **Seams only** (M2 / M3) |

### The two load-bearing risks this scaffold does NOT resolve

1. **wgpu version + Bevy clash (DESIGN.md D1/D2, doc 03 §10, Spike 0).**
   Verified against the installed toolchain (`cargo` feature list on slint 1.16.1):
   the available features are `unstable-wgpu-27` / **`unstable-wgpu-28`** — there is
   **no `unstable-wgpu-29`** and **no `renderer-wgpu`** feature on this build. So this
   crate uses **`unstable-wgpu-28`** + `backend-winit`. This **confirms DESIGN.md D1
   ("wgpu is 28")** and **contradicts the brief + seed-skill template + doc 03**, which
   all say "wgpu-29". The matching API names are `GraphicsAPI::WGPU28` /
   `BackendSelector::require_wgpu_28` (commented in `main.rs` until Spike 1).
   No released Bevy shares Slint 1.16's wgpu-28, so the shared-device handoff cannot
   typecheck/link today. Therefore **Bevy is behind the `viz` feature, OFF by default**;
   `src/world.rs` is `#[cfg(feature = "viz")]` and is **not compiled** in the default
   build and **not validated** against any Bevy version. The `bevy = "0.16"` pin is a
   PLACEHOLDER — the real pin is Spike 0's output. **Do not assume `--features viz`
   builds.** // TODO(Mn): Spike 0 → pin the Bevy release whose wgpu == Slint's.

2. **Live `world-protocol` WS connect.** `world-protocol` is a path dep and its public
   API (`WorldClient::connect`, `Event`, `intents::*`, `ids::SessionId`) is real and
   referenced. But `main.rs`'s `ws_task_stub` does **not** open a socket yet — it logs
   "offline" and drains outbound commands so the UI never blocks. M0 replaces it with a
   real `WorldClient` (capture `session_init`, defensive `session` filter, reconnect).

These two are the make-or-break integrations; they are deliberately stubbed, not
hidden. Everything above them (host, thread model, registry, HUD, Mode-II takeover,
the bridge channels) is real and runs.

## Files

| File | What it is |
|------|------------|
| `Cargo.toml` | slint 1.16 (`backend-winit`, `unstable-wgpu-28`) + optional Bevy (`viz`, off) + `world-protocol` path dep |
| `build.rs` | `slint_build::compile("ui/hud.slint")` |
| `src/main.rs` | runtime bootstrap — Slint main thread, tokio bg runtime, WS-task→channel→frame-timer→scene/models, UI callbacks → intents (stubbed WS) |
| `src/world.rs` | Bevy hub scene scaffolding (`#[cfg(feature = "viz")]`): ground/light/camera, placeholder STATION + AVATAR entities, picking/activate stub. **Uncompiled by default.** |
| `src/stations.rs` | `StationKind` enum + static `StationDesc` catalog + `StationRegistry` (real, unit-tested) |
| `ui/hud.slint` | `WorldWindow`: full-window viewport `Image`, persistent HUD chrome, Mode-II activated-station panel placeholder |

`// TODO(Mn): …` markers throughout flag the milestone (M0–M3 / Spike) that fills each
seam. Build order and acceptance gates: `world/docs/design/06-roadmap-and-scaffold-plan.md`.
