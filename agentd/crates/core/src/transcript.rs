//! Verbatim transcript search over session JSONL (`docs/session-rag.md` S1).
//!
//! The on-disk file is the store; this module is a pure matcher + identity
//! gate. The supervisor streams a file through [`search_transcript`] and
//! formats hits for the model. No Cerebro, no embeddings, no regex.

use crate::{is_spawn_session, is_worker_session, ContentBlock, Message};
use std::collections::VecDeque;
use std::io::BufRead;

/// One matching message in a JSONL transcript (most-recent ring).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscriptHit {
    /// 0-based message index in the file (one JSON object per line, blanks skipped).
    pub index: usize,
    pub role: &'static str,
    pub snippet: String,
}

pub const DEFAULT_MAX_HITS: usize = 20;
pub const MAX_HITS_CEIL: usize = 50;
const SNIPPET_CHARS: usize = 240;
const TOOL_TEXT_CAP: usize = 500;

/// Clamp the tool's `max` argument.
pub fn clamp_max(max: Option<u64>) -> usize {
    max.unwrap_or(DEFAULT_MAX_HITS as u64)
        .clamp(1, MAX_HITS_CEIL as u64) as usize
}

/// Identity + range gate (`docs/session-rag.md`). `Ok(())` = searchable.
pub fn target_allowed(
    caller: u64,
    target: u64,
    caller_is_node_agent: bool,
) -> Result<(), &'static str> {
    if is_worker_session(target) || is_spawn_session(target) {
        return Err(
            "worker and spawn transcripts are not the session-search corpus \
             (workers leave evidence files; spawns are not persisted)",
        );
    }
    if caller == target {
        return Ok(());
    }
    if caller_is_node_agent {
        return Ok(());
    }
    Err("a bound guest agent can only search its own session — pass no session_id, or this session's id")
}

/// Case-insensitive AND of whitespace-separated `query` terms over `reader`.
/// Keeps the most recent `max` hits (one pass, ring buffer). Empty query → no
/// hits (callers should refuse before scanning). Blank / unparseable lines are
/// skipped and do not increment the message index.
pub fn search_transcript<R: BufRead>(
    reader: R,
    query: &str,
    max: usize,
) -> Vec<TranscriptHit> {
    let terms = query_terms(query);
    if terms.is_empty() || max == 0 {
        return Vec::new();
    }
    let mut hits: VecDeque<TranscriptHit> = VecDeque::new();
    let mut index = 0usize;
    for line in reader.lines() {
        let Ok(line) = line else { continue };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(msg) = serde_json::from_str::<Message>(line) else {
            continue;
        };
        if let Some(hit) = match_message(index, &msg, &terms) {
            if hits.len() == max {
                hits.pop_front();
            }
            hits.push_back(hit);
        }
        index += 1;
    }
    hits.into_iter().collect()
}

/// Lowercased non-empty whitespace tokens.
pub fn query_terms(query: &str) -> Vec<String> {
    query
        .split_whitespace()
        .map(|t| t.to_lowercase())
        .filter(|t| !t.is_empty())
        .collect()
}

fn match_message(index: usize, msg: &Message, terms: &[String]) -> Option<TranscriptHit> {
    let (role, haystack) = searchable(msg);
    if haystack.is_empty() {
        return None;
    }
    let lower = haystack.to_lowercase();
    if !terms.iter().all(|t| lower.contains(t)) {
        return None;
    }
    Some(TranscriptHit {
        index,
        role,
        snippet: snippet(&haystack, terms),
    })
}

fn searchable(msg: &Message) -> (&'static str, String) {
    let (role, content) = match msg {
        Message::User { content } => ("user", content),
        Message::Assistant { content } => ("assistant", content),
    };
    let mut parts: Vec<String> = Vec::new();
    for b in content {
        match b {
            ContentBlock::Text { text } => {
                if !text.trim().is_empty() {
                    parts.push(text.clone());
                }
            }
            ContentBlock::ToolUse { name, input, .. } => {
                parts.push(format!("{name} {}", compact_value(input, TOOL_TEXT_CAP)));
            }
            ContentBlock::ToolResult { content, .. } => {
                parts.push(compact_value(content, TOOL_TEXT_CAP));
            }
            ContentBlock::Image { .. } | ContentBlock::Thinking { .. } => {}
        }
    }
    (role, parts.join(" "))
}

fn compact_value(v: &serde_json::Value, cap: usize) -> String {
    let s = match v {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    let s = s.replace('\n', " ");
    if s.chars().count() > cap {
        format!("{}…", s.chars().take(cap).collect::<String>())
    } else {
        s
    }
}

fn snippet(haystack: &str, terms: &[String]) -> String {
    let lower = haystack.to_lowercase();
    let pos = terms
        .iter()
        .filter_map(|t| lower.find(t.as_str()))
        .min()
        .unwrap_or(0);
    // Align to a char boundary at/before `pos`.
    let start_byte = haystack
        .char_indices()
        .take_while(|(i, _)| *i <= pos)
        .last()
        .map(|(i, _)| i)
        .unwrap_or(0);
    let from_match: String = haystack[start_byte..].chars().take(SNIPPET_CHARS).collect();
    if start_byte > 0 {
        format!("…{from_match}")
    } else if haystack.chars().count() > SNIPPET_CHARS {
        format!("{from_match}…")
    } else {
        from_match
    }
}

/// Count non-empty lines and take an 80-char first-user-text preview (the
/// Sessions-picker shape). Unparseable lines still count.
pub fn jsonl_count_and_preview<R: BufRead>(reader: R, preview_chars: usize) -> (usize, String) {
    let mut count = 0usize;
    let mut preview = String::new();
    for line in reader.lines() {
        let Ok(line) = line else { continue };
        if line.trim().is_empty() {
            continue;
        }
        count += 1;
        if preview.is_empty() {
            if let Ok(Message::User { content }) = serde_json::from_str::<Message>(&line) {
                if let Some(text) = content.into_iter().find_map(|b| match b {
                    ContentBlock::Text { text } if !text.trim().is_empty() => Some(text),
                    _ => None,
                }) {
                    preview = text.chars().take(preview_chars).collect();
                }
            }
        }
    }
    (count, preview)
}

pub fn format_list(rows: &[(u64, usize, String)]) -> String {
    if rows.is_empty() {
        return "no visible sessions.".into();
    }
    let mut out = format!("sessions ({} visible):\n", rows.len());
    for (id, n, preview) in rows {
        if preview.is_empty() {
            out.push_str(&format!("{id}  msgs={n}\n"));
        } else {
            out.push_str(&format!("{id}  msgs={n}  {preview}\n"));
        }
    }
    out
}

/// Format hits for the model. `ok` payload is this string.
pub fn format_hits(session_id: u64, hits: &[TranscriptHit], query: &str) -> String {
    if hits.is_empty() {
        return format!("session {session_id}: no matches for {query:?}.");
    }
    let mut out = format!(
        "session {session_id} ({} hit{}, most recent last):\n",
        hits.len(),
        if hits.len() == 1 { "" } else { "s" }
    );
    for h in hits {
        out.push_str(&format!("#{} {}: {}\n", h.index, h.role, h.snippet));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{WORKER_SESSION_BASE, SPAWN_SESSION_BASE};
    use serde_json::json;
    use std::io::Cursor;

    fn user(text: &str) -> Message {
        Message::User {
            content: vec![ContentBlock::Text { text: text.into() }],
        }
    }
    fn asst(text: &str) -> Message {
        Message::Assistant {
            content: vec![ContentBlock::Text { text: text.into() }],
        }
    }
    fn jsonl(msgs: &[Message]) -> Cursor<Vec<u8>> {
        let mut buf = Vec::new();
        for m in msgs {
            buf.extend(serde_json::to_string(m).unwrap().into_bytes());
            buf.push(b'\n');
        }
        Cursor::new(buf)
    }

    #[test]
    fn and_match_is_case_insensitive() {
        let hits = search_transcript(
            jsonl(&[user("USB Eject failed on apex1"), asst("try the path unit")]),
            "usb eject",
            20,
        );
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].index, 0);
        assert_eq!(hits[0].role, "user");
        assert!(hits[0].snippet.to_lowercase().contains("usb"));
    }

    #[test]
    fn missing_term_is_not_a_hit() {
        let hits = search_transcript(jsonl(&[user("USB mount ok")]), "usb eject", 20);
        assert!(hits.is_empty());
    }

    #[test]
    fn empty_query_scans_nothing() {
        let hits = search_transcript(jsonl(&[user("hello")]), "   ", 20);
        assert!(hits.is_empty());
    }

    #[test]
    fn skips_image_and_thinking() {
        let img = Message::User {
            content: vec![ContentBlock::Image {
                media_type: "image/png".into(),
                data: "iVBORw0KGgoAAAANS=".into(),
            }],
        };
        let think = Message::Assistant {
            content: vec![ContentBlock::Thinking {
                thinking: "secret usb plan".into(),
                signature: "x".into(),
            }],
        };
        let hits = search_transcript(jsonl(&[img, think, user("visible usb")]), "usb", 20);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].index, 2);
        assert!(!hits[0].snippet.contains("iVBOR"));
    }

    #[test]
    fn tool_use_is_searchable_by_name() {
        let call = Message::Assistant {
            content: vec![ContentBlock::ToolUse {
                id: "t1".into(),
                name: "eject_media".into(),
                input: json!({"label": "APEX-config"}),
            }],
        };
        let hits = search_transcript(jsonl(&[call]), "eject_media apex-config", 20);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].role, "assistant");
    }

    #[test]
    fn ring_keeps_most_recent_max() {
        let msgs: Vec<Message> = (0..8).map(|i| user(&format!("token {i} needle"))).collect();
        let hits = search_transcript(jsonl(&msgs), "needle", 3);
        assert_eq!(hits.len(), 3);
        assert_eq!(hits[0].index, 5);
        assert_eq!(hits[2].index, 7);
    }

    #[test]
    fn skips_blank_and_garbage_lines() {
        let mut raw = b"\nnot-json\n".to_vec();
        raw.extend(serde_json::to_string(&user("keep me needle")).unwrap().into_bytes());
        raw.push(b'\n');
        let hits = search_transcript(Cursor::new(raw), "needle", 20);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].index, 0);
    }

    #[test]
    fn target_gate_own_session_always() {
        assert!(target_allowed(12, 12, false).is_ok());
        assert!(target_allowed(0, 0, false).is_ok());
    }

    #[test]
    fn target_gate_guest_cannot_cross() {
        assert!(target_allowed(12, 0, false).is_err());
        assert!(target_allowed(12, 7, false).is_err());
    }

    #[test]
    fn target_gate_node_agent_can_cross_normal() {
        assert!(target_allowed(12, 0, true).is_ok());
        assert!(target_allowed(12, 7, true).is_ok());
    }

    #[test]
    fn target_gate_refuses_worker_and_spawn() {
        assert!(target_allowed(12, WORKER_SESSION_BASE, true).is_err());
        assert!(target_allowed(WORKER_SESSION_BASE, WORKER_SESSION_BASE, true).is_err());
        assert!(target_allowed(12, SPAWN_SESSION_BASE, true).is_err());
    }

    #[test]
    fn clamp_max_floors_and_ceils() {
        assert_eq!(clamp_max(None), 20);
        assert_eq!(clamp_max(Some(0)), 1);
        assert_eq!(clamp_max(Some(999)), 50);
    }

    #[test]
    fn count_and_preview_skips_blank_and_takes_first_user_text() {
        let mut raw = b"\n".to_vec();
        raw.extend(serde_json::to_string(&asst("later")).unwrap().into_bytes());
        raw.push(b'\n');
        raw.extend(serde_json::to_string(&user("hello world from the top")).unwrap().into_bytes());
        raw.push(b'\n');
        let (n, preview) = jsonl_count_and_preview(Cursor::new(raw), 11);
        assert_eq!(n, 2);
        assert_eq!(preview, "hello world");
    }

    #[test]
    fn format_list_empty_is_honest() {
        assert_eq!(format_list(&[]), "no visible sessions.");
    }
}
