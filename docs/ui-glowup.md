# ApexOS-RS — Desktop Shell & Persona Glowup Masterplan

> The plan to take the Slint UI from tabbed-MVP to a **multi-demographic desktop OS experience**.
> Strategic goal: a *familiar* interface lowers onboarding friction for everyone — mom, dad, the
> novice tinkerer, the tech-wiz kid — not just infra nerds. Same agentd core, radically different face.
>
> Status: **planning** (locked 2026-06-11). Build sessions execute phases G0→G7 below.
> Load this doc when working on `ui-slint/` shell/persona/window-manager features.

---

## 1. Locked decisions

- **Layered architecture, desktop-default.** The app has two *shell modes* — **Desktop** (windowed, the
  default boot face) and **Focus** (single full-screen app). This inverts the OG ApexOS (which booted to
  CLI). Desktop-first suits the multi-demographic goal; the appliance/CLI face becomes an opt-in.
- **"CLI" = a Focus-mode launcher on the desktop**, distinct from the **Terminal** app. Two separate things:
  - **CLI / Focus-chat** → full-screen pure agent conversation (the "just talk to it" appliance face).
  - **Terminal** → a desktop window running the real `/bin/bash` PTY (backend already exists: `/terminal-ws`).
- **Persona = visual + behavior.** A persona bundles theme + window chrome + wallpaper + app-set +
  **behavior profile** + default shell mode. Behavior adaptation is **in** (see §6 for the how + difficulty).
- **Windows-dad ships as Win-98 first, Win-7 later** — two distinct sub-variants for two distinct crowds.
  Win-98 is the classic retro vibe and the better opening showcase.
- **Take it as far as Rust/Slint + the hardware tier allow.** Eye candy is tier-gated, never assumed.
- The **theme/token layer already exists** (`palette.slint`, 6 themes). The glowup adds the *structural*
  layer (shell, window manager, chrome) on top — it does not rebuild theming.

## 2. Vocabulary

| Term | Meaning |
|------|---------|
| **Shell mode** | `Desktop` (windowed) or `Focus` (one full-screen app). Runtime-switchable property. |
| **Persona** | Named bundle: theme + chrome + wallpaper + visible app-set + behavior profile + default mode. |
| **App** | A reusable content component, hosted either in a window frame (Desktop) or full-screen (Focus). |
| **Chrome** | Per-persona window decoration (title bar, caption buttons, borders). Swappable component. |
| **Behavior profile** | Persona-driven config that changes *how the agent and UI behave*, not just colors. |

## 3. Architecture (six layers, bottom-up)

```
┌── Persona ─────────────────────────────────────────────────────────┐
│  theme tokens · chrome variant · wallpaper · app-set · behavior     │  L3/L4/L6
├── Shell ───────────────────────────────────────────────────────────┤
│  Desktop (wallpaper + dock/taskbar + window manager) ⇄ Focus (1 app)│  L1
├── Window manager ──────────────────────────────────────────────────┤
│  WindowDesc model · AppWindowFrame · drag/resize/z/focus/min/close  │  L1
├── Apps ────────────────────────────────────────────────────────────┤
│  Chat · System · Sensors · Sessions · Settings · Terminal · Face …  │  L2
├── View router ─────────────────────────────────────────────────────┤
│  one active surface at a time, no overlap, no thrash  (fixes bug)   │  L0
└── Palette / tokens (EXISTS) ───────────────────────────────────────┘
```

### L0 — View router (also the layout-bug fix)
Today all 5 views are always instantiated and "hidden" with `max-height: 0px`. Slint **does not clip by
default**, so collapsed views keep painting from y=0 → sensor text lands on top of the dashboard, and 5 live
view-trees thrash on tab switch ("SYSTEM hangs"). Fix: a proper router — chat stays always-instantiated (its
`<=>` callback aliases require it), the rest become `if active: View {}` (conditional instantiation; inactive
views leave the tree entirely). This is both the bug fix **and** the foundation the shell switches on.

### L1 — Shell + window manager
- `shell-mode: ShellMode` (Desktop default). Desktop renders: wallpaper → window layer → dock/taskbar →
  launcher/start. Focus renders one full-screen app + a "return to desktop" affordance.
- **WindowDesc** (Rust-owned set, Slint-mirrored): `{ id, app_kind, title, x, y, w, h, z, minimized,
  maximized, focused }`. Rust owns *which windows exist* (launch/close/focus, persisted layout); Slint owns
  live drag geometry for smoothness (round-tripping every pointer move would lag — notify Rust on drop).
- **AppWindowFrame**: chrome (persona variant) + content slot switching on `app_kind` to the app component.
  Draggable via title-bar TouchArea; resize grips; click-to-focus bumps z; min/maximize/close buttons.
- **Dock/taskbar + launcher**: `for w in windows` running-app buttons; a start/launcher `PopupWindow` lists
  installed apps. Chrome and placement vary by persona (mac dock bottom-center, win98 start bottom-left).

### L2 — Apps (reuse first)
Existing views become apps with **zero rewrite** — they already take props and emit callbacks:

| App | Source | Notes |
|-----|--------|-------|
| Chat | `chat_view` + `input_bar` | also the Focus-mode default |
| System | `dashboard` | CPU/RAM/disk |
| Sensors | `sensor_view` | IAQ + thermal |
| Sessions | `session_view` | history replay |
| Settings | `settings_view` | + new Appearance/Persona pane |
| Power | existing modal | stays a modal, not a window |
| **Terminal** | `/terminal-ws` PTY (backend done) | **new frontend**: text-grid + ANSI render in Slint is real work → later phase |
| **Face** | port `apex-face` render_face (in cerebro history) | ambient idle widget / screensaver |
| **Cerebro** | external browser link (no webview in Slint) | opens `http://host:8765/?token=…` |
| **About/Clock** | trivial | persona flavor pieces |

### L3 — Persona system (visual)
Theme already switches via `Palette.set_theme`. Add: **chrome variants** (one title-bar component per persona
family), **wallpaper** per persona, a **persona picker** in Settings, and a persona→(theme, chrome, wallpaper,
default-mode, app-set) resolver in Rust.

### L4 — Persona behavior  (see §6)
### L6 — Per-persona fidelity polish  (Win-98 showcase, then the rest)

## 4. Persona matrix

| Persona | Theme | Chrome | Default mode | Wallpaper vibe | Behavior profile |
|---------|-------|--------|--------------|----------------|------------------|
| **Apex** (default/nerd) | `ApexOS` | minimal neon | Focus or Desktop | dark grid | terse, technical, all surfaces |
| **mom** | `MacOS` | mac traffic-lights, rounded | Desktop | soft gradient | warm, plain-language, hide tool internals, voice-friendly |
| **ubuntu-dad** | `Gnome` | Adwaita headerbar | Desktop | Adwaita default | balanced, moderate detail |
| **windows-dad** | `Windows` (98 → 7) | Win-98 beveled / Win-7 Aero | Desktop | Bliss-style / teal | friendly, guided, classic affordances |
| **tech-wiz-kid** | `Jarvis` | HUD frame, scanlines | Focus (HUD) | starfield/HUD | telemetry-rich, fast, voice-forward, shows reasoning |
| **Aurum** (cerebro) | `Aurum` | gold minimal | Desktop | alchemy dark | reserved for the memory dashboard skin |

**First-boot persona wizard:** the install/first-run shows a "Who's this for?" tile picker (mom / dad /
nerd / kid). One screen, sets persona + default mode + wallpaper. This *is* the onboarding hook — the moment
a non-tech user feels at home. High ROI, build in G4.

## 5. State model (Rust ⇄ Slint)

New global/props (names provisional):
- `Palette.theme` (exists) · `shell-mode: ShellMode` · `persona: Persona` · `chrome: ChromeKind`
- `[WindowDesc] windows` (VecModel) · `wallpaper: image` · behavior flags (see §6)
- Callbacks: `launch-app(kind)`, `close-window(id)`, `focus-window(id)`, `move-window(id,x,y)`,
  `resize-window(id,w,h)`, `set-shell-mode(mode)`, `set-persona(p)`.
Persona resolution lives in Rust (`fn apply_persona(p) -> sets theme+chrome+wallpaper+app-set+mode+behavior`),
testable without a display.

## 6. Persona behavior — how, and how hard

Three tiers of increasing effort; **v1 ships tiers 1+2, defers tier 3.**

1. **UI surface gating** *(easy — pure Slint/Rust)*: persona flips config booleans/enums — which apps appear
   in the dock, whether tool-call internals are shown, label verbosity, base font size, voice-default-on,
   "simple mode" groupings. No agentd change. This alone makes mom's UI feel unlike the kid's.
2. **Agent style preamble** *(moderate — one clean agentd seam)*: persona contributes a system-prompt
   fragment / response-style hint ("warm, plain language, avoid jargon, short answers" for mom; "concise,
   technical, surface telemetry and reasoning" for kid). Rides the existing soul/system mechanism — either a
   per-session `persona_style` prepended to the system prompt, or a new lightweight WS field. Touches the
   RS workspace's own agentd copy only; small, additive, reversible.
3. **Deep behavioral adaptation** *(hard — defer)*: genuinely different interaction models, tool-exposure
   policy per persona, adaptive verbosity from usage. Tiers 1+2 already deliver ~90% of the *felt*
   difference, so this is post-showcase.

**Answer to "easy to implement?":** yes for the parts that matter. Surface-gating is trivial; the style
preamble is a small, well-scoped agentd addition. Deep adaptation is the only hard part and it's optional.

## 7. Tier-awareness (CLAUDE.md "Nano-first" rule still governs)

| Tier | Shell default | Effects | Windows |
|------|---------------|---------|---------|
| Nano (femtovg) | Focus (Desktop optional, flat) | none (no glow/scanline/blur) | 1–2, no live drag shadows |
| Micro/Standard | Desktop | moderate | full WM |
| Pro (winit/GPU) | Desktop | full glow/scanline/animations | full WM + eye candy |

Persona default mode is **tier-clamped**: a Nano node requesting the Jarvis HUD desktop falls back to flat
Focus + static accents. Heavy assets (wallpapers, fonts, sounds) are tier-gated to protect the ~10 MB ethos.

## 8. Build roadmap (phases + gates)

| Phase | Deliverable | Gate |
|-------|-------------|------|
| **G0** ✓ | View router fix | tab switch shows exactly one view, no overlap, no jank; chat aliases intact — **DONE** (commit e32ae46, verified live: stacking fixed) |
| **G1** ✓ | Shell scaffold | `shell-mode` toggles Desktop⇄Focus; Desktop shows wallpaper + dock + Chat in one window — **DONE** (verified live on desktop: toggle + smooth framed⇄fullscreen transition + dock). Notes: app content lives in one `surface` whose geometry switches on `shell-mode` (chat instantiated once → aliases intact); dock replaces the tab strip in Desktop, tab strip stays in Focus; Focus is still the full legacy tabbed face (narrowing to single-app deferred to G3+); default tier-clamped (femtovg→Focus). New: `components/dock.slint`, `ShellMode` enum. |
| **G2** ⚙ | Window manager core | launch/close/focus/min/maximize + smooth drag + resize + z-order; ≥3 windows — **BUILT** (clean compile, runs without panic; interactive drag/resize/focus pending live confirm on André's screen — agent can't drive the GUI). Design: "everything is a window" (André's call); `WindowDesc` VecModel is Rust-owned, model order = z-order; `AppWindowFrame` hosts any app by `kind` with frame-local drag/resize deltas committed to Rust on release; **raise-on-release** (not press) to dodge the repeater-reorder-mid-drag hazard (`for` keys by index). Chat aliases retired → `AgentBridge` global (approve/reject + scroll-tick), so ChatView is no longer single-instance. Dock launches/reveals one window per kind. Focus mode keeps the legacy tabbed surface. |
| **G3** ✓ | App catalog | all existing views run as windows; launcher/start menu + taskbar functional — **CORE DONE** (verified live): G3a unified taskbar (Start · running windows · tray) retires the dock + fixes the minimized-window trap (restore/focus/minimize-toggle via `task-activate`); G3b start menu (in-canvas catalog panel, backdrop-dismiss); G3c notification center (toasts persist to `Notifications.log` newest-first + tray unread badge). New: `components/{taskbar,start_menu,notif_center}.slint`. **Deferred to G3d/G3+:** Council app + read-only Terminal (need backend data paths). |
| **G3d** ✓ | Council + Terminal apps | **DONE** — closes out G3. **Terminal** (verified live): read-only-ish line-mode PTY pane over `/terminal-ws` — Rust owns the WS (lazy-spawned on first launch, reconnects w/ backoff), streams ANSI-stripped output (tabs→8-col stops) into `terminal-text` (60 KB ring buffer) + bumps `terminal-scroll-tick`; `TextInput` submits a line → `terminal-send` → stdin. No cursor grid (curses apps garble — real VTE deferred). **Council** (built; live streaming pending a real council run / API key): `components/council_view.slint` shows topic + per-agent streaming cards (accent dot from `CouncilAgentDef.color`) + convergence bar + synthesis, driven by the `council_*` WS event set; per-round transcript clears on `council_round_start`. Taskbar gains a `⚖` indicator that appears while a council is active or sub-agents are running, badge = running sub-agent count (`sub_agent_started` − child `turn_complete`). New: `AppKind::{terminal,council}` (ord 5/6), `CouncilAgent` struct, start-menu entries. |
| **G4** ✓ | Persona system + first-boot wizard | picker switches theme+chrome+wallpaper+mode live; wizard sets it on first run — **DONE** (verified live: first-boot wizard → picked "Simple" → full macOS vibes — light theme, traffic-light chrome left + centred title, soft wallpaper). Design: `Persona` + `ChromeKind` enums + `PersonaDef` catalogue struct; new `Personas` global (L3 above Palette) derives chrome/wallpaper/display from `current`; Rust `apply_persona(p)` sets Personas.current + Palette.theme + tier-clamped shell-mode + persists to `~/.config/apexos-rs/persona`. New `components/persona_picker.slint` (shared tile → compact in-canvas picker + full-screen wizard); chrome variation in `app_window_frame.slint` (mac traffic-lights vs right-button family); per-persona wallpaper gradients. Title-bar persona chip opens the live picker. Full Win-98 chrome fidelity deferred to G6. |
| **G5** ◑ | Persona behavior | **TIER-1 DONE** (verified live): surface-gating — Simple/mom hides the deep-tech apps (Sensors/Terminal/Council) from Start, collapses tool-call internals in chat (status + approve/reject kept; args/output un-expandable), reads at 1.15× chat font. New Settings → **PERSONA** pane (all six tiles, live switch). Behaviour flags derive in the `Personas` global (`show-tech-apps` / `hide-tool-internals` / `font-scale`); the catalogue (`defs`) + switch (`pick`) also moved onto the global so the title-bar picker, wizard, and Settings pane share one source with zero prop-threading. **TIER-2 (agent style preamble) DEFERRED** to a dedicated agentd session — it touches the diverged RS agentd copy and is unverifiable without `ANTHROPIC_API_KEY` on the Pi. |
| **G6** ✓ | **Win-98 showcase** | full-fidelity chrome — **DONE** (verified live on André's laptop: "THE win-98 look on the dot"). New `components/win98.slint` = self-contained `Win98` token global (face/hilite/light/shadow/dark ramp + navy title + teal desktop) + `BevelEdges` (2-ring layered-1px-rect 3D bevel overlay, raised/sunken) + `Win98Button` (raised→sunken-on-press). Gated on `Personas.chrome == ChromeKind.win`. Retuned **Theme.Windows** palette from Aero→classic 98 grey/white/navy, radius 0 (only windows-dad uses it). Window chrome: navy-gradient caption + white bold title + 3 beveled caption buttons + raised frame bevel. Taskbar: flush grey beveled bar (34px, no float/round) + iconic Start button (4-pane flag) + sunken task tabs + sunken tray. Start menu: grey beveled panel + left navy banner w/ rotated "ApexOS" wordmark + navy-on-white hover rows. Flat teal desktop. **Deferred (resource items, not blockers):** MS-Sans libre font embed (§9), tray clock (needs a Rust time source — see G6.1 timestamps), optional startup chime. **Zero Rust changes** (pure Slint). |
| **G6.1** ◑ | Temporal markers | **UI-SIDE DONE** (tray clock verified live; chat divider built, live-pending agentd). André request: temporal grounding for traceability + to stop the agent treating stale cerebro recalls as current. New `Clock` global (in types.slint, exported) fed by a 1s `slint::Timer` in main.rs (chrono `Local`). Tray clock `HH:MM` in the Win-98 sunken tray + the modern bar (all personas). Chat **time-dividers**: `maybe_push_time_divider()` drops a centered `role="time"` MessageItem (label in `text`, ~3-min interval guard, reset on `clear_messages`) on the send + voice paths; `chat_view` renders it as a centered chip. chrono added to ui-slint (workspace dep). **DEFERRED to the agentd session** (same bucket as G5 tier-2 — diverged agentd copy + needs `ANTHROPIC_API_KEY`): the *model-facing* half — agentd injecting the wall-clock time into the actual context so the LLM perceives time. UI seam is ready; the divider label already exists to mirror. |
| **G7** | Polish + tier pass | mac dock, jarvis boot anim, Win-7 variant, Nano perf pass; effects tier-gated |

Discipline unchanged: gate passes → commit + push; docs travel with code; `session_save` per session.

## 9. Resources to gather (before/at G4–G6)

- **Fonts (libre only):** Win-98 → an MS-Sans-Serif-like bitmap/`Pixelated` libre face; mac → Inter/SF-alt;
  GNOME → Cantarell; Jarvis → Orbitron / Share Tech Mono. Check licenses; embed only tier-appropriate ones.
- **Wallpapers:** one per persona, license-clean (CC0 / self-made). Bliss-style green hill for Win, soft
  gradient for mac, starfield/HUD for Jarvis, dark grid for Apex.
- **Icons:** a small libre app-icon set or self-drawn; consistent within each persona.
- **Sounds (optional, G6):** libre startup chimes — recreate, don't lift copyrighted originals.
- **Reference screenshots:** Win-98/7, classic macOS, GNOME, sci-fi HUDs — for chrome fidelity, not assets.
- **Decide:** asset-embedding strategy vs on-disk under `/usr/share/apexos/personas/…` (binary-size budget).

## 10. Slint capability notes (the honest map)

- ✅ In-canvas overlapping draggable windows, dock, start menu (`PopupWindow`), per-theme chrome, wallpapers,
  glow/scanline animation (Jarvis already proves it), bevels (layered light/dark rectangles for Win-98).
- ⚠️ **No free window manager** — drag/resize/z/focus are hand-rolled (~few hundred lines, reusable).
- ⚠️ **No webview/iframe** — cerebro dashboard stays an external-browser link; "apps" are native components.
- ⚠️ **Terminal app** = rendering a text grid + ANSI in Slint (no xterm.js); real work → later phase.
- ⚠️ **linuxkms = one fullscreen surface** — windows are in-canvas (correct for kiosk anyway).
- ⚠️ **Perf on Nano/femtovg** — many animated windows are heavy; tier-gate effects.

## 11. Open decisions to lock at build time

1. WM geometry source of truth: Rust-owned + Slint-live-drag (recommended) vs Slint-owned.
2. Persona style preamble: new WS field vs per-session soul augmentation.
3. Asset strategy: embedded vs `/usr/share` install (binary-size budget).
4. Default persona on a fresh install: Apex/Desktop, or force the first-boot wizard before first use.
5. Win-98 sound: ship the chime or stay silent by default (kiosk-friendliness).

---

## 12. Feature & feedback backlog (folded in from the mk1 deferred-scan)

Five items surfaced during the mk1 build that weren't captured anywhere. Classified and slotted here —
**not a separate plan.** Two are reclassified from the original scan; read the notes.

| Item | Class | Effort (corrected) | Lands |
|------|-------|--------------------|-------|
| **Feedback subsystem** (toasts + notifications, unified) | **Foundational** | core = small; center = medium | toast core **DONE** (e32ae46, verified live); notification center still at G3 |
| **Thermal pixel grid** (MLX90640 32×24 heatmap) | Feature / delight | **medium** (not 2h — see below) | focused feature, early for demo value |
| **Council / sub-agent visibility** | Surface | badge = small; app = medium | badge anytime; **Council app** in G3 catalog |
| **PTY terminal** | App | read-only = small; interactive = large | read-only intermediate early; full in G3+ |
| (Notifications folded into Feedback subsystem above) | — | — | — |

### Feedback subsystem (elevated — the glowup depends on it)
Today settings saves, voice failures, and power actions are **fire-and-forget with no visible result** — a
real UX defect, not just missing polish. Build **one** subsystem:
- **Toast primitive** *(quick win, do first)*: a `Notifications` global + a transient timed overlay
  component (`info | success | warn | error`, auto-dismiss). Reused everywhere — settings/voice/power
  feedback now; persona-switch confirms, window events, and background events later.
- **Notification center / tray** *(desktop expression, G3)*: persisted history of background events
  (dream-cycle complete, sensor threshold crossed, plugin crash, council updates) surfaced via a taskbar
  tray + a center panel. Same data model as toasts; transient ones can also persist to the center.

### Thermal pixel grid — corrected scope (IMPORTANT)
**Not half-done — the pixel data never leaves the sensor bridge.** `apex-sensor-bridge` reads SensorHead
`/api/thermal/data` but forwards only `min_c/max_c/avg_c` (`main.rs:162-164`); agentd's `thermal_frame`
Event **deliberately** carries no array (`agentd/.../core/src/types.rs:139` — "no raw array — keep events
small"). The 32×24 grid therefore requires a data path, not just a UI widget.
**Recommended design — on-demand frame fetch (don't bloat the broadcast):**
1. Confirm SensorHead `/api/thermal/data` already returns the raw 768-float array (the OG ApexOS heatmap
   used it — likely yes).
2. agentd exposes `GET /api/thermal/frame` returning the latest raw array (bridge pushes latest frame to
   agentd, or agentd proxies SensorHead). Mirrors the existing `/api/snapshot` on-demand pattern.
3. The Sensors surface polls it (~2–4 Hz) **only while visible**; UI maps array → `Image`
   (`SharedPixelBuffer` / `Image::from_rgba8`) with a thermal colormap. Live, interpolatable, cheap when
   nobody's looking. Keeps the WS events small (preserves the original design intent).
Alternative: render the colormap server-side to a small PNG and serve it (UI just shows `Image`) — simpler
UI, less "live." Prefer the raw-array path for smoothness.
**Priority: LOW / niche — bottom of the list.** The full grid is eye-candy. The existing summary data
(min/mean/max) already drives the desired outcome: a temp-reactive **"breathing" wallpaper** that shifts as
the room temperature fluctuates. That effect is the real want; the 32×24 grid is optional and only worth the
cross-crate plumbing if a build-craze itch demands it. Do not block any phase on it.

### Council badge + read-only terminal (cheap opportunistic adds)
- **Council badge**: a title-bar (Focus) / taskbar (Desktop) "N sub-agents running" indicator from the
  existing council subsystem — small, do whenever. Full **Council app** (session tree, butt-in) → G3.
- **Read-only terminal**: an output-only pane over `/terminal-ws` (no input/VTE) is a cheap intermediate
  toward the full interactive Terminal app — useful for log-watching before the hard ANSI-grid version.

### Revised near-term ordering (post Pi e2e test)
1. **Feedback/toast core** — top pick: fixes a real silent-failure UX defect *and* is glowup plumbing.
2. **G0 router fix** → glowup proper (G1…). Council badge + read-only terminal slot in opportunistically.
3. **Thermal grid** — niche eye-candy, bottom of the list (see priority note above); never a blocker.
