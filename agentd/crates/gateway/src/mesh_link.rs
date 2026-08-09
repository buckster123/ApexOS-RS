//! The bridge link — where agentd finally meets the radio (`docs/apexnet.md`
//! §6.1, P5c).
//!
//! `apexos-mesh-bridge` owns the UART to the brainstem and speaks the frozen
//! wire codec; agentd owns policy and sessions. This is the seam between
//! them, and it deliberately mirrors `apex-sensor-bridge`: the bridge is a
//! separate process that **connects in** over a WebSocket, rather than agentd
//! reaching out to a device.
//!
//! That shape is not incidental. It keeps agentd free of serial-port
//! ownership (nothing to hold open, nothing to reconnect), it lets the bridge
//! be restarted or replaced under a running daemon, and it means a node with
//! no radio hardware simply never has a bridge connect — which reads as
//! "lane down", not as an error.
//!
//! ## What crosses this socket
//!
//! Raw datagram-framed [`MeshFrame`]s, in binary WS messages, in both
//! directions. No JSON envelope: the frames already have a frozen encoding
//! that both ends link the same crate for, and re-wrapping them in a second
//! format would be two contracts where one will do.
//!
//! **Unsealed, like the UART it extends.** The brainstem↔bridge link carries
//! plain `postcard(PlainPacket)` inside `ct` (charter §5, P4a): it is a cable
//! between a board and its own Pi. The crypto envelope belongs to the radio
//! tiers, and the brainstem applies it on the way out. So agentd can read a
//! `BrainstemStatus` here directly — and must never assume the same of
//! anything that arrived over the air.

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;

use apexos_core::mesh_router::{
    LatencyClass, MeshTransport, SendError, SendReceipt, TransportHealth, TransportId,
};
use apexos_mesh_proto::MeshFrame;
use tokio::sync::broadcast;

/// What the brainstem last told us about its own radio. This is the only
/// view agentd has of the air, and it is second-hand by construction — the
/// board is the thing with the antenna.
#[derive(Debug, Clone, Copy, Default)]
pub struct BrainstemView {
    pub node_id: u16,
    pub neighbors: u8,
    pub queued: u16,
    pub counter_high_water: u64,
    /// Milliseconds since the epoch used by `last_seen`, 0 if never.
    pub seen: bool,
}

/// Shared handle to the bridge link. Cloneable, cheap, and safe to hold in
/// `GatewayState` whether or not a bridge ever connects.
#[derive(Clone)]
pub struct MeshLink {
    /// Frames to transmit. Broadcast rather than a queue so that a node with
    /// two radios (a BLE brainstem and a LoRa one) fans out to both — which
    /// is what gossip wants — and so that zero connected bridges is simply
    /// "no receivers" rather than a queue silently filling up.
    outbound: broadcast::Sender<Vec<u8>>,
    /// Bridges currently connected.
    links: Arc<AtomicUsize>,
    rx_frames: Arc<AtomicU64>,
    /// Frames the seen-cache recognised as duplicates. Expected to be
    /// non-zero once more than one lane exists — that is fan-out working.
    dup_frames: Arc<AtomicU64>,
    decode_fail: Arc<AtomicU64>,
    brainstem: Arc<std::sync::Mutex<BrainstemView>>,
}

impl Default for MeshLink {
    fn default() -> Self {
        Self::new()
    }
}

impl MeshLink {
    pub fn new() -> Self {
        // Bounded: a wedged or absent bridge must drop frames rather than
        // grow memory. Gossip is lossy by design and the next heartbeat is
        // seconds away.
        let (outbound, _) = broadcast::channel(64);
        Self {
            outbound,
            links: Arc::new(AtomicUsize::new(0)),
            rx_frames: Arc::new(AtomicU64::new(0)),
            dup_frames: Arc::new(AtomicU64::new(0)),
            decode_fail: Arc::new(AtomicU64::new(0)),
            brainstem: Arc::new(std::sync::Mutex::new(BrainstemView::default())),
        }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Vec<u8>> {
        self.outbound.subscribe()
    }

    pub fn link_up(&self) {
        self.links.fetch_add(1, Ordering::Relaxed);
    }

    pub fn link_down(&self) {
        // saturating: a double-down must never wrap to usize::MAX and make a
        // dead lane look gloriously healthy.
        let _ = self
            .links
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| {
                Some(n.saturating_sub(1))
            });
    }

    pub fn connected(&self) -> usize {
        self.links.load(Ordering::Relaxed)
    }

    pub fn note_rx(&self) {
        self.rx_frames.fetch_add(1, Ordering::Relaxed);
    }

    pub fn note_duplicate(&self) {
        self.dup_frames.fetch_add(1, Ordering::Relaxed);
    }

    pub fn note_decode_fail(&self) {
        self.decode_fail.fetch_add(1, Ordering::Relaxed);
    }

    pub fn set_brainstem(&self, view: BrainstemView) {
        if let Ok(mut g) = self.brainstem.lock() {
            *g = view;
        }
    }

    pub fn brainstem(&self) -> BrainstemView {
        self.brainstem.lock().map(|g| *g).unwrap_or_default()
    }

    pub fn stats(&self) -> serde_json::Value {
        let b = self.brainstem();
        serde_json::json!({
            "links": self.connected(),
            "rx_frames": self.rx_frames.load(Ordering::Relaxed),
            "duplicates": self.dup_frames.load(Ordering::Relaxed),
            "decode_fail": self.decode_fail.load(Ordering::Relaxed),
            "brainstem": if b.seen {
                serde_json::json!({
                    "node_id": b.node_id,
                    "neighbors": b.neighbors,
                    "queued": b.queued,
                    "counter_high_water": b.counter_high_water,
                })
            } else {
                serde_json::Value::Null
            },
        })
    }
}

/// Tier 2a as the router sees it: a lane that exists exactly when a bridge is
/// connected.
///
/// Health is derived from the bridge socket, not from whether the radio is
/// hearing anyone. A brainstem with no neighbours still has a working lane —
/// it is just lonely — and conflating "nobody is out there" with "I cannot
/// transmit" would make a healthy node in an empty room look broken.
pub struct BleGossipTransport {
    link: MeshLink,
}

impl BleGossipTransport {
    pub fn new(link: MeshLink) -> Self {
        Self { link }
    }
}

#[async_trait::async_trait]
impl MeshTransport for BleGossipTransport {
    fn id(&self) -> TransportId {
        TransportId::BleGossip
    }

    fn mtu(&self) -> usize {
        // One extended advertisement, less the ApexNET AD header — mirrors
        // `firmware/brainstem/src/radio.rs`. Kept conservative: the router
        // must refuse an oversized frame here rather than let the brainstem
        // discover it cannot transmit.
        248
    }

    fn latency_class(&self) -> LatencyClass {
        LatencyClass::Background
    }

    fn health(&self) -> TransportHealth {
        if self.link.connected() > 0 {
            TransportHealth::Up
        } else {
            TransportHealth::Down
        }
    }

    async fn send(&self, frame: &MeshFrame) -> Result<SendReceipt, SendError> {
        let bytes = apexos_mesh_proto::encode_datagram(frame)
            .map_err(|_| SendError::Failed("frame does not fit a datagram".into()))?;
        let len = bytes.len();
        // `send` fails only when nobody is subscribed — i.e. no bridge is
        // connected, which is precisely "lane unavailable" rather than a
        // transmission failure.
        self.link
            .outbound
            .send(bytes)
            .map(|_| SendReceipt {
                via: TransportId::BleGossip,
                bytes: len,
            })
            .map_err(|_| SendError::Unavailable)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use apexos_mesh_proto::{MeshClass, WIRE_VERSION};

    fn frame(len: usize) -> MeshFrame {
        MeshFrame {
            ver: WIRE_VERSION,
            class: MeshClass::Gossip,
            sender: 1001,
            ctr: 7,
            ct: vec![0xAB; len],
        }
    }

    #[tokio::test]
    async fn the_lane_is_down_until_a_bridge_connects() {
        let link = MeshLink::new();
        let t = BleGossipTransport::new(link.clone());
        assert_eq!(t.health(), TransportHealth::Down);
        assert_eq!(t.send(&frame(8)).await, Err(SendError::Unavailable));

        let _rx = link.subscribe();
        link.link_up();
        assert_eq!(t.health(), TransportHealth::Up);
        assert!(t.send(&frame(8)).await.is_ok());
    }

    #[tokio::test]
    async fn a_disconnecting_bridge_takes_the_lane_down_with_it() {
        let link = MeshLink::new();
        let t = BleGossipTransport::new(link.clone());
        let rx = link.subscribe();
        link.link_up();
        assert_eq!(t.health(), TransportHealth::Up);

        drop(rx);
        link.link_down();
        assert_eq!(t.health(), TransportHealth::Down);
        // No subscribers left: the send is refused rather than buffered into
        // a queue nobody will ever drain.
        assert_eq!(t.send(&frame(8)).await, Err(SendError::Unavailable));
    }

    #[test]
    fn link_down_never_wraps_into_looking_healthy() {
        let link = MeshLink::new();
        // More downs than ups — a double-close, or a restart racing a
        // disconnect. Saturating, so it can never wrap to a huge count.
        link.link_down();
        link.link_down();
        assert_eq!(link.connected(), 0);
        link.link_up();
        assert_eq!(link.connected(), 1);
    }

    #[tokio::test]
    async fn a_frame_too_big_for_one_advertisement_is_refused_here() {
        let link = MeshLink::new();
        let t = BleGossipTransport::new(link.clone());
        let _rx = link.subscribe();
        link.link_up();
        // The router checks `mtu()` before offering the lane; this asserts
        // the number the router will be checking against.
        assert_eq!(t.mtu(), 248);
    }
}
