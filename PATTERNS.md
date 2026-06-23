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
· lives in [`docs/pac.md`](docs/pac.md) + a reproducible benchmark in [`docs/pac-bench/`](docs/pac-bench/)
· **Lift:** the whole thing is a self-contained doc + corpus + harness; reimplement in any language.
The exemplar for "factored for theft" — an idea you can take without taking any code.

**Soul / embodiment / priming — the layered system prompt** ✅🟡
Separate the *portable identity* (soul, hand-authored) from the *live body* (embodiment,
machine-regenerated each turn from the node's actual state) from *per-session boot priming*. The
join is a pure function.
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

**CCBS — daemon-driven cognitive boot** 🟡
The *daemon* (not the agent's memory) injects orientation on a session's first turn: it calls
`cognitive_bootstrap` and appends the result as priming, so the agent wakes already oriented without
having to *remember* to orient.
· `root_turn` + `TurnEngine::with_priming` (agentd + `turn.rs`)
· explained in [`docs/agent-identity.md`](docs/agent-identity.md) (slice 2)
· **Lift:** reimplement from the contract — the value is the *inversion* (boot is pushed, not pulled).

---

## B. Daemon mechanics

**Bounded history window** ✅
Keep a token-bounded in-memory transcript; drop whole oldest turns only at clean user-turn
boundaries (never orphan a `tool_result` from its `tool_use`); keep the on-disk JSONL append-only.
· `trim_history()` in [`agentd/crates/core/src/history.rs`](agentd/crates/core/src/history.rs) — pure + unit-tested
· **Lift:** copy the function and its tests. The boundary rule is the subtle part; the tests encode it.

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
· explained in the turn-serialization note in [`CLAUDE.md`](CLAUDE.md)
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

**Tool confinement — the single-source-of-truth gate** ✅🟡
For allow-listed (no-approval) tools, the *tool process* is the only gate, so confinement can't live
in the approval layer. The **mechanism** — reject `..` (component-based), lenient-canonicalize
(resolve symlinks, tolerate non-existent write targets), root containment, read/secret split — is now
the std-only [`apexos-confine`](apexos-confine/) crate: pure, unit-tested incl. the symlink-escape
(TOCTOU) case. `apexos-tools` supplies the *policy* (per-agent workspace, read roots, secret denylist)
and renders the agent-facing strings.
· [`apexos-confine/`](apexos-confine/) (the algorithm) ← `confine()` / `confine_git_repo()` in [`tools/crates/apexos-tools/`](tools/crates/apexos-tools/)
· explained in the FS/git-confinement notes in [`CLAUDE.md`](CLAUDE.md)
· **Lift:** `cargo add apexos-confine` — std-only, no ApexOS deps. The sandbox algorithm on its own.

**Self-update loop** 🟡
A daemon that safely rewrites its own binary: a watchdog at the privilege boundary, a health
contract, Cerebro-as-recovery, and a recoverability invariant.
· `agentd/crates/agentd/src/self_update.rs` + [`deploy/`](deploy/)
· explained (design-first) in [`docs/self-update.md`](docs/self-update.md)
· **Lift:** doc-first — the failure-mode table and the invariants are the portable part.

---

## C. Mesh & morphology

**Colony mesh** 🟡
Peer-to-peer agent nodes over the LAN: mDNS discovery, pairing codes (no token typing), per-peer
a2a sessions, capability advertisement, node-to-node file relay, and *blocking* cross-node
`agent_spawn` (the delegation keystone) with timeout + circuit-breaker + hop guards.
· agentd + gateway · explained in [`docs/colony-mesh.md`](docs/colony-mesh.md)
· **Lift:** the doc is a buildable spec; the constitution (spine/edge, soft-governed) is reusable design.

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

---

## E. Seam exemplars (how the parts stay liftable)

**`apexos-protocol` — the thin wire-types crate** ✅
A lean, serde-only crate of the wire `Event`/message types, shared by the daemon and every client,
depended on without dragging in the whole daemon. The model for every clean seam in the repo.
· [`apexos-protocol/`](apexos-protocol/) · **Lift:** `cargo add`-grade. This is what "factored for theft" looks like in code.

**`apexos-confine` — the FS-sandbox algorithm** ✅
A std-only, zero-ApexOS-dep crate: reject `..`, lenient-canonicalize (resolve symlinks, tolerate
non-existent write targets), root containment, read/secret split — the path-confinement mechanism on
its own, unit-tested incl. the symlink-escape (TOCTOU) case. Born from this very smoothing pass.
· [`apexos-confine/`](apexos-confine/) · **Lift:** `cargo add apexos-confine`; supply your own policy + messages.

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
