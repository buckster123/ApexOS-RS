use std::collections::HashMap;
use std::path::Path;
use serde::Deserialize;
// ── config types (loaded from policy.toml) ────────────────────────────────────

// PolicyMode lives in apexos_core so EvolutionProposal can reference it
// without a circular dep. Re-exported here for call-site convenience.
pub use apexos_core::PolicyMode;

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(rename_all = "kebab-case")]
pub enum Rule {
    /// Auto-approve regardless of mode (overridden by Yolo).
    Allow,
    /// Always ask (overridden by Yolo).
    Ask,
    /// Auto if the path resolves inside AGENTD_WORKSPACE, else ask — see
    /// `workspace_decision` (canonicalized target, `..` rejected, fail-closed
    /// when nothing resolves). The supervisor feeds it every path-typed arg.
    Workspace,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Decision { Allow, Ask }

// NOTE: do NOT `#[derive(Default)]` here. PolicyConfig.subagents is
// `#[serde(default)]`, so a policy.toml with NO `[subagents]` table (the shipped
// default) fills this via `SubagentsConfig::default()` — NOT the per-field
// `#[serde(default = "…")]` fns (those only fire when the table is present but a
// field is omitted). A derived Default would yield `max_depth = 0`, and the spawn
// guard `parent_depth >= max_depth` then rejects EVERY local sub-agent spawn with
// "max sub-agent depth exceeded". The manual impl below keeps the absent-section
// default identical to the per-field defaults.
#[derive(Debug, Clone, Deserialize)]
pub struct SubagentsConfig {
    #[serde(default = "default_max_depth")]
    pub max_depth: u32,
    #[serde(default = "default_max_concurrent")]
    pub max_concurrent: u32,
    #[serde(default = "default_inherit_mode")]
    pub inherit_mode: bool,
}

impl Default for SubagentsConfig {
    fn default() -> Self {
        Self {
            max_depth:      default_max_depth(),
            max_concurrent: default_max_concurrent(),
            inherit_mode:   default_inherit_mode(),
        }
    }
}

fn default_max_depth()      -> u32  { 4 }
fn default_max_concurrent() -> u32  { 16 }
fn default_inherit_mode()   -> bool { true }

#[derive(Debug, Clone, Default, Deserialize)]
pub struct PolicyConfig {
    #[serde(default)]
    pub mode: PolicyMode,
    #[serde(default)]
    pub rules: HashMap<String, Rule>,
    #[serde(default)]
    pub subagents: SubagentsConfig,
}

impl PolicyConfig {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("cannot read {}: {}", path.display(), e))?;
        Self::parse(&text)
    }

    /// Parse a policy config from a TOML string. Shared by `load` and by the
    /// evolution applier's validate-before-persist check.
    pub fn parse(text: &str) -> anyhow::Result<Self> {
        toml::from_str(text)
            .map_err(|e| anyhow::anyhow!("policy.toml parse error: {}", e))
    }
}

impl From<apexos_core::PolicyRule> for Rule {
    fn from(r: apexos_core::PolicyRule) -> Self {
        match r {
            apexos_core::PolicyRule::Allow     => Rule::Allow,
            apexos_core::PolicyRule::Ask       => Rule::Ask,
            apexos_core::PolicyRule::Workspace => Rule::Workspace,
        }
    }
}

// ── pure rule evaluation ──────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct PolicyEngine {
    pub config: PolicyConfig,
}

impl PolicyEngine {
    pub fn new(config: PolicyConfig) -> Self { Self { config } }

    /// Evaluate whether `tool_name` may proceed without user confirmation.
    /// `path` is the filesystem path argument from the tool call, if any.
    /// Returns `Decision::Allow` or `Decision::Ask`.
    pub fn check(&self, tool_name: &str, path: Option<&str>) -> Decision {
        if self.config.mode == PolicyMode::Yolo {
            return Decision::Allow;
        }
        let rule = self.find_rule(tool_name);
        self.apply_rule(rule, path)
    }

    fn find_rule(&self, tool_name: &str) -> Option<&Rule> {
        // Exact match wins over wildcard.
        if let Some(r) = self.config.rules.get(tool_name) {
            return Some(r);
        }
        for (pattern, rule) in &self.config.rules {
            if matches_wildcard(pattern, tool_name) {
                return Some(rule);
            }
        }
        None
    }

    fn apply_rule(&self, rule: Option<&Rule>, path: Option<&str>) -> Decision {
        match rule {
            None                  => Decision::Ask,   // unknown tool → safe default
            Some(Rule::Allow)     => Decision::Allow,
            Some(Rule::Ask)       => Decision::Ask,
            Some(Rule::Workspace) => self.workspace_decision(path),
        }
    }

    fn workspace_decision(&self, path: Option<&str>) -> Decision {
        let Some(p) = path else { return Decision::Ask };
        // Reject traversal: a non-existent write target with `..` would otherwise
        // canonicalize-fail and slip past the component-prefix check below.
        // Mirrors the guard delete_path already applies.
        if p.contains("..") { return Decision::Ask; }
        let Ok(ws) = std::env::var("AGENTD_WORKSPACE") else { return Decision::Ask };
        if ws.is_empty() { return Decision::Ask; }

        let ws_canon = std::fs::canonicalize(&ws)
            .unwrap_or_else(|_| std::path::PathBuf::from(&ws));
        // Canonicalize the target. A not-yet-existing target resolves through
        // its nearest EXISTING ancestor (canonicalize_lenient), so a symlinked
        // parent dir can't smuggle an "inside the workspace" string whose real
        // location is elsewhere. Nothing resolvable (e.g. a relative path
        // outside the ws) → Ask, fail-closed; the tool's confine() stays the
        // hard gate either way — this only decides the approval prompt.
        let Some(tgt_canon) = apexos_confine::canonicalize_lenient(std::path::Path::new(p)) else {
            return Decision::Ask;
        };

        if tgt_canon.starts_with(&ws_canon) {
            Decision::Allow
        } else {
            Decision::Ask
        }
    }
}

/// Pattern `"prefix.*"` matches any `"prefix.<something>"`.
fn matches_wildcard(pattern: &str, tool: &str) -> bool {
    if pattern == tool {
        return true;
    }
    if let Some(prefix) = pattern.strip_suffix(".*") {
        tool.starts_with(&format!("{prefix}."))
    } else {
        false
    }
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // Serialize tests that mutate AGENTD_WORKSPACE — env vars are process-global
    // and Rust runs tests in parallel by default.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn engine(mode: PolicyMode, rules: &[(&str, Rule)]) -> PolicyEngine {
        PolicyEngine::new(PolicyConfig {
            mode,
            rules: rules.iter().map(|(k, v)| (k.to_string(), v.clone())).collect(),
            ..Default::default()
        })
    }

    #[test]
    fn yolo_allows_everything() {
        let e = engine(PolicyMode::Yolo, &[("shell.exec", Rule::Ask)]);
        assert_eq!(e.check("shell.exec", None), Decision::Allow);
        assert_eq!(e.check("anything", None),   Decision::Allow);
    }

    #[test]
    fn suggest_allow_rule_passes() {
        let e = engine(PolicyMode::Suggest, &[("fs.read", Rule::Allow)]);
        assert_eq!(e.check("fs.read", None), Decision::Allow);
    }

    #[test]
    fn suggest_ask_rule_blocks() {
        let e = engine(PolicyMode::Suggest, &[("shell.exec", Rule::Ask)]);
        assert_eq!(e.check("shell.exec", None), Decision::Ask);
    }

    #[test]
    fn suggest_unknown_tool_blocks() {
        let e = engine(PolicyMode::Suggest, &[]);
        assert_eq!(e.check("unknown.tool", None), Decision::Ask);
    }

    #[test]
    fn wildcard_matches_prefixed_tools() {
        let e = engine(PolicyMode::Suggest, &[("cerebro.*", Rule::Allow)]);
        assert_eq!(e.check("cerebro.recall", None), Decision::Allow);
        assert_eq!(e.check("cerebro.store", None),  Decision::Allow);
        assert_eq!(e.check("cerebro", None),        Decision::Ask);  // bare name, no dot
        assert_eq!(e.check("cerebro_other", None),  Decision::Ask);  // wrong separator
    }

    #[test]
    fn exact_match_wins_over_wildcard() {
        let e = engine(PolicyMode::Suggest, &[
            ("cerebro.*",    Rule::Allow),
            ("cerebro.exec", Rule::Ask),
        ]);
        assert_eq!(e.check("cerebro.exec", None),   Decision::Ask);
        assert_eq!(e.check("cerebro.recall", None), Decision::Allow);
    }

    #[test]
    fn workspace_rule_no_path_asks() {
        let e = engine(PolicyMode::AutoEdit, &[("write_file", Rule::Workspace)]);
        assert_eq!(e.check("write_file", None), Decision::Ask);
    }

    #[test]
    fn workspace_rule_no_env_var_asks() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("AGENTD_WORKSPACE");
        let e = engine(PolicyMode::Suggest, &[("write_file", Rule::Workspace)]);
        assert_eq!(e.check("write_file", Some("/tmp/file.txt")), Decision::Ask);
    }

    #[test]
    fn workspace_rule_inside_workspace_allows() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("AGENTD_WORKSPACE", "/tmp");
        let e = engine(PolicyMode::Suggest, &[("write_file", Rule::Workspace)]);
        // /tmp exists so canonicalize succeeds
        assert_eq!(e.check("write_file", Some("/tmp/file.txt")), Decision::Allow);
        std::env::remove_var("AGENTD_WORKSPACE");
    }

    #[test]
    fn workspace_rule_outside_workspace_asks() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("AGENTD_WORKSPACE", "/tmp");
        let e = engine(PolicyMode::Suggest, &[("write_file", Rule::Workspace)]);
        assert_eq!(e.check("write_file", Some("/etc/passwd")), Decision::Ask);
        std::env::remove_var("AGENTD_WORKSPACE");
    }

    #[test]
    fn workspace_rule_dotdot_traversal_asks() {
        // A non-existent path with `..` would canonicalize-fail and slip past the
        // starts_with check without the explicit `..` rejection guard.
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("AGENTD_WORKSPACE", "/tmp");
        let e = engine(PolicyMode::Suggest, &[("write_file", Rule::Workspace)]);
        assert_eq!(
            e.check("write_file", Some("/tmp/../etc/cron.d/x")),
            Decision::Ask
        );
        std::env::remove_var("AGENTD_WORKSPACE");
    }

    #[test]
    fn workspace_rule_symlinked_parent_resolves_to_real_location() {
        // A not-yet-existing target under a symlinked dir must be judged by the
        // symlink's REAL destination, not the raw string — the old fallback
        // (`canonicalize(p).unwrap_or(raw)`) trusted the unresolved prefix.
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let base = std::env::temp_dir().join(format!("apexos_policy_symlink_{}", std::process::id()));
        let ws      = base.join("ws");
        let outside = base.join("outside");
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        // ws/escape -> outside : a path string INSIDE the ws whose real home isn't
        std::os::unix::fs::symlink(&outside, ws.join("escape")).unwrap();
        std::env::set_var("AGENTD_WORKSPACE", &ws);

        let e = engine(PolicyMode::Suggest, &[("write_file", Rule::Workspace)]);
        let sneaky = ws.join("escape").join("new_file.txt");
        assert_eq!(e.check("write_file", Some(sneaky.to_str().unwrap())), Decision::Ask);
        // ...while an honest not-yet-existing nested target inside the ws stays Allow
        let honest = ws.join("newdir").join("new_file.txt");
        assert_eq!(e.check("write_file", Some(honest.to_str().unwrap())), Decision::Allow);

        std::env::remove_var("AGENTD_WORKSPACE");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn loads_default_policy_config() {
        let cfg = PolicyConfig::default();
        assert_eq!(cfg.mode, PolicyMode::Suggest);
        assert!(cfg.rules.is_empty());
    }

    #[test]
    fn policy_rule_toml_strings_are_valid_rule_values() {
        // Regression: the evolution applier writes PolicyRule::as_toml_str() into
        // the [rules] table. Every such string MUST deserialize back into a Rule,
        // or a single update_policy_rule evolution corrupts policy.toml and wipes
        // all rules on the next load.
        for pr in [
            apexos_core::PolicyRule::Allow,
            apexos_core::PolicyRule::Ask,
            apexos_core::PolicyRule::Workspace,
        ] {
            let toml = format!("[rules]\n\"some.tool\" = \"{}\"\n", pr.as_toml_str());
            let cfg = PolicyConfig::parse(&toml)
                .unwrap_or_else(|e| panic!("'{}' must parse as a rule: {e}", pr.as_toml_str()));
            assert_eq!(cfg.rules["some.tool"], Rule::from(pr));
        }
    }

    #[test]
    fn parses_policy_toml() {
        let toml = r#"
mode = "auto-edit"

[rules]
"fs.read"   = "allow"
"shell.exec" = "ask"
"cerebro.*" = "allow"
"fs.write"  = "workspace"

[subagents]
max_depth       = 3
max_concurrent  = 8
inherit_mode    = false
"#;
        let cfg: PolicyConfig = toml::from_str(toml).unwrap();
        assert_eq!(cfg.mode, PolicyMode::AutoEdit);
        assert_eq!(cfg.rules["fs.read"],    Rule::Allow);
        assert_eq!(cfg.rules["shell.exec"], Rule::Ask);
        assert_eq!(cfg.rules["cerebro.*"],  Rule::Allow);
        assert_eq!(cfg.rules["fs.write"],   Rule::Workspace);
        assert_eq!(cfg.subagents.max_depth, 3);
        assert_eq!(cfg.subagents.max_concurrent, 8);
        assert!(!cfg.subagents.inherit_mode);
    }

    // Regression: a policy.toml with NO [subagents] table (the shipped default)
    // must use the intended defaults — NOT the derived all-zeros, which would set
    // max_depth=0 and make the spawn guard reject every local sub-agent
    // ("max sub-agent depth exceeded"). Bit a live landing-site build (2026-06-22).
    #[test]
    fn absent_subagents_section_keeps_intended_defaults() {
        let cfg = PolicyConfig::parse("mode = \"yolo\"\n").unwrap();
        assert_eq!(cfg.subagents.max_depth, 4, "absent [subagents] must NOT yield max_depth=0");
        assert_eq!(cfg.subagents.max_concurrent, 16);
        assert!(cfg.subagents.inherit_mode);
        // And SubagentsConfig::default() (what serde calls for the absent section).
        assert_eq!(SubagentsConfig::default().max_depth, 4);
    }
}
