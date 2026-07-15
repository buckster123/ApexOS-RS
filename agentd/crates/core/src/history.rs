//! History windowing + integrity repair for long-lived sessions.
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
//!
//! [`repair_history`] is the load-time twin: a session JSONL written by racing
//! append tasks (or truncated by a crash/abort mid-batch) can reload in an order
//! the provider API rejects — a `tool_use` split from its `tool_result`, an
//! empty `content: []` message, consecutive same-role messages. One poisoned
//! file permanently wedges its session: every turn re-sends the corrupt history
//! and 400s before the model runs. Repair restores API validity (honest markers
//! where content is genuinely lost, never silent deletion of real content) so a
//! corrupt file costs at most one scrambled exchange, not the whole thread.

use crate::{ContentBlock, Message};
use std::collections::VecDeque;

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

// ── Load-time integrity repair ────────────────────────────────────────────────

/// Marker text for a `tool_result` synthesized at load time because the real one
/// never reached the file (crash/abort between appends). Honest-marker
/// discipline: the model is told the result is *lost*, not what it was.
pub const LOST_RESULT_MARKER: &str =
    "⊘ result lost — the session file was recovered after an interrupted write; \
     the call ran but its result was not persisted. Verify its effects rather \
     than assuming failure.";

fn content_of(m: &Message) -> &Vec<ContentBlock> {
    match m {
        Message::User { content } | Message::Assistant { content } => content,
    }
}

/// `tool_use` ids carried by an assistant message (empty for user messages).
fn tool_use_ids(m: &Message) -> Vec<String> {
    match m {
        Message::Assistant { content } => content.iter()
            .filter_map(|b| match b {
                ContentBlock::ToolUse { id, .. } => Some(id.clone()),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}

/// Whether `m` is a user message carrying a `tool_result` for any id in `ids`.
fn answers_any(m: &Message, ids: &[String]) -> bool {
    matches!(m, Message::User { content }
        if content.iter().any(|b| matches!(b, ContentBlock::ToolResult { tool_use_id, .. }
            if ids.iter().any(|i| i == tool_use_id))))
}

fn lost_result_block(id: &str) -> ContentBlock {
    ContentBlock::ToolResult {
        tool_use_id: id.to_string(),
        content:     serde_json::json!(LOST_RESULT_MARKER),
        is_error:    true,
    }
}

/// Normalize the user message that answers `use_ids`: results the pair actually
/// owns stay, alien results (their `tool_use` is elsewhere/gone — the API rejects
/// them here) are dropped, and any id of the pair with no surviving result gets a
/// synthesized lost-result marker. Non-result blocks (interleaved user text) ride
/// along after the results, as the API requires. Already-canonical messages are
/// returned untouched (the no-op guarantee for clean histories).
fn normalize_pair_answer(mut msg: Message, use_ids: &[String], changed: &mut bool) -> Message {
    let Message::User { content } = &mut msg else { return msg };
    let result_ids: Vec<&str> = content.iter()
        .filter_map(|b| match b {
            ContentBlock::ToolResult { tool_use_id, .. } => Some(tool_use_id.as_str()),
            _ => None,
        })
        .collect();
    let results_first = content.iter()
        .skip_while(|b| matches!(b, ContentBlock::ToolResult { .. }))
        .all(|b| !matches!(b, ContentBlock::ToolResult { .. }));
    let no_alien = result_ids.iter().all(|i| use_ids.iter().any(|u| u == i));
    let full     = use_ids.iter().all(|u| result_ids.iter().any(|i| i == u));
    if results_first && no_alien && full {
        return msg;
    }
    *changed = true;
    let mut results: Vec<ContentBlock> = Vec::new();
    let mut rest:    Vec<ContentBlock> = Vec::new();
    let mut covered: Vec<String>       = Vec::new();
    for b in content.drain(..) {
        match &b {
            ContentBlock::ToolResult { tool_use_id, .. } => {
                if use_ids.iter().any(|i| i == tool_use_id) {
                    covered.push(tool_use_id.clone());
                    results.push(b);
                } // alien result — its tool_use is not this pair; dropped
            }
            _ => rest.push(b),
        }
    }
    for id in use_ids {
        if !covered.iter().any(|c| c == id) {
            results.push(lost_result_block(id));
        }
    }
    results.extend(rest);
    *content = results;
    msg
}

/// Repair a history reloaded from disk into a shape the provider API accepts.
/// Returns `true` when anything changed. Guarantees, in order:
///
/// 1. no empty-content messages (an interrupted stream once persisted
///    `{"role":"assistant","content":[]}`);
/// 2. every assistant `tool_use` is immediately followed by the user message
///    carrying its `tool_result`s — an interleaved message (the append race) is
///    moved after the pair; a result missing from the file entirely is
///    synthesized as an honest [`LOST_RESULT_MARKER`];
/// 3. no stray `tool_result`s (their `tool_use` gone — e.g. a truncated head);
/// 4. the history opens with a user message.
///
/// Deliberately minimal: it fixes only shapes the API actually rejects.
/// Consecutive same-role messages are left alone — an errored turn already
/// leaves `user,user` on disk today and the providers accept it, so merging
/// would mutate benign histories. A clean history is returned byte-identical —
/// repair is a no-op on every session the (now-ordered) live persist path writes.
pub fn repair_history(history: &mut Vec<Message>) -> bool {
    let mut changed = false;

    // (1) Drop empty messages.
    let before = history.len();
    history.retain(|m| !content_of(m).is_empty());
    changed |= history.len() != before;

    // (2)+(3) Restore tool_use→tool_result adjacency; strip strays.
    let mut queue: VecDeque<Message> = std::mem::take(history).into();
    let mut out: Vec<Message> = Vec::with_capacity(queue.len());
    while let Some(msg) = queue.pop_front() {
        let use_ids = tool_use_ids(&msg);
        if use_ids.is_empty() {
            // Normal flow: any tool_result seen here is a stray — its tool_use
            // was consumed by a pairing below or never made the file. Keep the
            // message's other content (it may be real user text).
            if let Message::User { content } = &msg {
                if content.iter().any(|b| matches!(b, ContentBlock::ToolResult { .. })) {
                    changed = true;
                    let kept: Vec<ContentBlock> = content.iter()
                        .filter(|b| !matches!(b, ContentBlock::ToolResult { .. }))
                        .cloned().collect();
                    if !kept.is_empty() {
                        out.push(Message::User { content: kept });
                    }
                    continue;
                }
            }
            out.push(msg);
            continue;
        }
        // Assistant with tool_use: its answer must come next.
        out.push(msg);
        match queue.iter().position(|m| answers_any(m, &use_ids)) {
            Some(k) => {
                if k > 0 {
                    changed = true;
                }
                let displaced: Vec<Message> = queue.drain(0..k).collect();
                let answer = queue.pop_front().expect("position() guarantees presence");
                out.push(normalize_pair_answer(answer, &use_ids, &mut changed));
                // Displaced messages follow the pair, order preserved.
                for m in displaced.into_iter().rev() {
                    queue.push_front(m);
                }
            }
            None => {
                // No result anywhere in the file — synthesize the honest loss.
                changed = true;
                out.push(Message::User { content: use_ids.iter().map(|i| lost_result_block(i)).collect() });
            }
        }
    }

    // (4) The API requires the first message to be a user message. A truncated
    // head (assistant-first) gets an honest opening marker.
    if matches!(out.first(), Some(Message::Assistant { .. })) {
        changed = true;
        out.insert(0, Message::User {
            content: vec![ContentBlock::Text {
                text: "⊘ (session opening lost — the file head was truncated; \
                       earlier context is on disk replay only)".into(),
            }],
        });
    }

    *history = out;
    changed
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

    // ── repair_history ──────────────────────────────────────────────────────

    fn tool_call_id(id: &str) -> Message {
        Message::Assistant {
            content: vec![
                ContentBlock::Text { text: "on it".into() },
                ContentBlock::ToolUse { id: id.into(), name: "x".into(), input: json!({}) },
            ],
        }
    }
    fn tool_res_id(id: &str) -> Message {
        Message::User {
            content: vec![ContentBlock::ToolResult {
                tool_use_id: id.into(),
                content: json!("ok"),
                is_error: false,
            }],
        }
    }
    fn empty_asst() -> Message {
        Message::Assistant { content: vec![] }
    }

    /// Every assistant tool_use must be answered by tool_results for exactly its
    /// ids in the immediately-next message — the invariant the API 400s on.
    fn pairing_ok(h: &[Message]) -> bool {
        for (i, m) in h.iter().enumerate() {
            let uses = tool_use_ids(m);
            if uses.is_empty() {
                continue;
            }
            let Some(Message::User { content }) = h.get(i + 1) else { return false };
            let answered: Vec<&str> = content.iter()
                .filter_map(|b| match b {
                    ContentBlock::ToolResult { tool_use_id, .. } => Some(tool_use_id.as_str()),
                    _ => None,
                })
                .collect();
            if !uses.iter().all(|u| answered.iter().any(|a| a == u)) {
                return false;
            }
        }
        true
    }

    fn dump(h: &[Message]) -> String {
        serde_json::to_string(h).unwrap()
    }

    #[test]
    fn repair_is_a_noop_on_clean_history() {
        // A representative healthy session: plain turns + a proper tool exchange.
        let mut h = vec![
            user("hi"), asst("hello"),
            user("do it"), tool_call_id("t1"), tool_res_id("t1"), asst("done"),
            // an errored turn once left user,user on disk — benign, accepted live,
            // and must stay untouched
            user("retry?"), user("still there?"), asst("yes"),
        ];
        let before = dump(&h);
        assert!(!repair_history(&mut h), "clean history must not report repair");
        assert_eq!(dump(&h), before, "clean history must be byte-identical");
    }

    #[test]
    fn repair_heals_the_session_35_interleave() {
        // The exact field shape (apex1 s35): a queued prompt's user message raced
        // into the turn delta between tool_use and tool_result, plus an empty
        // assistant message — valid in memory that day, a 400 wedge after reload.
        let mut h = vec![
            user("earlier"),
            tool_call_id("t1"),      // 108: thinking+text+tool_use
            user("you here fren"),   // 109: the interleaved human prompt
            tool_res_id("t1"),       // 110: the pair's real result
            empty_asst(),            // 111: {"role":"assistant","content":[]}
            asst("Yeah, here. What's up?"), // 112
        ];
        assert!(repair_history(&mut h));
        assert!(pairing_ok(&h), "tool pairing must be adjacent after repair: {}", dump(&h));
        // The displaced prompt survives, after the pair, before its answer.
        let json = dump(&h);
        assert!(json.contains("you here fren"), "interleaved user text must never be dropped");
        assert!(!json.contains("\"content\":[]"), "empty messages must be gone");
        let displaced_pos = h.iter().position(|m| dump(std::slice::from_ref(m)).contains("you here fren")).unwrap();
        let result_pos    = h.iter().position(|m| matches!(m, Message::User { content }
            if content.iter().any(|b| matches!(b, ContentBlock::ToolResult { .. })))).unwrap();
        assert!(displaced_pos > result_pos, "displaced prompt follows the tool pair");
    }

    #[test]
    fn repair_synthesizes_lost_result_for_orphaned_tool_use() {
        // Crash between delta appends: tool_use persisted, result never was.
        let mut h = vec![user("go"), tool_call_id("t1"), asst("anyway")];
        assert!(repair_history(&mut h));
        assert!(pairing_ok(&h), "{}", dump(&h));
        assert!(dump(&h).contains("result lost"), "the synthesized result is an honest marker");
        // is_error so the model treats it as a failure signal, not real output.
        assert!(matches!(&h[2], Message::User { content }
            if matches!(&content[0], ContentBlock::ToolResult { is_error: true, .. })));
    }

    #[test]
    fn repair_fills_partially_lost_multi_tool_round() {
        // Two tool_uses, only one result survived the write.
        let mut h = vec![
            Message::Assistant { content: vec![
                ContentBlock::ToolUse { id: "t1".into(), name: "a".into(), input: json!({}) },
                ContentBlock::ToolUse { id: "t2".into(), name: "b".into(), input: json!({}) },
            ]},
            tool_res_id("t1"),
            asst("carrying on"),
        ];
        assert!(repair_history(&mut h));
        assert!(pairing_ok(&h), "{}", dump(&h));
        assert!(dump(&h).contains("result lost"));
    }

    #[test]
    fn repair_strips_stray_tool_results() {
        // A result whose tool_use never made the file (truncated head): the API
        // rejects it, so the block goes; real user text in the message survives.
        let mut h = vec![
            Message::User { content: vec![
                ContentBlock::ToolResult { tool_use_id: "ghost".into(), content: json!("x"), is_error: false },
                ContentBlock::Text { text: "and also".into() },
            ]},
            asst("ok"),
        ];
        assert!(repair_history(&mut h));
        let json = dump(&h);
        assert!(!json.contains("ghost"));
        assert!(json.contains("and also"));
        assert!(pairing_ok(&h));
    }

    #[test]
    fn repair_marks_a_truncated_head() {
        // Assistant-first violates the API's user-first rule → honest opener.
        let mut h = vec![asst("mid-conversation"), user("next"), asst("reply")];
        assert!(repair_history(&mut h));
        assert!(matches!(&h[0], Message::User { .. }));
        assert!(dump(std::slice::from_ref(&h[0])).contains("session opening lost"));
    }

    #[test]
    fn repair_drops_empty_messages_everywhere() {
        let mut h = vec![user("a"), empty_asst(), asst("b"),
                         Message::User { content: vec![] }, user("c"), asst("d")];
        assert!(repair_history(&mut h));
        assert_eq!(h.len(), 4);
        assert!(!dump(&h).contains("\"content\":[]"));
    }

    /// Forensic hook: point `APEXOS_REPAIR_CHECK_FILE` at a real session JSONL
    /// (e.g. one pulled off a wedged node) to verify it heals into a shape that
    /// passes the pairing invariant. Skips silently when the var is unset, so it
    /// costs nothing in CI.
    ///
    ///   APEXOS_REPAIR_CHECK_FILE=/tmp/35.jsonl cargo test -p apexos-core repair_check_file -- --nocapture
    #[test]
    fn repair_check_file() {
        let Ok(path) = std::env::var("APEXOS_REPAIR_CHECK_FILE") else { return };
        let text = std::fs::read_to_string(&path).expect("readable session file");
        let mut h: Vec<Message> = text.lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect();
        let n = h.len();
        let changed = repair_history(&mut h);
        println!("{path}: {n} messages → {} after repair (changed: {changed})", h.len());
        assert!(pairing_ok(&h), "file did not heal to valid pairing");
        assert!(!dump(&h).contains("\"content\":[]"), "empty messages survived");
    }
}
