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

# (label, prose_path, pac_path) — every pair is a PINNED SNAPSHOT. The soul pair
# originally read the LIVE config/soul.md, but the live soul evolves while the
# pac port stays frozen — by 2026-07-15 the prose side had grown ~1.5k tokens the
# pac side never ported, silently inflating the "cut" to 60% (the two sides were
# no longer the same content, so the ratio wasn't a compression ratio). L1: a
# bench compares equivalent content or it measures nothing. soul.prose.md is
# config/soul.md as of the porting commit (659b3ea). To re-bench a newer soul:
# re-port the pac side first, then re-snapshot the prose side in the same commit.
SAMPLES = [
    ("soul",      HERE / "samples/soul.prose.md",       HERE / "samples/soul.pac.md"),
    ("procedure", HERE / "samples/procedure.prose.md",  HERE / "samples/procedure.pac.md"),
    ("evolution", HERE / "samples/evolution.prose.md",  HERE / "samples/evolution.pac.md"),
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


def main(md: bool = False):
    cs = counters()
    if not cs:
        sys.exit("No tokenizer available. pip install -r requirements.txt")

    texts = [(label, load(pp), load(qp)) for label, pp, qp in SAMPLES]

    lines = []
    lines.append("### Token benchmark — prose vs PAC\n")
    lines.append("Bytes and words are tokenizer-independent; token columns are per real tokenizer.\n")
    # per-tokenizer table
    header = ["sample", "bytes p→pac", "words p→pac"] + [f"{n} p→pac (cut)" for n in cs]
    lines.append("| " + " | ".join(header) + " |")
    lines.append("|" + "|".join(["---"] * len(header)) + "|")
    agg = {n: [0, 0] for n in cs}
    for label, prose, pac in texts:
        row = [label,
               f"{len(prose.encode())}→{len(pac.encode())}",
               f"{words(prose)}→{words(pac)}"]
        for n, fn in cs.items():
            tp, tq = fn(prose), fn(pac)
            agg[n][0] += tp
            agg[n][1] += tq
            row.append(f"{tp}→{tq} (**{pct(tp, tq)}**)")
        lines.append("| " + " | ".join(row) + " |")
    total = ["**corpus**", "", ""]
    for n in cs:
        tp, tq = agg[n]
        total.append(f"{tp}→{tq} (**{pct(tp, tq)}**)")
    lines.append("| " + " | ".join(total) + " |")

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
