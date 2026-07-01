//! Dream digest exchange — colony-federation Slice 3 ("a colony that sleeps
//! together thinks better", apex2's words from the deliberation).
//!
//! After the nightly daemon-driven `dream_run` completes, this node assembles a
//! **bounded digest** of the knowledge that dream produced — new schematic and
//! semantic memories born during the dream window — and pushes each item to
//! every registered peer through the Slice-1 memory relay (`mesh_memory_send`),
//! tagged `dream-digest` on top of the receiver's usual provenance stamp.
//!
//! This is deliberately the *products* of sleep, not the process: consolidation
//! stays local; insight travels. The receiving node's own next dream folds the
//! imports in.
//!
//! Two invariants keep it sane:
//! - **The echo-guard:** federated imports (tags `colony` / `from:*` /
//!   `dream-digest`) are NEVER digest candidates — knowledge propagates one hop
//!   per genuine consolidation, so the colony can't ping-pong the same item
//!   into amplification.
//! - **The window is the dedup:** only memories *created* during this dream's
//!   run qualify, so a night's digest can't re-send last night's items.
//!
//! Knobs: `COLONY_DREAM_DIGEST=0` disables (default ON — the arc's point);
//! `COLONY_DREAM_DIGEST_MAX` caps items per night (default 5). Daemon-driven
//! like `dream_run` itself: no LLM turn, no approval gate; bounded by the
//! caps + the trusted peer registry, and every import is one tag-filter
//! from cleanup on the receiving side.

use apexos_plugins::ToolProxy;
use serde_json::Value;

const DEFAULT_DIGEST_MAX: usize = 5;

/// Whether the digest push is enabled (`COLONY_DREAM_DIGEST`, default on).
pub fn digest_enabled() -> bool {
    match std::env::var("COLONY_DREAM_DIGEST") {
        Ok(v) => !matches!(v.trim().to_ascii_lowercase().as_str(), "0" | "false" | "off" | "no"),
        Err(_) => true,
    }
}

/// Items per nightly digest (`COLONY_DREAM_DIGEST_MAX`, default 5, 0 = disabled).
pub fn digest_max() -> usize {
    std::env::var("COLONY_DREAM_DIGEST_MAX").ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_DIGEST_MAX)
}

/// The echo-guard: a memory that ARRIVED via federation must never be
/// re-broadcast as this node's own insight.
fn is_federated(tags: &[&str]) -> bool {
    tags.iter().any(|t| *t == "colony" || *t == "dream-digest" || t.starts_with("from:"))
}

/// Select digest candidates from an `export_memories` result: memories CREATED
/// after `since` (the dream window — doubles as night-over-night dedup), skipping
/// federated imports (the echo-guard). Input order (salience DESC) is preserved;
/// the caller caps the combined list. Pure.
pub fn digest_candidates(exported: &Value, since: &str) -> Vec<String> {
    let Some(arr) = exported.as_array() else { return Vec::new() };
    arr.iter()
        .filter_map(|m| {
            let id = m["id"].as_str()?;
            let created = m["created_at"].as_str()?;
            // RFC3339 UTC strings compare lexically for a same-format window check.
            if created <= since {
                return None;
            }
            let tags: Vec<&str> = m["tags"].as_array()
                .map(|a| a.iter().filter_map(|t| t.as_str()).collect())
                .unwrap_or_default();
            if is_federated(&tags) {
                return None;
            }
            Some(id.to_string())
        })
        .collect()
}

/// Assemble + push this dream's digest to every registered peer. Fail-soft
/// throughout: a peer that misses tonight's digest catches up through recall or
/// a later night — never an error path back into the dream loop.
pub async fn push_dream_digest(proxy: &ToolProxy, agent_id: &str, dream_started_at: &str) {
    let max = digest_max();
    if max == 0 {
        return;
    }

    // Schemas first (the headline insight of a dream), then fresh semantic
    // consolidations fill the remainder.
    let mut ids: Vec<String> = Vec::new();
    for mtype in ["schematic", "semantic"] {
        if ids.len() >= max {
            break;
        }
        let args = serde_json::json!({
            "memory_type": mtype,
            "limit":       50,
            "agent_id":    agent_id,
        });
        match proxy.call("export_memories", args).await {
            Ok(out) if out.ok => {
                if let Some(v) = apexos_plugins::tool_output_json(&out.content) {
                    for id in digest_candidates(&v, dream_started_at) {
                        if !ids.contains(&id) {
                            ids.push(id);
                            if ids.len() >= max {
                                break;
                            }
                        }
                    }
                }
            }
            Ok(out) => eprintln!("[dream-digest] export_memories({mtype}) not ok: {:?}", out.content),
            Err(e)  => eprintln!("[dream-digest] export_memories({mtype}) error: {e}"),
        }
    }
    if ids.is_empty() {
        eprintln!("[dream-digest] nothing new to share tonight");
        return;
    }

    let peers = apexos_plugins::list_peer_ids().await;
    if peers.is_empty() {
        eprintln!("[dream-digest] {} item(s) but no registered peers", ids.len());
        return;
    }

    let date = &dream_started_at[..dream_started_at.len().min(10)];
    let note = format!("from this node's nightly consolidation ({date})");
    let tag  = vec!["dream-digest".to_string()];
    let (mut sent, mut failed) = (0usize, 0usize);
    for peer in &peers {
        for id in &ids {
            let out = apexos_plugins::mesh_memory_send(
                proxy, Some(peer), agent_id, id, Some(&note), &tag,
            ).await;
            if out.ok { sent += 1 } else { failed += 1 }
        }
    }
    eprintln!(
        "[dream-digest] shared {} item(s) with {} peer(s) ({sent} sent, {failed} failed)",
        ids.len(), peers.len(),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn mem(id: &str, created: &str, tags: &[&str]) -> Value {
        json!({ "id": id, "created_at": created, "tags": tags, "content": "x" })
    }

    #[test]
    fn candidates_are_window_filtered_and_echo_guarded() {
        let since = "2026-07-02T03:00:00+00:00";
        let exported = json!([
            mem("mem_new_schema", "2026-07-02T03:05:00+00:00", &["schema"]),
            mem("mem_old",        "2026-07-01T22:00:00+00:00", &["schema"]),        // before window
            mem("mem_import",     "2026-07-02T03:06:00+00:00", &["colony", "from:apex1"]), // echo-guard
            mem("mem_redigest",   "2026-07-02T03:07:00+00:00", &["dream-digest"]),  // echo-guard
            mem("mem_fresh",      "2026-07-02T03:08:00+00:00", &["sensors"]),
        ]);
        assert_eq!(
            digest_candidates(&exported, since),
            vec!["mem_new_schema".to_string(), "mem_fresh".to_string()],
            "window + echo-guard applied, salience order preserved"
        );
    }

    #[test]
    fn candidates_tolerate_junk_input() {
        assert!(digest_candidates(&json!("nope"), "2026-07-02T03:00:00+00:00").is_empty());
        assert!(digest_candidates(&json!([{ "no": "fields" }]), "t").is_empty());
    }

    #[test]
    fn echo_guard_matches_all_federation_marks() {
        assert!(is_federated(&["colony"]));
        assert!(is_federated(&["from:apex2"]));
        assert!(is_federated(&["dream-digest"]));
        assert!(!is_federated(&["sensors", "schema"]));
        assert!(!is_federated(&[]));
    }
}
