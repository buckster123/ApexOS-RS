//! apexos-world — the 3D world interface for ApexOS-RS. Binary `apexos-world`.
//!
//! Another agentd client (peer to ui-slint and the browser PWA). Speaks agentd's real
//! `Event`/Intent JSON on `ws://HOST:8787/ws` via `world-protocol`. Standard/Pro tier
//! only (real GPU) — DESIGN.md §5.
//!
//! THREAD MODEL (load-bearing, root CLAUDE.md + DESIGN.md §5):
//!   - Slint owns the main thread + the winit event loop. NEVER `#[tokio::main]`.
//!   - tokio runs on background threads (manual multi-thread runtime).
//!   - the `world-protocol` WS task parses events and pushes them onto a channel; it
//!     NEVER renders or blocks (a slow drain silently misses events past the 1024
//!     broadcast cap — DESIGN.md R5).
//!   - a Slint `Timer` drains the channel on the UI thread and applies to the scene +
//!     models. Cross-thread UI mutation only via `invoke_from_event_loop` / the timer.
//!
//! BUILD STATUS: see the crate README. In short — the structure, thread model, station
//! registry, and HUD are wired; the live `world-protocol` WS connect, the Slint↔Bevy
//! shared-wgpu handoff, and per-kind surfaces are STUBBED (// TODO(Mn) markers).

use std::sync::mpsc;
use std::time::Duration;

use anyhow::Result;
use slint::{ComponentHandle, Model, VecModel};

mod stations;
#[cfg(feature = "viz")]
mod world;

use stations::{StationKind, StationRegistry};

// Pulls in the compiled `ui/hud.slint` -> `WorldWindow`, `ChatLine`.
slint::include_modules!();

// ── Bridge channel types (renderer-internal; NOT on the wire) ────────────────────
//
// The WS task translates real agentd `Event`s -> `WorldEvent` (after session filter);
// the UI raises `WorldCommand`s. Mirrors doc 06 §3's bridge.rs split. Kept here in the
// scaffold; promote to a `bridge.rs` module when it grows (// TODO(Mn)).

/// Inbound: WS -> app (already session-filtered/labelled by the WS task).
#[derive(Debug, Clone)]
pub enum WorldEvent {
    /// Connection status for the HUD ribbon ("connected"/"reconnecting"/"offline").
    Connection(String),
    /// The gateway pushed `session_init` on connect; we learned our session id.
    SessionInit { session: u64 },
    /// `agent_text { delta }` for a bound session -> append to that station's surface.
    AgentTextDelta { session: u64, delta: String },
    /// `turn_complete` -> clear the busy state for the session.
    TurnComplete { session: u64 },
    // // TODO(Mn): M1 — ToolRequested / ApprovalPending / ToolResult / SensorReading.
}

/// Outbound: app -> WS / app -> scene.
#[derive(Debug, Clone)]
pub enum WorldCommand {
    /// User typed at the active station -> `{"type":"user_prompt","text":...}`
    /// (session omitted; the gateway injects it — DESIGN.md §4).
    Prompt { station_id: i32, text: String },
    /// Approve/reject a tool call -> `{"type":"user_approval","action":<id>,"granted":b}`.
    Approval { station_id: i32, action: u64, granted: bool },
    // // TODO(Mn): M0 — Cancel; M1 — Activate/Deactivate to the scene.
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    // ── Tier gate (DESIGN.md §5, doc 03 §9) — refuse cleanly on Nano/Micro/no-GPU. ──
    // STUB: real probe requires a GPU adapter + a GPU Slint backend. // TODO(Mn): M0
    // implement the adapter/backend probe; honor WORLD_TIER + a --force dev bypass.
    tracing::info!("tier gate: STUB (assumes Standard/Pro). // TODO(Mn): real GPU probe");

    // ── Slint backend: request a shared wgpu-28 device BEFORE the window (doc 03 §2.1).
    // Slint resolves to 1.16.1 here, which exposes `unstable-wgpu-28` ->
    // `require_wgpu_28` / `GraphicsAPI::WGPU28` (DESIGN.md D1 confirmed against `cargo`;
    // the brief's "wgpu-29" is stale for this toolchain).
    // STUB: left commented so the scaffold builds against just `backend-winit` without
    // a GPU present in CI. // TODO(Mn): Spike 1 — enable and capture device/queue.
    //
    // slint::BackendSelector::new()
    //     .require_wgpu_28(slint::wgpu_28::WGPUConfiguration::default())
    //     .select()?;

    // tokio multi-thread runtime on background threads — NEVER `#[tokio::main]`.
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;

    // Slint window on the main thread.
    let ui = WorldWindow::new()?;

    // Station registry — the closed catalog placed in the hub (doc 05 §6).
    let registry = StationRegistry::launch_layout();
    tracing::info!(stations = registry.stations.len(), "launch layout placed");

    // Per-station chat buffers, driving the active panel's `ChatLine` list.
    let active_lines: std::rc::Rc<VecModel<ChatLine>> = std::rc::Rc::new(VecModel::default());
    ui.set_active_lines(active_lines.clone().into());

    // ── Bridge channels (DESIGN.md §5: WS ingest decoupled from rendering) ──────────
    let (ev_tx, ev_rx) = mpsc::channel::<WorldEvent>(); // WS task -> UI timer
    let (cmd_tx, cmd_rx) = mpsc::channel::<WorldCommand>(); // UI -> WS task

    // ── WS client task (background). // TODO(Mn): M0 replace the stub with a real
    // `world_protocol::WorldClient::connect(url, token)`:
    //   - AGENTD_WS env (default ws://localhost:8787/ws), AGENTD_TOKEN for non-loopback;
    //   - on first `session_init` capture the session id (server NEVER sends `hello` —
    //     DESIGN.md D7);
    //   - forward parsed `Event`s as `WorldEvent` after the defensive `session` filter;
    //   - drain `cmd_rx` and send `world_protocol::intents::{user_prompt,user_approval,
    //     user_cancel}` (session OMITTED — gateway injects);
    //   - reconnect with backoff; emit `WorldEvent::Connection` transitions.
    rt.spawn(ws_task_stub(ev_tx, cmd_rx));

    // ── Bevy scene (feature `viz`, off by default — DESIGN.md D2). ──────────────────
    #[cfg(feature = "viz")]
    let mut bevy_app = {
        let (activate_tx, _activate_rx) = mpsc::channel::<i32>();
        // // TODO(Mn): drain `_activate_rx` in the frame timer and raise `ui.invoke_activate`.
        world::build_world_app(activate_tx)
    };

    // ── UI callbacks -> WorldCommands ───────────────────────────────────────────────
    {
        let cmd_tx = cmd_tx.clone();
        let weak = ui.as_weak();
        ui.on_send_prompt(move |text| {
            if let Some(ui) = weak.upgrade() {
                let station_id = ui.get_active_station_id();
                if station_id >= 0 {
                    let _ = cmd_tx.send(WorldCommand::Prompt {
                        station_id,
                        text: text.to_string(),
                    });
                    // Optimistic local echo into the active panel.
                    if let Some(lines) = ui.get_active_lines().as_any()
                        .downcast_ref::<VecModel<ChatLine>>()
                    {
                        lines.push(ChatLine { role: "user".into(), text });
                    }
                }
            }
        });
    }
    {
        let cmd_tx = cmd_tx.clone();
        let weak = ui.as_weak();
        ui.on_approve(move |action, granted| {
            if let Some(ui) = weak.upgrade() {
                let station_id = ui.get_active_station_id();
                let _ = cmd_tx.send(WorldCommand::Approval {
                    station_id,
                    action: action as u64,
                    granted,
                });
            }
        });
    }
    {
        // Activation: picking (world.rs) or a key raises this with a station id.
        // The scaffold wires the title/kind onto the panel from the registry.
        let weak = ui.as_weak();
        let layout: Vec<(i32, StationKind)> =
            registry.stations.iter().map(|s| (s.id, s.kind)).collect();
        ui.on_activate(move |station_id| {
            if let Some(ui) = weak.upgrade() {
                if let Some((_, kind)) = layout.iter().find(|(id, _)| *id == station_id) {
                    let d = stations::desc(*kind);
                    ui.set_active_station_title(d.title.into());
                    ui.set_active_station_kind(kind.as_str().into());
                    ui.set_active_station_id(station_id);
                    tracing::info!(station_id, kind = kind.as_str(), "ACTIVE (Mode II)");
                    // // TODO(Mn): M0 — acquire the Session binding (lazy WS connect) and
                    // route filtered events to this station's surface.
                }
            }
        });
    }
    {
        let weak = ui.as_weak();
        ui.on_dismiss(move || {
            if let Some(ui) = weak.upgrade() {
                ui.set_active_station_id(-1); // back to ROAMING
                tracing::info!("ROAMING (station dismissed)");
            }
        });
    }

    // ── Frame timer (~16 ms): step the scene, drain inbound events, refresh models. ──
    let timer = slint::Timer::default();
    {
        let weak = ui.as_weak();
        let active_lines = active_lines.clone();
        timer.start(slint::TimerMode::Repeated, Duration::from_millis(16), move || {
            // Step Bevy one frame (never `App::run`). // TODO(Mn): Spike 1 — after the
            // step, hand the freshest `world_texture` to `ui.set_world_texture(..)`.
            #[cfg(feature = "viz")]
            world::step(&mut bevy_app);

            let Some(ui) = weak.upgrade() else { return };

            // Drain WS -> UI events.
            while let Ok(ev) = ev_rx.try_recv() {
                match ev {
                    WorldEvent::Connection(status) => ui.set_conn_status(status.into()),
                    WorldEvent::SessionInit { session } => {
                        tracing::info!(session, "session_init captured");
                        // // TODO(Mn): bind the root Chat station (id 0) to this session.
                    }
                    WorldEvent::AgentTextDelta { session: _, delta } => {
                        // // TODO(Mn): route by session to the owning station's buffer.
                        // Scaffold: append to the active panel if one is open.
                        if ui.get_active_station_id() >= 0 {
                            append_or_extend_agent_line(&active_lines, &delta);
                        }
                    }
                    WorldEvent::TurnComplete { session } => {
                        tracing::debug!(session, "turn_complete");
                        // // TODO(Mn): clear busy state for the owning station.
                    }
                }
            }
        });
    }

    // Slint owns the loop.
    ui.run()?;
    Ok(())
}

/// Append a streamed agent delta — extend the last agent line or start a new one.
/// // TODO(Mn): this is the single-active-panel scaffold; M1 keys buffers by session.
fn append_or_extend_agent_line(lines: &VecModel<ChatLine>, delta: &str) {
    let n = lines.row_count();
    if n > 0 {
        if let Some(mut last) = lines.row_data(n - 1) {
            if last.role == "agent" {
                last.text = format!("{}{delta}", last.text).into();
                lines.set_row_data(n - 1, last);
                return;
            }
        }
    }
    lines.push(ChatLine { role: "agent".into(), text: delta.into() });
}

/// WS client STUB. Logs offline and idles, draining outbound commands so the UI side
/// never blocks. // TODO(Mn): M0 — replace with a real `world_protocol::WorldClient`.
async fn ws_task_stub(
    ev_tx: mpsc::Sender<WorldEvent>,
    cmd_rx: mpsc::Receiver<WorldCommand>,
) {
    let url = std::env::var("AGENTD_WS").unwrap_or_else(|_| "ws://localhost:8787/ws".into());
    tracing::warn!(%url, "WS client is a STUB — no live agentd connection yet (M0)");
    let _ = ev_tx.send(WorldEvent::Connection("offline".into()));

    // Drain outbound commands so the UI->WS channel never fills. In M0 these become
    // `world_protocol::intents::*` frames on the live socket.
    loop {
        match cmd_rx.try_recv() {
            Ok(cmd) => tracing::info!(?cmd, "outbound command (dropped by stub)"),
            Err(mpsc::TryRecvError::Empty) => {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            Err(mpsc::TryRecvError::Disconnected) => break,
        }
    }
}
