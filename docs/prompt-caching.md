# Prompt caching — keep the prefix byte-stable

> The discipline that makes a long-lived agent cheap: maximise the byte-stable prefix of every
> request, and push everything that varies per turn *after* it. Portable to any provider with
> prefix caching. In ApexOS it's the dominant cost win on long sessions — combined with
> [PAC](pac.md), the two compound (PAC shrinks what you *write*, caching shrinks what you *re-send*).

This is a **liftable pattern** (see [`PATTERNS.md`](../PATTERNS.md)). The idea and the invariants
below are provider-agnostic; the ApexOS implementation map at the end shows where each piece lives.

---

## The idea

LLM providers cache a stable **prefix** of a request and re-serve it cheaply (~0.1× input price on
Anthropic) on the next request that shares that prefix. An agent re-sends a huge, mostly-unchanging
preamble every turn — system prompt, tool definitions, conversation history — so the entire game is:
**keep that preamble byte-identical turn to turn, and quarantine anything volatile after it.**

It applies to every provider with prefix caching; only the *mechanism* differs (Anthropic: explicit
`cache_control` markers; OpenAI / Ollama: automatic prefix caching with no markers). The discipline
is the same either way.

---

## The invariants (the portable contract)

1. **Render order is `tools → system → messages`.** Tools + system form the cacheable prefix; the
   messages (history) follow.
2. **Anything that varies per turn *inside* the prefix invalidates the whole cache.** One changed
   byte before the cache breakpoint and the prefix is re-billed at full price.
3. Therefore:
   - **Keep the system prefix byte-stable.** Identity/instructions and any machine-generated
     context (a live "current state" block) must only change on a *real* state change, never every
     turn.
   - **Order tools deterministically** (e.g. name-sort). A runtime tool-registry reorder (plugins
     registering/unregistering) would otherwise shuffle the position-0 tools prefix and bust it.
   - **Push per-turn-volatile text out of the prefix and into the messages** — the wall clock,
     uptime, per-request ids, anything that ticks. It lands *after* the cached span, costing nothing.
   - **Inject that volatile text ephemerally** — append it to the outbound request only, never
     persist it, so it can't bloat the stored history or reappear on replay.
   - **Gate the volatile injection** so even the message tail stays mostly stable: inject the clock
     only when it earns its keep (first turn + after an idle gap), not on every message.
4. **Conversation caching (long sessions):** roll *extra* breakpoints back through the **stable**
   history — everything except the volatile current turn — so a growing transcript caches
   incrementally instead of re-billing the whole tail each turn. Respect the provider's limits
   (Anthropic: ≤ 4 breakpoints total, ~20-block lookback → anchor a breakpoint every ~15 blocks).

---

## The failure mode + the tell

The classic mistake is slipping a timestamp / uptime / per-request id / any per-turn-varying string
into the system prefix (or a "current state" block inside it). It **silently** kills caching — no
error, just full-price re-billing every turn.

**Detect it:** the response's `cache_read_input_tokens` sits near zero. Log `read / write / uncached`
token counts per turn and watch `read` climb the moment the prefix is actually stable.

---

## Minimal version

The smallest version that works: a **byte-stable system string** + **deterministically-ordered
tools** + **volatile text appended to the last user message** (never the system). That alone earns
the prefix cache. Conversation caching, TTL tuning, and tokenomics accounting are refinements on top.

---

## The ApexOS implementation (idea → code)

| piece | where |
|---|---|
| System as one `cache_control:{ephemeral}` block; tools name-sorted | `agentd/crates/agent/src/anthropic.rs` — `build_body()` |
| Conversation caching (≤4 breakpoints, rolls back through stable history) | `anthropic.rs` — `apply_conversation_cache()` |
| Build the volatile clock | `agentd/crates/agentd/src/main.rs` — `build_ambient_clock()` |
| Append it ephemerally to the last user turn | `agentd/crates/agent/src/turn.rs` — `inject_ambient()` |
| Gate it (first turn + idle gap) | `turn.rs` — `TurnEngine::should_inject_ambient_at()` |
| Live config (enabled / cache_conversation / ttl), shared `Arc<RwLock>` | `agentd/crates/agent/src/cache.rs` — `CacheConfig`; env `AGENTD_CACHE*`; live via `GET`/`POST /api/cache` |
| Tokenomics (hit-rate, banked tokens, cost estimate) | `agentd/crates/agent/src/usage.rs`; `GET /api/usage`; surfaced as the **CACHE BANK** card in the ⚡ Inference UI |
| OpenAI variant (auto-prefix, no markers) | `agentd/crates/agent/src/oai.rs` — `build_body()` |

**The contract is unit-tested, and the test names read as the spec** — lift these with the code
(`anthropic.rs` test module):

- `build_body_system_carries_cache_control`
- `build_body_sorts_tools_by_name_for_stable_cache_prefix`
- `build_body_caches_conversation_but_not_the_current_turn`
- `build_body_conversation_caching_respects_four_breakpoint_cap`
- `build_body_1h_ttl_sets_ttl_on_system_block`
- `build_body_disabled_sends_plain_system_and_no_cache_control`

And in `turn.rs`: `inject_ambient_appends_clock_to_last_user_turn`,
`inject_ambient_lands_after_tool_results`.

---

## Provider notes

- **Anthropic** — explicit `cache_control` blocks. TTL is 5 min by default (write premium 1.25×) or
  1 hour (`AGENTD_CACHE_TTL=1h`, write premium 2×, survives >5-min human pauses without re-writing
  the whole prefix). ≤ 4 breakpoints per request.
- **OpenAI / Ollama** — automatic prefix caching, no markers to set. The *same* stable-prefix
  discipline is exactly what triggers it, so everything above still applies; you just don't place
  breakpoints by hand.

---

## Why it's the dominant win on long sessions

On a million-token "giga-session," the unbounded conversation tail is the cost driver. Conversation
caching shrinks that tail to ~0.1×, so the marginal cost of one more turn stops growing with history
length. Pair it with PAC — which cuts the tokens you author in the first place — and you compress
both axes at once: fewer tokens written, and the ones already written re-served nearly free.
