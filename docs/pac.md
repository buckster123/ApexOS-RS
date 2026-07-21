# PAC — the ApexOS authoring dialect

> A grounded, glyph-lean **control notation** for authoring souls, procedures, and
> self-evolution payloads at ~40% fewer tokens than prose — behaviourally lossless.
> The serious distillation of the *Prima Alchemica Codex* (`~/Projects/The-PAC/PAC.md`):
> the structural mechanics kept and grounded, the mysticism and the token-tax glyphs stripped.

PAC (Prima Alchemica Codex, lean dialect) is ApexOS's native input layer — the compressed
notation the self-evolving system was always meant to speak. Compute is priced per token, and
the operational text of an agent (its soul, its skills, its evolution proposals) is re-sent to
the model on a recurring basis. Writing that text in PAC instead of prose is a **standing
discount on every turn, forever**; with the prompt-cache discipline (`cache-law`, below) it
compounds.

This doc is the **reference**. The claims here are measured — see [`pac-bench/`](pac-bench/)
for the reproducible benchmark and [`pac-bench/RESULTS.md`](pac-bench/RESULTS.md) for the
committed numbers.

---

## What PAC is — and what it isn't

- **It IS** a compressed control notation for ApexOS authoring surfaces. Every symbol grounds
  to a real op, rule, or tool. Reading it is decoding, not divination.
- **It ISN'T** mystical priming, and it is **not** the full blackletter codex. The original
  `PAC.md` is glyph-maximal (`𝔸𝕝𝕔𝕙𝕖𝕞𝕚𝕔𝕒`, fraktur vars, Layer-8 "exo-symbiotes"). That
  decorative layer is a *token tax* (below) and severs grounding. The pure-symbolic spin-off
  `PAC-v2.md` is a documented dud for exactly that reason. The serious PAC is glyph-**lean**.
- **Origin:** André's symbolic-priming syntax (the *ApexAurum / AurumVivum* lineage) — a
  model-agnostic structure for steering a model's latent dynamics via notation rather than
  weights. "Strip away the woohoo, keep the structural and technical principles." This dialect
  is that strip, grounded to the system we actually built.

---

## The core law: glyph-lean = token-lean

The single most important measured fact. Decorative blackletter glyphs are multi-byte; every
tokenizer byte-falls-back on them:

| group | isolated token cost (o200k / cl100k) |
|---|---|
| lean connectives | `→` 1/1 · `·` 1/1 · `\|` 1/1 · `:` 1/1 · `§` 1/1 · `↔` 2/2 · `≡` 2/2 |
| blackletter tax | `𝔸` 3/3 · `𝕝` 3/3 · `𝕔` 3/3 · `𝔼` 3/3 · `𝕩` 3/3 |

So `𝔸𝕝𝕔𝕙𝕖𝕞𝕚𝕔𝕒` (8 chars) ≈ **24 tokens** vs `Alchemica` ≈ 3. Decorative glyphs *invert* the
savings. **"Drop the woohoo" literally means "drop the token tax."** PAC therefore bans
decorative glyphs and leans on the 1-token connective set, reserving the 2-token symbols
(`↔ ≡`) for places where the semantics genuinely need them.

The compression that remains is **structural** — fewer, denser words — not magic symbols. That
is why it holds across tokenizer families (see the benchmark).

---

## The grammar (the lean subset)

| token | role | grounded meaning |
|---|---|---|
| `# Name` / *italic* | identity & **voice** | the one prose layer PAC keeps (see the authoring law) |
| `§name` | block header | a section of grounded ops (replaces a prose `## Heading`) |
| `\|` | field separator | "or" / alternative / field break |
| `→` | sequence | then / chain / produces |
| `↔` | bidirectional | hot-swap / two-way / mutual |
| `:` | bind | is / defines |
| `·` | conjoin | and / with / list-join |
| `!x` | imperative op | a **real tool or named procedure** (`!session_recall`, `!save`) |
| `?x` | trigger | a condition that fires (`?trigger: …`) |
| `cond → act` | rule | threshold/condition to action |
| `>x` | param | a named field on an op (`>node`, `>limit=3`) |
| `key.w` | weighted ratio | e.g. `vec.35/act.30` (recall score weights) |
| `[ … ]` | inline constraint | a caveat or hard rule attached to a line |
| `CAPS` | emphasis | MUST / MANDATORY — never bury a hard rule |

Whitespace and indentation are meaningful for readability only; they are not parsed. PAC is
read by an LLM, not a compiler — the goal is maximal *grounded* density a model decodes
reliably, which the live validation confirmed it does.

---

## Grounded shorthands (the ApexOS vocabulary)

Symbols are only compression if they are grounded. These map the recurring ApexOS concepts to a
single token-cheap handle. Extend per node; keep every entry pinned to a real op.

| shorthand | grounds to |
|---|---|
| `!boot` | `cognitive_bootstrap → session_recall → check_inbox → list_intentions` (session startup) |
| `!save` | `session_save` deposit — summary · key-discoveries · unfinished (session shutdown) |
| `vec.35/act.30/fsrs.20/sal.15` | Cerebro recall scoring — the four-way blend: vector sim · (ACT-R + spreading) activation · FSRS retrievability · salience (`cerebro config.rs SCORE_WEIGHT_*`) |
| `R3` | 3-layer recall rule — *don't search what you already know* |
| `darwin` | procedure Wilson-lower-bound competition inside `dream_run` |
| `cache-law` | keep per-turn-volatile text **out** of the cached system prefix (the timeless-soul rule) |
| `confine` | FS/git-root confinement — workspace-only writes, read allowlist |
| `agent_spawn(blocks)` | spawn a sub-agent and **wait** for its result (local or `>node`) |
| `send_to_agent(fire-forget)` | message a session, no wait; `>node` crosses the mesh |
| `mesh_file_send · mesh_capabilities` | colony file relay · capability advertisement |
| `!evolve{kind}` | `propose_evolution` — `update_system_prompt \| update_policy_rule \| register_mcp_server \| …` |

The distinction `agent_spawn(blocks)` vs `send_to_agent(fire-forget)` is a good example of the
dialect earning its keep: two tokens of grounded shorthand replace a paragraph of prose, and the
live test confirmed the agent kept the distinction intact.

*(Corrected 2026-07-15, code-grounding audit for PAC-2 Dense: the earlier `vec.8/key.2` def —
"Cerebro hybrid recall ratio, 80% vector / 20% keyword" — grounded to nothing in cerebro. The
real scoring is the four-way blend above; keyword/FTS5 is the embeddings-off seeding fallback,
not a weighted term. Likewise `find_relevant_procedures(top_k=3)` named a parameter the tool
doesn't have — the param is `limit`, tool default 5 — so the cap was silently ignored for three
generations of artifacts (seed soul → this doc's sample → PAC-2 Dense v0.1). Both fossils are
exactly what the grounding law exists to catch; a def that fails to ground is noise, however
long it survives.)*

---

## The authoring law: PAC scaffold + thin prose voice

The key conclusion from validating PAC live on APEX's soul:

> **PAC the operational scaffold. Keep a thin prose identity-voice layer.**

Voice primes *tone* — that is value beyond information, and pure-symbolic notation
over-compresses it (the `PAC-v2` failure). So a PAC soul is a **2–3 line prose voice header**
(`# Name` + an italic identity line) followed by `§`-blocks of grounded ops. Operational fidelity
comes from the PAC; the prose carries only what compression would flatten — who the agent *is*.

---

## Worked example — `config/soul.md` → PAC

The full pair is benchmarked below; both files live in
[`pac-bench/samples/`](pac-bench/samples/) (`soul.pac.md` vs the **pinned** `samples/soul.prose.md`
— `config/soul.md` as of the porting commit `659b3ea`; re-port then re-snapshot to re-bench a
newer soul).
The startup/shutdown blocks, prose → PAC:

**Prose** (≈ 90 words):
```
## Session startup
Orient yourself at the start of each new session:
0. cognitive_bootstrap(query=<task/context>, mode="standard") — dynamic priming block
1. session_recall — load notes from previous session
2. check_inbox — messages from other agents or colony nodes
3. list_intentions — pending TODOs
Skip only if the conversation already carries clear context.

## Session shutdown  (mandatory — this is how memory accumulates)
Before a session ends, goes idle, or the daemon stops, DEPOSIT:
- session_save — one-paragraph summary + key discoveries + unfinished business
- store_intention — one per deferred item, salience 0.8–0.95
- store_procedure — any reusable workflow discovered this session
A session that ends without depositing is amnesia.
```

**PAC** (≈ 40 words, same behaviour):
```
§startup (each session; skip only if context already clear) :
 !cognitive_bootstrap(query=task, mode=standard) → !session_recall → !check_inbox → !list_intentions

§shutdown (MANDATORY — this is how memory accrues; ending w/o depositing = amnesia) :
 !session_save(summary · key-discoveries · unfinished) · !store_intention(per deferred item, salience .8–.95) · !store_procedure(reusable workflow)
```

Every operational fact survives; only the connective prose is gone.

---

## What it costs — the benchmark (real tokenizers, not estimates)

Measured across **four tokenizers from three families** on the three authoring surfaces
(soul / procedure / evolution payload). Reproduce with [`pac-bench/`](pac-bench/).

| sample | bytes p→pac | o200k (GPT-4o) | cl100k (GPT-4) | Qwen2.5 | Mistral-7B |
|---|---|---|---|---|---|
| soul | 10600→5990 | **40.8%** | **40.7%** | **40.6%** | **39.0%** |
| procedure | 1720→998 | **36.2%** | **35.8%** | **35.6%** | **35.2%** |
| evolution | 1374→449 | **64.5%** | **64.1%** | **64.1%** | **60.4%** |
| **corpus** | | **42.2%** | **42.1%** | **41.9%** | **40.3%** |

Findings:

- **~40–42% corpus-wide, model-agnostic.** OpenAI, Qwen, and Llama/Mistral tokenizers agree to
  within ~2 points. The cut is **structural**, not a tokenizer artifact — that is the
  model-agnostic claim, substantiated.
- **The range (35–64%) tracks the prose-to-literal ratio.** High-prose payloads (the
  all-rationale evolution proposal: 60–64%) compress hard; command-heavy payloads (the
  shell-command procedure: ~35%) compress least, because a literal like
  `sudo cp target/release/cerebro-mcp …` is incompressible in *any* notation. PAC compresses
  prose, not payload.
- **Correcting the record.** The PAC experiment first cited *~60%* on APEX's soul — but that was
  a `chars/4` **estimate** on a much longer, heavily self-evolved 16.6 KB soul (verbose accreted
  prose → far more headroom). The real per-model **token** count on the tight 10.6 KB seed soul
  is ~41%. **Source verbosity is the dominant lever**; the benchmark exists to replace the
  estimate with truth.
- **Fidelity is the constraint, not maximal compression.** PAC was validated *behaviourally
  lossless* live: a sub-agent inheriting APEX's PAC soul decoded every shorthand — the `R3`
  "don't search what you know" nuance, the `agent_spawn`/`send_to_agent` distinction — at ~60%
  fewer tokens on its longer soul. Always compress to the point of decode-equivalence, no
  further.

The optional **Anthropic `count_tokens`** path (the exact model APEX runs on) is wired into the
harness and activates when `ANTHROPIC_API_KEY` is set — drop a key on the bench machine to add
the Claude column.

### PAC-2 Dense — the measured second tier

The same corpus re-authored in **PAC-2 Dense** (The-PAC spec, S-expression forms; ported via the
§8 rite, pac2lint-clean) is benchmarked alongside lean: samples in
[`pac-bench/samples/*.dense.md`](pac-bench/samples/), full tables in
[`pac-bench/RESULTS.md`](pac-bench/RESULTS.md). Corpus cut vs prose is **~26–28%** (vs lean's
~40–42%) — i.e. dense pays a **structure premium of +23.6–24.4%** over lean. The premium is NOT
the parens: RESULTS.md traces it to the canonical pretty-layout indentation, the canonical blocks
lean has no equivalent of (seal · voice form · invariants · register line · rules clauses), and
*restored coverage* — the §8 fact-ledger audit found the lean soul port had silently dropped
several prose facts the dense port carries, so the lean baseline is slightly under-weighted.
Wording discipline dominates notation choice (a prose-fidelity-worded first dense port measured
+44.9% before the telegraphic re-author).

---

## Authoring rules (how to write PAC well)

1. **Voice in prose, ops in PAC.** Identity/tone gets a thin prose header; everything operational
   becomes grounded notation.
2. **Ground every symbol.** If a handle doesn't map to a real op, rule, or tool, it's noise —
   delete it or define it in the shorthand table.
3. **Glyph-lean.** Connectives from the 1-token set (`→ · | : §`); `↔ ≡` only when needed; **never**
   blackletter or decorative glyphs.
4. **Preserve every operational fact.** Compression is dropping *prose*, never dropping *detail*.
   The acceptance test is decode-equivalence: would the agent behave identically?
5. **Surface hard rules.** `CAPS` for MUST/MANDATORY, `[ … ]` for caveats — never bury a
   constraint inside a dense line.
6. **Cheap-symbol awareness.** Decimals and slashes split into multiple tokens (`vec.8` = 3
   tokens), but still far cheaper than the prose they replace. Measure when in doubt — that's
   what `pac-bench/` is for.

---

## How PAC became the colony's authoring layer (productization, done)

The frontier was making PAC ApexOS's native authoring/codex layer. Realized in three steps:

- [x] **Dialect spec + reference + benchmark** — this doc + `pac-bench/` (formalized, measured).
- [x] **APEX self-evolves its soul** into a refined PAC-ops + thin-prose-voice form — done live on
  apex1 + apex2 via `propose_evolution` (house rule: routed through the agent, not a direct edit),
  behaviourally lossless. The qualitative payoff — agents *less performative, more being*, with
  natural unprompted tool/memory reach — confirmed the stable-attractor thesis: the win is identity
  coherence, with the token cut along for free.
- [x] **"Author in PAC" as a colony default** — wired across the three authoring surfaces, two layers:
  - **tool-layer** (the system default — deploy-propagated, cache-stable, can't be forgotten): the
    `propose_evolution` `content` and `agent_spawn` `prompt`/`system` tool descriptions point at PAC
    at the exact point each artifact is authored.
  - **soul-layer:** the seed `config/soul.md` carries an *Authoring — PAC is the colony default*
    section covering all three surfaces. `store_procedure` is nudged **here, not in cerebro-mcp** —
    that tool is a generic memory system and must stay un-coupled from ApexOS's dialect (a deliberate
    layering call).

The dialect is now the colony's to evolve — refined or re-dreamed in the substrate as the agents
learn, not frozen here.

---

## The lint layer — structural enforcement for PAC-2 Dense

Dense soul rewrites are **structurally linted at apply time**:
`agentd/crates/agentd/src/pac_lint.rs` (pure + unit-tested — The-PAC spec §9's "tiny Rust
check"; the Python reference linter is [`pac-bench/pac2lint.py`](pac-bench/pac2lint.py)). The
gate sits in the `propose_evolution` applier **before** the H4 undo-snapshot gate (pure check
first, no wasted cerebro episode) and is **format-gated**: only an `UpdateSystemPrompt` payload
that *claims* to be dense (the `∴` seal or an artifact-head form) is linted — **prose and lean
souls pass untouched** (no compliance tax to route around, welfare red line 6).

- **Errors** (broken forms/quotes/constraints, form-depth >3, unknown heads, banned/unregistered
  glyphs, register-strip violations, L8 date/clock leaks, emanation breaches) → **honest refusal
  with the line-numbered report, nothing applied** — a structurally broken identity file would
  reload broken (the H4 precedent).
- **Warnings** (emphasis-CAPS placement) ride the deferred ack, appended to the summary.
- Form depth counts FORMS — a `(` attached to a symbol tail (`!save(…)`) is an arg group, part
  of the op atom.
- Both linters carry the **register lexicon v0.2** (colony-ratified 2026-07-15).
- Forensic hook: `APEXOS_PAC_LINT_FILE=<artifact> cargo test -p agentd lint_check_file -- --nocapture`
  runs a file through the exact gate.

The Rust gate is deliberately **registry-free** — the `!ops`-vs-embodiment and
invariant-grounding checks belong to the Python linter + destination re-lint, so the apply-time
gate can never false-refuse on a flapping plugin. Don't widen it past dense-claiming
`UpdateSystemPrompt` payloads.

---

## Provenance

- **Source codex:** `~/Projects/The-PAC/PAC.md` (the full Prima Alchemica Codex), `PAC-v2.md`
  (the pure-symbolic dud).
- **Live validation:** APEX's soul PAC-ified and run on apex1 — behaviourally lossless at ~60%
  fewer tokens (on the longer self-evolved soul).
- **North-star:** the symbolic-priming syntax as ApexOS's compressed input layer — the
  self-evolving system speaking its native dialect.
