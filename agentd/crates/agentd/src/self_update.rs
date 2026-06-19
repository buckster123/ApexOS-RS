//! `apply_daemon_update` — the agent-facing trigger of the daemon self-update loop
//! (docs/self-update.md, slice 3). The one tool that replaces the running process.
//!
//! It runs the PRE-SWAP gates (all while the live daemon keeps serving), and only
//! if they pass does it write `request.json` — handing off to the root watchdog
//! (slice 2, proven on apex2) for the privileged swap + health-gated rollback.
//! agentd never escalates; it only drops a request file behind the privilege
//! boundary.
//!
//! ```text
//! apply_daemon_update(commit, reason, test_cmd?, dry_run?)
//!  0. preconditions  repo is a clean git tree · commit == HEAD · cargo present · not in-flight
//!  1. staging build  cargo build --release -p agentd   (in repo; never over the live binary)
//!  2. tests          cargo test -p agentd  +  caller test_cmd
//!  (3. adversarial review — slice 4, not yet wired)
//!  4. pre-swap commit  session_save() + store_intention("resuming…") · write request.json
//! ```
//!
//! Result semantics: gate failures (0–2) and `dry_run` return a NORMAL tool result
//! (the daemon is untouched). On success the process is replaced before a return
//! could arrive, so the real outcome is delivered ASYNC via Cerebro + the
//! watchdog's `confirmed.json`/`rolled-back.json` marker on the next boot. The
//! pre-swap result here is a best-effort "filed" ack.

use std::path::PathBuf;
use std::process::Stdio;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use apexos_core::{ActionId, BusHandle, Event, SessionId, ToolOutput, ToolSpec};
use apexos_plugins::ToolProxy;
use serde::Serialize;
use tokio::sync::mpsc;

use crate::health::{build_commit, update_dir};

/// Generous ceiling for the on-node `cargo build` + tests (Nano-tier is slow).
fn build_timeout() -> Duration {
    let secs = std::env::var("AGENTD_SELF_UPDATE_BUILD_TIMEOUT")
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(1800);
    Duration::from_secs(secs)
}

/// Health-probe seconds written into `request.json` for the watchdog (locked
/// default 120s; env-tunable).
fn probe_timeout() -> u64 {
    std::env::var("AGENTD_SELF_UPDATE_TIMEOUT")
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(120)
}

/// The git checkout agentd self-builds from. The agent edits + commits source here
/// (git tools, #117) before calling this tool. Default matches the design's
/// `AGENTD_GIT_ROOTS=/opt/ApexOS-RS`.
fn self_update_repo() -> PathBuf {
    std::env::var("AGENTD_SELF_UPDATE_REPO")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/opt/ApexOS-RS"))
}

/// The request the watchdog consumes (flat JSON — see docs/self-update.md).
#[derive(Debug, Serialize)]
struct SelfUpdateRequest {
    staged: String,
    staged_sha256: String,
    target_commit: String,
    prev_commit: String,
    created_at: u64,
    timeout: u64,
    reason: String,
}

pub fn apply_daemon_update_spec() -> ToolSpec {
    ToolSpec {
        name: "apply_daemon_update".into(),
        description:
            "Rebuild and swap in a new agentd (this daemon's own binary) from a committed git \
             ref, guarded by the self-update watchdog. PRE-SWAP gates run while the daemon keeps \
             serving: clean-tree/HEAD-match preconditions, a staging `cargo build --release -p \
             agentd` (never over the live binary), then `cargo test -p agentd` plus any caller \
             `test_cmd`. Only if all pass is a swap request filed; a root watchdog then backs up \
             the current binary, swaps, restarts, and health-checks — rolling back automatically \
             to the known-good binary if the new one doesn't come up healthy. The `commit` must \
             be the repo's current HEAD (commit your source first). SUCCESS RETURNS NOTHING \
             SYNCHRONOUSLY — the process is replaced; the real outcome arrives on the next boot \
             via Cerebro and /var/lib/agentd/update/{confirmed,rolled-back}.json. Use \
             dry_run=true to run the build+test gates and report without swapping."
                .into(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "commit":   { "type": "string", "description": "Git commit SHA to build (must be the repo's current HEAD)." },
                "reason":   { "type": "string", "description": "Why this update — recorded in the resume intention + outcome marker." },
                "test_cmd": { "type": "string", "description": "Optional extra test command, run as `sh -c` in the repo after the built-in tests." },
                "dry_run":  { "type": "boolean", "description": "Run the build+test gates and report, WITHOUT filing a swap request. Default false." }
            },
            "required": ["commit", "reason"]
        }),
    }
}

fn now_unix() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Emit a (final) tool result on the bus.
async fn emit(bus: &BusHandle, session: SessionId, call_id: ActionId, ok: bool, msg: impl Into<String>) {
    bus.emit(Event::ToolResult {
        session,
        call: call_id,
        output: ToolOutput { ok, content: serde_json::json!(msg.into()) },
    })
    .await;
}

/// Run a command in `dir`, bounded by `timeout`. Returns combined stdout+stderr on
/// success; `Err(message)` on non-zero exit, timeout, or spawn failure.
async fn run_cmd(dir: &PathBuf, program: &str, args: &[&str], timeout: Duration) -> Result<String, String> {
    let mut cmd = tokio::process::Command::new(program);
    cmd.args(args)
        .current_dir(dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let child = cmd.spawn().map_err(|e| format!("spawn `{program}` failed: {e}"))?;
    let out = match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(Ok(o)) => o,
        Ok(Err(e)) => return Err(format!("`{program}` wait failed: {e}")),
        Err(_) => return Err(format!("`{program} {}` timed out after {}s", args.join(" "), timeout.as_secs())),
    };
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    if out.status.success() {
        Ok(format!("{stdout}{stderr}"))
    } else {
        // Tail the output so a giant compile log doesn't blow the tool result.
        let combined = format!("{stdout}\n{stderr}");
        Err(tail(&combined, 4000))
    }
}

/// Keep the last `max` chars (compiler errors live at the end).
fn tail(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("…(truncated)…\n{}", &s[s.len() - max..])
    }
}

async fn git(dir: &PathBuf, args: &[&str]) -> Result<String, String> {
    run_cmd(dir, "git", args, Duration::from_secs(30)).await.map(|s| s.trim().to_string())
}

/// Whether an update is already in flight: a `request.json` (watchdog will pick it
/// up / is mid-swap) or our build-window lock.
fn in_flight() -> bool {
    let d = update_dir();
    d.join("request.json").exists() || d.join("update.lock").exists()
}

/// The handler task: serializes updates (one at a time) and runs the full gate
/// pipeline for each `apply_daemon_update` call forwarded by the supervisor.
pub fn spawn_self_update_handler(
    mut rx: mpsc::Receiver<(SessionId, ActionId, serde_json::Value)>,
    bus: BusHandle,
    proxy: ToolProxy,
) {
    tokio::spawn(async move {
        while let Some((session, call_id, args)) = rx.recv().await {
            run_update(&bus, session, call_id, &args, &proxy).await;
        }
    });
}

async fn run_update(
    bus: &BusHandle,
    session: SessionId,
    call_id: ActionId,
    args: &serde_json::Value,
    proxy: &ToolProxy,
) {
    let commit = args.get("commit").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
    let reason = args.get("reason").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
    let test_cmd = args.get("test_cmd").and_then(|v| v.as_str()).map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    let dry_run = args.get("dry_run").and_then(|v| v.as_bool()).unwrap_or(false);

    if commit.is_empty() || reason.is_empty() {
        emit(bus, session, call_id, false, "apply_daemon_update requires `commit` and `reason`").await;
        return;
    }

    // ── stage 0: preconditions (live daemon untouched on any failure) ───────────
    if in_flight() {
        emit(bus, session, call_id, false,
            "a daemon update is already in flight (request.json/lock present) — wait for its outcome marker").await;
        return;
    }
    let repo = self_update_repo();
    if !repo.join(".git").exists() {
        emit(bus, session, call_id, false,
            format!("self-update repo not found at {} (set AGENTD_SELF_UPDATE_REPO)", repo.display())).await;
        return;
    }
    // Clean tree: no uncommitted drift may leak into the build.
    match git(&repo, &["status", "--porcelain"]).await {
        Ok(s) if !s.is_empty() => {
            emit(bus, session, call_id, false,
                format!("repo {} has uncommitted changes — commit or stash first:\n{}", repo.display(), tail(&s, 1000))).await;
            return;
        }
        Err(e) => { emit(bus, session, call_id, false, format!("git status failed: {e}")).await; return; }
        _ => {}
    }
    // commit must resolve AND equal HEAD (v1: build the committed HEAD in place).
    let head = match git(&repo, &["rev-parse", "HEAD"]).await {
        Ok(h) => h,
        Err(e) => { emit(bus, session, call_id, false, format!("git rev-parse HEAD failed: {e}")).await; return; }
    };
    let resolved = match git(&repo, &["rev-parse", "--verify", &format!("{commit}^{{commit}}")]).await {
        Ok(r) => r,
        Err(_) => { emit(bus, session, call_id, false, format!("commit {commit} does not resolve in {}", repo.display())).await; return; }
    };
    if resolved != head {
        emit(bus, session, call_id, false,
            format!("commit {commit} ({resolved}) is not the repo HEAD ({head}); check it out first (v1 builds HEAD in place)")).await;
        return;
    }
    if run_cmd(&repo, "cargo", &["--version"], Duration::from_secs(30)).await.is_err() {
        emit(bus, session, call_id, false, "cargo not available on PATH — cannot build").await;
        return;
    }

    // Take the build-window lock (best-effort; in_flight() already gated above).
    let lock = update_dir().join("update.lock");
    let _ = std::fs::create_dir_all(update_dir());
    let _ = std::fs::write(&lock, format!("{}\n", std::process::id()));
    // From here, every exit path must clear the lock.
    let clear_lock = || { let _ = std::fs::remove_file(update_dir().join("update.lock")); };

    // ── stage 1: staging build (never over the live binary) ─────────────────────
    if let Err(e) = run_cmd(&repo, "cargo", &["build", "--release", "-p", "agentd"], build_timeout()).await {
        clear_lock();
        emit(bus, session, call_id, false, format!("STAGE 1 build failed (daemon untouched):\n{e}")).await;
        return;
    }
    let built = repo.join("target/release/agentd");
    if !built.exists() {
        clear_lock();
        emit(bus, session, call_id, false, format!("build reported success but {} is missing", built.display())).await;
        return;
    }

    // ── stage 2: tests ──────────────────────────────────────────────────────────
    if let Err(e) = run_cmd(&repo, "cargo", &["test", "-p", "agentd"], build_timeout()).await {
        clear_lock();
        emit(bus, session, call_id, false, format!("STAGE 2 `cargo test -p agentd` failed (daemon untouched):\n{e}")).await;
        return;
    }
    if let Some(tc) = &test_cmd {
        if let Err(e) = run_cmd(&repo, "sh", &["-c", tc], build_timeout()).await {
            clear_lock();
            emit(bus, session, call_id, false, format!("STAGE 2 test_cmd failed (daemon untouched):\n{e}")).await;
            return;
        }
    }

    // (stage 3 adversarial review — slice 4)

    // ── dry-run: report without filing a swap ───────────────────────────────────
    if dry_run {
        clear_lock();
        emit(bus, session, call_id, true,
            format!("DRY RUN ok — build + tests passed for {commit}. No swap requested.")).await;
        return;
    }

    // ── stage 4: pre-swap commit (Cerebro continuity + the request) ─────────────
    let staged = update_dir().join("agentd.staged");
    if let Err(e) = stage_binary(&built, &staged) {
        clear_lock();
        emit(bus, session, call_id, false, format!("failed to stage built binary: {e}")).await;
        return;
    }
    let sha = match sha256_file(&staged) {
        Ok(s) => s,
        Err(e) => { clear_lock(); emit(bus, session, call_id, false, format!("sha256 of staged failed: {e}")).await; return; }
    };

    // Continuity: the agent re-orients from these on the far side (new or rolled-back).
    save_resume_state(proxy, &reason, &commit).await;

    let req = SelfUpdateRequest {
        staged: staged.to_string_lossy().to_string(),
        staged_sha256: sha,
        target_commit: commit.clone(),
        prev_commit: build_commit().to_string(),
        created_at: now_unix(),
        timeout: probe_timeout(),
        reason: reason.clone(),
    };

    // Writing request.json is the commit point — it triggers the watchdog (.path),
    // which will stop this process. Clear the build lock first (request.json is now
    // the in-flight guard); then write the request and ack best-effort.
    clear_lock();
    match write_request(&req) {
        Ok(()) => {
            emit(bus, session, call_id, true, format!(
                "Gates passed (build + tests). Swap request filed for {commit}; the watchdog will \
                 back up the current binary, swap, restart, and health-check — rolling back \
                 automatically if it doesn't come up healthy. This process is being replaced now; \
                 the outcome will appear on the next boot via Cerebro and \
                 /var/lib/agentd/update/{{confirmed,rolled-back}}.json.")).await;
        }
        Err(e) => {
            emit(bus, session, call_id, false, format!("failed to write request.json (daemon untouched): {e}")).await;
        }
    }
}

/// Copy the built binary to the staged path, preserving the executable bit (the
/// watchdog's final rename takes the staged file's mode, so it MUST be +x).
fn stage_binary(built: &PathBuf, staged: &PathBuf) -> std::io::Result<()> {
    std::fs::create_dir_all(update_dir())?;
    std::fs::copy(built, staged)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(staged, std::fs::Permissions::from_mode(0o755))?;
    }
    Ok(())
}

fn sha256_file(path: &PathBuf) -> Result<String, String> {
    let out = std::process::Command::new("sha256sum")
        .arg(path)
        .output()
        .map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).to_string());
    }
    String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .next()
        .map(|s| s.to_string())
        .ok_or_else(|| "empty sha256sum output".to_string())
}

/// Atomic request write (temp + rename within the update dir).
fn write_request(req: &SelfUpdateRequest) -> std::io::Result<()> {
    let dir = update_dir();
    std::fs::create_dir_all(&dir)?;
    let json = serde_json::to_string_pretty(req).map_err(std::io::Error::other)?;
    let tmp = dir.join("request.json.tmp");
    std::fs::write(&tmp, &json)?;
    std::fs::rename(&tmp, dir.join("request.json"))
}

/// session_save + a resume intention so the agent re-orients on the far side via
/// the normal cognitive_bootstrap. Best-effort + bounded — never blocks the swap.
async fn save_resume_state(proxy: &ToolProxy, reason: &str, commit: &str) {
    let agent = apexos_core::node_agent_id();
    let summary = format!(
        "Self-update in progress: rebuilding agentd to {commit} ({reason}). The process will be \
         replaced and health-checked; if it doesn't come up healthy the watchdog rolls back to \
         the previous binary. On wake, check /var/lib/agentd/update/confirmed.json vs \
         rolled-back.json for the outcome."
    );
    let _ = proxy.call("session_save", serde_json::json!({
        "session_summary": summary,
        "agent_id": agent,
        "priority": "HIGH",
    })).await;
    let _ = proxy.call("store_intention", serde_json::json!({
        "content": format!("resuming after self-update to {commit}: {reason} — verify confirmed.json vs rolled-back.json, and if rolled back, fix and retry."),
        "agent_id": agent,
        "salience": 0.9,
    })).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tail_keeps_the_end() {
        assert_eq!(tail("short", 100), "short");
        let big = "x".repeat(5000);
        let t = tail(&big, 100);
        assert!(t.starts_with("…(truncated)…"));
        assert!(t.len() < 200);
    }

    #[test]
    fn request_serializes_to_the_watchdog_schema() {
        let r = SelfUpdateRequest {
            staged: "/var/lib/agentd/update/agentd.staged".into(),
            staged_sha256: "abc".into(),
            target_commit: "deadbeef".into(),
            prev_commit: "cafe".into(),
            created_at: 1_700_000_000,
            timeout: 120,
            reason: "test".into(),
        };
        let v: serde_json::Value = serde_json::to_value(&r).unwrap();
        for k in ["staged", "staged_sha256", "target_commit", "prev_commit", "created_at", "timeout", "reason"] {
            assert!(v.get(k).is_some(), "missing field {k}");
        }
        assert_eq!(v["target_commit"], "deadbeef");
        assert_eq!(v["timeout"], 120);
    }

    #[test]
    fn spec_has_required_fields() {
        let s = apply_daemon_update_spec();
        assert_eq!(s.name, "apply_daemon_update");
        let req = &s.input_schema["required"];
        assert!(req.as_array().unwrap().iter().any(|v| v == "commit"));
        assert!(req.as_array().unwrap().iter().any(|v| v == "reason"));
    }

    #[test]
    fn repo_default_and_override() {
        std::env::remove_var("AGENTD_SELF_UPDATE_REPO");
        assert_eq!(self_update_repo(), PathBuf::from("/opt/ApexOS-RS"));
        std::env::set_var("AGENTD_SELF_UPDATE_REPO", "/tmp/x");
        assert_eq!(self_update_repo(), PathBuf::from("/tmp/x"));
        std::env::remove_var("AGENTD_SELF_UPDATE_REPO");
    }
}
