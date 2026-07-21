# ApexOS-RS — Agent & Developer Guide

> Pure-Rust native UI distro of ApexOS. Slint frontend + KMS/DRM direct rendering.
> Replaces Chromium kiosk with a single self-contained binary (~30 MB). agentd is unchanged.
> Runs on any spare device — Pi Zero 2W to GPU workstation.

**This file is the lean core** (2026-07-21 refactor — the fat moved out, load on demand):

- **`docs/gotchas.md` — the invariant ledger. MANDATORY: before modifying any subsystem, grep it for that subsystem and read the matching entries.** Most entries were written after something broke on a live node; new gotchas go THERE, not here.
- `docs/env-vars.md` — every environment variable (agentd, ui-slint, voice, sensors, cache, install).
- `docs/agentd-protocol.md` — the WS wire contract (events, frames, session routing, examples).
- Deferred/post-v1 ledger → end of `docs/build-roadmap.md`.

Reference runtime: `../ApexOS` (Rust — **do NOT modify**). Siblings: `../Occipital-RS` (web cortex, standalone), ApexOS-RV (`github.com/buckster123/ApexOS-RV`, no_std RISC-V — pins `apexos-protocol` as its wire contract).

---

## Platform vision

Target = any spare device, not just Pi 5 (16GB boards cost $300+ now). Nano-first **design rule:** build UI features for Nano constraints — no assumption of fast inference, graceful when embedding is disabled, no LLM timeouts under 30s. Faster tiers get the same UI, just faster.

| Tier | Example hardware | `SLINT_BACKEND` | cerebro RSS | LLM |
|------|-----------------|-----------------|-------------|-----|
| Nano | Pi Zero 2W, any 512MB board | `linuxkms-femtovg` | 23 MB (FTS5 only) | API only |
| Micro | Pi 4 1-2GB, older ARM64 | `linuxkms` | 275 MB (bge-small) | API or small local |
| Standard | Pi 5, x86 mini-PC | `linuxkms` | 275 MB | Ollama 7-13B |
| Pro | x86 + GPU (CUDA/ROCm/Metal) | `winit` | 275 MB | Ollama 30-70B local |

**Deployment mode** (orthogonal): Kiosk (Pi + HDMI, `linuxkms`) · Headless (no UI — browser/PWA is the surface) · Desktop (`winit` window in the user's session). Mesh inference: a GPU node hot-swaps as the cluster's backend via `POST /api/backend`, no restart.

---

## What this is

A **pure-Rust distro** — one Cargo workspace, one `cargo build --release --workspace`, one `install.sh`:

```
agentd/crates/       # agent daemon (core · gateway · plugins · agent · store · agentd)
cerebro/crates/      # cognitive memory (cerebro lib · cerebro-mcp · cerebro-api · cerebro-cli)
tools/crates/        # system tool plugins (apexos-tools · apex-sensor-bridge)
                     #   + workspace-EXCLUDED sidecars: apex-tts (Kokoro), apex-stt (Whisper)
apexos-protocol/     # shared wire types (no_std-capable; external consumer: ApexOS-RV)
ui-slint/            # Slint native UI (the unique contribution of this repo)
web/                 # -RS-owned browser/PWA frontend (headless nodes' human surface)
config/              # default plugins.toml, policy.toml, soul.md, parts inventory
deploy/              # systemd units, avahi, udev/USB helpers, fonts
install.sh           # one-shot installer (idempotent; `apexos-update` re-runs it)
```

agentd serves `ws://localhost:8787/ws` + REST; ui-slint renders via KMS/DRM (kiosk) or winit (desktop); browser/PWA speak the same WS contract.

---

## Locked decisions

- **Language**: Rust — every binary in the workspace
- **Repo model**: copy-and-diverge distro (no git submodules); canonical ApexOS stays Chromium
- **UI framework**: Slint (`.slint` declarative, compiles to native GL)
- **Rendering**: `SLINT_BACKEND=linuxkms` on Pi (KMS/DRM, no Wayland, no cage)
- **Thread model**: tokio on background threads, Slint event loop owns main thread — **never** `#[tokio::main]`
- **Cross-thread UI**: `slint::invoke_from_event_loop()` only — never touch UI handles from tokio tasks directly
- **Memory (cerebro Nano)**: `CEREBRO_EMBED_MODEL=""` → ~23 MB RSS, FTS5-only search
- **Memory (cerebro Micro+)**: `BAAI/bge-small-en-v1.5` → ~275 MB RSS, cosine ANN
- **Pi Zero 2W support**: `SLINT_BACKEND=linuxkms-femtovg` (software renderer, ~7 MB)
- **UI shape**: a persona-skinned desktop shell — 6 skins, Rust-owned window manager, per-persona agent voice (the `style` layer of the 4-layer system prompt). `docs/ui-glowup.md` is the UI source of truth (G0–G7)

---

## Pi 5 target

| Detail | Value |
|--------|-------|
| SSH | `ssh apex1@192.168.0.158` (LAN only, pw: `abnudc1337`) — borrowed board, separate drive for RS |
| OS | Debian trixie headless |
| Binary | `/usr/local/bin/apexos-rs-ui` |
| Service | `/etc/systemd/system/apexos-rs-ui.service` |
| agentd WS | `ws://localhost:8787/ws` |

**Always build on Pi — never cross-compile.** Pi is Cortex-A76 (arm64).

---

## Deploy workflow

```bash
# 1. Dev machine
cargo test --workspace --exclude ui-slint   # ui-slint needs fontconfig; skip on headless dev
git add -p && git commit -m "short imperative description"
git push

# 2. On Pi — build the whole workspace
cd ~/ApexOS-RS && git pull && cargo build --release --workspace

# 3. Hot-swap a binary (stop service → cp → start; `text file busy` = you skipped the stop)
sudo systemctl stop agentd && sudo cp target/release/cerebro-mcp /usr/local/bin/ && sudo systemctl start agentd
sudo systemctl stop apexos-rs-ui && sudo cp target/release/ui-slint /usr/local/bin/apexos-rs-ui && sudo systemctl start apexos-rs-ui
```

Dev UI run (no service): `AGENTD_WS=ws://localhost:8787/ws SLINT_BACKEND=linuxkms ./target/release/ui-slint`

## Dev on desktop (x86)

One-time: `sudo apt-get install -y libfontconfig1-dev libxkbcommon-dev libinput-dev libgbm-dev libegl-dev libudev-dev` (link-time deps of `backend-linuxkms-noseat`; `cargo check` passes without them, `cargo build` fails at link).

No Pi needed — connect to the Pi's agentd over LAN (install.sh seeds `AGENTD_BIND=0.0.0.0:8787`, token-gated):

```bash
AGENTD_TOKEN=$(ssh apex1@192.168.0.158 'sudo grep -oP "(?<=AGENTD_TOKEN=).*" /etc/agentd/env') \
AGENTD_WS=ws://192.168.0.158:8787/ws cargo run
```

`SLINT_BACKEND` auto-detects `winit` when `DISPLAY`/`WAYLAND_DISPLAY` is set. `SLINT_FULLSCREEN=1` simulates kiosk.

## Build status

Original bring-up steps 0–9 (scaffold → chat → tools → dashboard → sensors → sessions → voice → settings → power → KMS deploy) are **all ✓** — per-step detail in `docs/build-roadmap.md`. The desktop-shell/persona glowup that followed (G0–G7) lives in `docs/ui-glowup.md` — the current UI source of truth. Adaptive UI phases A1–C shipped — `docs/adaptive-ui.md` is that contract.

---

## Critical Slint patterns

Full notes in `docs/slint-notes.md`. The three you must know cold:

```rust
// 1. Thread model — never #[tokio::main]; Slint owns main
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    let ui = AppWindow::new()?;
    rt.spawn(async move { /* all WS + HTTP work here */ });
    ui.run()?;
    Ok(())
}

// 2. Cross-thread UI updates — only via invoke_from_event_loop (fire-and-forget)
let ui_weak = ui.as_weak();
rt.spawn(async move {
    slint::invoke_from_event_loop(move || {
        if let Some(ui) = ui_weak.upgrade() { ui.set_agent_text("hello".into()); }
    }).ok();
});

// 3. Dynamic lists — VecModel behind ModelRc
let messages: Rc<VecModel<MessageItem>> = Rc::new(VecModel::default());
ui.set_messages(ModelRc::from(messages.clone()));
```

---

## agentd WebSocket protocol — summary

Full contract with examples: **`docs/agentd-protocol.md`**. The load-bearing minimum:

- On connect the **gateway pushes** `session_init` (client sends nothing first). `hello{resume_session}` restores, `hello{new:true}` mints; `hello` may carry `agent_id` (gated) + `persona`.
- Outbound frames are the raw `Event` enum (shared `apexos-protocol` types) — tool fields nest under `call`, IDs serialize as **bare numbers**. Key events: `agent_text{delta}`, `turn_complete`, `tool_requested{call}`, `tool_result{call, output}`, `approval_pending{call}`, `sensor_reading`.
- Inbound: `user_prompt{text, images?}`, `user_approval{action, granted}` (action = numeric ToolCall id), `user_cancel` (emits no TurnComplete — UI clears its own busy state). Gateway injects `session` into every inbound frame; undecodable frames are silently dropped.
- The gateway write task filters outbound per-socket: session-scoped stream reaches only the bound socket, global/status events (sensors, mesh, council…) reach every client. Clients never filter.

---

## Environment variables

Full table: **`docs/env-vars.md`**. Dev-critical: `AGENTD_WS` (default `ws://localhost:8787/ws`) · `SLINT_BACKEND` (auto) · `SLINT_FULLSCREEN=1` · `RUST_LOG` (`info`). Most agentd behavior knobs (backend/model, cache, dream, wakeups, sensors, voice) are **seed-only env — a Settings/API choice persists to a config file and wins on restart**.

---

## Gotchas — read `docs/gotchas.md` before touching a subsystem

The full ledger is **`docs/gotchas.md`** — grep it for your subsystem; entries end with explicit "don't do X" invariants. Topic map (grep keys):

- **Build/deploy**: fontconfig + KMS build deps · `text file busy` · low-RAM OOM build guard · verify deploys by *running commit* (`health.json`) · `apexos-update` idempotency (`install.conf`) · policy additive sync (quote-insensitive, validate-before-persist)
- **Slint/UI**: `#[tokio::main]` ban · `invoke_from_event_loop` · `SharedString` · bare `Rectangle` ≠ layout · no key-repeat / no wheel-scroll on linuxkms (→ `ScrollView` pattern) · mono emoji · `touch build.rs`
- **agentd core**: per-session turn gate · history trim + honest markers · session JSONL append order + `repair_history` · serde `#[derive(Default)]` shadowing trap · prompt-cache byte-stable prefix (never put volatile text in soul/embodiment/priming)
- **Identity/memory**: `agent_id` is system-**stamped**, never model-supplied · per-agent workspace stamping · CCBS boot priming · evolution undo snapshots private + H4 snapshot gate · PAC lint gate · `soul_rehearse` · wakeup bounds
- **Mesh/colony**: LAN bind + per-peer tokens · a2a lands in per-peer sessions · federation relays (provenance-stamped copies, `shared_only()` wire boundary, dream-digest echo-guard) · beacon · capabilities · cross-node spawn guards · vast.ai bridge invariants
- **Sensors/voice/vision**: SensorHead is external Python · persistence filter + sensitivity profiles · `SensorAlert` pairing · TTS/STT are workspace-excluded sidecars (ort decoupling) · client-side audio on desktop · camera/audio groups
- **FS/safety**: confinement lives in the tool (`apexos-confine`) · git roots · USB exo-workspace under the workspace · eject via root systemd unit, never sudo (`NoNewPrivileges`)
- **Adaptive UI**: tool-family idiom (no protocol changes) · latch etiquette (human always wins) · mutation cap · drag guard · reflex trigger mirror · geometry seed deferral
- **Protocol**: `apexos-protocol` no_std — run BOTH test gates; `Map<K,V>` alias, never bare `HashMap`
- **Welfare seams**: trim markers, substrate notices, honest tool-failure signals are **correctness fixes** — never strip them to save tokens

---

## Git discipline — PR workflow (default since June 2026)

- **Never commit to `main`. Work on a feature branch off freshly-fetched `origin/main`:** `feat/…`, `fix/…`, `chore/…`, `proto/…`. One branch = one slice. **Never open a PR whose base is another branch** (squash-merge + kept branches = the stacked PR merges into its base and never reaches main).
- **Ship via PR** (`gh pr create`). **Do NOT merge it yourself** — André reviews and merges, or explicitly tells you to merge. After merge → André runs `apexos-update` to deploy.
- **Commit format:** imperative, lowercase; end with the `Co-Authored-By` trailer.
- **Never amend a pushed commit. Never force-push.**
- **Docs travel with code.** Update `docs/gotchas.md` / the relevant docs/ file (and this file only for structural changes) in the same PR.

---

## Cerebro agent

All Cerebro MCP calls use agent `FORGE` (agent_id=`"FORGE"`, ⚒, #B7410E).

**Session START** — `session_recall(query="ApexOS-RS build status step progress", agent_id="FORGE")` before touching code.
**Session END** (and at milestones on long sessions) — `session_save(session_summary=…, key_discoveries=[…], unfinished_business=[…], agent_id="FORGE", priority="HIGH")`; plus `store_procedure` / `store_intention` / `episode_*` as needed.

The vaults: **CLAUDE.md** = lean core + pointers · **docs/gotchas.md** = invariants · **docs/*.md** = per-topic detail · **cerebro** = session memory, survives compaction · **git** = code truth.

---

## Docs

Load only the relevant doc when entering a subsystem.

| File | Load when working on |
|------|----------------------|
| `docs/gotchas.md` | **Any subsystem change — grep it first (mandatory)** |
| `docs/env-vars.md` | Any knob/config question — the full env reference |
| `docs/agentd-protocol.md` | WS wire contract — events, frames, session routing |
| `docs/repo-map.md` | Navigation — crate tree, key files, "where do I change X?" |
| `BACKLOG.md` | Outstanding work — audited findings + parked items |
| `PATTERNS.md` | Reusable-pattern manifest (idea · location · liftability) |
| `docs/architecture.md` | System layout, workspace structure, dependency graph |
| `docs/build-roadmap.md` | Bring-up steps 0–9 detail + the deferred/post-v1 ledger |
| `docs/slint-notes.md` | Slint patterns, binding loops, layout gotchas |
| `docs/slint-reference/` | Exact widget/element API (vendored) — look up before guessing |
| `docs/ui-glowup.md` | Desktop shell, persona skins, window manager (G0–G7) |
| `docs/adaptive-ui.md` | Adaptive UI — ui_* tools, /state, latch, reflexes, roadmap |
| `docs/symbiosis.md` | Runtime cognitive architecture — APEX⇄agentd⇄Cerebro loops |
| `docs/evolutionary-layer.md` | Exo-evolution charter — skills grow in Cerebro, not weights |
| `docs/edk.md` | Self-extension manual — identity/competence/morphology evolution |
| `docs/app-parity.md` | Bringing original ApexOS apps to -RS — matrix + recipe |
| `docs/agent-identity.md` | Identity charter — system-stamped agent_id, auth, souls |
| `docs/web-ui.md` | Browser + PWA frontend (`web/`) — login flow, WS contract |
| `docs/voice.md` | Voice I/O — sidecars, backends, env, roadmap |
| `docs/usb-workspace.md` | USB exo-workspace — marker-gated mount, eject, prep |
| `docs/occipital.md` | Web cortex integration — registration, deploy, policy |
| `docs/self-update.md` | Daemon self-update loop (mk3) — design + invariants |
| `docs/colony-mesh.md` | Mesh expansion — spine/edge, relay → capabilities → spawn |
| `docs/colony-federation.md` | Cross-cerebro federation — share/query/consolidate charter |
| `docs/pac.md` | PAC authoring dialect — glyph-lean souls/procedures (+ bench) |
| `docs/prompt-caching.md` | Prompt-caching discipline as a portable pattern |
| `docs/post-mk1.md` | Post-mk1 vision — hardening tracks, v0.1.0 release path |
| `docs/model-welfare.md` | Welfare charter — doctrine, red lines, audit ledger |
| `docs/porting-guide.md` | Porting -RS patterns to other projects |
| `docs/sdk/` | Extension SDK — outsider-facing guides + tool/event catalog for building on -RS |
| `docs/ideas/` | Design sketches & evals (state machine, goal driver) — inputs, some superseded |

---

## Meta — when to update this file

- A locked decision changes → update `## Locked decisions`
- **A gotcha is discovered → add it to `docs/gotchas.md`** (not here); keep the topic map above in sync only when a new *subsystem area* appears
- A new env var → `docs/env-vars.md`; protocol change → `docs/agentd-protocol.md`
- A deferred item resolves → the ledger at the end of `docs/build-roadmap.md`
- A doc file is created → add a row to `## Docs`
- **Periodic hygiene**: every few weeks / after a big arc, run the saved `docs-hygiene-audit` workflow (`.claude/workflows/`) — review findings, apply on a branch, ship via PR
- **Keep this file under ~250 lines / ~20 KB** — Claude Code warns on oversized CLAUDE.md and it loads into every session's context. Fat goes to docs/, this file points.

### What never goes in CLAUDE.md or docs/*.md

- Task progress, session logs, completed-work summaries → Cerebro (`session_save`)
- Git SHAs, version pins → stale in days, belong in git history
- Commentary on what you just did → belongs in commit messages
