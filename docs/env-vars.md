# Environment variables — the full reference

> Moved verbatim from CLAUDE.md (2026-07-21 docs refactor). Every knob across agentd,
> ui-slint, the voice sidecars, sensors, cache, and install. Dev-critical basics
> (`AGENTD_WS`, `SLINT_BACKEND`, `RUST_LOG`) are also summarized in CLAUDE.md.


| Var | Default | Purpose |
|-----|---------|---------|
| `AGENTD_WS` | `ws://localhost:8787/ws` | agentd WebSocket URL |
| `AGENTD_AGENT_ID` | `APEX` | agentd: the node's bound agent identity. agentd **stamps** it onto every Cerebro tool call (overriding the model) and uses it for its own Cerebro writes — single source of truth, see `docs/agent-identity.md`. Per-session identities (multi-agent) layer on later |
| `AGENTD_CCBS` | unset | agentd: set `0`/`false` to disable CCBS boot-priming (the daemon-side `cognitive_bootstrap` injected into the system prompt on a session's first turn) |
| `AGENTD_BOOTSTRAP_MODE` | `standard` | agentd: CCBS token budget — `minimal` (1000) / `standard` (2000) / `full` (4500) |
| `AGENTD_DREAM_CRON` | `0 0 3 * * *` | agentd: cron (6-field, UTC) for the nightly autonomous `dream_run`; **empty disables it** |
| `AGENTD_DREAM_TIMEOUT_SECS` | `1800` | agentd: how long the nightly loop waits for `dream_run` to complete (60s floor). The dispatched dream runs to completion regardless — this is the caller's patience, and the digest push is gated on the result arriving (a ~50s dream vs the old fixed 10s ToolProxy timeout = every nightly dream logged as failed, digest never fired) |
| `COLONY_DREAM_DIGEST` | `1` | agentd: after a successful nightly dream, push the dream's newly-born schemas/consolidations to every mesh peer via the federation relay (`dream-digest`-tagged; echo-guarded so imports never re-broadcast). `0` disables |
| `COLONY_DREAM_DIGEST_MAX` | `5` | agentd: max digest items per night (`0` = disabled) |
| `AGENTD_DREAM_JOURNAL` | `1` | agentd: after each nightly dream, deposit a first-person journal (`dream-journal`-tagged memory + a "Last dream" section in wake priming, persisted to `<log_dir>/last_dream_journal.txt`). `0` disables |
| `AGENTD_SWAP_NOTIFY_AGENT` | `1` | agentd: inject a root-session substrate notice when the inference backend/model hot-swaps (operator switch or vast attach/revert) — the agent is told its own capability just changed. `0` silences |
| `AGENTD_HISTORY_TOKEN_BUDGET` | `120000` | agentd: per-session in-memory history window (rough tokens — per-block-type calibrated against real `count_tokens` 2026-07-26: prose/4, JSON/3 + an escape-density term; real windows now land within ~±20% of the configured number, so raise this deliberately if you want the old, accidentally-larger windows back). Soft ceiling with hysteresis: trim fires only past 1.2× the budget and cuts to 0.75×, so steady-state sessions don't rewrite the window front (= bust the conversation prompt-cache) on every message; peak resident ≈ 1.2× budget. Oldest whole turns drop at clean boundaries (an honest trim marker is injected at the seam). `0` disables trimming. Lower it for small-context local models |
| `AGENTD_AMBIENT_GAP_SECS` | `600` | agentd: idle gap before the live clock (Now + uptime) is re-injected into a turn. The clock lands on a session's first turn and then only after this much quiet — keeps temporal grounding without per-message noise. Lower for more frequent stamps; very high ≈ first-turn-only |
| `GOAL_STEP_TIMEOUT_SECS` | `900` | agentd: per-step stall window for the autonomous goal driver — an Acting goal whose step produces no `TurnComplete` within this window Fails (instead of hanging). Clamped to a 30s floor; lower it (e.g. `120`) for live goal testing |
| `AGENTD_WORKER_CAP` | tier default | agentd: worker admission cap (Fabrica W tier) — how many fanned-out workers hold a thermal slot (Running/Blocked) at once; the rest queue FIFO. Default from the hardware tier: nano/unknown 1 · micro 2 · standard 4 · pro 8. Floor 1; keep ≤ the turn-engine semaphore (16) — the cap is residency, not a provider-call guarantee |
| `WORKER_STEP_TIMEOUT_SECS` | `900` | agentd: per-worker stall window — a Running worker whose turn produces no `TurnComplete` within this window Fails. 30s floor; Blocked (awaiting approval) is exempt — that wait runs on the human's clock |
| `WORKER_IDLE_TTL_SECS` | `1800` | agentd: how long a yielded (Idle) or verdict-blocked worker sits before parking — RAM history evicted, `sessions/<id>.jsonl` stays truth, a send revives it. 60s floor |
| `WORKER_MAX_STEPS` | `12` | agentd: step ceiling for `worker_report{continue}` loops — at the ceiling the worker goes Done ("step budget reached"). Clamped 1–100 |
| `AGENTD_WAKEUP` | `1` | agentd: `0`/`false`/`off` disables `schedule_wakeup` (the agent's one-shot self-continuity alarm; pending ones stop firing too) |
| `AGENTD_WAKEUP_MAX_PENDING` | `16` | agentd: max un-fired wakeups the agent may hold at once |
| `AGENTD_WAKEUP_DAILY_CAP` | `24` | agentd: max wakeup fires per UTC day, enforced at *schedule* time — bounds a schedule-on-every-wake chain to this many turns/day |
| `AGENTD_IDENTITIES` | `/etc/agentd/identities.toml` | agentd: the multi-agent identity registry (`[[user]]` + `[[agent]]`); see `docs/agent-identity.md`. Data layer only so far (3a) |
| `AGENTD_CACHE` | `1` | agentd (Anthropic only): `0`/`false`/`off` disables prompt caching entirely (system sent as a plain string, no `cache_control`). On = cache the system+tools prefix + (by default) the conversation. OpenAI/Ollama auto-cache regardless |
| `AGENTD_CACHE_CONVERSATION` | `1` | agentd (Anthropic only): `0`/`false`/`off` caches only the stable system+tools prefix, not the growing transcript. On = roll up to 3 breakpoints back through the conversation (the big 1M-giga-session win). No effect when `AGENTD_CACHE=0` |
| `AGENTD_CACHE_TTL` | `1h` | agentd (Anthropic only): cache TTL. Default `1h` (write premium 2×) — survives human pauses, sensor-alert and wakeup gaps without re-writing the whole prefix; every ApexOS mode is gappy, so this is the economical choice (field-measured 2026-07-25: at `5m`, human-paced sessions read ~0 from cache and re-wrote ~300k tokens per exchange). Set `5m` (any non-1h value) for the 5-minute TTL (write 1.25×) on pure burst loops |
| `AGENTD_BACKEND` | `anthropic` | agentd: LLM provider — `anthropic` \| `openrouter` \| `ollama` \| `vllm` \| `oai`. **Seed only** — a Settings/`POST /api/backend` choice persists to `AGENTD_BACKEND_CONFIG` and wins on restart (see the backend-switcher gotcha) |
| `AGENTD_MODEL` | per-backend | agentd: model id. Unset → backend default (anthropic `claude-sonnet-4-6`, ollama/vllm `qwen3:27b`, openrouter `qwen/qwen3-70b-a3b`). Seed only, like `AGENTD_BACKEND` |
| `AGENTD_OAI_BASE_URL` | `http://localhost:11434/v1` | agentd: OpenAI-compat endpoint for the non-anthropic backends. Switching to `openrouter` auto-pins `https://openrouter.ai/api/v1` |
| `OAI_API_KEY` / `OPENROUTER_API_KEY` | unset | agentd: bearer key for the OpenAI-compat backends (either name works); `POST /api/keys {oai}` sets it live + persists to `/var/lib/agentd/.oai_api_key` (0600) |
| `AGENTD_BACKEND_CONFIG` | `/var/lib/agentd/backend_config.json` | agentd: the persisted backend/model/URL selection (file-wins-on-restart; delete the file to return to env control) |
| `SLINT_BACKEND` | auto | `winit` (desktop), `linuxkms` (Pi), `linuxkms-femtovg` (Pi Zero) |
| `SLINT_FULLSCREEN` | unset | `1` = fullscreen, no window chrome |
| `RUST_LOG` | `info` | tracing filter |
| `VISION_MAX_EDGE` | `1024` | agentd: longest-edge px cap for images entering model context (the token-bomb shim, clamped 128–4096) |
| `APEX_FACE_GL` | auto | ui-slint: GL/SDF face render. Auto-on wherever a real GL context exists (desktop, Pi 4/5 V3D), 2D `FaceView` fallback otherwise; `0` forces 2D everywhere. Dev: `APEX_FACE_AUTOOPEN=1` opens the Face window at launch, `APEX_FACE_STATE=<emote>` previews an expression without agentd |
| `APEXOS_UI_SNAPSHOT_ADDR` | `127.0.0.1:8788` | ui-slint: loopback bind for the screen-mirror snapshot server (`take_snapshot`→PNG); the `screenshot_mirror` tool fetches from the matching `APEXOS_UI_SNAPSHOT_URL` (`http://127.0.0.1:8788/snapshot`) |
| `APEXOS_CAMERA_DEVICE` | auto | `camera_capture` tool: force a V4L2 node (e.g. `/dev/video0`) instead of auto-detecting. Auto order = Pi CSI camera (rpicam) → first `/dev/video*` webcam |
| `APEXOS_CAMERA_CMD` | unset | `camera_capture` tool: full custom capture command with a `{out}` placeholder (e.g. a gphoto2/network-cam grab); overrides all auto-detection |
| `KOKORO_DIR` | `/var/lib/agentd/kokoro` | apex-tts: Kokoro model dir (`*.onnx` int8 + `voices-v1.0.bin`) |
| `APEX_TTS_ADDR` | `127.0.0.1:8770` | apex-tts: loopback bind for the TTS sidecar |
| `APEX_TTS_URL` | `http://127.0.0.1:8770/synth` | gateway: where `/api/speak` reaches the apex-tts sidecar |
| `AGENTD_VOICE_BACKEND` | `auto` | gateway: TTS backend `auto`\|`local`\|`api`\|`off` (auto = local Kokoro → cloud API → piper → espeak) |
| `AGENTD_TTS_API` | auto-by-key | gateway: cloud TTS provider `elevenlabs`\|`openai` (auto picks by which key is set) |
| `ELEVENLABS_API_KEY` (+ `ELEVENLABS_VOICE_ID`/`ELEVENLABS_MODEL`) | unset / Rachel / `eleven_flash_v2_5` | gateway: ElevenLabs cloud TTS |
| `OPENAI_API_KEY` (+ `OPENAI_TTS_MODEL`/`OPENAI_TTS_VOICE`) | unset / `gpt-4o-mini-tts` / `alloy` | gateway: OpenAI cloud TTS (a **real** api.openai.com key, not the routing OAI/OpenRouter key) |
| `AGENTD_STT_BACKEND` | `auto` | gateway: STT backend `auto`\|`local`\|`api`\|`off` (auto = local whisper-cpp → cloud) |
| `AGENTD_STT_API` (+ `OPENAI_STT_MODEL`/`ELEVENLABS_STT_MODEL`) | auto-by-key / `whisper-1` / `scribe_v2` | gateway: cloud STT provider `openai`\|`elevenlabs` (OpenAI preferred), reusing the TTS keys |
| `APEX_STT_ADDR` / `APEX_STT_URL` | `127.0.0.1:8771` / `…/transcribe` | apex-stt loopback bind / where local STT reaches it |
| `WHISPER_MODEL` (+ `WHISPER_GGML_URL`/`WHISPER_LANG`) | `…/ggml-base.en.bin` / ggerganov base.en / `en` | apex-stt Whisper ggml model path / install.sh download src / language hint |
| `IMAGINARIUM_URL` | `http://127.0.0.1:8791` (seeded) | the local Imaginarium fat node. Read by the `imaginarium mcp` proxy plugin (inherited from agentd's env — its presence is what selects proxy mode) and the ui-slint Imagine app. Seeded into `/etc/agentd/env` by install.sh when Imaginarium is provisioned |
| `IMAGINARIUM_TOKEN` | minted at install | LAN bearer token for the Imaginarium node — the ONLY credential agentd-side processes hold (the xAI key stays in `/etc/imaginarium/env`, never here). Minted once into `/etc/imaginarium/env`, mirrored seed-if-absent into `/etc/agentd/env`; rotate = update both (they must agree). The **desktop** ui-slint app can't read the 0600 env file — it fetches the value from agentd's `GET /api/imaginarium` after login; env wins when exported. See `docs/imaginarium.md` |
| `APEXOS_IMAGINARIUM` | `0` | install.sh (boot-file/install.conf): `1` opts the node into Imaginarium provisioning (clone+build sibling, mint token, unit + MCP plugin when an `XAI_API_KEY` is present). CLI: `--imaginarium` / `--no-imaginarium` |

---

