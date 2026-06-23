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
| 3b-2 | **Per-agent soul** | A bound session also loads its agent's `soul_file` → `engine.with_system(Some(soul))` (composed with the slice-2 priming). Unbound/APEX → the global soul. Reads the 3a store. | ✅ shipped — `root_turn` loads identities (seeded at startup), `agent_soul_for()` reads a bound non-default agent's `soul_file` (async, graceful → default on miss), composed `with_system(soul).with_priming(block)`. **Self-evolution is per-agent too** (`soul_target_for`): `read_soul_md` reads, and `propose_evolution{UpdateSystemPrompt}`/apply + rollback write, the **bound** agent's own `soul_file`; only APEX/unbound touches the global `soul.md`+`soul_arc`. (Fixed the bug where a bound agent's soul-evolution clobbered APEX's global soul — surfaced live on apex2.) |
| 3c | **Identity API + lockout** | HTTP CRUD (list/create users+agents, seed soul file) + `verify` (PIN + 5-guess lockout). Drives the UI. | ✅ shipped — token-gated `GET /api/identities` (PIN redacted), `POST /api/identities/{user,agent,verify}`; `PinLockout` (5 fails → 5-min cooldown, in-memory); agent create seeds `souls/<id>.md`; install.sh chowns `identities.toml` + `souls/` |
| 3d | **Boot UI** (ui-slint) | `IdentityWizard` component: profile tiles → numeric PIN keypad (Rust owns the buffer; verifies via 3c) → agent tiles; picking an agent binds the session (`hello{agent_id}`). | ✅ shipped — gated so the trivial single-owner+APEX node boots straight through unchanged; renders above the persona first-boot. **Polish/harden pass:** accent lock badge on PIN profiles; empty-state on the agent step for a profile owning no agents (no blank dead-end — Back still returns to the picker); keypad message distinguishes lockout / unreachable-agentd from a wrong guess |
| 3e | **Session-token auth** (login → minted credential → gate) | Closes the connection-auth gap: a login mints a short-lived bearer token, so a human client (desktop UI / web / PWA) authenticates **without the node's shared `AGENTD_TOKEN`** — that retreats to being the machine / mesh / admin secret (node↔node a2a, kiosk-as-root, operator curl/CI). `POST /api/auth/login {user_id, pin}` is **UNgated** (authenticated by the PIN itself, mirroring `/api/mesh/pair/claim`) → verifies the profile (reusing the 3c per-user guess-lockout) → mints a **256-bit** token (in-memory, **24 h**, **never persisted** → cleared on restart, re-login) → `{token, agent_id, expires_in}`. `require_token` then accepts **the admin token OR a valid session token** (header or `?token=` on the WS); `POST /api/auth/logout` revokes. **Open profile = LAN-trusted one-tap** (mints with no secret — the decided auth-weight); a PIN profile is verified + lockout-guarded. | ✅ shipped (server) — `apexos_gateway::session_auth` (pure `SessionStore`: insert/verify/revoke/sweep, unit-tested with injected `Instant`s; `gen_session_token` from `/dev/urandom`). **Native UI wiring also shipped** (ui-slint): with no env `AGENTD_TOKEN`, the IdentityWizard runs as a **login screen** (fetches the UNgated `GET /api/auth/profiles` → tiles; open profile = one tap, PIN profile = keypad → `/api/auth/login`), then **re-execs itself with `AGENTD_TOKEN=<minted token>`** so the proven connection path runs unchanged — no boot refactor, no token-copy. With an env token (kiosk/dev) the wizard stays the identity-selector (unchanged). An agentd restart drops the in-memory token → next launch re-shows login. Web/PWA use the same endpoints natively. |

> Slice 3 split into 3a/3b/3b-2/3c/3d during build, smallest-first: data layer
> (3a, inert) → memory binding (3b) → per-agent soul (3b-2) → API (3c) → UI (3d).
> Each keeps the hot-path blast radius small (the unbound path stays byte-identical).

The cognitive boot loop is the **missing middle**: Slice 1 *enforces* an identity, Slice 3
*lets a human pick* one, and Slice 2 is *what loads when an identity wakes up*.

> **Arc complete (all slices shipped).** Identity is system-stamped (1), the agent wakes
> oriented (2), the registry + API + PIN exist (3a/3c), selecting an agent switches its
> memory *and* soul (3b/3b-2), a human picks at boot (3d), and the wizard was polished/hardened
> (lock badge · agent empty-state · lockout messaging). a human picks at boot (3d), and **connection auth is real** — a login mints a session
> token so the desktop/web/PWA client no longer needs the shared node secret (3e, server **and**
> native-UI wiring — login screen → `/api/auth/login` → re-exec with the minted token).
> **Binding security closed (3e):** the multi-agent `hello{agent_id}` bind is now **auth-gated** — a
> session-token human may only bind an agent **they own** (`Identities::agents_for(user)`); a
> disallowed/blank request falls back to their own `default_agent`, so a guest can never inherit APEX
> (the node owner's agent). Gated at every entry: the initial connect session, `hello{new}`/`{resume}`,
> and an explicit `{agent_id}` pick. The admin / token-less path is trusted (binds anything). And
> **bindings are evicted when the socket closes**, so a later resume must re-bind (and re-gate) rather
> than silently re-enter a stale identity. The pure gate (`session_auth::gate_agent_bind`) is
> unit-tested; the WS wiring (`resolve_ws_auth` → `handle_socket`) recovers WHO from the session token.
> Residual edge: a profile that owns **no** agents falls through to the node default (a setup error —
> the boot flow gives every user an agent).
>
> **Set-default + skip (3e, closed).** The registry carries a `default_user`; the **UNgated**
> `/api/auth/profiles` exposes it so the login screen **auto-skips** the picker — an OPEN default logs in
> with zero taps, a PIN default jumps straight to the keypad (`‹ Back` still reaches the picker). It's
> set/cleared from **Settings → LOGIN** (a toggle: `GET /api/auth/me` tells the client who it's logged in
> as, `POST /api/auth/default {user_id}` sets, `{""}` clears — both **gated**). The kiosk/device-token
> path has no per-profile login, so the toggle hides there. **Slice 3e is fully closed**, and with it the
> whole identity arc.

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
