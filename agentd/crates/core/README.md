# apexos-core

> Shared types + the in-process event Bus. The hub everything fans out from.

The foundation crate of the agent daemon: it re-exports the wire types from `apexos-protocol`
and adds the in-process **event Bus** (mpsc inbox → `SystemState::apply` → broadcast out) that
every subsystem subscribes to, plus `SystemState`, the ID newtypes, the bounded-history trimmer,
and the vision downscale shim.

- **Key files:** `src/bus.rs` (`Bus::new` → `BusHandle` emit + broadcast subscribe; `Bus::run`) · `src/state.rs` (`SystemState::apply`) · `src/history.rs` (`trim_history`, pure + tested) · `src/vision.rs` · `src/lib.rs`
- **Depends on:** `apexos-protocol`, `serde`, `serde_json`, `tokio`.
- **Lift via:** copy the Bus pattern (mpsc-in → broadcast-out with a state-apply step) or `trim_history` (pure, tested) directly; depend on the crate to share the daemon's types + bus.

Part of [ApexOS-RS](https://github.com/buckster123/ApexOS-RS) — see [`PATTERNS.md`](../../../PATTERNS.md) (lift-me index) and [`docs/repo-map.md`](../../../docs/repo-map.md) (full map).
