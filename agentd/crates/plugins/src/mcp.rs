use std::collections::HashMap;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};
use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::Child,
    sync::{oneshot, Mutex},
};
use apexos_core::{ToolOutput, ToolSpec};

/// MCP protocol version we negotiate.
const PROTOCOL_VERSION: &str = "2024-11-05";

/// Async JSON-RPC client attached to a child process's stdio.
///
/// Wire format: newline-delimited JSON (no Content-Length framing).
/// A background reader task dispatches incoming response lines to
/// pending oneshot channels keyed by request id.
pub struct McpClient {
    write_tx: tokio::sync::mpsc::Sender<String>,
    pending:  Arc<Mutex<HashMap<u64, oneshot::Sender<Value>>>>,
    next_id:  Arc<AtomicU64>,
}

impl McpClient {
    /// Attach to a freshly-spawned child, wiring stdin/stdout.
    /// Call [`initialize`] next; do NOT send any requests before that.
    pub async fn attach(child: &mut Child) -> Result<Self> {
        let stdin  = child.stdin .take().ok_or_else(|| anyhow!("child has no stdin"))?;
        let stdout = child.stdout.take().ok_or_else(|| anyhow!("child has no stdout"))?;

        let pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Value>>>> =
            Arc::new(Mutex::new(HashMap::new()));

        let (write_tx, mut write_rx) = tokio::sync::mpsc::channel::<String>(64);

        // Writer task: forward serialized messages to the child's stdin.
        let mut stdin = stdin;
        tokio::spawn(async move {
            while let Some(msg) = write_rx.recv().await {
                if stdin.write_all(msg.as_bytes()).await.is_err() {
                    break;
                }
            }
        });

        // Reader task: parse response lines and dispatch to pending senders.
        let pending_clone = pending.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let line = line.trim().to_string();
                if line.is_empty() {
                    continue;
                }
                let msg: Value = match serde_json::from_str(&line) {
                    Ok(v)  => v,
                    Err(_) => continue,
                };
                // Messages with an id are responses; dispatch to pending.
                if let Some(id) = msg.get("id").and_then(|v| v.as_u64()) {
                    if let Some(tx) = pending_clone.lock().await.remove(&id) {
                        let _ = tx.send(msg);
                    }
                }
                // Messages without id are server notifications — ignore for now.
            }
        });

        Ok(Self {
            write_tx,
            pending,
            next_id: Arc::new(AtomicU64::new(1)),
        })
    }

    /// Perform the MCP initialize handshake.
    pub async fn initialize(&self) -> Result<()> {
        self.request("initialize", json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities":    {},
            "clientInfo":      { "name": "agentd", "version": env!("CARGO_PKG_VERSION") },
        })).await?;
        // initialized notification has no id and expects no response.
        self.notify("notifications/initialized", json!({})).await
    }

    /// Retrieve the tool manifest from the server.
    pub async fn list_tools(&self) -> Result<Vec<ToolSpec>> {
        let result = self.request("tools/list", json!({})).await?;
        let tools = result["tools"]
            .as_array()
            .ok_or_else(|| anyhow!("tools/list: missing tools array"))?;

        tools.iter().map(|t| {
            Ok(ToolSpec {
                name:         t["name"].as_str()
                                  .ok_or_else(|| anyhow!("tool missing name"))?.to_string(),
                description:  t.get("description").and_then(|v| v.as_str())
                                  .unwrap_or("").to_string(),
                input_schema: t.get("inputSchema").cloned().unwrap_or(json!({})),
            })
        }).collect()
    }

    /// Invoke a tool by name.
    pub async fn call_tool(&self, name: &str, args: &Value) -> Result<ToolOutput> {
        let result = self.request("tools/call", json!({
            "name":      name,
            "arguments": args,
        })).await?;

        let is_error = result.get("isError").and_then(|v| v.as_bool()).unwrap_or(false);
        Ok(ToolOutput {
            ok:      !is_error,
            content: result.get("content").cloned().unwrap_or(Value::Null),
        })
    }

    // ── internal ──────────────────────────────────────────────────────────────

    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);

        self.send_line(&json!({
            "jsonrpc": "2.0",
            "id":      id,
            "method":  method,
            "params":  params,
        })).await?;

        let response = rx.await.map_err(|_| anyhow!("MCP server closed before responding to {method}"))?;

        if let Some(err) = response.get("error") {
            anyhow::bail!("MCP error on {method}: {err}");
        }
        Ok(response.get("result").cloned().unwrap_or(Value::Null))
    }

    async fn notify(&self, method: &str, params: Value) -> Result<()> {
        self.send_line(&json!({
            "jsonrpc": "2.0",
            "method":  method,
            "params":  params,
        })).await
    }

    async fn send_line(&self, msg: &Value) -> Result<()> {
        let line = format!("{msg}\n");
        self.write_tx.send(line).await
            .map_err(|_| anyhow!("MCP writer task is gone"))
    }
}
