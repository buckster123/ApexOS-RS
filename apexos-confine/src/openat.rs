//! `openat2(RESOLVE_BENEATH|RESOLVE_NO_SYMLINKS)` — operate through a
//! pre-opened root fd instead of a checked pathname.
//!
//! `confine_fs` still answers "is this path allowed?" for policy. Actual
//! reads/writes/deletes go through [`Beneath`] so a rename+symlink of an
//! ancestor between the check and the syscall cannot escape the root.

use crate::{has_traversal, Access, Denied};
use std::ffi::CString;
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::{AsRawFd, FromRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::sync::Arc;

const RESOLVE_NO_SYMLINKS: u64 = 0x04;
const RESOLVE_BENEATH: u64 = 0x08;
const SYS_OPENAT2: libc::c_long = 437;

#[repr(C)]
struct OpenHow {
    flags: u64,
    mode: u64,
    resolve: u64,
}

fn c_path(p: &Path) -> io::Result<CString> {
    CString::new(p.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains NUL"))
}

fn openat2(dfd: libc::c_int, path: &Path, flags: i32, mode: u32) -> io::Result<OwnedFd> {
    let c = c_path(path)?;
    let how = OpenHow {
        flags: flags as u64,
        mode: mode as u64,
        resolve: RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS,
    };
    let fd = unsafe {
        libc::syscall(
            SYS_OPENAT2,
            dfd,
            c.as_ptr(),
            &how as *const OpenHow,
            std::mem::size_of::<OpenHow>(),
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(fd as i32) })
}

/// A pre-opened confinement root. All ops stay beneath this directory
/// descriptor; the kernel refuses symlink hops and `..` escapes.
#[derive(Clone, Debug)]
pub struct Beneath {
    fd: Arc<File>,
    display: PathBuf,
}

impl Beneath {
    /// Open `root` as `O_DIRECTORY|O_CLOEXEC`. Follows the last component of
    /// `root` itself (the configured workspace path) so a legitimate bind-mount
    /// still works; everything *under* it is then `RESOLVE_NO_SYMLINKS`.
    pub fn open(root: &Path) -> io::Result<Self> {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_CLOEXEC)
            .open(root)?;
        let display = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
        Ok(Self {
            fd: Arc::new(file),
            display,
        })
    }

    pub fn display(&self) -> &Path {
        &self.display
    }

    fn raw(&self) -> libc::c_int {
        self.fd.as_raw_fd()
    }

    /// Open `rel` (relative to this root). Empty / `.` opens the root itself
    /// via `O_PATH` on `.` — callers that need a real fd for IO should pass
    /// a file component. `O_CLOEXEC` is always added.
    pub fn open_at(&self, rel: &Path, flags: i32, mode: u32) -> io::Result<File> {
        let rel = normalize_rel(rel)?;
        let flags = flags | libc::O_CLOEXEC;
        let owned = openat2(self.raw(), &rel, flags, mode)?;
        Ok(File::from(owned))
    }

    pub fn read(&self, rel: &Path) -> io::Result<Vec<u8>> {
        let mut f = self.open_at(rel, libc::O_RDONLY, 0)?;
        let mut buf = Vec::new();
        f.read_to_end(&mut buf)?;
        Ok(buf)
    }

    pub fn write(&self, rel: &Path, bytes: &[u8], append: bool) -> io::Result<()> {
        if let Some(parent) = rel.parent() {
            if !parent.as_os_str().is_empty() && parent != Path::new(".") {
                self.mkdir_all(parent)?;
            }
        }
        let mut flags = libc::O_WRONLY | libc::O_CREAT;
        if append {
            flags |= libc::O_APPEND;
        } else {
            flags |= libc::O_TRUNC;
        }
        let mut f = self.open_at(rel, flags, 0o644)?;
        f.write_all(bytes)?;
        Ok(())
    }

    pub fn mkdir_all(&self, rel: &Path) -> io::Result<()> {
        let rel = normalize_rel(rel)?;
        if rel == Path::new(".") {
            return Ok(());
        }
        let mut cur = self.raw();
        let mut held: Vec<OwnedFd> = Vec::new();
        for comp in rel.components() {
            let name = Path::new(comp.as_os_str());
            let c = c_path(name)?;
            let rc = unsafe { libc::mkdirat(cur, c.as_ptr(), 0o755) };
            if rc != 0 {
                let err = io::Error::last_os_error();
                if err.raw_os_error() != Some(libc::EEXIST) {
                    return Err(err);
                }
            }
            let next = openat2(cur, name, libc::O_DIRECTORY | libc::O_CLOEXEC, 0)?;
            cur = next.as_raw_fd();
            held.push(next);
        }
        Ok(())
    }

    fn parent_dfd(&self, parent: &Path) -> io::Result<(libc::c_int, Option<OwnedFd>)> {
        if parent == Path::new(".") {
            Ok((self.raw(), None))
        } else {
            let owned = openat2(self.raw(), parent, libc::O_DIRECTORY | libc::O_CLOEXEC, 0)?;
            let fd = owned.as_raw_fd();
            Ok((fd, Some(owned)))
        }
    }

    pub fn mkdir(&self, rel: &Path) -> io::Result<()> {
        let (parent, name) = split_parent(rel)?;
        let (dfd, _hold) = self.parent_dfd(&parent)?;
        let c = c_path(Path::new(&name))?;
        let rc = unsafe { libc::mkdirat(dfd, c.as_ptr(), 0o755) };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    pub fn unlink(&self, rel: &Path, dir: bool) -> io::Result<()> {
        let (parent, name) = split_parent(rel)?;
        let (dfd, _hold) = self.parent_dfd(&parent)?;
        let c = c_path(Path::new(&name))?;
        let flags = if dir { libc::AT_REMOVEDIR } else { 0 };
        let rc = unsafe { libc::unlinkat(dfd, c.as_ptr(), flags) };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    pub fn remove_all(&self, rel: &Path) -> io::Result<()> {
        let rel = normalize_rel(rel)?;
        if rel == Path::new(".") {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "refusing to delete the confinement root",
            ));
        }
        match self.stat(&rel) {
            Ok(st) if st.is_dir && !st.is_symlink => {
                for ent in self.read_dir(&rel)? {
                    if ent.name == "." || ent.name == ".." {
                        continue;
                    }
                    self.remove_all(&rel.join(&ent.name))?;
                }
                self.unlink(&rel, true)
            }
            Ok(_) => self.unlink(&rel, false),
            Err(e) => Err(e),
        }
    }

    pub fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        let (fp, fn_) = split_parent(from)?;
        let (tp, tn) = split_parent(to)?;
        let (fdfd, _fh) = self.parent_dfd(&fp)?;
        let (tdfd, _th) = self.parent_dfd(&tp)?;
        let old = c_path(Path::new(&fn_))?;
        let new = c_path(Path::new(&tn))?;
        let rc = unsafe { libc::renameat(fdfd, old.as_ptr(), tdfd, new.as_ptr()) };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    pub fn stat(&self, rel: &Path) -> io::Result<Stat> {
        let (parent, name) = split_parent(rel)?;
        let (dfd, _hold) = self.parent_dfd(&parent)?;
        let c = c_path(Path::new(&name))?;
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        let rc = unsafe { libc::fstatat(dfd, c.as_ptr(), &mut st, libc::AT_SYMLINK_NOFOLLOW) };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Stat {
            is_dir: (st.st_mode & libc::S_IFMT) == libc::S_IFDIR,
            is_symlink: (st.st_mode & libc::S_IFMT) == libc::S_IFLNK,
            is_file: (st.st_mode & libc::S_IFMT) == libc::S_IFREG,
            len: st.st_size as u64,
            mtime: st.st_mtime as u64,
        })
    }

    pub fn exists(&self, rel: &Path) -> bool {
        self.stat(rel).is_ok()
    }

    pub fn read_dir(&self, rel: &Path) -> io::Result<Vec<DirEnt>> {
        let rel = normalize_rel(rel)?;
        let dir_fd = openat2(self.raw(), &rel, libc::O_DIRECTORY | libc::O_RDONLY | libc::O_CLOEXEC, 0)?;
        // fdopendir takes ownership; dup so Drop of OwnedFd is not a double-close.
        let dup = unsafe { libc::dup(dir_fd.as_raw_fd()) };
        if dup < 0 {
            return Err(io::Error::last_os_error());
        }
        let dirp = unsafe { libc::fdopendir(dup) };
        if dirp.is_null() {
            unsafe { libc::close(dup) };
            return Err(io::Error::last_os_error());
        }
        let mut out = Vec::new();
        loop {
            errno_clear();
            let ent = unsafe { libc::readdir(dirp) };
            if ent.is_null() {
                break;
            }
            let name = unsafe { std::ffi::CStr::from_ptr((*ent).d_name.as_ptr()) };
            let name = match name.to_str() {
                Ok(s) => s.to_string(),
                Err(_) => continue,
            };
            if name == "." || name == ".." {
                continue;
            }
            let dtype = unsafe { (*ent).d_type };
            let mut is_dir = dtype == libc::DT_DIR;
            let mut is_symlink = dtype == libc::DT_LNK;
            let mut is_file = dtype == libc::DT_REG;
            let mut len = 0u64;
            let mut mtime = 0u64;
            if let Ok(st) = self.stat(&rel.join(&name)) {
                is_dir = st.is_dir;
                is_symlink = st.is_symlink;
                is_file = st.is_file;
                len = st.len;
                mtime = st.mtime;
            }
            out.push(DirEnt {
                name,
                is_dir,
                is_symlink,
                is_file,
                len,
                mtime,
            });
        }
        unsafe { libc::closedir(dirp) };
        Ok(out)
    }
}

fn errno_clear() {
    unsafe { *libc::__errno_location() = 0 };
}

#[derive(Debug, Clone)]
pub struct Stat {
    pub is_dir: bool,
    pub is_symlink: bool,
    pub is_file: bool,
    pub len: u64,
    pub mtime: u64,
}

#[derive(Debug, Clone)]
pub struct DirEnt {
    pub name: String,
    pub is_dir: bool,
    pub is_symlink: bool,
    pub is_file: bool,
    pub len: u64,
    pub mtime: u64,
}

/// Strip `root` off `requested` without resolving `requested`. `..` already
/// rejected. The result is what we hand to `openat2`.
pub fn relative_under(root: &Path, requested: &Path) -> Option<PathBuf> {
    if has_traversal(requested) || has_traversal(root) {
        return None;
    }
    match requested.strip_prefix(root) {
        Ok(rel) => {
            if has_traversal(rel) {
                None
            } else {
                Some(rel.to_path_buf())
            }
        }
        Err(_) => None,
    }
}

fn normalize_rel(rel: &Path) -> io::Result<PathBuf> {
    if rel.as_os_str().is_empty() || rel == Path::new(".") {
        return Ok(PathBuf::from("."));
    }
    if rel.is_absolute() || has_traversal(rel) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "relative path must not be absolute or contain ..",
        ));
    }
    Ok(rel.to_path_buf())
}

fn split_parent(rel: &Path) -> io::Result<(PathBuf, std::ffi::OsString)> {
    let rel = normalize_rel(rel)?;
    if rel == Path::new(".") {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "operation on the confinement root itself is not allowed",
        ));
    }
    match (rel.parent(), rel.file_name()) {
        (Some(p), Some(n)) if !p.as_os_str().is_empty() => Ok((p.to_path_buf(), n.to_os_string())),
        (_, Some(n)) => Ok((PathBuf::from("."), n.to_os_string())),
        _ => Err(io::Error::new(io::ErrorKind::InvalidInput, "empty path")),
    }
}

/// Policy + open. Picks workspace (writes) or workspace/read-root (reads),
/// opens that root, and returns `(root, relative)` ready for `Beneath` ops.
/// Secret predicate runs on the *requested* path before any open.
pub fn confine_beneath(
    requested: &Path,
    access: Access,
    workspace: &Path,
    read_roots: &[PathBuf],
    is_secret: impl Fn(&Path) -> bool,
) -> Result<(Beneath, PathBuf), Denied> {
    if has_traversal(requested) {
        return Err(Denied::Traversal);
    }
    if is_secret(requested) {
        return Err(Denied::Secret(requested.to_path_buf()));
    }

    let mut candidates: Vec<PathBuf> = Vec::new();
    candidates.push(workspace.to_path_buf());
    if access == Access::Read {
        for r in read_roots {
            let canon = std::fs::canonicalize(r).unwrap_or_else(|_| r.clone());
            candidates.push(canon);
        }
    }

    let mut last_denied: Option<Denied> = None;
    for root in candidates {
        if let Some(rel) = relative_under(&root, requested) {
            let joined = if rel.as_os_str().is_empty() {
                root.clone()
            } else {
                root.join(&rel)
            };
            if is_secret(&joined) {
                return Err(Denied::Secret(joined));
            }
            match open_root(&root) {
                Ok((b, file_prefix)) => {
                    let full = match (file_prefix.as_os_str().is_empty(), rel.as_os_str().is_empty()) {
                        (true, true) => PathBuf::from("."),
                        (true, false) => rel,
                        (false, true) => file_prefix,
                        (false, false) => file_prefix.join(rel),
                    };
                    return Ok((b, full));
                }
                Err(_) => last_denied = Some(Denied::Unresolvable(root)),
            }
        } else {
            last_denied = Some(match access {
                Access::Write => Denied::OutsideWorkspace {
                    workspace: workspace.to_path_buf(),
                    path: requested.to_path_buf(),
                },
                Access::Read => Denied::OutsideReadAllowlist(requested.to_path_buf()),
            });
        }
    }
    Err(last_denied.unwrap_or(Denied::OutsideReadAllowlist(requested.to_path_buf())))
}

/// Directory roots open as themselves. File roots (e.g. `/proc/cpuinfo`) open
/// their parent; the filename is prepended to the relative path.
fn open_root(root: &Path) -> io::Result<(Beneath, PathBuf)> {
    match Beneath::open(root) {
        Ok(b) => Ok((b, PathBuf::new())),
        Err(e) if e.raw_os_error() == Some(libc::ENOTDIR) => {
            let parent = root.parent().ok_or_else(|| {
                io::Error::new(io::ErrorKind::NotADirectory, "file root has no parent")
            })?;
            let name = root.file_name().ok_or_else(|| {
                io::Error::new(io::ErrorKind::NotADirectory, "file root has no name")
            })?;
            Ok((Beneath::open(parent)?, PathBuf::from(name)))
        }
        Err(e) => Err(e),
    }
}

/// Map an `openat2`/`*at` error onto [`Denied`] so callers stay on one type.
pub fn io_denied(err: io::Error, access: Access, workspace: &Path, path: &Path) -> Denied {
    match err.raw_os_error() {
        Some(libc::ELOOP) | Some(libc::EXDEV) => match access {
            Access::Write => Denied::OutsideWorkspace {
                workspace: workspace.to_path_buf(),
                path: path.to_path_buf(),
            },
            Access::Read => Denied::OutsideReadAllowlist(path.to_path_buf()),
        },
        _ => Denied::Unresolvable(path.to_path_buf()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static N: AtomicUsize = AtomicUsize::new(0);

    fn mktmp(tag: &str) -> PathBuf {
        let n = N.fetch_add(1, Ordering::Relaxed);
        let p = std::env::temp_dir().join(format!("apexos-beneath-{}-{tag}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        std::fs::canonicalize(&p).unwrap()
    }

    #[test]
    fn write_and_read_roundtrip() {
        let ws = mktmp("ws");
        let (root, rel) = confine_beneath(&ws.join("notes/hi.txt"), Access::Write, &ws, &[], |_| false)
            .unwrap();
        root.write(&rel, b"hello", false).unwrap();
        assert_eq!(root.read(&rel).unwrap(), b"hello");
    }

    #[test]
    fn write_outside_denied() {
        let ws = mktmp("ws");
        let out = mktmp("out");
        let r = confine_beneath(&out.join("x"), Access::Write, &ws, &[], |_| false);
        assert!(matches!(r, Err(Denied::OutsideWorkspace { .. })), "{r:?}");
    }

    #[test]
    fn symlink_file_is_refused() {
        let ws = mktmp("ws");
        let outside = mktmp("out");
        std::fs::write(outside.join("secret"), b"x").unwrap();
        symlink(outside.join("secret"), ws.join("link")).unwrap();
        let (root, rel) = confine_beneath(&ws.join("link"), Access::Read, &ws, &[], |_| false).unwrap();
        let err = root.read(&rel).unwrap_err();
        assert_eq!(err.raw_os_error(), Some(libc::ELOOP));
    }

    #[test]
    fn toctou_renamed_ancestor_is_refused() {
        // The finding 8 attack: after a check, swap an ancestor for a symlink
        // at /etc. openat2 must not follow it.
        let ws = mktmp("ws");
        let sub = ws.join("sub");
        std::fs::create_dir(&sub).unwrap();
        std::fs::write(sub.join("file"), b"in").unwrap();
        let (root, rel) =
            confine_beneath(&ws.join("sub/file"), Access::Write, &ws, &[], |_| false).unwrap();

        std::fs::rename(&sub, ws.join("sub.bak")).unwrap();
        symlink("/etc", ws.join("sub")).unwrap();

        let err = root.read(&rel).unwrap_err();
        assert!(
            err.raw_os_error() == Some(libc::ELOOP) || err.raw_os_error() == Some(libc::EXDEV),
            "got {err:?}"
        );
        // And a write must not create /etc/file.
        assert!(root.write(&rel, b"pwn", false).is_err());
        assert!(!Path::new("/etc/file").exists() || std::fs::read("/etc/file").ok() != Some(b"pwn".to_vec()));
    }

    #[test]
    fn mkdir_unlink_and_list() {
        let ws = mktmp("ws");
        let root = Beneath::open(&ws).unwrap();
        root.mkdir_all(Path::new("a/b")).unwrap();
        root.write(Path::new("a/b/c.txt"), b"z", false).unwrap();
        let ents = root.read_dir(Path::new("a/b")).unwrap();
        assert!(ents.iter().any(|e| e.name == "c.txt" && e.is_file));
        root.remove_all(Path::new("a")).unwrap();
        assert!(!root.exists(Path::new("a")));
    }

    #[test]
    fn relative_under_rejects_dotdot_and_sibling() {
        let ws = Path::new("/tmp/ws");
        assert!(relative_under(ws, Path::new("/tmp/ws/a")).is_some());
        assert!(relative_under(ws, Path::new("/tmp/ws-evil/a")).is_none());
        assert!(relative_under(ws, Path::new("/tmp/ws/../etc")).is_none());
    }
}
