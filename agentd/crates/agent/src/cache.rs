//! Prompt-caching configuration for the Anthropic provider.
//!
//! Anthropic prompt caching keys off a byte-stable prefix and allows at most **4**
//! `cache_control` breakpoints per request. We always spend one on the system+tools
//! prefix (the stable soul+embodiment+tools block — render order is tools → system →
//! messages, so one breakpoint there caches both). When `cache_conversation` is on we
//! roll up to **3 more** back through the message history, so a long agentic transcript
//! caches incrementally — each turn reads the prior history at ~0.1× input price and
//! writes only the new turn's delta, instead of re-sending the whole transcript at full
//! price every turn. That is the dominant cost on 1M-context giga-sessions.
//!
//! Runtime-adjustable: agentd holds this behind an `Arc<RwLock<_>>` (like the model and
//! API key) so it can be retuned without a restart. OpenAI/Ollama auto-cache by prefix,
//! so this config is a no-op there (they still benefit from the stable-prefix discipline).

/// Cache-entry lifetime. 5-minute is the default (write premium 1.25×); 1-hour (write
/// premium 2×) survives human pauses mid-session — on a giant transcript that avoids
/// re-writing the whole prefix on resume, so it's the more economical choice for gappy
/// interactive sessions even though each write costs more.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CacheTtl {
    FiveMin,
    OneHour,
}

impl CacheTtl {
    /// The `ttl` value for an Anthropic `cache_control` block. `None` → omit the field
    /// (the API default is the 5-minute TTL).
    fn as_str(self) -> Option<&'static str> {
        match self {
            CacheTtl::FiveMin => None,
            CacheTtl::OneHour => Some("1h"),
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            CacheTtl::FiveMin => "5m",
            CacheTtl::OneHour => "1h",
        }
    }
}

/// How agentd caches the Anthropic request prefix. Cheap to clone; held behind an
/// `Arc<RwLock<_>>` and read once per provider call.
#[derive(Clone, Debug)]
pub struct CacheConfig {
    /// Master switch. Off → no `cache_control` anywhere (system sent as a plain string,
    /// exactly the pre-caching shape).
    pub enabled: bool,
    /// Also roll breakpoints through the conversation history — the big long-session
    /// win. Off → only the system+tools prefix caches.
    pub cache_conversation: bool,
    /// Cache-entry lifetime.
    pub ttl: CacheTtl,
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self { enabled: true, cache_conversation: true, ttl: CacheTtl::FiveMin }
    }
}

fn env_off(v: &str) -> bool {
    matches!(v.trim().to_ascii_lowercase().as_str(), "0" | "false" | "off" | "no")
}

impl CacheConfig {
    /// Read defaults from the environment:
    /// - `AGENTD_CACHE=0|false|off|no` → disable caching entirely
    /// - `AGENTD_CACHE_CONVERSATION=0|false|off|no` → cache only the system+tools prefix
    /// - `AGENTD_CACHE_TTL=1h` (or `1hr`/`hour`/`3600`) → 1-hour TTL; anything else → 5m
    pub fn from_env() -> Self {
        let mut c = Self::default();
        if let Ok(v) = std::env::var("AGENTD_CACHE") {
            c.enabled = !env_off(&v);
        }
        if let Ok(v) = std::env::var("AGENTD_CACHE_CONVERSATION") {
            c.cache_conversation = !env_off(&v);
        }
        if let Ok(v) = std::env::var("AGENTD_CACHE_TTL") {
            c.ttl = match v.trim().to_ascii_lowercase().as_str() {
                "1h" | "1hr" | "hour" | "3600" => CacheTtl::OneHour,
                _ => CacheTtl::FiveMin,
            };
        }
        c
    }

    /// One-line summary for the startup log / settings readout.
    pub fn summary(&self) -> String {
        if !self.enabled {
            return "off".to_string();
        }
        format!(
            "on · conversation={} · ttl={}",
            if self.cache_conversation { "yes" } else { "no" },
            self.ttl.label(),
        )
    }

    /// The `cache_control` JSON value for this config's TTL.
    pub(crate) fn control(&self) -> serde_json::Value {
        match self.ttl.as_str() {
            Some(ttl) => serde_json::json!({ "type": "ephemeral", "ttl": ttl }),
            None => serde_json::json!({ "type": "ephemeral" }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_on_conversation_5m() {
        let c = CacheConfig::default();
        assert!(c.enabled && c.cache_conversation);
        assert_eq!(c.ttl, CacheTtl::FiveMin);
    }

    #[test]
    fn control_omits_ttl_for_5m_and_sets_it_for_1h() {
        let five = CacheConfig { ttl: CacheTtl::FiveMin, ..Default::default() };
        assert_eq!(five.control(), serde_json::json!({ "type": "ephemeral" }));
        let hour = CacheConfig { ttl: CacheTtl::OneHour, ..Default::default() };
        assert_eq!(hour.control(), serde_json::json!({ "type": "ephemeral", "ttl": "1h" }));
    }

    #[test]
    fn summary_reads_clearly() {
        assert_eq!(CacheConfig::default().summary(), "on · conversation=yes · ttl=5m");
        assert_eq!(
            CacheConfig { enabled: false, ..Default::default() }.summary(),
            "off"
        );
    }
}
