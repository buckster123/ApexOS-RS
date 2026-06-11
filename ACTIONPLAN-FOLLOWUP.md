# ApexOS-RS — Follow-up Action Plan (Wave 7)

> **For:** Sonnet (bullet-train 🚄)
> **From:** Opus 4.8 verification pass, 2026-06-11 (session 2)
> **Context:** Your 6 fix waves (`a4a2c51`→`f69b656`) were verified against actual code — **all 33 original
> findings genuinely resolved, full workspace compiles clean incl. ui-slint, zero warnings. Excellent work.**
> A second pass found **4 new items** introduced/left by the auth work. Full detail in `REVIEW.md` →
> "Verification pass — session 2". This file is the actionable plan for them.
>
> **Priority:** F034 is a **must-fix before the fresh-Pi end-to-end test** — it breaks ~half the kiosk UI on a
> default install. F035 + F036 are cheap, do them same trip. F037 is optional/cosmetic.
>
> **Cerebro routine:** `session_recall(query="ApexOS-RS audit verification F034", agent_id="FORGE")` first;
> `session_save` + commit-per-fix at the end. **Build rule:** `cargo check --workspace` must stay clean.

---

## F034 🟠 HIGH — UI REST calls omit the bearer token → 401 on every default install

**Why this is urgent:** Wave 1 appended `?token=` to the **WebSocket** URL only. The UI's shared HTTP client
carries no token. `install.sh` now **always** generates `AGENTD_TOKEN`, and `apexos-rs-ui.service` loads
`/etc/agentd/env`, so on every fresh install the token IS set → **all ~15 UI REST calls return 401**: home
sys-stats (`/api/run`), session picker (`/api/sessions`), settings load+save (`/api/soul` `/api/policy`
`/api/model`), power modal (`/api/power`), and all voice (`/api/speak` `/api/record/start` `/api/record/stop`
`/api/transcribe`). WS chat still works, which masks the breakage. This will look like a disaster in the
noob-mode test when it's actually a one-spot fix.

**File:** `ui-slint/src/main.rs`
**Location:** line **452** — `let http_client = Arc::new(reqwest::Client::new());`
The token is already resolved just above at lines 441–448 (the `ws_url` block reads `AGENTD_TOKEN`).

**Change — replace line 452:**
```rust
    // Shared HTTP client — carries the bearer token (if set) on every REST call,
    // mirroring the ?token= already on the WS URL. Without this, every /api/* call
    // 401s whenever AGENTD_TOKEN is set (which install.sh now always does).
    let http_client = Arc::new({
        let mut builder = reqwest::Client::builder();
        if let Ok(t) = std::env::var("AGENTD_TOKEN") {
            if !t.is_empty() {
                let mut headers = reqwest::header::HeaderMap::new();
                if let Ok(val) = reqwest::header::HeaderValue::from_str(&format!("Bearer {t}")) {
                    headers.insert(reqwest::header::AUTHORIZATION, val);
                }
                builder = builder.default_headers(headers);
            }
        }
        builder.build().unwrap_or_default()
    });
```
No new `use` lines needed (fully-qualified `reqwest::header::*`). `Client: Default`, so `unwrap_or_default()`
is safe. Every existing call site already clones this `http_client`, so they're all fixed at once.

**Verify:**
1. `cargo check -p ui-slint` clean.
2. Manual smoke (dev box, against the Pi or local agentd):
   ```bash
   AGENTD_TOKEN=$(grep ^AGENTD_TOKEN= /etc/agentd/env | cut -d= -f2) \
   AGENTD_WS=ws://localhost:8787/ws cargo run -p ui-slint
   ```
   Confirm the home dashboard shows CPU/RAM/disk (proves `/api/run` is authed), the Settings tab loads soul/policy,
   and the power modal opens without a 401 in the logs.

---

## F035 🟡 MEDIUM — Workspace confinement bypassable via `..` on a non-existent write target

**Why:** `workspace_decision()` canonicalizes the target, but `write_file`/`create_dir` target paths that don't
exist yet → `std::fs::canonicalize(p)` fails → falls back to the **raw** path. `PathBuf::starts_with` is
component-wise, so `<workspace>/../../../etc/cron.d/x` still "starts with" the workspace prefix → returns
`Allow` with no confirmation. `delete_path` already rejects `..` (Wave 2) but writes don't. In production
`agentd.service` `ProtectSystem=strict` + `ReadWritePaths` blocks the actual OS write, but the **policy decision
itself is wrong** and a non-systemd dev run is unprotected.

**File:** `agentd/crates/plugins/src/policy.rs`
**Location:** `fn workspace_decision` starts line **118**. Insert immediately after the `path` guard:

```rust
    fn workspace_decision(&self, path: Option<&str>) -> Decision {
        let Some(p) = path else { return Decision::Ask };
        // Reject traversal: a non-existent write target with `..` would otherwise
        // canonicalize-fail and slip past the component-prefix check below.
        // Mirrors the guard delete_path already applies.
        if p.contains("..") { return Decision::Ask; }
        let Ok(ws) = std::env::var("AGENTD_WORKSPACE") else { return Decision::Ask };
        // ... rest unchanged ...
```

**Verify:** `cargo test -p apexos-plugins` (the 4 workspace tests from Wave 2 should still pass). Add one case:
a `write_file` decision for `"<workspace>/../escape"` must return `Decision::Ask`, not `Allow`.

---

## F036 🟢 LOW — Auth fail-open when token empty + non-loopback bind (fail closed instead)

**Why:** Empty `AGENTD_TOKEN` disables auth by design (safe on localhost). But `AGENTD_BIND=0.0.0.0` (or
`CEREBRO_API_ADDR=0.0.0.0:8765`) without a token binds all interfaces unauthenticated with only a stderr
warning — a single env typo re-opens F001/F019. Make it fail closed.

### Site A — `agentd/crates/agentd/src/main.rs`
`api_token` is moved into `gw_state` (~line 228), so capture emptiness **before** that. Near line 168 (where
`api_token` is created), add:
```rust
    let api_token_empty = api_token.is_empty();
```
Then at the bind (lines 239–240) add the guard:
```rust
    let gw_bind = std::env::var("AGENTD_BIND").unwrap_or_else(|_| "127.0.0.1:8787".into());
    let gw_addr: std::net::SocketAddr = gw_bind.parse()?;
    if api_token_empty && !gw_addr.ip().is_loopback() {
        anyhow::bail!(
            "refusing to bind {gw_addr} without AGENTD_TOKEN — set a token or bind 127.0.0.1"
        );
    }
```

### Site B — `cerebro/crates/cerebro-api/src/main.rs`
`api_token` (Arc) is still in scope at the bind (line 934; `token_mw` is a clone). Add after the `addr` line:
```rust
    let addr = std::env::var("CEREBRO_API_ADDR")
        .unwrap_or_else(|_| "127.0.0.1:8765".into());
    if api_token.is_empty() {
        if let Ok(sa) = addr.parse::<std::net::SocketAddr>() {
            if !sa.ip().is_loopback() {
                anyhow::bail!("refusing to bind {addr} without AGENTD_TOKEN");
            }
        }
    }
```
(The `if let Ok` guard means a hostname like `localhost:8765` that doesn't parse as a `SocketAddr` is left
alone — acceptable; only literal non-loopback IPs are blocked.)

**Verify:** `cargo check --workspace` clean. `AGENTD_BIND=0.0.0.0:8787` with no `AGENTD_TOKEN` must exit with the
bail message; `127.0.0.1:8787` with no token must still start (warning only).

---

## F037 🔵 INFO — OPTIONAL — cerebro-api auth gates its own static dashboard

cerebro-api's `.layer()` wraps the whole app, so opening `http://host:8765/` in the documented external browser
returns 401 (a browser can't send a bearer header, only `?token=`). The middleware already accepts `?token=`, so
the cheapest fix is **documentation**: note that the dashboard is reached as `http://host:8765/?token=<AGENTD_TOKEN>`.
Alternatively, split the router so static/dashboard routes are public and only `/api/*` + destructive routes are
gated. Low priority — skip unless the dashboard is a shipped mk1 feature.

---

## Definition of done

- [ ] F034, F035, F036 applied; F037 decided (fix or document).
- [ ] `cargo check --workspace` clean, no new warnings.
- [ ] `cargo test -p apexos-plugins` passes (incl. new F035 `..` case).
- [ ] Manual UI smoke with `AGENTD_TOKEN` set: dashboard stats + settings + power + voice all work (no 401s).
- [ ] One commit per fix (or one `wave 7` commit), pushed. Update `REVIEW.md` verification section to mark
      F034–F037 resolved, and tick the `AUDIT-PLAN.md` session log.
- [ ] `session_save` (FORGE, HIGH): note F034 was the UI-token regression and that the fresh-Pi e2e test is next.

Then Andre wipes the testing Pi → full fresh-OS `sudo bash install.sh` end-to-end noob-mode run.
