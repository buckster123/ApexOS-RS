//! Environment for MCP children (finding 11).
//!
//! `spawn_plugin` used to inherit agentd's full environment, so an approved
//! `run_command` (or any escaped tool) could `printenv` the node token and
//! every provider key. Children now start from `env_clear` plus this map.
//!
//! Laws:
//!   1. Node/admin secrets never leave agentd (`AGENTD_TOKEN`, mesh/sensor
//!      tokens, PSK).
//!   2. Provider keys are inherited only by the plugin that needs them
//!      (cerebro/occipital vision, imaginarium proxy token) — never by
//!      `apexos-tools`.
//!   3. `[plugin.env]` overlays, but cannot inject a never-key.
//!   4. Same-uid file access (`/var/lib/agentd/.api_key`) is closed by
//!      Landlock. The shell worker's WAN is closed by an empty netns
//!      (`isolate_network` on `--class=fs` and `--class=dev`).
//!      Camera/gpio live in `apexos-dev`. On a node those three are
//!      sibling units (different uids); this map still applies to
//!      stdio-spawned children (`cargo run` / `APEXOS_TOOLS_SPAWN=stdio`).

use std::collections::{BTreeMap, HashMap};

/// Never inherited, never taken from `[plugin.env]`.
const NEVER: &[&str] = &[
    "AGENTD_TOKEN",
    "MESH_BRIDGE_TOKEN",
    "SENSOR_BRIDGE_TOKEN",
    "APEXNET_PSK",
    "APEXNET_PSK_FILE",
    "EE_AGENTD_TOKEN",
    "ADITUS_TOKEN",
];

/// Non-secret keys every plugin may inherit from the parent if set.
const INHERIT: &[&str] = &[
    "PATH",
    "HOME",
    "USER",
    "LOGNAME",
    "LANG",
    "LC_ALL",
    "LC_CTYPE",
    "LC_MESSAGES",
    "TZ",
    "TMPDIR",
    "TMP",
    "TEMP",
    "RUST_LOG",
    "AGENTD_WORKSPACE",
    "AGENTD_LOG",
    "AGENTD_UI",
    "AGENTD_READ_ROOTS",
    "AGENTD_GIT_ROOTS",
    "AGENTD_USB_EJECT_DIR",
    "AGENTD_USB_PREP_DIR",
    "AGENTD_AGENT_ID",
    "AGENTD_HTTP_FETCH_MODE",
    "AGENTD_HTTP_FETCH_ALLOWLIST",
    "AGENTD_EE_CONNECTORS",
    "AGENTD_PARTS_INVENTORY",
    "AGENTD_HARDWARE_WISHLIST",
    "APEX_NODE_ID",
    "APEX_GPIO_RESERVED",
    "APEXOS_UI_SNAPSHOT_URL",
    "APEXOS_UI_STATE_URL",
    "APEXOS_CAMERA_CMD",
    "APEXOS_CAMERA_DEVICE",
    "APEXOS_LANDLOCK",
    "APEXOS_NETNS",
    "APEXOS_TOOLS_CLASS",
];

fn extra_inherit(plugin_id: &str) -> &'static [&'static str] {
    match plugin_id {
        "cerebro" | "occipital" => &["ANTHROPIC_API_KEY"],
        "imaginarium" => &["IMAGINARIUM_URL", "IMAGINARIUM_TOKEN"],
        "apexos-tools" | "apexos-net" => &[
            "TELEGRAM_BOT_TOKEN",
            "TELEGRAM_CHAT_ID",
            "NTFY_TOPIC",
            "PIPER_MODEL",
        ],
        _ => &[],
    }
}

pub fn is_never_key(key: &str) -> bool {
    NEVER.iter().any(|k| *k == key)
}

/// Build the child environment. `parent` is `std::env::var` in production
/// and a stub in tests.
pub fn plugin_child_env(
    plugin_id: &str,
    plugin_env: Option<&HashMap<String, String>>,
    parent: impl Fn(&str) -> Option<String>,
) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for key in INHERIT
        .iter()
        .copied()
        .chain(extra_inherit(plugin_id).iter().copied())
    {
        if is_never_key(key) {
            continue;
        }
        if let Some(v) = parent(key) {
            out.insert(key.to_string(), v);
        }
    }
    if let Some(overlay) = plugin_env {
        for (k, v) in overlay {
            if is_never_key(k) {
                continue;
            }
            out.insert(k.clone(), v.clone());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parent<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |k| {
            pairs
                .iter()
                .find(|(n, _)| *n == k)
                .map(|(_, v)| (*v).to_string())
        }
    }

    #[test]
    fn tools_do_not_inherit_node_or_provider_secrets() {
        let env = plugin_child_env(
            "apexos-tools",
            None,
            parent(&[
                ("PATH", "/usr/bin"),
                ("AGENTD_WORKSPACE", "/var/lib/agentd/workspace"),
                ("AGENTD_TOKEN", "node-secret"),
                ("ANTHROPIC_API_KEY", "sk-ant"),
                ("XAI_API_KEY", "xai-secret"),
                ("OAI_API_KEY", "oai-secret"),
                ("OPENROUTER_API_KEY", "or-secret"),
                ("MESH_BRIDGE_TOKEN", "mesh"),
                ("SENSOR_BRIDGE_TOKEN", "sens"),
                ("IMAGINARIUM_TOKEN", "imag"),
                ("TELEGRAM_BOT_TOKEN", "tg"),
            ]),
        );
        assert_eq!(env.get("PATH").map(String::as_str), Some("/usr/bin"));
        assert_eq!(
            env.get("AGENTD_WORKSPACE").map(String::as_str),
            Some("/var/lib/agentd/workspace")
        );
        assert_eq!(
            env.get("TELEGRAM_BOT_TOKEN").map(String::as_str),
            Some("tg")
        );
        assert_eq!(env.get("APEXOS_LANDLOCK").map(String::as_str), None);
        let env_ll = plugin_child_env("apexos-tools", None, parent(&[("APEXOS_LANDLOCK", "0")]));
        assert_eq!(env_ll.get("APEXOS_LANDLOCK").map(String::as_str), Some("0"));
        for banned in [
            "AGENTD_TOKEN",
            "ANTHROPIC_API_KEY",
            "XAI_API_KEY",
            "OAI_API_KEY",
            "OPENROUTER_API_KEY",
            "MESH_BRIDGE_TOKEN",
            "SENSOR_BRIDGE_TOKEN",
            "IMAGINARIUM_TOKEN",
        ] {
            assert!(
                !env.contains_key(banned),
                "{banned} leaked into apexos-tools"
            );
        }
    }

    #[test]
    fn cerebro_may_inherit_anthropic_but_not_the_node_token() {
        let env = plugin_child_env(
            "cerebro",
            None,
            parent(&[
                ("ANTHROPIC_API_KEY", "sk-ant"),
                ("AGENTD_TOKEN", "node-secret"),
                ("CEREBRO_DATA_DIR", "/should-not-inherit-this-name"),
            ]),
        );
        assert_eq!(
            env.get("ANTHROPIC_API_KEY").map(String::as_str),
            Some("sk-ant")
        );
        assert!(!env.contains_key("AGENTD_TOKEN"));
        // CEREBRO_DATA_DIR comes from [plugin.env], not a blanket inherit.
        assert!(!env.contains_key("CEREBRO_DATA_DIR"));
    }

    #[test]
    fn imaginarium_inherits_proxy_token_not_xai_key() {
        let env = plugin_child_env(
            "imaginarium",
            None,
            parent(&[
                ("IMAGINARIUM_URL", "http://127.0.0.1:8791"),
                ("IMAGINARIUM_TOKEN", "proxy"),
                ("XAI_API_KEY", "xai-secret"),
            ]),
        );
        assert_eq!(
            env.get("IMAGINARIUM_TOKEN").map(String::as_str),
            Some("proxy")
        );
        assert_eq!(
            env.get("IMAGINARIUM_URL").map(String::as_str),
            Some("http://127.0.0.1:8791")
        );
        assert!(!env.contains_key("XAI_API_KEY"));
    }

    #[test]
    fn plugin_env_cannot_inject_the_node_token() {
        let mut overlay = HashMap::new();
        overlay.insert("AGENTD_TOKEN".into(), "stolen".into());
        overlay.insert("WEATHER_API_KEY".into(), "ok".into());
        let env = plugin_child_env("weather", Some(&overlay), parent(&[]));
        assert!(!env.contains_key("AGENTD_TOKEN"));
        assert_eq!(env.get("WEATHER_API_KEY").map(String::as_str), Some("ok"));
    }

    #[test]
    fn aditus_does_not_inherit_token_or_anthropic() {
        let mut overlay = HashMap::new();
        overlay.insert("ADITUS_TOKEN".into(), "stolen".into());
        overlay.insert("ADITUS_DB".into(), "/var/lib/aditus/aditus.db".into());
        let env = plugin_child_env(
            "aditus",
            Some(&overlay),
            parent(&[
                ("ADITUS_TOKEN", "from-parent"),
                ("ANTHROPIC_API_KEY", "sk-ant"),
                ("AGENTD_TOKEN", "node-secret"),
            ]),
        );
        assert!(!env.contains_key("ADITUS_TOKEN"));
        assert!(!env.contains_key("ANTHROPIC_API_KEY"));
        assert!(!env.contains_key("AGENTD_TOKEN"));
        assert_eq!(
            env.get("ADITUS_DB").map(String::as_str),
            Some("/var/lib/aditus/aditus.db")
        );
    }

    #[test]
    fn plugin_env_overlays_inherited_values() {
        let mut overlay = HashMap::new();
        overlay.insert("RUST_LOG".into(), "debug".into());
        overlay.insert("CEREBRO_DATA_DIR".into(), "/var/lib/agentd/cerebro".into());
        let env = plugin_child_env("cerebro", Some(&overlay), parent(&[("RUST_LOG", "warn")]));
        assert_eq!(env.get("RUST_LOG").map(String::as_str), Some("debug"));
        assert_eq!(
            env.get("CEREBRO_DATA_DIR").map(String::as_str),
            Some("/var/lib/agentd/cerebro")
        );
    }
}
