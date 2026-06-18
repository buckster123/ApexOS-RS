# apexos-world

> **An AI-native 3D world interface for ApexOS-RS — just another agentd client.**

apexos-world turns an ApexOS-RS session from a *list* into a *place*. It is an
**interface first, a world second** (not a game): a navigable 3D **Atrium** where agents
stand as figures on the floor and functions live as stations around the perimeter. You
walk up to a thing, activate it, and its real UI fills the view; you step back and you
are in the space again.

Crucially, it is **just another agentd client** — peer to `ui-slint` and the browser/PWA.
It speaks agentd's real `Event`/Intent wire protocol on `ws://HOST:8787/ws`, reusing the
existing Slint function-views (chat, sensors, council, terminal, memory) as the surfaces
that fill the screen on activation. New powers (agent-vision, world-state, generative UI)
arrive only through agentd's documented **MCP-plugin** extension surface — **never** by
forking the daemon.

The defining capability: agents are **embodied** and can **see**. An agent's avatar
carries a camera, so "show me what you're looking at" becomes a literal rendered
snapshot fed back into its turn. Concurrent agents are concurrent figures; a council is a
ring whose tightening *is* its convergence.

## Status

**PROTOTYPE — design + scaffold.** The authoritative design is complete and
critique-reviewed; the crate scaffold is being built. Nothing is feature-complete yet.

- Target tier: **Standard/Pro desktop only** (real GPU). On Nano/Micro (Pi Zero/Pi 4)
  run `apexos-rs-ui` or the browser PWA instead — apexos-world refuses cleanly there.
- Composition: **Slint owns the window; the 3D scene renders to a full-window texture**
  (Pattern A). At prototype stage the 3D side uses **Slint's own wgpu (`wgpu_28`)** —
  Bevy is a deferred migration (no released Bevy currently shares Slint's wgpu version;
  see `docs/DESIGN.md` §5).
- Feasibility verdict: the entire interaction surface plugs into **agentd as-is, zero
  core changes**. The one carve-out is making the model *see* a vision snapshot, which
  needs a small core provider change (see `docs/DESIGN.md` §4/R1).

## Quickstart (intent)

Once scaffolded, the prototype runs against a live agentd, mirroring `ui-slint`:

```bash
# point at a local or LAN agentd (same env convention as ui-slint)
AGENTD_WS=ws://localhost:8787/ws cargo run -p world-app

# against the LAN Pi (token required for non-loopback binds)
AGENTD_TOKEN=<token> AGENTD_WS=ws://192.168.0.158:8787/ws cargo run -p world-app
```

`world-protocol` and `world-vision` build and test on a **headless** box with no GPU;
`world-app` needs the GPU/UI toolchain (`libfontconfig1-dev`, `libxkbcommon-dev`, a
working wgpu stack).

## Crate layout

```
world/
├── Cargo.toml                 # [workspace]
├── README.md                  # this file
├── docs/
│   ├── DESIGN.md              # ← the authoritative master design
│   └── design/                # the six dimension docs + two adversarial critiques
└── crates/
    ├── world-protocol/        # agentd Event/Intent mirror + WS client — LIGHT deps, CI-green, no GPU
    ├── world-app/             # Slint + wgpu renderer binary (+ deferred Bevy) — HEAVY deps
    └── world-vision/          # snapshot / act MCP stdio plugin — LIGHT deps, no GPU
```

`world-protocol` is the only crate everything depends on and stays heavy-dep-free (the
future "agentd client SDK"). `world-vision` is a leaf, buildable and MCP-handshake-testable
from day one. `world-app` is the only crate carrying the GPU toolchain and the
known-incomplete integrations.

## Docs

| File | Read for |
|------|----------|
| [docs/DESIGN.md](docs/DESIGN.md) | **Start here** — vision, anatomy, agentd contract + verdict, rendering + de-risking, station/embodiment systems, decisions/risks, milestone roadmap |
| [docs/design/01-world-and-interaction.md](docs/design/01-world-and-interaction.md) | The Atrium, the approach→activate→fill→dismiss loop, council-as-a-place |
| [docs/design/02-agentd-integration.md](docs/design/02-agentd-integration.md) | The wire protocol, session model, intents, feasibility (load-bearing) |
| [docs/design/03-rendering-architecture.md](docs/design/03-rendering-architecture.md) | Pattern A, shared-wgpu, station screens, LOD, tier gating |
| [docs/design/04-agent-embodiment-and-vision.md](docs/design/04-agent-embodiment-and-vision.md) | Avatars, the `world_look` vision loop, the wgpu readback path |
| [docs/design/05-station-and-app-system.md](docs/design/05-station-and-app-system.md) | The StationKind catalog, bindings, generative UI |
| [docs/design/06-roadmap-and-scaffold-plan.md](docs/design/06-roadmap-and-scaffold-plan.md) | Milestones M0–M3, the exact scaffold spec |
| [docs/design/critique-agentd.md](docs/design/critique-agentd.md) | Adversarial review of the agentd integration (the R1 vision blocker) |
| [docs/design/critique-rendering.md](docs/design/critique-rendering.md) | Adversarial review of the rendering stack (the wgpu-28 / Bevy / Mode I findings) |

> When the dimension docs and `DESIGN.md` disagree, **`DESIGN.md` wins** — it resolves the
> conflicts and folds in the critique blockers as explicit decisions and risks.
