# ApexOS-RS — Agent & Developer Guide

> Pure-Rust native UI distro of ApexOS. Slint frontend + KMS/DRM direct rendering.
> Replaces Chromium kiosk with a single self-contained binary (~30 MB, vs Chromium's hundreds of MB + runtime). agentd is unchanged.
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
The gateway injects `session` into every inbound (frontend→gateway) frame before
deserializing into `Event`, so frontends omit it. A frame that fails to
deserialize **on the gateway** is still silently dropped — wrong field names =
no error. **Outbound (gateway→UI) the ui-slint client now deserializes into the
shared `apexos-protocol::Event` and logs any undecodable frame** (no longer the
hand-rolled `["field"].as_str()` matching that vanished on a rename). Both sides
share the same `Event` types via the `apexos-protocol` crate. **The gateway
write task filters outbound frames per-socket** (`event_session`): a session-scoped
event (the conversation stream — `agent_text`/`tool_requested`/`turn_complete`/
`approval_pending`/…, plus `sub_agent_started`→parent) reaches only the socket bound
to that session; global/status events (sensors, council, mesh, vast, evolution) go
to every client. So a frontend receives **only its own session's stream + globals**
— clients don't (and shouldn't) filter outbound frames themselves. The supervisor
subscribes to the bus separately, so this never affects routing.

Full event list: `agentd/crates/core/src/types.rs` — `Event` enum.

---

## Environment variables

| Var | Default | Purpose |
|-----|---------|---------|
| `AGENTD_WS` | `ws://localhost:8787/ws` | agentd WebSocket URL |
| `AGENTD_AGENT_ID` | `APEX` | agentd: the node's bound agent identity. agentd **stamps** it onto every Cerebro tool call (overriding the model) and uses it for its own Cerebro writes — single source of truth, see `docs/agent-identity.md`. Per-session identities (multi-agent) layer on later |
| `AGENTD_CCBS` | unset | agentd: set `0`/`false` to disable CCBS boot-priming (the daemon-side `cognitive_bootstrap` injected into the system prompt on a session's first turn) |
| `AGENTD_BOOTSTRAP_MODE` | `standard` | agentd: CCBS token budget — `minimal` (1000) / `standard` (2000) / `full` (4500) |
| `AGENTD_DREAM_CRON` | `0 0 3 * * *` | agentd: cron (6-field, UTC) for the nightly autonomous `dream_run`; **empty disables it** |
| `AGENTD_HISTORY_TOKEN_BUDGET` | `120000` | agentd: per-session in-memory history window (rough tokens). Caps the always-on root session so it can't overrun the model context window; oldest whole turns drop at clean boundaries. `0` disables trimming. Lower it for small-context local models |
| `AGENTD_IDENTITIES` | `/etc/agentd/identities.toml` | agentd: the multi-agent identity registry (`[[user]]` + `[[agent]]`); see `docs/agent-identity.md`. Data layer only so far (3a) |
| `AGENTD_CACHE` | `1` | agentd (Anthropic only): `0`/`false`/`off` disables prompt caching entirely (system sent as a plain string, no `cache_control`). On = cache the system+tools prefix + (by default) the conversation. OpenAI/Ollama auto-cache regardless |
| `AGENTD_CACHE_CONVERSATION` | `1` | agentd (Anthropic only): `0`/`false`/`off` caches only the stable system+tools prefix, not the growing transcript. On = roll up to 3 breakpoints back through the conversation (the big 1M-giga-session win). No effect when `AGENTD_CACHE=0` |
| `AGENTD_CACHE_TTL` | `5m` | agentd (Anthropic only): `1h` (or `1hr`/`hour`/`3600`) selects the 1-hour cache TTL (write premium 2×, survives >5-min human pauses without re-writing the whole prefix); anything else = 5-minute (write 1.25×, cheaper for steady agentic loops) |
| `SLINT_BACKEND` | auto | `winit` (desktop), `linuxkms` (Pi), `linuxkms-femtovg` (Pi Zero) |
| `SLINT_FULLSCREEN` | unset | `1` = fullscreen, no window chrome |
| `RUST_LOG` | `info` | tracing filter |
| `VISION_MAX_EDGE` | `1024` | agentd: longest-edge px cap for images entering model context (the token-bomb shim, clamped 128–4096) |
| `APEX_FACE_GL` | auto | ui-slint: GL/SDF face render. Auto-on wherever a real GL context exists (desktop, Pi 4/5 V3D), 2D `FaceView` fallback otherwise; `0` forces 2D everywhere. Dev: `APEX_FACE_AUTOOPEN=1` opens the Face window at launch, `APEX_FACE_STATE=<emote>` previews an expression without agentd |
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
- **`apexos-rs-ui` must `Wants=` agentd, NOT `Requires=`** — the UI rides through an agentd restart via its WS reconnect loop, so it must not be lifecycle-coupled. `Requires=agentd.service` cascade-**stops** the kiosk whenever agentd stops (the self-update watchdog's `systemctl stop agentd` during a binary swap, or any restart) and does **not** bring it back → the kiosk goes dark until a manual `systemctl start apexos-rs-ui`. `Wants` + `After` keeps boot ordering without the teardown coupling. (Found closing the self-update loop: every self-update killed the kiosk.) Separate, still-open: an intermittent linuxkms/femtovg render fault (exit 1, no message) during heavy tool-card bursts — Pi-only, auto-restarts; tracked.
- **`slint` needs `backend-linuxkms-noseat` feature** — default `slint = "1"` only compiles winit. Add `features = ["backend-linuxkms-noseat", "backend-winit"]`.
- **KMS build deps on Pi** — `libssl-dev libgbm-dev libegl-dev libudev-dev libinput-dev libxkbcommon-dev libfontconfig1-dev` all required; missing any fails the build or link step.
- **`text file busy`** — always `systemctl stop apexos-rs-ui` before `cp`. A running binary cannot be overwritten.
- **`fontconfig` missing on Pi** — `sudo apt-get install -y libfontconfig1-dev` if build fails.
- **`/etc/agentd` config writes (os error 13)** — agentd self-writes `soul.md` (Settings save + `update_system_prompt`) and `policy.toml`/`plugins.toml`/`peers.toml` (self-evolution). `/etc/agentd` stays root-owned (the `env` token file must be `600 root:root`), so install.sh chowns those four *files* to `agentd`. Atomic writes (temp+rename) need dir-write the agentd user lacks, so `write_atomic` falls back to an in-place write. Re-run `install.sh` to fix ownership on an already-deployed Pi. **Also chowned (multi-agent identity, slice 3c): `/etc/agentd/identities.toml` + the `/etc/agentd/souls/` dir** (agentd seeds the registry; the identity API writes agents + per-agent soul files there).
- **Cerebro `agent_id` is system-stamped, not agent-supplied.** agentd's `Supervisor::dispatch_tool` **overwrites** `agent_id` on every `cerebro`-plugin call with the node's bound identity (`apexos_core::node_agent_id()`, env `AGENTD_AGENT_ID`, default `APEX`) via `stamp_agent_id()` — the model can't forget/typo/spoof its memory space (Cerebro's `Visibility::Private` isolation keys off this exact field). agentd's own Cerebro writes (council summaries, the evolution rollback store) use the **same** helper, so there's one identity, no `APEX`/`CLAUDE-APEX` drift. `agent_id` = the *caller's* space everywhere; cross-agent targets use distinct params (`target_agent_id`/`to_agent_id`), so the override never redirects a cross-agent op. Per-session binding (multi-agent): a WS `hello` frame may carry `agent_id` → agentd records `SessionId→agent_id` (`apexos_core::SessionBindings`, a `std::sync::Mutex` map shared gateway↔supervisor↔router); the stamp + CCBS resolve via `resolve_agent_id(session)` (bound agent → else `node_agent_id()`). So selecting an agent switches its Cerebro memory space; unbound sessions = APEX (unchanged). A bound **non-default** agent also runs on its own `soul_file` (`root_turn`'s `agent_soul_for` → `engine.with_system(soul).with_priming(block)`); APEX keeps the global hot-reloadable `soul.md`. **The self-evolution tools are agent-aware too** (`soul_target_for`, resolves `session→agent`): `read_soul_md` *reads*, and `propose_evolution{UpdateSystemPrompt}`/apply + rollback *write*, the **bound** agent's own `soul_file` — only APEX/unbound touches the global `soul.md` + live `soul_arc`. Before this a bound agent's soul-evolution silently clobbered APEX's global soul (the model reads/writes the wrong identity). Agent records live in `identities.toml` (`AGENTD_IDENTITIES`, seeded with APEX at startup). **Don't add a Cerebro call that trusts a model-passed `agent_id`, or a soul read/write that ignores the bound agent.**
- **Cognitive boot is daemon-driven, not agent-remembered.** On a session's **first turn**, `root_turn` calls `cognitive_bootstrap` via the `ToolProxy` (query = the user's prompt, `agent_id` = `node_agent_id()`), caches the result per session, and `TurnEngine::with_priming` appends it to the system prompt (`soul + embodiment + priming`, via the pure `compose_system`). So the agent wakes already oriented (where-it-left-off / skills / intentions) without the soul Wake-loop having to remember to call it. Bounded (15s) + graceful → an unavailable Cerebro never delays/wedges the first turn (it just runs un-primed). Consolidation is autonomous too: `spawn_nightly_dream` calls `dream_run` directly on a cron (`AGENTD_DREAM_CRON`, default 03:00 UTC) — a background task, **not** a scheduled `UserPrompt`, so it costs no LLM turn and can't be skipped. Both are per-identity (scoped to `node_agent_id()`). Opt-out `AGENTD_CCBS=0`. See `docs/agent-identity.md` (slice 2).
- **FS-tool confinement lives in the tool, not the policy layer.** `read_file`/`list_dir` are policy `allow` (no approval prompt), so the **tool process is the only gate** — confinement can't live in the approval layer alone. `tools.rs::confine(path, write)` is the single source of truth: writes/creates/deletes → **workspace only** (hard); reads/lists → **workspace + a small read allowlist** (`/etc/agentd/parts` for the EDK inventory, `/sys`, `/proc/cpuinfo`/`meminfo`, `/var/lib/agentd/update` for the self-update outcome markers; extend with `AGENTD_READ_ROOTS`, colon-sep) **minus** an always-blocked secret denylist (`/proc/*/environ`, `/etc/agentd/env`, `~/.ssh`, `/etc/shadow`, `*.api_key`). It rejects `..` (component-based) and operates on the **canonical** path (symlinks resolved; closes `delete_path`'s TOCTOU). Two *other* enforcement points exist by design (separate processes, can't share code): the gateway's `resolve_workspace_path`/`_write_path` (HTTP routes — images, audio in/out) and `policy.rs::workspace_decision` (approval gating, now fed **all** path-typed args — `path`/`output_path`/`dest`/… — by the supervisor, not just `path`). `http_fetch` re-runs `ssrf_guard` on every redirect hop; residual DNS-rebind TOCTOU (needs a pinned-IP connector) is the only known SSRF gap left.
- **Git tools confine to "git roots", not just the workspace.** `apexos-tools` ships `git_*` (status/diff/log/branch/init/commit/push/checkout/reset/merge) that shell out to the system `git` via `cmd_capture` — **argv, never `/bin/sh`** (no shell-injection surface), with a leading-`-` guard (`git_safe_arg`) against option injection on positional refs/branches/paths. The repo dir is confined by `confine_git_repo` to `git_roots()` = the agent workspace **+ `AGENTD_GIT_ROOTS`** (colon-sep absolute paths, env-extensible exactly like `read_roots()`/`AGENTD_READ_ROOTS`). Policy: read-only verbs + `git_init`/`git_branch`/`git_commit` are `allow` (so they MUST self-confine — `git_roots` is the gate, mirroring the FS-confinement rule); `git_push`/`git_checkout`/`git_reset`/`git_merge` (publish or rewrite the tree) are `ask`. **Safe-by-default:** with no `AGENTD_GIT_ROOTS` the only git root is the workspace, so an `allow` commit can only touch the agent's own repo; an operator opts a repo in (e.g. `AGENTD_GIT_ROOTS=/opt/ApexOS-RS` for APEX's self-update loop) deliberately — and source edits still go through `write_file` (workspace-only) / `run_command` (`ask`), so committing isn't a back-door to editing. Committer = `AGENTD_AGENT_ID` (default APEX). **Already-deployed nodes need the git rules added to their live `/etc/agentd/policy.toml`** (config/policy.toml only seeds fresh nodes) — or APEX `propose_evolution{update_policy_rule}`s them — else the new tools gate as `unknown → ask` in suggest mode.
- **The workspace is per-agent, system-stamped like `agent_id`.** apexos-tools is **one process for all agents**, so its FS root can't be a process-global env var — `apexos_core::agent_workspace_root(agent_id)` is the single source of truth (APEX/unbound → `AGENTD_WORKSPACE`, **byte-identical** to before; a bound non-default agent → `<base>/workspaces/<agent_id>`, with the `slug()`-safe id guarded against `/`/`..`). `Supervisor::dispatch_tool` stamps it as **`__workspace`** on every `apexos-tools`-plugin call (mirrors `stamp_agent_id`: the insert overwrites any model-supplied value → the model can't widen its own confinement). The tool pins it in a thread-local for the dispatch (`tools.rs::call()` + a RAII `WorkspaceGuard`; the MCP server is single-threaded/synchronous so this is race-free) and `resolve_path`/`workspace_root` resolve against it (env fallback for direct-MCP/tests). Per-agent dirs **nest under** the global workspace, so `policy.rs::workspace_decision` (`starts_with(AGENTD_WORKSPACE)`) + the gateway resolvers keep working unmodified — **`confine()` is the real per-agent gate**: guests are mutually sealed (confine + `..` rejection), APEX (node owner, root = the global ws) can read down into `workspaces/*` (intentional oversight, not full mutual isolation). The gateway provisions `<base>/workspaces/<id>` on agent-create. **Don't read `AGENTD_WORKSPACE` directly in a new FS path — go through `confine()`/`resolve_path` (tool side) or `agent_workspace_root` (agentd side), or you'll bypass the per-agent root.** *Known gap:* the gateway HTTP image/audio handlers aren't agent-aware yet (a bound session's upload lands in the node root — inside the ws, not an escape).
- **Session history is a bounded window, not unbounded.** Every `UserPrompt` re-sends the session's full `Vec<Message>` to the model; the always-on root session (`SessionId(0)`) funnels every sensor alert + scheduled task into it forever, so with no cap it eventually overruns the context window → restart-surviving crash-loop. The handler runs `apexos_core::history::trim_history(history, AGENTD_HISTORY_TOKEN_BUDGET)` (default 120k tokens, `0` disables) right after pushing the new user message — it drops whole oldest turns but **only at clean user-turn boundaries** (a genuine user message with no `tool_result`), so a kept `tool_result` is never orphaned from its `tool_use` (the Anthropic API rejects that), and always keeps ≥ the last turn. It bounds **both** the resident `Vec` and the context sent to the model; the **on-disk JSONL stays append-only** (full history preserved for replay — the working window is a subset). So replaying an ancient root session shows everything, but the model only ever sees the recent window. Token estimate is rough (≈chars/4; images flat-charged, not by base64 length). Applies to all sessions, but only the root session realistically reaches the cap.
- **Turns are serialized per session — at most one `root_turn` in flight.** The router loop drives a `TurnGate` (`admit`/`complete`/`cancel`): a `UserPrompt` for a session with a turn already running is **queued FIFO**, not spawned, and runs when the slot frees. The slot frees via a `turn_done` mpsc fired by a `TurnSlotGuard` whose `Drop` runs on completion, abort **and** panic (so a cancelled/crashed turn can't wedge the session). The loop is a `tokio::select!` over the bus and `turn_done`. Without this, two concurrent prompts each spawned a turn → the second's abort handle overwrote the first (uncancellable), their history writes raced (later wins, drops messages), the disk JSONL diverged, ActionIds collided. **All** turn-spawning paths funnel through the one `UserPrompt` arm (sensor alerts + a2a messages re-emit `UserPrompt` on the bus), so they're covered too. **Don't spawn `root_turn` outside the gate, or you reintroduce the race.** `UserCancel` clears the session's queue ("stop means stop"); sub-agent (`SpawnAgent`) turns use unique child ids and are not gated (no per-session concurrency).
- **A tool absent from `policy.toml` gates in suggest mode (`unknown → ask`, `policy.rs`).** So a read-only/virtual tool with no rule *silently* requires approval — and if that approval never resolves the turn looks "cut off" (this bit self-evolution: `read_soul_md`/`propose_evolution`/`rollback_evolution` had no rules, so suggest-mode evolving stalled; yolo bypassed the gate and masked it). Read-only tools (e.g. `read_soul_md`, the documented soul pre-flight) must be explicit `allow`; self-modifies `ask`. **`install.sh` only writes `config/policy.toml` when `/etc/agentd/policy.toml` is absent** (preserves self-evolved policy), so a default-policy fix reaches *fresh* nodes only — already-deployed nodes need their live file patched (or have APEX `propose_evolution{update_policy_rule}` in yolo). **`propose_evolution` acks *after* the apply lands (deferred)**, delivered to the applier over a **dedicated mpsc** (`set_propose_tx`), NOT the broadcast bus — a busy turn can lag-drop a bus event, which with a deferred ack would hang the agent's turn. So the tool result carries the real apply outcome (a failed apply no longer reads as success). `EvolutionProposed` still goes on the bus for the UI/event-log only.
- **Slint build step** — `.slint` files are compiled by `build.rs` at build time. If you change a `.slint` file but `cargo build` doesn't recompile, `touch ui-slint/build.rs`.
- **Pi Zero 2W rendering** — BCM2837 uses `vc4` not `v3d`. Set `SLINT_BACKEND=linuxkms-femtovg` for software rendering; no GPU required.
- **Emoji render monochrome, not colour — femtovg can't draw colour-glyph fonts.** ui-slint compiles only the **femtovg** + software renderers (Skia is too heavy for the Nano-first tier ladder — `cargo tree -p ui-slint` shows no skia). femtovg rasterizes glyph **outlines only** — no COLR/CBDT/sbix — so a colour-bitmap font ("Noto Color Emoji") comes out as tofu. Slint 1.16 selects fonts via **parley + fontique**, and fontique's Linux fallback goes through the real **fontconfig** lib (honours `/etc/fonts`). Fix (shipped): install.sh installs the bundled OFL **monochrome** `deploy/fonts/NotoEmoji-mono.ttf` to `/usr/local/share/fonts/apexos-rs/`, and ui-slint's `ensure_mono_emoji_fontconfig()` (runs before `AppWindow::new()`) writes a per-process `FONTCONFIG_FILE` that `<include>`s the system config then `<rejectfont>`s "Noto Color Emoji" — so fallback lands on the mono outline font **for ui-slint only** (the rest of the machine keeps colour emoji). Kaomoji (ツ via Noto CJK) and many symbols (Noto Sans Symbols2) already render. Colour emoji would need the Skia renderer — **measured +5 MB** on x86 (30→36 MB; the linker prunes ~23 MB of the 28 MB `libskia.a`), GL-only so Nano stays femtovg; deferred to a Pro/Standard-tier opt-in build. Verify a font resolves with `fc-match emoji` / `FONTCONFIG_FILE=~/.cache/apexos-rs/fonts.conf fc-match emoji`.
- **agentd must be running** — the UI will retry the WS connection on disconnect. In dev, agentd can be on a remote Pi; just set `AGENTD_WS`.
- **Session replay** — send `{"type": "session_init", "session_id": 42}` to restore a prior session. agentd replays the full message history.
- **A plain `Rectangle` is not a layout** — children are absolutely positioned and it does **not** report their size upward. A `for`-row built on a bare `Rectangle` collapses to ~0 height and rows draw on top of each other. Use a `VerticalLayout`/`HorizontalLayout` for any row that must size to its content.
- **No key auto-repeat on linuxkms** — `i-slint-backend-linuxkms` dispatches one `KeyPressed`/`KeyReleased` per libinput event with no repeat synthesis (libinput doesn't repeat; a compositor normally does). Holding a key = one hop on the Pi. Works on desktop (winit). Backend limitation — not fixable in app code without forking the backend.
- **No mouse-wheel scroll on linuxkms** — same backend, same class of gap: `calloop_backend/input.rs` translates only pointer `Motion`/`MotionAbsolute`/`Button` libinput events; scroll-axis events hit the `_ => {}` arm and are dropped. The wheel does **nothing** on the Pi kiosk (works on desktop winit). Fix is app-side: **every scrollable view uses a std-widgets `ScrollView`** (draggable scrollbar — pointer drag *is* delivered), never a bare `Flickable` (no visible affordance, unscrollable on the kiosk). Pattern: `ScrollView { horizontal-scrollbar-policy: ScrollBarPolicy.always-off; viewport-width: self.visible-width; viewport-height: <col>.preferred-height; … }`. Auto-scroll views (chat/terminal/council) keep the scroll-tick by setting `sv.viewport-y = min(0px, -(sv.viewport-height - sv.visible-height))` and bottom-anchor sparse content with `viewport-height: max(<col>.preferred-height, sv.visible-height)`.
- **Sonus playback = `ffmpeg -f alsa` + the `audio` group + a real ALSA device** — server-side playback (`POST /api/sonus/play`) decodes with `ffmpeg -f alsa <dev>` (not ffplay: SDL routes to the ALSA `default`, which on a **Pi 5 is a nonexistent card 0 — no analog jack, HDMI only**). Three things must hold: (1) the `agentd` user is in the **`audio`** group (`usermod -aG audio agentd` — install.sh does this; restart agentd to pick it up); (2) `SONUS_AUDIO_DEVICE` points at a real card in `/etc/agentd/env` (Pi 5 HDMI-0 = `plughw:1,0`; find cards with `aplay -l`); (3) the HDMI display actually has speakers. Same `audio`-group requirement applies to `/api/speak` TTS (`aplay`). One track at a time (process-global child); a new play or `/api/sonus/stop` kills the previous.
- **Camera eyes = the `video` group + a backend tool** — `camera_capture` (and `/api/snapshot`) shell out: Pi CSI needs `rpicam-apps` (install.sh adds it on Pi), USB/laptop needs `ffmpeg` (always installed) reading `/dev/video*`. The `agentd` user must be in the **`video`** group (install.sh now grants it even on headless nodes; restart agentd to pick it up) or `/dev/video*` opens fail with permission denied. First V4L2 frame is often dark — the tool grabs warmup frames (`ffmpeg -frames:v 5 -update 1`, `rpicam -t 1200`, `fswebcam -S 8`); a stubborn webcam can be pinned via `APEXOS_CAMERA_DEVICE` or fully overridden with `APEXOS_CAMERA_CMD`. Auto-detect tries Pi CSI first, then `/dev/video*` in order; "no camera" returns a note, not an error.
- **Sensor head (BME688 + MLX90640) = an EXTERNAL Python SensorHead service, NOT `apex-sensor-bridge`.** The bridge reads only CPU temp (`/sys/class/thermal`) directly; for air-quality + thermal it **HTTP-polls** a separate SensorHead dashboard (`buckster123/SensorHead`, Python/FastAPI on `:8080`) via `SENSORHEAD_URL` (unset → CPU temp only, never errors). So `-RS` never opens `/dev/i2c` — the bridge sandbox is `PrivateDevices=true`. Bringing a sensor node up takes three things (all verified live on apex1): **(1) I2C** — `dtparam=i2c_arm=on` **plus** `i2c-dev` at boot (on Pi 5 the dtparam alone leaves **no `/dev/i2c-*`** until `i2c-dev` loads). **install.sh now auto-provisions this** (`dtparam` + `/etc/modules-load.d/i2c-dev.conf` + `i2c-tools`) when the sensor head is selected on a Pi — *needs a reboot to take effect*; **(2) the dashboard** (the installer does NOT bundle this — it's an external repo) — venv install `fastapi uvicorn numpy pillow smbus2 adafruit-blinka adafruit-circuitpython-mlx90640 adafruit-circuitpython-bme680` (Pi-5 build deps: `python3-dev` for RPi.GPIO, `swig` for lgpio), run as a user in the **`i2c`** group (no sudo); **no BSEC2 needed** — the dashboard's `adafruit_bme680` fallback gives T/RH/P/gas (just no IAQ/CO₂eq; the prebuilt BSEC2 egg can be dropped in for full IAQ); **(3)** `SENSORHEAD_URL=http://localhost:8080` on the bridge (systemd drop-in, also not auto-wired). Symptom when missing: `sensor-bridge` is "active" but only emits `cpu_thermal`. **Stuck-pixel guard:** a dead/saturated MLX90640 pixel pins at the sensor's ~300°C range ceiling and fired false *autonomous* thermal-hotspot alerts; agentd's sensor-alert loop now suppresses (+ logs) the hotspot alert when `max_c ≥ SENSOR_THERMAL_MAX_VALID` (default 150°C — far above any real surface, far below 300). True per-pixel masking/interpolation/coordinate-blacklisting belongs upstream in the SensorHead service (it owns the 32×24 grid; the bridge forwards only scalar `min/max/mean_c`).
- **Mesh discovery is symmetric — every node must ADVERTISE, not just browse.** agentd only *browses* (`avahi-browse -rpt _apexos._tcp` in the discovery loop + `/api/mesh/nodes`); the *publish* half is a static avahi service file (`deploy/avahi/apexos-rs.service` → `/etc/avahi/services/apexos-rs.service`, installed by install.sh, which also adds `avahi-daemon avahi-utils`). Without it a node browses an empty mesh forever — the original "no devices, REFRESH does nothing" bug. avahi watches `/etc/avahi/services/` live, so `systemctl reload avahi-daemon` (not restart) picks up the file. `%h` → hostname → `parse_avahi_output` reads it back as node_id (field 6); IP is field 7; port hardcoded 8787. mDNS is link-local — both nodes must share the same L2 segment. Already-deployed nodes need `apexos-update` (re-runs install.sh) to gain advertisement.
- **Cross-node a2a needs BOTH a per-peer token AND a LAN bind.** `send_to_agent(node=…)` proxies to the peer's token-gated `POST /api/sessions/{id}/message`, so two things must hold on the *target*: (1) peers.toml has that peer's `token` (its `AGENTD_TOKEN`) — stored per-peer (`PeerRecord.token`, 0600 file), sent as `Authorization: Bearer` via reqwest (never curl argv); GET `/api/mesh/peers` **redacts** it to `has_token`. (2) the target binds the LAN, not loopback — agentd **defaults to `127.0.0.1:8787`**, so set `AGENTD_BIND=0.0.0.0:8787` in `/etc/agentd/env` + restart. The token is exactly what makes the non-loopback bind safe (F036). Discovery (mDNS/UDP) works on loopback-bound nodes, which masks the gap — a peer ADDs fine but delivery fails with a connection error until the bind is opened. No token stored → `send_to_agent` returns `detail: "no token stored…"`; wrong token → `401`. The easy way to supply the token is the **pairing code** (no typing): the peer's Mesh app shows a 6-digit code (`PAIR` → `/api/mesh/pair/start`, single-use, 5-min, 5-guess lockout); the other node redeems it (`+ ADD` → enter code → `/api/mesh/pair/redeem` → `/api/mesh/pair/claim`) and **both nodes store each other with tokens** in one exchange.
- **`apexos-update` does NOT rebuild ui-slint on `NO_UI` (headless/desktop) nodes.** install.sh skips the UI build there, so on a desktop node where you run `ui-slint` manually (winit), `apexos-update` refreshes agentd but leaves the UI binary stale — UI-side changes won't appear until you `cargo build --release -p ui-slint` and relaunch. Symptom: a just-shipped UI feature "doesn't work" after update while agentd is current. (Kiosk nodes are fine — install.sh builds + hot-swaps `apexos-rs-ui` there.)
- **Verify a deploy by the *running commit*, not "I ran `apexos-update`".** `apexos-update` is `curl .../main/install.sh | bash` (pull → rebuild → reinstall). A PR merged *after* the update ran (or a clone that didn't advance) simply isn't deployed. Worse for **deploy-only PRs** (touch `deploy/` + `install.sh`, no `agentd/` source): even a correct pull rebuilds a **byte-identical** binary, so nothing *looks* changed — and a new install.sh block only runs if the clone actually advanced to that commit. Always confirm the live commit via `/var/lib/agentd/update/health.json` `.commit` (the `build.rs`-embedded SHA, slice 1) and check the actual artifact landed (unit present? file there?). (Hit live 2026-06-19: apex2 was at slice-1 `28f89d1` while `main` was slice-2 `957be56`; the watchdog units were absent after an "update".)
- **`apexos-update` is idempotent — the resolved deployment shape is persisted, not re-detected.** `apexos-update` re-runs install.sh as a no-flag, no-USB `curl|bash`, which has no keyboard and would otherwise **re-auto-detect** mode/tier every run — flipping a deliberately-headless Pi into kiosk (builds + enables `apexos-rs-ui` as root) and reverting a manually-enabled sensor head. Fix: install.sh writes the resolved `APEXOS_MODE`/`TIER`/`NO_UI`/`NO_SENSOR`/`NO_CEREBRO_API`/`VOICE` to **`/etc/agentd/install.conf`** (`write_install_conf`, root-owned 0644, non-secret) at the end of a successful install, and **`load_persisted_config` restores it before auto-detect** on every run. Precedence is `*_CLI`-marker-gated: **CLI flag > freshly-plugged USB `apexos.conf` > install.conf > auto-detect** (a deliberate re-provision still wins; a stored choice always beats a guess). `plugins.toml` is now **seed-if-absent** too (like `policy.toml`/`soul.md`/`peers.toml`), so a re-run can't clobber self-evolved `register_mcp_server` entries — at the cost that a *new built-in* plugin won't reach an already-deployed node until merged. To intentionally change a node's shape: pass a CLI flag, drop a fresh `apexos.conf` on USB, or delete `/etc/agentd/install.conf`.
- **soul.md is identity ONLY — never list hardware or tools in it.** agentd generates a live `## Current embodiment` block (node tier, senses camera/thermal/GPIO, memory mode, mesh peers, and the **actual tool registry**) and appends it to soul.md when building the system prompt each turn (`build_embodiment` in agentd `main.rs`, refreshed every 30s; `TurnEngine` holds soul + embodiment separately so `read_soul_md`/`update_system_prompt` touch only the soul). A hardcoded tool list in soul.md is what made APEX "unable to see" tools that were actually loaded — the live block can't go stale. soul.md is identity (portable across the mesh, evolves at runtime); the embodiment block is the body it currently inhabits. The block also appends an **"Extensions on hand"** hint (EDK, `docs/edk.md`): it reads `config/parts/inventory.toml` (path `AGENTD_PARTS_INVENTORY`, deployed to `/etc/agentd/parts/inventory.toml`) and lists on-hand parts that would grant a capability this node *lacks* (cheap built-in probe — camera/thermal — never runs a part's `detect` shell) and whose `compat` matches this board (so x86/desktop shows nothing). The buyable universe is intentionally NOT injected — APEX web-searches it on demand — keeping the hint a short pointer, not noise. APEX files a hardware request via `propose_evolution {kind:"request_hardware"}` (`EvolutionProposal::RequestHardware`) — the one evolution that **can't auto-apply**: `apply_evolution` appends it to the hardware wishlist (`AGENTD_HARDWARE_WISHLIST`, deployed `/var/lib/agentd/hardware-wishlist.md`), a human seats the part, and the next-boot embodiment probe flips the sense ✗→✓ (`compute_undo` = `None`).
- **Prompt caching keys off a byte-stable system prefix — keep per-turn-volatile text OUT of it.** The Anthropic provider sends the system prompt as a single `cache_control:{ephemeral}` block (`build_body`, `anthropic.rs`), so — because render order is `tools → system → messages` — **tools + system cache together** and read at ~0.1× input price on every turn after the first. Tools are **name-sorted** there so a runtime `register_mcp_server` reorder (`gather_tools` flattens a HashMap) can't bust the position-0 tools prefix. The catch: anything that changes per-turn *inside* that prefix invalidates the whole cache. So the agent's live clock (Now + uptime) lives **outside** the embodiment block — `build_ambient_clock()` builds it and `turn.rs::inject_ambient` appends it to the latest user turn **ephemerally** (never persisted → can't bloat the JSONL or appear on replay), landing after the cached prefix at zero breakpoint cost. **Never put a timestamp, uptime, per-request id, or any per-turn-varying text into soul.md / the embodiment block / CCBS priming** — it silently kills caching (tell: response `usage.cache_read_input_tokens` stuck near zero; agentd logs `[anthropic] prompt cache: read=… write=… uncached=…` per turn — watch `read` climb). OpenAI/Ollama auto-cache by prefix and benefit from the same stable-prefix discipline; the injected clock is provider-agnostic so it reaches them too. **Conversation caching too:** `apply_conversation_cache` (`anthropic.rs`) rolls up to **3** more breakpoints back through the **stable** history (everything but the clock-bearing current turn) so a long transcript caches incrementally — newest stable turn always (the rolling read), anchors every ~15 blocks back to stay under Anthropic's 20-block lookback, total ≤4 with the system breakpoint. This is the dominant win on 1M giga-sessions (the unbounded `uncached` history shrinks to ~0.1×). All tunable at runtime via `CacheConfig` (`apexos_agent::cache`, an `Arc<RwLock>` shared through `RoutingProvider`/`AnthropicProvider`): `enabled`, `cache_conversation`, `ttl` (5m default vs 1h — see the `AGENTD_CACHE*` env vars; 1h survives >5-min human pauses without re-writing the whole transcript). `enabled=false` reverts to the exact pre-caching shape (plain-string system, no markers). **Runtime knobs:** the Settings window's **PROMPT CACHE** section and `GET`/`POST /api/cache` (the gateway holds the *same* arc, wired in `main.rs` before `GatewayState`) retune `enabled`/`cache_conversation`/`ttl` live — effective on the next turn, no restart.
- **The face has two control layers — activity (automatic) + emote (agent-driven).** `FaceView` reads `face-state` + `face-gaze` + `face-intensity`. Activity states (idle/thinking/speaking/listening/alert/sleeping) are set by Rust from the event stream. Emotive states (neutral/happy/curious/amused/confused/sad/surprised/wink/skeptical/proud/love/focused) come from APEX calling the **`display_face`** tool (apexos-tools, already `allow` in policy.toml) — the UI consumes the `tool_requested` event directly (no new agentd event, no tool card shown) and sets the face. An agent emote is **held past turn-end** (`FACE_HELD` in `main.rs`; restored by `face_rest`) so the expression lingers until the user's next prompt (`clear_face_hold`). The tool's GC9A01A socket write is best-effort/back-compat; on -RS the Slint UI is the renderer. The 2D Slint face is the universal renderer (all tiers, the Nano fallback). **Phase 2** — a raymarched-SDF GL face driven by this same emote layer — is the **default on GL tiers** (Pi 4/5 + x86 via `set_rendering_notifier` + femtovg `NativeOpenGL`): it auto-enables wherever a real `NativeOpenGL` context exists and **silently falls back to the 2D face** when one doesn't (notifier errors / no GL → nothing drawn → 2D shows); `APEX_FACE_GL=0` forces 2D everywhere. Scissored to the Face-window rect, emote uniforms mirrored from `FaceView` via the `FaceGl` global. On-Pi V3D validation is the remaining gate. (Dev: `APEX_FACE_AUTOOPEN=1` opens the Face window at launch, `APEX_FACE_STATE=<emote>` previews an expression without agentd.)

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
| `docs/edk.md` | Evolutionary Development Kit — **agent-facing** manual for self-extension: the three evolutions (identity·competence·**morphology**), the embodiment gradient, the request-to-incarnate loop, the "yolo if set" autonomy ladder. Paired with the on-hand parts inventory in `config/parts/inventory.toml` (buyable universe is web-searched, never listed) |
| `docs/app-parity.md` | Bringing original ApexOS apps to -RS — parity matrix, build tiers, AI⇄app symbiosis contract, and the "how to add an app" recipe |
| `docs/agent-identity.md` | Agent Identity charter — identity is system-**stamped** not agent-supplied: enforced per-session `agent_id` (Cerebro space + soul + policy + skin), the user↔agent↔skin boot flow, the auth-weight open decision, the 3-slice arc |
| `docs/occipital.md` | Occipital integration — the agent's reading cortex (standalone sibling repo): registering `occipital-mcp` (web_search/fetch/recall), the deploy + activation paths, and the ui-slint follow-along reader window |
| `docs/self-update.md` | Daemon self-update loop (mk3) — DESIGN: the agent rewriting its own core binary safely. The recoverability invariant, the privilege-boundary watchdog, the health contract, Cerebro-as-recovery, the failure-mode table, and the implementation slices |

---

## Deferred / post-v1

- ~~**Shader/3D face — embodiment Phase 2**~~ — **shipped & live on the Pi 5 V3D.** Phase 1 (emote *control*, #51): APEX drives its face via the `display_face` tool (12 expressions + gaze + intensity, held past turn-end — see the face-two-layers gotcha). Phase 2 (GL *render*) is **default on GL tiers** (#52 spike → 4 slices, #54–#58): renders in-window via `Window::set_rendering_notifier` + femtovg `GraphicsAPI::NativeOpenGL` + `glow` (no Skia, no 2nd window/process); auto-on wherever a real GL context exists, 2D fallback otherwise (Nano), `APEX_FACE_GL=0` to force 2D. The arc: **(1)** `glScissor` to the Face-window rect (published via the `FaceGl` global, sampled by a Timer); **(2)** emote uniforms mirrored from `FaceView` (GL + 2D can't drift); **(3)** raymarched-SDF head (lit ellipsoid + nose, ink features on the true 3D normal, head-turn on gaze); **(4)** promoted to default (auto-detect) + redraw gated on a visible Face window. Then three expressiveness rounds: **glossy catchlight eyes + shaped brows (angry-⋀/worried-⋁) + blush** (#59), **motion** — talking lip-flap, head-tilt, blink, idle saccades, sad tear (#60), and **facial muscles** — Duchenne smiling-eye crinkle, lower-lid squint, teeth/tongue in the open mouth, dark-accent ambient lift (#61). Verify any change via the snapshot server (`take_snapshot()` captures the GL overlay) — see the procedure in cerebro + docs/slint-notes.md. *Remaining flourishes (optional): ✨ sparkle for proud, eye-waggle micro-motions.*
- ~~PTY terminal~~ — shipped (libc `openpty`, `/terminal-ws` WebSocket endpoint in agentd gateway)
- ~~Sketchpad~~ — shipped (`sketchpad_view`: `Path`-stroke canvas + `POST /api/sketch` tiny-skia raster + `sketch_snapshot` tool + line/rect/ellipse shape tools)
- ~~Cerebro web UI integration~~ — shipped as the `Web` launcher (🌐): external-browser tiles for Cerebro/Sensor Head + open-any-URL bar (Slint can't embed a webview; opens via `xdg-open`/`$BROWSER`)
- Monaco / code editor — SSH/vim or embedded webkit2gtk webview for soul.md heavy editing
- Sub-agent windows — `Popup` per child session, maps to `SubAgentStarted` events
- ~~`apexos-core` vendor for shared `Event` types~~ — **DONE (both slices)**: wire-protocol types live in a lean serde-only `apexos-protocol` crate (`core` re-exports it, so `apexos_core::Event` is unchanged daemon-side). The UI now deserializes WS frames into the typed `Event` (`serde_json::from_value::<Event>` → `match event { … }`) instead of `["field"].as_str()` string-matching, and **logs** an undecodable frame instead of silently dropping it. Outbound frontend-intent frames (`user_prompt`/`user_approval`/`user_cancel`) stay hand-built JSON on purpose — they omit `session` (the gateway injects it), which the required-`session` `Event` variants can't express
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
