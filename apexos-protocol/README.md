# apexos-protocol

> Shared wire-protocol types for the ApexOS-RS workspace. Lean: serde only.

The single source of truth for what goes over the wire between the agent daemon and
every client (UI, PWA, mesh peers): the `Event` enum, the WS/a2a frame contract, and the
newtype IDs. `apexos-core` re-exports it, so `apexos_core::Event` *is* these types.

- **Key files:** `src/lib.rs` (`Event`, `EvolutionProposal`, `PolicyRule`/`PolicyMode`, `Subsystem`, `SensorReading`, …)
- **Depends on:** `serde`, `serde_json` — nothing else.
- **Features:** `default = ["std"]` — every workspace consumer is untouched. `--no-default-features --features alloc` builds `#![no_std]` for bare-metal nodes (`Map<K,V>` = `HashMap` under `std` ⇄ `BTreeMap` under `no_std`; identical JSON either way, locked by `tests/wire_compat.rs`). Run both gates when touching this crate: `cargo test -p apexos-protocol` and `cargo test -p apexos-protocol --no-default-features --features alloc`.
- **Lift via:** `cargo add apexos-protocol` (or copy `lib.rs`). Depend on it to speak agentd's wire protocol without pulling in the daemon. The model for every clean seam in this repo. First external consumer: [ApexOS-RV](https://github.com/buckster123/ApexOS-RV) — a `no_std` bare-metal RISC-V kernel that streams these events over UART.

Part of [ApexOS-RS](https://github.com/buckster123/ApexOS-RS) — see [`PATTERNS.md`](../PATTERNS.md) (lift-me index) and [`docs/repo-map.md`](../docs/repo-map.md) (full map).
