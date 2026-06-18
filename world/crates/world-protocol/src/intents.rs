// MIRRORS the client→server intent frames agentd's gateway accepts.
//
// Outbound intents **omit `session`** — the gateway injects `frame["session"] = <id>`
// from the socket's bound session before deserializing into its `Event` enum. A frame
// with the wrong field names deserializes to nothing and is **silently dropped** (no
// error), so the exact field names here are load-bearing and unit-tested:
//   - approval uses `action` (a bare number = `ToolCall.id`), NOT `call_id`
//   - approval uses `granted` (a bool), NOT `approved`
//   - no `session` key on any outbound frame
//   - resume uses `hello` with `resume_session` (the server replies `session_init`,
//     never `hello` — DESIGN.md D7)

use serde::Serialize;

use crate::ids::ActionId;

/// An outbound intent frame. `#[serde(tag = "type", rename_all = "snake_case")]`
/// mirrors agentd's `Event` discriminant. Built via the constructors below; never
/// carries a `session` field.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Intent {
    /// Speak to the session this socket is bound to.
    UserPrompt { text: String },
    /// Approve or reject a pending tool call. `action` is the `ToolCall.id`.
    UserApproval { action: ActionId, granted: bool },
    /// Cancel the in-flight turn. NB: agentd emits **no** `turn_complete` in
    /// response — the client must clear its own busy + tool affordances.
    UserCancel,
    /// Re-point this socket at an existing session; the server replies with a
    /// fresh `session_init` carrying that session's history.
    Hello { resume_session: u64 },
}

impl Intent {
    /// Serialize to the JSON text sent over the WebSocket. Infallible for these
    /// shapes (all fields are plain owned types).
    pub fn to_json(&self) -> String {
        // serde_json::to_string on these variants cannot fail.
        serde_json::to_string(self).expect("intent serialization is infallible")
    }
}

/// `{"type":"user_prompt","text":"…"}` — speak to the bound session.
pub fn user_prompt(text: impl Into<String>) -> Intent {
    Intent::UserPrompt { text: text.into() }
}

/// `{"type":"user_approval","action":<id>,"granted":<bool>}`.
/// `action` is the numeric `ToolCall.id`; pass the `ActionId` you saw on
/// `tool_requested`/`approval_pending`.
pub fn user_approval(action: ActionId, granted: bool) -> Intent {
    Intent::UserApproval { action, granted }
}

/// `{"type":"user_cancel"}` — abort the in-flight turn.
pub fn user_cancel() -> Intent {
    Intent::UserCancel
}

/// `{"type":"hello","resume_session":<id>}` — resume an existing session.
pub fn hello(resume_session: u64) -> Intent {
    Intent::Hello { resume_session }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_prompt_omits_session_and_has_correct_type() {
        let v: serde_json::Value =
            serde_json::from_str(&user_prompt("hello").to_json()).unwrap();
        assert_eq!(v["type"], "user_prompt");
        assert_eq!(v["text"], "hello");
        assert!(v.get("session").is_none(), "outbound frames must omit `session`");
    }

    #[test]
    fn user_approval_uses_action_and_granted_with_bare_number() {
        // The {action, granted} shape — NOT {call_id, approved}.
        let v: serde_json::Value =
            serde_json::from_str(&user_approval(ActionId(5), true).to_json()).unwrap();
        assert_eq!(v["type"], "user_approval");
        assert_eq!(v["action"], 5, "action must be a bare number = ToolCall.id");
        assert_eq!(v["granted"], true);
        assert!(v.get("call_id").is_none(), "must be `action`, not `call_id`");
        assert!(v.get("approved").is_none(), "must be `granted`, not `approved`");
        assert!(v.get("session").is_none());
    }

    #[test]
    fn user_cancel_is_just_the_type_tag() {
        let v: serde_json::Value =
            serde_json::from_str(&user_cancel().to_json()).unwrap();
        assert_eq!(v["type"], "user_cancel");
        assert!(v.get("session").is_none());
    }

    #[test]
    fn hello_carries_resume_session() {
        let v: serde_json::Value =
            serde_json::from_str(&hello(42).to_json()).unwrap();
        assert_eq!(v["type"], "hello");
        assert_eq!(v["resume_session"], 42);
        assert!(v.get("session").is_none());
    }
}
