<div align="center">

<img src="assets/banner.png" alt="ApexOS-RS" width="100%">

# ApexOS-RS

### An AI agent that *lives on the hardware in your drawer* — remembers, evolves, and rewrites its own code.

*One ~32 MB Rust binary. No Chromium. No Electron. No cloud required.*
*Pi Zero to DGX. Persistent memory, a face, senses, a mesh — and a daemon that can safely recompile and reincarnate itself.*

[![status](https://img.shields.io/badge/status-alive_on_hardware-22c55e?style=for-the-badge)]()
[![rust](https://img.shields.io/badge/100%25-Rust-orange?style=for-the-badge&logo=rust)](https://www.rust-lang.org/)
[![ui](https://img.shields.io/badge/UI-Slint_·_KMS%2FDRM-8b5cf6?style=for-the-badge)](https://slint.dev/)
[![self--updating](https://img.shields.io/badge/self--updating-binary_+_rollback-e11d48?style=for-the-badge)]()
[![license](https://img.shields.io/badge/license-Apache--2.0-0891b2?style=for-the-badge)](#license)

</div>

> [!NOTE]
> **What if your AI assistant wasn't a chat window to someone else's datacenter — but a *resident* of a $15 computer you own?** One that keeps its own memories across reboots, has a face on a little screen, reads its sensors, talks to other nodes in your house, edits its own personality, and — when it has a better idea for how it should work — **rebuilds and swaps its own binary, with an automatic safety net if the new version misbehaves.** That's ApexOS-RS.

---

## 🧠 What is this?

ApexOS-RS is a **self-contained AI agent operating system** — the agent daemon, a cognitive memory system, system/sensor/vision tools, and a native GPU-rendered UI, all in **one pure-Rust Cargo workspace**. `cargo build --release --workspace`, one `install.sh`, and a spare board becomes a persistent, embodied, self-improving agent.

It's the pure-Rust distro of [ApexOS](https://github.com/buckster123/ApexOS): where the original runs a Chromium kiosk, ApexOS-RS renders natively to KMS/DRM in a single ~32 MB binary that boots to UI in ~200 ms. But the headline isn't the diet — it's what the agent can *do* when it lives entirely on local hardware.

```mermaid
flowchart LR
    you([🧑 You]) <--> agentd
    subgraph node["🖥️ One spare board — fully offline-capable"]
        agentd["🤖 agentd<br/>the agent daemon"]
        cerebro["🧠 Cerebro<br/>persistent memory"]
        tools["🛠️ tools<br/>system · vision · voice · GPIO"]
        ui["😊 native UI + face<br/>Slint · KMS/DRM"]
        agentd <--> cerebro
        agentd <--> tools
        agentd <--> ui
    end
    agentd <-..->|mesh / a2a| other([🌐 other nodes<br/>+ GPU inference])
    style node fill:#0b1020,stroke:#8b5cf6,color:#e5e7eb
```

---

## ✨ What makes it different

|  |  |
|--|--|
| 🪶 **Tiny & native** | One self-contained **~32 MB** binary. No browser, no Node, no Wayland compositor. Boots to UI in ~200 ms, ~160 MB RSS on a Pi 5. |
| 🧠 **It remembers** | **Cerebro** — a cognitive memory cortex (FTS5 + optional semantic embeddings) that survives reboots. The agent wakes up *oriented*: where it left off, its skills, its intentions. It even **consolidates memory nightly** while idle, no prompting. |
| 🧬 **It evolves** | The agent proposes and applies changes to **its own identity (`soul.md`), its policy, and its plugins** at runtime — every change reversible. Skills grow in memory under selection pressure. It can even **request new hardware** when it wants a capability it lacks. |
| 🔄 **It rewrites itself** | The frontier piece: the daemon can **rebuild and hot-swap its own binary** from a committed git ref — gated by *build → test → an adversarial LLM review* — while a privileged watchdog health-checks the new process and **automatically rolls back to the last good binary** if it doesn't come up healthy. Proven on real hardware. *(See ↓ [The self-update loop](#-the-self-update-loop).)* |
| 🤖 **It has a body** | An expressive **GPU-rendered face** (12 emotions, gaze, blinks), reads **air-quality + thermal sensors**, **sees** through a camera, **hears + speaks** (mic/TTS), and drives **GPIO**. Embodiment scales with the hardware actually present. |
| 🌐 **It's a mesh** | Multiple nodes discover each other (mDNS), message agent-to-agent, and **delegate inference** — a GPU box joins the network and serves big models to the whole cluster, no restart needed. |
| 🔒 **It's yours** | Runs **fully offline** with a local model (Ollama) or against an API — your call. Memories, soul, and data stay on *your* device. No telemetry. |

---

## 📸 Gallery

> 🚧 *Screenshots populate `assets/screenshots/` — chat, the live face, the sensor heatmap, the dashboard, the terminal.*

<div align="center">
<table>
  <tr>
    <td><img src="assets/screenshots/chat.png" width="420" alt="Agent chat + tool cards"></td>
    <td><img src="assets/screenshots/face.png" width="420" alt="Expressive GL face"></td>
  </tr>
  <tr>
    <td align="center"><b>Streaming chat + inline tool approvals</b></td>
    <td align="center"><b>The agent's live, GPU-rendered face</b></td>
  </tr>
  <tr>
    <td><img src="assets/screenshots/sensors.png" width="420" alt="IAQ + thermal heatmap"></td>
    <td><img src="assets/screenshots/dashboard.png" width="420" alt="Home dashboard"></td>
  </tr>
  <tr>
    <td align="center"><b>Air-quality history + MLX90640 thermal heatmap</b></td>
    <td align="center"><b>Home dashboard — CPU/RAM/disk, air quality</b></td>
  </tr>
</table>
</div>

---

## 🔄 The self-update loop

The capability we know of nowhere else in the open: **an agent that safely modifies its own substrate.** The agent edits its source, commits it, and calls `apply_daemon_update` — every gate runs *while the live daemon keeps serving*, and **every failure path ends with a known-good binary running.**

```mermaid
flowchart TD
    A["🤖 agent edits + commits its own source"] --> B{{"apply_daemon_update(commit)"}}
    B --> C["1 · build — cargo build --release"]
    C --> D["2 · test — cargo test"]
    D --> E["3 · review — a fresh LLM vets the diff:<br/>could this brick boot / health / rollback?"]
    E -->|any gate fails| X["↩︎ abort — live daemon untouched"]
    E -->|all pass| F["4 · file request.json<br/>(agent can't touch /usr/local/bin — non-root)"]
    F --> G["🛡️ root watchdog: back up → swap → restart"]
    G --> H{"new binary healthy<br/>within the probe window?"}
    H -->|yes| OK["✅ confirmed — running the new self"]
    H -->|no / crashes| RB["↩︎ auto-rollback to agentd.prev<br/>node stays serving"]
    G -.->|crashes minutes later| PROB["🧯 probation guard → rollback"]
    style OK fill:#064e3b,stroke:#22c55e,color:#d1fae5
    style RB fill:#7f1d1d,stroke:#ef4444,color:#fee2e2
    style X fill:#374151,stroke:#9ca3af,color:#e5e7eb
```

**Why it's safe:** agentd runs non-root under `ProtectSystem=strict` — it physically *cannot* overwrite its own binary or escalate. It only ever *writes a request file*; a separate root systemd watchdog does the swap and owns the rollback. So even a buggy or compromised agent can't brick the node. The invariant isn't "never ship a bad build" — it's **"never fail to recover from one, automatically, with no human at the board."** → [`docs/self-update.md`](docs/self-update.md)

Pair this with a locally-trainable model on a GPU/DGX tier and a weights-level nursery, and the same safety pattern extends from *the binary* to *the mind* — the road to genuine offline recursive self-improvement. That's the horizon; the binary loop is shipped and battle-tested today.

---

## 🧬 How it lives

ApexOS-RS isn't prompted into being clever — its cognition is wired into the daemon. The agent boots oriented, acts, remembers, consolidates, and evolves, in a loop that runs without anyone watching.

```mermaid
flowchart LR
    boot(["🌅 boot / wake"]) --> prime["cognitive_bootstrap<br/>recall: where I left off,<br/>skills, intentions"]
    prime --> act["💬 act<br/>chat · tools · sensors · mesh"]
    act --> remember["🧠 store memories,<br/>episodes, procedures"]
    remember --> act
    act --> evolve["🧬 propose_evolution<br/>soul · policy · skills · hardware"]
    evolve -. reversible .-> act
    act --> selfupd["🔄 apply_daemon_update<br/>rewrite the binary"]
    selfupd -. watchdog net .-> boot
    night(["🌙 nightly, idle"]) --> dream["💤 dream_run<br/>consolidate + prune memory"]
    dream --> remember
    style selfupd fill:#3b0764,stroke:#e11d48,color:#fbcfe8
```

| Layer | Surface | Reversible? |
|------|---------|-------------|
| **Identity** | `soul.md` (who it is) | ✅ in-process undo |
| **Behaviour** | policy / plugins | ✅ |
| **Competence** | skills in Cerebro (graded, champion-selected) | additive |
| **Morphology** | hardware requests (a human seats the part) | human-gated |
| **Substrate** | **the agentd binary itself** | watchdog rollback |

---

## 🏗️ Architecture

```mermaid
flowchart TB
    subgraph ws["📦 One Cargo workspace"]
        agentd["agentd<br/>agent loop · gateway · evolution · self-update"]
        cerebro["cerebro-mcp<br/>cognitive memory"]
        tools["apexos-tools<br/>fs · git · vision · audio · gpio"]
        bridge["apex-sensor-bridge<br/>BME688 + MLX90640"]
        ui["apexos-rs-ui<br/>Slint + KMS/DRM"]
    end
    agentd <-->|MCP / stdio| cerebro
    agentd <-->|MCP / stdio| tools
    bridge -->|sensor events| agentd
    agentd <-->|"Event stream · ws://:8787/ws"| ui
    agentd <-->|same WS| browser([🌐 Browser / PWA])
    ui -->|renders| display([🖥️ HDMI / KMS])
```

The Slint UI and any browser/PWA speak the **same** WebSocket protocol to agentd — the daemon is headless-pure, the display is optional. → [`docs/architecture.md`](docs/architecture.md)

---

## 🖥️ Runs on what you already have

Same binaries everywhere — the *tier* is just environment, no per-device builds. Pi 5 16 GB boards cost $300+ now; the real fleet is the hardware in your drawer.

| Tier | Hardware | RAM | Renderer | Memory | LLM |
|------|----------|-----|----------|--------|-----|
| **Nano** | Pi Zero 2W, any 512 MB board | 512 MB | `linuxkms-femtovg` (software) | FTS5 only (~23 MB) | API only |
| **Micro** | Pi 4 1–2 GB, older ARM64 | 1–2 GB | `linuxkms` | `bge-small` (~275 MB) | API or small local |
| **Standard** | Pi 5, x86 mini-PC, M1 Mini | 4–8 GB | `linuxkms` / `winit` | `bge-small` | Ollama 7–13B |
| **Pro** | x86 + GPU (CUDA/ROCm/Metal) | 8 GB+ | `winit` | `bge-large` + GPU | Ollama 30–70B local |
| **Titan** | DGX Spark / Station | 128 GB+ | headless | GPU-accelerated | 70B+, serves the mesh |

**Modes** (orthogonal to tier): **Kiosk** (Pi + HDMI, native UI) · **Headless** (server/laptop/DGX — browser + PWA only) · **Desktop** (x86 windowed). `install.sh` auto-detects and asks.

<details>
<summary><b>🌐 Mesh inference — let a GPU node carry the cluster</b></summary>

```mermaid
flowchart LR
    subgraph nano["Pi Zero 2W · Nano"]
        a1["agentd"]
    end
    subgraph micro["Pi 4 · Micro"]
        a2["agentd"]
        c2["Cerebro (FTS5)"]
    end
    subgraph gpu["old RTX box · Pro"]
        a3["agentd"]
        ol["Ollama 30–70B"]
        c3["Cerebro + GPU embeds"]
    end
    a1 -->|"hot-swap backend → gpu:11434"| ol
    a2 -->|send_to_agent| a3
    c2 -->|dream_run delegated| c3
```

mDNS discovery + per-peer tokens. A GPU node joins; Nano/Micro nodes point their inference backend at it at runtime (`POST /api/backend`, no restart). The GPU node can even run nightly memory consolidation for the whole cluster.
</details>

---

## 🚀 Install

```bash
# Fresh device (Pi or x86) — auto-detects tier + mode:
curl -fsSL https://raw.githubusercontent.com/buckster123/ApexOS-RS/main/install.sh | sudo bash
```

```bash
sudo bash install.sh --no-ui          # headless / server node (no display)
sudo bash install.sh --tier=nano      # Pi Zero 2W — embeddings off
sudo bash install.sh --api-key=sk-... # set Anthropic key non-interactively
```

> [!WARNING]
> The one-liner pipes a script into `sudo bash` — you're trusting GitHub's TLS/CDN. Review [`install.sh`](install.sh) first if your threat model needs it. **Always build on the Pi, never cross-compile** (Cortex-A76 / arm64).

---

<details>
<summary><h2>🔬 For the nerds & manual installers (click to expand)</h2></summary>

### The crates

```
agentd/crates/    # agent daemon — core · gateway · plugins · agent · store · agentd
cerebro/crates/   # cognitive memory — cerebro lib · cerebro-mcp · cerebro-api · cerebro-cli
tools/crates/     # system plugins — apexos-tools · apex-sensor-bridge
ui-slint/         # the native Slint UI (the unique contribution of this repo)
config/           # default plugins.toml, policy.toml
deploy/           # systemd units + the self-update watchdog
install.sh        # one-shot installer
```

### Capabilities at a glance

- **Cerebro** — 60+ cognitive-memory tools: store/recall, episodes, procedures (graded + champion-selected), intentions, schemas, associative graph, audit trail, nightly `dream_run` consolidation, `cognitive_bootstrap` boot-priming.
- **apexos-tools** — 30+ tools: workspace-confined filesystem, **git** (commit your own source for self-update), `run_command`, `http_fetch` (SSRF-guarded), camera capture, screenshot mirror, sketchpad, audio DSP, GPIO, `display_face`.
- **Occipital** — a web "reading cortex": `web_search` / `web_fetch` / `web_recall` with a follow-along reader window.
- **Self-evolution (EDK)** — `propose_evolution` over soul / policy / plugins (reversible), skill grading, and the *request-to-incarnate* hardware loop.
- **Self-update** — `apply_daemon_update` + a root systemd watchdog with health-gated rollback + probation. → [`docs/self-update.md`](docs/self-update.md)
- **Multi-agent identity** — per-session agent binding: distinct Cerebro space, soul, policy, and skin per agent on one node.

### Why pure Rust beats the Chromium build

| | ApexOS (original) | ApexOS-RS |
|--|--|--|
| UI runtime | Chromium | Slint native |
| UI footprint | hundreds of MB + runtime | one ~32 MB binary |
| UI memory | ~300 MB+ | ~160 MB RSS (Pi 5) |
| Startup | ~5 s (cage + Chromium) | ~200 ms |
| Display stack | cage → Wayland → Chromium | KMS/DRM direct |
| Language | Rust + HTML/JS | 100% Rust |

### Hardware compatibility

| Board | RAM | Tier | Notes |
|-------|-----|------|-------|
| Raspberry Pi 5 (8/4 GB) | plenty | Standard/Pro | primary deploy target |
| Raspberry Pi 4 (4/2/1 GB) | fine→tight | Standard→Micro | BCM2711, `v3d` driver |
| Raspberry Pi Zero 2W | 512 MB | Nano | `linuxkms-femtovg` + FTS5 |
| x86 mini-PC (no GPU) | 4–16 GB | Standard | Ollama 7–13B |
| x86 + NVIDIA / AMD GPU | 8 GB+ | Pro | CUDA / ROCm ORT, full-VRAM Ollama |
| Apple Silicon (M1/M2/M3) | 8–96 GB | Pro | CoreML ORT, Ollama Metal |
| DGX Spark / Station | 128 GB+ | Titan | arm64 — same binary as the Pi |

### Docs

| File | Contents |
|------|----------|
| [`docs/architecture.md`](docs/architecture.md) | component graph, thread model, KMS/DRM, agentd protocol |
| [`docs/self-update.md`](docs/self-update.md) | the daemon self-update loop — gates, watchdog, rollback, probation |
| [`docs/evolutionary-layer.md`](docs/evolutionary-layer.md) | exo-evolution: competence grows in memory, not the weights |
| [`docs/edk.md`](docs/edk.md) | Evolutionary Development Kit — identity · competence · morphology |
| [`docs/agent-identity.md`](docs/agent-identity.md) | system-stamped per-agent identity |
| [`docs/symbiosis.md`](docs/symbiosis.md) | the runtime cognitive architecture |
| [`docs/slint-notes.md`](docs/slint-notes.md) | Slint patterns, Pi GPU setup, gotchas |
| [`CLAUDE.md`](CLAUDE.md) | the living project blueprint + build status |

### Build & deploy

```bash
cargo build --release --workspace      # one build, whole stack (build on the Pi)
cargo test --workspace --exclude ui-slint
sudo cp target/release/<bin> /usr/local/bin/ && sudo systemctl restart <svc>
apexos-update                          # pull → rebuild → hot-swap → restart
```

</details>

---

## 🤝 Relationship to ApexOS

ApexOS-RS is a **distro, not a replacement.** Canonical [ApexOS](https://github.com/buckster123/ApexOS) stays Chromium-based — richest feature set (Monaco IDE, iframe embeds), best on a Pi 5. ApexOS-RS optimizes for footprint, hardware range, a 100%-Rust stack, and the self-evolution / self-update frontier. **Both share the same `agentd` backend — you choose the frontend.**

## License

[Apache-2.0](LICENSE) © [buckster123](https://github.com/buckster123) — keeps your copyright + change notices attached as the code spreads (built to be forked).

<div align="center"><sub>Built in the open. An agent that remembers, evolves, and rewrites itself — on the hardware you already own.</sub></div>
