# apexos-plugins

> MCP plugin host + approval PolicyEngine + the virtual-tool chain.

Spawns and supervises stdio MCP plugins, routes every tool call, and enforces the approval
policy. The `Supervisor` is where real tools dispatch to child processes and virtual tools
(propose_evolution, schedule_*, convene_council, agent_spawn, mesh_*, vast_*) are intercepted;
the `PolicyEngine` decides allow / ask / workspace per tool.

- **Key files:** `src/supervisor.rs` (`Supervisor::run`, `ToolProxy::call`, `dispatch_tool`) · `src/mcp.rs` (`McpClient` over child stdio) · `src/policy.rs` (`PolicyEngine`, `Rule`, `Decision`) · `src/config.rs` · `src/vast.rs`
- **Depends on:** `apexos-core`, `tokio`, `serde`/`serde_json`, `anyhow`, `toml`, `chrono`, `reqwest`.
- **Lift via:** the `PolicyEngine` (rule table → decision) and the `McpClient` stdio-MCP host are the self-contained, reusable pieces; the virtual-tool chain is ApexOS-specific.

Part of [ApexOS-RS](https://github.com/buckster123/ApexOS-RS) — see [`PATTERNS.md`](../../../PATTERNS.md) (lift-me index) and [`docs/repo-map.md`](../../../docs/repo-map.md) (full map).
