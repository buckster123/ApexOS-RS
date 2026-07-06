//! Session → Cerebro consolidation (Slice 2 of session management).
//!
//! Before a chat is exported / archived / deleted, distil it into long-term
//! memory so the useful part survives the transcript. One LLM turn summarizes the
//! session into `{summary, key_discoveries}`, then `session_save` stores it in the
//! session's bound agent space (APEX for normal/mesh sessions, the bound agent for
//! a bound session — `resolve_agent_id`). Runs in a small agentd-side worker that
//! owns the provider + Cerebro `ToolProxy`; the gateway handler sends a request and
//! awaits the oneshot reply (see `apexos_gateway::ConsolidateReq`).

use std::path::Path;
use std::sync::Arc;

use apexos_agent::{Chunk, Provider};
use apexos_core::{ContentBlock, Message, SessionBindings, SessionId};
use apexos_plugins::ToolProxy;
use futures_util::StreamExt;
use serde_json::{json, Value};

const SYSTEM: &str = "You are a memory-consolidation assistant. You are given a transcript of an \
agent session that is about to be archived or deleted. Extract the durably useful information so it \
survives in long-term memory after the raw transcript is gone. Respond with ONLY a JSON object: \
{\"summary\": \"one tight paragraph — what happened, what was built/decided/learned\", \
\"key_discoveries\": [\"a specific reusable fact, gotcha, or decision\", ...]}. \
No prose or code fences outside the JSON. If the session is trivial or empty, return an empty \
summary and an empty list.";

/// Cap the transcript fed to the model (rough char budget — keeps a giant session
/// from blowing the context window; head+tail preserved, middle elided).
const MAX_TRANSCRIPT_CHARS: usize = 48_000;

/// Consolidate session `id` into Cerebro. Returns the JSON the HTTP handler relays:
/// `{ok:true, session_id, agent_id, summary, discoveries}` or `{ok:false, error}`.
pub async fn run(
    provider:     Arc<dyn Provider>,
    proxy:        &ToolProxy,
    sessions_dir: &Path,
    bindings:     &SessionBindings,
    id:           u64,
) -> Value {
    let jsonl = match tokio::fs::read_to_string(sessions_dir.join(format!("{id}.jsonl"))).await {
        Ok(t) if !t.trim().is_empty() => t,
        _ => return json!({ "ok": false, "error": format!("session {id} not found or empty") }),
    };

    // Reuse the gateway's transcript renderer, then bound its size.
    let transcript = truncate_middle(&apexos_gateway::render_session_markdown(id, &jsonl), MAX_TRANSCRIPT_CHARS);

    let history = [Message::User {
        content: vec![ContentBlock::Text {
            text: format!("Session {id} transcript follows. Consolidate it into the JSON object.\n\n{transcript}"),
        }],
    }];
    let raw = match collect(&provider, &history, SYSTEM).await {
        Ok(t)  => t,
        Err(e) => return json!({ "ok": false, "error": format!("summarization failed: {e}") }),
    };

    let (summary, discoveries) = parse_summary(&raw);
    if summary.trim().is_empty() {
        return json!({ "ok": false, "error": "nothing substantive to consolidate" });
    }

    let agent_id = apexos_core::resolve_agent_id(bindings, SessionId(id));
    let mut content = format!("SESSION {id} (consolidated before archive/delete): {summary}");
    if !discoveries.is_empty() {
        content.push_str("\n\nKey discoveries:");
        for d in &discoveries {
            content.push_str(&format!("\n- {d}"));
        }
    }

    // DirectCall does NOT stamp agent_id (unlike a model-issued tool call), so the
    // explicit space here is honored — consolidate into the session's own memory.
    let args = json!({
        "content":      content,
        "agent_id":     agent_id,
        "priority":     "MEDIUM",
        "session_type": "archived",
    });
    match proxy.call("session_save", args).await {
        Ok(out) if out.ok => json!({
            "ok":          true,
            "session_id":  id,
            "agent_id":    agent_id,
            "summary":     summary,
            "discoveries": discoveries,
        }),
        Ok(out) => json!({ "ok": false, "error": format!("session_save rejected: {}", out.content) }),
        Err(e)  => json!({ "ok": false, "error": format!("session_save failed: {e}") }),
    }
}

/// One-shot completion (no tools) — mirrors self_update's `collect_completion`.
/// Shared with `rehearse` (the soul fitting room): both are off-turn ephemeral
/// LLM work owned by this worker seam, so the system prompt is a parameter.
pub(crate) async fn collect(
    provider: &Arc<dyn Provider>,
    history:  &[Message],
    system:   &str,
) -> Result<String, String> {
    let mut stream = provider.messages_stream(history, &[], Some(system)).await.map_err(|e| e.to_string())?;
    let mut text = String::new();
    while let Some(chunk) = stream.next().await {
        match chunk {
            Ok(Chunk::TextDelta(t)) => text.push_str(&t),
            Ok(Chunk::TextBlock(t)) => { text = t; break; }
            Ok(Chunk::Done)         => break,
            Err(e)                  => return Err(e.to_string()),
            _                       => {}
        }
    }
    Ok(text)
}

/// Head + tail with the middle elided when over budget (char-based, multibyte-safe).
fn truncate_middle(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let half = max / 2;
    let head: String = s.chars().take(half).collect();
    let tail: String = {
        let mut t: Vec<char> = s.chars().rev().take(half).collect();
        t.reverse();
        t.into_iter().collect()
    };
    format!("{head}\n\n…[transcript truncated for length]…\n\n{tail}")
}

/// Pull `{summary, key_discoveries}` out of the model's reply, tolerant of code
/// fences / surrounding prose. Falls back to treating the whole text as the summary.
fn parse_summary(raw: &str) -> (String, Vec<String>) {
    if let Some(slice) = extract_json_object(raw) {
        if let Ok(v) = serde_json::from_str::<Value>(&slice) {
            let summary = v["summary"].as_str().unwrap_or("").trim().to_string();
            let discoveries = v["key_discoveries"].as_array()
                .map(|a| a.iter()
                    .filter_map(|x| x.as_str().map(|s| s.trim().to_string()))
                    .filter(|s| !s.is_empty())
                    .collect())
                .unwrap_or_default();
            return (summary, discoveries);
        }
    }
    (raw.trim().to_string(), Vec::new())
}

/// The first `{` … last `}` span, if any.
fn extract_json_object(s: &str) -> Option<String> {
    let start = s.find('{')?;
    let end = s.rfind('}')?;
    (end > start).then(|| s[start..=end].to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_summary_extracts_fenced_json() {
        let raw = "```json\n{\"summary\": \"built X\", \"key_discoveries\": [\"a\", \" \", \"b\"]}\n```";
        let (s, d) = parse_summary(raw);
        assert_eq!(s, "built X");
        assert_eq!(d, vec!["a".to_string(), "b".to_string()], "blanks dropped");
    }

    #[test]
    fn parse_summary_falls_back_to_plain_text() {
        let (s, d) = parse_summary("just a sentence, no json");
        assert_eq!(s, "just a sentence, no json");
        assert!(d.is_empty());
    }

    #[test]
    fn truncate_middle_preserves_head_and_tail() {
        let s = "a".repeat(100);
        let out = truncate_middle(&s, 20);
        assert!(out.contains("truncated"));
        assert!(out.starts_with("aaaa"));
        assert!(out.ends_with("aaaa"));
    }
}
