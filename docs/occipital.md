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

`occipital-mcp` is a separate binary, so `cargo build --release --workspace` does **not** produce it
— the `config/plugins.toml` entry ships **commented** (like Sonus), and the binary is built + deployed
on the node before activation.

### Build + deploy on a node

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
| **Fresh install** | install.sh seeds `config/plugins.toml` → uncomment the `occipital` block before first boot. |
| **Already deployed** | `/etc/agentd/plugins.toml` is *seed-if-absent*, so the repo change won't reach it. Either **(a)** APEX self-evolves it via the `register_mcp_server` tool, or **(b)** edit the live file by hand and restart agentd. |

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

*(Documented when the window lands — PR2.)*
