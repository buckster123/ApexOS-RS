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
- `samples/` — the corpus, one prose⇄PAC pair per authoring surface:
  - `soul.*` — prose side is the **real shipped** `config/soul.md`; PAC side is `soul.pac.md`.
  - `procedure.*` — a `store_procedure` skill (command-heavy → compresses least).
  - `evolution.*` — a `propose_evolution` payload (all rationale → compresses most).
- `RESULTS.md` — committed snapshot of the numbers (4 tokenizers, 3 families).

## Why three samples

They span the prose-to-literal spectrum on purpose: the evolution payload is nearly all
prose rationale (compresses hardest), the procedure is mostly literal shell commands
(compresses least — commands are incompressible in any notation), and the soul sits in
between. The spread is the point — it shows *what drives* the savings.
