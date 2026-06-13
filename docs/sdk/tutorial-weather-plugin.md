# Tutorial · Build a Weather MCP plugin for ApexOS-RS

> **Goal:** go from an empty directory to a working `get_weather` tool the agent can
> call out loud — *"what's the weather in Oslo?"* — and have APEX hit a real public
> API and answer.
>
> **Time:** ~20 minutes. **You will write:** one Rust crate (~120 lines), one
> `plugins.toml` stanza, one `policy.toml` line.
>
> **Prerequisites:** a working ApexOS-RS checkout, `cargo`, and a deployed `agentd`
> (the Pi at `~/ApexOS-RS`, or any node running the daemon). No API key required —
> this tutorial uses the keyless [Open-Meteo](https://open-meteo.com) API so it works
> the moment you build it. A keyed-provider variant is covered at the end.

Read first if you want the full reference rather than a walkthrough:
[`02-mcp-plugins.md`](02-mcp-plugins.md) (the protocol + supervisor contract) and
[`03-adding-tools.md`](03-adding-tools.md) (the tool/result/policy model). This tutorial
assumes neither — it teaches by building.

---

## 0. What you are actually building

An **MCP plugin** is a standalone binary that `agentd` spawns at boot and talks to over
**newline-delimited JSON-RPC on stdin/stdout**. Each line in is one request; each line
out is one response. agentd speaks exactly four messages:

```
                spawn (stdin/stdout piped, kill_on_drop)
  agentd  ───────────────────────────────────────────────►  weather-mcp  (your binary)
            │  1. initialize                  ──── line ───►  reply: serverInfo
            │  2. notifications/initialized   ──── line ───►  (notification — no reply)
            │  3. tools/list                  ──── line ───►  reply: { tools:[…] }
            │       registers "get_weather" → plugin "weather"
            │  4. tools/call {name,arguments} ──── line ───►  your get_weather() fn
            ◄──── ToolResult on the bus ◄──── parses { content, isError? } ──┘
```

Four rules you cannot violate:

1. **stdout is JSON-RPC only.** One JSON object per line, flushed every time. *Anything*
   else you print to stdout (a stray `println!`, a debug dump) is silently dropped by
   agentd's reader and corrupts nothing — but it is wasted. **All logging goes to
   stderr** (`eprintln!`); agentd pipes your stderr into its own log as `[plugin:weather]`.
2. **The notification has no `id` — never reply to it.** `notifications/initialized`
   arrives once after the handshake; you `continue` past it.
3. **Always reply to every request that has an `id`.** There is no per-request timeout
   on agentd's call path — a `tools/call` you never answer blocks that call forever.
   Don't hang; on any error, reply with an error *envelope* (see §4), not silence.
4. **The protocol version is pinned to `2024-11-05`.** Echo it back verbatim in
   `initialize`.

That's the whole contract. The two stock plugins —
[`apexos-tools`](../../tools/crates/apexos-tools/src/main.rs) (sync) and
[`cerebro-mcp`](../../cerebro/crates/cerebro-mcp/src/main.rs) (async) — are both just
this loop. We copy the sync shape.

---

## 1. Create the crate

Put it alongside the other tool plugins. From the repo root:

```bash
mkdir -p tools/crates/weather-mcp/src
```

**`tools/crates/weather-mcp/Cargo.toml`:**

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

This matches [`apexos-tools/Cargo.toml`](../../tools/crates/apexos-tools/Cargo.toml)
exactly — `reqwest` in **blocking** mode, because our stdio loop is synchronous (no
tokio). The blocking client is fine here: agentd handles each `tools/call` on its own
task, so one slow HTTP request blocks only that one call, not the daemon.

**Register the crate in the workspace.** Add the path to `members` in the root
`Cargo.toml` so `cargo build --release --workspace` picks it up:

```toml
# root Cargo.toml, in members = [ … ]
    "tools/crates/weather-mcp",
```

> The root `Cargo.toml` edit is the one file outside the plugin crate you must touch.
> If you are an agent self-extending at runtime, note that **you cannot do this from a
> turn** — adding a new Rust binary to the workspace is a human build-and-deploy step
> (see [`02-mcp-plugins.md` § For agents](02-mcp-plugins.md)). Editing `plugins.toml`
> for an *existing* binary is within reach of `propose_evolution`; compiling a new one
> is not.

---

## 2. The stdio JSON-RPC loop

This is the skeleton, lifted directly from
[`apexos-tools/src/main.rs`](../../tools/crates/apexos-tools/src/main.rs). It is
deliberately boring: read a line, match on `method`, write a line.

**`tools/crates/weather-mcp/src/main.rs`:**

```rust
use serde_json::{json, Value};
use std::io::{self, BufRead, Write};

mod tools;

fn main() {
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut out = stdout.lock();

    for line in stdin.lock().lines() {
        // Skip blank / unreadable lines — never crash the loop.
        let line = match line {
            Ok(l) if !l.trim().is_empty() => l,
            _ => continue,
        };

        // Not valid JSON? Drop it. A malformed line is never a crash.
        let req: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };

        let id = req.get("id").cloned().unwrap_or(Value::Null);
        let method = req["method"].as_str().unwrap_or("");

        let response = match method {
            // ── 1. initialize: echo the pinned protocol version ──
            "initialize" => json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "protocolVersion": "2024-11-05",
                    "capabilities": { "tools": {} },
                    "serverInfo": { "name": "weather-mcp", "version": "0.1.0" }
                }
            }),

            // ── 2. notifications/initialized: no id, no reply ──
            "notifications/initialized" => continue,

            // ── 3. tools/list: the manifest the LLM reads ──
            "tools/list" => json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": { "tools": tools::list() }
            }),

            // ── 4. tools/call: dispatch by tool name ──
            "tools/call" => {
                let params = &req["params"];
                let name = params["name"].as_str().unwrap_or("");
                let args = params.get("arguments").cloned().unwrap_or(json!({}));
                let result = tools::call(name, &args);
                json!({ "jsonrpc": "2.0", "id": id, "result": result })
            }

            // Anything else: standard JSON-RPC "method not found".
            _ => json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": { "code": -32601, "message": "method not found" }
            }),
        };

        // stdout = JSON-RPC only. One line per response. Flush every time.
        let _ = writeln!(out, "{}", serde_json::to_string(&response).unwrap());
        let _ = out.flush();
    }
}
```

Two details that bite people:

- **`continue` for the notification** — it returns to the top of the loop *without*
  writing anything. If you instead emit a reply with `id: null`, agentd ignores it, but
  it's wrong protocol; don't.
- **The final `flush()`** — without it the response can sit in the BufWriter and agentd
  blocks waiting for a line that never arrives. Flush every response.

---

## 3. The tool manifest (`tools/list`)

`tools::list()` returns the JSON-Schema array agentd hands to the LLM. **This text is
the only thing the model sees to decide whether and how to call your tool** — be precise
about what it does, what each argument means, and any side effects.

**`tools/crates/weather-mcp/src/tools.rs`** (top half — manifest + dispatch):

```rust
use serde_json::{json, Value};
use std::time::Duration;

// ─── Tool manifest (what the LLM sees) ───────────────────────────────────────

pub fn list() -> Value {
    json!([
        {
            "name": "get_weather",
            "description": "Get the current weather for a city by name. Read-only \
                            outbound HTTP call to the public Open-Meteo API. Returns \
                            temperature (°C), apparent temperature, wind speed, and a \
                            human-readable condition.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "city": {
                        "type": "string",
                        "description": "City name, e.g. \"Oslo\" or \"San Francisco\""
                    }
                },
                "required": ["city"]
            }
        }
    ])
}

// ─── Dispatch (bare tool name → fn) ──────────────────────────────────────────

pub fn call(name: &str, args: &Value) -> Value {
    match name {
        "get_weather" => get_weather(args),
        other => tool_error(format!("unknown tool: {}", other)),
    }
}
```

> **Namespacing.** Tool names are **global and flat across every plugin** — at
> `tools/list` time the supervisor inserts each name into one `HashMap<String,
> PluginId>`, and a collision means last-plugin-to-start silently wins. `get_weather`
> is safe (no stock tool uses it), but check against
> [`03-adding-tools.md` § Existing tools](03-adding-tools.md) and Cerebro's ~66 tools
> before picking a name. When in doubt, prefix: `weather_get`, not `get`.

---

## 4. The result envelope

Every tool returns **one of two shapes**, never a bare value. These are the exact
helpers from [`apexos-tools/src/tools.rs:333`](../../tools/crates/apexos-tools/src/tools.rs):

```rust
// ─── MCP result envelope — the only two shapes a tool returns ────────────────

fn tool_ok(content: Value) -> Value {
    json!({ "content": [{ "type": "text", "text": content.to_string() }] })
}

fn tool_error(msg: impl Into<String>) -> Value {
    json!({
        "content": [{ "type": "text", "text": json!({"error": msg.into()}).to_string() }],
        "isError": true
    })
}
```

What's happening: your structured JSON is **stringified into a single `text` content
block**. agentd reads back exactly two things — `content` (passed through verbatim to
the LLM) and the optional `isError` bool. It maps `ok = !isError`. The agent then parses
`content[0].text` back into JSON and reads your fields.

Two rules:

- **`isError: true` means "the tool malfunctioned"** — bad arguments, network failure,
  the API was unreachable. The turn engine flags the `tool_result` as `ok=false` and the
  agent treats it as a failure to recover from.
- **A "negative but valid" answer is still `tool_ok`.** If a city isn't found, that's a
  *successful lookup with an empty result*, not a tool failure — return `tool_ok` with a
  `found: false` field. (We do this below.) Reserve `tool_error` for "I could not run
  the lookup at all."
- **Never** report a tool failure via a top-level JSON-RPC `error` object. That path is
  treated as a *transport* error and aborts the call instead of giving the agent a clean,
  readable failure.

---

## 5. Implement `get_weather`

Open-Meteo needs coordinates, not a city name, so this is a two-call tool: **geocode**
the city to lat/lon, then fetch **current weather** for those coordinates. Both endpoints
are keyless. Append to `tools.rs`:

```rust
// ─── The tool ────────────────────────────────────────────────────────────────

fn get_weather(args: &Value) -> Value {
    // 1. Validate the input up front — return tool_error on a missing required arg.
    let city = match args["city"].as_str() {
        Some(c) if !c.trim().is_empty() => c.trim(),
        _ => return tool_error("city is required"),
    };

    eprintln!("[weather] lookup: {city}"); // logging → stderr, NEVER stdout

    // One blocking client with a bounded timeout. No tier-specific timeouts shorter
    // than is reasonable for a Nano board on a slow link.
    let client = match reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(15))
        .user_agent("apexos-rs-weather/0.1")
        .build()
    {
        Ok(c) => c,
        Err(e) => return tool_error(format!("http client build failed: {e}")),
    };

    // 2. Geocode the city name → coordinates.
    let geo_url = format!(
        "https://geocoding-api.open-meteo.com/v1/search?name={}&count=1&language=en&format=json",
        urlencode(city)
    );
    let geo: Value = match client.get(&geo_url).send().and_then(|r| r.json()) {
        Ok(v) => v,
        Err(e) => return tool_error(format!("geocoding request failed: {e}")),
    };

    let first = match geo["results"].as_array().and_then(|a| a.first()) {
        Some(r) => r,
        // City not found is a *valid* answer, not a tool failure → tool_ok.
        None => return tool_ok(json!({ "city": city, "found": false })),
    };
    let lat = first["latitude"].as_f64().unwrap_or(f64::NAN);
    let lon = first["longitude"].as_f64().unwrap_or(f64::NAN);
    let resolved = first["name"].as_str().unwrap_or(city);
    let country = first["country"].as_str().unwrap_or("");

    // 3. Fetch current weather for those coordinates.
    let wx_url = format!(
        "https://api.open-meteo.com/v1/forecast?latitude={lat}&longitude={lon}\
         &current=temperature_2m,apparent_temperature,wind_speed_10m,weather_code"
    );
    let wx: Value = match client.get(&wx_url).send().and_then(|r| r.json()) {
        Ok(v) => v,
        Err(e) => return tool_error(format!("weather request failed: {e}")),
    };
    let cur = &wx["current"];

    // 4. Build the structured result and wrap it in the success envelope.
    tool_ok(json!({
        "city": resolved,
        "country": country,
        "found": true,
        "temp_c": cur["temperature_2m"].as_f64(),
        "feels_like_c": cur["apparent_temperature"].as_f64(),
        "wind_kmh": cur["wind_speed_10m"].as_f64(),
        "conditions": weather_code_label(cur["weather_code"].as_u64().unwrap_or(0)),
    }))
}

// Minimal percent-encoding for the city query param (avoids a url-encoding dep).
fn urlencode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            b' ' => "+".to_string(),
            other => format!("%{other:02X}"),
        })
        .collect()
}

// Open-Meteo WMO weather codes → words. (Subset; extend as you like.)
fn weather_code_label(code: u64) -> &'static str {
    match code {
        0 => "clear sky",
        1 | 2 | 3 => "partly cloudy",
        45 | 48 => "fog",
        51 | 53 | 55 => "drizzle",
        61 | 63 | 65 => "rain",
        71 | 73 | 75 => "snow",
        80 | 81 | 82 => "rain showers",
        95 | 96 | 99 => "thunderstorm",
        _ => "unknown",
    }
}
```

Don't forget the two envelope helpers from §4 also live in this file. The complete
`tools.rs` is: `use` lines → `list()` → `call()` → `get_weather()` → `urlencode()` →
`weather_code_label()` → `tool_ok()` → `tool_error()`.

---

## 6. Smoke-test the protocol locally (no daemon)

Before touching agentd, feed the exact bytes agentd sends straight into the binary. This
catches handshake and envelope bugs in seconds.

```bash
cargo build --release -p weather-mcp

printf '%s\n%s\n%s\n' \
 '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{}}}' \
 '{"jsonrpc":"2.0","id":2,"method":"tools/list"}' \
 '{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"get_weather","arguments":{"city":"Oslo"}}}' \
 | ./target/release/weather-mcp
```

You should see **three** JSON lines back (and a `[weather] lookup: Oslo` line on stderr):

```
{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05","capabilities":{"tools":{}},"serverInfo":{"name":"weather-mcp","version":"0.1.0"}}}
{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"get_weather",…}]}}
{"jsonrpc":"2.0","id":3,"result":{"content":[{"type":"text","text":"{\"city\":\"Oslo\",\"conditions\":\"…\",\"temp_c\":…,…}"}]}}
```

If line 3 is missing, you're hanging on a request — check you `flush()` and that
`get_weather` always returns one of the two envelopes. If the third line has
`"isError":true`, your HTTP call failed — read the `error` text.

> **The handshake notification isn't in this test** because it's a notification with no
> reply; agentd sends it but we only need to prove the four request paths here.

---

## 7. Register it in `plugins.toml`

`plugins.toml` declares **which binaries agentd spawns**. The live file on a deployed
node is `/etc/agentd/plugins.toml` (the repo `config/plugins.toml` is only the
install-time template — edit the deployed file for an existing node). Append:

```toml
[[plugin]]
id      = "weather"                       # unique PluginId — registry owner + log prefix
cmd     = "/usr/local/bin/weather-mcp"    # absolute path to the binary
args    = []                              # optional argv
restart = "always"                        # restart after 1s on any exit
```

The `[[plugin]]` schema (`PluginConfig`):

| Field | Required | Default | Meaning |
|---|---|---|---|
| `id` | yes | — | unique; owns the tool name in the registry + log prefix |
| `cmd` | yes | — | absolute path to the binary/interpreter |
| `args` | no | `[]` | argv |
| `restart` | no | `never` | `always` \| `on-failure` \| `never` |
| `cwd` | no | — | child working directory |
| `env` | no | — | `[plugin.env]` table injected into the child |

> **`restart = "always"` is a footgun if your binary crashes on startup** — it
> hot-loops with a 1 s backoff and spams the log. You already proved it survives the
> handshake in §6, so `always` is safe here. (`on-failure` currently parses but behaves
> like `never` — only `always` auto-restarts today.)

No `[plugin.env]` is needed for the keyless Open-Meteo path. (The keyed-provider variant
in §11 adds one.)

---

## 8. Add the policy rule

Before agentd ever calls your tool, every `tools/call` runs through the `PolicyEngine`.
A tool **with no rule defaults to `Ask`** in the default `suggest` mode — the agent
would have to get an approval prompt approved on every call. `get_weather` is a
**read-only outbound HTTP call**, so it's reasonable to `allow` it (the same class as
`read_file`/`cpu_temp`).

Add one line under `[rules]` in the deployed `/etc/agentd/policy.toml` (and, for the
repo template, `config/policy.toml`):

```toml
"get_weather" = "allow"   # read-only outbound HTTP probe, no mutation
```

The rule values:

| TOML value | Effect | Use for |
|---|---|---|
| `"allow"` | always dispatch, no prompt | read-only / telemetry / safe |
| `"ask"` | emit `ApprovalPending`, dispatch only after user approves | delete / shell / outbound that costs money / hardware |
| `"workspace"` | allow iff the `path` arg is inside `AGENTD_WORKSPACE` | tools whose arg is literally named `path` |

> **Threat-model call.** A stricter operator could set `get_weather = "ask"` to gate all
> outbound network reach the way `http_fetch` is gated. Since this hits a free public
> endpoint with no auth and no side effects, `allow` is the friction-free choice — but
> it's a deliberate decision, not a default. Note also: the policy engine reads **only**
> the argument literally named `path`; our `city` arg is invisible to the `workspace`
> rule, which is correct here (we don't touch the filesystem).

---

## 9. Build and hot-swap it in

Build on the target device — **never cross-compile** (the Pi is arm64; build on the Pi).
A new binary means agentd must (re)spawn it, so restart the daemon.

```bash
# On the Pi (or wherever agentd runs)
cd ~/ApexOS-RS && git pull
cargo build --release -p weather-mcp

sudo systemctl stop agentd                              # avoid "text file busy"
sudo cp target/release/weather-mcp /usr/local/bin/weather-mcp
# (first deploy only) make sure the deployed config has the stanza + rule from §7–§8
sudo systemctl start agentd

sudo journalctl -u agentd -n 30 --no-pager | grep -i weather
```

Expect a line like:

```
[supervisor] plugin 'weather' up — 1 tools
```

If instead you see the plugin dying and restarting on a loop, your binary is crashing on
startup — run it by hand (`printf … | /usr/local/bin/weather-mcp`) to see the stderr
panic.

> **`text file busy`** — you cannot overwrite a running binary. Always
> `systemctl stop agentd` before `cp` (agentd holds the child open).

---

## 10. Verify it works (ask the agent)

The end-to-end test is to let APEX call the tool. From the UI, the PWA, or any session,
ask:

> **"What's the weather in Oslo right now?"**

What happens under the hood:

1. The turn engine decides to call `get_weather` and emits `Event::ToolRequested`.
2. The supervisor runs `PolicyEngine.check("get_weather", None)` → your `"allow"` rule →
   `Allow`. (Had you left it `ask`, the UI would show an approve/reject card first.)
3. `dispatch_tool` falls past every virtual tool to the registry, finds `weather` owns
   `get_weather`, and sends `tools/call` to your binary.
4. Your `get_weather` geocodes + fetches, returns `tool_ok({…})`.
5. agentd wraps the reply as a `ToolResult`, the UI renders a tool card, and the turn
   engine feeds your stringified JSON back to the LLM, which answers in prose.

Watch it live:

```bash
sudo journalctl -u agentd -f | grep -i weather
# you'll see your own  [plugin:weather] [weather] lookup: Oslo  on stderr
```

If the agent says it doesn't have a weather tool, the manifest didn't register — confirm
the `plugin 'weather' up — 1 tools` line from §9 and that you restarted agentd after
editing `plugins.toml`. A plugin added mid-session isn't visible to an in-flight turn; a
new turn picks it up.

---

## 11. systemd sandbox + HTTP considerations

Your plugin is a **child of `agentd`**, which runs as the unprivileged `agentd` user
under a tight systemd sandbox (`NoNewPrivileges`, `ProtectSystem=strict`, `ProtectHome`,
`PrivateTmp`, a `ReadWritePaths` allowlist — see `deploy/agentd.service`). Your tool can
do exactly what that user can do and nothing more. For a read-only HTTP tool this matters
in two specific ways:

- **Outbound network is allowed** by default — the stock sandbox does not set
  `PrivateNetwork` or an egress filter, so `reqwest` reaching `api.open-meteo.com` works.
  *If your deployment hardens egress* (a firewall, `IPAddressDeny`, an HTTP proxy
  requirement), your plugin sees the same restriction agentd does — test against the
  hardened node, not just your dev box.
- **TLS needs a CA bundle and a resolver.** `reqwest` with rustls bundles roots, so
  HTTPS works under `ProtectSystem=strict` without `/etc/ssl` access; DNS resolution
  needs `/etc/resolv.conf` readable, which it is. If you swap to a TLS backend that reads
  the system trust store, confirm the sandbox still exposes `/etc/ssl/certs`.
- **No new writable paths.** This tool writes nothing — good. If a future weather tool
  cached responses to disk, it could only write under the sandbox's `ReadWritePaths`
  (`/var/lib/agentd`, `/etc/agentd`); writing anywhere else fails regardless of `path`.
  Adding a writable location is a **deliberate `agentd.service` change**, not a plugin
  detail.

Treat the systemd sandbox as the real security perimeter. The policy rule is an
**approval** gate, not a sandbox — `allow` means "don't prompt the human," not "this
can't reach the network."

---

## 12. Variant — a keyed provider (OpenWeatherMap)

If you'd rather use a provider that needs an API key, the only changes are: pass the key
via `[plugin.env]` (never via tool args — args are logged to the event log and shown in
the UI tool card; the env block in `/etc/agentd/plugins.toml` is `agentd`-readable but
not world-readable), and read it with `std::env::var`.

`plugins.toml`:

```toml
[[plugin]]
id      = "weather"
cmd     = "/usr/local/bin/weather-mcp"
restart = "always"
[plugin.env]
WEATHER_API_KEY = "your-key-here"
```

The tool body becomes a single keyed request:

```rust
fn get_weather(args: &Value) -> Value {
    let city = match args["city"].as_str() {
        Some(c) if !c.trim().is_empty() => c.trim(),
        _ => return tool_error("city is required"),
    };
    let key = match std::env::var("WEATHER_API_KEY") {
        Ok(k) => k,
        Err(_) => return tool_error("WEATHER_API_KEY not set in [plugin.env]"),
    };
    let url = format!(
        "https://api.openweathermap.org/data/2.5/weather?q={}&units=metric&appid={key}",
        urlencode(city)
    );
    eprintln!("[weather] GET {city}");
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
        .expect("client");
    match client.get(&url).send().and_then(|r| r.json::<Value>()) {
        Ok(body) if body["main"].is_object() => tool_ok(json!({
            "city": body["name"].as_str().unwrap_or(city),
            "found": true,
            "temp_c": body["main"]["temp"].as_f64(),
            "conditions": body["weather"][0]["description"].as_str().unwrap_or("?"),
        })),
        // 404 etc. — valid "no result", not a tool failure.
        Ok(_) => tool_ok(json!({ "city": city, "found": false })),
        Err(e) => tool_error(format!("weather API error: {e}")),
    }
}
```

Everything else — the loop, the manifest, the envelope, the policy rule, the deploy —
is identical.

---

## What you built, recapped

| Piece | File | Role |
|---|---|---|
| Crate manifest | `tools/crates/weather-mcp/Cargo.toml` | blocking `reqwest` + `serde_json` |
| Workspace registration | root `Cargo.toml` `members` | so `--workspace` builds it |
| stdio JSON-RPC loop | `src/main.rs` | the four-message contract |
| Tool manifest + envelope | `src/tools.rs` | `list()`, `call()`, `tool_ok`/`tool_error` |
| The tool | `src/tools.rs` `get_weather()` | geocode → fetch → structured `tool_ok` |
| Spawn declaration | `/etc/agentd/plugins.toml` | `[[plugin]] id="weather"` |
| Approval rule | `/etc/agentd/policy.toml` | `"get_weather" = "allow"` |

The agent can now answer weather questions against a live API. To add a second tool
(say `get_forecast`), you add one object to `list()`, one arm to `call()`, and one fn —
no new crate, no new plugin stanza; the supervisor re-registers everything from
`tools/list` on the next agentd restart.

**Next:** [`02-mcp-plugins.md`](02-mcp-plugins.md) for the full supervisor/restart
semantics and the non-Rust (Python) plugin pattern; [`03-adding-tools.md`](03-adding-tools.md)
if your capability is plain local Rust and belongs in `apexos-tools` instead of a
separate binary.
