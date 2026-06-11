# ApexOS-RS mk1 — Code Review / Audit Findings

> Status: **IN PROGRESS** (see `AUDIT-PLAN.md` for pass tracker). Pure review — no code changed.
> Reviewer: Claude (Opus 4.8). Started 2026-06-11.
> Each finding: `[ID] CRITICALITY [Category] — title` · location · problem · recommendation.

## Criticality legend
🔴 Critical · 🟠 High · 🟡 Medium · 🟢 Low · 🔵 Info

---

## Executive summary

**Status:** review complete. 33 findings across all subsystems + installer/deploy/docs.

| Criticality | Count |
|-------------|-------|
| 🔴 Critical | 5 |
| 🟠 High | 5 |
| 🟡 Medium | 8 |
| 🟢 Low | 10 |
| 🔵 Info | 5 |

**Overall health.** The *internal architecture is genuinely good* — the agent turn engine, policy/supervisor split, and Cerebro core are well-structured, thoughtfully commented, and well-tested (165 test fns; the turn-engine permit/timeout reasoning and Cerebro's FTS5-fallback + dream-engine bounds are highlights). `cargo check` is clean. The agentd systemd unit is properly hardened (non-root, `ProtectSystem=strict`, `NoNewPrivileges`, `PrivateTmp`). **The problems are concentrated in two places: (1) there is no network authentication model at all, and (2) the safety/reliability config doesn't do what it appears to.** These are systemic, not scattered — fixing the two root causes resolves most of the Critical/High list.

**The one architectural decision to make first:** both daemons — agentd (`0.0.0.0:8787`) and cerebro-api (`0.0.0.0:8765`) — bind all interfaces with **zero authentication**, on a platform whose own threat model assumes LAN peers (mesh, remote `AGENTD_WS`, an SSH password in the repo). This single gap turns ordinary features into LAN-exploitable primitives: arbitrary command execution (F001), an interactive shell (F002), **LLM-backend hijack that exfiltrates every conversation and can drive tool calls (F015)**, API-key read/overwrite (F003), **irreversible wipe of the entire cognitive memory (F019)**, device power-off (F004), and paid-GPU launch + remote injection (F016). The agentd hardening means most of this runs as the `agentd` user rather than root — real mitigation, but key-exfil, data-destruction, conversation-hijack, and lateral movement remain. *Decide auth once* (a shared-secret/bearer middleware applied gateway-wide, reusing the existing `sensor_bridge_token` pattern, plus default-binding to `127.0.0.1`) and 6 of the 10 Critical/High findings close together.

### Top must-fix (in order)
1. **F001/F002/F003/F004/F015/F016/F019 — authenticate the control plane** (one middleware + default localhost bind). The single highest-leverage fix.
2. **F027 — the policy allowlist is inert** (rule keys match no real tool name → every tool falls to `Ask`). The core safety mechanism doesn't function as shipped; this likely pushes operators to `yolo`. Align rule names with actual tool names.
3. **F025 — the UI never reconnects** (dies permanently on any agentd restart, contradicting the docs). A kiosk-reliability defect triggered by the project's own documented deploy workflow.
4. **F010/F024 — "workspace" confinement is not enforced** (path checks deferred/bypassable); a tool can read/write/delete anywhere. Pairs with F027.
5. **F009/F011 — streaming correctness** (UTF-8 chunk-split drops text; dropped broadcast result reported as tool failure).
6. **F031 — docs conceal the exposure** ("localhost", "no attack surface") — fix so operators firewall the ports until auth lands.

Full detail below, grouped by criticality. Severity note: F001/F002 run as the *agentd service user* (hardened), not root — serious but not instant-root; F019/F015 are rated Critical for data-loss / mass-exfil respectively.

---

## Findings

### 🔴 Critical

#### F001 🔴 [Security] Unauthenticated arbitrary command execution on the LAN — `/api/run`
**Location:** `agentd/crates/gateway/src/lib.rs:802` (`run_command_handler`), route registered `:102`; bind `agentd/crates/agentd/src/main.rs:232` (`0.0.0.0:8787`).
**Problem:** `POST /api/run` runs `sh -c <command>` with no authentication. The only guard is a 4-entry substring deny-list (`"rm -rf /"`, `"mkfs"`, `"dd if=/dev/zero"`, fork-bomb). This is trivially bypassed:
- `rm  -rf /` (two spaces) — not a substring match
- `rm -rf /home`, `rm -rf ~`, `find / -delete`
- `cat /var/lib/agentd/.api_key` (exfiltrate the Anthropic key — see F003)
- any reverse shell / curl-pipe-bash

Because the listener binds `0.0.0.0`, every device on the LAN (the CLAUDE.md threat model explicitly assumes LAN peers: SSH pw in repo, mesh peers, `AGENTD_WS` remote) can call this. agentd runs under systemd hardening (`NoNewPrivileges`) but still executes as its own user with full access to its data dir, the API keys, the mesh, and `/api/power`.
**Recommendation:** This endpoint is a remote-code-execution primitive. Options, in order of preference: (1) remove `/api/run` entirely if the UI doesn't need raw shell; (2) require a bearer token / shared secret on every `/api/*` and `/terminal-ws` route (the pattern already exists for `/sensor-bridge` via `sensor_bridge_token` at `:226`) — reuse it gateway-wide via a `tower` auth middleware layer; (3) bind to `127.0.0.1` by default and put LAN exposure behind an explicit opt-in + auth. A deny-list can never make `sh -c` safe — drop it as the security boundary.

#### F002 🔴 [Security] Unauthenticated interactive shell (agentd-user) — `/terminal-ws`
**Location:** `agentd/crates/gateway/src/lib.rs:1206` (`terminal_ws_handler`), route `:114`, PTY at `:1210`.
**Problem:** `GET /terminal-ws` upgrades to a WebSocket and spawns `/bin/bash` over a PTY with **no authentication** (unlike `/sensor-bridge`, which checks a token). Anyone who can reach `:8787` gets a full interactive shell **as the agentd service user** (the process runs as `agentd` under the hardened unit, so this is *not* a root shell despite `HOME` defaulting to `/root` at `:1233` — but it still grants full read/write to the agentd data dir, API keys, mesh, and the polkit-granted power rights). Same `0.0.0.0` exposure as F001.
**Recommendation:** Gate behind the same auth as F001. Additionally this endpoint is documented as **deferred/post-v1** in CLAUDE.md (see F012) yet is live and routed — decide whether it should ship at all in mk1; if not, remove the route.

#### F003 🔴 [Security] Unauthenticated API-key read/overwrite & key-at-rest — `/api/key`, `/api/keys`
**Location:** `agentd/crates/gateway/src/lib.rs:364` (`set_key_handler`), routes `:87-88`; key persisted to `/var/lib/agentd/.api_key` (`:375`).
**Problem:** `POST /api/key` and `POST /api/keys` set the Anthropic/OpenRouter keys with no auth — a LAN attacker can overwrite the key (denial of service / billing redirect) or, combined with F001/F002, read the persisted key file and exfiltrate it. Need to verify file permissions on the written `.api_key` (chmod 600?) — flagged for Pass 6.
**Recommendation:** Authenticate all key endpoints; never return key material in `GET /api/keys` (verify it only returns booleans — it appears to at `:383`, good); ensure `.api_key` is written `0600` owned by the service user. **Confirmed in Pass 6:** the installer writes `/etc/agentd/env` as `chmod 600 root:root` (good), but `set_key_handler` writes the runtime `/var/lib/agentd/.api_key` via `tokio::fs::write` with **no explicit mode** (umask-dependent, typically 0644). Set it `0600` explicitly. Note the key also lives in agentd's process environment (via systemd `EnvironmentFile`), so an F001/F002 RCE can read it from `/proc/self/environ` regardless — fixing auth (F001) is the real mitigation.

#### F015 🔴 [Security] Unauthenticated LLM-backend hijack — `/api/backend`
**Location:** route `agentd/crates/gateway/src/lib.rs:91` (`set_backend_handler`); live-swappable `base_url` `agentd/crates/agent/src/oai.rs:14`.
**Problem:** `POST /api/backend` rewrites the OpenAI-compatible base URL at runtime with no auth. A LAN attacker can point inference at a server they control. Consequences: (1) **every prompt and conversation** (which includes soul.md, memory recalls, tool outputs, potentially secrets) is exfiltrated to the attacker; (2) the attacker controls the "assistant" responses, including `tool_use` blocks — so they can drive the agent to call tools, and in `AutoEdit`/`Yolo` policy or with auto-approval they can achieve code/command execution on the device through the legitimate tool path. This is the highest-impact item alongside F001/F002 because it subverts the trusted core, not just a single endpoint.
**Recommendation:** Authenticate `/api/backend` (and treat backend URL changes as privileged). Same gateway-wide auth as F001.

#### F019 🔴 [Security] cerebro-api unauthenticated on `0.0.0.0:8765` — full memory read + irreversible purge
**Location:** bind `cerebro/crates/cerebro-api/src/main.rs:903-905`; destructive routes `:747` `purge_trash`, `:756` `purge_all_trash`, `:763` `bulk_delete`, `:297` `delete_memory`, `:587` `delete_tag`; service `deploy/cerebro-api.service`.
**Problem:** The cognitive-memory REST API binds all interfaces with no authentication, exposing the full CRUD + **destructive** surface: any LAN host can read every stored memory (episodes, procedures, soul context, anything the agent learned) and call `purge_all_trash` / `bulk_delete` to **permanently destroy the entire memory store** — the central asset of the project. This is a second unauthenticated `0.0.0.0` control plane alongside agentd (F001). Data-loss + data-exfil, hence Critical.
**Recommendation:** Bind `127.0.0.1` by default and/or require a shared-secret token (same fix as F001). Gate destructive routes (`purge_*`, `bulk_delete`) behind explicit auth even if reads are opened. The two daemons (agentd:8787, cerebro-api:8765) together mean the platform currently has **no network authentication model at all** — address this as one cross-cutting decision.

### 🟠 High

#### F004 🟠 [Security] Unauthenticated power control — `/api/power`
**Location:** `agentd/crates/gateway/src/lib.rs:528` (`power_handler`), route `:95`.
**Problem:** `POST /api/power {action: reboot|shutdown}` reboots/powers off the device with no auth, via `systemctl` authorized by the installed polkit rule. LAN-reachable → trivial denial of service against any node.
**Recommendation:** Same gateway-wide auth. Lower than F001-F003 only because impact is DoS, not RCE/exfil.

#### F025 🟠 [Correctness/Reliability] UI WebSocket never reconnects — kiosk dies on any agentd restart
**Location:** `ui-slint/src/main.rs:454-525` (WS task). Initial-connect-fail path `:459-468` (`return`); disconnect path `:505-514` (`break`, loop ends).
**Problem:** The WS connection is established once in a spawned task. If the initial `connect_async` fails (agentd not yet up at boot — a real ordering race), the task sets "Connection failed" and **returns permanently**. If the socket drops mid-session, the loop sets "Disconnected" and **breaks** — the task ends. There is **no outer reconnect loop**, so the UI never re-establishes the connection and stays dead until the `ui-slint` process is restarted. This directly contradicts CLAUDE.md ("the UI will retry the WS connection on disconnect" / "agentd must be running — the UI will retry"). Real-world triggers: the documented hot-swap deploy (`systemctl restart agentd`), an agentd crash-restart, a transient network blip, or simply UI-before-agentd boot order. For an always-on kiosk appliance this is a reliability defect.
**Recommendation:** Wrap connect + read-loop in an outer `loop` with backoff (e.g. retry every 2–5s, capped), re-sending `session_init` (or replaying `session_id` per the replay protocol) on each reconnect. Update CLAUDE.md only after the behavior matches the claim.

#### F005 🟠 [Resource] PTY child never reaped + orphaned WS task per terminal session
**Location:** `agentd/crates/gateway/src/lib.rs:1326-1328` and `:1289-1295`.
**Problem:** On session close the code calls `child.kill()` but never `child.wait()`/`try_wait()`, leaving a **zombie** per terminal session in a long-running daemon. Also, `ws_write` and `ws_read` are `tokio::spawn`ed and joined via `tokio::select!`; when one completes the other JoinHandle is dropped but the detached task keeps running (the reader/writer std::threads + `to_pty_tx` linger until the socket fully closes). Accumulates across sessions.
**Recommendation:** `let _ = child.wait().await;` (or spawn a reaper) after kill; abort the losing task explicitly (`handle.abort()`) instead of relying on select drop.

#### F008 🟠 [Resource/Security] No timeout or retry on LLM provider HTTP; permanently exposed to hangs
**Location:** `agentd/crates/agent/src/anthropic.rs:19` & `:49`; `agentd/crates/agent/src/oai.rs:25` & `:55`.
**Problem:** Both providers build `reqwest::Client::new()` with **no connect/read timeout** and issue the streaming request with no retry/backoff. A wedged upstream (Ollama hang, network black-hole, proxy stall) leaves the turn blocked indefinitely — the only backstop is the 1800s tool-result timeout, which doesn't cover the provider stream itself. There is **zero retry** on transient 429/5xx/connection-reset, so a single blip fails the whole turn. CLAUDE.md's design rule ("no hard-coded timeouts shorter than 30s") is satisfied, but the opposite failure — unbounded waits and no resilience — is unhandled, and it bites hardest on the flaky-network Nano/Micro tiers the project explicitly targets.
**Recommendation:** Configure `reqwest::Client::builder().connect_timeout(10s)` (a *connect* timeout is safe for streaming; avoid a total-request timeout that would kill long generations). Add bounded exponential-backoff retry on connection errors and 429/5xx before the stream starts. Note: `thinking:{type:"adaptive"}` and `max_tokens:16000` were verified correct against the current Anthropic API — not issues.

### 🟡 Medium

#### F009 🟡 [Correctness] UTF-8 multibyte split across SSE chunks silently drops streamed text
**Location:** `agentd/crates/agent/src/anthropic.rs:163` and `agentd/crates/agent/src/oai.rs:191` — `buf.push_str(std::str::from_utf8(&bytes).unwrap_or(""))`.
**Problem:** Network/SSE byte chunks can split in the middle of a multibyte UTF-8 codepoint. `from_utf8` then returns `Err`, and `unwrap_or("")` **discards the entire chunk** — the bytes are not re-buffered, so any text spanning that boundary is permanently lost and the SSE line buffer can desync. Manifests as occasional dropped/garbled characters in non-ASCII output (emoji, accents, CJK), more often on slow links (smaller chunks) — again the Nano/Micro tiers.
**Recommendation:** Accumulate raw bytes in a `Vec<u8>` and decode incrementally (e.g. carry the incomplete tail forward), or use an incremental UTF-8 decoder / `bytes`-aware SSE parser. Never `unwrap_or("")` a partial decode.

#### F010 🟡 [Security] `Workspace` policy rule is not enforced — behaves as `Allow` in AutoEdit
**Location:** `agentd/crates/plugins/src/policy.rs:18-20` & `:113-116`.
**Problem:** The `Workspace` rule is documented as "auto if path is inside AGENTD_WORKSPACE, else ask," but the path check is **deferred/unimplemented** — in `AutoEdit` mode it returns `Decision::Allow` unconditionally regardless of the actual path. A user who marks `fs.write = "workspace"` believing writes are confined to the workspace gets no such confinement; a tool can write anywhere the agentd user can. False sense of security.
**Recommendation:** Either implement the real path check (resolve the tool's path arg against `AGENTD_WORKSPACE`, canonicalize, reject escapes/symlinks) or, until then, treat `Workspace` as `Ask` in all modes and document that confinement is not yet enforced. The policy engine itself is otherwise clean and well-tested.

#### F011 🟡 [Resource/Correctness] Dropped broadcast `ToolResult` is reported as tool failure
**Location:** `agentd/crates/agent/src/turn.rs:147-201` (`collect_tool_results`, `Lagged` branch `:170`).
**Problem:** Tool results are delivered over a `tokio::broadcast` channel. Under load, a slow consumer can lag and the channel drops messages; the code `continue`s on `Lagged`, so a genuinely-successful tool result that was dropped never arrives, and after the (default 1800s) timeout the turn synthesizes an `is_error` result — telling the model the tool failed even though its side effect *did* happen. Worst case the user also waits up to 30 min. Acknowledged in comments as a tradeoff, but the dropped-success case is a real correctness hazard.
**Recommendation:** Deliver tool results to the turn via a dedicated per-turn oneshot/mpsc keyed by `call_id` rather than a shared lossy broadcast, or raise the broadcast capacity and treat `Lagged` as a hard error that retries result collection. At minimum, lower the synthesized-error timeout for the common case so a wedged turn doesn't hang 30 min.

#### F024 🟡 [Security] `delete_path` protection is incomplete and bypassable; file tools have no path confinement
**Location:** `tools/crates/apexos-tools/src/tools.rs:606-639` (`delete_path`); file tools `read_file`/`write_file`/`create_dir` (`:493`,`:528`,`:600`); `run_command` deny-list `:347-407`.
**Problem:** `delete_path` blocks a hard-coded list (`/`, `/usr`, `/bin`, `/lib`, `/sbin`, `/boot`, `/etc/passwd`, `/etc/shadow`) via literal `path.starts_with("{p}/")`. Gaps: (1) **`/etc`, `/home`, `/root`, `/var`, `/usr/local`, `/var/lib/agentd` are not protected** — `delete_path("/etc", recursive=true)` or deleting the agent's own data dir is allowed; (2) no canonicalization, so `..`, `//`, symlinks, and relative paths (`etc`) bypass the prefix check; (3) `read/write/create/delete` have **no workspace confinement** at all (compounds F010 — "workspace" mode doesn't constrain paths anywhere). These are agent tools behind the policy gate (good — `run_command`/`delete_path` should default to `Ask`; verify in `config/plugins.toml`, Pass 6), so exploitation needs the agent to call them and a user to approve (or AutoEdit/Yolo). `run_command`'s deny-list (`mkfs`/`dd`/`/dev/`) is the same theater as F001 and shouldn't be relied on.
**Recommendation:** Canonicalize the target and verify it stays within `AGENTD_WORKSPACE` (or a configurable allowlist) before any write/delete; expand the protected set; reject symlink escapes. Treat the deny-list as advisory, not a boundary.

#### F027 🟡 [Correctness/Security] Default `policy.toml` rules match no real tool name — entire allowlist is inert
**Location:** generated rules `install.sh:463-472` (`fs.read`/`fs.write`/`fs.delete`/`shell.run`/`network`); matcher `agentd/crates/plugins/src/policy.rs:95-118`; actual tool names `tools/crates/apexos-tools/src/tools.rs:304-328` (`run_command`,`read_file`,`write_file`,`delete_path`,`http_fetch`,…) + cerebro MCP tools (`recall`,`memory_store`,…).
**Problem:** The policy engine matches a tool name by exact match, then `prefix.*` wildcard. The shipped rule keys are dotted names (`fs.read`, `shell.run`) that **do not exist** — the real tools are snake_case (`read_file`, `run_command`) and the cerebro tools are bare (`recall`). No rule ever matches, so `find_rule` returns `None` → `apply_rule(None)` → `Decision::Ask` for **every** tool. Consequences: (1) the intended auto-allow rules are completely dead — including `fs.read = "allow"`; (2) in the default `suggest` mode, every single tool call requires manual approval, including the many Cerebro memory reads/writes the agent performs continuously — unusable for an autonomous/kiosk appliance, which likely pushes operators to `yolo` mode (auto-allow everything) to make it usable, removing the gate entirely; (3) makes F010's `fs.write="workspace"` doubly moot. Fail-secure by luck, not design — the safety config as shipped does not function.
**Recommendation:** Align the rule keys with the actual registered tool names (`run_command`, `read_file`, `write_file`, `delete_path`, `http_fetch`, `cerebro.*`-style if cerebro tools are namespaced — they aren't currently). Add a startup check that warns when a policy rule references an unknown tool. Decide and document one naming convention and apply it across tools + policy + tests (the unit tests use `fs.read`/`cerebro.*`, matching neither the tools nor each other).

#### F020 🟡 [Correctness] FTS5 search query not escaped — ordinary queries can error and return nothing
**Location:** `cerebro/crates/cerebro/src/storage/vector.rs:197` (`let safe_query = query.replace('"', " ")`).
**Problem:** The code comment says "Wrap in quotes to treat as a phrase for safety," but the implementation only *strips* double-quotes and binds the raw user string as an FTS5 `MATCH` expression. (SQL injection is not possible — the value is bound via `?` — but FTS5 interprets the string as query syntax.) User queries containing FTS5 operators/special chars (`*`, `:`, `^`, `-`, `(`, `)`, `NEAR`, bare `OR`/`AND`, unbalanced quotes) raise an FTS5 syntax error → the search returns `Err` and recall fails for that input. This is the sole search path on Nano tier (no embeddings), so the impact is amplified there.
**Recommendation:** Implement the intended phrase-wrapping: tokenize and wrap each term in double quotes (`"term1" "term2"`), or sanitize to alphanumerics, so arbitrary natural-language queries always form a valid FTS5 expression.

#### F012 🟡 [Docs/Drift] PTY terminal shipped despite being listed deferred/post-v1
**Location:** `CLAUDE.md` "Deferred / post-v1" → "PTY terminal — alacritty_terminal crate"; actual impl `agentd/crates/gateway/src/lib.rs:1204-1329` + route `:114`.
**Problem:** CLAUDE.md says the PTY terminal is deferred, but a working `/bin/bash` PTY endpoint is live and routed in mk1. Docs and code disagree on whether this feature exists — and it's the unauthenticated shell of F002.
**Recommendation:** Decide if PTY ships in mk1. If yes, move it out of Deferred, document it, and gate it (F002). If no, remove the route.

#### F016 🟠 [Security] Unauthenticated `/api/vast/*` and `/api/mesh/*` — financial DoS, remote injection, peer poisoning
**Location:** routes `agentd/crates/gateway/src/lib.rs:115-121`; launch path `agentd/crates/plugins/src/supervisor.rs:1024-1142`; recipe interpolation `:1025-1032`.
**Problem:** All vast and mesh endpoints are unauthenticated (F001 family) but have distinct, higher impact: (1) `POST /api/vast/...` can **launch paid GPU instances** on the user's vast.ai account → direct financial loss; (2) recipe fields (`model_repo`, `model_quant`, etc.), settable via unauthenticated `POST /api/vast/recipes`, are string-interpolated into the remote `--onstart-cmd` (`bash /app/launch.sh`) at `:1025` → command injection on the rented instance; (3) `POST /api/mesh/peers` lets an attacker inject a malicious peer the daemon will connect to. The vast CLI calls themselves are built safely (arg vectors, `VAST_API_KEY` via env not argv — good), so the exposure is purely the missing auth + the recipe→onstart interpolation.
**Recommendation:** Gateway-wide auth (F001). Additionally, validate/escape recipe fields before interpolating into `onstart`, or pass them only via `--env` (already done for most) rather than building a shell string.

### 🟢 Low (additional)

#### F017 🟢 [Security] SSH tunnel uses `StrictHostKeyChecking=no`
**Location:** `agentd/crates/plugins/src/supervisor.rs:1133`.
**Problem:** The vast SSH tunnel disables host-key verification, accepting any key → MITM on the inference tunnel. Common for ephemeral cloud instances, but it removes a layer of protection on traffic that carries prompts/responses.
**Recommendation:** Pin the host key returned by `vastai show instance` if available, or document the accepted risk.

#### F018 🔵 [Resource] Event log flushes but never fsyncs
**Location:** `agentd/crates/store/src/lib.rs:32` (`w.flush()` only).
**Problem:** The JSONL event log flushes to the kernel after each event but never fsyncs. On a kiosk that loses power (common for a wall-powered Pi), recent events can be lost. The comment frames this as an intentional tradeoff — noting it so it's a conscious choice.
**Recommendation:** Acceptable as-is for most cases; if the evolution/audit log must survive power loss, fsync periodically or on critical events.

#### F031 🟡 [Docs/Security] Docs say `localhost:8787` and market "no attack surface" while the plane is `0.0.0.0` + unauthenticated
**Location:** `README.md:31,36,88,167` ("ws://localhost:8787", "browser UI at http://host:8787", "no browser attack surface", "Embedded / industrial"); `CLAUDE.md` Pi-target table ("agentd WS ws://localhost:8787"); actual bind `agentd/crates/agentd/src/main.rs:232` (`0.0.0.0:8787`).
**Problem:** Every doc reference frames the endpoint as `localhost`, implying local-only, and the README positions the distro as a *security* improvement ("no browser attack surface," "embedded/industrial," "no compositor to crash"). In reality both daemons bind all interfaces with no auth (F001/F019). An operator trusting the docs will not firewall the ports or realize the LAN exposure — the documentation actively conceals the single biggest risk.
**Recommendation:** Correct the docs to state the real bind address and the (current) absence of authentication, and add a "Security / network exposure" section telling operators to firewall `8787`/`8765` to localhost or trusted hosts until auth lands. Re-frame the security marketing honestly.

#### F032 🟢 [Docs] README status badge says "planning" though mk1 is complete and deployed
**Location:** `README.md:9` (`status-planning-yellow`).
**Problem:** The project is mk1-complete and running in production (per the build table in CLAUDE.md, all 10 steps ✓), but the README badge still reads "planning."
**Recommendation:** Update the badge to reflect mk1/released status.

#### F033 🔵 [Build/Deploy] `.gitignore` ignores `Cargo.lock` — wrong for a binary workspace
**Location:** `.gitignore` (`Cargo.lock`).
**Problem:** This workspace builds applications/binaries, for which `Cargo.lock` **should** be committed (reproducible builds — important for "always build on Pi"). It currently *is* committed (good), but the ignore rule contradicts that and risks the lock being dropped, leading to non-reproducible dependency resolution across the Nano→Pro tiers.
**Recommendation:** Remove `Cargo.lock` from `.gitignore` and keep it tracked. (No committed secrets were found — `.gitignore` otherwise fine.)

#### F021 🟢 [Correctness] `enum_to_str(v).unwrap()` is a latent panic on the DB write path
**Location:** `cerebro/crates/cerebro/src/storage/sqlite.rs:468` and `:533` (`emotional_valence` mapping in insert/update).
**Problem:** Every sibling `enum_to_str(...)` call uses `?` to propagate errors, but the two `emotional_valence` mappings use `.unwrap()` inside `.map(...)`. If `enum_to_str` ever returns `Err` for a valence variant (e.g. a new variant added without a match arm), the daemon panics mid-write instead of returning an error. Safe today (all current variants map), but a latent foot-gun.
**Recommendation:** Hoist the conversion out of the closure and use `?`, or change `enum_to_str` for valence to be total. Consistency with the surrounding code.

#### F022 🟢 [Maintainability] Dead/confused param-building code in `fts5_search`
**Location:** `cerebro/crates/cerebro/src/storage/vector.rs:207-216`.
**Problem:** `dyn_params` is built, then immediately `drop`ped and rebuilt as `all_params` with a "Rebuild correctly" comment — leftover from a hasty fix. Allocates and discards a vector; reads as half-finished.
**Recommendation:** Delete the dead `dyn_params` construction; keep only `all_params`.

#### F023 🟢 [Docs/Drift] cerebro-api port inconsistency (code 8765 vs service/desc 8767)
**Location:** code default `cerebro/crates/cerebro-api/src/main.rs:904` (`0.0.0.0:8765`); `deploy/cerebro-api.service` Description says "port 8767".
**Problem:** The code listens on 8765 by default; the systemd unit description claims 8767. Whichever is intended, they disagree — confusing for ops and firewall rules.
**Recommendation:** Reconcile to one port; set `CEREBRO_API_ADDR` explicitly in the unit's `EnvironmentFile` and fix the description.

#### F028 🟢 [Build/Deploy] `apexos-rs-ui` runs as root with no systemd hardening
**Location:** `deploy/apexos-rs-ui.service`.
**Problem:** The UI must run as root for DRM master (documented, legitimate), but unlike `agentd.service` it has **no** hardening directives — no `NoNewPrivileges`, `ProtectSystem`, `ProtectHome`, or `PrivateTmp`. A root process processing network data (the agentd WS) with zero confinement is a larger blast radius than necessary.
**Recommendation:** Add the hardening that's compatible with KMS/DRM: `NoNewPrivileges=true`, `ProtectHome=true`, `PrivateTmp=true`, `ProtectSystem=strict` with `ReadWritePaths` for the DRM/tty device nodes, and a tight `SystemCallFilter`/`DeviceAllow` (only `/dev/dri/*`, `/dev/tty7`, input devices). Test against linuxkms which needs `/dev/dri` + input.

#### F029 🔵 [Build/Deploy] `policy.toml` absent from `config/`; generated inline by installer (source/docs drift)
**Location:** `config/` (only `plugins.toml`); `CLAUDE.md:66` claims `config/` ships `plugins.toml, policy.toml`; canonical copy is `install.sh:463-472`.
**Problem:** CLAUDE.md and the layout diagram say `config/policy.toml` exists; it doesn't — the default policy lives inline in the installer. Not a runtime bug, but the source tree and docs disagree, and the inert-rules bug (F027) is therefore easy to miss because the file isn't in the repo where one would review it.
**Recommendation:** Add `config/policy.toml` as the canonical source and have install.sh copy it (single source of truth), or update CLAUDE.md to say policy.toml is generated by the installer.

#### F030 🔵 [Security] `curl | sudo bash` install executes unverified remote code as root
**Location:** `install.sh:9` (documented quick-install).
**Problem:** The advertised install path pipes a network-fetched script straight into `sudo bash` with no checksum/signature verification — standard for the genre, but it means a compromised host/MITM yields root. Acceptable for a hobby distro; worth stating the trust assumption.
**Recommendation:** Offer a checksum/signature for the script, or document "review before running"; the `--repo-dir` local-clone path (already supported) is the safer default for users who can use it.

#### F026 🟢 [Reliability] UI shared `AppState` mutex uses `.lock().unwrap()` (poison cascade)
**Location:** `ui-slint/src/main.rs:835` (and other `state.lock().unwrap()` sites).
**Problem:** If any thread panics while holding the `AppState` mutex, every later `.lock().unwrap()` panics too, cascading a single fault into a dead UI. Critical sections are tiny so the risk is low, but `.unwrap()` on a lock is fragile.
**Recommendation:** Use `lock().unwrap_or_else(|e| e.into_inner())` to recover from poison, or a non-poisoning lock (e.g. `parking_lot::Mutex`). Note: `.slint` files compile cleanly (binding loops would fail the build, which mk1 passes), so the declarative UI is build-verified.

#### F013 🟢 [Security/Resource] Predictable `/tmp` paths and whole-file-in-RAM serving
**Location:** transcribe `agentd/crates/gateway/src/lib.rs:1116` (`/tmp/apex_stt_{stamp}.webm`), snapshot `:841` (`/tmp/apex_snapshot.jpg` fixed); file serving `:937-976` reads the whole file into a `Vec` per request.
**Problem:** Predictable world-readable `/tmp` filenames invite local symlink/TOCTOU races on a multi-user host (low risk on a single-user Pi). The sonus/audio range handler loads the entire file into memory per request — a large media file on a 512MB Nano node is a memory-pressure risk.
**Recommendation:** Use `tempfile::NamedTempFile` (O_EXCL, 0600) for transient media; stream file bodies with `tokio_util::io::ReaderStream` instead of buffering whole files.

### 🔵 Info (additional)

#### F014 🔵 [Security] `ToolProxy::call` (DirectCall) bypasses the policy engine — currently safe, fragile
**Location:** `agentd/crates/plugins/src/supervisor.rs:49-62` (`ToolProxy::call`), `:264-290` (`DirectCall`).
**Problem:** `ToolProxy` dispatches tools without the policy check. Today its only callers are internal Cerebro bookkeeping (evolution episode tracking in `main.rs`, council `memory_store`) — not reachable from untrusted input, so it's an acceptable controlled bypass. Noted because if `ToolProxy` is ever wired to a gateway HTTP handler or user-supplied tool name, it becomes a full policy bypass.
**Recommendation:** Keep `ToolProxy` internal-only; add a comment/guard documenting that it must never take a user-controlled tool name. The agent → supervisor → policy path is otherwise sound (supervisor independently enforces policy at `:162`, so the agent layer's `needs_approval:false` is harmless).

### 🟢 Low

#### F006 🟢 [Maintainability] Compiler warnings (unused vars/imports, dead field)
**Location:** workspace `cargo check` (ui-slint excluded — needs fontconfig). Examples: unused `q` (×3), unused `scope`, unused import `put`, field `visibility` never read; cerebro 2 warnings, cerebro-api 5 warnings.
**Problem:** Minor dead code / unused bindings.
**Recommendation:** `cargo fix` + address the dead `visibility` field. No correctness impact.

### 🔵 Info

#### F007 🔵 [Build/Deploy] Clippy not available in dev environment
**Location:** dev toolchain.
**Problem:** `cargo clippy` → "no such command: clippy"; lints have likely never been run on this codebase. The deny-list-as-security and unwrap patterns are exactly what clippy/▸`cargo audit` would surface.
**Recommendation:** `rustup component add clippy`, add `cargo clippy --workspace -- -D warnings` and `cargo audit` to a pre-commit / CI gate.

---

## Panic-site catalogue (Pass 1 — to be triaged in per-subsystem passes)
Most `unwrap/expect/panic` sites are in `#[cfg(test)]` code (safe). **Production-reachable** sites flagged for verification in later passes:
- `cerebro/.../storage/sqlite.rs:468,533` — `enum_to_str(v).unwrap()` on node insert (Pass 3)
- `cerebro/.../engines/temporal.rs:167,176` — `metadata["concepts"]` array unwrap/expect (Pass 3)
- `cerebro/crates/cerebro-cli/src/main.rs:288` — `as_object().unwrap()` on output (Pass 3)
- `agentd/crates/gateway/src/lib.rs:964,976,1131` — `.unwrap()` incl. `ff.unwrap()` (Pass 2)
- `agentd/crates/plugins/src/policy.rs:223` — `panic!` on rule parse at config load (Pass 2)
- `ui-slint/src/main.rs:835` — `state.lock().unwrap()` (mutex poison) (Pass 5)
- `tools/crates/apex-sensor-bridge/src/main.rs:62` — `.expect("reqwest client")` at startup (Pass 4)

---

## Per-subsystem coverage notes
- **agentd/gateway** — security surface reviewed (routes, bind, auth, PTY). Remaining: streaming handlers, council, mesh, error paths. (Pass 2)
- agentd/core, plugins, agent, store — pending (Pass 2)
- cerebro — pending (Pass 3)
- tools — pending (Pass 4)
- ui-slint — pending (Pass 5)
- install/deploy/config — pending (Pass 6)
- docs drift — pending (Pass 7)

## Appendix: tooling
- `cargo check --workspace --exclude ui-slint` → compiles clean, ~9 minor warnings.
- `cargo clippy` → unavailable (F007).
- ui-slint not checked locally (fontconfig); build truth is on Pi.

---

## Verification pass — 2026-06-11 (session 2, Opus 4.8)

All 6 fix waves (`a4a2c51` → `f69b656`) verified against actual code, not just the report.
**Result: 30/33 original findings genuinely resolved + F029 folded into Wave 2 + F014/F018 excluded by design.**
Full workspace (incl. ui-slint) **compiles clean**, zero warnings. Spot-verified correct: auth route
coverage (all 41 routes gated, `/sensor-bridge` keeps own token), default `127.0.0.1` bind, F009 UTF-8
carry buffer, F011 broadcast-lagged synthesis, F020 FTS5 per-token quoting, F005 PTY reap + abort,
F008 connect-timeout, F025 reconnect loop, F021 `.transpose()?`, F028 UI service hardening.

The second pass found **4 new items** — one HIGH regression introduced by the auth work, plus residuals:

#### F034 🟠 [Security/Correctness] — REGRESSION: UI REST calls omit the bearer token → 401 on every default install
**Location:** `ui-slint/src/main.rs:452` (`reqwest::Client::new()`, no default auth header); token is appended
to the WS URL only at `:444-445`.
**Problem:** Wave 1 added `?token=` to the **WebSocket** URL but the UI's shared HTTP client carries no token.
`install.sh` now **always** generates `AGENTD_TOKEN`, and `apexos-rs-ui.service` loads `/etc/agentd/env`, so on
every fresh install the token is set and **all ~15 UI REST calls return 401**: `/api/run` (home sys-stats),
`/api/sessions` (picker), `/api/soul` `/api/policy` `/api/model` (settings load+save), `/api/power` (power modal),
`/api/speak` `/api/record/start` `/api/record/stop` `/api/transcribe` (voice). WS chat still works, masking it.
This will surface immediately in a fresh-Pi noob-mode test: chat works, the rest of the UI is dead.
**Recommendation:** Build the UI's client with a default header when the token is set —
`reqwest::Client::builder().default_headers({Authorization: Bearer <AGENTD_TOKEN>})`. One change, all calls fixed.

#### F035 🟡 [Security] — Workspace confinement bypassable via `..` on a non-existent write target
**Location:** `agentd/crates/plugins/src/policy.rs` `workspace_decision()` (~`:120-140`); `write_file`/`create_dir`
have no own path guard (only `delete_path` got `..` rejection in Wave 2).
**Problem:** `workspace_decision` canonicalizes the target, but `write_file`/`create_dir` target paths that don't
exist yet → `canonicalize()` fails → falls back to the **raw** path. `PathBuf::starts_with` is component-wise, so
`<workspace>/../../../etc/cron.d/x` still "starts with" the workspace prefix → returns `Allow` (no confirmation).
Mitigated in production by `agentd.service` `ProtectSystem=strict` + `ReadWritePaths` (the OS write fails), but the
policy decision itself is wrong and a non-systemd dev run is unprotected.
**Recommendation:** Reject any `..` component (mirror `delete_path`), or normalize lexically / canonicalize the
**parent** dir + join the filename before the `starts_with` check.

#### F036 🟢 [Security] — Auth fail-open when token empty + non-loopback bind (operator footgun)
**Location:** `agentd/crates/gateway/src/lib.rs` `require_token` (empty token → pass-through); bind
`agentd/crates/agentd/src/main.rs` (`AGENTD_BIND`); same for cerebro-api / `CEREBRO_API_ADDR`.
**Problem:** Empty `AGENTD_TOKEN` disables auth by design (safe on localhost). But setting `AGENTD_BIND=0.0.0.0`
without a token binds all interfaces with **no auth** and only a stderr warning — re-opens F001 by misconfiguration.
**Recommendation:** Hard-refuse to bind a non-loopback address when the token is empty (fail closed, not a warning).

#### F037 🔵 [Security/UX] — cerebro-api auth also gates its own static dashboard
**Location:** `cerebro/crates/cerebro-api/src/main.rs` (`.layer()` wraps the whole app, including dashboard routes).
**Problem:** Opening `http://host:8765/` in the documented "external browser" returns 401 — a browser can't send a
bearer header, only `?token=`. Minor (localhost-bound), noted in case the dashboard is meant to be browsable.
**Recommendation:** Either accept `?token=` for dashboard navigation (already supported by the middleware) and
document it, or leave the static dashboard public while gating only `/api/*` + destructive routes.

> Tally after verification: original 33 resolved/excluded; **4 new (1 🟠 regression, 1 🟡, 1 🟢, 1 🔵)**.
> F034 is the only must-fix-before-ship — it breaks half the kiosk UI on a default install.
