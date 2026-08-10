# xAI / Grok as an inference backend

First-class support for **xAI Grok** as the node LLM (chat + tools + streaming). Image/video gen
stays on the Imaginarium sibling — this doc is the **agentd LLM** path only.

## Design

xAI’s API is **OpenAI Chat Completions–compatible**. ApexOS reuses `OaiProvider`
(`agentd/crates/agent/src/oai.rs`) — the same path as OpenRouter / Ollama / vLLM.

| Knob | Value |
|------|--------|
| Backend name | `xai` (`KNOWN_BACKENDS`) |
| Default URL | `https://api.x.ai/v1` (auto-pinned on backend switch) |
| Default model | `grok-4.5` |
| Transport | `POST {base}/chat/completions` + SSE |
| Auth | `Authorization: Bearer …` from the **xai** slot of `OaiKeyRing` |

There is **no** separate `xai.rs` client in v1. A future Responses-API client stays open if we need
reasoning-summary streaming into the thinking rail; Chat Completions already covers agent tool loops.

## Operator setup

```bash
# /etc/agentd/env (or Settings → INFERENCE BACKEND → xai)
AGENTD_BACKEND=xai
XAI_API_KEY=xai-...          # xAI slot only — does NOT fight OPENROUTER_API_KEY
# OPENROUTER_API_KEY=...     # can stay set forever; unused while backend=xai
# AGENTD_MODEL=grok-4.5      # default
# AGENTD_XAI_REASONING_EFFORT=low   # default for grok-4.5*
```

Settings: chip **xai** → paste key in the key field (saves the **xai** ring slot) → pick a model from the live `/api/models` list.

**Pre-named workaround** (still works): `AGENTD_BACKEND=oai` + `AGENTD_OAI_BASE_URL=https://api.x.ai/v1` + put the key in the **oai** slot.

## Key ring (coexistence)

`OaiKeyRing` holds three independent Bearer slots:

| Slot | Backend | Env | File |
|------|---------|-----|------|
| `oai` | `oai` / `vllm` / custom | `OAI_API_KEY` | `.oai_api_key` |
| `openrouter` | `openrouter` | `OPENROUTER_API_KEY` | `.openrouter_api_key` |
| `xai` | `xai` | `XAI_API_KEY` | `.xai_api_key` |

Selection is **by live backend at request time** — no first-wins env chain. Switching backends never swaps keys; each slot keeps its value.

## Keys vs Imaginarium

| Consumer | Where the key lives | Env |
|----------|---------------------|-----|
| **agentd LLM** (`backend=xai`) | `/etc/agentd/env` or Settings → `.xai_api_key` | **`XAI_API_KEY`** (agentd LLM slot) |
| **Imaginarium gen** | `/etc/imaginarium/env` only | `XAI_API_KEY` |

- Two independent consumers, two files. Same key *value* may be duplicated; never auto-copied.
- LLM key in agentd is intentional (same trust model as `ANTHROPIC_API_KEY`).
- Gen isolation stays: never put the imaginarium key into plugins.toml or the MCP child env.

## Reasoning effort

`grok-4.5` defaults to **high** reasoning server-side and cannot disable it. For agent tool-loops that
is too slow/expensive, so `OaiProvider` sends:

```json
"reasoning_effort": "low"
```

when `model` starts with `grok-4.5`. Override with `AGENTD_XAI_REASONING_EFFORT` = `low` | `medium` | `high`.
Unknown values omit the field (API default). Non-Grok models never receive the field.

## Explicit non-goals (v1)

- Responses API client / encrypted reasoning / thinking-rail deltas
- xAI built-in `web_search` / `x_search` server tools (would bypass ApexOS tools/policy)
- Separate `XaiProvider` struct
- Auto-sharing keys with Imaginarium
- Changing the seed default backend away from `anthropic`

## Code map

| Piece | Location |
|-------|----------|
| Known backends + defaults | `agentd/crates/gateway/src/backend_config.rs` |
| Route arm | `agentd/crates/agent/src/routing.rs`, `council.rs` |
| Wire client | `agentd/crates/agent/src/oai.rs` |
| Live switch + URL pin + family model reset | `gateway` `POST /api/backend` |
| Key ring | `apexos_agent::OaiKeyRing`; boot `load_oai_key_ring`; `POST /api/keys` per slot |
| Settings chips | `ui-slint/.../settings_view.slint` |

## Adding another named OAI cloud backend

1. Add to `KNOWN_BACKENDS`, `default_model_for`, `default_url_for`
2. OAI match arms in `routing.rs` + `council.rs`
3. Auto-pin in `set_backend_handler` (`openrouter | xai | …`)
4. Add a slot on `OaiKeyRing` + env/file load (do **not** first-wins collapse)
5. Settings chip + fixed-endpoint UX; save key to that slot
6. `docs/env-vars.md` + this recipe
