# ApexOS-RS Extension Manifest

The consolidated agent-readable reference for **every extension point** across
the SDK. Use it to recall *how do I add a tool / event / app / plugin / policy
rule / memory verb / node*. Each row is a "to add X, edit these files, follow
this schema" recipe; the deep walkthrough for any row lives in its surface guide
([README](README.md) has the index). All `file:line` anchors are
ground-truthed (last re-swept 2026-08-06, after the Fabrica worker/mandala
tool family landed); where this disagrees with `CLAUDE.md`, this is correct.

> **Two truths that govern everything below.**
> 1. **Runtime vs. compile-time.** An agent changes only *config* at runtime
>    (`soul.md`/`policy.toml`/plugin set) via `propose_evolution`. All new Rust
>    code (Event variant, tool, UI view, compiled plugin) is a human build +
>    hot-swap — the agent can propose it, not grant it.
> 2. **Safety is downstream.** Neither the protocol enum nor tool code is a
>    boundary. Every capability is gated by `PolicyEngine` (`policy.rs:106`) plus
>    the systemd sandbox. Adding a wire variant or a tool schema grants nothing
>    by itself.

---

## Recipes — "to add X, edit these files"

### Wire protocol (guide 01)

| To add… | Edit (in order) | Schema / signature | Gate / notes |
|---|---|---|---|
| **An outbound Event variant** | `apexos-protocol/src/lib.rs` (declare in `Event`, :306) → `core/src/state.rs` (exhaustive `apply` arm, no-op unless canonical state) → producer calls `bus.emit(...)` → client `dispatch_event` arm | `enum Event {#[serde(tag="type", rename_all="snake_case")] NewVariant { field: T }}` | Store + every gateway WS task relay automatically — usually zero consumer edits. Compile-time; not self-evolution. **The crate is `no_std`-capable with an external bare-metal consumer (ApexOS-RV):** map-bearing fields use the crate's `Map<K,V>` alias (never `HashMap` directly), no bare `std::` paths, ID newtypes derive `Ord`; run BOTH `cargo test -p apexos-protocol` and `--no-default-features --features alloc`. |
| **An inbound frontend intent** | `apexos-protocol/src/lib.rs` → `core/src/state.rs` → `agentd/src/main.rs` (router match in `spawn_agent_router`, :1588) | Variant MUST include `session: SessionId`; gateway injects it (`gateway/src/lib.rs:705`). Client sends frame **without** `session`. | Router has catch-all `Ok(_)=>{}` → unmatched variant is inert. A frame that fails deserialization is **silently dropped** (the WS read loop, `lib.rs:619`) — test a real round-trip. |
| **A new client/frontend** | client-side only (no daemon files) | Connect `ws://HOST:8787/ws?token=<AGENTD_TOKEN>`; server pushes `session_init` first; send `user_prompt`/`user_approval`/`user_cancel` without `session`; resume via `{type:hello,resume_session:id}` | Client MUST filter inbound on `session` (gateway broadcasts every session to every socket). Busy ← `agent_text` (not `turn_started`). `user_cancel` emits no `turn_complete`. Approval = `{action:<numeric call.id>, granted:bool}`. |

### MCP plugin — new process (guide 02)

| To add… | Edit | Schema / signature | Gate / notes |
|---|---|---|---|
| **A new MCP plugin binary** | `tools/crates/<your-mcp>/src/main.rs` + `Cargo.toml` (new); add to root `Cargo.toml` workspace members (outside sandbox — flag for human) | Handle 4 methods: `initialize`→`{protocolVersion:"2024-11-05",capabilities:{tools:{}},serverInfo:{name,version}}`; `notifications/initialized`→no reply; `tools/list`→`{tools:[ToolSpec]}`; `tools/call(params{name,arguments})`→`{content:[{type:"text",text:<stringified-json>}],isError?:bool}`. **stdout = JSON-RPC only, one line per response, flush each; log to stderr.** | Runs as the agentd user inside the systemd sandbox — that is the boundary, not the tool code. |
| **Register the plugin** | `/etc/agentd/plugins.toml` (live) + `config/plugins.toml` (install template) | `[[plugin]]` `id`(req) `cmd`(req, abs path) `args`(`[str]`) `restart`(`always`\|`on-failure`\|`never`, default `never`) `cwd`? `[plugin.env]`? — `PluginConfig` (`config.rs:5-15`) | Only `always` auto-restarts (`handle_died` `supervisor.rs:1441`). Secrets via `[plugin.env]`, never `args` (args are logged + shown in UI). For an agent: reachable via `register_mcp_server` **only if the binary already exists on disk**. |

### apexos-tools — new built-in tool (guide 03)

| To add… | Edit | Schema / signature | Gate / notes |
|---|---|---|---|
| **A new system tool** | `tools/crates/apexos-tools/src/tools.rs` (3 edits) + `config/policy.toml` (1 line) | (1) `list()` (:11): append `{name, description, inputSchema:{type:"object",properties,required}}`. (2) `call()` (:589): add `"name" => name(args),` arm. (3) impl `fn name(args:&Value)->Value` returning `tool_ok(json!{...})`\|`tool_error(msg)`. (4) `[rules]`: `"name" = "allow"\|"ask"\|"workspace"`. | **No `plugins.toml` edit** — supervisor auto-registers from `tools/list`. Tool names are global (don't collide). Name filesystem args with a path-typed key (`path`/`output_path`/`dest`/`destination`/`target`/`to`) for the `workspace` rule to engage — the supervisor feeds every path-typed arg to policy, most-restrictive wins (`supervisor.rs:389`). `tool_error` only for "couldn't run" — a valid negative result is `tool_ok`. Build + hot-swap is a human step. |

### Cerebro — new memory verb (guide 04)

| To add… | Edit | Schema / signature | Gate / notes |
|---|---|---|---|
| **A new memory tool** | `cerebro/crates/cerebro-mcp/src/tools.rs` + `cerebro/crates/cerebro-mcp/src/dispatch.rs` | `tools.rs`: add name to the `TOOL_NAMES` const + `"name" => json!({name,description,inputSchema})` arm in `tool_schema()`. `dispatch.rs`: add `"name" => { let scope = agent_scope(args); /* call brain.* */ Ok(json!(...)) }` in `route()`. Sig: `async fn route(name:&str, args:&Value, brain:Arc<CerebroCortex>) -> anyhow::Result<Value>`. | Both halves required (schema-only = visible no-op; route-only = invisible). New verb defaults to Ask under `suggest` unless added to `policy.toml`; allow-list **read-only** verbs only. Confined by the systemd sandbox (DB under `/var/lib/agentd/cerebro`). |

### UI — new desktop app/view (guide 05)

| To add… | Edit | Schema / signature | Gate / notes |
|---|---|---|---|
| **A new app/view** | `ui-slint/src/ui/components/<name>_view.slint` (new) + `types.slint` (`AppKind` variant) + `components/app_window_frame.slint` (content arm) + `components/start_menu.slint` (launcher row) + `src/main.rs` (4 helper arms + data wiring) | New `export component MyAppView { in property <T> ...; callback do-thing(); }`; append `AppKind` variant; mirror ordinal in `kind_ordinal`/`kind_from_ordinal`/`kind_title`/`default_geom` (all in `ui-slint/src/main.rs`); `if root.kind == AppKind.x: MyAppView {...}` arm. **`AppKind` ordinal MUST agree with enum order.** | Almost always **zero agentd code**. Slint owns main thread (never `#[tokio::main]`); all UI mutation via `slint::invoke_from_event_loop`; lists are `Rc<VecModel<T>>` mutated on Slint thread only. `touch ui-slint/build.rs` to force `.slint` recompile. Rebuild + hot-swap (code commit, not a self-grant). |
| **Feed it from `/api` poll** | `ui-slint/src/main.rs` | `ui.on_<app>_refresh(move || rt_h.spawn(async move { /* http_client GET/POST */ invoke_from_event_loop(set_prop) }))`; add `AppKind::<X> => ui.invoke_<app>_refresh()` to `on_launch_app` (:7143). | Fetch is subject to agentd auth + policy. `/api/run`, `/api/soul` write, `/api/policy`, `/api/model`, `/api/power` are gated; read-only endpoints allowed. Shared `http_client` carries the bearer token (`main.rs:7309`). |
| **Drive it from a WS event** | `ui-slint/src/main.rs` | Add a `match ev_type` arm in `dispatch_event` (:10569) keyed on the event's `type` string; mutate a `VecModel`/property inside `invoke_from_event_loop`. | UI only renders. Emitting the Event is an agentd concern (guide 01). Filter on the bare-number `session` field for multi-client. |
| **Launcher / persona gating** | `ui-slint/src/ui/components/start_menu.slint` | Core: `MenuRow { glyph; label; clicked => { root.launch(<ord>); } }`. Deep-tech: wrap in `if Personas.show-tech-apps:` (`personas.slint:32`). | Pure presentation; no policy gate. |

### Self-evolution — runtime config change (guide 06)

| To add / do… | Tool call | Schema / signature | Gate / notes |
|---|---|---|---|
| **Change a policy rule** | `propose_evolution` | `{kind:"update_policy_rule", tool_pattern, new_rule:"allow"\|"ask"\|"workspace", reason}` | `new_rule` is a **PolicyRule, not PolicyMode**. Rollback-able only if the rule already existed. |
| **Edit soul.md** | `read_soul_md` then `propose_evolution` | `{kind:"update_system_prompt", content:<full new soul.md>, reason}` | Full replacement, not diff. MUST `read_soul_md` first. Live Arc swapped immediately. Written non-atomically. |
| **Add/remove an MCP plugin** | `propose_evolution` | `{kind:"register_mcp_server", name, command, env:{}, reason}` \| `{kind:"unregister_mcp_server", name, reason}` | Highest-trust kind: arbitrary process spawn, confined only by sandbox. `register` undo = unregister; `unregister` undo loses env. Binary must already exist on disk. |
| **Hot-reload a subsystem** | `propose_evolution` | `{kind:"hot_reload_subsystem", subsystem:"plugins"\|"policy"\|"agent"\|"gateway"}` | **NO undo.** `plugins`=no-op, `gateway`=unsupported without restart. |
| **Roll back an evolution** | `rollback_evolution` | `{evolution_id:int, reason}` — `evolution_id` = the original `propose_evolution` call's `ToolCall.id` | In-memory `rollback_store`, **current daemon session only**; cold-start rebuild from Cerebro is best-effort. Returns "no rollback snapshot" if undo absent. |
| **Journal the rationale (mandatory)** | `memory_store` | `{content:WHY, type:"semantic", salience:0.9, tags:["evolution","rationale"]}` (`agent_id` is system-stamped by agentd — default `APEX`) | The daemon journals the undo snapshot automatically but NEVER the rationale. Omitting it = identity drift (symbiosis.md §5). |
| **Add a new EvolutionProposal kind (Rust)** | — | (1) variant in `EvolutionProposal` (`apexos-protocol/src/lib.rs:212`, snake_case tag) (2) `apply_evolution` arm (`main.rs:1469`) (3) `compute_undo` arm (`main.rs:1389`) (4) `propose_evolution_spec` enum+args (`main.rs:3045`) (5) `soul.md` self-evolution table row | Validate-before-persist (parse candidate before writing, like `update_policy_rule` `main.rs:1500`); use `write_atomic` for `/etc/agentd`. Normal git discipline. |

> **There is NO `evolution.*` policy namespace.** The gate is the literal tool
> name — `config/policy.toml` now seeds `read_soul_md = "allow"` and
> `propose_evolution` / `rollback_evolution` = `"ask"` (an *unlisted* tool still
> defaults to **Ask** under `suggest` mode). Every self-evolution needs approval
> by default. Do NOT bypass this by inventing an Event that writes config
> directly — that loses audit + undo.

### Mesh & deployment (guide 07)

| To add… | Edit | Schema / signature | Gate / notes |
|---|---|---|---|
| **A hardware tier** | `install.sh` | tier detect `if (( RAM_MB < N )); then TIER="name"` (:359); `TIER_DESC` case (:372); `EMBED_MODEL` case (:692) → `CEREBRO_EMBED_MODEL` in `plugins.toml` (:700) | Install-time only, no Rust. Gates Cerebro embed model / RSS. |
| **A deployment mode** | `install.sh` | auto-detect branch (:367); component gate via `NO_UI`/`NO_SENSOR`/`NO_CEREBRO_API` (:430); `install_svc`/`systemctl enable` gating (:773,:779) | Install-time only. Gates which systemd services install. |
| **A mesh node (peer)** | runtime: `POST /api/mesh/peers` (no source edit) — or `gateway/src/mesh.rs` to change schema/roles | `POST /api/mesh/peers {node_id, ws_url, role?(full\|sensor\|thin)}` (`lib.rs:4051`). Discovery: `spawn_discovery_loop` (`main.rs:3811`) emits `PeerSeen`. Route: `send_to_agent{node,session_id,message}` (`supervisor.rs:1015`). | `send_to_agent` is seeded `allow` (see Virtual tools). Cross-node send posts `{"message"}` + auto-stamped `origin_session` (`supervisor.rs:1060`) — the old `{"text"}`-vs-`{"message"}` mismatch is fixed. |
| **A vast.ai GPU recipe** | `/etc/agentd/recipes.toml` (not auto-created) — or `vast.rs` for schema | `[[recipes]] {name,label,gpu,model_repo,model_quant,ctx,parallel,kv_type,description}`; `[gpu_tiers.<key>]`; `[docker].prebuilt`. `load_recipes()` `vast.rs:43`. Lifecycle: `vast_launch` → `VastInstanceReady` → backend hot-swap (`main.rs:386`). | `vast_launch`/`vast_destroy` unlisted → Ask (spends money). Needs `VAST_API_KEY` + `vastai` CLI. Instance persists across restarts — reconcile `vast_status` after reboot. |
| **A systemd service** | `deploy/<name>.service` + `install.sh` | Template = `deploy/agentd.service`: `User=agentd`, `NoNewPrivileges=true`, `ProtectSystem=strict`, `ProtectHome=true`, `PrivateTmp=true`, `ReadWritePaths=/var/lib/agentd /etc/agentd`, `EnvironmentFile=-/etc/agentd/env`, `WantedBy=multi-user.target`. Wire via `install_svc`/`systemctl enable` (:760-781). | Never drop the sandbox. Hardware → device allowlist (`DevicePolicy=closed` + `DeviceAllow`). Root reserved for `apexos-rs-ui` (DRM master) only. |

---

## Catalog — tool names, arg schemas, Event variants

### Event enum (`apexos-protocol/src/lib.rs:306`, `#[serde(tag="type", rename_all="snake_case")]`)

ID newtypes `SessionId`/`ActionId`/`EvolutionId`/`GoalId`/`WorkerId` (u64) and
`PluginId` (String) serialize as **bare scalars**, not strings (all derive `Ord`
— the `no_std` consumer keys BTreeMaps with them). The gateway injects `session`
into inbound frames; clients omit it.

**Inbound (client → daemon, omit `session`):**

| Event | Fields |
|---|---|
| `hello` | `resume_session: u64?`, `new: bool?`, `agent_id?`, `persona?` — gateway frame, not an `Event` variant (`gateway/src/lib.rs:621`) |
| `set_persona` | `persona` — gateway frame (`gateway/src/lib.rs:693`) |
| `user_prompt` | `text`, `images: ImageSource[]?` |
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
| `sensor_alert` | `node_id`, `kind` (`cpu_temp`\|`motion`\|`air_quality`\|`thermal_hotspot`), `value`, `threshold`, `sensor_id` — fired only after the persistence filter + cooldown; GLOBAL (:356) |
| `wake_triggered` | — |
| `agent_message` / `agent_message_ack` | `from, to, body, msg_id` / `msg_id, from` |
| `council_*` | `council_started/round_start/agent_delta/agent_done/round_done/complete/butt_in` (:376-383) |
| `error` | `session: u64?`, `message` |
| `vast_*` | `vast_instance_launched/ready/destroyed`, `vast_tunnel_lost` (:389-400) |
| `mesh_message` | `from_node, session, preview` — inbound mesh a2a landed; GLOBAL notification (:410) |
| `mesh_memory_shared` | `from_node, memory_id, preview` — federation import; GLOBAL (:416) |
| `peer_*` | `peer_seen/peer_registered/peer_lost` (:418-422) |
| `mesh_node_status` | `node_id, status ("dark"\|"alive"), last_seen_secs` — downtime-beacon edge; GLOBAL (:429) |
| `evolution_*` | `evolution_proposed{id,proposal,proposed_by}` / `evolution_applied` / `evolution_rolled_back` (:434-451) |
| `goal_state_changed` | `goal: GoalId, objective, state: GoalState, step, max_steps, detail, yolo: bool?, session: u64?` — GLOBAL Work-Board event; `session` = the goal's own dedicated session, `None` on legacy frames (:457) |
| `task_batch_done` | `batch: u64, parent: u64, rows: BatchWorkerRow[]` — a `task_fanout` batch reached its report point (every worker terminal, or the deadline fired with stragglers `timed_out`, still revivable); rows are evidence POINTERS, never payloads; GLOBAL (:483) |
| `worker_state_changed` | `worker: WorkerId, batch: u64, parent: u64, session: u64, task, state: WorkerState, detail, yolo: bool?, node: str?` — `goal_state_changed`'s twin for the WORKERS lane; `parent` = the conductor session, `session` = the worker's own; remote rows (`node` set, W2) carry sentinel `session: 0`; GLOBAL (:493) |

> **`turn_started` is NOT emitted by the Rust daemon.** Busy is driven by
> `agent_text`. `needs_approval` is hardcoded `false` by the agent
> (`turn.rs:118`) — rely on the `approval_pending` event for gating.

**Nested structs / enums:**

- `ToolCall{ id:ActionId, tool:String, args:Value, needs_approval:bool }` (:523)
- `ToolOutput{ ok:bool, content:Value }` (:532)
- `ToolSpec{ name, description, input_schema }` (:538)
- `ContentBlock` (tag `type`): `text` / `thinking`(+`signature`) / `tool_use` / `tool_result` / `image`(`media_type`,`data` b64) (:581)
- `Message` (tag `role`): `user` / `assistant` (:574)
- `ImageSource{ media_type, data }` — prepared b64 riding `user_prompt.images` (:597)
- `SensorReading` (tag `kind`): `temperature/humidity/pressure/motion/distance/gpio_level/air_quality/thermal_frame` (:263)
- `EvolutionProposal` (tag `kind`): `register_mcp_server/unregister_mcp_server/update_policy_rule/update_system_prompt/hot_reload_subsystem/request_hardware` (:212)
- `GoalState` (snake): `planning/acting/blocked/reflecting/done/failed/cancelled` (:87)
- `WorkerState` (snake): `queued/running/idle/parked/blocked/done/failed/cancelled` — `parked` = evicted from memory, `sessions/<id>.jsonl` is truth; revive-on-send is the only Parked→Running edge (:105)
- `BatchWorkerRow{ worker:WorkerId, state:WorkerState, evidence:String, timed_out:bool?, node:str? }` — evidence file pointer per worker; `node` set = hosted on a mesh peer (then `evidence` is the conductor-side mirror) (:120)
- `PolicyMode` (global, kebab): `suggest`(default) `auto-edit` `yolo` (:153)
- `PolicyRule` (per-tool `[rules]` value, kebab): `allow` `ask` `workspace` (:169)
- `Subsystem` (snake): `plugins/policy/agent/gateway` (:201)

### Bus & policy

- `BusHandle::emit(Event).await`; broadcast capacity 1024 (`bus.rs`).
  `SystemState{sessions,tools,plugins,pending_approvals}` mutated only in
  `apply()` (`state.rs:18`).
- `PolicyEngine.check()` (`policy.rs:106`): `yolo` short-circuits Allow →
  exact tool key → `prefix.*` wildcard (matches `prefix.<x>`, not bare `prefix`,
  `:166`) → unknown defaults to **Ask** (:129). `workspace` canonicalizes the
  path arg inside `AGENTD_WORKSPACE`, rejects `..` (:136).

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

51 tools, advertised by `list()` and dispatched by `call()` (both in
`tools/crates/apexos-tools/src/tools.rs`):

`run_command read_file write_file list_dir create_dir delete_path notes_list
notes_read notes_append sketch_snapshot sketch_draw screenshot_mirror ui_open
ui_close ui_focus ui_query ui_arrange ui_theme ui_reflex camera_capture
http_fetch cpu_temp disk_usage memory_info uptime notify audio_analyze
audio_trim_silence audio_normalize audio_peak_limit audio_trim audio_clean
gpio_info gpio_read gpio_write gpio_pulse gpio_pwm gpio_servo display_face
git_status git_diff git_log git_branch git_init git_commit git_push
git_checkout git_reset git_merge git_worktree eject_media`

`sketch_draw`, the `ui_*` family (adaptive UI, docs/adaptive-ui.md) and
`display_face` are validate+echo handlers: ui-slint intercepts the
`tool_requested` event and applies them client-side (no tool card). `git_*`
shell out to system `git` via argv (never `/bin/sh`), repo-confined to
`confine_git_repo` roots (workspace + `AGENTD_GIT_ROOTS`). `eject_media` drops
a request file for the root systemd eject drain (never sudo).

`sketch_snapshot`/`screenshot_mirror`/`camera_capture` are the **vision** tools:
each returns a `{"vision":{"path"|"b64"},"text"}` sentinel that the agent turn
loop (`vision_rewrite` in `agentd/crates/agent/src/turn.rs`) converts to a
`ContentBlock::Image` via `prepare_image`/`prepare_b64`
(`agentd/crates/core/src/vision.rs`) — zero agentd schema changes.

FS confinement lives in the tool process: `tools.rs::confine(path, write)`
(:930, delegating to the std-only `apexos-confine` crate) gates every FS verb —
writes/creates/deletes are workspace-only (per-agent root, system-stamped as
`__workspace`), reads/lists get the workspace + a small read allowlist
(`AGENTD_READ_ROOTS`-extensible) minus a secret denylist; `confine_git_repo`
(:979) and `confine_audio_io` (:2385) confine the git and audio families. The
`run_command` denylist is a bypassable heuristic, not security — the systemd
sandbox is the outer boundary. `SupervisorCmd::CallTool` (`dispatch_tool` in
`agentd/crates/plugins/src/supervisor.rs`) dispatches **without** a policy
check.

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
unread. Conventions: FORGE→`"FORGE"`, APEX→`"APEX"` — but agentd **system-stamps**
`agent_id` on every cerebro call (`AGENTD_AGENT_ID`, default `APEX`), so the
model-supplied value never lands.

**Stubs: none.** `TOOL_NAMES` (`cerebro/crates/cerebro-mcp/src/tools.rs`) has
67 entries, all functional — `ingest_file` (the last deferred stub) landed with
the ingestion port; the dispatch fallthrough now answers only unadvertised
names with "tool not implemented". **`describe_image`, `search_vision` (CLIP
visual recall) and `cognitive_bootstrap` are SHIPPED too** — the latter routes
to the live-state priming assembler (`assemble_bootstrap` in `dispatch.rs`).
Reinforcement is live (a recall's returned top-k record an access —
`cortex.rs:270`), and visibility scope is enforced at all three recall touch
points (SQL filter, `can_access`, and the spreading-activation `visible_nodes`
map — `activation/spreading.rs:98`).

### Virtual tools (agentd-built-in, intercepted in `supervisor.rs` `dispatch_tool`)

Specs live in `agentd/src/main.rs` unless noted; intercepts in
`agentd/crates/plugins/src/supervisor.rs`. Policy = the shipped
`config/policy.toml` value (unlisted → Ask under `suggest`).

| Tool | Signature | Spec / intercept | Policy |
|---|---|---|---|
| `read_soul_md` | `()` → the bound agent's live soul string | spec `main.rs:2987`, intercept `supervisor.rs:676` | allow |
| `soul_rehearse` | `(soul, probes?≤6, compare_to?)` → transcripts from an ephemeral, tool-less mind (nothing persists) | spec `main.rs:3001`, intercept `supervisor.rs:885` | allow |
| `propose_evolution` | `(kind, reason, +per-kind args)` — deferred ack carries the real apply outcome | spec `main.rs:3045`, intercept `supervisor.rs:564` | ask |
| `rollback_evolution` | `(evolution_id:int, reason)` → `{status:"rolled_back", summary}` | spec `main.rs:3136`, intercept `supervisor.rs:626` | ask |
| `agent_spawn` | `(prompt, system?, inherit_soul?, node?, timeout_s?)` — with `node` = blocking cross-node spawn | spec `main.rs:2936`, intercept `supervisor.rs:1985` | allow |
| `schedule_task` / `list_schedules` / `cancel_schedule` / `schedule_wakeup` / `list_wakeups` / `cancel_wakeup` | scheduler family; wakeups fire into the SESSION that scheduled them (root for legacy/worker-range callers) and are identity-gated to the node agent | specs `main.rs:3159-3281`, intercept `supervisor.rs:723` | `schedule_task` ask, rest allow |
| `goal_create` / `goal_step` / `list_goals` / `goal_resume` / `goal_cancel` | autonomous goal driver (`goal_create{yolo:true}` arms goal-scoped auto-approval) | specs `goal.rs:119`, intercept `supervisor.rs:817` | allow |
| `task_fanout` / `worker_report` / `list_workers` / `worker_cancel` | worker tier (Fabrica, docs/fabrica.md): `task_fanout{tasks[≤32], mode?(async\|inline), model?, node?, yolo?("inherit"), batch_deadline_s?, mandala?, parent_cell?, join?, barrier_timeout_s?}` fans one batch of parallel worker sessions (tier-aware admission cap, FIFO queue; per-task items may carry `{prompt, model?, node?, measure?, voucher?}` — `node` ships a task to a mesh peer's worker tier); `worker_report{status(continue\|done\|blocked\|yield), summary?, artifacts?, next?, reason?, measure?}` is the worker-side verdict; `worker_cancel{worker?\|batch?}` is terminal; `list_workers()` is a read | specs `worker.rs:419/:508/:536/:546`, routed via `worker_tx` (`supervisor.rs:787`) to the worker driver (`worker.rs:1044`) | allow |
| `mandala_create` / `mandala_status` / `mandala_close` | Mandala Mode (recursion manifold over the worker tier): `mandala_create{objective, done_when, verify, lattice?(spine\|quad\|fan\|spiral\|funnel), repo?, depth?≤6, steps?, deadline_s?}` writes the invariant + opens the tree (grow it with `task_fanout{mandala, parent_cell, tasks}`; `repo` = code regime, per-cell git worktree branches, cells stay local); `mandala_status{mandala?}` reads the live tree (omit = list all); `mandala_close{mandala}` is bookkeeping — refuses while any non-root cell is open | specs `worker.rs:564/:597/:489`, same `worker_tx` route (`supervisor.rs:787`) | allow |
| `apply_daemon_update` | self-update apply | spec `self_update.rs:101`, intercept `supervisor.rs:913` | ask |
| `convene_council` / `query_event_log` | multi-agent council / event-log query | specs `main.rs:3287`/`:3326`, intercepts `supervisor.rs:847`/`:943` | unlisted → Ask |
| `send_to_agent` | `(session_id:int, message:str, node?:str)` — cross-node posts `{message}` + auto-stamped `origin_session`; result reports `landed_session` | spec `main.rs:3358`, intercept `supervisor.rs:1015` | allow |
| `mesh_file_send` / `mesh_memory_send` / `mesh_procedure_send` / `mesh_recall` / `mesh_capabilities` | mesh relay + federation family (workspace-confined / provenance-stamped / `shared_only()` on the wire) | specs `main.rs:3403-3541`, intercepts `supervisor.rs:1155-1247` | allow |
| `list_mesh_peers` | `()` → peers.toml text | spec `main.rs:3542`, intercept `supervisor.rs:1249` | allow |
| `bootstrap_node` | `(target_ip, ssh_password, ssh_user?=apexos, api_key?, repo_url?)` — needs `sshpass` (not auto-installed) | spec `main.rs:3555`, intercept `supervisor.rs:1267` | unlisted → Ask |
| `vast_list_recipes` / `vast_launch` / `vast_destroy` / `vast_status` | recipe array / `(recipe, geo?=EU_NORDIC)`→`{status:ready,...}` / teardown / phase | specs `main.rs:3591-3634`, intercepts `supervisor.rs:1444/:1524/:1921/:1482` | unlisted → Ask |

### Mesh REST routes (`gateway/src/lib.rs`)

`GET /api/mesh/nodes` (:3982) · `GET /api/mesh/peers` (:4019) ·
`POST /api/mesh/peers {node_id,ws_url,role?}` (:4051, emits `PeerRegistered`) ·
`DELETE /api/mesh/peers/{id}` (:4093) · `GET /api/sessions/active` (:1532) ·
`POST /api/sessions/{id}/message {message}` (:1751, A2A landing, emits
`UserPrompt`) · `GET/POST /api/backend {backend,oai_base_url?,model?}`
(:1147/:1192, live hot-swap) · `POST /api/worker/fanout|query|cancel|report`
(:3707/:3727/:3738/:3749, the W2 mesh-worker hosting seams behind
`task_fanout{node}` — `from` must be in the peer registry (a bearer token
alone never authorizes), refused when `AGENTD_MESH_WORKERS=0`).

### UI surface (`ui-slint`)

- `AppKind` ordinals (enum in `ui-slint/src/ui/types.slint:13`, mirrored by
  `kind_ordinal`/`kind_from_ordinal` in `ui-slint/src/main.rs`): `chat=0,
  system=1, sensor=2, sessions=3, settings=4, terminal=5, council=6,
  event-log=7, mesh=8, inference=9, audio-editor=10, sonus=11, notes=12,
  face=13, sketchpad=14, web=15, calculator=16, explorer=17, occipital=18,
  board=19, imagine=20, mandala=21` (append new variants; the ordinal in the
  two `main.rs` arms MUST agree with enum order).
- `WindowDesc{ id, kind:AppKind, title, x/y/w/h, minimized, maximized }`
  (`types.slint:69`); `WINDOWS` VecModel order == z-order.
- Thread-local models (`main.rs:31-60`): `MESSAGES, SESSIONS, MODELS, TOASTS,
  NOTIF_LOG, WINDOWS, COUNCIL, BOARD` — mutated on the Slint thread only.
- REST base = `ws_to_http(AGENTD_WS)` (`main.rs:3581`); shared `http_client`
  carries the bearer token (`main.rs:7309`).

### Key environment variables

`AGENTD_WS` (`ws://localhost:8787/ws`) · `AGENTD_BIND` (`127.0.0.1:8787`;
non-loopback requires `AGENTD_TOKEN`) · `AGENTD_TOKEN` (gates `/ws` via `?token=`
and `/api/*` via Bearer) · `SENSOR_BRIDGE_TOKEN` · `AGENTD_TOOL_RESULT_TIMEOUT_SECS`
(1800) · `AGENTD_WORKSPACE` (workspace root for the `workspace` rule) ·
`CEREBRO_EMBED_MODEL` (`""`→FTS5-only ~23 MB) · `SLINT_BACKEND` /
`SLINT_FULLSCREEN` · `AGENTD_WORKER_CAP` (worker admission cap; ≥1 wins over
the hardware tier's default) · `AGENTD_MESH_WORKERS` (`0` disables the
`/api/worker/*` hosting seams and `task_fanout{node}`) ·
`MESH_DISCOVERY_INTERVAL` (60) · `MESH_SUBNET_GUARD` (on,
/24) · `PEERS_TOML` / `RECIPES_TOML` (`/etc/agentd/...`) · `VAST_API_KEY` (req) ·
`VAST_DEFAULT_GEO` (EU_NORDIC) · `VAST_LOCAL_PORT` (8000).
