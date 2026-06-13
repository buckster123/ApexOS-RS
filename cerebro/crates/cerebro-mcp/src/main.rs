use std::sync::Arc;

use anyhow::Result;
use cerebro::CerebroCortex;
use tracing::info;

mod dispatch;
mod tools;
mod transport;

use transport::{Frame, StdioTransport};

/// cerebro-mcp — MCP-over-stdio server exposing the CerebroCortex tool surface:
/// 66 advertised tools (63 functional + 3 deferred Tier-7 stubs).
/// Drop-in replacement for `python -m cerebrocortex.mcp` in plugins.toml.
#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)  // MCP uses stdout for JSON-RPC; logs go to stderr
        .init();

    let config = cerebro::config::Config::from_env()?;
    let brain  = Arc::new(CerebroCortex::new(config).await?);

    info!("cerebro-mcp starting");

    let mut transport = StdioTransport::new();

    // MCP initialize handshake (C-RS-006: guard on the method — a non-initialize
    // first message must get a proper method_not_found, not an init response).
    // CB-010: a malformed first frame is answered with a parse-error, not a crash.
    let init_req = match transport.read().await? {
        Frame::Value(v) => v,
        Frame::Eof => {
            info!("cerebro-mcp: stdin closed before initialize");
            return Ok(());
        }
        Frame::ParseError(e) => {
            tracing::error!("malformed initialize frame: {e}");
            transport.write(&dispatch::parse_error()).await?;
            // Without a valid initialize we cannot continue the handshake.
            return Ok(());
        }
    };
    let init_resp = if init_req["method"].as_str() == Some("initialize") {
        dispatch::handle_initialize(&init_req)
    } else {
        tracing::warn!("first message was not 'initialize': {:?}", init_req["method"]);
        dispatch::method_not_found(&init_req)
    };
    transport.write(&init_resp).await?;

    // Main dispatch loop
    loop {
        let msg = match transport.read().await {
            // Genuine IO error on stdin — the stream is gone; stop serving.
            Err(e) => {
                tracing::error!("transport IO error: {e}");
                break;
            }
            // Clean client disconnect.
            Ok(Frame::Eof) => break,
            // CB-010: a single malformed frame is NOT fatal. Log, reply with a
            // JSON-RPC -32700 parse error, and keep the daemon alive for the
            // next frame instead of taking the whole memory subsystem down.
            Ok(Frame::ParseError(e)) => {
                tracing::warn!("dropping malformed JSON-RPC frame: {e}");
                transport.write(&dispatch::parse_error()).await?;
                continue;
            }
            Ok(Frame::Value(v)) => v,
        };

        // Notifications carry no "id" — never send a response to them.
        let is_notification = msg["id"].is_null()
            || msg["method"].as_str().map(|m| m.starts_with("notifications/")).unwrap_or(false);
        if is_notification { continue; }

        let method = msg["method"].as_str().unwrap_or("").to_string();
        let resp = match method.as_str() {
            "tools/list" => dispatch::tools_list(&msg),
            "tools/call" => dispatch::dispatch_tool(msg, Arc::clone(&brain)).await,
            _ => dispatch::method_not_found(&msg),
        };
        transport.write(&resp).await?;
    }

    info!("cerebro-mcp exiting");
    Ok(())
}
