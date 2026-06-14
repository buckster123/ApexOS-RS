//! Tool catalog: the agentd-facing surface advertised in `tools/list`.
//!
//! Schemas track design doc 04 §3.1 (`world_look`) and DESIGN.md §4. Read-only vision
//! tools default to `allow` in `policy.toml`; world-mutating verbs (added later — see
//! design doc 04 §2) default to `ask`. This scaffold ships the two read-only views only.

use serde_json::{json, Value};

/// The MCP `tools/list` payload — a JSON array of `{name, description, inputSchema}`.
pub fn list() -> Value {
    json!([
        {
            "name": "world_look",
            "description": "Render what an avatar / station / free camera currently sees in the \
                            apexos-world 3D scene and return it as an image plus a text manifest of \
                            visible entities. Use to inspect the world, read a station's surface, or \
                            check where another agent is. Returns a placeholder until world-app is wired \
                            (DESIGN.md §M2).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "view": {
                        "description": "Which camera to render from. \"self\" = the caller's own avatar \
                                        camera (requires the avatar↔session roster; see DESIGN.md R2). \
                                        Otherwise name an avatar session id, a station, or a free camera.",
                        "oneOf": [
                            { "const": "self" },
                            { "type": "object", "properties": { "avatar":  { "type": "integer", "description": "avatar SessionId (bare u64)" } }, "required": ["avatar"] },
                            { "type": "object", "properties": { "station": { "type": "string",  "description": "station name, e.g. \"sensors\"" } }, "required": ["station"] },
                            { "type": "object", "properties": { "free_cam": {
                                "type": "object",
                                "properties": {
                                    "eye":     { "type": "array", "items": { "type": "number" }, "minItems": 3, "maxItems": 3 },
                                    "target":  { "type": "array", "items": { "type": "number" }, "minItems": 3, "maxItems": 3 },
                                    "fov_deg": { "type": "number" }
                                },
                                "required": ["eye", "target"]
                            } }, "required": ["free_cam"] }
                        ]
                    },
                    "width":    { "type": "integer", "default": 1024, "maximum": 1920, "description": "render width in px" },
                    "height":   { "type": "integer", "default": 576,  "maximum": 1080, "description": "render height in px" },
                    "format":   { "type": "string",  "enum": ["jpeg", "png"], "default": "jpeg" },
                    "annotate": { "type": "boolean", "default": true, "description": "overlay / list visible entity labels in the manifest" }
                },
                "required": ["view"]
            }
        },
        {
            "name": "world_snapshot",
            "description": "Render the apexos-world overview camera (a fixed wide shot of the Atrium) and \
                            return it as an image. A zero-argument convenience wrapper over world_look's \
                            free-camera view — \"show me the whole room\". Returns a placeholder until \
                            world-app is wired (DESIGN.md §M2).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "width":  { "type": "integer", "default": 1024, "maximum": 1920 },
                    "height": { "type": "integer", "default": 576,  "maximum": 1080 },
                    "format": { "type": "string",  "enum": ["jpeg", "png"], "default": "jpeg" }
                }
            }
        }
    ])
}
