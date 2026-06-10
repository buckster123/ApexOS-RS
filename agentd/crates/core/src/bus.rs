use tokio::sync::{broadcast, mpsc};
use crate::{Event, SystemState};

pub struct Bus {
    inbox:    mpsc::Receiver<Event>,
    outbound: broadcast::Sender<Event>,
    state:    SystemState,
}

/// Cheap-to-clone handle other tasks use to send events to the bus.
#[derive(Clone)]
pub struct BusHandle {
    tx: mpsc::Sender<Event>,
}

impl BusHandle {
    pub async fn emit(&self, e: Event) {
        let _ = self.tx.send(e).await;
    }
}

impl Bus {
    /// Returns (Bus, handle-to-emit, broadcast-sender-to-subscribe).
    pub fn new(state: SystemState) -> (Self, BusHandle, broadcast::Sender<Event>) {
        let (tx, inbox)       = mpsc::channel(1024);
        let (outbound, _)     = broadcast::channel(1024);
        let handle            = BusHandle { tx };
        let bus               = Self { inbox, outbound: outbound.clone(), state };
        (bus, handle, outbound)
    }

    pub async fn run(mut self) {
        while let Some(event) = self.inbox.recv().await {
            self.state.apply(&event);
            let _ = self.outbound.send(event);
        }
    }
}
