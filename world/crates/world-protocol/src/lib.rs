//! `world-protocol` — the agentd wire boundary for apexos-world.
//!
//! apexos-world is **just another agentd client**, peer to `ui-slint` and the browser
//! PWA. This crate is the only piece that touches the wire: it mirrors agentd's
//! `Event`/Intent JSON shapes and provides a typed [`WorldClient`] over a WebSocket.
//!
//! ## MIRRORS agentd/crates/core/src/types.rs — keep in sync.
//!
//! Per DESIGN.md D5, this crate **mirrors** agentd's types rather than taking a path
//! dependency on `apexos-core` (which is unpublished, and `world/` is meant to be
//! cleanly extractable to its own repo). The coupling is documented by the `MIRRORS`
//! banner on every wire module and guarded by round-trip tests + an [`Event::Unknown`]
//! fallback so a new agentd event never crashes the client. If `apexos-core` is ever
//! published as a library, revisit this decision.
//!
//! ## Load-bearing wire facts (DESIGN.md §4)
//!
//! - `Event` is `#[serde(tag = "type", rename_all = "snake_case")]`; `SessionId` /
//!   `ActionId` serialize as **bare numbers**.
//! - On `tool_requested`/`approval_pending` tool data nests under `call` (`call.id` a
//!   bare number); on `tool_result`, `call` is the **bare `ActionId`**.
//! - The gateway pushes `session_init` on connect; the client sends **nothing** on
//!   connect and reads its session id from that frame. The server never replies `hello`.
//! - Outbound intents **omit `session`** (the gateway injects it). A frame with wrong
//!   field names is **silently dropped** — hence [`intents`]' exact-field-name tests.
//!
//! ## Dependency discipline
//!
//! LIGHT deps only — tokio, tokio-tungstenite, serde/serde_json, futures-util, tracing.
//! No GPU/UI/Bevy/Slint. This crate `cargo check`s and `cargo test`s on a headless box;
//! it is the CI-green gate and the future "agentd client SDK".
//!
//! ## Example
//!
//! ```no_run
//! use world_protocol::{WorldClient, Event, intents};
//!
//! # async fn run() {
//! let (client, mut events, intents_tx) =
//!     WorldClient::connect("ws://localhost:8787/ws", None);
//!
//! // Drain inbound events (do this off any render/event loop).
//! while let Some(event) = events.recv().await {
//!     match event {
//!         Event::SessionInit { session_id, .. } => {
//!             println!("bound to session {session_id}");
//!         }
//!         Event::AgentText { delta, .. } => print!("{delta}"),
//!         _ => {}
//!     }
//! }
//!
//! // Speak to the bound session (no `session` field — the gateway injects it).
//! let _ = intents_tx.send(intents::user_prompt("hello"));
//! # let _ = client.session_id();
//! # }
//! ```

pub mod client;
pub mod events;
pub mod ids;
pub mod intents;

pub use client::{EventRx, IntentTx, WorldClient};
pub use events::{
    CouncilAgentDef, Event, SensorReading, ToolCall, ToolOutput, ToolSpec,
};
pub use ids::{ActionId, PluginId, SessionId};
pub use intents::{hello, user_approval, user_cancel, user_prompt, Intent};
