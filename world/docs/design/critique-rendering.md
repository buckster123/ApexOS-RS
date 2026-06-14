# Critique — Rendering Architecture (adversarial feasibility review)

> Adversarial feasibility review of the `apexos-world` rendering stack, covering
> `03-rendering-architecture.md`, `04-agent-embodiment-and-vision.md`,
> `05-station-and-app-system.md`, and the seed skill at
> `docs/rust-ai-3d-hud-skill/`. Scope: is the Slint + Bevy shared-wgpu integration
> actually viable in mid-2026, or a known-pain trap? Focus on version reality,
> the "live Slint UI on a 3D surface" claim, the wgpu async vision readback, perf
> for many avatars, and the VR path.
>
> Verdict in one line: **the architecture is sound, but two load-bearing
> assumptions are factually wrong as written and will fail Spike 0 / Spike 2 on
> contact. The design is salvageable — but only via its own fallbacks, which
> should be promoted from "insurance" to "primary plan" until proven otherwise.**

The doc deserves real credit: it *names* the version clash as the chief risk (§10),
ships a four-spike de-risking ladder, and pre-writes a Pattern A-lite fallback. That
is exactly the right instinct. The problem is the doc then writes the happy path
(shared Bevy device, live Slint-on-quad) as the baseline plan and the fallbacks as
insurance — when the verified facts below invert that probability.

---

## A. The two factual errors that sink the happy path

### A1. `unstable-wgpu-29` does not exist. Slint is on wgpu **28**.

The doc pins, in `main.rs`, `Cargo.toml`, and the crate skeleton:

```rust
.require_wgpu_29(slint::wgpu::WGPUConfiguration::default())
...
slint::GraphicsAPI::WGPU29 { device, queue, .. }
```
```toml
slint = { ... features = ["renderer-wgpu", "unstable-wgpu-29"] }
```

Verified against Slint's published docs: the feature is **`unstable-wgpu-28`**, the
GraphicsAPI variant is **`WGPU28`**, and the module is `slint::wgpu_28`. There is no
`require_wgpu_29` / `WGPU29` / `unstable-wgpu-29` in shipping Slint. The seed skill's
own `Cargo.toml` template even says `unstable-wgpu-29` while SKILL.md prose elsewhere
references wgpu-28 — the skill is internally inconsistent and the design copied the
wrong number. **Every wgpu code snippet in `03` as written will not compile.**

This is mechanical to fix (s/29/28/) *but* it changes the version-reconciliation
math in A2 — it does not make it easier.

### A2. No Bevy release shares Slint's wgpu version. Spike 0 fails today.

The entire Pattern A "single device, zero-copy" claim (§1.4, §2) requires Slint and
Bevy to resolve to **one** `wgpu` semver in the dependency tree. Verified mid-2026
reality:

| Crate | wgpu major |
|-------|-----------|
| Slint 1.16 (`unstable-wgpu-28`) | **28** |
| Bevy 0.16 | 24 |
| Bevy 0.17 | 25 |
| Bevy 0.18 | 26 |
| Bevy 0.19 (rc, June 2026) | ≤ 27 |

The newest Bevy is **at least one, realistically two** wgpu majors behind Slint.
`cargo tree -d -i wgpu` on the doc's own dependency set produces **two `wgpu`
versions**, which is the exact failure Spike 0 is defined to catch. wgpu makes
breaking changes every major; 26↔28 (or 27↔28) is not patchable via
`[patch.crates-io]` without API surgery the doc explicitly forbids assuming
("verify, don't assume"). The `Device`/`Queue` types are nominally distinct across
majors — Slint's `wgpu_28::Device` is *not* the type Bevy's `RenderCreation::Manual`
expects, so the shared-device handoff in §2.2 **cannot typecheck**, let alone link.

**Consequence:** Spike 0's success criterion ("exactly one wgpu version") is, as of
this review, **unsatisfiable** with any released Bevy. Option (a) "pick the Bevy
release whose wgpu matches Slint" has no valid answer today. Option (b) `[patch]` is
infeasible across a major. Option (c) — Pattern A-lite — is therefore not a fallback;
**it is the only currently-buildable path.**

This is not a reason to abandon the project. It is a reason to **reorder the plan**:
build Pattern A-lite first, treat Bevy as a future migration gated on Slint and Bevy
converging on a shared wgpu (which will happen eventually — both track wgpu — but you
cannot schedule it).

---

## B. "Live Slint UI rendered to a 3D texture" (Mode I) is not a documented Slint capability

This is the project's *headline mechanic* (§4 Mode I; the "screen on the wall is
alive before you walk up" felt-experience the doc explicitly says it will "pay K live
textures to get"). It requires rendering a **Slint component** to an offscreen texture
that Bevy then samples as a material.

Verified: Slint documents the **reverse** direction well — importing an
externally-produced `wgpu::Texture` into a Slint `Image` via `Image::try_from`
(Bevy→Slint, which is the *world_texture* full-window path, and that part is real and
fine). But "render a Slint component/window **to** a texture/buffer offscreen" is
tracked as an **open feature request (slint-ui/slint issue #704)**, not a stable
shipping API. The doc's §12 open question already half-admits this ("must be validated
as a *stable* 1.16 API in Spike 2; if only full-window render-to-texture is stable,
Mode I may need a per-station hidden window").

The "per-station hidden Slint window/surface" fallback is worse than the doc implies:

- Multiple Slint windows under a single shared-wgpu backend, each rendered to its own
  surface, then sampled by Bevy — this is an **unproven multi-surface configuration**,
  not a documented pattern. Slint's backend is built around one event-loop-owning
  window.
- Even if it works, K simultaneous live Slint windows is K full Slint render trees
  ticking every frame — the cost the doc budgets for, but now with per-window overhead
  (separate render setup, separate notifier) on top.

**Honest assessment:** Mode I (live in-world Slint surface) is the single most likely
feature to be **cut or heavily degraded**. The good news: the design already has the
right escape — Mode II (fullscreen takeover) is a pure Slint z-order change and needs
*none* of this; it only needs the full-window `Image` path, which is real. **Mode II
is viable; Mode I is speculative.** The project should be willing to ship "dead/static
quads that come alive on activation" (which §4.3 explicitly argues against) if Spike 2
shows Mode I is not stably available.

---

## C. The vision-loop async readback (§04 §3.3) — the *soundest* part

This is the strongest section of the whole stack and largely correct:

- `copy_texture_to_buffer` → `map_async(Read)` → `device.poll(Wait)` → un-pad rows is
  the textbook wgpu readback. The doc correctly flags `COPY_BYTES_PER_ROW_ALIGNMENT =
  256` row padding as the #1 footgun — that is the real footgun and it's named.
- Render-on-demand (camera `is_active=false`, flip for one frame) is the right
  cost model; steady-state cost is genuinely zero.
- Encode on `spawn_blocking`, base64 envelope, MCP image content-block shape — all
  plausible and correctly hedged with the "provider may only forward text → fallback to
  URL/path" assumption.

Residual risks (medium/low):

- **C1 — readback still depends on a wgpu device.** In the Pattern A-lite world (no
  Bevy), the readback uses Slint's `wgpu_28` device directly, which is *fine* and
  actually simpler. In the Bevy world it depends on A2 being solved. So the vision loop
  inherits A2's risk only on the Bevy path; on A-lite it's clean. This is another point
  in A-lite's favor.
- **C2 — Bevy's `gpu_readback::Readback` API churn.** The doc leans on
  `bevy_render::gpu_readback` / `bevy_image_export`. These are young, churning across
  0.16→0.19. If you do go Bevy, hand-rolling the readback against the raw wgpu device is
  *more* stable than depending on Bevy's observer-based readback, contrary to the doc's
  "don't hand-roll if avoidable." Reverse that advice.
- **C3 — `device.poll(Wait)` on which thread.** Must not run on the Slint main thread.
  The doc says encode runs on `spawn_blocking` (good) but the *poll/map* step is on the
  renderer side; ensure the blocking poll is off the event loop, or use the async
  `map_async` callback wired to the tokio side. The doc is slightly vague here; pin it.
- **C4 — `map_async` one-frame latency + offscreen render not stealing the swapchain.**
  Correctly anticipated. Low risk.

Net: the vision loop is the part most likely to *just work*. The doc's 10–30 ms budget
on a desktop GPU is realistic.

---

## D. Perf for many avatars (§03 §8) — plausible targets, one quiet contradiction

- 50 avatars via instancing + frustum/distance culling + impostors for far band: this
  is standard and achievable on a Pro GPU. The targets (60 fps Pro, ≥30 Standard) are
  reasonable **if** the engine is Bevy. On Pattern A-lite (hand-rolled wgpu), you must
  build instancing/culling yourself — that is real work the A-lite section
  under-prices ("strictly more work for the 3D side" is an understatement; it's
  re-implementing the parts of Bevy you actually wanted).
- **D1 — the live-station-texture cost is the real ceiling, and it collides with B.**
  §8 calls live station textures "the scarce resource" and caps K. But per B, each live
  station texture may require a *separate Slint render pass / hidden window*. K=4–8
  Slint trees ticking at 16 ms is plausibly the dominant frame cost — and if Mode I
  needs hidden windows, the per-K overhead is higher than a single offscreen pass. The
  perf budget in §8 silently assumes Mode I is a cheap offscreen component render
  (issue #704), which is exactly the unproven capability. **If Mode I is cut, the perf
  picture actually improves** (no live Slint passes), which is another argument that
  cutting Mode I is low-regret.
- **D2 — adaptive frame timer (16→33 ms).** Fine, but driving `app.update()` from a
  Slint `Timer` couples Bevy's logic tick to Slint's frame cadence. Bevy's
  fixed-timestep decoupling helps, but a stalled Slint frame (e.g. a heavy fullscreen
  takeover layout) stalls the world tick. Acceptable for a prototype; note it.

---

## E. VR path (§05 §8) — realistically out of scope, correctly deferred (but under-caveated)

The VR mention is one bullet in `05` §8 and a line in the skill. The skill claims
`bevy_mod_openxr ~0.5.x`. Reality check raises hard doubts:

- **E1 — VR requires Bevy to own the OpenXR swapchain and the frame loop.** That is
  **Pattern B** (Bevy-primary), the exact inversion of the chosen Pattern A. OpenXR
  drives its own frame timing (`xrWaitFrame`/`xrBeginFrame`); you cannot cleanly run
  the XR session as a guest stepped from a Slint `Timer`. So VR is not a "mount + input
  route differ, station model unchanged" tweak (as `05` §8 claims) — **it is a
  different top-level architecture.** The doc materially understates this.
- **E2 — VR + the shared-wgpu version clash is compounded.** `bevy_mod_openxr` pins its
  own wgpu/Bevy versions; stacking that on top of the Slint-wgpu mismatch (A2) is a
  three-way version-pin problem. Effectively infeasible to share one device across
  Slint + Bevy + OpenXR in one binary today.
- **Verdict:** VR is fine to list as a north star but should be explicitly marked
  **"requires Pattern B, separate binary, no shared-Slint-device — re-architecture, not
  a toggle."** Do not let the roadmap imply VR is a near-term flag on the desktop build.

---

## Risk register

| ID | Risk | Likelihood | Impact | Where |
|----|------|-----------|--------|-------|
| **R1** | `unstable-wgpu-29`/`WGPU29`/`require_wgpu_29` don't exist — code won't compile | **Certain** | Low (mechanical fix to `-28`) | 03 §2 |
| **R2** | No released Bevy shares Slint's wgpu (28 vs ≤27) → Spike 0 unsatisfiable → shared device can't typecheck/link | **High** | **Critical** (kills the Bevy happy path) | 03 §1,§2,§10 |
| **R3** | "Live Slint component → offscreen texture" (Mode I) is an open Slint feature request (#704), not a stable API | **High** | High (kills the headline in-world-screen mechanic) | 03 §4 Mode I, §12 |
| **R4** | Per-station "hidden Slint window" fallback for Mode I is an unproven multi-surface config | Medium | High | 03 §12 |
| **R5** | VR claimed as a near-term toggle; actually needs Pattern B + a 3-way wgpu version reconciliation | Medium | Medium (scope creep / false roadmap promise) | 05 §8, skill |
| **R6** | Bevy `gpu_readback` API churn 0.16→0.19; doc advises *against* hand-rolling (backwards) | Medium | Medium | 04 §3.3 |
| **R7** | Pattern A-lite under-prices re-implementing instancing/culling/asset pipeline (= rebuilding Bevy) | Medium | Medium (schedule) | 03 §10 |
| **R8** | Perf budget (§8) silently assumes Mode I is a cheap offscreen pass; collides with R3/R4 | Medium | Medium | 03 §8 |
| **R9** | `device.poll(Wait)` thread placement under-specified; risk of stalling event loop | Low | Medium | 04 §3.3 |
| **R10** | Adaptive timer couples Bevy tick to Slint frame; a heavy Slint layout stalls the world | Low | Low | 03 §8 |
| **R11** | Skill itself is internally inconsistent on wgpu version (template says 29, prose says 28) — copying it propagates errors | Certain | Low | skill |

---

## De-risking recommendations (concrete)

### 1. Fix the version facts in the doc *now* (R1, R11)
- `require_wgpu_29` → `require_wgpu_28`; `GraphicsAPI::WGPU29` → `WGPU28`;
  `unstable-wgpu-29` → `unstable-wgpu-28` everywhere in `03` + the crate skeleton.
- Add a one-line warning that the seed skill's `Cargo.toml` template is wrong (says 29)
  so nobody re-copies it.

### 2. Promote Pattern A-lite from "insurance" to **primary path** (R2, R7)
- Rewrite §1's decision to: **"Pattern A composition, Slint-only wgpu renderer for the
  3D side at prototype stage; Bevy is a deferred migration gated on Slint+Bevy sharing a
  wgpu major."** Keep the Bevy aspiration; stop scheduling it as baseline.
- Spike 0 stays — but reframe its success criterion: it now *confirms the clash exists*
  and *selects A-lite*, rather than being a gate that must turn green for the project to
  proceed. The project proceeds **on A-lite** regardless.
- Set explicit migration trigger: "Adopt Bevy when `cargo tree -d -i wgpu` over
  `slint 1.x (unstable-wgpu-N)` + `bevy 0.y` yields exactly one wgpu — recheck on every
  Slint/Bevy release." Make this a tracked open question, not a spike that blocks.

### 3. Spike order — reorder and add one (R2, R3)
Do these **before** any scene-graph / LOD / picking work:

- **Spike 0 (½ day, unchanged but reframed):** `cargo tree -d -i wgpu` on
  slint-1.16(`unstable-wgpu-28`) + candidate Bevy. Expected result: **two versions →
  choose A-lite.** Record the pins + the result in the doc.
- **Spike 1' (1 day) — A-lite triangle:** Slint window, full-window `Image` fed by a
  **hand-rolled wgpu renderer using Slint's own `wgpu_28` device** (no Bevy), one
  rotating triangle, driven by a Slint `Timer`. Gate: triangle spins at 60 fps, one
  wgpu in the tree (guaranteed — it's Slint's). This is buildable *today*; the original
  Spike 1 (Bevy shared device) is not.
- **Spike 2 — THE make-or-break spike, do it second, before committing to Mode I
  (R3, R4):** try to render a real reused `ui-slint` component to an offscreen texture
  and sample it on a 3D quad. Two sub-attempts:
  - 2a: any documented Slint component→texture path (likely none stable → expect
    failure, that's the finding).
  - 2b: hidden-window-per-station via the shared wgpu backend.
  Gate: a live Slint panel visible on a 3D quad **at acceptable cost**. **If both fail,
  cut Mode I and ship Mode-II-only** (activation makes the screen live; quads are static
  personae until activated). Decide this *before* building the station-texture pool
  (§4), the K-cap LOD logic (§8), and the promotion/demotion machinery — all of which
  exist only to serve Mode I.
- **Spike 3 (vision readback, can run in parallel, low risk):** offscreen render + wgpu
  readback + jpeg + base64 against Slint's `wgpu_28` device directly (no Bevy
  dependency). Hand-roll the readback (R6). Gate: a JPEG comes back in <30 ms.
- **Spike 4 (agentd attach, unchanged):** real WS client, session filter, one ChatPanel
  `VecModel` driven by `agent_text`, Mode II takeover on `E`.

### 4. Version pins to write into `world/crates/apexos-world/Cargo.toml`
```toml
# VERIFIED June 2026 — Slint exposes wgpu 28, NOT 29. Do not write unstable-wgpu-29.
slint = { version = "1.16", features = ["backend-winit", "renderer-wgpu", "unstable-wgpu-28"] }
# Bevy DELIBERATELY ABSENT at prototype: no released Bevy (≤0.19/wgpu≤27) shares
# Slint's wgpu 28. The 3D side uses Slint's own wgpu_28 device (Pattern A-lite).
# Re-evaluate adding `bevy` only when `cargo tree -d -i wgpu` yields a single version.
```

### 5. Re-caveat VR (R5)
- In `05` §8 and any roadmap, mark VR: **"Pattern B (Bevy-primary OpenXR), separate
  binary, cannot share Slint's device — a re-architecture, not a runtime toggle. Out of
  scope until the desktop Bevy path itself is viable (R2)."**

### 6. Pin the readback thread model (R9)
- State explicitly: offscreen render + `device.poll`/`map_async` resolution runs on the
  renderer/tokio side, encode on `spawn_blocking`, and the Slint event loop is never
  blocked on GPU completion. One sentence in §04 §3.3.

---

## What to keep (this design got a lot right)

- The **two-process split** (`world-app` renderer + `world-vision-mcp` stdio plugin,
  04 §0) is clean and correctly mirrors `cerebro-mcp`/`apexos-tools`. No agentd fork.
  Solid.
- **Deleting the skill's placeholder protocol** and hand-matching the real agentd
  `Event` enum, including the `turn_started`-doesn't-exist trap and the mandatory
  session-broadcast filter (03 §3, 05 §3.2) — exactly right, these are the real traps.
- **Mode II fullscreen takeover as pure z-order, shared VecModel, no re-subscribe**
  (03 §4 Mode II) — viable on the real Slint API and the right reuse story.
- **Tier gating / clean refusal over degrade-to-2D** (03 §9) — honest and correct;
  `apexos-rs-ui` already is the 2D fallback.
- **Vision readback** (04 §3.3) — the soundest GPU section; only thread-placement and
  "don't hand-roll" need fixing.
- **Generative-UI = data→templates, closed UiSpec** (05 §5) — correctly respects
  Slint's compiled-not-runtime-widgets limit.
- The doc **already wrote its own escape hatches** (Pattern A-lite, the §12 open
  questions). The fix is mostly to *believe its own caveats* and reorder the plan so
  the fallbacks lead.

---

## Bottom line

The composition architecture (Slint hosts; 3D renders to a full-window `Image`; Mode II
takeover; two-process plugin split; vision readback) is **viable and well-reasoned.**
But as written the doc bets the baseline on two things that are false in mid-2026:
(1) `unstable-wgpu-29` and a wgpu-version-matched Bevy, and (2) a stable
Slint-component-to-offscreen-texture API. Both fail at the first spike.

**Recommendation: do not start with shared-wgpu Bevy or Mode I.** Build Pattern A-lite
(Slint's own wgpu_28 device, hand-rolled 3D) + Mode II fullscreen surfaces + the vision
readback. Prove Mode I (live in-world Slint surface) in an early, explicitly
make-or-break spike and be ready to cut it. Treat Bevy and VR as future migrations
gated on external version convergence you cannot schedule. The prototype is very much
buildable on this reordered plan — just not on the one the doc currently leads with.

---

### Sources (version facts verified June 2026)
- Slint wgpu feature is `unstable-wgpu-28` / `WGPU28` / `slint::wgpu_28`:
  [Slint cargo features](https://docs.rs/slint/latest/slint/docs/cargo_features/index.html),
  [slint::wgpu_28](https://docs.slint.dev/latest/docs/rust/slint/wgpu_28/),
  [WGPU texture support PR #8278](https://github.com/slint-ui/slint/pull/8278)
- Slint component→offscreen-texture is an open request:
  [slint-ui/slint issue #704 — Render to texture/headless](https://github.com/slint-ui/slint/issues/704)
- Bevy wgpu versions / latest release:
  [Bevy 0.17 (wgpu 25)](https://bevy.org/news/bevy-0-17/),
  [0.17→0.18 migration (wgpu 26)](https://bevy.org/learn/migration-guides/0-17-to-0-18/),
  [bevy 0.19.0-rc.3 on docs.rs](https://docs.rs/crate/bevy/latest),
  [RenderCreation::manual](https://docs.rs/bevy/latest/bevy/render/settings/enum.RenderCreation.html)
