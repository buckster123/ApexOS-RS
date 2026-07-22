// ApexOS-RS: Slint native UI
//
// Thread model:
//   main thread — Slint event loop (never use #[tokio::main])
//   tokio pool  — WebSocket I/O + HTTP polling
//
// Cross-thread bridge:
//   slint::invoke_from_event_loop() queues closures to the Slint thread.
//   VecModel mutations happen on the Slint thread via MESSAGES thread-local.
//   Outbound WS messages go through an unbounded mpsc channel.

slint::include_modules!();

mod face_gl; // Phase-2 face — raw GL via the rendering notifier (default on GL tiers)

use slint::Model; // row_count / row_data / set_row_data on VecModel
use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
// Selective import (NOT a glob): apexos_protocol::Message would collide with
// tokio_tungstenite's Message used below.
use apexos_protocol::{Event, GoalState, SensorReading};
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::mpsc;
use tokio_tungstenite::{connect_async, tungstenite::Message};

// ── Thread-local model access ─────────────────────────────────────────────────
thread_local! {
    static MESSAGES: RefCell<Option<Rc<slint::VecModel<MessageItem>>>> =
        const { RefCell::new(None) };
    static SESSIONS: RefCell<Option<Rc<slint::VecModel<SessionItem>>>> =
        const { RefCell::new(None) };
    static MODELS: RefCell<Option<Rc<slint::VecModel<ModelItem>>>> =
        const { RefCell::new(None) };
    static TOASTS: RefCell<Option<Rc<slint::VecModel<ToastItem>>>> =
        const { RefCell::new(None) };
    // Notification center (G3c): persisted history, newest first.
    static NOTIF_LOG: RefCell<Option<Rc<slint::VecModel<ToastItem>>>> =
        const { RefCell::new(None) };
    // Weak handle for updating the unread badge from toast() on the Slint thread.
    static UI_WEAK: RefCell<Option<slint::Weak<AppWindow>>> =
        const { RefCell::new(None) };
    // Window manager (G2): Rust owns the window set; model order = z-order.
    static WINDOWS: RefCell<Option<Rc<slint::VecModel<WindowDesc>>>> =
        const { RefCell::new(None) };
    static WIN_NEXT_ID: std::cell::Cell<i32> = const { std::cell::Cell::new(1) };
    // Terminal app (G3d): stdin sender (UI→task) + the matching receiver, parked
    // until the Terminal window is first launched, when the WS task is spawned.
    static TERM_TX: RefCell<Option<mpsc::UnboundedSender<String>>> =
        const { RefCell::new(None) };
    static TERM_RX: RefCell<Option<mpsc::UnboundedReceiver<String>>> =
        const { RefCell::new(None) };
    static TERM_STARTED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    // Council app (G3d): the deliberating-agent model, driven by Council* events.
    static COUNCIL: RefCell<Option<Rc<slint::VecModel<CouncilAgent>>>> =
        const { RefCell::new(None) };
    // Work Board (🗂): four live column models, mutated in place from WS events.
    static BOARD: RefCell<Option<BoardModels>> = const { RefCell::new(None) };
    // Tier-A parity apps: each replaced wholesale on REFRESH.
    static EVENTS: RefCell<Option<Rc<slint::VecModel<EventLogItem>>>> =
        const { RefCell::new(None) };
    static MESH: RefCell<Option<Rc<slint::VecModel<MeshNode>>>> =
        const { RefCell::new(None) };
    // Mesh INBOX: per-peer a2a threads, mutated in place from the mesh_message
    // event stream (distinct from MESH, which the HTTP roster replaces wholesale).
    static INBOX: RefCell<Option<Rc<slint::VecModel<InboxThread>>>> =
        const { RefCell::new(None) };
    static INFER_MODELS: RefCell<Option<Rc<slint::VecModel<ModelItem>>>> =
        const { RefCell::new(None) };
    static AUDIO_FILES: RefCell<Option<Rc<slint::VecModel<AudioFileItem>>>> =
        const { RefCell::new(None) };
    static WAVEFORM: RefCell<Option<Rc<slint::VecModel<f32>>>> =
        const { RefCell::new(None) };
    static SONUS_FILES: RefCell<Option<Rc<slint::VecModel<SonusFileItem>>>> =
        const { RefCell::new(None) };
    static NOTES_FILES: RefCell<Option<Rc<slint::VecModel<NoteItem>>>> =
        const { RefCell::new(None) };
    // Chat-composer image attach: workspace images offered in the 🖼 picker.
    static WORKSPACE_IMAGES: RefCell<Option<Rc<slint::VecModel<ImageItem>>>> =
        const { RefCell::new(None) };
    // Explorer (📁 Files): the current directory's entries.
    static EXPLORER_ENTRIES: RefCell<Option<Rc<slint::VecModel<ExplorerEntry>>>> =
        const { RefCell::new(None) };
    // "Use this drive" picker: the adoptable USB sticks from /api/media/candidates.
    static DRIVE_CANDIDATES: RefCell<Option<Rc<slint::VecModel<UsbCandidate>>>> =
        const { RefCell::new(None) };
    // Sketchpad: the rendered stroke model (Slint Paths) + the raw point data we
    // post to /api/sketch. Index into SKETCH_PALETTE drives colour; width index 0/1.
    static SKETCH_STROKES: RefCell<Option<Rc<slint::VecModel<SketchStroke>>>> =
        const { RefCell::new(None) };
    static SKETCH_DATA: RefCell<Vec<StrokeData>> = const { RefCell::new(Vec::new()) };
    static SKETCH_COLOR: std::cell::Cell<i32> = const { std::cell::Cell::new(0) };
    static SKETCH_WIDTH: std::cell::Cell<i32> = const { std::cell::Cell::new(0) };
    // Shape tool: 0 freehand · 1 line · 2 rect · 3 ellipse; + the drag anchor.
    static SKETCH_TOOL: std::cell::Cell<i32> = const { std::cell::Cell::new(0) };
    static SKETCH_ANCHOR: std::cell::Cell<(f32, f32)> = const { std::cell::Cell::new((0.0, 0.0)) };
    // Last-reported canvas pixel size (from SketchpadView's changed handler).
    // Lets agent-driven `sketch_draw` scale its normalized 0-1 coords to px.
    // Default ≈ the sketchpad window's canvas before the first report lands.
    static SKETCH_CANVAS: std::cell::Cell<(f32, f32)> = const { std::cell::Cell::new((600.0, 433.0)) };
    // Slice 3e: the logged-in human's user_id ("" for the admin/device token), set on
    // a settings refresh from /api/auth/me — so the LOGIN toggle knows whom to make
    // (or clear as) this device's auto-login default.
    static LOGIN_ME: RefCell<String> = const { RefCell::new(String::new()) };
    // Calculator — pure-UI immediate-execution state machine.
    static CALC: RefCell<Calc> = RefCell::new(Calc::new());
    // Identity boot wizard (3d): wizard state + its two tile models. Thread-local
    // so the async identities fetch carries only Send data and populates via
    // invoke_from_event_loop (Rc models can't cross the tokio thread boundary).
    static ID_STATE: RefCell<IdState> = RefCell::new(IdState::new());
    static ID_USERS: RefCell<Option<Rc<slint::VecModel<UserDef>>>> = const { RefCell::new(None) };
    static ID_AGENTS: RefCell<Option<Rc<slint::VecModel<AgentDef>>>> = const { RefCell::new(None) };
    // Occipital (📖) follow-along reader (Phase 9): the breadcrumb trail of the
    // agent's reads this session (newest last, capped). Auto-reveal suppression
    // lives in the generalized UI_LATCHED map below (A3) — the reader
    // force-latches on any user close; a menu launch re-invites.
    static OCCIPITAL_TRAIL: RefCell<Option<Rc<slint::VecModel<ReaderLink>>>> = const { RefCell::new(None) };
    // Adaptive UI (Loop 6, docs/adaptive-ui.md): per-AppKind bitmasks, bit index =
    // the AppKind ordinal (APP_TABLE order). AGENT_OPENED marks windows the agent
    // created via `ui_open`; a USER close of one moves the bit to UI_LATCHED —
    // `ui_open` for that app is then suppressed for the session (the human always
    // wins). A menu launch by the user clears both bits (re-invitation).
    static AGENT_OPENED: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
    static UI_LATCHED: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
    // A3 rate rail: ui_* mutations applied in the current turn. Reset on
    // TurnComplete / cancel / session switch; enforcement in the ToolRequested
    // arm; the live counter rides /state so the agent can SEE it throttled.
    static UI_TURN_MUTATIONS: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

// ── Identity boot wizard (3d) state + helpers ───────────────────────────────────
#[derive(Clone, Default)]
struct UserRow { id: String, name: String, has_pin: bool }
#[derive(Clone, Default)]
struct AgentRow { id: String, name: String, owner: String }
#[derive(Default)]
struct IdState { users: Vec<UserRow>, agents: Vec<AgentRow>, selected: String, pin: String,
    /// True when the wizard is acting as the LOGIN screen (no AGENTD_TOKEN in env →
    /// the desktop/PWA path): profiles come from /api/auth/profiles and a pick/OK
    /// mints a session token via /api/auth/login + re-execs. See agent-identity.md 3e.
    login: bool }
impl IdState { fn new() -> Self { Self::default() } }

/// Re-exec this binary with `AGENTD_TOKEN` set to the freshly-minted session token,
/// so the normal (token-present) connection path runs unchanged — no boot refactor.
/// Returns ONLY on failure (`exec` replaces the process image on success). Unix-only
/// (every ApexOS-RS tier is Linux/Unix).
fn reexec_with_token(token: &str) -> std::io::Error {
    use std::os::unix::process::CommandExt;
    let exe = std::env::current_exe()
        .unwrap_or_else(|_| std::path::PathBuf::from("apexos-rs-ui"));
    std::process::Command::new(exe)
        .args(std::env::args().skip(1))
        .env("AGENTD_TOKEN", token)
        .exec()
}

/// Slice-3e login: POST profile+PIN to the ungated `/api/auth/login`. On success,
/// re-exec with the minted token (→ the normal connection path). On failure, surface
/// it on the keypad + a toast. Runs in a tokio task (the re-exec replaces the whole
/// process, so it doesn't matter which thread calls it).
async fn do_login(
    client:  &reqwest::Client,
    base:    &str,
    user_id: String,
    pin:     String,
    ui_w:    slint::Weak<AppWindow>,
) {
    let body = serde_json::json!({ "user_id": user_id, "pin": pin });
    let resp = client.post(format!("{base}/api/auth/login"))
        .json(&body)
        .timeout(std::time::Duration::from_secs(10))
        .send().await;
    match resp {
        Ok(r) => {
            let v = r.json::<Value>().await.unwrap_or(Value::Null);
            if v["ok"].as_bool().unwrap_or(false) {
                if let Some(tok) = v["token"].as_str() {
                    let e = reexec_with_token(tok);   // returns only if exec failed
                    notify(ToastKind::Error, format!("Re-launch after login failed: {e}"));
                    return;
                }
            }
            let locked = v["locked"].as_bool().unwrap_or(false);
            let retry  = v["retry_after_secs"].as_u64();
            let msg = if locked {
                match retry {
                    Some(s) => format!("Too many tries — locked {s}s"),
                    None    => "Too many tries — locked".to_string(),
                }
            } else {
                "Wrong PIN — try again".to_string()
            };
            let m = msg.clone();
            slint::invoke_from_event_loop(move || {
                if let Some(ui) = ui_w.upgrade() {
                    ID_STATE.with(|s| s.borrow_mut().pin.clear());
                    ui.set_identity_pin_filled(0);
                    ui.set_identity_pin_error(true);
                    ui.set_identity_pin_message(m.into());
                }
            }).ok();
            notify(ToastKind::Error, msg);
        }
        Err(_) => {
            slint::invoke_from_event_loop(move || {
                if let Some(ui) = ui_w.upgrade() {
                    ui.set_identity_pin_error(true);
                    ui.set_identity_pin_message("Can't reach agentd — try again".into());
                }
            }).ok();
            notify(ToastKind::Error, "Login failed — can't reach agentd");
        }
    }
}

/// Tile glyph: the name's first character, uppercased (fallback "?").
fn id_glyph(name: &str) -> slint::SharedString {
    name.chars().next()
        .map(|c| c.to_uppercase().to_string())
        .unwrap_or_else(|| "?".to_string())
        .into()
}

/// Populate the agent tile model from ID_STATE, filtered to `owner`.
fn id_load_agents(owner: &str) {
    let rows: Vec<AgentDef> = ID_STATE.with(|s| s.borrow().agents.iter()
        .filter(|a| a.owner == owner)
        .map(|a| AgentDef { id: a.id.clone().into(), name: a.name.clone().into(), glyph: id_glyph(&a.name) })
        .collect());
    ID_AGENTS.with(|m| { if let Some(model) = m.borrow().as_ref() { model.set_vec(rows); } });
}

// ── Calculator (🧮) — a basic immediate-execution calculator, no agentd ─────────
#[derive(Default)]
struct Calc {
    entry: String,         // the number currently being typed / shown
    acc: f64,              // accumulator (left operand)
    pending: Option<char>, // pending operator
    fresh: bool,           // next digit starts a new entry (after =, op, or boot)
}

impl Calc {
    fn new() -> Self {
        Calc { entry: "0".into(), acc: 0.0, pending: None, fresh: true }
    }

    fn cur(&self) -> f64 { self.entry.parse().unwrap_or(0.0) }

    /// Format a value for the display: trim trailing zeros, guard non-finite.
    fn fmt(v: f64) -> String {
        if !v.is_finite() { return "Error".into(); }
        let s = format!("{v:.10}");
        let s = s.trim_end_matches('0').trim_end_matches('.');
        if s.is_empty() || s == "-0" { "0".into() } else { s.to_string() }
    }

    fn apply_pending(&mut self) {
        let rhs = self.cur();
        self.acc = match self.pending.take() {
            Some('+') => self.acc + rhs,
            Some('-') => self.acc - rhs,
            Some('*') => self.acc * rhs,
            Some('/') => if rhs == 0.0 { f64::NAN } else { self.acc / rhs },
            _ => rhs,
        };
    }

    /// Feed one key; returns the new display string.
    fn key(&mut self, k: &str) -> String {
        match k {
            "C" => { *self = Calc::new(); }
            "+" | "-" | "*" | "/" => {
                self.apply_pending();
                self.pending = k.chars().next();
                self.fresh = true;
                return Self::fmt(self.acc);
            }
            "=" => {
                self.apply_pending();
                self.entry = Self::fmt(self.acc);
                self.fresh = true;
                return self.entry.clone();
            }
            "±" => {
                if let Some(rest) = self.entry.strip_prefix('-') { self.entry = rest.to_string(); }
                else if self.entry != "0" { self.entry.insert(0, '-'); }
            }
            "%" => {
                self.entry = Self::fmt(self.cur() / 100.0);
                self.fresh = false;
            }
            "." => {
                if self.fresh { self.entry = "0".into(); self.fresh = false; }
                if !self.entry.contains('.') { self.entry.push('.'); }
            }
            d if d.len() == 1 && d.as_bytes()[0].is_ascii_digit() => {
                if self.fresh { self.entry.clear(); self.fresh = false; }
                if self.entry == "0" { self.entry = d.to_string(); }
                else { self.entry.push_str(d); }
            }
            _ => {}
        }
        if self.entry.is_empty() { self.entry = "0".into(); }
        self.entry.clone()
    }
}

// Raw geometry for one stroke — mirrored into a SketchStroke (for rendering) and
// serialised to /api/sketch (for rasterisation).
#[derive(Clone)]
struct StrokeData {
    color_hex: String,
    width: f32,
    points: Vec<(f32, f32)>,
}

// Swatch index → "#rrggbb". MUST mirror SketchpadView.swatches.
const SKETCH_PALETTE: [&str; 5] = ["#e6e6eb", "#00d4ff", "#eab308", "#39ff14", "#ef4444"];
// Width index → logical px.
const SKETCH_WIDTHS: [f32; 2] = [2.5, 6.0];

fn sketch_hex(idx: i32) -> &'static str {
    SKETCH_PALETTE.get(idx.clamp(0, 4) as usize).copied().unwrap_or("#e6e6eb")
}
fn sketch_width_px(idx: i32) -> f32 {
    SKETCH_WIDTHS.get(idx.clamp(0, 1) as usize).copied().unwrap_or(2.5)
}
fn sketch_color(idx: i32) -> slint::Color {
    let h = sketch_hex(idx).trim_start_matches('#');
    let v = u32::from_str_radix(h, 16).unwrap_or(0xe6e6eb);
    slint::Color::from_rgb_u8((v >> 16) as u8, (v >> 8) as u8, v as u8)
}

// ── Feedback subsystem (toasts) ───────────────────────────────────────────────
static TOAST_SEQ: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(1);

/// Push a toast. Must run on the Slint thread (touches the TOASTS thread-local).
fn toast(kind: ToastKind, text: &str) {
    toast_action(kind, text, -1);
}

/// Push a toast that, when `action_session >= 0`, opens that session on click
/// (the transient toast AND its persisted notification-center copy both carry it).
/// Used by the mesh-message notification so a peer's message is one click from its
/// thread. Must run on the Slint thread.
fn toast_action(kind: ToastKind, text: &str, action_session: i32) {
    let timeout_ms = match kind {
        ToastKind::Error => 7000,
        ToastKind::Warn  => 6000,
        _                => 4000,
    };
    let id = TOAST_SEQ.fetch_add(1, Ordering::SeqCst);
    let item = ToastItem { id, kind, text: text.into(), timeout_ms, action_session };
    TOASTS.with(|t| {
        if let Some(model) = t.borrow().as_ref() {
            model.push(item.clone());
        }
    });
    // Persist a copy to the notification center history (newest first) and bump
    // the tray's unread badge.
    NOTIF_LOG.with(|l| {
        if let Some(model) = l.borrow().as_ref() {
            model.insert(0, item);
        }
    });
    UI_WEAK.with(|u| {
        if let Some(ui) = u.borrow().as_ref().and_then(|w| w.upgrade()) {
            ui.set_notif_unread(ui.get_notif_unread() + 1);
        }
    });
}

/// Remove a toast by id (called by the card Timer / click, and on dismiss()).
fn dismiss_toast(id: i32) {
    TOASTS.with(|t| {
        if let Some(model) = t.borrow().as_ref() {
            for i in 0..model.row_count() {
                if model.row_data(i).map(|it| it.id) == Some(id) {
                    model.remove(i);
                    break;
                }
            }
        }
    });
}

/// Raise a toast from any thread — marshals onto the Slint event loop.
fn notify(kind: ToastKind, text: impl Into<String>) {
    let text = text.into();
    slint::invoke_from_event_loop(move || toast(kind, &text)).ok();
}

/// Like `notify`, but the toast/notification opens `action_session` on click.
fn notify_action(kind: ToastKind, text: impl Into<String>, action_session: i32) {
    let text = text.into();
    slint::invoke_from_event_loop(move || toast_action(kind, &text, action_session)).ok();
}

// ── Window manager (G2) ───────────────────────────────────────────────────────
// All helpers run on the Slint thread (called from UI callbacks). The WINDOWS
// VecModel's order IS the z-order: the last row paints on top.

// ── Work Board (🗂) ───────────────────────────────────────────────────────────
// Four live column models, mutated in place from the WS event stream (Phase 1 of
// docs/ideas/state-machine-eval.md — read-only, view-driven). All board_* helpers
// run on the Slint thread (called from inside invoke_from_event_loop), so the
// thread-local BOARD is race-free, like MESSAGES / EVENTS.
struct BoardModels {
    goals:     Rc<slint::VecModel<BoardCard>>,   // autonomous goals, keyed by "goal<id>"
    active:    Rc<slint::VecModel<BoardCard>>,   // the current turn (one card)
    blocked:   Rc<slint::VecModel<BoardCard>>,   // pending approvals, keyed by call id
    subagents: Rc<slint::VecModel<BoardCard>>,   // live sub-agents, keyed by "sub<session>"
    recent:    Rc<slint::VecModel<BoardCard>>,   // finished turns / evolutions / mesh (capped)
}

const BOARD_RECENT_CAP: usize = 16;

fn board_color(r: u8, g: u8, b: u8) -> slint::Color { slint::Color::from_rgb_u8(r, g, b) }

fn board_with(f: impl FnOnce(&BoardModels)) {
    BOARD.with(|b| { if let Some(bm) = b.borrow().as_ref() { f(bm); } });
}

fn board_find(m: &slint::VecModel<BoardCard>, id: &str) -> Option<usize> {
    (0..m.row_count()).find(|&i| m.row_data(i).map(|c| c.id == id).unwrap_or(false))
}

fn board_remove(m: &slint::VecModel<BoardCard>, id: &str) {
    if let Some(i) = board_find(m, id) { m.remove(i); }
}

fn board_upsert(m: &slint::VecModel<BoardCard>, card: BoardCard) {
    match board_find(m, &card.id) {
        Some(i) => m.set_row_data(i, card),
        None    => m.push(card),
    }
}

fn board_card(id: &str, title: String, subtitle: String, badge: &str, c: slint::Color) -> BoardCard {
    BoardCard { id: id.into(), title: title.into(), subtitle: subtitle.into(), badge: badge.into(), accent: c }
}

/// Upsert the single "Active" card (the current turn) with a fresh subtitle.
fn board_active(subtitle: &str) {
    board_with(|bm| board_upsert(&bm.active,
        board_card("turn", "Agent turn".into(), subtitle.into(), "RUN", board_color(96, 165, 250))));
}

fn board_add_blocked(call_id: &str, tool: &str, preview: &str) {
    board_with(|bm| board_upsert(&bm.blocked,
        board_card(call_id, format!("approve: {tool}"), preview.into(), "ASK", board_color(251, 191, 36))));
}

fn board_clear_blocked(call_id: &str) { board_with(|bm| board_remove(&bm.blocked, call_id)); }

fn board_add_subagent(session: u64, prompt: &str) {
    let sub: String = prompt.chars().take(80).collect();
    board_with(|bm| board_upsert(&bm.subagents,
        board_card(&format!("sub{session}"), format!("Sub-agent {session}"), sub, "SUB", board_color(167, 139, 250))));
}

fn board_remove_subagent(session: u64) {
    board_with(|bm| board_remove(&bm.subagents, &format!("sub{session}")));
}

fn board_push_recent(title: String, subtitle: String, badge: &str, c: slint::Color) {
    board_with(|bm| {
        bm.recent.insert(0, board_card("", title, subtitle, badge, c));
        while bm.recent.row_count() > BOARD_RECENT_CAP { bm.recent.remove(bm.recent.row_count() - 1); }
    });
}

/// Upsert an autonomous goal's card in the GOALS column (keyed by goal id, so the
/// card updates in place through Acting → Done/Failed).
fn board_goal(id: u64, title: String, subtitle: String, badge: &str, c: slint::Color) {
    board_with(|bm| board_upsert(&bm.goals, board_card(&format!("goal{id}"), title, subtitle, badge, c)));
}

/// The (main-session) turn finished: drop the Active card + any stale approvals,
/// and drop a "done" card into Recent.
fn board_turn_done() {
    board_with(|bm| {
        board_remove(&bm.active, "turn");
        while bm.blocked.row_count() > 0 { bm.blocked.remove(bm.blocked.row_count() - 1); }
    });
    board_push_recent("Turn complete".into(), String::new(), "DONE", board_color(148, 163, 184));
}

fn kind_from_ordinal(o: i32) -> AppKind {
    match o {
        1 => AppKind::System,
        2 => AppKind::Sensor,
        3 => AppKind::Sessions,
        4 => AppKind::Settings,
        5 => AppKind::Terminal,
        6 => AppKind::Council,
        7 => AppKind::EventLog,
        8 => AppKind::Mesh,
        9 => AppKind::Inference,
        10 => AppKind::AudioEditor,
        11 => AppKind::Sonus,
        12 => AppKind::Notes,
        13 => AppKind::Face,
        14 => AppKind::Sketchpad,
        15 => AppKind::Web,
        16 => AppKind::Calculator,
        17 => AppKind::Explorer,
        18 => AppKind::Occipital,
        19 => AppKind::Board,
        _ => AppKind::Chat,
    }
}

// ── Adaptive UI (Loop 6, docs/adaptive-ui.md) ─────────────────────────────────
// AppKind ↔ ordinal ↔ agent-facing slug. Index in this table IS the ordinal —
// it must mirror `kind_from_ordinal` and the AppKind declaration order
// (types.slint); a unit test locks the agreement. The slugs are the `ui_*` tool
// vocabulary and also live in apexos-tools' UI_APPS — a new AppKind needs a slug
// in both places to be agent-reachable.
const APP_TABLE: &[(AppKind, &str)] = &[
    (AppKind::Chat, "chat"),
    (AppKind::System, "system"),
    (AppKind::Sensor, "sensor"),
    (AppKind::Sessions, "sessions"),
    (AppKind::Settings, "settings"),
    (AppKind::Terminal, "terminal"),
    (AppKind::Council, "council"),
    (AppKind::EventLog, "event-log"),
    (AppKind::Mesh, "mesh"),
    (AppKind::Inference, "inference"),
    (AppKind::AudioEditor, "audio-editor"),
    (AppKind::Sonus, "sonus"),
    (AppKind::Notes, "notes"),
    (AppKind::Face, "face"),
    (AppKind::Sketchpad, "sketchpad"),
    (AppKind::Web, "web"),
    (AppKind::Calculator, "calculator"),
    (AppKind::Explorer, "explorer"),
    (AppKind::Occipital, "occipital"),
    (AppKind::Board, "board"),
];

fn kind_ordinal(k: AppKind) -> i32 {
    APP_TABLE.iter().position(|(kk, _)| *kk == k).unwrap_or(0) as i32
}

fn kind_slug(k: AppKind) -> &'static str {
    APP_TABLE.iter().find(|(kk, _)| *kk == k).map(|(_, s)| *s).unwrap_or("chat")
}

fn kind_from_slug(s: &str) -> Option<AppKind> {
    APP_TABLE.iter().find(|(_, sl)| *sl == s.trim()).map(|(k, _)| *k)
}

fn ui_latch_bit(k: AppKind) -> u32 {
    1u32 << kind_ordinal(k)
}

/// The `ui_arrange` preset vocabulary (A2). Mirrors apexos-tools' UI_LAYOUTS.
const ARRANGE_LAYOUTS: &[&str] = &["focus", "split", "main-side", "grid"];
/// Gap between tiles and at the desktop edges, logical px.
const ARRANGE_GAP: f32 = 12.0;
/// Most windows a single arrange touches (grid caps at 3×2) — presets stage a
/// workspace, they don't tile the world.
const ARRANGE_MAX: usize = 6;
/// Most ui_* mutations that APPLY within one agent turn (A3 etiquette rail):
/// an adaptation is a deliberate act, not a strobe. Beyond the cap, verbs drop
/// silently; ui_query's `turn_mutations` shows the throttle. Mirrors the tool
/// descriptions in apexos-tools.
const UI_TURN_MUTATION_CAP: u32 = 4;

/// Pure preset-topology → rects. `n` participating windows in priority order
/// (first = main) + the usable desktop area → up to `n` `(x, y, w, h)` rects
/// in the SAME order. `focus` returns exactly ONE rect — the applier minimizes
/// the remaining participants (that is the preset's meaning). Geometry is
/// unspeakable agent-side; this fn and the WM own every pixel.
fn arrange_rects(layout: &str, n: usize, area_w: f32, area_h: f32) -> Vec<(f32, f32, f32, f32)> {
    let g = ARRANGE_GAP;
    // Degenerate areas (boot races, tiny windows) still produce sane rects.
    let aw = (area_w - 2.0 * g).max(200.0);
    let ah = (area_h - 2.0 * g).max(150.0);
    if n == 0 || !ARRANGE_LAYOUTS.contains(&layout) {
        return vec![];
    }
    let full = vec![(g, g, aw, ah)];
    match layout {
        "focus" => full, // one rect; the applier minimizes the rest
        _ if n == 1 => full,
        "split" => {
            // n equal columns, left→right in priority order.
            let w = (aw - g * (n as f32 - 1.0)) / n as f32;
            (0..n).map(|i| (g + i as f32 * (w + g), g, w, ah)).collect()
        }
        "main-side" => {
            // Main pane ~62% left; the rest stack equally in the right column.
            let main_w = (aw - g) * 0.62;
            let side_w = aw - g - main_w;
            let side_n = n - 1;
            let side_h = (ah - g * (side_n as f32 - 1.0)) / side_n as f32;
            let mut rects = vec![(g, g, main_w, ah)];
            let side_x = g + main_w + g;
            rects.extend((0..side_n).map(|i| (side_x, g + i as f32 * (side_h + g), side_w, side_h)));
            rects
        }
        "grid" => {
            // ceil(sqrt) columns; uniform cells, row-major in priority order.
            let cols = (n as f32).sqrt().ceil() as usize;
            let rows = n.div_ceil(cols);
            let w = (aw - g * (cols as f32 - 1.0)) / cols as f32;
            let h = (ah - g * (rows as f32 - 1.0)) / rows as f32;
            (0..n)
                .map(|i| {
                    let (c, r) = (i % cols, i / cols);
                    (g + c as f32 * (w + g), g + r as f32 * (h + g), w, h)
                })
                .collect()
        }
        _ => vec![],
    }
}

// ── Persona system (G4) ───────────────────────────────────────────────────────
// A persona bundles theme + chrome + wallpaper + default shell mode. Resolution
// lives here (CLAUDE.md / ui-glowup.md §5): apply_persona sets the Slint
// Personas global (chrome/wallpaper derive from it) + Palette.theme + shell-mode
// together, then persists. Ordinals mirror the Personas global:
// 0 apex · 1 mom · 2 ubuntu-dad · 3 windows-dad · 4 tech-kid · 5 aurum.

fn persona_from_ordinal(o: i32) -> Persona {
    match o {
        1 => Persona::Mom,
        2 => Persona::UbuntuDad,
        3 => Persona::WindowsDad,
        4 => Persona::TechKid,
        5 => Persona::Aurum,
        _ => Persona::Apex,
    }
}

fn persona_slug(p: Persona) -> &'static str {
    match p {
        Persona::Apex => "apex",
        Persona::Mom => "mom",
        Persona::UbuntuDad => "ubuntu-dad",
        Persona::WindowsDad => "windows-dad",
        Persona::TechKid => "tech-kid",
        Persona::Aurum => "aurum",
    }
}

fn persona_from_slug(s: &str) -> Option<Persona> {
    Some(match s.trim() {
        "apex" => Persona::Apex,
        "mom" => Persona::Mom,
        "ubuntu-dad" => Persona::UbuntuDad,
        "windows-dad" => Persona::WindowsDad,
        "tech-kid" => Persona::TechKid,
        "aurum" => Persona::Aurum,
        _ => return None,
    })
}

fn persona_theme(p: Persona) -> Theme {
    match p {
        Persona::Apex => Theme::ApexOS,
        Persona::Mom => Theme::MacOS,
        Persona::UbuntuDad => Theme::Gnome,
        Persona::WindowsDad => Theme::Windows,
        Persona::TechKid => Theme::Jarvis,
        Persona::Aurum => Theme::Aurum,
    }
}

// Default shell mode per persona (desktop-default; the tech kid boots to the
// HUD Focus face). Tier-clamped to Focus on the femtovg Nano renderer.
fn persona_default_mode(p: Persona) -> ShellMode {
    match p {
        Persona::TechKid => ShellMode::Focus,
        _ => ShellMode::Desktop,
    }
}

fn is_femtovg() -> bool {
    std::env::var("SLINT_BACKEND")
        .map(|b| b.contains("femtovg"))
        .unwrap_or(false)
}

/// Switch persona live: theme + chrome/wallpaper (derived in the global from
/// `current`) + shell mode (tier-clamped). Persists the choice when `persist`.
/// Must run on the Slint thread (touches globals + properties).
// G5 tier-2 (agent style preamble): the outbound WS sender + the active persona
// slug, process-global so `apply_persona` (Slint thread) can push a live
// `set_persona` frame and the WS task (tokio thread) can re-send the current
// persona on every (re)connect. agentd maps the persona → a response-style fragment
// it appends to the system prompt, so the agent's voice matches the chosen face.
static WS_TX: std::sync::OnceLock<mpsc::UnboundedSender<String>> = std::sync::OnceLock::new();
static CURRENT_PERSONA: std::sync::OnceLock<std::sync::Mutex<String>> = std::sync::OnceLock::new();

/// The active persona slug (defaults to "apex" before any persona is applied).
fn current_persona_slug() -> String {
    CURRENT_PERSONA.get()
        .and_then(|m| m.lock().ok().map(|s| s.clone()))
        .unwrap_or_else(|| "apex".into())
}

/// Record the active persona + push a live `set_persona` if the WS is up (no-op
/// before connect — the connect path re-sends `current_persona_slug()` anyway).
fn update_persona_voice(slug: &str) {
    let cell = CURRENT_PERSONA.get_or_init(|| std::sync::Mutex::new("apex".into()));
    if let Ok(mut s) = cell.lock() { *s = slug.to_string(); }
    if let Some(tx) = WS_TX.get() {
        let _ = tx.send(serde_json::json!({ "type": "set_persona", "persona": slug }).to_string());
    }
}

fn apply_persona(ui: &AppWindow, p: Persona, persist: bool) {
    ui.global::<Personas>().set_current(p);
    // Tell agentd the active persona so the agent's *voice* matches the face (G5
    // tier-2). Runs at boot (persisted persona) + on every live switch.
    update_persona_voice(persona_slug(p));
    ui.global::<Palette>().set_theme(persona_theme(p));
    let mode = if is_femtovg() { ShellMode::Focus } else { persona_default_mode(p) };
    ui.set_shell_mode(mode);
    if persist {
        if let Err(e) = persist_persona(p) {
            eprintln!("[ui-slint] persona persist failed: {e}");
        }
    }
}

fn persona_config_path() -> std::path::PathBuf {
    let base = std::env::var("XDG_CONFIG_HOME")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| {
            format!("{}/.config", std::env::var("HOME").unwrap_or_else(|_| ".".into()))
        });
    std::path::PathBuf::from(base).join("apexos-rs").join("persona")
}

fn persist_persona(p: Persona) -> std::io::Result<()> {
    let path = persona_config_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, persona_slug(p))
}

fn load_persona() -> Option<Persona> {
    std::fs::read_to_string(persona_config_path())
        .ok()
        .and_then(|s| persona_from_slug(&s))
}

// ── Adaptive UI Phase B — geometry persistence (docs/adaptive-ui.md §5) ──────
// The UI remembers its *shape*: per-AppKind last-known window rect + maximized,
// persisted UI-locally beside the persona file. Cerebro remembers the *why*
// (`ui-adaptation` deposits); this file is the mechanical half — don't blur
// them. Deliberately shape-not-session: the open window SET is never restored
// (a fresh boot starts clean; windows re-open on demand wearing their last
// shape). move/resize callbacks fire per pointer-move, so notes only mark a
// dirty flag and a 2s Slint Timer debounces the actual file write.

/// Mirrors app_window_frame.slint `min-w`/`min-h` — keep in sync.
const GEOM_MIN_W: f32 = 220.0;
const GEOM_MIN_H: f32 = 140.0;
/// Below this the desktop area is not believable (pre-first-frame or a broken
/// backend) — restore then floors sizes but won't invent an edge to clamp to.
const GEOM_AREA_LIVE_W: f32 = 320.0;
const GEOM_AREA_LIVE_H: f32 = 240.0;

#[derive(Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
struct GeomRec {
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    #[serde(default)]
    maximized: bool,
}

thread_local! {
    // slug → last-known shape. Loaded once at startup (Slint thread), upserted
    // by geom_note, flushed by the debounce timer.
    static GEOM_STORE: RefCell<std::collections::HashMap<String, GeomRec>> =
        RefCell::new(std::collections::HashMap::new());
    static GEOM_DIRTY: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

fn geometry_config_path() -> std::path::PathBuf {
    persona_config_path().with_file_name("geometry.json")
}

/// Seed the store from disk. Missing or corrupt file = empty store (the file
/// is a cache of preference, never load-bearing — losing it costs a cascade).
fn geom_load() {
    let map: std::collections::HashMap<String, GeomRec> =
        std::fs::read_to_string(geometry_config_path())
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
    GEOM_STORE.with(|s| *s.borrow_mut() = map);
}

/// Upsert one app's shape. No-op (no dirty mark) when unchanged, so idle
/// pointer traffic never schedules a write.
fn geom_note(kind: AppKind, x: f32, y: f32, w: f32, h: f32, maximized: bool) {
    let rec = GeomRec { x, y, w, h, maximized };
    GEOM_STORE.with(|s| {
        let mut map = s.borrow_mut();
        let slug = kind_slug(kind);
        if map.get(slug) != Some(&rec) {
            map.insert(slug.to_string(), rec);
            GEOM_DIRTY.with(|d| d.set(true));
        }
    });
}

/// Note the current shape of window `id` straight off the model row.
fn geom_note_id(model: &Rc<slint::VecModel<WindowDesc>>, id: i32) {
    if let Some(d) = wm_index_by_id(model, id).and_then(|i| model.row_data(i)) {
        geom_note(d.kind, d.x, d.y, d.w, d.h, d.maximized);
    }
}

fn geom_lookup(kind: AppKind) -> Option<GeomRec> {
    GEOM_STORE.with(|s| s.borrow().get(kind_slug(kind)).copied())
}

/// Debounced flush — the 2s timer body. Temp+rename so a mid-write crash
/// can't leave a torn file (the loader tolerates one anyway).
fn geom_flush_if_dirty() {
    if !GEOM_DIRTY.with(|d| d.replace(false)) {
        return;
    }
    let json = GEOM_STORE.with(|s| serde_json::to_string(&*s.borrow()).unwrap_or_default());
    let path = geometry_config_path();
    let write = || -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, &json)?;
        std::fs::rename(&tmp, &path)
    };
    if let Err(e) = write() {
        eprintln!("[ui-slint] geometry persist failed: {e}");
    }
}

// ── Adaptive UI Phase C — reflexes (docs/adaptive-ui.md §6) ──────────────────
// Agent-installed event→action rules the shell executes directly off its own
// event stream — below inference: zero tokens, zero latency, and they fire off
// GLOBAL events, so a root-session 3am event reaches the shell even when the
// socket follows another session. Installs/removes arrive as `ui_reflex` tool
// events (they spend a turn-mutation slot like any staging verb); FIRES are
// ambient and never spend one. The human-wins latch applies to `open` fires.

/// Trigger vocabulary. Mirrors apexos-tools `UI_REFLEX_TRIGGERS` — every entry
/// is a global event type this file's `dispatch_event` receives.
const REFLEX_TRIGGERS: &[&str] = &[
    "sensor_alert", "wake_triggered", "mesh_message", "mesh_node_status",
    "goal_state_changed", "council_started", "evolution_proposed", "error",
];
/// Action vocabulary. Mirrors apexos-tools `UI_REFLEX_ACTIONS`.
const REFLEX_ACTIONS: &[&str] = &["open", "focus", "close"];
/// Most reflexes held at once. Mirrors apexos-tools `UI_REFLEX_MAX`.
const REFLEX_MAX: usize = 8;
/// A fired reflex cools down this long — event bursts (goal steps, mesh
/// chatter) must not strobe the shell.
const REFLEX_COOLDOWN_SECS: u64 = 30;

#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct ReflexRec {
    on: String,
    #[serde(rename = "do")]
    action: String,
    app: String,
    #[serde(default)]
    fires: u32,
    /// Cooldown stamp — runtime-only, never persisted.
    #[serde(skip)]
    last_fired: Option<std::time::Instant>,
}

thread_local! {
    static REFLEXES: RefCell<Vec<ReflexRec>> = const { RefCell::new(Vec::new()) };
}

fn reflexes_config_path() -> std::path::PathBuf {
    persona_config_path().with_file_name("reflexes.json")
}

fn reflex_load() {
    let table: Vec<ReflexRec> = std::fs::read_to_string(reflexes_config_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    REFLEXES.with(|r| *r.borrow_mut() = table);
}

/// Immediate save — installs and fires are human-scale rare (no debounce needed).
fn reflex_save() {
    let json = REFLEXES.with(|r| serde_json::to_string(&*r.borrow()).unwrap_or_default());
    let path = reflexes_config_path();
    let write = || -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, &json)?;
        std::fs::rename(&tmp, &path)
    };
    if let Err(e) = write() {
        eprintln!("[ui-slint] reflex persist failed: {e}");
    }
}

/// Install (or update) a rule. Keyed by (on, app) — reinstalling updates the
/// action and resets the fire ledger. Returns false only when the table is
/// full and the key is new. Pure; unit-tested.
fn reflex_table_install(
    table: &mut Vec<ReflexRec>,
    on: &str,
    action: &str,
    app: &str,
    max: usize,
) -> bool {
    if let Some(r) = table.iter_mut().find(|r| r.on == on && r.app == app) {
        r.action = action.to_string();
        r.fires = 0;
        r.last_fired = None;
        return true;
    }
    if table.len() >= max {
        return false;
    }
    table.push(ReflexRec {
        on: on.to_string(),
        action: action.to_string(),
        app: app.to_string(),
        fires: 0,
        last_fired: None,
    });
    true
}

/// Remove the (on, app) rule. Returns whether one was removed. Pure; unit-tested.
fn reflex_table_remove(table: &mut Vec<ReflexRec>, on: &str, app: &str) -> bool {
    let before = table.len();
    table.retain(|r| !(r.on == on && r.app == app));
    table.len() != before
}

/// Apply a `ui_reflex` install/remove verb (the UI is the last validator —
/// unknown vocab is ignored, not an error). Slint thread only.
fn apply_ui_reflex(_ui: &AppWindow, args: &serde_json::Value) {
    let on = args["on"].as_str().unwrap_or("");
    let app = args["app"].as_str().unwrap_or("");
    if !REFLEX_TRIGGERS.contains(&on) || kind_from_slug(app).is_none() {
        return;
    }
    if args["remove"].as_bool().unwrap_or(false) {
        let removed = REFLEXES.with(|r| reflex_table_remove(&mut r.borrow_mut(), on, app));
        if removed {
            reflex_save();
            notify(ToastKind::Info, format!("⚡ APEX removed a reflex: {on} → {app}"));
        }
        return;
    }
    let action = args["do"].as_str().unwrap_or("");
    if !REFLEX_ACTIONS.contains(&action) {
        return;
    }
    let installed =
        REFLEXES.with(|r| reflex_table_install(&mut r.borrow_mut(), on, action, app, REFLEX_MAX));
    if installed {
        reflex_save();
        notify(
            ToastKind::Success,
            format!("⚡ APEX installed a reflex: {on} → {action} {app}"),
        );
    }
}

/// Fire the reflexes registered for `trigger` — the below-inference path.
/// Cooldown is consumed per attempt (a latch-suppressed open doesn't retry on
/// every event of a burst); `fires` counts only actual applies. Slint thread.
fn reflex_fire(ui: &AppWindow, trigger: &str) {
    let Some(model) = WINDOWS.with(|w| w.borrow().clone()) else { return };
    // Collect due rules first — never hold the table borrow across apply calls
    // (they notify → touch other thread-local models).
    let due: Vec<(String, String)> = REFLEXES.with(|r| {
        let mut tbl = r.borrow_mut();
        let mut due = Vec::new();
        for rec in tbl.iter_mut().filter(|r| r.on == trigger) {
            let cooled = rec
                .last_fired
                .is_none_or(|t| t.elapsed().as_secs() >= REFLEX_COOLDOWN_SECS);
            if cooled {
                rec.last_fired = Some(std::time::Instant::now());
                due.push((rec.action.clone(), rec.app.clone()));
            }
        }
        due
    });
    let mut applied: Vec<(String, String)> = Vec::new();
    for (action, app) in due {
        let Some(kind) = kind_from_slug(&app) else { continue };
        let did = match action.as_str() {
            // Latch-aware, no built-in toast — the reflex attribution below
            // names the trigger instead.
            "open" => agent_open_window(ui, &model, kind, false),
            "focus" | "close" => {
                let existed = wm_index_by_kind(&model, kind).is_some();
                apply_ui_verb(ui, if action == "focus" { "ui_focus" } else { "ui_close" }, &app);
                existed
            }
            _ => false,
        };
        if did {
            let verb = match action.as_str() {
                "open" => "opened",
                "focus" => "focused",
                _ => "closed",
            };
            notify(
                ToastKind::Info,
                format!("⚡ reflex {verb} {} (on {trigger})", kind_title(kind)),
            );
            applied.push((action, app));
        }
    }
    if !applied.is_empty() {
        REFLEXES.with(|r| {
            let mut tbl = r.borrow_mut();
            for (action, app) in &applied {
                if let Some(rec) = tbl
                    .iter_mut()
                    .find(|r| r.on == trigger && &r.app == app && &r.action == action)
                {
                    rec.fires += 1;
                }
            }
        });
        reflex_save();
    }
}

/// Boot seed (Phase B): launch the seed windows once the desktop area is LIVE,
/// so remembered shapes clamp against the real display. Re-arms itself every
/// 50ms while the area still reads dead (pre-first-configure); after ~2s gives
/// up waiting and launches anyway — restore then floors sizes but can't clamp
/// position (exactly the pre-deferral behavior, now only on a broken backend).
fn seed_windows_when_area_live(
    uw: slint::Weak<AppWindow>,
    w: Rc<slint::VecModel<WindowDesc>>,
    tries: u32,
) {
    let Some(ui) = uw.upgrade() else { return };
    let live = ui.get_desktop_area_w() >= GEOM_AREA_LIVE_W
        && ui.get_desktop_area_h() >= GEOM_AREA_LIVE_H;
    if !live && tries < 40 {
        slint::Timer::single_shot(std::time::Duration::from_millis(50), move || {
            seed_windows_when_area_live(uw, w, tries + 1);
        });
        return;
    }
    wm_launch(&ui, &w, AppKind::Chat);
    // Dev: APEX_FACE_AUTOOPEN=1 opens the Face window at launch (single-command
    // verification of the face, GL or 2D). Independent of the render path.
    if std::env::var_os("APEX_FACE_AUTOOPEN").is_some() {
        wm_launch(&ui, &w, AppKind::Face);
    }
    if std::env::var_os("APEX_SKETCH_AUTOOPEN").is_some() {
        wm_launch(&ui, &w, AppKind::Sketchpad);
    }
    // Dev: APEX_OCCIPITAL_DEMO=1 opens the Occipital reader at launch with a
    // sample page so the follow-along window can be verified without agentd
    // (snapshot server). =results|recall previews those modes. (Its
    // auto-reveal places a window too — same wait applies.)
    if let Some(demo) = std::env::var_os("APEX_OCCIPITAL_DEMO") {
        apply_occipital_render(&ui, occipital_demo_render(&demo.to_string_lossy()));
    }
}

/// Clamp a restored shape into the live desktop area — displays change between
/// sessions (kiosk 1080p ⇄ laptop hidpi), and a remembered rect must never
/// strand a window off-stage. Pure; unit-tested.
fn restore_geom(rec: GeomRec, area_w: f32, area_h: f32) -> (f32, f32, f32, f32) {
    let mut w = rec.w.max(GEOM_MIN_W);
    let mut h = rec.h.max(GEOM_MIN_H);
    if area_w < GEOM_AREA_LIVE_W || area_h < GEOM_AREA_LIVE_H {
        // Area not believable yet — floor sizes, keep the window on-canvas
        // top-left-wards, but don't invent a right/bottom edge.
        return (rec.x.max(0.0), rec.y.max(0.0), w, h);
    }
    w = w.min(area_w);
    h = h.min(area_h);
    let x = rec.x.clamp(0.0, area_w - w);
    let y = rec.y.clamp(0.0, area_h - h);
    (x, y, w, h)
}

fn persona_rgb(hex: u32) -> slint::Color {
    slint::Color::from_rgb_u8((hex >> 16) as u8, (hex >> 8) as u8, hex as u8)
}

// The persona catalogue — backs the first-boot wizard + the picker tiles.
fn build_persona_defs() -> Vec<PersonaDef> {
    let row = |id: i32, name: &str, title: &str, tagline: &str, glyph: &str, swatch: u32, bg: u32| {
        PersonaDef {
            id,
            name: name.into(),
            title: title.into(),
            tagline: tagline.into(),
            glyph: glyph.into(),
            swatch: persona_rgb(swatch),
            swatch_bg: persona_rgb(bg),
        }
    };
    vec![
        row(0, "Apex", "DEVELOPER", "Terse and technical — every surface exposed.", "⬢", 0x39ff14, 0x0d0f18),
        row(1, "Simple", "WARM", "Big text, plain language, voice-friendly.", "☺", 0x007aff, 0xf5f5f7),
        row(2, "Ubuntu", "BALANCED", "A familiar Linux desktop with moderate detail.", "◆", 0xe95420, 0x2c001e),
        row(3, "Classic", "GUIDED", "Friendly and guided — classic Windows affordances.", "▣", 0x0078d4, 0x0b1a2e),
        row(4, "HUD", "TECH KID", "Telemetry-rich and fast — shows the reasoning.", "⬡", 0x00d4ff, 0x000a14),
        row(5, "Aurum", "MEMORY", "Gold dashboard skin for the cerebro mind.", "⚗", 0xd4a017, 0x1a0f00),
    ]
}

fn kind_title(k: AppKind) -> &'static str {
    match k {
        AppKind::Chat => "Chat",
        AppKind::System => "System",
        AppKind::Sensor => "Sensors",
        AppKind::Sessions => "Sessions",
        AppKind::Settings => "Settings",
        AppKind::Terminal => "Terminal",
        AppKind::Council => "Council",
        AppKind::EventLog => "Event Log",
        AppKind::Mesh => "Mesh",
        AppKind::Inference => "Inference",
        AppKind::AudioEditor => "Audio Editor",
        AppKind::Sonus => "Sonus",
        AppKind::Notes => "Notes",
        AppKind::Face => "APEX",
        AppKind::Sketchpad => "Sketchpad",
        AppKind::Web => "Web",
        AppKind::Calculator => "Calculator",
        AppKind::Explorer => "Files",
        AppKind::Occipital => "Occipital",
        AppKind::Board => "Work Board",
    }
}

/// Default size for a freshly-launched window of `kind`; `n` is the current
/// window count, used to cascade so new windows don't perfectly overlap.
fn default_geom(kind: AppKind, n: i32) -> (f32, f32, f32, f32) {
    let (w, h) = match kind {
        AppKind::Chat => (760.0, 540.0),
        AppKind::System => (440.0, 460.0),
        AppKind::Sensor => (560.0, 480.0),
        AppKind::Sessions => (500.0, 520.0),
        AppKind::Settings => (660.0, 560.0),
        AppKind::Terminal => (640.0, 420.0),
        AppKind::Council => (560.0, 560.0),
        AppKind::EventLog => (560.0, 520.0),
        AppKind::Mesh => (520.0, 460.0),
        AppKind::Inference => (520.0, 520.0),
        AppKind::AudioEditor => (660.0, 600.0),
        AppKind::Sonus => (480.0, 540.0),
        AppKind::Notes => (640.0, 540.0),
        AppKind::Face => (380.0, 460.0),
        AppKind::Sketchpad => (600.0, 580.0),
        AppKind::Web => (460.0, 400.0),
        AppKind::Calculator => (300.0, 440.0),
        AppKind::Explorer => (680.0, 520.0),
        AppKind::Occipital => (720.0, 620.0),
        AppKind::Board => (880.0, 600.0),
    };
    let step = (n % 6) as f32 * 30.0;
    (72.0 + step, 32.0 + step, w, h)
}

// ── Occipital follow-along reader (Phase 9) ─────────────────────────────────────
// The agent's web reads (web_fetch/web_search/web_recall) return a flat,
// `kind`-discriminated payload (Occipital's docs/follow-along.md). agentd's MCP
// client passes the tool result through as the MCP content array
// `[{"type":"text","text":"<json>"}]` (mcp.rs) and Event::ToolResult carries no
// tool name — so we recover the payload from any transport shape and route on
// its `kind`, mirroring how turn.rs recovers the vision sentinel. Markdown is
// parsed into ReaderBlocks and rendered natively (Slint has no webview).

/// Plain (Send) render plan built off the Slint thread; the invoke closure turns
/// the tuples into ReaderBlock/ReaderLink on the Slint thread.
struct OccipitalRender {
    mode:        String,   // page|results|recall
    title:       String,
    url:         String,
    meta:        String,
    badge:       String,   // cache|live|""
    blocks:      Vec<(String, String, i32)>,             // kind, text, depth
    links:       Vec<(String, String, String, String)>,  // label, url, detail, badge
    crumb_label: String,
    crumb_url:   String,
}

/// Recover an Occipital payload (an object with `kind` ∈ {page,results,recall})
/// from a tool result's content, whatever the transport shape: a bare object, a
/// JSON string, or an MCP text-content array.
fn occipital_payload(content: &Value) -> Option<Value> {
    fn is_occipital(v: &Value) -> bool {
        matches!(
            v.get("kind").and_then(|k| k.as_str()),
            Some("page" | "results" | "recall")
        )
    }
    if is_occipital(content) {
        return Some(content.clone());
    }
    if let Some(s) = content.as_str() {
        if let Ok(v) = serde_json::from_str::<Value>(s) {
            if is_occipital(&v) {
                return Some(v);
            }
        }
    }
    if let Some(arr) = content.as_array() {
        for item in arr {
            if item.get("type").and_then(|t| t.as_str()) == Some("text") {
                if let Some(s) = item.get("text").and_then(|t| t.as_str()) {
                    if let Ok(v) = serde_json::from_str::<Value>(s) {
                        if is_occipital(&v) {
                            return Some(v);
                        }
                    }
                }
            }
        }
    }
    None
}

/// Strip inline markdown to clean reading text: `[t](u)`→t, `![a](u)`→"🖼 a",
/// and the `**`/`*`/`` ` `` emphasis+code markers (links are surfaced separately
/// in the page's link list). Occipital uses `*` for emphasis, never `_`, so
/// underscores in identifiers/URLs are left intact.
fn strip_inline_md(s: &str) -> String {
    fn take_until(chars: &mut std::iter::Peekable<std::str::Chars>, end: char) -> String {
        let mut out = String::new();
        for c in chars.by_ref() {
            if c == end { break; }
            out.push(c);
        }
        out
    }
    fn skip_paren(chars: &mut std::iter::Peekable<std::str::Chars>) {
        if chars.peek() == Some(&'(') {
            chars.next();
            for c in chars.by_ref() {
                if c == ')' { break; }
            }
        }
    }
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' => { if let Some(n) = chars.next() { out.push(n); } }
            '`' | '*' => {}
            '!' if chars.peek() == Some(&'[') => {
                chars.next();
                let alt = take_until(&mut chars, ']');
                skip_paren(&mut chars);
                if !alt.is_empty() { out.push_str("🖼 "); out.push_str(&alt); }
            }
            '[' => {
                let text = take_until(&mut chars, ']');
                if chars.peek() == Some(&'(') {
                    skip_paren(&mut chars);
                    out.push_str(&text);
                } else {
                    out.push('['); out.push_str(&text); out.push(']');
                }
            }
            _ => out.push(c),
        }
    }
    out.trim().to_string()
}

const OCCIPITAL_MAX_BLOCKS: usize = 400;

/// Parse reader-mode markdown into a flat list of (kind, text, depth) blocks.
fn parse_reader_markdown(md: &str) -> Vec<(String, String, i32)> {
    let mut blocks: Vec<(String, String, i32)> = Vec::new();
    let mut para = String::new();
    let mut in_code = false;
    let mut code = String::new();

    let flush_para = |para: &mut String, blocks: &mut Vec<(String, String, i32)>| {
        let p = para.trim();
        if !p.is_empty() {
            blocks.push(("p".into(), strip_inline_md(p), 0));
        }
        para.clear();
    };

    for raw in md.lines() {
        if blocks.len() >= OCCIPITAL_MAX_BLOCKS {
            blocks.push(("rule".into(), String::new(), 0));
            blocks.push(("p".into(), "… (page truncated for display)".into(), 0));
            return blocks;
        }
        let trimmed = raw.trim_end();
        let lead = trimmed.trim_start();

        if in_code {
            if lead.starts_with("```") {
                let body = code.trim_end().to_string();
                let body = if body.len() > 4000 { format!("{}…", &body[..4000]) } else { body };
                blocks.push(("code".into(), body, 0));
                code.clear();
                in_code = false;
            } else {
                code.push_str(raw);
                code.push('\n');
            }
            continue;
        }

        if lead.starts_with("```") {
            flush_para(&mut para, &mut blocks);
            in_code = true;
        } else if lead.is_empty() {
            flush_para(&mut para, &mut blocks);
        } else if lead.starts_with('#') {
            flush_para(&mut para, &mut blocks);
            let hashes = lead.chars().take_while(|&c| c == '#').count();
            let level = hashes.clamp(1, 3);
            let text = lead.trim_start_matches('#').trim();
            blocks.push((format!("h{level}"), strip_inline_md(text), 0));
        } else if matches!(lead, "---" | "***" | "___" | "- - -") {
            flush_para(&mut para, &mut blocks);
            blocks.push(("rule".into(), String::new(), 0));
        } else if let Some(rest) = bullet_rest(lead) {
            flush_para(&mut para, &mut blocks);
            let indent = trimmed.len() - lead.len();
            let depth = (indent / 2).min(4) as i32;
            blocks.push(("bullet".into(), strip_inline_md(rest), depth));
        } else if let Some(rest) = lead.strip_prefix("> ").or_else(|| lead.strip_prefix(">")) {
            flush_para(&mut para, &mut blocks);
            blocks.push(("quote".into(), strip_inline_md(rest.trim()), 0));
        } else {
            if !para.is_empty() { para.push(' '); }
            para.push_str(lead);
        }
    }
    flush_para(&mut para, &mut blocks);
    if in_code && !code.trim().is_empty() {
        blocks.push(("code".into(), code.trim_end().to_string(), 0));
    }
    blocks
}

/// A leading `- ` / `* ` / `+ ` bullet marker → the text after it.
fn bullet_rest(lead: &str) -> Option<&str> {
    for m in ["- ", "* ", "+ "] {
        if let Some(rest) = lead.strip_prefix(m) {
            return Some(rest);
        }
    }
    None
}

fn json_str(v: &Value, key: &str) -> String {
    v.get(key).and_then(|x| x.as_str()).unwrap_or("").to_string()
}

/// Trim a breadcrumb label to a chip-friendly length (char-safe).
fn cap_crumb(s: &str) -> String {
    let mut out: String = s.chars().take(24).collect();
    if s.chars().count() > 24 {
        out.push('…');
    }
    out
}

/// Build the (Send) render plan from a recovered Occipital payload.
fn build_occipital_render(p: &Value) -> OccipitalRender {
    let kind = json_str(p, "kind");
    let from_cache = p.get("from_cache").and_then(|b| b.as_bool());
    let badge = match (kind.as_str(), from_cache) {
        ("recall", _) => String::new(),
        (_, Some(true)) => "cache".into(),
        (_, Some(false)) => "live".into(),
        _ => String::new(),
    };

    match kind.as_str() {
        "page" => {
            let url = json_str(p, "url");
            let title = {
                let t = json_str(p, "title");
                if t.is_empty() { url.clone() } else { t }
            };
            let markdown = json_str(p, "markdown");
            let saved = json_str(p, "status") == "saved";
            let blocks = if markdown.is_empty() {
                Vec::new()
            } else {
                parse_reader_markdown(&markdown)
            };
            let links: Vec<(String, String, String, String)> = p
                .get("links")
                .and_then(|l| l.as_array())
                .map(|arr| {
                    arr.iter()
                        .take(60)
                        .map(|l| {
                            let u = json_str(l, "url");
                            let t = json_str(l, "text");
                            let label = if t.trim().is_empty() { u.clone() } else { t };
                            (label, u, String::new(), String::new())
                        })
                        .filter(|(_, u, _, _)| !u.is_empty())
                        .collect()
                })
                .unwrap_or_default();
            let meta = if saved {
                "📌 saved to memory".into()
            } else {
                format!("{} link{} on page", links.len(), if links.len() == 1 { "" } else { "s" })
            };
            let crumb = cap_crumb(&title);
            OccipitalRender {
                mode: "page".into(), title, url, meta, badge, blocks, links,
                crumb_label: crumb, crumb_url: json_str(p, "url"),
            }
        }
        "results" => {
            let query = json_str(p, "query");
            let provider = json_str(p, "provider");
            let links: Vec<(String, String, String, String)> = p
                .get("results")
                .and_then(|r| r.as_array())
                .map(|arr| {
                    arr.iter()
                        .take(60)
                        .map(|r| {
                            let u = json_str(r, "url");
                            let t = json_str(r, "title");
                            let label = if t.trim().is_empty() { u.clone() } else { t };
                            let rank = r.get("rank").and_then(|x| x.as_u64()).unwrap_or(0);
                            (label, u, json_str(r, "snippet"), format!("#{}", rank + 1))
                        })
                        .collect()
                })
                .unwrap_or_default();
            let meta = format!(
                "{}{} result{}",
                if provider.is_empty() { String::new() } else { format!("{provider} · ") },
                links.len(),
                if links.len() == 1 { "" } else { "s" }
            );
            OccipitalRender {
                mode: "results".into(),
                title: query.clone(),
                url: String::new(),
                meta, badge,
                blocks: Vec::new(),
                links,
                crumb_label: cap_crumb(&format!("find: {query}")),
                crumb_url: String::new(),
            }
        }
        _ => {
            // recall
            let query = json_str(p, "query");
            let links: Vec<(String, String, String, String)> = p
                .get("hits")
                .and_then(|h| h.as_array())
                .map(|arr| {
                    arr.iter()
                        .take(60)
                        .map(|h| {
                            let u = json_str(h, "url");
                            let t = json_str(h, "title");
                            let label = if t.trim().is_empty() { u.clone() } else { t };
                            let badge = h
                                .get("score")
                                .and_then(|s| s.as_f64())
                                .map(|s| format!("{s:.2}"))
                                .unwrap_or_else(|| "kw".into());
                            (label, u, json_str(h, "snippet"), badge)
                        })
                        .collect()
                })
                .unwrap_or_default();
            let meta = format!("{} memory hit{}", links.len(), if links.len() == 1 { "" } else { "s" });
            OccipitalRender {
                mode: "recall".into(),
                title: query.clone(),
                url: String::new(),
                meta,
                badge: String::new(),
                blocks: Vec::new(),
                links,
                crumb_label: cap_crumb(&format!("mem: {query}")),
                crumb_url: String::new(),
            }
        }
    }
}

/// Apply a built render plan to the reader window (Slint thread only): set the
/// scalars, rebuild the block/link models, push the trail breadcrumb, and reveal
/// the window the first time APEX browses (unless the user has closed it).
fn apply_occipital_render(ui: &AppWindow, r: OccipitalRender) {
    let blocks: Vec<ReaderBlock> = r
        .blocks
        .into_iter()
        .map(|(kind, text, depth)| ReaderBlock { kind: kind.into(), text: text.into(), depth })
        .collect();
    let links: Vec<ReaderLink> = r
        .links
        .into_iter()
        .map(|(label, url, detail, badge)| ReaderLink {
            label: label.into(),
            url: url.into(),
            detail: detail.into(),
            badge: badge.into(),
        })
        .collect();

    ui.set_occipital_mode(r.mode.into());
    ui.set_occipital_title(r.title.into());
    ui.set_occipital_url(r.url.into());
    ui.set_occipital_meta(r.meta.into());
    ui.set_occipital_badge(r.badge.into());
    ui.set_occipital_blocks(slint::ModelRc::from(Rc::new(slint::VecModel::from(blocks))));
    ui.set_occipital_links(slint::ModelRc::from(Rc::new(slint::VecModel::from(links))));

    // Trail breadcrumb (newest last, cap 8; skip an immediate repeat).
    OCCIPITAL_TRAIL.with(|t| {
        if let Some(model) = t.borrow().as_ref() {
            let crumb = ReaderLink {
                label: r.crumb_label.into(),
                url: r.crumb_url.into(),
                detail: "".into(),
                badge: "".into(),
            };
            let dup = model
                .row_count()
                .checked_sub(1)
                .and_then(|i| model.row_data(i))
                .map(|l| l.label == crumb.label)
                .unwrap_or(false);
            if !dup {
                model.push(crumb);
                while model.row_count() > 8 {
                    model.remove(0);
                }
            }
        }
    });

    ui.set_occipital_scroll_tick(ui.get_occipital_scroll_tick() + 1);

    // Reveal the reader the first time APEX browses — an agent act through the
    // SAME latch-aware path as ui_open (A3: the standalone suppress flag folded
    // into the generalized latch): a user-closed reader stays closed until the
    // user re-invites it from the menu. Quiet — auto-reveal never toasted.
    WINDOWS.with(|w| {
        if let Some(model) = w.borrow().as_ref() {
            if wm_index_by_kind(model, AppKind::Occipital).is_none() {
                agent_open_window(ui, model, AppKind::Occipital, false);
            }
        }
    });
}

/// Sample render for `APEX_OCCIPITAL_DEMO` (page|results|recall) — lets the reader
/// window be verified via the snapshot server with no agentd / no network.
fn occipital_demo_render(mode: &str) -> OccipitalRender {
    let payload = match mode.trim() {
        "results" => serde_json::json!({
            "kind": "results", "query": "raspberry pi 5 power delivery",
            "provider": "duckduckgo", "count": 3, "from_cache": false,
            "results": [
                {"title": "Raspberry Pi 5 — 27W Power Supply", "url": "https://www.raspberrypi.com/products/27w-power-supply/", "snippet": "The official 27W USB-C PD supply delivers 5V/5A for full Pi 5 performance and peripheral power.", "rank": 0},
                {"title": "Pi 5 USB-C PD requirements", "url": "https://forums.raspberrypi.com/viewtopic.php?t=357789", "snippet": "Without a 5V/5A PD source the firmware caps downstream USB to 600mA.", "rank": 1},
                {"title": "USB-C PD trigger boards explained", "url": "https://example.com/pd-trigger", "snippet": "A PD trigger negotiates a fixed 5V/5A profile from any compliant USB-C PD brick.", "rank": 2}
            ]
        }),
        "recall" => serde_json::json!({
            "kind": "recall", "query": "pi power delivery", "count": 2,
            "hits": [
                {"url": "https://www.raspberrypi.com/products/27w-power-supply/", "title": "Pi 5 27W PSU", "snippet": "5V/5A USB-C PD — the official supply.", "score": 0.83},
                {"url": "https://forums.raspberrypi.com/viewtopic.php?t=357789", "title": "PD requirements thread", "snippet": "Caps peripherals without 5A.", "score": null}
            ]
        }),
        _ => serde_json::json!({
            "kind": "page", "url": "https://www.raspberrypi.com/products/raspberry-pi-5/",
            "from_cache": true, "title": "Raspberry Pi 5",
            "markdown": "# Raspberry Pi 5\n\nThe **Raspberry Pi 5** is the latest single-board computer, delivering a *significant* performance uplift over the Pi 4.\n\n## Specifications\n\n- Broadcom BCM2712 quad-core Cortex-A76 @ 2.4GHz\n- VideoCore VII GPU\n- Up to 16GB LPDDR4X RAM\n\n## Power\n\nUse the [official 27W PD supply](https://www.raspberrypi.com/products/27w-power-supply/) for full performance.\n\n> A 5V/5A USB-C PD source is required to power peripherals at full current.\n\n```\nvcgencmd measure_temp\n```\n\n---\n\nMore on the [product page](https://www.raspberrypi.com/products/raspberry-pi-5/).",
            "links": [
                {"text": "official 27W PD supply", "url": "https://www.raspberrypi.com/products/27w-power-supply/"},
                {"text": "product page", "url": "https://www.raspberrypi.com/products/raspberry-pi-5/"}
            ],
            "content_hash": "abc123"
        }),
    };
    build_occipital_render(&payload)
}

fn wm_index_by_id(model: &Rc<slint::VecModel<WindowDesc>>, id: i32) -> Option<usize> {
    (0..model.row_count()).find(|&i| model.row_data(i).map(|d| d.id) == Some(id))
}

fn wm_index_by_kind(model: &Rc<slint::VecModel<WindowDesc>>, kind: AppKind) -> Option<usize> {
    (0..model.row_count()).find(|&i| model.row_data(i).map(|d| d.kind) == Some(kind))
}

/// True when a face window exists and is not minimised. Slint-thread only
/// (reads the WINDOWS thread-local). Gates both the GL face draw and its 30fps
/// redraw loop, so a closed face window costs nothing on the kiosk.
fn face_window_visible() -> bool {
    WINDOWS.with(|w| {
        w.borrow().as_ref().is_some_and(|m| {
            wm_index_by_kind(m, AppKind::Face)
                .and_then(|i| m.row_data(i))
                .is_some_and(|d| !d.minimized)
        })
    })
}

/// Move a window to the top of the z-order (end of the model) and mark it focused.
fn wm_focus(ui: &AppWindow, model: &Rc<slint::VecModel<WindowDesc>>, id: i32) {
    if let Some(i) = wm_index_by_id(model, id) {
        let d = model.remove(i);
        model.push(d);
        ui.set_focused_id(id);
    }
}

/// Recompute focus to the top-most non-minimised window (after a close/minimise).
fn wm_refocus_top(ui: &AppWindow, model: &Rc<slint::VecModel<WindowDesc>>) {
    for i in (0..model.row_count()).rev() {
        if let Some(d) = model.row_data(i) {
            if !d.minimized {
                ui.set_focused_id(d.id);
                return;
            }
        }
    }
    ui.set_focused_id(0);
}

fn wm_update_row(model: &Rc<slint::VecModel<WindowDesc>>, id: i32, f: impl FnOnce(&mut WindowDesc)) {
    if let Some(i) = wm_index_by_id(model, id) {
        if let Some(mut d) = model.row_data(i) {
            f(&mut d);
            model.set_row_data(i, d);
        }
    }
}

/// Open (or reveal) the single window of `kind`: un-minimise + focus if it
/// already exists, else create it with a cascaded default geometry.
fn wm_launch(ui: &AppWindow, model: &Rc<slint::VecModel<WindowDesc>>, kind: AppKind) {
    if let Some(i) = wm_index_by_kind(model, kind) {
        let id = model.row_data(i).map(|d| d.id).unwrap_or(0);
        wm_update_row(model, id, |d| d.minimized = false);
        wm_focus(ui, model, id);
        return;
    }
    let id = WIN_NEXT_ID.with(|c| {
        let v = c.get();
        c.set(v + 1);
        v
    });
    // Phase B: a kind we've seen before re-opens wearing its last shape
    // (clamped to the live desktop area); first-ever opens cascade as always.
    let (x, y, w, h, maximized) = match geom_lookup(kind) {
        Some(rec) => {
            let (x, y, w, h) =
                restore_geom(rec, ui.get_desktop_area_w(), ui.get_desktop_area_h());
            (x, y, w, h, rec.maximized)
        }
        None => {
            let (x, y, w, h) = default_geom(kind, model.row_count() as i32);
            (x, y, w, h, false)
        }
    };
    model.push(WindowDesc {
        id,
        kind,
        title: kind_title(kind).into(),
        x,
        y,
        w,
        h,
        minimized: false,
        maximized,
    });
    wm_focus(ui, model, id);
}

/// Apply an agent `ui_open` / `ui_close` / `ui_focus` verb (adaptive UI, Loop 6
/// — docs/adaptive-ui.md). Slint thread only. Unknown or inapplicable targets
/// are ignored, not errors — the UI is the last validator; the agent discovers
/// outcomes by looking (`ui_query`), not from acks. The latch enforces "the
/// human always wins": an app the user closed after the agent opened it will
/// not re-open this session.
/// Open-or-reveal `kind` on the agent's behalf: latch-guarded, agent-marked,
/// per-app refresh included. Returns false when the latch suppressed it.
/// `toast` is false when `ui_arrange` stages several windows under its own
/// single attribution toast. Slint thread only.
fn agent_open_window(
    ui: &AppWindow,
    model: &Rc<slint::VecModel<WindowDesc>>,
    kind: AppKind,
    toast: bool,
) -> bool {
    let bit = ui_latch_bit(kind);
    if UI_LATCHED.with(|m| m.get()) & bit != 0 {
        return false; // user overruled this app earlier — the overrule stands
    }
    let existed = wm_index_by_kind(model, kind).is_some();
    let was_agent = AGENT_OPENED.with(|m| m.get()) & bit != 0;
    // The full menu-launch path — per-app refresh included (a raw wm_launch
    // leaves Settings/Sessions/Terminal windows empty). Runs synchronously;
    // it also clears this kind's latch bits, so the agent marks re-land after
    // it returns. (Occipital is uniform since A3: its auto-reveal suppression
    // IS the latch, so there's no separate flag an agent open could re-arm.)
    ui.invoke_launch_app(kind_ordinal(kind));
    if !existed || was_agent {
        AGENT_OPENED.with(|m| m.set(m.get() | bit));
    }
    if !existed && toast {
        notify(ToastKind::Info, format!("🪟 APEX opened {}", kind_title(kind)));
    }
    true
}

fn apply_ui_verb(ui: &AppWindow, verb: &str, app: &str) {
    let Some(kind) = kind_from_slug(app) else { return };
    let Some(model) = WINDOWS.with(|w| w.borrow().clone()) else { return };
    let bit = ui_latch_bit(kind);
    match verb {
        "ui_open" => {
            agent_open_window(ui, &model, kind, true);
        }
        "ui_close" => {
            // Agent-close ≠ user-close: no latch — the latch encodes the
            // human's overrule, not tidying-up.
            if let Some(i) = wm_index_by_kind(&model, kind) {
                // Drag guard (A3): never yank a window out from under the
                // pointer — skip entirely, mark intact.
                if let Some(d) = model.row_data(i) {
                    if d.id == ui.global::<WmState>().get_dragging_id() {
                        return;
                    }
                    // Phase B: capture the final shape before removal.
                    geom_note(d.kind, d.x, d.y, d.w, d.h, d.maximized);
                }
                AGENT_OPENED.with(|m| m.set(m.get() & !bit));
                model.remove(i);
                wm_refocus_top(ui, &model);
            } else {
                AGENT_OPENED.with(|m| m.set(m.get() & !bit));
            }
        }
        "ui_focus" => {
            if let Some(i) = wm_index_by_kind(&model, kind) {
                if let Some(id) = model.row_data(i).map(|d| d.id) {
                    wm_update_row(&model, id, |d| d.minimized = false);
                    wm_focus(ui, &model, id);
                }
            }
        }
        _ => {}
    }
}

/// Apply `ui_arrange` (adaptive UI A2): stage participants into a preset
/// topology. Desktop-mode only — the Focus shell has no window layer, so
/// there it is a structural no-op the agent can read via ui_query's
/// shell_mode. Participants: the agent's explicit `apps` (validated,
/// de-duped, latch-respecting, missing windows opened quietly — one arrange
/// = one toast) or, when omitted, the currently visible windows topmost-first
/// (minimized ones the user tucked away are not resurrected). Slint thread only.
fn apply_ui_arrange(ui: &AppWindow, layout: &str, apps: &[String]) {
    if !ARRANGE_LAYOUTS.contains(&layout) {
        return;
    }
    if ui.get_shell_mode() != ShellMode::Desktop {
        return;
    }
    let Some(model) = WINDOWS.with(|w| w.borrow().clone()) else { return };

    // Resolve participants in priority order (first = main).
    let mut kinds: Vec<AppKind> = Vec::new();
    if apps.is_empty() {
        for i in (0..model.row_count()).rev() {
            if let Some(d) = model.row_data(i) {
                if !d.minimized && !kinds.contains(&d.kind) {
                    kinds.push(d.kind);
                }
            }
        }
        kinds.truncate(ARRANGE_MAX);
    } else {
        let cap = if layout == "focus" { 1 } else { ARRANGE_MAX };
        for slug in apps {
            if kinds.len() >= cap {
                break; // don't open windows that couldn't participate anyway
            }
            let Some(k) = kind_from_slug(slug) else { continue };
            if kinds.contains(&k) {
                continue;
            }
            if agent_open_window(ui, &model, k, false) {
                kinds.push(k);
            }
        }
    }
    let Some(&main_kind) = kinds.first() else { return };

    let rects = arrange_rects(
        layout,
        kinds.len(),
        ui.get_desktop_area_w(),
        ui.get_desktop_area_h(),
    );
    if rects.is_empty() {
        return;
    }

    // Drag guard (A3): a window under live pointer interaction keeps its
    // geometry — the frame's local drag deltas would commit over whatever we
    // set, and fighting the hand is the one thing an adaptation must never do.
    let dragging = ui.global::<WmState>().get_dragging_id();
    for (i, kind) in kinds.iter().enumerate() {
        let Some(row) = wm_index_by_kind(&model, *kind) else { continue };
        let Some(id) = model.row_data(row).map(|d| d.id) else { continue };
        if id == dragging {
            continue;
        }
        if let Some(&(x, y, w, h)) = rects.get(i) {
            wm_update_row(&model, id, |d| {
                d.minimized = false;
                d.maximized = false;
                d.x = x;
                d.y = y;
                d.w = w;
                d.h = h;
            });
            // Phase B: an arranged shape is the new remembered shape — APEX's
            // tidy-up survives a restart the same as a hand-placed one.
            geom_note(*kind, x, y, w, h, false);
        }
    }
    // `focus` means ONE thing on stage: every other open window minimizes
    // (reversible from the taskbar — and `arrange_rects` returned one rect).
    if layout == "focus" {
        for i in 0..model.row_count() {
            if let Some(d) = model.row_data(i) {
                if d.kind != main_kind && !d.minimized && d.id != dragging {
                    wm_update_row(&model, d.id, |d| d.minimized = true);
                }
            }
        }
    }
    // The main participant ends on top.
    if let Some(row) = wm_index_by_kind(&model, main_kind) {
        if let Some(id) = model.row_data(row).map(|d| d.id) {
            wm_focus(ui, &model, id);
        }
    }
    notify(ToastKind::Info, format!("🪟 APEX arranged the desktop ({layout})"));
}

/// Apply `ui_theme` (adaptive UI A2): switch the persona skin through the
/// same chokepoint as the picker — theme + chrome + wallpaper + shell mode +
/// the agent's voice (`set_persona` → the style layer), persisted like a
/// human pick. Policy is allow; the etiquette (offer, don't theme unprompted
/// — the conversational yes is the confirmation) lives soul-side. Slint
/// thread only.
fn apply_ui_theme(ui: &AppWindow, persona: &str) {
    let Some(p) = persona_from_slug(persona) else { return };
    apply_persona(ui, p, true);
    notify(
        ToastKind::Info,
        format!("🎨 APEX switched the skin to {}", persona_title(p)),
    );
}

/// Display names matching the persona catalogue tiles (`persona_defs`).
fn persona_title(p: Persona) -> &'static str {
    match p {
        Persona::Apex => "Apex",
        Persona::Mom => "Simple",
        Persona::UbuntuDad => "Ubuntu",
        Persona::WindowsDad => "Classic",
        Persona::TechKid => "HUD",
        Persona::Aurum => "Aurum",
    }
}

/// Strip ANSI/VT escape sequences for the line-mode terminal (no cursor grid).
/// Drops CSI (ESC[…), OSC (ESC]…BEL/ST), charset designations, carriage returns,
/// and other C0 control bytes — keeping only printable text plus \n and \t.
fn strip_ansi(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut col = 0usize; // current column, for tab expansion (8-wide stops)
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\u{1b}' => match chars.next() {
                // CSI: consume params/intermediates until a final byte @–~
                Some('[') => {
                    while let Some(&n) = chars.peek() {
                        chars.next();
                        if ('@'..='~').contains(&n) { break; }
                    }
                }
                // OSC: consume until BEL or ST (ESC \)
                Some(']') => {
                    while let Some(&n) = chars.peek() {
                        chars.next();
                        if n == '\u{07}' { break; }
                        if n == '\u{1b}' {
                            if chars.peek() == Some(&'\\') { chars.next(); }
                            break;
                        }
                    }
                }
                // Charset designation (ESC( / ESC) ) — drop the one trailing byte.
                Some('(') | Some(')') => { chars.next(); }
                // Any other single-char escape: the following char is already consumed.
                _ => {}
            },
            '\r' | '\u{07}' => {} // carriage return / bell — meaningless without a grid
            '\n' => { out.push('\n'); col = 0; }
            '\t' => { // expand to the next 8-col tab stop (raw \t renders as a box)
                let spaces = 8 - (col % 8);
                for _ in 0..spaces { out.push(' '); }
                col += spaces;
            }
            c if (c as u32) < 0x20 => {} // other C0 control chars
            c => { out.push(c); col += 1; }
        }
    }
    out
}

/// The /terminal-ws PTY task: streams binary PTY output into `terminal-text`
/// (ANSI stripped, ring-buffered) and writes stdin lines from `rx`. Reconnects
/// with backoff; a fresh bash is spawned on each (re)connect.
async fn run_terminal_ws(
    url: String,
    ui_weak: slint::Weak<AppWindow>,
    mut rx: mpsc::UnboundedReceiver<String>,
) {
    const CAP: usize = 60_000; // keep the last ~60 KB of scrollback
    let mut buf = String::new();
    let mut backoff_secs: u64 = 2;

    loop {
        eprintln!("[ui-slint] terminal connecting to {}", redact_ws_url(&url));
        let (ws, _) = match connect_async(&url).await {
            Ok(pair) => pair,
            Err(e) => {
                eprintln!("[ui-slint] terminal WS connect failed: {e}");
                tokio::time::sleep(std::time::Duration::from_secs(backoff_secs)).await;
                backoff_secs = (backoff_secs * 2).min(30);
                continue;
            }
        };
        backoff_secs = 2;
        let (mut write, mut read) = ws.split();

        loop {
            tokio::select! {
                msg = read.next() => match msg {
                    Some(Ok(Message::Binary(data))) => {
                        buf.push_str(&strip_ansi(&String::from_utf8_lossy(&data)));
                        if buf.len() > CAP {
                            let mut start = buf.len() - CAP / 2;
                            while !buf.is_char_boundary(start) { start += 1; }
                            buf.drain(..start);
                        }
                        let snap = buf.clone();
                        let w = ui_weak.clone();
                        slint::invoke_from_event_loop(move || {
                            if let Some(ui) = w.upgrade() {
                                ui.set_terminal_text(snap.into());
                                let t = ui.get_terminal_scroll_tick();
                                ui.set_terminal_scroll_tick(t.wrapping_add(1));
                            }
                        }).ok();
                    }
                    Some(Ok(Message::Text(t))) => {
                        buf.push_str(&strip_ansi(&t));
                    }
                    _ => {
                        eprintln!("[ui-slint] terminal WS disconnected — reconnecting in {backoff_secs}s");
                        tokio::time::sleep(std::time::Duration::from_secs(backoff_secs)).await;
                        backoff_secs = (backoff_secs * 2).min(30);
                        break;
                    }
                },
                line = rx.recv() => {
                    if let Some(l) = line {
                        write.send(Message::Binary(l.into_bytes())).await.ok();
                    }
                }
            }
        }
    }
}

/// Spawn the terminal WS task on first Terminal-window launch (once).
fn start_terminal(rt: &tokio::runtime::Handle, url: &str, ui_weak: slint::Weak<AppWindow>) {
    if TERM_STARTED.with(|c| c.get()) {
        return;
    }
    // Latch STARTED only once we actually hold the receiver and spawn the task.
    // Setting it before this guard bricked the terminal: if TERM_RX was ever None
    // (already taken / not yet seeded) STARTED stayed true with no task and no retry.
    if let Some(rx) = TERM_RX.with(|r| r.borrow_mut().take()) {
        TERM_STARTED.with(|c| c.set(true));
        rt.spawn(run_terminal_ws(url.to_string(), ui_weak, rx));
    }
}

/// Parse a "#RRGGBB" hex string into a Slint colour; falls back to a rotating
/// palette (indexed by `idx`) when a council agent supplies no colour.
fn council_accent(hex: Option<&str>, idx: usize) -> slint::Color {
    const FALLBACK: [(u8, u8, u8); 6] = [
        (0x00, 0xd4, 0xff), (0xd7, 0x77, 0x57), (0xff, 0xc1, 0x07),
        (0x82, 0x7d, 0xbd), (0x4a, 0xde, 0x80), (0xf4, 0x72, 0xb6),
    ];
    if let Some(h) = hex {
        let h = h.trim_start_matches('#');
        if h.len() == 6 {
            if let Ok(n) = u32::from_str_radix(h, 16) {
                return slint::Color::from_rgb_u8((n >> 16) as u8, (n >> 8) as u8, n as u8);
            }
        }
    }
    let (r, g, b) = FALLBACK[idx % FALLBACK.len()];
    slint::Color::from_rgb_u8(r, g, b)
}

/// Mutate the council agent with the given id (delta append / done).
fn council_update(id: &str, f: impl FnOnce(&mut CouncilAgent)) {
    COUNCIL.with(|c| {
        if let Some(model) = c.borrow().as_ref() {
            for i in 0..model.row_count() {
                if let Some(mut a) = model.row_data(i) {
                    if a.id == id {
                        f(&mut a);
                        model.set_row_data(i, a);
                        return;
                    }
                }
            }
        }
    });
}

/// Nudge the chat ScrollView to the bottom by bumping the AgentBridge tick.
fn bump_scroll(ui: &AppWindow) {
    let t = ui.global::<AgentBridge>().get_chat_scroll_tick();
    ui.global::<AgentBridge>().set_chat_scroll_tick(t.wrapping_add(1));
}

fn push_message(item: MessageItem) {
    MESSAGES.with(|m| {
        if let Some(model) = m.borrow().as_ref() {
            model.push(item);
        }
    });
}

fn update_last_agent_message(delta: &str) {
    MESSAGES.with(|m| {
        if let Some(model) = m.borrow().as_ref() {
            let len = model.row_count();
            if len > 0 {
                let mut last = model.row_data(len - 1).unwrap();
                if last.role.as_str() == "agent" {
                    let new_text = last.text.as_str().to_string() + delta;
                    last.text = new_text.into();
                    model.set_row_data(len - 1, last);
                }
            }
        }
    });
}

fn finish_last_agent_message() {
    MESSAGES.with(|m| {
        if let Some(model) = m.borrow().as_ref() {
            let len = model.row_count();
            if len > 0 {
                let mut last = model.row_data(len - 1).unwrap();
                if last.role.as_str() == "agent" {
                    // A tool-only turn (Python-agentd path) can end on an agent
                    // bubble that never received a delta — drop it instead of
                    // leaving an empty row in the transcript.
                    if last.streaming && last.text.is_empty() {
                        model.remove(len - 1);
                    } else {
                        last.streaming = false;
                        model.set_row_data(len - 1, last);
                    }
                }
            }
        }
    });
}

fn clear_messages() {
    MESSAGES.with(|m| {
        if let Some(model) = m.borrow().as_ref() {
            while model.row_count() > 0 {
                model.remove(model.row_count() - 1);
            }
        }
    });
    // A fresh transcript should re-stamp on the next exchange.
    LAST_STAMP.with(|c| c.set(0));
}

thread_local! {
    // Epoch (secs) of the last chat time-divider; 0 = none yet this transcript.
    static LAST_STAMP: std::cell::Cell<i64> = const { std::cell::Cell::new(0) };
    // Agent-chosen expression (state, gaze, intensity) from `display_face`, held
    // so it lingers past turn-end instead of snapping back to idle. Cleared when
    // the user sends the next prompt (a fresh exchange). None = no held emote.
    static FACE_HELD: RefCell<Option<(String, String, f32)>> =
        const { RefCell::new(None) };
}

/// Apply an agent emote to the face and remember it as the held expression.
/// Runs on the Slint thread.
fn set_face_emote(ui: &AppWindow, state: &str, gaze: &str, intensity: f32) {
    ui.set_face_state(state.into());
    ui.set_face_gaze(gaze.into());
    ui.set_face_intensity(intensity);
    FACE_HELD.with(|h| *h.borrow_mut() = Some((state.to_string(), gaze.to_string(), intensity)));
}

/// Revert the face after a turn: restore a held agent emote if there is one,
/// else fall back to a calm idle (gaze re-centred, intensity reset).
fn face_rest(ui: &AppWindow) {
    match FACE_HELD.with(|h| h.borrow().clone()) {
        Some((state, gaze, intensity)) => {
            ui.set_face_state(state.into());
            ui.set_face_gaze(gaze.into());
            ui.set_face_intensity(intensity);
        }
        None => {
            ui.set_face_state("idle".into());
            ui.set_face_gaze("center".into());
            ui.set_face_intensity(0.7);
        }
    }
}

/// Drop any held emote — called when the user starts a fresh exchange.
fn clear_face_hold() {
    FACE_HELD.with(|h| *h.borrow_mut() = None);
}

// Drop a centered date/time marker into the chat at the start of an exchange,
// but only once per ~3-minute window so rapid back-and-forth doesn't spam them.
// role="time"; the formatted label rides in `text` (no per-message field, so
// every MessageItem construction site stays untouched). Grounds the thread in
// wall-clock time for both the reader and (later, via agentd) the model.
fn maybe_push_time_divider() {
    let now = chrono::Local::now();
    let epoch = now.timestamp();
    let due = LAST_STAMP.with(|c| {
        let last = c.get();
        last == 0 || epoch - last >= 180
    });
    if !due {
        return;
    }
    LAST_STAMP.with(|c| c.set(epoch));
    push_message(MessageItem {
        role: "time".into(),
        text: now.format("%-d %b %Y, %H:%M").to_string().into(),
        streaming: false,
        call_id: "".into(),
        tool_name: "".into(),
        tool_args: "".into(),
        tool_output: "".into(),
        tool_status: "".into(),
        awaiting_approval: false,
    });
}

// Refresh the Clock global from local wall-clock time (driven by a 1s timer).
fn update_clock(ui: &AppWindow) {
    let now = chrono::Local::now();
    let clock = ui.global::<Clock>();
    clock.set_time(now.format("%H:%M").to_string().into());
    clock.set_date(now.format("%a %-d %b").to_string().into());
}

fn replace_sessions(items: Vec<SessionItem>) {
    SESSIONS.with(|s| {
        if let Some(model) = s.borrow().as_ref() {
            while model.row_count() > 0 {
                model.remove(model.row_count() - 1);
            }
            for item in items {
                model.push(item);
            }
        }
    });
}

/// Session ids currently checked in the SESSIONS model. Slint-thread only
/// (reads the SESSIONS thread-local) — call from a callback handler.
fn selected_session_ids() -> Vec<u64> {
    SESSIONS.with(|s| {
        s.borrow().as_ref().map(|m| {
            (0..m.row_count())
                .filter_map(|i| m.row_data(i))
                .filter(|it| it.selected)
                .map(|it| it.session_id as u64)
                .collect()
        }).unwrap_or_default()
    })
}

/// Uncheck every session row. Slint-thread only.
fn clear_session_selection() {
    SESSIONS.with(|s| {
        if let Some(m) = s.borrow().as_ref() {
            for i in 0..m.row_count() {
                if let Some(mut it) = m.row_data(i) {
                    if it.selected { it.selected = false; m.set_row_data(i, it); }
                }
            }
        }
    });
}

fn replace_models(items: Vec<ModelItem>) {
    MODELS.with(|m| {
        if let Some(model) = m.borrow().as_ref() {
            while model.row_count() > 0 {
                model.remove(model.row_count() - 1);
            }
            for item in items {
                model.push(item);
            }
        }
    });
}

// The un-filtered model catalog for the Settings picker. An OAI backend's /models can
// be huge (OpenRouter: hundreds), so the visible list is a capped/filtered view over
// this cache — `set_models_full` stores + shows the head, `apply_model_filter` narrows.
thread_local! {
    static MODELS_FULL: RefCell<Vec<ModelItem>> = const { RefCell::new(Vec::new()) };
}
const MODELS_VIEW_CAP: usize = 60;

fn set_models_full(items: Vec<ModelItem>) {
    replace_models(items.iter().take(MODELS_VIEW_CAP).cloned().collect());
    MODELS_FULL.with(|f| *f.borrow_mut() = items);
}

fn apply_model_filter(filter: &str) {
    let f = filter.trim().to_lowercase();
    let view: Vec<ModelItem> = MODELS_FULL.with(|full| {
        full.borrow()
            .iter()
            .filter(|m| {
                f.is_empty()
                    || m.model_id.to_lowercase().contains(&f)
                    || m.model_name.to_lowercase().contains(&f)
            })
            .take(MODELS_VIEW_CAP)
            .cloned()
            .collect()
    });
    replace_models(view);
}

fn replace_events(items: Vec<EventLogItem>) {
    EVENTS.with(|e| {
        if let Some(model) = e.borrow().as_ref() {
            while model.row_count() > 0 {
                model.remove(model.row_count() - 1);
            }
            for item in items {
                model.push(item);
            }
        }
    });
}

fn replace_mesh(items: Vec<MeshNode>) {
    MESH.with(|m| {
        if let Some(model) = m.borrow().as_ref() {
            while model.row_count() > 0 {
                model.remove(model.row_count() - 1);
            }
            for item in items {
                model.push(item);
            }
        }
    });
}

// ── Mesh INBOX (per-peer a2a threads) ───────────────────────────────────────────
// Event-driven (the `mesh_message` stream), not HTTP-polled like the roster. The
// unread counts are UI-session-scoped (the messages themselves persist in each
// peer's thread JSONL — only the "since you last looked" count is ephemeral).

/// Epoch seconds (UI-side wall clock) for relative inbox timestamps.
fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// "just now" / "5m ago" / "3h ago" / "2d ago" from a seconds delta.
fn ago_label(delta: i64) -> String {
    let d = delta.max(0);
    if d < 45 {
        "just now".into()
    } else if d < 5_400 {
        format!("{}m ago", (d + 30) / 60)
    } else if d < 86_400 {
        format!("{}h ago", (d + 1_800) / 3_600)
    } else {
        format!("{}d ago", d / 86_400)
    }
}

/// Total unread across inbox threads → the Mesh badge (header pill + start menu).
/// Slint thread only.
fn inbox_refresh_badge() {
    let total: i32 = INBOX.with(|m| {
        m.borrow()
            .as_ref()
            .map(|model| {
                (0..model.row_count())
                    .filter_map(|i| model.row_data(i))
                    .map(|t| t.unread)
                    .sum()
            })
            .unwrap_or(0)
    });
    UI_WEAK.with(|u| {
        if let Some(ui) = u.borrow().as_ref().and_then(|w| w.upgrade()) {
            ui.set_mesh_unread(total);
        }
    });
}

/// A mesh a2a message from `from` (its thread = `session`) arrived: bump that
/// peer's unread, refresh preview/time, float it to the top. Marshals onto the
/// Slint thread (called from the WS receive task).
fn inbox_upsert(from: String, session: i32, preview: String) {
    slint::invoke_from_event_loop(move || {
        INBOX.with(|m| {
            if let Some(model) = m.borrow().as_ref() {
                let existing = (0..model.row_count()).find(|&i| {
                    model.row_data(i).map(|t| t.node_id.as_str() == from).unwrap_or(false)
                });
                let prior_unread =
                    existing.and_then(|i| model.row_data(i)).map(|t| t.unread).unwrap_or(0);
                if let Some(i) = existing {
                    model.remove(i);
                }
                model.insert(
                    0,
                    InboxThread {
                        node_id: from.as_str().into(),
                        preview: preview.as_str().into(),
                        unread: prior_unread + 1,
                        last_seen: ago_label(0).into(),
                        last_ts: now_secs() as i32,
                        session,
                    },
                );
            }
        });
        inbox_refresh_badge();
    })
    .ok();
}

/// User opened the thread for `session` → clear that peer's unread. Slint thread.
fn inbox_clear_session(session: i32) {
    INBOX.with(|m| {
        if let Some(model) = m.borrow().as_ref() {
            for i in 0..model.row_count() {
                if let Some(mut t) = model.row_data(i) {
                    if t.session == session && t.unread != 0 {
                        t.unread = 0;
                        model.set_row_data(i, t);
                    }
                }
            }
        }
    });
    inbox_refresh_badge();
}

/// Re-stamp every thread's relative-time label (called on the 1 s clock tick).
/// Only writes a row when its label actually changes, so most ticks are no-ops.
fn inbox_restamp() {
    INBOX.with(|m| {
        if let Some(model) = m.borrow().as_ref() {
            let now = now_secs();
            for i in 0..model.row_count() {
                if let Some(mut t) = model.row_data(i) {
                    let lbl = ago_label(now - t.last_ts as i64);
                    if t.last_seen.as_str() != lbl.as_str() {
                        t.last_seen = lbl.into();
                        model.set_row_data(i, t);
                    }
                }
            }
        }
    });
}

fn replace_infer_models(items: Vec<ModelItem>) {
    INFER_MODELS.with(|m| {
        if let Some(model) = m.borrow().as_ref() {
            while model.row_count() > 0 {
                model.remove(model.row_count() - 1);
            }
            for item in items {
                model.push(item);
            }
        }
    });
}

fn replace_audio_files(items: Vec<AudioFileItem>) {
    AUDIO_FILES.with(|m| {
        if let Some(model) = m.borrow().as_ref() {
            while model.row_count() > 0 {
                model.remove(model.row_count() - 1);
            }
            for item in items {
                model.push(item);
            }
        }
    });
}

fn replace_waveform(samples: Vec<f32>) {
    WAVEFORM.with(|m| {
        if let Some(model) = m.borrow().as_ref() {
            while model.row_count() > 0 {
                model.remove(model.row_count() - 1);
            }
            for s in samples {
                model.push(s);
            }
        }
    });
}

fn replace_sonus_files(items: Vec<SonusFileItem>) {
    SONUS_FILES.with(|m| {
        if let Some(model) = m.borrow().as_ref() {
            while model.row_count() > 0 {
                model.remove(model.row_count() - 1);
            }
            for item in items {
                model.push(item);
            }
        }
    });
}

fn replace_notes_files(items: Vec<NoteItem>) {
    NOTES_FILES.with(|m| {
        if let Some(model) = m.borrow().as_ref() {
            while model.row_count() > 0 {
                model.remove(model.row_count() - 1);
            }
            for item in items {
                model.push(item);
            }
        }
    });
}

fn replace_workspace_images(items: Vec<ImageItem>) {
    WORKSPACE_IMAGES.with(|m| {
        if let Some(model) = m.borrow().as_ref() {
            while model.row_count() > 0 {
                model.remove(model.row_count() - 1);
            }
            for item in items {
                model.push(item);
            }
        }
    });
}

fn replace_explorer_entries(items: Vec<ExplorerEntry>) {
    EXPLORER_ENTRIES.with(|m| {
        if let Some(model) = m.borrow().as_ref() {
            while model.row_count() > 0 {
                model.remove(model.row_count() - 1);
            }
            for item in items {
                model.push(item);
            }
        }
    });
}

fn replace_drive_candidates(items: Vec<UsbCandidate>) {
    DRIVE_CANDIDATES.with(|m| {
        if let Some(model) = m.borrow().as_ref() {
            while model.row_count() > 0 {
                model.remove(model.row_count() - 1);
            }
            for item in items {
                model.push(item);
            }
        }
    });
}

/// Icon for an Explorer entry — directory or file-by-extension.
fn explorer_glyph(is_dir: bool, ext: &str) -> &'static str {
    if is_dir { return "📁"; }
    match ext {
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" => "🖼",
        "mp3" | "wav" | "flac" | "ogg" | "m4a" | "aac"  => "🎵",
        "md" | "txt" | "log"                            => "📄",
        "json" | "toml" | "yaml" | "yml" | "rs" | "py" | "js" | "sh" | "css" | "html" => "⚙",
        "pdf"                                           => "📕",
        "zip" | "gz" | "tar" | "xz"                     => "🗜",
        _                                               => "📄",
    }
}

/// True when an extension is a previewable raster image (loaded directly from the
/// absolute path — UI + agentd are co-located on the kiosk / desktop).
fn is_image_ext(ext: &str) -> bool {
    matches!(ext, "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp")
}

// ── Sketchpad helpers (run on the Slint thread) ────────────────────────────────

/// Start a new stroke at (x, y) with the current colour/width.
fn sketch_begin_stroke(x: f32, y: f32) {
    let color_idx = SKETCH_COLOR.with(|c| c.get());
    let width_idx = SKETCH_WIDTH.with(|c| c.get());
    let hex = sketch_hex(color_idx).to_string();
    let width = sketch_width_px(width_idx);
    SKETCH_DATA.with(|d| d.borrow_mut().push(StrokeData {
        color_hex: hex,
        width,
        points: vec![(x, y)],
    }));
    SKETCH_STROKES.with(|m| {
        if let Some(model) = m.borrow().as_ref() {
            model.push(SketchStroke {
                commands: format!("M {x} {y}").into(),
                color: sketch_color(color_idx),
                width,
            });
        }
    });
}

/// Extend the in-progress stroke to (x, y).
fn sketch_extend_stroke(x: f32, y: f32) {
    SKETCH_DATA.with(|d| {
        if let Some(s) = d.borrow_mut().last_mut() { s.points.push((x, y)); }
    });
    SKETCH_STROKES.with(|m| {
        if let Some(model) = m.borrow().as_ref() {
            let n = model.row_count();
            if n > 0 {
                if let Some(mut row) = model.row_data(n - 1) {
                    row.commands = format!("{} L {x} {y}", row.commands).into();
                    model.set_row_data(n - 1, row);
                }
            }
        }
    });
}

/// Build an SVG polyline command string from a point list.
fn sketch_points_to_commands(points: &[(f32, f32)]) -> String {
    let mut s = String::new();
    for (i, (x, y)) in points.iter().enumerate() {
        if i == 0 { s.push_str(&format!("M {x} {y}")); }
        else      { s.push_str(&format!(" L {x} {y}")); }
    }
    s
}

/// Point list for a shape tool dragged from anchor (ax, ay) to (x, y).
/// tool: 1 line · 2 rectangle · 3 ellipse (else: a single point).
fn sketch_shape_points(tool: i32, ax: f32, ay: f32, x: f32, y: f32) -> Vec<(f32, f32)> {
    match tool {
        1 => vec![(ax, ay), (x, y)],
        2 => vec![(ax, ay), (x, ay), (x, y), (ax, y), (ax, ay)],
        3 => {
            let (cx, cy) = ((ax + x) / 2.0, (ay + y) / 2.0);
            let (rx, ry) = ((x - ax).abs() / 2.0, (y - ay).abs() / 2.0);
            const N: usize = 48;
            (0..=N).map(|i| {
                let t = (i as f32 / N as f32) * std::f32::consts::TAU;
                (cx + rx * t.cos(), cy + ry * t.sin())
            }).collect()
        }
        _ => vec![(x, y)],
    }
}

/// Begin a shape: anchor the drag and seed a one-point stroke.
fn sketch_begin_shape(x: f32, y: f32) {
    SKETCH_ANCHOR.with(|a| a.set((x, y)));
    sketch_begin_stroke(x, y);
}

/// Update the in-progress shape stroke to span anchor → (x, y).
fn sketch_update_shape(x: f32, y: f32) {
    let tool = SKETCH_TOOL.with(|t| t.get());
    let (ax, ay) = SKETCH_ANCHOR.with(|a| a.get());
    let points = sketch_shape_points(tool, ax, ay, x, y);
    let commands = sketch_points_to_commands(&points);
    SKETCH_DATA.with(|d| {
        if let Some(s) = d.borrow_mut().last_mut() { s.points = points; }
    });
    SKETCH_STROKES.with(|m| {
        if let Some(model) = m.borrow().as_ref() {
            let n = model.row_count();
            if n > 0 {
                if let Some(mut row) = model.row_data(n - 1) {
                    row.commands = commands.into();
                    model.set_row_data(n - 1, row);
                }
            }
        }
    });
}

/// Drop all strokes.
fn sketch_clear_all() {
    SKETCH_DATA.with(|d| d.borrow_mut().clear());
    SKETCH_STROKES.with(|m| {
        if let Some(model) = m.borrow().as_ref() {
            while model.row_count() > 0 { model.remove(model.row_count() - 1); }
        }
    });
}

/// Build the /api/sketch JSON body from the captured strokes.
fn sketch_payload(width: f32, height: f32) -> Value {
    let strokes: Vec<Value> = SKETCH_DATA.with(|d| {
        d.borrow().iter().map(|s| serde_json::json!({
            "color": s.color_hex,
            "width": s.width,
            "points": s.points.iter().map(|(x, y)| serde_json::json!({ "x": x, "y": y })).collect::<Vec<_>>(),
        })).collect()
    });
    serde_json::json!({
        "width": width.max(1.0).round() as u32,
        "height": height.max(1.0).round() as u32,
        "bg": "#0d0f18",
        "strokes": strokes,
    })
}

// "#rrggbb" (or "rrggbb") → slint::Color, falling back to off-white.
fn hex_to_color(hex: &str) -> slint::Color {
    let h = hex.trim().trim_start_matches('#');
    let v = u32::from_str_radix(h, 16).ok().filter(|_| h.len() == 6).unwrap_or(0xe6e6eb);
    slint::Color::from_rgb_u8((v >> 16) as u8, (v >> 8) as u8, v as u8)
}

// One agent-drawn stroke, points in NORMALIZED 0-1 space (scaled to canvas px
// when applied). Built off the Slint thread → only Send data.
struct AgentStroke {
    points: Vec<(f32, f32)>,
    color_hex: String,
    width: f32,
}

// Read an [x, y] pair from a JSON array ([x,y]) or object ({x,y}).
fn read_xy(v: &Value) -> Option<(f32, f32)> {
    if let Some(a) = v.as_array() {
        if a.len() >= 2 {
            return Some((a[0].as_f64()? as f32, a[1].as_f64()? as f32));
        }
    }
    Some((v["x"].as_f64()? as f32, v["y"].as_f64()? as f32))
}

// Parse a `sketch_draw` tool call's `strokes` into normalized AgentStrokes.
// Each stroke is a freehand `points` path or a `shape`+`from`+`to` primitive.
// Coords are clamped to 0-1; invalid/empty strokes are dropped.
fn parse_agent_strokes(args: &Value) -> Vec<AgentStroke> {
    let Some(arr) = args["strokes"].as_array() else { return Vec::new() };
    let mut out = Vec::new();
    for s in arr {
        let color = s["color"].as_str().unwrap_or("#e6e6eb").to_string();
        let width = s["width"].as_f64().unwrap_or(3.0).clamp(0.5, 64.0) as f32;
        let pts: Vec<(f32, f32)> = if let Some(shape) = s["shape"].as_str() {
            match (read_xy(&s["from"]), read_xy(&s["to"])) {
                (Some((ax, ay)), Some((bx, by))) => {
                    let tool = match shape { "line" => 1, "rect" => 2, "ellipse" => 3, _ => 0 };
                    sketch_shape_points(tool, ax, ay, bx, by)
                }
                _ => Vec::new(),
            }
        } else if let Some(ps) = s["points"].as_array() {
            ps.iter().filter_map(read_xy).collect()
        } else {
            Vec::new()
        };
        if pts.is_empty() { continue; }
        let pts = pts.into_iter().map(|(x, y)| (x.clamp(0.0, 1.0), y.clamp(0.0, 1.0))).collect();
        out.push(AgentStroke { points: pts, color_hex: color, width });
    }
    out
}

// Reveal (or focus) the Sketchpad window so the human watches APEX draw.
fn reveal_sketchpad(ui: &AppWindow) {
    WINDOWS.with(|w| {
        if let Some(model) = w.borrow().as_ref() {
            wm_launch(ui, model, AppKind::Sketchpad);
        }
    });
}

// Apply agent-drawn strokes to the live canvas (same models the user draws into,
// so the existing save path persists a USER+AGENT composite). Returns the
// /api/sketch payload to persist, or None if nothing changed. Slint thread only.
fn apply_agent_sketch(ui: &AppWindow, clear: bool, strokes: &[AgentStroke]) -> Option<Value> {
    if clear { sketch_clear_all(); }
    let (cw, ch) = SKETCH_CANVAS.with(|c| c.get());
    let (cw, ch) = (cw.max(1.0), ch.max(1.0));
    let mut added = 0;
    for st in strokes {
        let px: Vec<(f32, f32)> = st.points.iter().map(|(x, y)| (x * cw, y * ch)).collect();
        let commands = sketch_points_to_commands(&px);
        let color = hex_to_color(&st.color_hex);
        SKETCH_DATA.with(|d| d.borrow_mut().push(StrokeData {
            color_hex: st.color_hex.clone(),
            width: st.width,
            points: px,
        }));
        SKETCH_STROKES.with(|m| {
            if let Some(model) = m.borrow().as_ref() {
                model.push(SketchStroke { commands: commands.into(), color, width: st.width });
            }
        });
        added += 1;
    }
    if added == 0 && !clear { return None; }
    reveal_sketchpad(ui);
    Some(sketch_payload(cw, ch))
}

fn find_tool_row(call_id: &str) -> Option<usize> {
    MESSAGES.with(|m| {
        if let Some(model) = m.borrow().as_ref() {
            // Scan newest-first: agentd's ActionIds are globally unique now,
            // but a transcript from an older daemon (or a restored replay) can
            // hold duplicate call ids from the per-turn-counter era — the live
            // event always belongs to the NEWEST matching card, never a twin
            // far up the chat.
            for i in (0..model.row_count()).rev() {
                if let Some(item) = model.row_data(i) {
                    if item.role.as_str() == "tool" && item.call_id.as_str() == call_id {
                        return Some(i);
                    }
                }
            }
        }
        None
    })
}

fn update_tool_row(row: usize, f: impl FnOnce(&mut MessageItem)) {
    MESSAGES.with(|m| {
        if let Some(model) = m.borrow().as_ref() {
            if let Some(mut item) = model.row_data(row) {
                f(&mut item);
                model.set_row_data(row, item);
            }
        }
    });
}

/// On cancel, retire any tool cards still awaiting approval (or running) so they
/// don't hang in the chat — agentd aborts the turn but emits no TurnComplete.
fn clear_pending_tools() {
    MESSAGES.with(|m| {
        if let Some(model) = m.borrow().as_ref() {
            for i in 0..model.row_count() {
                if let Some(mut item) = model.row_data(i) {
                    if item.role.as_str() == "tool"
                        && (item.awaiting_approval || item.tool_status.as_str() == "running")
                    {
                        item.awaiting_approval = false;
                        item.tool_status = "error".into();
                        if item.tool_output.as_str().is_empty() {
                            item.tool_output = "cancelled".into();
                        }
                        model.set_row_data(i, item);
                    }
                }
            }
        }
    });
}

// ── SysStats helpers ──────────────────────────────────────────────────────────

fn empty_sys_stats() -> SysStats {
    SysStats {
        cpu_pct:       0.0,
        ram_pct:       0.0,
        disk_pct:      0.0,
        iaq_score:     0.0,
        iaq_label:     "—".into(),
        temp_c:        0.0,
        humidity_pct:  0.0,
        online:        false,
        thermal_min_c:  0.0,
        thermal_max_c:  0.0,
        thermal_mean_c: 0.0,
        thermal_active: false,
    }
}

fn iaq_label(score: f32) -> &'static str {
    match score as u32 {
        0..=50   => "Good",
        51..=100 => "Moderate",
        101..=150 => "Unhealthy (Sensitive)",
        151..=200 => "Unhealthy",
        201..=300 => "Very Unhealthy",
        _         => "Hazardous",
    }
}

// Derive HTTP base from WS URL: "ws://host:port/ws" → "http://host:port"
/// Extract the bare host from an http(s) base URL (drops scheme, port, path).
/// "http://192.168.0.158:8787" → "192.168.0.158".
fn web_host(base: &str) -> String {
    let no_scheme = base.split("://").nth(1).unwrap_or(base);
    let host_port = no_scheme.split('/').next().unwrap_or(no_scheme);
    host_port.rsplit_once(':').map(|(h, _)| h).unwrap_or(host_port).to_string()
}

fn ws_to_http(ws_url: &str) -> String {
    // Strip any query string first (e.g. "?token=…" appended for WS auth),
    // otherwise the trailing "/ws" is no longer at the end and survives,
    // producing a malformed REST base ("http://host/ws?token=…/api/…").
    ws_url
        .split('?').next().unwrap_or(ws_url)
        .trim_end_matches("/ws")
        .replacen("ws://", "http://", 1)
        .replacen("wss://", "https://", 1)
}

/// Render a WS URL for logging with any `token=` query value masked. Session
/// tokens must never land in terminal scrollback or log files — found live on
/// the first desktop-Pi node: a launch log saved into `~/Public/` carried the
/// full minted token from the post-login connect line.
fn redact_ws_url(url: &str) -> String {
    match url.split_once("token=") {
        Some((head, _)) => format!("{head}token=<redacted>"),
        None => url.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::{ws_to_http, ironbow, build_thermal_image, parse_agent_strokes};
    use super::{kind_from_ordinal, kind_from_slug, kind_ordinal, kind_slug, APP_TABLE};
    use super::redact_ws_url;
    use super::{restore_geom, GeomRec, GEOM_MIN_H, GEOM_MIN_W};

    #[test]
    fn restore_geom_clamps_into_live_area() {
        // Remembered on a big display, restored on a smaller one: size caps to
        // the area, position pulls fully on-stage.
        let rec = GeomRec { x: 1700.0, y: 950.0, w: 900.0, h: 700.0, maximized: false };
        let (x, y, w, h) = restore_geom(rec, 1024.0, 600.0);
        assert_eq!((w, h), (900.0_f32.min(1024.0), 600.0));
        assert_eq!(x, 1024.0 - w);
        assert_eq!(y, 0.0); // h fills the area → y clamps to 0
        // Negative coords (window parked off the left edge) come back on-stage.
        let rec = GeomRec { x: -400.0, y: -50.0, w: 300.0, h: 200.0, maximized: false };
        assert_eq!(restore_geom(rec, 1024.0, 600.0), (0.0, 0.0, 300.0, 200.0));
        // A shape already inside the area is untouched.
        let rec = GeomRec { x: 40.0, y: 30.0, w: 500.0, h: 400.0, maximized: true };
        assert_eq!(restore_geom(rec, 1024.0, 600.0), (40.0, 30.0, 500.0, 400.0));
    }

    #[test]
    fn restore_geom_floors_sizes_and_never_invents_an_edge_when_area_dead() {
        // Pre-first-frame / broken backend: area reads 0×0. Sizes floor to the
        // frame minimums, x/y stay non-negative, and nothing clamps against a
        // fictional right/bottom edge.
        let rec = GeomRec { x: -10.0, y: 5000.0, w: 10.0, h: 10.0, maximized: false };
        let (x, y, w, h) = restore_geom(rec, 0.0, 0.0);
        assert_eq!((x, y), (0.0, 5000.0));
        assert_eq!((w, h), (GEOM_MIN_W, GEOM_MIN_H));
    }

    #[test]
    fn reflex_table_install_updates_caps_and_removes() {
        use super::{reflex_table_install, reflex_table_remove, ReflexRec, REFLEX_MAX};
        let mut t: Vec<ReflexRec> = Vec::new();
        assert!(reflex_table_install(&mut t, "error", "open", "event-log", REFLEX_MAX));
        assert!(reflex_table_install(&mut t, "mesh_message", "open", "mesh", REFLEX_MAX));
        assert_eq!(t.len(), 2);
        // Same (on, app) updates in place — action swaps, ledger resets, no new row.
        t[0].fires = 5;
        assert!(reflex_table_install(&mut t, "error", "focus", "event-log", REFLEX_MAX));
        assert_eq!(t.len(), 2);
        assert_eq!(t[0].action, "focus");
        assert_eq!(t[0].fires, 0);
        // The cap rejects only NEW keys.
        for i in 0..REFLEX_MAX {
            reflex_table_install(&mut t, "wake_triggered", "open", &format!("app{i}"), REFLEX_MAX);
        }
        assert_eq!(t.len(), REFLEX_MAX);
        assert!(!reflex_table_install(&mut t, "wake_triggered", "open", "one-more", REFLEX_MAX));
        assert!(reflex_table_install(&mut t, "error", "close", "event-log", REFLEX_MAX)); // update still fine
        // Remove is keyed the same way.
        assert!(reflex_table_remove(&mut t, "mesh_message", "mesh"));
        assert!(!reflex_table_remove(&mut t, "mesh_message", "mesh"));
        assert_eq!(t.len(), REFLEX_MAX - 1);
    }

    #[test]
    fn reflex_rec_serde_uses_do_and_skips_runtime_state() {
        use super::ReflexRec;
        let rec = ReflexRec {
            on: "mesh_message".into(),
            action: "open".into(),
            app: "mesh".into(),
            fires: 3,
            last_fired: Some(std::time::Instant::now()),
        };
        let json = serde_json::to_string(&rec).unwrap();
        // The wire/file field is `do` (the tool arg name), and the cooldown
        // stamp never persists.
        assert!(json.contains("\"do\":\"open\""));
        assert!(!json.contains("last_fired"));
        let back: ReflexRec = serde_json::from_str(&json).unwrap();
        assert_eq!(back.action, "open");
        assert_eq!(back.fires, 3);
        assert!(back.last_fired.is_none());
        // A hand-trimmed file without `fires` still loads.
        let old: ReflexRec = serde_json::from_str(
            r#"{"on":"error","do":"open","app":"event-log"}"#,
        )
        .unwrap();
        assert_eq!(old.fires, 0);
    }

    #[test]
    fn reflex_triggers_are_the_locked_global_set() {
        use super::{REFLEX_ACTIONS, REFLEX_TRIGGERS};
        // Mirrors apexos-tools UI_REFLEX_TRIGGERS/UI_REFLEX_ACTIONS — a drift
        // means the tool accepts a trigger the shell never fires (or vice
        // versa). Change BOTH crates together, and only additively.
        assert_eq!(
            REFLEX_TRIGGERS,
            &[
                "sensor_alert", "wake_triggered", "mesh_message",
                "mesh_node_status", "goal_state_changed", "council_started",
                "evolution_proposed", "error",
            ]
        );
        assert_eq!(REFLEX_ACTIONS, &["open", "focus", "close"]);
    }

    #[test]
    fn geom_rec_json_roundtrips_and_tolerates_missing_maximized() {
        let rec = GeomRec { x: 12.5, y: 30.0, w: 640.0, h: 480.0, maximized: true };
        let json = serde_json::to_string(&rec).unwrap();
        let back: GeomRec = serde_json::from_str(&json).unwrap();
        assert!(back == rec);
        // A pre-maximized-era file (or hand-trimmed one) still loads.
        let old: GeomRec =
            serde_json::from_str(r#"{"x":1.0,"y":2.0,"w":300.0,"h":200.0}"#).unwrap();
        assert!(!old.maximized);
    }

    #[test]
    fn redact_masks_the_token_and_only_the_token() {
        assert_eq!(
            redact_ws_url("ws://localhost:8787/ws?token=a8237939428c"),
            "ws://localhost:8787/ws?token=<redacted>"
        );
        assert_eq!(
            redact_ws_url("ws://host:8787/terminal-ws?token=abc"),
            "ws://host:8787/terminal-ws?token=<redacted>"
        );
        // Token-less URLs pass through byte-identical.
        assert_eq!(redact_ws_url("ws://localhost:8787/ws"), "ws://localhost:8787/ws");
    }

    #[test]
    fn app_table_is_the_ordinal_order() {
        // APP_TABLE index IS the AppKind ordinal — the adaptive-UI verbs route
        // through invoke_launch_app(ordinal), so a drift between the table and
        // kind_from_ordinal would open the wrong app.
        for (i, (kind, slug)) in APP_TABLE.iter().enumerate() {
            assert_eq!(kind_from_ordinal(i as i32), *kind, "ordinal {i} ({slug}) drifted");
            assert_eq!(kind_ordinal(*kind), i as i32);
            assert_eq!(kind_from_slug(slug), Some(*kind));
            assert_eq!(kind_slug(*kind), *slug);
        }
        assert_eq!(APP_TABLE.len(), 20);
        // Slugs fit the u32 latch bitmasks and stay unique.
        assert!(APP_TABLE.len() <= 32);
        let mut slugs: Vec<_> = APP_TABLE.iter().map(|(_, s)| *s).collect();
        slugs.sort_unstable();
        slugs.dedup();
        assert_eq!(slugs.len(), APP_TABLE.len(), "duplicate slug in APP_TABLE");
        // Unknown slugs are inexpressible.
        assert_eq!(kind_from_slug("xterm"), None);
        assert_eq!(kind_from_slug(""), None);
    }

    #[test]
    fn arrange_rects_presets() {
        use super::{arrange_rects, ARRANGE_GAP};
        let (aw, ah) = (1200.0, 700.0);
        let g = ARRANGE_GAP;

        // focus: exactly ONE near-full rect regardless of n (applier minimizes the rest).
        let f = arrange_rects("focus", 4, aw, ah);
        assert_eq!(f.len(), 1);
        assert_eq!(f[0], (g, g, aw - 2.0 * g, ah - 2.0 * g));

        // Any layout with one window degrades to the full rect.
        assert_eq!(arrange_rects("split", 1, aw, ah), f);
        assert_eq!(arrange_rects("grid", 1, aw, ah), f);

        // split: n equal columns, left→right, tiling the width exactly.
        let s = arrange_rects("split", 2, aw, ah);
        assert_eq!(s.len(), 2);
        assert_eq!(s[0].2, s[1].2);
        assert!(s[0].0 < s[1].0);
        let right_edge = s[1].0 + s[1].2;
        assert!((right_edge - (aw - g)).abs() < 0.5, "columns must fill the width");

        // main-side: first pane is the wide one; sides stack in one right column.
        let m = arrange_rects("main-side", 3, aw, ah);
        assert_eq!(m.len(), 3);
        assert!(m[0].2 > m[1].2, "main pane must be wider than the side panes");
        assert_eq!(m[1].0, m[2].0, "side panes share the same column");
        assert!(m[1].1 < m[2].1, "side panes stack top→down");
        assert_eq!(m[0].3, ah - 2.0 * g, "main pane spans the full height");

        // grid: 4 → 2×2, uniform cells, row-major.
        let gr = arrange_rects("grid", 4, aw, ah);
        assert_eq!(gr.len(), 4);
        assert_eq!(gr[0].1, gr[1].1, "first row shares y");
        assert!(gr[2].1 > gr[0].1, "second row below the first");
        assert_eq!(gr[0].0, gr[2].0, "columns align");
        assert_eq!(gr[0].2, gr[3].2, "uniform cell width");

        // Order is priority order: rect[0] is always the main slot.
        for layout in ["split", "main-side", "grid"] {
            let r = arrange_rects(layout, 3, aw, ah);
            assert_eq!(r.len(), 3, "{layout} must place every participant");
            assert_eq!((r[0].0, r[0].1), (g, g), "{layout}: main slot sits top-left");
        }

        // Degenerate inputs stay sane: no rects for n=0 / unknown layout;
        // a tiny area clamps instead of going negative.
        assert!(arrange_rects("split", 0, aw, ah).is_empty());
        assert!(arrange_rects("cascade", 3, aw, ah).is_empty());
        assert!(arrange_rects("cascade", 1, aw, ah).is_empty());
        let tiny = arrange_rects("grid", 4, 50.0, 40.0);
        assert!(tiny.iter().all(|r| r.2 > 0.0 && r.3 > 0.0));
    }

    #[test]
    fn ironbow_spans_black_to_white() {
        assert_eq!(ironbow(0.0), (0, 0, 0));         // coldest → black
        assert_eq!(ironbow(1.0), (255, 255, 255));   // hottest → white
        assert_eq!(ironbow(-5.0), (0, 0, 0));        // clamped
        assert_eq!(ironbow(9.0), (255, 255, 255));   // clamped
        let (r, g, b) = ironbow(0.55);               // mid → warm (red-ish, non-grey)
        assert!(r > g && r > b);
    }

    #[test]
    fn build_thermal_image_is_32x24_and_ranges() {
        // Too-short frame → None.
        assert!(build_thermal_image(&[20.0; 100]).is_none());
        // A real-size frame yields a 32×24 image; uniform input doesn't panic on /0 range.
        let img = build_thermal_image(&[25.0_f32; 768]).expect("image");
        assert_eq!(img.size().width, 32);
        assert_eq!(img.size().height, 24);
    }

    #[test]
    fn agent_strokes_parse_points_and_clamp() {
        // A freehand path; out-of-range coords clamp into 0-1.
        let strokes = parse_agent_strokes(&json!({
            "strokes": [{ "points": [[0.1, 0.2], [1.5, -0.3]], "color": "#39ff14", "width": 4 }]
        }));
        assert_eq!(strokes.len(), 1);
        assert_eq!(strokes[0].color_hex, "#39ff14");
        assert_eq!(strokes[0].width, 4.0);
        assert_eq!(strokes[0].points, vec![(0.1, 0.2), (1.0, 0.0)]);
    }

    #[test]
    fn agent_strokes_expand_shapes() {
        // A line shape → 2 points; a rect → 5 (closed); ellipse → many.
        let parsed = parse_agent_strokes(&json!({
            "strokes": [
                { "shape": "line", "from": [0.0, 0.0], "to": [1.0, 1.0] },
                { "shape": "rect", "from": [0.2, 0.2], "to": [0.8, 0.8] },
                { "shape": "ellipse", "from": [0.1, 0.1], "to": [0.9, 0.9] }
            ]
        }));
        assert_eq!(parsed.len(), 3);
        assert_eq!(parsed[0].points.len(), 2);
        assert_eq!(parsed[1].points.len(), 5);
        assert!(parsed[2].points.len() > 5);
        // Default colour/width when omitted.
        assert_eq!(parsed[0].color_hex, "#e6e6eb");
        assert_eq!(parsed[0].width, 3.0);
    }

    #[test]
    fn agent_strokes_drop_invalid_and_accept_xy_objects() {
        // No points + no complete shape → dropped; {x,y} object form accepted.
        let parsed = parse_agent_strokes(&json!({
            "strokes": [
                { "color": "#fff" },                                  // dropped: empty
                { "shape": "line", "from": [0.0, 0.0] },              // dropped: no `to`
                { "points": [{ "x": 0.5, "y": 0.5 }, { "x": 0.6, "y": 0.7 }] }
            ]
        }));
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].points, vec![(0.5, 0.5), (0.6, 0.7)]);
    }

    #[test]
    fn rest_base_strips_token_query_and_ws_suffix() {
        // Regression: with AGENTD_TOKEN set the WS URL carries "?token=…",
        // which used to leave "/ws" mid-string so the REST base was mangled.
        assert_eq!(
            ws_to_http("ws://192.168.0.158:8787/ws?token=abc123"),
            "http://192.168.0.158:8787"
        );
        // No token (default) still works.
        assert_eq!(ws_to_http("ws://localhost:8787/ws"), "http://localhost:8787");
        // TLS scheme + token.
        assert_eq!(ws_to_http("wss://host:8787/ws?token=x"), "https://host:8787");
    }

    // ── Occipital follow-along reader (Phase 9) ─────────────────────────────
    use super::{occipital_payload, strip_inline_md, parse_reader_markdown, build_occipital_render};
    use serde_json::json;

    #[test]
    fn occipital_payload_recovers_from_every_transport_shape() {
        let obj = json!({"kind": "page", "url": "https://x", "markdown": "# hi"});
        // 1. Bare object.
        assert!(occipital_payload(&obj).is_some());
        // 2. A JSON string.
        assert!(occipital_payload(&json!(obj.to_string())).is_some());
        // 3. The MCP content array agentd actually delivers (mcp.rs).
        let mcp = json!([{ "type": "text", "text": obj.to_string() }]);
        assert!(occipital_payload(&mcp).is_some());
        // Non-occipital tool output is ignored.
        assert!(occipital_payload(&json!({"ok": true, "content": "hello"})).is_none());
        assert!(occipital_payload(&json!([{ "type": "text", "text": "{\"foo\":1}" }])).is_none());
    }

    #[test]
    fn strip_inline_md_cleans_links_and_emphasis() {
        assert_eq!(strip_inline_md("see [the docs](https://x/y) now"), "see the docs now");
        assert_eq!(strip_inline_md("**bold** and *italic* and `code`"), "bold and italic and code");
        assert_eq!(strip_inline_md("![a cat](https://x/c.png)"), "🖼 a cat");
        // Underscores in identifiers survive (Occipital emits * for emphasis, not _).
        assert_eq!(strip_inline_md("call foo_bar_baz()"), "call foo_bar_baz()");
        // A literal bracket pair that isn't a link keeps its brackets.
        assert_eq!(strip_inline_md("array[0] value"), "array[0] value");
    }

    #[test]
    fn parse_reader_markdown_classifies_blocks() {
        let md = "# Title\n\nA para with **bold**.\n\n## Section\n\n- one\n- two\n\n> a quote\n\n```\ncode line\n```\n\n---\n";
        let blocks = parse_reader_markdown(md);
        let kinds: Vec<&str> = blocks.iter().map(|(k, _, _)| k.as_str()).collect();
        assert_eq!(kinds, ["h1", "p", "h2", "bullet", "bullet", "quote", "code", "rule"]);
        assert_eq!(blocks[0].1, "Title");
        assert_eq!(blocks[1].1, "A para with bold.");   // emphasis stripped
        assert_eq!(blocks[6].1, "code line");           // code body verbatim
    }

    #[test]
    fn build_occipital_render_per_mode() {
        // results → live badge + ranked rows
        let r = build_occipital_render(&json!({
            "kind": "results", "query": "q", "provider": "duckduckgo", "from_cache": false,
            "results": [{"title": "T", "url": "https://a", "snippet": "s", "rank": 0}]
        }));
        assert_eq!(r.mode, "results");
        assert_eq!(r.badge, "live");
        assert_eq!(r.links[0].3, "#1");                 // 1-based rank chip

        // recall → cosine score vs keyword fallback, no fetch badge
        let r = build_occipital_render(&json!({
            "kind": "recall", "query": "q",
            "hits": [
                {"url": "https://a", "title": "A", "snippet": "s", "score": 0.83},
                {"url": "https://b", "title": "B", "snippet": "s", "score": null}
            ]
        }));
        assert_eq!(r.mode, "recall");
        assert_eq!(r.badge, "");
        assert_eq!(r.links[0].3, "0.83");
        assert_eq!(r.links[1].3, "kw");

        // page (cached) → parsed blocks + page links
        let r = build_occipital_render(&json!({
            "kind": "page", "url": "https://x", "title": "X", "from_cache": true,
            "markdown": "# X\n\nbody", "links": [{"text": "next", "url": "https://n"}]
        }));
        assert_eq!(r.mode, "page");
        assert_eq!(r.badge, "cache");
        assert_eq!(r.title, "X");
        assert!(r.blocks.iter().any(|(k, t, _)| k == "h1" && t == "X"));
        assert_eq!(r.links[0].0, "next");
    }
}

fn format_time_ago(unix_secs: u64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let diff = now.saturating_sub(unix_secs);
    match diff {
        0..=59       => "just now".into(),
        60..=3599    => format!("{} min ago", diff / 60),
        3600..=86399 => format!("{} hr ago", diff / 3600),
        _            => format!("{} days ago", diff / 86400),
    }
}

// Parse agentd session history (Anthropic API format) into MessageItems.
// Two-pass: collect tool outputs first, then build items in order.
fn replay_history(history: &[Value]) -> Vec<MessageItem> {
    // Pass 1: collect tool_result outputs keyed by tool_use_id
    let mut tool_outputs: std::collections::HashMap<String, String> = Default::default();
    for msg in history {
        if msg["role"].as_str() != Some("user") { continue; }
        if let Some(content) = msg["content"].as_array() {
            for block in content {
                if block["type"].as_str() != Some("tool_result") { continue; }
                let id = block["tool_use_id"].as_str().unwrap_or("").to_string();
                let output = match &block["content"] {
                    Value::String(s) => s.clone(),
                    Value::Array(arr) => arr.iter()
                        .filter(|b| b["type"].as_str() == Some("text"))
                        .filter_map(|b| b["text"].as_str())
                        .collect::<Vec<_>>()
                        .join("\n"),
                    v => v.to_string(),
                };
                tool_outputs.insert(id, output);
            }
        }
    }

    // Pass 2: build MessageItems in conversation order
    let mut items = Vec::new();
    for msg in history {
        match msg["role"].as_str() {
            Some("user") => {
                if let Some(content) = msg["content"].as_array() {
                    for block in content {
                        if block["type"].as_str() == Some("text") {
                            let text = block["text"].as_str().unwrap_or("").to_string();
                            if !text.is_empty() {
                                items.push(MessageItem {
                                    role: "user".into(), text: text.into(), streaming: false,
                                    call_id: "".into(), tool_name: "".into(),
                                    tool_args: "".into(), tool_output: "".into(),
                                    tool_status: "".into(), awaiting_approval: false,
                                });
                            }
                        }
                        // tool_result blocks handled via tool_outputs map — skip here
                    }
                }
            }
            Some("assistant") => {
                if let Some(content) = msg["content"].as_array() {
                    // Collect text across all text blocks in this message
                    let text: String = content.iter()
                        .filter(|b| b["type"].as_str() == Some("text"))
                        .filter_map(|b| b["text"].as_str())
                        .collect::<Vec<_>>()
                        .join("");
                    if !text.is_empty() {
                        items.push(MessageItem {
                            role: "agent".into(), text: text.into(), streaming: false,
                            call_id: "".into(), tool_name: "".into(),
                            tool_args: "".into(), tool_output: "".into(),
                            tool_status: "".into(), awaiting_approval: false,
                        });
                    }
                    // Tool-use blocks become tool cards (with output filled in)
                    for block in content {
                        if block["type"].as_str() != Some("tool_use") { continue; }
                        let id    = block["id"].as_str().unwrap_or("").to_string();
                        let name  = block["name"].as_str().unwrap_or("").to_string();
                        let args  = block["input"].as_object()
                            .map(|o| serde_json::to_string_pretty(o).unwrap_or_default())
                            .unwrap_or_default();
                        let output = tool_outputs.get(&id).cloned().unwrap_or_default();
                        items.push(MessageItem {
                            role: "tool".into(), text: "".into(), streaming: false,
                            call_id: id.into(), tool_name: name.into(),
                            tool_args: args.into(), tool_output: output.into(),
                            tool_status: "done".into(), awaiting_approval: false,
                        });
                    }
                }
            }
            _ => {}
        }
    }
    items
}

// GET /api/sessions → Vec<SessionItem> sorted newest-first.
async fn fetch_sessions(client: &reqwest::Client, base_url: &str) -> Vec<SessionItem> {
    let resp = match client
        .get(format!("{base_url}/api/sessions"))
        .timeout(std::time::Duration::from_secs(8))
        .send()
        .await
    {
        Ok(r) => r,
        Err(_) => return vec![],
    };
    let body: Value = match resp.json().await {
        Ok(v) => v,
        Err(_) => return vec![],
    };
    let arr = match body.as_array() {
        Some(a) => a,
        None => return vec![],
    };
    arr.iter().map(|item| SessionItem {
        session_id:    item["session_id"].as_u64().unwrap_or(0) as i32,
        time_ago:      format_time_ago(item["last_active"].as_u64().unwrap_or(0)).into(),
        message_count: item["message_count"].as_u64().unwrap_or(0) as i32,
        preview:       item["preview"].as_str().unwrap_or("").into(),
        selected:      false,
    }).collect()
}

// POST /api/sessions/export — export sessions to workspace/exports/, then toast.
// `body` is `{ids:[…]}` (selected) or `{all:true}`; format defaults to markdown.
async fn export_sessions(client: &reqwest::Client, base_url: &str, body: Value) {
    match client
        .post(format!("{base_url}/api/sessions/export"))
        .json(&body)
        .timeout(std::time::Duration::from_secs(20))
        .send()
        .await
    {
        Ok(r) => {
            let v: Value = r.json().await.unwrap_or_default();
            if v["ok"].as_bool().unwrap_or(false) {
                let n = v["count"].as_u64().unwrap_or(0);
                notify(ToastKind::Success, format!("Exported {n} session(s) → workspace/exports/"));
            } else {
                notify(ToastKind::Warn,
                    format!("Export failed: {}", v["error"].as_str().unwrap_or("nothing exported")));
            }
        }
        Err(e) => notify(ToastKind::Error, format!("Export error: {e}")),
    }
}

// POST /api/sessions/{id}/consolidate — distil a session into cerebro. Returns
// whether it succeeded (the endpoint replies 200 with {ok:bool}; the LLM summary
// can take a while, hence the generous timeout).
async fn consolidate_one(client: &reqwest::Client, base_url: &str, id: u64) -> bool {
    match client
        .post(format!("{base_url}/api/sessions/{id}/consolidate"))
        .timeout(std::time::Duration::from_secs(130))
        .send()
        .await
    {
        Ok(r)  => r.json::<Value>().await.ok().and_then(|v| v["ok"].as_bool()).unwrap_or(false),
        Err(_) => false,
    }
}

// DELETE /api/sessions/{id} — returns whether the transcript was actually removed
// (checks body `ok`, not just status — root 0 is refused with 200 + ok:false).
async fn delete_one(client: &reqwest::Client, base_url: &str, id: u64) -> bool {
    match client
        .delete(format!("{base_url}/api/sessions/{id}"))
        .timeout(std::time::Duration::from_secs(8))
        .send()
        .await
    {
        Ok(r)  => r.json::<Value>().await.ok().and_then(|v| v["ok"].as_bool()).unwrap_or(false),
        Err(_) => false,
    }
}

// Client-side mic capture. Like client-side TTS, the UI records in the user's
// session (a local `arecord`), so it reaches the mic — unlike agentd's server-side
// /api/record/* (the sandboxed agentd user can't reach a desktop's PipeWire). The
// captured WAV is POSTed to /api/transcribe (which runs the STT backend plan).
const MIC_WAV: &str = "/tmp/apex_mic_capture.wav";

fn mic_recorder() -> &'static std::sync::Mutex<Option<tokio::process::Child>> {
    static R: std::sync::OnceLock<std::sync::Mutex<Option<tokio::process::Child>>> =
        std::sync::OnceLock::new();
    R.get_or_init(|| std::sync::Mutex::new(None))
}

/// Start a local arecord capturing 16 kHz mono WAV. Returns true on spawn success.
/// Capture device defaults to ALSA "default" (the user session's mic); override with
/// ALSA_CAPTURE_DEVICE. A 120 s cap guards against a forgotten recording.
fn mic_record_start() -> bool {
    if let Ok(mut g) = mic_recorder().lock() {
        if let Some(mut c) = g.take() {
            let _ = c.start_kill();
        }
    }
    let _ = std::fs::remove_file(MIC_WAV);
    let mut cmd = tokio::process::Command::new("arecord");
    if let Ok(dev) = std::env::var("ALSA_CAPTURE_DEVICE") {
        if !dev.trim().is_empty() {
            cmd.args(["-D", &dev]);
        }
    }
    cmd.args(["-q", "-f", "S16_LE", "-r", "16000", "-c", "1", "-d", "120", MIC_WAV]);
    cmd.kill_on_drop(true);
    match cmd.spawn() {
        Ok(child) => {
            if let Ok(mut g) = mic_recorder().lock() {
                *g = Some(child);
            }
            true
        }
        Err(_) => false,
    }
}

/// Stop arecord (SIGINT → clean WAV header) and transcribe via /api/transcribe.
async fn mic_stop_and_transcribe(client: &reqwest::Client, base_url: &str) -> String {
    let child = mic_recorder().lock().ok().and_then(|mut g| g.take());
    let Some(mut child) = child else { return String::new() };
    // SIGINT (not kill/SIGKILL) so arecord finalizes the WAV header before exit.
    if let Some(pid) = child.id() {
        unsafe { libc::kill(pid as i32, libc::SIGINT); }
    }
    let _ = child.wait().await;
    let Ok(bytes) = tokio::fs::read(MIC_WAV).await else { return String::new() };
    let _ = tokio::fs::remove_file(MIC_WAV).await;
    if bytes.is_empty() {
        return String::new();
    }
    match client
        .post(format!("{base_url}/api/transcribe"))
        .body(bytes)
        .timeout(std::time::Duration::from_secs(35))
        .send()
        .await
    {
        Ok(resp) => resp
            .json::<Value>()
            .await
            .ok()
            .and_then(|v| v["text"].as_str().map(|s| s.trim().to_string()))
            .unwrap_or_default(),
        Err(_) => String::new(),
    }
}

// Client-side TTS: fetch the synthesized WAV from /api/tts and play it on THIS
// machine's audio. ui-slint runs in the user's session (desktop) or as root with
// ALSA (kiosk), so its `aplay` reaches the local speakers — unlike agentd, which
// runs as the sandboxed `agentd` user and can't reach a desktop's PipeWire session.
// Falls back to server-side /api/speak if the fetch or local playback fails.
async fn speak_text(client: &reqwest::Client, base: &str, text: String) {
    let wav = async {
        let r = client
            .post(format!("{base}/api/tts"))
            .json(&serde_json::json!({ "text": &text }))
            .timeout(std::time::Duration::from_secs(30))
            .send()
            .await
            .ok()?;
        if !r.status().is_success() {
            return None;
        }
        let b = r.bytes().await.ok()?;
        (!b.is_empty()).then(|| b.to_vec())
    }
    .await;

    if let Some(bytes) = wav {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_micros();
        let path = format!("/tmp/apex_tts_{stamp}.wav");
        if tokio::fs::write(&path, &bytes).await.is_ok() {
            let played = tokio::process::Command::new("aplay")
                .args(["-q", &path])
                .output()
                .await
                .map(|o| o.status.success())
                .unwrap_or(false);
            let _ = tokio::fs::remove_file(&path).await;
            if played {
                return;
            }
        }
    }
    // Fallback: server-side playback (kiosk/headless where agentd owns the audio).
    let _ = client
        .post(format!("{base}/api/speak"))
        .json(&serde_json::json!({ "text": text }))
        .timeout(std::time::Duration::from_secs(5))
        .send()
        .await;
}

// Context shared between the WS task and dispatch_event.
struct DispatchCtx {
    rt_handle:   tokio::runtime::Handle,
    http_client: Arc<reqwest::Client>,
    http_base:   String,
    tts_enabled: Arc<AtomicBool>,
}

// GET a URL and parse the JSON body; returns Value::Null on any error.
async fn json_get(client: &reqwest::Client, url: String) -> Value {
    match client.get(&url)
        .timeout(std::time::Duration::from_secs(8))
        .send()
        .await
    {
        Ok(resp) => resp.json::<Value>().await.unwrap_or(Value::Null),
        Err(_)   => Value::Null,
    }
}

struct SettingsData {
    soul_text:     String,
    policy_mode:   String,
    current_model: String,
    api_key_set:   bool,
    models:        Vec<ModelItem>,
    cache_enabled:      bool,
    cache_conversation: bool,
    cache_ttl:          String,
    sensor_profile:     String,
    voice_backend:        String,
    voice_api_available:  bool,
    backend:       String,
    oai_base_url:  String,
    oai_key_set:   bool,
}

// Fetch /api/status, /api/soul, /api/models, /api/cache, /api/sensors/config, /api/voice,
// /api/backend in parallel.
async fn fetch_settings(client: &reqwest::Client, base_url: &str) -> SettingsData {
    let (status, soul, models_resp, cache, sensors, voice, backend) = tokio::join!(
        json_get(client, format!("{base_url}/api/status")),
        json_get(client, format!("{base_url}/api/soul")),
        json_get(client, format!("{base_url}/api/models")),
        json_get(client, format!("{base_url}/api/cache")),
        json_get(client, format!("{base_url}/api/sensors/config")),
        json_get(client, format!("{base_url}/api/voice")),
        json_get(client, format!("{base_url}/api/backend")),
    );
    let models: Vec<ModelItem> = models_resp["models"]
        .as_array()
        .unwrap_or(&vec![])
        .iter()
        .map(|m| ModelItem {
            model_id:   m["id"].as_str().unwrap_or("").into(),
            model_name: m["name"].as_str().unwrap_or("").into(),
        })
        .collect();
    SettingsData {
        soul_text:     soul["content"].as_str().unwrap_or("").to_string(),
        policy_mode:   status["policy_mode"].as_str().unwrap_or("suggest").to_string(),
        current_model: status["model"].as_str().unwrap_or("").to_string(),
        api_key_set:   status["api_key_set"].as_bool().unwrap_or(false),
        models,
        // Defaults (caching on, 5m) if agentd predates /api/cache.
        cache_enabled:      cache["enabled"].as_bool().unwrap_or(true),
        cache_conversation: cache["cache_conversation"].as_bool().unwrap_or(true),
        cache_ttl:          cache["ttl"].as_str().unwrap_or("5m").to_string(),
        sensor_profile:     sensors["profile"].as_str().unwrap_or("standard").to_string(),
        voice_backend:       voice["voice_backend"].as_str().unwrap_or("auto").to_string(),
        voice_api_available: voice["has_elevenlabs"].as_bool().unwrap_or(false)
            || voice["has_openai"].as_bool().unwrap_or(false),
        backend:      backend["backend"].as_str().unwrap_or("anthropic").to_string(),
        oai_base_url: backend["oai_base_url"].as_str().unwrap_or("").to_string(),
        oai_key_set:  backend["oai_key_set"].as_bool().unwrap_or(false),
    }
}

// POST /api/run to fetch CPU / RAM / disk percentages from the server.
// Returns (cpu_pct, ram_pct, disk_pct) on success.
async fn fetch_sys_stats(client: &reqwest::Client, base_url: &str) -> Option<(f32, f32, f32)> {
    // One command: mem_pct on line 1, disk_pct on line 2, nproc on line 3, load_1m on line 4
    let cmd = concat!(
        "awk '/^MemTotal/{t=$2}/^MemAvailable/{a=$2}END{printf \"%.0f\\n\",100*(t-a)/t}' /proc/meminfo",
        " && df / | awk 'NR==2{gsub(/%/,\"\",$5);print $5}'",
        " && nproc",
        " && awk '{print $1}' /proc/loadavg",
    );
    let resp = client
        .post(format!("{base_url}/api/run"))
        .json(&serde_json::json!({"command": cmd}))
        .timeout(std::time::Duration::from_secs(8))
        .send()
        .await
        .ok()?;
    let body: Value = resp.json().await.ok()?;
    if body["ok"].as_bool() != Some(true) {
        return None;
    }
    let stdout = body["stdout"].as_str()?;
    let lines: Vec<&str> = stdout.lines().collect();
    let ram_pct:  f32 = lines.first()?.trim().parse().ok()?;
    let disk_pct: f32 = lines.get(1)?.trim().parse().ok()?;
    let nproc:    f32 = lines.get(2)?.trim().parse::<f32>().ok()?.max(1.0);
    let loadavg:  f32 = lines.get(3)?.trim().parse().ok()?;
    let cpu_pct = (loadavg / nproc * 100.0).min(100.0);
    Some((cpu_pct, ram_pct, disk_pct))
}

// ── Thermal heatmap (MLX90640) ──────────────────────────────────────────────

/// Ironbow thermal palette: black → purple → magenta → red → orange → yellow →
/// white, piecewise-linear over the stops below.
fn ironbow(t: f32) -> (u8, u8, u8) {
    const STOPS: [(f32, f32, f32, f32); 7] = [
        (0.00,   0.0,   0.0,   0.0),
        (0.15,  40.0,   0.0,  80.0),
        (0.35, 140.0,   0.0, 120.0),
        (0.55, 220.0,  40.0,  40.0),
        (0.75, 255.0, 140.0,   0.0),
        (0.90, 255.0, 230.0,  60.0),
        (1.00, 255.0, 255.0, 255.0),
    ];
    let t = t.clamp(0.0, 1.0);
    for w in STOPS.windows(2) {
        let (t0, r0, g0, b0) = w[0];
        let (t1, r1, g1, b1) = w[1];
        if t <= t1 {
            let f = if (t1 - t0).abs() < 1e-6 { 0.0 } else { (t - t0) / (t1 - t0) };
            return ((r0 + (r1 - r0) * f) as u8, (g0 + (g1 - g0) * f) as u8, (b0 + (b1 - b0) * f) as u8);
        }
    }
    (255, 255, 255)
}

/// Build a 32×24 ironbow image from an MLX90640 frame (≥768 °C floats, row-major),
/// auto-ranged min→max. None if the frame is too short.
fn build_thermal_image(frame: &[f32]) -> Option<slint::Image> {
    const W: usize = 32;
    const H: usize = 24;
    if frame.len() < W * H {
        return None;
    }
    let (min, max) = frame.iter().take(W * H)
        .fold((f32::MAX, f32::MIN), |(lo, hi), &v| (lo.min(v), hi.max(v)));
    let range = (max - min).max(0.1);
    let mut buf = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::new(W as u32, H as u32);
    let px = buf.make_mut_slice();
    for (i, &v) in frame.iter().take(W * H).enumerate() {
        let (r, g, b) = ironbow((v - min) / range);
        px[i] = slint::Rgba8Pixel { r, g, b, a: 255 };
    }
    Some(slint::Image::from_rgba8(buf))
}

/// GET /api/thermal/frame → the SensorHead's raw MLX90640 grid (768 °C floats).
/// None on any non-sensor node / dashboard-down (the endpoint 503s with an empty frame).
async fn fetch_thermal_frame(client: &reqwest::Client, base_url: &str) -> Option<Vec<f32>> {
    let resp = client
        .get(format!("{base_url}/api/thermal/frame"))
        .timeout(std::time::Duration::from_secs(5))
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let body: Value = resp.json().await.ok()?;
    let arr = body["frame"].as_array()?;
    if arr.is_empty() {
        return None;
    }
    Some(arr.iter().map(|v| v.as_f64().unwrap_or(0.0) as f32).collect())
}

// ── Tier-A parity app fetchers ──────────────────────────────────────────────

fn event_accent(ty: &str) -> slint::Color {
    let hex: u32 = match ty {
        t if t.contains("error") || t.contains("denied") || t.contains("reject") => 0xef4444,
        "tool_requested" | "approval_pending" => 0xeab308,
        "tool_result" => 0x39ff14,
        "wake_triggered" => 0x00d4ff,
        "sensor_reading" | "thermal_frame" => 0x6c8aff,
        _ => 0x8b93a7,
    };
    slint::Color::from_rgb_u8((hex >> 16) as u8, (hex >> 8) as u8, hex as u8)
}

// One-line detail from an event's notable fields; falls back to compacting the
// top-level scalar fields so unknown event shapes still read sensibly.
fn event_summary(ev: &Value) -> String {
    let trunc = |s: &str, n: usize| -> String {
        let t: String = s.chars().take(n).collect();
        if s.chars().count() > n { format!("{t}…") } else { t }
    };
    if let Some(tool) = ev["call"]["tool"].as_str() {
        return tool.to_string();
    }
    if let Some(kind) = ev["reading"]["kind"].as_str() {
        return kind.to_string();
    }
    if let Some(text) = ev["text"].as_str().filter(|s| !s.is_empty()) {
        return trunc(text, 120);
    }
    let Some(obj) = ev.as_object() else { return String::new() };
    let parts: Vec<String> = obj.iter()
        .filter(|(k, _)| k.as_str() != "type")
        .filter_map(|(k, v)| match v {
            Value::String(s) => Some(format!("{k}={}", trunc(s, 40))),
            Value::Number(n) => Some(format!("{k}={n}")),
            Value::Bool(b)   => Some(format!("{k}={b}")),
            _ => None,
        })
        .take(4)
        .collect();
    parts.join("  ")
}

// GET /api/events/recent → newest-first EventLogItem list.
// `types` = CSV of Event "type" tags to keep (empty = all; server still strips
// the noisy streaming events). `hours` = lookback window (server caps at 168).
async fn fetch_events(
    client: &reqwest::Client,
    base_url: &str,
    types: &str,
    hours: i32,
) -> Vec<EventLogItem> {
    let mut url = format!("{base_url}/api/events/recent?max=200&hours={hours}");
    let types = types.trim();
    if !types.is_empty() {
        // type tags are snake_case CSV ([a-z_,]) — query-safe, no encoding; the
        // server splits the value on a literal comma.
        url.push_str("&types=");
        url.push_str(types);
    }
    let body = json_get(client, url).await;
    let arr = match body.as_array() { Some(a) => a.clone(), None => return Vec::new() };
    arr.iter().rev().map(|ev| {
        let ty = ev["type"].as_str().unwrap_or("event");
        EventLogItem {
            ev_type: ty.into(),
            summary: event_summary(ev).into(),
            accent:  event_accent(ty),
        }
    }).collect()
}

// GET /api/mesh/{peers,nodes} → saved peers first, then discovered-but-unsaved.
// GET /api/mesh/inbox → the persisted per-peer unread threads, used to SEED the
// inbox at launch so unread + previews survive a restart (the live `mesh_message`
// stream takes over from there). Relative-time labels are re-derived from last_ts.
async fn fetch_inbox(client: &reqwest::Client, base_url: &str) -> Vec<InboxThread> {
    let body = json_get(client, format!("{base_url}/api/mesh/inbox")).await;
    let now = now_secs();
    body["threads"].as_array().map(|a| a.iter().filter_map(|t| {
        let node_id = t["node_id"].as_str().unwrap_or("");
        if node_id.is_empty() { return None; }
        let last_ts = t["last_ts"].as_i64().unwrap_or(0);
        Some(InboxThread {
            node_id:   node_id.into(),
            preview:   t["preview"].as_str().unwrap_or("").into(),
            unread:    t["unread"].as_i64().unwrap_or(0) as i32,
            last_seen: ago_label(now - last_ts).into(),
            last_ts:   last_ts as i32,
            session:   t["session"].as_i64().unwrap_or(0) as i32,
        })
    }).collect()).unwrap_or_default()
}

/// Seed the inbox model wholesale from the persisted threads (launch only). Safe
/// against a racing live event: the server already counted it, so its snapshot is
/// authoritative. Slint thread only.
fn seed_inbox(rows: Vec<InboxThread>) {
    INBOX.with(|m| {
        if let Some(model) = m.borrow().as_ref() {
            while model.row_count() > 0 { model.remove(model.row_count() - 1); }
            for r in rows { model.push(r); }
        }
    });
    inbox_refresh_badge();
}

/// Render a peer's inbound-federation counters ("federation" from
/// GET /api/mesh/peers) into one dim roster line; "" when no traffic yet
/// (the row hides). Pure.
fn fed_stats_line(f: &serde_json::Value) -> String {
    let recv = f["memories_received"].as_u64().unwrap_or(0);
    let dup  = f["duplicates"].as_u64().unwrap_or(0);
    let srv  = f["recall_served"].as_u64().unwrap_or(0);
    let hits = f["recall_hits"].as_u64().unwrap_or(0);
    if recv == 0 && dup == 0 && srv == 0 {
        return String::new();
    }
    format!("fed ↓{recv} mem · {dup} dup · {srv} recall ({hits} hits)")
}

async fn fetch_mesh(client: &reqwest::Client, base_url: &str) -> Vec<MeshNode> {
    let (peers_resp, nodes_resp) = tokio::join!(
        json_get(client, format!("{base_url}/api/mesh/peers")),
        json_get(client, format!("{base_url}/api/mesh/nodes")),
    );
    let mut out: Vec<MeshNode> = Vec::new();
    if let Some(peers) = peers_resp["peers"].as_array() {
        for p in peers {
            out.push(MeshNode {
                node_id:   p["node_id"].as_str().unwrap_or("").into(),
                detail:    p["ws_url"].as_str().unwrap_or("").into(),
                role:      p["role"].as_str().unwrap_or("full").into(),
                // Prefer the downtime beacon's live status (alive/dark) over the
                // static peers.toml status — it's the real-time truth.
                status:    p["live"].as_str().or_else(|| p["status"].as_str()).unwrap_or("online").into(),
                is_peer:   true,
                has_token: p["has_token"].as_bool().unwrap_or(false),
                fed_line:  fed_stats_line(&p["federation"]).into(),
            });
        }
    }
    if let Some(nodes) = nodes_resp["nodes"].as_array() {
        for n in nodes {
            // Skip nodes already saved as peers (server flags them "known").
            if n["known"].as_bool() == Some(true) { continue; }
            let ip   = n["ip"].as_str().unwrap_or("");
            let port = n["port"].as_u64().unwrap_or(8787);
            out.push(MeshNode {
                node_id:   n["node_id"].as_str().unwrap_or("").into(),
                detail:    n["ws_url"].as_str().map(|s| s.to_string())
                            .unwrap_or_else(|| format!("{ip}:{port}")).into(),
                role:      "—".into(),
                status:    "discovered".into(),
                is_peer:   false,
                has_token: false,
                fed_line:  "".into(),
            });
        }
    }
    out
}

struct InferenceData {
    backend:  String,
    base_url: String,
    models:   Vec<ModelItem>,
    usage:    Usage,
}

// GET /api/backend + /api/models + /api/usage → backend + model list + cache-bank stats.
async fn fetch_inference(client: &reqwest::Client, base_url: &str) -> InferenceData {
    let (backend_resp, models_resp, usage_resp) = tokio::join!(
        json_get(client, format!("{base_url}/api/backend")),
        json_get(client, format!("{base_url}/api/models")),
        json_get(client, format!("{base_url}/api/usage")),
    );
    let models: Vec<ModelItem> = models_resp["models"]
        .as_array()
        .unwrap_or(&vec![])
        .iter()
        .map(|m| ModelItem {
            model_id:   m["id"].as_str().unwrap_or("").into(),
            model_name: m["name"].as_str().unwrap_or("").into(),
        })
        .collect();
    InferenceData {
        backend:  backend_resp["backend"].as_str().unwrap_or("—").to_string(),
        base_url: backend_resp["oai_base_url"].as_str().unwrap_or("").to_string(),
        models,
        usage:    build_usage(&usage_resp),
    }
}

/// Humanize a token count: 2_770_000 → "2.8M", 31_200 → "31K", 412 → "412". Negative-safe.
fn humanize_tokens(n: i64) -> String {
    let a = n.unsigned_abs() as f64;
    let s = if a >= 1e6 { format!("{:.1}M", a / 1e6) }
            else if a >= 1e3 { format!("{:.0}K", a / 1e3) }
            else { format!("{}", a as u64) };
    if n < 0 { format!("-{s}") } else { s }
}

/// Format a USD estimate: ≥1¢ → "$1.79"/"$0.05"; sub-cent → "$0.0021"; ≤0 → "$0.00".
fn fmt_money(x: f64) -> String {
    if x <= 0.0 { "$0.00".to_string() }
    else if x >= 0.01 { format!("${x:.2}") }
    else { format!("${x:.4}") }
}

/// Build the Inference view's cache-bank readout from a GET /api/usage body. Returns the
/// all-empty default before any turn has run (the view renders an empty-state for that).
fn build_usage(r: &serde_json::Value) -> Usage {
    let turns = r["turns"].as_u64().unwrap_or(0);
    if turns == 0 { return Usage::default(); }
    let hit        = r["cache_hit_rate"].as_f64().unwrap_or(0.0);
    let banked     = r["banked_tokens"].as_i64().unwrap_or(0);
    let saved      = r["cost_usd"]["saved"].as_f64().unwrap_or(0.0);
    let spent      = r["cost_usd"]["spent"].as_f64().unwrap_or(0.0);
    let cache_read = r["tokens"]["cache_read"].as_u64().unwrap_or(0) as i64;
    let input      = r["tokens"]["input"].as_u64().unwrap_or(0) as i64;
    let output     = r["tokens"]["output"].as_u64().unwrap_or(0) as i64;
    Usage {
        turns:    turns.to_string().into(),
        hit_rate: format!("{:.1}%", hit * 100.0).into(),
        banked:   humanize_tokens(banked).into(),
        saved:    fmt_money(saved).into(),
        spent:    fmt_money(spent).into(),
        detail:   format!("{} cached · {} fresh · {} out",
                      humanize_tokens(cache_read), humanize_tokens(input), humanize_tokens(output)).into(),
        model:    r["model"].as_str().unwrap_or("").into(),
    }
}

fn human_size(bytes: u64) -> String {
    if bytes >= 1 << 20 {
        format!("{:.1} MB", bytes as f64 / (1u64 << 20) as f64)
    } else if bytes >= 1 << 10 {
        format!("{:.0} KB", bytes as f64 / (1u64 << 10) as f64)
    } else {
        format!("{bytes} B")
    }
}

// GET /api/audio/files → AudioFileItem list.
async fn fetch_audio_files(client: &reqwest::Client, base_url: &str) -> Vec<AudioFileItem> {
    let body = json_get(client, format!("{base_url}/api/audio/files")).await;
    body["files"].as_array().unwrap_or(&vec![]).iter().map(|f| AudioFileItem {
        path:       f["path"].as_str().unwrap_or("").into(),
        name:       f["name"].as_str().unwrap_or("").into(),
        size_label: human_size(f["size"].as_u64().unwrap_or(0)).into(),
    }).collect()
}

// POST /api/audio/waveform → (normalised 0..1 envelope, duration label).
async fn fetch_waveform(client: &reqwest::Client, base_url: &str, path: &str) -> (Vec<f32>, String) {
    let resp = client.post(format!("{base_url}/api/audio/waveform"))
        .json(&serde_json::json!({"path": path, "samples": 240}))
        .timeout(std::time::Duration::from_secs(30))
        .send().await;
    let body: Value = match resp {
        Ok(r) => r.json().await.unwrap_or(Value::Null),
        Err(_) => Value::Null,
    };
    let raw: Vec<f32> = body["samples"].as_array().unwrap_or(&vec![])
        .iter().map(|v| v.as_f64().unwrap_or(0.0) as f32).collect();
    // Normalise to the peak so quiet tracks still fill the view.
    let peak = raw.iter().cloned().fold(0.0f32, f32::max).max(1e-6);
    let norm: Vec<f32> = raw.iter().map(|s| (s / peak).clamp(0.0, 1.0)).collect();
    let dur = body["duration_s"].as_f64().unwrap_or(0.0);
    let dur_label = if dur > 0.0 {
        format!("{}:{:02}", (dur as u64) / 60, (dur as u64) % 60)
    } else {
        String::new()
    };
    (norm, dur_label)
}

// POST /api/audio/analyze → one-line loudness summary.
async fn fetch_audio_stats(client: &reqwest::Client, base_url: &str, path: &str) -> String {
    let resp = client.post(format!("{base_url}/api/audio/analyze"))
        .json(&serde_json::json!({"path": path}))
        .timeout(std::time::Duration::from_secs(30))
        .send().await;
    let body: Value = match resp {
        Ok(r) => r.json().await.unwrap_or(Value::Null),
        Err(_) => Value::Null,
    };
    if !body["error"].is_null() {
        return format!("analyze failed: {}", body["error"].as_str().unwrap_or("?"));
    }
    let fmt  = body["format"].as_str().unwrap_or("?");
    let sr   = body["sample_rate"].as_u64().unwrap_or(0);
    let ch   = body["channels"].as_u64().unwrap_or(0);
    let lufs = body["lufs_integrated"].as_f64().unwrap_or(-99.0);
    let peak = body["peak_db"].as_f64().unwrap_or(-99.0);
    let rms  = body["rms_db"].as_f64().unwrap_or(-99.0);
    let clip = body["has_clipping"].as_bool().unwrap_or(false);
    format!(
        "{fmt} · {} kHz · {}ch    LUFS {lufs:.1} · peak {peak:.1} dB · RMS {rms:.1} dB{}",
        sr / 1000, ch,
        if clip { " · ⚠ clipping" } else { "" },
    )
}

// Map a one-click op name to the /api/audio/process ops array.
fn audio_op_chain(op: &str) -> Vec<Value> {
    match op {
        "normalize"    => vec![serde_json::json!({"type": "normalize"})],
        "trim_silence" => vec![serde_json::json!({"type": "trim_silence"})],
        "peak_limit"   => vec![serde_json::json!({"type": "peak_limit"})],
        // Composite "clean": strip silence, normalise loudness, then limit peaks.
        "clean" => vec![
            serde_json::json!({"type": "trim_silence"}),
            serde_json::json!({"type": "normalize"}),
            serde_json::json!({"type": "peak_limit"}),
        ],
        _ => Vec::new(),
    }
}

// GET /api/sonus/files → SonusFileItem list (bare JSON array).
async fn fetch_sonus_files(client: &reqwest::Client, base_url: &str) -> Vec<SonusFileItem> {
    let body = json_get(client, format!("{base_url}/api/sonus/files")).await;
    body.as_array().unwrap_or(&vec![]).iter().map(|f| SonusFileItem {
        name:       f["name"].as_str().unwrap_or("").into(),
        size_label: human_size(f["size"].as_u64().unwrap_or(0)).into(),
    }).collect()
}

async fn fetch_notes(client: &reqwest::Client, base_url: &str) -> Vec<NoteItem> {
    // GET /api/notes → { files: [{ name, size }] }
    let body = json_get(client, format!("{base_url}/api/notes")).await;
    body["files"].as_array().unwrap_or(&vec![]).iter().map(|f| NoteItem {
        name:       f["name"].as_str().unwrap_or("").into(),
        size_label: human_size(f["size"].as_u64().unwrap_or(0)).into(),
    }).collect()
}

async fn fetch_workspace_images(client: &reqwest::Client, base_url: &str) -> Vec<ImageItem> {
    // GET /api/workspace/images → { images: [{ path, name, size, modified }] } (newest first)
    let body = json_get(client, format!("{base_url}/api/workspace/images")).await;
    body["images"].as_array().unwrap_or(&vec![]).iter().map(|f| ImageItem {
        path: f["path"].as_str().unwrap_or("").into(),
        name: f["name"].as_str().unwrap_or("").into(),
    }).collect()
}

async fn fetch_explorer_list(client: &reqwest::Client, base_url: &str, path: &str) -> Vec<ExplorerEntry> {
    // GET /api/workspace/list?path= → { entries: [{ name, kind, size, ext, path, abs }] }
    let body: Value = match client.get(format!("{base_url}/api/workspace/list"))
        .query(&[("path", path)])
        .timeout(std::time::Duration::from_secs(10))
        .send().await
    {
        Ok(r) => r.json().await.unwrap_or(Value::Null),
        Err(_) => Value::Null,
    };
    body["entries"].as_array().unwrap_or(&vec![]).iter().map(|e| {
        let is_dir = e["kind"].as_str() == Some("dir");
        let ext = e["ext"].as_str().unwrap_or("");
        ExplorerEntry {
            name:       e["name"].as_str().unwrap_or("").into(),
            kind:       e["kind"].as_str().unwrap_or("file").into(),
            size_label: if is_dir { "".into() } else { human_size(e["size"].as_u64().unwrap_or(0)).into() },
            ext:        ext.into(),
            path:       e["path"].as_str().unwrap_or("").into(),
            abs:        e["abs"].as_str().unwrap_or("").into(),
            glyph:      explorer_glyph(is_dir, ext).into(),
        }
    }).collect()
}

/// POST a confined workspace write op (mkdir/delete/rename/move/copy). Returns
/// (ok, error) — `error` is the server's message on failure, "" on success.
async fn workspace_op(client: &reqwest::Client, base_url: &str, endpoint: &str, body: Value) -> (bool, String) {
    match client.post(format!("{base_url}/api/workspace/{endpoint}"))
        .json(&body)
        .timeout(std::time::Duration::from_secs(30))
        .send().await
    {
        Ok(r) => {
            let v: Value = r.json().await.unwrap_or(Value::Null);
            let ok = v["ok"].as_bool().unwrap_or(false);
            (ok, v["error"].as_str().unwrap_or("").to_string())
        }
        Err(e) => (false, format!("request failed: {e}")),
    }
}

/// GET /api/media/candidates?mode= → the USB sticks the "Use this drive" picker can adopt
/// (`relabel` = keep-files set; `format` = the broader wipeable set). Rust pre-formats each
/// into one display line ("SanDisk Ultra · 57.3 GB · MYSTICK (exfat)" / "… · blank").
async fn fetch_drive_candidates(client: &reqwest::Client, base_url: &str, mode: &str) -> Vec<UsbCandidate> {
    let body: Value = match client.get(format!("{base_url}/api/media/candidates"))
        .query(&[("mode", mode)])
        .timeout(std::time::Duration::from_secs(10))
        .send().await
    {
        Ok(r) => r.json().await.unwrap_or(Value::Null),
        Err(_) => Value::Null,
    };
    body["candidates"].as_array().unwrap_or(&vec![]).iter().map(|c| {
        let model = c["model"].as_str().unwrap_or("").trim();
        let size  = c["size"].as_str().unwrap_or("");
        let label = c["label"].as_str().unwrap_or("");
        let fs    = c["fstype"].as_str().unwrap_or("");
        let blank = c["blank"].as_bool().unwrap_or(false);
        let mut parts: Vec<String> = Vec::new();
        if !model.is_empty() { parts.push(model.to_string()); }
        if !size.is_empty()  { parts.push(size.to_string()); }
        parts.push(if blank { "blank".to_string() }
                   else if label.is_empty() { format!("unlabeled · {fs}") }
                   else { format!("{label} · {fs}") });
        UsbCandidate {
            dev:     c["dev"].as_str().unwrap_or("").into(),
            display: parts.join("  ·  ").into(),
            label:   label.into(),
        }
    }).collect()
}

/// GET /api/workspace/read?path= → (content, binary). Empty + binary=true on a
/// non-text file; empty + false on error.
async fn fetch_explorer_read(client: &reqwest::Client, base_url: &str, path: &str) -> (String, bool) {
    let body: Value = match client.get(format!("{base_url}/api/workspace/read"))
        .query(&[("path", path)])
        .timeout(std::time::Duration::from_secs(10))
        .send().await
    {
        Ok(r) => r.json().await.unwrap_or(Value::Null),
        Err(_) => Value::Null,
    };
    let binary = body["binary"].as_bool().unwrap_or(false);
    let mut content = body["content"].as_str().unwrap_or("").to_string();
    if body["truncated"].as_bool().unwrap_or(false) {
        content.push_str("\n\n… (truncated)");
    }
    (content, binary)
}

/// POST /api/notes/read → the note's content (empty string on any error).
async fn fetch_note_content(client: &reqwest::Client, base_url: &str, name: &str) -> String {
    match client.post(format!("{base_url}/api/notes/read"))
        .json(&serde_json::json!({ "name": name }))
        .timeout(std::time::Duration::from_secs(8))
        .send().await
    {
        Ok(r) if r.status().is_success() => r.json::<Value>().await
            .ok()
            .and_then(|v| v["content"].as_str().map(|s| s.to_string()))
            .unwrap_or_default(),
        _ => String::new(),
    }
}

// ── App state ─────────────────────────────────────────────────────────────────
#[derive(Default)]
struct AppState {
    session_id: Option<u64>,
    // Child sessions spawned via agent.spawn and not yet turn-complete; drives
    // the taskbar "N sub-agents running" badge.
    subagents: std::collections::HashSet<u64>,
}

// ── Screen mirror (#36): serve a PNG of APEX's own screen ────────────────────
// APEX's `screenshot_mirror` tool GETs http://127.0.0.1:8788/snapshot. We render
// the live window via Slint's renderer-agnostic Window::take_snapshot() — works
// on winit/femtovg (desktop), linuxkms/skia (Pi 5) and femtovg-software (Pi
// Zero) alike, so there's no DRM framebuffer readback and no Wayland screencopy
// to fight. Loopback-only: the screen is never exposed on the network.

fn snapshot_addr() -> String {
    std::env::var("APEXOS_UI_SNAPSHOT_ADDR")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "127.0.0.1:8788".to_string())
}

async fn run_snapshot_server(addr: String, ui_weak: slint::Weak<AppWindow>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = match tokio::net::TcpListener::bind(&addr).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("[mirror] snapshot server bind {addr} failed: {e}");
            return;
        }
    };
    eprintln!("[mirror] screen-snapshot server on http://{addr}/snapshot (+ /state)");
    loop {
        let (mut stream, _) = match listener.accept().await {
            Ok(p) => p,
            Err(_) => continue,
        };
        let uw = ui_weak.clone();
        tokio::spawn(async move {
            // Read the request head for the target only: `/state` serves the
            // shell-structure JSON (adaptive UI's ui_query eyes); anything else
            // keeps the historical behaviour — the snapshot PNG.
            let mut scratch = [0u8; 1024];
            let n = stream.read(&mut scratch).await.unwrap_or(0);
            let is_state = std::str::from_utf8(&scratch[..n])
                .ok()
                .and_then(|h| h.lines().next())
                .and_then(|l| l.split_whitespace().nth(1))
                .is_some_and(|t| t == "/state" || t.starts_with("/state?"));
            let (status, ctype, body) = if is_state {
                match capture_state(uw).await {
                    Ok(json) => ("200 OK", "application/json", json.into_bytes()),
                    Err(e) => ("500 Internal Server Error", "text/plain", e.into_bytes()),
                }
            } else {
                match capture_png(uw).await {
                    Ok(png) => ("200 OK", "image/png", png),
                    Err(e) => ("500 Internal Server Error", "text/plain", e.into_bytes()),
                }
            };
            let head = format!(
                "HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = stream.write_all(head.as_bytes()).await;
            let _ = stream.write_all(&body).await;
            let _ = stream.shutdown().await;
        });
    }
}

/// The shell's structure as JSON — the adaptive-UI eyes (`ui_query`, Loop 6).
/// Built on the Slint thread (window model + latch masks are thread-local),
/// handed back over a oneshot like `capture_png`. Deliberately structural, not
/// geometric: window rects stay the WM's business (topology, never geometry).
async fn capture_state(ui_weak: slint::Weak<AppWindow>) -> Result<String, String> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    slint::invoke_from_event_loop(move || {
        let res = match ui_weak.upgrade() {
            Some(ui) => Ok(shell_state_json(&ui).to_string()),
            None => Err("UI window gone".to_string()),
        };
        let _ = tx.send(res);
    })
    .map_err(|e| format!("event loop: {e}"))?;
    rx.await.map_err(|_| "state capture canceled".to_string())?
}

/// Assemble the /state payload. Slint thread only.
fn shell_state_json(ui: &AppWindow) -> serde_json::Value {
    let focused_id = ui.get_focused_id();
    let mode = match ui.get_shell_mode() {
        ShellMode::Focus => "focus",
        _ => "desktop",
    };
    let windows: Vec<serde_json::Value> = WINDOWS.with(|w| {
        w.borrow()
            .as_ref()
            .map(|m| {
                (0..m.row_count())
                    .filter_map(|i| m.row_data(i))
                    .map(|d| {
                        serde_json::json!({
                            "app": kind_slug(d.kind),
                            "title": d.title.as_str(),
                            "minimized": d.minimized,
                            "maximized": d.maximized,
                            "focused": d.id == focused_id,
                        })
                    })
                    .collect()
            })
            .unwrap_or_default()
    });
    let mask_slugs = |mask: u32| -> Vec<&'static str> {
        APP_TABLE
            .iter()
            .enumerate()
            .filter(|(i, _)| mask & (1u32 << i) != 0)
            .map(|(_, (_, s))| *s)
            .collect::<Vec<_>>()
    };
    let latched = UI_LATCHED.with(|m| mask_slugs(m.get()));
    let agent_opened = AGENT_OPENED.with(|m| mask_slugs(m.get()));
    serde_json::json!({
        "shell_mode": mode,
        "persona": current_persona_slug(),
        "windows": windows,
        "agent_opened": agent_opened,
        "latched": latched,
        // A3 rate rail: mutations applied this turn vs the cap — visible so
        // the agent can SEE it throttled instead of wondering why a verb
        // didn't land.
        "turn_mutations": UI_TURN_MUTATIONS.with(|m| m.get()),
        "mutation_cap": UI_TURN_MUTATION_CAP,
        // Phase C: the installed reflex table + per-rule fire ledger — the
        // agent sees what's installed and what's actually earning its fires.
        "reflexes": REFLEXES.with(|r| {
            r.borrow()
                .iter()
                .map(|x| serde_json::json!({
                    "on": x.on, "do": x.action, "app": x.app, "fires": x.fires,
                }))
                .collect::<Vec<_>>()
        }),
        "apps": APP_TABLE.iter().map(|(_, s)| *s).collect::<Vec<_>>(),
    })
}

/// Snapshot the live window on the Slint thread, then PNG-encode off-thread.
async fn capture_png(ui_weak: slint::Weak<AppWindow>) -> Result<Vec<u8>, String> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    slint::invoke_from_event_loop(move || {
        let res = match ui_weak.upgrade() {
            Some(ui) => ui
                .window()
                .take_snapshot()
                .map_err(|e| format!("take_snapshot: {e}")),
            None => Err("UI window gone".to_string()),
        };
        let _ = tx.send(res);
    })
    .map_err(|e| format!("event loop: {e}"))?;
    let buf = rx.await.map_err(|_| "snapshot canceled".to_string())??;
    // SharedPixelBuffer<Rgba8Pixel> → PNG, off the Slint thread.
    let (w, h) = (buf.width(), buf.height());
    let img = image::RgbaImage::from_raw(w, h, buf.as_bytes().to_vec())
        .ok_or_else(|| "pixel buffer size mismatch".to_string())?;
    let mut out = std::io::Cursor::new(Vec::new());
    img.write_to(&mut out, image::ImageFormat::Png)
        .map_err(|e| format!("png encode: {e}"))?;
    Ok(out.into_inner())
}

/// Point THIS process's fontconfig at a config that loads the system one and
/// then rejects the color-bitmap emoji font, so font fallback lands on the
/// monochrome `Noto Emoji` instead.
///
/// Why: femtovg is the only renderer we compile (Nano-first — Skia is too heavy
/// for the tier ladder), and femtovg can't rasterize colour-bitmap/COLR glyphs.
/// A char from "Noto Color Emoji" therefore renders as tofu. The bundled mono
/// `Noto Emoji` (installed by install.sh / shipped in `deploy/fonts/`) is plain
/// outlines femtovg *can* draw — but fontconfig prefers the colour font by
/// default, so we drop it for our process only. Scoped via `FONTCONFIG_FILE`:
/// the rest of the machine keeps colour emoji. Must run before the first font
/// query (i.e. before `AppWindow::new()`). Best-effort — any failure leaves the
/// default config in place (emoji stay tofu, nothing breaks). Respects an
/// existing `FONTCONFIG_FILE` so a user override always wins.
fn ensure_mono_emoji_fontconfig() {
    if std::env::var_os("FONTCONFIG_FILE").is_some() {
        return; // user/operator override — leave it alone
    }
    const CONF: &str = r#"<?xml version="1.0"?>
<!DOCTYPE fontconfig SYSTEM "fonts.dtd">
<fontconfig>
  <!-- Load the system config (all font dirs + rules)… -->
  <include ignore_missing="yes">/etc/fonts/fonts.conf</include>
  <!-- …then drop the colour-bitmap emoji font: femtovg can't rasterize it, so
       fallback lands on the monochrome Noto Emoji (outline) instead of tofu. -->
  <selectfont>
    <rejectfont>
      <pattern>
        <patelt name="family"><string>Noto Color Emoji</string></patelt>
      </pattern>
    </rejectfont>
  </selectfont>
</fontconfig>
"#;
    let dir = std::env::var_os("XDG_CACHE_HOME")
        .map(std::path::PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .or_else(|| std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".cache")))
        .map(|p| p.join("apexos-rs"));
    let Some(dir) = dir else { return };
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let path = dir.join("fonts.conf");
    if std::fs::write(&path, CONF).is_ok() {
        std::env::set_var("FONTCONFIG_FILE", &path);
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Capture Slint/femtovg/linuxkms backend `log` output (default warn) so a GL/DRM
    // fault is recorded in the journal instead of vanishing into a silent exit-1.
    // Bump with RUST_LOG (e.g. `RUST_LOG=i_slint_backend_linuxkms=debug,femtovg=debug`).
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();

    // Steer this process's emoji fallback to a monochrome font before any font
    // is loaded (femtovg can't draw colour emoji). See the fn doc.
    ensure_mono_emoji_fontconfig();

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;

    let ui = AppWindow::new()?;

    // ── Persona system (G4): catalogue + boot resolution ─────────────────────
    // The catalogue backs the wizard + picker tiles. On boot: a persisted
    // persona is applied silently; a fresh install shows the first-boot wizard
    // over a sane Apex default. apply_persona tier-clamps the shell mode, so the
    // femtovg "Nano-first" Focus default is handled there (CLAUDE.md).
    ui.global::<Personas>().set_defs(slint::ModelRc::from(Rc::new(
        slint::VecModel::from(build_persona_defs()),
    )));
    match load_persona() {
        Some(p) => apply_persona(&ui, p, false),
        None => {
            apply_persona(&ui, Persona::Apex, false);
            ui.set_first_boot(true);
        }
    }
    {
        let uw = ui.as_weak();
        ui.global::<Personas>().on_pick(move |ord| {
            if let Some(ui) = uw.upgrade() {
                apply_persona(&ui, persona_from_ordinal(ord), true);
            }
        });
    }

    // Message model
    let messages: Rc<slint::VecModel<MessageItem>> = Rc::new(slint::VecModel::default());
    ui.set_messages(slint::ModelRc::from(messages.clone()));
    MESSAGES.with(|m| *m.borrow_mut() = Some(messages.clone()));

    // Session model
    let sessions: Rc<slint::VecModel<SessionItem>> = Rc::new(slint::VecModel::default());
    ui.set_sessions(slint::ModelRc::from(sessions.clone()));
    SESSIONS.with(|s| *s.borrow_mut() = Some(sessions.clone()));

    // Council model (G3d) — deliberating agents, driven by Council* WS events.
    let council: Rc<slint::VecModel<CouncilAgent>> = Rc::new(slint::VecModel::default());
    ui.set_council_agents(slint::ModelRc::from(council.clone()));
    COUNCIL.with(|c| *c.borrow_mut() = Some(council.clone()));

    let models_vec: Rc<slint::VecModel<ModelItem>> = Rc::new(slint::VecModel::default());
    ui.set_available_models(slint::ModelRc::from(models_vec.clone()));
    MODELS.with(|m| *m.borrow_mut() = Some(models_vec.clone()));

    // Work Board (🗂) — four live column models driven off the WS event stream.
    let board = BoardModels {
        goals:     Rc::new(slint::VecModel::default()),
        active:    Rc::new(slint::VecModel::default()),
        blocked:   Rc::new(slint::VecModel::default()),
        subagents: Rc::new(slint::VecModel::default()),
        recent:    Rc::new(slint::VecModel::default()),
    };
    ui.set_board_goals(slint::ModelRc::from(board.goals.clone()));
    ui.set_board_active(slint::ModelRc::from(board.active.clone()));
    ui.set_board_blocked(slint::ModelRc::from(board.blocked.clone()));
    ui.set_board_subagents(slint::ModelRc::from(board.subagents.clone()));
    ui.set_board_recent(slint::ModelRc::from(board.recent.clone()));
    BOARD.with(|b| *b.borrow_mut() = Some(board));

    // Tier-A parity app models — each replaced wholesale on the app's REFRESH.
    let events_vec: Rc<slint::VecModel<EventLogItem>> = Rc::new(slint::VecModel::default());
    ui.set_event_log(slint::ModelRc::from(events_vec.clone()));
    EVENTS.with(|e| *e.borrow_mut() = Some(events_vec.clone()));

    let mesh_vec: Rc<slint::VecModel<MeshNode>> = Rc::new(slint::VecModel::default());
    ui.set_mesh_nodes(slint::ModelRc::from(mesh_vec.clone()));
    MESH.with(|m| *m.borrow_mut() = Some(mesh_vec.clone()));

    let inbox_vec: Rc<slint::VecModel<InboxThread>> = Rc::new(slint::VecModel::default());
    ui.set_mesh_threads(slint::ModelRc::from(inbox_vec.clone()));
    INBOX.with(|m| *m.borrow_mut() = Some(inbox_vec.clone()));

    let infer_models_vec: Rc<slint::VecModel<ModelItem>> = Rc::new(slint::VecModel::default());
    ui.set_inference_models(slint::ModelRc::from(infer_models_vec.clone()));
    INFER_MODELS.with(|m| *m.borrow_mut() = Some(infer_models_vec.clone()));

    let audio_files_vec: Rc<slint::VecModel<AudioFileItem>> = Rc::new(slint::VecModel::default());
    ui.set_audio_files(slint::ModelRc::from(audio_files_vec.clone()));
    AUDIO_FILES.with(|m| *m.borrow_mut() = Some(audio_files_vec.clone()));

    let waveform_vec: Rc<slint::VecModel<f32>> = Rc::new(slint::VecModel::default());
    ui.set_audio_waveform(slint::ModelRc::from(waveform_vec.clone()));
    WAVEFORM.with(|m| *m.borrow_mut() = Some(waveform_vec.clone()));

    let sonus_files_vec: Rc<slint::VecModel<SonusFileItem>> = Rc::new(slint::VecModel::default());
    ui.set_sonus_files(slint::ModelRc::from(sonus_files_vec.clone()));
    SONUS_FILES.with(|m| *m.borrow_mut() = Some(sonus_files_vec.clone()));

    let notes_files_vec: Rc<slint::VecModel<NoteItem>> = Rc::new(slint::VecModel::default());
    ui.set_notes(slint::ModelRc::from(notes_files_vec.clone()));
    NOTES_FILES.with(|m| *m.borrow_mut() = Some(notes_files_vec.clone()));

    let workspace_images_vec: Rc<slint::VecModel<ImageItem>> = Rc::new(slint::VecModel::default());
    ui.set_workspace_images(slint::ModelRc::from(workspace_images_vec.clone()));
    WORKSPACE_IMAGES.with(|m| *m.borrow_mut() = Some(workspace_images_vec.clone()));

    let explorer_entries_vec: Rc<slint::VecModel<ExplorerEntry>> = Rc::new(slint::VecModel::default());
    ui.set_explorer_entries(slint::ModelRc::from(explorer_entries_vec.clone()));
    EXPLORER_ENTRIES.with(|m| *m.borrow_mut() = Some(explorer_entries_vec.clone()));

    let drive_candidates_vec: Rc<slint::VecModel<UsbCandidate>> = Rc::new(slint::VecModel::default());
    ui.set_explorer_drive_candidates(slint::ModelRc::from(drive_candidates_vec.clone()));
    DRIVE_CANDIDATES.with(|m| *m.borrow_mut() = Some(drive_candidates_vec.clone()));

    let sketch_strokes_vec: Rc<slint::VecModel<SketchStroke>> = Rc::new(slint::VecModel::default());
    ui.set_sketch_strokes(slint::ModelRc::from(sketch_strokes_vec.clone()));
    SKETCH_STROKES.with(|m| *m.borrow_mut() = Some(sketch_strokes_vec.clone()));

    // Occipital (📖) reader trail — persistent breadcrumb model (Phase 9).
    let occipital_trail_vec: Rc<slint::VecModel<ReaderLink>> = Rc::new(slint::VecModel::default());
    ui.set_occipital_trail(slint::ModelRc::from(occipital_trail_vec.clone()));
    OCCIPITAL_TRAIL.with(|t| *t.borrow_mut() = Some(occipital_trail_vec.clone()));

    // Feedback subsystem: bind the toast model + global callbacks.
    let toasts_vec: Rc<slint::VecModel<ToastItem>> = Rc::new(slint::VecModel::default());
    ui.global::<Notifications>().set_toasts(slint::ModelRc::from(toasts_vec.clone()));
    TOASTS.with(|t| *t.borrow_mut() = Some(toasts_vec.clone()));
    ui.global::<Notifications>().on_show(|kind, text| toast(kind, text.as_str()));
    ui.global::<Notifications>().on_dismiss(dismiss_toast);

    // Notification center (G3c): persisted history model + clear-all. UI_WEAK
    // lets toast() bump the unread badge from the Slint thread.
    let notif_log: Rc<slint::VecModel<ToastItem>> = Rc::new(slint::VecModel::default());
    ui.global::<Notifications>().set_log(slint::ModelRc::from(notif_log.clone()));
    NOTIF_LOG.with(|l| *l.borrow_mut() = Some(notif_log.clone()));
    UI_WEAK.with(|u| *u.borrow_mut() = Some(ui.as_weak()));
    {
        let uw = ui.as_weak();
        ui.global::<Notifications>().on_clear_log(move || {
            NOTIF_LOG.with(|l| {
                if let Some(model) = l.borrow().as_ref() {
                    model.set_vec(Vec::new());
                }
            });
            if let Some(ui) = uw.upgrade() { ui.set_notif_unread(0); }
        });
    }
    // Click on an actionable toast / notification (mesh a2a) → open that session.
    // Reuses the exact restore path (replay + switch to chat) and closes the notif
    // center overlay if it was open.
    {
        let uw = ui.as_weak();
        ui.global::<Notifications>().on_action(move |session_id| {
            if let Some(ui) = uw.upgrade() {
                ui.set_notif_center_open(false);
                ui.invoke_restore_session(session_id);
            }
        });
    }

    // Initial sys stats (all zeros, offline)
    ui.set_sys_stats(empty_sys_stats());

    // ── Window manager (G2): model + seed the Chat window ─────────────────────
    // Phase B: seed remembered shapes BEFORE the first launch, so even the boot
    // Chat window wears its last one. Phase C: reflexes survive a restart too.
    geom_load();
    reflex_load();
    let windows: Rc<slint::VecModel<WindowDesc>> = Rc::new(slint::VecModel::default());
    ui.set_windows(slint::ModelRc::from(windows.clone()));
    WINDOWS.with(|w| *w.borrow_mut() = Some(windows.clone()));
    // The seed launches wait for the desktop area to go LIVE (Phase B): before
    // the window has its real size (winit/Wayland deliver it at first
    // configure, a few ticks into the loop) the area reads dead, so a
    // remembered shape can't clamp to the live display — a display shrink
    // between sessions stranded the boot Chat window off-stage (caught in the
    // Phase B E2E clamp test). seed_windows_when_area_live re-arms a 50ms
    // timer until the area is real (bounded ~2s, then launches anyway — a
    // broken backend still gets its Chat window). Imperceptible in practice:
    // the area goes live before or with the first frame.
    seed_windows_when_area_live(ui.as_weak(), windows.clone(), 0);

    // ── Terminal (G3d): stdin channel + WS URL (parked until first launch) ────
    let term_url = {
        let base = std::env::var("AGENTD_WS")
            .unwrap_or_else(|_| "ws://localhost:8787/ws".to_string());
        let base = base
            .strip_suffix("/ws")
            .map(|b| format!("{b}/terminal-ws"))
            .unwrap_or(base);
        match std::env::var("AGENTD_TOKEN") {
            Ok(t) if !t.is_empty() => format!("{base}?token={t}"),
            _ => base,
        }
    };
    {
        let (term_tx, term_rx) = mpsc::unbounded_channel::<String>();
        TERM_TX.with(|t| *t.borrow_mut() = Some(term_tx));
        TERM_RX.with(|r| *r.borrow_mut() = Some(term_rx));
    }
    ui.on_terminal_send(move |line| {
        TERM_TX.with(|t| {
            if let Some(tx) = t.borrow().as_ref() {
                let _ = tx.send(format!("{line}\n"));
            }
        });
    });

    // ── Window-management callbacks ───────────────────────────────────────────
    {
        let w = windows.clone();
        let uw = ui.as_weak();
        let rt_h_term = rt.handle().clone();
        let term_url = term_url.clone();
        ui.on_launch_app(move |ord| {
            if let Some(ui) = uw.upgrade() {
                let kind = kind_from_ordinal(ord);
                // Adaptive UI: a menu launch is the user's own act — clear any
                // ui_open latch for this app (re-invitation) and release the
                // agent-opened mark (the user owns the window now). The agent
                // path re-marks after this returns, so its bookkeeping holds.
                let bit = ui_latch_bit(kind);
                UI_LATCHED.with(|m| m.set(m.get() & !bit));
                AGENT_OPENED.with(|m| m.set(m.get() & !bit));
                wm_launch(&ui, &w, kind);
                // Fire the per-app refresh the legacy tab strip used to trigger on
                // open-view — without it Settings/Sessions windows launch empty.
                match kind {
                    AppKind::Settings => ui.invoke_refresh_settings(),
                    AppKind::Sessions => ui.invoke_refresh_sessions(),
                    AppKind::Terminal => start_terminal(&rt_h_term, &term_url, ui.as_weak()),
                    // Fresh window → default filter (ALL / 24h), matching the
                    // EventLogView's reset state.
                    AppKind::EventLog => ui.invoke_refresh_events("".into(), 24),
                    AppKind::Mesh => ui.invoke_refresh_mesh(),
                    AppKind::Inference => ui.invoke_refresh_inference(),
                    AppKind::AudioEditor => ui.invoke_refresh_audio(),
                    AppKind::Sonus => ui.invoke_refresh_sonus(),
                    AppKind::Notes => ui.invoke_refresh_notes(),
                    AppKind::Explorer => ui.invoke_refresh_explorer(),
                    // (Occipital: the menu launch's generic latch-clear above IS
                    // the auto-reveal re-invitation since A3 — no separate flag.)
                    _ => {}
                }
            }
        });
    }
    {
        let w = windows.clone();
        let uw = ui.as_weak();
        ui.on_focus_window(move |id| {
            if let Some(ui) = uw.upgrade() { wm_focus(&ui, &w, id); }
        });
    }
    {
        let w = windows.clone();
        let uw = ui.as_weak();
        ui.on_close_window(move |id| {
            if let Some(ui) = uw.upgrade() {
                if let Some(i) = wm_index_by_id(&w, id) {
                    if let Some(d) = w.row_data(i) {
                        // Adaptive UI (the human always wins): closing an
                        // agent-opened window latches that app — ui_open is
                        // suppressed for the rest of the session; the agent
                        // sees it in ui_query's `latched` and learns. The
                        // Occipital reader force-latches on ANY user close:
                        // its auto-reveal makes it agent-ish even when the
                        // user opened it (A3 — the old standalone suppress
                        // flag, folded). A menu launch re-invites.
                        let bit = ui_latch_bit(d.kind);
                        if d.kind == AppKind::Occipital
                            || AGENT_OPENED.with(|m| m.get()) & bit != 0
                        {
                            AGENT_OPENED.with(|m| m.set(m.get() & !bit));
                            UI_LATCHED.with(|m| m.set(m.get() | bit));
                        }
                        // Phase B: the row is about to vanish — capture its
                        // final shape so the next open wears it.
                        geom_note(d.kind, d.x, d.y, d.w, d.h, d.maximized);
                    }
                    w.remove(i);
                }
                wm_refocus_top(&ui, &w);
            }
        });
    }
    {
        let w = windows.clone();
        let uw = ui.as_weak();
        ui.on_minimize_window(move |id| {
            if let Some(ui) = uw.upgrade() {
                wm_update_row(&w, id, |d| d.minimized = true);
                wm_refocus_top(&ui, &w);
            }
        });
    }
    {
        let w = windows.clone();
        let uw = ui.as_weak();
        ui.on_maximize_window(move |id| {
            if let Some(ui) = uw.upgrade() {
                wm_update_row(&w, id, |d| d.maximized = !d.maximized);
                geom_note_id(&w, id);
                wm_focus(&ui, &w, id);
            }
        });
    }
    {
        let w = windows.clone();
        let uw = ui.as_weak();
        ui.on_task_activate(move |id| {
            if let Some(ui) = uw.upgrade() {
                let minimized = wm_index_by_id(&w, id)
                    .and_then(|i| w.row_data(i))
                    .map(|d| d.minimized)
                    .unwrap_or(false);
                if minimized {
                    // Restore: bring it back and focus it.
                    wm_update_row(&w, id, |d| d.minimized = false);
                    wm_focus(&ui, &w, id);
                } else if ui.get_focused_id() == id {
                    // Clicking the already-focused window minimizes it (Windows-style).
                    wm_update_row(&w, id, |d| d.minimized = true);
                    wm_refocus_top(&ui, &w);
                } else {
                    wm_focus(&ui, &w, id);
                }
            }
        });
    }
    {
        let w = windows.clone();
        ui.on_move_window(move |id, x, y| {
            wm_update_row(&w, id, |d| { d.x = x; d.y = y; });
            geom_note_id(&w, id); // fires per pointer-move; the flush is debounced
        });
    }
    {
        let w = windows.clone();
        ui.on_resize_window(move |id, ww, hh| {
            wm_update_row(&w, id, |d| { d.w = ww; d.h = hh; });
            geom_note_id(&w, id);
        });
    }

    let state = Arc::new(Mutex::new(AppState::default()));

    // Voice state
    let tts_enabled = Arc::new(AtomicBool::new(false));

    // Outbound WS channel
    let (tx, mut rx) = mpsc::unbounded_channel::<String>();
    // Expose the sender globally so apply_persona can push live `set_persona` frames
    // (G5 tier-2). Set once; ignore if already set (single WS task per process).
    let _ = WS_TX.set(tx.clone());

    let ws_url = {
        let base = std::env::var("AGENTD_WS")
            .unwrap_or_else(|_| "ws://localhost:8787/ws".to_string());
        match std::env::var("AGENTD_TOKEN") {
            Ok(t) if !t.is_empty() => format!("{base}?token={t}"),
            _ => base,
        }
    };
    let http_base = ws_to_http(&ws_url);

    // Web launcher (Tier D): point the dashboard tiles at the real agentd host
    // (not localhost), so the URL is usable from any device on the LAN. Full-URL
    // env overrides win.
    {
        let host = web_host(&http_base);
        let cerebro = std::env::var("CEREBRO_WEB_URL").ok().filter(|s| !s.is_empty())
            .unwrap_or_else(|| format!("http://{host}:8765"));
        let sensorhead = std::env::var("SENSORHEAD_URL").ok().filter(|s| !s.is_empty())
            .unwrap_or_else(|| format!("http://{host}:8080"));
        ui.set_web_cerebro_url(cerebro.into());
        ui.set_web_sensorhead_url(sensorhead.into());
    }

    // Shared HTTP client — carries the bearer token (if set) on every REST call,
    // mirroring the ?token= already on the WS URL. Without this, every /api/* call
    // 401s whenever AGENTD_TOKEN is set (which install.sh now always does).
    let http_client = Arc::new({
        let mut builder = reqwest::Client::builder();
        if let Ok(t) = std::env::var("AGENTD_TOKEN") {
            if !t.is_empty() {
                let mut headers = reqwest::header::HeaderMap::new();
                if let Ok(val) = reqwest::header::HeaderValue::from_str(&format!("Bearer {t}")) {
                    headers.insert(reqwest::header::AUTHORIZATION, val);
                }
                builder = builder.default_headers(headers);
            }
        }
        builder.build().unwrap_or_default()
    });

    // ── WS task ──────────────────────────────────────────────────────────────
    let ui_weak = ui.as_weak();
    let state_ws    = state.clone();
    let tts_ws      = Arc::clone(&tts_enabled);
    let client_ws   = Arc::clone(&http_client);
    let base_ws     = http_base.clone();
    rt.spawn(async move {
        let mut backoff_secs: u64 = 2;

        'reconnect: loop {
            eprintln!("[ui-slint] connecting to {}", redact_ws_url(&ws_url));

            let (ws, _) = match connect_async(&ws_url).await {
                Ok(pair) => pair,
                Err(e) => {
                    // A token-less 401 is not a failure — it is the documented
                    // desktop login flow: agentd requires a session token, the
                    // profile screen is up, and this loop idles behind it until
                    // login re-execs the UI with a minted token. Say that,
                    // instead of spamming a scary error per retry.
                    let msg = e.to_string();
                    if msg.contains("401") && !ws_url.contains("token=") {
                        eprintln!("[ui-slint] agentd requires login — waiting for a profile (the login screen is up)");
                    } else {
                        eprintln!("[ui-slint] WS connect failed: {e}");
                    }
                    let w = ui_weak.clone();
                    let b = backoff_secs;
                    slint::invoke_from_event_loop(move || {
                        if let Some(ui) = w.upgrade() {
                            ui.set_status(
                                format!("Connection failed — retrying in {b}s").into()
                            );
                        }
                    })
                    .ok();
                    tokio::time::sleep(std::time::Duration::from_secs(backoff_secs)).await;
                    backoff_secs = (backoff_secs * 2).min(30);
                    continue 'reconnect;
                }
            };

            backoff_secs = 2; // reset on successful connect
            let (mut write, mut read) = ws.split();

            let init = serde_json::json!({"type": "session_init"});
            write.send(Message::Text(init.to_string())).await.ok();
            // G5 tier-2: announce the active persona on every (re)connect so the
            // agent's voice matches the current face from the first turn.
            let persona_frame = serde_json::json!({
                "type": "set_persona", "persona": current_persona_slug(),
            });
            write.send(Message::Text(persona_frame.to_string())).await.ok();

            {
                let w = ui_weak.clone();
                slint::invoke_from_event_loop(move || {
                    if let Some(ui) = w.upgrade() {
                        ui.set_status("Connected".into());
                    }
                })
                .ok();
            }

            let rt_current = tokio::runtime::Handle::current();

            loop {
                tokio::select! {
                    msg = read.next() => {
                        match msg {
                            Some(Ok(Message::Text(text))) => {
                                if let Ok(ev) = serde_json::from_str::<Value>(&text) {
                                    let ctx = DispatchCtx {
                                        rt_handle:   rt_current.clone(),
                                        http_client: Arc::clone(&client_ws),
                                        http_base:   base_ws.clone(),
                                        tts_enabled: Arc::clone(&tts_ws),
                                    };
                                    dispatch_event(ui_weak.clone(), ev, state_ws.clone(), ctx);
                                }
                            }
                            Some(Ok(_)) => {}
                            _ => {
                                eprintln!("[ui-slint] WS disconnected — reconnecting in {backoff_secs}s");
                                let w = ui_weak.clone();
                                let b = backoff_secs;
                                slint::invoke_from_event_loop(move || {
                                    if let Some(ui) = w.upgrade() {
                                        ui.set_status(
                                            format!("Disconnected — reconnecting in {b}s").into()
                                        );
                                    }
                                })
                                .ok();
                                tokio::time::sleep(
                                    std::time::Duration::from_secs(backoff_secs)
                                ).await;
                                backoff_secs = (backoff_secs * 2).min(30);
                                break; // inner loop → outer 'reconnect loop
                            }
                        }
                    }
                    out = rx.recv() => {
                        if let Some(text) = out {
                            write.send(Message::Text(text)).await.ok();
                        }
                    }
                }
            }
        }
    });

    // ── System stats polling (every 5 s) ─────────────────────────────────────
    let ui_weak_poll = ui.as_weak();
    let client_poll  = Arc::clone(&http_client);
    let http_base_poll = http_base.clone();
    rt.spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(5));
        loop {
            interval.tick().await;
            if let Some((cpu, ram, disk)) = fetch_sys_stats(&client_poll, &http_base_poll).await {
                let w = ui_weak_poll.clone();
                slint::invoke_from_event_loop(move || {
                    if let Some(ui) = w.upgrade() {
                        let mut s = ui.get_sys_stats();
                        s.cpu_pct  = cpu;
                        s.ram_pct  = ram;
                        s.disk_pct = disk;
                        s.online   = true;
                        ui.set_sys_stats(s);
                    }
                })
                .ok();
            }
        }
    });

    // ── Thermal heatmap polling (adaptive cadence) ───────────────────────────
    // The sensor_reading WS events carry only min/max/mean, so fetch the full 32×24
    // grid from /api/thermal/frame and build an ironbow image (on the UI thread —
    // the Vec<f32> is Send, the slint::Image isn't). Polls fast (2s) while a sensor
    // answers, backs off to 30s otherwise so a non-sensor node barely touches it.
    let ui_weak_therm   = ui.as_weak();
    let client_therm    = Arc::clone(&http_client);
    let http_base_therm = http_base.clone();
    rt.spawn(async move {
        loop {
            let frame = fetch_thermal_frame(&client_therm, &http_base_therm).await;
            let had_frame = frame.as_ref().is_some_and(|f| f.len() >= 768);
            if let Some(frame) = frame {
                let w = ui_weak_therm.clone();
                slint::invoke_from_event_loop(move || {
                    if let Some(ui) = w.upgrade() {
                        if let Some(img) = build_thermal_image(&frame) {
                            ui.set_thermal_image(img);
                        }
                    }
                })
                .ok();
            }
            tokio::time::sleep(std::time::Duration::from_secs(if had_frame { 2 } else { 30 })).await;
        }
    });

    // ── approve / reject callbacks (via AgentBridge global) ───────────────────
    let tx_approve = tx.clone();
    ui.global::<AgentBridge>().on_approve_tool(move |call_id| {
        if let Some(row) = find_tool_row(call_id.as_str()) {
            update_tool_row(row, |item| item.awaiting_approval = false);
        }
        // Event::UserApproval { session, action: ActionId, granted } — gateway injects session.
        // call_id is the stringified action-id; parse it back to the bare number agentd expects.
        let action: u64 = call_id.as_str().parse().unwrap_or(0);
        let payload = serde_json::json!({
            "type": "user_approval",
            "action": action,
            "granted": true
        })
        .to_string();
        tx_approve.send(payload).ok();
    });

    let tx_reject = tx.clone();
    ui.global::<AgentBridge>().on_reject_tool(move |call_id| {
        if let Some(row) = find_tool_row(call_id.as_str()) {
            update_tool_row(row, |item| {
                item.awaiting_approval = false;
                item.tool_status = "error".into();
            });
        }
        let action: u64 = call_id.as_str().parse().unwrap_or(0);
        let payload = serde_json::json!({
            "type": "user_approval",
            "action": action,
            "granted": false
        })
        .to_string();
        tx_reject.send(payload).ok();
    });

    // ── "+ New chat" callback (via AgentBridge global) ────────────────────────
    // Mint a fresh session without restarting agentd: hello{new:true} → the gateway
    // allocates a new session id + empty history, and its session_init reply clears
    // the view + sets current_session_id (the same path session restore uses).
    let tx_new = tx.clone();
    ui.global::<AgentBridge>().on_new_chat(move || {
        // Carry the active persona so the fresh session starts in the right voice
        // (G5 tier-2) — the new session id has no persona until we set one.
        let payload = serde_json::json!({
            "type": "hello", "new": true, "persona": current_persona_slug(),
        }).to_string();
        tx_new.send(payload).ok();
    });

    // ── send-message callback ─────────────────────────────────────────────────
    let tx_send = tx.clone();
    let messages_send = messages.clone();
    let send_weak = ui.as_weak();
    ui.on_send_message(move |text| {
        // Pull (and clear) any staged workspace image — image-only prompts are ok.
        let (img_path, img_name) = send_weak.upgrade().map(|ui| {
            let p = ui.get_staged_image_path().to_string();
            let n = ui.get_staged_image_name().to_string();
            if !p.is_empty() {
                ui.set_staged_image_path("".into());
                ui.set_staged_image_name("".into());
            }
            (p, n)
        }).unwrap_or_default();

        if text.is_empty() && img_path.is_empty() {
            return;
        }

        // Fresh exchange — drop any emote APEX was holding so this turn's
        // activity/idle face shows, and APEX can re-emote in its reply.
        clear_face_hold();

        maybe_push_time_divider();
        // The chat bubble shows the text, prefixed with a 🖼 chip line when an
        // image rode along (image-only prompts show just the chip).
        let bubble = if img_path.is_empty() {
            text.to_string()
        } else if text.is_empty() {
            format!("🖼 {img_name}")
        } else {
            format!("🖼 {img_name}\n{text}")
        };
        messages_send.push(MessageItem {
            role: "user".into(),
            text: bubble.into(),
            streaming: false,
            call_id: "".into(),
            tool_name: "".into(),
            tool_args: "".into(),
            tool_output: "".into(),
            tool_status: "".into(),
            awaiting_approval: false,
        });

        let mut frame = serde_json::json!({ "type": "user_prompt", "text": text.as_str() });
        if !img_path.is_empty() {
            frame["images"] = serde_json::json!([{ "path": img_path }]);
        }
        tx_send.send(frame.to_string()).ok();
    });

    // ── stop / cancel callback ────────────────────────────────────────────────
    // Abort the in-flight turn. agentd's cascade_cancel aborts the task but emits
    // no TurnComplete, so we also clear busy + retire pending tool cards locally.
    let tx_stop   = tx.clone();
    let stop_weak = ui.as_weak();
    ui.on_stop_turn(move || {
        let payload = serde_json::json!({"type": "user_cancel"}).to_string();
        tx_stop.send(payload).ok();
        clear_pending_tools();
        // Cancel ends the turn without a TurnComplete — reset the adaptive-UI
        // rate rail here too, or the next turn starts pre-throttled.
        UI_TURN_MUTATIONS.with(|m| m.set(0));
        if let Some(ui) = stop_weak.upgrade() {
            ui.set_agent_busy(false);
            ui.set_face_state("idle".into());
        }
    });

    // ── Occipital steer (9c): a clicked link / URL-bar "go here" nudge ─────────
    // Routes a normal user_prompt through the WS — the gateway injects the
    // session and it funnels through the TurnGate like any user message, so it
    // can't race the in-flight turn (ApexOS's serialized-turn invariant). The
    // agent finishes its step, then sees the hint and web_fetches the URL. No
    // new agentd code (additive: register_mcp_server + tool-event + user_prompt).
    let tx_occ   = tx.clone();
    let occ_weak = ui.as_weak();
    ui.on_occipital_steer(move |url| {
        let url = url.trim().to_string();
        if url.is_empty() {
            return;
        }
        clear_face_hold();
        maybe_push_time_divider();
        push_message(MessageItem {
            role: "user".into(),
            text: format!("🧭 go here: {url}").into(),
            streaming: false,
            call_id: "".into(),
            tool_name: "".into(),
            tool_args: "".into(),
            tool_output: "".into(),
            tool_status: "".into(),
            awaiting_approval: false,
        });
        let text =
            format!("(navigation) Go here next: {url}\n\nFetch and read it with web_fetch, then continue.");
        let frame = serde_json::json!({ "type": "user_prompt", "text": text }).to_string();
        tx_occ.send(frame).ok();
        if let Some(ui) = occ_weak.upgrade() {
            bump_scroll(&ui);
        }
    });

    // ── refresh-sessions callback ─────────────────────────────────────────────
    let rt_handle     = rt.handle().clone();
    let client_sess   = Arc::clone(&http_client);
    let http_base_sess = http_base.clone();
    ui.on_refresh_sessions(move || {
        let base   = http_base_sess.clone();
        let client = Arc::clone(&client_sess);
        rt_handle.spawn(async move {
            let items = fetch_sessions(&client, &base).await;
            slint::invoke_from_event_loop(move || {
                replace_sessions(items);
            })
            .ok();
        });
    });

    // ── restore-session callback ──────────────────────────────────────────────
    let tx_restore       = tx.clone();
    let ui_weak_restore  = ui.as_weak();
    ui.on_restore_session(move |session_id| {
        // Clear current message list and switch to chat view
        clear_messages();
        if let Some(ui) = ui_weak_restore.upgrade() {
            ui.set_current_view(0);
            ui.set_current_session_id(session_id);
            ui.set_status("Restoring…".into());
            // Restoring must also SURFACE the chat: in desktop mode the replay
            // otherwise lands in a closed/hidden window (set_current_view only
            // drives the focus shell). Route through the menu-launch path — a
            // restore is the user's own act, so the latch-clear it performs is
            // correct — which reveals/creates + focuses the Chat window. Covers
            // all restore entry points (session row, toast, mesh inbox).
            ui.invoke_launch_app(0); // AppKind::Chat ordinal
        }
        // Ask agentd to replay the session (Rust agentd: hello + resume_session field)
        let payload = serde_json::json!({
            "type": "hello",
            "resume_session": session_id as u64
        })
        .to_string();
        tx_restore.send(payload).ok();
    });

    // ── Session management: select / export / archive / delete ────────────────
    {
        let uw = ui.as_weak();
        ui.on_sessions_toggle_select(move |id| {
            SESSIONS.with(|s| {
                if let Some(m) = s.borrow().as_ref() {
                    for i in 0..m.row_count() {
                        if let Some(mut it) = m.row_data(i) {
                            if it.session_id == id {
                                it.selected = !it.selected;
                                m.set_row_data(i, it);
                                break;
                            }
                        }
                    }
                }
            });
            if let Some(ui) = uw.upgrade() {
                ui.set_sessions_selected_count(selected_session_ids().len() as i32);
            }
        });
    }
    {
        let uw = ui.as_weak();
        ui.on_sessions_clear_selection(move || {
            clear_session_selection();
            if let Some(ui) = uw.upgrade() { ui.set_sessions_selected_count(0); }
        });
    }
    {
        let base = http_base.clone();
        let client = Arc::clone(&http_client);
        let h = rt.handle().clone();
        ui.on_sessions_export_selected(move || {
            let ids = selected_session_ids();
            if ids.is_empty() { return; }
            let (base, client) = (base.clone(), Arc::clone(&client));
            h.spawn(async move {
                export_sessions(&client, &base, serde_json::json!({ "ids": ids, "format": "md" })).await;
            });
        });
    }
    {
        let base = http_base.clone();
        let client = Arc::clone(&http_client);
        let h = rt.handle().clone();
        ui.on_sessions_export_all(move || {
            let (base, client) = (base.clone(), Arc::clone(&client));
            h.spawn(async move {
                export_sessions(&client, &base, serde_json::json!({ "all": true, "format": "md" })).await;
            });
        });
    }
    {
        let base = http_base.clone();
        let client = Arc::clone(&http_client);
        let h = rt.handle().clone();
        let uw = ui.as_weak();
        ui.on_sessions_archive_selected(move || {
            let ids = selected_session_ids();
            if ids.is_empty() { return; }
            let (base, client, uw) = (base.clone(), Arc::clone(&client), uw.clone());
            h.spawn(async move {
                let mut n = 0;
                for id in &ids {
                    if client.post(format!("{base}/api/sessions/{id}/archive"))
                        .timeout(std::time::Duration::from_secs(8)).send().await
                        .map(|r| r.status().is_success()).unwrap_or(false) { n += 1; }
                }
                let items = fetch_sessions(&client, &base).await;
                slint::invoke_from_event_loop(move || {
                    replace_sessions(items);
                    clear_session_selection();
                    if let Some(ui) = uw.upgrade() { ui.set_sessions_selected_count(0); }
                }).ok();
                notify(ToastKind::Info, format!("Archived {n} session(s)"));
            });
        });
    }
    {
        let base = http_base.clone();
        let client = Arc::clone(&http_client);
        let h = rt.handle().clone();
        let uw = ui.as_weak();
        ui.on_sessions_delete_selected(move || {
            let ids = selected_session_ids();
            if ids.is_empty() { return; }
            let (base, client, uw) = (base.clone(), Arc::clone(&client), uw.clone());
            h.spawn(async move {
                let mut n = 0;
                for id in &ids {
                    if client.delete(format!("{base}/api/sessions/{id}"))
                        .timeout(std::time::Duration::from_secs(8)).send().await
                        .ok()
                        .map(|r| r.status().is_success()).unwrap_or(false) { n += 1; }
                }
                let items = fetch_sessions(&client, &base).await;
                slint::invoke_from_event_loop(move || {
                    replace_sessions(items);
                    clear_session_selection();
                    if let Some(ui) = uw.upgrade() { ui.set_sessions_selected_count(0); }
                }).ok();
                notify(ToastKind::Warn, format!("Deleted {n} session(s)"));
            });
        });
    }
    {
        // Consolidate selected → cerebro (no delete). Sequential LLM calls; toasts
        // bracket the run since it can take a few seconds per session.
        let base = http_base.clone();
        let client = Arc::clone(&http_client);
        let h = rt.handle().clone();
        let uw = ui.as_weak();
        ui.on_sessions_consolidate_selected(move || {
            let ids = selected_session_ids();
            if ids.is_empty() { return; }
            let (base, client, uw) = (base.clone(), Arc::clone(&client), uw.clone());
            h.spawn(async move {
                notify(ToastKind::Info, format!("Consolidating {} session(s) into cerebro…", ids.len()));
                let mut ok = 0;
                for id in &ids {
                    if consolidate_one(&client, &base, *id).await { ok += 1; }
                }
                slint::invoke_from_event_loop(move || {
                    clear_session_selection();
                    if let Some(ui) = uw.upgrade() { ui.set_sessions_selected_count(0); }
                }).ok();
                notify(ToastKind::Success, format!("Consolidated {ok}/{} into cerebro", ids.len()));
            });
        });
    }
    {
        // Consolidate selected → cerebro, THEN delete. A session whose consolidation
        // FAILS is kept (never lose data to a failed extraction).
        let base = http_base.clone();
        let client = Arc::clone(&http_client);
        let h = rt.handle().clone();
        let uw = ui.as_weak();
        ui.on_sessions_consolidate_delete_selected(move || {
            let ids = selected_session_ids();
            if ids.is_empty() { return; }
            let (base, client, uw) = (base.clone(), Arc::clone(&client), uw.clone());
            h.spawn(async move {
                notify(ToastKind::Info, format!("Consolidating {} session(s) before delete…", ids.len()));
                let (mut deleted, mut kept) = (0, 0);
                for id in &ids {
                    if consolidate_one(&client, &base, *id).await && delete_one(&client, &base, *id).await {
                        deleted += 1;
                    } else {
                        kept += 1; // consolidation (or delete) failed → keep the session
                    }
                }
                let items = fetch_sessions(&client, &base).await;
                slint::invoke_from_event_loop(move || {
                    replace_sessions(items);
                    clear_session_selection();
                    if let Some(ui) = uw.upgrade() { ui.set_sessions_selected_count(0); }
                }).ok();
                if kept > 0 {
                    notify(ToastKind::Warn, format!("Saved + deleted {deleted}; kept {kept} (not consolidated)"));
                } else {
                    notify(ToastKind::Success, format!("Consolidated → cerebro + deleted {deleted}"));
                }
            });
        });
    }

    // ── Identity boot wizard (agent-identity.md slice 3d) ─────────────────────
    // Fetch the identity registry; show the wizard only when there's a real
    // choice (>1 profile, a PIN, or >1 agent). The trivial single-owner+APEX
    // case boots straight through unchanged (unbound session = APEX). Picking an
    // agent binds the session via a `hello{agent_id}` frame; the persona first-
    // boot (if any) is revealed underneath.
    {
        // Models live in thread-locals (Slint-thread-owned) so the async fetch
        // carries only Send data and populates them via invoke_from_event_loop.
        let users_model:  Rc<slint::VecModel<UserDef>>  = Rc::new(slint::VecModel::default());
        let agents_model: Rc<slint::VecModel<AgentDef>> = Rc::new(slint::VecModel::default());
        ui.set_identity_users(slint::ModelRc::from(users_model.clone()));
        ui.set_identity_agents(slint::ModelRc::from(agents_model.clone()));
        ID_USERS.with(|m| *m.borrow_mut() = Some(users_model));
        ID_AGENTS.with(|m| *m.borrow_mut() = Some(agents_model));

        // Fetch + gate on boot. WITH an env AGENTD_TOKEN (kiosk/dev) → the identity
        // wizard over the already-authed connection (3d, below). WITHOUT one
        // (desktop/PWA) → LOGIN mode (3e): fetch the UNgated profile list, show the
        // same wizard as a login screen; a pick/OK mints a session token and re-execs
        // with it (the connection task spins harmlessly behind the modal meanwhile).
        {
            let ui_w = ui.as_weak();
            let client = Arc::clone(&http_client);
            let base = http_base.clone();
            let has_token = std::env::var("AGENTD_TOKEN").map(|t| !t.is_empty()).unwrap_or(false);
            rt.handle().spawn(async move {
                if has_token {
                    let v = json_get(&client, format!("{base}/api/identities")).await;
                    let users: Vec<UserRow> = v["users"].as_array().map(|a| a.iter().map(|u| UserRow {
                        id:      u["id"].as_str().unwrap_or("").to_string(),
                        name:    u["name"].as_str().unwrap_or("").to_string(),
                        has_pin: u["has_pin"].as_bool().unwrap_or(false),
                    }).collect()).unwrap_or_default();
                    let agents: Vec<AgentRow> = v["agents"].as_array().map(|a| a.iter().map(|g| AgentRow {
                        id:    g["id"].as_str().unwrap_or("").to_string(),
                        name:  g["name"].as_str().unwrap_or("").to_string(),
                        owner: g["owner"].as_str().unwrap_or("").to_string(),
                    }).collect()).unwrap_or_default();
                    let trivial = users.len() <= 1
                        && users.iter().all(|u| !u.has_pin)
                        && agents.len() <= 1;
                    slint::invoke_from_event_loop(move || {
                        let Some(ui) = ui_w.upgrade() else { return };
                        let user_defs: Vec<UserDef> = users.iter().map(|u| UserDef {
                            id: u.id.clone().into(), name: u.name.clone().into(),
                            has_pin: u.has_pin, glyph: id_glyph(&u.name),
                        }).collect();
                        ID_STATE.with(|s| { let mut s = s.borrow_mut(); s.users = users; s.agents = agents; });
                        if !trivial {
                            ID_USERS.with(|m| { if let Some(model) = m.borrow().as_ref() { model.set_vec(user_defs); } });
                            ui.set_identity_step(0);
                            ui.set_identity_pin_filled(0);
                            ui.set_identity_pin_error(false);
                            ui.set_identity_wizard_open(true);
                        }
                    }).ok();
                } else {
                    // LOGIN mode — ungated profile list. A pick → PIN or immediate login.
                    let v = json_get(&client, format!("{base}/api/auth/profiles")).await;
                    let users: Vec<UserRow> = v["users"].as_array().map(|a| a.iter().map(|u| UserRow {
                        id:      u["id"].as_str().unwrap_or("").to_string(),
                        name:    u["name"].as_str().unwrap_or("").to_string(),
                        has_pin: u["has_pin"].as_bool().unwrap_or(false),
                    }).collect()).unwrap_or_default();

                    // Auto-skip (slice 3e): if a default profile is set, an OPEN one
                    // logs in with zero taps; a PIN one jumps straight to the keypad.
                    let default_user = v["default_user"].as_str().map(|s| s.to_string());
                    let default_profile = default_user.as_ref()
                        .and_then(|du| users.iter().find(|u| &u.id == du).cloned());
                    if let Some(dp) = default_profile.as_ref().filter(|u| !u.has_pin) {
                        // Re-execs on success; only RETURNS on failure → fall through
                        // and show the picker so the user isn't stranded.
                        do_login(&client, &base, dp.id.clone(), String::new(), ui_w.clone()).await;
                    }
                    let pin_default = default_profile.filter(|u| u.has_pin);

                    slint::invoke_from_event_loop(move || {
                        let Some(ui) = ui_w.upgrade() else { return };
                        let user_defs: Vec<UserDef> = users.iter().map(|u| UserDef {
                            id: u.id.clone().into(), name: u.name.clone().into(),
                            has_pin: u.has_pin, glyph: id_glyph(&u.name),
                        }).collect();
                        ID_STATE.with(|s| {
                            let mut s = s.borrow_mut();
                            s.users = users; s.login = true;
                            if let Some(pd) = &pin_default { s.selected = pd.id.clone(); }
                        });
                        ID_USERS.with(|m| { if let Some(model) = m.borrow().as_ref() { model.set_vec(user_defs); } });
                        ui.set_identity_pin_filled(0);
                        ui.set_identity_pin_error(false);
                        ui.set_identity_pin_message("".into());
                        // PIN default → pre-selected keypad (step 1, ‹ Back returns to
                        // the picker); otherwise the profile picker (step 0).
                        if let Some(pd) = pin_default {
                            ui.set_identity_selected_name(pd.name.into());
                            ui.set_identity_step(1);
                        } else {
                            ui.set_identity_step(0);
                        }
                        ui.set_identity_wizard_open(true);
                    }).ok();
                }
            });
        }

        // Pick a profile → PIN step (if protected); else agents (identity mode) or an
        // immediate token mint + re-exec (login mode, open profile = one tap).
        {
            let ui_w = ui.as_weak();
            let client_c = Arc::clone(&http_client);
            let base_c = http_base.clone();
            let rt_h = rt.handle().clone();
            ui.on_identity_pick_user(move |id| {
                let id = id.to_string();
                let Some(ui) = ui_w.upgrade() else { return };
                let (has_pin, name) = ID_STATE.with(|s| {
                    let s = s.borrow();
                    s.users.iter().find(|u| u.id == id)
                        .map(|u| (u.has_pin, u.name.clone()))
                        .unwrap_or((false, id.clone()))
                });
                let login = ID_STATE.with(|s| s.borrow().login);
                ID_STATE.with(|s| { let mut s = s.borrow_mut(); s.selected = id.clone(); s.pin.clear(); });
                ui.set_identity_selected_name(name.into());
                ui.set_identity_pin_filled(0);
                ui.set_identity_pin_error(false);
                ui.set_identity_pin_message("".into());
                if has_pin {
                    ui.set_identity_step(1);
                } else if login {
                    let (client, base, ui_w2) = (Arc::clone(&client_c), base_c.clone(), ui_w.clone());
                    rt_h.spawn(async move { do_login(&client, &base, id, String::new(), ui_w2).await; });
                } else {
                    id_load_agents(&id);
                    ui.set_identity_step(2);
                }
            });
        }

        // PIN keypad (Rust owns the buffer; OK verifies via the API).
        {
            let ui_w = ui.as_weak();
            let client_c = Arc::clone(&http_client);
            let base_c = http_base.clone();
            let rt_h = rt.handle().clone();
            ui.on_identity_key(move |k| {
                let k = k.to_string();
                let Some(ui) = ui_w.upgrade() else { return };
                if k == "DEL" {
                    let n = ID_STATE.with(|s| { let mut s = s.borrow_mut(); s.pin.pop(); s.pin.chars().count() });
                    ui.set_identity_pin_filled(n as i32);
                    ui.set_identity_pin_error(false);
                    ui.set_identity_pin_message("".into());
                } else if k == "OK" {
                    let (user_id, pin) = ID_STATE.with(|s| { let s = s.borrow(); (s.selected.clone(), s.pin.clone()) });
                    let login = ID_STATE.with(|s| s.borrow().login);
                    let ui_w2 = ui_w.clone();
                    let client = Arc::clone(&client_c);
                    let base = base_c.clone();
                    rt_h.spawn(async move {
                        // Login mode (3e): mint a session token + re-exec instead of verify.
                        if login {
                            do_login(&client, &base, user_id, pin, ui_w2).await;
                            return;
                        }
                        let body = serde_json::json!({ "user_id": user_id, "pin": pin });
                        let (ok, locked, retry, reached) = match client.post(format!("{base}/api/identities/verify"))
                            .json(&body)
                            .timeout(std::time::Duration::from_secs(8))
                            .send().await
                        {
                            Ok(r) => {
                                let v = r.json::<Value>().await.unwrap_or(Value::Null);
                                (v["ok"].as_bool().unwrap_or(false),
                                 v["locked"].as_bool().unwrap_or(false),
                                 v["retry_after_secs"].as_u64(),
                                 true)
                            }
                            Err(_) => (false, false, None, false),
                        };
                        slint::invoke_from_event_loop(move || {
                            let Some(ui) = ui_w2.upgrade() else { return };
                            let owner = ID_STATE.with(|s| { let mut s = s.borrow_mut(); s.pin.clear(); s.selected.clone() });
                            ui.set_identity_pin_filled(0);
                            if ok {
                                id_load_agents(&owner);
                                ui.set_identity_pin_error(false);
                                ui.set_identity_pin_message("".into());
                                ui.set_identity_step(2);
                            } else {
                                ui.set_identity_pin_error(true);
                                let msg = if !reached {
                                    "Can't reach agentd — try again".to_string()
                                } else if locked {
                                    match retry {
                                        Some(s) => format!("Too many tries — locked {s}s"),
                                        None    => "Too many tries — locked".to_string(),
                                    }
                                } else {
                                    "Wrong PIN — try again".to_string()
                                };
                                ui.set_identity_pin_message(msg.into());
                            }
                        }).ok();
                    });
                } else {
                    let n = ID_STATE.with(|s| {
                        let mut s = s.borrow_mut();
                        if s.pin.chars().count() < 6 { s.pin.push_str(&k); }
                        s.pin.chars().count()
                    });
                    ui.set_identity_pin_filled(n as i32);
                    ui.set_identity_pin_error(false);
                    ui.set_identity_pin_message("".into());
                }
            });
        }

        // Pick an agent → bind the session (hello{agent_id}) + dismiss.
        {
            let ui_w = ui.as_weak();
            let tx_c = tx.clone();
            ui.on_identity_pick_agent(move |id| {
                let payload = serde_json::json!({ "type": "hello", "agent_id": id.to_string() }).to_string();
                tx_c.send(payload).ok();
                if let Some(ui) = ui_w.upgrade() {
                    ui.set_identity_wizard_open(false);
                }
            });
        }

        // Back → profile select.
        {
            let ui_w = ui.as_weak();
            ui.on_identity_back(move || {
                let Some(ui) = ui_w.upgrade() else { return };
                ID_STATE.with(|s| s.borrow_mut().pin.clear());
                ui.set_identity_pin_filled(0);
                ui.set_identity_pin_error(false);
                ui.set_identity_pin_message("".into());
                ui.set_identity_step(0);
            });
        }
    }

    // ── toggle-recording callback ─────────────────────────────────────────────
    // First tap  → POST /api/record/start → set recording=true
    // Second tap → POST /api/record/stop  → whisper transcription → auto-send
    let rt_h_rec     = rt.handle().clone();
    let client_rec   = Arc::clone(&http_client);
    let base_rec     = http_base.clone();
    let ui_weak_rec  = ui.as_weak();
    let tx_rec       = tx.clone();
    ui.on_toggle_recording(move || {
        let currently_recording = ui_weak_rec.upgrade()
            .map(|u| u.get_recording())
            .unwrap_or(false);
        let client = Arc::clone(&client_rec);
        let base   = base_rec.clone();
        let ui_w   = ui_weak_rec.clone();
        let tx     = tx_rec.clone();
        let rt_h   = rt_h_rec.clone();
        if !currently_recording {
            rt_h.spawn(async move {
                let ok = mic_record_start();
                slint::invoke_from_event_loop(move || {
                    if let Some(ui) = ui_w.upgrade() {
                        if ok { ui.set_recording(true); ui.set_face_state("listening".into()); }
                        else  { toast(ToastKind::Error, "Microphone unavailable"); }
                    }
                }).ok();
            });
        } else {
            rt_h.spawn(async move {
                let text = mic_stop_and_transcribe(&client, &base).await;
                slint::invoke_from_event_loop(move || {
                    if let Some(ui) = ui_w.upgrade() {
                        ui.set_recording(false);
                        if !ui.get_agent_busy() { ui.set_face_state("idle".into()); }
                        if !text.is_empty() {
                            maybe_push_time_divider();
                            push_message(MessageItem {
                                role: "user".into(),
                                text: text.clone().into(),
                                streaming: false,
                                call_id: "".into(),
                                tool_name: "".into(),
                                tool_args: "".into(),
                                tool_output: "".into(),
                                tool_status: "".into(),
                                awaiting_approval: false,
                            });
                            let payload = serde_json::json!({"type":"user_prompt","text":&text}).to_string();
                            tx.send(payload).ok();
                            bump_scroll(&ui);
                        }
                    }
                }).ok();
            });
        }
    });

    // ── toggle-tts callback ───────────────────────────────────────────────────
    let tts_flag    = Arc::clone(&tts_enabled);
    let ui_weak_tts = ui.as_weak();
    ui.on_toggle_tts(move || {
        let new_val = !tts_flag.load(Ordering::SeqCst);
        tts_flag.store(new_val, Ordering::SeqCst);
        if let Some(ui) = ui_weak_tts.upgrade() {
            ui.set_tts_enabled(new_val);
        }
    });

    // ── refresh-settings callback ─────────────────────────────────────────────
    let rt_h_stg   = rt.handle().clone();
    let client_stg = Arc::clone(&http_client);
    let base_stg   = http_base.clone();
    let ui_weak_stg = ui.as_weak();
    ui.on_refresh_settings(move || {
        let client = Arc::clone(&client_stg);
        let base   = base_stg.clone();
        let ui_w   = ui_weak_stg.clone();
        rt_h_stg.spawn(async move {
            let data = fetch_settings(&client, &base).await;
            // Slice 3e: who am I (session-token login) + is my profile this device's
            // auto-login default? `me.user_id` is null for the admin/device token.
            let me = json_get(&client, format!("{base}/api/auth/me")).await;
            let me_id   = me["user_id"].as_str().unwrap_or("").to_string();
            let me_name = me["name"].as_str().unwrap_or("").to_string();
            let default_user = json_get(&client, format!("{base}/api/auth/profiles")).await
                ["default_user"].as_str().unwrap_or("").to_string();
            let is_default = !me_id.is_empty() && me_id == default_user;
            slint::invoke_from_event_loop(move || {
                if let Some(ui) = ui_w.upgrade() {
                    ui.set_soul_text(data.soul_text.into());
                    ui.set_settings_policy(data.policy_mode.into());
                    ui.set_settings_model(data.current_model.into());
                    ui.set_settings_api_key_set(data.api_key_set);
                    ui.set_settings_cache_enabled(data.cache_enabled);
                    ui.set_settings_cache_conversation(data.cache_conversation);
                    ui.set_settings_cache_ttl(data.cache_ttl.into());
                    ui.set_settings_sensor_profile(data.sensor_profile.into());
                    ui.set_settings_voice_backend(data.voice_backend.into());
                    ui.set_settings_voice_api_available(data.voice_api_available);
                    ui.set_settings_backend(data.backend.into());
                    ui.set_settings_oai_url(data.oai_base_url.into());
                    ui.set_settings_oai_key_set(data.oai_key_set);
                    LOGIN_ME.with(|m| *m.borrow_mut() = me_id);
                    ui.set_settings_login_user_name(me_name.into());
                    ui.set_settings_login_is_default(is_default);
                    set_models_full(data.models);
                }
            }).ok();
        });
    });

    // Slice 3e: set/clear this device's auto-login default = the logged-in profile.
    let rt_h_dl   = rt.handle().clone();
    let client_dl = Arc::clone(&http_client);
    let base_dl   = http_base.clone();
    let ui_weak_dl = ui.as_weak();
    ui.on_set_default_login(move |enabled| {
        let me = LOGIN_ME.with(|m| m.borrow().clone());
        if me.is_empty() { return; }   // admin/device token — no profile to default
        let user_id = if enabled { me } else { String::new() };
        let client = Arc::clone(&client_dl);
        let base   = base_dl.clone();
        let ui_w   = ui_weak_dl.clone();
        rt_h_dl.spawn(async move {
            let ok = client.post(format!("{base}/api/auth/default"))
                .json(&serde_json::json!({ "user_id": user_id }))
                .timeout(std::time::Duration::from_secs(8))
                .send().await
                .map(|r| r.status().is_success())
                .unwrap_or(false);
            if ok {
                notify(ToastKind::Success, if enabled { "Auto-login set for this device" } else { "Auto-login cleared" });
                slint::invoke_from_event_loop(move || {
                    if let Some(ui) = ui_w.upgrade() { ui.set_settings_login_is_default(enabled); }
                }).ok();
            } else {
                notify(ToastKind::Error, "Couldn't update auto-login");
            }
        });
    });

    // ── Tier-A parity apps: refresh + mesh peer actions ───────────────────────
    let rt_h_ev    = rt.handle().clone();
    let client_ev  = Arc::clone(&http_client);
    let base_ev    = http_base.clone();
    ui.on_refresh_events(move |types, hours| {
        let client = Arc::clone(&client_ev);
        let base   = base_ev.clone();
        rt_h_ev.spawn(async move {
            let items = fetch_events(&client, &base, types.as_str(), hours).await;
            slint::invoke_from_event_loop(move || replace_events(items)).ok();
        });
    });

    let rt_h_mesh   = rt.handle().clone();
    let client_mesh = Arc::clone(&http_client);
    let base_mesh   = http_base.clone();
    ui.on_refresh_mesh(move || {
        let client = Arc::clone(&client_mesh);
        let base   = base_mesh.clone();
        rt_h_mesh.spawn(async move {
            let items = fetch_mesh(&client, &base).await;
            slint::invoke_from_event_loop(move || replace_mesh(items)).ok();
        });
    });

    // One-shot at launch: seed the inbox from agentd's persisted unread so the
    // badge + threads survive a restart. The live `mesh_message` stream then drives
    // it as before (the server also persisted each, so the two stay in step).
    {
        let client = Arc::clone(&http_client);
        let base   = http_base.clone();
        rt.handle().spawn(async move {
            let rows = fetch_inbox(&client, &base).await;
            slint::invoke_from_event_loop(move || seed_inbox(rows)).ok();
        });
    }

    // Tap a mesh inbox thread → clear its unread + restore the peer's session
    // (the exact replay path the notification click uses).
    {
        let uw = ui.as_weak();
        let rt_h_read = rt.handle().clone();
        let client_read = Arc::clone(&http_client);
        let base_read = http_base.clone();
        ui.on_open_mesh_thread(move |session| {
            inbox_clear_session(session);
            // Persist the read so the cleared unread survives a restart.
            let client = Arc::clone(&client_read);
            let base   = base_read.clone();
            rt_h_read.spawn(async move {
                let _ = client.post(format!("{base}/api/mesh/inbox/read"))
                    .json(&serde_json::json!({ "session": session as u64 }))
                    .timeout(std::time::Duration::from_secs(8))
                    .send().await;
            });
            if let Some(ui) = uw.upgrade() {
                ui.invoke_restore_session(session);
            }
        });
    }

    let rt_h_addp    = rt.handle().clone();
    let client_addp  = Arc::clone(&http_client);
    let base_addp    = http_base.clone();
    ui.on_add_peer(move |node_id, ws_url, token| {
        let client = Arc::clone(&client_addp);
        let base   = base_addp.clone();
        let id     = node_id.to_string();
        let url    = ws_url.to_string();
        let tok    = token.trim().to_string();
        rt_h_addp.spawn(async move {
            // token is the peer's AGENTD_TOKEN, needed for cross-node a2a. Optional —
            // omit for an auth-disabled peer. Send it only when non-empty.
            let mut body = serde_json::json!({"node_id": id, "ws_url": url});
            if !tok.is_empty() { body["token"] = serde_json::Value::String(tok); }
            // The handler returns {ok:false} as HTTP 200, so check the body, not the
            // status — otherwise a failed save() (e.g. EPERM on peers.toml) would
            // flash "Peer added" while the row never moves to saved.
            let ok = match client.post(format!("{base}/api/mesh/peers"))
                .json(&body)
                .timeout(std::time::Duration::from_secs(8))
                .send().await
            {
                Ok(r)  => r.json::<serde_json::Value>().await
                            .map(|v| v["ok"].as_bool().unwrap_or(false))
                            .unwrap_or(false),
                Err(_) => false,
            };
            if ok { notify(ToastKind::Success, "Peer added"); }
            else  { notify(ToastKind::Error, "Failed to add peer"); }
            // Re-scan so the row moves from discovered → saved.
            let items = fetch_mesh(&client, &base).await;
            slint::invoke_from_event_loop(move || replace_mesh(items)).ok();
        });
    });

    let rt_h_rmp    = rt.handle().clone();
    let client_rmp  = Arc::clone(&http_client);
    let base_rmp    = http_base.clone();
    ui.on_remove_peer(move |node_id| {
        let client = Arc::clone(&client_rmp);
        let base   = base_rmp.clone();
        let id     = node_id.to_string();
        rt_h_rmp.spawn(async move {
            let ok = match client.delete(format!("{base}/api/mesh/peers/{id}"))
                .timeout(std::time::Duration::from_secs(8))
                .send().await
            {
                Ok(r)  => r.json::<serde_json::Value>().await
                            .map(|v| v["ok"].as_bool().unwrap_or(false))
                            .unwrap_or(false),
                Err(_) => false,
            };
            if ok { notify(ToastKind::Info, "Peer removed"); }
            else  { notify(ToastKind::Error, "Failed to remove peer"); }
            let items = fetch_mesh(&client, &base).await;
            slint::invoke_from_event_loop(move || replace_mesh(items)).ok();
        });
    });

    // PAIR (host): generate a code on THIS node, show it for another node to enter.
    let rt_h_spair    = rt.handle().clone();
    let client_spair  = Arc::clone(&http_client);
    let base_spair    = http_base.clone();
    let ui_weak_spair = ui.as_weak();
    ui.on_start_pairing(move || {
        let client = Arc::clone(&client_spair);
        let base   = base_spair.clone();
        let ui_w   = ui_weak_spair.clone();
        rt_h_spair.spawn(async move {
            let (code, ttl) = match client.post(format!("{base}/api/mesh/pair/start"))
                .timeout(std::time::Duration::from_secs(8))
                .send().await
            {
                Ok(r) => {
                    let v = r.json::<serde_json::Value>().await.unwrap_or_default();
                    (v["code"].as_str().unwrap_or("").to_string(),
                     v["ttl_secs"].as_i64().unwrap_or(300) as i32)
                }
                Err(_) => (String::new(), 0),
            };
            if code.is_empty() { notify(ToastKind::Error, "Couldn't start pairing"); return; }
            slint::invoke_from_event_loop(move || {
                if let Some(ui) = ui_w.upgrade() {
                    ui.set_mesh_pair_code(code.into());
                    ui.set_mesh_pair_remaining(ttl);
                }
            }).ok();
        });
    });

    // Redeem a pairing code shown on a discovered peer (exchanges tokens both ways).
    let rt_h_rdm    = rt.handle().clone();
    let client_rdm  = Arc::clone(&http_client);
    let base_rdm    = http_base.clone();
    ui.on_redeem_pairing(move |ws_url, code| {
        let client = Arc::clone(&client_rdm);
        let base   = base_rdm.clone();
        let url    = ws_url.to_string();
        let code   = code.trim().to_string();
        rt_h_rdm.spawn(async move {
            let ok = match client.post(format!("{base}/api/mesh/pair/redeem"))
                .json(&serde_json::json!({"ws_url": url, "code": code}))
                .timeout(std::time::Duration::from_secs(12))
                .send().await
            {
                Ok(r)  => r.json::<serde_json::Value>().await
                            .map(|v| v["ok"].as_bool().unwrap_or(false))
                            .unwrap_or(false),
                Err(_) => false,
            };
            if ok { notify(ToastKind::Success, "Paired — peer added"); }
            else  { notify(ToastKind::Error, "Pairing failed (bad or expired code?)"); }
            let items = fetch_mesh(&client, &base).await;
            slint::invoke_from_event_loop(move || replace_mesh(items)).ok();
        });
    });

    let rt_h_inf    = rt.handle().clone();
    let client_inf  = Arc::clone(&http_client);
    let base_inf    = http_base.clone();
    let ui_weak_inf = ui.as_weak();
    ui.on_refresh_inference(move || {
        let client = Arc::clone(&client_inf);
        let base   = base_inf.clone();
        let ui_w   = ui_weak_inf.clone();
        rt_h_inf.spawn(async move {
            let data = fetch_inference(&client, &base).await;
            slint::invoke_from_event_loop(move || {
                if let Some(ui) = ui_w.upgrade() {
                    ui.set_inference_backend(data.backend.into());
                    ui.set_inference_base_url(data.base_url.into());
                    ui.set_inference_usage(data.usage);
                    replace_infer_models(data.models);
                }
            }).ok();
        });
    });

    // ── Audio Editor (🎛️) — list / select (waveform+analyze) / process ─────────
    let rt_h_audio    = rt.handle().clone();
    let client_audio  = Arc::clone(&http_client);
    let base_audio    = http_base.clone();
    ui.on_refresh_audio(move || {
        let client = Arc::clone(&client_audio);
        let base   = base_audio.clone();
        rt_h_audio.spawn(async move {
            let items = fetch_audio_files(&client, &base).await;
            slint::invoke_from_event_loop(move || replace_audio_files(items)).ok();
        });
    });

    let rt_h_asel    = rt.handle().clone();
    let client_asel  = Arc::clone(&http_client);
    let base_asel    = http_base.clone();
    let ui_weak_asel = ui.as_weak();
    ui.on_select_audio(move |path, name| {
        let client = Arc::clone(&client_asel);
        let base   = base_asel.clone();
        let ui_w   = ui_weak_asel.clone();
        let p      = path.to_string();
        // Immediate UI feedback: set selection, clear stale waveform, mark busy.
        if let Some(ui) = ui_w.upgrade() {
            ui.set_audio_selected_path(path.clone());
            ui.set_audio_selected_name(name.clone());
            ui.set_audio_stats("".into());
            ui.set_audio_duration("".into());
            ui.set_audio_busy(true);
        }
        replace_waveform(Vec::new());
        rt_h_asel.spawn(async move {
            let (samples, dur) = fetch_waveform(&client, &base, &p).await;
            let stats = fetch_audio_stats(&client, &base, &p).await;
            slint::invoke_from_event_loop(move || {
                replace_waveform(samples);
                if let Some(ui) = ui_w.upgrade() {
                    ui.set_audio_duration(dur.into());
                    ui.set_audio_stats(stats.into());
                    ui.set_audio_busy(false);
                }
            }).ok();
        });
    });

    let rt_h_aproc    = rt.handle().clone();
    let client_aproc  = Arc::clone(&http_client);
    let base_aproc    = http_base.clone();
    let ui_weak_aproc = ui.as_weak();
    ui.on_process_audio(move |path, op| {
        let ops = audio_op_chain(&op);
        if ops.is_empty() { return; }
        let client = Arc::clone(&client_aproc);
        let base   = base_aproc.clone();
        let ui_w   = ui_weak_aproc.clone();
        let p      = path.to_string();
        if let Some(ui) = ui_w.upgrade() { ui.set_audio_busy(true); }
        rt_h_aproc.spawn(async move {
            let resp = client.post(format!("{base}/api/audio/process"))
                .json(&serde_json::json!({"path": p, "ops": ops}))
                .timeout(std::time::Duration::from_secs(120))
                .send().await;
            let body: Value = match resp {
                Ok(r) => r.json().await.unwrap_or(Value::Null),
                Err(_) => Value::Null,
            };
            let ok = body["output_path"].as_str().is_some();
            if ok { notify(ToastKind::Success, "Audio processed → _edit file"); }
            else  { notify(ToastKind::Error, "Audio processing failed"); }
            // Re-scan so the new _edit file appears in the list.
            let items = fetch_audio_files(&client, &base).await;
            slint::invoke_from_event_loop(move || {
                replace_audio_files(items);
                if let Some(ui) = ui_w.upgrade() { ui.set_audio_busy(false); }
            }).ok();
        });
    });

    // ── Sonus player (🎵) — list / play (server-side) / stop ───────────────────
    let rt_h_son    = rt.handle().clone();
    let client_son  = Arc::clone(&http_client);
    let base_son    = http_base.clone();
    ui.on_refresh_sonus(move || {
        let client = Arc::clone(&client_son);
        let base   = base_son.clone();
        rt_h_son.spawn(async move {
            let items = fetch_sonus_files(&client, &base).await;
            slint::invoke_from_event_loop(move || replace_sonus_files(items)).ok();
        });
    });

    let rt_h_splay    = rt.handle().clone();
    let client_splay  = Arc::clone(&http_client);
    let base_splay    = http_base.clone();
    let ui_weak_splay = ui.as_weak();
    ui.on_play_sonus(move |name| {
        let client = Arc::clone(&client_splay);
        let base   = base_splay.clone();
        let ui_w   = ui_weak_splay.clone();
        let n      = name.to_string();
        // Optimistic now-playing; cleared if the server rejects it.
        if let Some(ui) = ui_w.upgrade() { ui.set_sonus_now_playing(name.clone()); }
        rt_h_splay.spawn(async move {
            let ok = client.post(format!("{base}/api/sonus/play"))
                .json(&serde_json::json!({"name": n}))
                .timeout(std::time::Duration::from_secs(8))
                .send().await
                .map(|r| r.status().is_success())
                .unwrap_or(false);
            if !ok {
                notify(ToastKind::Error, "Playback failed (ffplay/track missing?)");
                slint::invoke_from_event_loop(move || {
                    if let Some(ui) = ui_w.upgrade() { ui.set_sonus_now_playing("".into()); }
                }).ok();
            }
        });
    });

    let rt_h_sstop    = rt.handle().clone();
    let client_sstop  = Arc::clone(&http_client);
    let base_sstop    = http_base.clone();
    let ui_weak_sstop = ui.as_weak();
    ui.on_stop_sonus(move || {
        let client = Arc::clone(&client_sstop);
        let base   = base_sstop.clone();
        let ui_w   = ui_weak_sstop.clone();
        if let Some(ui) = ui_w.upgrade() { ui.set_sonus_now_playing("".into()); }
        rt_h_sstop.spawn(async move {
            let _ = client.post(format!("{base}/api/sonus/stop"))
                .timeout(std::time::Duration::from_secs(8))
                .send().await;
        });
    });

    // ── Notes callbacks ───────────────────────────────────────────────────────
    let rt_h_nref   = rt.handle().clone();
    let client_nref = Arc::clone(&http_client);
    let base_nref   = http_base.clone();
    ui.on_refresh_notes(move || {
        let client = Arc::clone(&client_nref);
        let base   = base_nref.clone();
        rt_h_nref.spawn(async move {
            let items = fetch_notes(&client, &base).await;
            slint::invoke_from_event_loop(move || replace_notes_files(items)).ok();
        });
    });

    // Image attach (🖼): refresh the workspace-image picker on demand.
    let rt_h_wsimg   = rt.handle().clone();
    let client_wsimg = Arc::clone(&http_client);
    let base_wsimg   = http_base.clone();
    ui.on_refresh_workspace_images(move || {
        let client = Arc::clone(&client_wsimg);
        let base   = base_wsimg.clone();
        rt_h_wsimg.spawn(async move {
            let items = fetch_workspace_images(&client, &base).await;
            slint::invoke_from_event_loop(move || replace_workspace_images(items)).ok();
        });
    });

    // ── Explorer (📁 Files) ───────────────────────────────────────────────────
    // refresh: re-list the current directory.
    let rt_h_exr    = rt.handle().clone();
    let client_exr  = Arc::clone(&http_client);
    let base_exr    = http_base.clone();
    let ui_weak_exr = ui.as_weak();
    ui.on_refresh_explorer(move || {
        let client = Arc::clone(&client_exr);
        let base   = base_exr.clone();
        let path   = ui_weak_exr.upgrade().map(|ui| ui.get_explorer_current_path().to_string()).unwrap_or_default();
        rt_h_exr.spawn(async move {
            let items = fetch_explorer_list(&client, &base, &path).await;
            slint::invoke_from_event_loop(move || replace_explorer_entries(items)).ok();
        });
    });

    // navigate: enter a directory (clears any selection).
    let rt_h_exn    = rt.handle().clone();
    let client_exn  = Arc::clone(&http_client);
    let base_exn    = http_base.clone();
    let ui_weak_exn = ui.as_weak();
    ui.on_explorer_navigate(move |path| {
        let client = Arc::clone(&client_exn);
        let base   = base_exn.clone();
        let p      = path.to_string();
        if let Some(ui) = ui_weak_exn.upgrade() {
            ui.set_explorer_current_path(path.clone());
            ui.set_explorer_selected_path("".into());
            ui.set_explorer_selected_name("".into());
            ui.set_explorer_selected_info("".into());
            ui.set_explorer_preview_kind("none".into());
            ui.set_explorer_preview_text("".into());
            ui.set_explorer_can_attach(false);
        }
        rt_h_exn.spawn(async move {
            let items = fetch_explorer_list(&client, &base, &p).await;
            slint::invoke_from_event_loop(move || replace_explorer_entries(items)).ok();
        });
    });

    // up: navigate to the parent of the current directory.
    let ui_weak_exu = ui.as_weak();
    ui.on_explorer_up(move || {
        if let Some(ui) = ui_weak_exu.upgrade() {
            let cur = ui.get_explorer_current_path().to_string();
            if cur.is_empty() { return; }
            let parent = cur.rsplit_once('/').map(|(p, _)| p.to_string()).unwrap_or_default();
            ui.invoke_explorer_navigate(parent.into());
        }
    });

    // eject: safe-unmount an exo-workspace stick (POST /api/media/eject {label}); on
    // success refresh the Explorer so the now-gone stick disappears from media/.
    let rt_h_exe    = rt.handle().clone();
    let client_exe  = Arc::clone(&http_client);
    let base_exe    = http_base.clone();
    let ui_weak_exe = ui.as_weak();
    ui.on_explorer_eject(move |label| {
        let label  = label.to_string();
        let client = Arc::clone(&client_exe);
        let base   = base_exe.clone();
        let uw     = ui_weak_exe.clone();
        rt_h_exe.spawn(async move {
            let ok = match client.post(format!("{base}/api/media/eject"))
                .json(&serde_json::json!({ "label": label }))
                .timeout(std::time::Duration::from_secs(15))
                .send().await
            {
                Ok(r) => r.json::<serde_json::Value>().await.ok()
                    .and_then(|v| v["ok"].as_bool()).unwrap_or(false),
                Err(_) => false,
            };
            slint::invoke_from_event_loop(move || {
                if let Some(ui) = uw.upgrade() {
                    if ok { ui.invoke_refresh_explorer(); }
                    ui.set_status(if ok { format!("Ejected {label} — safe to remove") }
                                  else   { format!("Eject failed: {label}") }.into());
                }
            }).ok();
        });
    });

    // mkdir: create a new folder in the current directory (POST /api/workspace/mkdir).
    let rt_h_exmk    = rt.handle().clone();
    let client_exmk  = Arc::clone(&http_client);
    let base_exmk    = http_base.clone();
    let ui_weak_exmk = ui.as_weak();
    ui.on_explorer_mkdir(move |name| {
        let name   = name.to_string();
        let client = Arc::clone(&client_exmk);
        let base   = base_exmk.clone();
        let uw     = ui_weak_exmk.clone();
        let cur    = uw.upgrade().map(|ui| ui.get_explorer_current_path().to_string()).unwrap_or_default();
        let path   = if cur.is_empty() { name.clone() } else { format!("{cur}/{name}") };
        rt_h_exmk.spawn(async move {
            let (ok, err) = workspace_op(&client, &base, "mkdir", serde_json::json!({ "path": path })).await;
            slint::invoke_from_event_loop(move || {
                if let Some(ui) = uw.upgrade() {
                    if ok { ui.invoke_refresh_explorer(); }
                    ui.set_status(if ok { format!("Created folder {name}") }
                                  else   { format!("New folder failed: {err}") }.into());
                }
            }).ok();
        });
    });

    // rename: rename an entry in place (POST /api/workspace/rename {path, name}).
    let rt_h_exrn    = rt.handle().clone();
    let client_exrn  = Arc::clone(&http_client);
    let base_exrn    = http_base.clone();
    let ui_weak_exrn = ui.as_weak();
    ui.on_explorer_rename(move |path, name| {
        let path   = path.to_string();
        let name   = name.to_string();
        let client = Arc::clone(&client_exrn);
        let base   = base_exrn.clone();
        let uw     = ui_weak_exrn.clone();
        rt_h_exrn.spawn(async move {
            let (ok, err) = workspace_op(&client, &base, "rename", serde_json::json!({ "path": path, "name": name })).await;
            slint::invoke_from_event_loop(move || {
                if let Some(ui) = uw.upgrade() {
                    if ok { ui.invoke_refresh_explorer(); }
                    ui.set_status(if ok { format!("Renamed to {name}") }
                                  else   { format!("Rename failed: {err}") }.into());
                }
            }).ok();
        });
    });

    // delete: remove a file/folder (POST /api/workspace/delete {path}); clears the
    // preview if the deleted entry was the one on show.
    let rt_h_exdl    = rt.handle().clone();
    let client_exdl  = Arc::clone(&http_client);
    let base_exdl    = http_base.clone();
    let ui_weak_exdl = ui.as_weak();
    ui.on_explorer_delete(move |path| {
        let path   = path.to_string();
        let name   = path.rsplit('/').next().unwrap_or(&path).to_string();
        let client = Arc::clone(&client_exdl);
        let base   = base_exdl.clone();
        let uw     = ui_weak_exdl.clone();
        rt_h_exdl.spawn(async move {
            let (ok, err) = workspace_op(&client, &base, "delete", serde_json::json!({ "path": path })).await;
            slint::invoke_from_event_loop(move || {
                if let Some(ui) = uw.upgrade() {
                    if ok {
                        ui.invoke_refresh_explorer();
                        if path == ui.get_explorer_selected_path().as_str() {
                            ui.set_explorer_selected_path("".into());
                            ui.set_explorer_selected_name("".into());
                            ui.set_explorer_selected_info("".into());
                            ui.set_explorer_preview_kind("none".into());
                            ui.set_explorer_preview_text("".into());
                            ui.set_explorer_can_attach(false);
                        }
                    }
                    ui.set_status(if ok { format!("Deleted {name}") }
                                  else   { format!("Delete failed: {err}") }.into());
                }
            }).ok();
        });
    });

    // paste: move (cut) or copy the clipboard entry into the current directory
    // (POST /api/workspace/{move,copy} {src, dest}). dest = the folder in view.
    let rt_h_expt    = rt.handle().clone();
    let client_expt  = Arc::clone(&http_client);
    let base_expt    = http_base.clone();
    let ui_weak_expt = ui.as_weak();
    ui.on_explorer_paste(move |src, mode| {
        let src      = src.to_string();
        let mode     = mode.to_string();
        let name     = src.rsplit('/').next().unwrap_or(&src).to_string();
        let client   = Arc::clone(&client_expt);
        let base     = base_expt.clone();
        let uw        = ui_weak_expt.clone();
        let dest     = uw.upgrade().map(|ui| ui.get_explorer_current_path().to_string()).unwrap_or_default();
        let endpoint = if mode == "cut" { "move" } else { "copy" };
        let verb     = if mode == "cut" { "Moved" } else { "Copied" };
        rt_h_expt.spawn(async move {
            let (ok, err) = workspace_op(&client, &base, endpoint, serde_json::json!({ "src": src, "dest": dest })).await;
            slint::invoke_from_event_loop(move || {
                if let Some(ui) = uw.upgrade() {
                    if ok { ui.invoke_refresh_explorer(); }
                    ui.set_status(if ok { format!("{verb} {name}") }
                                  else   { format!("Paste failed: {err}") }.into());
                }
            }).ok();
        });
    });

    // drive-scan: (re)list the USB sticks "Use this drive" can adopt → the picker model.
    let rt_h_exds    = rt.handle().clone();
    let client_exds  = Arc::clone(&http_client);
    let base_exds    = http_base.clone();
    ui.on_explorer_drive_scan(move |mode| {
        let client = Arc::clone(&client_exds);
        let base   = base_exds.clone();
        let mode   = mode.to_string();
        rt_h_exds.spawn(async move {
            let items = fetch_drive_candidates(&client, &base, &mode).await;
            slint::invoke_from_event_loop(move || replace_drive_candidates(items)).ok();
        });
    });

    // drive-prep: adopt the picked stick as an exo-workspace (POST /api/media/prep,
    // relabel mode). Shows a busy state for the ≤25s prep; on success the picker
    // auto-closes (the view's `changed drive-result` handler), and we hop to media/.
    let rt_h_exdp    = rt.handle().clone();
    let client_exdp  = Arc::clone(&http_client);
    let base_exdp    = http_base.clone();
    let ui_weak_exdp = ui.as_weak();
    ui.on_explorer_drive_prep(move |dev, name, mode| {
        let dev    = dev.to_string();
        let name   = name.to_string();
        let mode   = mode.to_string();
        let client = Arc::clone(&client_exdp);
        let base   = base_exdp.clone();
        let uw     = ui_weak_exdp.clone();
        if let Some(ui) = uw.upgrade() {
            ui.set_explorer_drive_busy(true);
            ui.set_explorer_drive_result("".into());
        }
        rt_h_exdp.spawn(async move {
            let resp: Value = match client.post(format!("{base}/api/media/prep"))
                .json(&serde_json::json!({ "dev": dev, "name": name, "mode": mode }))
                .timeout(std::time::Duration::from_secs(35))
                .send().await
            {
                Ok(r) => r.json().await.unwrap_or(Value::Null),
                Err(e) => serde_json::json!({ "ok": false, "error": format!("request failed: {e}") }),
            };
            let ok    = resp["ok"].as_bool().unwrap_or(false);
            let label = resp["label"].as_str().unwrap_or("").to_string();
            let err   = resp["error"].as_str().unwrap_or("prep failed").to_string();
            slint::invoke_from_event_loop(move || {
                if let Some(ui) = uw.upgrade() {
                    ui.set_explorer_drive_busy(false);
                    if ok {
                        ui.set_explorer_drive_result("ok".into());   // view auto-closes the picker + confirm
                        ui.set_status(format!("Drive ready: {label} (in media/)").into());
                        ui.invoke_explorer_navigate("media".into());  // show the freshly-adopted stick
                    } else {
                        ui.set_explorer_drive_result(format!("Couldn't set up the drive: {err}").into());
                    }
                }
            }).ok();
        });
    });

    // select: a file was clicked — load its preview (image from abs path; text via
    // the read endpoint; otherwise binary/no-preview).
    let rt_h_exs    = rt.handle().clone();
    let client_exs  = Arc::clone(&http_client);
    let base_exs    = http_base.clone();
    let ui_weak_exs = ui.as_weak();
    ui.on_explorer_select(move |path, abs, ext| {
        let p    = path.to_string();
        let a    = abs.to_string();
        let e    = ext.to_string();
        let name = p.rsplit('/').next().unwrap_or(&p).to_string();
        let Some(ui) = ui_weak_exs.upgrade() else { return };
        ui.set_explorer_selected_path(path.clone());
        ui.set_explorer_selected_name(name.into());

        if is_image_ext(&e) {
            // Load directly from the absolute path (UI + agentd co-located).
            match slint::Image::load_from_path(std::path::Path::new(&a)) {
                Ok(img) => {
                    let sz = img.size();
                    ui.set_explorer_preview_image(img);
                    ui.set_explorer_preview_kind("image".into());
                    ui.set_explorer_selected_info(format!("{} · {}×{}", e.to_uppercase(), sz.width, sz.height).into());
                }
                Err(_) => {
                    ui.set_explorer_preview_kind("binary".into());
                    ui.set_explorer_selected_info(format!("{} image (no preview)", e.to_uppercase()).into());
                }
            }
            ui.set_explorer_preview_text("".into());
            ui.set_explorer_can_attach(true);
        } else {
            ui.set_explorer_can_attach(false);
            ui.set_explorer_selected_info(if e.is_empty() { "file".into() } else { format!("{} file", e.to_uppercase()).into() });
            let client = Arc::clone(&client_exs);
            let base   = base_exs.clone();
            let uw     = ui_weak_exs.clone();
            rt_h_exs.spawn(async move {
                let (content, binary) = fetch_explorer_read(&client, &base, &p).await;
                slint::invoke_from_event_loop(move || {
                    if let Some(ui) = uw.upgrade() {
                        if binary {
                            ui.set_explorer_preview_kind("binary".into());
                            ui.set_explorer_preview_text("".into());
                        } else {
                            ui.set_explorer_preview_text(content.into());
                            ui.set_explorer_preview_kind("text".into());
                        }
                    }
                }).ok();
            });
        }
    });

    // attach: stage the selected image into the chat composer (reuses the 🖼 flow).
    let ui_weak_exa = ui.as_weak();
    ui.on_explorer_attach(move || {
        if let Some(ui) = ui_weak_exa.upgrade() {
            let path = ui.get_explorer_selected_path().to_string();
            let name = ui.get_explorer_selected_name().to_string();
            if path.is_empty() { return; }
            ui.set_staged_image_path(path.into());
            ui.set_staged_image_name(name.into());
            ui.set_current_view(0); // focus mode → chat (desktop shows the chip in-place)
            notify(ToastKind::Success, "Image attached — open Chat and send");
        }
    });

    let rt_h_nopen    = rt.handle().clone();
    let client_nopen  = Arc::clone(&http_client);
    let base_nopen    = http_base.clone();
    let ui_weak_nopen = ui.as_weak();
    ui.on_open_note(move |name| {
        let client = Arc::clone(&client_nopen);
        let base   = base_nopen.clone();
        let ui_w   = ui_weak_nopen.clone();
        let n      = name.to_string();
        rt_h_nopen.spawn(async move {
            let content = fetch_note_content(&client, &base, &n).await;
            slint::invoke_from_event_loop(move || {
                if let Some(ui) = ui_w.upgrade() {
                    ui.set_notes_current_name(n.into());
                    ui.set_notes_current_text(content.into());
                }
            }).ok();
        });
    });

    let rt_h_nsave   = rt.handle().clone();
    let client_nsave = Arc::clone(&http_client);
    let base_nsave   = http_base.clone();
    ui.on_save_note(move |name, text| {
        let client  = Arc::clone(&client_nsave);
        let base    = base_nsave.clone();
        let n       = name.to_string();
        let content = text.to_string();
        rt_h_nsave.spawn(async move {
            let ok = client.post(format!("{base}/api/notes/write"))
                .json(&serde_json::json!({ "name": n, "content": content }))
                .timeout(std::time::Duration::from_secs(8))
                .send().await
                .map(|r| r.status().is_success())
                .unwrap_or(false);
            if ok {
                notify(ToastKind::Success, "Note saved");
                // Refresh the list so the size label reflects the save.
                let items = fetch_notes(&client, &base).await;
                slint::invoke_from_event_loop(move || replace_notes_files(items)).ok();
            } else {
                notify(ToastKind::Error, "Failed to save note");
            }
        });
    });

    let rt_h_ncreate    = rt.handle().clone();
    let client_ncreate  = Arc::clone(&http_client);
    let base_ncreate    = http_base.clone();
    let ui_weak_ncreate = ui.as_weak();
    ui.on_create_note(move |name| {
        let client = Arc::clone(&client_ncreate);
        let base   = base_ncreate.clone();
        let ui_w   = ui_weak_ncreate.clone();
        let n      = name.to_string();
        rt_h_ncreate.spawn(async move {
            // Create an empty note, then open it (server returns the sanitized name).
            let created = client.post(format!("{base}/api/notes/write"))
                .json(&serde_json::json!({ "name": n, "content": "" }))
                .timeout(std::time::Duration::from_secs(8))
                .send().await
                .ok()
                .and_then(|r| if r.status().is_success() { Some(r) } else { None });
            let saved_name = match created {
                Some(r) => r.json::<Value>().await.ok()
                    .and_then(|v| v["name"].as_str().map(|s| s.to_string())),
                None => None,
            };
            match saved_name {
                Some(sn) => {
                    let items = fetch_notes(&client, &base).await;
                    slint::invoke_from_event_loop(move || {
                        replace_notes_files(items);
                        if let Some(ui) = ui_w.upgrade() {
                            ui.set_notes_current_name(sn.into());
                            ui.set_notes_current_text("".into());
                        }
                    }).ok();
                }
                None => notify(ToastKind::Error, "Failed to create note"),
            }
        });
    });

    // ── Sketchpad callbacks ─────────────────────────────────────────────────────
    // Drawing is pure Slint-thread state; only "send" touches the network.
    ui.on_sketch_down(|x, y| {
        if SKETCH_TOOL.with(|t| t.get()) == 0 { sketch_begin_stroke(x, y); }
        else { sketch_begin_shape(x, y); }
    });
    ui.on_sketch_move(|x, y| {
        if SKETCH_TOOL.with(|t| t.get()) == 0 { sketch_extend_stroke(x, y); }
        else { sketch_update_shape(x, y); }
    });
    ui.on_sketch_up(|| { /* stroke/shape complete; nothing to finalise */ });
    ui.on_sketch_clear(sketch_clear_all);
    // Canvas reports its pixel size → agent `sketch_draw` scales 0-1 coords to it.
    ui.on_sketch_report_canvas(|w, h| SKETCH_CANVAS.with(|c| c.set((w, h))));
    ui.on_sketch_set_color(|i| SKETCH_COLOR.with(|c| c.set(i)));
    ui.on_sketch_set_width(|i| SKETCH_WIDTH.with(|c| c.set(i)));
    ui.on_sketch_set_tool(|i| SKETCH_TOOL.with(|t| t.set(i)));

    // ── Web launcher: open a URL in the host browser (best-effort) ──────────────
    let rt_h_url = rt.handle().clone();
    ui.on_open_url(move |url| {
        let u = url.to_string();
        if u.is_empty() { return; }
        notify(ToastKind::Info, format!("Opening {u}…"));
        let prog = std::env::var("BROWSER").ok().filter(|s| !s.is_empty())
            .unwrap_or_else(|| "xdg-open".into());
        // Run + reap on the blocking pool so we neither block the UI nor leave a zombie.
        rt_h_url.spawn_blocking(move || {
            match std::process::Command::new(&prog).arg(&u).spawn() {
                Ok(mut child) => { let _ = child.wait(); }
                Err(_) => notify(ToastKind::Warn,
                    format!("No browser here — open {u} on another device")),
            }
        });
    });

    // ── Calculator: feed a key to the Rust state machine, show the result ───────
    {
        let ui_w = ui.as_weak();
        ui.on_calc_key(move |k| {
            let disp = CALC.with(|c| c.borrow_mut().key(&k));
            if let Some(ui) = ui_w.upgrade() { ui.set_calc_display(disp.into()); }
        });
    }

    let rt_h_sk     = rt.handle().clone();
    let client_sk   = Arc::clone(&http_client);
    let base_sk     = http_base.clone();
    let tx_sk       = tx.clone();
    ui.on_sketch_send(move |w, h| {
        // Send carries the exact canvas px — refresh the agent-draw scale too
        // (report-canvas only fires from pointer events now, see the view).
        SKETCH_CANVAS.with(|c| c.set((w, h)));
        let payload = sketch_payload(w, h);
        let empty = payload["strokes"].as_array().map(|a| a.is_empty()).unwrap_or(true);
        if empty {
            notify(ToastKind::Warn, "Nothing drawn yet");
            return;
        }
        let client = Arc::clone(&client_sk);
        let base   = base_sk.clone();
        let tx     = tx_sk.clone();
        rt_h_sk.spawn(async move {
            let ok = client.post(format!("{base}/api/sketch"))
                .json(&payload)
                .timeout(std::time::Duration::from_secs(10))
                .send().await
                .map(|r| r.status().is_success())
                .unwrap_or(false);
            if !ok {
                notify(ToastKind::Error, "Failed to send sketch");
                return;
            }
            notify(ToastKind::Success, "Sent to APEX 👁");
            // Surface the request in the chat + drive APEX to look at it.
            slint::invoke_from_event_loop(|| {
                maybe_push_time_divider();
                push_message(MessageItem {
                    role: "user".into(),
                    text: "🎨 I drew something on the Sketchpad — take a look.".into(),
                    streaming: false,
                    call_id: "".into(), tool_name: "".into(), tool_args: "".into(),
                    tool_output: "".into(), tool_status: "".into(),
                    awaiting_approval: false,
                });
            }).ok();
            let prompt = serde_json::json!({
                "type": "user_prompt",
                "text": "I drew something on the Sketchpad. Use the sketch_snapshot tool to get the image and tell me what you see.",
            }).to_string();
            tx.send(prompt).ok();
        });
    });

    // ── save-soul callback ────────────────────────────────────────────────────
    let rt_h_soul   = rt.handle().clone();
    let client_soul = Arc::clone(&http_client);
    let base_soul   = http_base.clone();
    ui.on_save_soul(move |text| {
        let client  = Arc::clone(&client_soul);
        let base    = base_soul.clone();
        let content = text.to_string();
        rt_h_soul.spawn(async move {
            let ok = client.post(format!("{base}/api/soul"))
                .json(&serde_json::json!({"content": content}))
                .timeout(std::time::Duration::from_secs(8))
                .send().await
                .map(|r| r.status().is_success())
                .unwrap_or(false);
            if ok { notify(ToastKind::Success, "Soul saved"); }
            else  { notify(ToastKind::Error, "Failed to save soul"); }
        });
    });

    // ── set-policy callback ───────────────────────────────────────────────────
    let rt_h_pol    = rt.handle().clone();
    let client_pol  = Arc::clone(&http_client);
    let base_pol    = http_base.clone();
    let ui_weak_pol = ui.as_weak();
    ui.on_set_policy(move |mode| {
        let mode_str = mode.to_string();
        // Optimistic UI update
        if let Some(ui) = ui_weak_pol.upgrade() {
            ui.set_settings_policy(mode_str.clone().into());
        }
        let client = Arc::clone(&client_pol);
        let base   = base_pol.clone();
        rt_h_pol.spawn(async move {
            let ok = client.post(format!("{base}/api/policy"))
                .json(&serde_json::json!({"mode": mode_str}))
                .timeout(std::time::Duration::from_secs(8))
                .send().await
                .map(|r| r.status().is_success())
                .unwrap_or(false);
            if ok { notify(ToastKind::Info, "Policy updated"); }
            else  { notify(ToastKind::Error, "Failed to update policy"); }
        });
    });

    // ── set-model callback ────────────────────────────────────────────────────
    let rt_h_mod    = rt.handle().clone();
    let client_mod  = Arc::clone(&http_client);
    let base_mod    = http_base.clone();
    let ui_weak_mod = ui.as_weak();
    ui.on_set_model(move |model_id| {
        let id = model_id.to_string();
        // Optimistic: update current-model display and highlight
        if let Some(ui) = ui_weak_mod.upgrade() {
            ui.set_settings_model(id.clone().into());
        }
        let client = Arc::clone(&client_mod);
        let base   = base_mod.clone();
        rt_h_mod.spawn(async move {
            let ok = client.post(format!("{base}/api/model"))
                .json(&serde_json::json!({"model": id}))
                .timeout(std::time::Duration::from_secs(8))
                .send().await
                .map(|r| r.status().is_success())
                .unwrap_or(false);
            if ok { notify(ToastKind::Info, "Model switched"); }
            else  { notify(ToastKind::Error, "Failed to switch model"); }
        });
    });

    // ── set-backend callback ──────────────────────────────────────────────────
    // Chip tap → POST /api/backend (live next turn, persisted server-side). The
    // server pins openrouter's canonical URL itself; a full settings refetch then
    // pulls the new backend's model catalog into the picker.
    let rt_h_be    = rt.handle().clone();
    let client_be  = Arc::clone(&http_client);
    let base_be    = http_base.clone();
    let ui_weak_be = ui.as_weak();
    ui.on_set_backend(move |backend| {
        let b = backend.to_string();
        if let Some(ui) = ui_weak_be.upgrade() {
            ui.set_settings_backend(b.clone().into());
        }
        let client = Arc::clone(&client_be);
        let base   = base_be.clone();
        let ui_w   = ui_weak_be.clone();
        rt_h_be.spawn(async move {
            let ok = client.post(format!("{base}/api/backend"))
                .json(&serde_json::json!({"backend": b}))
                .timeout(std::time::Duration::from_secs(8))
                .send().await
                .map(|r| r.status().is_success())
                .unwrap_or(false);
            if ok { notify(ToastKind::Info, "Backend switched"); }
            else  { notify(ToastKind::Error, "Failed to switch backend"); }
            slint::invoke_from_event_loop(move || {
                if let Some(ui) = ui_w.upgrade() {
                    ui.invoke_refresh_settings();
                }
            }).ok();
        });
    });

    // ── apply-endpoint callback ───────────────────────────────────────────────
    // A typed URL (APPLY) or a discovered endpoint row → adopt backend + URL.
    let rt_h_ep    = rt.handle().clone();
    let client_ep  = Arc::clone(&http_client);
    let base_ep    = http_base.clone();
    let ui_weak_ep = ui.as_weak();
    ui.on_apply_endpoint(move |url, kind| {
        let (u, k) = (url.to_string(), kind.to_string());
        let client = Arc::clone(&client_ep);
        let base   = base_ep.clone();
        let ui_w   = ui_weak_ep.clone();
        rt_h_ep.spawn(async move {
            let ok = client.post(format!("{base}/api/backend"))
                .json(&serde_json::json!({"backend": k, "oai_base_url": u}))
                .timeout(std::time::Duration::from_secs(8))
                .send().await
                .map(|r| r.status().is_success())
                .unwrap_or(false);
            if ok { notify(ToastKind::Info, "Endpoint adopted"); }
            else  { notify(ToastKind::Error, "Failed to set endpoint"); }
            slint::invoke_from_event_loop(move || {
                if let Some(ui) = ui_w.upgrade() {
                    ui.invoke_refresh_settings();
                }
            }).ok();
        });
    });

    // ── save-oai-key callback ─────────────────────────────────────────────────
    let rt_h_key    = rt.handle().clone();
    let client_key  = Arc::clone(&http_client);
    let base_key    = http_base.clone();
    let ui_weak_key = ui.as_weak();
    ui.on_save_oai_key(move |key| {
        let k = key.to_string();
        let client = Arc::clone(&client_key);
        let base   = base_key.clone();
        let ui_w   = ui_weak_key.clone();
        rt_h_key.spawn(async move {
            let ok = client.post(format!("{base}/api/keys"))
                .json(&serde_json::json!({"oai": k}))
                .timeout(std::time::Duration::from_secs(8))
                .send().await
                .map(|r| r.status().is_success())
                .unwrap_or(false);
            if ok { notify(ToastKind::Success, "API key saved"); }
            else  { notify(ToastKind::Error, "Failed to save key"); }
            slint::invoke_from_event_loop(move || {
                if let Some(ui) = ui_w.upgrade() {
                    if ok { ui.set_settings_oai_key_set(true); }
                }
            }).ok();
        });
    });

    // ── scan-compute callback ─────────────────────────────────────────────────
    // Operator-triggered LAN sweep (GET /api/compute/discover, a few seconds).
    let rt_h_scan    = rt.handle().clone();
    let client_scan  = Arc::clone(&http_client);
    let base_scan    = http_base.clone();
    let ui_weak_scan = ui.as_weak();
    ui.on_scan_compute(move || {
        if let Some(ui) = ui_weak_scan.upgrade() {
            ui.set_settings_scan_busy(true);
        }
        let client = Arc::clone(&client_scan);
        let base   = base_scan.clone();
        let ui_w   = ui_weak_scan.clone();
        rt_h_scan.spawn(async move {
            let resp = client.get(format!("{base}/api/compute/discover"))
                .timeout(std::time::Duration::from_secs(30))
                .send().await;
            let body = match resp {
                Ok(r) if r.status().is_success() =>
                    r.json::<serde_json::Value>().await.unwrap_or(serde_json::Value::Null),
                _ => serde_json::Value::Null,
            };
            let found: Vec<EndpointItem> = body["endpoints"]
                .as_array()
                .unwrap_or(&vec![])
                .iter()
                .map(|e| EndpointItem {
                    url:  e["url"].as_str().unwrap_or("").into(),
                    kind: e["kind"].as_str().unwrap_or("").into(),
                    host: e["host"].as_str().unwrap_or("").into(),
                    model_count: e["models"].as_array().map(|m| m.len() as i32).unwrap_or(0),
                })
                .collect();
            let n = found.len();
            slint::invoke_from_event_loop(move || {
                if let Some(ui) = ui_w.upgrade() {
                    ui.set_settings_endpoints(slint::ModelRc::from(Rc::new(
                        slint::VecModel::from(found),
                    )));
                    ui.set_settings_scan_busy(false);
                }
            }).ok();
            if n == 0 { notify(ToastKind::Info, "No LAN compute found"); }
            else      { notify(ToastKind::Success, format!("{n} endpoint{} found", if n == 1 {""} else {"s"})); }
        });
    });

    // ── filter-models callback ────────────────────────────────────────────────
    // Pure view narrowing over the cached full catalog — no network.
    ui.on_filter_models(move |f| {
        apply_model_filter(&f);
    });

    // ── set-cache callback ────────────────────────────────────────────────────
    // (enabled, cache_conversation, ttl) → POST /api/cache. Takes effect next turn.
    let rt_h_cache    = rt.handle().clone();
    let client_cache  = Arc::clone(&http_client);
    let base_cache    = http_base.clone();
    let ui_weak_cache = ui.as_weak();
    ui.on_set_cache(move |enabled, conversation, ttl| {
        let ttl_s = ttl.to_string();
        // Optimistic: reflect the new state immediately.
        if let Some(ui) = ui_weak_cache.upgrade() {
            ui.set_settings_cache_enabled(enabled);
            ui.set_settings_cache_conversation(conversation);
            ui.set_settings_cache_ttl(ttl_s.clone().into());
        }
        let client = Arc::clone(&client_cache);
        let base   = base_cache.clone();
        rt_h_cache.spawn(async move {
            let ok = client.post(format!("{base}/api/cache"))
                .json(&serde_json::json!({
                    "enabled": enabled,
                    "cache_conversation": conversation,
                    "ttl": ttl_s,
                }))
                .timeout(std::time::Duration::from_secs(8))
                .send().await
                .map(|r| r.status().is_success())
                .unwrap_or(false);
            if ok { notify(ToastKind::Info, "Cache settings updated"); }
            else  { notify(ToastKind::Error, "Failed to update cache settings"); }
        });
    });

    let rt_h_sensor    = rt.handle().clone();
    let client_sensor  = Arc::clone(&http_client);
    let base_sensor    = http_base.clone();
    let ui_weak_sensor = ui.as_weak();
    ui.on_set_sensor_profile(move |profile| {
        let p = profile.to_string();
        // Optimistic: reflect the selection immediately.
        if let Some(ui) = ui_weak_sensor.upgrade() {
            ui.set_settings_sensor_profile(p.clone().into());
        }
        let client = Arc::clone(&client_sensor);
        let base   = base_sensor.clone();
        rt_h_sensor.spawn(async move {
            let ok = client.post(format!("{base}/api/sensors/config"))
                .json(&serde_json::json!({ "profile": p }))
                .timeout(std::time::Duration::from_secs(8))
                .send().await
                .map(|r| r.status().is_success())
                .unwrap_or(false);
            if ok { notify(ToastKind::Info, "Sensor profile updated"); }
            else  { notify(ToastKind::Error, "Failed to update sensor profile"); }
        });
    });

    // ── voice-backend callback ────────────────────────────────────────────────
    let rt_h_voice    = rt.handle().clone();
    let client_voice  = Arc::clone(&http_client);
    let base_voice    = http_base.clone();
    let ui_weak_voice = ui.as_weak();
    ui.on_set_voice_backend(move |backend| {
        let b = backend.to_string();
        // Optimistic: reflect the selection immediately.
        if let Some(ui) = ui_weak_voice.upgrade() {
            ui.set_settings_voice_backend(b.clone().into());
        }
        let client = Arc::clone(&client_voice);
        let base   = base_voice.clone();
        rt_h_voice.spawn(async move {
            // One chip drives both TTS + STT backends (the common case); power users
            // can split them via /api/voice's tts_api/stt_api fields directly.
            let ok = client.post(format!("{base}/api/voice"))
                .json(&serde_json::json!({ "voice_backend": b, "stt_backend": b }))
                .timeout(std::time::Duration::from_secs(8))
                .send().await
                .map(|r| r.status().is_success())
                .unwrap_or(false);
            if ok { notify(ToastKind::Info, "Voice backend updated"); }
            else  { notify(ToastKind::Error, "Failed to update voice backend"); }
        });
    });

    // ── power-action callback ─────────────────────────────────────────────────
    let rt_h_pwr   = rt.handle().clone();
    let client_pwr = Arc::clone(&http_client);
    let base_pwr   = http_base.clone();
    ui.on_power_action(move |action| {
        let action_str = action.to_string();
        // Callback runs on the Slint thread → toast directly. The box may go
        // down before the POST returns, so confirm optimistically on click.
        toast(ToastKind::Warn,
            if action_str == "reboot" { "Rebooting…" } else { "Shutting down…" });
        let client = Arc::clone(&client_pwr);
        let base   = base_pwr.clone();
        rt_h_pwr.spawn(async move {
            client.post(format!("{base}/api/power"))
                .json(&serde_json::json!({"action": action_str}))
                .timeout(std::time::Duration::from_secs(10))
                .send().await.ok();
        });
    });

    // ── Clock (G6.1) — tick the tray/temporal clock every second on the Slint
    // thread. Held until run() returns so it isn't dropped (which would stop it).
    update_clock(&ui);
    let clock_timer = slint::Timer::default();
    {
        let ui_weak = ui.as_weak();
        clock_timer.start(
            slint::TimerMode::Repeated,
            std::time::Duration::from_secs(1),
            move || {
                if let Some(ui) = ui_weak.upgrade() {
                    update_clock(&ui);
                    inbox_restamp();
                }
            },
        );
    }

    // ── APEX face (😊) — a slow tick drives blink / talk / aura motion. Held
    // until run() returns so it isn't dropped (which would stop it).
    let face_timer = slint::Timer::default();
    {
        let ui_weak = ui.as_weak();
        face_timer.start(
            slint::TimerMode::Repeated,
            std::time::Duration::from_millis(450),
            move || {
                if let Some(ui) = ui_weak.upgrade() {
                    ui.set_face_tick((ui.get_face_tick() + 1) % 100_000);
                }
            },
        );
    }

    // ── Screen mirror (#36): self-snapshot server for APEX's screenshot tool ──
    rt.spawn(run_snapshot_server(snapshot_addr(), ui.as_weak()));

    // ── Phase-2 face — GL render (default on GL tiers) ────────────────────────
    // A custom GLSL face rendered inside our window via the rendering notifier
    // (femtovg NativeOpenGL), sharing femtovg's GL context. Scissored to the
    // FaceView's live on-window rect (published via the FaceGl global) so it
    // renders inside the face window and tracks it. This is now the DEFAULT: it
    // turns on automatically wherever a real GL context is available (desktop
    // winit, Pi 4/5 V3D) and **silently falls back to the 2D FaceView** when one
    // isn't (the notifier errors or never delivers NativeOpenGL → face_gl stays
    // None → nothing is drawn, the 2D face shows). `APEX_FACE_GL=0` forces the
    // 2D face everywhere (escape hatch). A repeated timer drives redraws so the
    // animation runs (Slint renders on-demand), gated on a visible face window
    // so a closed face costs nothing on the kiosk.
    let face_gl_enabled = std::env::var("APEX_FACE_GL").ok().as_deref() != Some("0");
    if face_gl_enabled {
        let start = std::time::Instant::now();
        let geom_weak = ui.as_weak();
        let mut face_gl: Option<face_gl::FaceGl> = None;
        let res = ui.window().set_rendering_notifier(move |state, api| match state {
            slint::RenderingState::RenderingSetup => {
                if let slint::GraphicsAPI::NativeOpenGL { get_proc_address } = api {
                    match face_gl::FaceGl::new(get_proc_address) {
                        Ok(f) => {
                            eprintln!("[face-gl] GL face initialised");
                            face_gl = Some(f);
                        }
                        Err(e) => eprintln!("[face-gl] setup failed: {e}"),
                    }
                }
            }
            slint::RenderingState::AfterRendering => {
                // Only paint when a face window is open & visible — the FaceGl
                // global keeps stale geometry after it closes.
                if let (Some(f), Some(ui)) = (&face_gl, geom_weak.upgrade()) {
                    if !face_window_visible() {
                        return;
                    }
                    let sf = ui.window().scale_factor();
                    let win = ui.window().size();
                    let g = ui.global::<FaceGl>();
                    let a = g.get_accent();
                    let expr = face_gl::FaceExpr {
                        accent: [
                            a.red() as f32 / 255.0,
                            a.green() as f32 / 255.0,
                            a.blue() as f32 / 255.0,
                        ],
                        eye_l: g.get_eye_l(),
                        eye_r: g.get_eye_r(),
                        brow: g.get_brow(),
                        brow_skew: g.get_brow_skew(),
                        brow_angle: g.get_brow_angle(),
                        mouth: g.get_mouth(),
                        open: g.get_mouth_open(),
                        gaze: [g.get_gaze_x(), g.get_gaze_y()],
                        intensity: g.get_intensity(),
                        blush: g.get_blush(),
                        talk: g.get_talk(),
                        head_roll: g.get_head_roll(),
                        head_pitch: g.get_head_pitch(),
                        tear: g.get_tear(),
                        cheek: g.get_cheek(),
                    };
                    f.draw(
                        start.elapsed().as_secs_f32(),
                        win.width as f32,
                        win.height as f32,
                        g.get_x() * sf,
                        g.get_y() * sf,
                        g.get_w() * sf,
                        g.get_h() * sf,
                        &expr,
                    );
                }
            }
            slint::RenderingState::RenderingTeardown => face_gl = None,
            _ => {}
        });
        match res {
            Ok(()) => {
                // Tell FaceView to publish its rect (gates its sample Timer).
                // The actual GL draw is separately gated on a real NativeOpenGL
                // context (face_gl.is_some()), so on a notifier-but-no-GL backend
                // this just runs a cheap idle Timer while the 2D face shows.
                ui.global::<FaceGl>().set_active(true);
                // Drive ~30fps redraws so the GL animation runs (Slint is
                // on-demand) — but only while a face window is visible, so a
                // closed face doesn't pin the CPU at 30fps on the kiosk.
                let redraw_weak = ui.as_weak();
                let timer = slint::Timer::default();
                timer.start(
                    slint::TimerMode::Repeated,
                    std::time::Duration::from_millis(33),
                    move || {
                        if let Some(ui) = redraw_weak.upgrade() {
                            if face_window_visible() {
                                ui.window().request_redraw();
                            }
                        }
                    },
                );
                std::mem::forget(timer); // keep the redraw loop alive for the process
                eprintln!("[face-gl] GL face active (auto; APEX_FACE_GL=0 to disable)");
            }
            Err(e) => eprintln!(
                "[face-gl] rendering notifier unavailable → 2D face (software renderer / Nano?): {e:?}"
            ),
        }
    }

    // Dev: APEX_FACE_STATE=<emote> previews a specific expression without agentd
    // (deterministic for snapshot verification), on either the GL or 2D face.
    if let Ok(s) = std::env::var("APEX_FACE_STATE") {
        if !s.is_empty() {
            ui.set_face_state(s.into());
            ui.set_face_intensity(1.0);
        }
    }

    // Phase B: debounce the geometry file — move/resize note per pointer-move,
    // this timer turns that into at most one write per 2s. Lives on the stack
    // so it runs for the life of the event loop.
    let geom_flush_timer = slint::Timer::default();
    geom_flush_timer.start(
        slint::TimerMode::Repeated,
        std::time::Duration::from_secs(2),
        geom_flush_if_dirty,
    );

    // Don't swallow the event-loop error. On linuxkms a GL/DRM fault can make
    // `run()` return Err — previously `?` propagated it as a bare exit-1 with no
    // message (the "render gremlin"), dropping the kiosk with zero diagnostics.
    // Log the full error so the cause is captured; systemd still restarts us.
    if let Err(e) = ui.run() {
        eprintln!("[ui-slint] FATAL: Slint event loop exited with error: {e:?}");
        return Err(e.into());
    }
    // Final flush — a shape changed in the last debounce window still lands.
    geom_flush_if_dirty();
    Ok(())
}

/// Queue a UI update on the Slint main thread for the given agentd event.
fn dispatch_event(
    ui_weak: slint::Weak<AppWindow>,
    ev: Value,
    state: Arc<Mutex<AppState>>,
    ctx: DispatchCtx,
) {
    let ev_type = ev["type"].as_str().unwrap_or("").to_string();

    // Adaptive UI Phase C: reflexes — below-inference event→action rules. ONE
    // chokepoint for every trigger type (string-handled and typed arms alike),
    // before the arms so a `return` above can't skip it. Cooldown + latch are
    // enforced inside reflex_fire, on the Slint thread.
    if REFLEX_TRIGGERS.contains(&ev_type.as_str()) {
        let w = ui_weak.clone();
        let t = ev_type.clone();
        slint::invoke_from_event_loop(move || {
            if let Some(ui) = w.upgrade() {
                reflex_fire(&ui, &t);
            }
        })
        .ok();
    }

    match ev_type.as_str() {
        // Server greeting: sent on connect (empty history) and on session resume
        // (with full history). Rust agentd: type="session_init".
        // Python agentd: type="hello". Handle both for compatibility.
        "session_init" | "hello" => {
            let id      = ev["session_id"].as_u64();
            let history = ev["history"].as_array().cloned().unwrap_or_default();
            let items   = replay_history(&history);
            let has_history = !items.is_empty();
            let w = ui_weak.clone();
            slint::invoke_from_event_loop(move || {
                if let Some(ui) = w.upgrade() {
                    if let Some(id) = id {
                        state.lock().unwrap_or_else(|e| e.into_inner()).session_id = Some(id);
                        ui.set_status(format!("Session {id}").into());
                        ui.set_current_session_id(id as i32);
                    }
                    // Fresh/restored session — the adaptive-UI rate rail refills.
                    UI_TURN_MUTATIONS.with(|m| m.set(0));
                    clear_messages();
                    for item in items {
                        push_message(item);
                    }
                    if has_history {
                        bump_scroll(&ui);
                    }
                }
            })
            .ok();
            return;
        }

        "turn_started" => {
            let w = ui_weak.clone();
            slint::invoke_from_event_loop(move || {
                if let Some(ui) = w.upgrade() {
                    ui.set_agent_busy(true);
                    ui.set_face_state("thinking".into());
                    push_message(MessageItem {
                        role: "agent".into(),
                        text: "".into(),
                        streaming: true,
                        call_id: "".into(),
                        tool_name: "".into(),
                        tool_args: "".into(),
                        tool_output: "".into(),
                        tool_status: "".into(),
                        awaiting_approval: false,
                    });
                    bump_scroll(&ui);
                }
            })
            .ok();
            return;
        }

        // A mesh peer messaged this node (a2a). It already landed in that peer's
        // own session (agentd routes it there); surface a global, click-to-open
        // notification so the user sees it from any active session.
        "mesh_message" => {
            let from    = ev["from_node"].as_str().unwrap_or("peer");
            let session = ev["session"].as_u64().unwrap_or(0) as i32;
            let preview = ev["preview"].as_str().unwrap_or("");
            let body = if preview.is_empty() {
                format!("✉ {from}")
            } else {
                format!("✉ {from}: {preview}")
            };
            notify_action(ToastKind::Info, body, session);
            // Fold it into the per-peer inbox (grouped threads + unread badge).
            inbox_upsert(from.to_string(), session, preview.to_string());
            return;
        }

        _ => {}
    }

    // ── Typed `Event` dispatch ─────────────────────────────────────────────────
    // Deserialize into the SAME enum agentd serialized from (the gateway sends the
    // raw Event with no reshaping, so this can't fail on a real event). A frame
    // that doesn't match the shared contract is LOGGED, not silently dropped — the
    // old footgun was that a renamed field/variant just vanished with no error.
    let event: Event = match serde_json::from_value(ev) {
        Ok(e) => e,
        Err(err) => {
            eprintln!("[ws] dropping undecodable '{ev_type}' frame: {err}");
            return;
        }
    };

    match event {
        Event::AgentText { delta, .. } => {
            if delta.is_empty() {
                return;
            }
            let w = ui_weak.clone();
            slint::invoke_from_event_loop(move || {
                if let Some(ui) = w.upgrade() {
                    // Lazily create an agent bubble if none is in progress.
                    // The Rust agentd has no TurnStarted event; Python agentd does.
                    let needs_bubble = MESSAGES.with(|m| {
                        m.borrow().as_ref().map(|model| {
                            let len = model.row_count();
                            len == 0 || model.row_data(len - 1)
                                .map(|last| last.role.as_str() != "agent" || !last.streaming)
                                .unwrap_or(true)
                        }).unwrap_or(true)
                    });
                    if needs_bubble {
                        push_message(MessageItem {
                            role: "agent".into(), text: "".into(), streaming: true,
                            call_id: "".into(), tool_name: "".into(), tool_args: "".into(),
                            tool_output: "".into(), tool_status: "".into(),
                            awaiting_approval: false,
                        });
                        ui.set_agent_busy(true);
                        board_active("responding…");
                    }
                    // Streaming text → APEX is speaking.
                    ui.set_face_state("speaking".into());
                    update_last_agent_message(&delta);
                    bump_scroll(&ui);
                }
            })
            .ok();
        }

        Event::TurnComplete { session } => {
            let tts    = ctx.tts_enabled.load(Ordering::SeqCst);
            let rt_h   = ctx.rt_handle.clone();
            let client = Arc::clone(&ctx.http_client);
            let base   = ctx.http_base.clone();
            let sess   = Some(session.0);
            let st     = state.clone();
            let w = ui_weak.clone();
            slint::invoke_from_event_loop(move || {
                if let Some(ui) = w.upgrade() {
                    // A sub-agent's turn finishing drops it from the running set + badge.
                    if let Some(s) = sess {
                        let remaining = {
                            let mut g = st.lock().unwrap_or_else(|e| e.into_inner());
                            if g.subagents.remove(&s) { Some(g.subagents.len() as i32) } else { None }
                        };
                        if let Some(n) = remaining { ui.set_subagent_count(n); }
                        // Work Board: a sub-agent finishing clears its card; a main-session
                        // turn finishing closes the Active card into RECENT.
                        match remaining {
                            Some(_) => board_remove_subagent(s),
                            None    => board_turn_done(),
                        }
                    }
                    finish_last_agent_message();
                    ui.set_agent_busy(false);
                    // Turn boundary — the adaptive-UI rate rail refills.
                    UI_TURN_MUTATIONS.with(|m| m.set(0));
                    // Turn done — restore APEX's held emote if it set one this turn,
                    // else a calm idle (unless mic is live; see below).
                    if !ui.get_recording() { face_rest(&ui); }
                    if tts {
                        // Grab last agent bubble text for TTS
                        let text = MESSAGES.with(|m| {
                            m.borrow().as_ref().and_then(|model| {
                                let len = model.row_count();
                                (0..len).rev().find_map(|i| {
                                    model.row_data(i)
                                        .filter(|item| item.role.as_str() == "agent")
                                        .map(|item| item.text.to_string())
                                })
                            }).unwrap_or_default()
                        });
                        if !text.is_empty() {
                            rt_h.spawn(async move {
                                speak_text(&client, &base, text).await;
                            });
                        }
                    }
                }
            })
            .ok();
        }

        Event::SensorAlert { node_id, kind, value, threshold, .. } => {
            // The persistence-filtered "this is real" signal — surface it as a
            // warn toast (lands in the notif center too). Any staging is the
            // reflex layer's job (the chokepoint above fired before this arm);
            // the agent's own response arrives via the paired root prompt.
            let body = if kind == "motion" {
                format!("⚠ {node_id}: motion detected")
            } else {
                format!("⚠ {node_id}: {kind} alert — {value:.0} (threshold {threshold:.0})")
            };
            slint::invoke_from_event_loop(move || {
                notify(ToastKind::Warn, body);
            })
            .ok();
        }

        Event::WakeTriggered => {
            // Wake word detected — switch to chat and auto-start recording
            let rt_h   = ctx.rt_handle.clone();
            let ui_w1  = ui_weak.clone();
            let ui_w2  = ui_weak.clone();
            slint::invoke_from_event_loop(move || {
                if let Some(ui) = ui_w1.upgrade() {
                    ui.set_face_state("listening".into());
                    if !ui.get_recording() {
                        ui.set_current_view(0);
                        rt_h.spawn(async move {
                            let ok = mic_record_start();
                            slint::invoke_from_event_loop(move || {
                                if let Some(ui) = ui_w2.upgrade() {
                                    if ok { ui.set_recording(true); ui.set_face_state("listening".into()); }
                                }
                            }).ok();
                        });
                    }
                }
            }).ok();
        }

        Event::ToolRequested { call, .. } => {
            // ToolCall.id is ActionId(u64) → a bare number; stringify for the row key.
            let tool_name = call.tool.clone();

            // Work Board: reflect the running tool on the Active card. display_face
            // (emoting), sketch_draw (drawing) and the ui_* staging verbs (the
            // agent moving its own windows) aren't work steps — skip them.
            let is_ui_effect = matches!(
                tool_name.as_str(),
                "display_face"
                    | "sketch_draw"
                    | "ui_open"
                    | "ui_close"
                    | "ui_focus"
                    | "ui_arrange"
                    | "ui_theme"
                    | "ui_reflex"
            );
            if !is_ui_effect {
                let t = tool_name.clone();
                slint::invoke_from_event_loop(move || board_active(&format!("running {t}"))).ok();
            }

            // `display_face` is APEX emoting, not a "tool action" — drive the face
            // directly from the call args and show NO tool card (it'd be noise).
            if tool_name == "display_face" {
                let a = &call.args;
                let fstate = a["state"].as_str().unwrap_or("neutral").to_string();
                let fgaze  = a["gaze"].as_str().unwrap_or("center").to_string();
                let fint   = a["intensity"].as_f64().unwrap_or(0.7).clamp(0.0, 1.0) as f32;
                let w = ui_weak.clone();
                slint::invoke_from_event_loop(move || {
                    if let Some(ui) = w.upgrade() {
                        set_face_emote(&ui, &fstate, &fgaze, fint);
                    }
                })
                .ok();
            } else if tool_name == "sketch_draw" {
                // APEX drawing on the canvas — apply to the live stroke models and
                // persist a composite PNG (so sketch_snapshot reflects it). No tool
                // card; the canvas IS the feedback.
                let clear  = call.args["clear"].as_bool().unwrap_or(false);
                let parsed = parse_agent_strokes(&call.args);
                let w      = ui_weak.clone();
                let rt_h   = ctx.rt_handle.clone();
                let client = Arc::clone(&ctx.http_client);
                let base   = ctx.http_base.clone();
                slint::invoke_from_event_loop(move || {
                    if let Some(ui) = w.upgrade() {
                        if let Some(payload) = apply_agent_sketch(&ui, clear, &parsed) {
                            let empty = payload["strokes"].as_array()
                                .map(|a| a.is_empty()).unwrap_or(true);
                            if !empty {
                                rt_h.spawn(async move {
                                    let _ = client.post(format!("{base}/api/sketch"))
                                        .json(&payload)
                                        .timeout(std::time::Duration::from_secs(10))
                                        .send().await;
                                });
                            }
                            notify(ToastKind::Success, "🎨 APEX drew on the Sketchpad");
                        }
                    }
                })
                .ok();
            } else if matches!(
                tool_name.as_str(),
                "ui_open" | "ui_close" | "ui_focus" | "ui_arrange" | "ui_theme" | "ui_reflex"
            ) {
                // Adaptive UI (Loop 6): the agent staging its own shell. Same
                // idiom as display_face — no tool card; the shell changing IS
                // the feedback (plus an attribution toast).
                let verb = tool_name.clone();
                let args = call.args.clone();
                let w = ui_weak.clone();
                slint::invoke_from_event_loop(move || {
                    if let Some(ui) = w.upgrade() {
                        // A3 rate rail: at most UI_TURN_MUTATION_CAP staging
                        // mutations apply per turn — a deliberate act, not a
                        // strobe. Beyond it verbs drop silently; the counter
                        // rides /state so the agent can see it throttled.
                        let spent = UI_TURN_MUTATIONS.with(|m| m.get());
                        if spent >= UI_TURN_MUTATION_CAP {
                            return;
                        }
                        UI_TURN_MUTATIONS.with(|m| m.set(spent + 1));
                        match verb.as_str() {
                            "ui_arrange" => {
                                let layout =
                                    args["layout"].as_str().unwrap_or("").to_string();
                                let apps: Vec<String> = args["apps"]
                                    .as_array()
                                    .map(|a| {
                                        a.iter()
                                            .filter_map(|v| v.as_str())
                                            .map(str::to_string)
                                            .collect()
                                    })
                                    .unwrap_or_default();
                                apply_ui_arrange(&ui, &layout, &apps);
                            }
                            "ui_theme" => {
                                apply_ui_theme(&ui, args["persona"].as_str().unwrap_or(""));
                            }
                            "ui_reflex" => {
                                apply_ui_reflex(&ui, &args);
                            }
                            _ => {
                                apply_ui_verb(&ui, &verb, args["app"].as_str().unwrap_or(""));
                            }
                        }
                    }
                })
                .ok();
            } else {
                let call_id   = call.id.0.to_string();
                let tool_args = if call.args.is_null() {
                    String::new()
                } else {
                    serde_json::to_string_pretty(&call.args).unwrap_or_default()
                };
                let w = ui_weak.clone();
                slint::invoke_from_event_loop(move || {
                    if let Some(ui) = w.upgrade() {
                        push_message(MessageItem {
                            role: "tool".into(),
                            text: "".into(),
                            streaming: false,
                            call_id: call_id.into(),
                            tool_name: tool_name.into(),
                            tool_args: tool_args.into(),
                            tool_output: "".into(),
                            tool_status: "running".into(),
                            awaiting_approval: false,
                        });
                        // Running a tool — APEX is working.
                        ui.set_face_state("thinking".into());
                        // Rust agentd emits no TurnStarted; a tool-first turn never
                        // hits the agent_text lazy-bubble path, so set busy here too —
                        // otherwise the Stop button never appears and input stays
                        // enabled (double-send). Idempotent if agent_text already set it.
                        ui.set_agent_busy(true);
                        bump_scroll(&ui);
                    }
                })
                .ok();
            }
        }

        Event::ToolResult { call, output: out, .. } => {
            // Work Board: a tool finished — clear its approval card (if any), keep Active alive.
            {
                let cid = call.0.to_string();
                slint::invoke_from_event_loop(move || { board_clear_blocked(&cid); board_active("working…"); }).ok();
            }
            // `call` is the bare action-id (ActionId.0); output nests { ok, content }.
            let call_id = call.0.to_string();
            let ok      = out.ok;
            let output  = match &out.content {
                serde_json::Value::String(s) => s.clone(),
                serde_json::Value::Null      => String::new(),
                other => serde_json::to_string_pretty(other).unwrap_or_default(),
            };
            let status = if ok { "done" } else { "error" };
            // Occipital follow-along: a successful web read mirrors into the
            // reader window (detected by the flat `kind` payload, not the tool
            // name — ToolResult carries none). Built off-thread (Send tuples).
            let occ = if ok {
                occipital_payload(&out.content).map(|p| build_occipital_render(&p))
            } else {
                None
            };
            let w = ui_weak.clone();
            slint::invoke_from_event_loop(move || {
                if let Some(row) = find_tool_row(&call_id) {
                    update_tool_row(row, |item| {
                        item.tool_output = output.into();
                        item.tool_status = status.into();
                        item.awaiting_approval = false;
                    });
                }
                if let Some(ui) = w.upgrade() {
                    if let Some(r) = occ {
                        apply_occipital_render(&ui, r);
                    }
                    bump_scroll(&ui);
                }
            })
            .ok();
        }

        Event::ApprovalPending { call, .. } => {
            // Work Board: the turn is blocked awaiting approval → a card in NEEDS APPROVAL.
            {
                let cid = call.id.0.to_string();
                let tool = call.tool.clone();
                let preview: String = call.args.to_string().chars().take(60).collect();
                slint::invoke_from_event_loop(move || {
                    board_add_blocked(&cid, &tool, &preview);
                    board_active("waiting for approval");
                }).ok();
            }
            // Same nesting as tool_requested. Normally a tool_requested arrives
            // first (card exists); the else-branch is a fallback.
            let call_id   = call.id.0.to_string();
            let tool_name = call.tool.clone();
            let tool_args = if call.args.is_null() {
                String::new()
            } else {
                serde_json::to_string_pretty(&call.args).unwrap_or_default()
            };
            let w = ui_weak.clone();
            slint::invoke_from_event_loop(move || {
                if let Some(row) = find_tool_row(&call_id) {
                    update_tool_row(row, |item| item.awaiting_approval = true);
                } else {
                    push_message(MessageItem {
                        role: "tool".into(),
                        text: "".into(),
                        streaming: false,
                        call_id: call_id.into(),
                        tool_name: tool_name.into(),
                        tool_args: tool_args.into(),
                        tool_output: "".into(),
                        tool_status: "running".into(),
                        awaiting_approval: true,
                    });
                }
                // Pin the latest into view whether the card was just created or
                // an existing one flipped to awaiting-approval (e.g. 3 at once).
                if let Some(ui) = w.upgrade() {
                    ui.set_face_state("alert".into());
                    ui.set_agent_busy(true);   // a tool awaiting approval = a turn in flight
                    bump_scroll(&ui);
                }
            })
            .ok();
        }

        // Sensor bridge events: BME688 (air_quality) + MLX90640 (thermal_frame)
        Event::SensorReading { reading, .. } => {
            match reading {
                SensorReading::AirQuality { iaq, temperature_c, humidity_pct, .. } => {
                    let temp  = temperature_c;
                    let humid = humidity_pct;
                    let label = iaq_label(iaq).to_string();
                    let w = ui_weak.clone();
                    slint::invoke_from_event_loop(move || {
                        if let Some(ui) = w.upgrade() {
                            let mut s = ui.get_sys_stats();
                            s.iaq_score    = iaq;
                            s.iaq_label    = label.into();
                            s.temp_c       = temp;
                            s.humidity_pct = humid;
                            ui.set_sys_stats(s);
                        }
                    })
                    .ok();
                }
                SensorReading::ThermalFrame { min_c, max_c, mean_c, .. } => {
                    let w = ui_weak.clone();
                    slint::invoke_from_event_loop(move || {
                        if let Some(ui) = w.upgrade() {
                            let mut s = ui.get_sys_stats();
                            s.thermal_min_c  = min_c;
                            s.thermal_max_c  = max_c;
                            s.thermal_mean_c = mean_c;
                            s.thermal_active = true;
                            ui.set_sys_stats(s);
                        }
                    })
                    .ok();
                }
                _ => {}
            }
        }

        // ── Council (G3d) ──────────────────────────────────────────────
        Event::CouncilStarted { topic, agents, .. } => {
            let agents: Vec<CouncilAgent> = agents.iter().enumerate().map(|(i, a)| {
                let id = a.id.as_str();
                let persona = a.persona.as_str();
                CouncilAgent {
                    id: id.into(),
                    persona: if persona.is_empty() { id.into() } else { persona.into() },
                    accent: council_accent(a.color.as_deref(), i),
                    text: "".into(),
                    done: false,
                }
            }).collect();
            let topic2 = topic.clone();
            let w = ui_weak.clone();
            slint::invoke_from_event_loop(move || {
                if let Some(ui) = w.upgrade() {
                    COUNCIL.with(|c| {
                        if let Some(model) = c.borrow().as_ref() { model.set_vec(agents); }
                    });
                    ui.set_council_topic(topic2.into());
                    ui.set_council_round(0);
                    ui.set_council_convergence(0.0);
                    ui.set_council_active(true);
                    ui.set_council_status("deliberating".into());
                    ui.set_council_synthesis("".into());
                    let t = ui.get_council_scroll_tick();
                    ui.set_council_scroll_tick(t.wrapping_add(1));
                }
            }).ok();
            notify(ToastKind::Info, format!("Council convened: {topic}"));
        }

        Event::CouncilRoundStart { round, .. } => {
            let round = round as i32;
            let w = ui_weak.clone();
            slint::invoke_from_event_loop(move || {
                if let Some(ui) = w.upgrade() {
                    ui.set_council_round(round);
                    // New round → clear each agent's transcript + done flag.
                    COUNCIL.with(|c| {
                        if let Some(model) = c.borrow().as_ref() {
                            for i in 0..model.row_count() {
                                if let Some(mut a) = model.row_data(i) {
                                    a.text = "".into();
                                    a.done = false;
                                    model.set_row_data(i, a);
                                }
                            }
                        }
                    });
                }
            }).ok();
        }

        Event::CouncilAgentDelta { agent_id, delta, .. } => {
            if delta.is_empty() { return; }
            let w = ui_weak.clone();
            slint::invoke_from_event_loop(move || {
                if let Some(ui) = w.upgrade() {
                    council_update(&agent_id, |a| {
                        let mut s = a.text.to_string();
                        s.push_str(&delta);
                        a.text = s.into();
                    });
                    let t = ui.get_council_scroll_tick();
                    ui.set_council_scroll_tick(t.wrapping_add(1));
                }
            }).ok();
        }

        Event::CouncilAgentDone { agent_id, full_text, .. } => {
            slint::invoke_from_event_loop(move || {
                council_update(&agent_id, |a| {
                    if !full_text.is_empty() { a.text = full_text.into(); }
                    a.done = true;
                });
            }).ok();
        }

        Event::CouncilRoundDone { convergence: conv, .. } => {
            let w = ui_weak.clone();
            slint::invoke_from_event_loop(move || {
                if let Some(ui) = w.upgrade() { ui.set_council_convergence(conv); }
            }).ok();
        }

        Event::CouncilComplete { reason, synthesis, rounds, .. } => {
            let rounds = rounds as i32;
            let status = match reason.as_str() {
                "consensus"  => "consensus",
                "max_rounds" => "max rounds",
                "stopped"    => "stopped",
                _            => "complete",
            };
            let syn2 = synthesis.clone();
            let w = ui_weak.clone();
            slint::invoke_from_event_loop(move || {
                if let Some(ui) = w.upgrade() {
                    ui.set_council_active(false);
                    ui.set_council_status(status.into());
                    ui.set_council_round(rounds);
                    ui.set_council_synthesis(syn2.into());
                    let t = ui.get_council_scroll_tick();
                    ui.set_council_scroll_tick(t.wrapping_add(1));
                }
            }).ok();
            notify(ToastKind::Success, format!("Council {status}"));
        }

        Event::CouncilButtIn { message: msg, .. } => {
            if !msg.is_empty() { notify(ToastKind::Info, format!("Council: {msg}")); }
        }

        Event::SubAgentStarted { child, prompt, .. } => {
            let cid = child.0;
            let st = state.clone();
            let w = ui_weak.clone();
            slint::invoke_from_event_loop(move || {
                if let Some(ui) = w.upgrade() {
                    let n = {
                        let mut g = st.lock().unwrap_or_else(|e| e.into_inner());
                        g.subagents.insert(cid);
                        g.subagents.len() as i32
                    };
                    ui.set_subagent_count(n);
                    board_add_subagent(cid, &prompt);
                }
            }).ok();
            notify(ToastKind::Info, "Sub-agent started");
        }

        // Work Board: global colony activity → RECENT cards (these events are
        // session-less, so every client sees them).
        Event::EvolutionApplied { patch_summary, .. } => {
            let s = patch_summary.clone();
            slint::invoke_from_event_loop(move || {
                board_push_recent("Evolved".into(), s, "EVO", board_color(52, 211, 153));
            }).ok();
        }

        Event::MeshMessage { from_node, preview, .. } => {
            let (from, prev) = (from_node.clone(), preview.clone());
            slint::invoke_from_event_loop(move || {
                board_push_recent(format!("Mesh ← {from}"), prev, "MESH", board_color(45, 212, 191));
            }).ok();
        }

        // Downtime beacon: a peer crossed the up↔down boundary → board notification.
        Event::MeshNodeStatus { node_id, status, last_seen_secs } => {
            let dark = status == "dark";
            let title = format!("Node {} {}", node_id, if dark { "DARK" } else { "back online" });
            let detail = if dark { format!("no heartbeat for ~{last_seen_secs}s") } else { "heartbeat restored".into() };
            let (badge, c) = if dark { ("DARK", board_color(239, 68, 68)) } else { ("UP", board_color(52, 211, 153)) };
            slint::invoke_from_event_loop(move || board_push_recent(title, detail, badge, c)).ok();
        }

        // Work Board: an autonomous goal advanced → upsert its card in the GOALS lane.
        Event::GoalStateChanged { goal, objective, state, step, max_steps, detail, yolo } => {
            let (badge, c) = match state {
                GoalState::Acting    => ("RUN",   board_color(96, 165, 250)),
                GoalState::Done      => ("DONE",  board_color(52, 211, 153)),
                GoalState::Failed    => ("FAIL",  board_color(239, 68, 68)),
                GoalState::Blocked   => ("BLOCK", board_color(251, 191, 36)),
                GoalState::Cancelled => ("STOP",  board_color(148, 163, 184)),
                _                    => ("…",     board_color(148, 163, 184)),
            };
            let gid = goal.0;
            let title: String = objective.chars().take(60).collect();
            let base = if detail.is_empty() {
                format!("step {step}/{max_steps}")
            } else {
                format!("step {step}/{max_steps} · {detail}")
            };
            // Goal-scoped yolo: mark the card AUTO (text + ⚡ — the glyph renders mono on
            // the kiosk, so the word carries it if the emoji tofus). (#3)
            let subtitle = if yolo { format!("⚡ AUTO · {base}") } else { base };
            slint::invoke_from_event_loop(move || board_goal(gid, title, subtitle, badge, c)).ok();
        }

        _ => {}
    }
}
