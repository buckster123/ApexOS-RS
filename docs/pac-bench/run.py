#!/usr/bin/env python3
"""PAC dialect token benchmark — measures real tokenizer cost of prose vs PAC.

The ApexOS PAC dialect (docs/pac.md) claims ~60% fewer tokens than prose for
souls / procedures / evolution payloads, *behaviorally lossless*. This script
proves the token half with real tokenizers — no estimates.

Tokenizers (each skipped gracefully if unavailable):
  - tiktoken o200k_base   (GPT-4o / GPT-4.1 family)        — pip install tiktoken
  - tiktoken cl100k_base  (GPT-4 / GPT-3.5 family)         — pip install tiktoken
  - Anthropic count_tokens (the exact model APEX runs on)  — needs ANTHROPIC_API_KEY
  - HF AutoTokenizer (an open model, model-agnostic check) — PAC_HF_MODEL=<repo>

Run:
  python3 -m venv venv && ./venv/bin/pip install -r requirements.txt
  ./venv/bin/python run.py            # corpus table + symbol-cost table
  ./venv/bin/python run.py --md       # emit the markdown block for docs/pac.md
"""
from __future__ import annotations
import os, sys, pathlib

HERE = pathlib.Path(__file__).resolve().parent
ROOT = HERE.parent.parent  # repo root

# (label, prose_path, pac_path, dense_path) — every set is a PINNED SNAPSHOT.
# The soul pair originally read the LIVE config/soul.md, but the live soul
# evolves while the ports stay frozen — by 2026-07-15 the prose side had grown
# ~1.5k tokens the pac side never ported, silently inflating the "cut" to 60%
# (the sides were no longer the same content, so the ratio wasn't a compression
# ratio). L1: a bench compares equivalent content or it measures nothing.
# soul.prose.md is config/soul.md as of the porting commit (659b3ea). To
# re-bench a newer soul: re-port the pac + dense sides first, then re-snapshot
# the prose side in the same commit.
#
# *.dense.md are the PAC-2 Dense ports (The-PAC spec v0.1.1), authored via the
# §8 port rite (fact ledger → probe → emit → anneal incl. a fresh-model
# expansion test) and pac2lint-clean. Register: none — the token comparison vs
# lean stays register-free; the register's cost/benefit is the behavior bench's
# question (spec §10 H6), not this one's.
SAMPLES = [
    ("soul",      HERE / "samples/soul.prose.md",       HERE / "samples/soul.pac.md",       HERE / "samples/soul.dense.md"),
    ("procedure", HERE / "samples/procedure.prose.md",  HERE / "samples/procedure.pac.md",  HERE / "samples/procedure.dense.md"),
    ("evolution", HERE / "samples/evolution.prose.md",  HERE / "samples/evolution.pac.md",  HERE / "samples/evolution.dense.md"),
]

# Symbols whose isolated cost the dialect is designed around (see docs/pac.md).
SYMBOL_GROUPS = {
    "lean connectives": ["→", "·", "|", ":", "§", "↔", "≡", "∴", "↦"],
    "blackletter tax":  ["𝔸", "𝕝", "𝕔", "𝔼", "𝕩", "𝕊", "𝔾"],
}


def load(p: pathlib.Path) -> str:
    return p.read_text(encoding="utf-8")


def words(s: str) -> int:
    return len(s.split())


# ---- tokenizer backends: each returns a callable str->int, or None -----------

def tiktoken_counters():
    out = {}
    try:
        import tiktoken
    except ImportError:
        return out
    for enc_name, label in [("o200k_base", "o200k (GPT-4o/4.1)"),
                            ("cl100k_base", "cl100k (GPT-4)")]:
        try:
            enc = tiktoken.get_encoding(enc_name)
            out[label] = (lambda e: (lambda t: len(e.encode(t))))(enc)
        except Exception as e:  # vocab download blocked, etc.
            print(f"  (skip {label}: {e})", file=sys.stderr)
    return out


def anthropic_counter():
    key = os.environ.get("ANTHROPIC_API_KEY")
    if not key:
        return {}
    try:
        import anthropic
    except ImportError:
        print("  (skip Anthropic: pip install anthropic)", file=sys.stderr)
        return {}
    client = anthropic.Anthropic(api_key=key)
    model = os.environ.get("PAC_ANTHROPIC_MODEL", "claude-opus-4-8")

    def count(t: str) -> int:
        r = client.messages.count_tokens(
            model=model, messages=[{"role": "user", "content": t}])
        return r.input_tokens

    return {f"Anthropic ({model})": count}


def hf_counter():
    """Cross-family check via the lightweight `tokenizers` lib (fetches only the
    tokenizer.json, not model weights). PAC_HF_MODELS=repo1,repo2 — e.g.
    Qwen/Qwen2.5-0.5B,mistralai/Mistral-7B-Instruct-v0.3 (Qwen + Llama/Mistral
    families confirm the cut is structural, not an OpenAI-BPE artifact)."""
    repos = [r.strip() for r in os.environ.get("PAC_HF_MODELS", "").split(",") if r.strip()]
    if not repos:
        return {}
    try:
        from tokenizers import Tokenizer
    except ImportError:
        print("  (skip HF: pip install tokenizers huggingface_hub)", file=sys.stderr)
        return {}
    out = {}
    for repo in repos:
        try:
            tok = Tokenizer.from_pretrained(repo)
        except Exception as e:
            print(f"  (skip HF {repo}: {str(e)[:80]})", file=sys.stderr)
            continue
        short = repo.split("/")[-1]
        out[f"{short}"] = (lambda tk: lambda t: len(tk.encode(t).ids))(tok)
    return out


def counters():
    c = {}
    c.update(tiktoken_counters())
    c.update(anthropic_counter())
    c.update(hf_counter())
    return c


# ---- reporting ---------------------------------------------------------------

def pct(prose: int, pac: int) -> str:
    return f"{(1 - pac / prose) * 100:.1f}%" if prose else "n/a"


def variant_table(title, note, rows, cs, short_b):
    """One prose→variant comparison table + per-tokenizer corpus totals.
    `rows` = [(label, prose_text, variant_text)]. Returns (lines, agg) where
    agg[n] = [prose_total, variant_total]."""
    lines = [f"### {title}\n", note + "\n"]
    header = ["sample", f"bytes p→{short_b}", f"words p→{short_b}"] + [f"{n} p→{short_b} (cut)" for n in cs]
    lines.append("| " + " | ".join(header) + " |")
    lines.append("|" + "|".join(["---"] * len(header)) + "|")
    agg = {n: [0, 0] for n in cs}
    for label, prose, variant in rows:
        row = [label,
               f"{len(prose.encode())}→{len(variant.encode())}",
               f"{words(prose)}→{words(variant)}"]
        for n, fn in cs.items():
            tp, tq = fn(prose), fn(variant)
            agg[n][0] += tp
            agg[n][1] += tq
            row.append(f"{tp}→{tq} (**{pct(tp, tq)}**)")
        lines.append("| " + " | ".join(row) + " |")
    total = ["**corpus**", "", ""]
    for n in cs:
        tp, tq = agg[n]
        total.append(f"{tp}→{tq} (**{pct(tp, tq)}**)")
    lines.append("| " + " | ".join(total) + " |")
    return lines, agg


def main(md: bool = False):
    cs = counters()
    if not cs:
        sys.exit("No tokenizer available. pip install -r requirements.txt")

    texts = [(label, load(pp), load(qp), load(dp)) for label, pp, qp, dp in SAMPLES]

    lean_lines, lean_agg = variant_table(
        "Token benchmark — prose vs PAC (lean)",
        "Bytes and words are tokenizer-independent; token columns are per real tokenizer.",
        [(l, p, q) for l, p, q, _ in texts], cs, "pac")
    dense_lines, dense_agg = variant_table(
        "Token benchmark — prose vs PAC-2 Dense",
        "The same corpus re-authored in PAC-2 Dense (The-PAC spec, S-expression forms; "
        "register: none), authored via the §8 port rite and pac2lint-clean.",
        [(l, p, d) for l, p, _, d in texts], cs, "dense")

    lines = list(lean_lines) + [""] + list(dense_lines)

    # Dense vs lean — the structure premium, the number the spec's micro-bench
    # estimated at +1–2 tokens per rite. Corpus-level truth lands here.
    lines.append("\n### Dense vs lean — the structure premium\n")
    lines.append("| tokenizer | lean corpus | dense corpus | premium |")
    lines.append("|---|---|---|---|")
    for n in cs:
        lt, dt = lean_agg[n][1], dense_agg[n][1]
        sign = "+" if dt >= lt else ""
        lines.append(f"| {n} | {lt} | {dt} | {sign}{dt - lt} tok ({sign}{(dt - lt) / lt * 100:.1f}%) |")
    lines.append(
        "\nReading the premium (2026-07-15 port): it is NOT the parens. Three components — "
        "(1) indentation whitespace of the canonical pretty layout (~75 tok on the soul, ≈4%); "
        "(2) the canonical blocks lean has no equivalent of (seal · voice form · invariants · "
        "register line · rules clauses); (3) **restored coverage** — the §8 fact-ledger audit "
        "found the lean soul port silently dropped several prose facts (the capabilities "
        "enumeration, the KMS/DRM face line, the end|idle|daemon-stop shutdown trigger, the "
        "continuity-contract sentence), which the dense port carries — so the lean baseline is "
        "slightly under-weighted. The micro-bench's \"+1–2 tokens per rite\" holds for what it "
        "measured (op-dense rites at identical wording); it does not extrapolate to full-soul "
        "scale. A first dense port at prose-fidelity wording measured +44.9% before the "
        "telegraphic re-author — wording discipline dominates notation choice.")

    # symbol-cost table (only with tiktoken present)
    try:
        import tiktoken
        o2 = tiktoken.get_encoding("o200k_base")
        cl = tiktoken.get_encoding("cl100k_base")
        lines.append("\n### Symbol cost — why the dialect is glyph-lean\n")
        lines.append("Isolated token cost. The dialect leans on 1-token connectives and "
                     "bans blackletter (the 3-token tax that inverts the savings).\n")
        lines.append("| group | symbol=o200k/cl100k |")
        lines.append("|---|---|")
        for g, syms in SYMBOL_GROUPS.items():
            cells = " · ".join(f"`{s}`={len(o2.encode(s))}/{len(cl.encode(s))}" for s in syms)
            lines.append(f"| {g} | {cells} |")
    except Exception:
        pass

    out = "\n".join(lines)
    print(out)
    if md:
        (HERE / "RESULTS.md").write_text(out + "\n", encoding="utf-8")
        print(f"\n[wrote {HERE / 'RESULTS.md'}]", file=sys.stderr)


if __name__ == "__main__":
    main(md="--md" in sys.argv)
