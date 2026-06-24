//! Per-persona agent style preamble (ui-glowup G5, tier-2).
//!
//! A persona/skin (mom · ubuntu-dad · windows-dad · tech-kid · …) contributes a
//! short **response-style** fragment to the system prompt, so the agent's *voice*
//! matches the *face* the human chose — warm + plain for "mom", terse + telemetry
//! for the tech kid. It rides the existing soul/embodiment/priming compose path
//! (`apexos_agent::compose_system`) as one more optional layer, resolved per session.
//!
//! The active persona reaches the daemon over the WS (`set_persona` frame / a
//! `persona` field on `hello`) and is stored per session in [`PersonaSessions`],
//! mirroring [`crate::SessionBindings`]. The default persona ("apex") and any
//! unknown/empty id map to **no fragment** — APEX's own soul already *is* the
//! terse-technical voice, so the default path is byte-identical to before (zero
//! regression, and nothing extra in the cached prompt prefix).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use apexos_protocol::SessionId;

/// Process-wide per-session active persona (slug → e.g. "mom"). A `std::sync::Mutex`
/// (not tokio) — the read on the turn path is a tiny lock→clone→drop, never held
/// across an await. Shared gateway↔router exactly like [`crate::SessionBindings`].
pub type PersonaSessions = Arc<Mutex<HashMap<SessionId, String>>>;

/// The response-style preamble for a persona/skin slug (the ui-slint `persona_slug`
/// ids, plus their theme aliases for robustness). `None` for "apex"/"aurum"/unknown/
/// empty — those keep the agent's default soul voice, so the system prompt is
/// unchanged (the common path adds nothing).
pub fn persona_style(persona: &str) -> Option<&'static str> {
    match persona.trim().to_ascii_lowercase().as_str() {
        "mom" | "macos" => Some(
            "## Interaction style\n\
             You're speaking with someone who prefers warmth and plain language. \
             Use everyday words, not jargon, and don't surface tool names, code, or \
             internal mechanics unless asked. Keep replies short, friendly, and calm, \
             and phrase them so they read well aloud.",
        ),
        "ubuntu-dad" | "gnome" => Some(
            "## Interaction style\n\
             Speak in a balanced, approachable way with a moderate level of detail. \
             Explain technical points clearly without over-simplifying or burying them \
             in jargon.",
        ),
        "windows-dad" | "windows" => Some(
            "## Interaction style\n\
             Be friendly and guided. Walk through anything technical one clear step at \
             a time, using familiar everyday computing terms, and confirm along the way.",
        ),
        "tech-kid" | "tech-wiz-kid" | "jarvis" => Some(
            "## Interaction style\n\
             Be concise and technical. Surface telemetry, your reasoning, and the \
             underlying mechanism — don't hide the internals. Move fast and show your work.",
        ),
        // "apex" (default — terse/technical soul voice), "aurum" (reserved for the
        // memory dashboard skin), unknown, empty → no fragment.
        _ => None,
    }
}

/// The style fragment for `session`'s active persona, or `None` when the session has
/// no persona set, the persona is the default, or the lock is poisoned (fail-soft:
/// a lock error just drops the style, never panics the turn).
pub fn resolve_persona_style(map: &Mutex<HashMap<SessionId, String>>, session: SessionId) -> Option<String> {
    let slug = map.lock().ok()?.get(&session).cloned()?;
    persona_style(&slug).map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_personas_get_distinct_voices() {
        let mom = persona_style("mom").unwrap();
        let kid = persona_style("tech-kid").unwrap();
        assert!(mom.contains("plain language"));
        assert!(kid.contains("technical"));
        assert_ne!(mom, kid);
        // theme aliases resolve to the same fragment as the slug
        assert_eq!(persona_style("macos"), persona_style("mom"));
        assert_eq!(persona_style("jarvis"), persona_style("tech-kid"));
    }

    #[test]
    fn default_and_unknown_get_no_fragment() {
        // The default soul voice — adding nothing keeps the common path unchanged.
        assert_eq!(persona_style("apex"), None);
        assert_eq!(persona_style("aurum"), None);
        assert_eq!(persona_style(""), None);
        assert_eq!(persona_style("nope"), None);
        // case / whitespace insensitive
        assert_eq!(persona_style("  MOM "), persona_style("mom"));
    }

    #[test]
    fn resolve_reads_the_session_map() {
        let map: Mutex<HashMap<SessionId, String>> = Mutex::new(HashMap::new());
        assert_eq!(resolve_persona_style(&map, SessionId(7)), None); // unset
        map.lock().unwrap().insert(SessionId(7), "mom".into());
        assert!(resolve_persona_style(&map, SessionId(7)).unwrap().contains("plain language"));
        map.lock().unwrap().insert(SessionId(8), "apex".into());
        assert_eq!(resolve_persona_style(&map, SessionId(8)), None); // default → no fragment
    }
}
