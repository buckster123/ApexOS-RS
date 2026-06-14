// MIRRORS agentd/crates/core/src/types.rs — keep in sync.
//
// Inbound wire events: what agentd broadcasts to every connected socket. This is a
// **subset** of agentd's full `Event` enum — only the variants apexos-world needs to
// drive the world (avatar status, chat, tool round-trip, sensors, council, sub-agents).
//
// Two load-bearing wire facts this file encodes (DESIGN.md §4):
//   1. `Event` is `#[serde(tag = "type", rename_all = "snake_case")]`.
//   2. On `tool_requested`/`approval_pending` the tool data nests under `call`
//      (a `ToolCall` with `call.id` a bare number). On `tool_result`, `call` is the
//      **bare `ActionId`** and the body is `output: {ok, content}`.
//
// A frame that fails to deserialize is **silently dropped** by agentd, and the client
// mirrors that policy. To avoid every new agentd event becoming a dropped frame, this
// enum carries a `#[serde(other)] Unknown` fallback: unrecognized `type`s deserialize
// to `Event::Unknown` rather than erroring, so the read loop can log-and-skip them.

use serde::{Deserialize, Serialize};

use crate::ids::{ActionId, PluginId, SessionId};

/// A reading from one sensor. `kind` is the serde discriminant.
/// MIRRORS `SensorReading` in agentd core (subset of fields preserved verbatim).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SensorReading {
    Temperature { celsius: f32, sensor_id: String },
    Humidity { percent: f32, sensor_id: String },
    Pressure { hpa: f32, sensor_id: String },
    Motion { detected: bool, sensor_id: String },
    Distance { cm: f32, sensor_id: String },
    GpioLevel { pin: u8, high: bool },
    /// BME688 BSEC2 air quality bundle.
    AirQuality {
        iaq: f32,
        co2_eq_ppm: f32,
        voc_ppm: f32,
        accuracy: u8,
        temperature_c: f32,
        humidity_pct: f32,
        pressure_hpa: f32,
        sensor_id: String,
    },
    /// MLX90640 32×24 thermal frame summary.
    ThermalFrame {
        min_c: f32,
        max_c: f32,
        mean_c: f32,
        sensor_id: String,
    },
    /// Any sensor kind this mirror does not model yet.
    #[serde(other)]
    Unknown,
}

/// One participant in a council session. MIRRORS `CouncilAgentDef`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CouncilAgentDef {
    pub id: String,
    pub persona: String,
    pub backend: Option<String>,
    pub model: Option<String>,
    pub color: Option<String>,
}

/// A tool call. On the wire `id` is a bare number (see `ActionId`). MIRRORS `ToolCall`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolCall {
    pub id: ActionId,
    pub tool: String,
    pub args: serde_json::Value,
    /// Set by agentd's policy engine, not the agent.
    pub needs_approval: bool,
}

/// The result body carried by `tool_result`. MIRRORS `ToolOutput`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolOutput {
    pub ok: bool,
    pub content: serde_json::Value,
}

/// A tool spec announced on `plugin_up`. MIRRORS `ToolSpec`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
}

/// Inbound events broadcast by agentd. `#[serde(tag = "type", rename_all =
/// "snake_case")]` — the discriminant is a `"type"` field with a snake_case value.
///
/// The gateway injects `frame["session"] = <id>` into client→server frames before
/// deserializing; for **inbound** broadcast frames agentd populates `session` itself.
/// Every event for every session reaches every socket, so the client must filter on
/// `session` (see [`Event::session`]).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    /// Pushed by the gateway immediately on connect (and on resume). Carries the
    /// session id this socket is bound to and the replayed history.
    SessionInit {
        session_id: SessionId,
        #[serde(default)]
        history: Vec<serde_json::Value>,
    },

    // ── agent loop ──────────────────────────────────────────
    AgentText { session: SessionId, delta: String },
    AgentThinking { session: SessionId, delta: String },
    ToolRequested { session: SessionId, call: ToolCall },
    /// NB: agentd has no `turn_started`; busy begins on the first `agent_text`/
    /// `tool_requested` after a prompt and is cleared by `turn_complete`.
    TurnComplete { session: SessionId },

    // ── plugin supervisor ───────────────────────────────────
    /// `call` is the **bare** `ActionId` here (not a nested `ToolCall`).
    ToolResult {
        session: SessionId,
        call: ActionId,
        output: ToolOutput,
    },
    PluginUp { plugin: PluginId, tools: Vec<ToolSpec> },
    PluginDown { plugin: PluginId, reason: String },

    // ── policy engine ───────────────────────────────────────
    ApprovalPending { session: SessionId, call: ToolCall },

    // ── sub-agent routing ───────────────────────────────────
    SubAgentStarted {
        parent: SessionId,
        child: SessionId,
        prompt: String,
    },

    // ── sensor bridge ───────────────────────────────────────
    SensorReading {
        node_id: String,
        reading: SensorReading,
        timestamp: u64,
    },

    // ── voice / wake word ───────────────────────────────────
    WakeTriggered,

    // ── agent-to-agent messaging ────────────────────────────
    AgentMessage {
        from: SessionId,
        to: SessionId,
        body: String,
        msg_id: u64,
    },
    AgentMessageAck { msg_id: u64, from: SessionId },

    // ── council ─────────────────────────────────────────────
    CouncilStarted {
        council_id: String,
        topic: String,
        agents: Vec<CouncilAgentDef>,
    },
    CouncilRoundStart { council_id: String, round: u32 },
    CouncilAgentDelta {
        council_id: String,
        round: u32,
        agent_id: String,
        delta: String,
    },
    CouncilAgentDone {
        council_id: String,
        round: u32,
        agent_id: String,
        full_text: String,
    },
    CouncilRoundDone {
        council_id: String,
        round: u32,
        convergence: f32,
        agreements: Vec<String>,
    },
    CouncilComplete {
        council_id: String,
        rounds: u32,
        reason: String,
        synthesis: String,
    },
    CouncilButtIn { council_id: String, message: String },

    // ── errors ──────────────────────────────────────────────
    Error {
        session: Option<SessionId>,
        message: String,
    },

    /// Any agentd event this mirror does not model yet. agentd silently drops
    /// frames that fail to deserialize; this fallback lets the client do the same
    /// (log-and-skip) without crashing as agentd grows new events.
    #[serde(other)]
    Unknown,
}

impl Event {
    /// The `SessionId` this event pertains to, if any. The world client filters
    /// inbound events on this so a station only sees its own session's stream.
    ///
    /// Events with no per-session scope (`WakeTriggered`, plugin/council/mesh
    /// events keyed on other ids, `Unknown`) return `None`. `Error` carries an
    /// `Option<SessionId>` and returns its inner value.
    pub fn session(&self) -> Option<SessionId> {
        match self {
            Event::SessionInit { session_id, .. } => Some(*session_id),
            Event::AgentText { session, .. }
            | Event::AgentThinking { session, .. }
            | Event::ToolRequested { session, .. }
            | Event::TurnComplete { session }
            | Event::ToolResult { session, .. }
            | Event::ApprovalPending { session, .. } => Some(*session),
            Event::SubAgentStarted { parent, .. } => Some(*parent),
            Event::AgentMessage { to, .. } => Some(*to),
            Event::AgentMessageAck { from, .. } => Some(*from),
            Event::Error { session, .. } => *session,
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deserializes_session_init() {
        let frame = r#"{"type":"session_init","session_id":42,"history":[]}"#;
        let ev: Event = serde_json::from_str(frame).unwrap();
        assert_eq!(ev, Event::SessionInit { session_id: SessionId(42), history: vec![] });
        assert_eq!(ev.session(), Some(SessionId(42)));
    }

    #[test]
    fn deserializes_agent_text_delta() {
        let frame = r#"{"type":"agent_text","session":7,"delta":"hello"}"#;
        let ev: Event = serde_json::from_str(frame).unwrap();
        assert_eq!(ev, Event::AgentText { session: SessionId(7), delta: "hello".into() });
    }

    #[test]
    fn tool_requested_has_nested_call_with_bare_number_id() {
        // The real wire shape: tool data nests under `call`, and `call.id` is a
        // bare number (not a string, not flattened to `call_id`).
        let frame = r#"{
            "type":"tool_requested",
            "session":3,
            "call":{"id":5,"tool":"shell","args":{"cmd":"ls"},"needs_approval":true}
        }"#;
        let ev: Event = serde_json::from_str(frame).unwrap();
        match ev {
            Event::ToolRequested { session, call } => {
                assert_eq!(session, SessionId(3));
                assert_eq!(call.id, ActionId(5));
                assert_eq!(call.tool, "shell");
                assert!(call.needs_approval);
            }
            other => panic!("expected ToolRequested, got {other:?}"),
        }
    }

    #[test]
    fn tool_result_call_is_a_bare_action_id() {
        // On tool_result, `call` is the bare ActionId, and the body is output:{ok,content}.
        let frame = r#"{
            "type":"tool_result",
            "session":3,
            "call":5,
            "output":{"ok":true,"content":"done"}
        }"#;
        let ev: Event = serde_json::from_str(frame).unwrap();
        match ev {
            Event::ToolResult { session, call, output } => {
                assert_eq!(session, SessionId(3));
                assert_eq!(call, ActionId(5));
                assert!(output.ok);
                assert_eq!(output.content, serde_json::json!("done"));
            }
            other => panic!("expected ToolResult, got {other:?}"),
        }
    }

    #[test]
    fn deserializes_approval_pending_with_nested_call() {
        let frame = r#"{
            "type":"approval_pending",
            "session":3,
            "call":{"id":9,"tool":"shell","args":{},"needs_approval":true}
        }"#;
        let ev: Event = serde_json::from_str(frame).unwrap();
        match ev {
            Event::ApprovalPending { call, .. } => assert_eq!(call.id, ActionId(9)),
            other => panic!("expected ApprovalPending, got {other:?}"),
        }
    }

    #[test]
    fn deserializes_sensor_reading_air_quality() {
        let frame = r#"{
            "type":"sensor_reading",
            "node_id":"body-pi-1",
            "timestamp":1700000000,
            "reading":{"kind":"air_quality","iaq":42.0,"co2_eq_ppm":500.0,"voc_ppm":0.5,
                       "accuracy":3,"temperature_c":22.0,"humidity_pct":40.0,
                       "pressure_hpa":1013.0,"sensor_id":"bme688"}
        }"#;
        let ev: Event = serde_json::from_str(frame).unwrap();
        match ev {
            Event::SensorReading { reading, node_id, .. } => {
                assert_eq!(node_id, "body-pi-1");
                assert!(matches!(reading, SensorReading::AirQuality { accuracy: 3, .. }));
            }
            other => panic!("expected SensorReading, got {other:?}"),
        }
    }

    #[test]
    fn deserializes_wake_triggered_unit_variant() {
        let ev: Event = serde_json::from_str(r#"{"type":"wake_triggered"}"#).unwrap();
        assert_eq!(ev, Event::WakeTriggered);
    }

    #[test]
    fn unknown_event_type_falls_back_not_errors() {
        // A future agentd event we don't model must not crash the client.
        let frame = r#"{"type":"vast_instance_ready","instance_id":"x","local_port":8080}"#;
        let ev: Event = serde_json::from_str(frame).unwrap();
        assert_eq!(ev, Event::Unknown);
        assert_eq!(ev.session(), None);
    }

    #[test]
    fn sub_agent_started_session_is_parent() {
        let frame = r#"{"type":"sub_agent_started","parent":1,"child":2,"prompt":"go"}"#;
        let ev: Event = serde_json::from_str(frame).unwrap();
        assert_eq!(ev.session(), Some(SessionId(1)));
    }
}
