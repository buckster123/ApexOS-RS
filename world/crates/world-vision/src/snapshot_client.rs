//! IPC bridge to the running `world-app` renderer.
//!
//! `world-vision` owns no GPU and no scene — it forwards every vision/act call over a
//! **local IPC** to `world-app`, awaits the rendered result, and hands it back to agentd.
//!
//! ## Wire (design doc 04 §4, DESIGN.md §6)
//! Newline-delimited JSON over a **unix domain socket** at `$XDG_RUNTIME_DIR/apexos-world.sock`
//! (override `WORLD_IPC_PATH`) — the same transport shape as the MCP stdio channel the team
//! already knows. One request object per line, one reply per line, correlated by a `req` id.
//! Only `world-app` binds the socket; this plugin only connects, with backoff. If world-app
//! is down, every world tool returns a clean error (graceful degradation — DESIGN.md §6),
//! never a wedged turn.
//!
//! ```jsonc
//! // plugin → world-app
//! { "req": 7, "op": "look", "view": {"self": true}, "caller_session": 42,
//!   "width": 1024, "height": 576, "format": "jpeg", "annotate": true }
//! // world-app → plugin
//! { "req": 7, "ok": true, "image_b64": "...", "media_type": "image/jpeg",
//!   "manifest": [ {"entity":"station:sensors","kind":"station","label":"Sensors","xy":[210,140]} ] }
//! ```
//!
//! ## Status: SCAFFOLD
//! The socket round-trip is **not yet wired** — see the `TODO(M2)` in [`render`]. Until then
//! every call resolves to a 1×1 placeholder image so the MCP handshake, `tools/list`, and
//! `tools/call` paths are exercisable end-to-end against agentd today (DESIGN.md §M2 / doc 04 E2–E3).

use anyhow::Result;
use serde_json::{json, Value};

/// Default IPC socket path when `WORLD_IPC_PATH` is unset.
/// (Falls back under `$XDG_RUNTIME_DIR` when that is set; see [`ipc_path`].)
pub const DEFAULT_SOCK_NAME: &str = "apexos-world.sock";

/// A request for a rendered view, normalized from a tool call.
#[derive(Debug, Clone)]
pub struct RenderRequest {
    /// `look` (camera-by-view) or `snapshot` (overview cam) — maps to the IPC `op`.
    pub op: &'static str,
    /// The resolved `view` selector for `world_look`; `None` for `world_snapshot`.
    pub view: Option<Value>,
    pub width: u32,
    pub height: u32,
    pub format: String,
    pub annotate: bool,
}

/// A rendered view returned by world-app.
#[derive(Debug, Clone)]
pub struct RenderResult {
    /// Base64-encoded image bytes (no data-URI prefix).
    pub image_b64: String,
    /// MIME type, e.g. `image/jpeg`.
    pub media_type: String,
    /// A short human/agent-readable manifest of what's in frame (entity labels, kinds).
    /// Doubles as the text-only fallback for non-vision models (DESIGN.md R1).
    pub manifest: String,
}

/// Resolve the IPC socket path: `WORLD_IPC_PATH`, else `$XDG_RUNTIME_DIR/apexos-world.sock`,
/// else `/tmp/apexos-world.sock`.
pub fn ipc_path() -> String {
    if let Ok(p) = std::env::var("WORLD_IPC_PATH") {
        if !p.is_empty() {
            return p;
        }
    }
    let base = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".to_string());
    format!("{}/{}", base.trim_end_matches('/'), DEFAULT_SOCK_NAME)
}

/// Forward a render request to world-app and await the result.
///
/// Returns `Err` if world-app is unreachable — the caller maps that to an MCP
/// `isError:true` result so the agent degrades gracefully.
pub async fn render(req: &RenderRequest) -> Result<RenderResult> {
    let sock = ipc_path();

    // TODO(M2 — design doc 04 §3.3/§4, DESIGN.md Spike 3): wire the real IPC.
    //   1. Connect to the unix socket at `sock` (tokio::net::UnixStream), with fixed
    //      backoff retry à la apex-sensor-bridge; bail clean if world-app is not running.
    //   2. Write one line of newline-delimited JSON:
    //        { "req": <n>, "op": req.op, "view": req.view, "caller_session": <see R2>,
    //          "width": req.width, "height": req.height, "format": req.format,
    //          "annotate": req.annotate }
    //      NB R2 (DESIGN.md): agentd's tools/call does NOT forward the caller's SessionId,
    //      so `view:"self"` needs either an explicit `avatar` arg or the ActionId-nonce
    //      roster correlation. Until that lands, "self" should error asking for a target.
    //   3. Read one reply line, match on `req`, decode `{ ok, image_b64, media_type, manifest }`.
    //   4. world-app does the offscreen render + wgpu copy_texture_to_buffer + map_async
    //      (256-byte row-pad un-stride is the #1 footgun) + JPEG encode + base64 on its side.
    let _ = &sock;

    Ok(placeholder(req))
}

/// A tiny valid image so the tool round-trip is testable before world-app exists.
/// 1×1 transparent PNG (base64), independent of `req.format` for now.
fn placeholder(req: &RenderRequest) -> RenderResult {
    // 1×1 transparent PNG.
    const PNG_1X1_B64: &str =
        "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==";

    let view_desc = req
        .view
        .as_ref()
        .map(|v| v.to_string())
        .unwrap_or_else(|| "overview".to_string());

    RenderResult {
        image_b64: PNG_1X1_B64.to_string(),
        media_type: "image/png".to_string(),
        manifest: json!({
            "placeholder": true,
            "note": "world-app IPC not yet wired (DESIGN.md §M2); returning a 1x1 placeholder",
            "op": req.op,
            "view": view_desc,
            "requested": { "width": req.width, "height": req.height, "format": req.format, "annotate": req.annotate },
            "entities": []
        })
        .to_string(),
    }
}
