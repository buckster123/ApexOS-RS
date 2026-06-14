// MIRRORS agentd/crates/core/src/types.rs — keep in sync.
//
// ID newtypes. On the wire these serialize as **bare numbers** (e.g. `42`), not
// `{"0":42}` or `"42"`. The agentd source derives `Serialize`/`Deserialize` on
// `struct SessionId(pub u64)` directly — a single-field tuple struct already
// (de)serializes as its inner value. We add `#[serde(transparent)]` to make that
// guarantee explicit and resistant to accidental field additions.

use serde::{Deserialize, Serialize};

/// One agentd session. Identity of an avatar in the world (one socket ⇄ one
/// `SessionId`). Serializes as a bare `u64`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SessionId(pub u64);

/// A tool call id (the `ToolCall.id`). This is the value an approval intent
/// carries as `action` — **not** a `call_id` string. Serializes as a bare `u64`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ActionId(pub u64);

/// A plugin identifier (e.g. `"cerebro-mcp"`). Serializes as a bare string.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PluginId(pub String);

impl std::fmt::Display for SessionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::fmt::Display for ActionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::fmt::Display for PluginId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_id_is_a_bare_number() {
        assert_eq!(serde_json::to_string(&SessionId(42)).unwrap(), "42");
        assert_eq!(
            serde_json::from_str::<SessionId>("42").unwrap(),
            SessionId(42)
        );
    }

    #[test]
    fn action_id_round_trips_as_bare_number() {
        let a = ActionId(5);
        let s = serde_json::to_string(&a).unwrap();
        assert_eq!(s, "5");
        assert_eq!(serde_json::from_str::<ActionId>(&s).unwrap(), a);
    }

    #[test]
    fn plugin_id_is_a_bare_string() {
        assert_eq!(
            serde_json::to_string(&PluginId("cerebro-mcp".into())).unwrap(),
            "\"cerebro-mcp\""
        );
    }
}
