# cerebro-cli

> clap CLI over the Cerebro engine (binary named `cerebro`).

A command-line front end to the `cerebro` engine for humans and scripts — recall, store, inspect,
run maintenance — without the MCP or HTTP layers. Builds the binary named `cerebro`.

- **Key files:** `src/main.rs` (Cli / Command / Subcommand tree)
- **Depends on:** `cerebro`, `clap`, `tokio`, `serde`/`serde_json`, `chrono`, `uuid`, `tracing`.
- **Lift via:** run directly against a Cerebro DB; a clean clap-over-engine template.

Part of [ApexOS-RS](https://github.com/buckster123/ApexOS-RS) — see [`PATTERNS.md`](../../../PATTERNS.md) (lift-me index) and [`docs/repo-map.md`](../../../docs/repo-map.md) (full map).
