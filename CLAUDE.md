# ApexOS-RS — Agent & Developer Guide

> Pure-Rust native UI distro of ApexOS. Slint frontend + KMS/DRM direct rendering.
> Replaces Chromium kiosk with a single ~10 MB binary. agentd is unchanged.
> Runs on any spare device — Pi Zero 2W to GPU workstation.

See also: [docs/architecture.md](docs/architecture.md) | [docs/build-roadmap.md](docs/build-roadmap.md) | [docs/slint-notes.md](docs/slint-notes.md)

Reference runtime: `../ApexOS` (Rust — **do NOT modify** during this port).

---

## Platform vision

ApexOS-RS targets any spare device — not just Pi 5. Pi 5 16GB boards now cost $300+ due to AI demand on RAM supply. The real hardware base is what people already own: Pi 4 2GB, last-gen mini-PCs, old laptops, replaced Mac Minis, studios. Some of these have GPUs that run models far beyond what Pi native hardware can handle.

| Tier | Example hardware | `SLINT_BACKEND` | cerebro RSS | LLM |
|------|-----------------|-----------------|-------------|-----|
| Nano | Pi Zero 2W, any 512MB Linux board | `linuxkms-femtovg` | 23 MB (FTS5 only) | API only |
| Micro | Pi 4 1-2GB, older ARM64 | `linuxkms` | 275 MB (bge-small) | API or small local |
| Standard | Pi 5, x86 mini-PC | `linuxkms` | 275 MB | Ollama 7-13B |
| Pro | x86 + GPU (CUDA/ROCm/Metal) | `winit` | 275 MB (bge-small) | Ollama 30-70B local |

**Design rule:** build UI features for Nano constraints first — no assumption of fast inference, graceful when embedding is disabled, no hard-coded timeouts shorter than 30s for LLM calls. Faster tiers get the same UI, they just respond faster.

**Deployment mode** (orthogonal to hardware tier):

| Mode | Device | apexos-rs-ui? | Interface |
|------|--------|---------------|-----------|
| Kiosk | Pi + HDMI | yes, `linuxkms` | local display |
| Headless | server, laptop, DGX Spark | no | browser + mobile PWA |
| Desktop | x86 with shared monitor | yes, `winit` | native window |

Headless is already fully supported — agentd is a pure daemon. Mobile PWA and browser UI are the interfaces. Install flow asks "dedicated display?" and skips apexos-rs-ui on headless nodes. On a ROCm laptop: run agentd headless, access at `http://laptop:8787`, join the mesh — it's just an inference node.

**Mesh inference:** a Pro/GPU node (CUDA/ROCm/Metal) hot-swaps as inference backend for the cluster. agentd `POST /api/backend` at runtime, no restart needed. DGX Spark = Titan tier: arm64 binary runs as-is, serves 70B+ models to whole mesh.

---

## What this is

ApexOS-RS is a **pure-Rust distro** — a single Cargo workspace containing the full stack:
the agent daemon, cognitive memory system, system tool plugins, and native Slint UI.
One `cargo build --release --workspace`. One `install.sh`.

```
┌─────────────────────── ApexOS-RS workspace ──────────────────────────┐
│                                                                        │
│  agentd         ──── ws://localhost:8787/ws ──┬──→ Browser / PWA      │
│  (agentd/)                                    │                        │
│                                         apexos-rs-ui                  │
│  cerebro-mcp   (cerebro/)            (Slint + KMS/DRM)                │
│  apexos-tools  (tools/)              renders to /dev/tty7              │
│  sensor-bridge (tools/)                                                │
│                                                                        │
└────────────────────────────────────────────────────────────────────────┘
```

Workspace layout:

```
agentd/crates/       # agent daemon (core · gateway · plugins · agent · store · agentd)
cerebro/crates/      # cognitive memory (cerebro lib · cerebro-mcp · cerebro-api · cerebro-cli)
tools/crates/        # system tool plugins (apexos-tools · apex-sensor-bridge)
ui-slint/            # Slint native UI (the unique contribution of this repo)
config/              # default plugins.toml, policy.toml
deploy/              # systemd service units
install.sh           # one-shot installer
```

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

---

## Pi 5 target

| Detail | Value |
|--------|-------|
| SSH | `ssh apex1@192.168.0.158` (LAN only, pw: `abnudc1337`) — borrowed board, separate drive for RS (the `apexos` user is the original ApexOS dev board) |
| OS | Debian trixie headless |
| Binary | `/usr/local/bin/apexos-rs-ui` |
| Service | `/etc/systemd/system/apexos-rs-ui.service` (from `deploy/apexos-rs-ui.service`) |
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
cd ~/ApexOS-RS
git pull
cargo build --release --workspace

# 3. Hot-swap a single binary (e.g. cerebro-mcp)
sudo systemctl stop agentd
sudo cp target/release/cerebro-mcp /usr/local/bin/cerebro-mcp
sudo systemctl start agentd
sudo journalctl -u agentd -n 20 --no-pager

# 4. Hot-swap the UI
sudo systemctl stop apexos-rs-ui
sudo cp target/release/ui-slint /usr/local/bin/apexos-rs-ui
sudo systemctl start apexos-rs-ui
sudo journalctl -u apexos-rs-ui -n 10 --no-pager
```

During UI development — run apexos-rs-ui directly (no service needed):
```bash
AGENTD_WS=ws://localhost:8787/ws SLINT_BACKEND=linuxkms ./target/release/ui-slint
```

---

## Dev on desktop (x86)

One-time setup: `sudo apt-get install -y libfontconfig1-dev libxkbcommon-dev libinput-dev libgbm-dev libegl-dev libudev-dev`.
These are **link-time** deps of the `backend-linuxkms-noseat` feature (compiled in even on desktop). `cargo check` passes without them; `cargo run`/`build` fails at link (`cannot find -lxkbcommon/-linput/-lgbm`).

No Pi needed for steps 1–9. Connect to the Pi's agentd over LAN — the post-hardening agentd
**defaults to a loopback-only bind**, so for LAN dev set `AGENTD_BIND=0.0.0.0:8787` in the Pi's
`/etc/agentd/env` (safe: a token is required for any non-loopback bind — see F036) and pass the token:

```bash
AGENTD_TOKEN=$(ssh apex1@192.168.0.158 'sudo grep -oP "(?<=AGENTD_TOKEN=).*" /etc/agentd/env') \
AGENTD_WS=ws://192.168.0.158:8787/ws cargo run
```

`SLINT_BACKEND` auto-detects `winit` when `DISPLAY` or `WAYLAND_DISPLAY` is set. No special config.

To simulate kiosk:
```bash
SLINT_FULLSCREEN=1 AGENTD_WS=ws://192.168.0.158:8787/ws cargo run
```

---

## Build order (current progress)

| Step | Feature | Gate | Status |
|------|---------|------|--------|
| 0 | Scaffold | `cargo build` compiles, WS connects, events logged | ✓ |
| 1 | Agent chat | Streaming text view, dark theme, send input | ✓ |
| 2 | Tool call blocks | Collapsible cards, inline approval buttons | ✓ |
| 3 | Home dashboard | CPU/RAM/disk bars, IAQ badge (`/api/run` poll) | ✓ |
| 4 | Sensor window | IAQ stats + thermal heatmap (custom painter) | ✓ |
| 5 | Session management | Session init, picker, history replay | ✓ |
| 6 | Voice controls | Mic → `/api/record/start`, speaker → `/api/speak` | ✓ |
| 7 | Settings | Soul.md editor (`TextEdit`), policy mode, plugin list | ✓ |
| 8 | Power + model/policy | Power modal, model/policy `ComboBox` | ✓ |
| 9 | KMS/DRM deploy | `SLINT_BACKEND=linuxkms`, systemd service, retire cage | ✓ |

Full per-step detail in [docs/build-roadmap.md](docs/build-roadmap.md).

**Gate to move to next step:** the feature described in `Gate` works end-to-end against a live agentd. Steps 1–9 are testable on desktop; step 9 requires Pi with KMS/DRM.

---

## Critical Slint patterns

Full notes in [docs/slint-notes.md](docs/slint-notes.md). The three you must know cold:

### 1. Thread model — never `#[tokio::main]`

```rust
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    let ui = AppWindow::new()?;
    rt.spawn(async move { /* all WS + HTTP work here */ });
    ui.run()?;  // Slint owns main thread
    Ok(())
}
```

### 2. Cross-thread UI updates

```rust
let ui_weak = ui.as_weak();   // Weak<AppWindow> — Send + Clone
rt.spawn(async move {
    // ... receive WS event ...
    slint::invoke_from_event_loop(move || {
        if let Some(ui) = ui_weak.upgrade() {
            ui.set_agent_text("hello".into());
        }
    }).ok();
});
```

### 3. Dynamic lists — `VecModel`

```rust
use slint::{VecModel, ModelRc};
let messages: Rc<VecModel<MessageItem>> = Rc::new(VecModel::default());
ui.set_messages(ModelRc::from(messages.clone()));
// push from Slint thread or invoke_from_event_loop:
messages.push(MessageItem { text: "hello".into(), ..Default::default() });
```

---

## agentd WebSocket protocol

On connect, send:
```json
{"type": "session_init"}
```
agentd responds:
```json
{"type": "hello", "session_id": 42}
```

Key inbound events. **NB:** the gateway sends the raw `Event` enum
(`serde_json::to_string(&event)`, no reshaping). Tool fields nest under
`call` (a `ToolCall`), and `ActionId`/`SessionId` are newtypes that
serialize as **bare numbers**, not strings — read `call.id` (number),
stringify it for the row key; don't expect a flat `call_id`.

| Event | Fields | Action |
|-------|--------|--------|
| `agent_text` | `delta: string` | append to text buffer (lazily creates the agent bubble + sets busy — Rust agentd has no `turn_started`) |
| `turn_started` | — | **Python agentd only — Rust agentd never emits it.** UI keeps a handler for cross-compat; on Rust the `agent_text` lazy-bubble path sets busy instead |
| `turn_complete` | — | clear busy, TTS if enabled |
| `tool_requested` | `call: {id, tool, args, needs_approval}` | push tool block (status=running) |
| `tool_result` | `call: <id>, output: {ok, content}` | update block by `call`; ok→done, !ok→error |
| `approval_pending` | `call: {id, tool, args}` | show approve/reject buttons |
| `sensor_reading` | `reading: {kind, …}` | update IAQ / thermal state |
| `wake_triggered` | — | flash wake indicator |

Send user message:
```json
{"type": "user_prompt", "text": "hello"}
```
Attach image(s) — the gateway shims each through `vision::prepare` (decode →
downscale ≤`VISION_MAX_EDGE` → re-encode) before the event, so `UserPrompt.images`
is always prepared b64 (`ContentBlock::Image`). `path` is workspace-confined;
arbitrary local images use `b64`. Also via HTTP: `POST /api/sessions/{id}/image`
with the same `{text?, images:[…]}` body (PWA / phone camera / curl).
```json
{"type": "user_prompt", "text": "what is this?",
 "images": [{"path": "screenshots/latest.png"}, {"b64": "<base64>", "media_type": "image/jpeg"}]}
```
Send approval (`action` = the numeric `ToolCall.id`; **not** `call_id`/`approved`):
```json
{"type": "user_approval", "action": 5, "granted": true}
```
Cancel a turn (agentd `cascade_cancel` aborts it but emits no `TurnComplete`,
so the UI must also clear its own busy + pending tool cards):
```json
{"type": "user_cancel"}
```
The gateway injects `session` into every inbound frame before deserializing
into `Event`, so frontends omit it. A frame that fails to deserialize is
**silently dropped** — wrong field names = no error, just nothing happens.

Full event list: `agentd/crates/core/src/types.rs` — `Event` enum.

---

## Environment variables

| Var | Default | Purpose |
|-----|---------|---------|
| `AGENTD_WS` | `ws://localhost:8787/ws` | agentd WebSocket URL |
| `SLINT_BACKEND` | auto | `winit` (desktop), `linuxkms` (Pi), `linuxkms-femtovg` (Pi Zero) |
| `SLINT_FULLSCREEN` | unset | `1` = fullscreen, no window chrome |
| `RUST_LOG` | `info` | tracing filter |
| `VISION_MAX_EDGE` | `1024` | agentd: longest-edge px cap for images entering model context (the token-bomb shim, clamped 128–4096) |
| `APEXOS_UI_SNAPSHOT_ADDR` | `127.0.0.1:8788` | ui-slint: loopback bind for the screen-mirror snapshot server (`take_snapshot`→PNG); the `screenshot_mirror` tool fetches from the matching `APEXOS_UI_SNAPSHOT_URL` (`http://127.0.0.1:8788/snapshot`) |
| `APEXOS_CAMERA_DEVICE` | auto | `camera_capture` tool: force a V4L2 node (e.g. `/dev/video0`) instead of auto-detecting. Auto order = Pi CSI camera (rpicam) → first `/dev/video*` webcam |
| `APEXOS_CAMERA_CMD` | unset | `camera_capture` tool: full custom capture command with a `{out}` placeholder (e.g. a gphoto2/network-cam grab); overrides all auto-detection |

---

## Gotchas

- **`libfontconfig1-dev` required for ui-slint** — `sudo apt-get install -y libfontconfig1-dev` on both Pi and dev machine. Without it `cargo check -p ui-slint` panics. Use `--exclude ui-slint` to check the rest of the workspace on a headless machine.
- **Never `#[tokio::main]`** — Slint requires the main thread. `#[tokio::main]` hijacks it. Build the runtime manually with `Builder::new_multi_thread()`.
- **`invoke_from_event_loop` is fire-and-forget** — it queues a closure and returns immediately. The closure runs asynchronously on the Slint thread. Do not assume immediate effect.
- **Slint strings are `SharedString`** — convert with `.into()`. Never pass a `&str` or `String` directly where Slint expects `SharedString`.
- **Pi KMS groups** — `agentd` user needs `render`, `video`, `input` groups: `sudo usermod -aG render,video,input agentd`. Only done once.
- **`apexos-rs-ui` runs as root** — `drmSetMaster` + `drmModePageFlip` require DRM master; on Pi without logind seat management, only root wins reliably. Service uses `User=root`, `PAMName=login`, `TTYPath=/dev/tty7`.
- **`WantedBy=multi-user.target`** — Pi boots to `multi-user.target` by default, not `graphical.target`. Service must be in `multi-user.target.wants` or it never starts.
- **`slint` needs `backend-linuxkms-noseat` feature** — default `slint = "1"` only compiles winit. Add `features = ["backend-linuxkms-noseat", "backend-winit"]`.
- **KMS build deps on Pi** — `libssl-dev libgbm-dev libegl-dev libudev-dev libinput-dev libxkbcommon-dev libfontconfig1-dev` all required; missing any fails the build or link step.
- **`text file busy`** — always `systemctl stop apexos-rs-ui` before `cp`. A running binary cannot be overwritten.
- **`fontconfig` missing on Pi** — `sudo apt-get install -y libfontconfig1-dev` if build fails.
- **`/etc/agentd` config writes (os error 13)** — agentd self-writes `soul.md` (Settings save + `update_system_prompt`) and `policy.toml`/`plugins.toml`/`peers.toml` (self-evolution). `/etc/agentd` stays root-owned (the `env` token file must be `600 root:root`), so install.sh chowns those four *files* to `agentd`. Atomic writes (temp+rename) need dir-write the agentd user lacks, so `write_atomic` falls back to an in-place write. Re-run `install.sh` to fix ownership on an already-deployed Pi.
- **Slint build step** — `.slint` files are compiled by `build.rs` at build time. If you change a `.slint` file but `cargo build` doesn't recompile, `touch ui-slint/build.rs`.
- **Pi Zero 2W rendering** — BCM2837 uses `vc4` not `v3d`. Set `SLINT_BACKEND=linuxkms-femtovg` for software rendering; no GPU required.
- **agentd must be running** — the UI will retry the WS connection on disconnect. In dev, agentd can be on a remote Pi; just set `AGENTD_WS`.
- **Session replay** — send `{"type": "session_init", "session_id": 42}` to restore a prior session. agentd replays the full message history.
- **A plain `Rectangle` is not a layout** — children are absolutely positioned and it does **not** report their size upward. A `for`-row built on a bare `Rectangle` collapses to ~0 height and rows draw on top of each other. Use a `VerticalLayout`/`HorizontalLayout` for any row that must size to its content.
- **No key auto-repeat on linuxkms** — `i-slint-backend-linuxkms` dispatches one `KeyPressed`/`KeyReleased` per libinput event with no repeat synthesis (libinput doesn't repeat; a compositor normally does). Holding a key = one hop on the Pi. Works on desktop (winit). Backend limitation — not fixable in app code without forking the backend.
- **Sonus playback = `ffmpeg -f alsa` + the `audio` group + a real ALSA device** — server-side playback (`POST /api/sonus/play`) decodes with `ffmpeg -f alsa <dev>` (not ffplay: SDL routes to the ALSA `default`, which on a **Pi 5 is a nonexistent card 0 — no analog jack, HDMI only**). Three things must hold: (1) the `agentd` user is in the **`audio`** group (`usermod -aG audio agentd` — install.sh does this; restart agentd to pick it up); (2) `SONUS_AUDIO_DEVICE` points at a real card in `/etc/agentd/env` (Pi 5 HDMI-0 = `plughw:1,0`; find cards with `aplay -l`); (3) the HDMI display actually has speakers. Same `audio`-group requirement applies to `/api/speak` TTS (`aplay`). One track at a time (process-global child); a new play or `/api/sonus/stop` kills the previous.
- **Camera eyes = the `video` group + a backend tool** — `camera_capture` (and `/api/snapshot`) shell out: Pi CSI needs `rpicam-apps` (install.sh adds it on Pi), USB/laptop needs `ffmpeg` (always installed) reading `/dev/video*`. The `agentd` user must be in the **`video`** group (install.sh now grants it even on headless nodes; restart agentd to pick it up) or `/dev/video*` opens fail with permission denied. First V4L2 frame is often dark — the tool grabs warmup frames (`ffmpeg -frames:v 5 -update 1`, `rpicam -t 1200`, `fswebcam -S 8`); a stubborn webcam can be pinned via `APEXOS_CAMERA_DEVICE` or fully overridden with `APEXOS_CAMERA_CMD`. Auto-detect tries Pi CSI first, then `/dev/video*` in order; "no camera" returns a note, not an error.
- **soul.md is identity ONLY — never list hardware or tools in it.** agentd generates a live `## Current embodiment` block (node tier, senses camera/thermal/GPIO, memory mode, mesh peers, and the **actual tool registry**) and appends it to soul.md when building the system prompt each turn (`build_embodiment` in agentd `main.rs`, refreshed every 30s; `TurnEngine` holds soul + embodiment separately so `read_soul_md`/`update_system_prompt` touch only the soul). A hardcoded tool list in soul.md is what made APEX "unable to see" tools that were actually loaded — the live block can't go stale. soul.md is identity (portable across the mesh, evolves at runtime); the embodiment block is the body it currently inhabits.

---

## Git discipline — PR workflow (default since June 2026)

- **Never commit to `main`. Work on a feature branch off the latest `origin/main`:** `feat/…`, `fix/…`, `chore/…`, `proto/…`. One branch = one slice.
- **Ship via PR.** When a step/feature gate passes: commit, push the branch, open a PR with `gh pr create`. **Do NOT merge it yourself** — André reviews and merges, or explicitly tells you to merge.
- **After merge → André runs `apexos-update`** to deploy, then we take the next slice.
- **Commit format:** imperative, lowercase (`implement agent chat streaming view`); end the message with the `Co-Authored-By` trailer.
- **Never amend a pushed commit. Never force-push.**
- **Docs travel with code.** Update CLAUDE.md + the relevant docs/ file in the same PR.

---

## Cerebro agent

All Cerebro MCP calls use agent `FORGE` (agent_id=`"FORGE"`, ⚒, #B7410E).

## Cerebro session protocol (mandatory)

**Session START** — call `session_recall` before touching any code:
```
session_recall(query="ApexOS-RS Slint UI build status step progress", agent_id="FORGE")
```
This pulls prior session summaries, unfinished business, and stored procedures — instant
hotstart even after a context reset, reboot, or compaction.

**Session END** — always save before closing:
```
session_save(
  session_summary="one paragraph: what was built, what broke, what was learned",
  key_discoveries=["Slint gotcha X", "agentd protocol detail Y"],
  unfinished_business=["step 6 voice half done — POST /api/record/start wired, TTS pending"],
  agent_id="FORGE",
  priority="HIGH"
)
```
Then as needed:
- `store_procedure` — Slint patterns, Pi gotchas, WS/agentd protocol quirks
- `store_intention` — next concrete action (salience 0.8–0.95); one intention per deferred item
- `episode_start` / `episode_add_step` / `episode_end` — wrap any multi-step implementation sequence

The three vaults:
- **CLAUDE.md** — static project blueprint; locked decisions, architecture, critical patterns
- **docs/*.md** — dynamic per-topic detail; evolve as the project progresses, grow without limit
- **cerebro** — session memory, discoveries, intentions, procedures; survives compaction and cold starts
- **git** — code truth; commit messages are the implementation log

---

## Docs

Load only the relevant doc when entering a subsystem — do not load all of them.

| File | Load when working on |
|------|----------------------|
| `docs/repo-map.md` | Navigation — crate tree, per-crate key files, "how a message flows", "where do I change X?" |
| `BACKLOG.md` (repo root) | Outstanding work — audited findings + parked items, de-duped & prioritized |
| `docs/architecture.md` | System layout, workspace crate structure, dependency graph |
| `docs/build-roadmap.md` | Build order, step-by-step detail, deferred items |
| `docs/slint-notes.md` | Slint patterns, binding loop rules, layout gotchas |
| `docs/slint-reference/` | Exact widget/element API (vendored from official Slint repo) — look up before guessing syntax |
| `docs/ui-glowup.md` | Desktop shell, persona skins, window manager, glowup roadmap (G0–G7) |
| `docs/symbiosis.md` | Runtime cognitive architecture — APEX⇄agentd⇄Cerebro loops, the soul.md Sleep-loop gap, CCBS boot wiring |
| `docs/evolutionary-layer.md` | Exo-evolution charter — competence grows in Cerebro (not the weights): the Darwinian skill loop, schematic layer, selection pressure, skill↔identity boundary |
| `docs/app-parity.md` | Bringing original ApexOS apps to -RS — parity matrix, build tiers, AI⇄app symbiosis contract, and the "how to add an app" recipe |

---

## Deferred / post-v1

- ~~PTY terminal~~ — shipped (libc `openpty`, `/terminal-ws` WebSocket endpoint in agentd gateway)
- ~~Sketchpad~~ — shipped (`sketchpad_view`: `Path`-stroke canvas + `POST /api/sketch` tiny-skia raster + `sketch_snapshot` tool + line/rect/ellipse shape tools)
- ~~Cerebro web UI integration~~ — shipped as the `Web` launcher (🌐): external-browser tiles for Cerebro/Sensor Head + open-any-URL bar (Slint can't embed a webview; opens via `xdg-open`/`$BROWSER`)
- Monaco / code editor — SSH/vim or embedded webkit2gtk webview for soul.md heavy editing
- Sub-agent windows — `Popup` per child session, maps to `SubAgentStarted` events
- `apexos-core` vendor — optionally vendor agentd's core crate for shared `Event` types (avoids JSON string matching), blocked on agentd publishing it as a library crate
- ~~Vision input — core eyes~~ — shipped: the downscale **shim** (`apexos_core::vision`, `VISION_MAX_EDGE` cap = the SensorHead token-bomb guard) + the **vision tool-result path** (a tool returns `{"vision":{"path"|"b64"},"text"}` → `turn.rs::vision_rewrite` shims it → multimodal content block; Anthropic native, OAI/Ollama follow-up user msg). `sketch_snapshot` now hands APEX the drawing inline. Remaining vision follow-ups still deferred:
  - ~~**Screenshot "mirror" tool**~~ — shipped: `screenshot_mirror` (apexos-tools) → ui-slint serves its own `Window::take_snapshot()` PNG over a loopback endpoint (renderer-agnostic — winit/femtovg, linuxkms/skia, femtovg-software all snapshot the rendered scene, so **no** DRM readback and **no** Wayland screencopy) → tool writes it under the workspace and returns the same `{vision:{path}}` sentinel, zero agentd changes. Graceful "no display" when headless.
  - ~~**Camera eyes — physical-world capture**~~ — shipped: the `camera_capture` tool (apexos-tools) snaps one frame and returns the same `{vision:{path}}` sentinel — zero agentd changes, mirrors `screenshot_mirror`. Device-agnostic backend pick (the capture half of HW-tier detection): Pi CSI camera (`rpicam-jpeg`/`libcamera-jpeg`) → USB/laptop webcam over V4L2 (`ffmpeg -f v4l2`) → `fswebcam`; warmup frames per backend (no black first frame); `APEXOS_CAMERA_DEVICE`/`APEXOS_CAMERA_CMD` overrides; graceful "no camera" note. The PWA's `GET /api/snapshot` was generalized to the **same** multi-backend detection (was Pi-CSI-only) so laptop/USB-cam nodes work too. install.sh adds `rpicam-apps` on Pi (ffmpeg covers V4L2) and grants the `video` group to agentd even headless.
  - **User-attached images** — *plumbing shipped*: first-class `ContentBlock::Image` + `UserPrompt.images`, folded in `state`/router, serialized by both providers (Anthropic `image` / OpenAI `image_url`), gateway shims raw `path`|`b64` refs via `vision::prepare` on the WS `user_prompt` frame and at `POST /api/sessions/{id}/image`. Remaining surface: an **external-PWA upload/camera button** (`mobile.html` lives outside this repo) — the native Slint workspace image picker shipped (#31).
  - **cerebro `describe_image` / `search_vision`** — still stubbed; describe_image wants a VLM, search_vision wants CLIP-style image embeddings.

---

## Meta — when to update this file

- A locked decision changes → update `## Locked decisions`
- A build-order step completes → tick it in the table
- A Pi gotcha is discovered → add to `## Gotchas`
- A deferred item resolves → move it out of `## Deferred`
- A doc file is created → add a row to the `## Docs` table
- Keep this file under ~160 lines of content (excluding this Meta section)

### What never goes in CLAUDE.md or docs/*.md

- Task progress, session logs, completed-work summaries → use Cerebro (`session_save`)
- Git SHAs, version pins → stale in days, belong in git history
- Commentary on what you just did → belongs in commit messages
