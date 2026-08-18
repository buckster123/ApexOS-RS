//! Apex Loom v0.1 — Schema & pure engine type sketch
//! For implementation inside agentd (or a small apexos-loom crate).
//! This is illustrative; adjust names/paths to match live code style.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::path::PathBuf;

pub type MachineId = u64; // or newtype matching GoalId / SessionId style

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MachineFile {
    pub spec: String,              // "loom"
    pub spec_version: String,      // "0.1.0"
    pub data: MachineDef,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MachineDef {
    pub name: String,
    pub description: Option<String>,
    pub context: HashMap<String, Value>, // initial values + template seeds
    pub settings: Settings,
    pub states: HashMap<String, StateDef>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settings {
    pub max_steps: u32,
    #[serde(default)]
    pub checkpoint: bool,
    #[serde(default)]
    pub timeout_s: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StateDef {
    #[serde(rename = "type")]
    pub ty: Option<StateType>,     // initial | final
    pub action: Option<ActionKind>,
    pub input: Option<HashMap<String, String>>, // templates
    pub output_to_context: Option<HashMap<String, String>>,
    pub transitions: Vec<Transition>,
    pub execution: Option<Execution>,
    pub on_error: Option<OnError>,
    pub timeout: Option<u64>,
    pub wait_for: Option<String>,  // channel name for wait action
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StateType {
    Initial,
    Final,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ActionKind {
    Prompt,
    Tool,
    Fanout,
    Wait,
    Machine,   // sub-machine
    Hook,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Transition {
    pub condition: Option<String>, // simple expr
    pub to: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Execution {
    #[serde(rename = "type")]
    pub ty: String,                // "retry"
    pub max_attempts: Option<u32>,
    pub backoffs: Option<Vec<u64>>,
    pub jitter: Option<f32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum OnError {
    State(String),
    Map(HashMap<String, String>),
}

/// Runtime instance (persisted)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Instance {
    pub id: MachineId,
    pub def_name: String,
    pub def: MachineDef,           // or Arc + path for large defs
    pub context: Value,
    pub current: String,
    pub step_count: u32,
    pub history: Vec<StepRecord>,
    pub status: InstanceStatus,
    pub bound_session: Option<u64>,
    pub bound_goal: Option<u64>,
    pub waiting_channel: Option<String>,
    pub checkpoint_path: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum InstanceStatus {
    Running,
    Waiting,
    Done,
    Failed { reason: String },
    Parked,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StepRecord {
    pub from: String,
    pub to: String,
    pub action: String,
    pub at: chrono::DateTime<chrono::Utc>, // or Instant serializable
    pub result_summary: Option<String>,
}

/// What the pure engine returns; the driver executes it.
#[derive(Debug, Clone)]
pub enum Action {
    Prompt {
        text: String,
        session: Option<u64>,
    },
    Tool {
        name: String,
        args: Value,
    },
    Fanout {
        tasks: Vec<Value>,         // or structured
        mode: Option<String>,
        batch_deadline_s: Option<u64>,
    },
    Wait {
        channel: String,
        timeout_s: Option<u64>,
    },
    SubMachine {
        def_or_path: String,
        input: Value,
    },
    Hook {
        name: String,
        args: Value,
    },
    Finish {
        output: Value,
    },
}

/// Pure engine surface
pub trait LoomEngine {
    fn load(yaml: &str) -> Result<MachineDef, LoomError>;
    fn load_file(path: &std::path::Path) -> Result<MachineDef, LoomError>;
    fn validate(def: &MachineDef) -> Result<(), LoomError>;

    fn start(def: MachineDef, input: Value) -> Result<Instance, LoomError>;
    fn step(instance: &mut Instance, event: Option<StepEvent>) -> Result<Action, LoomError>;
    fn checkpoint(instance: &Instance) -> Result<(), LoomError>;
    fn restore(path: &std::path::Path) -> Result<Instance, LoomError>;
}

#[derive(Debug)]
pub enum StepEvent {
    TurnComplete { output: String, ok: bool },
    BatchDone { evidence_paths: Vec<String>, timed_out: Vec<u64> },
    Signal { channel: String, payload: Option<Value> },
    Timeout,
    Error { kind: String, message: String },
}

#[derive(Debug, thiserror::Error)]
pub enum LoomError {
    #[error("parse: {0}")]
    Parse(String),
    #[error("validation: {0}")]
    Validation(String),
    #[error("no transition from {0}")]
    NoTransition(String),
    #[error("max steps exceeded")]
    MaxSteps,
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("other: {0}")]
    Other(String),
}

// Condition evaluator (v0.1 — deliberately limited)
pub fn eval_condition(expr: &str, context: &Value, last: Option<&Value>) -> Result<bool, LoomError> {
    // Implement simple path comparisons, contains(), len(), ==, !=, >=, etc.
    // No arbitrary code execution. Document the exact grammar.
    todo!("tiny safe evaluator")
}

// Template renderer (v0.1)
pub fn render_template(tmpl: &str, context: &Value, extra: Option<&Value>) -> Result<String, LoomError> {
    // Support {{ context.foo }}, {{ output }}, basic filters later.
    todo!("minimal {{ }} interpolator")
}
