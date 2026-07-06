//! LAN compute discovery — find OpenAI-compatible inference endpoints (ollama, vLLM,
//! LM Studio, llama.cpp server) on the local network, so a node can adopt a nearby
//! GPU box as its brain from the Settings UI ("auto-discover compute").
//!
//! Operator-triggered only (the Settings SCAN button → GET /api/compute/discover),
//! never ambient — a subnet sweep is a deliberate act, like the Mesh REFRESH.
//!
//! Verification is the shape, not the port: a candidate counts only when
//! `GET /v1/models` answers with the OpenAI list shape (`data: [{id, …}]`), so an
//! arbitrary HTTP service on a probed port (e.g. SensorHead on :8080) is never
//! reported as compute. Probes send NO Authorization header — the stored OAI key is
//! the OpenRouter/cloud credential and must not be sprayed at LAN hosts.

use futures_util::{stream, StreamExt};
use std::collections::HashSet;
use std::net::{IpAddr, Ipv4Addr};
use std::time::Duration;

/// (port, kind) pairs probed on every candidate host, by convention:
/// ollama 11434 · vLLM 8000 · LM Studio 1234 · llama.cpp server 8080.
pub const OAI_PROBE_PORTS: &[(u16, &str)] = &[
    (11434, "ollama"),
    (8000, "vllm"),
    (1234, "lmstudio"),
    (8080, "llama.cpp"),
];

/// Cap on models reported per endpoint (an OpenRouter-style catalog behind a LAN
/// proxy shouldn't flood the Settings UI).
const MODELS_CAP: usize = 60;

/// Per-host TCP connect budget. LAN round-trips are ~1ms; 250ms is generous while
/// keeping a full /24 sweep at a few seconds under the concurrency cap.
const CONNECT_TIMEOUT: Duration = Duration::from_millis(250);
const HTTP_TIMEOUT: Duration = Duration::from_secs(2);
const CONCURRENCY: usize = 64;

#[derive(Clone, Debug, serde::Serialize)]
pub struct DiscoveredEndpoint {
    /// Ready-to-adopt OAI base URL, e.g. "http://192.168.0.42:11434/v1".
    pub url: String,
    pub kind: String,
    pub host: String,
    pub models: Vec<String>,
}

/// All /24 sibling hosts of `local`, excluding the network/broadcast addresses and
/// `local` itself (localhost is probed separately, first).
pub fn subnet_hosts(local: Ipv4Addr) -> Vec<Ipv4Addr> {
    let [a, b, c, d] = local.octets();
    (1u8..=254)
        .filter(|&h| h != d)
        .map(|h| Ipv4Addr::new(a, b, c, h))
        .collect()
}

/// Verify + extract a `/v1/models` answer. `Some(ids)` only for the OpenAI list shape;
/// anything else (SensorHead JSON, an HTML error page parsed as null) is `None`.
pub fn parse_models_json(v: &serde_json::Value) -> Option<Vec<String>> {
    let data = v.get("data")?.as_array()?;
    let ids: Vec<String> = data
        .iter()
        .filter_map(|m| m.get("id").and_then(|i| i.as_str()).map(String::from))
        .take(MODELS_CAP)
        .collect();
    Some(ids)
}

/// Extract candidate host strings from mesh-peer ws_urls ("ws://192.168.0.146:8787/ws").
pub fn peer_hosts(ws_urls: &[String]) -> Vec<String> {
    ws_urls
        .iter()
        .filter_map(|u| {
            let rest = u.split("://").nth(1)?;
            let hostport = rest.split('/').next()?;
            Some(hostport.split(':').next()?.to_string())
        })
        .filter(|h| !h.is_empty())
        .collect()
}

/// This node's primary IPv4, via the connected-UDP-socket trick (no packet is sent;
/// 192.0.2.1 is TEST-NET-1). None on a non-IPv4 / no-route host — sweep degrades to
/// localhost + peers.
fn local_ipv4() -> Option<Ipv4Addr> {
    let sock = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    sock.connect("192.0.2.1:9").ok()?;
    match sock.local_addr().ok()?.ip() {
        IpAddr::V4(v4) if !v4.is_loopback() => Some(v4),
        _ => None,
    }
}

async fn probe(client: reqwest::Client, host: String, port: u16, kind: String) -> Option<DiscoveredEndpoint> {
    let addr = format!("{host}:{port}");
    // Cheap reachability gate first, so dead hosts cost 250ms not an HTTP timeout.
    let connect = tokio::time::timeout(CONNECT_TIMEOUT, tokio::net::TcpStream::connect(&addr)).await;
    if !matches!(connect, Ok(Ok(_))) {
        return None;
    }
    let url = format!("http://{addr}/v1/models");
    let resp = client.get(&url).send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let body = resp.json::<serde_json::Value>().await.ok()?;
    let models = parse_models_json(&body)?;
    Some(DiscoveredEndpoint {
        url: format!("http://{addr}/v1"),
        kind,
        host,
        models,
    })
}

/// Sweep localhost + the local /24 + `extra_hosts` (mesh peers) across the probe
/// ports. Returns confirmed OAI endpoints, localhost first, then by host string.
pub async fn discover(extra_hosts: Vec<String>) -> Vec<DiscoveredEndpoint> {
    let client = reqwest::Client::builder()
        .timeout(HTTP_TIMEOUT)
        .build()
        .unwrap_or_default();

    let mut hosts: Vec<String> = vec!["127.0.0.1".into()];
    if let Some(local) = local_ipv4() {
        hosts.extend(subnet_hosts(local).into_iter().map(|ip| ip.to_string()));
    }
    hosts.extend(extra_hosts);
    let mut seen = HashSet::new();
    hosts.retain(|h| seen.insert(h.clone()));

    let candidates: Vec<(String, u16, String)> = hosts
        .into_iter()
        .flat_map(|h| OAI_PROBE_PORTS.iter().map(move |&(p, k)| (h.clone(), p, k.to_string())))
        .collect();

    let mut found: Vec<DiscoveredEndpoint> = stream::iter(candidates)
        .map(|(h, p, k)| probe(client.clone(), h, p, k))
        .buffer_unordered(CONCURRENCY)
        .filter_map(|r| async move { r })
        .collect()
        .await;

    found.sort_by(|a, b| {
        let a_local = a.host == "127.0.0.1";
        let b_local = b.host == "127.0.0.1";
        b_local.cmp(&a_local).then(a.host.cmp(&b.host)).then(a.url.cmp(&b.url))
    });
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subnet_hosts_excludes_self_network_broadcast() {
        let hosts = subnet_hosts(Ipv4Addr::new(192, 168, 0, 158));
        assert_eq!(hosts.len(), 253); // 254 usable minus self
        assert!(!hosts.contains(&Ipv4Addr::new(192, 168, 0, 158)));
        assert!(!hosts.contains(&Ipv4Addr::new(192, 168, 0, 0)));
        assert!(!hosts.contains(&Ipv4Addr::new(192, 168, 0, 255)));
        assert!(hosts.contains(&Ipv4Addr::new(192, 168, 0, 1)));
        assert!(hosts.contains(&Ipv4Addr::new(192, 168, 0, 254)));
    }

    #[test]
    fn parse_models_accepts_openai_shape_only() {
        let ok = serde_json::json!({"data": [{"id": "qwen3:27b"}, {"id": "moondream"}]});
        assert_eq!(parse_models_json(&ok).unwrap(), vec!["qwen3:27b", "moondream"]);

        // SensorHead-ish / arbitrary JSON must not count as compute.
        assert!(parse_models_json(&serde_json::json!({"frame": [1, 2, 3]})).is_none());
        assert!(parse_models_json(&serde_json::json!({"data": "nope"})).is_none());
        assert!(parse_models_json(&serde_json::Value::Null).is_none());

        // Empty model list is still a valid (idle) server.
        assert_eq!(parse_models_json(&serde_json::json!({"data": []})).unwrap().len(), 0);
    }

    #[test]
    fn peer_hosts_from_ws_urls() {
        let urls = vec![
            "ws://192.168.0.146:8787/ws".to_string(),
            "ws://apex1.local:8787/ws".to_string(),
            "garbage".to_string(),
        ];
        assert_eq!(peer_hosts(&urls), vec!["192.168.0.146", "apex1.local"]);
    }
}
