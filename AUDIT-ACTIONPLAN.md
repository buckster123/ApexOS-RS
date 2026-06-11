# ApexOS-RS mk1 — Audit Action Plan

> Living tracker. Update status column as work lands. Commit this file with each wave.
> Source: `REVIEW.md` (33 findings, Opus 4.8, 2026-06-11).
> Status: ⬜ todo · 🟦 in progress · ✅ done

---

## Fix waves (ordered by leverage)

### Wave 1 — Authentication layer *(closes 7 findings in one shot)*
> Root cause: both daemons bind `0.0.0.0` with zero auth. Pattern already exists (`sensor_bridge_token`). Fix once, close F001/F002/F003/F004/F015/F016/F019.

**Approach:**
1. Generate `AGENTD_TOKEN` at install time → `/etc/agentd/env`
2. agentd: change default bind to `127.0.0.1:8787`; add env var `AGENTD_BIND` for opt-in LAN exposure
3. agentd gateway: add a tower middleware layer checking `Authorization: Bearer <token>` on all routes (reuse `sensor_bridge_token` shape — already wired into `GatewayState`)
4. cerebro-api: change default bind to `127.0.0.1:8765` (already reads `CEREBRO_API_ADDR`); add same bearer middleware
5. ui-slint: read token from env / config file; pass as `Authorization` header on WS + HTTP requests
6. install.sh: write `AGENTD_TOKEN=$(openssl rand -hex 32)` to `/etc/agentd/env` on first install (idempotent — don't overwrite existing)

| ID | Finding | Status |
|----|---------|--------|
| F001 | `/api/run` unauthenticated RCE | ✅ |
| F002 | `/terminal-ws` unauthenticated shell | ✅ |
| F003 | `/api/key` unauthenticated key read/overwrite | ✅ |
| F004 | `/api/power` unauthenticated reboot/shutdown | ✅ |
| F015 | `/api/backend` unauthenticated LLM-hijack | ✅ |
| F016 | `/api/vast/*` + `/api/mesh/*` unauthenticated | ✅ |
| F019 | cerebro-api unauthenticated on 0.0.0.0 | ✅ |

**Files:**
- `agentd/crates/agentd/src/main.rs` — bind address + token generation
- `agentd/crates/gateway/src/lib.rs` — bearer middleware layer
- `cerebro/crates/cerebro-api/src/main.rs` — bind address + bearer middleware
- `ui-slint/src/main.rs` — pass token on connect
- `install.sh` — generate + write token
- `deploy/agentd.service`, `deploy/cerebro-api.service` — document bind vars

---

### Wave 2 — Policy: make the safety gate actually work *(3 findings)*
> F027 is the most dangerous: the policy allowlist matches no real tool name → every tool falls to `Ask` → operators flip to `yolo` to make it usable. Fix the naming, implement Workspace, harden path checks.

| ID | Finding | Status |
|----|---------|--------|
| F027 | Policy rule keys (`fs.read`, `shell.run`) don't match real tool names (`read_file`, `run_command`) — allowlist inert | ✅ |
| F010 | `Workspace` rule returns `Allow` unconditionally — path confinement never enforced | ✅ |
| F024 | `delete_path` protection list incomplete; no path canonicalization; no workspace confinement on file tools | ✅ |

**Approach:**
- F027: align rule names in `config/policy.toml` (new canonical file) + `install.sh` with actual tool names: `run_command`, `read_file`, `write_file`, `delete_path`, `http_fetch`; add cerebro wildcard `recall`, `memory_store`, etc. Add startup warning when a policy rule references an unknown tool.
- F010: implement the path check in `policy.rs` — resolve tool path arg against `AGENTD_WORKSPACE`, canonicalize, reject traversal/symlinks; or degrade to `Ask` until implemented.
- F024: canonicalize in `delete_path`; expand blocked set to include `/etc`, `/home`, `/root`, `/var`, `/var/lib/agentd`; reject `..` / symlinks.
- F029: add `config/policy.toml` as source-of-truth; install.sh copies it instead of writing inline.

**Files:**
- `config/policy.toml` — new file, canonical rules
- `agentd/crates/plugins/src/policy.rs` — Workspace path check + unknown-rule warning
- `tools/crates/apexos-tools/src/tools.rs` — `delete_path` + file tool path hardening
- `install.sh` — copy policy.toml instead of writing inline

---

### Wave 3 — UI reconnect *(1 finding, kiosk reliability)*
> F025: WS connect task returns/breaks on failure — UI stays dead until process restart. Directly breaks the documented hot-swap deploy workflow.

| ID | Finding | Status |
|----|---------|--------|
| F025 | UI WebSocket never reconnects after disconnect or initial failure | ✅ |

**Approach:** Wrap the connect + read loop in an outer `loop` with exponential backoff (start 2s, cap 30s). Re-send `session_init` on each reconnect. Update CLAUDE.md claim only after behavior matches.

**Files:**
- `ui-slint/src/main.rs` — WS task (`:454-525`)

---

### Wave 4 — Correctness bugs *(5 findings)*
> Runtime bugs that affect real output quality. UTF-8 drops affect non-ASCII text on slow links; FTS5 breaks recall on any query with operators; broadcast drop frames a successful tool as failed.

| ID | Finding | Status |
|----|---------|--------|
| F009 | UTF-8 chunk-split drops streamed text silently | ✅ |
| F011 | Broadcast `Lagged` drop reports successful tool as failure; 30-min hang | ✅ |
| F020 | FTS5 query not escaped — any query with operators errors and returns nothing | ✅ |
| F021 | `enum_to_str(v).unwrap()` latent panic on DB write path | ✅ |
| F022 | Dead `dyn_params` construction in `fts5_search` | ✅ |

**Files:**
- `agentd/crates/agent/src/anthropic.rs` — incremental UTF-8 decode (F009)
- `agentd/crates/agent/src/oai.rs` — same (F009)
- `agentd/crates/agent/src/turn.rs` — tool result delivery / Lagged handling (F011)
- `cerebro/crates/cerebro/src/storage/vector.rs` — FTS5 phrase-wrap (F020); dead code (F022)
- `cerebro/crates/cerebro/src/storage/sqlite.rs` — valence `unwrap` → `?` (F021)

---

### Wave 5 — Resource / service hardening *(3 findings)*

| ID | Finding | Status |
|----|---------|--------|
| F005 | PTY child not reaped (zombie per session) + orphaned WS task | ⬜ |
| F008 | No connect timeout or retry on LLM provider HTTP — hangs indefinitely | ⬜ |
| F028 | `apexos-rs-ui.service` runs as root with no systemd hardening | ⬜ |

**Files:**
- `agentd/crates/gateway/src/lib.rs` — `child.wait().await` after kill; abort losing task (F005)
- `agentd/crates/agent/src/anthropic.rs` — `connect_timeout(10s)` + bounded retry (F008)
- `agentd/crates/agent/src/oai.rs` — same (F008)
- `deploy/apexos-rs-ui.service` — add `NoNewPrivileges`, `ProtectHome`, `PrivateTmp`, `DeviceAllow=/dev/dri/* /dev/tty7` (F028)

---

### Wave 6 — Docs, build, and low-effort cleanup *(11 findings)*
> Mix of doc accuracy, dead code, and one-liner fixes. Fast to knock out in a batch.

| ID | Finding | Status |
|----|---------|--------|
| F031 | README/CLAUDE.md say `localhost` but daemon binds `0.0.0.0`; "no attack surface" claim | ⬜ |
| F032 | README badge says "planning" — project is mk1 complete | ⬜ |
| F033 | `.gitignore` excludes `Cargo.lock` — wrong for a binary workspace | ⬜ |
| F012 | PTY terminal shipped but listed as deferred in CLAUDE.md | ⬜ |
| F023 | cerebro-api port mismatch: code `8765` vs service description `8767` | ⬜ |
| F026 | `AppState` mutex `.lock().unwrap()` — poison cascade | ⬜ |
| F013 | Predictable `/tmp/apex_*` paths; whole-file-in-RAM media serving | ⬜ |
| F017 | SSH tunnel `StrictHostKeyChecking=no` — MITM on inference tunnel | ⬜ |
| F006 | Compiler warnings (unused vars/imports/dead field) | ⬜ |
| F007 | clippy not installed in dev environment | ⬜ |
| F030 | `curl | sudo bash` install has no checksum — document trust assumption | ⬜ |

---

## Findings not actioned (info / deferred by design)

| ID | Finding | Disposition |
|----|---------|-------------|
| F014 | `ToolProxy::call` bypasses policy — currently safe (internal only) | Add comment guard; no code change needed |
| F018 | Event log flushes but never fsyncs | Accepted tradeoff per reviewer; add comment |

---

## Progress summary

| Wave | Findings | Done | Status |
|------|----------|------|--------|
| 1 — Auth layer | 7 | 7 | ✅ |
| 2 — Policy | 3 | 3 | ✅ |
| 3 — UI reconnect | 1 | 1 | ✅ |
| 4 — Correctness | 5 | 5 | ✅ |
| 5 — Resource/service | 3 | 0 | ⬜ |
| 6 — Docs/cleanup | 11 | 0 | ⬜ |
| **Total** | **30** | **7** | |

*(F014, F018 excluded — no code change)*

---

## Session log

- **2026-06-11:** Action plan created from REVIEW.md (33 findings, Opus 4.8). Waves 1–6 defined.
- **2026-06-11:** Wave 1 complete. Both daemons default-bind `127.0.0.1`. `AGENTD_TOKEN` bearer middleware on all agentd API+WS routes (gated router split); same token on cerebro-api. UI appends `?token=` to WS URL. install.sh generates token once at install. All 162 tests green.
- **2026-06-11:** Wave 3 complete. WS task wrapped in outer `'reconnect: loop` with exponential backoff (2s→4s→...→30s cap). Status shows "Connection failed — retrying in Ns" / "Disconnected — reconnecting in Ns". session_init re-sent on each reconnect.
- **2026-06-11:** Wave 4 complete. F009: incremental UTF-8 carry buffer in anthropic.rs + oai.rs SSE decoders (no more silent drop on split multi-byte chars). F011: broadcast Lagged returns Ok(false) instead of `continue`; outer match falls through to error synthesis immediately instead of 30-min hang. F020: FTS5 query escapes each token as individual quoted phrase ("word1" "word2" = implicit AND, neutralizes operators). F021: emotional_valence enum_to_str `.unwrap()` → `.transpose()?` (both insert + update). F022: removed dead dyn_params construction + drop in fts5_search.
- **2026-06-11:** Wave 2 complete. Created `config/policy.toml` with real tool names (`read_file`, `write_file`, `run_command`, `delete_path`, `http_fetch`). install.sh copies it instead of writing inline. `policy.rs check()` gains `path: Option<&str>` and implements actual workspace path check via `AGENTD_WORKSPACE` canonicalization. `delete_path` hardened: `..` traversal rejected, symlinks resolved via canonicalize, denylist expanded (+ `/etc /home /root /var`), workspace confinement added. 4 new workspace policy tests. All tests green.
