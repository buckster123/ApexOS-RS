use crate::{Event, SystemState};
use tokio::sync::{broadcast, mpsc};

/// How many command events one subscriber may hold. Matches the inbox so a
/// slow command consumer backpressures `emit` instead of dropping a prompt.
const COMMAND_CAP: usize = 1024;

/// Events the router or supervisor must not lose (SA-6). Status/telemetry
/// stays on the lossy broadcast; these ride a dedicated mpsc per subscriber.
pub fn is_command(e: &Event) -> bool {
    matches!(
        e,
        Event::UserPrompt { .. }
            | Event::UserCancel { .. }
            | Event::UserApproval { .. }
            | Event::ToolRequested { .. }
            | Event::ToolResult { .. }
            | Event::SpawnAgent { .. }
            | Event::AgentMessage { .. }
            | Event::TurnComplete { .. }
    )
}

pub struct Bus {
    inbox: mpsc::Receiver<Event>,
    outbound: broadcast::Sender<Event>,
    command_sinks: Vec<mpsc::Sender<Event>>,
    state: SystemState,
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
    /// Call [`subscribe_commands`] on the Bus *before* [`run`] for any
    /// consumer that must see prompts / tool calls.
    pub fn new(state: SystemState) -> (Self, BusHandle, broadcast::Sender<Event>) {
        let (tx, inbox) = mpsc::channel(1024);
        let (outbound, _) = broadcast::channel(1024);
        let handle = BusHandle { tx };
        let bus = Self {
            inbox,
            outbound: outbound.clone(),
            command_sinks: Vec::new(),
            state,
        };
        (bus, handle, outbound)
    }

    /// Reliable command lane. Register before `run`. A full channel
    /// backpressures `emit` rather than dropping the event.
    pub fn subscribe_commands(&mut self) -> mpsc::Receiver<Event> {
        let (tx, rx) = mpsc::channel(COMMAND_CAP);
        self.command_sinks.push(tx);
        rx
    }

    pub async fn run(mut self) {
        while let Some(event) = self.inbox.recv().await {
            self.state.apply(&event);
            if is_command(&event) {
                for sink in &self.command_sinks {
                    if sink.send(event.clone()).await.is_err() {
                        // subscriber gone
                    }
                }
            }
            let _ = self.outbound.send(event);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Event, SessionId, SystemState};

    fn prompt(n: u64) -> Event {
        Event::UserPrompt {
            session: SessionId(n),
            text: format!("p{n}"),
            images: vec![],
        }
    }

    #[test]
    fn commands_are_the_non_replayable_ones() {
        assert!(is_command(&prompt(1)));
        assert!(is_command(&Event::UserCancel {
            session: SessionId(1)
        }));
        assert!(!is_command(&Event::SensorReading {
            node_id: "n".into(),
            reading: crate::SensorReading::Temperature {
                celsius: 40.0,
                sensor_id: "t".into(),
            },
            timestamp: 0,
        }));
        assert!(!is_command(&Event::Error {
            session: None,
            message: "x".into(),
        }));
    }

    #[tokio::test]
    async fn command_lane_delivers_every_prompt_while_broadcast_is_unread() {
        let (mut bus, handle, bcast) = Bus::new(SystemState::default());
        let mut cmd = bus.subscribe_commands();
        let _unread = bcast.subscribe();
        tokio::spawn(bus.run());

        let n = 200u64;
        let drain = tokio::spawn(async move {
            let mut got = Vec::new();
            while got.len() < n as usize {
                match cmd.recv().await {
                    Some(Event::UserPrompt { session, text, .. }) => got.push((session.0, text)),
                    other => panic!("bad command: {other:?}"),
                }
            }
            got
        });
        for i in 0..n {
            handle.emit(prompt(i)).await;
        }
        let got = drain.await.expect("drain");
        assert_eq!(got.len(), n as usize);
        for i in 0..n {
            assert_eq!(got[i as usize], (i, format!("p{i}")));
        }
    }
}
