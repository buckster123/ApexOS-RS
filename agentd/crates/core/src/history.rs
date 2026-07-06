//! History windowing for long-lived sessions.
//!
//! The always-on root session (`SessionId(0)`) funnels every sensor alert and
//! scheduled task into one ever-growing `Vec<Message>` that is re-sent in full
//! each turn — with no bound it eventually exceeds the model context window and
//! the daemon wedges in a restart-surviving crash-loop. [`trim_history`] caps the
//! in-memory window to a rough token budget, dropping whole oldest turns but
//! cutting only at clean user-turn boundaries so a kept `tool_result` is never
//! orphaned from its `tool_use` (which the Anthropic API rejects). The on-disk
//! JSONL stays append-only (the full history is preserved for replay) — only the
//! working window the model sees each turn is bounded.

use crate::{ContentBlock, Message};

/// Rough per-message token estimate (≈ chars/4 over the text-bearing blocks).
/// Images are token-capped upstream by the vision shim, so they're charged a flat
/// cost rather than their (huge) base64 length, which would wildly over-count.
fn msg_tokens(m: &Message) -> usize {
    let content = match m {
        Message::User { content } | Message::Assistant { content } => content,
    };
    let body: usize = content
        .iter()
        .map(|b| match b {
            ContentBlock::Text { text }              => text.len() / 4,
            ContentBlock::Thinking { thinking, .. }  => thinking.len() / 4,
            ContentBlock::ToolUse { input, .. }      => input.to_string().len() / 4,
            ContentBlock::ToolResult { content, .. } => content.to_string().len() / 4,
            ContentBlock::Image { .. }               => 1_600,
        })
        .sum();
    body + 4 // small per-message/role framing overhead
}

/// A clean turn boundary: a genuine user message (text/image), **not** a user
/// message that only delivers `tool_result`s. Trimming may only cut here —
/// cutting mid-exchange would orphan a `tool_result` from its `tool_use`.
fn is_turn_start(m: &Message) -> bool {
    matches!(m, Message::User { content }
        if !content.iter().any(|b| matches!(b, ContentBlock::ToolResult { .. })))
}

/// Trim `history` in place so its rough token estimate fits `budget_tokens`,
/// dropping whole oldest turns at clean user-turn boundaries. Keeps the most
/// recent turns that fit; always keeps at least the last turn even if it alone
/// exceeds the budget (intra-message trimming is out of scope). No-op when
/// already under budget, when `budget_tokens == 0` (disabled), or when no safe
/// cut point exists — it never orphans a `tool_result` from its `tool_use`.
pub fn trim_history(history: &mut Vec<Message>, budget_tokens: usize) {
    if budget_tokens == 0 {
        return;
    }
    let n = history.len();
    if n == 0 {
        return;
    }

    // suffix[i] = estimated tokens of history[i..].
    let mut suffix = vec![0usize; n + 1];
    for i in (0..n).rev() {
        suffix[i] = suffix[i + 1] + msg_tokens(&history[i]);
    }
    if suffix[0] <= budget_tokens {
        return;
    }

    // The earliest turn-start whose suffix fits keeps the most history. If even
    // the last turn alone exceeds the budget, fall back to the last turn-start
    // (best effort — we never split below one turn).
    let mut last_start = None;
    let mut cut = None;
    for i in 0..n {
        if is_turn_start(&history[i]) {
            last_start = Some(i);
            if suffix[i] <= budget_tokens {
                cut = Some(i);
                break;
            }
        }
    }
    let cut = match cut.or(last_start) {
        Some(c) => c,
        None => return, // no safe boundary anywhere — leave intact
    };
    if cut > 0 {
        // Honesty marker (model-welfare H2): the window now starts later than the
        // agent remembers. Without a seam marker the model faces a silent hole and
        // does the worst thing — confabulates continuity. The marker names the
        // hole and points at the real record. Cumulative: an existing marker at
        // the front is folded into the new count rather than stacked.
        let prior = marker_dropped(&history[0]);
        let dropped_new = cut - usize::from(prior.is_some());
        let total = prior.unwrap_or(0) + dropped_new;
        history.drain(0..cut);
        history.insert(0, trim_marker(total));
    }
}

/// Prefix identifying the trim-seam honesty marker (also how successive trims
/// recognise + fold the previous marker instead of stacking new ones).
pub const TRIM_MARKER_PREFIX: &str = "[context-window notice: ";

fn trim_marker(dropped_total: usize) -> Message {
    Message::User {
        content: vec![ContentBlock::Text {
            text: format!(
                "{TRIM_MARKER_PREFIX}{dropped_total} earlier messages were trimmed from your \
                 working window to fit the context budget. This is a hole in the transcript you \
                 see, not in the record — the full history is preserved on disk for session \
                 replay, and your memory covers the period. Recall rather than reconstruct.]"
            ),
        }],
    }
}

/// If `m` is a trim marker, its cumulative dropped count.
fn marker_dropped(m: &Message) -> Option<usize> {
    let Message::User { content } = m else { return None };
    let [ContentBlock::Text { text }] = content.as_slice() else { return None };
    let rest = text.strip_prefix(TRIM_MARKER_PREFIX)?;
    rest.split_whitespace().next()?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn user(text: &str) -> Message {
        Message::User { content: vec![ContentBlock::Text { text: text.into() }] }
    }
    fn asst(text: &str) -> Message {
        Message::Assistant { content: vec![ContentBlock::Text { text: text.into() }] }
    }
    fn tool_call() -> Message {
        Message::Assistant {
            content: vec![ContentBlock::ToolUse { id: "t1".into(), name: "x".into(), input: json!({}) }],
        }
    }
    fn tool_res() -> Message {
        Message::User {
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "t1".into(),
                content: json!("ok"),
                is_error: false,
            }],
        }
    }

    fn has_orphan_tool_result(h: &[Message]) -> bool {
        h.iter().any(|m| matches!(m, Message::User { content }
            if content.iter().any(|b| matches!(b, ContentBlock::ToolResult { .. }))))
    }

    #[test]
    fn under_budget_is_noop() {
        let mut h = vec![user("hi"), asst("hello")];
        trim_history(&mut h, 100_000);
        assert_eq!(h.len(), 2);
    }

    #[test]
    fn zero_budget_disables_trim() {
        let mut h = vec![user(&"x".repeat(10_000)), asst("y")];
        trim_history(&mut h, 0);
        assert_eq!(h.len(), 2);
    }

    #[test]
    fn trims_oldest_turns_at_clean_boundaries() {
        // 3 turns; turn 1 contains a tool exchange (idx 0..=3).
        let mut h = vec![
            user(&"a".repeat(4000)), // ~1000 tok  idx0 (turn start)
            tool_call(),             //            idx1
            tool_res(),              //            idx2 (NOT a turn start)
            asst(&"b".repeat(4000)), // ~1000 tok  idx3
            user(&"c".repeat(400)),  // ~100 tok   idx4 (turn start)
            asst(&"d".repeat(400)),  // ~100 tok   idx5
            user(&"e".repeat(400)),  // ~100 tok   idx6 (turn start)
            asst(&"f".repeat(400)),  // ~100 tok   idx7
        ];
        // Budget fits the last two turns (~400 tok) but not turn 1 (~2000) → cut at idx4,
        // with the honesty marker inserted at the seam (+1 message).
        trim_history(&mut h, 600);
        assert_eq!(h.len(), 5);
        assert_eq!(marker_dropped(&h[0]), Some(4));
        assert!(is_turn_start(&h[1]));
        assert!(!has_orphan_tool_result(&h));
    }

    #[test]
    fn successive_trims_fold_the_marker_instead_of_stacking() {
        let mut h = vec![
            user(&"a".repeat(4000)),
            asst(&"b".repeat(4000)),
            user(&"c".repeat(400)),
            asst(&"d".repeat(400)),
        ];
        trim_history(&mut h, 300);
        assert_eq!(marker_dropped(&h[0]), Some(2));

        // Grow the conversation past budget again — the old marker must be FOLDED
        // (2 prior + 2 newly dropped real messages), never stacked twice.
        h.push(user(&"e".repeat(400)));
        h.push(asst(&"f".repeat(400)));
        trim_history(&mut h, 300);
        assert_eq!(marker_dropped(&h[0]), Some(4));
        assert_eq!(
            h.iter().filter(|m| marker_dropped(m).is_some()).count(),
            1,
            "exactly one marker at any time"
        );
        assert!(!has_orphan_tool_result(&h));
    }

    #[test]
    fn marker_counts_real_messages_not_itself() {
        // marker_dropped parses its own render round-trip.
        let m = trim_marker(7);
        assert_eq!(marker_dropped(&m), Some(7));
        // A normal user message is not a marker.
        assert_eq!(marker_dropped(&user("hello")), None);
    }

    #[test]
    fn never_cuts_between_tool_use_and_result() {
        // A budget that would naively cut at the tool_result must instead cut at
        // the next genuine user turn — never orphaning the tool_result.
        let mut h = vec![
            user(&"a".repeat(4000)),
            tool_call(),
            tool_res(),
            asst(&"b".repeat(4000)),
            user(&"c".repeat(40)),
            asst(&"d".repeat(40)),
        ];
        trim_history(&mut h, 100); // only the last tiny turn fits
        assert!(is_turn_start(&h[0]));
        assert!(!has_orphan_tool_result(&h));
    }

    #[test]
    fn keeps_last_turn_even_if_over_budget() {
        let mut h = vec![user("first"), asst("x"), user(&"huge".repeat(10_000)), asst("y")];
        trim_history(&mut h, 100);
        // Can't shrink below the last turn; keeps it, still starting at a user turn.
        assert!(is_turn_start(&h[0]));
        assert!(!has_orphan_tool_result(&h));
        assert!(!h.is_empty() && h.len() < 4);
    }
}
