pub mod config;
pub mod courier;
pub mod mcp;
pub mod plugin_env;
pub mod policy;
pub mod supervisor;
pub mod tool_claim;
pub mod vast;

pub use config::{load, PluginConfig, RestartPolicy};
pub use mcp::{tool_output_json, McpClient};
pub use policy::{
    apply_yolo_grant, requests_yolo_elevation, yolo_grant_present, Decision, PolicyConfig,
    PolicyEngine, PolicyMode, Rule, YOLO_GRANT_KEY,
};
pub use tool_claim::{claim_tool_name, name_owner, ClaimError, NameOwner};
pub use supervisor::{list_peer_ids, mesh_memory_send, seed_evolution_id, Supervisor, SupervisorCmd, ToolProxy};
pub use vast::{VastState, VastInstance, VastPhase, load_recipes, Recipe};
