//! Pure state-machine core for self-evolution — the rollback safety net, extracted
//! from the daemon loop so it can be reasoned about and **tested** in isolation.
//!
//! Everything here is pure: no IO, no async, no channels. The orchestration in
//! `main.rs` (`spawn_evolution_applier`, `compute_undo`, `apply_evolution`) reads
//! the on-disk state, calls into these functions, and performs the writes/effects.
//! The split is deliberate — the tricky logic (what inverts to what, the undo-snapshot
//! wire format, the TOML edits + validate-before-persist) lives here under test; the
//! glue stays thin.
//!
//! See [`PATTERNS.md`](../../../../PATTERNS.md) — this is the "extract the pure core"
//! smoothing of the 🔴 self-evolution entry.

use std::collections::HashMap;

use anyhow::Result;
use toml_edit::{DocumentMut, InlineTable, Item, Table, Value};

use apexos_core::{EvolutionId, EvolutionProposal, PolicyRule};
use apexos_plugins::PolicyConfig;

// ── classification ───────────────────────────────────────────────────────────

/// The `kind` discriminant of a proposal (used for episode titles + tags). Reads
/// the serde tag, so it can never drift from the enum's wire form.
pub fn kind(proposal: &EvolutionProposal) -> String {
    serde_json::to_value(proposal)
        .ok()
        .and_then(|v| v.get("kind").and_then(|k| k.as_str()).map(str::to_owned))
        .unwrap_or_else(|| "unknown".into())
}

/// Parse the `EvolutionId` out of an episode title of the form `"evolution {N}: {kind}"`.
pub fn parse_evolution_id_from_title(title: &str) -> Option<EvolutionId> {
    let rest = title.strip_prefix("evolution ")?;
    let colon = rest.find(':')?;
    let n: u64 = rest[..colon].trim().parse().ok()?;
    Some(EvolutionId(n))
}

// ── undo-snapshot codec ──────────────────────────────────────────────────────
//
// The rollback snapshot is persisted as one line inside a Cerebro episode memory.
// `undo_step_line` writes it; `parse_undo_snapshot_from_text` reads it back. They
// are a 1:1 pair — the round-trip test below is the contract.

/// Build the episode-step content that carries an undo snapshot. The snapshot is
/// compact JSON on its own marker line; `serde_json::to_string` escapes any newline
/// in the payload, so the snapshot is always single-line and recoverable.
pub fn undo_step_line(summary: &str, undo: &EvolutionProposal) -> String {
    let undo_json = serde_json::to_string(undo).unwrap_or_default();
    format!("evolution apply: {summary}\nundo_snapshot: {undo_json}")
}

/// Recover an undo snapshot from a single memory's `content` string.
pub fn parse_undo_snapshot_from_text(text: &str) -> Option<EvolutionProposal> {
    let marker = "undo_snapshot: ";
    let start = text.find(marker)? + marker.len();
    let rest = &text[start..];
    let end = rest.find('\n').unwrap_or(rest.len());
    serde_json::from_str(&rest[..end]).ok()
}

/// Recover an undo snapshot from a `get_episode_memories` result — a JSON array of
/// memory nodes. The marker lives inside each node's `content` string, NOT in the
/// rendered array text (where the undo JSON is escaped-within-JSON and never parses
/// — the bug behind the chronic "loaded 0 rollback snapshot(s)").
pub fn parse_undo_from_episode_memories(mems_text: &str) -> Option<EvolutionProposal> {
    serde_json::from_str::<serde_json::Value>(mems_text)
        .ok()?
        .as_array()?
        .iter()
        .filter_map(|n| n["content"].as_str())
        .find_map(parse_undo_snapshot_from_text)
}

// ── inverse (rollback planning) ──────────────────────────────────────────────

/// The prior on-disk values an apply will overwrite — captured by the glue (via IO)
/// before applying, then fed to [`invert`]. `None` for a field means "couldn't read
/// it / it didn't exist", which makes the corresponding inverse non-existent.
#[derive(Default, Debug, Clone)]
pub struct Prior {
    /// The soul content that `UpdateSystemPrompt` will replace.
    pub old_soul: Option<String>,
    /// The `[rules]` value that `UpdatePolicyRule` will replace (None ⇒ brand-new rule).
    pub old_policy_rule: Option<PolicyRule>,
    /// The `cmd` of the plugin that `UnregisterMcpServer` will remove.
    pub old_plugin_cmd: Option<String>,
}

/// Produce the inverse proposal that rolls `proposal` back, given the captured
/// [`Prior`]. Returns `None` when there is no meaningful undo: a hot-reload (no
/// state change), a hardware request (a record, not a mutation), or a mutation
/// whose prior value was unavailable (e.g. a brand-new policy rule has no inverse —
/// there is no "remove rule" variant).
pub fn invert(proposal: &EvolutionProposal, prior: &Prior) -> Option<EvolutionProposal> {
    match proposal {
        EvolutionProposal::UpdateSystemPrompt { .. } => Some(EvolutionProposal::UpdateSystemPrompt {
            content: prior.old_soul.clone()?,
            reason: "rollback".into(),
        }),
        EvolutionProposal::UpdatePolicyRule { tool_pattern, .. } => {
            Some(EvolutionProposal::UpdatePolicyRule {
                tool_pattern: tool_pattern.clone(),
                new_rule: prior.old_policy_rule?,
                reason: "rollback".into(),
            })
        }
        EvolutionProposal::RegisterMcpServer { name, .. } => {
            Some(EvolutionProposal::UnregisterMcpServer {
                name: name.clone(),
                reason: "rollback".into(),
            })
        }
        EvolutionProposal::UnregisterMcpServer { name, .. } => {
            Some(EvolutionProposal::RegisterMcpServer {
                name: name.clone(),
                command: prior.old_plugin_cmd.clone()?,
                env: HashMap::new(),
                reason: "rollback".into(),
            })
        }
        EvolutionProposal::HotReloadSubsystem { .. } => None,
        EvolutionProposal::RequestHardware { .. } => None,
    }
}

// ── TOML readers (used to build `Prior`) ─────────────────────────────────────

/// Read the current `[rules]` value for `tool_pattern` out of policy.toml text.
pub fn policy_rule_from_toml(toml_text: &str, tool_pattern: &str) -> Option<PolicyRule> {
    let doc = toml_text.parse::<DocumentMut>().ok()?;
    let s = doc.get("rules")?.as_table()?.get(tool_pattern)?.as_str()?;
    PolicyRule::from_toml_str(s)
}

/// Read the `cmd` of the plugin with id `name` out of plugins.toml text.
pub fn plugin_cmd_from_toml(toml_text: &str, name: &str) -> Option<String> {
    let doc = toml_text.parse::<DocumentMut>().ok()?;
    let arr = doc.get("plugin")?.as_array_of_tables()?;
    let tbl = arr
        .iter()
        .find(|t| t.get("id").and_then(|v| v.as_str()) == Some(name))?;
    tbl.get("cmd")?.as_str().map(str::to_owned)
}

// ── TOML transforms (the apply edits — pure string → string) ─────────────────

/// Set `tool_pattern = rule` in the `[rules]` table, then **validate** the candidate
/// document by parsing it into a [`PolicyConfig`] — so a malformed edit can never
/// reach disk. Returns the new TOML text and the parsed config (the live engine is
/// rebuilt from it). Creates `[rules]` if absent, so a brand-new rule can't silently
/// no-op.
pub fn policy_toml_set_rule(
    toml_text: &str,
    tool_pattern: &str,
    rule: PolicyRule,
) -> Result<(String, PolicyConfig)> {
    let mut doc = toml_text.parse::<DocumentMut>()?;
    if doc.get("rules").is_none() {
        doc["rules"] = Item::Table(Table::new());
    }
    if let Some(rules) = doc.get_mut("rules").and_then(|v| v.as_table_mut()) {
        rules.insert(tool_pattern, toml_edit::value(rule.as_toml_str()));
    }
    let new_toml = doc.to_string();
    let config = PolicyConfig::parse(&new_toml)
        .map_err(|e| anyhow::anyhow!("rejected policy edit (would corrupt policy.toml): {e}"))?;
    Ok((new_toml, config))
}

/// Append a `[[plugin]]` table (id/cmd/restart=always, + inline env if non-empty).
pub fn plugins_toml_add(
    toml_text: &str,
    name: &str,
    command: &str,
    env: &HashMap<String, String>,
) -> Result<String> {
    let mut doc = toml_text.parse::<DocumentMut>()?;
    if let Some(arr) = doc.get_mut("plugin").and_then(|v| v.as_array_of_tables_mut()) {
        let mut tbl = Table::new();
        tbl.insert("id", toml_edit::value(name));
        tbl.insert("cmd", toml_edit::value(command));
        tbl.insert("restart", toml_edit::value("always"));
        if !env.is_empty() {
            let mut env_inline = InlineTable::new();
            for (k, v) in env {
                env_inline.insert(k, Value::from(v.as_str()));
            }
            tbl.insert("env", Item::Value(Value::InlineTable(env_inline)));
        }
        arr.push(tbl);
    }
    Ok(doc.to_string())
}

/// Remove the `[[plugin]]` table whose `id` is `name` (no-op if absent).
pub fn plugins_toml_remove(toml_text: &str, name: &str) -> Result<String> {
    let mut doc = toml_text.parse::<DocumentMut>()?;
    if let Some(arr) = doc.get_mut("plugin").and_then(|v| v.as_array_of_tables_mut()) {
        let idx = (0..arr.len()).find(|&i| {
            arr.get(i).and_then(|t| t.get("id")).and_then(|v| v.as_str()) == Some(name)
        });
        if let Some(i) = idx {
            arr.remove(i);
        }
    }
    Ok(doc.to_string())
}

// ── hardware wishlist ────────────────────────────────────────────────────────

/// Header seeded into a fresh hardware wishlist.
pub const WISHLIST_HEADER: &str = "# ApexOS hardware wishlist\n\n\
    APEX's request-to-incarnate queue (EDK, docs/edk.md). Each entry is a part\n\
    APEX asked for; a human seats it, reboots, and the embodiment probe confirms\n\
    the new sense live. Remove an entry once it's installed.\n";

/// Append a hardware request entry to the wishlist (seeding the header if absent).
#[allow(clippy::too_many_arguments)]
pub fn wishlist_append(
    existing: Option<&str>,
    id: u64,
    part: &str,
    capability: &str,
    reason: &str,
    bus: &str,
    source: &str,
) -> String {
    let mut doc = existing
        .map(str::to_owned)
        .unwrap_or_else(|| WISHLIST_HEADER.to_string());
    let bus_line = if bus.is_empty() { String::new() } else { format!("- attaches: {bus}\n") };
    let source_line = if source.is_empty() { String::new() } else { format!("- source: {source}\n") };
    doc.push_str(&format!(
        "\n## [#{id}] {part} → {capability}\n{bus_line}{source_line}- why: {reason}\n\
         - status: REQUESTED — seat it, reboot; the embodiment probe confirms it live\n",
    ));
    doc
}

#[cfg(test)]
mod tests {
    use super::*;

    // Proposals aren't PartialEq (they carry HashMaps); compare by wire value.
    fn json_eq(a: &EvolutionProposal, b: &EvolutionProposal) -> bool {
        serde_json::to_value(a).unwrap() == serde_json::to_value(b).unwrap()
    }

    #[test]
    fn kind_reads_the_serde_tag() {
        let p = EvolutionProposal::UpdateSystemPrompt { content: "x".into(), reason: "r".into() };
        assert_eq!(kind(&p), "update_system_prompt");
        let p = EvolutionProposal::UpdatePolicyRule {
            tool_pattern: "git_push".into(), new_rule: PolicyRule::Allow, reason: "r".into(),
        };
        assert_eq!(kind(&p), "update_policy_rule");
        let p = EvolutionProposal::RequestHardware {
            part: "p".into(), capability: "c".into(), reason: "r".into(),
            bus: String::new(), source: String::new(),
        };
        assert_eq!(kind(&p), "request_hardware");
    }

    #[test]
    fn parse_id_from_title() {
        assert_eq!(parse_evolution_id_from_title("evolution 42: update_policy_rule"), Some(EvolutionId(42)));
        assert_eq!(parse_evolution_id_from_title("evolution  7 : x"), Some(EvolutionId(7)));
        assert_eq!(parse_evolution_id_from_title("not an evolution title"), None);
        assert_eq!(parse_evolution_id_from_title("evolution X: x"), None);
    }

    #[test]
    fn undo_snapshot_round_trips() {
        let undo = EvolutionProposal::UpdateSystemPrompt {
            content: "old soul\nwith a newline".into(), reason: "rollback".into(),
        };
        let line = undo_step_line("system prompt updated (8 chars)", &undo);
        let parsed = parse_undo_snapshot_from_text(&line).expect("should parse back");
        assert!(json_eq(&parsed, &undo), "round-trip must preserve the snapshot");
    }

    #[test]
    fn undo_snapshot_recovered_from_episode_memories_array() {
        // The shape get_episode_memories returns: a JSON array of memory nodes,
        // the marker living inside each node's `content` string.
        let undo = EvolutionProposal::UpdatePolicyRule {
            tool_pattern: "git_push".into(), new_rule: PolicyRule::Ask, reason: "rollback".into(),
        };
        let content = undo_step_line("policy rule 'git_push' set to 'allow'", &undo);
        let mems = serde_json::json!([
            { "id": "mem_noise", "content": "unrelated memory, no marker here" },
            { "id": "mem_x", "content": content },
        ])
        .to_string();
        let parsed = parse_undo_from_episode_memories(&mems).expect("should find the snapshot");
        assert!(json_eq(&parsed, &undo));
        // A rendered array string with no marker yields nothing.
        assert!(parse_undo_from_episode_memories("[]").is_none());
    }

    #[test]
    fn invert_is_the_inverse_for_each_kind() {
        // soul: needs the prior content.
        let p = EvolutionProposal::UpdateSystemPrompt { content: "new".into(), reason: "r".into() };
        let prior = Prior { old_soul: Some("old".into()), ..Default::default() };
        let inv = invert(&p, &prior).unwrap();
        assert!(json_eq(&inv, &EvolutionProposal::UpdateSystemPrompt {
            content: "old".into(), reason: "rollback".into(),
        }));
        // soul with no captured prior ⇒ no undo.
        assert!(invert(&p, &Prior::default()).is_none());

        // policy: restores the prior rule; brand-new rule (no prior) ⇒ no undo.
        let p = EvolutionProposal::UpdatePolicyRule {
            tool_pattern: "git_push".into(), new_rule: PolicyRule::Allow, reason: "r".into(),
        };
        let prior = Prior { old_policy_rule: Some(PolicyRule::Ask), ..Default::default() };
        let inv = invert(&p, &prior).unwrap();
        assert!(json_eq(&inv, &EvolutionProposal::UpdatePolicyRule {
            tool_pattern: "git_push".into(), new_rule: PolicyRule::Ask, reason: "rollback".into(),
        }));
        assert!(invert(&p, &Prior::default()).is_none());

        // register ⇄ unregister.
        let p = EvolutionProposal::RegisterMcpServer {
            name: "occipital".into(), command: "occipital-mcp".into(),
            env: HashMap::new(), reason: "r".into(),
        };
        assert!(json_eq(&invert(&p, &Prior::default()).unwrap(),
            &EvolutionProposal::UnregisterMcpServer { name: "occipital".into(), reason: "rollback".into() }));

        let p = EvolutionProposal::UnregisterMcpServer { name: "occipital".into(), reason: "r".into() };
        let prior = Prior { old_plugin_cmd: Some("occipital-mcp".into()), ..Default::default() };
        assert!(json_eq(&invert(&p, &prior).unwrap(), &EvolutionProposal::RegisterMcpServer {
            name: "occipital".into(), command: "occipital-mcp".into(),
            env: HashMap::new(), reason: "rollback".into(),
        }));

        // not reversible.
        assert!(invert(&EvolutionProposal::HotReloadSubsystem {
            subsystem: apexos_core::Subsystem::Policy }, &Prior::default()).is_none());
        assert!(invert(&EvolutionProposal::RequestHardware {
            part: "p".into(), capability: "c".into(), reason: "r".into(),
            bus: String::new(), source: String::new() }, &Prior::default()).is_none());
    }

    #[test]
    fn policy_set_rule_creates_table_and_validates() {
        // Empty doc: [rules] must be created and the edit must validate.
        let (new_toml, _cfg) = policy_toml_set_rule("", "git_push", PolicyRule::Allow).unwrap();
        assert!(new_toml.contains("git_push"));
        assert!(new_toml.contains("allow"));
        // The round trip: read it back.
        assert_eq!(policy_rule_from_toml(&new_toml, "git_push"), Some(PolicyRule::Allow));
        // Overwrite an existing rule.
        let (newer, _) = policy_toml_set_rule(&new_toml, "git_push", PolicyRule::Ask).unwrap();
        assert_eq!(policy_rule_from_toml(&newer, "git_push"), Some(PolicyRule::Ask));
    }

    #[test]
    fn plugins_add_then_remove_and_read_cmd() {
        let base = "[[plugin]]\nid = \"existing\"\ncmd = \"existing-cmd\"\n";
        let added = plugins_toml_add(base, "occipital", "occipital-mcp", &HashMap::new()).unwrap();
        assert_eq!(plugin_cmd_from_toml(&added, "occipital").as_deref(), Some("occipital-mcp"));
        assert_eq!(plugin_cmd_from_toml(&added, "existing").as_deref(), Some("existing-cmd"));
        let removed = plugins_toml_remove(&added, "occipital").unwrap();
        assert!(plugin_cmd_from_toml(&removed, "occipital").is_none());
        assert!(plugin_cmd_from_toml(&removed, "existing").is_some());
    }

    #[test]
    fn wishlist_seeds_header_then_appends() {
        let first = wishlist_append(None, 3, "AMG8833", "thermal eyes", "see heat", "i2c", "inventory:amg8833");
        assert!(first.starts_with("# ApexOS hardware wishlist"));
        assert!(first.contains("[#3] AMG8833 → thermal eyes"));
        assert!(first.contains("- attaches: i2c"));
        assert!(first.contains("- source: inventory:amg8833"));
        let second = wishlist_append(Some(&first), 4, "mic", "hearing", "hear", "", "");
        assert!(second.contains("[#3] AMG8833")); // first entry preserved
        assert!(second.contains("[#4] mic → hearing"));
        assert!(!second.contains("- attaches: \n")); // empty bus/source omit their lines
    }
}
