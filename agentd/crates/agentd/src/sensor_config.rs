//! Sensor-alert **sensitivity profile** — the environment baseline for autonomous alerts.
//!
//! An APEX in a smoker's room, a kitchen, or a workshop all see "smoking" numbers
//! routinely — a cigarette / frying / soldering reads as IAQ ~170–300 and a 50–110 °C
//! blip, which crosses the standard thresholds (IAQ 150 / thermal 45 °C) and trips an
//! *autonomous* alert on every puff/sizzle. A profile raises the IAQ + thermal
//! thresholds above that environment's normal baseline and lengthens persistence past
//! a transient, so routine activity stays quiet while a sustained, hotter real event (a
//! developing fire) still alerts. The threshold values per profile live in
//! `profile_thresholds` (agentd `main.rs`); the classifier + persistence gate are
//! unchanged — a profile only swaps which thresholds feed them.
//!
//! It's a runtime toggle (Settings / Sensors), persisted here so it survives a restart.

use std::path::Path;

/// The selectable profiles — canonical list lives in the gateway (`SENSOR_PROFILES`),
/// which validates + advertises them; we reference it so there's one source of truth.
use apexos_gateway::SENSOR_PROFILES;

/// True if `p` is a known profile id (POST validates against this; unknown ⇒ standard).
pub fn is_valid_profile(p: &str) -> bool {
    SENSOR_PROFILES.contains(&p)
}

/// Load the persisted sensitivity profile (`<log_dir>/sensor_config.json`, written by
/// the gateway's `POST /api/sensors/config`). Missing / unreadable / unknown ⇒
/// "standard". Migrates the legacy `{"smoker_mode":true}` form (the first cut) →
/// "smoker". agentd only READS this at startup to seed the live profile; the gateway
/// owns the write, so there's a single toggle path.
pub fn load_profile(path: &Path) -> String {
    let Some(v) = std::fs::read_to_string(path).ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
    else {
        return "standard".into();
    };
    if let Some(p) = v.get("profile").and_then(|p| p.as_str()) {
        if is_valid_profile(p) { return p.to_string(); }
    }
    // Legacy migration: the boolean first cut.
    if v.get("smoker_mode").and_then(|b| b.as_bool()) == Some(true) {
        return "smoker".into();
    }
    "standard".into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_defaults_standard() {
        assert_eq!(load_profile(Path::new("/nonexistent/apexos-sensor-xyz.json")), "standard");
    }

    #[test]
    fn reads_profile_and_validates() {
        let path = std::env::temp_dir().join(format!("apexos-prof-test-{}.json", std::process::id()));
        for p in SENSOR_PROFILES {
            std::fs::write(&path, format!(r#"{{"profile":"{p}"}}"#)).unwrap();
            assert_eq!(load_profile(&path), p);
        }
        // Unknown profile / garbage → standard.
        std::fs::write(&path, r#"{"profile":"bogus"}"#).unwrap();
        assert_eq!(load_profile(&path), "standard");
        std::fs::write(&path, "garbage").unwrap();
        assert_eq!(load_profile(&path), "standard");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn migrates_legacy_smoker_mode_bool() {
        let path = std::env::temp_dir().join(format!("apexos-prof-legacy-{}.json", std::process::id()));
        std::fs::write(&path, r#"{"smoker_mode":true}"#).unwrap();
        assert_eq!(load_profile(&path), "smoker");
        std::fs::write(&path, r#"{"smoker_mode":false}"#).unwrap();
        assert_eq!(load_profile(&path), "standard");
        let _ = std::fs::remove_file(&path);
    }
}
