//! Tier 1 as the router sees it (ApexNET P5d): today's HTTP mesh paths.
//!
//! Health is the LAN, not the WAN — a node that can still POST to a peer is
//! Up even when `api.anthropic.com` is not. The watcher writes that fact
//! through [`apexos_core::mesh_lanes::set_wifi_health`]; this lane only reads.

use apexos_core::mesh_router::{
    LatencyClass, MeshTransport, SendError, SendReceipt, TransportHealth, TransportId,
};
use apexos_mesh_proto::{MeshFrame, Payload, PlainPacket};
use serde::{Deserialize, Serialize};

/// What WifiLan (and the BLE fallback) carry for a `send_to_agent`.
/// Named-peer HTTP needs the string `node`; the radio only needs `target`
/// on the packet. Both live here so one encode serves every lane.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct A2aEnvelope {
    pub node: String,
    pub session_id: u64,
    pub message: String,
    pub from: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin_session: Option<u64>,
}

pub struct PeerDest {
    pub ws_url: String,
    pub token: Option<String>,
    pub radio_id: Option<u16>,
}

/// Look up a peer in peers.toml (same file `find_peer` has always read).
pub fn lookup_peer(node_id: &str) -> Option<PeerDest> {
    #[derive(Deserialize)]
    struct PeersFile {
        #[serde(default)]
        peer: Vec<PeerEntry>,
    }
    #[derive(Deserialize)]
    struct PeerEntry {
        node_id: String,
        ws_url: String,
        #[serde(default)]
        token: Option<String>,
        #[serde(default)]
        radio_id: Option<u16>,
    }
    let path = std::env::var("PEERS_TOML").unwrap_or_else(|_| "/etc/agentd/peers.toml".into());
    let raw = std::fs::read_to_string(path).ok()?;
    let file: PeersFile = toml::from_str(&raw).ok()?;
    file.peer
        .into_iter()
        .find(|p| p.node_id == node_id)
        .map(|p| PeerDest {
            ws_url: p.ws_url,
            token: p.token,
            radio_id: p.radio_id.or_else(|| radio_map_lookup(node_id)),
        })
}

/// `APEXNET_RADIO_MAP=ApexOS-2=7,ApexOS-RS=3` — bench/override when
/// peers.toml has no `radio_id` yet.
pub fn radio_map_lookup(node_id: &str) -> Option<u16> {
    let raw = std::env::var("APEXNET_RADIO_MAP").ok()?;
    for part in raw.split(',') {
        let (name, id) = part.split_once('=')?;
        if name.trim() == node_id {
            return id.trim().parse().ok();
        }
    }
    None
}

pub fn radio_for_peer(node_id: &str) -> Option<u16> {
    lookup_peer(node_id)
        .and_then(|p| p.radio_id)
        .or_else(|| radio_map_lookup(node_id))
}

pub fn encode_a2a_frame(target: u16, env: &A2aEnvelope) -> Result<MeshFrame, String> {
    let body = serde_json::to_vec(env).map_err(|e| format!("a2a encode: {e}"))?;
    let packet = PlainPacket {
        target,
        hop_limit: apexos_mesh_proto::DEFAULT_HOP_LIMIT,
        flags: 0,
        payload: Payload::A2A { body },
    };
    let ct = postcard::to_allocvec(&packet).map_err(|e| format!("a2a postcard: {e}"))?;
    Ok(MeshFrame {
        ver: apexos_mesh_proto::WIRE_VERSION,
        class: apexos_mesh_proto::MeshClass::Gossip,
        sender: 0,
        ctr: 1,
        ct,
    })
}

/// POST `/api/sessions/{id}/message` — the path send_to_agent has always used.
pub async fn http_deliver_a2a(env: &A2aEnvelope) -> Result<serde_json::Value, String> {
    let dest = lookup_peer(&env.node)
        .ok_or_else(|| format!("send_to_agent: peer '{}' not found in peers.toml", env.node))?;
    let http_base = dest
        .ws_url
        .replacen("ws://", "http://", 1)
        .replacen("wss://", "https://", 1);
    let url = format!("{http_base}/api/sessions/{}/message", env.session_id);
    let mut outbound = serde_json::json!({
        "message": env.message,
        "from": env.from,
    });
    if let Some(o) = env.origin_session {
        outbound["origin_session"] = serde_json::json!(o);
    }
    let mut req = reqwest::Client::new()
        .post(&url)
        .json(&outbound)
        .timeout(std::time::Duration::from_secs(15));
    if let Some(tok) = dest.token.as_deref() {
        req = req.bearer_auth(tok);
    }
    let resp = req.send().await.map_err(|e| e.to_string())?;
    let status = resp.status();
    let body_json = resp.json::<serde_json::Value>().await.ok();
    let body_ok = body_json.as_ref().and_then(|v| v["ok"].as_bool());
    let landed = body_json.as_ref().and_then(|v| v["session_id"].as_u64());
    let ok = status.is_success() && body_ok != Some(false);
    let detail = match (&dest.token, status.as_u16()) {
        (None, _) => "no token stored for peer — set one to reach a token-gated node",
        (Some(_), 401) => "peer rejected the token (401) — stale credential?",
        _ => {
            if ok {
                "sent"
            } else {
                "delivery failed"
            }
        }
    };
    let mut content = serde_json::json!({
        "status": if ok { "sent" } else { "error" },
        "detail": detail,
        "node": env.node,
        "via": "wifi-lan",
    });
    match landed {
        Some(s) => content["landed_session"] = serde_json::json!(s),
        None => content["target_session"] = serde_json::json!(env.session_id),
    }
    if ok {
        Ok(content)
    } else {
        Err(format!("{detail} (status {status})"))
    }
}

pub struct WifiLanTransport;

impl WifiLanTransport {
    pub fn new() -> Self {
        Self
    }
}

impl Default for WifiLanTransport {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl MeshTransport for WifiLanTransport {
    fn id(&self) -> TransportId {
        TransportId::WifiLan
    }

    fn mtu(&self) -> usize {
        // HTTP a2a is large; files do not ride a MeshFrame (see mesh_file_send).
        64 * 1024
    }

    fn latency_class(&self) -> LatencyClass {
        LatencyClass::Interactive
    }

    fn health(&self) -> TransportHealth {
        apexos_core::mesh_lanes::wifi_health()
    }

    async fn send(&self, frame: &MeshFrame) -> Result<SendReceipt, SendError> {
        let (packet, _): (PlainPacket, _) = postcard::take_from_bytes(&frame.ct)
            .map_err(|_| SendError::Failed("wifi-lan: undecodable frame".into()))?;
        let Payload::A2A { body } = packet.payload else {
            return Err(SendError::Failed(
                "wifi-lan: only A2A frames ride this lane in P5d".into(),
            ));
        };
        let env: A2aEnvelope = serde_json::from_slice(&body)
            .map_err(|e| SendError::Failed(format!("wifi-lan: a2a envelope: {e}")))?;
        match http_deliver_a2a(&env).await {
            Ok(_) => Ok(SendReceipt {
                via: TransportId::WifiLan,
                bytes: body.len(),
            }),
            Err(e) => {
                // So the next Gossip send falls through to BLE immediately.
                apexos_core::mesh_lanes::set_wifi_health(TransportHealth::Flaky);
                Err(SendError::Failed(e))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn radio_map_parses_pairs() {
        std::env::set_var("APEXNET_RADIO_MAP", "ApexOS-2=7, ApexOS-RS=3");
        assert_eq!(radio_map_lookup("ApexOS-2"), Some(7));
        assert_eq!(radio_map_lookup("ApexOS-RS"), Some(3));
        assert_eq!(radio_map_lookup("tvpi"), None);
        std::env::remove_var("APEXNET_RADIO_MAP");
    }

    #[test]
    fn a2a_envelope_roundtrips_in_a_frame() {
        let env = A2aEnvelope {
            node: "ApexOS-2".into(),
            session_id: 13,
            message: "hello".into(),
            from: "ApexOS-RS".into(),
            origin_session: Some(103),
        };
        let frame = encode_a2a_frame(7, &env).unwrap();
        let (packet, _): (PlainPacket, _) = postcard::take_from_bytes(&frame.ct).unwrap();
        assert_eq!(packet.target, 7);
        let Payload::A2A { body } = packet.payload else {
            panic!("expected A2A");
        };
        let back: A2aEnvelope = serde_json::from_slice(&body).unwrap();
        assert_eq!(back, env);
    }

    #[test]
    fn mesh_bridge_unit_keeps_the_uart() {
        let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../..");
        let unit = std::fs::read_to_string(root.join("deploy/apexos-mesh-bridge.service"))
            .expect("apexos-mesh-bridge.service");
        assert!(unit.contains("User=agentd"));
        assert!(unit.contains("SupplementaryGroups=dialout"));
        assert!(
            !unit.contains("PrivateDevices=yes"),
            "PrivateDevices would hide the brainstem UART"
        );
        assert!(unit.contains("DeviceAllow=/dev/ttyACM*"));
        assert!(unit.contains("EnvironmentFile=-/etc/agentd/env"));
        assert!(!unit.contains("EnvironmentFile=-/etc/agentd/ui.env"));
    }
}
