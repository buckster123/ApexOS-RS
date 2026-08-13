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

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct WorkerId(pub u64);

// ── Session-id classes (wire-relevant: ids travel every frame) ──────────────
// The id space is a strict three-way partition, enforced daemon-side (boot
// seeding, persistence, hydration — see agentd) and USED by frontends (e.g.
// grouping worker sessions on the board):
//
//   normal < 1<<62 ≤ worker < 1<<63 ≤ spawn

/// Session ids in `[WORKER_SESSION_BASE, SPAWN_SESSION_BASE)` are persistent
/// WORKER sessions — `task_fanout` children (Fabrica W tier). Unlike spawns
/// they persist (`sessions/<id>.jsonl` is a parked worker's truth).
pub const WORKER_SESSION_BASE: u64 = 1 << 62;

/// Session ids at/above this base are EPHEMERAL SPAWN sessions — local
/// sub-agents and cross-node spawn children. Never persisted; spawn
/// provenance is stamped onto their memory writes (H6).
pub const SPAWN_SESSION_BASE: u64 = 1 << 63;

/// Bounded range check — deliberately not `>=`: the spawn range sits above
/// and must stay disjoint (never both predicates true for one id).
pub fn is_worker_session(session_id: u64) -> bool {
    (WORKER_SESSION_BASE..SPAWN_SESSION_BASE).contains(&session_id)
}

/// Whether `session_id` is an ephemeral spawn session (see [`SPAWN_SESSION_BASE`]).
pub fn is_spawn_session(session_id: u64) -> bool {
    session_id >= SPAWN_SESSION_BASE
}

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

/// Lifecycle state of a fanned-out Worker (docs/fabrica.md, W tier). The full
/// vocabulary ships at once so `workers.json` never needs a schema migration;
/// W1a activates Queued / Running / Blocked / Parked / Done / Failed — `Idle`
/// (the `yield` verdict) and `Cancelled` (cancel cascade) arm in W1b/W1d.
/// `Parked` = evicted from memory, `sessions/<id>.jsonl` is truth (revive-on-send
/// is the only Parked→Running edge, PB-3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerState {
    Queued,
    Running,
    Idle,
    Parked,
    Blocked,
    Done,
    Failed,
    Cancelled,
}

/// One worker's line in a batch report (`TaskBatchDone`) — a POINTER to
/// evidence, never the payload: the conductor reads the file, the summary
/// string is not the deliverable (the evidence rule, docs/fabrica.md W1c).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BatchWorkerRow {
    pub worker: WorkerId,
    pub state:  WorkerState,
    /// Evidence file path (`<log_dir>/agents/<worker_id>.json`); "" for a
    /// straggler that never reached a terminal state.
    pub evidence: String,
    /// True when the batch deadline fired before this worker went terminal —
    /// it is still revivable; a later revive finishes it outside the report.
    #[serde(default)]
    pub timed_out: bool,
    /// The peer node hosting this worker (W2 mesh workers); `None` = local.
    /// For a remote row, `evidence` is the conductor-side MIRROR file and the
    /// worker id is the conductor's local row id, not the peer's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node: Option<String>,
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
    UserApproval {
        session: SessionId,
        action: ActionId,
        granted: bool,
        /// Capability minted on `ApprovalPending`. Zero = missing (pre-nonce
        /// client). The supervisor rejects a zero or mismatched nonce.
        #[serde(default)]
        nonce: u64,
    },
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
    ApprovalPending {
        session: SessionId,
        call: ToolCall,
        /// Random capability the client must echo on `UserApproval`.
        #[serde(default)]
        nonce: u64,
    },

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
    /// A threshold-crossing that SURVIVED the persistence filter + cooldown —
    /// the daemon's considered "this is real" signal, fired once per sustained
    /// event (the raw stream above fires every few seconds). GLOBAL in the
    /// gateway's `event_session` (falls to the every-client default), so any
    /// frontend can react ambiently — it is also a `ui_reflex` trigger
    /// ("sensor_alert → open sensor"). The same alert is injected into the
    /// root session as a prompt for the agent; this is the machine-readable
    /// twin. `kind` = the stable alert-key suffix: `cpu_temp` | `motion` |
    /// `air_quality` | `thermal_hotspot`. Motion (instantaneous, no
    /// threshold) carries value 1.0 / threshold 0.0.
    SensorAlert {
        node_id:   String,
        kind:      String,
        value:     f32,
        threshold: f32,
        sensor_id: String,
    },

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
        /// The goal's own dedicated session — lets the worker driver cascade-cancel
        /// a cancelled conductor's batch (W1d). `None` on legacy frames.
        #[serde(default)]
        session:   Option<SessionId>,
    },

    // worker tier (docs/fabrica.md, W1a)
    /// A batch reached its report point: every worker terminal, or the batch
    /// deadline fired with stragglers marked `timed_out` (still revivable).
    /// Rows are POINTERS — evidence paths, never payloads (the evidence rule):
    /// integration must actually read the artifacts. GLOBAL, like the worker
    /// lane. (W1c)
    TaskBatchDone {
        batch:  u64,
        /// The conductor session that fanned the batch out.
        parent: SessionId,
        rows:   Vec<BatchWorkerRow>,
    },

    /// A fanned-out Worker changed state. `GoalStateChanged`'s twin: GLOBAL
    /// (session-less in `event_session`) so every client's board sees the worker
    /// lane, even though each worker's own turns run session-scoped.
    WorkerStateChanged {
        worker:  WorkerId,
        /// The batch this worker belongs to (one `task_fanout` call = one batch).
        batch:   u64,
        /// The conductor session that fanned this worker out.
        parent:  SessionId,
        /// The worker's own dedicated session (WORKER_SESSION_BASE range, persisted).
        session: SessionId,
        /// The task, truncated for card rendering — never the full carry.
        task:    String,
        state:   WorkerState,
        /// Short context for the current state — queue position, block reason,
        /// stall note, "" otherwise.
        detail:  String,
        /// Batch-inherited yolo (`task_fanout{yolo:"inherit"}`, W1d): this worker
        /// auto-approves its own ask tools. AUTO marker on the board card.
        #[serde(default)]
        yolo:    bool,
        /// The peer node hosting this worker (W2 mesh workers); `None` = local.
        /// Remote rows carry `session: SessionId(0)` as a sentinel — the worker's
        /// real session lives on the peer, and nothing on this node may key
        /// residency off it (the router's eviction guard checks the range).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        node:    Option<String>,
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
        assert_eq!(serde_json::to_string(&WorkerId(9)).unwrap(), "9");
    }

    #[test]
    fn worker_state_changed_round_trips_snake_case() {
        // The worker lane's wire contract (W1a): snake_case tag + states, bare ids.
        let j = r#"{"type":"worker_state_changed","worker":3,"batch":1,"parent":7,
            "session":4611686018427387904,"task":"write the tests","state":"running","detail":""}"#;
        match serde_json::from_str::<Event>(j).unwrap() {
            Event::WorkerStateChanged { worker, batch, parent, session, task, state, detail, yolo, node } => {
                assert_eq!(worker, WorkerId(3));
                assert_eq!(batch, 1);
                assert_eq!(parent, SessionId(7));
                assert_eq!(session, SessionId(1 << 62));
                assert_eq!(task, "write the tests");
                assert_eq!(state, WorkerState::Running);
                assert_eq!(detail, "");
                assert!(!yolo, "missing yolo reads false (W1a-era frame)");
                assert!(node.is_none(), "missing node reads None (pre-W2 frame)");
            }
            other => panic!("wrong variant: {other:?}"),
        }
        // Every WorkerState variant survives a round trip as snake_case.
        for s in [WorkerState::Queued, WorkerState::Running, WorkerState::Idle, WorkerState::Parked,
                  WorkerState::Blocked, WorkerState::Done, WorkerState::Failed, WorkerState::Cancelled] {
            let enc = serde_json::to_string(&s).unwrap();
            assert_eq!(enc, enc.to_lowercase(), "snake_case wire form: {enc}");
            assert_eq!(serde_json::from_str::<WorkerState>(&enc).unwrap(), s);
        }
    }

    #[test]
    fn worker_node_fields_are_additive_and_absent_when_local(){
        // W2 mesh workers: `node` is #[serde(default)] + skip-when-None, so a
        // local row's wire bytes are IDENTICAL to the pre-W2 shape (no `node`
        // key at all), and a pre-W2 decoder reading a remote frame just sees an
        // extra field it ignores (deny_unknown_fields is deliberately absent).
        let local = BatchWorkerRow {
            worker: WorkerId(1), state: WorkerState::Done,
            evidence: "e".into(), timed_out: false, node: None,
        };
        let enc = serde_json::to_string(&local).unwrap();
        assert!(!enc.contains("node"), "local rows must not grow a node key: {enc}");
        // A pre-W2 row (no node key) decodes with node = None.
        let old: BatchWorkerRow =
            serde_json::from_str(r#"{"worker":2,"state":"failed","evidence":""}"#).unwrap();
        assert!(old.node.is_none());
        assert!(!old.timed_out);
        // A remote row round-trips its node.
        let remote = BatchWorkerRow { node: Some("apex-3".into()), ..local };
        let back: BatchWorkerRow = serde_json::from_str(&serde_json::to_string(&remote).unwrap()).unwrap();
        assert_eq!(back.node.as_deref(), Some("apex-3"));
        // WorkerStateChanged: a remote card carries node + the session-0 sentinel;
        // local emissions stay byte-free of the node key.
        let ev = Event::WorkerStateChanged {
            worker: WorkerId(9), batch: 2, parent: SessionId(7), session: SessionId(0),
            task: "t".into(), state: WorkerState::Running, detail: "".into(),
            yolo: false, node: Some("apex-3".into()),
        };
        let enc = serde_json::to_string(&ev).unwrap();
        assert!(enc.contains(r#""node":"apex-3""#));
        match serde_json::from_str::<Event>(&enc).unwrap() {
            Event::WorkerStateChanged { node, session, .. } => {
                assert_eq!(node.as_deref(), Some("apex-3"));
                assert_eq!(session, SessionId(0), "remote rows ride the sentinel session");
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn session_id_space_is_a_three_way_partition() {
        // normal < 1<<62 ≤ worker < 1<<63 ≤ spawn — the ranges never overlap.
        for id in [0u64, 1, 42, WORKER_SESSION_BASE - 1] {
            assert!(!is_worker_session(id), "{id} must be normal");
            assert!(!is_spawn_session(id), "{id} must be normal");
        }
        // Worker range: persisted, NOT spawn (a worker id that also read as spawn
        // would silently skip JSONL persistence — the exact trap the bounded
        // range check exists to prevent).
        for id in [WORKER_SESSION_BASE, WORKER_SESSION_BASE + 1, SPAWN_SESSION_BASE - 1] {
            assert!(is_worker_session(id), "{id} must be worker");
            assert!(!is_spawn_session(id), "{id} must not be spawn");
        }
        // Spawn range: ephemeral, NOT worker.
        for id in [SPAWN_SESSION_BASE, SPAWN_SESSION_BASE + 1, u64::MAX] {
            assert!(is_spawn_session(id), "{id} must be spawn");
            assert!(!is_worker_session(id), "{id} must not be worker");
        }
    }

    #[test]
    fn task_batch_done_rows_are_pointers() {
        // Rows carry evidence PATHS, and a missing timed_out reads false
        // (forward-compat: a W1c-era consumer of a straggler-free report).
        let j = r#"{"type":"task_batch_done","batch":2,"parent":7,"rows":[
            {"worker":3,"state":"done","evidence":"events/agents/3.json"},
            {"worker":4,"state":"parked","evidence":"","timed_out":true}]}"#;
        match serde_json::from_str::<Event>(j).unwrap() {
            Event::TaskBatchDone { batch, parent, rows } => {
                assert_eq!(batch, 2);
                assert_eq!(parent, SessionId(7));
                assert_eq!(rows.len(), 2);
                assert_eq!(rows[0].worker, WorkerId(3));
                assert_eq!(rows[0].state, WorkerState::Done);
                assert_eq!(rows[0].evidence, "events/agents/3.json");
                assert!(!rows[0].timed_out);
                assert!(rows[1].timed_out);
                assert!(rows[1].evidence.is_empty());
            }
            other => panic!("wrong variant: {other:?}"),
        }
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
    fn approval_nonce_is_additive() {
        // Pre-nonce clients still deserialize; missing nonce is 0 (rejected
        // as a grant). New frames carry a non-zero nonce.
        let old = r#"{"type":"user_approval","session":1,"action":5,"granted":true}"#;
        match serde_json::from_str::<Event>(old).unwrap() {
            Event::UserApproval { action, granted, nonce, .. } => {
                assert_eq!(action, ActionId(5));
                assert!(granted);
                assert_eq!(nonce, 0);
            }
            other => panic!("wrong variant: {other:?}"),
        }
        let pending = r#"{"type":"approval_pending","session":1,"nonce":42,
            "call":{"id":5,"tool":"run_command","args":{},"needs_approval":true}}"#;
        match serde_json::from_str::<Event>(pending).unwrap() {
            Event::ApprovalPending { nonce, call, .. } => {
                assert_eq!(nonce, 42);
                assert_eq!(call.id, ActionId(5));
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
    fn sensor_alert_wire_shape_round_trips() {
        // The machine-readable twin of the root-session alert prompt — a
        // ui_reflex trigger, so the flat field names ARE the contract.
        let j = r#"{"type":"sensor_alert","node_id":"apex1","kind":"air_quality",
            "value":304.0,"threshold":300.0,"sensor_id":"bme688"}"#;
        match serde_json::from_str::<Event>(j).unwrap() {
            Event::SensorAlert { node_id, kind, value, threshold, sensor_id } => {
                assert_eq!(node_id, "apex1");
                assert_eq!(kind, "air_quality");
                assert!((value - 304.0).abs() < 0.01);
                assert!((threshold - 300.0).abs() < 0.01);
                assert_eq!(sensor_id, "bme688");
            }
            other => panic!("wrong variant: {other:?}"),
        }
        let ev = Event::SensorAlert {
            node_id: "apex1".into(), kind: "thermal_hotspot".into(),
            value: 120.5, threshold: 110.0, sensor_id: "mlx90640".into(),
        };
        let v: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&ev).unwrap()).unwrap();
        assert_eq!(v["type"], "sensor_alert");
        assert_eq!(v["kind"], "thermal_hotspot");
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
