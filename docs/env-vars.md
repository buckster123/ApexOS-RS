# Environment variables — the full reference

> Rebuilt 2026-08-02 from a three-scout code inventory (agentd crates ·
> cerebro+tools · ui/install/deploy) — every runtime variable, receipted at
> its read site. Dev-critical basics (`AGENTD_WS`, `SLINT_BACKEND`,
> `RUST_LOG`) are also summarized in CLAUDE.md.
>
> **The seed-only doctrine:** most agentd *behavior* knobs (backend/model,
> cache, voice, sensor profile) are seed-only env — a Settings/API choice
> persists to a config file that **wins on restart** (delete the file to
> return to env control). Rows below say "seed-only" where that applies.
> Kill-switch rows accept `0`/`false`/`off` unless noted — exact spellings
> vary and are quoted where they bite.

## Dev basics

| Var | Default | Purpose |
|-----|---------|---------|
| `AGENTD_WS` | `ws://localhost:8787/ws` | ui-slint: agentd WebSocket URL (terminal WS derives `/terminal-ws` from it). Kiosk unit sets it; the desktop launcher sets it inline |
| `SLINT_BACKEND` | auto | `winit` (desktop), `linuxkms` (Pi kiosk), `linuxkms-femtovg` (Pi Zero software render). Kiosk unit pins `linuxkms` |
| `SLINT_FULLSCREEN` | unset | `1` = fullscreen, no window chrome (simulates kiosk on desktop) |
| `RUST_LOG` | `info` (agentd) / `warn` (ui-slint, plugins) | tracing/log filter. Read by agentd, ui-slint, all three cerebro binaries, and seeded as `warn` into every `[plugin.env]` block by install.sh |
| `APEXOS_LANDLOCK` | on | `0`/`false`/`off` skips the tools-worker Landlock allowlist (finding 11 part 2). Inherited by `apexos-tools` via `plugin_child_env`. Leave unset on a live node |
| `APEXOS_NETNS` | on | `0`/`false`/`off` skips the fs-class empty netns (finding 11 Wave 28). Inherited. Leave unset on a live node |
| `APEXOS_TOOLS_CLASS` | unset (all tools, no netns) | `fs` / `net` / `dev` / `all`. Prefer `--class` on the plugin args (install.sh pins this). Unset keeps a single-process compat node working |
| `APEXOS_TOOLS_SPAWN` | unset | `stdio` / `child` / `1` forces the supervisor to spawn `cmd` even when `plugins.toml` says `transport = "unix"`. Dev/laptop only. A live node with unix transport **must not** fall back to spawning as `agentd` — that reopens the same-uid hole |
| *(tools unit env)* | `/etc/agentd/tools.env` | Non-secret knobs for the sibling tools units (workspace, log, USB dirs, camera/gpio, HTTP-fetch/EE flags). install.sh rewrites it. Never put `AGENTD_TOKEN` here |
| *(tools-net unit env)* | `/etc/agentd/tools-net.env` | Optional `TELEGRAM_*` / `NTFY_TOPIC` / `PIPER_MODEL` for `apexos-net` only (0640 `root:apexos-net`) |

## Auth, bind & core paths

| Var | Default | Purpose |
|-----|---------|---------|
| `AGENTD_TOKEN` | `""` (auth **disabled**, warns) | gateway bearer token (WS `?token=` + REST `Authorization: Bearer`). Empty + non-loopback `AGENTD_BIND` → the process **refuses to start**. Minted once by install.sh into `/etc/agentd/env` (never overwritten) and **mirrored into `/etc/agentd/ui.env`** (token + `AGENTD_WS` only — the kiosk unit must not load the full env, SA-13); ui-slint reads it for its own connections and re-exports it on login re-exec. cerebro-api uses the SAME secret with the same non-loopback interlock |
| `AGENTD_BIND` | `127.0.0.1:8787` (code) / `0.0.0.0:8787` (installer seed — LAN, token-gated) | gateway listen address; must parse as a SocketAddr (else fatal) |
| `AGENTD_WORKSPACE` | `/var/lib/agentd/workspace` | the agent workspace root — relative fs-tool paths resolve here, writes are hard-confined to it, USB sticks mount under `media/`. Empty string → default everywhere EXCEPT the policy engine, which fails **closed** (unset/empty → `Ask`); in tools, the per-call supervisor-stamped workspace wins over the env (per-agent subroots) |
| `AGENTD_LOG` | `events` (code) / `/var/lib/agentd/events` (unit) | event/session log dir — also roots `goals.json`, `workers.json`, `batches.json`, `mandalas.json`, `remote_workers.json`, `agents/` (evidence), `worktrees/` (mandala trees). apexos-tools appends `<AGENTD_LOG>/agents` to the read-roots so evidence files are readable |
| `AGENTD_READ_ROOTS` | unset | colon-separated extra **read-only** roots for the fs tools. Widening is read-only (writes stay workspace-confined) and the secret-path denylist still applies (`/etc/agentd/env`, `.ssh`, `*.api_key`, `/proc/*/environ`, …) |
| `AGENTD_GIT_ROOTS` | unset | colon-separated extra dirs the `git_*` tools may operate in, on top of the workspace. Seeded by the self-update provisioner to include the agentd-owned repo clone |
| `AGENTD_UI` | `ui` (code) / `/var/lib/agentd/ui` (unit) | static browser/PWA asset dir served at `:8787` (installer copies `web/*` there) |
| `AGENTD_IDENTITIES` | `/etc/agentd/identities.toml` | the multi-agent identity registry (`[[user]]` + `[[agent]]`) — drives `/api/identities`, login, per-agent souls. See `docs/agent-identity.md` |
| `AGENTD_SOUL` | `/etc/agentd/soul.md` | system-prompt (soul) file; missing/blank falls through to `AGENTD_SOUL_DEV`, then no soul. Evolution writes go to the `AGENTD_SOUL` path |
| `AGENTD_SOUL_DEV` | `config/soul.md` | dev fallback soul path (only consulted when the primary is missing) |
| `AGENTD_PLUGINS_TOML` | `config/plugins.toml` (code) / `/etc/agentd/plugins.toml` (unit) | plugin/MCP-server registry; hot-rewritten by `RegisterMcpServer` evolution proposals |
| `AGENTD_POLICY_TOML` | `config/policy.toml` (code) / `/etc/agentd/policy.toml` (unit) | approval-policy file (seed-if-absent + additive sync on `apexos-update`); load failure → policy defaults |
| `EE_WORKSPACE` | falls back to `AGENTD_WORKSPACE`, then `./data/workspace` | **enterprise feature only** — confinement root for the EE tool-gate (`docs/enterprise.md`) |
| `EE_TOOL_GATE_URL` | unset | **enterprise** — full URL of `POST` tool-gate; if set, overrides local shim |
| `EE_ADMIN_URL` | unset | **enterprise** — EE admin origin; gate URL becomes `{EE_ADMIN_URL}/api/agentd/tool-gate` |
| `EE_AGENTD_TOKEN` | unset | **enterprise** — bearer for the HTTP tool-gate sidecar |
| `EE_DEFAULT_ROLE` | `operator` | **enterprise** — role stamped into every gate eval (`admin` / `operator` / `user`) |
| `AGENTD_EE_CONNECTORS` | unset | When `1`/`true`, **deny** free-form `http_fetch` (prefer OpenAPI/connectors). See `docs/enterprise.md` |
| `AGENTD_HTTP_FETCH_MODE` | auto | `open` \| `allowlist` \| `deny` — overrides EE connector default for `http_fetch` |
| `AGENTD_HTTP_FETCH_ALLOWLIST` | empty | Comma hosts permitted when mode is allowlist (implies allowlist if MODE unset) |
| `AGENTD_KEY_FILE` | `/var/lib/agentd/.api_key` | Anthropic key persistence (0600), written by the UI; `ANTHROPIC_API_KEY` env wins over the file at boot |
| `AGENTD_OAI_KEY_FILE` | `/var/lib/agentd/.oai_api_key` | OAI/OpenRouter key persistence (0600); the env keys win at boot |
| `AGENTD_HARDWARE_WISHLIST` | `hardware-wishlist.md` (code) / `/var/lib/agentd/hardware-wishlist.md` (unit) | file the `RequestHardware` evolution proposal appends to (atomic) |
| `AGENTD_PARTS_INVENTORY` | `config/parts/inventory.toml` (code) / `/etc/agentd/parts/inventory.toml` (unit) | EDK on-hand parts inventory for the embodiment hint; missing → hint doesn't render |
| `APEX_NODE_ID` | `hostname`, else `apexos` | the node's **mesh** identity (distinct from the agent id) — task_fanout `node:` names, peer registry entries, provenance prefixes. Cached in a OnceLock: resolved once per process, runtime changes ignored |
| `PEERS_TOML` | `/etc/agentd/peers.toml` | mesh peer registry (node_id/ws_url/role/token per peer); created empty at boot if missing; live-persisted as peers join/leave |
| `HOME` / `PATH` | ambient | `HOME` reaches the web-terminal PTY child (default `/root`) and roots ui-slint's XDG fallbacks; `PATH` feeds the embodiment block's which-binary probes |

## Inference backend, keys & caching

| Var | Default | Purpose |
|-----|---------|---------|
| `AGENTD_BACKEND` | `anthropic` | LLM provider — `anthropic` \| `openrouter` \| `xai` \| `ollama` \| `vllm` \| `oai` (unknown values are forced back to `anthropic`). **Seed-only** — a Settings/`POST /api/backend` choice persists to `AGENTD_BACKEND_CONFIG` and wins on restart |
| `AGENTD_MODEL` | per-backend | model id. Unset → backend default (`xai` → `grok-4.5`). Seed-only; the boot-resolved value is what a vast hot-swap reverts to |
| `AGENTD_OAI_BASE_URL` | `http://localhost:11434/v1` | OpenAI-compat endpoint for the non-anthropic backends; switching to `openrouter` auto-pins `https://openrouter.ai/api/v1`, `xai` auto-pins `https://api.x.ai/v1`. Seed-only |
| `AGENTD_BACKEND_CONFIG` | `/var/lib/agentd/backend_config.json` | the persisted backend/model/URL selection (file-wins-on-restart; delete to return to env control) |
| `ANTHROPIC_API_KEY` | unset | Anthropic key; env wins over `AGENTD_KEY_FILE`. Also read by cerebro's dream engine and the Anthropic vision tier. Boot-file flag for install.sh (write-if-nonempty seed, existing key preserved + live-verified) |
| `OAI_API_KEY` | unset | generic OAI / vLLM / custom endpoint key → slot `oai` (file `AGENTD_OAI_KEY_FILE`, default `/var/lib/agentd/.oai_api_key`). Independent of openrouter/xai |
| `OPENROUTER_API_KEY` | unset | OpenRouter-only slot (file `AGENTD_OPENROUTER_KEY_FILE` → `.openrouter_api_key`). Coexists with xAI and generic OAI — **no first-wins chain** |
| `XAI_API_KEY` | unset | xAI/Grok LLM slot (file `AGENTD_XAI_KEY_FILE` → `.xai_api_key`). **agentd LLM only** — Imaginarium gen keeps its own copy in `/etc/imaginarium/env` (see `docs/xai-provider.md`) |
| `AGENTD_OAI_KEY_FILE` / `AGENTD_OPENROUTER_KEY_FILE` / `AGENTD_XAI_KEY_FILE` | see above | per-slot secret files (0600). `POST /api/keys` accepts `{oai}`, `{openrouter}`, `{xai}` independently; Settings saves the **active backend's** slot |
| `AGENTD_XAI_REASONING_EFFORT` | `low` | when model is `grok-4.5*` only: chat-completions `reasoning_effort` (`low`\|`medium`\|`high`). Server default is high; we pin low for agent tool-loops. Unknown values omit the field (API default). No effect on non-Grok models |
| `AGENTD_CACHE` | `1` | (Anthropic only) `0`/`false`/`off`/`no` disables prompt caching entirely. On = cache system+tools prefix + (by default) the conversation |
| `AGENTD_CACHE_CONVERSATION` | `1` | (Anthropic only) off = cache only the stable prefix, not the growing transcript (the big 1M-giga-session win when on). No effect when `AGENTD_CACHE=0` |
| `AGENTD_CACHE_TTL` | `1h` | (Anthropic only) cache TTL. `1h`/`1hr`/`hour`/`3600` → 1h (write premium 2×) — survives human pauses and wakeup gaps; every ApexOS mode is gappy, so this is the economical choice (field-measured 2026-07-25: at `5m`, human-paced sessions read ~0 from cache). **Any other set value → 5m** (write 1.25×) — unset keeps 1h, garbage yields 5m |
| `AGENTD_TOOL_RESULT_TIMEOUT_SECS` | `1800` | ceiling on waiting for a tool result — read independently by the turn engine and the MCP transport (one knob, two clocks) |
| `AGENTD_HISTORY_TOKEN_BUDGET` | `120000` | per-session in-memory history window (rough tokens, per-block-type calibrated ±20%). Soft ceiling with hysteresis: trim fires past 1.2×, cuts to 0.75×, oldest whole turns drop with an honest trim marker at the seam (`session_search` retrieves them from the on-disk JSONL — `docs/session-rag.md`). `0` disables trimming. Lower it for small-context local models. **Seed-only since `#328`** — the Settings HISTORY WINDOW control / `POST /api/history` persists a choice that wins on restart |
| `AGENTD_HISTORY_CONFIG` | `/var/lib/agentd/history_config.json` | the persisted history-budget selection (file-wins-on-restart; delete to return to env control). `GET /api/history` also reports per-session "window in use" estimates |
| `AGENTD_AMBIENT_GAP_SECS` | `600` | idle gap before the live clock (Now + uptime) is re-injected into a turn — temporal grounding without per-message noise |
| `VISION_MAX_EDGE` | `1024` | longest-edge px cap for images entering model context (the token-bomb shim), hard-clamped 128–4096 |

## Identity boot & memory priming

| Var | Default | Purpose |
|-----|---------|---------|
| `AGENTD_AGENT_ID` | `APEX` | the node's bound agent identity — **stamped** onto every Cerebro call (overriding the model), routes per-agent workspaces, and signs `git_commit` (`<id>@apexos.local`). Per-session binding via `hello{agent_id}`. See `docs/agent-identity.md` |
| `AGENTD_CCBS` | enabled | `0`/`false` disables CCBS boot-priming (`cognitive_bootstrap` injected on a session's first turn; result cached per session) |
| `AGENTD_BOOTSTRAP_MODE` | `standard` | CCBS token budget — `minimal` (1000) / `standard` (2000) / `full` (4500) |

## Dream & welfare

| Var | Default | Purpose |
|-----|---------|---------|
| `AGENTD_DREAM_CRON` | `0 0 3 * * *` | cron (6-field, UTC) for the nightly autonomous `dream_run`; **empty or unparseable disables it** |
| `AGENTD_DREAM_TIMEOUT_SECS` | `1800` (60s floor) | how long the nightly loop waits for `dream_run`. The dispatched dream runs to completion regardless — this is the caller's patience, and the digest push is gated on the result arriving |
| `AGENTD_DREAM_JOURNAL` | `1` | deposit the first-person dream journal (`dream-journal` memory + "Last dream" wake-priming section + `<log_dir>/last_dream_journal.txt`). `0` disables |
| `COLONY_DREAM_DIGEST` | `1` | push the dream's newly-born schemas/consolidations to mesh peers (echo-guarded). `0` disables |
| `COLONY_DREAM_DIGEST_MAX` | `5` | max digest items per night (`0` = effectively disabled) |
| `AGENTD_SWAP_NOTIFY_AGENT` | `1` | inject the root-session substrate notice when the backend/model hot-swaps — the agent is told its own capability changed. `0` silences |

## Goals, wakeups & scheduler

| Var | Default | Purpose |
|-----|---------|---------|
| `GOAL_STEP_TIMEOUT_SECS` | `900` (30s floor) | per-step stall window for the goal driver — a step with no `TurnComplete` Fails the goal. AwaitingBatch conductors are exempt (the batch deadline is their clock). Lower (e.g. `120`) for live goal testing |
| `AGENTD_WAKEUP` | `1` | `0`/`false`/`off` disables `schedule_wakeup` entirely — new schedules refused AND pending ones hold (they fire late if re-enabled). Wakeups fire into the **session that scheduled them** (worker/spawn-range callers stay rooted at session 0) |
| `AGENTD_WAKEUP_MAX_PENDING` | `16` | max un-fired wakeups held at once (schedule-time cap) |
| `AGENTD_WAKEUP_DAILY_CAP` | `24` | max wakeup fires per UTC day, enforced at *schedule* time — bounds a schedule-on-every-wake chain |

## Fabrica — the worker/mandala tier

| Var | Default | Purpose |
|-----|---------|---------|
| `AGENTD_WORKER_CAP` | tier default | worker admission cap — how many fanned workers hold a thermal slot at once; the rest queue FIFO. Tier defaults: nano/unknown 1 · micro 2 · standard 4 · pro 8. Floor 1, resolved once at boot, advertised in `/api/capabilities` |
| `AGENTD_MESH_WORKERS` | `1` | W2/M2 mesh kill switch — exactly `0` refuses BOTH directions: `task_fanout{node}` (conducting, incl. cross-node rings) and every `/api/worker/*` endpoint (hosting). Boot-read |
| `WORKER_STEP_TIMEOUT_SECS` | `900` (30s floor) | per-worker stall window; approval-Blocked is exempt (the human's clock) |
| `WORKER_IDLE_TTL_SECS` | `1800` (60s floor) | idle/verdict-blocked time before parking (RAM evicted, JSONL stays truth, a send revives) |
| `WORKER_MAX_STEPS` | `12` (clamp 1–100) | step ceiling for `worker_report{continue}` loops. Mandala cells override with their own `budget.steps` (crosses the wire as the M2 `steps` assignment field); R-cells renew past it while their measure falls |

## Mesh & colony

| Var | Default | Purpose |
|-----|---------|---------|
| `MESH_BEACON` | `1` | downtime-beacon loop (peer liveness probing). `0`/`false`/`off` → loop never spawns — beacon-dark detection, fail-fast assigns and poll skips all go with it |
| `MESH_BEACON_INTERVAL_SECS` | `30` (floor 10) | probe interval; the floor stops a typo hammering the LAN |
| `MESH_BEACON_STALE_MISSES` | `3` (floor 1) | consecutive misses before a peer is marked dark |
| `MESH_BEACON_NOTIFY_AGENT` | `1` | root-session note when a peer goes dark/returns. `0` silences |
| `MESH_DISCOVERY_INTERVAL` | `60` | avahi-browse mDNS discovery interval (seconds) |
| `MESH_AUTO_BOOTSTRAP` | off | auto-add discovered peers. **Presence-only**: ANY value — including `0` or empty — enables it; unset is the only off |
| `MESH_SUBNET_GUARD` | `1` | restrict auto-discovered peers to the local /24. `0`/`false` loosens a safety guard |
| `AGENTD_MESH_WS` | *(unset)* | `apexos-mesh-bridge` only: the cortex link, e.g. `ws://127.0.0.1:8787/mesh-bridge`. **Unset = standalone** — the bridge logs frames instead of forwarding them, which is how a board is debugged without a daemon. The bridge dials agentd (never the reverse), so agentd holds no serial port and a node with no radio simply never has a lane |
| `MESH_BRIDGE_TOKEN` | **minted by install.sh** | Bearer token for `/mesh-bridge`, sent as a header (not `?token=`, which lands in logs). **Empty = no auth**, and agentd warns loudly at boot if it is — on a LAN-bound node an empty token lets anyone connect as a bridge, read every outbound mesh frame, and make the radio lane report itself healthy. Its own token on purpose: a bridge that can inject mesh frames is a different trust grant from one that can inject sensor readings |
| `APEXNET_PSK_FILE` | `/etc/agentd/apexnet.psk` | colony PSK (hex, 32 B) sealing Tier-4 courier manifests/receipts (and the future radio envelope). Minted per-colony by install.sh; copy the SAME file to every node (Tier 1/USB only — never radio). Absent/malformed → courier crypto disabled with honest notices. Also read by `apexos-brainstem-provision` to hand a brainstem its colony key — the ONLY other reader, and only for the seconds a commissioning takes (the bridge daemon stays PSK-free) |
| `APEXNET_NOTIFY_AGENT` | `1` | root-session notes for courier-ledger gossip (cargo announced en route, delivery receipts). `0` silences; the plug-time greeting is governed by `AGENTD_USB_NOTIFY_AGENT` |
| `APEXNET_WAN_PROBE` | `api.anthropic.com:443` | the connectivity watcher's WAN target — one TCP connect per round, nothing sent. Point it elsewhere for other backends/regions |
| `APEXNET_CHECK_SECS` | `60` (floor 15) | connectivity check cadence |
| `APEXNET_LATCH_CHECKS` | `3` (floor 1) | consecutive rounds a candidate state must hold before the latch flips (hysteresis — a flapping link must not churn the tool list / prompt-cache prefix) |
| `APEXNET_CONNECTIVITY_CONFIG` | `/etc/agentd/connectivity.toml` | the §6.3 tool-gating side table (additively synced like policy). Absent/invalid → gating disabled, all tools available |
| `MESH_BRIDGE_DEV` | unset | `apexos-mesh-bridge` serial device — the brainstem UART (P4) or a PTY. **Never guessed.** Absent → the unit stays *idle* (no crash-loop); set it and restart. install.sh enables the unit on every node |
| `APEXNET_RADIO_MAP` | unset | `NodeId=u16` pairs (`ApexOS-2=7,ApexOS-RS=3`) mapping mesh names to brainstem radio ids when `peers.toml` has no `radio_id`. BLE a2a fallback needs one or the other |
| `MESH_BRIDGE_BAUD` | `115200` | UART baud rate |
| `MESH_BRIDGE_STATS_SECS` | `30` (floor 5) | MUST-6 counter JSON line to stderr every N seconds |

## Vast.ai GPU bridge

| Var | Default | Purpose |
|-----|---------|---------|
| `VAST_API_KEY` | unset | vast.ai key — absent → `vast_*` tools error honestly; passed through as child env to the `vastai` CLI |
| `VAST_DEFAULT_GEO` | `EU_NORDIC` | default geo filter for `vast_launch` offer search (the tool arg wins) |
| `VAST_LOCAL_PORT` | `8000` | local tunnel port for the attached instance |
| `RECIPES_TOML` | `/etc/agentd/recipes.toml` | GPU recipe/tier definitions; a missing file is a hard error in recipe loading |

## Self-update (mk3) & the root watchdog

agentd side (consumed by the daemon's own self-update loop):

| Var | Default | Purpose |
|-----|---------|---------|
| `AGENTD_SELF_UPDATE_REPO` | `/var/lib/agentd/self-update/ApexOS-RS` | the agentd-owned checkout it self-builds from (provisioner-seeded) |
| `AGENTD_SELF_UPDATE_TARGET` | `<repo>/../isolated-target` | isolated `CARGO_TARGET_DIR` for the attested `--locked` build; ignored if the path sits inside the live self-update repo |
| `AGENTD_SELF_UPDATE_BUILD_TIMEOUT` | `1800` | ceiling on the on-node cargo build + tests |
| `AGENTD_SELF_UPDATE_TIMEOUT` | `120` | health-probe seconds written into `request.json` for the watchdog |
| `AGENTD_SELF_UPDATE_REVIEW` | on | pre-build adversarial diff review gate; `0`/`false`/`no` skips it |
| `AGENTD_CARGO` | `cargo` (PATH) | cargo binary for the self-build (provisioner seeds the agentd-owned toolchain path) |
| `AGENTD_UPDATE_DIR` | `/var/lib/agentd/update` | request/health/state dir shared with the root watchdog |
| `CARGO_HOME` / `RUSTUP_HOME` | `/var/lib/agentd/.cargo` / `.rustup` | agentd-owned toolchain roots (seeded by the self-update provisioner) |

Root watchdog side (`deploy/apexos-self-update.sh` + `apexos-rollback.sh`; operator/drill overrides — not unit-set):

| Var | Default | Purpose |
|-----|---------|---------|
| `APEXOS_SELF_UPDATE_BIN` | `/usr/local/bin/agentd` | binary the watchdog swaps/restores (`.prev` derived) |
| `APEXOS_SELF_UPDATE_POLL` | `2` | health-poll seconds after a swap |
| `APEXOS_SELF_UPDATE_SYSTEMCTL` | `systemctl` | systemctl shim so drills can fake unit state |
| `APEXOS_PROBATION_WINDOW` | `600` | seconds after a confirmed update during which a crash-loop still counts as a regression (rollback) |
| `APEXOS_REPO_URL` | the GitHub repo | clone source for the provisioner |

Notes: `GIT_COMMIT` is **compile-time** (`option_env!`, stamped by build.rs) — the watchdog matches the *running binary's* build SHA against the requested target, so a runtime env var cannot lie about it. `FAKE_BIN`/`FAKE_HEALTH`/`FAKE_STATEFILE` exist only inside the self-update **drill harness** (`apexos-self-update-drill.sh`), never on a real node.

## Sensors

| Var | Default | Purpose |
|-----|---------|---------|
| `SENSORHEAD_URL` | gateway `http://localhost:8080` · bridge (apex1) `http://127.0.0.1:8080` · UI `http://{agentd-host}:8080` | SensorHead-RS HTTP face. Three independent readers; the bridge polls `<url>/api/environment` + `<url>/api/thermal/data` when set. On apex1 thin S4 the operator drop-in is live. Without it the bridge forwards CPU temp only |
| `SENSOR_BRIDGE_HOST` | `localhost:8787` | gateway host:port the bridge connects to (`ws://{host}/sensor-bridge` — scheme/path hardcoded) |
| `SENSOR_BRIDGE_TOKEN` | **minted by install.sh** | Bearer token for `/sensor-bridge`, sent in the `Authorization` header (never the URL). agentd reads the same name. **Empty = no auth** (loopback bench only); a non-loopback `AGENTD_BIND` **refuses to start** if this is unset. The socket deserializes `sensor_reading` only — never the rest of the `Event` enum |
| `SENSOR_NODE_ID` | hostname | node id stamped on every reading |
| `SENSOR_INTERVAL_SECS` | `30` | seconds between reading pushes (unclamped — `0` busy-loops; don't) |
| `SENSOR_CPU_TEMP_THRESHOLD` | `85.0` °C | CPU-temp alert threshold — the `standard`-profile baseline; the live sensitivity profile adjusts per reading |
| `SENSOR_IAQ_THRESHOLD` | `150.0` | BME688 air-quality baseline (profile-adjusted live) |
| `SENSOR_THERMAL_THRESHOLD` | `45.0` °C | MLX90640 hotspot baseline (profile-adjusted live) |
| `SENSOR_ALERT_PERSIST_SECS` | `30` | how long a crossing must hold before it fires (`0` = fire immediately) — the flapping filter |
| `SENSOR_ALERT_COOLDOWN_SECS` | `1800` | per-alert-key cooldown preventing autonomous turn storms |

## Voice — TTS/STT plans, sidecars & keys

Seed-only note: the four plan pickers below persist to `AGENTD_VOICE_CONFIG` — the file wins over all of them once a Settings choice is made.

| Var | Default | Purpose |
|-----|---------|---------|
| `AGENTD_VOICE_BACKEND` | auto | TTS plan `auto`\|`local`\|`api`\|`off` (auto = Kokoro → cloud → piper → espeak) |
| `AGENTD_TTS_API` | auto-by-key | cloud TTS provider `elevenlabs`\|`openai` |
| `AGENTD_STT_BACKEND` | auto | STT plan `auto`\|`local`\|`api`\|`off` (auto = sidecar → whisper-cpp binary → cloud) |
| `AGENTD_STT_API` | auto-by-key | cloud STT provider `openai`\|`elevenlabs` (OpenAI preferred) |
| `AGENTD_VOICE_CONFIG` | `/var/lib/agentd/voice_config.json` | the persisted voice selection (file-wins-on-restart) |
| `ELEVENLABS_API_KEY` (+ `ELEVENLABS_VOICE_ID`/`ELEVENLABS_MODEL`/`ELEVENLABS_STT_MODEL`) | unset / Rachel / `eleven_flash_v2_5` / `scribe_v2` | ElevenLabs cloud TTS + Scribe STT (blank = unset) |
| `OPENAI_API_KEY` (+ `OPENAI_TTS_MODEL`/`OPENAI_TTS_VOICE`/`OPENAI_STT_MODEL`) | unset / `gpt-4o-mini-tts` / `alloy` / `whisper-1` | OpenAI cloud TTS/STT — a **real** api.openai.com key, distinct from the routing OAI/OpenRouter key |
| `APEX_TTS_ADDR` / `APEX_TTS_URL` | `127.0.0.1:8770` / `…/synth` | Kokoro sidecar bind / where the gateway reaches it |
| `KOKORO_DIR` | `/var/lib/agentd/kokoro` | Kokoro model dir (`*.onnx` int8 + `voices-v1.0.bin`); load failure exits so systemd surfaces it; needs `espeak-ng` on PATH |
| `APEX_STT_ADDR` / `APEX_STT_URL` | `127.0.0.1:8771` / `…/transcribe` | Whisper sidecar bind / endpoint |
| `WHISPER_MODEL` | sidecar `…/ggml-base.en.bin` · gateway fallback `…/ggml-tiny.en.bin` | ggml model paths — note the two readers default to different files |
| `WHISPER_BIN` | `/usr/local/bin/whisper-cpp` | hand-installed whisper-cpp fallback binary (missing → plan falls through to cloud) |
| `WHISPER_LANG` | `en` | language hint — re-read on **every** `/transcribe` request (changeable without restart) |
| `ALSA_CAPTURE_DEVICE` | arecord default / `plughw:2,0` (server-side) | mic capture device — ui-slint push-to-talk and the gateway's server-side recording read it separately |
| `PIPER_MODEL` | unset | legacy piper voice model path — presence alone enables the piper fallback tier (and the wake "yes?" ding); also read by the notify tool's TTS surface |

## System tools — camera, GPIO, notify

| Var | Default | Purpose |
|-----|---------|---------|
| `APEXOS_CAMERA_DEVICE` | auto | force a V4L2 node for `camera_capture` (the per-call `device` arg wins). Auto order: custom cmd → forced device → rpicam CSI → USB webcam → fswebcam |
| `APEXOS_CAMERA_CMD` | unset | full custom capture command with `{out}` placeholder — highest priority, hard-errors instead of falling through |
| `APEX_GPIO_RESERVED` | guard on | reserved-pin guard for the `gpio_*` tools (pins 0-3, 27-28: HAT EEPROM + sensor-head I2C). ONLY the exact literal `none` disables — not `0`, not case-insensitive |
| `NTFY_TOPIC` | unset (surface skipped) | ntfy.sh topic for the `notify` tool's push surface |
| `TELEGRAM_BOT_TOKEN` + `TELEGRAM_CHAT_ID` | unset (surface skipped) | Telegram notify surface — **both** must be set; either missing skips it entirely |

## ui-slint (the Slint shell)

| Var | Default | Purpose |
|-----|---------|---------|
| `APEXOS_UI_SNAPSHOT_ADDR` | `127.0.0.1:8788` | loopback bind for the screen-mirror snapshot server (`/snapshot` PNG + `/state` JSON) |
| `APEXOS_UI_SNAPSHOT_URL` / `APEXOS_UI_STATE_URL` | `http://127.0.0.1:8788/snapshot` / `…/state` | where the `screenshot_mirror` / `ui_*` tools fetch them; connection-refused degrades gracefully on headless nodes |
| `APEX_FACE_GL` | auto | GL/SDF face wherever a real GL context exists; `0` forces the 2D face |
| `APEX_FACE_AUTOOPEN` / `APEX_FACE_STATE` / `APEX_SKETCH_AUTOOPEN` / `APEX_OCCIPITAL_DEMO` | unset | dev hooks: auto-open Face/Sketchpad at launch, pin a face emote, open the Occipital reader with a sample payload — no agentd needed |
| `FONTCONFIG_FILE` | UI-generated | the UI writes `$XDG_CACHE_HOME/apexos-rs/fonts.conf` (mono emoji, no tofu) and sets this — a pre-set value is respected as an operator override |
| `BROWSER` | `xdg-open` | program for opening external URLs from the UI |
| `CEREBRO_WEB_URL` | `http://{agentd-host}:8765` | Lucida observatory URL the Web tile opens (cerebro-api; ui-slint appends `?token=` from `AGENTD_TOKEN` when set) |
| `XDG_CACHE_HOME` / `XDG_CONFIG_HOME` | `$HOME/.cache` / `$HOME/.config` | roots for the imagine cache, generated fonts.conf, and the persisted persona file |

## USB exo-workspace

| Var | Default | Purpose |
|-----|---------|---------|
| `AGENTD_USB_EJECT_DIR` | `/var/lib/agentd/usb-eject` | drop-dir where `eject_media` writes request files for the root eject drain (`NoNewPrivileges` — never sudo) |
| `AGENTD_USB_PREP_DIR` | `/var/lib/agentd/usb-prep` | drop-dir for "use this drive" format/prep requests |
| `AGENTD_USB_NOTIFY_AGENT` | `1` | proactive root-session greeting when an exo-stick mounts. `0` silences |

udev note: `ID_FS_LABEL` (only `APEX-*` sticks are claimed), `UDISKS_IGNORE`, and `SYSTEMD_WANTS` in `deploy/udev/99-apexos-usb.rules` are udev **device properties**, not process env.

## Cerebro (memory)

| Var | Default | Purpose |
|-----|---------|---------|
| `CEREBRO_EMBED_MODEL` | `BAAI/bge-small-en-v1.5` (384-dim, ~33 MB) | fastembed text model. **Explicit empty string is the documented Nano mode**: no embedder loads (~23 MB RSS, FTS5-only search) — unset gets the default, `""` survives. Also the tier signal that flips CLIP visual embedding off by default |
| `CEREBRO_DATA_DIR` | `$HOME/.cerebro-cortex` (unit: `/var/lib/agentd/cerebro`) | data root — derives `cerebro.db` and `exports/` |
| `CEREBRO_API_ADDR` | `127.0.0.1:8765` | cerebro-api listen address. Safety interlock: a non-loopback addr with empty `AGENTD_TOKEN` refuses to start. LAN form: `0.0.0.0:8765` |
| `CEREBRO_VISION_BACKEND` | auto | `describe_image` VLM transport — `ollama`/`lan`/`local` · `anthropic`/`api` · `off`/`none`. Auto = Ollama first, Anthropic if keyed, else honest error. Unrecognized values fall to auto, not error |
| `CEREBRO_VISION_URL` | `http://localhost:11434` | Ollama base for vision — Pi-local and LAN are the same transport, so repointing hot-swaps the cluster's vision backend |
| `CEREBRO_VISION_MODEL` | `moondream` (~1.6B) | Ollama vision model name |
| `CEREBRO_VISION_EMBED` | follow embed tier | CLIP visual embedding: unset/`auto` follows `CEREBRO_EMBED_MODEL` (on for Micro+, off for Nano); `off`/`0`/`false`/`""` force off; **any other value force-enables**. The ~350 MB model lazy-loads on first use |
| `CEREBRO_RETAIN_VERSIONS` | `10` | version snapshots kept per edited memory (`0` = keep forever) |
| `CEREBRO_RETAIN_DREAM_REPORTS` | `90` | dream reports kept (~3 months) (`0` = forever) |
| `CEREBRO_RETAIN_AUDIT_ROWS` | `20000` | audit-log rows kept (`0` = forever); the dream's retention sweep audits what it pruned |
| `CEREBRO_AGENT` | unset | cerebro-cli only: default `--agent` scope for subcommands |
| `FASTEMBED_CACHE_DIR` | `/var/lib/agentd/cerebro/models` | on-disk ONNX model cache (Occipital gets its own under `occipital/models`) |

## Sibling nodes & plugins

| Var | Default | Purpose |
|-----|---------|---------|
| `IMAGINARIUM_URL` | `http://127.0.0.1:8791` | the local Imaginarium node — presence selects the MCP plugin's proxy mode; ui-slint's Imagine app pins an env-set URL over the agentd-supplied one. Seeded when provisioned |
| `IMAGINARIUM_TOKEN` | minted at install | LAN bearer for the node — the ONLY credential agentd-side processes hold (the xAI key stays in `/etc/imaginarium/env`, 0600, never mirrored). Minted into `/etc/imaginarium/env` + seed-if-absent mirror into `/etc/agentd/env`; rotate both together. Desktop UIs fetch it from `GET /api/imaginarium` after login |
| `IMAGINARIUM_HOME` | `/var/lib/imaginarium` | media library + jobs DB root (imaginarium unit) |
| `XAI_API_KEY` | empty placeholder | xAI/Grok key — lives ONLY in `/etc/imaginarium/env`; installer reads it back to gate INSTALLED→ACTIVE |
| `OCCIPITAL_DB` / `OCCIPITAL_KEYS_FILE` / `OCCIPITAL_EMBED_MODEL` | `/var/lib/agentd/occipital/…` / bge-small (micro+) | Occipital web-cortex plugin env, seeded into `plugins.toml` by install.sh |
| `SUNO_API_KEY` | empty placeholder | sunoapi.org key — sonus-mcp **self-loads** `/etc/sonus/env` (0640 root:agentd); deliberately never in agentd's env or plugins.toml |
| `SUNO_DOWNLOAD_DIR` | `/var/lib/agentd/workspace/sonus` | downloaded tracks (inside the workspace so the player sees them). Note the `SUNO_` prefix |
| `SONUS_AUDIO_DEVICE` | `default` | ALSA output for server-side Sonus playback (`/api/sonus/play`; Pi 5 kiosks often need `plughw:1,0`) |

## Installer — boot-file flags & build knobs

`install.sh` reads these from the process env, a `apexos.conf` boot/USB file, or `/etc/agentd/install.conf` (persisted answers — `apexos-update` re-runs with them; CLI flags win):

| Var | Default | Purpose |
|-----|---------|---------|
| `APEXOS_MODE` | `auto` | deployment shape `kiosk`\|`headless`\|`desktop` (auto-detect: `DISPLAY`/`WAYLAND_DISPLAY` present on a Pi → desktop) |
| `APEXOS_TIER` | `auto` | `nano`\|`micro`\|`standard`\|`pro` (RAM/arch detect) — drives embeddings + model choices |
| `APEXOS_NO_UI` / `APEXOS_NO_OCCIPITAL` / `APEXOS_NO_CEREBRO_API` | `false` | skip the Slint UI build / the Occipital plugin / the cerebro REST dashboard |
| `APEXOS_NO_SENSOR` | `true` (sensor OFF) | `false` is how you turn the BME688+MLX90640 head ON |
| `APEXOS_VOICE` | off | `1` provisions Kokoro TTS + Whisper STT |
| `APEXOS_IMAGINARIUM` | off | `1` provisions the Imaginarium node (needs an `XAI_API_KEY` for ACTIVE) |
| `APEXOS_SONUS` | off | `1` provisions Sonus-RS — boot/USB file, install.conf, or `--sonus`, same provenance precedence as the other add-on flags (USB-parse gap closed `#326`) |
| `APEXOS_UI_AS_ROOT` | `false` | kiosk DRM fallback (SA-13). Default unit is `User=apexos-ui`. `true` (or `--ui-as-root`) installs a systemd drop-in that runs the UI as root with `CAP_SYS_ADMIN`+`CAP_SYS_TTY_CONFIG` only — still loads `/etc/agentd/ui.env`, never `/etc/agentd/env`. install.sh auto-sets this if the unprivileged start fails. `--ui-unpriv` clears it |
| `CARGO_BUILD_JOBS` (+ `CARGO_PROFILE_RELEASE_{OPT_LEVEL,LTO,CODEGEN_UNITS}`) | low-RAM only | the OOM build guard: ≤4 GiB nodes build with jobs 1, opt-level 2, LTO off, 16 codegen units — installer-set for the build only, never persisted |
| `KOKORO_MODEL_URL` / `KOKORO_VOICES_URL` / `WHISPER_GGML_URL` | upstream releases | model download overrides (mirrors / air-gapped installs) |
| `SUDO_USER` | ambient | the unprivileged user cargo builds as (falls back to the repo owner) |

## Dev & test hooks (never set on a live node)

`APEXOS_PAC_LINT_FILE` (run a dense artifact through the PAC lint gate) and `APEXOS_REPAIR_CHECK_FILE` (verify a session JSONL heals) are forensic hooks read only inside `cargo test`. The `FAKE_*` trio belongs to the self-update drill harness. `CARGO_PKG_VERSION` in the MCP handshake is compile-time.
