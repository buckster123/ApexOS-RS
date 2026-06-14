//! world-vision — agent-vision MCP-over-stdio plugin for apexos-world.
//!
//! A child process agentd's `Supervisor` spawns and lists in `plugins.toml`. It exposes
//! read-only world-vision tools (`world_look`, `world_snapshot`) that let an embodied agent
//! SEE through an avatar / station / free camera. It owns no renderer: every call is
//! forwarded over a local IPC to the running `world-app` (see `snapshot_client`), which does
//! the offscreen render + readback. See `world/docs/design/04-agent-embodiment-and-vision.md`.
//!
//! ## MCP contract (matched against cerebro-mcp / apexos-tools, agentd plugins/src/mcp.rs)
//! - newline-delimited JSON-RPC 2.0 over stdio
//! - **stdout is JSON-RPC only**; all logs go to **stderr**
//! - `initialize` → `notifications/initialized` (no reply) → `tools/list` → `tools/call`
//! - tool result: `{ "content": [...blocks], "isError": bool }`
//!
//! This is a leaf process — the Slint-main-thread / never-`#[tokio::main]` rule that governs
//! `world-app` does NOT apply here, so `#[tokio::main]` is correct (matches cerebro-mcp).

use anyhow::Result;
use tracing::info;

mod dispatch;
mod snapshot_client;
mod tools;
mod transport;

use transport::StdioTransport;

#[tokio::main]
async fn main() -> Result<()> {
    // MCP uses stdout for JSON-RPC; logs MUST go to stderr or they corrupt the protocol.
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();

    info!("world-vision starting (ipc={})", snapshot_client::ipc_path());

    let mut transport = StdioTransport::new();

    // MCP initialize handshake — agentd sends `initialize` first and expects a result.
    let init_req = transport.read().await?;
    let init_resp = dispatch::handle_initialize(&init_req);
    transport.write(&init_resp).await?;

    // Main dispatch loop.
    loop {
        match transport.read().await {
            Err(e) => {
                // EOF on stdin = agentd closed the pipe (clean shutdown).
                if e.to_string().contains("EOF") {
                    break;
                }
                tracing::error!("transport error: {e}");
                break;
            }
            Ok(msg) => {
                // Notifications carry no `id` (or a `notifications/*` method) — never reply.
                let is_notification = msg["id"].is_null()
                    || msg["method"]
                        .as_str()
                        .map(|m| m.starts_with("notifications/"))
                        .unwrap_or(false);
                if is_notification {
                    continue;
                }

                let method = msg["method"].as_str().unwrap_or("").to_string();
                let resp = match method.as_str() {
                    "tools/list" => dispatch::tools_list(&msg),
                    "tools/call" => dispatch::dispatch_tool(msg).await,
                    _ => dispatch::method_not_found(&msg),
                };
                transport.write(&resp).await?;
            }
        }
    }

    info!("world-vision exiting");
    Ok(())
}
