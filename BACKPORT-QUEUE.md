# Cerebro backport queue ← CerebroCortex-RS

> Queued 2026-08-08 from the standalone's Lucida arc (CC-RS PRs #22–#28).
> Same discipline as the standalone's retired BACKPORT-QUEUE.md: port an
> entry, delete the entry; delete the file when empty. ALWAYS diff the real
> files before porting — the repos drift (mirror-wave lockstep helps but is
> not a guarantee). Target: `cerebro/crates/`.
>
> Standing context: ApexOS-RS is already on axum 0.8, so CC-RS's brace-route
> 404 fix does NOT apply here (the 0.8-native router code originated in this
> tree). Everything below does.

## Landed 2026-08-10 — integrity wave (entries 1–3)

- **W-A data-integrity** (CC-RS #22) — R-04 `update_memory_noted`, R-05
  visibility SHARED default + `remember_with_visibility` (R-02 REM truncate
  was already present), R-06 purge cleans `memory_versions` +
  `vision_embeddings`. Evolution undo path now passes `visibility:"private"`
  explicitly (R-05 broke the old agent_id→Private derivation).
- **Ghost-FK repair** (CC-RS #27) — `repair_ghost_fk_memory_versions` on open.
- **Seed-cap spread fix** (CC-RS #24) — budget = growth beyond seeds;
  `full_seed_page_still_spreads` regression.

## Landed 2026-08-10 — traced recall (entry 4)

- **Traced recall** (CC-RS #24) — `spread_events` + `TraceEvent`;
  `recall_traced` + `RecallTrace` (plain `recall` is a thin wrapper, same
  reinforcement); `POST /recall/trace` in cerebro-api. Thought-lens data.

## 5. Optional, when the colony wants eyes: Lucida + U1b API hardening — CC-RS PRs #23/#25/#27/#28

The whole observatory rides cerebro-api: `ui-web/` embedded at `/`,
`/graph/export`, `/graph/layout` (PCA cache in a new `graph_layout` table,
ON DELETE CASCADE), `/events` SSE audit tail + `/audit/since/{id}`,
`/dream/reports`, `/meta`, plus API hardening worth taking even without the
UI: **API mutations audit** (the MCP-only audit left REST writes invisible
to self-history), **R-08** (trash lifecycle + bulk_delete via the
coordinator's graph-eviction wrappers — deleted nodes stop spreading
without a restart), REST remember/update accept `visibility`. Design
charter: CC-RS `docs/UI-DESIGN.md`. Adapt into this repo (not a pin): web
dash for laptop/desktop installs; native Cerebro app in the ApexOS-RS Slint
shell for kiosk (lift Lucida patterns, rewrite as needed). A colony node
running Lucida over its own brain is the demo that sells itself — but it's
dessert, not integrity.
