// The `tools::list()` schema is one large `json!` array literal; the extra git
// tool schemas push it past serde_json's default macro recursion depth (128).
#![recursion_limit = "256"]

use serde_json::{json, Value};
use std::io::{self, BufRead, Write};

mod tools;

struct ToolsArgs {
    class: Option<tools::ToolClass>,
    listen: Option<std::path::PathBuf>,
}

fn parse_args() -> Result<ToolsArgs, String> {
    let mut class = None;
    let mut listen = None;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        if let Some(v) = a.strip_prefix("--class=") {
            class = tools::parse_class(v)?;
            continue;
        }
        if a == "--class" {
            let v = args
                .next()
                .ok_or_else(|| "--class needs a value (fs, net, dev, or all)".to_string())?;
            class = tools::parse_class(&v)?;
            continue;
        }
        if let Some(v) = a.strip_prefix("--listen=") {
            listen = Some(std::path::PathBuf::from(v));
            continue;
        }
        if a == "--listen" {
            let v = args
                .next()
                .ok_or_else(|| "--listen needs a socket path".to_string())?;
            listen = Some(std::path::PathBuf::from(v));
            continue;
        }
        return Err(format!("unknown argument: {a}"));
    }
    if class.is_none() {
        class = match std::env::var("APEXOS_TOOLS_CLASS") {
            Ok(v) => tools::parse_class(&v)?,
            Err(_) => None,
        };
    }
    Ok(ToolsArgs { class, listen })
}

fn main() {
    let ToolsArgs { class, listen } = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("[apexos-tools] {e}");
            std::process::exit(2);
        }
    };

    // Bind before Landlock so --listen on /run/apexos (PR 2) keeps its fd.
    let listener = listen.map(|path| bind_listen(&path));

    // Finding 11: fs and dev workers drop into an empty netns. The net worker
    // keeps the host network. Compat (no --class) stays on the host net.
    if matches!(
        class,
        Some(tools::ToolClass::Fs) | Some(tools::ToolClass::Dev)
    ) {
        match apexos_confine::isolate_network() {
            apexos_confine::NetnsStatus::Isolated => {
                eprintln!("[apexos-tools] netns isolated");
            }
            apexos_confine::NetnsStatus::Disabled => {
                eprintln!("[apexos-tools] netns disabled (APEXOS_NETNS)");
            }
            apexos_confine::NetnsStatus::Unsupported => {
                eprintln!(
                    "[apexos-tools] netns unsupported — this worker still shares the host net"
                );
            }
            apexos_confine::NetnsStatus::Error(e) => {
                eprintln!(
                    "[apexos-tools] netns failed ({e}) — this worker still shares the host net"
                );
            }
        }
    }

    // Finding 11 part 2: same-uid DAC still sees /var/lib/agentd/.api_key.
    // Only the device worker (and the unclassed compat process) get /dev.
    // The shell worker must not — run_command would inherit it.
    let grant_devices = matches!(class, None | Some(tools::ToolClass::Dev));
    match apexos_confine::restrict_tools_worker_for(grant_devices) {
        apexos_confine::LandlockStatus::Restricted { abi } => {
            eprintln!("[apexos-tools] landlock restricted (abi {abi})");
        }
        apexos_confine::LandlockStatus::Disabled => {
            eprintln!("[apexos-tools] landlock disabled (APEXOS_LANDLOCK)");
        }
        apexos_confine::LandlockStatus::Unsupported => {
            eprintln!("[apexos-tools] landlock unsupported — same-uid residual remains");
        }
        apexos_confine::LandlockStatus::Error(e) => {
            eprintln!("[apexos-tools] landlock failed ({e}) — same-uid residual remains");
        }
    }

    if let Some(listener) = listener {
        eprintln!(
            "[apexos-tools] listening on {}",
            listener
                .local_addr()
                .map(|a| a
                    .as_pathname()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "fd".into()))
                .unwrap_or_else(|_| "?".into())
        );
        serve_listen(listener, class);
    } else {
        let stdin = io::stdin();
        let stdout = io::stdout();
        serve_rpc(stdin.lock(), stdout.lock(), class);
    }
}

fn bind_listen(path: &std::path::Path) -> std::os::unix::net::UnixListener {
    let _ = std::fs::remove_file(path);
    if let Some(dir) = path.parent() {
        if !dir.as_os_str().is_empty() {
            let _ = std::fs::create_dir_all(dir);
        }
    }
    match std::os::unix::net::UnixListener::bind(path) {
        Ok(l) => {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o660));
            l
        }
        Err(e) => {
            eprintln!("[apexos-tools] listen {}: {e}", path.display());
            std::process::exit(2);
        }
    }
}

fn serve_listen(listener: std::os::unix::net::UnixListener, class: Option<tools::ToolClass>) {
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                let reader = match stream.try_clone() {
                    Ok(s) => io::BufReader::new(s),
                    Err(e) => {
                        eprintln!("[apexos-tools] clone: {e}");
                        continue;
                    }
                };
                serve_rpc(reader, stream, class);
            }
            Err(e) => {
                eprintln!("[apexos-tools] accept: {e}");
                break;
            }
        }
    }
}

fn serve_rpc<R: BufRead, W: Write>(input: R, mut out: W, class: Option<tools::ToolClass>) {
    for line in input.lines() {
        let line = match line {
            Ok(l) if !l.trim().is_empty() => l,
            _ => continue,
        };

        let req: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };

        let id = req.get("id").cloned().unwrap_or(Value::Null);
        let method = req["method"].as_str().unwrap_or("");

        let response = match method {
            "initialize" => json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "protocolVersion": "2024-11-05",
                    "capabilities": { "tools": {} },
                    "serverInfo": { "name": "apexos-tools", "version": "0.1.0" }
                }
            }),
            "notifications/initialized" => continue,
            "tools/list" => json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": { "tools": tools::list_for(class) }
            }),
            "tools/call" => {
                let params = &req["params"];
                let name = params["name"].as_str().unwrap_or("");
                let args = params.get("arguments").cloned().unwrap_or(json!({}));
                let result = tools::call_for(name, &args, class);
                json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": result
                })
            }
            _ => json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": { "code": -32601, "message": "method not found" }
            }),
        };

        let _ = writeln!(out, "{}", serde_json::to_string(&response).unwrap());
        let _ = out.flush();
    }
}
