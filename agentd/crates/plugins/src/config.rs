use std::collections::HashMap;
use std::path::Path;
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct PluginConfig {
    pub id:      String,
    pub cmd:     String,
    #[serde(default)]
    pub args:    Vec<String>,
    #[serde(default)]
    pub restart: RestartPolicy,
    pub cwd:     Option<String>,
    pub env:     Option<HashMap<String, String>>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Default)]
#[serde(rename_all = "kebab-case")]
pub enum RestartPolicy {
    Always,
    OnFailure,
    #[default]
    Never,
}

#[derive(Deserialize)]
struct PluginsFile {
    plugin: Vec<PluginConfig>,
}

pub fn load(path: &Path) -> anyhow::Result<Vec<PluginConfig>> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("cannot read {}: {}", path.display(), e))?;
    let file: PluginsFile = toml::from_str(&text)
        .map_err(|e| anyhow::anyhow!("plugins.toml parse error: {}", e))?;
    Ok(file.plugin)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_restart_policies() {
        let toml = r#"
[[plugin]]
id      = "cerebro"
cmd     = "/usr/bin/cerebro-mcp"
restart = "always"

[[plugin]]
id      = "shell"
cmd     = "/usr/bin/shell-mcp"
restart = "on-failure"

[[plugin]]
id      = "fs"
cmd     = "/usr/bin/fs-mcp"
"#;
        let file: PluginsFile = toml::from_str(toml).unwrap();
        assert_eq!(file.plugin.len(), 3);
        assert_eq!(file.plugin[0].restart, RestartPolicy::Always);
        assert_eq!(file.plugin[1].restart, RestartPolicy::OnFailure);
        assert_eq!(file.plugin[2].restart, RestartPolicy::Never); // default
    }

    #[test]
    fn parses_args_env_cwd() {
        let toml = r#"
[[plugin]]
id   = "test"
cmd  = "python"
args = ["-m", "mymod"]
cwd  = "/opt/mymod"
[plugin.env]
FOO = "bar"
"#;
        let file: PluginsFile = toml::from_str(toml).unwrap();
        let p = &file.plugin[0];
        assert_eq!(p.args, vec!["-m", "mymod"]);
        assert_eq!(p.cwd.as_deref(), Some("/opt/mymod"));
        assert_eq!(p.env.as_ref().unwrap()["FOO"], "bar");
    }
}
