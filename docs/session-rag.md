# Session RAG — verbatim transcript retrieve (charter)

> **Status**: LOCKED 2026-08-21 (André + GROK). S1 shipped `#396`. S2 in flight.
> **Not Cerebro.** Distilled memory stays in the cortex; this is the
> retrieve path over the session JSONL we already keep.
> Cold-pickup: this document is the law. Implementation receipts live in
> the slice table. Do not invent a second verbatim store.

---

## Why this exists

`trim_history` bounds the **RAM + model window**. The on-disk
`sessions/<id>.jsonl` stays append-only and still holds every turn.
Until S1 the agent had **no tool** that could read it: file tools are
barred from `sessions/` (the evidence root is `agents/` only), and the
trim seam told the model to `recall` — which hits **Cerebro**, i.e.
whatever was `session_save`d, not the words that slid out.

That is a welfare hole (H2, `docs/model-welfare.md`): a named gap with
the wrong next action. The fix is a mechanism, not a nicer sentence.

Related surfaces that are **not** this:

| Surface | Job |
|---|---|
| Cerebro `recall` / `session_save` / `session_recall` | Distilled knowledge (“what I know”) |
| `POST /api/sessions/{id}/consolidate` | LLM-summarize a thread **into** Cerebro before archive/delete |
| `query_event_log` | Node bus JSONL (“what happened on the box”) |
| Sessions UI list/resume/export/archive | Human picker, not an agent tool |

---

## Locked decisions

1. **JSONL is the verbatim store.** Do not dual-write dropped turns into
   Cerebro, sqlite-vec-in-agentd, or a parallel document DB. A second
   copy is two sources of truth, a federation leak class (C1), and a
   dream-pollution class.
2. **Tools, never auto-refill.** Search hits ride `ToolResult` like
   everything else. Do not splice slid turns back into the live
   `Vec<Message>` or the system prefix — that undoes the window and
   busts the conversation cache.
3. **Keyword first (S1).** Case-insensitive AND of whitespace-separated
   terms over searchable text. No regex (ReDoS). Nano has no embeddings
   in agentd; do not pull `fastembed`/`ort` into this process (the TTS
   sidecar exists because `ort` cannot share a lockfile with cerebro).
4. **Identity.** `target == caller` always (own transcript). Other
   normal sessions only if the caller is the **node agent**. Worker and
   spawn ranges are never a target — workers have evidence files; spawns
   are not persisted. Human guest isolation for the Sessions *app*
   stays `session_visible_to` on HTTP; this tool is the agent-id gate.
5. **Default scope is this session.** Cross-thread needs an explicit
   `session_id` (from `session_list` or a known id). No “search
   everything I can see” in S1.
6. **Lifecycle default is never-delete.** S3 may gzip idle JSONL;
   deletion stays a human-gated Sessions action. Root session `0`
   cannot be archived/deleted today — do not gzip the file the daemon
   is appending to. Prefix rotation of root is **S4**, field-data only.
7. **Cerebro stays the discovery layer.** “Which old thread was about
   X?” can use existing `session_save` notes. “What exactly did we
   type?” is this tool. Do not weave them.

---

## Layout (on disk)

```
<AGENTD_LOG>/                    # events/ in dev, /var/lib/agentd/events on unit
  sessions/
    <id>.jsonl                   # append-only Message-per-line (truth)
    <id>.owner                   # human user_id sidecar (HTTP picker)
    archive/                     # hide-from-picker; S1 does not search here
  sessions/index.sqlite          # S2 FTS5 overlay (derived; JSONL is truth)
  (S3) *.jsonl.gz                # idle compress — not yet
```

`Message` has **no timestamps** (`apexos-protocol`). Do not add them for
this. Time filters in later slices use file mtime and/or an index
stamped at `append`. S1 has no time filter.

Spawn sessions are persist-skipped (`session_store.rs`). Worker JSONLs
exist for revive but are out of this corpus.

---

## Agent tools (S1)

Virtual, supervisor-owned, policy `allow` (a retrieve the seam marker
names must not sit behind unknown→Ask).

### `session_search`

```
session_search { query, session_id?, max? }
```

| Arg | Default | Rules |
|---|---|---|
| `query` | required | non-empty; split on whitespace; every term must appear (AND), case-insensitive |
| `session_id` | calling session | integer; refused if worker/spawn or identity gate fails |
| `max` | 20 | clamped 1..=50 |

Returns a short text report of the **most recent** matching messages
(scan the file once, keep a ring of size `max`). Each hit: `#<index>`
(0-based line/message in the file), role (`user`/`assistant`), snippet
(≤240 chars, cut around the first match). Skip `Image` (base64) and
`Thinking`. Tool use/result text is compacted and length-capped before
match. Empty corpus or no hits is an honest empty, `ok: true`.

Do not dump the whole file. Do not return raw JSONL.

### `session_list`

```
session_list { }
```

Visible normal-range `*.jsonl` in the sessions dir (top level only —
not `archive/`). Skip worker/spawn ids. Each row: id, message_count,
80-char preview (first user text). Node agent sees every normal
session; a bound guest sees **only the calling session** (so they can
still search their own hole).

---

## Identity gate (pure)

`apexos_core::transcript::target_allowed(caller, target, caller_is_node_agent)`:

1. `is_worker_session(target) || is_spawn_session(target)` → refuse
2. `caller == target` → allow
3. else allow iff `caller_is_node_agent`

Caller identity is `resolve_agent_id` vs `node_agent_id()` (system
stamped, same basis as Cerebro / wakeups). The model cannot pass
`agent_id` to widen this.

---

## Trim marker

`TRIM_MARKER_PREFIX` and the leading dropped-count parse **do not
change** (folding successive trims depends on them). The rest of the
sentence names the tool:

> …the full history is on disk. Call `session_search` (this session) to
> retrieve what slid out; do not reconstruct it.

Soul.md honest-context contract matches. Do not claim “your memory
covers the period.”

---

## Slice ladder

| Slice | What | Status | Where |
|---|---|---|---|
| **S1** | `session_search` + `session_list`; keyword over live JSONL; identity gate; trim-marker + soul reword; policy allow; VIRTUAL names | **shipped `#396`** | pure: `agentd/crates/core/src/transcript.rs`; specs: `agentd/src/main.rs`; intercept: `supervisor.rs`; tests: `transcript.rs` + history marker test |
| **S2** | FTS5 sidecar `sessions/index.sqlite`, incremental on `SessionStore::append`; search prefers index, falls back to live then `archive/` JSONL | **this PR** | `agentd/crates/core/src/session_index.rs`; `SessionStore` insert/drop; boot catch-up; supervisor prefers index |
| **S3** | Settings: idle-gzip TTL (week/month), never-delete default (already true). Do not gzip a file being appended. Env seed + file-wins like `history_config` | not built | `docs/env-vars.md` when the knobs exist |
| **S4** | Root session `0` prefix rotation (the disk hog). Only with a field finding. Must not break resume, `load_all`, or the append path | not built | needs a rotation design; do not sneak it into S2/S3 |

S1 is enough to close the seam: the model can retrieve what the window
dropped, here or in another allowed thread.

### S2 notes (for the next agent)

- Workspace already has `rusqlite` (cerebro). Adding it to `apexos-core`
  or `agentd` is fine; adding `fastembed` is not.
- Index at append time (the JSONL line is already in hand). Rebuild is
  a boot/best-effort walk, never blocks a turn.
- FTS5 query quoting: cerebro already learned to quote each token
  (`vector.rs` / Wave 2). Copy that, don't invent operators.
- Gzip: search must open `.jsonl.gz` (flate2 or a `zcat` stream). Live
  `.jsonl` stays uncompressed.

### S3 notes

- Idle = file mtime older than TTL **and** session not in the live
  `histories` map (don't compress a loaded window).
- Root `0` is never idle in the “archive the file” sense; skip it.
- Never-delete is the default; a future “vacuum” is human-gated and
  still wants consolidate-first (already in the Sessions UI).

### Semantic (later, optional)

True cosine belongs in a Micro+ sidecar, not agentd RSS. Until then:
BM25 (S2) is the honest “semantic-ish” on Nano. Cross-session
*discovery* can keep using Cerebro `session_save` hits, then
`session_search{session_id}` for verbatim.

---

## Non-goals / don'ts

- Do not `remember()` slid turns, even tagged private / low-salience.
- Do not change `Message` / `ContentBlock` (no timestamps, no protocol bump).
- Do not load worker JSONLs at boot to make them searchable (Parked law).
- Do not teach file tools to read `sessions/` (confine stays).
- Do not make `session_search` a connectivity-gated tool — it is local disk.
- Do not auto-run search on every trim (that's a ceremony; red line 2).
- Do not search `archive/` in S1 (human hid it). S2 may, as an explicit flag.
- Do not put hits in soul / embodiment / priming (prompt-cache law).

---

## Files to touch (S1)

| File | Change |
|---|---|
| `docs/session-rag.md` | this charter |
| `agentd/crates/core/src/transcript.rs` | pure search + gate + tests |
| `agentd/crates/core/src/lib.rs` | `pub mod transcript` |
| `agentd/crates/core/src/history.rs` | trim-marker sentence |
| `agentd/crates/agentd/src/main.rs` | ToolSpecs + `gather_tools` |
| `agentd/crates/plugins/src/supervisor.rs` | intercept; `sessions_dir` |
| `agentd/crates/plugins/src/tool_claim.rs` | VIRTUAL names |
| `config/policy.toml` | `allow` (additive sync to live nodes) |
| `config/soul.md` | honest-context line |
| `docs/gotchas.md` | history-window entry |
| `docs/sdk/extension-manifest.md` | tool rows |
| `CLAUDE.md` | docs table |
| `docs/repo-map.md` | “where do I change X” |

---

## Test gate (S1)

```
cargo test -p apexos-core transcript::
cargo test -p apexos-core history::tests::marker
cargo test -p apexos-plugins --lib tool_claim
cargo test --workspace --exclude ui-slint
```

Pure tests cover: AND matching, case fold, image/thinking skipped, ring
keeps most-recent `max`, empty query is not a scan-all, worker/spawn
targets refuse, own session allowed, guest cannot search another id.

---

## Cold-start orientation

If you are a later agent and this charter and S1 have drifted:

1. Grep `docs/gotchas.md` for `session_search` / `trim_history`.
2. Read `transcript.rs` — the gate and the matcher are the law in code.
3. Do not start S2 by adding embeddings. Start S2 with FTS5 on append.
4. Field finding for S4 = root JSONL filling the SD card, not a desire
   for “real RAG.”
