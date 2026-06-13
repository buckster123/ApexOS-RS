# apexos-world — Master Design

> **Status: PROTOTYPE — design + scaffold.** This is the authoritative design,
> consolidated from the six dimension docs (`docs/design/01`–`06`) and the two
> adversarial critiques (`critique-agentd.md`, `critique-rendering.md`). Where the
> dimension docs conflicted, this document resolves it; where a critique found a
> blocker, it is folded in below as an explicit **Decision** or **Risk**.
> The dimension docs remain as the long-form rationale; **this file wins on conflict.**

Codename `apexos-world`. Binary `apexos-world` (mirrors `apexos-rs-ui`). Prototype on
branch `proto/world-3d`, scaffolded under `world/`, to be extracted to its own repo later.

---

## 1. Vision

ApexOS-RS sessions are normally a *list*; apexos-world makes them a *place*. It is an
**interface first, a world second** — not a game — a third client of agentd, peer to
`ui-slint` and the browser/PWA, that happens to be a navigable 3D **Atrium**. You walk
up to a thing (an agent avatar, a station), activate it, the world recedes and that
thing's own UI fills the view; you step back and you are in the space again. The 3D
layer is a **router and a context**, not a renderer of business data — the
function-appropriate UI (chat, sensors, council, terminal, memory) is the real surface,
lifted from `ui-slint`'s views. Space earns its place specifically for an *AI* interface:
an agent's work is plural and concurrent (root + sub-agents + councils coexist as
figures you can glance at), agents can be embodied and can *see* through an avatar
camera, telemetry has a natural periphery, and approach itself is low-commitment intent.
It speaks agentd's **real** `Event`/Intent wire protocol on `ws://HOST:8787/ws` and
earns new powers (agent-vision, world-state) only through agentd's documented MCP-plugin
extension surface — **never** by forking core. Target tier is **Standard/Pro desktop
only** (real GPU); on Nano/Micro the user runs `apexos-rs-ui` or the browser PWA.

---

## 2. Anatomy — two processes, both documented agentd extension surfaces

apexos-world is **two cooperating processes**, mirroring exactly how `apexos-tools` and
`cerebro-mcp` already relate to agentd:

```
┌─────────────────────────── one Standard/Pro node ─────────────────────────────┐
│                                                                                │
│  ┌─────────────┐   ws://HOST:8787/ws          ┌──────────────────────────────┐ │
│  │   agentd    │◀──── Event / Intent JSON ─────▶│  world-app  (the renderer)   │ │
│  │  (daemon,   │   (a normal WS client, like   │  Slint + wgpu, owns GPU +    │ │
│  │  UNCHANGED) │    ui-slint or the browser)   │  window/event loop           │ │
│  └─────┬───────┘                              └──────────────┬───────────────┘ │
│        │ spawns stdio MCP plugins (plugins.toml)             │ local IPC        │
│        ▼                                          (unix socket / localhost HTTP)│
│  ┌──────────────────────┐  newline-delimited JSON-RPC        │                 │
│  │  world-vision (MCP)   │◀──────────(stdio)──── agentd ──────┘                 │
│  │  agentd-facing tools  │  forwards every tool call over IPC to world-app      │
│  └──────────────────────┘                                                       │
└────────────────────────────────────────────────────────────────────────────────┘
```

- **`world-app`** — the 3D renderer and the WS client. Speaks the real `Event`/Intent
  JSON on `/ws`. Owns the wgpu device, the scene, avatars, cameras. The *only* process
  that can render a viewport and read pixels back. Also hosts a small **localhost IPC
  server** so the vision plugin can ask it for renders.
- **`world-vision`** — a stdio MCP plugin declared in `plugins.toml` like any other.
  agentd's `Supervisor` spawns it and lists its tools. It owns *no* renderer; every tool
  call it forwards over the local IPC to `world-app`, awaits the result, and returns it
  to agentd as an MCP `tools/call` response.

This is the whole trick: the agent calls a tool → agentd routes it to the plugin → the
plugin asks the renderer → the renderer reads back pixels → the result flows back up the
same path as any tool result. **No new agentd code** for any of the interaction surface;
agent-vision adds a plugin (and one small core change — see §4).

### The space — an Atrium

One bounded room (~20–30 m, circular/hex) you can see across. Chosen over a *campus*
(travel time is friction with no payoff on a single node) and a *starfield*
(disorienting, no floor). **Stations line the perimeter** (fixed furniture, one per
agentd *function*); **agents stand on the floor** (dynamic, one per `SessionId`); the
**architecture itself is ambient telemetry** (floor pulse = CPU, wall tint = IAQ, light
= busy). Fixed canonical layout learned once. Scales past one node only when a mesh
exists: a **concourse of atria**, one room per node, connected by mesh portals.

### Crate layout

```
world/
├── Cargo.toml                 # [workspace]
├── README.md
└── crates/
    ├── world-protocol/        # agentd Event/Intent mirror + WS client — LIGHT deps (CI-green, no GPU)
    ├── world-app/             # Slint + wgpu (+ deferred Bevy) renderer binary — HEAVY deps
    └── world-vision/          # snapshot/act MCP stdio plugin — LIGHT deps (no GPU)
```

Dependency discipline: `world-protocol` is the only crate everything depends on, and it
stays heavy-dep-free so it `cargo check`s and `cargo test`s on a headless box with no
GPU/X/fontconfig. `world-vision` is a leaf, buildable and handshake-testable from day
one. `world-app` is the only crate that needs the GPU/UI toolchain and the only one that
carries known-incomplete integrations.

---

## 3. The two primary entity types and the core loop

### Agent avatars (dynamic, one per `SessionId`)

A figure on the floor that *is* an agentd session. Identity = `SessionId` (bare `u64`).
Root = no parent, near center; child appears via `SubAgentStarted{parent,child,prompt}`
and stands beside its parent with a tether. v0 body = capsule + emissive head orb +
billboard label (glTF rig is v1). Status is **derived from existing events** keyed by
`session`, never a new channel:

| Event (by `session`) | Avatar phase / visual |
|---|---|
| `agent_text{delta}` | **Speaking** — head bright, glow pulse |
| `agent_thinking{delta}` | **Thinking** — dim inward shimmer |
| `tool_requested{call}` | **Acting** — accent ring spins |
| `approval_pending{call}` | **Blocked** — amber halo, "!" billboard |
| `tool_result{ok:false}` / `error` | flash / red halo |
| `turn_complete` | **Idle** — slow pulse, dim |
| `wake_triggered` | all avatars flash |
| `agent_message{from,to}` | a visible arc/beam between avatars |

> **The avatar IS the chat station.** Activating an avatar opens that session's chat
> surface. There is no separate Chat furniture for the root agent — you talk to APEX by
> walking up to APEX. This is the single most AI-native gesture: the conversation has a
> location and a face, and concurrent conversations are concurrent figures.

### Stations (fixed, one per function — reuse, never reinvent)

A station = *placement* + *binding* + *surface*, where the surface is the **same Slint
view `ui-slint` already ships**, parameterized by a session scope. Catalog (the launch
set, an enum + static descriptor table, the analogue of ui-glowup's `AppKind`):

| StationKind | Function | agentd binding |
|---|---|---|
| `Chat` | streaming chat + tool cards + approvals (usually the avatar itself) | a `SessionId`, WS events filtered on it |
| `System` | CPU/RAM/disk/uptime + IAQ badge | REST poll `POST /api/run`, focus-gated, no session |
| `Sensors` | IAQ stats + thermal heatmap | broadcast `sensor_reading` + on-demand `GET /api/snapshot` |
| `Council` | per-agent cards + convergence + synthesis (a ring of avatars) | `POST /api/council`, WS `council_*` keyed on `council_id` |
| `Terminal` | line-mode PTY | `/terminal-ws` (its **own** socket, not `/ws`) |
| `Memory` | cerebro recall/search/graph | `cerebro-mcp` tools via a hidden session, or external `:8765` link |
| `Generative` | agent-authored panel layout | a `SessionId` + a `world.render_ui` tool result → closed `UiSpec` → templates |

### The core loop — approach → activate → fill → dismiss

Four fully-reversible states: **ROAMING** (move through the room, ambient telemetry
plays) → **FOCUSED** (proximity/gaze primes a target: outline glow, billboard tooltip
with live status, "E — open" hint; *no network intent*) → **ACTIVE** (camera eases in
~250 ms, world dims/blurs, the function UI fills the focus plane; alerts still bleed
through) → **dismiss** (Esc / step back; the session keeps running underneath). Loop
invariants: every state reversible with one input; no full-screen modal without a
visible exit; **approach never sends an intent** (only activate + in-surface actions
do); **busy is owned by the avatar, not the surface** (dismiss chat, still see it
glowing from across the room).

Navigation (desktop first): **Walk** (WASD + mouse-look, default), **Fly** (overview),
and **Teleport/snap-to** (first-class — daily use is "snap to APEX, talk, snap to
sensors, glance, snap back"; walking is for *reading the room*, not traversal tax). A
persistent minimal Slint HUD (never blurred) shows agentd connection status, node name,
heading, and an alert ribbon — it is the OS chrome and does not obey the world.

**Council as a place** is the canonical "made spatial" win: `council_started` raises a
ring of avatars; `council_agent_delta` spotlights the speaker; `council_round_done.
convergence` *tightens the ring*; `council_complete.synthesis` resolves it to a central
panel. The same stream, but convergence is a distance and turn-taking is a spotlight.

---

## 4. agentd integration contract + feasibility verdict

This is the load-bearing dimension, checked against real source
(`agentd/crates/core/src/types.rs`, `gateway/src/lib.rs`, `plugins/src/mcp.rs`,
`agent/src/{turn,anthropic,oai}.rs`) and independently re-verified by the adversarial
critique.

### Wire facts that dominate the design (all verified)

1. **One socket ⇄ one `session_id`.** Gateway assigns a fresh id on connect and
   **injects** it into every inbound frame (`frame["session"] = id`), so the client
   **omits `session` on outbound intents**. `hello{resume_session}` re-points a socket.
2. **The broadcast is global.** Every event for every session goes to every socket; no
   server-side per-session filter. **Clients MUST filter inbound on `session`.**
3. **Encoding:** `Event` is `#[serde(tag="type", rename_all="snake_case")]`; `SessionId`/
   `ActionId` serialize as **bare numbers**. On `tool_requested`/`approval_pending` the
   tool data nests under `call` (`call.id`, `call.tool`, `call.args`); on `tool_result`,
   `call` is the **bare `ActionId`** and the body is `output:{ok,content}`.
4. **A frame that fails to deserialize is silently dropped** — wrong field names = no
   error, nothing happens. The client must mirror this (log-and-drop unknown frames).
5. **`turn_started` does NOT exist** in the Rust daemon. Busy is driven by the first
   `agent_text`/`tool_requested` after a prompt; cleared by `turn_complete`.
6. **`user_cancel` emits no `TurnComplete`** — the client clears busy + tears down
   in-flight tool affordances + drops open approvals itself.
7. **`user_approval`/`user_cancel` are WS-only**, bound to a socket's session (no REST
   equivalent). Cross-session *prompts* can go via `POST /api/sessions/:id/message`, but
   approve/cancel require holding a socket bound to that session.
8. **Handshake:** on connect the gateway *immediately* pushes
   `{"type":"session_init","session_id":N,"history":[…]}` on a biased channel. The
   client reads this first to learn its id. **The server never sends a `hello` reply** —
   `hello` is the *client→server* resume frame. *(Resolves the doc 05 / CLAUDE.md typo;
   see Decision D7.)* Resume succeeds only for sessions agentd has loaded; an unknown id
   silently falls through to a fresh session, so compare the returned id to the requested.

### Outbound intents (session omitted; gateway injects)

| Interaction | Frame |
|---|---|
| Speak to avatar | `{"type":"user_prompt","text":"…"}` |
| Approve / reject | `{"type":"user_approval","action":<call.id number>,"granted":bool}` |
| Cancel a turn | `{"type":"user_cancel"}` |
| Resume a session | `{"type":"hello","resume_session":<id>}` |

### Agent-vision and acting — the MCP plugin

The vision/act tools live in `world-vision` (`plugins.toml`, policy-gated). The agent
calls a tool → policy → supervisor dispatches → plugin forwards over local IPC to
world-app → world-app renders an **offscreen** frame (camera `is_active=false` flipped
for one frame), reads back pixels (`copy_texture_to_buffer` → `map_async` → un-pad rows;
`COPY_BYTES_PER_ROW_ALIGNMENT=256` padding is the #1 footgun), JPEG-encodes on a
blocking task, base64s, returns. Tools: `world_look` (the vision tool, returns an image
+ a text manifest of visible entities), `world_describe`/`world_list_entities` (cheap
structured text — the Nano/text-only fallback), `world_move`/`world_look_at`/
`world_activate`/`world_point` (advisory, tweened movement — the human watching sees the
agent walk). Read-only tools default `allow`; world-mutating verbs default `ask`.

### Feasibility verdict

| Capability | agentd change? | Mechanism |
|---|---|---|
| Chat / tools / approvals / cancel | **None** | `UserPrompt` ⇄ `AgentText`/`TurnComplete`; id-keyed tool round-trip |
| Sensors / sub-agents / A2A / council / mesh / evolution / vast | **None** | existing broadcast events |
| Cross-session send | **None** | `POST /api/sessions/:id/message` |
| Multi-session (many avatars) | **None** | session-per-socket + inbound filter |
| Agent-vision: plugin returns a base64 image | **None** | new `world-vision` plugin + local IPC bridge |
| **Agent-vision: the model actually SEES the image** | **SMALL CORE CHANGE** | provider content-shaping (see R1) |

**VERDICT: GO. The entire human/agent interaction surface plugs into agentd as-is, zero
core changes** — re-verified against code, not just types. The **one carve-out** is
agent-vision: the plugin half is fork-free, but making the *model* see the image is
**NOT** fork-free as the dimension docs claimed. This is reclassified below as a
**Decision + Risk R1**, not glossed.

---

## 5. Rendering approach + the de-risking path

**Composition: Pattern A** — Slint owns the window + winit event loop and draws all 2D
chrome and the activated function-UIs; the 3D scene renders to an offscreen wgpu texture
that Slint draws into a full-window `Image`. This preserves the project's load-bearing
invariant (Slint owns the main thread, never `#[tokio::main]`, tokio on background
threads, `invoke_from_event_loop` for UI mutation) and maximizes `ui-slint` reuse.
Fullscreen takeover is a Slint z-order change, not an event-loop handoff. WS ingest is
**decoupled** from rendering via channels (parse + push to mpsc in the read loop; never
render or block in it — a slow drain silently misses events past the 1024 broadcast cap).

The two critique-confirmed facts that reorder the *engine* choice (the dimension docs led
with Bevy + Mode I; the verified facts invert that):

### Decision: prototype the 3D side on Slint's own wgpu — defer Bevy

- **Slint exposes wgpu `28`, not `29`.** The feature is `unstable-wgpu-28`, the variant
  `WGPU28`, the module `slint::wgpu_28`. Every `wgpu-29`/`WGPU29`/`require_wgpu_29`
  snippet in the dimension docs and the seed skill is wrong and will not compile — use
  `28` everywhere. *(Critique R1-rendering, R11.)*
- **No released Bevy shares wgpu 28** (Bevy ≤0.19 is on wgpu ≤27). `cargo tree -d -i
  wgpu` over `slint(unstable-wgpu-28)` + any Bevy yields **two** wgpu versions, so the
  shared-device handoff cannot typecheck or link today. *(Critique R2-rendering,
  Critical.)* Therefore **Pattern A-lite is the primary path**, not insurance: a thin
  hand-rolled wgpu renderer using **Slint's own `wgpu_28` device** (guaranteed one wgpu
  in the tree). Bevy is a **deferred migration**, gated on a tracked trigger: *adopt Bevy
  the day `cargo tree -d -i wgpu` over the chosen Slint + Bevy yields exactly one wgpu*
  — recheck on every release. The host, the protocol mapping, the station screens, the
  tier gate, and all `ui-slint` reuse are identical either way. Cost honestly noted: on
  A-lite we hand-roll instancing/culling for the 50-avatar target (R7).

### Decision: Mode II (fullscreen takeover) is the primary surface; Mode I is a cuttable spike

- **Mode II** — the activated function-UI shown full-window over the dimmed 3D `Image`,
  a pure z-order/visibility change sharing the same `VecModel` (no state handoff, no
  re-subscribe). Uses only the documented full-window `Image::try_from(texture)` path.
  **Viable.**
- **Mode I** — a *live* Slint function-UI rendered to an offscreen texture sampled on a
  3D station quad ("the screen on the wall is alive before you walk up"). This is the
  headline mechanic but **rendering a Slint component to an offscreen texture is an open
  Slint feature request (#704), not a stable API**; the "hidden window per station"
  fallback is an unproven multi-surface config. *(Critique R3/R4/R8-rendering, High.)*
  **Mode I is speculative and the most likely feature to be cut.** Cutting it *improves*
  the perf picture (no K live Slint passes) — low regret. The station-texture pool, the
  K-cap LOD logic, and promotion/demotion machinery exist only to serve Mode I and are
  not built until Spike 2 proves Mode I is stably available.

### Tier gating

apexos-world is **Standard/Pro only** and **refuses cleanly** rather than degrading to
2D (a better 2D client, `apexos-rs-ui`, already exists). At launch it requires a real GPU
adapter and a GPU Slint backend (`winit`/`linuxkms`, never `linuxkms-femtovg`); on
Nano/Micro or no-GPU it prints a pointer to `apexos-rs-ui` and `exit(2)`. `WORLD_TIER`
env override + a `--force` dev bypass exist. Standard: K=4 live stations (if Mode I),
≤24 full avatars; Pro: K=8, ≤50.

### De-risking spike ladder (reordered per the critique — do before any scene/LOD/picking)

| Spike | Goal | Gate |
|---|---|---|
| **0** (½ day, reframed) | `cargo tree -d -i wgpu` on Slint(`unstable-wgpu-28`) + candidate Bevy | **expected: two versions → select A-lite.** Records pins; confirms the clash, does not block |
| **1'** (1 day) | A-lite triangle: Slint window, full-window `Image` fed by a hand-rolled wgpu renderer on **Slint's own `wgpu_28` device** | triangle spins at 60 fps, exactly one wgpu (guaranteed) — buildable *today* |
| **2** (make-or-break) | render a reused `ui-slint` component to an offscreen texture, sample on a 3D quad (2a: any stable component→texture path — expect none; 2b: hidden-window-per-station) | live Slint panel on a quad at acceptable cost. **If both fail → cut Mode I, ship Mode-II-only.** Decide *before* building the texture pool |
| **3** (parallel, low risk) | offscreen render + wgpu readback + JPEG + base64 against `wgpu_28` directly; **hand-roll** the readback; poll/map off the event loop | a JPEG back in <30 ms |
| **4** (agentd attach) | real WS client (`tokio-tungstenite`), session filter, one `ChatPanel` `VecModel` driven by `agent_text`, Mode II takeover on `E` | type at an avatar, see streamed text, against a live agentd |

---

## 6. Station + embodiment systems

**Stations** are a closed registry (`StationKind` enum + static `StationDesc` table):
each kind names a `BindingSpec` (`Session` / `BroadcastFilter` / `RestPoll` / `Council` /
`SideSocket` / `Tooled`), a `SurfaceFactory` (builds the reused Slint component with an
explicit `SessionScope`), its feeds, and a `tier_min`. Activation flow: look up the
descriptor → acquire the binding (e.g. for `Session`: receive `session_init`, bind the
station to that id; **never** send `session_init` — the gateway pushes it on connect) →
build the scoped surface → route inbound events where `event.session == id` → enter
FOCUS. Lifecycle: *placed → bound → focused → blurred → released*; sessions are sticky
across blur (replayable via `hello{resume_session}`), REST polls pause on blur.

**Adding a station** is a small localized diff (variant + descriptor row + scoped
surface + optional binding arm + world placement) — never an agentd change. **New agentd
capability** arrives only as an MCP plugin exposing tools consumed over `tool_result`.

**Generative UI** (`StationKind::Generative`) is the skill's headline idea mapped onto
real mechanics: the agent calls `world.render_ui`, the result returns via the normal
`tool_result` path carrying a small **closed, versioned `UiSpec`** (panel kinds:
`metric`, `gauge`, `bar_list`, `text`, `actions`, `image`) that maps to pre-compiled
Slint templates — data→templates, respecting Slint's compiled-not-runtime-widgets limit.
`actions` buttons carry a raw intent or a tool request, closing the loop; unknown kinds
render as a labeled fallback (forward-compatible).

**Embodiment** drives the *same* Bevy/scene `Transform`/`AvatarStatus` components that
the human sees: status fan-out (§3) and the act tools (`world_move` etc.) mutate one
shared scene, so when an agent walks to a station, the human watching sees it move while
the avatar's glow already reflects its `agent_text`/`tool_requested` activity. Movement
is advisory and **tweened, never snapped**; illegal moves return `ok:false`. v0 avatars
are primitives; glTF rigs (pose clips driven by `AvatarStatus.phase`) are v1.

**The local IPC** between `world-vision` and `world-app` is newline-delimited JSON over a
unix socket (`$XDG_RUNTIME_DIR/apexos-world.sock`, override `WORLD_IPC_PATH`) — or
localhost HTTP for the scaffold's simplicity — reusing the MCP-stdio pattern the team
knows. Only `world-app` binds; the plugin connects with backoff. If world-app is down,
every world tool returns `ok:false` "world renderer not running" (graceful degradation;
agentd's bounded tool timeout would synthesize an error anyway). On connect, world-app
pushes a **roster** (`session → avatar/label`, station names) so `world_look{view:"self"}`
resolves to the caller's body — with the correlation caveat in R2.

---

## 7. Decisions & risks (the critique blockers, folded in)

### Resolved decisions (conflicts between dimension docs)

- **D1 — wgpu version is `28`.** All `29` references in the dimension docs and seed skill
  are wrong. Do not re-copy the skill's `Cargo.toml` (it says 29).
- **D2 — Engine: Pattern A-lite (Slint's own `wgpu_28`) is the prototype path; Bevy is a
  deferred migration** gated on a single-wgpu-version trigger. (Dimension docs led with
  Bevy; verified facts invert this.)
- **D3 — Mode II is primary; Mode I is a make-or-break spike and is cuttable.** Do not
  build the live-texture pool / K-cap LOD until Spike 2 proves Mode I.
- **D4 — Session model: one WS socket per *active* session, opened lazily on activation;
  the inbound `session` filter is *defensive*, not load-bearing for correctness.** This
  is doc 06 + critique R6 over doc 02's Pattern-C-with-load-bearing-filter — one socket
  per session sidesteps the de-dup-or-chat-lines-double trap. A console socket catches
  ambient cross-session events. Approve/cancel require the session's own socket (R7).
- **D5 — `world-protocol` mirrors agentd's types (with a `// MIRRORS … keep in sync`
  banner + round-trip tests + `#[serde(other)] Unknown` fallback); it does not take a
  path dep on `apexos-core`.** This is doc 06's reasoning (keep `world/` cleanly
  extractable to its own repo; `apexos-core` is unpublished) over doc 02 + critique R9's
  preference for a hard dep. **Risk acknowledged:** mirroring re-introduces the
  wrong-field-name → silent-drop trap, *mitigated* by the round-trip tests and a
  live-daemon test each milestone. If `apexos-core` is ever published, revisit.
- **D6 — One world MCP plugin** hosts all server-side world tools (`world_look`,
  `world.render_ui`, act verbs). Memory-station routing may use `cerebro-mcp` directly.
- **D7 — Handshake: the server replies `session_init`, never `hello`.** Fix the stale
  doc 05 / CLAUDE.md text; a client coded to expect a `hello` reply would hang.
- **D8 — VR is a north-star, NOT a near-term toggle.** It requires Bevy-primary
  (Pattern B) owning the OpenXR swapchain/frame loop — the inverse of Pattern A — plus a
  three-way wgpu reconciliation. It is a separate binary and a re-architecture, out of
  scope until the desktop Bevy path itself is viable.

### Risk register (top items load-bearing for the build)

| # | Risk | Sev | Resolution |
|---|---|---|---|
| **R1** | **Agent-vision is NOT fork-free.** `turn.rs` puts raw `ToolOutput.content` into the tool-result block; `anthropic.rs`/`oai.rs` `.to_string()` any non-string content → an `{image_b64}` object reaches the model as literal **text**, never an image block. The "rides the normal `ToolResult.content` path, no fork" claim is **false**. | **High** | A **small, well-scoped core change** in `agentd/crates/agent`: teach the provider content-shaping to detect an MCP image content block in a tool result and emit the provider-native image block (Anthropic `{"type":"image","source":{base64…}}` / OAI `image_url`). Until then, **`world_describe` (structured text) is the only working vision path** — lead with it; it works against agentd exactly as-is. Reclassified in §4's verdict table. |
| **R2** | **The MCP plugin cannot learn the caller's `SessionId`** — agentd's `tools/call` forwards only `{name, arguments}`. So `world_look{view:"self"}` can't know which avatar's camera to use from agentd alone. | Med | Make correlation explicit + race-safe: either the **agent passes its avatar/session id as an explicit tool arg** (model-visible), **or** the plugin↔world-app IPC carries a nonce world-app matches to the `ToolRequested.id` (`ActionId`) it observed on its own `/ws` feed. `ActionId` is the only stable correlator both sides see. If unreliable, drop the `self` default and require an explicit target. |
| **R3** | **Bevy/Slint shared-wgpu version clash** (28 vs ≤27) makes the Bevy happy path un-buildable today. | Critical (for Bevy) | D2: build on A-lite; Bevy gated on the single-wgpu trigger. Spike 0 confirms, does not block. |
| **R4** | **Mode I (live Slint→texture) is an open feature request (#704), not a stable API**; the hidden-window fallback is unproven. | High | D3: Mode II primary; Spike 2 is make-or-break; ship Mode-II-only if it fails. |
| **R5** | **Broadcast lag = silent event loss** past the 1024 cap; the gateway drops to `continue` on `Lagged` with no gap signal. | Med | Keep the WS read loop parse-and-push-to-channel only; never render/block in it. Add a lag counter for observability. |
| **R6** | **De-dup correctness** under any multi-socket scheme (a session-scoped event arrives on every open socket). | Med | D4: one socket per session makes the filter defensive, not load-bearing — the safer default. |
| **R7** | **A-lite under-prices re-implementing instancing/culling** (= rebuilding the parts of Bevy you wanted) for the 50-avatar target. | Med | Accept as schedule cost; the 50-avatar target is a later milestone, not M0/M1. |
| **R8** | **Readback thread placement** — `device.poll(Wait)`/`map_async` must not run on the Slint main thread. | Low | Resolve poll/map on the renderer/tokio side, encode on `spawn_blocking`; never block the event loop. Prefer **hand-rolling** the readback over Bevy's churning `gpu_readback`. |
| **R9** | **`user_cancel` emits no `TurnComplete`**; **silent drop** of malformed intents. | Low | Client clears busy + tears down affordances on cancel; intent builders unit-tested for exact field names (`action` not `call_id`, `granted` not `approved`, no `session`). |

---

## 8. Milestone roadmap

Each milestone is a vertical slice that runs end-to-end against a **live agentd** (dev:
the Pi over LAN, `AGENTD_WS=ws://192.168.0.158:8787/ws` + token, or a local agentd). Each
gate = a commit.

| Milestone | Goal | Key acceptance |
|---|---|---|
| **Spikes 0–4** | De-risk before features (§5) | A-lite triangle at 60 fps; vision readback <30 ms; Mode-I cut/keep decided; chat streams via Mode II |
| **M0 — spine** | Hub scene + camera + connect + ONE real station | window >30 fps; `session_init` captured; walk to station → chat panel; type "hello" → streamed reply; reconnect on drop; frames with wrong `session` ignored |
| **M1 — populate** | Multiple stations + avatars + picking | ≥3 entities of ≥2 kinds; ray-pick highlight; two chat surfaces = two independent sessions (no cross-talk); live `sensor_reading`; tool cards with approve/reject sending `user_approval`; >30 fps with all bound |
| **M2 — agent-vision** | Embodied agent sees through its avatar camera | `world-vision` passes MCP handshake (logs to stderr); `plugin_up` carries `world_snapshot`/`world_look`; prompt → `tool_requested` → `tool_result` with a real frame; world-app down → clean `isError`. **Includes the R1 core provider change** (or ships `world_describe` text-only first) |
| **M3 — VR** | Same scene, stereo, on Quest 3 — **post-prototype, Pattern B re-architecture (D8)** | feature-gated; head tracking + controller-ray pick; desktop build unchanged. Gated on the desktop Bevy path first being viable |

Crate touch map: `world-protocol` written once in M0, only extended after (always
CI-green); `world-vision` buildable/handshake-testable from day one, real in M2;
`world-app` carries every milestone's net-new surface and the known-incomplete
integrations.

---

## 9. Anti-patterns this design rejects

- Don't rebuild agentd's UIs in 3D — the space *routes to* existing Slint views.
- Don't make distance cost time on a single node — teleport is first-class.
- Don't trap the user in a modal — every ACTIVE surface has a one-input exit.
- Don't invent a wire protocol — speak agentd's real `Event` enum; delete the seed
  skill's placeholder `AgentMessage`/`UiEvent`.
- Don't fork agentd core for world powers — new capability is MCP plugins only. *(The one
  unavoidable exception, R1's small provider content-shaping change, is named, not hidden.)*
- Don't assume `turn_started` — it does not exist; busy is `agent_text`, ended by
  `turn_complete`.
- Don't write `wgpu-29` — Slint is on `28`.
- Don't lead with Bevy or Mode I — they are deferred/cuttable until their spikes prove out.
