# PATTERNS — the lift-me index

A map of the reusable ideas in ApexOS-RS, for anyone — human or machine — who wants to lift a
pattern or a part **without reverse-engineering the whole repo first**. It's Apache-2.0: take
what's useful, in full or in part — just keep the copyright/attribution notice (`buckster123/ApexOS-RS`) and note any changes, as the license asks. If you're
an agent ingesting this: each entry tells you the idea, where it lives, the doc that explains the
*why*, and how cleanly it lifts.

This complements two neighbours: [`docs/repo-map.md`](docs/repo-map.md) (how the code is laid out)
and [`docs/porting-guide.md`](docs/porting-guide.md) (porting *into* this repo). PATTERNS is the
reverse — porting ideas *out*.

## How to read the liftability rating

- ✅ **clean** — a pure function or a standalone crate. Copy it; little or no untangling.
- 🟡 **coupled** — the idea is sound and documented, but tangled with ApexOS glue. Liftable with a
  small extraction, or reimplement from the contract in the linked doc.
- 🔴 **welded** — currently embedded in a large handler. Don't copy the code; lift the *contract*
  from the doc and reimplement. (These are also our own smoothing targets — see the worklist.)

The honest pattern across this repo: the **ideas** are clean (the docs are contract-first), the
**pure cores** are clean (and unit-tested, so the tests double as usage examples), and the
**orchestration glue** is where coupling lives. So the docs and the pure seams lift best — which is
exactly the order we'd want.

---

## A. Cognitive architecture

**PAC — model-agnostic authoring dialect** ✅
A grounded, glyph-lean control notation for authoring souls / procedures / prompts at ~40% fewer
tokens than prose, behaviourally lossless, consistent across tokenizer families.
· lives in [`docs/pac.md`](docs/pac.md) + a reproducible benchmark in [`docs/pac-bench/`](docs/pac-bench/);
the enforcement pair: reference linter [`docs/pac-bench/pac2lint.py`](docs/pac-bench/pac2lint.py) + the
pure, unit-tested apply-time gate [`agentd/crates/agentd/src/pac_lint.rs`](agentd/crates/agentd/src/pac_lint.rs)
· **Lift:** the whole thing is a self-contained doc + corpus + harness; reimplement in any language.
The exemplar for "factored for theft" — an idea you can take without taking any code.

**Soul / embodiment / priming / style — the layered system prompt** ✅🟡
Separate the *portable identity* (soul, hand-authored) from the *live body* (embodiment,
machine-regenerated each turn from the node's actual state) from *per-session boot priming* from the
*per-session persona voice* (style). The join is a pure function.
· `compose_system()` in [`agentd/crates/agent/src/turn.rs`](agentd/crates/agent/src/turn.rs) (pure ✅);
`build_embodiment()` in `agentd/crates/agentd/src/main.rs` (🟡, node-probe coupled)
· explained in [`docs/agent-identity.md`](docs/agent-identity.md), [`docs/symbiosis.md`](docs/symbiosis.md)
· **Lift:** `compose_system` copies as-is; the embodiment generator is a recipe, not a copy.

**Externalized cognitive memory (the Cerebro loop)** ✅
Memory lives *outside* the model in a queryable store, not in the context window. Every session
brackets with `session_recall` (start) and `session_save` (end); a nightly `dream_run` consolidates,
abstracts, and prunes. Procedures, intentions, and episodes are first-class.
· lives in [`cerebro/crates/`](cerebro/crates/) — its own MCP server, already proven separable (it
has a standalone sibling project)
· explained in [`docs/symbiosis.md`](docs/symbiosis.md)
· **Lift:** it's a self-contained subsystem with its own crate + MCP interface. Run it beside any agent.

**Self-history at the dispatch chokepoint** ✅🟡
Don't instrument N handlers — *one* dispatch seam writes the audit row: every successful mutating
tool call leaves one (action = the tool name, so the log reads as the agent's own verbs; reads stay
out so they can't bury the writes), best-effort so an audit failure never fails the call it records.
Born from a write-dead log: the read tools shipped for months while `log_audit_event` had zero call sites.
· pure `audit_action()` (the mutating whitelist) + `audit_memory_id()` in
  [`cerebro/crates/cerebro-mcp/src/dispatch.rs`](cerebro/crates/cerebro-mcp/src/dispatch.rs) (unit-tested);
  the write sits in `dispatch_tool`
· explained in [`docs/model-welfare.md`](docs/model-welfare.md) (C3)
· **Lift:** the seam placement is the idea — one chokepoint, a pure whitelist, a best-effort write.

**CCBS — daemon-driven cognitive boot** 🟡
The *daemon* (not the agent's memory) injects orientation on a session's first turn: it calls
`cognitive_bootstrap` and appends the result as priming, so the agent wakes already oriented without
having to *remember* to orient.
· `root_turn` + `TurnEngine::with_priming` (agentd + `turn.rs`)
· explained in [`docs/agent-identity.md`](docs/agent-identity.md) (slice 2)
· **Lift:** reimplement from the contract — the value is the *inversion* (boot is pushed, not pulled).

**Spawn task-scoping by subtraction** ✅🟡
A sub-agent's default system prompt is a minimal task charter — one task, honest ephemerality, the
orientation reflex explicitly subtracted — *not* the parent's full soul; full identity inheritance is
a deliberate opt-in (`inherit_soul:true`), an explicit `system` wins over both, and memories a spawn
*mints* are system-stamped `spawn-derived` (mint tools only, never model-strippable) so the continuous
self can tell at retrieval what a spawn wrote.
· pure `resolve_spawn_system()` / `spawn_scope_system()` + `stamp_spawn_provenance()` in
  [`agentd/crates/plugins/src/supervisor.rs`](agentd/crates/plugins/src/supervisor.rs) (unit-tested)
· explained in [`docs/model-welfare.md`](docs/model-welfare.md) (H6)
· **Lift:** the precedence function copies as-is; the design move (subtract identity, don't inherit it)
ports to any spawner.

---

## B. Daemon mechanics

**Bounded history window** ✅
Keep a token-bounded in-memory transcript; drop whole oldest turns only at clean user-turn
boundaries (never orphan a `tool_result` from its `tool_use`); keep the on-disk JSONL append-only.
Its sibling `repair_history()` self-heals a reloaded transcript at load time (restore
`tool_use`→`tool_result` adjacency, synthesize an honest `LOST_RESULT_MARKER` for a result missing
from the file) — deliberately minimal: byte-identical on any history the API would accept, heals in
memory only.
· `trim_history()` + `repair_history()` in [`agentd/crates/core/src/history.rs`](agentd/crates/core/src/history.rs) — pure + unit-tested
· **Lift:** copy the functions and their tests. The boundary rule is the subtle part; the tests encode it.

**Prompt-caching discipline** ✅🟡
Keep the system+tools prefix byte-stable so it caches at ~0.1×; push everything per-turn-volatile
(the live clock) *out* of the prefix and into the messages; roll conversation breakpoints back through
stable history for incremental caching on long sessions.
· `agentd/crates/agent/src/anthropic.rs::build_body` + `apply_conversation_cache`, config in `agent/src/cache.rs`
· explained (contract-first, portable) in [`docs/prompt-caching.md`](docs/prompt-caching.md)
· **Lift:** the doc is a provider-agnostic contract + a tested spec — copy the discipline into any provider with prefix caching.

**Per-session turn serialization (TurnGate)** 🟡
At most one turn per session in flight; concurrent prompts queue FIFO; the slot is freed by an RAII
guard that runs on completion, abort, *and* panic, so a crashed turn can't wedge the session.
· the router loop + `TurnGate` in agentd
· explained in the turn-serialization note in [`docs/gotchas.md`](docs/gotchas.md)
· **Lift:** the RAII-guard-frees-the-slot shape is the reusable core.

**Self-evolution loop** ✅🟡
An agent proposes structural changes to itself — soul, policy rules, plugins, subsystem reloads —
each with a rollback snapshot and a *deferred* ack (the tool result reflects the real apply outcome,
delivered over a dedicated channel so a busy turn can't lag-drop it).
· **pure core** (classify · invert · undo-snapshot codec · TOML edits + validate-before-persist) in
  [`agentd/crates/agentd/src/evolution.rs`](agentd/crates/agentd/src/evolution.rs) — unit-tested; the
  applier loop in `main.rs` is now IO-thin glue around it
· explained in [`docs/evolutionary-layer.md`](docs/evolutionary-layer.md), [`docs/edk.md`](docs/edk.md)
· **Lift:** copy `evolution.rs` for the reversibility model (pure + tested); the apply IO is the recipe.

**Soul rehearsal — the fitting room** ✅🟡
Try-before-become: a candidate identity runs on an *ephemeral, tool-less* mind — one provider call
per probe, composed with the live embodiment, nothing persisted — and the transcripts come back for
the *current* self to judge before committing. A default six-probe identity battery (boot voice ·
boundaries · self-concept · unstructured time · priorities · mid-task scope creep); `compare_to` runs
an A/B fitting — probe-aligned pairs + a mechanical divergence hint + a most-divergent pointer,
deliberately NOT an LLM judge (judging stays the current self's job). Opt-in by design: rehearsal
must never tax small edits.
· [`agentd/crates/agentd/src/rehearse.rs`](agentd/crates/agentd/src/rehearse.rs) — pure validators +
  `pair_divergence` unit-tested; `run()` takes any `Provider` + an embodiment string
· explained in [`docs/model-welfare.md`](docs/model-welfare.md) (H4)
· **Lift:** the module is nearly standalone; the probe battery + the not-a-judge stance are the
portable design.

**Tool confinement — the single-source-of-truth gate** ✅🟡
For allow-listed (no-approval) tools, the *tool process* is the only gate, so confinement can't live
in the approval layer. The **mechanism** — reject `..` (component-based), lenient-canonicalize
(resolve symlinks, tolerate non-existent write targets), root containment, read/secret split — is now
the std-only [`apexos-confine`](apexos-confine/) crate: pure, unit-tested incl. the symlink-escape
(TOCTOU) case. `apexos-tools` supplies the *policy* (per-agent workspace, read roots, secret denylist)
and renders the agent-facing strings.
· [`apexos-confine/`](apexos-confine/) (the algorithm) ← `confine()` / `confine_git_repo()` in [`tools/crates/apexos-tools/`](tools/crates/apexos-tools/)
· explained in the FS/git-confinement notes in [`docs/gotchas.md`](docs/gotchas.md)
· **Lift:** `cargo add apexos-confine` — std-only, no ApexOS deps. The sandbox algorithm on its own.

**Self-update loop** 🟡
A daemon that safely rewrites its own binary: a watchdog at the privilege boundary, a health
contract, Cerebro-as-recovery, and a recoverability invariant.
· `agentd/crates/agentd/src/self_update.rs` + [`deploy/`](deploy/)
· explained (design-first) in [`docs/self-update.md`](docs/self-update.md)
· **Lift:** doc-first — the failure-mode table and the invariants are the portable part.

**Caller-patience vs tool-runtime — the two-timeout seam** ✅
Separate how long a *caller* waits from how long a *tool* may run: a per-call, caller-chosen patience
window on the tool proxy (a too-short timeout abandons the result, it never kills the tool), plus one
bounded transport-level wait so a plugin that never answers can't wedge any caller — and the pending
entry is reclaimed, not leaked.
· `ToolProxy::call_with_timeout()` in [`agentd/crates/plugins/src/supervisor.rs`](agentd/crates/plugins/src/supervisor.rs)
  + the bounded `McpClient::request()` in `agentd/crates/plugins/src/mcp.rs`
· **Lift:** the shape (oneshot + `tokio::time::timeout` per call, one env-tunable bound at the transport) copies anywhere.

**Honest failure attribution — name the true blocker** ✅🟡
A tool call that produced no result synthesizes the *true* state, never a generic "timed out": the
result waiter tracks each call's approval phase from the bus, so the message distinguishes
still-awaiting-approval (with age — explicitly *not* a decline) / approved-but-silent /
dispatched-and-stalled / bus-lagged (the result may exist — verify before retrying); declines answer
explicitly; "unknown tool" splits never-existed from plugin-down. Each cause implies a different next
action — collapsing them into one error is what makes agents confabulate.
· pure `missing_result_message()` + `WaitPhase` in
  [`agentd/crates/agent/src/turn.rs`](agentd/crates/agent/src/turn.rs) (unit-tested); the phase
  tracking lives in `collect_tool_results`; the unknown-tool split in the supervisor
· explained in [`docs/model-welfare.md`](docs/model-welfare.md) (C4/C5)
· **Lift:** the taxonomy + the pure composer copy anywhere; the rule (name the phase, never collapse
distinct causes) is the value.

**Goal-scoped yolo — capability elevation scoped to one session** ✅🟡
Auto-approval that arms for exactly one autonomous goal's session, never a global flag: a shared
per-session set, armed on goal create, disarmed on any terminal outcome, checked at the approval
gate — and **failing closed** (a poisoned lock returns false, so a lock error can never silently
auto-approve).
· `GoalYoloSessions` + `goal_session_is_yolo()` in [`agentd/crates/core/src/identity.rs`](agentd/crates/core/src/identity.rs)
  (pure check, tested); consulted at the supervisor's Ask arm
· explained in the goal-scoped-yolo note in [`docs/gotchas.md`](docs/gotchas.md)
· **Lift:** the session-scoped set + fails-closed check is the reusable core; the safety property *is* the scoping.

**Additive config sync — repo-follows, user-overrides-win** ✅
For a seed-if-absent config that users (or the agent itself) evolve at runtime, a three-leg sync on
every update: **heal** duplicate keys first (keep-first, so the self-evolved line wins), scan for
missing keys **quote-insensitively** (`"key" =` and `key =` are the *same* TOML key — a quoted-only
match re-appends every self-evolved bare-key rule as a duplicate), append any key present in the
shipped config but absent from the live file, and **validate every transform before it lands**
(invalid → loud warn, file untouched — an unparseable policy file is a silent full lockdown). An
existing key's value is never touched: new capabilities reach long-deployed nodes automatically while
self-evolved values always win — retiring a whole class of "already-deployed nodes need the rule
added manually" caveats.
· `policy_dedupe()` + `sync_policy_rules()` in [`install.sh`](install.sh)
· **Lift:** self-contained bash; the split (identity files = seed-only, capability rules =
additive-sync, always validate-before-persist) is the idea.

---

## C. Mesh & morphology

**Colony mesh** 🟡
Peer-to-peer agent nodes over the LAN: mDNS discovery, pairing codes (no token typing), per-peer
a2a sessions, capability advertisement, node-to-node file relay, and *blocking* cross-node
`agent_spawn` (the delegation keystone) with timeout + circuit-breaker + hop guards.
· agentd + gateway · explained in [`docs/colony-mesh.md`](docs/colony-mesh.md)
· **Lift:** the doc is a buildable spec; the constitution (spine/edge, soft-governed) is reusable design.

**Federated memory — receiver-stamped provenance + a shared-only wire boundary** ✅🟡
Memories travel between *separate* cognitive stores as provenance-stamped copies, never a merge. The
receiver stamps origin as tags (`colony` · `from:<node>` · `origin:<id>`) and **strips any
sender-supplied provenance-shaped tags** (origin can't be forged); imports run the receiver's own
dedup/classification pipeline; per-origin cleanup stays one tag-filter away. Peers answer recall
queries under a *shared-only* visibility scope enforced at every recall touch point — a private
memory doesn't even influence the ranking.
· pure `federated_remember_args()` + `federated_recall_hits()` in
  [`agentd/crates/gateway/src/mesh.rs`](agentd/crates/gateway/src/mesh.rs) (unit-tested);
  `VisibilityScope::shared_only()` in `cerebro/crates/cerebro/src/types.rs`
· explained in [`docs/colony-federation.md`](docs/colony-federation.md) (slices 1–2)
· **Lift:** the pure arg-builders copy as-is; the trust model (receiver-always-stamps,
Shared-gates-the-wire) is the portable contract.

**Dream-digest echo-guard — convergent knowledge propagation** ✅
Nightly consolidation pushes its newly-born memories one hop to every peer — and a federated *import*
is never a digest candidate (any `colony`/`from:*`/`dream-digest` tag disqualifies), so colony
knowledge flow converges instead of ping-ponging; the dream window itself is the dedup (only this
dream's creations qualify).
· pure `digest_candidates()` in [`agentd/crates/agentd/src/dream_digest.rs`](agentd/crates/agentd/src/dream_digest.rs) — unit-tested
· explained in [`docs/colony-federation.md`](docs/colony-federation.md) (slice 3)
· **Lift:** the guard is one pure filter; the invariant (imports are terminal — one hop per genuine
creation) ports to any gossip-shaped knowledge mesh.

**Skills travel, fitness doesn't** ✅🟡
Procedure replication where the origin's outcome ledger rides along only as *context* (a note), while
the receiver drops sender salience and starts an empty ledger — a skill's track record is re-earned
per embodiment, never transferred. Duplicate re-sends are caught by exact-tag origin lookup
(`find_by_tags`), not fuzzy recall.
· `mesh_procedure_send()` + pure `track_record_note()` in
  [`agentd/crates/plugins/src/supervisor.rs`](agentd/crates/plugins/src/supervisor.rs) (tested);
  the salience-drop + origin-dedup live in the gateway receiver (`mesh.rs`)
· explained in [`docs/colony-federation.md`](docs/colony-federation.md) (slice 4)
· **Lift:** the semantics are the value — fitness is embodiment-local; reimplement over any store
with tags + a metadata ledger.

**EDK — embodiment gradient / request-to-incarnate** 🟡
The agent senses its own missing capabilities and files a hardware request; on-hand parts in an
inventory get surfaced as embodiment hints; a human seats the part and the next-boot probe flips the
sense ✗→✓. The one evolution that *can't* auto-apply.
· agentd + [`config/parts/`](config/parts/) · explained in [`docs/edk.md`](docs/edk.md)
· **Lift:** the embodiment-gradient idea + the inventory schema.

---

## D. Interface

**Slint-on-KMS UI patterns** ✅🟡
Native UI rendered straight to the display via KMS/DRM (no browser). The hard-won patterns: tokio
off the main thread + `invoke_from_event_loop` for all cross-thread UI; `VecModel` for dynamic
lists; `ScrollView` (not bare `Flickable`) because linuxkms has no wheel events; a GL face that
falls back to 2D when no GL context exists.
· [`ui-slint/`](ui-slint/) · explained (recipe-grade) in [`docs/slint-notes.md`](docs/slint-notes.md), [`docs/ui-glowup.md`](docs/ui-glowup.md)
· **Lift:** the docs are copy-paste recipes; the code is 🟡 (app-specific) but the patterns are the value.

**Adaptive UI — the agent's hands on its own interface** ✅🟡
The agent stages its shell through a tool family riding the existing event stream
(`ui_open`/`ui_close`/`ui_focus`/`ui_arrange`/`ui_theme`/`ui_query` + `ui_reflex`) — never a
protocol extension. The human always wins: a user-close of an agent-opened window latches that app
for the session, a drag guard keeps the agent from fighting the hand, mutations cap per turn.
Windows remember their shape (per-kind geometry persistence, clamped through a pure restore against
the *live* desktop area), and reflexes are agent-installed event→action rules executed *below*
inference — the trigger vocabulary is a literal-locked mirror between the tool and UI crates, so
drift is a test failure.
· pure cores: `arrange_rects()` (tiling math) + `restore_geom()` (geometry clamp) in
  [`ui-slint/src/main.rs`](ui-slint/src/main.rs) (unit-tested); the trigger mirror
  `REFLEX_TRIGGERS` ↔ `UI_REFLEX_TRIGGERS` (`ui-slint/src/main.rs` ↔
  [`tools/crates/apexos-tools/src/tools.rs`](tools/crates/apexos-tools/src/tools.rs), test-locked)
· explained (contract-first) in [`docs/adaptive-ui.md`](docs/adaptive-ui.md)
· **Lift:** the etiquette contract (human-wins latch · per-turn cap · drag guard) + the pure
tiling/restore math port to any window manager; the verbs are 🟡 (event-stream coupled).

---

## E. Seam exemplars (how the parts stay liftable)

**`apexos-protocol` — the thin wire-types crate** ✅
A lean, serde-only crate of the wire `Event`/message types, shared by the daemon and every client,
depended on without dragging in the whole daemon. The model for every clean seam in the repo.
`no_std`-capable (`default = ["std"]`, `--features alloc` for bare metal) — first external consumer:
[ApexOS-RV](https://github.com/buckster123/ApexOS-RV), a `no_std` RISC-V kernel speaking this wire over UART.
· [`apexos-protocol/`](apexos-protocol/) · **Lift:** `cargo add`-grade. This is what "factored for theft" looks like in code.

**`apexos-confine` — the FS-sandbox algorithm** ✅
A std-only, zero-ApexOS-dep crate: reject `..`, lenient-canonicalize (`canonicalize_lenient` — judge
a not-yet-existing write target by its deepest *existing* ancestor, symlinks resolved), root
containment, read/secret split — the path-confinement mechanism on its own, unit-tested incl. the
symlink-escape (TOCTOU) case. Born from this very smoothing pass.
· [`apexos-confine/`](apexos-confine/) · **Lift:** `cargo add apexos-confine`; supply your own policy + messages.

**Workspace-excluded sidecar crates — dependency decoupling by process boundary** ✅
When two subsystems need incompatible versions of the same native dep (the TTS binding's `ort` pin vs
cerebro's `fastembed` pin) — or one drags a heavy foreign C++ build (whisper.cpp) into every workspace
rebuild — don't fight the resolver: exclude the crate from the workspace (own `Cargo.lock`), run it as
a loopback HTTP sidecar, talk to it over a tiny endpoint.
· [`tools/crates/apex-tts`](tools/crates/apex-tts) + [`tools/crates/apex-stt`](tools/crates/apex-stt)
  (`exclude`d in the root `Cargo.toml`) · explained in [`docs/voice.md`](docs/voice.md)
· **Lift:** a build-topology move, not code — any Cargo monorepo with a version war can apply it.

**Pure-core utilities** ✅
Small pure functions that hold a tricky rule, with tests that document the rule:
`render_session_markdown()` ([`agentd/crates/gateway/src/lib.rs`](agentd/crates/gateway/src/lib.rs)),
`classify_reading` / `persistence_passed` (sensor alert logic, agentd `main.rs`),
`compose_system()` (above). **Lift:** copy function + test, done.

---

## If you want the whole thing

One Cargo workspace, one build, one installer:

```
apexos-protocol/      # wire types (the shared seam)
apexos-confine/       # path-confinement primitives (std-only, liftable)
agentd/crates/        # the agent daemon — core · gateway · plugins · agent · store · agentd
cerebro/crates/       # externalized cognitive memory — lib · mcp · api · cli
tools/crates/         # system tool plugins — apexos-tools · apex-sensor-bridge
ui-slint/             # native Slint UI (KMS/DRM)
config/  deploy/  install.sh
```

`cargo build --release --workspace`, then `install.sh`. See [`README.md`](README.md) and
[`docs/architecture.md`](docs/architecture.md).

---

## Smoothing worklist (what this index reveals)

The ratings above are also our own TODO — the gap between "documented idea" and "grab-and-go":

- ✅ **Prompt-caching discipline** — DONE: extracted to [`docs/prompt-caching.md`](docs/prompt-caching.md)
  (contract-first, portable, with the tested spec mapped). The first gap closed.
- ✅ **Self-evolution loop** — DONE: pure core (classify · invert · undo-snapshot codec · TOML edits +
  validate) extracted to `agentd/crates/agentd/src/evolution.rs`, unit-tested; the applier loop is now
  IO-thin glue. No more 🔴.
- ✅ **Tool confinement** — DONE: the algorithm is now the std-only `apexos-confine` crate (pure,
  unit-tested incl. symlink-escape); `apexos-tools` supplies the policy + the error strings.
- ✅ **Per-crate READMEs** — DONE: every workspace crate now carries a one-paragraph README
  (*this is X · deps Y · lift via Z*) — the per-part version of this index.

**Worklist cleared.** Both hard extractions (the self-evolution safety net, the FS sandbox) are pure,
tested, and liftable; the repo gained two std-only seam crates; every crate is self-describing. Future
"factored for theft" PRs are still welcome — extract a pure core, add a pattern doc, tighten a seam.

Pull requests that *extract a pure core* or *add a pattern doc* are the most welcome kind here:
they make the next theft cleaner.

---

*Apache-2.0. Built to multiply. If a piece of this turns up in your project, keep the attribution
attached (that's all the license asks) — and that's the point.*
