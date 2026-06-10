/// Tests for McpClient + Supervisor.
///
/// Unit tests use an inline Python mock server (requires python3).
/// CerebroCortex integration test is #[ignore] — run with:
///   cargo test -p apexos-plugins -- --include-ignored cerebro
use apexos_plugins::McpClient;
use std::process::Stdio;
use tokio::process::Command;

fn python_mock_server() -> &'static str {
    // Minimal MCP server: handles initialize, tools/list, tools/call.
    // Ignores notifications (no id field).
    r#"
import sys, json
sys.stdout.reconfigure(line_buffering=True)
TOOLS = [{"name":"mock_tool","description":"A test tool","inputSchema":{"type":"object","properties":{}}}]
for raw in sys.stdin:
    raw = raw.strip()
    if not raw: continue
    msg = json.loads(raw)
    if "id" not in msg: continue
    m = msg.get("method","")
    if m == "initialize":
        r = {"protocolVersion":"2024-11-05","capabilities":{},"serverInfo":{"name":"mock","version":"0.1"}}
    elif m == "tools/list":
        r = {"tools": TOOLS}
    elif m == "tools/call":
        r = {"content":[{"type":"text","text":"called:" + msg["params"]["name"]}],"isError":False}
    else:
        r = {}
    print(json.dumps({"jsonrpc":"2.0","id":msg["id"],"result":r}))
    sys.stdout.flush()
"#
}

async fn spawn_mock() -> (apexos_plugins::McpClient, tokio::process::Child) {
    let python = std::env::var("PYTHON3").unwrap_or_else(|_| "python3".into());
    let mut child = Command::new(&python)
        .args(["-c", python_mock_server()])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn mock server (is python3 in PATH?)");

    let client = McpClient::attach(&mut child).await.unwrap();
    (client, child)
}

#[tokio::test]
async fn handshake_and_list_tools() {
    let (client, _child) = spawn_mock().await;
    client.initialize().await.unwrap();
    let tools = client.list_tools().await.unwrap();
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].name, "mock_tool");
    assert_eq!(tools[0].description, "A test tool");
}

#[tokio::test]
async fn call_tool_returns_output() {
    let (client, _child) = spawn_mock().await;
    client.initialize().await.unwrap();
    let out = client.call_tool("mock_tool", &serde_json::json!({})).await.unwrap();
    assert!(out.ok);
    // content is a JSON array of {type,text} blocks
    let text = out.content[0]["text"].as_str().unwrap();
    assert_eq!(text, "called:mock_tool");
}

// ── Real CerebroCortex ────────────────────────────────────────────────────────

const CEREBRO_MCP: &str = "/home/andre/Projects/CerebroCortex/cerebro-mcp";

#[tokio::test]
#[ignore]
async fn cerebro_handshake_and_tool_list() {
    assert!(
        std::path::Path::new(CEREBRO_MCP).exists(),
        "cerebro-mcp not found at {CEREBRO_MCP}"
    );

    let mut child = Command::new(CEREBRO_MCP)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn cerebro-mcp");

    let client = McpClient::attach(&mut child).await.unwrap();
    client.initialize().await.unwrap();

    let tools = client.list_tools().await.unwrap();
    assert!(!tools.is_empty(), "expected at least one tool from CerebroCortex");
    println!("CerebroCortex tools ({}):", tools.len());
    for t in &tools {
        println!("  {}", t.name);
    }
}

#[tokio::test]
#[ignore]
async fn cerebro_recall_tool_call() {
    let mut child = Command::new(CEREBRO_MCP)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();

    let client = McpClient::attach(&mut child).await.unwrap();
    client.initialize().await.unwrap();

    let out = client.call_tool("recall", &serde_json::json!({
        "query": "ApexOS",
        "limit": 1,
    })).await.unwrap();

    assert!(out.ok, "recall returned is_error=true: {:?}", out.content);
    println!("recall result: {}", out.content);
}
