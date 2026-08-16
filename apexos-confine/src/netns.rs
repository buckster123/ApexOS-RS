//! Unprivileged network isolation for the fs/shell tools worker (finding 11).
//!
//! `CLONE_NEWNET` needs `CAP_SYS_ADMIN` *or* a user namespace. agentd is
//! `NoNewPrivileges` and cannot setuid, so we enter `CLONE_NEWUSER|CLONE_NEWNET`
//! and map our own uid/gid 1:1. Host uid stays `agentd` (Landlock still owns
//! the FS residual); the new netns has no interfaces, so `run_command curl`
//! cannot phone home. The net-class worker does not call this.
//!
//! `APEXOS_NETNS=0` skips. A kernel with unprivileged userns off logs
//! unsupported and continues.

use std::io;

/// Outcome of a netns attempt. Never panics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NetnsStatus {
    Isolated,
    Disabled,
    Unsupported,
    Error(String),
}

fn netns_disabled() -> bool {
    match std::env::var("APEXOS_NETNS") {
        Ok(v) => matches!(
            v.to_ascii_lowercase().as_str(),
            "0" | "false" | "off" | "no"
        ),
        Err(_) => false,
    }
}

/// Drop this process (and every child) into an empty network namespace.
pub fn isolate_network() -> NetnsStatus {
    if netns_disabled() {
        return NetnsStatus::Disabled;
    }
    #[cfg(not(target_os = "linux"))]
    {
        NetnsStatus::Unsupported
    }
    #[cfg(target_os = "linux")]
    {
        linux::isolate()
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use super::*;

    pub(super) fn isolate() -> NetnsStatus {
        // Probe in a child first. A failed uid_map after unshare leaves the
        // process as overflowuid — we must not do that to the tools worker.
        // Ubuntu's apparmor_restrict_unprivileged_userns=1 is the usual miss
        // (Debian/Pi kiosk is 0). Call from a single-threaded main().
        match fork_probe() {
            NetnsStatus::Isolated => enter(),
            other => other,
        }
    }

    fn fork_probe() -> NetnsStatus {
        let pid = unsafe { libc::fork() };
        if pid < 0 {
            return NetnsStatus::Error(format!("fork: {}", io::Error::last_os_error()));
        }
        if pid == 0 {
            let code = match enter() {
                NetnsStatus::Isolated => 0,
                NetnsStatus::Disabled => 1,
                NetnsStatus::Unsupported => 2,
                NetnsStatus::Error(_) => 3,
            };
            unsafe { libc::_exit(code) };
        }
        let mut status = 0;
        if unsafe { libc::waitpid(pid, &mut status, 0) } < 0 {
            return NetnsStatus::Error(format!("waitpid: {}", io::Error::last_os_error()));
        }
        if !libc::WIFEXITED(status) {
            return NetnsStatus::Error("netns probe did not exit".into());
        }
        match libc::WEXITSTATUS(status) {
            0 => NetnsStatus::Isolated,
            2 | 3 => NetnsStatus::Unsupported,
            other => NetnsStatus::Error(format!("netns probe exit {other}")),
        }
    }

    fn enter() -> NetnsStatus {
        // Capture BEFORE unshare — afterwards getuid() is overflowuid until mapped.
        let uid = unsafe { libc::getuid() };
        let gid = unsafe { libc::getgid() };
        let rc = unsafe { libc::unshare(libc::CLONE_NEWUSER | libc::CLONE_NEWNET) };
        if rc != 0 {
            let e = io::Error::last_os_error();
            let raw = e.raw_os_error();
            if matches!(
                raw,
                Some(libc::EPERM) | Some(libc::ENOSYS) | Some(libc::EOPNOTSUPP)
            ) {
                return NetnsStatus::Unsupported;
            }
            return NetnsStatus::Error(format!("unshare: {e}"));
        }
        // setgroups=deny must precede gid_map for an unprivileged writer.
        let _ = std::fs::write("/proc/self/setgroups", b"deny\n");
        if std::fs::write("/proc/self/uid_map", format!("{uid} {uid} 1\n")).is_err() {
            // AppArmor userns restriction (Ubuntu) or a nested userns we
            // cannot extend. The caller must not keep this process.
            return NetnsStatus::Unsupported;
        }
        let _ = std::fs::write("/proc/self/gid_map", format!("{gid} {gid} 1\n"));
        NetnsStatus::Isolated
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn disabled_env_is_recognized() {
        let _g = ENV.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("APEXOS_NETNS", "0");
        assert_eq!(super::isolate_network(), super::NetnsStatus::Disabled);
        std::env::set_var("APEXOS_NETNS", "off");
        assert_eq!(super::isolate_network(), super::NetnsStatus::Disabled);
        std::env::remove_var("APEXOS_NETNS");
    }

    static ENV: std::sync::Mutex<()> = std::sync::Mutex::new(());
}
