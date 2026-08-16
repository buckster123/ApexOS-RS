//! Process-local Landlock apply (harness = false so we stay single-threaded).
//!
//! Proves the kernel ruleset denies a sibling secret while the workspace
//! file stays readable — the same-uid residual (`cat /var/lib/agentd/.api_key`).

use apexos_confine::{restrict, FsRules, LandlockStatus};
use std::process::ExitCode;

fn main() -> ExitCode {
    if cfg!(not(target_os = "linux")) {
        eprintln!("skip: not linux");
        return ExitCode::SUCCESS;
    }
    let pid = std::process::id();
    let root = std::env::temp_dir().join(format!("apexos-ll-{pid}"));
    let _ = std::fs::remove_dir_all(&root);
    let ws = root.join("workspace");
    let secret_dir = root.join("daemon");
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::create_dir_all(&secret_dir).unwrap();
    let allowed = ws.join("note.txt");
    let secret = secret_dir.join(".api_key");
    std::fs::write(&allowed, b"ok").unwrap();
    std::fs::write(&secret, b"sk-secret").unwrap();

    // Minimal ruleset: only the workspace. The full tools_worker_rules also
    // grants /tmp (run_command scratch), which would hide this sibling.
    let rules = FsRules {
        rw: vec![ws.clone()],
        ro: vec![],
    };
    match restrict(&rules) {
        LandlockStatus::Restricted { abi } => eprintln!("restricted abi={abi}"),
        LandlockStatus::Unsupported => {
            eprintln!("skip: landlock unsupported");
            let _ = std::fs::remove_dir_all(&root);
            return ExitCode::SUCCESS;
        }
        LandlockStatus::Disabled => {
            eprintln!("skip: disabled");
            let _ = std::fs::remove_dir_all(&root);
            return ExitCode::SUCCESS;
        }
        LandlockStatus::Error(e) => {
            eprintln!("restrict error: {e}");
            let _ = std::fs::remove_dir_all(&root);
            return ExitCode::from(2);
        }
    }

    let note = std::fs::read_to_string(&allowed);
    let stolen = std::fs::read_to_string(&secret);
    let _ = std::fs::remove_dir_all(&root);

    match note {
        Ok(s) if s == "ok" => {}
        other => {
            eprintln!("workspace read failed: {other:?}");
            return ExitCode::from(3);
        }
    }
    match stolen {
        Ok(s) => {
            eprintln!("SECRET STILL READABLE: {s:?}");
            return ExitCode::from(4);
        }
        Err(e) => eprintln!("denied sibling secret ({e})"),
    }
    ExitCode::SUCCESS
}
