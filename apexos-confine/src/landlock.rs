//! Landlock LSM allowlist for the tools worker (finding 11 part 2).
//!
//! `apexos-tools` shares the `agentd` uid, so DAC still lets an approved
//! `run_command` open `/var/lib/agentd/.api_key`. agentd cannot setuid a
//! different user (`NoNewPrivileges`). Landlock is the kernel FS gate that
//! survives `run_command` children: only listed roots stay reachable.
//!
//! This module is Linux-only and std + libc. `restrict_self` is a one-way
//! drop — call it from `apexos-tools` `main` before any threads.
//! `APEXOS_LANDLOCK=0` skips (dev/test). Missing paths are skipped. A kernel
//! without Landlock logs unsupported and continues (Pi / Debian trixie have it).

use std::io;
use std::path::{Path, PathBuf};

/// Outcome of a restrict attempt. Never panics — the caller logs and decides.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LandlockStatus {
    Restricted { abi: i32 },
    Disabled,
    Unsupported,
    Error(String),
}

/// Allowlisted roots. RW vs RO is the only distinction the kernel sees here.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FsRules {
    pub rw: Vec<PathBuf>,
    pub ro: Vec<PathBuf>,
}

/// Paths the tools worker must never be granted, even if an operator stuffed
/// them into `AGENTD_READ_ROOTS` / `AGENTD_WORKSPACE`. Landlock is an
/// allowlist — a parent grant (`/var/lib/agentd`, `/etc/agentd`, `/proc`,
/// `/etc`) would re-open the residual this slice closes.
pub fn is_forbidden_landlock_root(path: &Path) -> bool {
    let s = path.to_string_lossy();
    let s = s.trim_end_matches('/');
    s == "/var/lib/agentd"
        || s == "/etc/agentd"
        || s == "/etc"
        || s == "/proc"
        || s == "/root"
        || s == "/home"
        || s.starts_with("/etc/agentd/env")
        || s.starts_with("/etc/agentd/ui.env")
        || s == "/etc/agentd/peers.toml"
        || s == "/etc/agentd/identities.toml"
        || s == "/etc/agentd/apexnet.psk"
        || s == "/etc/shadow"
        || s == "/etc/gshadow"
        || s.ends_with(".api_key")
        || s.ends_with(".oai_api_key")
        || s.ends_with(".xai_api_key")
        || s.ends_with(".openrouter_api_key")
}

const SYSTEM_RO: &[&str] = &[
    "/usr",
    "/bin",
    "/sbin",
    "/lib",
    "/lib64",
    "/lib32",
    "/etc/ssl",
    "/etc/ca-certificates",
    "/etc/pki",
    "/etc/alternatives",
    "/etc/hosts",
    "/etc/resolv.conf",
    "/etc/nsswitch.conf",
    "/etc/passwd",
    "/etc/group",
    "/etc/ld.so.cache",
    "/etc/ld.so.conf",
    "/etc/ld.so.conf.d",
    "/etc/protocols",
    "/etc/services",
    "/etc/localtime",
    "/etc/timezone",
    "/etc/alsa",
    "/usr/share",
    "/proc/cpuinfo",
    "/proc/meminfo",
    "/proc/mounts",
    "/proc/uptime",
    "/proc/loadavg",
    "/proc/device-tree",
    "/sys",
];

const SYSTEM_RW: &[&str] = &["/tmp", "/var/tmp", "/dev"];

fn push_admissible(out: &mut Vec<PathBuf>, path: PathBuf) {
    if is_forbidden_landlock_root(&path) {
        return;
    }
    if !out.iter().any(|p| p == &path) {
        out.push(path);
    }
}

/// Build the tools-worker allowlist. Pure — does not talk to the kernel.
/// Missing system paths stay in the list; `restrict` skips those at apply.
pub fn tools_worker_rules(
    workspace: &Path,
    read_roots: &[PathBuf],
    git_roots: &[PathBuf],
    extra_rw: &[PathBuf],
) -> FsRules {
    let mut rw = Vec::new();
    let mut ro = Vec::new();

    push_admissible(&mut rw, workspace.to_path_buf());
    for p in git_roots {
        push_admissible(&mut rw, p.clone());
    }
    for p in extra_rw {
        push_admissible(&mut rw, p.clone());
    }
    for p in SYSTEM_RW {
        push_admissible(&mut rw, PathBuf::from(p));
    }

    for p in read_roots {
        push_admissible(&mut ro, p.clone());
    }
    for p in SYSTEM_RO {
        push_admissible(&mut ro, PathBuf::from(p));
    }

    FsRules { rw, ro }
}

fn env_paths(key: &str) -> Vec<PathBuf> {
    match std::env::var(key) {
        Ok(v) => v
            .split(':')
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .collect(),
        Err(_) => Vec::new(),
    }
}

fn landlock_disabled() -> bool {
    match std::env::var("APEXOS_LANDLOCK") {
        Ok(v) => matches!(
            v.to_ascii_lowercase().as_str(),
            "0" | "false" | "off" | "no"
        ),
        Err(_) => false,
    }
}

/// Ensure the notify JSONL exists as a *file* so Landlock can grant that
/// inode without granting `/var/lib/agentd` (which would re-open `.api_key`).
fn ensure_notifications_file() -> Option<PathBuf> {
    let path = PathBuf::from("/var/lib/agentd/notifications.jsonl");
    if path.is_file() {
        return Some(path);
    }
    if path.parent().map(|p| p.is_dir()).unwrap_or(false) {
        if std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .is_ok()
        {
            return Some(path);
        }
    }
    None
}

/// Read the live tools env and apply the Landlock ruleset.
pub fn restrict_tools_worker() -> LandlockStatus {
    if landlock_disabled() {
        return LandlockStatus::Disabled;
    }
    let workspace = PathBuf::from(
        std::env::var("AGENTD_WORKSPACE")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "/var/lib/agentd/workspace".into()),
    );
    let mut extra_rw = Vec::new();
    if let Ok(p) = std::env::var("AGENTD_USB_EJECT_DIR") {
        if !p.is_empty() {
            extra_rw.push(PathBuf::from(p));
        }
    }
    if let Ok(p) = std::env::var("AGENTD_USB_PREP_DIR") {
        if !p.is_empty() {
            extra_rw.push(PathBuf::from(p));
        }
    }
    if extra_rw.is_empty() {
        extra_rw.push(PathBuf::from("/var/lib/agentd/usb-eject"));
        extra_rw.push(PathBuf::from("/var/lib/agentd/usb-prep"));
    }
    if let Some(n) = ensure_notifications_file() {
        extra_rw.push(n);
    }
    let mut read_roots = env_paths("AGENTD_READ_ROOTS");
    read_roots.push(PathBuf::from("/etc/agentd/parts"));
    read_roots.push(PathBuf::from("/var/lib/agentd/update"));
    let agents = match std::env::var("AGENTD_LOG") {
        Ok(log) if !log.is_empty() => PathBuf::from(log).join("agents"),
        _ => PathBuf::from("/var/lib/agentd/events/agents"),
    };
    read_roots.push(agents);
    let rules = tools_worker_rules(
        &workspace,
        &read_roots,
        &env_paths("AGENTD_GIT_ROOTS"),
        &extra_rw,
    );
    restrict(&rules)
}

/// Apply `rules` to the current process. No-op on non-Linux.
pub fn restrict(rules: &FsRules) -> LandlockStatus {
    #[cfg(not(target_os = "linux"))]
    {
        let _ = rules;
        LandlockStatus::Unsupported
    }
    #[cfg(target_os = "linux")]
    {
        linux::restrict(rules)
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use super::*;
    use std::fs::OpenOptions;
    use std::os::unix::fs::OpenOptionsExt;
    use std::os::unix::io::AsRawFd;

    const ACCESS_FS_EXECUTE: u64 = 1 << 0;
    const ACCESS_FS_WRITE_FILE: u64 = 1 << 1;
    const ACCESS_FS_READ_FILE: u64 = 1 << 2;
    const ACCESS_FS_READ_DIR: u64 = 1 << 3;
    const ACCESS_FS_REMOVE_DIR: u64 = 1 << 4;
    const ACCESS_FS_REMOVE_FILE: u64 = 1 << 5;
    const ACCESS_FS_MAKE_CHAR: u64 = 1 << 6;
    const ACCESS_FS_MAKE_DIR: u64 = 1 << 7;
    const ACCESS_FS_MAKE_REG: u64 = 1 << 8;
    const ACCESS_FS_MAKE_SOCK: u64 = 1 << 9;
    const ACCESS_FS_MAKE_FIFO: u64 = 1 << 10;
    const ACCESS_FS_MAKE_BLOCK: u64 = 1 << 11;
    const ACCESS_FS_MAKE_SYM: u64 = 1 << 12;
    const ACCESS_FS_REFER: u64 = 1 << 13;
    const ACCESS_FS_TRUNCATE: u64 = 1 << 14;

    const ABI1_FS: u64 = ACCESS_FS_EXECUTE
        | ACCESS_FS_WRITE_FILE
        | ACCESS_FS_READ_FILE
        | ACCESS_FS_READ_DIR
        | ACCESS_FS_REMOVE_DIR
        | ACCESS_FS_REMOVE_FILE
        | ACCESS_FS_MAKE_CHAR
        | ACCESS_FS_MAKE_DIR
        | ACCESS_FS_MAKE_REG
        | ACCESS_FS_MAKE_SOCK
        | ACCESS_FS_MAKE_FIFO
        | ACCESS_FS_MAKE_BLOCK
        | ACCESS_FS_MAKE_SYM;

    const RULE_PATH_BENEATH: i32 = 1;
    const CREATE_VERSION: u32 = 1;
    const PR_SET_NO_NEW_PRIVS: i32 = 38;

    #[repr(C)]
    struct RulesetAttr {
        handled_access_fs: u64,
    }

    #[repr(C)]
    struct PathBeneathAttr {
        allowed_access: u64,
        parent_fd: i32,
    }

    fn create_ruleset(attr: *const RulesetAttr, size: usize, flags: u32) -> io::Result<i32> {
        let rc = unsafe {
            libc::syscall(
                libc::SYS_landlock_create_ruleset,
                attr,
                size,
                flags as libc::c_ulong,
            )
        };
        if rc < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(rc as i32)
        }
    }

    fn add_rule(fd: i32, attr: &PathBeneathAttr) -> io::Result<()> {
        let rc = unsafe {
            libc::syscall(
                libc::SYS_landlock_add_rule,
                fd,
                RULE_PATH_BENEATH,
                attr as *const PathBeneathAttr,
                0u32,
            )
        };
        if rc < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    fn restrict_self(fd: i32) -> io::Result<()> {
        let rc = unsafe { libc::syscall(libc::SYS_landlock_restrict_self, fd, 0u32) };
        if rc < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    fn probe_abi() -> io::Result<i32> {
        create_ruleset(std::ptr::null(), 0, CREATE_VERSION)
    }

    fn handled_fs(abi: i32) -> u64 {
        let mut bits = ABI1_FS;
        if abi >= 2 {
            bits |= ACCESS_FS_REFER;
        }
        if abi >= 3 {
            bits |= ACCESS_FS_TRUNCATE;
        }
        bits
    }

    fn ro_access(handled: u64) -> u64 {
        handled & (ACCESS_FS_EXECUTE | ACCESS_FS_READ_FILE | ACCESS_FS_READ_DIR)
    }

    fn rw_access(handled: u64) -> u64 {
        // Device nodes stay under /dev; workspace does not need mkdev.
        handled & !ACCESS_FS_MAKE_CHAR & !ACCESS_FS_MAKE_BLOCK
    }

    fn add_path(ruleset: i32, path: &Path, access: u64) -> io::Result<()> {
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_PATH | libc::O_CLOEXEC)
            .open(path)?;
        let attr = PathBeneathAttr {
            allowed_access: access,
            parent_fd: file.as_raw_fd(),
        };
        add_rule(ruleset, &attr)
    }

    pub(super) fn restrict(rules: &FsRules) -> LandlockStatus {
        let abi = match probe_abi() {
            Ok(n) if n >= 1 => n,
            Ok(_) => return LandlockStatus::Unsupported,
            Err(e) => {
                let raw = e.raw_os_error();
                if matches!(raw, Some(libc::ENOSYS) | Some(libc::EOPNOTSUPP)) {
                    return LandlockStatus::Unsupported;
                }
                return LandlockStatus::Error(format!("probe: {e}"));
            }
        };

        let handled = handled_fs(abi);
        let attr = RulesetAttr {
            handled_access_fs: handled,
        };
        let ruleset = match create_ruleset(
            &attr as *const RulesetAttr,
            std::mem::size_of::<RulesetAttr>(),
            0,
        ) {
            Ok(fd) => fd,
            Err(e) => return LandlockStatus::Error(format!("create: {e}")),
        };

        let ro = ro_access(handled);
        let rw = rw_access(handled);
        for p in &rules.ro {
            // ENOENT / EACCES: skip. Other errors are still skip — a missing
            // optional root must not fail the whole jail closed (Nano / odd FS).
            let _ = add_path(ruleset, p, ro);
        }
        for p in &rules.rw {
            let _ = add_path(ruleset, p, rw);
        }

        let nnp = unsafe { libc::prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) };
        if nnp != 0 {
            let e = io::Error::last_os_error();
            unsafe { libc::close(ruleset) };
            return LandlockStatus::Error(format!("no_new_privs: {e}"));
        }

        let applied = restrict_self(ruleset);
        unsafe { libc::close(ruleset) };
        match applied {
            Ok(()) => LandlockStatus::Restricted { abi },
            Err(e) => LandlockStatus::Error(format!("restrict: {e}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forbidden_roots_cover_the_same_uid_residual() {
        assert!(is_forbidden_landlock_root(Path::new("/var/lib/agentd")));
        assert!(is_forbidden_landlock_root(Path::new("/var/lib/agentd/")));
        assert!(is_forbidden_landlock_root(Path::new(
            "/var/lib/agentd/.api_key"
        )));
        assert!(is_forbidden_landlock_root(Path::new(
            "/var/lib/agentd/.oai_api_key"
        )));
        assert!(is_forbidden_landlock_root(Path::new("/etc/agentd")));
        assert!(is_forbidden_landlock_root(Path::new("/etc/agentd/env")));
        assert!(is_forbidden_landlock_root(Path::new("/etc/agentd/ui.env")));
        assert!(is_forbidden_landlock_root(Path::new(
            "/etc/agentd/peers.toml"
        )));
        assert!(is_forbidden_landlock_root(Path::new("/etc")));
        assert!(is_forbidden_landlock_root(Path::new("/proc")));
        assert!(!is_forbidden_landlock_root(Path::new(
            "/var/lib/agentd/workspace"
        )));
        assert!(!is_forbidden_landlock_root(Path::new("/etc/agentd/parts")));
        assert!(!is_forbidden_landlock_root(Path::new("/usr")));
    }

    #[test]
    fn tools_rules_never_grant_the_agentd_secret_tree() {
        let rules = tools_worker_rules(
            Path::new("/var/lib/agentd/workspace"),
            &[
                PathBuf::from("/etc/agentd"),
                PathBuf::from("/etc/agentd/parts"),
                PathBuf::from("/var/lib/agentd"),
                PathBuf::from("/var/lib/agentd/.api_key"),
                PathBuf::from("/proc"),
            ],
            &[PathBuf::from("/opt/ApexOS-RS")],
            &[
                PathBuf::from("/var/lib/agentd/usb-eject"),
                PathBuf::from("/var/lib/agentd/notifications.jsonl"),
            ],
        );
        for p in rules.rw.iter().chain(rules.ro.iter()) {
            assert!(
                !is_forbidden_landlock_root(p),
                "planner leaked forbidden root {}",
                p.display()
            );
        }
        assert!(rules
            .rw
            .iter()
            .any(|p| p == Path::new("/var/lib/agentd/workspace")));
        assert!(rules
            .rw
            .iter()
            .any(|p| p == Path::new("/var/lib/agentd/usb-eject")));
        assert!(rules.rw.iter().any(|p| p == Path::new("/opt/ApexOS-RS")));
        assert!(rules.ro.iter().any(|p| p == Path::new("/etc/agentd/parts")));
        assert!(rules.ro.iter().any(|p| p == Path::new("/usr")));
        assert!(!rules.rw.iter().any(|p| p == Path::new("/var/lib/agentd")));
        assert!(!rules.ro.iter().any(|p| p == Path::new("/var/lib/agentd")));
        assert!(!rules.ro.iter().any(|p| p == Path::new("/etc/agentd")));
        assert!(!rules.ro.iter().any(|p| p == Path::new("/proc")));
    }

    #[test]
    fn disabled_env_is_recognized() {
        // restrict_tools_worker honors the kill switch; we only assert the
        // parser here via a scoped env set (tests share a process).
        let _g = ENV.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("APEXOS_LANDLOCK", "0");
        assert_eq!(restrict_tools_worker(), LandlockStatus::Disabled);
        std::env::set_var("APEXOS_LANDLOCK", "off");
        assert_eq!(restrict_tools_worker(), LandlockStatus::Disabled);
        std::env::remove_var("APEXOS_LANDLOCK");
    }

    static ENV: std::sync::Mutex<()> = std::sync::Mutex::new(());
}
