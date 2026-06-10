/// Peer registry — reads/writes /etc/agentd/peers.toml.
/// Shared via Arc<RwLock<PeerRegistry>> between gateway routes and the
/// discovery loop in main. All writes are atomic (write tmp → rename).
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum PeerRole {
    Full,
    Sensor,
    Thin,
}

impl Default for PeerRole {
    fn default() -> Self { PeerRole::Full }
}

impl std::fmt::Display for PeerRole {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PeerRole::Full   => write!(f, "full"),
            PeerRole::Sensor => write!(f, "sensor"),
            PeerRole::Thin   => write!(f, "thin"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerRecord {
    pub node_id: String,
    pub ws_url:  String,
    #[serde(default)]
    pub role:    PeerRole,
    #[serde(default = "online")]
    pub status:  String,
}

fn online() -> String { "online".into() }

#[derive(Debug, Clone, Deserialize, Default)]
struct PeersFile {
    #[serde(default)]
    peer: Vec<PeerRecord>,
}

#[derive(Debug, Clone)]
pub struct PeerRegistry {
    pub peers: Vec<PeerRecord>,
    pub path:  PathBuf,
}

impl PeerRegistry {
    pub fn load(path: &Path) -> Self {
        let peers = std::fs::read_to_string(path)
            .ok()
            .and_then(|s| toml::from_str::<PeersFile>(&s).ok())
            .map(|f| f.peer)
            .unwrap_or_default();
        PeerRegistry { peers, path: path.to_path_buf() }
    }

    pub fn contains(&self, node_id: &str) -> bool {
        self.peers.iter().any(|p| p.node_id == node_id)
    }

    pub fn add(&mut self, record: PeerRecord) -> std::io::Result<()> {
        if let Some(existing) = self.peers.iter_mut().find(|p| p.node_id == record.node_id) {
            *existing = record;
        } else {
            self.peers.push(record);
        }
        self.save()
    }

    pub fn remove(&mut self, node_id: &str) -> std::io::Result<bool> {
        let before = self.peers.len();
        self.peers.retain(|p| p.node_id != node_id);
        if self.peers.len() != before { self.save()?; Ok(true) } else { Ok(false) }
    }

    pub fn set_status(&mut self, node_id: &str, status: &str) -> std::io::Result<()> {
        if let Some(p) = self.peers.iter_mut().find(|p| p.node_id == node_id) {
            p.status = status.to_string();
        }
        self.save()
    }

    fn save(&self) -> std::io::Result<()> {
        let mut out = String::from("# ApexOS mesh peers — managed by agentd\n");
        for p in &self.peers {
            out.push_str(&format!(
                "\n[[peer]]\nnode_id = {:?}\nws_url  = {:?}\nrole    = {:?}\nstatus  = {:?}\n",
                p.node_id, p.ws_url, p.role.to_string(), p.status,
            ));
        }
        let tmp = self.path.with_extension("toml.tmp");
        std::fs::write(&tmp, &out)?;
        std::fs::rename(&tmp, &self.path)
    }
}

/// Parse `avahi-browse -rpt _apexos._tcp --no-db-lookup` stdout into (node_id, ip) pairs.
/// Only processes fully-resolved lines (starting with `=`).
pub fn parse_avahi_output(raw: &str) -> Vec<(String, String)> {
    let mut results = Vec::new();
    for line in raw.lines() {
        if !line.starts_with('=') { continue; }
        // =;eth0;IPv4;ApexOS apex-kitchen;_apexos._tcp;local;apex-kitchen.local;192.168.0.201;8787;...
        let parts: Vec<&str> = line.split(';').collect();
        if parts.len() < 9 { continue; }
        let hostname = parts[6].trim_end_matches(".local");
        let ip = parts[7];
        if !ip.is_empty() && !hostname.is_empty() {
            results.push((hostname.to_string(), ip.to_string()));
        }
    }
    results
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_resolved_line() {
        let raw = "=;eth0;IPv4;ApexOS apex-kitchen;_apexos._tcp;local;apex-kitchen.local;192.168.0.201;8787;\n\
                   +;eth0;IPv4;ApexOS apex-garage;_apexos._tcp;local;;;;\n";
        let nodes = parse_avahi_output(raw);
        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0].0, "apex-kitchen");
        assert_eq!(nodes[0].1, "192.168.0.201");
    }
}
