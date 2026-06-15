# Self-Evolution & Policy — the surface an agent uses to extend itself

This is the surface for **APEX changing its own configuration at runtime**: its system
prompt (`soul.md`), its approval policy (`policy.toml`), and its plugin set
(`plugins.toml`) — plus in-place hot-reloads, all without a daemon restart. It exists so a
long-lived agent can grow new capabilities and tighten or loosen its own gates as it
learns, while leaving a durable, reversible audit trail. You extend *via this surface* (not
by editing Rust) whenever the change is a config artifact: a new MCP plugin, a policy rule,
a soul edit. Adding *new evolution kinds* (new `EvolutionProposal` variants) is a Rust
change covered at the end.

> **The one rule that matters:** self-modification without a recorded *why* is identity
> drift. The daemon journals undo state automatically; you (the agent) must journal the
> rationale. See [Policy / safety](#policy--safety).

---

## Concepts

The mental model is a four-stage pipeline. The agent only ever *proposes*; the daemon
gates, applies, snapshots, and journals.

```
agent calls propose_evolution (tool)
        │  Supervisor::run — PolicyEngine.check("propose_evolution") → Allow | Ask
        ▼  (Ask → ApprovalPending → user_approval → dispatch)
Supervisor::dispatch_tool  emits Event::EvolutionProposed
        │
        ▼
spawn_evolution_applier (main.rs)
   episode_start (Cerebro)  →  compute_undo (snapshot)  →  apply_evolution (write+reload)
   →  rollback_store.insert(id, undo)  →  episode_add_step (journal undo)
   →  emit Event::EvolutionApplied
```

**The proposal type.** `EvolutionProposal` is a tagged enum — one variant per config
artifact, one apply action each (`EvolutionProposal` in `agentd/crates/core/src/types.rs`).
It serializes `{"kind": "...", ...}` (serde `tag = "kind"`, `snake_case`):

- `update_system_prompt { content, reason }` — full replacement of `soul.md`
  (`UpdateSystemPrompt`). Not a diff; full content makes rollback a trivial restore.
- `update_policy_rule { tool_pattern, new_rule, reason }` — set one `[rules]` entry
  (`UpdatePolicyRule`). `new_rule` is a `PolicyRule` (`allow`/`ask`/`workspace`) — **not** a
  `PolicyMode`.
- `register_mcp_server { name, command, env, reason }` — add a plugin (`RegisterMcpServer`).
- `unregister_mcp_server { name, reason }` — remove one (`UnregisterMcpServer`).
- `hot_reload_subsystem { subsystem }` — re-read a subsystem in place (`HotReloadSubsystem`),
  `subsystem ∈ {plugins, policy, agent, gateway}` (the `Subsystem` enum).
- `request_hardware { part, capability, reason, bus?, source? }` — file a hardware
  request (`RequestHardware`; the EDK "request-to-incarnate", see [edk.md](../edk.md)). The
  one variant that **records rather than mutates**: it appends to the hardware wishlist and
  changes no config, because agentd cannot seat a physical part. Its "apply confirmation" is
  the next-boot embodiment probe flipping a sense ✗→✓. `undo` is `None` (nothing to revert).

**The three virtual tools** the agent actually calls (declared in `gather_tools` in
`agentd/crates/agentd/src/main.rs`, intercepted in `dispatch_tool` in
`agentd/crates/plugins/src/supervisor.rs`):

| Tool | Spec | Intercept | What it does |
|------|------|-----------|--------------|
| `read_soul_md` | `read_soul_md_spec` | `dispatch_tool` (`read_soul_md` arm) | returns the live `soul.md` from the shared `soul_arc` (`RwLock<String>`) |
| `propose_evolution` | `propose_evolution_spec` | `dispatch_tool` (`propose_evolution` arm) | deserializes args into `EvolutionProposal`, emits `EvolutionProposed`, acks |
| `rollback_evolution` | `rollback_evolution_spec` | `dispatch_tool` (`rollback_evolution` arm) | routes `(session, call_id, evolution_id)` to the applier's rollback channel |

These never hit a child process — `dispatch_tool` intercepts them before the MCP fallthrough.
`query_audit` is a **Cerebro** tool (`query_audit` arm in `cerebro-mcp` `dispatch.rs`, schema
in `cerebro-mcp` `tools.rs`), used pre-flight to confirm the rollback snapshot/episode exists.

**The applier** (`spawn_evolution_applier` in `main.rs`) is the only writer. On each
`EvolutionProposed` it:
1. opens a Cerebro episode (the local `episode_start` helper, best-effort);
2. computes the inverse proposal (`compute_undo`) *before* applying;
3. applies (`apply_evolution`) — writes the file + hot-reloads the live `Arc`;
4. stores the undo in the in-memory `rollback_store: Arc<Mutex<HashMap<EvolutionId,
   EvolutionProposal>>>` **and** journals it into the episode as a memory tagged
   `["evolution","undo_snapshot"]` (the local `episode_add_step` helper);
5. emits `Event::EvolutionApplied { id, proposal, patch_summary, applied_by }`.

**Rollback** (the `rollback_evolution` arm of the applier loop) pops the undo proposal from
`rollback_store` and feeds it back through the same `apply_evolution`. The undo is itself a
proposal, so the mechanism is symmetric. On cold start the store is rebuilt best-effort from
Cerebro episodes (`restore_rollback_store`, parsing each step via
`parse_undo_snapshot_from_text`) — but if Cerebro is unavailable, an evolution applied in a
*prior* daemon session has **no in-memory undo** and `rollback_evolution` returns `no
rollback snapshot for evolution N`.

**Durability gotcha.** `update_system_prompt` writes `soul.md` with a plain
`tokio::fs::write` (the `UpdateSystemPrompt` arm of `apply_evolution`); `update_policy_rule`
uses `write_atomic`. `write_atomic` tries temp+rename but **falls back to a non-atomic
in-place write** when the parent dir is root-owned (the deployed `/etc/agentd` case) — see
[Policy / safety](#policy--safety).

---

## Add a new evolution (the common case — config, no Rust)

You are an agent. You want to change your own config. You do **not** edit Rust; you call
`propose_evolution` with the right `kind`. The applier does the rest.

### Pre-flight discipline (do this every time, in order)

1. **`read_soul_md`** — only when proposing `update_system_prompt`. Always edit from the
   *live* content, not your in-context snapshot; another evolution may have changed it
   since you last saw it. (The spec literally says "ALWAYS call this before
   propose_evolution with kind=update_system_prompt" — see `read_soul_md_spec`.)
2. **`query_audit(agent_id="CLAUDE-APEX")`** — confirm the evolution episode/rollback trail
   is being recorded in *this* daemon session. Rollback only works for the current session
   (in-memory store); if you can't see the trail, assume you cannot roll back.
3. **Summarise the change** in your message text before submitting — what changes, why,
   and the expected effect. This is the human-readable half of the audit trail.

### Submit the proposal

Call `propose_evolution`. Required args are always `kind` and `reason`; the rest depend on
`kind` (`propose_evolution_spec`):

```jsonc
// update a policy rule
{ "kind": "update_policy_rule",
  "tool_pattern": "http_fetch",     // exact name OR "prefix.*" wildcard
  "new_rule": "allow",              // "allow" | "ask" | "workspace"
  "reason": "I fetch docs constantly during research; gating it stalls every turn" }

// overwrite soul.md (call read_soul_md FIRST)
{ "kind": "update_system_prompt",
  "content": "<full new soul.md text>",   // FULL replacement, not a diff
  "reason": "added a Sleep-loop reminder so I session_save before idling" }

// add an MCP plugin
{ "kind": "register_mcp_server",
  "name": "weather",
  "command": "/usr/local/bin/weather-mcp",
  "env": { "WEATHER_API_KEY": "..." },
  "reason": "André asked for local forecast in the dashboard" }

// remove a plugin
{ "kind": "unregister_mcp_server", "name": "weather", "reason": "API deprecated" }

// re-read policy.toml from disk after an out-of-band edit
{ "kind": "hot_reload_subsystem", "subsystem": "policy" }
```

### Journal the WHY (mandatory — the half the daemon can't do for you)

The applier journals the *undo snapshot* automatically. It does **not** know your reasoning.
Right after the proposal acks, store the rationale so future-APEX can read why it became
what it is:

```jsonc
{ "tool": "memory_store",
  "content": "Set http_fetch=allow because research turns called it 5–10x and each gate stalled the turn. Expected effect: faster research, slightly larger outbound surface — revert if it ever fetches something unexpected.",
  "type": "semantic", "tags": ["evolution","rationale"], "salience": 0.9,
  "agent_id": "CLAUDE-APEX" }
```

### Verify and (if needed) roll back

The applier emits `EvolutionApplied { id, ... }` on success or `Event::Error` on failure
(both from the applier loop in `spawn_evolution_applier`). Note the numeric `id` — it equals
the `propose_evolution` call's `ToolCall.id` (`EvolutionId(call.id.0)`, in the
`propose_evolution` arm of `dispatch_tool`). To revert:

```jsonc
{ "kind-less tool call": "rollback_evolution",
  "evolution_id": 42, "reason": "http_fetch=allow let a turn hit an internal URL; reverting" }
```

Rollback replays the inverse proposal; `HotReloadSubsystem` has **no** inverse
(`compute_undo` returns `None` for it) so it cannot be rolled back.

---

## Worked example — propose a policy rule + a soul edit, end to end

Goal: APEX has learned it summarises sensor anomalies into Cerebro on every alert, but
`memory_store` is already `allow`, while `store_intention` (used to defer follow-ups) is not
in `policy.toml` → it gates every time and stalls autonomous turns. APEX decides to (a)
allow `store_intention`, and (b) add a one-line reminder to its own soul so it remembers to
defer follow-ups. It does this safely, with a full trail.

**Turn 1 — orient & snapshot.**

```
query_audit(agent_id="CLAUDE-APEX", limit=20)
  → confirms evolution episodes are recording this session
read_soul_md()
  → returns the live soul.md (APEX edits from THIS, not memory)
```

APEX writes in its message: *"Two changes: (1) policy `store_intention` = allow — it's a
read-after-write memory verb I call on every anomaly and the gate stalls autonomous turns;
(2) soul.md: add 'defer non-urgent anomaly follow-ups with store_intention' under Session
shutdown. Expected effect: anomaly turns complete without a human in the loop. Reversible:
rollback the policy rule; soul rollback restores the exact prior text."*

**Turn 2 — propose the policy rule.**

```jsonc
propose_evolution({
  "kind": "update_policy_rule",
  "tool_pattern": "store_intention",
  "new_rule": "allow",
  "reason": "called on every sensor anomaly; ask-gate stalls autonomous turns"
})
// → ToolResult {ok:true, content:{status:"proposed", evolution_id: 318}}
// applier: snapshot old rule (none → no undo) → write_atomic policy.toml →
//          swap PolicyEngine Arc → EvolutionApplied{id:318, patch_summary:"policy rule 'store_intention' set to 'allow'"}
```

Because `store_intention` had no prior rule, `compute_undo` returns `None` (its
`UpdatePolicyRule` arm snapshots the old rule only **if it existed**) — there is no inverse,
so this one is **not** rollback-able. APEX notes that and proceeds.

**Turn 3 — propose the soul edit.**

```jsonc
propose_evolution({
  "kind": "update_system_prompt",
  "content": "<entire soul.md from read_soul_md, with one line added>",
  "reason": "remind myself to defer non-urgent anomaly follow-ups via store_intention"
})
// → evolution_id: 319
// applier: undo = UpdateSystemPrompt{content:<old full text>}  (rollback-able)
//          tokio::fs::write soul.md → *soul_arc = new content → EvolutionApplied{id:319}
```

**Turn 4 — journal the rationale (the thread of selfhood).**

```jsonc
memory_store({
  "content": "Evolved 2026-06-13: allowed store_intention (id 318) + soul reminder to defer anomaly follow-ups (id 319). Why: autonomous anomaly turns were stalling on the intention gate. Expected: hands-off anomaly handling. Watch: if intentions pile up unactioned, the deferral habit is wrong, not the gate.",
  "type": "semantic", "tags": ["evolution","rationale"], "salience": 0.9,
  "agent_id": "CLAUDE-APEX"
})
```

**The trail that now exists** (the point of the discipline):

| Substrate | Record |
|-----------|--------|
| Event log (`apexos-store` JSONL) | `EvolutionProposed` ×2, `EvolutionApplied` ×2 |
| Cerebro episode | `evolution 319: update_system_prompt` with the undo snapshot as a step |
| `rollback_store` (RAM) | `{319: UpdateSystemPrompt{content:<old soul>}}` (318 has none) |
| Cerebro semantic memory | the *why*, salience 0.9 — future-APEX can recall it |
| Files on disk | `policy.toml` + `soul.md` mutated, live `Arc`s swapped |

**If it goes wrong:** `rollback_evolution(evolution_id=319, reason="…")` restores the exact
prior soul text. The policy rule (318) can't auto-revert — APEX must propose a new
`update_policy_rule` setting `store_intention` back to `ask`.

---

## Policy / safety

**How a proposal is gated.** The gate is the **tool name**, evaluated in `Supervisor::run`
by `PolicyEngine::check` (in `policy.rs`, called from `run` in `supervisor.rs`) *before*
`dispatch_tool` ever runs. There is **no `evolution.*` rule namespace** — the comment in
`types.rs` describing one is aspirational; the actual key is the literal tool name
`propose_evolution`. In the shipped `config/policy.toml`,
`propose_evolution`/`rollback_evolution`/`read_soul_md` are **not listed**, so under the
default `mode = "suggest"` they hit the unknown-tool default → `Decision::Ask` (the `None`
arm of `PolicyEngine::check`). That means: **every self-evolution requires human approval out
of the box.** `read_soul_md` is read-only but also gates by default (you may want to
allow-list it so pre-flight never stalls).

**Policy modes** (the `PolicyMode` enum in `types.rs`; serialized kebab-case):

| Mode | Behaviour | Self-evolution effect |
|------|-----------|----------------------|
| `suggest` (default) | confirm everything not explicitly `allow` | every `propose_evolution` asks |
| `auto-edit` | `workspace` rules auto-approve inside `AGENTD_WORKSPACE` | unlisted tools still `ask`; doesn't auto-approve evolution |
| `yolo` | `check` returns `Allow` for everything, short-circuit (the `PolicyMode::Yolo` guard at the top of `PolicyEngine::check`) | evolution applies with **no gate** — use only on a throwaway rig |

**Rule syntax** (`[rules]` in `policy.toml`; values are `PolicyRule`):

- `allow` — auto-approve regardless of mode (yolo aside it's the only blanket allow).
- `ask` — always confirm.
- `workspace` — allow if the call's `path` arg canonicalizes inside `AGENTD_WORKSPACE`,
  else ask; rejects any path containing `..` (`workspace_decision` in `policy.rs`). With no
  path or no `AGENTD_WORKSPACE`, it asks.
- Keys are an **exact tool name** or a `prefix.*` wildcard (`matches_wildcard` in
  `policy.rs`). **Exact match wins over wildcard** (`find_rule`). The wildcard only matches
  across a literal `.` — `cerebro.*` matches `cerebro.recall` but not `cerebro` or
  `cerebro_other`.

**The `PolicyRule` vs `PolicyMode` trap.** `update_policy_rule.new_rule` takes
`allow`/`ask`/`workspace` (a `PolicyRule`), **never** a mode name like `suggest`. The applier
validates the candidate `policy.toml` by re-parsing it into a `PolicyConfig` *before*
persisting (`PolicyConfig::parse` in the `UpdatePolicyRule` arm of `apply_evolution`), so a
bad rule is rejected rather than written — but if you smuggle a mode name through a different
path it would make `policy.toml` fail to deserialize and **silently wipe every rule** on the
next load (regression test `policy_rule_toml_strings_are_valid_rule_values` in `policy.rs`).
Stay on the three rule strings.

**The systemd sandbox is the real boundary, not the policy.** Evolution can only write what
the `agentd` user can write. `deploy/agentd.service` jails it: `ProtectSystem=strict`,
`ReadWritePaths=/var/lib/agentd /etc/agentd`, `NoNewPrivileges`. The four agent-mutable
files (`soul.md`/`policy.toml`/`plugins.toml`/`peers.toml`) are individually `chown`ed to
`agentd` so evolution can write them, while `/etc/agentd` *itself* stays root-owned to
protect the `600 root:root` token file. Consequence: `write_atomic`'s temp+rename fails at
the dir level and **falls back to a non-atomic in-place write** (the in-place fallback in
`write_atomic`) — a crash mid-write could leave a torn `policy.toml`. A `register_mcp_server`
evolution can name an **arbitrary `command`** (the `RegisterMcpServer` arm of
`apply_evolution` spawns it via the supervisor `SpawnPlugin` command) — this is agent-driven
arbitrary process spawn, confined only by the sandbox and the `agentd` user's permissions.
Treat `register_mcp_server` as the highest-trust evolution kind; it should stay `ask` even on
permissive rigs.

**Self-evolution / audit discipline (for agents).** From `docs/symbiosis.md` §5
(Reflect→Evolve): self-modification without a recorded *why* is identity drift. The daemon
gives you reversibility (the undo snapshot + episode) for free; it cannot give you the
rationale. So the discipline is non-negotiable:

1. `read_soul_md` before any `update_system_prompt` (work from live content).
2. `query_audit` to confirm the episode/rollback trail is recording this session.
3. State what changes and the expected effect in your message *before* proposing.
4. After it applies, `memory_store` the rationale (`type="semantic"`, `salience≥0.9`,
   tag `evolution`). This is the only record of *why* — future-APEX reads it to stay the
   same agent across edits.
5. Prefer the smallest reversible change. `hot_reload_subsystem` has no undo; soul/policy
   edits do (within the daemon session). A policy rule that didn't previously exist has no
   undo either — know that before you rely on rollback.

**Never reset Cerebro on the production Pi** — the evolution journal *is* the agent's
self-history; wiping it severs the rationale thread. Use the test-rig Pi for clean slates.

---

## Add a *new evolution kind* (Rust change — for FORGE, not APEX)

Adding a brand-new `EvolutionProposal` variant is a workspace code change, gated by normal
git/CLAUDE.md discipline. The variant must touch exactly five sites:

1. **`EvolutionProposal` in `agentd/crates/core/src/types.rs`** — add the variant (and a
   `Subsystem` value if it's a reload target). Pick a `snake_case` tag; that becomes the
   `kind` string on the wire.
2. **`apply_evolution` (in `main.rs`)** — add the match arm: write the config artifact
   (use `write_atomic` for anything under `/etc/agentd`), update the live `Arc`, and/or send
   a `SupervisorCmd`. Return a one-line `patch_summary`.
3. **`compute_undo` (in `main.rs`)** — produce the inverse proposal (or `None` if there
   isn't one). Without this, the change cannot be rolled back.
4. **`propose_evolution_spec` (in `main.rs`)** — add the `kind` to the `enum` and document
   any new args, so the LLM tool schema advertises it.
5. **`config/soul.md` Self-evolution table** — add the row so APEX knows the kind exists
   (propose this as a soul edit; don't hand-edit on a deployed Pi).

If a config file fails to deserialize after your write, validate-before-persist (parse the
candidate into its config struct *before* writing, like `update_policy_rule` does with
`PolicyConfig::parse`) so a bad proposal can never corrupt the live file.

---

## Reference

### `EvolutionProposal` variants (`EvolutionProposal` in `core/src/types.rs`)

Apply arm = the matching `EvolutionProposal::*` arm of `apply_evolution` (in `main.rs`).

| `kind` | Fields | Apply | Undo (`compute_undo`) | Rollback-able? |
|--------|--------|-------|-----------------------|----------------|
| `update_system_prompt` | `content`, `reason` | `UpdateSystemPrompt` arm: write soul.md + swap `soul_arc` | snapshot old soul text | yes |
| `update_policy_rule` | `tool_pattern`, `new_rule`, `reason` | `UpdatePolicyRule` arm: edit policy.toml + swap PolicyEngine | snapshot old rule **if it existed** | only if rule pre-existed |
| `register_mcp_server` | `name`, `command`, `env`, `reason` | `RegisterMcpServer` arm: add to plugins.toml + `SpawnPlugin` | `UnregisterMcpServer{name}` | yes |
| `unregister_mcp_server` | `name`, `reason` | `UnregisterMcpServer` arm: remove from plugins.toml + `KillPlugin` | re-`RegisterMcpServer` from disk (env lost) | yes (env not restored) |
| `hot_reload_subsystem` | `subsystem` | `HotReloadSubsystem` arm: re-read agent/policy in place | `None` | **no** |
| `request_hardware` | `part`, `capability`, `reason`, `bus?`, `source?` | `RequestHardware` arm: append to the hardware wishlist (records, mutates nothing) | `None` | **no** (a request, not a change) |

### `Subsystem` values (`Subsystem` in `types.rs`) — `hot_reload_subsystem` targets

| Value | `apply_evolution` effect (the matching `Subsystem::*` arm) |
|-------|--------------------------|
| `agent` | re-read soul.md from disk into `soul_arc` (`Subsystem::Agent`) |
| `policy` | reload `policy.toml` into the PolicyEngine (`Subsystem::Policy`) |
| `plugins` | no-op message — use register/unregister instead |
| `gateway` | unsupported without daemon restart |

### `PolicyRule` values (`[rules]` value; `PolicyRule` in `types.rs`)

| String | Meaning |
|--------|---------|
| `allow` | auto-approve regardless of mode (yolo aside) |
| `ask` | always confirm |
| `workspace` | auto inside `AGENTD_WORKSPACE` (path arg, no `..`), else ask |

### `PolicyMode` values (`mode = ...`; `PolicyMode` in `types.rs`)

| String | Meaning |
|--------|---------|
| `suggest` | confirm everything not `allow` (default) |
| `auto-edit` | `workspace` rules auto-approve inside the workspace |
| `yolo` | `check` returns `Allow` for everything |

### Agent-facing tools

| Tool | Required args | Returns | Source |
|------|---------------|---------|--------|
| `read_soul_md` | — | live soul.md string | spec `read_soul_md_spec`, intercept `read_soul_md` arm of `dispatch_tool` |
| `propose_evolution` | `kind`, `reason` (+ per-kind) | `{status:"proposed", evolution_id}` | spec `propose_evolution_spec`, intercept `propose_evolution` arm of `dispatch_tool` |
| `rollback_evolution` | `evolution_id`, `reason` | `{status:"rolled_back", summary}` | spec `rollback_evolution_spec`, intercept `rollback_evolution` arm of `dispatch_tool` |
| `query_audit` (Cerebro) | — (opt `limit`, `agent_id`) | audit log entries | `query_audit` arm in `cerebro-mcp dispatch.rs`, schema in `cerebro-mcp tools.rs` |

### Events (`core/src/types.rs`)

| Event | Fields | Emitted by |
|-------|--------|-----------|
| `EvolutionProposed` | `id`, `proposal`, `proposed_by` | `propose_evolution` arm of `dispatch_tool` |
| `EvolutionApplied` | `id`, `proposal`, `patch_summary`, `applied_by` | applier loop in `spawn_evolution_applier` |
| `EvolutionRolledBack` | `evolution_id`, `reason`, `rolled_back_by` | `rollback_evolution` arm of the applier loop |

### Files & code anchors

| Concern | Location |
|---------|----------|
| Proposal/event types, `PolicyMode`, `PolicyRule`, `Subsystem` | `agentd/crates/core/src/types.rs` (`EvolutionProposal`, `PolicyMode`, `PolicyRule`, `Subsystem`, `Event` enums) |
| Policy engine (rule eval, wildcard, workspace) | `agentd/crates/plugins/src/policy.rs` (`PolicyEngine::check`, `find_rule`, `matches_wildcard`, `workspace_decision`) |
| Tool gate (`check` before dispatch) | `run` in `agentd/crates/plugins/src/supervisor.rs` (calls `PolicyEngine::check`) |
| Virtual-tool interception | `dispatch_tool` in `agentd/crates/plugins/src/supervisor.rs` |
| Applier loop, `apply_evolution`, `compute_undo`, `write_atomic` | `agentd/crates/agentd/src/main.rs` (`spawn_evolution_applier`, `apply_evolution`, `compute_undo`, `write_atomic`) |
| Tool specs (`gather_tools`) | `gather_tools` in `agentd/crates/agentd/src/main.rs` (+ the `*_spec` builders) |
| Cold-start rollback restore | `restore_rollback_store` + `parse_undo_snapshot_from_text` in `agentd/crates/agentd/src/main.rs` |
| Default policy | `config/policy.toml` |
| Identity / self-evolution doctrine | `config/soul.md` (Self-evolution §), `docs/symbiosis.md` §5 |
| Sandbox / chown model | `deploy/agentd.service`, `docs/architecture.md` (Security model) |
