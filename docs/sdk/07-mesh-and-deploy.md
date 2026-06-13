# SDK 07 — Mesh Colony & Deployment

> **What this surface is.** Everything about *where* ApexOS-RS runs and *how nodes find each
> other*: the hardware-tier / deployment-mode matrix that decides which components install on a
> box, the mDNS-discovered mesh of `agentd` nodes that route work to each other, the vast.ai
> GPU-rental recipe model that hot-swaps the inference backend for the whole colony, and the
> systemd hardening contract every service must follow.
>
> **When you'd extend it.** Adding a new hardware tier or deployment mode; adding a mesh node and
> making it route cross-node A2A traffic; defining a new vast.ai GPU recipe; or shipping a new
> systemd-managed binary alongside `agentd`. For an *agent* extending this at runtime, this is the
> surface that lets APEX provision a second board (`bootstrap_node`), rent a GPU (`vast_launch`),
> or message an agent on another node (`send_to_agent --node`).

---

## Concepts

The mesh is **not** a clustering framework. There is no leader election, no shared state, no RPC
mesh. It is three loosely-coupled mechanisms layered on top of independent `agentd` daemons:

1. **Discovery (mDNS / avahi).** Each node advertises `_apexos._tcp` on the LAN. Every `agentd`
   runs a polling loop that `avahi-browse`s for peers and emits a `PeerSeen` event when a new one
   shows up. Discovery is *informational* — it never auto-joins anything unless you opt in.

2. **The peer registry (`peers.toml`).** The durable list of nodes this daemon will route to.
   Discovery surfaces candidates; the registry is the committed set. Cross-node messaging reads
   it; the gateway REST API and the `list_mesh_peers` virtual tool read/write it.

3. **Cross-node A2A.** `send_to_agent` with a `node` arg looks up the peer's `ws_url` in
   `peers.toml`, derives the HTTP base, and `POST`s to that peer's
   `/api/sessions/{id}/message` — which simply emits a `UserPrompt` on the remote bus. That is the
   entire wire: one fire-and-forget HTTP POST. No streaming back, no result.

Orthogonal to the mesh are two *single-node* axes that `install.sh` decides at provision time:

- **Hardware tier** (`nano` / `micro` / `standard` / `pro`, plus the aspirational `titan` for
  DGX-class arm64) — gates the **Cerebro embedding model**, i.e. memory RSS and search quality.
- **Deployment mode** (`kiosk` / `headless` / `desktop`) — gates whether `apexos-rs-ui` installs.

And **vast.ai** is a fourth, runtime mechanism: a *single* node rents a cloud GPU, opens an SSH
tunnel to its `llama-server`, and hot-swaps its own inference backend to point at it. Other mesh
nodes then reach that model by being routed at the renting node (`send_to_agent --node`).

### Named types & files (file:line truth)

| Thing | Where |
|-------|-------|
| Peer registry type, `peers.toml` (de)serialize, avahi line parser | `agentd/crates/gateway/src/mesh.rs` — `PeerRegistry` :47, `PeerRecord` :29, `PeerRole` :7, `save()` :89, `parse_avahi_output` :105 |
| Mesh REST routes | `agentd/crates/gateway/src/lib.rs` — routes :135-153; `mesh_nodes_handler` :1453, `mesh_peers_get/post/delete` :1489-1545, `session_message_handler` :712, `active_sessions_handler` :693 |
| Discovery loop (mDNS poll, subnet guard, auto-bootstrap) | `agentd/crates/agentd/src/main.rs` — `spawn_discovery_loop` :1699, `local_subnet_prefix` :1686, started :409 |
| Cross-node `send_to_agent`, `list_mesh_peers`, `bootstrap_node` virtual tools | `agentd/crates/plugins/src/supervisor.rs` — `send_to_agent` :557, `list_mesh_peers` :640, `bootstrap_node` :658, `find_peer_ws_url` :1548 |
| Tool specs (schemas shown to the LLM) | `agentd/crates/agentd/src/main.rs` — `send_to_agent_spec` :1473, `list_mesh_peers_spec` :1502, `bootstrap_node_spec` :1515, `vast_*_spec` :1551-1599; registered in `gather_tools` :1194 |
| vast.ai recipe types, state, CLI wrapper | `agentd/crates/plugins/src/vast.rs` — `RecipeFile`/`GpuTier`/`Recipe` :7-41, `load_recipes` :43, `VastState`/`VastInstance`/`VastPhase` :64-138, `vastai()` :143 |
| vast lifecycle (`vast_launch` etc.) | `agentd/crates/plugins/src/supervisor.rs` — `vast_list_recipes` :804, `vast_status` :842, `vast_launch` :884, `vast_destroy` :1255 |
| Backend hot-swap on `VastInstanceReady`/`Destroyed` | `agentd/crates/agentd/src/main.rs` :373-406; live backend route `/api/backend` in `gateway/src/lib.rs` :128, `set_backend_handler` :529 |
| Install-time tier/mode detection & embed-model gating | `install.sh` — tier detect :359-377, mode detect :367-368, `NO_UI` gating :430, embed model :692-700, peers.toml seed :713-715, env/token :727-746, service install/enable :760-781 |
| systemd hardening template | `deploy/agentd.service` (jailed daemon), `deploy/apexos-rs-ui.service` (root + device allowlist), `deploy/cerebro-api.service`, `deploy/apex-sensor-bridge.service` |

> **Reality check — three install gaps.** `install.sh` does **not** install `avahi-daemon`
> (discovery needs it), does **not** install `sshpass` (`bootstrap_node` needs it), and does
> **not** create `recipes.toml` (vast needs it at `/etc/agentd/recipes.toml`). All three are
> manual prerequisites today. Ground any "it just works" claim against this.

---

## Add a new hardware tier

Tiers gate the Cerebro embedding model (and, by convention, the local-LLM story). The selection
logic is a single `case` in `install.sh`.

1. **Add the detection branch.** `install.sh` :359 maps RAM to a tier:
   ```bash
   if   (( RAM_MB <  768 )); then TIER="nano"
   elif (( RAM_MB < 2048 )); then TIER="micro"
   elif (( RAM_MB < 8192 )); then TIER="standard"
   else                           TIER="pro"
   fi
   ```
   Insert your branch (e.g. detect a CUDA/arm64 DGX for `titan`). Tiers can also be forced with
   `--tier=NAME`, `APEXOS_TIER=NAME` in a boot file, or the manual whiptail picker.

2. **Add a description** in the `case "$TIER"` block at :372 (`TIER_DESC=...`). This is shown in
   the install summary; keep it one line.

3. **Map it to an embedding model** at :692:
   ```bash
   case "$TIER" in
     micro|standard) EMBED_MODEL="BAAI/bge-small-en-v1.5" ;;
     pro)            EMBED_MODEL="BAAI/bge-large-en-v1.5" ;;
   esac
   ```
   `nano` is intentionally absent → empty `EMBED_MODEL` → `CEREBRO_EMBED_MODEL=""` → ~23 MB RSS,
   FTS5-only search. A non-empty value is written into `/etc/agentd/plugins.toml` at :700.

4. **(Optional) gate the LLM default.** Tiers don't currently change `AGENTD_BACKEND`; if you want
   a tier to default to a local Ollama backend, set `AGENTD_BACKEND`/`AGENTD_MODEL`/
   `AGENTD_OAI_BASE_URL` in the env file (`write_env_key` at :731). The daemon reads these at
   `main.rs` :96-104.

There is **no Rust code** behind a tier — it is purely an install-time knob over the embed model
and env defaults. Build UI/agent features for the Nano floor (no fast inference, embedding may be
off); faster tiers get the same behaviour, just quicker.

## Add a new deployment mode

Modes gate which binaries install. The logic is `install.sh` :367 + :430.

1. **Add the auto-detect branch** at :367 (`$IS_PI && MODE="kiosk" || MODE="headless"`), and a
   picker entry in the manual menu at :423.
2. **Decide component gating.** The one rule today is :430:
   ```bash
   [[ "$MODE" == "headless" || "$MODE" == "desktop" ]] && NO_UI=true
   ```
   `NO_UI=true` skips installing/enabling `apexos-rs-ui` (:582, :773, :779). Add your mode's
   gating here. Other gates: `NO_SENSOR` (sensor bridge), `NO_CEREBRO_API` (REST dashboard).
3. A mode is just a label that flips `NO_*` booleans. agentd itself is mode-agnostic — it is a
   pure daemon; headless = "don't install the local display."

## Add a mesh node (and route to it)

Two ways: **manual** (you provision the box yourself) or **agent-driven** (`bootstrap_node`).

### Manual

1. **Provision the new box** by running `install.sh` on it (or `curl … | sudo bash`). It will
   come up as an independent `agentd`. Give it a stable identity with `APEX_NODE_ID` (defaults to
   `hostname`, `main.rs` :200).
2. **Install + start avahi on both nodes** so they advertise/see `_apexos._tcp` (NOT done by
   `install.sh`): `sudo apt-get install -y avahi-daemon && sudo systemctl enable --now avahi-daemon`.
   You must *also* publish the service — the simplest is an `avahi` static service file or
   `avahi-publish -s "ApexOS $(hostname)" _apexos._tcp 8787` on each node.
3. **Register the peer** on the node that will route to it. Either let discovery surface it (watch
   for `[mesh] new peer discovered` and a `PeerSeen` event) and then commit it, or POST directly:
   ```bash
   curl -fsS -X POST "http://NODE_A:8787/api/mesh/peers?token=$AGENTD_TOKEN" \
     -H 'Content-Type: application/json' \
     -d '{"node_id":"apex-garage","ws_url":"ws://192.168.0.201:8787","role":"full"}'
   ```
   This writes a `[[peer]]` block into `/etc/agentd/peers.toml` (`PeerRegistry::add` → `save`,
   `mesh.rs` :67/:89) and emits `PeerRegistered`.
4. **Route to it.** From an agent on NODE_A:
   ```json
   {"tool":"send_to_agent","args":{"node":"apex-garage","session_id":0,"message":"recall today's IAQ trend"}}
   ```
   `find_peer_ws_url("apex-garage")` (supervisor :1548) reads `peers.toml`, converts
   `ws://…` → `http://…`, and POSTs to `http://192.168.0.201:8787/api/sessions/0/message`. Session
   `0` is the remote node's root session. Fire-and-forget — no result comes back.
   **Caveat:** the field-name mismatch above means this POST currently lands a `missing message`
   error (reported as a false `sent`); until fixed, do the POST yourself with `{"message": …}`.

### Agent-driven (`bootstrap_node`)

`bootstrap_node` SSHes to a fresh box, clones the repo, and runs `install.sh` in the background.

1. **Prereq on the calling node:** `apt-get install -y sshpass` (not done by `install.sh`).
2. Call the tool (target must be SSH-reachable with a password):
   ```json
   {"tool":"bootstrap_node","args":{
     "target_ip":"192.168.0.205","ssh_password":"…","ssh_user":"apexos",
     "api_key":"sk-ant-…"}}
   ```
   It connectivity-checks, skips if `agentd` is already active, installs git, clones
   `repo_url` (default `https://github.com/buckster123/ApexOS.git`), and `nohup`s `install.sh`
   (supervisor :658-797). Returns immediately; install takes ~15-20 min. The node appears in the
   mesh once *its* avahi is up — so the same avahi prereq applies to the new box.

### Discovery loop knobs (env on the routing node)

| Var | Default | Effect (`main.rs` :1704) |
|-----|---------|--------------------------|
| `MESH_DISCOVERY_INTERVAL` | `60` | seconds between `avahi-browse` scans |
| `MESH_SUBNET_GUARD` | on | only consider peers on the same `/24` (`local_subnet_prefix` :1686) |
| `MESH_AUTO_BOOTSTRAP` | off | when set, a newly-seen peer injects a `UserPrompt` into root session suggesting the agent call `bootstrap_node` |
| `APEX_NODE_ID` | `hostname` | this node's mesh identity |
| `PEERS_TOML` | `/etc/agentd/peers.toml` | registry path (also read by supervisor + `list_mesh_peers`) |

## Add a vast.ai GPU recipe

A recipe is a row in `/etc/agentd/recipes.toml` mapping a name → GPU tier + model + llama-server
params. No Rust change needed — `load_recipes()` (`vast.rs` :43) reads the file at call time.

1. **Create `recipes.toml`** (install.sh does *not*). Minimal shape (mirrors `RecipeFile`,
   `vast.rs` :7-41):
   ```toml
   [docker]
   prebuilt = "your/llama-server-image:tag"   # must expose /health and /v1 on :8000, run /app/launch.sh

   [gpu_tiers.rtx5090]
   vast_names  = ["RTX_5090"]   # matched against vast offers as gpu_name=<n>
   label       = "RTX 5090 32GB"
   max_price   = "0.80"          # dph_total ceiling, string
   min_disk_gb = 60
   vram_gb     = 32

   [[recipes]]
   name        = "qwen36-27b-q6-5090"   # the handle vast_launch takes
   label       = "Qwen3 27B Q6"
   gpu         = "rtx5090"               # must match a [gpu_tiers.*] key
   model_repo  = "Qwen/Qwen3-27B-GGUF"
   model_quant = "Q6_K"
   ctx         = 32768
   parallel    = 2
   kv_type     = "q8_0"
   description = "Balanced reasoning model for the colony"
   ```
2. **Set `VAST_API_KEY`** in `/etc/agentd/env` (the `vastai()` wrapper requires it, :143) and
   install the `vastai` CLI on the node.
3. **Use it.** `vast_list_recipes` → pick a name → `vast_launch {"recipe":"qwen36-27b-q6-5090"}`.
   The launch flow (supervisor :884) searches offers in the geo (`VAST_DEFAULT_GEO`, default
   `EU_NORDIC`), creates the instance, opens an SSH `-L {VAST_LOCAL_PORT|8000}:127.0.0.1:8000`
   tunnel, polls `/health` (≤20 min), then emits `VastInstanceReady` → `main.rs` :386 hot-swaps
   `backend → "ollama"` and `oai_base_url → http://127.0.0.1:<port>/v1`. `vast_destroy` tears it
   all down and reverts the backend (:392).

Geo filters are hard-coded in the launch flow (supervisor :996): `EU_NORDIC`, `EU`, `US`, or
anything else = no filter. To add a geo, extend that `match`.

## Ship a new systemd service (the hardening contract)

`deploy/agentd.service` is the template for any **daemon** you add. The contract:

1. **Run as the `agentd` system user, not root.** `User=agentd / Group=agentd`. Root is reserved
   for `apexos-rs-ui` only, and *only* because seatless KMS needs DRM master.
2. **Apply the sandbox** verbatim from `agentd.service`:
   ```ini
   NoNewPrivileges=true
   ProtectSystem=strict
   ProtectHome=true
   PrivateTmp=true
   ReadWritePaths=/var/lib/agentd /etc/agentd
   WorkingDirectory=/var/lib/agentd/workspace
   ```
   This sandbox — not the tool denylist — is the real confinement boundary. `apexos-tools` is
   otherwise unconfined; the systemd jail is what stops a `run_command` from touching the system.
3. **Read secrets from the shared env file**, never inline:
   `EnvironmentFile=-/etc/agentd/env`. The `-` makes it optional (no crash if absent).
4. **`WantedBy=multi-user.target`** — Pi boots to `multi-user.target`, not `graphical.target`.
5. **Order after agentd if you depend on it:** `After=agentd.service` + `Requires=agentd.service`
   (see `cerebro-api.service`).
6. **Wire it into `install.sh`:** drop the unit in `deploy/`, then add `install_svc <name>` and
   `systemctl enable <name>` (and a `svc_start` health check) — `install.sh` :760-838. Gate it
   behind a `NO_*` boolean if it's mode-dependent.

If your service needs hardware (DRM, input, GPIO), do **not** drop the sandbox — use a device
allowlist like `apexos-rs-ui.service` (`DevicePolicy=closed` + explicit `DeviceAllow=` lines).

---

## Worked example: bring up a second node as an inference backend

Goal: a spare x86+GPU box (`apex-gpu`) joins the colony and the Pi (`apex-kitchen`) routes heavy
reasoning to it. Two valid topologies — pick one.

### Topology A — peer node runs a local model, Pi routes A2A to it

This uses the mesh proper: the GPU box is its own `agentd` with a local Ollama backend; the Pi
delegates via `send_to_agent`.

1. **Provision the GPU box.** On `apex-gpu`:
   ```bash
   curl -fsSL https://raw.githubusercontent.com/buckster123/ApexOS/main/install.sh \
     | sudo APEXOS_MODE=headless APEXOS_TIER=pro bash
   ```
   `headless` ⇒ `NO_UI=true` (no display installed); `pro` ⇒ bge-large embeddings. Then point its
   backend at a local Ollama model:
   ```bash
   curl -fsS -X POST "http://localhost:8787/api/backend?token=$AGENTD_TOKEN" \
     -H 'Content-Type: application/json' \
     -d '{"backend":"ollama","oai_base_url":"http://localhost:11434/v1","model":"qwen2.5:32b"}'
   ```

2. **Avahi on both** (the missing-prereq step):
   ```bash
   sudo apt-get install -y avahi-daemon avahi-utils
   sudo systemctl enable --now avahi-daemon
   avahi-publish -s "ApexOS $(hostname)" _apexos._tcp 8787 &   # or a static .service file
   ```

3. **Register the peer on the Pi:**
   ```bash
   curl -fsS -X POST "http://apex-kitchen:8787/api/mesh/peers?token=$AGENTD_TOKEN" \
     -H 'Content-Type: application/json' \
     -d '{"node_id":"apex-gpu","ws_url":"ws://192.168.0.210:8787","role":"full"}'
   ```
   (Or wait for the Pi's discovery loop to log `[mesh] new peer discovered: apex-gpu` and confirm
   with `GET /api/mesh/nodes` showing `"known": false`, then register.)

4. **Route from APEX on the Pi:**
   ```json
   {"tool":"send_to_agent","args":{"node":"apex-gpu","session_id":0,
     "message":"Deep-analyze the last 7 days of IAQ events and propose a ventilation schedule."}}
   ```
   The Pi POSTs to `http://192.168.0.210:8787/api/sessions/0/message`; `apex-gpu`'s root session
   runs the turn on its 32B local model. **Result does not stream back** — the GPU node would need
   to `send_to_agent` back to the Pi (it can: it would register `apex-kitchen` as *its* peer).

### Topology B — Pi rents the GPU on vast.ai and uses it directly

No second physical box; one node hot-swaps its own backend to a rented cloud GPU.

1. On the Pi, ensure prereqs: `VAST_API_KEY` in `/etc/agentd/env`, the `vastai` CLI installed, and
   a `/etc/agentd/recipes.toml` (see "Add a vast.ai GPU recipe").
2. APEX runs:
   ```json
   {"tool":"vast_list_recipes","args":{}}
   {"tool":"vast_launch","args":{"recipe":"qwen36-27b-q6-5090","geo":"EU_NORDIC"}}
   ```
3. `vast_launch` finds the cheapest reliable EU-Nordic offer, creates the instance, tunnels
   `127.0.0.1:8000`, waits for the model, then emits `VastInstanceReady` — the Pi's backend
   hot-swaps to `http://127.0.0.1:8000/v1` (`main.rs` :386). Every subsequent turn on the Pi (and
   any peer routed to it) now runs on the rented GPU. `vast_status` shows cost/hr; `vast_destroy`
   stops billing and reverts the backend.

**Verification.** `GET /api/mesh/peers` lists the peer (Topology A); `GET /api/backend` shows the
swapped `oai_base_url` (Topology B); `vast_status` reports `ready` with the instance. Watch
`journalctl -u agentd -f` for `[mesh] new peer discovered`, `[vast] model ready`, and
`[vast] hot-swapping backend`.

---

## Policy / safety

**Approval gating.** None of `bootstrap_node`, `send_to_agent`, `vast_launch`, `vast_destroy`,
`list_mesh_peers`, or `vast_status` appear in `config/policy.toml`. Unlisted tools default to
`Decision::Ask` (`policy.rs` :111). So in the default `suggest` mode (and `auto-edit`), **every
one of these gates on human approval** — only `yolo` mode bypasses (`policy.rs` :89). This is the
intended posture: provisioning a node, spending money on a GPU, and messaging another machine are
all "ask first" by default. If you add a mesh/vast tool and want it auto-allowed, you must add an
explicit `"tool_name" = "allow"` rule — and that itself should go through `propose_evolution`, not
a hand-edit.

**Cross-node trust is transitive and unauthenticated by default.** `send_to_agent --node` POSTs to
the peer's `/api/sessions/{id}/message`. That route is under the token gate, but the cross-node
caller uses bare `curl` with **no `Authorization` header** (supervisor :585-589). It therefore only
works if the peer's gateway is on loopback-reachable / token-less terms with the caller — i.e. it
works against a peer bound loopback-only via its own loopback, or one whose token gate you've
satisfied out-of-band. Treat the mesh as a **trusted LAN** primitive. The `MESH_SUBNET_GUARD`
(`/24`, on by default) is a containment measure, not authentication — it stops discovery from
reaching off-segment, nothing more.

**`bootstrap_node` handles secrets in process args.** It passes `ssh_password` and `api_key`
through shell command lines (`echo '<pw>' | sudo -S`, `export ANTHROPIC_API_KEY=<key>`,
supervisor :752/:770). These are visible in the target's process table during install and end up in
`/tmp/apex-install.log` patterns. Acceptable for first-boot LAN provisioning; do not use it across
untrusted networks.

**systemd sandbox is the real boundary.** A new bootstrapped node inherits the same jail
(`deploy/agentd.service`): `NoNewPrivileges`, `ProtectSystem=strict`, writes confined to
`/var/lib/agentd` + `/etc/agentd`. The agent-mutable config files (`soul.md`, `policy.toml`,
`plugins.toml`, `peers.toml`) are individually `chown agentd` so self-evolution can write them
(`install.sh` :724); `/etc/agentd` itself stays root-owned to protect the `600 root:root` env
token. Because the dir is root-owned, `peers.toml` writes are *in-place*, not atomic temp+rename —
the gateway's `PeerRegistry::save` uses temp+rename (`mesh.rs` :97), which **fails inside the
root-owned dir**; a concurrent reader could momentarily see a missing file. Single-writer in
practice, but don't assume atomicity.

**Network exposure.** A mesh only forms if `agentd` binds beyond loopback. `agentd` **hard-bails on
a non-loopback bind when `AGENTD_TOKEN` is unset** (`main.rs` bind/auth gate). So to mesh, set
`AGENTD_BIND=0.0.0.0:8787` *and* keep the generated token. Discovery/registry routes
(`/api/mesh/*`) are under the token gate.

**Audit discipline (for agents).** When you provision a node, rent a GPU, or register a peer, you
are changing the colony's shape and (for vast) spending money. Journal it:
`episode_start` around the operation, `memory_store` the node_id / instance_id / cost, and
`store_intention` to destroy a vast instance after use (it bills per hour and survives an `agentd`
restart via `instance.json`, `vast.rs` :108). Persisted vast state means a crashed daemon can
silently keep a GPU billing — always reconcile `vast_status` after a restart.

---

## Reference

### Hardware tiers (`install.sh` :359, :692; CLAUDE.md)

| Tier | RAM gate | `CEREBRO_EMBED_MODEL` | Cerebro RSS | LLM story |
|------|----------|-----------------------|-------------|-----------|
| `nano` | `< 768 MB` | `""` (none) | ~23 MB, FTS5-only | API only |
| `micro` | `< 2048 MB` | `BAAI/bge-small-en-v1.5` | ~275 MB | API or small local |
| `standard` | `< 8192 MB` | `BAAI/bge-small-en-v1.5` | ~275 MB | Ollama 7-13B |
| `pro` | `≥ 8192 MB` | `BAAI/bge-large-en-v1.5` | 500 MB+ | Ollama 30-70B (GPU) |
| `titan` | (aspirational, arm64 DGX) | bge-large | 500 MB+ | 70B+ served to mesh |

### Deployment modes (`install.sh` :367, :430)

| Mode | Auto-detect | Installs `apexos-rs-ui`? | Interface | `SLINT_BACKEND` |
|------|-------------|--------------------------|-----------|-----------------|
| `kiosk` | Pi | yes | local HDMI display | `linuxkms` (or `linuxkms-femtovg` on Pi Zero) |
| `headless` | non-Pi | no (`NO_UI=true`) | browser / PWA | — |
| `desktop` | manual | no (`NO_UI=true`) | native window | `winit` |

### Mesh REST API (`gateway/src/lib.rs` :135-153)

| Method + path | Body / params | Effect |
|---------------|---------------|--------|
| `GET /api/mesh/nodes` | — | run `avahi-browse`, list discovered `_apexos._tcp` peers + `known` flag |
| `GET /api/mesh/peers` | — | dump `peers.toml` contents |
| `POST /api/mesh/peers` | `{node_id, ws_url, role?}` | add/update a peer (`role`: `full`\|`sensor`\|`thin`), emit `PeerRegistered` |
| `DELETE /api/mesh/peers/{id}` | — | remove peer by `node_id` |
| `GET /api/sessions/active` | — | in-memory sessions (id + msg count) — pick a target for `send_to_agent` |
| `POST /api/sessions/{id}/message` | `{message}` *(`message` only — `text` is NOT accepted, see live bug below)* | inject a `UserPrompt` into session `id` — this is the A2A landing point |
| `GET/POST /api/backend` | `{backend, oai_base_url?, model?}` | read / hot-swap inference backend (no restart) |

> **Live bug — cross-node `send_to_agent` does not actually deliver.** The caller POSTs
> `{"text": <message>}` (supervisor :584), but `session_message_handler` reads **only**
> `body["message"]` with no `text` fallback (gateway :717) and returns
> `{"ok":false,"error":"missing message"}` with HTTP 200. The caller only inspects `curl -f`'s exit
> status, so a 200-with-error-body reports `status:"sent"` — a **false success**. Until the field
> names are reconciled (send `message`, or have the handler accept both), prefer a direct
> `POST /api/sessions/{id}/message` with `{"message": …}` for cross-node injection. Local
> `send_to_agent` (no `node`) is unaffected — it emits `AgentMessage` on the bus directly.

### Mesh / deploy virtual tools (specs in `main.rs` :1473-1599; impl in `supervisor.rs`)

| Tool | Required args | Optional args | Returns | Default policy |
|------|---------------|---------------|---------|----------------|
| `list_mesh_peers` | — | — | `peers.toml` text | Ask |
| `send_to_agent` | `session_id`, `message` | `node` (peer node_id) | `{status, msg_id}` (local) / `{status, node, target_session}` (remote) | Ask |
| `bootstrap_node` | `target_ip`, `ssh_password` | `ssh_user`(=`apexos`), `api_key`, `repo_url` | status string (returns before install finishes) | Ask |
| `vast_list_recipes` | — | — | JSON array of recipes | Ask |
| `vast_launch` | `recipe` | `geo`(=`EU_NORDIC`) | `{status:"ready", instance_id, model, cost_per_hr, local_port, …}` | Ask |
| `vast_destroy` | — | — | `{status:"destroyed", instance_id}` | Ask |
| `vast_status` | — | — | `{status, phase?, instance?}` | Ask |

### `peers.toml` schema (`mesh.rs` :29; `/etc/agentd/peers.toml`)

```toml
# ApexOS mesh peers — managed by agentd
[[peer]]
node_id = "apex-garage"
ws_url  = "ws://192.168.0.201:8787"
role    = "full"      # full | sensor | thin   (default: full)
status  = "online"    # free-form, default "online"
```

### `recipes.toml` schema (`vast.rs` :7-41; `/etc/agentd/recipes.toml`, NOT auto-created)

| Section | Fields |
|---------|--------|
| `[docker]` | `prebuilt` (image; must expose `/health` + `/v1` on :8000 via `/app/launch.sh`) |
| `[gpu_tiers.<key>]` | `vast_names` (`[String]`, → `gpu_name=` offer filter), `label`, `max_price` (string, dph ceiling), `min_disk_gb`, `vram_gb` |
| `[[recipes]]` | `name` (launch handle), `label`, `gpu` (→ a `gpu_tiers` key), `model_repo`, `model_quant`, `ctx`, `parallel`, `kv_type`, `description` |

### Vast events & phases (`vast.rs` :83; `main.rs` :386)

| Event | When | Backend effect |
|-------|------|----------------|
| `VastInstanceLaunched` | instance created | — |
| `VastInstanceReady` | `/health` passes | backend → `ollama`, `oai_base_url` → tunnel |
| `VastTunnelLost` | 3 keepalive fails | (logged; instance kept) |
| `VastInstanceDestroyed` | `vast_destroy` | backend reverts to `AGENTD_BACKEND` default |

Phases (`VastPhase`): `idle` → `launching{phase}` → `ready` → `destroying`.

### Discovery / mesh env vars

| Var | Default | Read at |
|-----|---------|---------|
| `APEX_NODE_ID` | `hostname` | `main.rs` :200 |
| `MESH_DISCOVERY_INTERVAL` | `60` (s) | `main.rs` :1704 |
| `MESH_SUBNET_GUARD` | on | `main.rs` :1707 |
| `MESH_AUTO_BOOTSTRAP` | off | `main.rs` :1706 |
| `PEERS_TOML` | `/etc/agentd/peers.toml` | `main.rs` :189, supervisor :644/:1554 |
| `RECIPES_TOML` | `/etc/agentd/recipes.toml` | `vast.rs` :44 |
| `VAST_API_KEY` | — (required for vast) | `vast.rs` :144 |
| `VAST_DEFAULT_GEO` | `EU_NORDIC` | supervisor :888 |
| `VAST_LOCAL_PORT` | `8000` | supervisor :964 |
| `AGENTD_BIND` | `127.0.0.1:8787` | `main.rs` bind gate (non-loopback ⇒ token required) |

### Manual prerequisites (NOT installed by `install.sh`)

| Need | For | Install |
|------|-----|---------|
| `avahi-daemon` + published `_apexos._tcp` | mDNS discovery | `apt install avahi-daemon avahi-utils` + publish service |
| `sshpass` | `bootstrap_node` | `apt install sshpass` |
| `/etc/agentd/recipes.toml` | vast recipes | author by hand (schema above) |
| `vastai` CLI + `VAST_API_KEY` | vast lifecycle | `pip install vastai`; key in `/etc/agentd/env` |
