# Adaptive UI — Loop 6: the interface as a learned faculty

> The shell stops being static chrome the human arranges and becomes something the
> agent **stages, looks at, and is corrected through** — journaled and reversible like
> every other faculty. Phases A1 + A2 shipped; this doc is the durable contract.
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
| `ui_arrange` | `layout`, `apps?` | Stage a preset topology (A2, below). One toast per arrange |
| `ui_theme` | `persona` | Switch the persona skin via the `apply_persona` chokepoint (A2, below) |
| `ui_query` | — | GET the shell's `/state` → structure JSON (below). Graceful "no display" note on headless |

The app catalog = the 20 `AppKind` slugs: `chat system sensor sessions settings terminal
council event-log mesh inference audio-editor sonus notes face sketchpad web calculator
explorer occipital board`.

### `ui_arrange` — preset topologies (A2)

`layout ∈ focus | split | main-side | grid` — a closed set; the pure, unit-tested
`arrange_rects` (ui-slint) turns *(layout, n, desktop area)* into rects, and the WM owns
every pixel. The desktop area is exported from Slint (`desktop-area-w/h` out-properties
on AppWindow, sharing the root taskbar-zone metrics) so Rust tiles into exactly the
surface windows clamp to.

- **Participants**: `apps` in **priority order** (first = the main slot). Listed windows
  not yet open are opened through the same latch-respecting path as `ui_open` but
  *quietly* — one arrange, one toast. Latched apps sit out. `apps` omitted = the
  currently visible windows topmost-first (minimized ones the user tucked away are NOT
  resurrected). Capped at 6 (`ARRANGE_MAX`, grid 3×2); the tool rejects longer lists.
- **`focus`** means one thing on stage: the main window gets the near-full rect and
  every other open window minimizes (reversible from the taskbar).
- **`split`** = n equal columns left→right; **`main-side`** = first pane ~62% left, the
  rest stacked in the right column; **`grid`** = ceil-sqrt uniform cells, row-major.
- **Desktop shell mode only** — the Focus shell has no window layer, so an arrange
  there is a structural no-op (`ui_query.shell_mode` tells the agent which it is; the
  femtovg Nano tier is Focus-clamped by design).

### `ui_theme` — persona skin (A2)

`persona ∈ apex | mom | ubuntu-dad | windows-dad | tech-kid | aurum` (closed, mirrors
`persona_from_slug`). Routes through `apply_persona` — the SAME chokepoint as the
picker: theme + chrome + wallpaper + the persona's default shell mode (tech-kid boots
the Focus face) + the agent voice (`set_persona` → the style layer) + persistence.
Attribution toast ("🎨 APEX switched the skin to Simple"). Open question #1 resolved:
policy `allow` — the etiquette is *offer first, the conversational yes is the
confirmation* (the `eject_media` trust pattern), and a skin flip is one tap to revert.

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
   (re-invitation). The agent sees latches in `ui_query` and treats them as feedback to
   learn from (deposit the correction), not an obstacle. **The Occipital reader is fully
   folded in (A3)**: its auto-reveal goes through the same latch-aware
   `agent_open_window` (so it lands in `agent_opened`), and it **force-latches on ANY
   user close** — auto-reveal makes it agent-ish even when the user opened it. The old
   standalone `OCCIPITAL_SUPPRESS` flag is gone.
2. **Agent acts are never user signals.** Agent-close sets no latch; since the fold,
   there is no separate auto-reveal flag an agent open could re-arm — "auto-reveal
   armed" simply *is* "not latched".
3. **The rate rail (A3).** At most `UI_TURN_MUTATION_CAP` (4) `ui_*` mutations apply per
   turn — an adaptation is a deliberate act, not a strobe. Beyond the cap, verbs drop
   silently; the live counter rides `/state` (`turn_mutations` / `mutation_cap`) so the
   agent *sees* the throttle. Resets on TurnComplete, cancel, and session switch. The
   reader's ambient auto-reveal doesn't spend a slot (it isn't a verb).
4. **The drag guard (A3).** `WmState.dragging-id` (a Slint global set by the frame's
   title-bar/resize touch areas) marks the window under live pointer interaction:
   `ui_arrange` skips its geometry (and won't minimize it in `focus`), `ui_close` won't
   yank it. The agent never fights the hand.
5. **Attribution.** Creates toast "🪟 APEX opened …" — adaptations are visible acts,
   and the event log + session JSONL journal every call.
6. **Quiet by default** (soul-level): adapt at task boundaries, show-don't-tell, don't
   theme unprompted — offer, where the conversational yes IS the confirmation. The seed
   `config/soul.md` carries the etiquette section ("Your stage"); live nodes adopt it
   through their own `propose_evolution` (the config-changes discipline).

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
| A1 | `/state` + `ui_query` + `ui_open`/`ui_close`/`ui_focus` + latch | **shipped** (#255, latch field-confirmed on the colony) |
| A2 | `ui_arrange` presets + layout fn; `ui_theme` via `apply_persona` | **shipped** (#256, field-confirmed) |
| A3 | Etiquette pass: rate rail (4/turn, `/state`-visible), drag guard (`WmState.dragging-id`), occipital latch fold, seed-soul etiquette (live nodes via `propose_evolution`) | **shipped** |
| B | Loop-6 memory: deposit discipline (seeded in the soul's "Your stage"), UI-prefs procedure, geometry persistence | next |
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
| Layout presets | tools.rs `UI_LAYOUTS` ↔ ui-slint `ARRANGE_LAYOUTS` (+ `UI_ARRANGE_MAX` ↔ `ARRANGE_MAX`) | tests each side; `arrange_rects` rejects unknown layouts regardless |
| Persona slugs | tools.rs `UI_PERSONAS` ↔ ui-slint `persona_from_slug` | tool test; UI ignores unknowns |
| Desktop area | AppWindow `desktop-area-w/h` out-props ↔ the window layer's `area-w/h` clamps | both derive from the SAME root props (`title-bar-h`, `tb-zone`) — keep new chrome metrics on root |

## 10. Design principles

1. **Adaptation follows attention** — show-don't-tell, never decorative motion.
2. **The human always wins** — latch, reversibility; an overruled adaptation is a
   learning signal, not a retry.
3. **Topology, never geometry** — intents from closed vocabularies; Rust owns pixels.
4. **Quiet by default** — an interface *set correctly when you look up*, not churn.
5. **Everything remembers why** — no adaptation without a rationale deposit (Phase B).
6. **Below-inference first** — if a reflex covers it, no model call. Tokens are for
   judgment, not for opening windows.
