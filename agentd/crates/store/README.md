# apexos-store

> Append-only event-log writer — subscribe the bus, persist JSONL.

A tiny crate: one task that subscribes the broadcast bus and writes every `Event` to a
date-rolling append-only JSONL log. The durable record behind `query_event_log` and session replay.

- **Key files:** `src/lib.rs` (`run_log_writer` — a single `pub async fn`)
- **Depends on:** `apexos-core`, `serde_json`, `tokio`, `anyhow`, `chrono`.
- **Lift via:** near-copy-paste — it's one function. The pattern (subscribe broadcast → append JSONL, date-rolling) drops into any event-bus system.

Part of [ApexOS-RS](https://github.com/buckster123/ApexOS-RS) — see [`PATTERNS.md`](../../../PATTERNS.md) (lift-me index) and [`docs/repo-map.md`](../../../docs/repo-map.md) (full map).
