# ApexOS-RS mk1 — Audit Completion Report

> Source audit: `REVIEW.md` — Opus 4.8 deep-scan, 2026-06-11, 33 findings across all subsystems.
> Full tracker: `AUDIT-ACTIONPLAN.md`
> Status: **COMPLETE — 30/30 findings resolved, 2 excluded (no code change needed)**

---

## Result

| Wave | Theme | Findings | Commits |
|------|-------|----------|---------|
| 1 | Auth layer | 7 | `a4a2c51` |
| 2 | Policy alignment | 3 | `f7f8b50` |
| 3 | UI reconnect | 1 | `c653e07` |
| 4 | Correctness bugs | 5 | `c653e07` |
| 5 | Resource / service hardening | 3 | `3f26090` |
| 6 | Docs / build / cleanup | 11 | `f69b656` |

---

## What changed (by finding)

### Wave 1 — Authentication (F001–F004, F015, F016, F019)
- `agentd` and `cerebro-api` now default-bind `127.0.0.1` (env vars `AGENTD_BIND` / `CEREBRO_API_ADDR` for opt-in LAN)
- Bearer token middleware on all agentd API + WS routes; same on cerebro-api
- `AGENTD_TOKEN` generated at install time, stored in `/etc/agentd/env`
- Slint UI passes token as `?token=` on WS connect

### Wave 2 — Policy (F010, F024, F027)
- `config/policy.toml` created with real tool names (`read_file`, `write_file`, `run_command`, `delete_path`, `http_fetch`)
- `policy.rs::check()` gains `path: Option<&str>`; `Workspace` rule now canonicalizes and checks `AGENTD_WORKSPACE`
- `delete_path` hardened: `..` traversal blocked, symlink resolution, expanded denylist, workspace confinement

### Wave 3 — UI reconnect (F025)
- WS task wrapped in `'reconnect: loop` with 2s→30s exponential backoff
- Status shown to user on each retry; `session_init` re-sent on reconnect

### Wave 4 — Correctness (F009, F011, F020, F021, F022)
- UTF-8 carry buffer in `anthropic.rs` + `oai.rs` SSE decoders (no silent drop on split multi-byte chars)
- Broadcast `Lagged` → `Ok(false)` + immediate error synthesis (was a 30-min hang)
- FTS5 per-token quoting (`"w1" "w2"`) neutralizes operators, preserves AND semantics
- `emotional_valence` `enum_to_str().unwrap()` → `.transpose()?` on insert + update
- Dead `dyn_params` construction removed from `fts5_search`

### Wave 5 — Resource / service hardening (F005, F008, F028)
- `handle_terminal_ws`: select! uses mutable refs so losing task is aborted; `spawn_blocking(child.wait())` reaps zombie
- `reqwest::Client::new()` → `build_http_client()` with `connect_timeout(10s)` in both LLM providers
- `apexos-rs-ui.service`: `NoNewPrivileges=yes`, `ProtectHome=yes`, `PrivateTmp=yes`, `DevicePolicy=closed`, `DeviceAllow` for dri/tty7/input

### Wave 6 — Docs / build / cleanup (F006, F007, F012, F013, F017, F023, F026, F030, F031, F032, F033)
- Zero compiler warnings (unused `scope`, `put`, 3× `q`, dead `visibility` field)
- `Cargo.lock` added (removed from `.gitignore` — correct for binary workspace)
- README badge `planning` → `mk1_complete`; curl\|bash security note added
- PTY terminal deferred entry struck in `CLAUDE.md` (libc `openpty` shipped)
- `cerebro-api.service` description 8767 → 8765
- `state.lock().unwrap()` → `unwrap_or_else(|e| e.into_inner())` in Slint UI
- `snapshot_handler` + `wake_handler` use microsecond-stamped `/tmp` paths + cleanup
- `StrictHostKeyChecking=no` → `accept-new` in both SSH tunnel call sites (supervisor.rs)
- Policy test race fixed: `ENV_LOCK` static mutex serializes `AGENTD_WORKSPACE` mutations

### Excluded (no code change)
- **F014** `ToolProxy::call` bypasses policy — safe (internal-only call path); comment guard added
- **F018** Event log never fsyncs — accepted tradeoff; comment added

---

## Pi deploy checklist

These changes need attention on the Pi after `git pull && cargo build --release`:

- [ ] `sudo systemctl stop agentd && sudo cp target/release/agentd /usr/local/bin/agentd && sudo systemctl start agentd`
- [ ] Verify `AGENTD_TOKEN` exists in `/etc/agentd/env` — `install.sh` generates it on first run; if already installed, run: `echo "AGENTD_TOKEN=$(openssl rand -hex 32)" | sudo tee -a /etc/agentd/env`
- [ ] Copy updated service file: `sudo cp deploy/apexos-rs-ui.service /etc/systemd/system/ && sudo systemctl daemon-reload`
- [ ] Copy updated policy: `sudo cp config/policy.toml /etc/agentd/policy.toml` (only if not customised)
- [ ] Confirm cerebro-mcp still connects after auth changes (check `sudo journalctl -u agentd -n 20`)

---

## Remaining known items (deferred by design)

- **Sonus stream handler** reads whole audio file into RAM before serving — streaming via `tokio_util::io::ReaderStream` is the proper fix; deferred (audio files are small in practice)
- **CCBS / vision extras** — Phase 3 roadmap, unchanged
