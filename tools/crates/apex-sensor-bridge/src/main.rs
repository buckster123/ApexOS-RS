//! apex-sensor-bridge — forwards sensor readings to the ApexOS gateway
//!
//! Connects to ws://{SENSOR_BRIDGE_HOST}/sensor-bridge?token={SENSOR_BRIDGE_TOKEN}
//! and pushes SensorReading events on a configurable interval.
//!
//! Env vars:
//!   SENSOR_BRIDGE_HOST   (default: localhost:8787)
//!   SENSOR_BRIDGE_TOKEN  (default: empty)
//!   SENSOR_NODE_ID       (default: hostname)
//!   SENSOR_INTERVAL_SECS (default: 30)
//!   SENSORHEAD_URL       (optional: http://localhost:8080 — enables BME688 + MLX90640 polling)

use serde_json::json;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tungstenite::{connect, Message};

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs()
}

fn hostname() -> String {
    std::fs::read_to_string("/etc/hostname")
        .unwrap_or_else(|_| "unknown".into())
        .trim()
        .to_string()
}

// ── CPU temperature from sysfs ────────────────────────────────────────────────

fn read_cpu_temp() -> Option<f32> {
    let candidates = ["/sys/class/thermal/thermal_zone0/temp".to_string()];
    for path in &candidates {
        if let Ok(raw) = std::fs::read_to_string(path) {
            if let Ok(millideg) = raw.trim().parse::<i64>() {
                return Some(millideg as f32 / 1000.0);
            }
        }
    }
    for i in 1..8 {
        let path = format!("/sys/class/thermal/thermal_zone{i}/temp");
        if let Ok(raw) = std::fs::read_to_string(&path) {
            if let Ok(millideg) = raw.trim().parse::<i64>() {
                return Some(millideg as f32 / 1000.0);
            }
        }
    }
    None
}

// ── SensorHead HTTP polling ───────────────────────────────────────────────────

struct SensorHeadClient {
    base_url: String,
    client:   reqwest::blocking::Client,
}

impl SensorHeadClient {
    fn new(base_url: String) -> Self {
        let client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(8))
            .build()
            .expect("reqwest client");
        Self { base_url, client }
    }

    /// Poll /api/environment and return a vec of SensorReading-shaped JSON values.
    fn poll_environment(&self, node_id: &str) -> Vec<serde_json::Value> {
        let url = format!("{}/api/environment", self.base_url);
        let resp = match self.client.get(&url).send() {
            Ok(r) => r,
            Err(e) => {
                eprintln!("[apex-sensor-bridge] SensorHead /api/environment error: {e}");
                return vec![];
            }
        };
        let body: serde_json::Value = match resp.json() {
            Ok(v) => v,
            Err(e) => {
                eprintln!("[apex-sensor-bridge] SensorHead /api/environment parse error: {e}");
                return vec![];
            }
        };
        if body.get("error").and_then(|v| v.as_bool()).unwrap_or(false) {
            let status = body.get("status").and_then(|v| v.as_str()).unwrap_or("unknown");
            eprintln!("[apex-sensor-bridge] BME688 not ready: {status}");
            return vec![];
        }

        let ts = now_secs();
        let mut events = vec![];

        // Temperature + Humidity + Pressure as separate events (compatible with existing variants)
        if let Some(t) = body.get("temperature_c").and_then(|v| v.as_f64()) {
            events.push(json!({
                "type": "sensor_reading", "node_id": node_id, "timestamp": ts,
                "reading": { "kind": "temperature", "celsius": t as f32, "sensor_id": "bme688" }
            }));
        }
        if let Some(h) = body.get("humidity_pct").and_then(|v| v.as_f64()) {
            events.push(json!({
                "type": "sensor_reading", "node_id": node_id, "timestamp": ts,
                "reading": { "kind": "humidity", "percent": h as f32, "sensor_id": "bme688" }
            }));
        }
        if let Some(p) = body.get("pressure_hpa").and_then(|v| v.as_f64()) {
            events.push(json!({
                "type": "sensor_reading", "node_id": node_id, "timestamp": ts,
                "reading": { "kind": "pressure", "hpa": p as f32, "sensor_id": "bme688" }
            }));
        }

        // AirQuality event (only if BSEC2 is active — iaq will be present)
        if let Some(iaq) = body.get("iaq").and_then(|v| v.as_f64()) {
            let co2   = body.get("co2_equivalent_ppm").and_then(|v| v.as_f64()).unwrap_or(0.0);
            let voc   = body.get("breath_voc_ppm").and_then(|v| v.as_f64()).unwrap_or(0.0);
            let acc   = body.get("iaq_accuracy").and_then(|v| v.as_u64()).unwrap_or(0) as u8;
            let temp  = body.get("temperature_c").and_then(|v| v.as_f64()).unwrap_or(0.0);
            let hum   = body.get("humidity_pct").and_then(|v| v.as_f64()).unwrap_or(0.0);
            let pres  = body.get("pressure_hpa").and_then(|v| v.as_f64()).unwrap_or(0.0);
            events.push(json!({
                "type": "sensor_reading", "node_id": node_id, "timestamp": ts,
                "reading": {
                    "kind": "air_quality",
                    "iaq": iaq as f32,
                    "co2_eq_ppm": co2 as f32,
                    "voc_ppm": voc as f32,
                    "accuracy": acc,
                    "temperature_c": temp as f32,
                    "humidity_pct": hum as f32,
                    "pressure_hpa": pres as f32,
                    "sensor_id": "bme688"
                }
            }));
        }

        events
    }

    /// Poll /api/thermal/data and return a ThermalFrame event.
    fn poll_thermal(&self, node_id: &str) -> Option<serde_json::Value> {
        let url = format!("{}/api/thermal/data", self.base_url);
        let resp = match self.client.get(&url).send() {
            Ok(r) => r,
            Err(e) => {
                eprintln!("[apex-sensor-bridge] SensorHead /api/thermal/data error: {e}");
                return None;
            }
        };
        let body: serde_json::Value = match resp.json() {
            Ok(v) => v,
            Err(e) => {
                eprintln!("[apex-sensor-bridge] SensorHead /api/thermal/data parse error: {e}");
                return None;
            }
        };
        if body.get("error").and_then(|v| v.as_bool()).unwrap_or(false) {
            let status = body.get("status").and_then(|v| v.as_str()).unwrap_or("unknown");
            eprintln!("[apex-sensor-bridge] MLX90640 not ready: {status}");
            return None;
        }

        let min_c  = body.get("min_c").and_then(|v| v.as_f64())? as f32;
        let max_c  = body.get("max_c").and_then(|v| v.as_f64())? as f32;
        let mean_c = body.get("avg_c").and_then(|v| v.as_f64())? as f32;

        Some(json!({
            "type": "sensor_reading", "node_id": node_id, "timestamp": now_secs(),
            "reading": {
                "kind": "thermal_frame",
                "min_c": min_c, "max_c": max_c, "mean_c": mean_c,
                "sensor_id": "mlx90640"
            }
        }))
    }
}

// ── WS send helper ────────────────────────────────────────────────────────────

fn ws_send(ws: &mut tungstenite::WebSocket<tungstenite::stream::MaybeTlsStream<std::net::TcpStream>>, frame: serde_json::Value) -> bool {
    match ws.send(Message::Text(frame.to_string().into())) {
        Ok(_) => true,
        Err(e) => { eprintln!("[apex-sensor-bridge] send error: {e}"); false }
    }
}

// ── Main connect + send loop ──────────────────────────────────────────────────

fn run(url: &str, node_id: &str, interval: Duration, sensorhead: Option<SensorHeadClient>) {
    loop {
        eprintln!("[apex-sensor-bridge] connecting to {url}");
        match connect(url) {
            Err(e) => {
                eprintln!("[apex-sensor-bridge] connect failed: {e} — retry in 10s");
                std::thread::sleep(Duration::from_secs(10));
                continue;
            }
            Ok((mut ws, _)) => {
                eprintln!("[apex-sensor-bridge] connected");
                'inner: loop {
                    // ── CPU temperature (sysfs — always) ─────────────────────
                    if let Some(celsius) = read_cpu_temp() {
                        let frame = json!({
                            "type": "sensor_reading", "node_id": node_id, "timestamp": now_secs(),
                            "reading": { "kind": "temperature", "celsius": celsius, "sensor_id": "cpu_thermal" }
                        });
                        if !ws_send(&mut ws, frame) { break 'inner; }
                    }

                    // ── SensorHead HTTP polling (BME688 + MLX90640) ────────
                    if let Some(ref sh) = sensorhead {
                        for ev in sh.poll_environment(node_id) {
                            if !ws_send(&mut ws, ev) { break 'inner; }
                        }
                        if let Some(ev) = sh.poll_thermal(node_id) {
                            if !ws_send(&mut ws, ev) { break 'inner; }
                        }
                    }

                    // ── Ping ─────────────────────────────────────────────────
                    if let Err(e) = ws.send(Message::Ping(vec![].into())) {
                        eprintln!("[apex-sensor-bridge] ping error: {e} — reconnecting");
                        break 'inner;
                    }

                    std::thread::sleep(interval);
                }
                let _ = ws.close(None);
            }
        }
        std::thread::sleep(Duration::from_secs(5));
    }
}

fn main() {
    let host         = std::env::var("SENSOR_BRIDGE_HOST").unwrap_or_else(|_| "localhost:8787".into());
    let token        = std::env::var("SENSOR_BRIDGE_TOKEN").unwrap_or_default();
    let node_id      = std::env::var("SENSOR_NODE_ID").unwrap_or_else(|_| hostname());
    let sensorhead_url = std::env::var("SENSORHEAD_URL").ok();
    let interval_secs = std::env::var("SENSOR_INTERVAL_SECS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(30);

    let ws_url = if token.is_empty() {
        format!("ws://{host}/sensor-bridge")
    } else {
        format!("ws://{host}/sensor-bridge?token={token}")
    };

    let sensorhead = sensorhead_url.map(|url| {
        eprintln!("[apex-sensor-bridge] SensorHead polling enabled: {url}");
        SensorHeadClient::new(url)
    });

    eprintln!("[apex-sensor-bridge] node_id={node_id} interval={interval_secs}s");
    run(&ws_url, &node_id, Duration::from_secs(interval_secs), sensorhead);
}
