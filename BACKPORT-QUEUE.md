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

## 1. W-A data-integrity wave (HIGH) — CC-RS PR #22

The four high-severity fixes from the CC-RS self-review
(`SELF-REVIEW-2026-08-07.md` there, findings R-02/R-04/R-05/R-06):

- **R-02** `engines/dream.rs` REM recombination: the one prompt site still
  byte-slicing content (`&content[..len.min(300)]`) — panics mid-emoji/CJK
  and aborts the whole cycle; use the file's own `truncate_chars`.
- **R-04** store-level version snapshots: `update_memory_noted(node,
  edited_by, change_note)` on `SqliteStore` — SELECTs the prior row and
  snapshots it into `memory_versions` when content differs, one transaction;
  plain `update_memory` delegates. MCP `update_memory` route passes the
  caller scope as `edited_by`. Makes `get_memory_versions`' "each content
  change creates a snapshot" contract true for the first time.
- **R-05** `remember`/`memory_store` honor the advertised `visibility` arg
  (`remember_with_visibility` on the cortex; `parse_visibility` in dispatch,
  unknown values hard-error). **Default flips to SHARED regardless of agent
  scope (Python parity)** — the agent-scoped=Private derivation silently
  broke cross-agent sharing and even send_message (sender-scoped messages
  were private to the SENDER). Orphan guard: private + no agent_id refused.
  `memory_store` schema gains the visibility param (true-alias doctrine).
  ⚠ Behavior change for every node — colony ratification note in the PR.
- **R-06** `purge_memory`/`purge_all_deleted` delete `memory_versions` +
  `vision_embeddings` child rows in-transaction (no-CASCADE FKs under
  foreign_keys=ON made any purge of a versioned/captioned memory fail).

5 riding tests in CC-RS (3 storage, 2 dispatch) port with it.

## 2. Ghost-FK repair (HIGH — ships WITH or BEFORE entry 1) — CC-RS PR #27

`repair_ghost_fk_memory_versions` in `SqliteStore::open()`: a
Python-migrated DB already HAS `memory_versions`, so SCHEMA_SQL's IF NOT
EXISTS skips it — and the Python FK references `memory_nodes`, which
migration renamed and the reap dropped. With foreign_keys=ON, any R-04
snapshot or R-06 purge cleanup then fails "no such table:
main.memory_nodes". **Every colony brain is Python-migrated** — landing
entry 1 without this breaks update_memory/purge in the field (this bit both
CC-RS local brains within an hour of W-A). Row-preserving rebuild, cheap
idempotent probe every open. Test:
`python_ghost_fk_memory_versions_repaired_on_open`. (Python `attachments`
tables carry the same ghost FK but are inert — nothing writes them; leave.)

## 3. Spreading-activation seed-cap no-op fix (HIGH) — CC-RS PR #24

`activation/spreading.rs`: the budget check counted the SEEDS
(`activated.len() >= SPREADING_MAX_ACTIVATED`), and recall over-fetches
`k*5 = 50` = the cap — so on any store returning a full candidate page the
spread broke before hop 1. **Spreading activation has been a silent no-op on
every mature brain**: association scores never contributed to ranking, and
`never_traversed_links_pct` could sit at exactly 100.0 (the colony's 4/4
finding — the missing write half was only part of the story). Python
inherits the identical flaw (spreading.py:155 — reference only). Fix: the
budget is `new_count`, growth beyond the seeds. Regression test
`full_seed_page_still_spreads`. Measured on a 700-memory brain: same query
0 → 75 walks, top score 0.415 → 0.62, and never_traversed dropped 79.6 →
79.1 in ONE recall. The colony's brains get their spreading back.

## 4. Traced recall (feature — port with or ahead of any Lucida upstreaming) — CC-RS PR #24

`spread_events` + `TraceEvent` (spreading.rs; `spread_traced` derives from
it — recording only, math untouched, fixtures unaffected), `recall_traced` +
`RecallTrace` on the cortex (`recall` is a thin wrapper, same reinforcement
— watching a thought is thinking it), `POST /recall/trace` in cerebro-api.
The observable anatomy of a recall: seeds with similarities, per-hop walks
in firing order, post-spread activation map.

## 5. Optional, when the colony wants eyes: Lucida + U1b API hardening — CC-RS PRs #23/#25/#27/#28

The whole observatory rides cerebro-api: `ui-web/` embedded at `/`,
`/graph/export`, `/graph/layout` (PCA cache in a new `graph_layout` table,
ON DELETE CASCADE), `/events` SSE audit tail + `/audit/since/{id}`,
`/dream/reports`, `/meta`, plus API hardening worth taking even without the
UI: **API mutations audit** (the MCP-only audit left REST writes invisible
to self-history), **R-08** (trash lifecycle + bulk_delete via the
coordinator's graph-eviction wrappers — deleted nodes stop spreading
without a restart), REST remember/update accept `visibility`. Design
charter: CC-RS `docs/UI-DESIGN.md`. A colony node running Lucida over its
own brain is the demo that sells itself — but it's dessert, not integrity.
