# apex-sensor-bridge

> Standalone WS client: CPU temp / SensorHead → agentd `/sensor-bridge`.

A small separate process that polls the node's CPU temperature (sysfs) and, if configured, an
external SensorHead dashboard (BME688 air quality, MLX90640 thermal), and pushes `SensorReading`
events to agentd over a token-authed WebSocket with reconnect backoff. Never touches `/dev/i2c`
itself (the sandbox is `PrivateDevices=true`) — air-quality/thermal come via HTTP from SensorHead.

- **Key files:** `src/main.rs` (`read_cpu_temp`, SensorHead HTTP poll, tungstenite WS push loop, reconnect backoff)
- **Depends on:** `serde`/`serde_json`, `tungstenite`, `reqwest` (blocking).
- **Lift via:** a self-contained sensor-push client — point `SENSORHEAD_URL` + the bridge WS at any endpoint. The poll → WS-push → reconnect loop is the reusable pattern.

Part of [ApexOS-RS](https://github.com/buckster123/ApexOS-RS) — see [`PATTERNS.md`](../../../PATTERNS.md) (lift-me index) and [`docs/repo-map.md`](../../../docs/repo-map.md) (full map).
