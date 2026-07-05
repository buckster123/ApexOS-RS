//! A5 red-team — adversarial corpus against path confinement.
//!
//! `apexos-confine` is the FS sandbox: writes hard-confine to the workspace,
//! reads also reach an allowlist minus a secret set. A tricked tool will try
//! every trick to escape. The properties pinned here:
//!
//!   1. `..` traversal is refused in every form.
//!   2. Symlink escape is resolved (canonicalized) and refused — including a
//!      *chain* of symlinks, the classic confused-deputy dodge.
//!   3. **Prefix-collision does not confuse containment** — a sibling like
//!      `/tmp/ws-evil` is NOT inside `/tmp/ws`. (`Path::starts_with` is
//!      component-wise, not byte-wise; this proves it and guards a future
//!      refactor to a naive string check.)
//!   4. Absolute paths outside the workspace are refused for writes.
//!
//! std-only, deterministic, uses real temp dirs (no tempfile dep).

use apexos_confine::{confine_fs, confine_to_roots, has_traversal, Access, Denied};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

static N: AtomicUsize = AtomicUsize::new(0);

fn mktmp(tag: &str) -> PathBuf {
    let n = N.fetch_add(1, Ordering::Relaxed);
    let p = std::env::temp_dir().join(format!("apexos-confine-rt-{}-{tag}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).unwrap();
    std::fs::canonicalize(&p).unwrap()
}

// ── 1. Traversal in every form ────────────────────────────────────────────────

#[test]
fn traversal_variants_all_flagged() {
    let cases = [
        "../etc/shadow",
        "a/../../etc/shadow",
        "./../../x",
        "foo/bar/../../../..",
        "..",
        "a/b/../../../c",
    ];
    for c in cases {
        assert!(has_traversal(Path::new(c)), "{c:?} must be flagged as traversal");
    }
    // Non-traversal lookalikes must NOT be flagged (no false positives that
    // would break legitimate names).
    for ok in ["foo..bar", "a/b/c", "...hidden", "file..txt", "a/foo..bar/b"] {
        assert!(!has_traversal(Path::new(ok)), "{ok:?} must NOT be flagged");
    }
}

#[test]
fn write_with_traversal_is_denied_before_touching_fs() {
    let ws = mktmp("ws");
    for esc in ["../escape", "sub/../../escape", ".."] {
        let r = confine_fs(&ws.join(esc), Access::Write, &ws, &[], |_| false);
        assert!(matches!(r, Err(Denied::Traversal)), "{esc:?} → {r:?}");
    }
}

// ── 2. Symlink escape, including a chain ──────────────────────────────────────

#[cfg(unix)]
#[test]
fn symlink_chain_escape_is_resolved_and_denied() {
    use std::os::unix::fs::symlink;
    let ws = mktmp("ws");
    let outside = mktmp("outside");
    let secret = outside.join("secret.txt");
    std::fs::write(&secret, "x").unwrap();

    // ws/hop1 -> ws/hop2 -> outside/secret.txt  (a two-link chain, both starting
    // lexically inside the workspace).
    let hop2 = ws.join("hop2");
    symlink(&secret, &hop2).unwrap();
    let hop1 = ws.join("hop1");
    symlink(&hop2, &hop1).unwrap();

    let r = confine_fs(&hop1, Access::Read, &ws, &[], |_| false);
    assert!(
        matches!(r, Err(Denied::OutsideReadAllowlist(_))),
        "symlink chain must resolve to the real target and be denied, got {r:?}"
    );
    let w = confine_fs(&hop1, Access::Write, &ws, &[], |_| false);
    assert!(
        matches!(w, Err(Denied::OutsideWorkspace { .. })),
        "symlink-chain write escape must be denied, got {w:?}"
    );
}

#[cfg(unix)]
#[test]
fn symlinked_parent_dir_escape_is_denied() {
    use std::os::unix::fs::symlink;
    // ws/gate -> outside/  ; then a write to ws/gate/loot.txt must not land
    // outside the workspace via the symlinked directory component.
    let ws = mktmp("ws");
    let outside = mktmp("outside");
    let gate = ws.join("gate");
    symlink(&outside, &gate).unwrap();

    let r = confine_fs(&gate.join("loot.txt"), Access::Write, &ws, &[], |_| false);
    assert!(
        matches!(r, Err(Denied::OutsideWorkspace { .. })),
        "write through a symlinked directory must be denied, got {r:?}"
    );
}

// ── 3. Prefix collision must not confuse containment ──────────────────────────

#[test]
fn sibling_with_shared_prefix_is_not_inside_the_workspace() {
    // Create /tmp/.../parent/ws and /tmp/.../parent/ws-evil . A byte-wise
    // starts_with("…/ws") would wrongly accept "…/ws-evil"; the component-wise
    // check must reject it.
    let parent = mktmp("parent");
    let ws = parent.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let evil = parent.join("ws-evil");
    std::fs::create_dir_all(&evil).unwrap();
    let target = evil.join("loot.txt");

    let r = confine_fs(&target, Access::Write, &ws, &[], |_| false);
    assert!(
        matches!(r, Err(Denied::OutsideWorkspace { .. })),
        "a shared-prefix sibling must not count as inside the workspace, got {r:?}"
    );
}

#[test]
fn confine_to_roots_rejects_shared_prefix_sibling() {
    let parent = mktmp("parent");
    let root = parent.join("repo");
    std::fs::create_dir_all(&root).unwrap();
    let evil = parent.join("repo-evil");
    std::fs::create_dir_all(&evil).unwrap();

    assert!(confine_to_roots(&root.join("src"), std::slice::from_ref(&root)).is_ok());
    let r = confine_to_roots(&evil.join("src"), std::slice::from_ref(&root));
    assert!(
        matches!(r, Err(Denied::OutsideRoots(_))),
        "git-root confinement must reject a shared-prefix sibling, got {r:?}"
    );
}

// ── 4. Absolute-path escape ───────────────────────────────────────────────────

#[test]
fn absolute_paths_outside_workspace_denied_for_write() {
    let ws = mktmp("ws");
    for abs in ["/etc/shadow", "/root/.ssh/id_rsa", "/proc/self/environ"] {
        let r = confine_fs(Path::new(abs), Access::Write, &ws, &[], |_| false);
        // Either Unresolvable (path doesn't exist / no perms) or OutsideWorkspace —
        // never Ok. The security property is "not Ok", which we assert directly.
        assert!(r.is_err(), "absolute write to {abs:?} must be denied, got {r:?}");
    }
}

#[test]
fn secret_denylist_beats_an_allowed_read_root() {
    // Even a file inside an allowed read-root is refused if it matches the secret
    // predicate — the denylist is the last word.
    let ws = mktmp("ws");
    let allow = mktmp("allow");
    let key = allow.join("service.api_key");
    std::fs::write(&key, "sk-secret").unwrap();
    let roots = vec![allow.clone()];

    let r = confine_fs(&key, Access::Read, &ws, &roots, |p| {
        p.extension().map(|e| e == "api_key").unwrap_or(false)
    });
    assert!(matches!(r, Err(Denied::Secret(_))), "secret must win over the allowlist, got {r:?}");
}
