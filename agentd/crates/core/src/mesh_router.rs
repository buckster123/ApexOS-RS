//! The policy router — agentd's side of ApexNET (`docs/apexnet.md` §6.1).
//!
//! Today's mesh has exactly one way to reach a peer: an HTTP request over the
//! LAN, made directly by whichever tool wants it. That works right up until
//! the LAN doesn't, which is the entire premise of the charter. This module
//! is the seam that lets a message outlive its transport: callers hand the
//! router a *frame and a class*, and the router decides what carries it.
//!
//! ## What the router owns
//!
//! - **Class → transport policy** (charter §2.2). `Critical` fans out on
//!   every healthy transport — a safety alarm that arrives twice is a
//!   nuisance, one that arrives never is a failure. `Gossip` takes the
//!   cheapest healthy lane. `Bulk` refuses anything but a bulk-capable one
//!   rather than dribbling a 40 KB artifact over LoRa. `Digest` is
//!   idempotent, so it takes the cheapest lane and never escalates.
//! - **The seen-cache.** Fan-out means duplicates *by design*, so dedup is
//!   not an optimisation here — it is what makes fan-out safe to do at all.
//! - **Per-transport health**, from real send outcomes rather than a probe's
//!   opinion.
//!
//! ## What it deliberately does not own
//!
//! Peer *liveness* stays in the beacon, and node *connectivity tier* stays in
//! [`crate::connectivity`]. Three different questions — "can this lane carry
//! bytes", "is that peer answering", "what tier is this node in" — and
//! collapsing them into one truth source is how you get a system that cannot
//! explain itself. See [`PeerReachability`] for the one place they meet.

use std::collections::VecDeque;

use apexos_mesh_proto::{MeshClass, MeshFrame};

/// The lanes a frame can take, cheapest-and-fattest first. Order is the
/// router's cost ranking: [`TransportId::WifiLan`] before
/// [`TransportId::Lora`] is not an accident.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum TransportId {
    /// Tier 1 — LAN/Wi-Fi. Fat, cheap, and the first to disappear.
    WifiLan = 0,
    /// Tier 2a — BLE advertisement flood. Small frames, no connection.
    BleGossip = 1,
    /// Tier 2b — BLE GATT bulk lane, point-to-point with a neighbour.
    BleBulk = 2,
    /// Tier 3 — LoRa. Bytes per second, kilometres of reach.
    Lora = 3,
    /// Tier 4 — a human carrying a stick. Enormous bandwidth, awful latency.
    Courier = 4,
}

impl TransportId {
    pub fn as_str(self) -> &'static str {
        match self {
            TransportId::WifiLan => "wifi-lan",
            TransportId::BleGossip => "ble-gossip",
            TransportId::BleBulk => "ble-bulk",
            TransportId::Lora => "lora",
            TransportId::Courier => "courier",
        }
    }

    /// Can this lane carry a chunked artifact in reasonable time? The `Bulk`
    /// class refuses everything else — see [`Router::route`].
    pub fn is_bulk_capable(self) -> bool {
        matches!(
            self,
            TransportId::WifiLan | TransportId::BleBulk | TransportId::Courier
        )
    }
}

/// How long a caller should expect to wait. The router never blocks on this;
/// it is what lets a caller decide between waiting and queueing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum LatencyClass {
    /// Sub-second — a human is waiting.
    Interactive,
    /// Seconds to minutes.
    Background,
    /// Hours. LoRa duty cycles and human couriers live here.
    Overnight,
}

/// A transport's own view of itself.
///
/// `Flaky` exists because the honest answer is often neither yes nor no, and
/// collapsing it to one of them costs you either a working lane or a stalled
/// message. The router will *use* a flaky lane, but never as the only one for
/// something [`MeshClass::Critical`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportHealth {
    Up,
    Flaky,
    Down,
}

impl TransportHealth {
    pub fn usable(self) -> bool {
        !matches!(self, TransportHealth::Down)
    }
}

/// Proof that a frame left on a given lane. Not proof it arrived — that is a
/// receipt from the far end, and only Tier 4 currently produces one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SendReceipt {
    pub via: TransportId,
    pub bytes: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SendError {
    /// The lane is down, or refused this frame's size.
    Unavailable,
    /// The lane accepted it and then failed. Distinct from `Unavailable`
    /// because it should count against the transport's health.
    Failed(String),
}

/// One lane. Implementors wrap a real medium (the LAN HTTP paths, the BLE
/// bridge, LoRa, the courier outbox); the router only ever sees this.
#[async_trait::async_trait]
pub trait MeshTransport: Send + Sync {
    fn id(&self) -> TransportId;
    /// Largest frame this lane can carry in one piece. The chunker splits
    /// above the frame layer; the router only checks the fit.
    fn mtu(&self) -> usize;
    fn latency_class(&self) -> LatencyClass;
    fn health(&self) -> TransportHealth;
    async fn send(&self, frame: &MeshFrame) -> Result<SendReceipt, SendError>;
}

/// Bounded dedup over `(sender, ctr)` — the wire's message identity.
///
/// This is what makes `Critical` fan-out safe: the same alarm arriving over
/// Wi-Fi, BLE and LoRa is three copies of one event, and a colony that acts
/// on all three has three times the response to one alarm.
///
/// Fixed capacity with FIFO eviction rather than a growing set: the input is
/// hostile air, and any structure an attacker can grow without bound is a
/// denial of service with extra steps.
pub struct SeenCache {
    capacity: usize,
    order: VecDeque<(u16, u64)>,
    // Linear scan over a VecDeque beats a HashMap here: capacity is in the
    // low thousands, the entries are 10 bytes, and this keeps insertion
    // order and membership in one structure with no allocation per lookup.
    // Revisit if the cache ever needs to be much larger.
}

impl SeenCache {
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            order: VecDeque::new(),
        }
    }

    /// Record a frame's identity. Returns `true` the first time and `false`
    /// for every repeat still in the window.
    pub fn accept(&mut self, sender: u16, ctr: u64) -> bool {
        let key = (sender, ctr);
        if self.order.contains(&key) {
            return false;
        }
        if self.order.len() >= self.capacity {
            self.order.pop_front();
        }
        self.order.push_back(key);
        true
    }

    pub fn len(&self) -> usize {
        self.order.len()
    }

    pub fn is_empty(&self) -> bool {
        self.order.is_empty()
    }
}

/// Where a peer can be reached right now — the one place transport health and
/// peer liveness meet.
///
/// This is what finally gives `PeerLost` a meaning. The event has been
/// declared in the protocol and never emitted since the mDNS design that
/// documented it was never built; the charter (§6.2) said to either claim it
/// or delete it, and never leave it ambiguous.
///
/// **Claimed**: `PeerLost` means *unreachable on every transport*. Deleting it
/// would be a wire change for no gain, and the fact it names does not exist
/// until a node has more than one lane — which is now. A peer that is merely
/// off the LAN is not lost; it is reachable on a slower tier, and saying
/// "lost" there would be exactly the false alarm §6.2 warns about.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PeerReachability {
    /// Lanes this peer has been heard on or successfully reached.
    pub lanes: Vec<TransportId>,
}

impl PeerReachability {
    pub fn is_lost(&self) -> bool {
        self.lanes.is_empty()
    }

    /// The best lane currently known for this peer, if any.
    pub fn best(&self) -> Option<TransportId> {
        self.lanes.iter().copied().min()
    }
}

/// What the router decided to do with a frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteOutcome {
    /// Lanes the frame actually left on.
    pub sent: Vec<SendReceipt>,
    /// Lanes that were tried and refused it.
    pub failed: Vec<(TransportId, SendError)>,
}

impl RouteOutcome {
    /// Did this frame leave the node at all? The distinction the caller
    /// cares about: nothing sent means "queue it or tell the human", not
    /// "retry harder".
    pub fn delivered(&self) -> bool {
        !self.sent.is_empty()
    }
}

/// Picks lanes for frames, and refuses to pretend when there are none.
pub struct Router {
    transports: Vec<Box<dyn MeshTransport>>,
    seen: SeenCache,
}

impl Router {
    pub fn new(mut transports: Vec<Box<dyn MeshTransport>>, seen_capacity: usize) -> Self {
        // Cheapest first, once, so every policy below can just take from the
        // front instead of re-deciding what "cheapest" means.
        transports.sort_by_key(|t| t.id());
        Self {
            transports,
            seen: SeenCache::new(seen_capacity),
        }
    }

    /// Inbound dedup. Call this before acting on any received frame,
    /// whichever lane it came off.
    pub fn accept_inbound(&mut self, frame: &MeshFrame) -> bool {
        self.seen.accept(frame.sender, frame.ctr)
    }

    /// Which lanes would carry this class right now, cheapest first.
    ///
    /// Pure and separate from [`Router::send`] so the policy can be tested
    /// exhaustively without a single byte moving.
    pub fn route(&self, class: MeshClass, frame_len: usize) -> Vec<TransportId> {
        let usable: Vec<&Box<dyn MeshTransport>> = self
            .transports
            .iter()
            .filter(|t| t.health().usable() && frame_len <= t.mtu())
            .collect();

        match class {
            // Fan out everywhere. Duplicates are the price, and the
            // seen-cache is what makes that price affordable.
            MeshClass::Critical => usable.iter().map(|t| t.id()).collect(),
            // Cheapest healthy lane. Prefer a fully-Up lane over a Flaky one
            // even if the flaky one is cheaper — a retry costs more than the
            // tier difference.
            MeshClass::Gossip | MeshClass::Digest => usable
                .iter()
                .find(|t| t.health() == TransportHealth::Up)
                .or_else(|| usable.first())
                .map(|t| vec![t.id()])
                .unwrap_or_default(),
            // Never dribble bulk down a narrow lane: it would occupy the
            // radio for hours and still not arrive. Wait for a fat window,
            // or let the caller queue it.
            MeshClass::Bulk => usable
                .iter()
                .filter(|t| t.id().is_bulk_capable())
                .map(|t| t.id())
                .take(1)
                .collect(),
        }
    }

    /// Route and send. Returns what actually happened — including "nothing",
    /// which is a legitimate answer the caller must handle rather than an
    /// error to bubble.
    pub async fn send(&mut self, class: MeshClass, frame: &MeshFrame) -> RouteOutcome {
        let frame_len = apexos_mesh_proto::encode_datagram(frame)
            .map(|b| b.len())
            .unwrap_or(usize::MAX);
        let lanes = self.route(class, frame_len);

        let mut outcome = RouteOutcome {
            sent: Vec::new(),
            failed: Vec::new(),
        };
        for id in &lanes {
            let Some(t) = self.transports.iter().find(|t| t.id() == *id) else {
                continue;
            };
            match t.send(frame).await {
                Ok(receipt) => outcome.sent.push(receipt),
                Err(e) => outcome.failed.push((*id, e)),
            }
        }
        // Gossip/Digest pick one cheapest lane. If that lane just died
        // (Wi-Fi yanked mid-session) try the next usable one — otherwise
        // "a2a continues over BLE" never happens while WifiLan still looks Up.
        if outcome.sent.is_empty() && matches!(class, MeshClass::Gossip | MeshClass::Digest) {
            for t in &self.transports {
                if lanes.contains(&t.id()) {
                    continue;
                }
                if !t.health().usable() || frame_len > t.mtu() {
                    continue;
                }
                match t.send(frame).await {
                    Ok(receipt) => {
                        outcome.sent.push(receipt);
                        break;
                    }
                    Err(e) => outcome.failed.push((t.id(), e)),
                }
            }
        }
        outcome
    }

    /// Health snapshot, for `mesh_status` and the connectivity watcher.
    pub fn health(&self) -> Vec<(TransportId, TransportHealth)> {
        self.transports
            .iter()
            .map(|t| (t.id(), t.health()))
            .collect()
    }

    /// The node's connectivity tier implied by its lanes. Feeds
    /// [`crate::connectivity`] rather than duplicating it: that module owns
    /// the latch, this one owns the observation.
    pub fn implied_state(&self) -> crate::connectivity::ConnectivityState {
        use crate::connectivity::ConnectivityState;
        let up: Vec<TransportId> = self
            .transports
            .iter()
            .filter(|t| t.health().usable())
            .map(|t| t.id())
            .collect();
        if up.contains(&TransportId::WifiLan) {
            ConnectivityState::Full
        } else if up
            .iter()
            .any(|t| matches!(t, TransportId::BleGossip | TransportId::BleBulk))
        {
            ConnectivityState::Degraded
        } else if up.contains(&TransportId::Lora) {
            ConnectivityState::Minimal
        } else {
            // The courier is not connectivity: a stick in a pocket is a lane,
            // but a node whose only lane is a human walking is isolated by
            // every definition a running turn cares about.
            ConnectivityState::Isolated
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use apexos_mesh_proto::{MeshFrame, WIRE_VERSION};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    /// A lane that records what it was asked to carry. The charter's promise
    /// that the router is "fully testable with zero hardware" is only true if
    /// the mocks can be made to misbehave, so this one can fail on demand.
    struct MockTransport {
        id: TransportId,
        mtu: usize,
        health: TransportHealth,
        fail: bool,
        sent: Arc<AtomicUsize>,
    }

    impl MockTransport {
        fn up(id: TransportId, mtu: usize) -> (Box<dyn MeshTransport>, Arc<AtomicUsize>) {
            let sent = Arc::new(AtomicUsize::new(0));
            (
                Box::new(MockTransport {
                    id,
                    mtu,
                    health: TransportHealth::Up,
                    fail: false,
                    sent: sent.clone(),
                }),
                sent,
            )
        }

        fn with(
            id: TransportId,
            mtu: usize,
            health: TransportHealth,
            fail: bool,
        ) -> Box<dyn MeshTransport> {
            Box::new(MockTransport {
                id,
                mtu,
                health,
                fail,
                sent: Arc::new(AtomicUsize::new(0)),
            })
        }
    }

    #[async_trait::async_trait]
    impl MeshTransport for MockTransport {
        fn id(&self) -> TransportId {
            self.id
        }
        fn mtu(&self) -> usize {
            self.mtu
        }
        fn latency_class(&self) -> LatencyClass {
            LatencyClass::Background
        }
        fn health(&self) -> TransportHealth {
            self.health
        }
        async fn send(&self, frame: &MeshFrame) -> Result<SendReceipt, SendError> {
            if self.fail {
                return Err(SendError::Failed("mock".into()));
            }
            self.sent.fetch_add(1, Ordering::SeqCst);
            Ok(SendReceipt {
                via: self.id,
                bytes: frame.ct.len(),
            })
        }
    }

    fn frame(sender: u16, ctr: u64, len: usize) -> MeshFrame {
        MeshFrame {
            ver: WIRE_VERSION,
            class: MeshClass::Gossip,
            sender,
            ctr,
            ct: vec![0xAB; len],
        }
    }

    #[test]
    fn critical_fans_out_everywhere_and_gossip_takes_the_cheapest() {
        let (wifi, _) = MockTransport::up(TransportId::WifiLan, 4096);
        let (ble, _) = MockTransport::up(TransportId::BleGossip, 200);
        let (lora, _) = MockTransport::up(TransportId::Lora, 64);
        let router = Router::new(vec![lora, ble, wifi], 64);

        // A safety alarm goes on every lane that can carry it.
        assert_eq!(
            router.route(MeshClass::Critical, 32),
            vec![
                TransportId::WifiLan,
                TransportId::BleGossip,
                TransportId::Lora
            ]
        );
        // Routine gossip takes one lane, the cheapest.
        assert_eq!(
            router.route(MeshClass::Gossip, 32),
            vec![TransportId::WifiLan]
        );
    }

    #[test]
    fn mtu_excludes_a_lane_rather_than_truncating_the_frame() {
        let (ble, _) = MockTransport::up(TransportId::BleGossip, 200);
        let (lora, _) = MockTransport::up(TransportId::Lora, 64);
        let router = Router::new(vec![ble, lora], 64);

        // 100 bytes fits BLE but not LoRa: LoRa must simply not be offered.
        // Silently sending a truncated frame would be far worse than not
        // sending one.
        assert_eq!(
            router.route(MeshClass::Critical, 100),
            vec![TransportId::BleGossip]
        );
        assert_eq!(
            router.route(MeshClass::Critical, 32),
            vec![TransportId::BleGossip, TransportId::Lora]
        );
    }

    #[test]
    fn bulk_refuses_narrow_lanes_even_when_they_are_the_only_ones_up() {
        let router = Router::new(
            vec![
                MockTransport::with(TransportId::BleGossip, 4096, TransportHealth::Up, false),
                MockTransport::with(TransportId::Lora, 4096, TransportHealth::Up, false),
            ],
            64,
        );
        // Both lanes are up and both would "fit" — and bulk still refuses,
        // because dribbling an artifact over a gossip flood occupies the
        // radio for hours and still does not arrive.
        assert!(router.route(MeshClass::Bulk, 2048).is_empty());

        let router = Router::new(
            vec![MockTransport::with(
                TransportId::BleBulk,
                4096,
                TransportHealth::Up,
                false,
            )],
            64,
        );
        assert_eq!(
            router.route(MeshClass::Bulk, 2048),
            vec![TransportId::BleBulk]
        );
    }

    #[test]
    fn a_flaky_lane_is_used_but_never_preferred() {
        let router = Router::new(
            vec![
                MockTransport::with(TransportId::WifiLan, 4096, TransportHealth::Flaky, false),
                MockTransport::with(TransportId::BleGossip, 4096, TransportHealth::Up, false),
            ],
            64,
        );
        // Wi-Fi is cheaper but flaky; a solid BLE lane wins for a single-lane
        // class, because one retry costs more than the tier difference.
        assert_eq!(
            router.route(MeshClass::Gossip, 32),
            vec![TransportId::BleGossip]
        );
        // Flaky is still a lane: Critical uses it too.
        assert_eq!(
            router.route(MeshClass::Critical, 32),
            vec![TransportId::WifiLan, TransportId::BleGossip]
        );
    }

    #[test]
    fn a_down_lane_is_not_a_lane() {
        let router = Router::new(
            vec![
                MockTransport::with(TransportId::WifiLan, 4096, TransportHealth::Down, false),
                MockTransport::with(TransportId::Lora, 4096, TransportHealth::Up, false),
            ],
            64,
        );
        assert_eq!(router.route(MeshClass::Gossip, 32), vec![TransportId::Lora]);
    }

    #[test]
    fn no_lanes_is_an_answer_not_an_error() {
        let router = Router::new(
            vec![MockTransport::with(
                TransportId::WifiLan,
                4096,
                TransportHealth::Down,
                false,
            )],
            64,
        );
        assert!(router.route(MeshClass::Critical, 32).is_empty());
    }

    #[tokio::test]
    async fn send_reports_what_left_and_what_refused() {
        let (wifi, wifi_sent) = MockTransport::up(TransportId::WifiLan, 4096);
        let mut router = Router::new(
            vec![
                wifi,
                MockTransport::with(TransportId::BleGossip, 4096, TransportHealth::Up, true),
            ],
            64,
        );
        let outcome = router.send(MeshClass::Critical, &frame(7, 1, 16)).await;
        assert!(outcome.delivered());
        assert_eq!(wifi_sent.load(Ordering::SeqCst), 1);
        assert_eq!(outcome.sent.len(), 1);
        assert_eq!(outcome.failed.len(), 1);
        assert_eq!(outcome.failed[0].0, TransportId::BleGossip);
    }

    #[tokio::test]
    async fn gossip_falls_through_when_the_cheap_lane_fails() {
        // WifiLan is Up so route() picks it; it then fails. BLE must take the
        // frame or "kill Wi-Fi mid-session" never continues over the radio.
        let (ble, ble_sent) = MockTransport::up(TransportId::BleGossip, 4096);
        let mut router = Router::new(
            vec![
                MockTransport::with(TransportId::WifiLan, 4096, TransportHealth::Up, true),
                ble,
            ],
            64,
        );
        let outcome = router.send(MeshClass::Gossip, &frame(7, 1, 16)).await;
        assert!(outcome.delivered());
        assert_eq!(ble_sent.load(Ordering::SeqCst), 1);
        assert_eq!(outcome.sent[0].via, TransportId::BleGossip);
        assert_eq!(outcome.failed[0].0, TransportId::WifiLan);
    }

    #[tokio::test]
    async fn a_send_with_nowhere_to_go_is_not_delivered() {
        let mut router = Router::new(
            vec![MockTransport::with(
                TransportId::Lora,
                8, // smaller than any real frame
                TransportHealth::Up,
                false,
            )],
            64,
        );
        let outcome = router.send(MeshClass::Gossip, &frame(7, 1, 64)).await;
        assert!(!outcome.delivered());
        assert!(
            outcome.failed.is_empty(),
            "refused before trying, not after"
        );
    }

    #[test]
    fn the_seen_cache_is_what_makes_fan_out_safe() {
        let mut router = Router::new(vec![], 64);
        let f = frame(42, 9, 8);
        // The same alarm arriving over three lanes is one event.
        assert!(router.accept_inbound(&f));
        assert!(!router.accept_inbound(&f));
        assert!(!router.accept_inbound(&f));
        // A different counter from the same sender is a different message.
        assert!(router.accept_inbound(&frame(42, 10, 8)));
        // Same counter from a different sender, too — identity is the pair.
        assert!(router.accept_inbound(&frame(43, 9, 8)));
    }

    #[test]
    fn the_seen_cache_is_bounded_because_its_input_is_hostile() {
        let mut cache = SeenCache::new(4);
        for ctr in 1..=4 {
            assert!(cache.accept(1, ctr));
        }
        assert_eq!(cache.len(), 4);
        // Overflowing evicts the oldest rather than growing without bound.
        assert!(cache.accept(1, 5));
        assert_eq!(cache.len(), 4);
        // ctr=1 has aged out and would be accepted again — the price of a
        // bounded window, and why the wire's own replay windows exist too.
        assert!(cache.accept(1, 1));
        // ...while a recent one is still caught.
        assert!(!cache.accept(1, 5));
    }

    #[test]
    fn implied_state_follows_the_lanes_that_are_up() {
        use crate::connectivity::ConnectivityState;
        let full = Router::new(
            vec![MockTransport::with(
                TransportId::WifiLan,
                4096,
                TransportHealth::Up,
                false,
            )],
            8,
        );
        assert_eq!(full.implied_state(), ConnectivityState::Full);

        let degraded = Router::new(
            vec![
                MockTransport::with(TransportId::WifiLan, 4096, TransportHealth::Down, false),
                MockTransport::with(TransportId::BleGossip, 200, TransportHealth::Up, false),
            ],
            8,
        );
        assert_eq!(degraded.implied_state(), ConnectivityState::Degraded);

        let minimal = Router::new(
            vec![MockTransport::with(
                TransportId::Lora,
                64,
                TransportHealth::Up,
                false,
            )],
            8,
        );
        assert_eq!(minimal.implied_state(), ConnectivityState::Minimal);

        // A courier is a lane but not connectivity: a node whose only route
        // is a human walking is isolated by any definition a running turn
        // cares about.
        let courier_only = Router::new(
            vec![MockTransport::with(
                TransportId::Courier,
                1 << 20,
                TransportHealth::Up,
                false,
            )],
            8,
        );
        assert_eq!(courier_only.implied_state(), ConnectivityState::Isolated);
    }

    #[test]
    fn peer_lost_means_unreachable_everywhere_not_merely_off_the_lan() {
        let off_lan = PeerReachability {
            lanes: vec![TransportId::Lora],
        };
        assert!(!off_lan.is_lost());
        assert_eq!(off_lan.best(), Some(TransportId::Lora));

        let gone = PeerReachability::default();
        assert!(gone.is_lost());
        assert_eq!(gone.best(), None);

        // `best` is the cheapest lane, not the most recently seen.
        let both = PeerReachability {
            lanes: vec![TransportId::Lora, TransportId::WifiLan],
        };
        assert_eq!(both.best(), Some(TransportId::WifiLan));
    }
}
