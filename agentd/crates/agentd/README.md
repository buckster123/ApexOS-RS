# agentd

> The main daemon binary — wires the bus, gateway, supervisor, turn engine, scheduler.

The assembly point of the whole stack: builds the tokio runtime, the core Bus, and starts every
loop — the agent router (UserPrompt → `root_turn`), the evolution applier, the scheduler, the
council and consolidation workers, the self-update watchdog, mesh discovery. Loads the soul +
keys, binds the gateway. This is the integration crate; the reusable parts live in its libraries.

- **Key files:** `src/main.rs` (bus wiring, `spawn_agent_router`, `serve`, `build_embodiment`, `gather_tools`) · `src/evolution.rs` (pure self-evolution core, tested) · `src/scheduler.rs` · `src/council_handler.rs` · `src/consolidate.rs` · `src/self_update.rs` · `src/session_store.rs`
- **Depends on:** `apexos-core`, `apexos-gateway`, `apexos-plugins`, `apexos-agent`, `apexos-store`, `tokio`, `toml_edit`, `cron`, `chrono`, `anyhow`.
- **Lift via:** the binary itself is the wiring, not a unit to lift — but `src/evolution.rs` (the propose→snapshot→apply→ack→rollback state machine) is pure + tested and copies on its own.

Part of [ApexOS-RS](https://github.com/buckster123/ApexOS-RS) — see [`PATTERNS.md`](../../../PATTERNS.md) (lift-me index) and [`docs/repo-map.md`](../../../docs/repo-map.md) (full map).
