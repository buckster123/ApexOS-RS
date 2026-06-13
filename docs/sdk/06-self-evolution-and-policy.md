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
artifact, one apply action each (`agentd/crates/core/src/types.rs:86-114`). It serializes
`{"kind": "...", ...}` (serde `tag = "kind"`, `snake_case`):

- `update_system_prompt { content, reason }` — full replacement of `soul.md`
  (`:107`). Not a diff; full content makes rollback a trivial restore.
- `update_policy_rule { tool_pattern, new_rule, reason }` — set one `[rules]` entry
  (`:99`). `new_rule` is a `PolicyRule` (`allow`/`ask`/`workspace`) — **not** a
  `PolicyMode`.
- `register_mcp_server { name, command, env, reason }` — add a plugin (`:89`).
- `unregister_mcp_server { name, reason }` — remove one (`:95`).
- `hot_reload_subsystem { subsystem }` — re-read a subsystem in place (`:111`),
  `subsystem ∈ {plugins, policy, agent, gateway}` (`Subsystem`, `:77`).

**The three virtual tools** the agent actually calls (declared in
`agentd/crates/agentd/src/main.rs` `gather_tools` :1194, intercepted in
`agentd/crates/plugins/src/supervisor.rs` `dispatch_tool` :299):

| Tool | Spec | Intercept | What it does |
|------|------|-----------|--------------|
| `read_soul_md` | `main.rs:1243` | `supervisor.rs:399` | returns the live `soul.md` from the shared `soul_arc` (`RwLock<String>`) |
| `propose_evolution` | `main.rs:1257` | `supervisor.rs:302` | deserializes args into `EvolutionProposal`, emits `EvolutionProposed`, acks |
| `rollback_evolution` | `main.rs:1321` | `supervisor.rs:349` | routes `(session, call_id, evolution_id)` to the applier's rollback channel |

These never hit a child process — `dispatch_tool` intercepts them before the MCP fallthrough.
`query_audit` is a **Cerebro** tool (`cerebro-mcp` `dispatch.rs:631`, schema `tools.rs:612`),
used pre-flight to confirm the rollback snapshot/episode exists.

**The applier** (`spawn_evolution_applier`, `main.rs:422`) is the only writer. On each
`EvolutionProposed` it:
1. opens a Cerebro episode (`episode_start`, `main.rs:585`, best-effort);
2. computes the inverse proposal (`compute_undo`, `main.rs:712`) *before* applying;
3. applies (`apply_evolution`, `main.rs:808`) — writes the file + hot-reloads the live
   `Arc`;
4. stores the undo in the in-memory `rollback_store: Arc<Mutex<HashMap<EvolutionId,
   EvolutionProposal>>>` **and** journals it into the episode as a memory tagged
   `["evolution","undo_snapshot"]` (`episode_add_step`, `main.rs:598`);
5. emits `Event::EvolutionApplied { id, proposal, patch_summary, applied_by }` (`:467`).

**Rollback** (`main.rs:489`) pops the undo proposal from `rollback_store` and feeds it back
through the same `apply_evolution`. The undo is itself a proposal, so the mechanism is
symmetric. On cold start the store is rebuilt best-effort from Cerebro episodes
(`main.rs:636`, `parse_undo_snapshot_from_text` :700) — but if Cerebro is unavailable, an
evolution applied in a *prior* daemon session has **no in-memory undo** and
`rollback_evolution` returns `no rollback snapshot for evolution N` (`main.rs:498`).

**Durability gotcha.** `update_system_prompt` writes `soul.md` with a plain
`tokio::fs::write` (`main.rs:820`); `update_policy_rule` uses `write_atomic` (`main.rs:778`).
`write_atomic` tries temp+rename but **falls back to a non-atomic in-place write** when the
parent dir is root-owned (the deployed `/etc/agentd` case) — see
[Policy / safety](#policy--safety).

---

## Add a new evolution (the common case — config, no Rust)

You are an agent. You want to change your own config. You do **not** edit Rust; you call
`propose_evolution` with the right `kind`. The applier does the rest.

### Pre-flight discipline (do this every time, in order)

1. **`read_soul_md`** — only when proposing `update_system_prompt`. Always edit from the
   *live* content, not your in-context snapshot; another evolution may have changed it
   since you last saw it. (The spec literally says "ALWAYS call this first",
   `main.rs:1247`.)
2. **`query_audit(agent_id="CLAUDE-APEX")`** — confirm the evolution episode/rollback trail
   is being recorded in *this* daemon session. Rollback only works for the current session
   (in-memory store); if you can't see the trail, assume you cannot roll back.
3. **Summarise the change** in your message text before submitting — what changes, why,
   and the expected effect. This is the human-readable half of the audit trail.

### Submit the proposal

Call `propose_evolution`. Required args are always `kind` and `reason`; the rest depend on
`kind` (`propose_evolution_spec`, `main.rs:1264`):

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
(`main.rs:467/477`). Note the numeric `id` — it equals the `propose_evolution` call's
`ToolCall.id` (`EvolutionId(call.id.0)`, `supervisor.rs:303`). To revert:

```jsonc
{ "kind-less tool call": "rollback_evolution",
  "evolution_id": 42, "reason": "http_fetch=allow let a turn hit an internal URL; reverting" }
```

Rollback replays the inverse proposal; `HotReloadSubsystem` has **no** inverse
(`compute_undo` returns `None`, `main.rs:762`) so it cannot be rolled back.

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

Because `store_intention` had no prior rule, `compute_undo` returns `None`
(`main.rs:729-739`) — there is no inverse, so this one is **not** rollback-able. APEX notes
that and proceeds.

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
by `PolicyEngine.check` (`supervisor.rs:163`) *before* `dispatch_tool` ever runs. There is
**no `evolution.*` rule namespace** — the comment in `types.rs:248` describing one is
aspirational; the actual key is the literal tool name `propose_evolution`. In the shipped
`config/policy.toml`, `propose_evolution`/`rollback_evolution`/`read_soul_md` are **not
listed**, so under the default `mode = "suggest"` they hit the unknown-tool default →
`Decision::Ask` (`policy.rs:111`). That means: **every self-evolution requires human
approval out of the box.** `read_soul_md` is read-only but also gates by default (you may
want to allow-list it so pre-flight never stalls).

**Policy modes** (`PolicyMode`, `types.rs:29`; serialized kebab-case):

| Mode | Behaviour | Self-evolution effect |
|------|-----------|----------------------|
| `suggest` (default) | confirm everything not explicitly `allow` | every `propose_evolution` asks |
| `auto-edit` | `workspace` rules auto-approve inside `AGENTD_WORKSPACE` | unlisted tools still `ask`; doesn't auto-approve evolution |
| `yolo` | `check` returns `Allow` for everything, short-circuit (`policy.rs:89`) | evolution applies with **no gate** — use only on a throwaway rig |

**Rule syntax** (`[rules]` in `policy.toml`; values are `PolicyRule`):

- `allow` — auto-approve regardless of mode (yolo aside it's the only blanket allow).
- `ask` — always confirm.
- `workspace` — allow if the call's `path` arg canonicalizes inside `AGENTD_WORKSPACE`,
  else ask; rejects any path containing `..` (`policy.rs:118-138`). With no path or no
  `AGENTD_WORKSPACE`, it asks.
- Keys are an **exact tool name** or a `prefix.*` wildcard (`matches_wildcard`,
  `policy.rs:142`). **Exact match wins over wildcard** (`find_rule`, `policy.rs:96`). The
  wildcard only matches across a literal `.` — `cerebro.*` matches `cerebro.recall` but not
  `cerebro` or `cerebro_other`.

**The `PolicyRule` vs `PolicyMode` trap.** `update_policy_rule.new_rule` takes
`allow`/`ask`/`workspace` (a `PolicyRule`), **never** a mode name like `suggest`. The applier
validates the candidate `policy.toml` by re-parsing it into a `PolicyConfig` *before*
persisting (`main.rs:843`), so a bad rule is rejected rather than written — but if you smuggle
a mode name through a different path it would make `policy.toml` fail to deserialize and
**silently wipe every rule** on the next load (regression test `policy.rs:270`). Stay on the
three rule strings.

**The systemd sandbox is the real boundary, not the policy.** Evolution can only write what
the `agentd` user can write. `deploy/agentd.service` jails it: `ProtectSystem=strict`,
`ReadWritePaths=/var/lib/agentd /etc/agentd`, `NoNewPrivileges`. The four agent-mutable
files (`soul.md`/`policy.toml`/`plugins.toml`/`peers.toml`) are individually `chown`ed to
`agentd` so evolution can write them, while `/etc/agentd` *itself* stays root-owned to
protect the `600 root:root` token file. Consequence: `write_atomic`'s temp+rename fails at
the dir level and **falls back to a non-atomic in-place write** (`main.rs:790-804`) — a
crash mid-write could leave a torn `policy.toml`. A `register_mcp_server` evolution can name
an **arbitrary `command`** (`apply_evolution` :851 spawns it via the supervisor) — this is
agent-driven arbitrary process spawn, confined only by the sandbox and the `agentd` user's
permissions. Treat `register_mcp_server` as the highest-trust evolution kind; it should
stay `ask` even on permissive rigs.

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

1. **`agentd/crates/core/src/types.rs:88`** — add the variant to `EvolutionProposal` (and a
   `Subsystem` value if it's a reload target). Pick a `snake_case` tag; that becomes the
   `kind` string on the wire.
2. **`apply_evolution` (`main.rs:808`)** — add the match arm: write the config artifact
   (use `write_atomic` for anything under `/etc/agentd`), update the live `Arc`, and/or send
   a `SupervisorCmd`. Return a one-line `patch_summary`.
3. **`compute_undo` (`main.rs:712`)** — produce the inverse proposal (or `None` if there
   isn't one). Without this, the change cannot be rolled back.
4. **`propose_evolution_spec` (`main.rs:1257`)** — add the `kind` to the `enum` and document
   any new args, so the LLM tool schema advertises it.
5. **`config/soul.md` Self-evolution table** — add the row so APEX knows the kind exists
   (propose this as a soul edit; don't hand-edit on a deployed Pi).

If a config file fails to deserialize after your write, validate-before-persist (parse the
candidate into its config struct *before* writing, like `update_policy_rule` does at
`main.rs:843`) so a bad proposal can never corrupt the live file.

---

## Reference

### `EvolutionProposal` variants (`core/src/types.rs:86`)

| `kind` | Fields | Apply (`main.rs`) | Undo (`compute_undo`) | Rollback-able? |
|--------|--------|-------------------|-----------------------|----------------|
| `update_system_prompt` | `content`, `reason` | `:819` write soul.md + swap `soul_arc` | snapshot old soul text | yes |
| `update_policy_rule` | `tool_pattern`, `new_rule`, `reason` | `:826` edit policy.toml + swap PolicyEngine | snapshot old rule **if it existed** | only if rule pre-existed |
| `register_mcp_server` | `name`, `command`, `env`, `reason` | `:851` add to plugins.toml + `SpawnPlugin` | `UnregisterMcpServer{name}` | yes |
| `unregister_mcp_server` | `name`, `reason` | `:884` remove from plugins.toml + `KillPlugin` | re-`RegisterMcpServer` from disk (env lost) | yes (env not restored) |
| `hot_reload_subsystem` | `subsystem` | `:902` re-read agent/policy in place | `None` | **no** |

### `Subsystem` values (`types.rs:77`) — `hot_reload_subsystem` targets

| Value | `apply_evolution` effect |
|-------|--------------------------|
| `agent` | re-read soul.md from disk into `soul_arc` (`:904`) |
| `policy` | reload `policy.toml` into the PolicyEngine (`:910`) |
| `plugins` | no-op message — use register/unregister instead (`:916`) |
| `gateway` | unsupported without daemon restart (`:920`) |

### `PolicyRule` values (`[rules]` value; `types.rs:45`)

| String | Meaning |
|--------|---------|
| `allow` | auto-approve regardless of mode (yolo aside) |
| `ask` | always confirm |
| `workspace` | auto inside `AGENTD_WORKSPACE` (path arg, no `..`), else ask |

### `PolicyMode` values (`mode = ...`; `types.rs:29`)

| String | Meaning |
|--------|---------|
| `suggest` | confirm everything not `allow` (default) |
| `auto-edit` | `workspace` rules auto-approve inside the workspace |
| `yolo` | `check` returns `Allow` for everything |

### Agent-facing tools

| Tool | Required args | Returns | Source |
|------|---------------|---------|--------|
| `read_soul_md` | — | live soul.md string | spec `main.rs:1243`, intercept `supervisor.rs:399` |
| `propose_evolution` | `kind`, `reason` (+ per-kind) | `{status:"proposed", evolution_id}` | spec `main.rs:1257`, intercept `supervisor.rs:302` |
| `rollback_evolution` | `evolution_id`, `reason` | `{status:"rolled_back", summary}` | spec `main.rs:1321`, intercept `supervisor.rs:349` |
| `query_audit` (Cerebro) | — (opt `limit`, `agent_id`) | audit log entries | `cerebro-mcp dispatch.rs:631`, schema `tools.rs:612` |

### Events (`core/src/types.rs`)

| Event | Fields | Emitted by |
|-------|--------|-----------|
| `EvolutionProposed` | `id`, `proposal`, `proposed_by` | `supervisor.rs:309` |
| `EvolutionApplied` | `id`, `proposal`, `patch_summary`, `applied_by` | `main.rs:467` |
| `EvolutionRolledBack` | `evolution_id`, `reason`, `rolled_back_by` | `main.rs:513` |

### Files & code anchors

| Concern | Location |
|---------|----------|
| Proposal/event types, `PolicyMode`, `PolicyRule`, `Subsystem` | `agentd/crates/core/src/types.rs:20-114, 250-267` |
| Policy engine (rule eval, wildcard, workspace) | `agentd/crates/plugins/src/policy.rs` |
| Tool gate (`check` before dispatch) | `agentd/crates/plugins/src/supervisor.rs:161-177` |
| Virtual-tool interception | `agentd/crates/plugins/src/supervisor.rs:299-418` |
| Applier loop, `apply_evolution`, `compute_undo`, `write_atomic` | `agentd/crates/agentd/src/main.rs:422-926` |
| Tool specs (`gather_tools`) | `agentd/crates/agentd/src/main.rs:1194-1342` |
| Cold-start rollback restore | `agentd/crates/agentd/src/main.rs:636-708` |
| Default policy | `config/policy.toml` |
| Identity / self-evolution doctrine | `config/soul.md` (Self-evolution §), `docs/symbiosis.md` §5 |
| Sandbox / chown model | `deploy/agentd.service`, `docs/architecture.md` (Security model) |
