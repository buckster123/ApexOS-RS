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
    /// Auto if path is inside AGENTD_WORKSPACE, else ask.
    /// Path check is deferred to keyboard; currently treated as Allow in
    /// AutoEdit mode and Ask in Suggest mode.
    Workspace,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Decision { Allow, Ask }

#[derive(Debug, Clone, Default, Deserialize)]
pub struct SubagentsConfig {
    #[serde(default = "default_max_depth")]
    pub max_depth: u32,
    #[serde(default = "default_max_concurrent")]
    pub max_concurrent: u32,
    #[serde(default = "default_inherit_mode")]
    pub inherit_mode: bool,
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
    /// Returns `Decision::Allow` or `Decision::Ask`.
    pub fn check(&self, tool_name: &str) -> Decision {
        if self.config.mode == PolicyMode::Yolo {
            return Decision::Allow;
        }
        let rule = self.find_rule(tool_name);
        self.apply_rule(rule)
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

    fn apply_rule(&self, rule: Option<&Rule>) -> Decision {
        match rule {
            None                  => Decision::Ask,   // unknown tool → safe default
            Some(Rule::Allow)     => Decision::Allow,
            Some(Rule::Ask)       => Decision::Ask,
            Some(Rule::Workspace) => match self.config.mode {
                PolicyMode::AutoEdit => Decision::Allow, // path check deferred
                _              => Decision::Ask,
            },
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
        assert_eq!(e.check("shell.exec"), Decision::Allow);
        assert_eq!(e.check("anything"),   Decision::Allow);
    }

    #[test]
    fn suggest_allow_rule_passes() {
        let e = engine(PolicyMode::Suggest, &[("fs.read", Rule::Allow)]);
        assert_eq!(e.check("fs.read"), Decision::Allow);
    }

    #[test]
    fn suggest_ask_rule_blocks() {
        let e = engine(PolicyMode::Suggest, &[("shell.exec", Rule::Ask)]);
        assert_eq!(e.check("shell.exec"), Decision::Ask);
    }

    #[test]
    fn suggest_unknown_tool_blocks() {
        let e = engine(PolicyMode::Suggest, &[]);
        assert_eq!(e.check("unknown.tool"), Decision::Ask);
    }

    #[test]
    fn wildcard_matches_prefixed_tools() {
        let e = engine(PolicyMode::Suggest, &[("cerebro.*", Rule::Allow)]);
        assert_eq!(e.check("cerebro.recall"), Decision::Allow);
        assert_eq!(e.check("cerebro.store"),  Decision::Allow);
        assert_eq!(e.check("cerebro"),        Decision::Ask);  // bare name, no dot
        assert_eq!(e.check("cerebro_other"),  Decision::Ask);  // wrong separator
    }

    #[test]
    fn exact_match_wins_over_wildcard() {
        let e = engine(PolicyMode::Suggest, &[
            ("cerebro.*",    Rule::Allow),
            ("cerebro.exec", Rule::Ask),
        ]);
        assert_eq!(e.check("cerebro.exec"),   Decision::Ask);
        assert_eq!(e.check("cerebro.recall"), Decision::Allow);
    }

    #[test]
    fn auto_edit_workspace_rule_allows() {
        let e = engine(PolicyMode::AutoEdit, &[("fs.write", Rule::Workspace)]);
        assert_eq!(e.check("fs.write"), Decision::Allow);
    }

    #[test]
    fn suggest_workspace_rule_blocks() {
        let e = engine(PolicyMode::Suggest, &[("fs.write", Rule::Workspace)]);
        assert_eq!(e.check("fs.write"), Decision::Ask);
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
}
