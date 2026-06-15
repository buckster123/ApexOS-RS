# Agent Identity — who is acting, and how the system knows

> Sibling to [symbiosis.md](symbiosis.md), [evolutionary-layer.md](evolutionary-layer.md),
> and [edk.md](edk.md). Those three answer *stays the same agent over time*, *gets better
> over time*, and *grows a bigger body*. This one answers the question underneath all of them:
> **which agent is this, and is that identity enforced — or merely remembered?**

Every other layer assumes an answer to one question and never checks it: *whose* memory is
this, *whose* soul is evolving, *whose* body is this node? Today that answer is supplied by
the agent itself — APEX passes its own `agent_id` on every Cerebro call. That works while
there is exactly one agent and it never makes a mistake. It is not a foundation for a
multi-user, multi-agent OS, and it is a quiet security gap even for a single agent.

This charter makes identity **ambient and system-enforced** instead of **agent-supplied**,
and then builds the human-facing flow that lets a person pick which identity boots.

---

## The problem, precisely

Three symptoms of one root cause — *identity lives in the agent's output, not in the system*:

1. **Soft Cerebro routing.** Every memory call (`recall`, `remember`, `session_save`, …)
   takes an `agent_id` argument that the **model fills in**. Cerebro's isolation is real —
   `Visibility::Private ⇒ agent_id == node_agent_id`, enforced in SQL
   (`visibility='shared' OR (visibility='private' AND agent_id=?)`, `cerebro/.../types.rs`).
   But the boundary is only as good as the argument the model remembered to pass. Forget it,
   typo it, or (in a multi-agent world) pass *someone else's*, and memories land in — or leak
   from — the wrong space. The lock is sound; the key is handed to the prisoner.

2. **Identity-string drift.** Even the single agent isn't named consistently in code:
   `"APEX"` in the council handler, `"CLAUDE-APEX"` in the rollback store
   (`agentd/.../main.rs`). No single source of truth for "who am I."

3. **No human-facing identity at all.** The device boots straight into one agent (APEX) with
   one presentation. There is no notion of *users* of the box, and no way to run a second
   agent — a custom main, or a sub-agent — alongside APEX with its own memory and soul.

---

## What an "agent identity" is

An identity is **four facets bound to one `agent_id`**. The `agent_id` is the spine — it is
the Cerebro space key and the thing the system stamps everywhere.

| Facet | Lives in | Scope | Notes |
|-------|----------|-------|-------|
| **Memory space** | Cerebro `agent_id` (private/shared visibility) | **per-agent** | The isolation boundary. Private memories are visible only to their `agent_id`; `shared` crosses agents deliberately. |
| **Soul** (who I am) | `soul.md` | **per-agent** | Portable identity; evolves at runtime (see [symbiosis.md](symbiosis.md)). Today one file; multi-agent needs one soul per agent. |
| **Permissions** | `policy.toml` | per-agent *(open: or a shared default + per-agent overrides)* | What this agent may do without asking. |
| **Presentation** | persona / skin (theme + chrome + wallpaper + shell-mode) | **per-agent default, user-overridable** | A skin is *worn*, not *who you are* — see [ui-glowup.md](ui-glowup.md) §5. An agent has a default skin; a user may override and pin it. |

**Distinctions that matter:**

- **Agent ≠ skin.** The persona/skin system is presentation. APEX in a mac skin and APEX in a
  win98 skin is the *same agent* (same memory, same soul). Two agents in the same skin are
  *different agents*. Identity is the cognitive entity; the skin is its clothes.
- **Agent ≠ node.** The body ([edk.md](edk.md)) is per-node and physical; identity is portable
  and rides the mesh. The same APEX can inhabit the Pi 5 today and a GPU node tomorrow — the
  soul and memory travel, the embodiment block is swapped underneath (see the soul-vs-embodiment
  rule in CLAUDE.md).
- **Agent ≠ session.** A `SessionId` is one conversation/turn-stream; an agent identity spans
  many sessions (and reboots). One agent, many sessions; the binding is session → identity.

---

## The enforcement principle

> **Identity is stamped by the system at the boundary, never trusted from the agent's output.**

agentd holds the **session → identity** binding (today: every session → `APEX`). When a tool
call crosses into a plugin, agentd **stamps the bound `agent_id` onto the call**, overwriting
whatever the model produced. The model *cannot* write to another agent's space, because it no
longer chooses the space — the same way the gateway already injects `session` into inbound
frames rather than trusting the client to.

Concretely, the seam is `Supervisor::dispatch_tool` (`agentd/.../supervisor.rs`): for any
Cerebro-namespaced tool, set `call.args["agent_id"]` (and an owning-user field) from the
session's identity before forwarding to the MCP plugin. This is the executable form of the
note: *"system-provided `user: Andre, agent: APEX, …` follows along and is hooked into Cerebro
calls for routing."*

Why this is both **security** and **correctness**:
- *Security / isolation* — a session bound to agent B physically cannot read or write agent A's
  private space, even if the model is confused, jailbroken, or buggy.
- *Correctness* — no more "forgot to pass `agent_id`" memory bleed; routing is deterministic.

It is also a **prerequisite for everything downstream**: competence accrual
([evolutionary-layer.md](evolutionary-layer.md)) and the cognitive boot
([symbiosis.md](symbiosis.md)) are only trustworthy once "whose memory" is enforced rather
than hoped.

---

## The human layer — user ↔ agent ↔ skin

A device may have more than one person using it, and (once enforcement exists) more than one
agent. The boot flow makes that explicit, layering on the skin-select that already exists:

```
boot ─▶ select / register USER ─▶ select AGENT ─▶ select SKIN ─▶ desktop ready
                                  (APEX + any         (default-and-skip
                                   custom agents)      if a default is set)
```

- **User** — a human profile on the box. The boot flow selects or registers one.
- **Agent** — a cognitive identity (soul + Cerebro space + policy). **APEX is the built-in
  main.** A user may have custom agents alongside it — additional *mains*, or *sub-agents*
  promoted to first-class — each with its own enforced space.
- **Skin** — the presentation the chosen agent wears; default per agent, overridable per user,
  with a "set default + skip this step" option in Settings and on the select screen.

This is where the design has one genuinely open product decision:

### Auth weight — DECIDED: profile-select + optional per-profile PIN

| Option | What it is | Trade-off |
|--------|-----------|-----------|
| ✅ **Profile + optional PIN** | Pick a profile; a profile *may* set a 4–6 digit PIN | Light but real — keeps a kid's agent out of a parent's private memory space without turning a Pi kiosk into a login server. No PIN = one tap |
| ~~Profile-select only~~ | Pick a user, no secret ever | Rejected: no isolation between humans on a shared box |
| ~~OS-user-backed~~ | Map to system users / PAM | Rejected: overkill/heavy for the spare-device tier |

The PIN is salted-hashed at rest (`sha256(salt‖pin)`); its real protection is an
API-side guess **lockout** (a 3b sub-slice), since a 4-digit PIN is low-entropy
regardless of hash strength.

---

## Build slices (smallest-first; each ships as its own PR)

| # | Slice | What lands | Status |
|---|-------|-----------|--------|
| 1 | **System-stamped Cerebro identity** 🔑 | agentd binds session→`agent_id` and stamps it onto every Cerebro call in `dispatch_tool` (overriding the model). Unify the `APEX`/`CLAUDE-APEX` drift to one source of truth. Default identity `APEX` → **zero behavior change for today's single agent**; pure hardening + the substrate multi-agent needs. | ✅ shipped — `apexos_core::node_agent_id()` (env `AGENTD_AGENT_ID`, default `APEX`); `Supervisor` caches it and `stamp_agent_id()` overrides `agent_id` on `cerebro`-plugin calls; council + rollback-store writes unified to it |
| 2 | **Per-identity cognitive boot** | CCBS injection at session start keyed to the session's identity (select agent X → boot X's skills/intentions/memories) + nightly `dream_run` schedule. Absorbs the open symbiosis steps 3–4 (now unblocked: `cognitive_bootstrap` is implemented, not the stub the old BACKLOG claims). | ✅ shipped — `root_turn` calls `cognitive_bootstrap` via `ToolProxy` on a session's first turn (cached, 15s-bounded, graceful), composed into the prompt as `soul+embodiment+priming` by `TurnEngine::with_priming`; both scoped to `node_agent_id()`. Nightly `dream_run` runs as a dedicated direct-call task (`spawn_nightly_dream`, cron `AGENTD_DREAM_CRON`). Opt-out `AGENTD_CCBS=0` |
| 3a | **Identity store** (data layer) | `User`/`AgentRecord`/`Identities` in `apexos_core::identity`: toml persistence (`identities.toml`), `seed_defaults` (owner + APEX), optional salted PIN (hash/verify, constant-time). Pure + unit-tested, **inert** (no wiring → zero hot-path risk). | ✅ shipped |
| 3b | **Per-session binding** (memory) | A `hello` frame may carry `agent_id`; agentd records `SessionId→agent_id` (`SessionBindings`). The slice-1 stamp + slice-2 CCBS boot resolve identity via `resolve_agent_id(session)` — bound agent → else `node_agent_id()`. So selecting an agent switches its **Cerebro memory space**. Unbound = APEX (current behavior). | ✅ shipped — `apexos_core::{SessionBindings, resolve_agent_id}`; gateway binds on `hello`, supervisor stamp + `root_turn` CCBS resolve per-session |
| 3b-2 | **Per-agent soul** | A bound session also loads its agent's `soul_file` → `engine.with_system(Some(soul))` (composed with the slice-2 priming). Unbound/APEX → the global soul. Reads the 3a store. | ▢ |
| 3c | **Identity API + lockout** | HTTP CRUD (list/create users+agents, seed soul file) + `verify` (PIN + 5-guess lockout). Drives the UI. | ▢ |
| 3d | **Boot UI** (ui-slint) | Extend the first-boot wizard into `user → (PIN) → agent → skin → desktop`, wired to 3c; "set default + skip" persistence. | ▢ |

> Slice 3 split into 3a/3b/3b-2/3c/3d during build, smallest-first: data layer
> (3a, inert) → memory binding (3b) → per-agent soul (3b-2) → API (3c) → UI (3d).
> Each keeps the hot-path blast radius small (the unbound path stays byte-identical).

The cognitive boot loop is the **missing middle**: Slice 1 *enforces* an identity, Slice 3
*lets a human pick* one, and Slice 2 is *what loads when an identity wakes up*.

---

## Boundaries with the other charters

- **vs [symbiosis.md](symbiosis.md)** (same agent over time) — symbiosis keeps an identity
  *coherent across model swaps and resets*; this charter says *which* identity that is and
  enforces its edges. Slice 2 is literally symbiosis's boot loop, made per-identity.
- **vs [evolutionary-layer.md](evolutionary-layer.md)** (better over time) — competence accrues
  in an agent's Cerebro space. Enforcement is what makes "APEX got better at X" mean APEX, not
  a leaked aggregate of every agent on the box.
- **vs [edk.md](edk.md)** (bigger body) — orthogonal axis: body is per-node and physical,
  identity is portable across the mesh. A request-to-incarnate changes the body; the identity
  riding it is unchanged.
- **vs self-evolution hardening** (separate track) — the `EvolutionId` global counter is
  already fixed; the remaining bits (cold-start rollback JSON parse, defer `propose_evolution`
  ack) are pure correctness, do anytime. They become **more** relevant once multi-agent lands,
  because soul/policy evolution becomes *per-agent* — so land them before Slice 3, but they are
  not part of this arc.

---

## The one-sentence version

Identity stops being something the agent *says* and becomes something the system *stamps*: one
enforced `agent_id` per session binds a soul, a memory space, a policy and a skin — so a human
can pick who boots, agents can't read each other's minds by accident, and every other layer
finally has a trustworthy answer to "whose?".
