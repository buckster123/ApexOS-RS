# ApexOS-RS Extension SDK

ApexOS-RS is a pure-Rust native UI distro: one Cargo workspace holding the agent
daemon (`agentd`), the cognitive memory cortex (`cerebro`), the system-tool
plugins (`apexos-tools`), and the Slint native UI (`ui-slint`). This SDK
documents every **extension surface** of that stack — the places you (a human
developer, or APEX/FORGE at runtime) hook in new behaviour without rewriting the
core.

The whole system is glued together by **one wire contract**: agentd's `Event`
enum, serialized as JSON over `ws://HOST:8787/ws`. Tools, memory verbs, UI
views, mesh routing, and self-evolution are all *downstream* of that contract.
Read guide 01 first if you touch anything that crosses the daemon↔client
boundary.

> Every guide is ground-truthed against the source with `file:line` anchors.
> Where a guide contradicts `CLAUDE.md`, **the guide is correct and CLAUDE.md is
> stale** — the guides were written after a direct source audit.

---

## The guides

| # | Guide | Extend this when you want to… |
|---|-------|-------------------------------|
| 01 | [Core types & the WebSocket/event protocol](01-core-and-protocol.md) | Add a new `Event` variant, a new frontend intent, or write an alternate client (browser/PWA/CLI) over the JSON wire contract. |
| 02 | [MCP plugins](02-mcp-plugins.md) | Add a whole new tool-providing process (any language) that agentd spawns over stdio JSON-RPC and registers in `plugins.toml`. |
| 03 | [Adding a tool to `apexos-tools`](03-adding-tools.md) | Give the agent a new plain-Rust local-system capability (command, file/device op, small HTTP call) inside the built-in tool plugin. |
| 04 | [Cerebro: memory for agents](04-cerebro-for-agents.md) | Add a new memory verb to the ~66-tool cortex, or learn the Wake→Perceive→Act→Sleep verbs an agent uses to keep continuity. |
| 05 | [Building a desktop app / UI view](05-desktop-apps.md) | Add a new visible window/view to the Slint native UI, fed by WS events or `/api/*` polls. |
| 06 | [Self-evolution & policy](06-self-evolution-and-policy.md) | Let APEX change its own `soul.md` / `policy.toml` / plugin set at runtime via `propose_evolution`, with audit + rollback. |
| 07 | [Mesh colony & deployment](07-mesh-and-deploy.md) | Add a hardware tier or deployment mode, join a mesh node, define a vast.ai GPU recipe, or ship a new hardened systemd service. |

Two cross-cutting references live alongside the guides:

- **[llms.txt](llms.txt)** — the agent entry point. An LLM extending the system
  should read this first to orient on which surface owns what.
- **[extension-manifest.md](extension-manifest.md)** — the consolidated
  "to add X, edit these files, follow this schema" recipe table for every
  extension point, plus the full tool/event catalog. Use it for fast recall.

---

## Start here

### If you're a human developer

1. Read **[01 — Core & protocol](01-core-and-protocol.md)** for the wire model
   (`Event` enum, the bus, the gateway read/write path). Everything couples
   through it.
2. Jump to the guide for your surface (table above). Each one is a complete,
   self-contained walkthrough with a minimal worked example.
3. Build + hot-swap on the Pi (the SDK changes are code commits, not runtime
   knobs): `cargo build --release -p <crate>`, then
   `systemctl stop <svc> && cp target/release/<bin> /usr/local/bin/ && systemctl start <svc>`.
   See `CLAUDE.md` → *Deploy workflow*.

### If you're an agent (APEX / FORGE)

1. Read **[llms.txt](llms.txt)** — it routes you to the right surface in one
   hop.
2. **What you can change at runtime, yourself:** only config — `soul.md`,
   `policy.toml`, the plugin set — and only through `propose_evolution`
   ([guide 06](06-self-evolution-and-policy.md)). That path is policy-gated,
   journaled, and reversible. Always `memory_store` the *why* (salience ≥ 0.9,
   tag `evolution`) — the daemon records the undo but never the rationale.
3. **What needs a human:** any new Rust code — a new `Event` variant, a new
   `apexos-tools` tool, a new UI view, a new compiled MCP binary. You can
   *propose the code* and journal the intent, but the build + hot-swap is a
   human/CI step. Adding a `[[plugin]]` stanza is reachable via
   `register_mcp_server` **only if the binary already exists on disk**.
4. The safety boundary is never the protocol or the tool code — it's the
   `PolicyEngine` plus the systemd sandbox. See each guide's *needs-approval*
   notes and [extension-manifest.md](extension-manifest.md) → *Catalog*.
