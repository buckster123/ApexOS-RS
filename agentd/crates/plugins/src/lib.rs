pub mod config;
pub mod mcp;
pub mod policy;
pub mod supervisor;
pub mod vast;

pub use config::{load, PluginConfig, RestartPolicy};
pub use mcp::{tool_output_json, McpClient};
pub use policy::{Decision, PolicyConfig, PolicyEngine, PolicyMode, Rule};
pub use supervisor::{seed_evolution_id, Supervisor, SupervisorCmd, ToolProxy};
pub use vast::{VastState, VastInstance, VastPhase, load_recipes, Recipe};
