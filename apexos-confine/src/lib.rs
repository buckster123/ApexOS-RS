//! Path-confinement primitives — the FS-sandbox algorithm, factored out of
//! `apexos-tools` so it can be reasoned about, **tested in isolation**, and lifted
//! whole. Depends only on `std`; it knows nothing about ApexOS.
//!
//! The caller supplies the *policy* (which roots count as the workspace, which extra
//! roots reads may reach, which paths are always-secret); this crate supplies the
//! *mechanism*:
//!
//! - reject `..` up front, component-based (so `foo..bar` is fine, `../x` is not);
//! - canonicalize the request — resolving symlinks, tolerating a non-existent final
//!   component (a write target that doesn't exist yet) — so a symlink *inside* the
//!   workspace that points *outside* can't smuggle an escape past a `starts_with`
//!   check (closes the classic TOCTOU);
//! - then a containment decision: writes confine to the workspace; reads also accept
//!   a read-allowlist, minus an always-blocked secret set.
//!
//! Structured [`Denied`] reasons let the caller render its own messages while this
//! crate stays generic. See `PATTERNS.md` in the ApexOS-RS repo.

use std::path::{Component, Path, PathBuf};

/// What kind of access is being confined.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    /// Read / list — may also reach the read-allowlist (minus secrets).
    Read,
    /// Write / create / delete — workspace only, hard.
    Write,
}

/// Why a path was refused. The caller maps these to its own user-facing strings.
#[derive(Debug)]
pub enum Denied {
    /// The request contained a `..` component.
    Traversal,
    /// The path (and every ancestor) could not be canonicalized.
    Unresolvable(PathBuf),
    /// A write/delete resolved outside the workspace.
    OutsideWorkspace { workspace: PathBuf, path: PathBuf },
    /// A read hit the always-blocked secret set.
    Secret(PathBuf),
    /// A read resolved outside the workspace and the read-allowlist.
    OutsideReadAllowlist(PathBuf),
    /// A flat-roots ([`confine_to_roots`]) request resolved outside every root.
    OutsideRoots(PathBuf),
}

/// True if any component is `..`. Component-based, so `foo..bar` (no parent-dir
/// component) is **not** a traversal, while `a/../b` is. Callers reject before
/// canonicalizing — [`canonicalize_lenient`] does not normalize parent components,
/// so a `..` in a non-existent suffix would otherwise survive and defeat
/// `starts_with`.
pub fn has_traversal(path: &Path) -> bool {
    path.components().any(|c| c == Component::ParentDir)
}

/// Canonicalize `path`, tolerating a non-existent final component (write targets
/// that don't exist yet): canonicalize the deepest existing ancestor and re-append
/// the remainder, so symlinks in the existing prefix are resolved. Callers MUST
/// reject `..` first — this does not normalize parent components.
pub fn canonicalize_lenient(path: &Path) -> Option<PathBuf> {
    if let Ok(c) = std::fs::canonicalize(path) {
        return Some(c);
    }
    let mut suffix: Vec<std::ffi::OsString> = Vec::new();
    let mut cur = path;
    while let Some(parent) = cur.parent() {
        if let Some(name) = cur.file_name() {
            suffix.push(name.to_owned());
        }
        if let Ok(mut c) = std::fs::canonicalize(parent) {
            for comp in suffix.iter().rev() {
                c.push(comp);
            }
            return Some(c);
        }
        cur = parent;
    }
    None
}

/// Confine `requested` for `access`. Writes confine to `workspace`; reads also accept
/// `read_roots` (each canonicalized) minus anything `is_secret` flags. `requested`
/// should already be workspace-rooted by the caller. Returns the canonical path to
/// operate on (never the raw request — operate on *this*, not the input).
pub fn confine_fs(
    requested: &Path,
    access: Access,
    workspace: &Path,
    read_roots: &[PathBuf],
    is_secret: impl Fn(&Path) -> bool,
) -> Result<PathBuf, Denied> {
    if has_traversal(requested) {
        return Err(Denied::Traversal);
    }
    let canon =
        canonicalize_lenient(requested).ok_or_else(|| Denied::Unresolvable(requested.to_path_buf()))?;

    if canon.starts_with(workspace) {
        return Ok(canon);
    }
    match access {
        Access::Write => Err(Denied::OutsideWorkspace {
            workspace: workspace.to_path_buf(),
            path: canon,
        }),
        Access::Read => {
            if is_secret(&canon) {
                return Err(Denied::Secret(canon));
            }
            for root in read_roots {
                let root_canon = std::fs::canonicalize(root).unwrap_or_else(|_| root.clone());
                if canon.starts_with(&root_canon) {
                    return Ok(canon);
                }
            }
            Err(Denied::OutsideReadAllowlist(canon))
        }
    }
}

/// Confine `requested` to within any of `roots` (a flat allowlist — e.g. git roots).
/// Rejects `..`, canonicalizes (lenient). `roots` are matched as-is (canonicalize
/// them at construction if you need symlink-stable roots).
pub fn confine_to_roots(requested: &Path, roots: &[PathBuf]) -> Result<PathBuf, Denied> {
    if has_traversal(requested) {
        return Err(Denied::Traversal);
    }
    let canon =
        canonicalize_lenient(requested).ok_or_else(|| Denied::Unresolvable(requested.to_path_buf()))?;
    for root in roots {
        if canon.starts_with(root) {
            return Ok(canon);
        }
    }
    Err(Denied::OutsideRoots(canon))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static N: AtomicUsize = AtomicUsize::new(0);

    /// A unique, freshly-created temp dir (std-only — no tempfile dep).
    fn mktmp(tag: &str) -> PathBuf {
        let n = N.fetch_add(1, Ordering::Relaxed);
        let p = std::env::temp_dir().join(format!("apexos-confine-{}-{tag}-{n}", std::process::id()));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).unwrap();
        fs::canonicalize(&p).unwrap()
    }

    #[test]
    fn traversal_is_component_based() {
        assert!(has_traversal(Path::new("a/../b")));
        assert!(has_traversal(Path::new("../x")));
        assert!(!has_traversal(Path::new("a/b/c")));
        assert!(!has_traversal(Path::new("foo..bar/baz"))); // not a parent-dir component
    }

    #[test]
    fn write_inside_workspace_ok_outside_denied() {
        let ws = mktmp("ws");
        let inside = ws.join("notes.txt");
        // non-existent write target inside the ws → lenient canon → Ok
        let got = confine_fs(&inside, Access::Write, &ws, &[], |_| false).unwrap();
        assert!(got.starts_with(&ws));

        let outside = mktmp("outside");
        let r = confine_fs(&outside.join("x"), Access::Write, &ws, &[], |_| false);
        assert!(matches!(r, Err(Denied::OutsideWorkspace { .. })), "got {r:?}");
    }

    #[test]
    fn write_traversal_denied() {
        let ws = mktmp("ws");
        let r = confine_fs(&ws.join("../escape"), Access::Write, &ws, &[], |_| false);
        assert!(matches!(r, Err(Denied::Traversal)), "got {r:?}");
    }

    #[test]
    fn read_allowlist_and_secret_denylist() {
        let ws = mktmp("ws");
        let allow = mktmp("allow");
        let f = allow.join("ok.txt");
        fs::write(&f, "x").unwrap();
        let roots = vec![allow.clone()];

        // read inside a read-root → Ok
        assert!(confine_fs(&f, Access::Read, &ws, &roots, |_| false).is_ok());
        // read outside ws + roots → OutsideReadAllowlist
        let other = mktmp("other");
        let g = other.join("nope.txt");
        fs::write(&g, "x").unwrap();
        assert!(matches!(
            confine_fs(&g, Access::Read, &ws, &roots, |_| false),
            Err(Denied::OutsideReadAllowlist(_))
        ));
        // secret wins even inside an allowed read-root
        assert!(matches!(
            confine_fs(&f, Access::Read, &ws, &roots, |p| p.ends_with("ok.txt")),
            Err(Denied::Secret(_))
        ));
        // writes never consult the read-allowlist
        assert!(matches!(
            confine_fs(&f, Access::Write, &ws, &roots, |_| false),
            Err(Denied::OutsideWorkspace { .. })
        ));
    }

    /// The crown-jewel security property: a symlink *inside* the workspace pointing
    /// *outside* is resolved by canonicalization and refused — no escape.
    #[cfg(unix)]
    #[test]
    fn symlink_escape_is_resolved_and_denied() {
        let ws = mktmp("ws");
        let outside = mktmp("outside");
        let secret = outside.join("secret.txt");
        fs::write(&secret, "x").unwrap();
        let link = ws.join("link");
        std::os::unix::fs::symlink(&secret, &link).unwrap();

        // Even though `link` is lexically inside the workspace, canonicalization
        // resolves it to the outside target → denied.
        let r = confine_fs(&link, Access::Read, &ws, &[], |_| false);
        assert!(matches!(r, Err(Denied::OutsideReadAllowlist(_))), "symlink escape must be denied, got {r:?}");
        let w = confine_fs(&link, Access::Write, &ws, &[], |_| false);
        assert!(matches!(w, Err(Denied::OutsideWorkspace { .. })), "symlink write-escape must be denied, got {w:?}");
    }

    #[test]
    fn confine_to_roots_basics() {
        let a = mktmp("root-a");
        let b = mktmp("root-b");
        let roots = vec![a.clone(), b.clone()];
        assert!(confine_to_roots(&a.join("repo"), &roots).is_ok());
        assert!(confine_to_roots(&b.join("x/y"), &roots).is_ok());
        let outside = mktmp("outside");
        assert!(matches!(
            confine_to_roots(&outside.join("x"), &roots),
            Err(Denied::OutsideRoots(_))
        ));
        assert!(matches!(
            confine_to_roots(&a.join("../x"), &roots),
            Err(Denied::Traversal)
        ));
    }
}
