//! Persisted session-RAG lifecycle knobs (`docs/session-rag.md` S3).
//! Env is the seed; the file wins on restart (history_config shape).

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionRagConfig {
    /// Days of mtime-idle before a non-loaded JSONL is gzipped. `0` = off.
    #[serde(default)]
    pub idle_gzip_days: u32,
    /// Future vacuum must not delete when true. Gzip is not deletion.
    #[serde(default = "default_true")]
    pub never_delete: bool,
}

fn default_true() -> bool {
    true
}

impl Default for SessionRagConfig {
    fn default() -> Self {
        Self {
            idle_gzip_days: 0,
            never_delete: true,
        }
    }
}

pub fn config_path() -> String {
    std::env::var("AGENTD_SESSION_RAG_CONFIG")
        .unwrap_or_else(|_| "/var/lib/agentd/session_rag_config.json".into())
}

pub fn load_persisted() -> Option<SessionRagConfig> {
    let raw = std::fs::read_to_string(config_path()).ok()?;
    serde_json::from_str::<SessionRagConfig>(&raw).ok()
}

pub fn persist(cfg: &SessionRagConfig) -> bool {
    match serde_json::to_string_pretty(cfg) {
        Ok(s) => match std::fs::write(config_path(), s) {
            Ok(()) => true,
            Err(e) => {
                eprintln!(
                    "[gateway] persist session-rag config to {} failed: {e}",
                    config_path()
                );
                false
            }
        },
        Err(_) => false,
    }
}

/// `0` stays off; anything else clamps 1..=365.
pub fn sanitize_days(raw: u64) -> u32 {
    if raw == 0 {
        0
    } else {
        raw.clamp(1, 365) as u32
    }
}

pub fn env_days() -> u32 {
    std::env::var("AGENTD_SESSION_IDLE_GZIP_DAYS")
        .ok()
        .and_then(|s| s.parse().ok())
        .map(sanitize_days)
        .unwrap_or(0)
}

pub fn env_never_delete() -> bool {
    match std::env::var("AGENTD_SESSION_NEVER_DELETE") {
        Ok(s) => {
            let t = s.trim().to_ascii_lowercase();
            !matches!(t.as_str(), "0" | "false" | "off" | "no")
        }
        Err(_) => true,
    }
}

pub fn resolve_boot(env: SessionRagConfig, persisted: Option<&SessionRagConfig>) -> SessionRagConfig {
    match persisted {
        Some(p) => SessionRagConfig {
            idle_gzip_days: sanitize_days(p.idle_gzip_days as u64),
            never_delete: p.never_delete,
        },
        None => env,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_and_boot() {
        assert_eq!(sanitize_days(0), 0);
        assert_eq!(sanitize_days(30), 30);
        assert_eq!(sanitize_days(9999), 365);
        let env = SessionRagConfig {
            idle_gzip_days: 0,
            never_delete: true,
        };
        assert_eq!(resolve_boot(env.clone(), None).idle_gzip_days, 0);
        assert_eq!(
            resolve_boot(
                env,
                Some(&SessionRagConfig {
                    idle_gzip_days: 7,
                    never_delete: false
                })
            )
            .idle_gzip_days,
            7
        );
    }

    #[test]
    fn default_never_deletes() {
        let d: SessionRagConfig = serde_json::from_str("{}").unwrap();
        assert!(d.never_delete);
        assert_eq!(d.idle_gzip_days, 0);
    }
}
