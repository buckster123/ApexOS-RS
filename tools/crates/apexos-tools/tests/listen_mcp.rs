//! PR 1: `--listen` speaks the same newline MCP as stdio.
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::process::{Command, Stdio};
use std::time::Duration;

fn tools_bin() -> std::path::PathBuf {
    std::env::var_os("CARGO_BIN_EXE_apexos-tools")
        .map(std::path::PathBuf::from)
        .expect("cargo sets CARGO_BIN_EXE_apexos-tools for integration tests")
}

fn send(stream: &mut UnixStream, msg: Value) -> Value {
    writeln!(stream, "{msg}").unwrap();
    stream.flush().unwrap();
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    serde_json::from_str(line.trim()).unwrap()
}

#[test]
fn listen_fs_class_lists_tools() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("tools-fs.sock");
    let mut child = Command::new(tools_bin())
        .args(["--class", "fs", "--listen"])
        .arg(&sock)
        .env("APEXOS_LANDLOCK", "0")
        .env("APEXOS_NETNS", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    let mut stream = None;
    for _ in 0..40 {
        if let Ok(s) = UnixStream::connect(&sock) {
            stream = Some(s);
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let mut stream = stream.expect("worker did not bind listen socket");

    let init = send(
        &mut stream,
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": { "protocolVersion": "2024-11-05", "capabilities": {}, "clientInfo": {"name": "t"} }
        }),
    );
    assert_eq!(init["result"]["serverInfo"]["name"], "apexos-tools");

    writeln!(
        stream,
        "{}",
        json!({"jsonrpc":"2.0","method":"notifications/initialized","params":{}})
    )
    .unwrap();

    let listed = send(
        &mut stream,
        json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}),
    );
    let tools = listed["result"]["tools"].as_array().unwrap();
    assert!(
        tools.iter().any(|t| t["name"] == "read_file"),
        "fs class should advertise read_file"
    );
    assert!(
        !tools.iter().any(|t| t["name"] == "http_fetch"),
        "fs class must not advertise http_fetch"
    );

    child.kill().ok();
    let _ = child.wait();
}
