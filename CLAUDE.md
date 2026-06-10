# ApexOS-RS — Agent & Developer Guide

> Pure-Rust native UI distro of ApexOS. Slint frontend + KMS/DRM direct rendering.
> Replaces Chromium kiosk with a single ~10 MB binary. agentd is unchanged.

See also: [docs/architecture.md](docs/architecture.md) | [docs/build-roadmap.md](docs/build-roadmap.md) | [docs/slint-notes.md](docs/slint-notes.md)

Reference runtime: `../ApexOS` (Rust — **do NOT modify** during this port).

---

## What this is

ApexOS-RS is a **thin WebSocket renderer** that connects to the same `ws://localhost:8787/ws`
endpoint as the browser UI, consuming the same Event JSON stream and sending the same Intent JSON.
It is not a fork of agentd. It does not import agentd as a Rust library.

```
agentd (UNCHANGED) ──── ws://localhost:8787/ws ──┬──→ Browser (any device)
                                                  │
                                              apexos-rs-ui
                                           (Slint + KMS/DRM)
                                           renders to /dev/tty7
```

Single crate workspace (for now):

```
ui-slint/
  src/
    main.rs               # runtime + WS client loop + event dispatch
    ui/appwindow.slint    # root Slint window + all components
  Cargo.toml
```

---

## Locked decisions

- **Language**: Rust
- **UI framework**: Slint (`.slint` declarative, compiles to native GL)
- **Rendering**: `SLINT_BACKEND=linuxkms` on Pi (KMS/DRM, no Wayland, no cage)
- **Thread model**: tokio on background threads, Slint event loop owns main thread — **never** `#[tokio::main]`
- **Cross-thread UI**: `slint::invoke_from_event_loop()` only — never touch UI handles from tokio tasks directly
- **agentd dependency**: none — protocol is stable JSON over WS, no shared Rust types needed
- **Memory target**: ~10 MB RSS at idle (no GPU buffers counted)
- **Pi Zero 2W support**: `SLINT_BACKEND=linuxkms-femtovg` (software renderer, ~7 MB)

---

## Pi 5 target

| Detail | Value |
|--------|-------|
| SSH | `ssh apexos@192.168.0.158` (LAN only, pw: `abnudc1337`) |
| OS | Debian trixie headless |
| Binary | `/usr/local/bin/apexos-rs-ui` |
| Service | `/etc/systemd/system/apexos-rs-ui.service` (from `deploy/apexos-rs-ui.service`) |
| agentd WS | `ws://localhost:8787/ws` |

**Always build on Pi — never cross-compile.** Pi is Cortex-A76 (arm64).

---

## Deploy workflow

```bash
# 1. Dev machine
cargo test
git add -p && git commit -m "short imperative description"
git push

# 2. On Pi
cd ~/ApexOS-RS
git pull
cargo build --release -p ui-slint

# 3. Hot-swap
sudo systemctl stop apexos-rs-ui
sudo cp target/release/ui-slint /usr/local/bin/apexos-rs-ui
sudo systemctl start apexos-rs-ui
sudo journalctl -u apexos-rs-ui -n 20 --no-pager
```

On Pi the binary is not yet running as a service during early development — run directly:
```bash
AGENTD_WS=ws://localhost:8787/ws SLINT_BACKEND=linuxkms ./target/release/ui-slint
```

---

## Dev on desktop (x86)

No Pi needed for steps 1–9. Connect to the Pi's agentd over LAN:

```bash
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
| 1 | Agent chat | Streaming text view, dark theme, send input | ⬜ |
| 2 | Tool call blocks | Collapsible cards, inline approval buttons | ⬜ |
| 3 | Home dashboard | CPU/RAM/disk bars, IAQ badge (`/api/run` poll) | ⬜ |
| 4 | Sensor window | IAQ stats + thermal heatmap (custom painter) | ⬜ |
| 5 | Session management | Session init, picker, history replay | ⬜ |
| 6 | Voice controls | Mic → `/api/record/start`, speaker → `/api/speak` | ⬜ |
| 7 | Settings | Soul.md editor (`TextEdit`), policy mode, plugin list | ⬜ |
| 8 | Power + model/policy | Power modal, model/policy `ComboBox` | ⬜ |
| 9 | KMS/DRM deploy | `SLINT_BACKEND=linuxkms`, systemd service, retire cage | ⬜ |

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

Key inbound events:

| Event | Fields | Action |
|-------|--------|--------|
| `agent_text` | `delta: string` | append to text buffer |
| `turn_started` | — | clear buffer, set busy |
| `turn_complete` | — | clear busy, TTS if enabled |
| `tool_requested` | `call_id, name, input` | push tool block (status=running) |
| `tool_result` | `call_id, output` | update tool block by call_id |
| `approval_pending` | `call_id, name` | show approve/reject buttons |
| `sensor_reading` | `variant, data` | update IAQ / thermal state |
| `wake_triggered` | — | flash wake indicator |

Send user message:
```json
{"type": "user_prompt", "text": "hello"}
```
Send approval:
```json
{"type": "user_approval", "call_id": "abc", "approved": true}
```

Full event list: `../ApexOS/agentd/crates/core/src/types.rs` — `Event` enum.

---

## Environment variables

| Var | Default | Purpose |
|-----|---------|---------|
| `AGENTD_WS` | `ws://localhost:8787/ws` | agentd WebSocket URL |
| `SLINT_BACKEND` | auto | `winit` (desktop), `linuxkms` (Pi), `linuxkms-femtovg` (Pi Zero) |
| `SLINT_FULLSCREEN` | unset | `1` = fullscreen, no window chrome |
| `RUST_LOG` | `info` | tracing filter |

---

## Gotchas

- **Never `#[tokio::main]`** — Slint requires the main thread. `#[tokio::main]` hijacks it. Build the runtime manually with `Builder::new_multi_thread()`.
- **`invoke_from_event_loop` is fire-and-forget** — it queues a closure and returns immediately. The closure runs asynchronously on the Slint thread. Do not assume immediate effect.
- **Slint strings are `SharedString`** — convert with `.into()`. Never pass a `&str` or `String` directly where Slint expects `SharedString`.
- **Pi KMS groups** — `agentd` user needs `render`, `video`, `input` groups: `sudo usermod -aG render,video,input agentd`. Only done once.
- **`text file busy`** — always `systemctl stop apexos-rs-ui` before `cp`. A running binary cannot be overwritten.
- **`fontconfig` missing on Pi** — `sudo apt-get install -y libfontconfig1-dev` if build fails.
- **Slint build step** — `.slint` files are compiled by `build.rs` at build time. If you change a `.slint` file but `cargo build` doesn't recompile, `touch ui-slint/build.rs`.
- **Pi Zero 2W rendering** — BCM2837 uses `vc4` not `v3d`. Set `SLINT_BACKEND=linuxkms-femtovg` for software rendering; no GPU required.
- **agentd must be running** — the UI will retry the WS connection on disconnect. In dev, agentd can be on a remote Pi; just set `AGENTD_WS`.
- **Session replay** — send `{"type": "session_init", "session_id": 42}` to restore a prior session. agentd replays the full message history.

---

## Git discipline

- **Gate passes → commit immediately.** Each build-order step = at minimum one commit.
- **Commit format:** imperative, lowercase. `implement agent chat streaming view`
- **Push after every commit.**
- **Never amend a pushed commit. Never force-push.**
- **Docs travel with code.** Update CLAUDE.md + relevant docs/ file in the same commit.

---

## Cerebro agent

All Cerebro MCP calls in this project use agent `FORGE` (agent_id=`"FORGE"`, ⚒, #B7410E).

## Cerebro session protocol (mandatory)

**Session START** — always call `session_recall` before diving in:
```
session_recall(query="ApexOS-RS Slint UI build status step progress", agent_id="FORGE")
```

**Session END** — always call `session_save` plus supporting saves:
```
session_save(session_summary="...", key_discoveries=[...], unfinished_business=[...], agent_id="FORGE", priority="HIGH")
```
Then as needed:
- `store_procedure` — Slint patterns, Pi gotchas, WS protocol quirks
- `store_intention` — next steps / deferred work (salience 0.8–0.95)
- `episode_start` / `episode_add_step` / `episode_end` — multi-step implementation sequences

---

## Deferred / post-v1

- PTY terminal — `alacritty_terminal` crate, replaces xterm.js
- Monaco / code editor — SSH/vim or embedded webkit2gtk webview for soul.md heavy editing
- Sub-agent windows — `Popup` per child session, maps to `SubAgentStarted` events
- Sketchpad — Slint custom painter, post-v1 complexity
- Cerebro web UI integration — iframe not possible in Slint; link opens in external browser
- `apexos-core` vendor — optionally vendor agentd's core crate for shared `Event` types (avoids JSON string matching), blocked on agentd publishing it as a library crate

---

## Meta — when to update this file

- A locked decision changes → update `## Locked decisions`
- A build-order step completes → tick it in the table
- A Pi gotcha is discovered → add to `## Gotchas`
- A deferred item resolves → move it out of `## Deferred`
- Keep this file under ~120 lines of content (excluding this Meta section)
