//! Inference-backend selection persistence — the voice-config pattern applied to the
//! {backend, model, oai_base_url} trio. The operator's live choice (Settings / POST
//! /api/backend|model) persists here and wins over /etc/agentd/env on restart; env is
//! the seed for a fresh node only. agentd resolves this at boot via `resolve_boot`
//! BEFORE the shared arcs are created, so the whole daemon (router, council, vast
//! revert) sees one consistent resolved config.
//!
//! Pure precedence, per field: persisted (non-empty) > env (non-empty) > default.
//! Defaults are backend-aware — notably `openrouter` gets its canonical base URL, so
//! selecting the openrouter backend can never strand requests at the ollama localhost
//! default (the footgun the curl-era swap had).

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct BackendConfig {
    #[serde(default)]
    pub backend: String, // anthropic | openrouter | ollama | vllm | oai
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub oai_base_url: String,
}

/// The closed set POST /api/backend accepts. Anything else is a typo that would
/// silently fall through RoutingProvider's `_ => anthropic` arm — reject it instead.
pub const KNOWN_BACKENDS: &[&str] = &["anthropic", "openrouter", "ollama", "vllm", "oai"];

pub fn backend_valid(s: &str) -> bool {
    KNOWN_BACKENDS.contains(&s)
}

/// Per-backend default model (single source — agentd boot uses this too).
pub fn default_model_for(backend: &str) -> &'static str {
    match backend {
        "ollama" | "vllm" => "qwen3:27b",
        "openrouter"      => "qwen/qwen3-70b-a3b",
        _                 => "claude-sonnet-4-6",
    }
}

/// Per-backend default OAI base URL. openrouter has exactly one canonical endpoint;
/// every local backend defaults to ollama's port (vllm/oai operators set theirs).
pub fn default_url_for(backend: &str) -> &'static str {
    match backend {
        "openrouter" => "https://openrouter.ai/api/v1",
        _            => "http://localhost:11434/v1",
    }
}

pub fn config_path() -> String {
    std::env::var("AGENTD_BACKEND_CONFIG")
        .unwrap_or_else(|_| "/var/lib/agentd/backend_config.json".into())
}

/// The persisted operator choice, if any. `None` on a fresh node / unreadable file.
pub fn load_persisted() -> Option<BackendConfig> {
    let raw = std::fs::read_to_string(config_path()).ok()?;
    serde_json::from_str::<BackendConfig>(&raw).ok()
}

/// Best-effort persist — the in-memory arcs are already live for the next turn, so a
/// write failure only costs restart-durability (mirrors voice/sensor config).
pub fn persist(cfg: &BackendConfig) {
    if let Ok(s) = serde_json::to_string_pretty(cfg) {
        if let Err(e) = std::fs::write(config_path(), s) {
            eprintln!("[gateway] persist backend config to {} failed: {e}", config_path());
        }
    }
}

/// Pure boot resolution: overlay the persisted choice on the env seed, then fill
/// remaining gaps with backend-aware defaults. Unit-tested; agentd calls this once.
pub fn resolve_boot(
    env_backend: &str,
    env_model: &str,
    env_url: &str,
    persisted: Option<&BackendConfig>,
) -> BackendConfig {
    let pick = |file: Option<&str>, env: &str| -> String {
        match file {
            Some(f) if !f.trim().is_empty() => f.trim().to_string(),
            _ if !env.trim().is_empty()     => env.trim().to_string(),
            _                               => String::new(),
        }
    };
    let backend = {
        let b = pick(persisted.map(|p| p.backend.as_str()), env_backend).to_lowercase();
        if backend_valid(&b) { b } else { "anthropic".into() }
    };
    let model = {
        let m = pick(persisted.map(|p| p.model.as_str()), env_model);
        if m.is_empty() { default_model_for(&backend).to_string() } else { m }
    };
    let oai_base_url = {
        let u = pick(persisted.map(|p| p.oai_base_url.as_str()), env_url);
        if u.is_empty() { default_url_for(&backend).to_string() } else { u }
    };
    BackendConfig { backend, model, oai_base_url }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_node_env_only() {
        let c = resolve_boot("openrouter", "", "", None);
        assert_eq!(c.backend, "openrouter");
        assert_eq!(c.model, "qwen/qwen3-70b-a3b");
        assert_eq!(c.oai_base_url, "https://openrouter.ai/api/v1");
    }

    #[test]
    fn defaults_when_nothing_set() {
        let c = resolve_boot("", "", "", None);
        assert_eq!(c.backend, "anthropic");
        assert_eq!(c.model, "claude-sonnet-4-6");
        assert_eq!(c.oai_base_url, "http://localhost:11434/v1");
    }

    #[test]
    fn persisted_wins_over_env() {
        let file = BackendConfig {
            backend: "ollama".into(),
            model: "qwen3.6:27b".into(),
            oai_base_url: "http://192.168.0.42:11434/v1".into(),
        };
        let c = resolve_boot("anthropic", "claude-opus-4-8", "", Some(&file));
        assert_eq!(c.backend, "ollama");
        assert_eq!(c.model, "qwen3.6:27b");
        assert_eq!(c.oai_base_url, "http://192.168.0.42:11434/v1");
    }

    #[test]
    fn partial_persisted_fills_from_env_then_default() {
        // Backend persisted, model not: env model wins, URL falls to backend default.
        let file = BackendConfig { backend: "openrouter".into(), ..Default::default() };
        let c = resolve_boot("anthropic", "google/gemma-4-31b-it", "", Some(&file));
        assert_eq!(c.backend, "openrouter");
        assert_eq!(c.model, "google/gemma-4-31b-it");
        assert_eq!(c.oai_base_url, "https://openrouter.ai/api/v1");
    }

    #[test]
    fn bogus_backend_falls_to_anthropic() {
        let file = BackendConfig { backend: "openroutr".into(), ..Default::default() };
        let c = resolve_boot("", "", "", Some(&file));
        assert_eq!(c.backend, "anthropic");
        assert_eq!(c.model, "claude-sonnet-4-6");
    }

    #[test]
    fn round_trip_serde() {
        let c = BackendConfig {
            backend: "openrouter".into(),
            model: "qwen/qwen3.6-27b".into(),
            oai_base_url: "https://openrouter.ai/api/v1".into(),
        };
        let s = serde_json::to_string(&c).unwrap();
        assert_eq!(serde_json::from_str::<BackendConfig>(&s).unwrap(), c);
    }
}
