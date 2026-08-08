//! In-process hook API shaped for ApexOS-RS agentd integration.
//!
//! Drop this into the tools/supervisor chokepoint **in front of** the civilian
//! `PolicyEngine::check` for EE builds:
//!
//! ```ignore
//! use apexos_ee_dispatch::agentd_hook::{evaluate_tool_global, ToolHookInput, ToolHookResult};
//!
//! match evaluate_tool_global(&ToolHookInput {
//!     tool: tool_name.into(),
//!     role: role_str.into(),
//!     path: path_arg,
//!     agent_id: Some(agent_id),
//! }) {
//!     ToolHookResult::Execute { confined_path } => { /* run with confined_path */ }
//!     ToolHookResult::Ask { reason } => { /* approval queue */ }
//!     ToolHookResult::Deny { reason, layer } => { /* return error to model */ }
//! }
//! ```
//!
//! See `docs/enterprise.md`. Real ApexOS-Enterprise ships the same module path.

use crate::{AgentdToolGate, DispatchVerdict, GateError, ToolGateRequest};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::OnceLock;

/// Global gate for long-lived agentd processes (lazy `from_env`).
static GLOBAL_GATE: OnceLock<AgentdToolGate> = OnceLock::new();

/// Install the process-global gate (call once at agentd boot).
///
/// `OnceLock::set` is used: a second call is ignored (boot is single-shot).
pub fn init_global_gate(gate: AgentdToolGate) {
    let _ = GLOBAL_GATE.set(gate);
}

/// Get the global gate, initializing from env if needed.
pub fn global_gate() -> &'static AgentdToolGate {
    GLOBAL_GATE.get_or_init(AgentdToolGate::from_env)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolHookInput {
    pub tool: String,
    pub role: String,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub agent_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum ToolHookResult {
    /// Proceed with tool execution.
    Execute {
        #[serde(skip_serializing_if = "Option::is_none")]
        confined_path: Option<PathBuf>,
    },
    /// Hold for human approval.
    Ask { reason: String },
    /// Hard block.
    Deny {
        reason: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        layer: Option<String>,
    },
}

/// Evaluate using an explicit gate instance.
pub fn evaluate_tool(gate: &AgentdToolGate, input: &ToolHookInput) -> ToolHookResult {
    let req = ToolGateRequest {
        tool: input.tool.clone(),
        role: input.role.clone(),
        path: input.path.clone(),
        agent_id: input.agent_id.clone(),
    };
    match gate.evaluate(&req) {
        Ok(v) => verdict_to_hook(v),
        Err(GateError::BadRole(r)) => ToolHookResult::Deny {
            reason: format!("unknown role: {r}"),
            layer: Some("policy".into()),
        },
        Err(GateError::Http(e)) => ToolHookResult::Deny {
            // Fail closed when the sidecar is unreachable — never silent allow.
            reason: format!("enterprise tool-gate unavailable: {e}"),
            layer: Some("policy".into()),
        },
    }
}

/// Evaluate using the process-global gate (`EE_WORKSPACE` / sidecar URL).
pub fn evaluate_tool_global(input: &ToolHookInput) -> ToolHookResult {
    evaluate_tool(global_gate(), input)
}

fn verdict_to_hook(v: DispatchVerdict) -> ToolHookResult {
    match v {
        DispatchVerdict::Allow { confined_path } => ToolHookResult::Execute { confined_path },
        DispatchVerdict::Workspace { confined_path } => ToolHookResult::Execute {
            confined_path: Some(confined_path),
        },
        DispatchVerdict::Ask { reason, .. } => ToolHookResult::Ask {
            reason: reason.unwrap_or_else(|| "enterprise policy ask".into()),
        },
        DispatchVerdict::Deny { reason, layer } => ToolHookResult::Deny { reason, layer },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn execute_read() {
        let dir = std::env::temp_dir().join("ee-rs-hook-rd");
        let _ = fs::create_dir_all(&dir);
        fs::write(dir.join("f.txt"), b"ok").unwrap();
        let gate = AgentdToolGate::enterprise_defaults(&dir);
        let r = evaluate_tool(
            &gate,
            &ToolHookInput {
                tool: "read_file".into(),
                role: "user".into(),
                path: Some("f.txt".into()),
                agent_id: None,
            },
        );
        assert!(matches!(r, ToolHookResult::Execute { .. }));
    }

    #[test]
    fn deny_self_update() {
        let gate = AgentdToolGate::enterprise_defaults(std::env::temp_dir());
        let r = evaluate_tool(
            &gate,
            &ToolHookInput {
                tool: "apply_daemon_update".into(),
                role: "admin".into(),
                path: None,
                agent_id: None,
            },
        );
        assert!(matches!(r, ToolHookResult::Deny { .. }));
    }
}
