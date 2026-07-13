# Adaptive UI — Loop 6: the interface as a learned faculty

> The shell stops being static chrome the human arranges and becomes something the
> agent **stages, looks at, and is corrected through** — journaled and reversible like
> every other faculty. Phase A1 shipped; this doc is the durable contract.
> (Graduated from the grounded plan v0.2, 2026-07-06; verified against live code at ship time.)

The one-sentence version: **the agent speaks a small closed vocabulary of staging verbs
(tools, not protocol), the UI is the last validator and the human always wins, and the
agent learns whether an adaptation landed by looking — not from acks.**

---

## 1. Locked decision — the vocabulary is a tool family, not a protocol extension

The `ui_*` verbs ride the `display_face` idiom: a generic `tool_requested` WS event
carries `{tool, args}`; ui-slint intercepts those tool names in its `Event::ToolRequested`
handler, suppresses the tool card, and mutates UI state directly. The tool process
(apexos-tools) only **validates + echoes**. The protocol layer has zero per-tool knowledge.

What the tool layer gives for free: policy gating per verb (`config/policy.toml`),
teaching via tool descriptions (cache-stable, un-forgettable), automatic registry
surfacing in the embodiment block, journaling in session history/JSONL by construction,
deployment via `sync_policy_rules` on `apexos-update`, and renderer-agnosticism (any
frontend honors any subset; the PWA ignores them gracefully). Zero agentd changes.

Design rules (violating any of these is a regression):

- **Closed enums everywhere.** `app` validates against the AppKind catalog on BOTH ends
  (tools `UI_APPS`, ui-slint `APP_TABLE`). Malformed is inexpressible, not caught.
- **Topology, never geometry.** No pixel args exist. Rect math is the WM's business.
- **Additive-only.** New capability = new verb. Never repurpose an existing one.
- **Fire-and-forget mutations, verify by looking.** The echo reports what was
  *requested*, not what *landed*. When it matters, the agent calls `ui_query`
  (structure) or `screenshot_mirror` (pixels). Real transactional acks are Tier-B.

## 2. The v1 vocabulary

| Verb | Args | Semantics (all: unknown/inapplicable target = ignored, not an error) |
|---|---|---|
| `ui_open` | `app`, `hint?` | Open-or-reveal the single window of that kind (full menu-launch path, per-app refresh included). Latch-guarded. Toast on create ("🪟 APEX opened …") for attribution. `hint` is reserved (echoed, not yet interpreted) |
| `ui_close` | `app` | Remove the window. Agent-close ≠ user-close: sets **no** latch and never arms the occipital auto-reveal suppression |
| `ui_focus` | `app` | Un-minimize + raise + focus an existing window. No-op if not open |
| `ui_query` | — | GET the shell's `/state` → structure JSON (below). Graceful "no display" note on headless |

Phase A2 adds `ui_arrange {layout, apps?}` (closed preset topologies: focus / split /
main-side / grid) and `ui_theme {persona}` (via the `apply_persona` chokepoint).

The app catalog = the 20 `AppKind` slugs: `chat system sensor sessions settings terminal
council event-log mesh inference audio-editor sonus notes face sketchpad web calculator
explorer occipital board`.

## 3. The eyes — `/state` on the snapshot server

The UI's shape is per-turn-volatile and must never enter the embodiment block (the
prompt-cache discipline). So the agent pulls it on demand: ui-slint's loopback snapshot
server (`APEXOS_UI_SNAPSHOT_ADDR`, :8788) serves **`/state`** beside `/snapshot`:

```json
{
  "shell_mode": "desktop|focus",
  "persona": "apex",
  "windows": [{"app": "sensor", "title": "Sensors", "minimized": false,
               "maximized": false, "focused": true}],
  "agent_opened": ["sensor"],      // windows the agent created (still open)
  "latched": ["settings"],         // user closed after agent opened — ui_open suppressed
  "apps": ["chat", "..."]          // the valid catalog, self-describing
}
```

`ui_query` fetches it exactly the way `screenshot_mirror` fetches `/snapshot`
(`APEXOS_UI_STATE_URL`, default `http://127.0.0.1:8788/state`). The mutation tools
best-effort pre-read it for smarter echoes (e.g. "suppressed: latched") but **never
require it** — remote-UI dev setups keep working. Deliberately structural, not
geometric: no rects in the payload.

## 4. Etiquette — the human always wins

The interaction contract lives in mechanism, not approval gates (the whole family is
policy `allow`, same trust basis as `display_face`/`sketch_draw`: benign, reversible,
in-canvas):

1. **The latch.** Two per-AppKind bitmasks in ui-slint (`AGENT_OPENED`, `UI_LATCHED`,
   bit = ordinal): `ui_open` creating a window marks it agent-opened; a **user close**
   of a marked window moves the bit to latched — `ui_open` for that app is silently
   suppressed for the rest of the session. A **user menu-launch** clears both bits
   (re-invitation, mirroring the occipital reader's suppress-clear). The agent sees
   latches in `ui_query` and treats them as feedback to learn from (deposit the
   correction), not an obstacle.
2. **Agent acts are never user signals.** Agent-close sets no latch; an agent `ui_open`
   of the occipital reader uses the raw launch path so it can't re-arm auto-reveal
   (clearing `OCCIPITAL_SUPPRESS` is the *user's* menu-launch signal).
3. **Attribution.** Creates toast "🪟 APEX opened …" — adaptations are visible acts,
   and the event log + session JSONL journal every call.
4. **Quiet by default** (soul-level, Phase A3): adapt at task boundaries, show-don't-tell,
   don't theme unprompted — offer, where the conversational yes IS the confirmation.
   Rate limit + drag guard land in A3 as mechanism.

## 5. Loop 6 memory (Phase B) — adaptation without accumulation is amnesia

Deposits ride existing contracts, zero cerebro changes (tags are the coupling):
rationale memories tagged `ui-adaptation` ("opened Sensors during the thermal alert —
André checks visuals first"); stable preferences promoted to procedures (CCBS surfaces
them at wake); mechanical geometry persistence stays UI-local beside the persona file.
Cerebro remembers the *why*; the UI remembers its *shape*. Don't blur these.

## 6. Reflexes (Phase C) — below-inference adaptation

`ui_reflex {on, do, app}`: the agent installs event→action rules the UI executes
directly off its own event stream — zero tokens, zero latency ("sensor_alert → open
sensor"). A lookup table in `dispatch_event`, persisted UI-locally; installs deposit
rationale memories; latches apply. Also the answer to session-scoping: `tool_requested`
is session-scoped, so a root-session 3am alert can't reach a UI socket following another
session — reflexes fire UI-side off *global* events, which is exactly the alert case.

## 7. Tier B — Bevy, evidence-gated

Loop 6 ships and proves itself on Slint across every tier including the Pi kiosk. A
`ui-bevy` frontend (free tiling, `ui_compose`, constraint solver, transactional applier
with real acks) is taken up only if Tier-A field evidence shows novel composition adds
real value. Details + honest cost note: the plan archive
(`~/Projects/plan_drafts/ApexOS-RS/`, v0.2 §6) and `docs/ui-glowup.md`.

## 8. Roadmap

| Phase | Deliverable | Status |
|---|---|---|
| A1 | `/state` + `ui_query` + `ui_open`/`ui_close`/`ui_focus` + latch | **shipped** |
| A2 | `ui_arrange` presets + layout fn; `ui_theme` via `apply_persona` | next |
| A3 | Etiquette pass: rate limit, drag guard; soul etiquette section (via `propose_evolution`) | — |
| B | Loop-6 memory: deposit discipline, UI-prefs procedure, geometry persistence | — |
| C | `ui_reflex` family | — |
| D | Colony field cycle (apex1 kiosk / apex-3 desktop), dream-consolidation check | — |
| E | Decision gate: Bevy Tier B — go/no-go on Tier-A evidence | — |
| F | Fast-model field test (APEX-on-Cerebras via the OAI-compat backend) | independent |

## 9. Sync points (the price of the two-crate split)

| What | Where | Locked by |
|---|---|---|
| App slugs | tools.rs `UI_APPS` ↔ ui-slint `APP_TABLE` | test each side (count + closed-enum); a new `AppKind` needs both + a slug in the `ui_open` description (auto — it interpolates `UI_APPS`) |
| Ordinals | `APP_TABLE` index ↔ `kind_from_ordinal` ↔ types.slint declaration order | ui-slint test `app_table_is_the_ordinal_order` |
| Latch bits | `u32` masks | test asserts catalog ≤ 32 |

## 10. Design principles

1. **Adaptation follows attention** — show-don't-tell, never decorative motion.
2. **The human always wins** — latch, reversibility; an overruled adaptation is a
   learning signal, not a retry.
3. **Topology, never geometry** — intents from closed vocabularies; Rust owns pixels.
4. **Quiet by default** — an interface *set correctly when you look up*, not churn.
5. **Everything remembers why** — no adaptation without a rationale deposit (Phase B).
6. **Below-inference first** — if a reflex covers it, no model call. Tokens are for
   judgment, not for opening windows.
