//! JSON-RPC method handlers. Mirrors `cerebro-mcp/src/dispatch.rs`.
//!
//! Every reply is a complete JSON-RPC 2.0 object with the request `id` echoed. The
//! `tools/call` result wraps the rendered view in MCP content blocks per design doc 04 §3.2:
//! a `text` manifest block (the text-only fallback) followed by an `image` block in the
//! Anthropic shape (`source:{type:"base64", media_type, data}`) that agentd's provider layer
//! forwards to a vision model.

use serde_json::{json, Value};

use crate::snapshot_client::{self, RenderRequest, RenderResult};
use crate::tools;

pub fn handle_initialize(req: &Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": req["id"],
        "result": {
            "protocolVersion": "2024-11-05",
            "capabilities": { "tools": {} },
            "serverInfo": {
                "name": "world-vision",
                "version": env!("CARGO_PKG_VERSION")
            }
        }
    })
}

pub fn tools_list(req: &Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": req["id"],
        "result": { "tools": tools::list() }
    })
}

pub fn method_not_found(req: &Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": req["id"],
        "error": { "code": -32601, "message": "method not found" }
    })
}

/// Route a `tools/call` to the named world tool and build the MCP result envelope.
pub async fn dispatch_tool(msg: Value) -> Value {
    let id = msg["id"].clone();
    let params = &msg["params"];
    let name = params["name"].as_str().unwrap_or("");
    let args = params.get("arguments").cloned().unwrap_or_else(|| json!({}));

    match route(name, &args).await {
        Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
        // Tool-level failures ride the MCP content+isError channel, not a JSON-RPC error,
        // so the agent sees the message and can retry (DESIGN.md §6 graceful degradation).
        Err(e) => json!({ "jsonrpc": "2.0", "id": id, "result": tool_error(e.to_string()) }),
    }
}

/// Map a tool name + args to a `RenderRequest`, render, and wrap the result.
async fn route(name: &str, args: &Value) -> anyhow::Result<Value> {
    let req = match name {
        "world_look" => {
            let view = args
                .get("view")
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("world_look: `view` is required"))?;
            RenderRequest {
                op: "look",
                view: Some(view),
                width: args["width"].as_u64().unwrap_or(1024).min(1920) as u32,
                height: args["height"].as_u64().unwrap_or(576).min(1080) as u32,
                format: args["format"].as_str().unwrap_or("jpeg").to_string(),
                annotate: args["annotate"].as_bool().unwrap_or(true),
            }
        }
        "world_snapshot" => RenderRequest {
            op: "snapshot",
            view: None,
            width: args["width"].as_u64().unwrap_or(1024).min(1920) as u32,
            height: args["height"].as_u64().unwrap_or(576).min(1080) as u32,
            format: args["format"].as_str().unwrap_or("jpeg").to_string(),
            annotate: false,
        },
        other => return Err(anyhow::anyhow!("unknown tool: {other}")),
    };

    match snapshot_client::render(&req).await {
        Ok(view) => Ok(image_result(&view)),
        // world-app unreachable → clean tool error, not a wedged turn.
        Err(e) => Ok(tool_error(format!("world renderer not running: {e}"))),
    }
}

/// Build a success result: a `text` manifest block + an `image` block (Anthropic shape).
fn image_result(view: &RenderResult) -> Value {
    json!({
        "isError": false,
        "content": [
            { "type": "text", "text": view.manifest },
            { "type": "image",
              "source": {
                  "type": "base64",
                  "media_type": view.media_type,
                  "data": view.image_b64
              } }
        ]
    })
}

/// Build a tool-level error result (MCP `isError:true` with a text body).
fn tool_error(msg: impl Into<String>) -> Value {
    json!({
        "isError": true,
        "content": [{ "type": "text", "text": msg.into() }]
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initialize_echoes_id_and_advertises_tools() {
        let req = json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {} });
        let resp = handle_initialize(&req);
        assert_eq!(resp["id"], 1, "id must be echoed");
        assert_eq!(resp["result"]["protocolVersion"], "2024-11-05");
        assert_eq!(resp["result"]["serverInfo"]["name"], "world-vision");
        assert!(resp["result"]["capabilities"]["tools"].is_object());
    }

    #[test]
    fn tools_list_advertises_look_and_snapshot_with_schemas() {
        let req = json!({ "jsonrpc": "2.0", "id": 42, "method": "tools/list", "params": {} });
        let resp = tools_list(&req);
        assert_eq!(resp["id"], 42);
        let tools = resp["result"]["tools"].as_array().unwrap();
        let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert!(names.contains(&"world_look"));
        assert!(names.contains(&"world_snapshot"));

        let look = tools.iter().find(|t| t["name"] == "world_look").unwrap();
        assert_eq!(look["inputSchema"]["type"], "object");
        let required = look["inputSchema"]["required"].as_array().unwrap();
        assert!(required.iter().any(|v| v == "view"), "view must be required");
    }

    #[tokio::test]
    async fn world_snapshot_returns_image_and_text_blocks() {
        let msg = json!({
            "jsonrpc": "2.0", "id": 3, "method": "tools/call",
            "params": { "name": "world_snapshot", "arguments": {} }
        });
        let resp = dispatch_tool(msg).await;
        assert_eq!(resp["id"], 3);
        assert!(resp["error"].is_null(), "tool errors ride the result channel, not JSON-RPC error");
        let content = resp["result"]["content"].as_array().unwrap();
        assert_eq!(resp["result"]["isError"], false);
        assert_eq!(content[0]["type"], "text", "first block is the manifest");
        assert_eq!(content[1]["type"], "image", "second block is the image");
        assert_eq!(content[1]["source"]["type"], "base64");
        assert!(content[1]["source"]["data"].as_str().is_some_and(|s| !s.is_empty()));
    }

    #[tokio::test]
    async fn world_look_requires_view() {
        let msg = json!({
            "jsonrpc": "2.0", "id": 4, "method": "tools/call",
            "params": { "name": "world_look", "arguments": {} }
        });
        let resp = dispatch_tool(msg).await;
        // Missing `view` is a tool-level error: isError:true, not a JSON-RPC error.
        assert!(resp["error"].is_null());
        assert_eq!(resp["result"]["isError"], true);
    }

    #[tokio::test]
    async fn unknown_tool_is_error() {
        let msg = json!({
            "jsonrpc": "2.0", "id": 5, "method": "tools/call",
            "params": { "name": "world_fly_to_moon", "arguments": {} }
        });
        let resp = dispatch_tool(msg).await;
        assert_eq!(resp["result"]["isError"], true);
    }
}
