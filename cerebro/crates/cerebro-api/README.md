# cerebro-api

> axum REST API + dashboard over the Cerebro engine.

An optional HTTP service exposing the `cerebro` engine over ~40 REST routes plus a web dashboard,
behind `AGENTD_TOKEN` bearer auth. Use it to browse/query memory from a browser or other services.

- **Key files:** `src/main.rs` (handlers + router)
- **Depends on:** `cerebro`, `axum`, `tower`, `tokio`, `serde`/`serde_json`, `chrono`, `uuid`, `tracing`.
- **Lift via:** run as a standalone service over a Cerebro DB; read it as the reference for the REST surface + token middleware.

Part of [ApexOS-RS](https://github.com/buckster123/ApexOS-RS) — see [`PATTERNS.md`](../../../PATTERNS.md) (lift-me index) and [`docs/repo-map.md`](../../../docs/repo-map.md) (full map).
