# apexos-gateway

> axum HTTP+WS server — the entire external surface of agentd.

Every byte in or out of the daemon goes through here: the `/ws` agent stream, `/sensor-bridge`,
`/terminal-ws`, and the `/api/*` REST surface (sessions, cache, usage, mesh, image/audio). It
bridges the WS frames to the core Bus and back, and holds the mesh `PeerRegistry` + avahi discovery.

- **Key files:** `src/lib.rs` (`router()`, `serve()`, `ws_handler`/`handle_socket`, `GatewayState`, `require_token`) · `src/mesh.rs` (PeerRegistry, avahi)
- **Depends on:** `apexos-core`, `apexos-plugins`, `axum`, `tokio`, `reqwest`, `libc`, `toml`, `chrono`.
- **Lift via:** not standalone (wired to the core Bus + plugins). Read it as the reference for the WS↔event-bus bridge, per-socket session filtering, and token-gated bind patterns.

Part of [ApexOS-RS](https://github.com/buckster123/ApexOS-RS) — see [`PATTERNS.md`](../../../PATTERNS.md) (lift-me index) and [`docs/repo-map.md`](../../../docs/repo-map.md) (full map).
