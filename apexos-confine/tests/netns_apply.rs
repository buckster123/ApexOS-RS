//! Process-local netns apply (harness = false).
//!
//! Proves isolate_network() leaves this process unable to open a WAN TCP
//! socket — the finding 11 shell-worker close.

use apexos_confine::{isolate_network, NetnsStatus};
use std::net::{SocketAddr, TcpStream};
use std::process::ExitCode;
use std::time::Duration;

fn main() -> ExitCode {
    if cfg!(not(target_os = "linux")) {
        eprintln!("skip: not linux");
        return ExitCode::SUCCESS;
    }

    match isolate_network() {
        NetnsStatus::Isolated => eprintln!("isolated"),
        NetnsStatus::Unsupported => {
            eprintln!("skip: unprivileged userns/netns unsupported");
            return ExitCode::SUCCESS;
        }
        NetnsStatus::Disabled => {
            eprintln!("skip: disabled");
            return ExitCode::SUCCESS;
        }
        NetnsStatus::Error(e) => {
            eprintln!("isolate error: {e}");
            return ExitCode::from(2);
        }
    }

    let addr: SocketAddr = "1.1.1.1:443".parse().unwrap();
    match TcpStream::connect_timeout(&addr, Duration::from_millis(400)) {
        Ok(_) => {
            eprintln!("WAN connect succeeded — netns did not isolate");
            ExitCode::from(3)
        }
        Err(e) => {
            eprintln!("wan denied ({e})");
            ExitCode::SUCCESS
        }
    }
}
