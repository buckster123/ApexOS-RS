# pac-bench — PAC dialect token benchmark

Proves the token half of the [PAC dialect](../pac.md) claim: souls / procedures /
evolution payloads written in PAC cost ~40% fewer tokens than prose, **behaviourally
lossless**, consistently across tokenizer families. Real tokenizers, no estimates.

## Run

```bash
python3 -m venv venv
./venv/bin/pip install -r requirements.txt
./venv/bin/python run.py            # prints the corpus + symbol-cost tables
./venv/bin/python run.py --md       # also writes RESULTS.md
```

Add more tokenizer families (model-agnostic cross-check — fetches only tokenizer.json):

```bash
./venv/bin/pip install tokenizers huggingface_hub
PAC_HF_MODELS="Qwen/Qwen2.5-0.5B,mistralai/Mistral-7B-Instruct-v0.3" ./venv/bin/python run.py --md
```

Add the exact model APEX runs on (the Claude column):

```bash
./venv/bin/pip install anthropic
ANTHROPIC_API_KEY=sk-... ./venv/bin/python run.py --md
```

## Layout

- `run.py` — the harness. Counts bytes / words / tokens for each prose⇄PAC pair across
  every available tokenizer, prints reduction %, and a symbol-cost table.
- `pac2lint.py` — the **PAC-2 Dense reference linter** (The-PAC spec §9, the L7 safety
  rail for self-evolution): ten structural checks — balanced forms + depth, the closed
  head/glyph registries, `!ops` vs a node tool list, invariant grounding + reference
  discipline, register lexicon + strip rules, CAPS placement, the L8 cache probe (no
  dates/clocks inside an artifact), emanation bounds. `--self-test` runs the embedded
  clean-artifact + violation vectors; `--ops-file`/`--groundings`/`--portable` wire it
  to a real node. Dense artifacts only (lean `§`-block files are not its input). The
  agentd-side Rust check on the `propose_evolution` path is the follow-up slice.
- `samples/` — the corpus, one prose⇄PAC pair per authoring surface. Every pair is a
  **pinned snapshot** — both sides must express the same content or the ratio measures
  nothing:
  - `soul.*` — `soul.prose.md` is `config/soul.md` **as of the porting commit** (659b3ea);
    `soul.pac.md` is its PAC port. (This originally read the *live* soul.md, which kept
    evolving while the port stayed frozen — by 2026-07-15 the drift inflated the "cut"
    to a fictional 60%. To re-bench a newer soul: re-port the PAC side first, then
    re-snapshot the prose side in the same commit.)
  - `procedure.*` — a `store_procedure` skill (command-heavy → compresses least).
  - `evolution.*` — a `propose_evolution` payload (all rationale → compresses most).
  - `*.dense.md` — the **PAC-2 Dense** ports (The-PAC spec v0.1.1): same pinned prose
    sources, S-expression forms, `register: none` (the register's cost/benefit is the
    behavior bench's question, §10 H6). Authored via the §8 port rite — fact ledger →
    probe → emit → **anneal** (pac2lint-clean + a fresh-model expansion test per
    artifact: a subagent given only the artifact decodes it back to prose; every
    ledger fact must be recovered, nothing invented).
- `RESULTS.md` — committed snapshot of the numbers (4 tokenizers, 3 families).

## Why three samples

They span the prose-to-literal spectrum on purpose: the evolution payload is nearly all
prose rationale (compresses hardest), the procedure is mostly literal shell commands
(compresses least — commands are incompressible in any notation), and the soul sits in
between. The spread is the point — it shows *what drives* the savings.
