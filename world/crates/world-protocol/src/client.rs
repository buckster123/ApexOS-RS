// The typed agentd WebSocket client.
//
// One `WorldClient` owns one socket = one agentd session (DESIGN.md D4: one socket per
// active session; the inbound `session` filter is defensive, not load-bearing). It:
//   - connects to `ws://HOST:8787/ws` (optional bearer token for non-loopback binds),
//   - sends nothing on connect (the gateway pushes `session_init` itself — DESIGN.md D7),
//   - reads frames, deserializes each to [`Event`] (log-and-drop on failure, mirroring
//     agentd), and forwards them on an mpsc to consumers,
//   - captures the `session_id` from the first `session_init` and exposes it,
//   - accepts outbound [`Intent`]s on a second mpsc and writes them to the socket,
//   - reconnects with capped exponential backoff on drop.
//
// Architecture rule (DESIGN.md §5, R5): the read loop is parse-and-push-to-channel ONLY
// — never render or block in it, or a slow consumer silently misses events past the
// gateway's broadcast cap. Consumers drain `EventRx` from their own thread/event loop.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::header::AUTHORIZATION;
use tokio_tungstenite::tungstenite::Message;

use crate::events::Event;
use crate::intents::Intent;

/// Receiver of inbound [`Event`]s, drained by the caller's UI/event loop.
pub type EventRx = mpsc::UnboundedReceiver<Event>;
/// Sender of outbound [`Intent`]s into the client's write task.
pub type IntentTx = mpsc::UnboundedSender<Intent>;

/// Sentinel meaning "session id not yet known" (no `session_init` received).
const NO_SESSION: u64 = u64::MAX;

/// A live (or reconnecting) connection to one agentd session.
///
/// Construct with [`WorldClient::connect`]. The returned handle stays valid across
/// reconnects; the background task loops until the [`IntentTx`] is dropped or the
/// [`EventRx`] is closed.
pub struct WorldClient {
    /// The session id captured from the first `session_init`, or `NO_SESSION`.
    /// Shared with the read task; updated on (re)connect / resume.
    session_id: Arc<AtomicU64>,
}

impl WorldClient {
    /// Connect to `url` (e.g. `ws://localhost:8787/ws`). If `token` is set it is sent
    /// as an `Authorization: Bearer …` header (required by agentd for non-loopback
    /// binds — root CLAUDE.md F036).
    ///
    /// Returns the client handle, an [`EventRx`] to drain inbound events, and an
    /// [`IntentTx`] to send outbound intents. The connection runs on a spawned tokio
    /// task with reconnect/backoff; it requires a tokio runtime to be active.
    pub fn connect(url: impl Into<String>, token: Option<String>) -> (Self, EventRx, IntentTx) {
        let url = url.into();
        let (event_tx, event_rx) = mpsc::unbounded_channel::<Event>();
        let (intent_tx, intent_rx) = mpsc::unbounded_channel::<Intent>();
        let session_id = Arc::new(AtomicU64::new(NO_SESSION));

        let task_session = session_id.clone();
        tokio::spawn(async move {
            run_with_reconnect(url, token, event_tx, intent_rx, task_session).await;
        });

        (Self { session_id }, event_rx, intent_tx)
    }

    /// The session id this socket is bound to, once `session_init` has been seen.
    /// `None` until the handshake completes (and briefly during a reconnect).
    pub fn session_id(&self) -> Option<crate::ids::SessionId> {
        match self.session_id.load(Ordering::Acquire) {
            NO_SESSION => None,
            id => Some(crate::ids::SessionId(id)),
        }
    }
}

/// Reconnect supervisor: runs one connection attempt, and on disconnect backs off and
/// retries until the channels are closed. Outbound intents are funneled through a single
/// owned receiver that survives reconnects.
async fn run_with_reconnect(
    url: String,
    token: Option<String>,
    event_tx: mpsc::UnboundedSender<Event>,
    mut intent_rx: mpsc::UnboundedReceiver<Intent>,
    session_id: Arc<AtomicU64>,
) {
    let mut backoff = Duration::from_millis(250);
    let max_backoff = Duration::from_secs(30); // honor the 30s+ LLM/tier patience rule

    loop {
        // If the consumer dropped EventRx, stop entirely.
        if event_tx.is_closed() {
            tracing::debug!("world-protocol: event consumer gone, stopping client");
            return;
        }

        match run_connection(&url, token.as_deref(), &event_tx, &mut intent_rx, &session_id).await {
            ConnOutcome::ConsumerGone => {
                tracing::debug!("world-protocol: consumer gone, stopping client");
                return;
            }
            ConnOutcome::Disconnected(reason) => {
                // Mark the session unknown while we are detached.
                session_id.store(NO_SESSION, Ordering::Release);
                tracing::warn!(%url, reason, backoff_ms = backoff.as_millis() as u64,
                    "world-protocol: disconnected, reconnecting after backoff");
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(max_backoff);
            }
        }
    }
}

enum ConnOutcome {
    /// The consumer dropped the channels; the client should stop.
    ConsumerGone,
    /// The socket closed/errored; the supervisor should back off and retry.
    Disconnected(&'static str),
}

/// One connection lifecycle: connect → read frames + write intents until the socket
/// closes or the channels are dropped. Resets backoff implicitly (the supervisor only
/// grows backoff on `Disconnected`; a successful connect that later drops still backs
/// off, which is acceptable for a prototype).
async fn run_connection(
    url: &str,
    token: Option<&str>,
    event_tx: &mpsc::UnboundedSender<Event>,
    intent_rx: &mut mpsc::UnboundedReceiver<Intent>,
    session_id: &Arc<AtomicU64>,
) -> ConnOutcome {
    let request = match build_request(url, token) {
        Ok(req) => req,
        Err(e) => {
            tracing::error!(%url, error = %e, "world-protocol: bad WS url/token");
            return ConnOutcome::Disconnected("bad request");
        }
    };

    let stream = match tokio_tungstenite::connect_async(request).await {
        Ok((stream, _resp)) => stream,
        Err(e) => {
            tracing::warn!(%url, error = %e, "world-protocol: connect failed");
            return ConnOutcome::Disconnected("connect failed");
        }
    };
    tracing::info!(%url, "world-protocol: connected");

    let (mut write, mut read) = stream.split();

    loop {
        tokio::select! {
            // Outbound: app → socket.
            maybe_intent = intent_rx.recv() => {
                match maybe_intent {
                    Some(intent) => {
                        let json = intent.to_json();
                        if let Err(e) = write.send(Message::Text(json.into())).await {
                            tracing::warn!(error = %e, "world-protocol: send failed");
                            return ConnOutcome::Disconnected("send failed");
                        }
                    }
                    // The IntentTx was dropped: the app is shutting this client down.
                    None => {
                        let _ = write.close().await;
                        return ConnOutcome::ConsumerGone;
                    }
                }
            }

            // Inbound: socket → app. Parse-and-push only; never block here.
            maybe_msg = read.next() => {
                match maybe_msg {
                    Some(Ok(Message::Text(text))) => {
                        match serde_json::from_str::<Event>(&text) {
                            Ok(event) => {
                                // Capture the session id from the handshake/resume.
                                if let Event::SessionInit { session_id: sid, .. } = &event {
                                    session_id.store(sid.0, Ordering::Release);
                                    tracing::info!(session = sid.0, "world-protocol: session_init");
                                }
                                if event_tx.send(event).is_err() {
                                    return ConnOutcome::ConsumerGone;
                                }
                            }
                            // Mirror agentd: a frame that fails to deserialize is dropped.
                            // (`Event::Unknown` already absorbs unknown `type`s, so this
                            // path is genuinely malformed JSON / missing fields.)
                            Err(e) => {
                                tracing::debug!(error = %e, raw = %truncate(&text),
                                    "world-protocol: dropping undeserializable frame");
                            }
                        }
                    }
                    Some(Ok(Message::Binary(_))) => {
                        // agentd's /ws is JSON text only; ignore stray binary frames.
                    }
                    Some(Ok(Message::Ping(_) | Message::Pong(_) | Message::Frame(_))) => {}
                    Some(Ok(Message::Close(_))) => {
                        tracing::info!("world-protocol: server closed connection");
                        return ConnOutcome::Disconnected("server close");
                    }
                    Some(Err(e)) => {
                        tracing::warn!(error = %e, "world-protocol: read error");
                        return ConnOutcome::Disconnected("read error");
                    }
                    None => {
                        return ConnOutcome::Disconnected("stream ended");
                    }
                }
            }
        }
    }
}

/// Build the WS upgrade request, attaching `Authorization: Bearer <token>` if given.
fn build_request(
    url: &str,
    token: Option<&str>,
) -> Result<tokio_tungstenite::tungstenite::handshake::client::Request, tokio_tungstenite::tungstenite::Error>
{
    let mut request = url.into_client_request()?;
    if let Some(tok) = token {
        let value = format!("Bearer {tok}")
            .parse()
            .map_err(|_| tokio_tungstenite::tungstenite::Error::Url(
                tokio_tungstenite::tungstenite::error::UrlError::NoHostName,
            ))?;
        request.headers_mut().insert(AUTHORIZATION, value);
    }
    Ok(request)
}

/// Truncate a frame for log output so a giant tool result doesn't flood the log.
fn truncate(s: &str) -> String {
    const MAX: usize = 200;
    if s.len() <= MAX {
        s.to_string()
    } else {
        // Back off to the nearest char boundary at or below MAX (stable equivalent
        // of the unstable `str::floor_char_boundary`).
        let mut end = MAX;
        while end > 0 && !s.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}…", &s[..end])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_keeps_short_strings() {
        assert_eq!(truncate("hello"), "hello");
    }

    #[test]
    fn truncate_clamps_long_strings_on_char_boundary() {
        let long = "x".repeat(500);
        let out = truncate(&long);
        assert!(out.len() <= 201 + 4); // 200 bytes + ellipsis (3 bytes)
        assert!(out.ends_with('…'));
    }

    #[test]
    fn build_request_without_token_has_no_auth_header() {
        let req = build_request("ws://localhost:8787/ws", None).unwrap();
        assert!(req.headers().get(AUTHORIZATION).is_none());
    }

    #[test]
    fn build_request_with_token_sets_bearer_header() {
        let req = build_request("ws://localhost:8787/ws", Some("secret")).unwrap();
        assert_eq!(req.headers().get(AUTHORIZATION).unwrap(), "Bearer secret");
    }
}
