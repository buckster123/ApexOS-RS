# ApexOS-RS Extension Manifest

The consolidated agent-readable reference for **every extension point** across
the SDK. Use it to recall *how do I add a tool / event / app / plugin / policy
rule / memory verb / node*. Each row is a "to add X, edit these files, follow
this schema" recipe; the deep walkthrough for any row lives in its surface guide
([README](README.md) has the index). All `file:line` anchors are
ground-truthed; where this disagrees with `CLAUDE.md`, this is correct.

> **Two truths that govern everything below.**
> 1. **Runtime vs. compile-time.** An agent changes only *config* at runtime
>    (`soul.md`/`policy.toml`/plugin set) via `propose_evolution`. All new Rust
>    code (Event variant, tool, UI view, compiled plugin) is a human build +
>    hot-swap — the agent can propose it, not grant it.
> 2. **Safety is downstream.** Neither the protocol enum nor tool code is a
>    boundary. Every capability is gated by `PolicyEngine` (`policy.rs:88`) plus
>    the systemd sandbox. Adding a wire variant or a tool schema grants nothing
>    by itself.

---

## Recipes — "to add X, edit these files"

### Wire protocol (guide 01)

| To add… | Edit (in order) | Schema / signature | Gate / notes |
|---|---|---|---|
| **An outbound Event variant** | `core/src/types.rs` (declare in `Event`) → `core/src/state.rs` (exhaustive `apply` arm, no-op unless canonical state) → producer calls `bus.emit(...)` → client `dispatch_event` arm | `enum Event {#[serde(tag="type", rename_all="snake_case")] NewVariant { field: T }}` | Store + every gateway WS task relay automatically — usually zero consumer edits. Compile-time; not self-evolution. |
| **An inbound frontend intent** | `core/src/types.rs` → `core/src/state.rs` → `agentd/src/main.rs` (router match in `spawn_agent_router`, ~:969) | Variant MUST include `session: SessionId`; gateway injects it (`gateway/src/lib.rs:245`). Client sends frame **without** `session`. | Router has catch-all `Ok(_)=>{}` → unmatched variant is inert. A frame that fails deserialization is **silently dropped** (`lib.rs:246`) — test a real round-trip. |
| **A new client/frontend** | client-side only (no daemon files) | Connect `ws://HOST:8787/ws?token=<AGENTD_TOKEN>`; server pushes `session_init` first; send `user_prompt`/`user_approval`/`user_cancel` without `session`; resume via `{type:hello,resume_session:id}` | Client MUST filter inbound on `session` (gateway broadcasts every session to every socket). Busy ← `agent_text` (not `turn_started`). `user_cancel` emits no `turn_complete`. Approval = `{action:<numeric call.id>, granted:bool}`. |

### MCP plugin — new process (guide 02)

| To add… | Edit | Schema / signature | Gate / notes |
|---|---|---|---|
| **A new MCP plugin binary** | `tools/crates/<your-mcp>/src/main.rs` + `Cargo.toml` (new); add to root `Cargo.toml` workspace members (outside sandbox — flag for human) | Handle 4 methods: `initialize`→`{protocolVersion:"2024-11-05",capabilities:{tools:{}},serverInfo:{name,version}}`; `notifications/initialized`→no reply; `tools/list`→`{tools:[ToolSpec]}`; `tools/call(params{name,arguments})`→`{content:[{type:"text",text:<stringified-json>}],isError?:bool}`. **stdout = JSON-RPC only, one line per response, flush each; log to stderr.** | Runs as the agentd user inside the systemd sandbox — that is the boundary, not the tool code. |
| **Register the plugin** | `/etc/agentd/plugins.toml` (live) + `config/plugins.toml` (install template) | `[[plugin]]` `id`(req) `cmd`(req, abs path) `args`(`[str]`) `restart`(`always`\|`on-failure`\|`never`, default `never`) `cwd`? `[plugin.env]`? — `PluginConfig` (`config.rs:5-15`) | Only `always` auto-restarts (`handle_died` `supervisor.rs:1441`). Secrets via `[plugin.env]`, never `args` (args are logged + shown in UI). For an agent: reachable via `register_mcp_server` **only if the binary already exists on disk**. |

### apexos-tools — new built-in tool (guide 03)

| To add… | Edit | Schema / signature | Gate / notes |
|---|---|---|---|
| **A new system tool** | `tools/crates/apexos-tools/src/tools.rs` (3 edits) + `config/policy.toml` (1 line) | (1) `list()` (:10): append `{name, description, inputSchema:{type:"object",properties,required}}`. (2) `call()` (:302): add `"name" => name(args),` arm. (3) impl `fn name(args:&Value)->Value` returning `tool_ok(json!{...})`\|`tool_error(msg)`. (4) `[rules]`: `"name" = "allow"\|"ask"\|"workspace"`. | **No `plugins.toml` edit** — supervisor auto-registers from `tools/list`. Tool names are global (don't collide). Name the filesystem arg `path` for the `workspace` rule to engage (policy reads only `args["path"]`, `supervisor.rs:162`). `tool_error` only for "couldn't run" — a valid negative result is `tool_ok`. Build + hot-swap is a human step. |

### Cerebro — new memory verb (guide 04)

| To add… | Edit | Schema / signature | Gate / notes |
|---|---|---|---|
| **A new memory tool** | `cerebro/crates/cerebro-mcp/src/tools.rs` + `cerebro/crates/cerebro-mcp/src/dispatch.rs` | `tools.rs`: add name to `TOOL_NAMES` (:840) + `"name" => json!({name,description,inputSchema})` arm in `tool_schema()` (:11). `dispatch.rs`: add `"name" => { let scope = agent_scope(args); /* call brain.* */ Ok(json!(...)) }` in `route()` (:70). Sig: `async fn route(name:&str, args:&Value, brain:Arc<CerebroCortex>) -> anyhow::Result<Value>`. | Both halves required (schema-only = visible no-op; route-only = invisible). New verb defaults to Ask under `suggest` unless added to `policy.toml`; allow-list **read-only** verbs only. Confined by the systemd sandbox (DB under `/var/lib/agentd/cerebro`). |

### UI — new desktop app/view (guide 05)

| To add… | Edit | Schema / signature | Gate / notes |
|---|---|---|---|
| **A new app/view** | `ui-slint/src/ui/components/<name>_view.slint` (new) + `types.slint` (`AppKind` variant) + `components/app_window_frame.slint` (content arm) + `components/start_menu.slint` (launcher row) + `src/main.rs` (4 helper arms + data wiring) | New `export component MyAppView { in property <T> ...; callback do-thing(); }`; append `AppKind` variant; mirror ordinal in `kind_ordinal`/`kind_from_ordinal`/`kind_title`/`default_geom` (`main.rs:111-133`); `if root.kind == AppKind.x: MyAppView {...}` arm. **`AppKind` ordinal MUST agree with enum order.** | Almost always **zero agentd code**. Slint owns main thread (never `#[tokio::main]`); all UI mutation via `slint::invoke_from_event_loop`; lists are `Rc<VecModel<T>>` mutated on Slint thread only. `touch ui-slint/build.rs` to force `.slint` recompile. Rebuild + hot-swap (code commit, not a self-grant). |
| **Feed it from `/api` poll** | `ui-slint/src/main.rs` | `ui.on_<app>_refresh(move || rt_h.spawn(async move { /* http_client GET/POST */ invoke_from_event_loop(set_prop) }))`; add `AppKind::<X> => ui.invoke_<app>_refresh()` to `on_launch_app` (:1124). | Fetch is subject to agentd auth + policy. `/api/run`, `/api/soul` write, `/api/policy`, `/api/model`, `/api/power` are gated; read-only endpoints allowed. Shared `http_client` carries the bearer token (`main.rs:1227`). |
| **Drive it from a WS event** | `ui-slint/src/main.rs` | Add a `match ev_type` arm in `dispatch_event` (:1686) keyed on the event's `type` string; mutate a `VecModel`/property inside `invoke_from_event_loop`. | UI only renders. Emitting the Event is an agentd concern (guide 01). Filter on the bare-number `session` field for multi-client. |
| **Launcher / persona gating** | `ui-slint/src/ui/components/start_menu.slint` | Core: `MenuRow { glyph; label; clicked => { root.launch(<ord>); } }`. Deep-tech: wrap in `if Personas.show-tech-apps:` (`personas.slint:32`). | Pure presentation; no policy gate. |

### Self-evolution — runtime config change (guide 06)

| To add / do… | Tool call | Schema / signature | Gate / notes |
|---|---|---|---|
| **Change a policy rule** | `propose_evolution` | `{kind:"update_policy_rule", tool_pattern, new_rule:"allow"\|"ask"\|"workspace", reason}` | `new_rule` is a **PolicyRule, not PolicyMode**. Rollback-able only if the rule already existed. |
| **Edit soul.md** | `read_soul_md` then `propose_evolution` | `{kind:"update_system_prompt", content:<full new soul.md>, reason}` | Full replacement, not diff. MUST `read_soul_md` first. Live Arc swapped immediately. Written non-atomically. |
| **Add/remove an MCP plugin** | `propose_evolution` | `{kind:"register_mcp_server", name, command, env:{}, reason}` \| `{kind:"unregister_mcp_server", name, reason}` | Highest-trust kind: arbitrary process spawn, confined only by sandbox. `register` undo = unregister; `unregister` undo loses env. Binary must already exist on disk. |
| **Hot-reload a subsystem** | `propose_evolution` | `{kind:"hot_reload_subsystem", subsystem:"plugins"\|"policy"\|"agent"\|"gateway"}` | **NO undo.** `plugins`=no-op, `gateway`=unsupported without restart. |
| **Roll back an evolution** | `rollback_evolution` | `{evolution_id:int, reason}` — `evolution_id` = the original `propose_evolution` call's `ToolCall.id` | In-memory `rollback_store`, **current daemon session only**; cold-start rebuild from Cerebro is best-effort. Returns "no rollback snapshot" if undo absent. |
| **Journal the rationale (mandatory)** | `memory_store` | `{content:WHY, type:"semantic", salience:0.9, tags:["evolution","rationale"], agent_id:"CLAUDE-APEX"}` | The daemon journals the undo snapshot automatically but NEVER the rationale. Omitting it = identity drift (symbiosis.md §5). |
| **Add a new EvolutionProposal kind (Rust)** | — | (1) variant in `EvolutionProposal` (`types.rs:88`, snake_case tag) (2) `apply_evolution` arm (`main.rs:808`) (3) `compute_undo` arm (`main.rs:712`) (4) `propose_evolution_spec` enum+args (`main.rs:1257`) (5) `soul.md` self-evolution table row | Validate-before-persist (parse candidate before writing, like `update_policy_rule` `main.rs:843`); use `write_atomic` for `/etc/agentd`. Normal git discipline. |

> **There is NO `evolution.*` policy namespace.** The gate is the literal tool
> name (`propose_evolution` / `rollback_evolution` / `read_soul_md`), unlisted in
> `policy.toml` → defaults to **Ask** under `suggest` mode. Every self-evolution
> needs approval by default. Do NOT bypass this by inventing an Event that writes
> config directly — that loses audit + undo.

### Mesh & deployment (guide 07)

| To add… | Edit | Schema / signature | Gate / notes |
|---|---|---|---|
| **A hardware tier** | `install.sh` | tier detect `if (( RAM_MB < N )); then TIER="name"` (:359); `TIER_DESC` case (:372); `EMBED_MODEL` case (:692) → `CEREBRO_EMBED_MODEL` in `plugins.toml` (:700) | Install-time only, no Rust. Gates Cerebro embed model / RSS. |
| **A deployment mode** | `install.sh` | auto-detect branch (:367); component gate via `NO_UI`/`NO_SENSOR`/`NO_CEREBRO_API` (:430); `install_svc`/`systemctl enable` gating (:773,:779) | Install-time only. Gates which systemd services install. |
| **A mesh node (peer)** | runtime: `POST /api/mesh/peers` (no source edit) — or `gateway/src/mesh.rs` to change schema/roles | `POST /api/mesh/peers {node_id, ws_url, role?(full\|sensor\|thin)}` (`lib.rs:1496`). Discovery: `spawn_discovery_loop` (`main.rs:1699`) emits `PeerSeen`. Route: `send_to_agent{node,session_id,message}` (`supervisor:557`). | `send_to_agent` unlisted → Ask. Cross-node POST is unauthenticated (trusted-LAN primitive); subnet guard `/24` on. **Live bug:** cross-node send writes `{"text"}` but handler reads `{"message"}`. |
| **A vast.ai GPU recipe** | `/etc/agentd/recipes.toml` (not auto-created) — or `vast.rs` for schema | `[[recipes]] {name,label,gpu,model_repo,model_quant,ctx,parallel,kv_type,description}`; `[gpu_tiers.<key>]`; `[docker].prebuilt`. `load_recipes()` `vast.rs:43`. Lifecycle: `vast_launch` → `VastInstanceReady` → backend hot-swap (`main.rs:386`). | `vast_launch`/`vast_destroy` unlisted → Ask (spends money). Needs `VAST_API_KEY` + `vastai` CLI. Instance persists across restarts — reconcile `vast_status` after reboot. |
| **A systemd service** | `deploy/<name>.service` + `install.sh` | Template = `deploy/agentd.service`: `User=agentd`, `NoNewPrivileges=true`, `ProtectSystem=strict`, `ProtectHome=true`, `PrivateTmp=true`, `ReadWritePaths=/var/lib/agentd /etc/agentd`, `EnvironmentFile=-/etc/agentd/env`, `WantedBy=multi-user.target`. Wire via `install_svc`/`systemctl enable` (:760-781). | Never drop the sandbox. Hardware → device allowlist (`DevicePolicy=closed` + `DeviceAllow`). Root reserved for `apexos-rs-ui` (DRM master) only. |

---

## Catalog — tool names, arg schemas, Event variants

### Event enum (`core/src/types.rs:162`, `#[serde(tag="type", rename_all="snake_case")]`)

ID newtypes `SessionId`/`ActionId`/`EvolutionId` (u64) and `PluginId` (String)
serialize as **bare scalars**, not strings. The gateway injects `session` into
inbound frames; clients omit it.

**Inbound (client → daemon, omit `session`):**

| Event | Fields |
|---|---|
| `hello` | `resume_session: u64?` |
| `user_prompt` | `text` |
| `user_approval` | `action: u64`, `granted: bool` |
| `user_cancel` | — |

**Outbound (daemon → client):**

| Event | Fields |
|---|---|
| `session_init` | `session_id: u64`, `history: Message[]` (server-PUSHED on connect) |
| `agent_text` | `session`, `delta` (drives busy state) |
| `agent_thinking` | `session`, `delta` |
| `tool_requested` | `session`, `call: ToolCall` |
| `tool_result` | `session`, `call: u64` (bare), `output: ToolOutput` |
| `approval_pending` | `session`, `call: ToolCall` |
| `turn_complete` | `session` |
| `plugin_up` / `plugin_down` | `plugin, tools: ToolSpec[]` / `plugin, reason` |
| `spawn_agent` / `sub_agent_started` | `parent, call_id, prompt, system?` / `parent, child, prompt` |
| `sensor_reading` | `node_id`, `reading: SensorReading`, `timestamp: u64` |
| `wake_triggered` | — |
| `agent_message` / `agent_message_ack` | `from, to, body, msg_id` / `msg_id, from` |
| `council_*` | `council_started/round_start/agent_delta/agent_done/round_done/complete/butt_in` (:217-224) |
| `error` | `session: u64?`, `message` |
| `vast_*` | `vast_instance_launched/ready/destroyed`, `vast_tunnel_lost` (:230-237) |
| `peer_*` | `peer_seen/peer_registered/peer_lost` (:241-245) |
| `evolution_*` | `evolution_proposed{id,proposal,proposed_by}` / `evolution_applied` / `evolution_rolled_back` (:250-267) |

> **`turn_started` is NOT emitted by the Rust daemon.** Busy is driven by
> `agent_text`. `needs_approval` is hardcoded `false` by the agent
> (`turn.rs:118`) — rely on the `approval_pending` event for gating.

**Nested structs / enums:**

- `ToolCall{ id:ActionId, tool:String, args:Value, needs_approval:bool }` (:272)
- `ToolOutput{ ok:bool, content:Value }` (:281)
- `ToolSpec{ name, description, input_schema }` (:287)
- `ContentBlock` (tag `type`): `text` / `thinking`(+`signature`) / `tool_use` / `tool_result` (:329)
- `Message` (tag `role`): `user` / `assistant` (:322)
- `SensorReading` (tag `kind`): `temperature/humidity/pressure/motion/distance/gpio_level/air_quality/thermal_frame` (:121)
- `EvolutionProposal` (tag `kind`): `register_mcp_server/unregister_mcp_server/update_policy_rule/update_system_prompt/hot_reload_subsystem` (:88)
- `PolicyMode` (global, kebab): `suggest`(default) `auto-edit` `yolo` (:29)
- `PolicyRule` (per-tool `[rules]` value, kebab): `allow` `ask` `workspace` (:45)
- `Subsystem` (snake): `plugins/policy/agent/gateway` (:77)

### Bus & policy

- `BusHandle::emit(Event).await`; broadcast capacity 1024 (`bus.rs`).
  `SystemState{sessions,tools,plugins,pending_approvals}` mutated only in
  `apply()` (`state.rs:18`).
- `PolicyEngine.check()` (`policy.rs:88`): `yolo` short-circuits Allow →
  exact tool key → `prefix.*` wildcard (matches `prefix.<x>`, not bare `prefix`,
  `:142`) → unknown defaults to **Ask** (:111). `workspace` canonicalizes the
  `path` arg inside `AGENTD_WORKSPACE`, rejects `..` (:118).

### MCP JSON-RPC (agentd → plugin, protocol 2024-11-05)

| Method | Request → reply |
|---|---|
| `initialize` | → `{protocolVersion:"2024-11-05",capabilities:{tools:{}},serverInfo:{name,version}}` |
| `notifications/initialized` | notification, no id, no reply |
| `tools/list` | → `{tools:[{name(req), description?, inputSchema?}]}` → `ToolSpec` (no `name` ⇒ dropped) |
| `tools/call` | params `{name,arguments}` → `{content:[…], isError?:bool}` → `ToolOutput{ok = !isError, content}` |

Envelope helpers: `tool_ok(c)`→`{"content":[{"type":"text","text":<json-string>}]}`;
`tool_error(m)`→ same + `"isError":true`. Top-level JSON-RPC `error` = transport
error, aborts the call.

### apexos-tools — existing tool names (global; don't collide)

`run_command read_file write_file list_dir create_dir delete_path http_fetch
cpu_temp disk_usage memory_info uptime notify audio_analyze audio_trim_silence
audio_normalize audio_peak_limit audio_trim audio_clean gpio_info gpio_read
gpio_write gpio_pulse gpio_pwm gpio_servo display_face`

Confinement is honest: only `write_file`/`create_dir` use the `workspace` rule,
only `delete_path` self-roots to `AGENTD_WORKSPACE`; every other tool is
unconfined and the real boundary is the systemd sandbox. The `run_command`
denylist is a bypassable heuristic, not security. `SupervisorCmd::CallTool`
(`supervisor.rs:30`) dispatches **without** a policy check.

### Cerebro — core memory verbs (`name | required args | key optional | backing`)

`remember | content | memory_type,tags,salience,agent_id | →MemoryNode` ·
`memory_store`(alias of remember) · `recall | query | top_k,agent_id |
→[{memory,score}]` · `memory_search`(alias) · `associate | source_id,target_id |
link_type(semantic),weight(0.5)` · `get_memory`/`update_memory`(re-embeds if
content changed)/`delete_memory`(soft) · `session_save | content |
priority,session_type,salience,agent_id` · `session_recall | query |
top_k,priority,session_type,agent_id` · `episode_start`/`episode_add_step`/`episode_end`
· `store_intention | content | salience(0.7),tags` · `list_intentions` ·
`resolve_intention | memory_id` · `store_procedure | content |
tags,derived_from`(salience 0.8) · `list_procedures` · `find_relevant_procedures
| tags OR concepts | limit(5)` · `record_procedure_outcome | procedure_id,success`
· `create_schema | content,source_ids` · `check_inbox`/`send_message |
content,to_agent_id`/`share_memory | memory_id` · `register_agent | name` ·
`dream_run | — | agent_id,max_llm_calls(20,max20)` · `dream_status`.

Plus CRUD/graph/analytics/tags/audit/versions/threads/episodes families (see
guide 04 catalog). **Scoping:** `agent_id` set → `VisibilityScope::for_agent`
(own private + shared); absent → global (shared only). Write visibility derived
from scope (scoped→Private, unscoped→Shared); the schema `visibility` arg is
unread. Conventions: FORGE→`"FORGE"`, APEX→`"CLAUDE-APEX"`.

**Stubs (advertised, NOT routed — return `not_yet_implemented`):**
`cognitive_bootstrap`, `ingest_file`, `describe_image`, `search_vision`. Caveat:
reinforcement is inert (recall doesn't bump activation) and spreading activation
ignores scope — treat scope as a best-effort read filter, not hard isolation.

### Virtual tools (agentd-built-in, intercepted in `supervisor.rs` `dispatch_tool`)

| Tool | Signature | Spec / intercept |
|---|---|---|
| `read_soul_md` | `()` → live soul.md string | spec `main.rs:1243`, intercept `supervisor.rs:399` |
| `propose_evolution` | `(kind, reason, +per-kind args)` → `{status:"proposed", evolution_id}` | spec `main.rs:1257`, intercept `supervisor.rs:302` |
| `rollback_evolution` | `(evolution_id:int, reason)` → `{status:"rolled_back", summary}` | spec `main.rs:1321`, intercept `supervisor.rs:349` |
| `list_mesh_peers` | `()` → peers.toml text | `main.rs:1473-1599`, `supervisor.rs` |
| `send_to_agent` | `(session_id:int, message:str, node?:str)` | `supervisor:557` (cross-node bug: sends `{"text"}`, handler reads `{"message"}`) |
| `bootstrap_node` | `(target_ip, ssh_password, ssh_user?=apexos, api_key?, repo_url?)` | needs `sshpass` (not auto-installed) |
| `vast_list_recipes` / `vast_launch` / `vast_destroy` / `vast_status` | recipe array / `(recipe, geo?=EU_NORDIC)`→`{status:ready,...}` / teardown / phase | all default to Ask |

### Mesh REST routes (`gateway/src/lib.rs`)

`GET /api/mesh/nodes` (:1453) · `GET /api/mesh/peers` (:1489) ·
`POST /api/mesh/peers {node_id,ws_url,role?}` (:1496, emits `PeerRegistered`) ·
`DELETE /api/mesh/peers/{id}` (:1532) · `GET /api/sessions/active` (:693) ·
`POST /api/sessions/{id}/message {message}` (:712, A2A landing, emits
`UserPrompt`) · `GET/POST /api/backend {backend,oai_base_url?,model?}`
(:522/:529, live hot-swap).

### UI surface (`ui-slint`)

- `AppKind` ordinals (`types.slint:13`, mirrored `main.rs:111-133`): `chat=0,
  system=1, sensor=2, sessions=3, settings=4, terminal=5, council=6` (append
  new variants).
- `WindowDesc{ id, kind:AppKind, title, x/y/w/h, minimized, maximized }`
  (`types.slint:52-62`); `WINDOWS` VecModel order == z-order.
- Thread-local models (`main.rs:25-54`): `MESSAGES, SESSIONS, MODELS, TOASTS,
  NOTIF_LOG, WINDOWS, COUNCIL` — mutated on the Slint thread only.
- REST base = `ws_to_http(AGENTD_WS)` (`main.rs:733`); shared `http_client`
  carries the bearer token (`main.rs:1227`).

### Key environment variables

`AGENTD_WS` (`ws://localhost:8787/ws`) · `AGENTD_BIND` (`127.0.0.1:8787`;
non-loopback requires `AGENTD_TOKEN`) · `AGENTD_TOKEN` (gates `/ws` via `?token=`
and `/api/*` via Bearer) · `SENSOR_BRIDGE_TOKEN` · `AGENTD_TOOL_RESULT_TIMEOUT_SECS`
(1800) · `AGENTD_WORKSPACE` (workspace root for the `workspace` rule) ·
`CEREBRO_EMBED_MODEL` (`""`→FTS5-only ~23 MB) · `SLINT_BACKEND` /
`SLINT_FULLSCREEN` · `MESH_DISCOVERY_INTERVAL` (60) · `MESH_SUBNET_GUARD` (on,
/24) · `PEERS_TOML` / `RECIPES_TOML` (`/etc/agentd/...`) · `VAST_API_KEY` (req) ·
`VAST_DEFAULT_GEO` (EU_NORDIC) · `VAST_LOCAL_PORT` (8000).
