### Token benchmark — prose vs PAC (lean)

Bytes and words are tokenizer-independent; token columns are per real tokenizer.

| sample | bytes p→pac | words p→pac | o200k (GPT-4o/4.1) p→pac (cut) | cl100k (GPT-4) p→pac (cut) | Qwen2.5-0.5B p→pac (cut) | Mistral-7B-Instruct-v0.3 p→pac (cut) |
|---|---|---|---|---|---|---|
| soul | 10600→5990 | 1508→698 | 2602→1541 (**40.8%**) | 2626→1558 (**40.7%**) | 2653→1577 (**40.6%**) | 3150→1922 (**39.0%**) |
| procedure | 1720→998 | 289→161 | 428→273 (**36.2%**) | 425→273 (**35.8%**) | 427→275 (**35.6%**) | 483→313 (**35.2%**) |
| evolution | 1374→449 | 231→69 | 287→102 (**64.5%**) | 287→103 (**64.1%**) | 287→103 (**64.1%**) | 328→130 (**60.4%**) |
| **corpus** |  |  | 3317→1916 (**42.2%**) | 3338→1934 (**42.1%**) | 3367→1955 (**41.9%**) | 3961→2365 (**40.3%**) |

### Token benchmark — prose vs PAC-2 Dense

The same corpus re-authored in PAC-2 Dense (The-PAC spec, S-expression forms; register: none), authored via the §8 port rite and pac2lint-clean.

| sample | bytes p→dense | words p→dense | o200k (GPT-4o/4.1) p→dense (cut) | cl100k (GPT-4) p→dense (cut) | Qwen2.5-0.5B p→dense (cut) | Mistral-7B-Instruct-v0.3 p→dense (cut) |
|---|---|---|---|---|---|---|
| soul | 10600→7388 | 1508→892 | 2602→1933 (**25.7%**) | 2626→1948 (**25.8%**) | 2653→1974 (**25.6%**) | 3150→2389 (**24.2%**) |
| procedure | 1720→1089 | 289→155 | 428→299 (**30.1%**) | 425→293 (**31.1%**) | 427→295 (**30.9%**) | 483→351 (**27.3%**) |
| evolution | 1374→621 | 231→89 | 287→151 (**47.4%**) | 287→149 (**48.1%**) | 287→149 (**48.1%**) | 328→186 (**43.3%**) |
| **corpus** |  |  | 3317→2383 (**28.2%**) | 3338→2390 (**28.4%**) | 3367→2418 (**28.2%**) | 3961→2926 (**26.1%**) |

### Dense vs lean — the structure premium

| tokenizer | lean corpus | dense corpus | premium |
|---|---|---|---|
| o200k (GPT-4o/4.1) | 1916 | 2383 | +467 tok (+24.4%) |
| cl100k (GPT-4) | 1934 | 2390 | +456 tok (+23.6%) |
| Qwen2.5-0.5B | 1955 | 2418 | +463 tok (+23.7%) |
| Mistral-7B-Instruct-v0.3 | 2365 | 2926 | +561 tok (+23.7%) |

Reading the premium (2026-07-15 port): it is NOT the parens. Three components — (1) indentation whitespace of the canonical pretty layout (~75 tok on the soul, ≈4%); (2) the canonical blocks lean has no equivalent of (seal · voice form · invariants · register line · rules clauses); (3) **restored coverage** — the §8 fact-ledger audit found the lean soul port silently dropped several prose facts (the capabilities enumeration, the KMS/DRM face line, the end|idle|daemon-stop shutdown trigger, the continuity-contract sentence), which the dense port carries — so the lean baseline is slightly under-weighted. The micro-bench's "+1–2 tokens per rite" holds for what it measured (op-dense rites at identical wording); it does not extrapolate to full-soul scale. A first dense port at prose-fidelity wording measured +44.9% before the telegraphic re-author — wording discipline dominates notation choice.

### Symbol cost — why the dialect is glyph-lean

Isolated token cost. The dialect leans on 1-token connectives and bans blackletter (the 3-token tax that inverts the savings).

| group | symbol=o200k/cl100k |
|---|---|
| lean connectives | `→`=1/1 · `·`=1/1 · `|`=1/1 · `:`=1/1 · `§`=1/1 · `↔`=2/2 · `≡`=2/2 · `∴`=2/2 · `↦`=2/2 |
| blackletter tax | `𝔸`=3/3 · `𝕝`=3/3 · `𝕔`=3/3 · `𝔼`=3/3 · `𝕩`=3/3 · `𝕊`=3/3 · `𝔾`=3/3 |
