use std::sync::Arc;

use anyhow::Result;
use cerebro::CerebroCortex;
use tracing::info;

mod dispatch;
mod tools;
mod transport;

use transport::StdioTransport;

/// cerebro-mcp — MCP-over-stdio server exposing all 63 CerebroCortex tools.
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

    // MCP initialize handshake
    let init_req = transport.read().await?;
    let init_resp = dispatch::handle_initialize(&init_req);
    transport.write(&init_resp).await?;

    // Main dispatch loop
    loop {
        match transport.read().await {
            Err(e) => {
                // EOF on stdin = client disconnected cleanly
                if e.to_string().contains("EOF") { break; }
                tracing::error!("transport error: {e}");
                break;
            }
            Ok(msg) => {
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
        }
    }

    info!("cerebro-mcp exiting");
    Ok(())
}
