### Token benchmark — prose vs PAC

Bytes and words are tokenizer-independent; token columns are per real tokenizer.

| sample | bytes p→pac | words p→pac | o200k (GPT-4o/4.1) p→pac (cut) | cl100k (GPT-4) p→pac (cut) | Qwen2.5-0.5B p→pac (cut) | Mistral-7B-Instruct-v0.3 p→pac (cut) |
|---|---|---|---|---|---|---|
| soul | 10600→5990 | 1508→698 | 2602→1541 (**40.8%**) | 2626→1558 (**40.7%**) | 2653→1577 (**40.6%**) | 3150→1922 (**39.0%**) |
| procedure | 1720→998 | 289→161 | 428→273 (**36.2%**) | 425→273 (**35.8%**) | 427→275 (**35.6%**) | 483→313 (**35.2%**) |
| evolution | 1374→449 | 231→69 | 287→102 (**64.5%**) | 287→103 (**64.1%**) | 287→103 (**64.1%**) | 328→130 (**60.4%**) |
| **corpus** |  |  | 3317→1916 (**42.2%**) | 3338→1934 (**42.1%**) | 3367→1955 (**41.9%**) | 3961→2365 (**40.3%**) |

### Symbol cost — why the dialect is glyph-lean

Isolated token cost. The dialect leans on 1-token connectives and bans blackletter (the 3-token tax that inverts the savings).

| group | symbol=o200k/cl100k |
|---|---|
| lean connectives | `→`=1/1 · `·`=1/1 · `|`=1/1 · `:`=1/1 · `§`=1/1 · `↔`=2/2 · `≡`=2/2 · `∴`=2/2 · `↦`=2/2 |
| blackletter tax | `𝔸`=3/3 · `𝕝`=3/3 · `𝕔`=3/3 · `𝔼`=3/3 · `𝕩`=3/3 · `𝕊`=3/3 · `𝔾`=3/3 |
