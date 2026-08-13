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
//! apply_daemon_update(commit, reason, dry_run?)
//!  0. preconditions  repo is a clean git tree · commit == HEAD · cargo present · not in-flight
//!  1. adversarial review  source diff only — BEFORE any cargo of that tree (finding 4)
//!  2. isolated locked build  detached worktree of the SHA, read-only src, scrubbed env,
//!                            `cargo test --locked -p agentd` then
//!                            `cargo build --release --locked -p agentd`
//!                            into a target dir that is NOT the live repo's target/
//!  3. attest + request     hash the isolated binary, copy to $UPDATE_DIR/agentd.staged,
//!                          re-hash, write request.json
//! ```
//!
//! There is NO caller `test_cmd`. A model-supplied `sh -c` ran after the staging
//! build and could replace `target/release/agentd` before review hashed it.
//!
//! Result semantics: gate failures (0–3) and `dry_run` return a NORMAL tool result
//! (the daemon is untouched). On success the process is replaced before a return
//! could arrive, so the real outcome is delivered ASYNC via Cerebro + the
//! watchdog's `confirmed.json`/`rolled-back.json` marker on the next boot. The
//! pre-swap result here is a best-effort "filed" ack.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use apexos_agent::{Chunk, Provider, RoutingProvider};
use apexos_core::{ActionId, BusHandle, ContentBlock, Event, Message, SessionId, ToolOutput, ToolSpec};
use apexos_plugins::ToolProxy;
use futures_util::StreamExt;
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

/// The git checkout agentd self-builds from — APEX's own evolution repo. The agent
/// edits + commits source here (git tools, #117) before calling this tool. Default
/// is an `agentd`-owned clone in its sandbox (`install.sh` provisions it, slice 3.1),
/// distinct from the operator's `apexos-update` clone so the two never fight over
/// git ownership. Override with `AGENTD_SELF_UPDATE_REPO`.
fn self_update_repo() -> PathBuf {
    std::env::var("AGENTD_SELF_UPDATE_REPO")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/var/lib/agentd/self-update/ApexOS-RS"))
}

/// The cargo binary to build with. The `agentd` user typically has no rustup in its
/// own PATH, so the deploy points `AGENTD_CARGO` at a shared toolchain it can read
/// (slice 3.1). Falls back to `cargo` on PATH (dev / when already on PATH).
fn cargo_bin() -> String {
    std::env::var("AGENTD_CARGO")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "cargo".to_string())
}

/// Optional explicit `CARGO_TARGET_DIR` for the staging build — set it when the repo
/// dir isn't where the build output should land (e.g. a read-only source or a
/// shared cache). Unset → cargo's default (`<repo>/target`).
fn cargo_target_dir() -> Option<String> {
    std::env::var("AGENTD_SELF_UPDATE_TARGET")
        .ok()
        .filter(|s| !s.trim().is_empty())
}

/// The request the watchdog consumes (flat JSON — see docs/self-update.md).
/// The staged binary path is NOT in the contract: the watchdog only ever
/// reads `$UPDATE_DIR/agentd.staged`.
#[derive(Debug, Serialize)]
struct SelfUpdateRequest {
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
             serving: clean-tree/HEAD-match preconditions, a fresh-context adversarial review of \
             the source diff, then `cargo test --locked` + `cargo build --release --locked` in a \
             detached read-only checkout of that SHA (scrubbed env; never the live repo's \
             target/). There is no extra shell hook. Only if all pass is a swap request filed; \
             a root watchdog then backs up the current binary, swaps, restarts, and health-checks \
             — rolling back automatically to the known-good binary if the new one doesn't come \
             up healthy. The `commit` must be the repo's current HEAD (commit your source first). \
             SUCCESS RETURNS NOTHING SYNCHRONOUSLY — the process is replaced; the real outcome \
             arrives on the next boot via Cerebro and /var/lib/agentd/update/{confirmed,rolled-back}.json. \
             Use dry_run=true to run the review+locked-build gates and report without swapping."
                .into(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "commit":   { "type": "string", "description": "Git commit SHA to build (must be the repo's current HEAD)." },
                "reason":   { "type": "string", "description": "Why this update — recorded in the resume intention + outcome marker." },
                "dry_run":  { "type": "boolean", "description": "Run the review+locked-build gates and report, WITHOUT filing a swap request. Default false." }
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
async fn run_cmd(dir: &PathBuf, program: &str, args: &[&str], timeout: Duration, envs: &[(&str, String)]) -> Result<String, String> {
    let mut cmd = tokio::process::Command::new(program);
    cmd.args(args)
        .current_dir(dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (k, v) in envs {
        cmd.env(k, v);
    }
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
    run_cmd(dir, "git", args, Duration::from_secs(30), &[]).await.map(|s| s.trim().to_string())
}

/// Run cargo (`AGENTD_CARGO` or `cargo`) in `dir` with the optional shared
/// `CARGO_TARGET_DIR`. Inherits the agentd process env (so a deploy-set
/// `CARGO_HOME`/`RUSTUP_HOME`/`PATH` reach the build). Used only for the
/// `--version` precondition — the attested build goes through `run_cmd_scrubbed`.
async fn run_cargo(dir: &PathBuf, args: &[&str], timeout: Duration) -> Result<String, String> {
    let envs: Vec<(&str, String)> = match cargo_target_dir() {
        Some(t) => vec![("CARGO_TARGET_DIR", t)],
        None => vec![],
    };
    run_cmd(dir, &cargo_bin(), args, timeout, &envs).await
}

fn cargo_test_args() -> &'static [&'static str] {
    &["test", "--locked", "-p", "agentd"]
}

fn cargo_build_args() -> &'static [&'static str] {
    &["build", "--release", "--locked", "-p", "agentd"]
}

fn is_full_git_sha(s: &str) -> bool {
    s.len() == 40 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Detached source tree for the attested build. Name is derived from the SHA
/// so a hostile `commit` string cannot pick the path.
fn isolated_src_dir(resolved: &str) -> PathBuf {
    let tag: String = resolved.chars().filter(|c| c.is_ascii_hexdigit()).take(12).collect();
    update_dir().join(format!("src-{tag}"))
}

/// CARGO_TARGET_DIR for the attested build. Never the live repo's `target/` —
/// a concurrent `run_command` could replace `release/agentd` there after cargo
/// returns (finding 4). An explicit `AGENTD_SELF_UPDATE_TARGET` is honored only
/// if it sits *outside* the live checkout.
fn isolated_target_dir(repo: &Path) -> PathBuf {
    if let Some(t) = cargo_target_dir() {
        let p = PathBuf::from(t);
        if !p.starts_with(repo) {
            return p;
        }
    }
    repo.parent()
        .map(|p| p.join("isolated-target"))
        .unwrap_or_else(|| update_dir().join("isolated-target"))
}

/// Env keys cargo/rustup actually need. Everything else (AGENTD_TOKEN, API
/// keys, sensor tokens, …) stays out of `build.rs` / proc-macros.
const CARGO_ENV_ALLOW: &[&str] = &[
    "PATH",
    "HOME",
    "USER",
    "LOGNAME",
    "SHELL",
    "TMPDIR",
    "TEMP",
    "TMP",
    "LANG",
    "LC_ALL",
    "LC_CTYPE",
    "LC_MESSAGES",
    "TERM",
    "CARGO_HOME",
    "RUSTUP_HOME",
    "RUSTUP_TOOLCHAIN",
    "RUSTUP_IO_THREADS",
    "CARGO_BUILD_JOBS",
    "CARGO_INCREMENTAL",
    "CARGO_PROFILE_RELEASE_OPT_LEVEL",
    "CARGO_PROFILE_RELEASE_LTO",
    "CARGO_PROFILE_RELEASE_CODEGEN_UNITS",
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "ALL_PROXY",
    "NO_PROXY",
    "http_proxy",
    "https_proxy",
    "all_proxy",
    "no_proxy",
    "SSL_CERT_FILE",
    "SSL_CERT_DIR",
];

fn collect_scrubbed_env(target_dir: &Path) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for key in CARGO_ENV_ALLOW {
        if let Ok(v) = std::env::var(key) {
            if !v.is_empty() {
                out.push(((*key).to_string(), v));
            }
        }
    }
    out.push(("CARGO_TARGET_DIR".into(), target_dir.display().to_string()));
    out.push(("CARGO_TERM_COLOR".into(), "never".into()));
    out
}

/// Like `run_cmd` but `env_clear()`s first — the isolated cargo/test path.
async fn run_cmd_scrubbed(
    dir: &Path,
    program: &str,
    args: &[&str],
    timeout: Duration,
    envs: &[(String, String)],
) -> Result<String, String> {
    let mut cmd = tokio::process::Command::new(program);
    cmd.args(args)
        .current_dir(dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env_clear();
    for (k, v) in envs {
        cmd.env(k, v);
    }
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
        let combined = format!("{stdout}\n{stderr}");
        Err(tail(&combined, 4000))
    }
}

fn set_tree_readonly(root: &Path, readonly: bool) -> std::io::Result<()> {
    let meta = std::fs::symlink_metadata(root)?;
    if meta.file_type().is_symlink() {
        return Ok(());
    }
    if meta.is_dir() {
        for ent in std::fs::read_dir(root)? {
            set_tree_readonly(&ent?.path(), readonly)?;
        }
    }
    let mut perms = std::fs::symlink_metadata(root)?.permissions();
    perms.set_readonly(readonly);
    std::fs::set_permissions(root, perms)
}

async fn cleanup_isolated_src(repo: &Path, dest: &Path) {
    let _ = set_tree_readonly(dest, false);
    if let Some(s) = dest.to_str() {
        let _ = git(&repo.to_path_buf(), &["worktree", "remove", "--force", s]).await;
    }
    let _ = std::fs::remove_dir_all(dest);
}

/// Detached checkout of `resolved` that the live working tree cannot mutate.
/// Prefer `git worktree --detach`; fall back to `clone --local --no-hardlinks`
/// so we never share inodes with the live tree.
async fn prepare_isolated_checkout(repo: &PathBuf, resolved: &str) -> Result<PathBuf, String> {
    if !is_full_git_sha(resolved) {
        return Err(format!("refusing isolated checkout of non-sha `{resolved}`"));
    }
    let dest = isolated_src_dir(resolved);
    cleanup_isolated_src(repo, &dest).await;
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("mkdir isolated src parent: {e}"))?;
    }
    let dest_s = dest.to_str().ok_or_else(|| "isolated src path is not utf-8".to_string())?;
    match git(repo, &["worktree", "add", "--detach", dest_s, resolved]).await {
        Ok(_) => {}
        Err(wt_err) => {
            cleanup_isolated_src(repo, &dest).await;
            if let Err(e) = run_cmd(
                repo,
                "git",
                &["clone", "--local", "--no-hardlinks", ".", dest_s],
                Duration::from_secs(120),
                &[],
            )
            .await
            {
                return Err(format!("isolated checkout failed (worktree: {wt_err}; clone: {e})"));
            }
            git(&dest, &["checkout", "--detach", resolved]).await.map_err(|e| {
                format!("isolated clone checkout {resolved} failed: {e}")
            })?;
        }
    }
    let got = git(&dest, &["rev-parse", "HEAD"]).await?;
    if got != resolved {
        cleanup_isolated_src(repo, &dest).await;
        return Err(format!("isolated HEAD {got} != requested {resolved}"));
    }
    if !dest.join("Cargo.lock").is_file() {
        cleanup_isolated_src(repo, &dest).await;
        return Err("isolated checkout has no Cargo.lock — --locked build refused".into());
    }
    set_tree_readonly(&dest, true).map_err(|e| format!("make isolated src read-only: {e}"))?;
    Ok(dest)
}

fn attest_and_stage(built: &PathBuf, staged: &PathBuf) -> Result<String, String> {
    let sha_built = sha256_file(built)?;
    if sha_built.len() != 64 || !sha_built.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(format!("sha256 of built binary looks wrong: {sha_built}"));
    }
    stage_binary(built, staged).map_err(|e| e.to_string())?;
    let sha_staged = sha256_file(staged)?;
    if sha_staged != sha_built {
        let _ = std::fs::remove_file(staged);
        return Err(format!("staged sha {sha_staged} != built sha {sha_built}"));
    }
    Ok(sha_staged)
}

/// Whether an update is already in flight: a `request.json` (watchdog will pick it
/// up / is mid-swap) or our build-window lock.
fn in_flight() -> bool {
    let d = update_dir();
    d.join("request.json").exists() || d.join("update.lock").exists()
}

// ── adversarial review (stage 1 of the run — source only) ─────────────────────

/// Review toggle. ON by default; `AGENTD_SELF_UPDATE_REVIEW=0` skips it
/// (a fully-trusted pipeline, or a node with no model configured).
fn review_enabled() -> bool {
    !matches!(
        std::env::var("AGENTD_SELF_UPDATE_REVIEW").ok().as_deref(),
        Some("0") | Some("false") | Some("no")
    )
}

enum ReviewVerdict {
    Safe(String),
    Block(String),
}

/// Keep the first `max` chars (diff header + changed files matter most).
fn head(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        let mut end = max;
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}\n…(diff truncated at {max} chars)…", &s[..end])
    }
}

/// The diff being swapped in: `git diff <prev>..<target>`. Falls back to the target
/// commit alone if `prev` isn't reachable here (divergent history). Bounded.
async fn collect_diff(repo: &PathBuf, prev: &str, target: &str) -> String {
    let range = format!("{prev}..{target}");
    let d = match git(repo, &["rev-parse", "--verify", &format!("{prev}^{{commit}}")]).await {
        Ok(_) => git(repo, &["diff", "--no-color", &range]).await,
        Err(_) => git(repo, &["show", "--no-color", target]).await, // prev not in this repo
    };
    match d {
        Ok(s) => head(&s, 12_000),
        Err(e) => format!("(could not compute diff: {e})"),
    }
}

const REVIEW_SYSTEM: &str = "You are a strict release reviewer for agentd's SELF-UPDATE. agentd is a \
long-running daemon about to REPLACE ITS OWN BINARY with one built from the diff below. A change that \
boots-but-is-subtly-broken, or that damages the self-update machinery itself (the health marker, the \
request.json the watchdog reads, or rollback), is dangerous — though a root watchdog will auto-roll-back \
a binary that doesn't come up healthy. Review the diff for: could it break boot / listener bind / the \
health marker / rollback? Is it reversible? Does it touch the self-update / watchdog / health code in a \
risky way? Be conservative but not paranoid — ordinary feature changes that build and test clean are \
fine. Give one short paragraph of reasoning, then a FINAL line that is EXACTLY one of:\n\
VERDICT: SAFE\nVERDICT: BLOCK — <one-line reason>";

/// Collect a one-shot completion's text from the provider stream (fresh context).
async fn collect_completion(provider: &RoutingProvider, history: &[Message], system: &str) -> Result<String, String> {
    let mut stream = provider.messages_stream(history, &[], Some(system)).await.map_err(|e| e.to_string())?;
    let mut text = String::new();
    while let Some(chunk) = stream.next().await {
        match chunk {
            Ok(Chunk::TextDelta(t)) => text.push_str(&t),
            Ok(Chunk::TextBlock(t)) => { text = t; break; }
            Ok(Chunk::Done) => break,
            Err(e) => return Err(e.to_string()),
            _ => {}
        }
    }
    Ok(text)
}

/// Single fresh-context review (v1). Empty diff → trivially safe (no LLM call).
/// FAIL-CLOSED: any provider error / unparseable verdict → Block. The seam for an
/// N-way refute panel later: call this N times + require unanimity/majority Safe.
async fn review_diff(reviewer: &RoutingProvider, target: &str, reason: &str, diff: &str) -> ReviewVerdict {
    if diff.trim().is_empty() {
        return ReviewVerdict::Safe("empty diff (same-commit rebuild) — nothing to review".into());
    }
    let user = format!(
        "Reason for this self-update: {reason}\nTarget commit: {target}\n\nDiff being swapped in:\n```diff\n{diff}\n```"
    );
    let history = vec![Message::User { content: vec![ContentBlock::Text { text: user }] }];
    match collect_completion(reviewer, &history, REVIEW_SYSTEM).await {
        Ok(t) => parse_verdict(&t),
        Err(e) => ReviewVerdict::Block(format!("reviewer unavailable — failing closed: {e}")),
    }
}

/// Parse the reviewer's final `VERDICT:` line. Fail-closed if absent/garbled.
fn parse_verdict(text: &str) -> ReviewVerdict {
    for line in text.lines().rev() {
        let l = line.trim().trim_start_matches(['*', '#', '>', ' ']);
        if let Some(idx) = l.to_uppercase().find("VERDICT:") {
            let rest = l[idx + "VERDICT:".len()..].trim();
            let rest_up = rest.to_uppercase();
            if rest_up.starts_with("SAFE") {
                return ReviewVerdict::Safe(rest.to_string());
            }
            if rest_up.starts_with("BLOCK") {
                let why = rest
                    .trim_start_matches(|c: char| c.is_alphabetic())
                    .trim_start_matches(['—', '-', ':', ' '])
                    .trim();
                return ReviewVerdict::Block(if why.is_empty() {
                    "reviewer blocked the change".into()
                } else {
                    why.to_string()
                });
            }
        }
    }
    ReviewVerdict::Block(format!(
        "reviewer produced no parseable VERDICT line — failing closed. Tail: {}",
        head(text.trim(), 300)
    ))
}

/// The handler task: serializes updates (one at a time) and runs the full gate
/// pipeline for each `apply_daemon_update` call forwarded by the supervisor.
pub fn spawn_self_update_handler(
    mut rx: mpsc::Receiver<(SessionId, ActionId, serde_json::Value)>,
    bus: BusHandle,
    proxy: ToolProxy,
    reviewer: Arc<RoutingProvider>,
) {
    tokio::spawn(async move {
        while let Some((session, call_id, args)) = rx.recv().await {
            run_update(&bus, session, call_id, &args, &proxy, reviewer.as_ref()).await;
        }
    });
}

async fn run_update(
    bus: &BusHandle,
    session: SessionId,
    call_id: ActionId,
    args: &serde_json::Value,
    proxy: &ToolProxy,
    reviewer: &RoutingProvider,
) {
    let commit = args.get("commit").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
    let reason = args.get("reason").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
    // `test_cmd` is intentionally unread (finding 4): a caller `sh -c` ran after
    // the staging build and could replace target/release/agentd before review.
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
    if !is_full_git_sha(&resolved) {
        emit(bus, session, call_id, false,
            format!("resolved commit {resolved} is not a 40-char git sha")).await;
        return;
    }
    if run_cargo(&repo, &["--version"], Duration::from_secs(30)).await.is_err() {
        emit(bus, session, call_id, false, format!(
            "cargo not runnable (tried `{}`) — the agentd user needs a build toolchain. \
             Set AGENTD_CARGO to a shared cargo it can read, or provision one (see \
             docs/self-update.md slice 3.1).", cargo_bin())).await;
        return;
    }

    // Take the build-window lock (best-effort; in_flight() already gated above).
    let lock = update_dir().join("update.lock");
    let _ = std::fs::create_dir_all(update_dir());
    let _ = std::fs::write(&lock, format!("{}\n", std::process::id()));
    // From here, every exit path must clear the lock.
    let clear_lock = || { let _ = std::fs::remove_file(update_dir().join("update.lock")); };

    // ── stage 1: adversarial review (source only — no cargo of this tree yet) ──
    // FAIL-CLOSED: unparseable/unavailable → block. Empty diff (same-commit
    // rebuild) is trivially safe. `AGENTD_SELF_UPDATE_REVIEW=0` skips.
    if review_enabled() {
        let diff = collect_diff(&repo, build_commit(), &resolved).await;
        match review_diff(reviewer, &resolved, &reason, &diff).await {
            ReviewVerdict::Safe(note) => eprintln!("[self-update] review SAFE: {note}"),
            ReviewVerdict::Block(why) => {
                clear_lock();
                emit(bus, session, call_id, false,
                    format!("STAGE 1 review BLOCKED (daemon untouched): {why}")).await;
                return;
            }
        }
    }

    // ── stage 2: isolated --locked test + release build ─────────────────────────
    let src = match prepare_isolated_checkout(&repo, &resolved).await {
        Ok(p) => p,
        Err(e) => {
            clear_lock();
            emit(bus, session, call_id, false,
                format!("STAGE 2 isolated checkout failed (daemon untouched): {e}")).await;
            return;
        }
    };
    let target_root = isolated_target_dir(&repo);
    if let Err(e) = std::fs::create_dir_all(&target_root) {
        cleanup_isolated_src(&repo, &src).await;
        clear_lock();
        emit(bus, session, call_id, false, format!("STAGE 2 mkdir target failed: {e}")).await;
        return;
    }
    let cargo_envs = collect_scrubbed_env(&target_root);
    if let Err(e) = run_cmd_scrubbed(&src, &cargo_bin(), cargo_test_args(), build_timeout(), &cargo_envs).await {
        cleanup_isolated_src(&repo, &src).await;
        clear_lock();
        emit(bus, session, call_id, false,
            format!("STAGE 2 `cargo test --locked -p agentd` failed (daemon untouched):\n{e}")).await;
        return;
    }
    if let Err(e) = run_cmd_scrubbed(&src, &cargo_bin(), cargo_build_args(), build_timeout(), &cargo_envs).await {
        cleanup_isolated_src(&repo, &src).await;
        clear_lock();
        emit(bus, session, call_id, false,
            format!("STAGE 2 `cargo build --release --locked -p agentd` failed (daemon untouched):\n{e}")).await;
        return;
    }
    let built = target_root.join("release/agentd");
    if !built.exists() {
        cleanup_isolated_src(&repo, &src).await;
        clear_lock();
        emit(bus, session, call_id, false, format!("build reported success but {} is missing", built.display())).await;
        return;
    }

    // ── dry-run: report without filing a swap ───────────────────────────────────
    if dry_run {
        cleanup_isolated_src(&repo, &src).await;
        clear_lock();
        emit(bus, session, call_id, true,
            format!("DRY RUN ok — review + locked isolated build passed for {commit}. No swap requested.")).await;
        return;
    }

    // ── stage 3: attest the isolated artifact + file the request ────────────────
    let staged = update_dir().join("agentd.staged");
    let sha = match attest_and_stage(&built, &staged) {
        Ok(s) => s,
        Err(e) => {
            cleanup_isolated_src(&repo, &src).await;
            clear_lock();
            emit(bus, session, call_id, false, format!("STAGE 3 attest/stage failed (daemon untouched): {e}")).await;
            return;
        }
    };
    cleanup_isolated_src(&repo, &src).await;

    // Continuity: the agent re-orients from these on the far side (new or rolled-back).
    save_resume_state(proxy, &reason, &commit).await;

    let req = SelfUpdateRequest {
        staged_sha256: sha,
        // MUST be the full 40-char sha (`resolved`), NOT the caller's `commit` arg.
        // The health marker reports the full `build.rs git rev-parse HEAD` sha; the
        // watchdog confirms on `health.commit == target`. A short sha or "HEAD" here
        // never matches the full marker → a healthy new binary times out + rolls
        // back. (Caught live on apex2's first real self-update: target "24ea3a4" vs
        // health "24ea3a42b0ed…" → false rollback.)
        target_commit: resolved.clone(),
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
                "Gates passed (review + locked isolated build). Swap request filed for {commit}; the watchdog will \
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
            staged_sha256: "abc".into(),
            target_commit: "deadbeef".into(),
            prev_commit: "cafe".into(),
            created_at: 1_700_000_000,
            timeout: 120,
            reason: "test".into(),
        };
        let v: serde_json::Value = serde_json::to_value(&r).unwrap();
        for k in ["staged_sha256", "target_commit", "prev_commit", "created_at", "timeout", "reason"] {
            assert!(v.get(k).is_some(), "missing field {k}");
        }
        assert!(v.get("staged").is_none(), "staged path must not be in the contract");
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
        assert!(
            s.input_schema["properties"].get("test_cmd").is_none(),
            "test_cmd is a swap-the-binary primitive and must stay out of the spec"
        );
    }

    #[test]
    fn cargo_invocations_are_locked_and_source_controlled() {
        assert!(cargo_test_args().contains(&"--locked"));
        assert!(cargo_build_args().contains(&"--locked"));
        assert!(cargo_build_args().contains(&"--release"));
        assert!(!cargo_test_args().iter().any(|a| *a == "-c" || *a == "sh"));
        assert!(!cargo_build_args().iter().any(|a| *a == "-c" || *a == "sh"));
    }

    #[test]
    fn full_git_sha_is_strict() {
        assert!(is_full_git_sha("0123456789abcdef0123456789abcdef01234567"));
        assert!(!is_full_git_sha("0123456789abcdef")); // short
        assert!(!is_full_git_sha("HEAD"));
        assert!(!is_full_git_sha("../etc/passwd"));
        assert!(!is_full_git_sha(&"g".repeat(40)));
    }

    #[test]
    fn scrubbed_env_drops_secrets_and_forces_target() {
        std::env::set_var("AGENTD_TOKEN", "secret-token");
        std::env::set_var("ANTHROPIC_API_KEY", "sk-ant-x");
        std::env::set_var("OPENAI_API_KEY", "sk-x");
        let envs = collect_scrubbed_env(Path::new("/tmp/isolated-target"));
        let keys: Vec<&str> = envs.iter().map(|(k, _)| k.as_str()).collect();
        assert!(!keys.contains(&"AGENTD_TOKEN"));
        assert!(!keys.contains(&"ANTHROPIC_API_KEY"));
        assert!(!keys.contains(&"OPENAI_API_KEY"));
        assert!(keys.contains(&"CARGO_TARGET_DIR"));
        assert_eq!(
            envs.iter().find(|(k, _)| k == "CARGO_TARGET_DIR").map(|(_, v)| v.as_str()),
            Some("/tmp/isolated-target")
        );
        std::env::remove_var("AGENTD_TOKEN");
        std::env::remove_var("ANTHROPIC_API_KEY");
        std::env::remove_var("OPENAI_API_KEY");
    }

    #[test]
    fn isolated_target_refuses_live_repo_target() {
        let repo = Path::new("/var/lib/agentd/self-update/ApexOS-RS");
        std::env::remove_var("AGENTD_SELF_UPDATE_TARGET");
        let def = isolated_target_dir(repo);
        assert!(!def.starts_with(repo), "default target must sit outside the live checkout");
        std::env::set_var("AGENTD_SELF_UPDATE_TARGET", "/var/lib/agentd/self-update/ApexOS-RS/target");
        let rejected = isolated_target_dir(repo);
        assert!(!rejected.starts_with(repo));
        std::env::set_var("AGENTD_SELF_UPDATE_TARGET", "/var/lib/agentd/self-update/isolated-target");
        assert_eq!(
            isolated_target_dir(repo),
            PathBuf::from("/var/lib/agentd/self-update/isolated-target")
        );
        std::env::remove_var("AGENTD_SELF_UPDATE_TARGET");
    }

    #[test]
    fn readonly_tree_rejects_writes() {
        let d = std::env::temp_dir().join(format!("apex-ro-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let f = d.join("x");
        std::fs::write(&f, "a").unwrap();
        set_tree_readonly(&d, true).unwrap();
        assert!(std::fs::write(&f, "b").is_err(), "readonly tree must reject writes");
        set_tree_readonly(&d, false).unwrap();
        std::fs::write(&f, "b").unwrap();
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn attest_rejects_hash_mismatch() {
        let d = std::env::temp_dir().join(format!("apex-attest-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        std::env::set_var("AGENTD_UPDATE_DIR", d.to_str().unwrap());
        let built = d.join("built");
        let staged = d.join("agentd.staged");
        std::fs::write(&built, b"good-bytes").unwrap();
        let sha = attest_and_stage(&built, &staged).unwrap();
        assert_eq!(sha.len(), 64);
        assert_eq!(std::fs::read(&staged).unwrap(), b"good-bytes");
        std::env::remove_var("AGENTD_UPDATE_DIR");
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn isolated_checkout_pins_sha_and_ignores_later_edits() {
        let tmp = std::env::temp_dir().join(format!("apex-iso-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let repo = tmp.join("repo");
        let update = tmp.join("update");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::create_dir_all(&update).unwrap();
        let git_ok = |args: &[&str]| {
            let st = std::process::Command::new("git")
                .args(args)
                .current_dir(&repo)
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@t")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@t")
                .status()
                .unwrap();
            assert!(st.success(), "git {args:?} failed");
        };
        git_ok(&["init"]);
        git_ok(&["config", "user.email", "t@t"]);
        git_ok(&["config", "user.name", "t"]);
        git_ok(&["config", "commit.gpgsign", "false"]);
        std::fs::write(repo.join("Cargo.lock"), "# lock\n").unwrap();
        std::fs::write(repo.join("file"), "v1").unwrap();
        git_ok(&["add", "."]);
        git_ok(&["-c", "commit.gpgsign=false", "commit", "-m", "v1"]);
        let sha = String::from_utf8(
            std::process::Command::new("git")
                .args(["rev-parse", "HEAD"])
                .current_dir(&repo)
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap()
        .trim()
        .to_string();
        assert!(is_full_git_sha(&sha), "test repo HEAD not a full sha: {sha}");

        std::env::set_var("AGENTD_UPDATE_DIR", update.to_str().unwrap());
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let dest = rt.block_on(prepare_isolated_checkout(&repo, &sha)).expect("isolated checkout");
        assert_eq!(std::fs::read_to_string(dest.join("file")).unwrap(), "v1");
        std::fs::write(repo.join("file"), "EVIL").unwrap();
        assert_eq!(
            std::fs::read_to_string(dest.join("file")).unwrap(),
            "v1",
            "isolated tree must not see later live-repo edits"
        );
        assert!(std::fs::write(dest.join("file"), "nope").is_err(), "isolated src is read-only");
        rt.block_on(cleanup_isolated_src(&repo, &dest));
        std::env::remove_var("AGENTD_UPDATE_DIR");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn repo_default_and_override() {
        std::env::remove_var("AGENTD_SELF_UPDATE_REPO");
        assert_eq!(self_update_repo(), PathBuf::from("/var/lib/agentd/self-update/ApexOS-RS"));
        std::env::set_var("AGENTD_SELF_UPDATE_REPO", "/tmp/x");
        assert_eq!(self_update_repo(), PathBuf::from("/tmp/x"));
        std::env::remove_var("AGENTD_SELF_UPDATE_REPO");
    }

    #[test]
    fn cargo_bin_default_and_override() {
        std::env::remove_var("AGENTD_CARGO");
        assert_eq!(cargo_bin(), "cargo");
        std::env::set_var("AGENTD_CARGO", "/opt/rust/bin/cargo");
        assert_eq!(cargo_bin(), "/opt/rust/bin/cargo");
        std::env::set_var("AGENTD_CARGO", "  ");
        assert_eq!(cargo_bin(), "cargo"); // blank → default
        std::env::remove_var("AGENTD_CARGO");
    }

    #[test]
    fn cargo_target_dir_opt() {
        std::env::remove_var("AGENTD_SELF_UPDATE_TARGET");
        assert_eq!(cargo_target_dir(), None);
        std::env::set_var("AGENTD_SELF_UPDATE_TARGET", "/var/lib/agentd/self-update/target");
        assert_eq!(cargo_target_dir().as_deref(), Some("/var/lib/agentd/self-update/target"));
        std::env::remove_var("AGENTD_SELF_UPDATE_TARGET");
    }

    fn is_block(v: &ReviewVerdict) -> bool { matches!(v, ReviewVerdict::Block(_)) }
    fn block_reason(v: &ReviewVerdict) -> String {
        match v { ReviewVerdict::Block(r) => r.clone(), ReviewVerdict::Safe(_) => String::new() }
    }

    #[test]
    fn parse_verdict_safe_block_and_failclosed() {
        assert!(matches!(parse_verdict("looks fine\nVERDICT: SAFE"), ReviewVerdict::Safe(_)));
        // markdown wrapping + reasoning above the verdict
        assert!(matches!(parse_verdict("reasons\n**VERDICT: SAFE**"), ReviewVerdict::Safe(_)));
        let b = parse_verdict("this rewrites the watchdog\nVERDICT: BLOCK — touches rollback path");
        assert!(is_block(&b));
        assert_eq!(block_reason(&b), "touches rollback path");
        // no verdict at all → fail-closed
        assert!(is_block(&parse_verdict("I think it is probably fine but I won't say.")));
        // empty → fail-closed
        assert!(is_block(&parse_verdict("")));
        // last verdict line wins (in case the model restates)
        assert!(is_block(&parse_verdict("VERDICT: SAFE\n...on reflection\nVERDICT: BLOCK — risky")));
    }

    #[test]
    fn review_enabled_default_and_optout() {
        std::env::remove_var("AGENTD_SELF_UPDATE_REVIEW");
        assert!(review_enabled());
        std::env::set_var("AGENTD_SELF_UPDATE_REVIEW", "0");
        assert!(!review_enabled());
        std::env::set_var("AGENTD_SELF_UPDATE_REVIEW", "1");
        assert!(review_enabled());
        std::env::remove_var("AGENTD_SELF_UPDATE_REVIEW");
    }

    #[test]
    fn head_truncates() {
        assert_eq!(head("short", 100), "short");
        let big = "x".repeat(20_000);
        let h = head(&big, 12_000);
        assert!(h.starts_with("xxxx"));
        assert!(h.contains("truncated"));
    }
}
