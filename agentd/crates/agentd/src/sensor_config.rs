//! Smoker-mode sensor-alert sensitivity — the "light smoker" baseline.
//!
//! André smokes; a cigarette / lighter near the sensor head reads as IAQ 170–250
//! and a 50–80 °C thermal blip for up to ~5 min — which crosses the normal alert
//! thresholds (IAQ 150 / thermal 45 °C) and trips an *autonomous* alert on a whiff
//! (the BME688 air sensor is the sensitive one). Smoker mode raises the IAQ +
//! thermal thresholds *above* the cig range and lengthens the persistence window
//! beyond a cigarette's duration, so smoking stays quiet while a sustained, hotter
//! real event (a developing fire) still alerts. It's a runtime toggle (Settings /
//! Sensors), persisted here so it survives a restart. The classifier + persistence
//! gate are unchanged — smoker mode only swaps which thresholds feed them.

use std::path::Path;

/// Load the persisted smoker-mode flag (`<log_dir>/sensor_config.json`, written by
/// the gateway's `POST /api/sensors/config`). Missing / unreadable / unparseable ⇒
/// false (non-smoker default). agentd only READS this at startup to seed the live
/// `AtomicBool`; the gateway owns the write so there's a single toggle path.
pub fn load_smoker_mode(path: &Path) -> bool {
    std::fs::read_to_string(path).ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .and_then(|v| v.get("smoker_mode").and_then(|b| b.as_bool()))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_defaults_non_smoker() {
        assert!(!load_smoker_mode(Path::new("/nonexistent/apexos-sensor-xyz.json")));
    }

    #[test]
    fn reads_the_gateway_written_format() {
        let path = std::env::temp_dir().join(format!("apexos-sensor-test-{}.json", std::process::id()));
        std::fs::write(&path, r#"{"smoker_mode":true}"#).unwrap();
        assert!(load_smoker_mode(&path));
        std::fs::write(&path, r#"{"smoker_mode":false}"#).unwrap();
        assert!(!load_smoker_mode(&path));
        // A malformed / unrelated file falls back to non-smoker.
        std::fs::write(&path, "garbage").unwrap();
        assert!(!load_smoker_mode(&path));
        let _ = std::fs::remove_file(&path);
    }
}
