# Occipital integration — the agent's reading cortex

Occipital-RS is Cerebro's sibling: a standalone pure-Rust **web** layer (search · polite fetch ·
reader-mode · read-through cache · semantic recall · decay) behind MCP/CLI/REST. Repo:
[github.com/buckster123/Occipital-RS](https://github.com/buckster123/Occipital-RS). It is **not** a
member of this workspace — it ships and versions independently, exactly like Cerebro did before the
distro absorbed it. ApexOS-RS consumes it two ways, both **additive** (no agentd changes):

1. **As an MCP plugin** — register `occipital-mcp` so APEX gains five tools:
   `web_search` · `web_fetch` · `web_recall` · `web_save` · `web_forget`.
2. **As a follow-along reader window** — ui-slint renders each `web_fetch`/`web_search` result live,
   so a human watches the agent read (see *Follow-along window* below).

The seam between the two repos is the **flat, `kind`-discriminated tool result** — the design
contract lives in Occipital's own `docs/follow-along.md`. Occipital never knows about Slint or
agentd; ApexOS reads the result off the `tool_result` event and switches on `kind`.

## 1. Registering the MCP server (9a)

`occipital-mcp` is a separate binary from a **sibling repo** (not a workspace member), so
`cargo build --release --workspace` does **not** produce it. **install.sh now provisions it
automatically** — a fresh install (and every `apexos-update`) clones/pulls `Occipital-RS`, builds
`occipital-mcp`, installs it to `/usr/local/bin/`, creates `/var/lib/agentd/occipital`, and appends the
plugin block to `/etc/agentd/plugins.toml` (the `config/plugins.toml` entry stays **commented** — the
template never points agentd at a binary that may not exist; the live block is appended only on a
successful build). **Default ON**; skip with `--no-occipital` (or `APEXOS_NO_OCCIPITAL=1`), persisted
in `install.conf`. The repos stay separate — Occipital is cloned, not vendored into the workspace.

**Tier split** (mirrors cerebro's embed ladder): Micro/Standard/Pro build `--features embeddings` →
bge-small **semantic recall**; Nano builds without it → **FTS5 keyword recall** (no ONNX). Best-effort:
a clone/build failure warns and continues (agentd runs fine without the web cortex); the next
`apexos-update` retries.

### Manual build + deploy (dev, or a node where you skipped auto-provisioning)

```bash
# Nano/FTS5 default — no ONNX, keyword recall only (~small binary):
git clone https://github.com/buckster123/Occipital-RS && cd Occipital-RS
cargo build --release -p occipital-mcp
sudo cp target/release/occipital-mcp /usr/local/bin/
sudo install -d -o agentd -g agentd /var/lib/agentd/occipital

# Micro+ semantic recall: add ONNX embeddings (downloads bge-small ~127 MB on first embed)
cargo build --release -p occipital-mcp --features embeddings
#   then set OCCIPITAL_EMBED_MODEL = "BAAI/bge-small-en-v1.5" + FASTEMBED_CACHE_DIR in the plugin env.
```

The default (Nano) build excludes ONNX entirely and falls back to FTS5 keyword recall — `web_recall`
still works, just lexically. Match the build to the node's tier (see the CLAUDE.md tier ladder).

### Activate

| Node state | How to activate |
|------------|-----------------|
| **Fresh install** | Automatic — install.sh clones + builds Occipital-RS and registers `occipital-mcp` (default ON; `--no-occipital` to skip). Tier picks FTS5 vs `--features embeddings`. |
| **Already deployed** | `apexos-update` now provisions it: pulls/builds Occipital-RS, reinstalls the binary, and **appends** the plugin block to `/etc/agentd/plugins.toml` *if absent* (the grep is anchored to an uncommented `id = "occipital"`, so it's idempotent and won't duplicate an APEX `register_mcp_server` entry). The manual path above still works. |

Manual activation on a live node:

```bash
# append the occipital block to /etc/agentd/plugins.toml (see config/plugins.toml for the stanza), then:
sudo systemctl restart agentd
sudo journalctl -u agentd -n 20 --no-pager      # expect occipital tools in the registry
```

Env (in the plugin's `[plugin.env]`):

| Var | Purpose |
|-----|---------|
| `OCCIPITAL_DB` | SQLite cache + recall store (`/var/lib/agentd/occipital/occipital.db`) |
| `OCCIPITAL_KEYS_FILE` | `0600` provider-key store — optional Brave/Tavily/Bing keys |
| `OCCIPITAL_EMBED_MODEL` | Micro+ only — semantic recall model (needs `--features embeddings`) |
| `OCCIPITAL_SEARXNG_URL` | self-hosted SearXNG endpoint (otherwise DuckDuckGo HTML) |

Full env list: Occipital's `docs/build-roadmap.md`. Without any key, search uses DuckDuckGo HTML.

## 2. Follow-along reader window (9b/9c)

A native Slint window (`AppKind::Occipital`, 📖 in the Start menu under the tech apps,
next to 🌐 Web) that **mirrors what APEX reads** — a read-only follow-along browser. The
integration is additive: **no agentd changes**, reusing the tool-event side-channel + the
TurnGate, exactly like `display_face` / `sketch_snapshot`.

### How it consumes results (9b)

Every `web_fetch` / `web_search` / `web_recall` returns a flat, `kind`-discriminated object
(the contract in Occipital's `docs/follow-along.md`). agentd's MCP client passes it through as
the MCP content array `[{"type":"text","text":"<json>"}]` (`mcp.rs`), and `Event::ToolResult`
carries **no tool name** — so ui-slint detects an Occipital read by the payload's `kind`, not
the tool name (`occipital_payload()` recovers it from a bare object / JSON string / MCP array,
mirroring how `turn.rs` recovers the vision sentinel). It then switches on `kind`:

| `kind` | Rendered as |
|--------|-------------|
| `page` | reader-mode **markdown parsed natively** (Slint has no webview) into headings / paragraphs / bullets / blockquote / code / rule, plus the page's link list as clickable rows |
| `results` | ranked result rows (`#1…`), each with title · URL · snippet |
| `recall` | memory-hit rows with a cosine-score chip (`0.82`) or `kw` for FTS5 keyword hits |

Each row is clickable (the steer). A `● LIVE` / `● CACHED` badge (from `from_cache`) shows
freshness; a breadcrumb **trail** tracks the agent's path this session. The body is a std-widgets
`ScrollView` (the linuxkms no-wheel-scroll gotcha — a bare Flickable is unscrollable on the kiosk).

The window **auto-reveals the first time APEX browses** (so the human notices it start reading)
but won't re-pop if the user closes it — closing sets a suppress flag that relaunching from the
menu clears.

### The steer (9c)

Clicking a row — or typing into the window's URL bar — sends a queued `user_prompt`
*"(navigation) Go here next: &lt;url&gt;…"* over the WS. The gateway injects the session and it
funnels through the existing **TurnGate** like any user message, so it can't race the in-flight
turn (ApexOS's serialized-turn invariant). The agent finishes its step, sees the hint, and
`web_fetch`es the URL. The human is a collaborator in the agent's browsing, not a driver of a
separate browser.

### Dev / verify

`APEX_OCCIPITAL_DEMO=1` (or `=results` / `=recall`) opens the reader at launch with a sample
payload — no agentd, no network — so the window can be snapshotted via the screen-mirror server
(`APEXOS_UI_SNAPSHOT_ADDR`, `take_snapshot()`), the same way the GL face is verified.
