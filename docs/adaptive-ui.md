# Adaptive UI — Loop 6: the interface as a learned faculty

> The shell stops being static chrome the human arranges and becomes something the
> agent **stages, looks at, and is corrected through** — journaled and reversible like
> every other faculty. Phases A1–A3 + B + C shipped; this doc is the durable contract.
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
| `ui_reflex` | `on`, `do`, `app`, `remove?` | Install (or `remove: true` uninstall) an event→action rule the shell runs below inference (C, below) |

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

The behavioral half (deposits + procedure promotion) seeded with A3's soul "Your
stage" section ("Remember why"). The mechanical half shipped as **geometry
persistence** (ui-slint):

- **Per-AppKind shape** — `{x, y, w, h, maximized}` keyed by app slug in
  `$XDG_CONFIG_HOME/apexos-rs/geometry.json` (beside the `persona` file). Every
  open of a kind wears its last shape; first-ever opens cascade as before.
- **Noted at every geometry-changing chokepoint** — user move/resize/maximize,
  user close and agent `ui_close` (captured *before* row removal), and
  `ui_arrange` (an APEX tidy-up is the new remembered shape — it survives a
  restart the same as a hand-placed one). `geom_note` dedups, so idle pointer
  traffic never schedules a write; a 2s Slint Timer debounces the actual flush
  (move/resize fire per pointer-move), temp+rename so a crash can't tear the file.
- **Restore clamps to the live desktop area** (pure `restore_geom`, unit-tested):
  sizes floor to the frame minimums and cap to the area; positions pull fully
  on-stage — a shape remembered on the kiosk 1080p can't strand a window on a
  smaller display. When the area isn't believable yet, sizes still floor but no
  fictional edge is invented.
- **The boot seed waits for the area** (`seed_windows_when_area_live`): pre-run
  the window has no real size (winit/Wayland deliver it at first configure), so
  seed launches re-arm a 50ms timer until the area is live (bounded ~2s, then
  launch anyway). Caught live in the E2E clamp test — an off-screen remembered
  shape stranded the boot Chat window until the deferral. Don't move seed
  launches back before `run()`.
- **Shape, not session** — the open window SET is deliberately never restored: a
  fresh boot starts clean (Chat seed), windows re-open on demand wearing their
  last shape. Losing the file costs a cascade, nothing more.

## 6. Reflexes (Phase C) — below-inference adaptation

`ui_reflex {on, do, app}` (+ `remove: true`): the agent installs event→action rules
the UI executes directly off its own event stream — zero tokens, zero latency. Also
the answer to session-scoping: the conversation stream is session-scoped, so a
root-session 3am event can't reach a UI socket following another session — reflexes
fire UI-side off *global* events, which is exactly the ambient case.

- **Trigger vocabulary** (closed, mirror-locked both crates — tools
  `UI_REFLEX_TRIGGERS` ↔ ui-slint `REFLEX_TRIGGERS`; every entry is a GLOBAL event
  type): `sensor_alert · wake_triggered · mesh_message · mesh_node_status ·
  goal_state_changed · council_started · evolution_proposed · error`. Actions:
  `open | focus | close` (`open` is latch-aware via the same `agent_open_window`
  path; `close` carries agent-close semantics — no latch — and respects the drag
  guard). **`sensor_alert` is the persistence-filtered global `Event::SensorAlert`**
  (agentd emits it beside the root-session alert prompt — once per sustained
  event, post-persistence post-cooldown), NOT the raw `sensor_reading` stream
  (fires every few seconds; threshold judgment stays in agentd's classifier).
  The canonical reflex — `sensor_alert → open sensor` — stages the Sensors
  window before the agent's own turn even starts; the UI also surfaces every
  alert as a warn toast independent of any reflex.
- **Rails**: one rule per `(on, app)` key — reinstalling updates the action and
  resets the ledger; at most `REFLEX_MAX` (8) rules; a fired rule cools down
  `REFLEX_COOLDOWN_SECS` (30s) so event bursts (goal steps, mesh chatter) can't
  strobe the shell — the cooldown is consumed per *attempt*, so a latch-suppressed
  open doesn't retry on every event of a burst. **Installs spend a turn-mutation
  slot** (they're staging verbs in a turn, the A3 rail applies); **fires never do**
  (they're ambient, like the reader auto-reveal).
- **One fire chokepoint**: `dispatch_event` checks the trigger set at the top,
  before any dispatch arm (string-handled and typed events alike, so an arm's early
  `return` can't skip it) → `reflex_fire` on the Slint thread.
- **Visible + attributed**: the table + per-rule `fires` ledger ride `/state`
  (`reflexes`) so the agent sees what's installed and what's earning its keep; every
  fire toasts ("⚡ reflex opened Mesh (on mesh_message)"). `fires` counts actual
  applies, not attempts.
- **Persisted UI-locally**: `reflexes.json` beside the persona/geometry files
  (immediate save — installs and fires are human-scale rare). Survives restarts,
  ledger included; the runtime cooldown stamp does not persist.
- **Human-wins recovery**: a reflex-opened window is agent-marked, so a user close
  latches that app — the reflex then stays silent for the session (the overrule
  stands, mechanically). Removing the rule for good is the agent's job (`remove:
  true`), prompted by the latch showing up in `ui_query`.
- **Deposit discipline**: installs deserve a `ui-adaptation` memory (why this
  event→this window) — soul-seeded ("Reflexes for the recurring", config/soul.md);
  live nodes adopt via their own `propose_evolution`.

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
| B | Loop-6 memory: deposit discipline + procedure promotion (seeded in the soul's "Your stage", A3), geometry persistence (per-kind shape file, clamp-on-restore, boot-seed area wait — §5) | **shipped** (E2E-verified: restore, clamp, arrange→write, restart continuity) |
| C | `ui_reflex` family: 7 global triggers × open/focus/close, (on,app)-keyed table of 8, 30s cooldown, fires ledger on `/state`, soul "Reflexes for the recurring" (§6) | **shipped** (E2E-verified: install→persist→fire→ledger, cooldown swallows bursts, restart-proof) |
| D | Colony field cycle (apex1 kiosk / apex-3 desktop), dream-consolidation check | next |
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
| Reflex vocab | tools.rs `UI_REFLEX_TRIGGERS`/`UI_REFLEX_ACTIONS`/`UI_REFLEX_MAX` ↔ ui-slint `REFLEX_TRIGGERS`/`REFLEX_ACTIONS`/`REFLEX_MAX` | literal-locked test EACH side (change both crates together, additively; a trigger must be a GLOBAL event type) |
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
