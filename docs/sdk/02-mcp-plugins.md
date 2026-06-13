# SDK · Writing an MCP Plugin

> An **MCP plugin** is a child process `agentd` spawns at boot, talks to over
> newline-delimited JSON-RPC on stdio, and exposes as a set of agent **tools**.
> `cerebro-mcp` (memory) and `apexos-tools` (shell/file/http/GPIO/…) are the two
> stock plugins; both are plain binaries in this workspace. You extend this
> surface when you want APEX to be able to *do something new in the world* — call
> a new API, drive new hardware, expose a new capability — without touching the
> daemon, the turn engine, or the wire protocol. Drop a binary on disk, add a
> stanza to `plugins.toml`, and its tools appear in the agent's toolset on the
> next restart.

This is one of three tool surfaces. Pick the right one:

| You want to… | Surface | Where |
|---|---|---|
| Add an external capability that is its own process (API call, device, language other than Rust) | **MCP plugin** *(this doc)* | new binary + `plugins.toml` |
| Add a tool that needs daemon-internal state (bus, scheduler, soul.md, evolution) | **virtual tool** | `gather_tools` + `dispatch_tool` (see `agentd/crates/agentd/src/main.rs`, `plugins/src/supervisor.rs`) |
| Add a system tool to the existing shell/file plugin | edit `apexos-tools` | `tools/crates/apexos-tools/src/tools.rs` |

---

## Concepts

### The mental model

```
                              spawn (stdin/stdout piped, kill_on_drop)
  agentd ── Supervisor ─────────────────────────────────────────────►  your-mcp  (child process)
              │                                                            │
              │  1. initialize        ──── JSON-RPC line ───►             │ reply: serverInfo
              │  2. notifications/initialized ──────────────►             │ (no reply)
              │  3. tools/list        ──── JSON-RPC line ───►             │ reply: { tools: [ToolSpec…] }
              │       registers each tool name → your plugin id           │
              │                                                            │
   ToolRequested on bus ─► PolicyEngine.check ─► dispatch_tool ─►  4. tools/call ─►  your tool fn
              │                                                            │ reply: { content, isError? }
              ◄──── Event::ToolResult on bus ◄──── McpClient parses reply ─┘
```

Four facts to hold in your head:

1. **stdio is the channel, stderr is yours.** stdout carries *only* newline-delimited
   JSON-RPC. Anything you print to stdout that isn't a valid JSON-RPC line is
   silently dropped by the client reader (`mcp.rs:60-62`). **All logging must go to
   stderr** — `agentd` already pipes your stderr to its own log prefixed with
   `[plugin:<id>]` (`supervisor.rs:1389-1398`).
2. **One tool name → one plugin.** At `tools/list` time the supervisor inserts every
   returned tool name into a flat `tool_registry: HashMap<String, PluginId>`
   (`supervisor.rs:1404-1406`). Names are global; a name collision means last plugin
   to start wins. Namespace yours (`weather_now`, not `now`).
3. **No request timeout on the call path.** `McpClient` has no per-request timeout
   (`mcp.rs` — a `tools/call` that never replies blocks that call's oneshot forever).
   The agent turn engine has its own bounded wait that synthesizes an error so a turn
   never wedges, but your plugin must always reply to every request that carries an
   `id`. Don't hang.
4. **The protocol version is pinned.** agentd negotiates MCP `2024-11-05`
   (`mcp.rs:16`). Echo it back.

### The real types and files

| Thing | File:line | Role |
|---|---|---|
| `McpClient` | `agentd/crates/plugins/src/mcp.rs:23` | the JSON-RPC client agentd attaches to your child's stdio. `attach` → `initialize` → `list_tools` → `call_tool`. |
| `initialize` handshake | `mcp.rs:82-90` | sends `initialize` then the `notifications/initialized` notification. |
| `list_tools` | `mcp.rs:93-108` | reads `result.tools[]`, maps each `{name, description, inputSchema}` → `ToolSpec`. |
| `call_tool` | `mcp.rs:111-122` | sends `tools/call {name, arguments}`; maps reply → `ToolOutput { ok: !isError, content }`. |
| `Supervisor::spawn_plugin` | `supervisor.rs:1364-1420` | spawns child with stdio piped + `kill_on_drop(true)`, attaches `McpClient`, runs the handshake, registers tools, emits `PluginUp`, and starts the death-watcher. |
| `Supervisor::dispatch_tool` | `supervisor.rs:299-1362` | the dispatch chain: virtual tools first, then `tool_registry` lookup → `client.call_tool` → `Event::ToolResult`. Real plugins are the fall-through at `:1331-1348`. |
| `Supervisor::handle_died` | `supervisor.rs:1422-1448` | restart logic keyed on `RestartPolicy`. |
| `PluginConfig` / `RestartPolicy` | `agentd/crates/plugins/src/config.rs:5-24` | the `[[plugin]]` stanza shape. |
| `ToolProxy::call` | `supervisor.rs:49-63` | direct call path (10 s timeout) used by daemon-internal code, bypasses policy. |
| `ToolSpec` / `ToolOutput` / `ToolCall` | `agentd/crates/core/src/types.rs:288 / 282 / 273` | the in-daemon representations. |
| Reference server (Rust, sync) | `tools/crates/apexos-tools/src/main.rs` | minimal stdio loop — the template to copy. |
| Reference server (Rust, async + state) | `cerebro/crates/cerebro-mcp/src/{main.rs,transport.rs}` | tokio loop holding an `Arc<CerebroCortex>`. |

### The wire contract agentd expects

agentd is **not** a full MCP client — it speaks exactly the subset below. Implement
these four messages and nothing else is required.

**1. `initialize` (request → response).** agentd sends:
```json
{"jsonrpc":"2.0","id":1,"method":"initialize",
 "params":{"protocolVersion":"2024-11-05","capabilities":{},
           "clientInfo":{"name":"agentd","version":"…"}}}
```
You must reply with a result echoing the version:
```json
{"jsonrpc":"2.0","id":1,"result":{
   "protocolVersion":"2024-11-05",
   "capabilities":{"tools":{}},
   "serverInfo":{"name":"your-mcp","version":"0.1.0"}}}
```

**2. `notifications/initialized` (notification — NO id, NO reply).** agentd sends it
right after the handshake (`mcp.rs:89`). It has no `id`; you must **not** answer it.
The reference servers `continue` past it (`apexos-tools/src/main.rs:35`).

**3. `tools/list` (request → response).** Reply with your manifest:
```json
{"jsonrpc":"2.0","id":2,"result":{"tools":[
  {"name":"weather_now",
   "description":"Current weather for a city.",
   "inputSchema":{"type":"object",
     "properties":{"city":{"type":"string"}},
     "required":["city"]}}
]}}
```
`name` is mandatory (a tool entry missing it makes `list_tools` return an error
that aborts the **whole** manifest, not just that one entry — `mcp.rs:101-102`); `description`
defaults to `""`; `inputSchema` defaults to `{}` but **always provide a real JSON
Schema** — it is what the LLM sees to decide how to call your tool.

**4. `tools/call` (request → response).** agentd sends `{name, arguments}`:
```json
{"jsonrpc":"2.0","id":7,"method":"tools/call",
 "params":{"name":"weather_now","arguments":{"city":"Oslo"}}}
```
Reply with an MCP tool result. The **only** field agentd reads back is `content`
(opaque, passed through verbatim) and the optional `isError` boolean
(`mcp.rs:117-121`):
```json
{"jsonrpc":"2.0","id":7,"result":{
   "content":[{"type":"text","text":"Oslo: 4°C, clear"}]}}
```
On failure set `"isError": true` in the result. agentd maps
`ok = !isError`, `content = result.content`. **Do not** put errors in a top-level
JSON-RPC `error` object for tool failures — that path (`mcp.rs:140-142`) is treated
as a transport/protocol error and aborts the call, not a clean tool error the agent
can read and recover from.

> **Result-shape convention used by the stock plugins.** apexos-tools wraps every
> payload as `{"content":[{"type":"text","text":<stringified-json>}]}` and adds
> `"isError":true` on failure (`tools.rs:333-339`). Follow this: a single
> `text` content block whose text is your JSON, stringified. The agent then sees
> the stringified JSON as the tool's output.

---

## Add a new MCP plugin

Steps 1–6 build a Rust plugin in this workspace. Step 7 covers a non-Rust binary.

**1. Create the crate.** Add `tools/crates/<your-mcp>/` (or anywhere; the binary
location is what matters). `Cargo.toml`:
```toml
[package]
name = "weather-mcp"
version = "0.1.0"
edition = "2021"

[[bin]]
name = "weather-mcp"
path = "src/main.rs"

[dependencies]
serde_json = "1"
reqwest    = { version = "0.12", features = ["blocking", "json"] }
```
Add the crate to the workspace `members` in the root `Cargo.toml` so
`cargo build --release --workspace` picks it up. *(That root edit is outside the SDK
sandbox — note it for the human/agent applying the change.)*

**2. Implement the stdio loop.** Copy the shape of `apexos-tools/src/main.rs` — a
blocking `for line in stdin.lock().lines()` loop handling the four methods. Keep the
manifest (`list`) and dispatch (`call`) in a `tools` module. (Full code in the worked
example below.)

**3. Use the stock result helpers.** `tool_ok(json)` →
`{"content":[{"type":"text","text":json.to_string()}]}`; `tool_error(msg)` → same
plus `"isError":true`. (`apexos-tools/src/tools.rs:333-339`.)

**4. Log only to stderr.** `eprintln!`, never `println!`. stdout is JSON-RPC only.

**5. Build and place the binary.**
```bash
cargo build --release -p weather-mcp
sudo cp target/release/weather-mcp /usr/local/bin/weather-mcp   # on the Pi
```
(Always build on the Pi — never cross-compile. Stop the service first if you are
overwriting a running binary: `text file busy`.)

**6. Register it in `plugins.toml`.** Add a `[[plugin]]` stanza to
`/etc/agentd/plugins.toml` (deployed copy) — its schema is `PluginConfig`
(`config.rs:5-15`):
```toml
[[plugin]]
id      = "weather"                       # unique; PluginId, only for logs/registry
cmd     = "/usr/local/bin/weather-mcp"    # absolute path to the binary
args    = []                              # argv, optional
restart = "always"                        # always | on-failure | never (default never)
[plugin.env]                              # optional, injected into the child env
WEATHER_API_KEY = "…"
```
Restart agentd: `sudo systemctl restart agentd`. On boot the supervisor spawns every
`[[plugin]]`, runs the handshake, and logs `plugin '<id>' up — N tools`
(`supervisor.rs:1408`). Confirm with `journalctl -u agentd | grep weather`.

> The default `config/plugins.toml` in the repo is the install-time template; the
> *live* file is `/etc/agentd/plugins.toml` (chowned to `agentd` so self-evolution can
> append to it). Edit the deployed file, not just the repo template, for an existing node.

**7. Non-Rust plugins.** Any language works — agentd only cares about the stdio
JSON-RPC contract. Point `cmd` at the interpreter and pass the script in `args`:
```toml
[[plugin]]
id   = "sonus"
cmd  = "/usr/bin/python3"
args = ["-m", "sonus_mcp"]
restart = "always"
[plugin.env]
SUNO_DOWNLOAD_DIR = "/var/lib/agentd/workspace/sonus"
```
(There is a commented `sonus` stanza in `config/plugins.toml:23-28`, but note it
points `cmd` at a native binary `/usr/local/bin/sonus-mcp`, not at a Python
interpreter — adapt it to the `cmd`=interpreter + `args`=script form shown here for a
real Python server.)
A Python server must read a line from stdin, parse JSON-RPC, and write one JSON line
per request to stdout — same four methods.

### Restart / supervision semantics (`RestartPolicy`)

| `restart` value | On clean exit *or* crash | Used by |
|---|---|---|
| `always` *(stock plugins)* | restarted after a 1 s backoff (`supervisor.rs:1441-1447`) | cerebro, apexos-tools |
| `on-failure` | **note:** `handle_died` only auto-restarts on `Always` (`:1441`). `on-failure` parses but is **not** distinguished from `never` in the restart path today — treat it as "no auto-restart" until that gap closes. | — |
| `never` *(default)* | not restarted | one-shot helpers |

Other supervision facts:
- The child is spawned with `kill_on_drop(true)` (`supervisor.rs:1376`) — if the
  supervisor task is dropped, the child is killed too. No orphans.
- A death-watcher task `child.wait()`s and sends `SupervisorCmd::PluginDied`
  (`supervisor.rs:1411-1415`), which triggers `PluginDown` on the bus + the restart
  policy.
- A live plugin can be hot-reloaded or killed at runtime via `SupervisorCmd::HotReload`
  / `KillPlugin` (`supervisor.rs:220-252`) — these are how the evolution applier
  adds/removes plugins without a daemon restart after editing `plugins.toml`.
- When a plugin comes up its tools are added to the toolset agentd advertises to the
  LLM on the **next turn** (`gather_tools`); a plugin added mid-session is not visible
  to the in-flight turn.

---

## Worked example — a `weather_now` tool from scratch

A complete plugin exposing one tool. Reads `WEATHER_API_KEY` from env, calls an HTTP
API, returns the temperature.

**`tools/crates/weather-mcp/Cargo.toml`** — as in step 1 above.

**`tools/crates/weather-mcp/src/main.rs`:**
```rust
use serde_json::{json, Value};
use std::io::{self, BufRead, Write};

fn main() {
    let stdin = io::stdin();
    let mut out = io::stdout().lock();

    for line in stdin.lock().lines() {
        let line = match line {
            Ok(l) if !l.trim().is_empty() => l,
            _ => continue,
        };
        let req: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,                 // not JSON — drop, never crash
        };

        let id     = req.get("id").cloned().unwrap_or(Value::Null);
        let method = req["method"].as_str().unwrap_or("");

        let response = match method {
            "initialize" => json!({
                "jsonrpc": "2.0", "id": id,
                "result": {
                    "protocolVersion": "2024-11-05",
                    "capabilities": { "tools": {} },
                    "serverInfo": { "name": "weather-mcp", "version": "0.1.0" }
                }
            }),
            // Notification — no id, no reply. agentd sends this once after init.
            "notifications/initialized" => continue,
            "tools/list" => json!({
                "jsonrpc": "2.0", "id": id,
                "result": { "tools": list() }
            }),
            "tools/call" => {
                let p    = &req["params"];
                let name = p["name"].as_str().unwrap_or("");
                let args = p.get("arguments").cloned().unwrap_or(json!({}));
                json!({ "jsonrpc": "2.0", "id": id, "result": call(name, &args) })
            }
            _ => json!({
                "jsonrpc": "2.0", "id": id,
                "error": { "code": -32601, "message": "method not found" }
            }),
        };

        // stdout = JSON-RPC only. One line per response. Flush every time.
        let _ = writeln!(out, "{}", serde_json::to_string(&response).unwrap());
        let _ = out.flush();
    }
}

fn list() -> Value {
    json!([{
        "name": "weather_now",
        "description": "Current weather for a city.",
        "inputSchema": {
            "type": "object",
            "properties": { "city": { "type": "string", "description": "City name" } },
            "required": ["city"]
        }
    }])
}

fn call(name: &str, args: &Value) -> Value {
    match name {
        "weather_now" => weather_now(args),
        other => tool_error(format!("unknown tool: {other}")),
    }
}

fn weather_now(args: &Value) -> Value {
    let Some(city) = args["city"].as_str() else {
        return tool_error("missing 'city'");
    };
    let key = match std::env::var("WEATHER_API_KEY") {
        Ok(k) => k,
        Err(_) => return tool_error("WEATHER_API_KEY not set"),
    };
    let url = format!(
        "https://api.openweathermap.org/data/2.5/weather?q={city}&units=metric&appid={key}"
    );
    eprintln!("[weather] GET {city}");          // logging → stderr, never stdout
    match reqwest::blocking::get(&url).and_then(|r| r.json::<Value>()) {
        Ok(body) => {
            let temp = body["main"]["temp"].as_f64().unwrap_or(f64::NAN);
            let desc = body["weather"][0]["description"].as_str().unwrap_or("?");
            tool_ok(json!({ "city": city, "temp_c": temp, "conditions": desc }))
        }
        Err(e) => tool_error(format!("weather API error: {e}")),
    }
}

// Stock result envelope — matches apexos-tools/src/tools.rs:333-339.
fn tool_ok(content: Value) -> Value {
    json!({ "content": [{ "type": "text", "text": content.to_string() }] })
}
fn tool_error(msg: impl Into<String>) -> Value {
    json!({
        "content": [{ "type": "text", "text": json!({ "error": msg.into() }).to_string() }],
        "isError": true
    })
}
```

**Wire it in** (`/etc/agentd/plugins.toml`):
```toml
[[plugin]]
id      = "weather"
cmd     = "/usr/local/bin/weather-mcp"
restart = "always"
[plugin.env]
WEATHER_API_KEY = "REPLACE_ME"
```

**Build, deploy, verify:**
```bash
cargo build --release -p weather-mcp
sudo systemctl stop agentd
sudo cp target/release/weather-mcp /usr/local/bin/weather-mcp
sudo systemctl start agentd
journalctl -u agentd -n 20 --no-pager | grep -i weather   # expect: plugin 'weather' up — 1 tools
```

**Manual protocol smoke test** (no daemon needed — exercises the exact bytes agentd
sends):
```bash
printf '%s\n%s\n%s\n' \
 '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{}}}' \
 '{"jsonrpc":"2.0","id":2,"method":"tools/list"}' \
 '{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"weather_now","arguments":{"city":"Oslo"}}}' \
 | WEATHER_API_KEY=test ./target/release/weather-mcp
```
You should see three JSON lines back (init result, tools array, tool result).

**The call path once live:** when the agent calls `weather_now`, `run_turn` emits
`Event::ToolRequested` → Supervisor's `PolicyEngine.check("weather_now", None)`
(`supervisor.rs:163`) decides Allow/Ask → on Allow, `dispatch_tool` falls past every
virtual tool to the registry lookup (`:1331`), finds `weather` owns the name, calls
`client.call_tool` (`mcp.rs:111`), wraps the reply as `ToolOutput`, and emits
`Event::ToolResult` back onto the bus (`:1344`). The UI renders it in a tool card; the
turn engine feeds `content` back to the LLM.

---

## Policy / safety

Your plugin's tools are **subject to the same approval policy as every other tool** —
the supervisor checks policy before it ever calls you (`supervisor.rs:161-178`).

- **Approval gate.** Before dispatch, `PolicyEngine.check(tool_name, path)`
  (`policy.rs:88`) returns `Allow` or `Ask`. On `Ask`, the supervisor emits
  `ApprovalPending`, holds your call, and only dispatches after a matching
  `UserApproval { granted: true }` arrives (`supervisor.rs:168-201`). On deny it
  short-circuits to a `ToolResult { ok: false, content: "denied by user" }` and
  never calls you.
- **How rules match your tool.** Rules in `config/policy.toml` key on the tool name,
  matched exact-first then `prefix.*` wildcard (`policy.rs:141-147`). To auto-allow a
  read-only tool, add a rule like `weather_now = "allow"`. `Yolo` mode short-circuits
  to Allow for everything (`policy.rs:89`); the default `suggest` mode asks for any
  tool not explicitly allowed. **A tool with no rule defaults to Ask** in `suggest`
  mode — so a brand-new write-capable tool is gated until someone approves it or a
  rule is added. That is the safe default; do not weaken it without intent.
- **The `path` argument is policy-visible.** `check` reads `call.args["path"]`
  (`supervisor.rs:162`) and `Rule::Workspace` confines path-bearing tools to the
  workspace. If your tool touches the filesystem, name the argument `path` so the
  workspace rule can see it. *Caveat:* there is **no workspace rooting on reads** in
  the stock policy — "allowed" is not "sandboxed."
- **The real confinement is the systemd sandbox, not the tool layer.** Your plugin is
  a child of `agentd`, which runs as the unprivileged `agentd` user under
  `NoNewPrivileges`, `ProtectSystem=strict`, `ProtectHome`, `PrivateTmp`, and a
  `ReadWritePaths` allowlist (`deploy/agentd.service`). Your tool can do anything that
  user can do and nothing more. Do not rely on the policy denylist as a security
  boundary — apexos-tools' `run_command` denylist is a soft substring heuristic,
  trivially bypassable. Treat the sandbox as the perimeter; if your tool needs more
  access (a device node, a writable path), that is a deliberate sandbox change, not a
  plugin detail.
- **Secrets.** Pass API keys via `[plugin.env]`, not `args`. The env block is in
  `/etc/agentd/plugins.toml`, which is `agentd`-readable but not world-readable; args
  are logged to the event log (`run_log_writer`) and visible in the UI tool card.

### For agents self-extending at runtime

- **Adding a plugin is a config evolution, not a code change you can do yourself.**
  You can append a `[[plugin]]` stanza to `plugins.toml` via `propose_evolution`
  (the applier writes `plugins.toml` and can `SpawnPlugin` the new entry live), but
  the **binary must already exist on disk** — you cannot compile a new Rust binary
  into being from a turn. A new-binary plugin is a human-deploy step; a *re-point* or
  *env change* of an existing binary is within reach of evolution.
- **Audit discipline.** A plugin change is a capability change to the whole node.
  Treat it like any evolution: state the reason in the proposal, expect it to be
  journaled to a Cerebro episode and the in-memory rollback store by the applier
  (`spawn_evolution_applier`), and know it is `rollback_evolution`-able. Record the
  intent and outcome in a `session_save` / `store_procedure` so the next session knows
  the capability exists and why.
- **Self-inflicted footguns.** A new tool whose name collides with an existing one
  silently steals the binding (last-up wins). Namespace deliberately. A plugin with
  `restart = "always"` that crashes on startup will hot-loop with a 1 s backoff and
  spam the log — verify it survives the handshake before shipping it as `always`.

---

## Reference

### `[[plugin]]` stanza — `PluginConfig` (`config.rs:5-15`)

| Field | Type | Required | Default | Meaning |
|---|---|---|---|---|
| `id` | string | yes | — | unique `PluginId`; used for the tool registry owner + log prefix |
| `cmd` | string | yes | — | absolute path to the binary/interpreter to spawn |
| `args` | string[] | no | `[]` | argv passed to `cmd` |
| `restart` | enum | no | `never` | `always` \| `on-failure` \| `never` (kebab-case) |
| `cwd` | string | no | — | working directory for the child |
| `env` | table | no | — | env vars injected into the child (`[plugin.env]`) |

### `RestartPolicy` (`config.rs:17-24`)

| TOML value | Variant | Restart behaviour (`handle_died`, `:1441`) |
|---|---|---|
| `"always"` | `Always` | restart after 1 s on any exit |
| `"on-failure"` | `OnFailure` | parses, but currently treated as no-restart (only `Always` restarts) |
| `"never"` *(default)* | `Never` | no restart |

### JSON-RPC methods agentd uses

| Method | Kind | agentd params | Your reply (in `result`) |
|---|---|---|---|
| `initialize` | request | `{protocolVersion:"2024-11-05", capabilities, clientInfo}` | `{protocolVersion, capabilities:{tools:{}}, serverInfo:{name,version}}` |
| `notifications/initialized` | notification | `{}` | **none** (no `id`) |
| `tools/list` | request | `{}` | `{tools:[{name, description?, inputSchema?}]}` |
| `tools/call` | request | `{name, arguments}` | `{content:[…], isError?:bool}` |

### `ToolSpec` ← a `tools/list` entry (`mcp.rs:99-107`, `types.rs:288`)

| JSON field | → `ToolSpec` field | Required | Default if absent |
|---|---|---|---|
| `name` | `name` | yes | whole `tools/list` errors out |
| `description` | `description` | no | `""` |
| `inputSchema` | `input_schema` | no | `{}` |

### `tools/call` result → `ToolOutput` (`mcp.rs:117-121`, `types.rs:282`)

| JSON field | → | Notes |
|---|---|---|
| `isError` (bool) | `ok = !isError` | absent ⇒ `ok = true` |
| `content` (any JSON) | `content` | passed through verbatim; absent ⇒ `null` |
| top-level `error` object | — | treated as a **transport error**, aborts the call (`mcp.rs:140`) — not a clean tool error |

### Result envelope convention (stock plugins, `apexos-tools/src/tools.rs:333-339`)

| Helper | Emitted JSON |
|---|---|
| success | `{"content":[{"type":"text","text":<your-json-stringified>}]}` |
| error | `{"content":[{"type":"text","text":"{\"error\":\"…\"}"}],"isError":true}` |

### Stock plugins for reference

| Plugin | `id` | binary | language | restart | notes |
|---|---|---|---|---|---|
| Cerebro memory | `cerebro` | `/usr/local/bin/cerebro-mcp` | Rust (tokio) | `always` | ~63 tools, holds `Arc<CerebroCortex>`; async `StdioTransport` |
| System tools | `apexos-tools` | `/usr/local/bin/apexos-tools` | Rust (sync) | `always` | ~26 tools; the minimal template |
| Sonus (example) | `sonus` | `/usr/local/bin/sonus-mcp` | (intended Python) | `always` | commented in `config/plugins.toml`; the example here repoints it at a `python3` interpreter for the non-Rust pattern |
