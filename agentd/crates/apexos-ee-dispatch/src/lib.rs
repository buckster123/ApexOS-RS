//! Agentd tool-dispatch bridge for ApexOS-Enterprise.
//!
//! **Public shim** (this crate, in-tree under ApexOS-RS):
//! - Prefer `POST $EE_TOOL_GATE_URL` (or `$EE_ADMIN_URL/api/agentd/tool-gate`)
//!   with `Authorization: Bearer $EE_AGENTD_TOKEN`.
//! - If no sidecar URL is set, apply a **fail-closed** local deny list that
//!   mirrors EE `enterprise_safe` (self-update, yolo-ish tools blocked) and
//!   asks on shell / unknown high-risk tools.
//!
//! **Private dual-checkout** (real PolicyShim in-process):
//! override this package via Cargo `paths` — see `docs/enterprise.md`.
//! The agentd wiring imports only the surface below so both implementations
//! plug into the same chokepoint.

pub mod agentd_hook;

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

pub use agentd_hook::{
    evaluate_tool, evaluate_tool_global, global_gate, init_global_gate, ToolHookInput,
    ToolHookResult,
};

/// Request shape agentd (or a mesh peer) should POST / pass.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolGateRequest {
    pub tool: String,
    /// `admin` | `operator` | `user` (string form for wire convenience).
    pub role: String,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub agent_id: Option<String>,
}

/// Verdict returned by the real EE PolicyShim / admin-api.
///
/// Kept as a tagged enum so HTTP responses and the in-process override share a
/// shape. The public shim only needs enough to drive `ToolHookResult`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "verdict", rename_all = "snake_case")]
pub enum DispatchVerdict {
    Allow {
        #[serde(default)]
        confined_path: Option<PathBuf>,
    },
    Workspace {
        confined_path: PathBuf,
    },
    Ask {
        #[serde(default)]
        reason: Option<String>,
        #[serde(default)]
        confined_path: Option<PathBuf>,
    },
    Deny {
        reason: String,
        #[serde(default)]
        layer: Option<String>,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum GateError {
    #[error("unknown role: {0}")]
    BadRole(String),
    #[error("tool-gate HTTP error: {0}")]
    Http(String),
}

/// Pre-built gate — either HTTP sidecar or local fail-closed defaults.
#[derive(Debug, Clone)]
pub struct AgentdToolGate {
    mode: GateMode,
    workspace: PathBuf,
}

#[derive(Debug, Clone)]
enum GateMode {
    /// `POST {url}` with bearer token (optional).
    Http { url: String, token: Option<String> },
    /// No sidecar — deny enterprise-unsafe tools, ask on shell, allow FS in ws.
    Local,
}

impl AgentdToolGate {
    pub fn new_local(workspace: impl Into<PathBuf>) -> Self {
        Self {
            mode: GateMode::Local,
            workspace: workspace.into(),
        }
    }

    /// Alias used by the private EE crate / dual-checkout override.
    pub fn enterprise_defaults(workspace: impl Into<PathBuf>) -> Self {
        Self::new_local(workspace)
    }

    /// From env:
    /// - `EE_TOOL_GATE_URL` or `EE_ADMIN_URL` (+ `/api/agentd/tool-gate`)
    /// - `EE_AGENTD_TOKEN` (optional bearer)
    /// - `EE_WORKSPACE` (default `./data/workspace`, else `AGENTD_WORKSPACE`)
    pub fn from_env() -> Self {
        let workspace = std::env::var("EE_WORKSPACE")
            .or_else(|_| std::env::var("AGENTD_WORKSPACE"))
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("./data/workspace"));

        let url = std::env::var("EE_TOOL_GATE_URL").ok().or_else(|| {
            std::env::var("EE_ADMIN_URL").ok().map(|base| {
                let base = base.trim_end_matches('/');
                format!("{base}/api/agentd/tool-gate")
            })
        });

        let mode = match url {
            Some(url) if !url.is_empty() => GateMode::Http {
                url,
                token: std::env::var("EE_AGENTD_TOKEN").ok().filter(|s| !s.is_empty()),
            },
            _ => GateMode::Local,
        };

        Self { mode, workspace }
    }

    pub fn evaluate(&self, req: &ToolGateRequest) -> Result<DispatchVerdict, GateError> {
        if !matches_role(&req.role) {
            return Err(GateError::BadRole(req.role.clone()));
        }
        match &self.mode {
            GateMode::Http { url, token } => evaluate_http(url, token.as_deref(), req),
            GateMode::Local => Ok(evaluate_local(&self.workspace, req)),
        }
    }

    /// Whether agentd should run the tool now (no human wait).
    pub fn should_execute(verdict: &DispatchVerdict) -> bool {
        matches!(
            verdict,
            DispatchVerdict::Allow { .. } | DispatchVerdict::Workspace { .. }
        )
    }

    /// Optional confined path for FS tools after a positive verdict.
    pub fn confined_path(verdict: &DispatchVerdict) -> Option<&Path> {
        match verdict {
            DispatchVerdict::Allow {
                confined_path: Some(p),
                ..
            }
            | DispatchVerdict::Ask {
                confined_path: Some(p),
                ..
            }
            | DispatchVerdict::Workspace {
                confined_path: p, ..
            } => Some(p.as_path()),
            _ => None,
        }
    }
}

fn matches_role(role: &str) -> bool {
    matches!(
        role.to_ascii_lowercase().as_str(),
        "admin" | "operator" | "user"
    )
}

/// Tools denied under EE enterprise_safe defaults (feature layer).
fn is_feature_denied(tool: &str) -> bool {
    matches!(
        tool,
        "apply_daemon_update"
            | "self_update"
            | "yolo"
            | "set_policy_yolo"
            | "disable_policy"
            | "raw_shell_as_root"
    )
}

/// High-risk tools that always ask when no full PolicyShim is present.
fn is_ask_default(tool: &str) -> bool {
    tool.starts_with("shell.")
        || tool == "shell"
        || tool == "bash"
        || tool == "exec"
        || tool.starts_with("vast.")
        || tool == "propose_evolution"
}

fn evaluate_local(workspace: &Path, req: &ToolGateRequest) -> DispatchVerdict {
    if is_feature_denied(&req.tool) {
        return DispatchVerdict::Deny {
            reason: format!(
                "enterprise feature deny: '{}' (shim local gate; wire real EE for full policy)",
                req.tool
            ),
            layer: Some("feature".into()),
        };
    }

    if is_ask_default(&req.tool) {
        return DispatchVerdict::Ask {
            reason: Some(format!(
                "enterprise ask for '{}' (role {}) — no EE sidecar; human approval required",
                req.tool, req.role
            )),
            confined_path: None,
        };
    }

    // FS-ish: confine under EE_WORKSPACE / AGENTD_WORKSPACE when a path is present.
    if let Some(ref path) = req.path {
        if path.contains("..") {
            return DispatchVerdict::Deny {
                reason: "path traversal rejected".into(),
                layer: Some("confine".into()),
            };
        }
        let joined = if Path::new(path).is_absolute() {
            PathBuf::from(path)
        } else {
            workspace.join(path)
        };
        // Fail-closed if workspace missing: ask rather than allow outside.
        if !workspace.exists() {
            return DispatchVerdict::Ask {
                reason: Some(format!(
                    "workspace {} missing — cannot confine",
                    workspace.display()
                )),
                confined_path: None,
            };
        }
        return DispatchVerdict::Workspace {
            confined_path: joined,
        };
    }

    // Unknown tool, no path: allow (civilian PolicyEngine still runs when wired
    // after the EE gate for Ask-only paths; Execute here means "EE does not block").
    DispatchVerdict::Allow {
        confined_path: None,
    }
}

fn evaluate_http(
    url: &str,
    token: Option<&str>,
    req: &ToolGateRequest,
) -> Result<DispatchVerdict, GateError> {
    // Cached client — tool gate is on the hot path of every ToolRequested.
    static CLIENT: OnceLock<reqwest::blocking::Client> = OnceLock::new();
    let client = CLIENT.get_or_init(|| {
        reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(5))
            .build()
            .expect("reqwest blocking client")
    });

    let mut builder = client.post(url).json(req);
    if let Some(t) = token {
        builder = builder.bearer_auth(t);
    }

    let resp = builder
        .send()
        .map_err(|e| GateError::Http(e.to_string()))?;

    if !resp.status().is_success() {
        return Err(GateError::Http(format!(
            "status {} from {url}",
            resp.status()
        )));
    }

    // Admin-api may return either a DispatchVerdict directly or the wrapper
    // `{ execute, verdict, confined_path }` from docs/agentd-integration.md.
    let body: serde_json::Value = resp
        .json()
        .map_err(|e| GateError::Http(format!("json: {e}")))?;

    if let Ok(v) = serde_json::from_value::<DispatchVerdict>(body.clone()) {
        return Ok(v);
    }

    // Wrapper form
    let execute = body.get("execute").and_then(|v| v.as_bool());
    let confined = body
        .get("confined_path")
        .and_then(|v| v.as_str())
        .map(PathBuf::from);
    if let Some(inner) = body.get("verdict") {
        if let Ok(v) = serde_json::from_value::<DispatchVerdict>(inner.clone()) {
            return Ok(v);
        }
        // Nested `{ verdict: "workspace", decision: {...}, confined_path }` loose form
        if let Some(tag) = inner.get("verdict").and_then(|v| v.as_str()) {
            return Ok(match tag {
                "allow" => DispatchVerdict::Allow {
                    confined_path: confined.clone().or_else(|| {
                        inner
                            .get("confined_path")
                            .and_then(|v| v.as_str())
                            .map(PathBuf::from)
                    }),
                },
                "workspace" => DispatchVerdict::Workspace {
                    confined_path: confined.or_else(|| {
                        inner
                            .get("confined_path")
                            .and_then(|v| v.as_str())
                            .map(PathBuf::from)
                    }).unwrap_or_else(|| PathBuf::from(".")),
                },
                "ask" => DispatchVerdict::Ask {
                    reason: inner
                        .get("reason")
                        .and_then(|v| v.as_str())
                        .map(str::to_string),
                    confined_path: confined,
                },
                "deny" => DispatchVerdict::Deny {
                    reason: inner
                        .get("reason")
                        .and_then(|v| v.as_str())
                        .unwrap_or("denied")
                        .to_string(),
                    layer: inner
                        .get("layer")
                        .and_then(|v| v.as_str())
                        .map(str::to_string),
                },
                other => DispatchVerdict::Deny {
                    reason: format!("unknown verdict tag from EE: {other}"),
                    layer: Some("policy".into()),
                },
            });
        }
    }

    match execute {
        Some(true) => Ok(DispatchVerdict::Allow {
            confined_path: confined,
        }),
        Some(false) => Ok(DispatchVerdict::Deny {
            reason: body
                .get("reason")
                .and_then(|v| v.as_str())
                .unwrap_or("EE tool-gate refused execute")
                .to_string(),
            layer: Some("policy".into()),
        }),
        None => Err(GateError::Http(
            "unrecognized tool-gate response shape".into(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn deny_self_update() {
        let dir = std::env::temp_dir().join("ee-rs-gate-su");
        let _ = fs::create_dir_all(&dir);
        let gate = AgentdToolGate::enterprise_defaults(&dir);
        let v = gate
            .evaluate(&ToolGateRequest {
                tool: "apply_daemon_update".into(),
                role: "admin".into(),
                path: None,
                agent_id: None,
            })
            .unwrap();
        assert!(!AgentdToolGate::should_execute(&v));
        assert!(matches!(v, DispatchVerdict::Deny { .. }));
    }

    #[test]
    fn allow_read_executes() {
        let dir = std::env::temp_dir().join("ee-rs-gate-rd");
        let _ = fs::create_dir_all(&dir);
        fs::write(dir.join("a.txt"), b"x").unwrap();
        let gate = AgentdToolGate::enterprise_defaults(&dir);
        let v = gate
            .evaluate(&ToolGateRequest {
                tool: "read_file".into(),
                role: "user".into(),
                path: Some("a.txt".into()),
                agent_id: Some("A1".into()),
            })
            .unwrap();
        assert!(AgentdToolGate::should_execute(&v));
        assert!(AgentdToolGate::confined_path(&v).is_some());
    }

    #[test]
    fn bad_role_errors() {
        let gate = AgentdToolGate::enterprise_defaults(std::env::temp_dir());
        let err = gate
            .evaluate(&ToolGateRequest {
                tool: "read_file".into(),
                role: "superuser".into(),
                path: None,
                agent_id: None,
            })
            .unwrap_err();
        assert!(matches!(err, GateError::BadRole(_)));
    }
}
