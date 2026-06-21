# cerebro-mcp

> MCP-over-stdio server exposing the Cerebro memory tools — the plugin agentd spawns.

Wraps the `cerebro` engine in an MCP stdio server (~63 tools: recall, store, session_recall/save,
procedures, intentions, episodes, dream_run, …). This is the binary agentd launches as a child
plugin to give the agent memory; it also runs beside any MCP-speaking agent.

- **Key files:** `src/main.rs` (initialize handshake + read/dispatch/write loop) · `src/dispatch.rs` (`route(name, args, brain)`) · `src/tools.rs` (schema registry) · `src/transport.rs` (`StdioTransport`)
- **Depends on:** `cerebro`, `tokio`, `serde`/`serde_json`, `anyhow`, `tracing`, `uuid`.
- **Lift via:** drop-in memory for any MCP host — point your agent's plugin config at this binary. The dispatch/transport split is a clean MCP-server template.

Part of [ApexOS-RS](https://github.com/buckster123/ApexOS-RS) — see [`PATTERNS.md`](../../../PATTERNS.md) (lift-me index) and [`docs/repo-map.md`](../../../docs/repo-map.md) (full map).
