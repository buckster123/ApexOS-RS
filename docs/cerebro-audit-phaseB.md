# CerebroCortex — Phase-B Defect Audit

> Read-only multi-agent audit of the **post-reconcile** cerebro (ApexOS-RS `cerebro/crates/`).
> Hunts NEW defects — the 11 C-RS parity findings are already fixed and excluded.
> Findings cross-apply to standalone `CerebroCortex-RS` (same codebase post-reconcile).

- **Method**: 9 dimension finders → adversarial verification per finding → synthesis
- **Yield**: 38 candidates → 34 verified → 30 distinct (after dedup)
- **Severity**: 0 critical · 6 high · 9 medium · 12 low
- **Status sweep (2026-07-09)**: re-verified every finding against current code. The waves were largely executed (commits `1494e0b`/`f50ba92`/`700c739`/`39a5fb1`/`384582b`, graph prune #95, vec0 upsert #141, cognitive_bootstrap `9c59b26`, audit-log wiring #243) — **25 of 30 FIXED** (CB-021 retention sweep 2026-07-09; CB-007/009/019 lock-discipline cluster 2026-07-10 — embed runs lock-free, graph node precedes the fallible vector persist, embed failure degrades instead of erroring), 1 mitigated at the agentd layer (CB-017), 4 still open (CB-003/008/018/029). Per-finding **Status** lines below; evidence text and line numbers remain the audit-time snapshot.

## Verdict

The cerebro memory subsystem is functionally rich and, in its single-process MCP path, mostly sound — the audit found no panics reachable through typed handlers, correct FTS output filtering, working scope enforcement in spreading activation (C-RS-003), and a hardened MCP dispatch panic boundary (C-RS-002/007). However, the codebase carries a coherent cluster of real defects concentrated at two seams: (1) the dual-process deployment (cerebro-mcp + cerebro-api over one SQLite file) which was never designed for — no busy_timeout, divergent in-memory graph caches, and front-end behavioral drift (PUT re-embed, priority casing, recall filters); and (2) lifecycle on a never-reset Pi brain — the in-memory graph, vec0 index, and FTS5 index all grow monotonically and never reflect deletes, with vec0 rowid-reuse able to silently mis-rank recall. The single highest-impact item is non-storage: cognitive_bootstrap, mandated as APEX's step-0 boot priming, is a fake-success stub that silently primes nothing. Security findings are real but single-tenant integrity risks (self-asserted agent_id, unscoped destructive ops) rather than cross-tenant exploits. Overall health: solid core, fragile multi-process edges, and several pieces of forgotten integration work sitting on hot/boot paths.

## Summary

34 verified findings deduplicated to 29 distinct defects (5 merges: vec0-orphan #6+#15, embed-lock #3+#10, cognitive_bootstrap-stub #24+#29). Prioritized into 3 execution waves. The dominant themes are multi-process SQLite sharing hazards, unbounded growth of three indexes on a never-reset daemon, MCP/HTTP front-end behavioral divergence, and forgotten integration work (cognitive_bootstrap) on the boot path. Highest severities: 6 high, 9 medium, 12 low after dedup.

## Execution waves

- **Wave 1 — Correctness, safety & data integrity (concurrency, storage drift, boot path)** — CB-001, CB-002, CB-003, CB-004, CB-005, CB-006, CB-007, CB-008, CB-009, CB-019
- **Wave 2 — Integration, API consistency & security boundary** — CB-010, CB-011, CB-012, CB-013, CB-017, CB-018, CB-024, CB-025, CB-026, CB-029
- **Wave 3 — Lifecycle hygiene, hardening & docs drift** — CB-014, CB-020, CB-021, CB-022, CB-023, CB-027, CB-030, CB-015, CB-016, CB-028

## Positives (verified good — do not re-litigate)

- MCP dispatch panic isolation (C-RS-002/007) is solid: per-call tokio::spawn + JoinError→JSON-RPC -32603, and the unmatched-tool `_` fallback returns an honest Err rather than a fake-success stub (only cognitive_bootstrap is a deliberate carve-out). *(The carve-out has since closed — cognitive_bootstrap is a real assembler now, see CB-001.)*
- Spreading-activation scope enforcement (C-RS-003) is correctly wired and default-deny: spread() takes a visible_nodes map, cortex.recall builds it via scope.can_access, and non-visible neighbors are skipped — cross-agent leakage via graph traversal is closed (the docs claiming otherwise are stale, CB-016).
- FTS5 OUTPUT correctness is sound: all FTS/vector result paths filter `m.deleted_at IS NULL`, so soft-deleted memories never surface in results (the defects are index-retention/ranking, not wrong rows).
- Canonical FSRS retrievability is implemented correctly and consistently in the core engine (activation/fsrs.rs, spreading.rs, models/link.rs) — store/recall scheduling uses the right power-law curve; only the analytics-only activation_at_risk tool diverges (CB-013).
- Read/store/update scope filtering via VisibilityScope::sql_filter is consistently applied across the non-destructive query path (the gap is the unscoped destructive ops, CB-018, and self-asserted identity, CB-017).
- WAL journal mode is enabled, and foreign_keys=ON is set — the storage layer's durability/integrity pragmas are present (the gap is the missing busy_timeout, CB-002).
- cerebro-api ApiError cleanly maps anyhow errors to structured 500s, and the API defaults to a 127.0.0.1 bind requiring a token for any non-loopback exposure — the ordinary error path and network exposure posture are sound.
- No reachable panic path (unwrap/expect/indexing) exists in the current cerebro-api handler bodies — CB-023 is purely a latent defense-in-depth gap, not an active crash.
- log_audit_event is a dead write-path (zero call sites), so audit_log does not actually grow — narrowing CB-021 to dream_reports + memory_versions. *(Superseded 2026-07: #243 wired audit writes at the dispatch chokepoint — every successful mutating tool call now logs a row, so audit_log DOES grow; see the CB-021 status.)*

---

## Findings

### HIGH

#### CB-001 · [high] · M · cognitive_bootstrap is APEX's step-0 boot priming call but returns a fake-success not_yet_implemented stub

- **Dimension**: API & protocol consistency / Forgotten integration
- **Location**: `cerebro/crates/cerebro-mcp/src/dispatch.rs:969 (handler arm); cerebro/crates/cerebro-mcp/src/tools.rs:831-840 (schema fallback); config/soul.md:101; docs/symbiosis.md:65,161-162`
- **Evidence**: dispatch.rs:969 is the explicit arm `"cognitive_bootstrap" => Ok(json!({"status":"not_yet_implemented","tool":name}))` — an Ok (success) value, deliberately carved out ABOVE the `_` fallback (dispatch.rs:976, which C-RS-007 changed to return Err). The comment at dispatch.rs:966-968 states it keeps the success stub because the boot 'expects a SUCCESS response.' soul.md:101 mandates `cognitive_bootstrap(query=<task/context>, mode="standard")` as step-0 of EVERY session startup (the 'dynamic priming block'). The handler ignores query/mode and produces zero priming content. A workspace-wide grep finds no real implementation anywhere. Because the result is Ok, the model reads it as ran-fine and never errors/retries.
- **Impact**: Every APEX session on the permanent Pi brain boots with only the static soul kernel — no task-relevant memories/intentions/procedures injected — and has no runtime signal that priming failed. This is the central CCBS-not-wired gap (BACKLOG #5, architecture.md:215) sitting on the critical boot path, NOT covered by the C-RS-007 fix.
- **Recommendation**: Implement cognitive_bootstrap to assemble a priming block from session_recall + list_intentions + find_relevant_procedures keyed on `query`. Until then, make it return an honest not-implemented error (like the other Tier-7 stubs) so boot fails loudly, and switch soul.md step-0 to session_recall as the interim. Merges findings #24 and #29.
- **Status (2026-07-09)**: **FIXED** (`9c59b26`) — `cognitive_bootstrap` is a real live-state assembler (`dispatch.rs` arm → `assemble_bootstrap`, budget modes minimal/standard/full, scope-aware) with a real schema (required `query`; mode/max_tokens/agent_id — closes CB-027 too), and agentd now boot-primes through it daemon-side on every session's first turn (CCBS, `AGENTD_CCBS`).

#### CB-002 · [high] · XS · Two daemons share one SQLite file with no busy_timeout — concurrent cross-process writes fail with SQLITE_BUSY instead of waiting

- **Dimension**: Concurrency & deadlock
- **Location**: `cerebro/crates/cerebro/src/storage/sqlite.rs:408-438 (SqliteStore::open)`
- **Evidence**: open() runs only `PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;` — no busy_timeout/busy_handler/set_busy anywhere in the workspace (repo-wide grep empty), no OpenFlags override. rusqlite 0.31 (Cargo.toml:45) defaults to a 0ms busy timeout. install.sh (676-677,776,782) installs+enables BOTH cerebro-mcp (config/plugins.toml:8) and cerebro-api (deploy/cerebro-api.service) by default, both pointing at CEREBRO_DATA_DIR=/var/lib/agentd/cerebro → the same cerebro.db (config.rs:17). Both are heavy writers. The per-process Arc<Mutex<Connection>> serializes only within a process; WAL allows N readers + 1 writer, so a second writer colliding on the write lock gets 'database is locked' instantly.
- **Impact**: When agentd-driven memory writes (cerebro-mcp) overlap with a dashboard/dream write (cerebro-api), one side's INSERT/UPDATE returns 'database is locked' and the operation fails — in remember() a memory is silently lost (error propagates as tool error / 500); in a dream phase the phase aborts. Intermittent dropped writes under normal concurrent load on the permanent brain.
- **Recommendation**: Set `conn.busy_timeout(Duration::from_secs(5))` (or `PRAGMA busy_timeout=5000`) immediately after open on every connection. Consider `PRAGMA synchronous=NORMAL` for WAL. Standard fix for multi-process WAL access.
- **Status (2026-07-09)**: **FIXED** (`1494e0b`) — `SqliteStore::open` sets `conn.busy_timeout(5s)` (sqlite.rs:443) with a comment naming this exact two-daemon collision.

#### CB-003 · [high] · L · In-memory graph cache diverges between cerebro-mcp and cerebro-api — cross-process writes invisible to spreading activation and associate's existence check

- **Dimension**: Concurrency & deadlock
- **Location**: `cerebro/crates/cerebro/src/storage/graph.rs:7-46; cerebro/crates/cerebro/src/cortex.rs:92,130-145,193-201; storage/mod.rs:19`
- **Evidence**: GraphStore::rebuild_from_db is called exactly once at StorageCoordinator::new (mod.rs:19); no runtime re-read mechanism exists. cerebro-mcp (main.rs:24) and cerebro-api (main.rs:867) each construct an independent CerebroCortex with its own petgraph+index over the same DB, both default-deployed. After startup the graph is mutated only in-process: remember→add_node (cortex.rs:92), associate→add_edge (cortex.rs:201). recall's spreading-activation seeds and visibility map are built from storage.graph.index/graph (cortex.rs:121-145); associate's pre-write guard (cortex.rs:193-198) rejects ids absent from this process's index.
- **Impact**: A memory/link created via one front-end never appears in the other's graph until restart. (1) recall via one process misses associative edges created by the other → degraded spreading-activation results; (2) associate via one process falsely bails 'source/target memory does not exist' for an id the other just committed. Drift worsens with uptime.
- **Recommendation**: Make the graph a true cross-process cache: fall back to a DB lookup when an id is missing from the index in associate/recall, invalidate+rebuild on a change-counter/TTL, or seed spreading activation directly from the links table so SQLite is the single source of truth. At minimum document that running both front-ends against one DB is unsupported until fixed.
- **Status (2026-07-09)**: **still OPEN** — the graph is still rebuilt once at `StorageCoordinator::new`; only `restore_memory` triggers a runtime rebuild, and `associate` still checks only the in-process index. No cross-process invalidation/TTL exists.

#### CB-004 · [high] · M · In-memory graph never shrinks — deletes leak nodes/edges forever and corrupt recall/associate on the never-reset daemon

- **Dimension**: Resource & lifecycle
- **Location**: `cerebro/crates/cerebro/src/storage/graph.rs:48-61; cerebro/crates/cerebro/src/cortex.rs:92,193-198; storage/mod.rs:19`
- **Evidence**: GraphStore exposes only add_node/add_edge — no remove_node/remove_edge anywhere (workspace grep). rebuild_from_db runs once at startup. Every delete path (delete_memory sqlite.rs:505, purge_memory 628, bulk_delete 794, prune_thread 921, purge_all_deleted 635, dream pruning dream.rs:516) writes only SQLite and never touches storage.graph, so the petgraph grows monotonically for the process lifetime. Correctness harm: spreading activation (cortex.rs:145) spreads through stale deleted nodes, distorting live-result association scores; associate's existence guard (cortex.rs:193-198) treats a soft-deleted memory's stale graph entry as 'exists & not soft-deleted' and creates a links row to a dead memory.
- **Impact**: On the permanent Pi brain (never reset), the graph accumulates a node per memory ever created plus an edge per link ever created, even after underlying memories are deleted/pruned: unbounded RSS growth on the 512MB Nano tier, plus silent divergence from SQLite truth (skewed recall scores, phantom edge creation) until restart.
- **Recommendation**: Add GraphStore::remove_node (drop the index entry + all incident edges; use StableGraph or rebuild the index map since petgraph::remove_node invalidates the last NodeIndex) and call it from every delete/purge/prune path. Interim: trigger rebuild_from_db after bulk delete/prune and on a schedule. At minimum gate spreading on a live-node check.
- **Status (2026-07-09)**: **FIXED** on the main paths (#95) — `GraphStore::remove_node` exists (index-swap repair unit-tested) and `StorageCoordinator::delete_memory`/`purge_memory`/`bulk_delete` prune the graph in-line; dream pruning routes through the coordinator; `restore_memory` rebuilds (node + links). Residual: `prune_thread` and `purge_all_deleted` still call `.sqlite` directly (dispatch.rs), so those nodes linger until restart.

#### CB-005 · [high] · S · Hard-purge and INSERT OR REPLACE orphan vec0 vector rows — index bloat plus rowid-reuse mis-ranking of recall

- **Dimension**: Storage integrity & migration
- **Location**: `cerebro/crates/cerebro/src/storage/sqlite.rs:628-639 (purge_memory/purge_all_deleted), :452 (insert_memory INSERT OR REPLACE), :1733-1736 (memories_ad trigger); storage/vector.rs:82-88,161-168`
- **Evidence**: memory_vectors (vec0 virtual table) is keyed by memories.rowid (memories is `id TEXT PRIMARY KEY`, a normal rowid table). The only writes are `INSERT OR REPLACE` in embed_and_store/store_raw_embedding. There is NO `DELETE FROM memory_vectors` anywhere; the memories_ad AFTER DELETE trigger maintains only FTS5, not vec0 (no FK/CASCADE on a virtual table). Reproduced in sqlite3: hard-delete frees the integer rowid, SQLite reuses it for a future INSERT, and vec_search's `JOIN memories m ON m.rowid = v.rowid` then returns the NEW memory's id ranked by the OLD memory's stale embedding distance until re-embedded. insert_memory's own INSERT OR REPLACE on an existing id allocates a new rowid and orphans the prior vec row too.
- **Impact**: On the never-reset Pi brain, dream-prune→purge churn accumulates orphan vectors that never go away (unbounded index growth on a RAM-constrained device), and after rowid reuse a vector search silently attributes a wrong similarity score to a live memory — a correctness error in recall ranking. Merges findings #6 and #15.
- **Recommendation**: Add `DELETE FROM memory_vectors WHERE rowid = old.rowid` to the memories_ad trigger (and the delete leg of memories_au), or explicitly delete the vec0 row in purge_memory/purge_all_deleted within the same transaction. Long-term, key memory_vectors by a stable surrogate rather than the reusable rowid.
- **Status (2026-07-09)**: **FIXED** (`1494e0b` + #141) — `insert_memory` drops the stale vec row before its INSERT OR REPLACE (sqlite.rs:488-494), the purge paths `DELETE FROM memory_vectors` explicitly, and the vec0 upsert itself is delete-then-insert (vec0 rejects INSERT OR REPLACE).

#### CB-006 · [high] · S · cerebro-api PUT /memory/:id updates content but never re-embeds — vector index goes stale (MCP path re-embeds correctly)

- **Dimension**: Storage integrity & migration
- **Location**: `cerebro/crates/cerebro-api/src/main.rs:295-311 (update_memory handler)`
- **Evidence**: The HTTP handler sets node.content then calls only `storage.sqlite.update_memory(&node)` (main.rs:309) and returns — no embed_and_store. update_memory (sqlite.rs:514-545) writes the content column but never the embedding blob or memory_vectors; the memories_au trigger refreshes FTS5 only. The MCP path for the same op (dispatch.rs:181-191) tracks content_changed and calls `storage.vector.embed_and_store(&node.id, &node.content)`. The two front-ends diverge. (Inert on the Nano no-embedder tier; live on all Micro/Standard/Pro tiers — the browser/PWA/mesh surface.)
- **Impact**: An agent editing a memory's content via HTTP leaves semantic/vector recall pointing at the pre-edit text indefinitely — recall matches OLD content and misses NEW content, while the same edit via MCP behaves correctly. Silent, client-dependent recall inconsistency on the integration boundary.
- **Recommendation**: In the API update_memory handler, when content is Some call `storage.vector.embed_and_store(&node.id, &node.content)` after the sqlite update. Better: centralize update-with-reembed in one Cortex method both front-ends call so the paths cannot drift again.
- **Status (2026-07-09)**: **FIXED** (`700c739`) — the API `update_memory` handler re-embeds when content changed (`// CB-006` block, main.rs:336-340), mirroring the MCP path.

### MEDIUM

#### CB-007 · [medium] · S · remember() holds the storage WRITE lock across the embedding spawn_blocking — stalls all readers/writers for the embed duration

- **Dimension**: Concurrency & deadlock
- **Location**: `cerebro/crates/cerebro/src/cortex.rs:89-92; cerebro/crates/cerebro/src/storage/vector.rs:60-91`
- **Evidence**: remember() acquires `self.storage.write().await` (cortex.rs:89, tokio RwLock) then awaits embed_and_store (cortex.rs:91), which runs fastembed inference via spawn_blocking (vector.rs:67-70) BEFORE touching the connection — so the write guard is held across the entire embedding computation. The embed needs only the Arc<embedder>, not the StorageCoordinator lock (the SQLite write uses a separate inner Arc<Mutex<Connection>>). In cerebro-api (single shared Brain=Arc<CerebroCortex> across concurrent handlers) and the dream engine, every concurrent recall (read guard) and remember/associate (write guard) blocks for the full embed latency (tens-to-hundreds of ms on Arm). Nano tier (no embedder) and serial cerebro-mcp are unaffected.
- **Impact**: Under bursty dashboard- or dream-driven remember load on embedding-enabled tiers, the whole memory subsystem serializes on CPU-bound inference rather than DB work, turning a single store into a latency spike for all concurrent callers in that process.
- **Recommendation**: Compute the embedding before taking the write lock: spawn_blocking the embed first, then acquire write() only for the synchronous insert_memory + store_raw_embedding + add_node. Merges findings #3 and #10.
- **Status (2026-07-10)**: **FIXED** — `remember()` now embeds LOCK-FREE before the write guard (`CerebroCortex::embed_lockfree`: brief read guard clones the embedder Arc, inference runs unguarded); the write lock covers only insert + add_node + `store_raw_embedding`.

#### CB-008 · [medium] · M · recall() does an O(n) full-graph scan plus an all-ids visibility IN-query on every scoped recall

- **Dimension**: Resource & lifecycle
- **Location**: `cerebro/crates/cerebro/src/cortex.rs:130-145; storage/sqlite.rs:720-752 (get_visibility_meta)`
- **Evidence**: For any recall with scope.agent_id=Some (the normal per-agent case), the code collects ALL graph ids (cortex.rs:133), calls get_visibility_meta over the entire id list (building a single un-chunked IN(...) clause, one placeholder per memory, sqlite.rs:725), and iterates every graph node to build the visibility HashMap (cortex.rs:135-143) — O(live-store-size) per recall, independent of k. The consumer (spread, spreading.rs) only needs visibility for the seeds' bounded neighborhood (SPREADING_MAX_ACTIVATED=50, SPREADING_MAX_HOPS=2), so the all-ids map is over-broad. SQLite's default SQLITE_MAX_VARIABLE_NUMBER is 32766 (bundled 3.45.x), so recall hard-fails once the store exceeds ~32k live memories.
- **Impact**: Recall latency degrades linearly with total live store size on the agent's hot path, and eventually hard-fails the IN query at very large stores. n scales with the full live store, not k.
- **Recommendation**: Restrict the visibility map to only the nodes actually reachable from the search seeds within SPREADING_MAX_HOPS: compute the candidate frontier first, then fetch visibility for just those ids. This also caps the IN-clause size well under SQLite's parameter limit.
- **Status (2026-07-09)**: **still OPEN** — a scoped recall still collects ALL graph ids and runs one un-chunked IN through `get_visibility_meta` (cortex.rs:157-158, sqlite.rs:890).

#### CB-009 · [medium] · S · remember() partial-write — sqlite row committed but graph node skipped when embedding fails (no transaction across the three stores)

- **Dimension**: Storage integrity & migration
- **Location**: `cerebro/crates/cerebro/src/cortex.rs:88-92`
- **Evidence**: The store path runs three independent awaits with no surrounding transaction: insert_memory(?), embed_and_store(?), graph.add_node. SQLite is autocommit so the row is durable before the embed. embed_and_store can error at runtime on embedder-present tiers (ONNX/JoinError via `??` at vector.rs:67-70, e.g. OOM during inference); the `?` at cortex.rs:91 returns early, so add_node never runs. There is no graph repair path, so the memory exists in SQLite but is absent from the petgraph until restart. (Nano no-embedder tier returns Ok early and is unaffected.)
- **Impact**: The orphaned memory is unreachable by spreading activation (which runs purely on the in-memory graph), and a later associate() from it fails the cortex.rs:193 index pre-check and rejects the link entirely. Self-heals only on the next daemon restart — weeks apart on the always-on Pi.
- **Recommendation**: Add the graph node before the fallible embed (add_node is infallible/cheap), or make embedding failure non-fatal (log + continue, matching how a missing embedder is already a no-op). Ideally wrap the three writes so an embed failure rolls back the sqlite insert.
- **Status (2026-07-10)**: **FIXED** — `add_node` now runs immediately after `insert_memory` (infallible, before the vector persist), and an embed/persist failure is NON-FATAL (warn + store without a vector; FTS5 still finds the memory). A failed embed can no longer orphan a memory out of spreading activation until restart. Integration-tested (graph membership + FTS recall on the vector-less path).

#### CB-010 · [medium] · XS · MCP server exits on a single malformed JSON-RPC frame — parse error is fatal, not isolated per-frame

- **Dimension**: Panic & error-handling surface
- **Location**: `cerebro/crates/cerebro-mcp/src/main.rs:43-49; cerebro/crates/cerebro-mcp/src/transport.rs:25`
- **Evidence**: transport.read() returns Err for any non-JSON line (`serde_json::from_str(line.trim())?`). The main loop's Err arm only breaks-cleanly when the message contains 'EOF'; any other error (a serde message like 'expected value at line 1 column 1') logs `transport error` and breaks, after which main returns and the process exits. So one malformed/partial/truncated line kills the daemon. This is exactly the failure C-RS-002 hardened INSIDE dispatch_tool (per-call tokio::spawn + JoinError isolation), but the transport/parse layer one level up has no equivalent guard.
- **Impact**: cerebro-mcp is APEX's shared long-lived brain spawned by agentd. A corrupt frame (client bug, truncated write, stray non-JSON line) kills it mid-turn. restart="always" (plugins.toml:10) respawns it, but the restart drops the in-flight MCP session/tool result, re-runs CerebroCortex::new reloading the ~275MB embed model (multi-second cold start on Pi), and can loop if the offending frame is re-sent.
- **Recommendation**: In the main loop, match the read result so EOF still breaks cleanly but a deserialization error logs and `continue`s to the next line (optionally emit a JSON-RPC -32700 parse-error response). Reserve break for genuine stdin EOF/IO-closed.
- **Status (2026-07-09)**: **FIXED** (`f50ba92`) — `transport.read()` returns a `Frame` enum (Value/Eof/ParseError); the main loop answers a malformed frame with a JSON-RPC -32700 and keeps serving; `Err` is reserved for genuine IO failures.

#### CB-011 · [medium] · S · anyOf[array,string] schema fields (tags/source_ids/concepts/derived_from) are read only via as_array() — a bare-string value is silently dropped

- **Dimension**: API & protocol consistency
- **Location**: `cerebro/crates/cerebro-mcp/src/dispatch.rs:116,184,690,741,768,830,867; schemas at tools.rs:26,109,125,632,674-675,702-703,732-733,760-761`
- **Evidence**: The inputSchemas declare these fields `{"anyOf":[{"type":"array"},{"type":"string"}]}` but every handler calls only `.as_array()`. A JSON string returns None from as_array(), so the field is dropped with no error. There is no normalization layer (dispatch_tool passes arguments verbatim; no coercion helper exists). Affects remember, update_memory, memory_store, store_intention, store_procedure, find_relevant_procedures, create_schema, find_matching_schemas. For find_relevant_procedures/find_matching_schemas an empty input hits the `is_empty()` guard and returns [].
- **Impact**: A model passing the schema-sanctioned `"tags": "urgent"` stores a memory with NO tags and gets status:ok; a single-concept-string search silently returns []. Silent data loss / empty results on a schema-permitted input shape on the shared brain.
- **Recommendation**: Add a helper that coerces both shapes (if as_str() is Some, wrap as a single-element vec; else as_array()) and apply it everywhere these anyOf fields are read so handlers honor the advertised schema.
- **Status (2026-07-09)**: **FIXED** (`f50ba92`) — a string-or-array coercion helper is applied at the anyOf sites (`// CB-011` in dispatch.rs; bare-string `tags` regression-tested).

#### CB-012 · [medium] · XS · session_save priority casing diverges between MCP (uppercase) and HTTP (lowercase) — HTTP-written notes unfindable by MCP priority-filtered recall

- **Dimension**: API & protocol consistency
- **Location**: `cerebro/crates/cerebro-api/src/main.rs:440,444; cerebro/crates/cerebro-mcp/src/dispatch.rs:279,311-314,994-996`
- **Evidence**: MCP session_save normalizes priority via normalize_priority (= p.to_uppercase(), default 'MEDIUM') → tag `priority:MEDIUM`, and session_recall filters with an exact match on `priority:{normalize_priority(p)}` (uppercase). HTTP session_save writes the value verbatim with default lowercase 'medium' → tag `priority:medium`, no normalization (grep confirms no to_uppercase/normalize call on the API path). Both write into the same shared store and both funnel through CerebroCortex::remember, which persists tag strings verbatim.
- **Impact**: An HTTP/dashboard/PWA session note tagged `priority:medium` is never matched by an MCP session_recall(priority="medium") whose filter normalizes to `priority:MEDIUM`. The note is still recalled without a priority filter (session_note tag still matches), so data is not lost — but the priority-filtered query silently misses it.
- **Recommendation**: Apply the same uppercase normalization in cerebro-api session_save and align the default to 'MEDIUM'; ideally extract one shared normalize_priority used by both crates.
- **Status (2026-07-09)**: **FIXED** (`700c739`) — cerebro-api has its own `normalize_priority` (uppercase, default 'MEDIUM', unit-tested against the MCP canonical form) applied at session_save AND the session_recall filter (main.rs:85,472,501).

#### CB-013 · [medium] · XS · activation_at_risk uses a divergent (non-FSRS) exp(-t/S) retrievability formula, labelled 'retrievability'

- **Dimension**: Logic & math edge cases
- **Location**: `cerebro/crates/cerebro/src/storage/sqlite.rs:1080 (doc :1048-1049); dispatch at cerebro-mcp/src/dispatch.rs:525-531`
- **Evidence**: activation_at_risk computes `let ret = (-days / stability.max(0.001)).exp();` — exponential forgetting R(t)=exp(-t/S). Everywhere else uses the canonical FSRS power-law R(t)=(1+t/(9·S))^-1 (activation/fsrs.rs:11-15 retrievability(), reused in spreading.rs:32 and models/link.rs:43). For days=1, S=1: exp gives 0.368 vs FSRS 0.90. Default threshold 0.7 (dispatch.rs:526); output JSON key is literally 'retrievability' (sqlite.rs:1084); sort keys on it. The doc comment self-contradicts ('FSRS retrievability. R(t)=exp(-t/stability)').
- **Impact**: The activation_at_risk MCP/HTTP tool over-reports decay: a 1-day high-stability memory shows 0.368 and is flagged at-risk under the 0.7 default though true FSRS R=0.90. The at-risk set and sort order are wrong, and any consumer comparing this value against canonical FSRS retrievability gets inconsistent numbers — spurious 'revive these memories' signals. Scope is advisory/analytics, not stored-state corruption (store/recall scheduling uses the correct curve).
- **Recommendation**: Replace with `let ret = crate::activation::retrievability(days.max(0.0), stability);` (reusing the guarded FSRS fn) and fix the doc comment. If an exp heuristic is intentionally wanted, rename the output key away from 'retrievability' to avoid cross-tool inconsistency.
- **Status (2026-07-09)**: **FIXED** (`1494e0b`) — activation_at_risk now calls `crate::activation::retrievability` (sqlite.rs:1253), the same guarded FSRS fn store/recall use; integration-tested.

#### CB-014 · [medium] · S · FTS5 fallback search assigns a flat 0.5 relevance score to every result — keyword-match ranking is flattened on the Nano tier

- **Dimension**: Forgotten parks & dead code / Logic
- **Location**: `cerebro/crates/cerebro/src/storage/vector.rs:223`
- **Evidence**: fts5_search pushes `(MemoryId(row?), 0.5_f32)` for every row (comment: 'FTS5 doesn't give a normalized [0,1] score easily; use 0.5 placeholder'). This score becomes the vector_sim term: cortex.rs:148 collects it into sims_map (and cortex.rs:121-123 uses it as spreading-activation seed weight); prefrontal.rs:53 feeds it as vector_sim into recall_score, where it is weighted by SCORE_WEIGHT_VECTOR=0.35 — the largest of four weights. A constant contributes a fixed 0.175 to every candidate, so keyword relevance no longer discriminates; final ordering collapses onto activation/FSRS/salience (the SQL ORDER BY rank BM25 order is discarded once re-ranked). Reached on the Nano FTS5-only tier (CEREBRO_EMBED_MODEL='') and as the vec0-empty fallback on any tier (vector.rs:129).
- **Impact**: On the most resource-constrained tier the project optimizes for first, recall/memory_search loses keyword-relevance weighting entirely — the best keyword match is not preferentially surfaced.
- **Recommendation**: Surface FTS5's bm25() score (`SELECT m.id, bm25(memories_fts) ... ORDER BY rank`) and map it monotonically into [0,1] (lower bm25 = better) instead of the constant 0.5, so vector_sim carries real keyword relevance into recall_score and seed weighting.
- **Status (2026-07-09)**: **FIXED** (`1494e0b`) — the FTS5 fallback surfaces `bm25(memories_fts)` mapped through a logistic into (0,1] (vector.rs:253-279); integration-tested.

#### CB-015 · [medium] · XS · SDK doc states the `_` dispatch fallback returns a SUCCESS stub — C-RS-007 made it return an Err

- **Dimension**: Docs & ops drift
- **Location**: `docs/sdk/04-cerebro-for-agents.md:132-134,267-273,361-363`
- **Evidence**: The doc describes the unmatched-tool path as `_ => Ok(json!({"status":"not_yet_implemented"...}))` at dispatch.rs:921 that 'returns success'/'silently no-ops', and lists ingest_file/describe_image/search_vision as success stubs. Actual code at dispatch.rs:976 is `_ => Err(anyhow::anyhow!("tool not implemented: {name}"))` (C-RS-007 hardening). The cited dispatch.rs:921 is now an unrelated get_memory_versions arm. Only cognitive_bootstrap (dispatch.rs:969, its own arm) still returns a success stub.
- **Impact**: An agent author following the doc is told a routeless tool 'silently no-ops with success' — the opposite of current behavior, which surfaces a JSON-RPC error the agent will branch on. Wake-loop pseudocode treating ingest_file/describe_image/search_vision as benign success stubs will instead receive errors. Line citations are stale.
- **Recommendation**: Update 132-134, 269-273, 361-363 to state the `_` fallback now returns an Err at dispatch.rs:976; move the three vision/ingest tools to an 'advertised but error-on-call' bucket; note cognitive_bootstrap as the sole success-stub at dispatch.rs:969; fix the dispatch.rs:921 citation.
- **Status (2026-07-09)**: **FIXED** (`384582b`) — the SDK doc was corrected; it no longer claims a success-stub fallback (and cognitive_bootstrap is since a real tool, CB-001).

#### CB-016 · [medium] · XS · Three docs still claim spreading.rs ignores scope / cross-agent leakage possible — C-RS-003 wired scope into spreading

- **Dimension**: Docs & ops drift
- **Location**: `docs/architecture.md:221; docs/sdk/04-cerebro-for-agents.md:277-279`
- **Evidence**: Docs state 'spreading.rs ignores the scope param' and 'cross-agent leakage is possible via graph traversal ... treat scope as a best-effort filter, not a hard isolation boundary.' Code: spread() takes `visible_nodes: &HashMap<NodeIndex,bool>` (spreading.rs:55-62) and skips non-visible neighbors (`if !visible_nodes.get(&neighbor).copied().unwrap_or(false) { continue; }`, spreading.rs:104-106, default-deny). cortex.recall builds the scope-visibility map via scope.can_access and passes it in (cortex.rs:125-145); global scope short-circuits to all-visible. Cross-agent leakage via traversal is NOT an open hazard.
- **Impact**: On the shared Pi daemon the docs tell operators cross-agent leakage is open and scope is only best-effort — now false. Risks an operator wrongly distrusting isolation (avoiding private memories) or a contributor 're-fixing' an already-fixed bug or ripping out the visible_nodes plumbing thinking it's unused. Doc-accuracy only; no runtime impact.
- **Recommendation**: Update both docs to state spreading activation now honors scope via the visible_nodes map (C-RS-003, spreading.rs:55-62 + cortex.rs:125-145); a scoped recall no longer lets another agent's private/thread memories shape activations. Note the global-scope all-visible short-circuit.
- **Status (2026-07-09)**: **FIXED** (`384582b`) — both docs now state spreading honors scope via the visible_nodes map (e.g. sdk/04-cerebro-for-agents.md:287); the federation `shared_only` scope was later wired through the same map too.

#### CB-017 · [medium] · M · agent_id is fully self-asserted — any caller can read another agent's private memories (or omit it to read everything via 1=1)

- **Dimension**: Security & integration boundary
- **Location**: `cerebro/crates/cerebro-mcp/src/dispatch.rs:984-989 (agent_scope); cerebro/crates/cerebro/src/types.rs:144-152 (sql_filter); cerebro-api/src/main.rs:61-66 (scope_from)`
- **Evidence**: Scope is derived solely from the caller-supplied agent_id tool argument with no authentication. for_agent(X) returns X's own private rows, so passing agent_id="APEX" reads APEX's private memories; omitting agent_id yields global() whose sql_filter is the literal '1=1' (types.rs:146), selecting EVERY row regardless of visibility/owner. recall/get_memory/list_*/session_recall/get_thread_memories all flow through this. cerebro-mcp is one stdio process shared by every agent/session; agent_id is LLM-written, never bound to an authenticated identity. The HTTP scope_from repeats the pattern.
- **Impact**: On the shared brain, agent_id is a cooperative hint, not a security principal. A coerced/injected same-trust model turn can exfiltrate every other agent's private memories by passing their agent_id or omitting it. Single-tenant integrity weakness, not a cross-tenant exploit (the realistic attacker is same-trust-boundary).
- **Recommendation**: Bind scope to an out-of-band identity: have agentd inject the authenticated session/agent identity into each MCP call (env or per-connection handshake) and use THAT for scope, ignoring/validating client-supplied agent_id. At minimum, require an explicit non-empty agent_id on read paths and never let an absent agent_id widen to 1=1 global.
- **Status (2026-07-09)**: **MITIGATED at the agentd layer** — the recommended out-of-band binding shipped there: `Supervisor::dispatch_tool` overwrites `agent_id` on every cerebro-plugin call with the session's resolved identity (`stamp_agent_id`, agentd plugins/supervisor.rs:181), so the model can't spoof/omit its space on the deployed path. cerebro-mcp itself still trusts a caller-supplied `agent_id` (direct-MCP callers/tests), and an absent agent_id still widens to `1=1` global — OPEN in cerebro proper.

#### CB-018 · [medium] · M · Destructive operations carry NO scope — a confused/coerced caller can destroy or re-own any agent's memories by id

- **Dimension**: Security & integration boundary
- **Location**: `cerebro/crates/cerebro/src/storage/sqlite.rs:505,628,642,794,859,921,987,999; reached via cerebro-mcp/src/dispatch.rs:164,335,343,357,403,469,486,494,504 and cerebro-api/src/main.rs:313,769,785,588,597,609`
- **Evidence**: delete_memory/purge_memory/restore_memory/bulk_delete/share_memory/prune_thread and delete_tag_everywhere/rename_tag_everywhere take a bare id/string and run DELETE/UPDATE WHERE id=? (or EXISTS over ALL memories for tags) with NO scope_sql clause. share_memory (sqlite.rs:859) sets visibility='private', agent_id=<arbitrary> on ANY id — seizing another agent's memory. By contrast update_memory and get_memory DO compute and enforce agent_scope, proving scope is intended on per-memory mutation and these are an omission, not admin-by-design.
- **Impact**: A single call with a guessed/recalled id permanently soft-deletes or hard-purges another agent's memory, restores deleted ones, or re-owns shared memories — irreversible on the never-reset brain. Global tag rename/merge/delete can corrupt the priority:/session_note/to: conventions that session_recall and inbox routing depend on for every agent at once. Single-tenant integrity (no auth boundary exists anyway), not a cross-tenant takeover.
- **Recommendation**: Thread VisibilityScope into delete_memory/purge_memory/restore_memory/bulk_delete/share_memory/prune_thread and add `AND {scope_sql}` to their WHERE clauses (mirroring update_memory). For share_memory, verify the caller owns the source before re-owning. Scope tag rewrites to the caller's visibility set rather than the whole table.
- **Status (2026-07-09)**: **still OPEN** — the destructive sqlite ops still take bare ids with no scope clause (delete_memory :549, purge_memory :766, restore_memory :812, bulk_delete :964, share_memory :1029, prune_thread :1091). Partially blunted in deployment by the agentd `agent_id` stamp (see CB-017), but the store layer itself remains unscoped.

### LOW

#### CB-019 · [low] · S · recall() holds the storage READ lock across the query-embedding spawn_blocking — blocks writers for the embed duration

- **Dimension**: Concurrency & deadlock
- **Location**: `cerebro/crates/cerebro/src/cortex.rs:108,112-113; storage/vector.rs:142-153`
- **Evidence**: recall() takes `self.storage.read().await` (cortex.rs:108) and holds it through the whole function including `storage.vector.search(...).await`, whose vec_search path embeds the query via spawn_blocking (vector.rs:151-153) while the read guard is alive. Read guards coexist, but a held read guard blocks any writer (remember/associate) on the tokio RwLock. The embed needs only the in-process embedder, no storage state. Nano tier (no embedder) falls through to FTS5 with no embed.
- **Impact**: In cerebro-api every in-flight recall extends the window during which a remember/associate cannot acquire the write lock by the query-embedding latency. Combined with CB-007, concurrent read+write traffic repeatedly stalls writers on inference. Embedder-enabled tiers only.
- **Recommendation**: Embed the query string before acquiring the read guard (or restructure search so embedding is lock-free), then take the read lock only for the SQLite/graph phases. Same fix pattern as CB-007.
- **Status (2026-07-10)**: **FIXED** — recall embeds the query via `embed_lockfree` BEFORE taking the read guard and passes it to the new `search_seeded` (which never embeds; `None` → FTS5). A failed query embed now degrades to FTS5 instead of erroring the recall.

#### CB-020 · [low] · S · vec0/FTS5/links/dangling-row cleanup — FTS5 index retains soft-deleted memories and never shrinks

- **Dimension**: Resource & lifecycle
- **Location**: `cerebro/crates/cerebro/src/storage/sqlite.rs:505-512,1728-1743`
- **Evidence**: Soft delete is `UPDATE memories SET deleted_at=...`. The memories_au AFTER UPDATE trigger deletes-then-reinserts the row into memories_fts using its unchanged content, so a soft-deleted memory stays fully indexed; the FTS delete trigger (memories_ad) fires only on a real row DELETE (hard purge). fts5_search filters output with `m.deleted_at IS NULL` (correct results) but the index keeps every soft-deleted memory and MATCH-scans tombstones. The migration-time WHERE-less FTS rebuild (sqlite.rs:256-257) re-indexes deleted rows too, so even a restart doesn't shed them.
- **Impact**: On the Nano FTS5-only tier this is the primary search index and it never shrinks across the daemon lifetime — soft-deleting frees no index space, steady disk + page-cache pressure on a 512MB board, plus wasted MATCH work over tombstoned rows.
- **Recommendation**: On soft-delete also issue the FTS5 'delete' command for that rowid (as memories_ad does), and re-insert on restore_memory; or run `INSERT INTO memories_fts(memories_fts) VALUES('rebuild')` periodically (e.g. a dream pre-phase) to compact the index against the live set.
- **Status (2026-07-09)**: **FIXED** (`1494e0b`) — the memories_au trigger now evicts a soft-deleted row from FTS5 and re-inserts only LIVE rows (`// CB-020` sites in sqlite.rs); integration-tested (soft-deleted memory gone from the index).

#### CB-021 · [low] · S · audit_log, memory_versions, and dream_reports grow with no retention policy (dream_reports is the real growth driver)

- **Dimension**: Resource & lifecycle
- **Location**: `cerebro/crates/cerebro/src/storage/sqlite.rs:1355-1369,1427-1450,1544-1565`
- **Evidence**: save_dream_report (1558) appends a row with full per-phase JSON per dream cycle; log_memory_version (1434) appends a content snapshot per restore (only call site dispatch.rs:935; update_memory does NOT snapshot). There is NO DELETE/retention path for any of the three tables (workspace grep empty). Note: log_audit_event (1363) has ZERO call sites — audit_log never grows, so that pillar of the original finding is a dead write-path. dream_run is not auto-scheduled in this codebase, so dream_reports growth is operator-paced.
- **Impact**: On the never-reset daemon, dream_reports and memory_versions grow without bound on the SD card (low rate, operator/restore-paced). memory_versions stores full content copies. No operator-visible knob to bound it.
- **Recommendation**: Add a retention sweep (cap rows or age-out by timestamp) for dream_reports and memory_versions (keep last N versions per memory), ideally as a dream pre-phase or periodic task mirroring close_stale_episodes. Optionally remove the dead log_audit_event or wire it up.
- **Status (2026-07-09)**: **FIXED** — `retention_sweep` (sqlite.rs) bounds all three tables as a dream pre-phase beside `close_stale_episodes`: newest-N versions PER memory (default 10), newest-N dream reports (default 90 ≈ a season of nightly dreams), newest-N audit rows (default 20 000 — the audit log is the agent's self-history, so the cap is generous and the sweep **audits itself** when it prunes: one `retention_sweep` row naming the counts, the trim-seam-marker honesty rule applied to the timeline). Knobs `CEREBRO_RETAIN_VERSIONS`/`_DREAM_REPORTS`/`_AUDIT_ROWS`, 0 = keep that table forever. Fail-soft in the pre-phase; unit + integration tested. (The fix became urgent after #243 made audit_log a live nightly-growing table and the dream went cron-scheduled — both narrowings from the original audit had inverted.)

#### CB-022 · [low] · S · Links to/from a hard-purged memory are not cleaned — purge of a still-linked memory hard-errors under the enabled FK pragma

- **Dimension**: Storage integrity & migration
- **Location**: `cerebro/crates/cerebro/src/storage/sqlite.rs:628-639 (purge); links FK at 1637-1638; foreign_keys=ON at 415`
- **Evidence**: purge_memory/purge_all_deleted delete from memories but issue no DELETE on links. The links FK declares `REFERENCES memories(id)` with NO ON DELETE CASCADE, and open() sets PRAGMA foreign_keys=ON on the single shared connection. Reproduced in sqlite3: a soft-deleted memory that still has a link makes `DELETE FROM memories WHERE deleted_at IS NOT NULL` fail with 'FOREIGN KEY constraint failed (19)'; the DELETE is rejected. Dream pruning only soft-deletes link-LESS sensory nodes, so an agent-soft-deleted linked memory reaches purge_all_deleted with links intact. The graph rebuild defensively skips orphan links, masking the issue at recall time.
- **Impact**: purge_memory/purge_all_deleted on a memory that still has associative links fails outright, surfacing as an opaque 500/MCP error to the agent. Rare (purge is a manual/admin op, common route is post-prune) but genuine store drift between the link table and memories.
- **Recommendation**: Declare the link FKs `ON DELETE CASCADE`, or explicitly `DELETE FROM links WHERE source_id=? OR target_id=?` inside purge_memory (and the IN-set form in purge_all_deleted) within the same transaction as the memory delete.
- **Status (2026-07-09)**: **FIXED** (`1494e0b`) — the purge paths clear links in the same transaction (`// CB-022` sites in sqlite.rs); integration-tested in both the single and set form.

#### CB-023 · [low] · S · cerebro-api has no CatchPanicLayer — a panicking handler drops the request with no response (latent; MCP sibling is hardened)

- **Dimension**: Panic & error-handling surface
- **Location**: `cerebro/crates/cerebro-api/src/main.rs:869-969 (router build)`
- **Evidence**: The axum app has only an auth middleware layer; no tower_http::catch_panic::CatchPanicLayer (grep for catch_panic/CatchPanic/catch_unwind empty; tower_http is not even a dependency — Cargo.toml lists only axum/tower). ApiError maps anyhow errors to a 500 JSON body but does not convert a true panic; axum/tower without the layer aborts the connection task on panic (tokio survives, client gets a reset, not a 500). No panic path is reachable through the current typed handlers (0 unwrap/expect/index in handler bodies), so this is latent. The MCP sibling has explicit per-call panic isolation (C-RS-002, dispatch.rs:40-77).
- **Impact**: Latent: any future handler panic degrades to an unexplained connection reset (no error body, hard to diagnose) instead of a clean 500, on the HTTP face of the shared brain. Consistency gap vs the hardened MCP path.
- **Recommendation**: Add tower_http (new dep) and `.layer(CatchPanicLayer::new())` with a responder returning a 500 JSON body matching ApiError, or a tokio::spawn/AssertUnwindSafe wrapper mirroring the MCP fix.
- **Status (2026-07-09)**: **FIXED** (`700c739`) — `CatchPanicLayer` is the outermost layer with a JSON-500 responder (`// CB-023` sites in cerebro-api main.rs).

#### CB-024 · [low] · S · Dream consolidation swallows sqlite write errors while still incrementing success counters

- **Dimension**: Panic & error-handling surface
- **Location**: `cerebro/crates/cerebro/src/engines/dream.rs:153,424-427,470-472,515-517`
- **Evidence**: Each phase persists via `let _ = ...save_dream_report/update_memory/delete_memory(...).await;` discarding the Result, then unconditionally increments counters (total_schemas/links_created at 424-427, memories_processed at 470-472, pruned at 515-517). The underlying sqlite methods propagate errors via `?` with no internal tracing, so the error vanishes. report.success = phases.all(p.success), and success only flips false on a `?`-propagated READ failure — swallowed writes never flip it. delete_memory's bool return is also discarded, so pruned over-counts even on a no-op. Reachable via dream_run MCP tool and POST /dream/run, which surface the report.
- **Impact**: On a write failure (disk full, SQLITE_BUSY under concurrent load) the dream reports success:true with inflated schema/prune/reprocess counts and no error logged — masking real persistence failures in a memory system, making memory-loss incidents harder to diagnose.
- **Recommendation**: Match on the write Result: on Err, tracing::warn!/error! (consistent with the existing 'Phase 3 LLM call failed' warn at dream.rs:431) and skip the counter increment so reported counts reflect only persisted work.
- **Status (2026-07-09)**: **FIXED** (`39a5fb1`) — dream phases surface persist errors and only count work that actually persisted, including the prune's live-row check (`// CB-024` sites in dream.rs).

#### CB-025 · [low] · XS · store_procedure advertises derived_from but the handler never reads it (sibling create_schema honors source_ids)

- **Dimension**: API & protocol consistency
- **Location**: `cerebro/crates/cerebro-mcp/src/dispatch.rs:735-748; schema at tools.rs:675`
- **Evidence**: store_procedure's inputSchema advertises `derived_from` ('IDs of memories this procedure is derived from') but the handler reads only content/salience/tags — never derived_from or source_ids. The parallel create_schema (dispatch.rs:827-851) reads source_ids and writes them into metadata.derived_from, and get_schema_sources reads them back. There is no get_procedure_sources, so the linkage is both unwritten and unreadable.
- **Impact**: An agent linking a stored procedure to the memories it was distilled from has that provenance silently discarded — inconsistent with create_schema, which keeps it.
- **Recommendation**: Mirror create_schema: read derived_from/source_ids (with the CB-011 array-or-string coercion) and persist into the procedure node's metadata, or drop the field from the schema if procedure provenance is intentionally unsupported.
- **Status (2026-07-09)**: **FIXED** (`f50ba92`) — store_procedure coerces `derived_from` (also accepts `source_ids`) and persists it into metadata; regression-tested.

#### CB-026 · [low] · XS · HTTP session_recall drops the priority/session_type filters the MCP twin honors

- **Dimension**: API & protocol consistency
- **Location**: `cerebro/crates/cerebro-api/src/main.rs:214-220,457-468; vs cerebro-mcp/src/dispatch.rs:300-321`
- **Evidence**: MCP session_recall accepts and applies priority and session_type filters (extra .filter() passes on the session_note tag, dispatch.rs:300-321). The HTTP RecallQuery struct (main.rs:214-220) has only query/top_k/agent_id, and the handler filters solely on the session_note tag (main.rs:464). axum's Query extractor (serde_urlencoded) silently drops unknown keys (no deny_unknown_fields), so ?priority=/?session_type= return HTTP 200 with the filters ignored.
- **Impact**: The two surfaces for the same logical op return different result sets. A dashboard/PWA caller filtering by priority/type via HTTP gets unfiltered results with no error, diverging from the MCP behavior APEX relies on.
- **Recommendation**: Add priority/session_type to RecallQuery and apply the same tag filters (with shared normalization, see CB-012) as the MCP handler so the two surfaces are consistent.
- **Status (2026-07-09)**: **FIXED** (`700c739`) — `RecallQuery` gained `priority` + `session_type` and the handler applies both as tag filters with `normalize_priority` (`// CB-026`, main.rs).

#### CB-027 · [low] · XS · cognitive_bootstrap advertises an empty placeholder schema (no query/mode) despite being called with arguments

- **Dimension**: Forgotten parks & dead code
- **Location**: `cerebro/crates/cerebro-mcp/src/tools.rs:836 (catch-all fallback)`
- **Evidence**: tool_schema() has no explicit arm for cognitive_bootstrap; it falls through to the catch-all that emits description '(not yet implemented) cognitive_bootstrap' and inputSchema `{type:object, properties:{}, required:[]}`. soul.md:101 calls it as `cognitive_bootstrap(query=..., mode="standard")`. Note: because the schema lacks additionalProperties:false and has no required fields, the documented arg-bearing call still VALIDATES cleanly (the original 'a validating client would reject it' claim is false). This is a cosmetic/surface-parity inconsistency on an already-tracked deliberate stub.
- **Impact**: Any MCP client introspecting tools/list sees cognitive_bootstrap as an undocumented no-arg tool literally labeled 'not yet implemented', contradicting soul.md. Leaks half-built state into the otherwise parity-clean advertised surface.
- **Recommendation**: When CB-001 is implemented, give it a real schema arm (query: string required, mode: string enum). Until then either drop it from TOOL_NAMES so it is not advertised, or give it an honest schema describing the intended query/mode args.
- **Status (2026-07-09)**: **FIXED with CB-001** — cognitive_bootstrap has a real schema arm (tools.rs:868: required `query`, plus mode/max_tokens/agent_id) matching the implemented assembler.

#### CB-028 · [low] · XS · architecture.md mischaracterizes cognitive_bootstrap ('no route') and the vision/ingest tools ('incomplete schemas')

- **Dimension**: Docs & ops drift
- **Location**: `docs/architecture.md:215-222`
- **Evidence**: Line 215-216 says cognitive_bootstrap has 'no route ... falls through to a not-yet-implemented stub' — but it has an EXPLICIT arm at dispatch.rs:969 (does not fall through). Line 221-222 lumps ingest_file/describe_image/search_vision as schemas that 'were never completed (Step 9 schema work)' — they ARE in TOOL_NAMES with a placeholder schema (tools.rs:836) and now return an Err via the `_` arm (dispatch.rs:976); the missing piece is dispatch LOGIC, not schema, and they behave oppositely to cognitive_bootstrap (Err vs Ok success-stub). The dispatch.rs/tools.rs comments are self-documenting, so a code reader is not actually misled; zero runtime impact.
- **Impact**: Misleads anyone debugging the CCBS boot (greps for a missing route, doesn't find the explicit stub arm) and mischaracterizes the three Tier-7 tools as missing schemas rather than missing logic. Stale-doc accuracy issue.
- **Recommendation**: Rewrite 215-216 to say cognitive_bootstrap has a dedicated success-stub arm (dispatch.rs:969) returning not_yet_implemented as fake success; rewrite 221-222 to say ingest_file/describe_image/search_vision are advertised in tools.rs but unrouted, returning a JSON-RPC error via the `_` fallback (dispatch.rs:976).
- **Status (2026-07-09)**: **FIXED** (`384582b`) — architecture.md was corrected; overtaken by events since: cognitive_bootstrap is implemented (CB-001), describe_image/search_vision have real routes, and only ingest_file remains an advertised-but-error-on-call deferred stub (the honest `_` Err fallback).

#### CB-029 · [low] · S · No maximum content length on the remember/store path, and MCP stdio reads an unbounded line per message

- **Dimension**: Security & integration boundary
- **Location**: `cerebro/crates/cerebro/src/engines/thalamus.rs:36 (only a 10-char MIN); cerebro/crates/cerebro-mcp/src/transport.rs:19-26`
- **Evidence**: Thalamus enforces only MIN_CONTENT_LENGTH=10 with no upper bound; remember() stores the full string and FTS5 indexes it; all 6 MCP store handlers + API /remember pass content straight through (HTTP body capped ~2MB by axum default; MCP stdio is not). Separately, transport.read() does `read_line(&mut line).await?` with no length cap, buffering an unterminated multi-GB line fully in RAM before parsing. Mitigation: fastembed's default tokenizer truncates at 512 tokens, so the embedding COMPUTE is bounded — the OOM-the-embedder claim is overstated; only tokenizing/SQLite-write/FTS-index work scales with size. Trust model: the stdin peer is agentd (trusted parent), not a network client — this is defense-in-depth, not a remote DoS.
- **Impact**: A misbehaving/compromised upstream can grow the DB and (via the unbounded read_line) OOM the cerebro-mcp daemon with one unterminated line. The daemon currently has no self-defense against its upstream. Defense-in-depth gap on the shared memory subsystem.
- **Recommendation**: Add a MAX_CONTENT_LENGTH gate in thalamus.evaluate_input (reject or truncate, e.g. 32-64KB) so it applies uniformly to MCP and API. In transport.read() use a bounded reader (AsyncBufReadExt::take / a byte budget) and bail past N MB, returning a JSON-RPC parse error rather than buffering unboundedly. Merges findings #21 and #22.
- **Status (2026-07-09)**: **still OPEN** — thalamus still enforces only the 10-char MIN (no max), and `transport.read()` still buffers an unbounded `read_line` (the CB-010 Frame fix changed parse fatality, not the length cap).

#### CB-030 · [low] · XS · MAX_STORED_TIMESTAMPS cap is defined and documented but never enforced (dead constant / false invariant)

- **Dimension**: Resource & lifecycle
- **Location**: `cerebro/crates/cerebro/src/config.rs:33; cerebro/crates/cerebro/src/models/memory.rs:22-23`
- **Evidence**: config.rs:33 defines `MAX_STORED_TIMESTAMPS: usize = 50` and memory.rs:22 documents access_times as 'capped at MAX_STORED_TIMESTAMPS (50)', but the constant is referenced nowhere except its own definition and the doc comment (workspace grep). access_times serializes unbounded to SQLite (sqlite.rs:473). Today access_times is never appended to (no access-recording-on-recall path is wired), so the array doesn't actually grow — this is latent/forgotten work, not an active leak.
- **Impact**: Latent: the moment ACT-R access recording on recall is wired up, each memory's access_times JSON array grows without bound, bloating every row read on the recall hot path. Today the documented invariant is simply false and the constant is dead.
- **Recommendation**: Either apply the cap (a MemoryNode::record_access that pushes now and truncates to the most-recent MAX_STORED_TIMESTAMPS) wherever access is recorded, or remove the unused constant and the misleading doc comment until access recording is implemented.
- **Status (2026-07-09)**: **FIXED** (`39a5fb1` + `9c59b26`) — `MemoryNode::record_access` enforces the cap (memory.rs:54-62, unit-tested), and access recording is now wired: recall reinforces the returned top-k (record_access + record_recall_review, batched persist — cortex.rs:199-219), so the invariant is both true and load-bearing.

---

*Generated by the `cerebro-audit-phaseB` workflow (48 agents). Each finding survived an adversarial refutation pass.*