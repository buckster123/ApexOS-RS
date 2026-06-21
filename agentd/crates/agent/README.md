# apexos-agent

> The agent turn engine: LLM stream → tool round-trips → council. Plus prompt caching.

Drives one agent turn end to end: stream from a provider, emit `AgentText` deltas, request
tools over the Bus, await their results, loop to `TurnComplete`. Holds the provider trait, the
live-swappable routing provider, the Anthropic/OpenAI impls, the prompt-cache discipline, and
the multi-persona council runner.

- **Key files:** `src/turn.rs` (`run_turn`, `compose_system`, `inject_ambient`) · `src/provider.rs` (`Provider` trait + `Chunk`) · `src/routing.rs` (`RoutingProvider`, live backend swap) · `src/anthropic.rs`/`src/oai.rs` · `src/cache.rs` (`CacheConfig`) · `src/usage.rs` · `src/council.rs`
- **Depends on:** `apexos-core`, `tokio`, `reqwest`, `async-trait`, `async-stream`, `futures-util`, `serde`/`serde_json`.
- **Lift via:** the `Provider` trait + turn loop is a reusable agent core; `compose_system`/`trim`-style helpers are pure. The prompt-cache discipline it implements is documented portably in [`docs/prompt-caching.md`](../../../docs/prompt-caching.md).

Part of [ApexOS-RS](https://github.com/buckster123/ApexOS-RS) — see [`PATTERNS.md`](../../../PATTERNS.md) (lift-me index) and [`docs/repo-map.md`](../../../docs/repo-map.md) (full map).
