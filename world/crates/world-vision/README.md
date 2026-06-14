# world-vision

The **agent-vision MCP plugin** for `apexos-world`. A standalone stdio JSON-RPC server that
agentd spawns like any other plugin (`cerebro-mcp`, `apexos-tools`). It gives an *embodied*
agent the ability to **see** the 3D world — through its own avatar's camera, a station
camera, or a free camera — by calling a tool that returns a rendered image.

It owns **no renderer**. Every tool call is forwarded over a local IPC to the running
`world-app` process (the Slint + wgpu renderer), which does the offscreen render and pixel
readback. `world-vision` is the agentd-facing half; `world-app` is the GPU half.

```
agentd ──(stdio JSON-RPC)──► world-vision ──(unix-socket JSON)──► world-app (renderer)
   ▲                                                                    │
   └──────────── ToolResult: text manifest + base64 image ◄────────────┘
```

See `world/docs/DESIGN.md` (§2, §4, §6) and `world/docs/design/04-agent-embodiment-and-vision.md`
for the full design.

## Tools

| Tool | Args | Returns |
|------|------|---------|
| `world_look` | `view` (required: `"self"` \| `{avatar}` \| `{station}` \| `{free_cam}`), `width`, `height`, `format`, `annotate` | a text manifest of visible entities + a rendered image |
| `world_snapshot` | `width`, `height`, `format` | the Atrium overview shot as an image |

Both return MCP content blocks: a `text` manifest (the text-only fallback for non-vision
models) followed by an `image` block in the Anthropic `source:{type:"base64",media_type,data}`
shape that agentd's provider layer forwards to a vision model.

> **Scaffold status.** The IPC to `world-app` is **not yet wired** (`TODO(M2)` in
> `src/snapshot_client.rs`). Every call currently returns a 1×1 placeholder image so the MCP
> handshake, `tools/list`, and `tools/call` paths are testable against a live agentd today.
> Real renders land in milestone M2 (DESIGN.md §8).

## How it registers in `plugins.toml`

agentd discovers plugins from its `plugins.toml`. Add the block from
[`config/plugins.snippet.toml`](config/plugins.snippet.toml) to agentd's config (e.g.
`/etc/agentd/plugins.toml`):

```toml
[[plugin]]
id      = "world"
cmd     = "/usr/local/bin/world-vision"
args    = []
restart = "always"
[plugin.env]
WORLD_IPC_PATH = "/run/user/1000/apexos-world.sock"   # must match world-app's bind
RUST_LOG       = "warn"
```

On startup agentd's `Supervisor` spawns the binary, sends `initialize`, then `tools/list`,
advertises `world_look` + `world_snapshot` to the model, and routes any `tools/call` for them
to this process. **No agentd core change** — it plugs into the documented MCP extension
surface, exactly like `cerebro` and `apexos-tools`.

Policy: read-only vision tools default to `allow`; future world-mutating verbs default to
`ask` (snippet shows the `policy.toml` rules).

## How it talks to `world-app`

Local IPC = **newline-delimited JSON over a unix domain socket** at
`$XDG_RUNTIME_DIR/apexos-world.sock` (override with `WORLD_IPC_PATH`) — the same wire shape
as the MCP stdio transport. One request object per line, one reply per line, correlated by a
`req` id. Only `world-app` binds the socket; this plugin connects with backoff.

```jsonc
// plugin → world-app
{ "req": 7, "op": "look", "view": {"self": true}, "caller_session": 42,
  "width": 1024, "height": 576, "format": "jpeg", "annotate": true }

// world-app → plugin
{ "req": 7, "ok": true, "image_b64": "...", "media_type": "image/jpeg",
  "manifest": [ {"entity":"station:sensors","kind":"station","label":"Sensors","xy":[210,140]} ] }
```

If `world-app` is not running, every world tool returns a clean MCP `isError:true`
("world renderer not running") so the agent degrades gracefully and a turn never wedges.

> **Caller→avatar correlation (DESIGN.md R2).** agentd's `tools/call` does *not* forward the
> caller's `SessionId`, so `world_look{view:"self"}` cannot know which avatar's camera to use
> from agentd alone. Resolution: either the agent passes an explicit `avatar` id, or the
> plugin↔world-app IPC uses the `ActionId` nonce both sides observe. Until that lands,
> `"self"` should require an explicit target.

## Build & test

This crate is a **standalone package** (not yet a workspace member) with light deps and no
GPU — it builds and handshake-tests on a headless box:

```bash
cargo build --manifest-path world/crates/world-vision/Cargo.toml
cargo test  --manifest-path world/crates/world-vision/Cargo.toml
```

Manual smoke test (feed JSON-RPC on stdin; logs go to stderr, JSON-RPC to stdout):

```bash
printf '%s\n%s\n%s\n' \
  '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}' \
  '{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}' \
  '{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"world_snapshot","arguments":{}}}' \
  | ./target/debug/world-vision
```
