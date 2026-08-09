# Cerebro backport queue ← CerebroCortex-RS

> **Queue empty — 2026-08-10.** All Lucida-arc entries from CC-RS PRs #22–#28
> have landed. This file can be deleted on the next hygiene pass; kept as a
> tombstone until then. ALWAYS diff the real files before any future port —
> the repos drift.

## Landed

| Entry | PR / note |
|-------|-----------|
| 1–3 Integrity (W-A, ghost-FK, seed-cap) | #349 |
| 4 Traced recall | #350 |
| 5 Lucida + API hardening + shell tile | this PR |

### Entry 5 detail

- `cerebro/ui-web/` — Lucida observatory (vanilla Atlas/Thought/Dream/Health/Live)
- `cerebro-api` serves it at `/` + `/graph/export`, `/graph/layout`, `/events`
  SSE, `/audit/since/{id}`, `/meta`, `/dream/reports`
- Storage: `graph_layout` table, layout/embedding helpers, audit cursor,
  `list_dream_reports`
- API hardening: mutation audit, R-08 coordinator graph eviction on
  trash/delete/bulk, REST `visibility` on remember/update
- ui-slint Web app tile → Lucida URL with `?token=` for browser auth
