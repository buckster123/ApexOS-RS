use std::collections::HashMap;
use std::fmt;
use serde::{Deserialize, Serialize};

// ── ID newtypes (cheap, copyable, type-safe) ───────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SessionId(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ActionId(pub u64);

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PluginId(pub String);

impl fmt::Display for PluginId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { f.write_str(&self.0) }
}

// ── Evolution types ──────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct EvolutionId(pub u64);

/// Policy mode — lives here so EvolutionProposal (also in core) can reference
/// it without a circular dep. plugins::policy imports this via apexos_core.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum PolicyMode {
    #[default]
    Suggest,
    AutoEdit,
    Yolo,
}

/// Per-tool approval rule — the value side of the `[rules]` table in policy.toml.
/// Lives here so `EvolutionProposal::UpdatePolicyRule` can reference it without a
/// circular dep. `plugins::policy::Rule` mirrors these variants 1:1.
///
/// NOTE: this is distinct from [`PolicyMode`] (the global mode). The `[rules]`
/// table accepts `allow`/`ask`/`workspace`, NOT the mode names — conflating the
/// two corrupts policy.toml on reload.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PolicyRule {
    /// Auto-approve regardless of mode (overridden by yolo).
    Allow,
    /// Always ask (overridden by yolo).
    Ask,
    /// Auto if path is inside the workspace, else ask.
    Workspace,
}

impl PolicyRule {
    /// The exact string written into the `[rules]` table of policy.toml.
    pub fn as_toml_str(self) -> &'static str {
        match self {
            PolicyRule::Allow     => "allow",
            PolicyRule::Ask       => "ask",
            PolicyRule::Workspace => "workspace",
        }
    }

    /// Parse from a policy.toml rule value. Returns None for unknown strings.
    pub fn from_toml_str(s: &str) -> Option<Self> {
        match s {
            "allow"     => Some(PolicyRule::Allow),
            "ask"       => Some(PolicyRule::Ask),
            "workspace" => Some(PolicyRule::Workspace),
            _           => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Subsystem {
    Plugins,
    Policy,
    Agent,
    Gateway,
}

/// Discrete, auditable change proposals. Each variant maps to exactly one
/// config artifact and one hot-reload action.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EvolutionProposal {
    RegisterMcpServer {
        name:    String,
        command: String,
        env:     HashMap<String, String>,
        reason:  String,
    },
    UnregisterMcpServer {
        name:   String,
        reason: String,
    },
    UpdatePolicyRule {
        tool_pattern: String,
        /// Per-tool rule (`allow`/`ask`/`workspace`) — NOT a [`PolicyMode`].
        new_rule:     PolicyRule,
        reason:       String,
    },
    /// Full replacement content for /etc/agentd/soul.md (not a diff — full
    /// content makes rollback trivial: snapshot pre-patch, restore on demand).
    UpdateSystemPrompt {
        content: String,
        reason:  String,
    },
    HotReloadSubsystem {
        subsystem: Subsystem,
    },
}

// ── Sensor types ─────────────────────────────────────────────────────────────

/// A reading from one sensor. The `kind` field is the serde discriminant tag.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SensorReading {
    Temperature { celsius: f32, sensor_id: String },
    Humidity    { percent: f32, sensor_id: String },
    Pressure    { hpa: f32,     sensor_id: String },
    Motion      { detected: bool, sensor_id: String },
    Distance    { cm: f32,      sensor_id: String },
    GpioLevel   { pin: u8, high: bool },
    /// BME688 BSEC2 air quality bundle (IAQ, CO₂ eq, VOC eq + T/RH/P)
    AirQuality {
        iaq:          f32,
        co2_eq_ppm:   f32,
        voc_ppm:      f32,
        accuracy:     u8,
        temperature_c: f32,
        humidity_pct:  f32,
        pressure_hpa:  f32,
        sensor_id:    String,
    },
    /// MLX90640 32×24 thermal frame summary (no raw array — keep events small)
    ThermalFrame {
        min_c:      f32,
        max_c:      f32,
        mean_c:     f32,
        sensor_id:  String,
    },
}

// ── Council types ────────────────────────────────────────────────────────────

/// One participant in a council session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CouncilAgentDef {
    pub id:      String,
    pub persona: String,
    pub backend: Option<String>,  // "anthropic" | "ollama" | ... — inherits system default if None
    pub model:   Option<String>,
    pub color:   Option<String>,  // hex for UI
}

// ── The central event enum ──────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    // ── from frontends (intents) ──────────────────────────
    UserPrompt   { session: SessionId, text: String },
    UserApproval { session: SessionId, action: ActionId, granted: bool },
    UserCancel   { session: SessionId },

    // ── from the agent loop ───────────────────────────────
    AgentText     { session: SessionId, delta: String },
    AgentThinking { session: SessionId, delta: String },
    ToolRequested { session: SessionId, call: ToolCall },
    TurnComplete  { session: SessionId },

    // ── from the plugin supervisor ────────────────────────
    ToolResult { session: SessionId, call: ActionId, output: ToolOutput },
    PluginUp   { plugin: PluginId, tools: Vec<ToolSpec> },
    PluginDown { plugin: PluginId, reason: String },

    // ── from the policy engine ────────────────────────────
    ApprovalPending { session: SessionId, call: ToolCall },

    // ── sub-agent routing ─────────────────────────────────
    /// Emitted by the supervisor when agent.spawn is dispatched.
    /// The async router catches this and creates a child run_turn.
    SpawnAgent {
        parent:  SessionId,
        call_id: ActionId,
        prompt:  String,
        system:  Option<String>,
    },
    /// Emitted immediately after child session is created so the UI can
    /// open a new agent window for the child.
    SubAgentStarted {
        parent: SessionId,
        child:  SessionId,
        prompt: String,
    },

    // ── sensor bridge ─────────────────────────────────────
    /// Emitted by the /sensor-bridge WS handler when a body-pi node sends data.
    SensorReading { node_id: String, reading: SensorReading, timestamp: u64 },

    // ── voice / wake word ─────────────────────────────────
    /// Emitted by gateway after piper ding plays; frontend auto-records + submits.
    WakeTriggered,

    // ── agent-to-agent messaging ───────────────────────────
    /// Emitted by send_to_agent virtual tool; agent router injects as UserPrompt
    /// into the target session and then emits AgentMessageAck.
    AgentMessage    { from: SessionId, to: SessionId, body: String, msg_id: u64 },
    AgentMessageAck { msg_id: u64, from: SessionId },

    // ── system ────────────────────────────────────────────
    // council
    CouncilStarted    { council_id: String, topic: String, agents: Vec<CouncilAgentDef> },
    CouncilRoundStart { council_id: String, round: u32 },
    CouncilAgentDelta { council_id: String, round: u32, agent_id: String, delta: String },
    CouncilAgentDone  { council_id: String, round: u32, agent_id: String, full_text: String },
    CouncilRoundDone  { council_id: String, round: u32, convergence: f32, agreements: Vec<String> },
    /// reason = "consensus" | "max_rounds" | "stopped"
    CouncilComplete   { council_id: String, rounds: u32, reason: String, synthesis: String },
    CouncilButtIn     { council_id: String, message: String },

    Error { session: Option<SessionId>, message: String },

    // ── vast.ai inference ─────────────────────────────────
    /// Emitted when a Vast instance is created (before model is loaded).
    VastInstanceLaunched  { instance_id: String, recipe: String, cost_per_hr: f64 },
    /// Emitted when the SSH tunnel is up and model health check passes.
    /// main.rs catches this to hot-swap the OaiProvider backend.
    VastInstanceReady     { instance_id: String, local_port: u16 },
    /// Emitted after destroy completes; main.rs reverts backend.
    VastInstanceDestroyed { instance_id: String },
    /// Emitted by keepalive task after 3 consecutive health failures.
    VastTunnelLost        { instance_id: String },

    // ── mesh ──────────────────────────────────────────────
    /// A new _apexos._tcp node seen via mDNS that isn't in peers.toml yet.
    PeerSeen       { node_id: String, ip: String },
    /// A peer was successfully added to peers.toml (bootstrap complete or manual add).
    PeerRegistered { node_id: String, ws_url: String, role: String },
    /// A known peer stopped advertising (3 missed mDNS polls).
    PeerLost       { node_id: String },

    // self-evolution
    /// Agent has proposed a structural change. Routes through the policy engine
    /// under the `evolution.*` rule namespace (default: suggest -> ask user).
    EvolutionProposed {
        id:          EvolutionId,
        proposal:    EvolutionProposal,
        proposed_by: SessionId,
    },
    /// An EvolutionProposed was approved and applied.
    EvolutionApplied {
        id:            EvolutionId,
        proposal:      EvolutionProposal,
        patch_summary: String,
        applied_by:    Option<SessionId>,
    },
    /// A previously applied evolution was rolled back.
    EvolutionRolledBack {
        evolution_id:   EvolutionId,
        reason:         String,
        rolled_back_by: Option<SessionId>,
    },
}

// ── Tool call / result ────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id:   ActionId,
    pub tool: String,
    pub args: serde_json::Value,
    /// Set by the policy engine, not the agent.
    pub needs_approval: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolOutput {
    pub ok:      bool,
    pub content: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name:         String,
    pub description:  String,
    pub input_schema: serde_json::Value,
}

// ── Agent context — every session is one of these ─────────────────────────
//
// parent == None     -> root session, output streams to a frontend
// parent == Some(id) -> child session, TurnComplete -> ToolResult to parent

#[derive(Debug, Clone)]
pub struct AgentContext {
    pub id:      SessionId,
    pub parent:  Option<SessionId>,
    pub history: Vec<Message>,
    pub spawned: Vec<SessionId>,
}

impl AgentContext {
    pub fn root(id: SessionId) -> Self {
        Self { id, parent: None, history: Vec::new(), spawned: Vec::new() }
    }
    pub fn child(id: SessionId, parent: SessionId) -> Self {
        Self { id, parent: Some(parent), history: Vec::new(), spawned: Vec::new() }
    }
    pub fn is_root(&self) -> bool { self.parent.is_none() }
}

// ── Conversation message (maps to the Anthropic messages API) ──────────────
//
// Assistant MUST carry thinking blocks alongside text/tool_use — they must be
// replayed across tool round-trips or the API rejects the continuation.

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "snake_case")]
pub enum Message {
    User      { content: Vec<ContentBlock> },
    Assistant { content: Vec<ContentBlock> },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text     { text: String },
    Thinking { thinking: String, signature: String },
    ToolUse  { id: String, name: String, input: serde_json::Value },
    ToolResult { tool_use_id: String, content: serde_json::Value, is_error: bool },
}
