//! # apexos-protocol
//!
//! The wire contract shared across the ApexOS-RS workspace: the `Event` enum and
//! every type that crosses the agentd WebSocket / a2a boundary (IDs, `ToolCall`,
//! `ContentBlock`, `SensorReading`, `EvolutionProposal`, …).
//!
//! Extracted from `apexos-core` so the Slint UI (and any other frontend) can
//! **deserialize into the same types agentd serializes from** — protocol drift
//! becomes a compile/deserialize error instead of a silently-dropped frame.
//! Deliberately lean: `serde` + `serde_json` only, no `tokio`/`image`/runtime
//! deps, so a frontend pays nothing to depend on it.
//!
//! `apexos-core` re-exports this crate (`pub use apexos_protocol as types;` plus a
//! glob), so every existing `apexos_core::Event` / `apexos_core::types::Event`
//! path keeps resolving unchanged.

#![cfg_attr(not(feature = "std"), no_std)]

#[cfg(not(feature = "std"))]
extern crate alloc;

use core::fmt;
use serde::{Deserialize, Serialize};

#[cfg(not(feature = "std"))]
use alloc::{collections::BTreeMap, string::String, vec::Vec};
#[cfg(feature = "std")]
use std::collections::HashMap;

/// Map type for protocol fields: `HashMap` under `std` (unchanged behavior),
/// `BTreeMap` under `no_std + alloc`. Serializes to an identical JSON object
/// either way — JSON objects are unordered; `tests/wire_compat.rs` locks this.
/// Keys must be `Ord` for the `no_std` side (protocol keys are `String`s).
#[cfg(feature = "std")]
pub type Map<K, V> = HashMap<K, V>;
#[cfg(not(feature = "std"))]
pub type Map<K, V> = BTreeMap<K, V>;

// ── ID newtypes (cheap, copyable, type-safe) ───────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct SessionId(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ActionId(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct GoalId(pub u64);

/// Lifecycle state of an autonomous Goal run (docs/ideas/goal-driver-design.md).
/// P2a uses Acting / Done / Failed; the rest are reserved for later slices.
/// `Cancelled` is terminal-by-operator (goal_cancel) — distinct from Failed (a
/// stall/error) and not resumable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalState {
    Planning,
    Acting,
    Blocked,
    Reflecting,
    Done,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct PluginId(pub String);

impl fmt::Display for PluginId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { f.write_str(&self.0) }
}

// ── Evolution types ──────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
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
        env:     Map<String, String>,
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
    /// File a hardware request — the "request-to-incarnate" (EDK, docs/edk.md). The ONE
    /// evolution that cannot auto-apply: agentd records the request to the hardware
    /// wishlist, but a human must physically seat the part. The "apply confirmation" is
    /// the next-boot embodiment probe seeing the new device flip a sense ✗→✓.
    RequestHardware {
        /// Part id from config/parts/inventory.toml, or a product name for a buyable part.
        part:       String,
        /// What capability it grants, in agent terms ("eyes", "hearing").
        capability: String,
        /// Why it's needed now (the rationale).
        reason:     String,
        /// How/where it attaches ("csi port", "m.2-hat+"); "" if unknown.
        #[serde(default)]
        bus:        String,
        /// Provenance: "inventory:<id>" (on hand) or a URL / where it was found (buyable).
        #[serde(default)]
        source:     String,
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
    UserPrompt   { session: SessionId, text: String, #[serde(default)] images: Vec<ImageSource> },
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
    /// main.rs catches this to hot-swap the OaiProvider backend. `model` is the
    /// served model id (the recipe's model_repo) so the daemon swaps BOTH the
    /// endpoint AND the model id — an OAI-compat server rejects a turn whose model
    /// it doesn't serve, which is why leaving the Anthropic id in place broke
    /// every post-swap turn.
    VastInstanceReady     { instance_id: String, local_port: u16, model: String },
    /// Emitted after destroy completes; main.rs reverts backend.
    VastInstanceDestroyed { instance_id: String },
    /// Emitted by keepalive task after 3 consecutive health failures.
    VastTunnelLost        { instance_id: String },

    // ── mesh ──────────────────────────────────────────────
    /// A cross-node a2a message arrived from a mesh peer and was injected into a
    /// session on this node. Session-LESS in `event_session` (the `session` field
    /// is informational, not a scope), so the gateway broadcasts it to EVERY
    /// client as a global notification — a user watching any session sees that
    /// mesh traffic landed (the conversation stream itself stays scoped to
    /// `session`). `from_node` = the sending peer's node_id; `session` = where it
    /// landed (the peer's own thread); `preview` = a short body excerpt.
    MeshMessage    { from_node: String, session: SessionId, preview: String },
    /// A memory arrived from a mesh peer over the federation relay and was
    /// imported into this node's Cerebro with stamped provenance tags
    /// (colony-federation Slice 1). Session-less/global in `event_session` —
    /// every client sees that knowledge landed. `memory_id` = the id in THIS
    /// node's store (a provenance-stamped copy, not the sender's id).
    MeshMemoryShared { from_node: String, memory_id: String, preview: String },
    /// A new _apexos._tcp node seen via mDNS that isn't in peers.toml yet.
    PeerSeen       { node_id: String, ip: String },
    /// A peer was successfully added to peers.toml (bootstrap complete or manual add).
    PeerRegistered { node_id: String, ws_url: String, role: String },
    /// A known peer stopped advertising (3 missed mDNS polls).
    PeerLost       { node_id: String },
    /// Active-liveness transition from the downtime beacon (colony-mesh spine): a
    /// registered peer crossed the up↔down boundary as measured by periodic HTTP
    /// heartbeat polls — distinct from `PeerLost` (mDNS *advertising* loss). Global
    /// status event → every client gets the board notification. `status` = "dark"
    /// (went silent) | "alive" (recovered); `last_seen_secs` = seconds since the
    /// last successful contact (0 on recovery).
    MeshNodeStatus { node_id: String, status: String, last_seen_secs: u64 },

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

    // autonomous goals (docs/ideas/goal-driver-design.md, Phase 2)
    /// A Goal run advanced (created / step / done / failed). GLOBAL (session-less
    /// in `event_session`) so every client's Work Board sees it, even though the
    /// goal's own turns run in a dedicated, session-scoped stream.
    GoalStateChanged {
        goal:      GoalId,
        objective: String,
        state:     GoalState,
        step:      u32,
        max_steps: u32,
        /// Short context for the current state — the block reason, the stall note,
        /// "" otherwise. Surfaced on the board card. (P2c)
        detail:    String,
        /// Goal-scoped yolo: this goal auto-approves its own `ask` tools. The board
        /// renders a distinct AUTO marker. `#[serde(default)]` so a version-skewed UI
        /// (or an older event) reads it as false. (P2e, goal-driver-design.md #3)
        #[serde(default)]
        yolo:      bool,
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
    /// A user-attached image, already shimmed through `vision::prepare_*`
    /// (decoded → downscaled ≤ `VISION_MAX_EDGE` → re-encoded → base64). `data`
    /// is that base64; `media_type` is `image/jpeg` or `image/png`. Providers
    /// render it natively (Anthropic `image` block / OpenAI `image_url`).
    Image    { media_type: String, data: String },
}

/// A prepared image riding on an inbound [`Event::UserPrompt`]. Same shape as the
/// `image` content the providers emit — the gateway runs raw uploads through the
/// vision shim before constructing the event, so this is always downscaled b64.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageSource {
    pub media_type: String,
    pub data: String,
}

// ── tests ─────────────────────────────────────────────────────────────────────
// Lock the wire contract the frontends deserialize against. The gateway sends
// `serde_json::to_string(&event)` with no reshaping, so these strings are exactly
// what a frontend receives. A field/variant rename that would break the typed UI
// dispatch fails here instead of silently dropping a frame at runtime.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_serialize_as_bare_numbers() {
        // Historical UI footgun: the ID newtypes serialize as bare numbers, not
        // `{"0": n}` or strings — the UI must read them as numbers.
        assert_eq!(serde_json::to_string(&SessionId(42)).unwrap(), "42");
        assert_eq!(serde_json::to_string(&ActionId(5)).unwrap(), "5");
    }

    #[test]
    fn agent_text_round_trips() {
        let j = r#"{"type":"agent_text","session":42,"delta":"hi"}"#;
        match serde_json::from_str::<Event>(j).unwrap() {
            Event::AgentText { session, delta } => {
                assert_eq!(session, SessionId(42));
                assert_eq!(delta, "hi");
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn tool_requested_nests_under_call_with_bare_id() {
        // The UI reads call.tool / call.id / call.args; id is a bare number.
        let j = r#"{"type":"tool_requested","session":1,
            "call":{"id":7,"tool":"read_file","args":{"path":"x"},"needs_approval":false}}"#;
        match serde_json::from_str::<Event>(j).unwrap() {
            Event::ToolRequested { call, .. } => {
                assert_eq!(call.id, ActionId(7));
                assert_eq!(call.tool, "read_file");
                assert_eq!(call.args["path"], "x");
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn tool_result_call_is_a_bare_action_id() {
        let j = r#"{"type":"tool_result","session":1,"call":7,"output":{"ok":true,"content":"done"}}"#;
        match serde_json::from_str::<Event>(j).unwrap() {
            Event::ToolResult { call, output, .. } => {
                assert_eq!(call, ActionId(7));
                assert!(output.ok);
                assert_eq!(output.content, "done");
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn sensor_reading_carries_a_typed_inner_enum() {
        let j = r#"{"type":"sensor_reading","node_id":"pi","timestamp":0,
            "reading":{"kind":"air_quality","iaq":50.0,"co2_eq_ppm":400.0,"voc_ppm":0.5,
                       "accuracy":3,"temperature_c":22.0,"humidity_pct":40.0,
                       "pressure_hpa":1013.0,"sensor_id":"bme688"}}"#;
        match serde_json::from_str::<Event>(j).unwrap() {
            Event::SensorReading { reading: SensorReading::AirQuality { accuracy, iaq, humidity_pct, .. }, .. } => {
                assert_eq!(accuracy, 3);
                assert!((iaq - 50.0).abs() < 0.01);
                assert!((humidity_pct - 40.0).abs() < 0.01);
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn unit_variant_and_unknown_fields_tolerated() {
        // WakeTriggered is a unit variant: {"type":"wake_triggered"}.
        assert!(matches!(
            serde_json::from_str::<Event>(r#"{"type":"wake_triggered"}"#).unwrap(),
            Event::WakeTriggered
        ));
        // Unknown/extra fields are ignored (forward-compatible).
        let j = r#"{"type":"turn_complete","session":3,"extra":"ignored"}"#;
        assert!(matches!(
            serde_json::from_str::<Event>(j).unwrap(),
            Event::TurnComplete { .. }
        ));
    }
}
