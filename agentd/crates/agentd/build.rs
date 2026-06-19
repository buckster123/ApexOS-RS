//! Build script — embed the git commit SHA so a running agentd can report *which*
//! binary it is. The self-update health marker reports this commit and the root
//! watchdog matches it against the requested target (docs/self-update.md, slice 1).
//! The compiled-in commit is the trustworthy "what am I running" signal: env vars
//! and on-disk markers can lie, the embedded constant can't.
//!
//! Best-effort: falls back to "unknown" when git is unavailable (e.g. a source
//! tarball build or a checkout with no `.git`).

use std::process::Command;

fn main() {
    let commit = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_string());
    println!("cargo:rustc-env=GIT_COMMIT={commit}");

    // Re-run only when the checked-out commit changes. `logs/HEAD` is appended on
    // every commit / checkout / reset, so it catches new commits on the *same*
    // branch (plain `HEAD` only changes on a branch switch) — which matters for
    // the self-update staging build, which builds at a freshly-committed ref.
    // Emitting any rerun-if-changed disables Cargo's default full-package rescan
    // for this build script, which is correct: the embed depends only on git
    // state, never on the crate's source files.
    if let Ok(out) = Command::new("git").args(["rev-parse", "--git-dir"]).output() {
        if out.status.success() {
            if let Ok(gitdir) = String::from_utf8(out.stdout) {
                let gitdir = gitdir.trim();
                println!("cargo:rerun-if-changed={gitdir}/HEAD");
                println!("cargo:rerun-if-changed={gitdir}/logs/HEAD");
            }
        }
    }
}
