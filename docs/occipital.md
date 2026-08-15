# Occipital integration — the agent's reading cortex

Occipital-RS is Cerebro's sibling: a standalone pure-Rust **web** layer (search · polite fetch ·
reader-mode · read-through cache · semantic recall · decay) behind MCP/CLI/REST. Repo:
[github.com/buckster123/Occipital-RS](https://github.com/buckster123/Occipital-RS). It is **not** a
member of this workspace — it ships and versions independently, exactly like Cerebro did before the
distro absorbed it. ApexOS-RS consumes it two ways, both **additive** (no agentd changes):

1. **As an MCP plugin** — register `occipital-mcp` so APEX gains ten tools:
   `web_search` · `web_fetch` · `web_dom` · `web_click` · `web_submit` (the phase 12–16
   browsing verbs) · `web_recall` · `web_save` · `web_forget` · `web_distill` · `web_related`
   (the phase 17 knowledge-web walk).
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
| `OCCIPITAL_COOKIES` | opt-in persistent cookie jar (phase 15) — one jar, one identity, honest UA |
| `OCCIPITAL_HEADERS_FILE` | per-domain extra-header map (phase 15); UA stays honest |
| `OCCIPITAL_PROXY` | explicit egress proxy — topology, not evasion (phase 15) |
| `OCCIPITAL_ROBOTS_TTL_SECS` | robots.txt cache TTL (phase 16) |
| `OCCIPITAL_LOG_MAX` | request-trail ring size — `occipital log` / `GET /log` (phase 16) |
| `OCCIPITAL_AUTO_DISTILL` | opt-in background auto-curation (phase 11): `off` (default) · `local` (Ollama-pinned, never API) · `on` — budget-capped (see §1b) |

Full env list: Occipital's `CLAUDE.md` (§ Environment variables). Without any key, search uses
DuckDuckGo HTML.

### Policy rules

`config/policy.toml` seeds explicit rules for all ten `web_*` tools (`allow`, except
`web_submit` = `ask` — the one verb that can POST to a third party; note `web_click` on a
`form:N` element also submits, so tighten both to `ask` for a hard no-writes stance) —
without them a
suggest-mode node gates every web read behind the `unknown → ask` fallthrough (the standing
policy gotcha). **Already-deployed nodes gain them on the next `apexos-update`**: install.sh
additively syncs any `[rules]` key present in the shipped config but missing live into
`/etc/agentd/policy.toml` (`sync_policy_rules`, 2026-07-04 — existing/self-evolved values are
never overwritten; hardened 2026-07-20: the presence check is quote-insensitive, quoted
duplicates are healed first with the self-evolved line winning, and every transform is
TOML-validated before it lands).

## 1b. The knowledge hub — LLM curation (`web_distill` + `web_related`)

Occipital Phase 10 (ApexOS BACKLOG Top-10 #10, slice 1): the cache grows a **distillation
layer**. `web_distill` curates an already-read page into knowledge — a 2–4 sentence
**summary**, **key points**, **entities**, and **topic tags** — via a tiered LLM backend that
mirrors Cerebro's `describe_image` exactly:

| `OCCIPITAL_CURATE_BACKEND` | Behaviour |
|---------------------------|-----------|
| `auto` (default) | local/LAN Ollama first, Anthropic API fallback when `ANTHROPIC_API_KEY` is present |
| `ollama` | only `OCCIPITAL_CURATE_URL` (default `localhost:11434`, LAN-swappable) · model `OCCIPITAL_CURATE_MODEL` (default `llama3.2`) |
| `anthropic` | only the API — model `OCCIPITAL_CURATE_API_MODEL` (default `claude-haiku-4-5`) |
| `off` | `web_distill` returns an honest error |

The API fallback still sees `ANTHROPIC_API_KEY` from `/etc/agentd/env`: `spawn_plugin`
`env_clear`s the child, then `plugin_child_env` inherits that key **only** for the
`occipital` and `cerebro` plugin ids. `apexos-tools` never receives it (finding 11).

What it changes for the agent:

- `web_distill {url}` — distill that page (fetched first if uncached). A re-ask on unchanged
  content (hash-gated) is **free** — no LLM call.
- `web_distill {}` — sweep a bounded batch (default 3, ≤10) of never-distilled /
  content-changed pages; per-page fail-soft, returns a `remaining` count. Cron-able via the
  CLI (`occipital distill`) for a nightly consolidation pass later.
- `web_recall` now returns a distilled page as its **summary + tags** (`distilled: true`)
  instead of a raw-body snippet — recall serves knowledge, not HTML dregs. Distilled
  tags/entities/key-points are FTS-indexed, so **Nano keyword recall finds pages by curated
  terms too** (no embeddings needed).
- `web_related {url}` — walk the knowledge web from a curated page (Occipital phase 17):
  every other distilled page sharing its entities (×2) or tags (×1), best-tied first, with
  the shared terms as the edge label. **Zero network, zero LLM** — a pure local read over the
  distillations store; an undistilled page gets an honest "distill it first", and an empty
  result carries `distilled_total` so "unconnected" and "nothing curated yet" read
  differently. Every fresh distillation also carries its top-3 neighbours inline.
- The `kind:"distill"` and `kind:"related"` payloads both **render in the reader window**
  (the distill card shipped 2026-07-28) — see the kind table below.

**Distillation is explicit-only by default** — nothing spends tokens behind the operator's
back. Opt-in background auto-curation shipped (Occipital phase 11): `OCCIPITAL_AUTO_DISTILL=local`
(Ollama-pinned — never spends API tokens) or `=on` (the configured backend, API fallback
included) sweeps newly-read pages every `OCCIPITAL_AUTO_DISTILL_INTERVAL_SECS` (default 300,
floor 30), budget-guarded by a rolling-24h cap (`OCCIPITAL_AUTO_DISTILL_CAP`, default 50 —
counts explicit distills too, so it's a total-spend guard). The sqlite-vec ANN index also
shipped (phase 18): a vec0 `page_vectors` index leads `web_recall` at scale, with brute-force
cosine as the automatic fail-soft fallback (Nano builds compile the layer out). The remaining
knowledge-hub slices — semantic dedup on ingest, digests ("what did I read about X"),
freshness/re-distill — are tracked in `BACKLOG.md` #10 + Occipital's roadmap.

## 2. Follow-along reader window (9b/9c)

A native Slint window (`AppKind::Occipital`, 📖 in the Start menu under the tech apps,
next to 🌐 Web) that **mirrors what APEX reads** — a read-only follow-along browser. The
integration is additive: **no agentd changes**, reusing the tool-event side-channel + the
TurnGate, exactly like `display_face` / `sketch_snapshot`.

### How it consumes results (9b)

Every Occipital verb returns a flat, `kind`-discriminated object
(the contract in Occipital's `docs/follow-along.md`). agentd's MCP client passes it through as
the MCP content array `[{"type":"text","text":"<json>"}]` (`mcp.rs`), and `Event::ToolResult`
carries **no tool name** — so ui-slint detects an Occipital read by the payload's `kind`, not
the tool name (`occipital_payload()` recovers it from a bare object / JSON string / MCP array,
mirroring how `turn.rs` recovers the vision sentinel). It then switches on `kind`:

| `kind` | Rendered as |
|--------|-------------|
| `page` | reader-mode **markdown parsed natively** (Slint has no webview) into headings / paragraphs / bullets / blockquote / code / rule — plus inline `[form#N → …]` annotations styled as ⌨ affordance blocks — and the page's link list as clickable rows. `salvaged` pages say so in the meta line; a `js_required` page renders an honest "needs JavaScript" state instead of a blank fetch |
| `results` | ranked result rows (`#1…`), each with title · URL · snippet |
| `recall` | memory-hit rows with a cosine-score chip (`0.82`) or `kw` for FTS5 keyword hits |
| `dom` | the element registry (`web_dom`): links with their click ordinals (`#N`) + forms as non-clickable rows (method+action, visible fields, submit label) — what APEX can act on |
| `click` | the landed page (same layout as `page`); the meta line shows the hand: `clicked link:3 → url · HTTP 200` |
| `submit` | the response page; meta shows the submission: `form#1 GET action — q=… · HTTP 200`. Freshness badge reads `cached` (a POST result is never cached) |
| `distill` | per-page **knowledge cards** — title, summary, key-point bullets, a 🏷 tags/entities line; failures and the undistilled backlog surfaced honestly in the meta; distilled pages double as steerable rows chipped with their backend (`ollama` / `anthropic` / `cache`). A single-page distill gets page-like identity: title/url up top + a `live`/`cache` freshness badge |
| `related` | neighbour rows of a curated page — the shared entities/tags (🏷) lead each detail line, the overlap score is the chip; the meta counts connected pages against `distilled_total`, so "unconnected" and "nothing curated yet" read differently |

**Field-pass additions (2026-07-26, Occipital-RS PRs #11–#17):** the payloads carry more than
the table shows — all additive, and the reader ignores what it doesn't render. `links` is
capped at 120 on the wire with `links_total` the true count (`web_dom` windows via
`links_from`/`limit`, ordinals stay full-list); a POST result carries a `handle`
(`result:<id>`, ~15 min working memory) usable as the next verb's `url`, and handle-sourced
interactions echo `from_handle`; forms carry `submittable` (a dead GET form — no named
fields — is refused by `web_submit`); pages may carry `markdown_alternate` (an authored-`.md`
offer, reported never auto-followed) and `source_format` (`"markdown"`/`"text"` = the body
passed through verbatim — `.md`/`llms.txt` responses skip the HTML extractor entirely, so
their link registries survive and `web_click` traverses them). Full contract + the field-loop
story: Occipital-RS `docs/agent-browsing.md`.

Each row is clickable (the steer). A `● LIVE` / `● CACHED` badge (from `from_cache`) shows
freshness; a breadcrumb **trail** tracks the agent's path this session. The body is a std-widgets
`ScrollView` (the linuxkms no-wheel-scroll gotcha — a bare Flickable is unscrollable on the kiosk).

The window **auto-reveals the first time APEX browses** (so the human notices it start reading)
but won't re-pop if the user closes it — closing latches the reader via the shared adaptive-UI
latch (A3: the old standalone suppress flag folded into it; the latch also blocks agent `ui_open`
for the reader), and relaunching from the menu re-invites (clears the latch). See
`docs/adaptive-ui.md`.

### The steer (9c)

Clicking a row — or typing into the window's URL bar — sends a queued `user_prompt`
*"(navigation) Go here next: &lt;url&gt;…"* over the WS. The gateway injects the session and it
funnels through the existing **TurnGate** like any user message, so it can't race the in-flight
turn (ApexOS's serialized-turn invariant). The agent finishes its step, sees the hint, and
`web_fetch`es the URL. The human is a collaborator in the agent's browsing, not a driver of a
separate browser.

### Dev / verify

`APEX_OCCIPITAL_DEMO=1` (or `=results` / `=recall` / `=dom` / `=click` / `=submit` /
`=distill` / `=related`) opens the reader at launch with a sample payload for that kind — no
agentd, no network — so the window can be snapshotted via the screen-mirror server
(`APEXOS_UI_SNAPSHOT_ADDR`, `take_snapshot()`), the same way the GL face is verified.
