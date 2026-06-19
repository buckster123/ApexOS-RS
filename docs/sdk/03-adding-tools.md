# SDK: Adding a tool to `apexos-tools`

> `apexos-tools` is the built-in system-tool plugin agentd spawns over stdio: shell,
> file I/O, notes, HTTP, system telemetry, notify, audio, GPIO/PWM/servo, the vision
> tools (`sketch_snapshot`/`screenshot_mirror`/`camera_capture`), and `display_face`. You
> extend it when the agent needs a **new local-system capability** that is plain Rust
> (a command, a file/device op, a small HTTP call) — not memory (that's `cerebro-mcp`)
> and not a daemon-internal verb like `propose_evolution` (that's a *virtual tool*
> intercepted in the supervisor; see `agentd/crates/agentd/src/main.rs` `gather_tools`).
>
> A tool here is **two small edits in one file** (`tools/crates/apexos-tools/src/tools.rs`)
> plus **one policy line** in `config/policy.toml`. There is no plugin manifest to touch —
> the supervisor learns the new tool automatically from `tools/list`.

---

## Concepts

`apexos-tools` is an MCP-over-stdio server. The loop is dead simple:

- **`src/main.rs`** (`apexos-tools` `main`) reads newline-delimited JSON-RPC from stdin,
  dispatches by `method`, writes the response to stdout:
  - `initialize` → static handshake
  - `tools/list` → `tools::list()` — the JSON-Schema registry
  - `tools/call` → `tools::call(name, &args)` — the dispatch arm table
- **`src/tools.rs`** holds everything else:
  - `list()` returns a `json!([...])` array of tool specs. **This is the
    schema the LLM sees** — name, description, `inputSchema`.
  - `call(name, args)` is a `match` over the bare tool name routing to a
    per-tool `fn(args: &Value) -> Value`.
  - `tool_ok(content)` / `tool_error(msg)` — the two
    MCP result envelopes every tool returns. **You always return one of these**, never a
    bare value.

### The MCP result envelope (memorize this)

```rust
fn tool_ok(content: Value) -> Value {
    json!({ "content": [{ "type": "text", "text": content.to_string() }] })
}
fn tool_error(msg: impl Into<String>) -> Value {
    json!({ "content": [{ "type": "text", "text": json!({"error": msg.into()}).to_string() }],
            "isError": true })
}
```

So your structured JSON result is **stringified into a single text block**. The agent
reads `content[0].text` and parses it back. `isError: true` is the only signal that
distinguishes failure from success — the turn engine maps it to a `tool_result` with
`ok=false`.

### How the supervisor wires it up (no manifest edit needed)

The supervisor (`agentd/crates/plugins/src/supervisor.rs`) does **not** hard-code tool
names. On plugin start it calls `client.list_tools()` and registers every returned name
into a flat `tool_registry: HashMap<String, PluginId>`. Dispatch is a single lookup by
bare name in `dispatch`. Two consequences:

- **Tool names are global and flat across all plugins** — no `apexos-tools.` prefix. Your
  new name must not collide with a Cerebro tool name or another plugin's.
- **Adding a tool to `list()` is sufficient** for the supervisor to route to it. You do
  **not** edit `config/plugins.toml` (that only declares *which binaries* to spawn).

### The policy hop (`config/policy.toml` + `PolicyEngine`)

Before dispatch, every `ToolRequested` runs through the `PolicyEngine`
(`supervisor.rs:162-163`):

```rust
let path = call.args["path"].as_str();                 // supervisor.rs, dispatch hop
let decision = self.policy.read().await.check(&call.tool, path);
```

`PolicyEngine::check` (`agentd/crates/plugins/src/policy.rs`):

1. `mode == "yolo"` → `Allow` (short-circuit, ignores all rules).
2. Look up `tool_name` in `[rules]`: **exact match wins**, then `prefix.*` wildcard
   (`policy.rs:96-107`, `matches_wildcard` :142).
3. Apply the matched `Rule` (`policy.rs:109`):
   - `allow` → `Allow`
   - `ask` → `Ask` (emits `ApprovalPending`, UI shows approve/reject)
   - `workspace` → `workspace_decision(path)` (`policy.rs:118`)
   - **no rule found → `Ask`** (unknown tool is the safe default — `policy.rs:111`).

**The single most important fact for a new tool:** the policy engine only ever looks at
the argument literally named `path` (`supervisor.rs:162`). If your tool's filesystem
argument is called anything else (`output_path`, `cwd`, `file`), the `workspace` rule
**cannot see it** and will fall through to `Ask` (no path = Ask, `policy.rs:119`). Name
your primary path argument `path` if you want `workspace` confinement to work.

### Workspace confinement — the honest version

There are **two independent** workspace mechanisms, and they do not cover most tools:

1. **Policy-layer `workspace` rule** (`policy.rs:118-138`): canonicalizes `AGENTD_WORKSPACE`
   and the `path` arg, allows iff `path` is inside the workspace, rejects `..` traversal,
   else `Ask`. Only `write_file`/`create_dir` use it today (`policy.toml:15-16`).
2. **Tool-layer self-confinement**: only `delete_path` (`fn delete_path` in `tools.rs`)
   actually roots itself — it rejects `..`, canonicalizes, and hard-blocks deletions
   outside `AGENTD_WORKSPACE` (or applies a system-dir denylist when no workspace is set).

**Every other tool is unconfined.** `read_file`, `list_dir`, `run_command`, the audio
tools (which write `output_path`), and all GPIO tools operate on whatever path/device the
agent passes, limited only by the policy rule and the **systemd sandbox** in
`deploy/agentd.service` (`ProtectSystem=strict`, `ReadWritePaths=/var/lib/agentd
/etc/agentd`, `WorkingDirectory=/var/lib/agentd/workspace`). Treat the sandbox — not the
tool code — as the real filesystem boundary. The `run_command` denylist (`fn denylist_check`
in `tools.rs`) is a **soft substring heuristic** (blocks `mkfs`, `dd of=/dev/*`, `rm -rf /usr`, fork
bombs, …); it is trivially bypassable and is **not** a security boundary.

### The vision-tool convention — give the agent eyes without touching agentd

Three of the built-in tools — `sketch_snapshot`, `screenshot_mirror`, `camera_capture` —
do **not** return an image directly. They write a PNG somewhere readable and return a
plain `tool_ok` with a **vision sentinel**:

```jsonc
{ "vision": { "path": "screenshots/mirror-1718.png" }, "text": "the live UI" }
```

`path` is preferred (workspace-relative or absolute, read back from disk); a tool that
already has the bytes in hand can return `{"vision": {"b64": "<base64>"}, "text": …}`
instead. The optional `text` becomes the image caption.

The agent turn loop does the rest — **no agentd or gateway change is needed for a new
vision tool, only this return shape.** After a tool result comes back, `vision_rewrite`
(`agentd/crates/agent/src/turn.rs`) calls `find_vision_sentinel`, and on a hit hands the
ref to `apexos_core::vision` (`agentd/crates/core/src/vision.rs`):
`vision::load_and_prepare(path)` or `vision::prepare_b64(b64)` decode → downscale (longest
edge ≤ `VISION_MAX_EDGE`, the token-bomb cap) → re-encode, then
`vision::anthropic_tool_result_content` turns the `PreparedImage` into a multimodal
`ContentBlock::Image` (Anthropic native; OAI/Ollama get a follow-up user message). The
tool's stringified JSON result is replaced by the actual image block, so the model *sees*
the picture instead of reading a path string.

To add your own eye-style tool: capture to a PNG, return the `{"vision":{"path"},"text"}`
sentinel from `tool_ok`, give it an `allow` policy rule, done. Mirror `fn camera_capture`
/ `fn screenshot_mirror` / `fn sketch_snapshot` in `tools.rs` for the exact shape.

---

## Add a new tool

Five steps. Steps 1–3 are in `tools/crates/apexos-tools/src/tools.rs`; step 4 is
`config/policy.toml`; step 5 is deploy.

### 1. Declare the schema in `list()`

Add an object to the `json!([...])` array in `list()`. The shape is fixed
MCP: `name`, `description`, `inputSchema` (JSON Schema, `type: "object"`).

```jsonc
{
    "name": "my_tool",
    "description": "One precise sentence — the agent picks tools by this text. State side effects and any safety limits here.",
    "inputSchema": {
        "type": "object",
        "properties": {
            "path":   { "type": "string", "description": "Target file — named `path` so the workspace policy rule applies" },
            "factor": { "type": "integer", "description": "Optional multiplier (default 1)" }
        },
        "required": ["path"]
    }
}
```

Guidance:
- A no-arg tool uses `"inputSchema": { "type": "object", "properties": {} }` (see `cpu_temp`).
- Name the primary filesystem argument **`path`** if you want `workspace`/policy path
  checks to engage (see Concepts).
- The description is the *only* thing the LLM reads to choose the tool. Front-load the
  verb and the side effects; note hardware/safety limits inline (the GPIO specs in
  `list()` are the model — see `gpio_servo`).

### 2. Add the dispatch arm in `call()`

Add one line to the `match name` in `call()`:

```rust
"my_tool" => my_tool(args),
```

The fallthrough `_ => tool_error(format!("unknown tool: {}", name))`
already handles unknown names.

### 3. Implement the tool fn

A tool is `fn(args: &Value) -> Value` that always returns `tool_ok` or `tool_error`.
The house style: pull args with `args["x"].as_str()/.as_u64()/.as_f64()/.as_bool()`,
validate up front, return `tool_error` on a missing required arg.

```rust
fn my_tool(args: &Value) -> Value {
    let path = match args["path"].as_str() {
        Some(p) => p,
        None => return tool_error("path is required"),
    };
    let factor = args["factor"].as_u64().unwrap_or(1);

    match std::fs::metadata(path) {
        Ok(m) => tool_ok(json!({ "path": path, "scaled_size": m.len() * factor })),
        Err(e) => tool_error(format!("cannot stat {}: {}", path, e)),
    }
}
```

If you want workspace self-confinement (like `delete_path`), copy the canonicalize +
`AGENTD_WORKSPACE` `starts_with` guard from `fn delete_path` in `tools.rs` — don't rely on
policy alone for a destructive op.

### 4. Add a policy rule in `config/policy.toml`

**This step is not optional.** A tool with no rule resolves to `Ask` every call
(`policy.rs:111`) — fine for a gated op, friction for a read-only one, and a hard blocker
for any tool invoked during the wake-loop boot (those must be `allow`, see
`policy.toml:46-53`). Add a line under `[rules]`:

```toml
"my_tool" = "allow"       # read-only → allow
# or
"my_tool" = "workspace"   # writes a `path` → allow inside AGENTD_WORKSPACE, else ask
# or
"my_tool" = "ask"         # destructive / outbound / hardware-actuating → confirm
```

Pick by analogy to the existing matrix: read-only telemetry/reads → `allow`
(`policy.toml:7-13`); writes targeting a `path` → `workspace` (`:15-16`); deletes /
shell / outbound HTTP → `ask` (`:18-20`); hardware that actuates → `ask` (`:32-35`).
This is the repo default that `install.sh` writes to `/etc/agentd/policy.toml`; an
operator (or APEX via `propose_evolution` with `kind=update_policy_rule`) can loosen it later.

### 5. Build + hot-swap

`apexos-tools` is a child binary; swapping it does **not** need a daemon rebuild, but it
*is* spawned by agentd, so restart agentd to re-spawn it.

```bash
# On the Pi (always build on-device — arm64)
cd ~/ApexOS-RS && git pull
cargo build --release -p apexos-tools

sudo systemctl stop agentd
sudo cp target/release/apexos-tools /usr/local/bin/apexos-tools
sudo systemctl start agentd
sudo journalctl -u agentd -n 20 --no-pager   # look for: [supervisor] plugin 'apexos-tools' up — N tools
```

On restart the supervisor re-runs `tools/list` and re-registers the registry, so the new
tool is live with no manifest edit. If you also edited `config/policy.toml`, re-run
`install.sh` (or hand-edit `/etc/agentd/policy.toml`) so the deployed policy picks up the
new rule — the repo `config/policy.toml` is only the install-time default.

---

## Worked example: `port_check` — is a TCP port open?

A realistic, self-contained tool: probe a host:port and report reachability + latency.
Read-only, no filesystem, no new dependency (uses `std::net`).

**1. Schema — add to `list()` (`tools.rs`, inside the `json!([...])`):**

```jsonc
{
    "name": "port_check",
    "description": "Check whether a TCP port on a host is accepting connections. Read-only network probe — opens then immediately closes a socket. Reports reachable + connect latency in ms.",
    "inputSchema": {
        "type": "object",
        "properties": {
            "host":        { "type": "string",  "description": "Hostname or IP" },
            "port":        { "type": "integer", "description": "TCP port 1-65535" },
            "timeout_ms":  { "type": "integer", "description": "Connect timeout in ms (default 2000, max 10000)" }
        },
        "required": ["host", "port"]
    }
}
```

**2. Dispatch arm in `call()`:**

```rust
"port_check" => port_check(args),
```

**3. Implementation (add near the other `fn`s in `tools.rs`):**

```rust
fn port_check(args: &Value) -> Value {
    let host = match args["host"].as_str() {
        Some(h) => h,
        None => return tool_error("host is required"),
    };
    let port = match args["port"].as_u64() {
        Some(p) if (1..=65535).contains(&p) => p as u16,
        Some(_) => return tool_error("port must be 1-65535"),
        None => return tool_error("port is required"),
    };
    let timeout = std::time::Duration::from_millis(
        args["timeout_ms"].as_u64().unwrap_or(2000).min(10_000),
    );

    use std::net::ToSocketAddrs;
    let addr = match (host, port).to_socket_addrs().ok().and_then(|mut a| a.next()) {
        Some(a) => a,
        None => return tool_error(format!("cannot resolve {}:{}", host, port)),
    };

    let start = std::time::Instant::now();
    match std::net::TcpStream::connect_timeout(&addr, timeout) {
        Ok(_) => tool_ok(json!({
            "host": host, "port": port,
            "reachable": true,
            "latency_ms": start.elapsed().as_millis() as u64
        })),
        Err(e) => tool_ok(json!({
            "host": host, "port": port,
            "reachable": false,
            "error": e.to_string()
        })),
    }
}
```

Note the design choice: an unreachable port is a **successful probe**, so it returns
`tool_ok` with `reachable: false` — not `tool_error`. Reserve `tool_error` for "I could
not perform the probe" (bad args, unresolvable host). This matters because `isError`
flips the turn engine's `ok` flag and the agent treats it as a tool malfunction.

**4. Policy — add to `config/policy.toml` under `[rules]`:**

```toml
"port_check" = "allow"   # read-only network probe, no mutation
```

(A stricter operator could set it to `"ask"` to gate outbound network reach the way
`http_fetch` is gated at `policy.toml:20` — your call based on the deployment's threat model.)

**5. Build + swap** per step 5 above (`-p apexos-tools`, stop/cp/start agentd). The agent
can now call `port_check` and, because the rule is `allow`, it runs without an approval
prompt. Verify by asking the agent: *"is port 8787 open on localhost?"*

---

## Policy / safety

- **Approval policy.** Your tool's behavior is governed entirely by its `config/policy.toml`
  rule resolved through `PolicyEngine::check` (`policy.rs:88`). No rule = `Ask` every time.
  `yolo` mode bypasses all rules. The `workspace` rule **only** reads the `path` argument
  (`supervisor.rs:162`) — a path-mutating tool that names its arg `output_path` is invisible
  to it and will `Ask`. Default to `ask` for anything that writes outside the workspace,
  runs a shell, makes outbound requests, actuates hardware, or could exfiltrate/destroy.
- **Direct-call bypass.** `SupervisorCmd::DirectCall` (`supervisor.rs:31`, reached via the
  `ToolProxy::call` handle, `supervisor.rs:52`) dispatches a tool **without** the policy
  check — it's how agentd-internal machinery (e.g. the evolution rollback journal) calls
  tools. Agent turns always go through the policy hop; only trusted in-process callers use
  the bypass. Don't assume your tool is always policy-gated.
- **systemd sandbox is the real boundary.** agentd (and therefore `apexos-tools`, its child)
  runs as the unprivileged `agentd` user under `NoNewPrivileges`, `ProtectSystem=strict`,
  `ProtectHome`, `PrivateTmp`, with writes confined to `ReadWritePaths=/var/lib/agentd
  /etc/agentd` (`deploy/agentd.service`). A tool that "writes a file" can only write where
  the sandbox permits regardless of what `path` it's handed. Lean on this; do not invent a
  parallel allowlist in tool code unless the op is genuinely destructive (then copy
  `delete_path`'s canonicalize guard, `fn delete_path` in `tools.rs`).
- **The `run_command` denylist is not a sandbox.** `denylist_check` (`fn denylist_check` in
  `tools.rs`) is a substring heuristic and is bypassable (e.g. via env-indirection, base64, or paths it
  doesn't enumerate). If you add a tool that shells out, do not treat the denylist as
  protection — it catches honest mistakes, not adversarial input.
- **Self-evolution / audit discipline (for agents).** A 24/7 self-extending APEX adds tools
  the same way a human does — but the *code* edit (`tools.rs`) requires a rebuild + binary
  swap, which is **outside** the runtime self-evolution surface. Self-evolution
  (`propose_evolution`) can only rewrite `soul.md` / `policy.toml` / `plugins.toml` /
  `peers.toml` and hot-reload them; it **cannot** add a new Rust tool fn at runtime. So an
  agent's path is: (a) propose the code change for a human/CI to build and deploy, then (b)
  once the binary is live, use `propose_evolution` (`kind=update_policy_rule`) to add the
  `[rules]` line. Keep the two in lockstep — a tool present in the binary but absent from
  `policy.toml` silently defaults to `Ask`; a rule for a tool not in the binary is inert.
  Journal both moves (Cerebro `episode_add_step` / `session_save`) so the rollback story is
  intact, and never grant a new tool `allow` for a destructive/outbound op without an
  explicit human decision recorded.

---

## Reference

### Files to edit

| File | Edit |
|------|------|
| `tools/crates/apexos-tools/src/tools.rs` (`list()`) | add the schema object |
| `tools/crates/apexos-tools/src/tools.rs` (`call()`) | add `"name" => fn(args),` arm |
| `tools/crates/apexos-tools/src/tools.rs` (near other fns) | add the `fn name(args:&Value)->Value` impl |
| `config/policy.toml` (`[rules]`) | add `"name" = "allow"\|"ask"\|"workspace"` |
| *(not edited)* `config/plugins.toml` | the supervisor auto-registers via `tools/list` |

### Result envelope

| Helper | Shape | When |
|--------|-------|------|
| `tool_ok(v)` (`fn tool_ok`) | `{"content":[{"type":"text","text":"<v as JSON string>"}]}` | success (including "negative-but-valid" results) |
| `tool_error(msg)` (`fn tool_error`) | `{"content":[{"type":"text","text":"{\"error\":\"msg\"}"}],"isError":true}` | the tool could not run / bad args |

### Policy `Rule` values (`policy.rs:12`, kebab-case in TOML)

| TOML value | `Rule` | `check` result | Use for |
|-----------|--------|----------------|---------|
| `"allow"` | `Allow` | always `Allow` (unless `mode=yolo` anyway) | read-only / telemetry / safe |
| `"ask"` | `Ask` | always `Ask` → `ApprovalPending` | delete / shell / outbound / hardware actuate |
| `"workspace"` | `Workspace` | `Allow` iff `path` arg inside `AGENTD_WORKSPACE` (rejects `..`), else `Ask` | writes whose arg is literally `path` |
| *(absent)* | — | `Ask` (safe default, `policy.rs:111`) | never intentional — always add a rule |

Modes (`policy.toml:3`): `suggest` (default — confirm everything not `allow`), `auto-edit`,
`yolo` (no gates).

### Policy resolution order (`PolicyEngine::check`, `policy.rs:88`)

1. `mode == yolo` → `Allow`.
2. exact `[rules]` key match.
3. `prefix.*` wildcard match (`matches_wildcard`, `policy.rs:142` — matches `prefix.<x>`, not bare `prefix`).
4. no match → `Ask`.

Policy reads **only** `call.args["path"]` (`supervisor.rs:162`). No other arg name is inspected.

### Existing tools (the full `list()` / `call()` registry)

The 31 tools `apexos-tools` exposes today (verify against `list()` / `call()` in
`tools.rs`):

`run_command`, `read_file`, `write_file`, `list_dir`, `create_dir`, `delete_path`,
`notes_list`, `notes_read`, `notes_append`, `sketch_snapshot`, `screenshot_mirror`,
`camera_capture`, `http_fetch`, `cpu_temp`, `disk_usage`, `memory_info`, `uptime`,
`notify`, `audio_analyze`, `audio_trim_silence`, `audio_normalize`, `audio_peak_limit`,
`audio_trim`, `audio_clean`, `gpio_info`, `gpio_read`, `gpio_write`, `gpio_pulse`,
`gpio_pwm`, `gpio_servo`, `display_face`. Names are global across all plugins — don't
collide with these or with `cerebro-mcp`'s tools (`TOOL_NAMES`, 66 entries — 64 functional
+ 2 stubs: `ingest_file`, `search_vision`).

### Workspace confinement coverage (honest)

| Mechanism | Where | Covers |
|-----------|-------|--------|
| Policy `workspace` rule | `workspace_decision` in `policy.rs` | `write_file`, `create_dir` (any tool whose `path` arg is set to `workspace`) |
| Tool self-confinement | `fn delete_path` in `tools.rs` | `delete_path` only (canonicalize + `AGENTD_WORKSPACE` `starts_with`) |
| systemd sandbox | `deploy/agentd.service` | **everything** — the real filesystem boundary |
| `run_command` denylist | `fn denylist_check` in `tools.rs` | nothing (soft heuristic, bypassable — not a boundary) |

### Relevant env vars

| Var | Read by | Effect |
|-----|---------|--------|
| `AGENTD_WORKSPACE` | `workspace_decision` (`policy.rs`), `fn delete_path` (`tools.rs`) | workspace root for `workspace` rule + `delete_path` confinement |
| `APEX_GPIO_RESERVED=none` | GPIO tool fns (`tools.rs`) | bypass reserved-pin checks (unsafe with sensor head) |
| `PIPER_MODEL`/`NTFY_TOPIC`/`TELEGRAM_*` | `fn notify` (`tools.rs`) | enable optional notify surfaces |
| `APEXOS_CAMERA_DEVICE`/`APEXOS_CAMERA_CMD` | `fn camera_capture` (`tools.rs`) | force a V4L2 node / fully override the capture command for `camera_capture` |
| `APEXOS_UI_SNAPSHOT_URL` | `fn screenshot_mirror` (`tools.rs`) | loopback URL ui-slint serves its `take_snapshot` PNG on (default `http://127.0.0.1:8788/snapshot`) |
