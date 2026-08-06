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
use apexos_protocol::{Event, GoalState, SensorReading, WorkerState};
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
    // Imagine (🖼) — the Imaginarium studio's shared node jobs list.
    static IMAGINE_JOBS: RefCell<Option<Rc<slint::VecModel<ImagineJobItem>>>> = const { RefCell::new(None) };
    // Imagine prompt-from-file picker rows (workspace text files).
    static IMAGINE_PROMPT_FILES: RefCell<Option<Rc<slint::VecModel<ImageItem>>>> = const { RefCell::new(None) };
    // The Cutting Room (A5): Rust owns the edit list; the model is its projection.
    static CUT_SEGS: RefCell<Vec<CutSeg>> = const { RefCell::new(Vec::new()) };
    static CUT_MODEL: RefCell<Option<Rc<slint::VecModel<CutSegItem>>>> = const { RefCell::new(None) };
    // Music bed: (job id, display label).
    static CUT_MUSIC: RefCell<Option<(String, String)>> = const { RefCell::new(None) };
    // Sonus tracks already imported into the imaginarium library this session
    // (track name → library job id) — scoring twice must not duplicate imports.
    static CUT_SONUS_IMPORTED: RefCell<std::collections::HashMap<String, String>> =
        RefCell::new(std::collections::HashMap::new());
    // Image-edit sources (A4): up to 3 library refs feeding /v1/images/edits.
    static EDIT_SOURCES: RefCell<Vec<(String, String)>> = const { RefCell::new(Vec::new()) };
    static EDIT_SOURCES_MODEL: RefCell<Option<Rc<slint::VecModel<ImageItem>>>> =
        const { RefCell::new(None) };
    // U3 poster cache — job id → fetch state. Failed stays failed (audio jobs
    // have no thumb; retrying every 3s watcher tick would be a storm).
    static IMAGINE_THUMBS: RefCell<std::collections::HashMap<String, ThumbState>> =
        RefCell::new(std::collections::HashMap::new());
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
    workers:   Rc<slint::VecModel<BoardCard>>,   // fanned-out workers, keyed by "worker<id>" (Fabrica W1a)
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

/// Upsert a fanned-out worker's card in the WORKERS column (keyed by worker id,
/// so the card updates in place through Queued → Running → Done/Parked/Failed).
fn board_worker(id: u64, title: String, subtitle: String, badge: &str, c: slint::Color) {
    board_with(|bm| board_upsert(&bm.workers, board_card(&format!("worker{id}"), title, subtitle, badge, c)));
}

// Per-batch approval digests (W1d, the digest principle): N workers awaiting
// approvals collapse to ONE card per batch in NEEDS APPROVAL with a count —
// never N cards. Driven off WorkerStateChanged (Blocked + the driver's
// "awaiting approval — " detail prefix, a pinned contract); the underlying
// per-call approval mechanics are untouched. Slint-thread-only, like BOARD.
thread_local! {
    static WORKER_APPR: RefCell<std::collections::HashMap<u64, std::collections::HashSet<u64>>> = RefCell::new(std::collections::HashMap::new());
}

fn board_worker_approval(batch: u64, worker: u64, awaiting: bool) {
    WORKER_APPR.with(|m| {
        let mut m = m.borrow_mut();
        let set = m.entry(batch).or_default();
        if awaiting { set.insert(worker); } else { set.remove(&worker); }
        let n = set.len();
        if n == 0 { m.remove(&batch); }
        board_with(|bm| {
            let key = format!("batchappr{batch}");
            if n == 0 {
                board_remove(&bm.blocked, &key);
            } else {
                board_upsert(&bm.blocked, board_card(&key,
                    format!("Batch {batch} workers"),
                    format!("{n} approval{} pending", if n == 1 { "" } else { "s" }),
                    "BATCH", board_color(251, 191, 36)));
            }
        });
    });
}

/// The (main-session) turn finished: drop the Active card + THIS turn's stale
/// per-call asks, and drop a "done" card into Recent. The sweep is SURGICAL
/// (M1d fix of the M1b field finding): batch approval digests (`batchappr…`)
/// belong to WORKERS whose turns are still suspended on a card — a main-
/// session turn completing says nothing about them, and the old whole-lane
/// drain made live approvals visually absent (a Blocked worker read as dead).
fn board_turn_done() {
    board_with(|bm| {
        board_remove(&bm.active, "turn");
        let mut i = 0;
        while i < bm.blocked.row_count() {
            let keep = bm.blocked.row_data(i)
                .map(|c| c.id.starts_with("batchappr"))
                .unwrap_or(false);
            if keep { i += 1; } else { bm.blocked.remove(i); }
        }
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
        20 => AppKind::Imagine,
        21 => AppKind::Mandala,
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
    (AppKind::Imagine, "imagine"),
    (AppKind::Mandala, "mandala"),
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
    // (snapshot server). =results|recall|dom|click|submit previews those
    // modes. (Its auto-reveal places a window too — same wait applies.)
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
        AppKind::Imagine => "Imagine",
        AppKind::Mandala => "Mandalas",
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
        AppKind::Board => (1040.0, 600.0), // 6 columns since the WORKERS lane (W1a)
        AppKind::Imagine => (900.0, 640.0),
        AppKind::Mandala => (640.0, 620.0),
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
    mode:        String,   // page|results|recall|dom|click|submit|distill|related
    title:       String,
    url:         String,
    meta:        String,
    badge:       String,   // cache|live|""
    blocks:      Vec<(String, String, i32)>,             // kind, text, depth
    links:       Vec<(String, String, String, String)>,  // label, url, detail, badge
    crumb_label: String,
    crumb_url:   String,
}

/// Recover an Occipital payload (an object with a known reader `kind`) from a
/// tool result's content, whatever the transport shape: a bare object, a JSON
/// string, or an MCP text-content array.
///
/// ⚠ Two-places trap: this whitelist and the `build_occipital_render` match
/// arms must move TOGETHER — a kind admitted here without a real arm falls to
/// the honest `_` fallback (visible), but a kind with an arm that isn't listed
/// here is silently dropped before rendering ever runs.
fn occipital_payload(content: &Value) -> Option<Value> {
    fn is_occipital(v: &Value) -> bool {
        matches!(
            v.get("kind").and_then(|k| k.as_str()),
            Some("page" | "results" | "recall" | "dom" | "click" | "submit" | "distill" | "related")
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

/// Recover a mandala_status payload (an object carrying a `mandalas` array)
/// from the same three transport shapes as `occipital_payload`: direct object,
/// string-encoded JSON, or an MCP content array. Shape-sniffed — ToolResult
/// carries no tool name (the occipital follow-along idiom).
fn mandala_payload(content: &Value) -> Option<Value> {
    fn is_mandala(v: &Value) -> bool {
        v.get("mandalas").map(|m| m.is_array()).unwrap_or(false)
    }
    if is_mandala(content) {
        return Some(content.clone());
    }
    if let Some(s) = content.as_str() {
        if let Ok(v) = serde_json::from_str::<Value>(s) {
            if is_mandala(&v) {
                return Some(v);
            }
        }
    }
    if let Some(arr) = content.as_array() {
        for item in arr {
            if item.get("type").and_then(|t| t.as_str()) == Some("text") {
                if let Some(s) = item.get("text").and_then(|t| t.as_str()) {
                    if let Ok(v) = serde_json::from_str::<Value>(s) {
                        if is_mandala(&v) {
                            return Some(v);
                        }
                    }
                }
            }
        }
    }
    None
}

/// Build the Mandala window's rows from a mandala_status payload — pure and
/// Send (plain tuples cross to the Slint thread). One flat list, the
/// occipital-block idiom: each mandala contributes a header row, note rows
/// (objective / census / council synthesis), then its cells indented by
/// address depth. Returns (meta line, rows as (text, depth, state, kind)).
fn build_mandala_rows(p: &Value) -> (String, Vec<(String, i32, String, String)>) {
    let empty = vec![];
    let mandalas = p["mandalas"].as_array().unwrap_or(&empty);
    let mut rows: Vec<(String, i32, String, String)> = Vec::new();
    let mut open_total = 0u64;
    for m in mandalas {
        let id = m["mandala"].as_u64().unwrap_or(0);
        let state = m["state"].as_str().unwrap_or("open").to_string();
        let open = m["open_cells"].as_u64().unwrap_or(0);
        open_total += open;
        let mut head = format!(
            "☸ mandala {id} · {} · {} · {open}/{} open",
            m["lattice"].as_str().unwrap_or("?"),
            state,
            m["cells_budget"].as_u64().unwrap_or(64),
        );
        if let Some(e) = m["epoch"].as_u64() {
            head.push_str(&format!(" · epoch {e}"));
        }
        if let Some(fp) = m["fingerprint"].as_str() {
            head.push_str(&format!(" ({fp})"));
        }
        if let Some(o) = m["orbits"].as_u64().filter(|o| *o > 0) {
            head.push_str(&format!(" · ⚠ {o} orbit(s)"));
        }
        if m["repo"].as_str().is_some() {
            head.push_str(" · code");
        }
        rows.push((head, 0, state, "mandala".into()));
        if let Some(obj) = m["objective"].as_str() {
            rows.push((format!("◦ {}", obj.chars().take(110).collect::<String>()), 1, String::new(), "note".into()));
        }
        if let Some(census) = m["census"].as_object() {
            if !census.is_empty() {
                let line = census.iter().map(|(k, v)| format!("{k}×{v}")).collect::<Vec<_>>().join("  ");
                rows.push((format!("census {line}"), 1, String::new(), "note".into()));
            }
        }
        if let Some(s) = m["orbit_synthesis"].as_str() {
            rows.push((format!("council: {}", s.chars().take(140).collect::<String>()), 1, "orbit".into(), "note".into()));
        }
        for c in m["cells"].as_array().unwrap_or(&empty) {
            let addr = c["addr"].as_str().unwrap_or("?");
            let depth = addr.matches('.').count() as i32 + 1;
            let cstate = c["state"].as_str().unwrap_or("open").to_string();
            let form = c["form"].as_str().unwrap_or("cell");
            let mut line = format!("{addr} · {form} · {cstate}");
            if let Some(w) = c["worker"].as_u64() {
                line.push_str(&format!(" · w{w}"));
            }
            if let Some(n) = c["node"].as_str() {
                line.push_str(&format!(" @ {n}"));
                if let Some(b) = c["body"].as_str() {
                    line.push_str(&format!(" ({b})"));
                }
            }
            if let Some(hist) = c["measure_history"].as_array() {
                if !hist.is_empty() {
                    let tail: Vec<String> = hist.iter().filter_map(|v| v.as_u64()).map(|n| n.to_string()).collect();
                    line.push_str(&format!(" · m {}", tail.join("→")));
                }
            }
            if c["voucher"].as_bool() == Some(true) {
                line.push_str(" · voucher");
            }
            if c["barrier_opened"].as_bool() == Some(true) {
                line.push_str(" · barrier open");
            } else if c["barrier_timeout_s"].as_u64().is_some() && cstate == "open" {
                line.push_str(" · holding");
            }
            if let Some(rp) = c["reparented_to"].as_str() {
                line.push_str(&format!(" · ⤴ {rp}"));
            }
            let kind = if matches!(form, "gate" | "diamond" | "forge" | "mandala") { "gate" } else { "cell" };
            rows.push((line, depth, cstate, kind.into()));
        }
    }
    let meta = if mandalas.is_empty() {
        String::new()
    } else {
        format!("{} mandala(s) · {} open cell(s)", mandalas.len(), open_total)
    };
    (meta, rows)
}

/// Apply a built mandala reading to the tree window (Slint thread only) and
/// reveal it the first time a reading lands — through the SAME latch-aware
/// agent path as the occipital reader: a user-closed window stays closed
/// until the user re-invites it from the menu. Quiet, never toasted.
fn apply_mandala_render(ui: &AppWindow, meta: String, rows: Vec<(String, i32, String, String)>) {
    let rows: Vec<MandalaRow> = rows
        .into_iter()
        .map(|(text, depth, state, kind)| MandalaRow {
            text: text.into(),
            depth,
            state: state.into(),
            kind: kind.into(),
        })
        .collect();
    ui.set_mandala_meta(meta.into());
    ui.set_mandala_rows(slint::ModelRc::from(Rc::new(slint::VecModel::from(rows))));
    ui.set_mandala_scroll_tick(ui.get_mandala_scroll_tick() + 1);
    WINDOWS.with(|w| {
        if let Some(model) = w.borrow().as_ref() {
            if wm_index_by_kind(model, AppKind::Mandala).is_none() {
                agent_open_window(ui, model, AppKind::Mandala, false);
            }
        }
    });
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

fn json_flag(p: &Value, key: &str) -> bool {
    p.get(key).and_then(|x| x.as_bool()).unwrap_or(false)
}

/// (kind, text, depth) — a parsed reader block before Slint conversion.
type BlockPlan = (String, String, i32);
/// (label, url, detail, badge) — a link row before Slint conversion.
type LinkPlan = (String, String, String, String);

/// Markdown blocks + link rows of a page-ish payload — `page`, `click` and
/// `submit` all carry a full page body, so they share this. Occipital inlines
/// each form as a prose annotation (`[form#1 → GET /search — search "q" ·
/// submit "Go"]`); re-kind those to "form" blocks so the view can style the
/// interactive surface as an affordance instead of plain text. A JS-only page
/// that yielded nothing gets an honest empty state rather than a blank fetch.
fn page_blocks_links(p: &Value) -> (Vec<BlockPlan>, Vec<LinkPlan>) {
    let markdown = json_str(p, "markdown");
    let mut blocks = if markdown.is_empty() {
        Vec::new()
    } else {
        parse_reader_markdown(&markdown)
    };
    for b in &mut blocks {
        if b.0 == "p" && b.1.starts_with("[form#") && b.1.ends_with(']') {
            b.0 = "form".into();
            b.1 = b.1[1..b.1.len() - 1].to_string();
        }
    }
    if blocks.is_empty() && json_flag(p, "js_required") {
        blocks.push((
            "quote".into(),
            "This page needs JavaScript — nothing was recoverable from static HTML.".into(),
            0,
        ));
    }
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
    (blocks, links)
}

/// Build the (Send) render plan from a recovered Occipital payload.
fn build_occipital_render(p: &Value) -> OccipitalRender {
    let kind = json_str(p, "kind");
    // `submit` reports freshness as `cached` (a POST result is never cached, so
    // `from_cache` would be a lie there); everything else uses `from_cache`.
    let from_cache = match kind.as_str() {
        "submit" => p.get("cached").and_then(|b| b.as_bool()),
        _ => p.get("from_cache").and_then(|b| b.as_bool()),
    };
    let badge = match (kind.as_str(), from_cache) {
        ("recall" | "dom", _) => String::new(),
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
            let saved = json_str(p, "status") == "saved";
            let (blocks, links) = page_blocks_links(p);
            let mut meta = if saved {
                "📌 saved to memory".into()
            } else {
                format!("{} link{} on page", links.len(), if links.len() == 1 { "" } else { "s" })
            };
            if json_flag(p, "salvaged") {
                meta.push_str(" · salvaged from embedded data");
            }
            let crumb = cap_crumb(&title);
            OccipitalRender {
                mode: "page".into(), title, url, meta, badge, blocks, links,
                crumb_label: crumb, crumb_url: json_str(p, "url"),
            }
        }

        // An interaction result is a page payload plus what the agent DID —
        // same reader layout, the meta line shows the hands.
        "click" => {
            let url = json_str(p, "url");
            let title = {
                let t = json_str(p, "title");
                if t.is_empty() { url.clone() } else { t }
            };
            let (blocks, links) = page_blocks_links(p);
            let mut meta = format!(
                "clicked {} → {}",
                json_str(p, "element"),
                json_str(p, "target_url")
            );
            if let Some(s) = p.get("status").and_then(|x| x.as_u64()) {
                meta.push_str(&format!(" · HTTP {s}"));
            }
            if json_flag(p, "salvaged") {
                meta.push_str(" · salvaged");
            }
            let crumb = cap_crumb(&format!("click: {title}"));
            OccipitalRender {
                mode: "click".into(), title, url, meta, badge, blocks, links,
                crumb_label: crumb, crumb_url: json_str(p, "url"),
            }
        }

        "submit" => {
            let url = json_str(p, "url");
            let title = {
                let t = json_str(p, "title");
                if t.is_empty() { url.clone() } else { t }
            };
            let (blocks, links) = page_blocks_links(p);
            let form = p.get("form").and_then(|x| x.as_u64()).unwrap_or(0);
            let sent: Vec<String> = p
                .get("sent")
                .and_then(|s| s.as_array())
                .map(|arr| {
                    arr.iter()
                        .take(3)
                        .map(|f| format!("{}={}", json_str(f, "name"), json_str(f, "value")))
                        .collect()
                })
                .unwrap_or_default();
            let mut meta = format!(
                "form#{form} {} {}",
                json_str(p, "method").to_uppercase(),
                json_str(p, "action")
            );
            if !sent.is_empty() {
                meta.push_str(&format!(" — {}", sent.join(" · ")));
            }
            if let Some(s) = p.get("status").and_then(|x| x.as_u64()) {
                meta.push_str(&format!(" · HTTP {s}"));
            }
            if json_flag(p, "salvaged") {
                meta.push_str(" · salvaged");
            }
            let crumb = cap_crumb(&format!("form: {title}"));
            OccipitalRender {
                mode: "submit".into(), title, url, meta, badge, blocks, links,
                crumb_label: crumb, crumb_url: json_str(p, "url"),
            }
        }

        // The element registry: links (with their click ordinals) + forms as
        // rows. Agent-facing data, human-legible list — no page body.
        "dom" => {
            let url = json_str(p, "url");
            let title = {
                let t = json_str(p, "title");
                if t.is_empty() { url.clone() } else { t }
            };
            let mut links: Vec<(String, String, String, String)> = p
                .get("links")
                .and_then(|l| l.as_array())
                .map(|arr| {
                    arr.iter()
                        .take(60)
                        .map(|l| {
                            let u = json_str(l, "url");
                            let t = json_str(l, "text");
                            let label = if t.trim().is_empty() { u.clone() } else { t };
                            let idx = l.get("idx").and_then(|x| x.as_u64()).unwrap_or(0);
                            (label, u, String::new(), format!("#{idx}"))
                        })
                        .filter(|(_, u, _, _)| !u.is_empty())
                        .collect()
                })
                .unwrap_or_default();
            let n_links = links.len();
            let forms = p
                .get("forms")
                .and_then(|f| f.as_array())
                .cloned()
                .unwrap_or_default();
            for f in forms.iter().take(20) {
                let idx = f.get("idx").and_then(|x| x.as_u64()).unwrap_or(0);
                let fields: Vec<String> = f
                    .get("fields")
                    .and_then(|x| x.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter(|fd| json_str(fd, "kind") != "hidden")
                            .take(4)
                            .map(|fd| format!("{} \"{}\"", json_str(fd, "kind"), json_str(fd, "name")))
                            .collect()
                    })
                    .unwrap_or_default();
                let mut detail = fields.join(" · ");
                if let Some(s) = f.get("submit").and_then(|x| x.as_str()) {
                    if !detail.is_empty() {
                        detail.push_str(" · ");
                    }
                    detail.push_str(&format!("submit \"{s}\""));
                }
                // url stays empty — a form row is an affordance to read, not a
                // link to steer to (LinkRow ignores clicks on an empty url).
                links.push((
                    format!("form#{idx} → {} {}", json_str(f, "method").to_uppercase(), json_str(f, "action")),
                    String::new(),
                    detail,
                    "form".into(),
                ));
            }
            let n_forms = forms.len();
            let mut meta = format!(
                "{n_links} link{} · {n_forms} form{}",
                if n_links == 1 { "" } else { "s" },
                if n_forms == 1 { "" } else { "s" },
            );
            if json_flag(p, "snapshot") {
                meta.push_str(" · snapshot held");
            }
            if json_flag(p, "js_required") {
                meta.push_str(" · needs JS");
            } else if json_flag(p, "salvaged") {
                meta.push_str(" · salvaged");
            }
            let crumb = cap_crumb(&format!("dom: {title}"));
            OccipitalRender {
                mode: "dom".into(), title, url, meta, badge, blocks: Vec::new(), links,
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
        "recall" => {
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

        // The knowledge web: a curated page's neighbours as rows — the shared
        // terms (the edge label) lead the detail line, the overlap score is
        // the chip. An empty neighbourhood explains itself via the meta.
        "related" => {
            let title = {
                let t = json_str(p, "title");
                if t.is_empty() { json_str(p, "url") } else { t }
            };
            let links: Vec<(String, String, String, String)> = p
                .get("related")
                .and_then(|r| r.as_array())
                .map(|arr| {
                    arr.iter()
                        .take(60)
                        .map(|r| {
                            let u = json_str(r, "url");
                            let t = json_str(r, "title");
                            let label = if t.trim().is_empty() { u.clone() } else { t };
                            let shared: Vec<String> = r
                                .get("shared_entities")
                                .and_then(|a| a.as_array())
                                .into_iter()
                                .flatten()
                                .chain(r.get("shared_tags").and_then(|a| a.as_array()).into_iter().flatten())
                                .filter_map(|v| v.as_str().map(String::from))
                                .collect();
                            let mut detail = json_str(r, "summary_head");
                            if !shared.is_empty() {
                                detail = format!("🏷 {} — {detail}", shared.join(", "));
                            }
                            let score = r
                                .get("score")
                                .and_then(|s| s.as_f64())
                                .map(|s| format!("{s:.1}"))
                                .unwrap_or_default();
                            (label, u, detail, score)
                        })
                        .collect()
                })
                .unwrap_or_default();
            let total = p.get("distilled_total").and_then(|t| t.as_u64()).unwrap_or(0);
            let meta = format!(
                "{} connected page{} · {total} distilled in store",
                links.len(),
                if links.len() == 1 { "" } else { "s" }
            );
            let crumb = cap_crumb(&format!("related: {title}"));
            OccipitalRender {
                mode: "related".into(),
                title,
                url: json_str(p, "url"),
                meta,
                badge: String::new(),
                blocks: Vec::new(),
                links,
                crumb_label: crumb,
                crumb_url: json_str(p, "url"),
            }
        }

        // A distillation is curated knowledge — render each page as a card
        // (title, summary, key-point bullets, a tags/entities line), with the
        // source pages as steerable link rows. Failures and the undistilled
        // backlog are surfaced honestly in the meta + a warning block.
        "distill" => {
            let empty = Vec::new();
            let distilled = p.get("distilled").and_then(|d| d.as_array()).unwrap_or(&empty);
            let failed = p.get("failed").and_then(|f| f.as_array()).unwrap_or(&empty);
            let remaining = p.get("remaining").and_then(|r| r.as_u64()).unwrap_or(0);
            let join = |d: &Value, key: &str| -> String {
                d.get(key)
                    .and_then(|a| a.as_array())
                    .map(|a| a.iter().filter_map(|v| v.as_str()).collect::<Vec<_>>().join(", "))
                    .unwrap_or_default()
            };

            let mut blocks: Vec<BlockPlan> = Vec::new();
            let mut links: Vec<LinkPlan> = Vec::new();
            for (i, d) in distilled.iter().enumerate() {
                let url = json_str(d, "url");
                let title = {
                    let t = json_str(d, "title");
                    if t.is_empty() { url.clone() } else { t }
                };
                if i > 0 {
                    blocks.push(("rule".into(), String::new(), 0));
                }
                blocks.push(("h2".into(), title.clone(), 0));
                let summary = json_str(d, "summary");
                if !summary.is_empty() {
                    blocks.push(("p".into(), summary.clone(), 0));
                }
                for kp in d.get("key_points").and_then(|k| k.as_array()).into_iter().flatten() {
                    if let Some(s) = kp.as_str() {
                        blocks.push(("bullet".into(), s.to_string(), 0));
                    }
                }
                let (tags, entities) = (join(d, "tags"), join(d, "entities"));
                let mut tagline = String::new();
                if !tags.is_empty() {
                    tagline = format!("🏷 {tags}");
                }
                if !entities.is_empty() {
                    if !tagline.is_empty() {
                        tagline.push_str(" · ");
                    }
                    tagline.push_str(&entities);
                }
                if !tagline.is_empty() {
                    blocks.push(("quote".into(), tagline, 0));
                }
                if !url.is_empty() {
                    let cached = d.get("from_cache").and_then(|b| b.as_bool()).unwrap_or(false);
                    let chip = if cached { "cache".into() } else { json_str(d, "backend") };
                    let detail: String = summary.chars().take(160).collect();
                    links.push((title, url, detail, chip));
                }
            }
            for f in failed {
                blocks.push((
                    "quote".into(),
                    format!("⚠ {} — {}", json_str(f, "url"), json_str(f, "error")),
                    0,
                ));
            }
            if distilled.is_empty() && failed.is_empty() {
                blocks.push((
                    "quote".into(),
                    "Nothing to distill — every cached page is already curated.".into(),
                    0,
                ));
            }

            let mut meta = format!(
                "{} page{} distilled",
                distilled.len(),
                if distilled.len() == 1 { "" } else { "s" }
            );
            if !failed.is_empty() {
                meta.push_str(&format!(" · {} failed", failed.len()));
            }
            if remaining > 0 {
                meta.push_str(&format!(" · {remaining} still undistilled"));
            }

            // A single-page distill gets page-like identity: its title/url up
            // top, freshness as the badge, and provenance in the meta.
            let (title, url, badge) = if let [d] = distilled.as_slice() {
                let u = json_str(d, "url");
                let t = {
                    let t = json_str(d, "title");
                    if t.is_empty() { u.clone() } else { t }
                };
                let cached = d.get("from_cache").and_then(|b| b.as_bool()).unwrap_or(false);
                if cached {
                    meta.push_str(" · already current (no LLM spend)");
                } else {
                    let model = json_str(d, "model");
                    if !model.is_empty() {
                        meta.push_str(&format!(" · {model}"));
                    }
                }
                (t, u, if cached { "cache" } else { "live" }.to_string())
            } else {
                (
                    format!(
                        "Distilled {} page{}",
                        distilled.len(),
                        if distilled.len() == 1 { "" } else { "s" }
                    ),
                    String::new(),
                    String::new(),
                )
            };
            let crumb = cap_crumb(&format!("distill: {title}"));
            OccipitalRender {
                mode: "distill".into(),
                title,
                url: url.clone(),
                meta,
                badge,
                blocks,
                links,
                crumb_label: crumb,
                crumb_url: url,
            }
        }

        // Unreachable while the `occipital_payload` whitelist and these arms
        // stay in sync. If they drift, say so — the old `_ => recall` wildcard
        // silently rendered every new kind as "0 memory hits" (the two-places
        // trap), which is exactly the failure this arm exists to make visible.
        other => OccipitalRender {
            mode: "page".into(),
            title: format!("unrenderable payload: kind \"{other}\""),
            url: String::new(),
            meta: "the reader has no renderer for this kind — the gate and the render arms have drifted".into(),
            badge: String::new(),
            blocks: Vec::new(),
            links: Vec::new(),
            crumb_label: cap_crumb(other),
            crumb_url: String::new(),
        },
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

/// Sample render for `APEX_OCCIPITAL_DEMO` (page|results|recall|dom|click|submit|distill|related)
/// — lets the reader window be verified via the snapshot server with no agentd /
/// no network. The samples mirror the real Occipital payload shapes.
fn occipital_demo_render(mode: &str) -> OccipitalRender {
    let payload = match mode.trim() {
        "dom" => serde_json::json!({
            "kind": "dom", "url": "https://www.raspberrypi.com/products/raspberry-pi-5/",
            "title": "Raspberry Pi 5", "from_cache": true, "snapshot": true,
            "salvaged": false, "js_required": false, "content_hash": "abc123",
            "links": [
                {"idx": 1, "text": "official 27W PD supply", "url": "https://www.raspberrypi.com/products/27w-power-supply/"},
                {"idx": 2, "text": "product page", "url": "https://www.raspberrypi.com/products/raspberry-pi-5/"}
            ],
            "forms": [
                {"idx": 1, "action": "https://www.raspberrypi.com/search", "method": "get",
                 "fields": [{"name": "q", "kind": "search", "label": "Search"}], "submit": "Go"}
            ]
        }),
        "click" => serde_json::json!({
            "kind": "click", "element": "link:1",
            "source_url": "https://www.raspberrypi.com/products/raspberry-pi-5/",
            "target_url": "https://www.raspberrypi.com/products/27w-power-supply/",
            "url": "https://www.raspberrypi.com/products/27w-power-supply/",
            "title": "27W USB-C Power Supply", "from_cache": false, "status": null,
            "markdown": "# 27W USB-C Power Supply\n\nThe official Raspberry Pi 27W USB-C PD supply delivers **5V/5A** for full Pi 5 performance and peripheral power.\n\n[form#1 → GET https://www.raspberrypi.com/search — search \"q\" · submit \"Go\"]",
            "links": [{"text": "Raspberry Pi 5", "url": "https://www.raspberrypi.com/products/raspberry-pi-5/"}],
            "forms": [
                {"idx": 1, "action": "https://www.raspberrypi.com/search", "method": "get",
                 "fields": [{"name": "q", "kind": "search"}], "submit": "Go"}
            ],
            "salvaged": false, "js_required": false, "content_hash": "def456"
        }),
        "submit" => serde_json::json!({
            "kind": "submit", "source_url": "https://www.raspberrypi.com/",
            "form": 1, "action": "https://www.raspberrypi.com/search", "method": "get",
            "sent": [{"name": "q", "value": "pi 5 power delivery"}], "status": null, "cached": false,
            "url": "https://www.raspberrypi.com/search?q=pi+5+power+delivery",
            "title": "Search — pi 5 power delivery",
            "markdown": "## Results\n\n- [Raspberry Pi 5](https://www.raspberrypi.com/products/raspberry-pi-5/) — the board itself\n- [27W Power Supply](https://www.raspberrypi.com/products/27w-power-supply/) — the official 5V/5A PD supply",
            "links": [
                {"text": "Raspberry Pi 5", "url": "https://www.raspberrypi.com/products/raspberry-pi-5/"},
                {"text": "27W Power Supply", "url": "https://www.raspberrypi.com/products/27w-power-supply/"}
            ],
            "forms": [], "salvaged": false, "js_required": false, "content_hash": "ghi789"
        }),
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
        "related" => serde_json::json!({
            "kind": "related", "url": "https://www.raspberrypi.com/products/raspberry-pi-5/",
            "title": "Raspberry Pi 5", "count": 2, "distilled_total": 7,
            "related": [
                {"url": "https://www.raspberrypi.com/products/27w-power-supply/",
                 "title": "27W Power Supply",
                 "summary_head": "The official 27W USB-C PD supply delivers the 5V/5A profile the Pi 5 needs.",
                 "score": 5.0, "shared_entities": ["Raspberry Pi 5", "USB-C PD"], "shared_tags": ["raspberry-pi"]},
                {"url": "https://forums.raspberrypi.com/viewtopic.php?t=357789",
                 "title": "PD requirements thread",
                 "summary_head": "Without a 5V/5A PD source the firmware caps downstream USB current.",
                 "score": 1.0, "shared_entities": [], "shared_tags": ["power"]}
            ]
        }),
        "distill" => serde_json::json!({
            "kind": "distill", "count": 2,
            "distilled": [
                {"url": "https://www.raspberrypi.com/products/raspberry-pi-5/",
                 "title": "Raspberry Pi 5",
                 "summary": "The Raspberry Pi 5 is a quad-core Cortex-A76 single-board computer at 2.4GHz with up to 16GB RAM. It requires a 5V/5A USB-C PD supply for full performance.",
                 "key_points": ["BCM2712 quad-core Cortex-A76 @ 2.4GHz", "Up to 16GB LPDDR4X RAM", "Needs 27W (5V/5A) USB-C PD for full peripheral power"],
                 "entities": ["Raspberry Pi 5", "BCM2712", "VideoCore VII"],
                 "tags": ["raspberry-pi", "sbc", "hardware"],
                 "model": "llama3.2", "backend": "ollama", "from_cache": false},
                {"url": "https://www.raspberrypi.com/products/27w-power-supply/",
                 "title": "27W Power Supply",
                 "summary": "The official 27W USB-C PD supply delivers the 5V/5A profile the Pi 5 needs; without it downstream USB current is capped.",
                 "key_points": ["5V/5A fixed PD profile", "Firmware caps USB to 600mA on weaker supplies"],
                 "entities": ["USB-C PD"],
                 "tags": ["power", "raspberry-pi"],
                 "model": "llama3.2", "backend": "cache", "from_cache": true}
            ],
            "failed": [{"url": "https://example.com/spa", "error": "page needs JavaScript — nothing to distill"}],
            "remaining": 3
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
    use super::{history_budget_label, history_label_tokens, history_usage_line};

    #[test]
    fn history_budget_labels_round_trip() {
        for label in ["60k", "120k", "200k", "400k", "off"] {
            let tokens = history_label_tokens(label).expect("preset label maps");
            assert_eq!(history_budget_label(tokens), label, "round trip for {label}");
        }
        assert_eq!(history_label_tokens("weird"), None, "unknown labels are ignored");
        assert_eq!(history_budget_label(150_000), "150k", "custom env budget still displays");
    }

    #[test]
    fn history_usage_line_renders_and_hides() {
        let h = serde_json::json!({
            "budget": 120_000, "trim_trigger": 144_000,
            "sessions": [{ "session_id": 0, "est_tokens": 87_000 }],
        });
        assert_eq!(
            history_usage_line(&h),
            "Largest window: session 0 ≈ 87k of 120k (trims at 144k)"
        );
        let off = serde_json::json!({
            "budget": 0, "sessions": [{ "session_id": 3, "est_tokens": 12_000 }],
        });
        assert_eq!(
            history_usage_line(&off),
            "Largest window: session 3 ≈ 12k tokens (trimming off)"
        );
        assert_eq!(history_usage_line(&serde_json::json!({"budget": 120_000, "sessions": []})), "");
        assert_eq!(history_usage_line(&serde_json::Value::Null), "", "pre-/api/history agentd");
    }

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
        assert_eq!(APP_TABLE.len(), 22);
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
    fn imagine_job_rows_parses_the_jobs_list() {
        use super::imagine_job_rows;
        // Well-formed JobListItem rows: model stem trimmed, created_at cut to
        // date+minute (both RFC3339 and SQLite forms), error carried through.
        let v = serde_json::json!([
            { "job_id": "01ABC", "status": "done", "mode": "image_generate",
              "model": "grok-imagine-image-quality",
              "created_at": "2026-07-28T18:03:11.123Z", "error": null,
              "prompt": "marble amphitheater" },
            { "job_id": "01DEF", "status": "failed", "mode": "video_generate",
              "model": "grok-imagine-video",
              "created_at": "2026-07-28 18:05:02", "error": "upstream 500" },
        ]);
        let rows = imagine_job_rows(&v);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0], ("01ABC".into(), "image_generate".into(), "done".into(),
                             "image-quality".into(), "2026-07-28 18:03".into(), "".into(),
                             "marble amphitheater".into()));
        assert_eq!(rows[1].3, "video");
        assert_eq!(rows[1].4, "2026-07-28 18:05");
        assert_eq!(rows[1].5, "upstream 500");
        // No prompt in the row (old node / import) → "" (backward compatible).
        assert_eq!(rows[1].6, "");
        // A row without a job_id is skipped, not a panic; non-arrays yield nothing.
        let partial = serde_json::json!([{ "status": "done" }, { "job_id": "01X" }]);
        let rows = imagine_job_rows(&partial);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].0, "01X");
        assert_eq!(rows[0].2, "?");
        assert!(imagine_job_rows(&serde_json::json!({"not":"an array"})).is_empty());
        assert!(imagine_job_rows(&serde_json::Value::Null).is_empty());
    }

    #[test]
    fn fit_dims_preserves_aspect_and_evens() {
        use super::fit_dims;
        // Landscape 720p into a 960×720 stage → width-bound, even dims.
        assert_eq!(fit_dims(1280, 720, 960, 720), (960, 540));
        // Portrait into the same stage → height-bound.
        assert_eq!(fit_dims(720, 1280, 960, 720), (404, 720));
        // Never upscale past the source.
        assert_eq!(fit_dims(320, 240, 960, 720), (320, 240));
        // Odd results round DOWN to even (rawvideo/yuv requirement).
        let (w, h) = fit_dims(1279, 719, 640, 480);
        assert_eq!((w % 2, h % 2), (0, 0));
        // Degenerate inputs stay sane.
        assert_eq!(fit_dims(0, 0, 960, 720), (2, 2));
        assert_eq!(fit_dims(1280, 720, 0, 0), (2, 2));
    }

    #[test]
    fn parse_ffprobe_video_reads_dims_and_duration() {
        use super::parse_ffprobe_video;
        // Typical csv=p=0 output: stream line then format line.
        assert_eq!(parse_ffprobe_video("1280,720\n8.342000\n"), Some((1280, 720, 8.342)));
        // Missing duration (N/A on some containers) → 0.0, still playable.
        assert_eq!(parse_ffprobe_video("640,480\nN/A\n"), Some((640, 480, 0.0)));
        // Garbage / empty → None.
        assert_eq!(parse_ffprobe_video(""), None);
        assert_eq!(parse_ffprobe_video("not,numbers\n"), None);
    }

    #[test]
    fn imagine_clock_formats_mm_ss() {
        use super::imagine_clock;
        assert_eq!(imagine_clock(0.0), "0:00");
        assert_eq!(imagine_clock(8.4), "0:08");
        assert_eq!(imagine_clock(75.0), "1:15");
        assert_eq!(imagine_clock(-3.0), "0:00");
    }

    #[test]
    fn imagine_token_clean_heals_operator_forms() {
        use super::imagine_token_clean;
        // The canonical form passes through untouched.
        assert_eq!(imagine_token_clean("abc123"), "abc123");
        // Whitespace + one pair of matching quotes healed (shell-export form).
        assert_eq!(imagine_token_clean("  abc123 \n"), "abc123");
        assert_eq!(imagine_token_clean("\"abc123\""), "abc123");
        assert_eq!(imagine_token_clean("'abc123'"), "abc123");
        assert_eq!(imagine_token_clean(" \"abc123\" "), "abc123");
        // Mismatched quotes are NOT stripped (they're part of the value, honest).
        assert_eq!(imagine_token_clean("\"abc123'"), "\"abc123'");
        // Empty / whitespace-only → empty (treated as unset).
        assert_eq!(imagine_token_clean("   "), "");
        assert_eq!(imagine_token_clean("\"\""), "");
    }

    #[test]
    fn imagine_video_body_routes_by_source_kind() {
        use super::imagine_video_body;
        // T2V: no source — duration clamped 1..15, auto omits model, defaults omitted.
        let (path, body) = imagine_video_body("orbit shot", "auto", 8, "default", "default", "", "");
        assert_eq!(path, "/v1/videos/generations");
        assert_eq!(body["prompt"], "orbit shot");
        assert_eq!(body["duration"], 8);
        assert_eq!(body["no_wait"], true);
        assert!(body.get("model").is_none());
        assert!(body.get("image").is_none());
        assert!(body.get("resolution").is_none());
        // I2V: image source becomes a library: ref (U1); explicit knobs pass.
        let (path, body) =
            imagine_video_body("", "1.5", 20, "1080p", "16:9", "01JOB", "image");
        assert_eq!(path, "/v1/videos/generations");
        assert_eq!(body["image"], "library:01JOB");
        assert_eq!(body["duration"], 15); // clamped
        assert_eq!(body["model"], "1.5");
        assert_eq!(body["resolution"], "1080p");
        assert_eq!(body["aspect_ratio"], "16:9");
        // Extend: video source → the extensions route, duration clamped 2..10,
        // no resolution/aspect fields (the route doesn't take them).
        let (path, body) =
            imagine_video_body("keep panning", "auto", 15, "720p", "16:9", "01VID", "video");
        assert_eq!(path, "/v1/videos/extensions");
        assert_eq!(body["video"], "library:01VID");
        assert_eq!(body["duration"], 10); // clamped
        assert!(body.get("resolution").is_none());
        assert!(body.get("aspect_ratio").is_none());
    }

    #[test]
    fn imagine_done_note_reports_cost_and_batch() {
        use super::imagine_done_note;
        let job = serde_json::json!({ "usage": { "estimated_usd": 0.04 } });
        assert_eq!(imagine_done_note(&job, 1), "done · ~$0.04");
        assert_eq!(
            imagine_done_note(&job, 4),
            "done · ~$0.04 · 4 images in the node library (first shown)"
        );
        // No usage block → still an honest "done".
        assert_eq!(imagine_done_note(&serde_json::json!({}), 1), "done");
    }

    #[test]
    fn cut_timeline_builds_the_v1_contract() {
        use super::{cut_build_timeline, CutSeg};
        let mut clip = CutSeg::clip("01VID", "hammer strike");
        clip.in_s = 1.0;
        clip.out_s = 5.0;
        clip.gain_ix = 1; // -6dB
        clip.speed_ix = 3; // 2×
        clip.caption = "the forge wakes".into();
        let mut still = CutSeg::still("01IMG", "poster");
        still.dur_ix = 2; // 4s
        still.zoom_ix = 2; // push+ → 1.25
        let mut card = CutSeg::card();
        card.color_ix = 2; // rust
        card.caption = "FIN".into();

        let body = cut_build_timeline(&[clip, still, card], Some("01WAV"), 2, true, 1);
        assert_eq!(body["version"], 1);
        let clips = body["clips"].as_array().unwrap();
        assert_eq!(clips.len(), 3);
        // clip: trim + gain + speed + a whole-segment caption
        assert_eq!(clips[0]["job_id"], "01VID");
        assert_eq!(clips[0]["gain_db"], -6.0);
        assert_eq!(clips[0]["speed"], 2.0);
        assert_eq!(clips[0]["captions"][0]["text"], "the forge wakes");
        assert_eq!(clips[0]["captions"][0]["fontsize"], 0); // inherit
        // still: kind + dur + Ken Burns
        assert_eq!(clips[1]["kind"], "still");
        assert_eq!(clips[1]["dur_s"], 4.0);
        assert_eq!(clips[1]["zoom_to"], 1.25);
        // card: color + big caption
        assert_eq!(clips[2]["kind"], "card");
        assert_eq!(clips[2]["card_color"], "#B7410E");
        assert_eq!(clips[2]["captions"][0]["fontsize"], 44);
        assert!(clips[2].get("job_id").is_none());
        // style: letterbox reveal + loudnorm; fades preset SOFT
        assert_eq!(body["style"]["letterbox_frac"], 0.12);
        assert_eq!(body["style"]["letterbox_reveal_s"], 1.5);
        assert_eq!(body["style"]["loudnorm"], true);
        assert_eq!(body["video_fade_in_s"], 0.3);
        assert_eq!(body["audio_fade_out_s"], 0.5);
        // music bed on the master clock
        assert_eq!(body["music"]["job_id"], "01WAV");
        assert_eq!(body["music"]["gain_db"], -8.0);

        // bare defaults: no style/fades/music keys at all, speed omitted at 1×
        let bare = cut_build_timeline(&[CutSeg::clip("01A", "x")], None, 0, false, 0);
        assert!(bare.get("style").is_none());
        assert!(bare.get("music").is_none());
        assert!(bare.get("video_fade_in_s").is_none());
        assert!(bare["clips"][0].get("speed").is_none());
        assert!(bare["clips"][0].get("captions").is_none());
    }

    #[test]
    fn cut_details_and_totals_read_honestly() {
        use super::{cut_detail, cut_total_label, CutSeg};
        let mut c = CutSeg::clip("01A", "x");
        assert_eq!(cut_detail(&c), "full");
        c.in_s = 1.0;
        assert_eq!(cut_detail(&c), "1.0s→end");
        c.out_s = 5.0;
        c.speed_ix = 2;
        c.gain_ix = 0;
        assert_eq!(cut_detail(&c), "1.0–5.0s · 1.5× · -12dB");
        let mut s = CutSeg::still("01B", "y");
        s.dur_ix = 1;
        s.zoom_ix = 1;
        assert_eq!(cut_detail(&s), "3s · push");

        // totals: known seconds sum (speed-adjusted); open clips flag "+?"
        let open = CutSeg::clip("01C", "z");
        assert_eq!(cut_total_label(&[c.clone(), s.clone()], true), "2 segments · ~6s · 🎵");
        assert!(cut_total_label(&[open], false).ends_with("~0s+?"));
        assert_eq!(cut_total_label(&[], false), "");
    }

    #[test]
    fn edit_body_carries_library_refs() {
        use super::imagine_edit_body;
        let sources = vec![
            ("01AAA".to_string(), "a".to_string()),
            ("01BBB".to_string(), "b".to_string()),
        ];
        let body = imagine_edit_body("make it night", 2, &sources);
        assert_eq!(body["prompt"], "make it night");
        assert_eq!(body["n"], 2);
        assert_eq!(body["images"][0], "library:01AAA");
        assert_eq!(body["images"][1], "library:01BBB");
        // model/aspect stay absent — the server's edit default rules
        assert!(body.get("model").is_none());
        assert!(body.get("aspect_ratio").is_none());
        // a fourth source never leaves the client
        let four: Vec<(String, String)> = (0..4).map(|i| (format!("0{i}"), "x".into())).collect();
        assert_eq!(imagine_edit_body("p", 1, &four)["images"].as_array().unwrap().len(), 3);
    }

    #[test]
    fn sonus_scoring_maps_mimes_and_briefs() {
        use super::{cut_compose_prompt, sonus_mime_for_name};
        assert_eq!(sonus_mime_for_name("dream.wav"), "audio/wav");
        assert_eq!(sonus_mime_for_name("BED.MP3"), "audio/mpeg");
        assert_eq!(sonus_mime_for_name("weird.flac"), "audio/flac");
        assert_eq!(sonus_mime_for_name("noext"), "audio/wav");
        let brief = cut_compose_prompt("3 segments · ~12s · 🎵");
        assert!(brief.starts_with("(cutting room)"));
        assert!(brief.contains("3 segments · ~12s"));
        assert!(cut_compose_prompt("").contains("empty timeline"));
    }

    #[test]
    fn cut_apply_steps_and_reopens_the_out_point() {
        use super::{cut_apply, CutSeg};
        let mut seg = CutSeg::clip("01A", "x");
        cut_apply(&mut seg, "in", "0.5");
        cut_apply(&mut seg, "in", "0.5");
        assert_eq!(seg.in_s, 1.0);
        cut_apply(&mut seg, "in", "-2.0");
        assert_eq!(seg.in_s, 0.0); // floor
        // out steps up from in when unset; stepping back through in reopens (0)
        seg.in_s = 1.0;
        cut_apply(&mut seg, "out", "2.0");
        assert_eq!(seg.out_s, 3.0);
        cut_apply(&mut seg, "out", "-2.0");
        assert_eq!(seg.out_s, 0.0);
        cut_apply(&mut seg, "caption", "  hello  ");
        assert_eq!(seg.caption, "hello");
        cut_apply(&mut seg, "caption-clear", "");
        assert_eq!(seg.caption, "");
        // unknown fields are a no-op, never a panic
        let before = seg.clone();
        cut_apply(&mut seg, "warp", "9");
        assert_eq!(seg, before);
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

    // ── Mandala tree window (Fabrica app track) ─────────────────────────────

    #[test]
    fn mandala_payload_recovers_from_every_transport_shape() {
        use super::mandala_payload;
        let obj = json!({"mandalas": [], "count": 0});
        assert!(mandala_payload(&obj).is_some());
        assert!(mandala_payload(&json!(obj.to_string())).is_some());
        let mcp = json!([{ "type": "text", "text": obj.to_string() }]);
        assert!(mandala_payload(&mcp).is_some());
        // The signature is the ARRAY — anything else is ignored.
        assert!(mandala_payload(&json!({"ok": true, "content": "hello"})).is_none());
        assert!(mandala_payload(&json!({"mandalas": "not-an-array"})).is_none());
    }

    #[test]
    fn mandala_rows_flatten_the_tree_with_depth_and_body_tags() {
        use super::build_mandala_rows;
        let p = json!({"mandalas": [{
            "mandala": 5, "lattice": "quad", "state": "open",
            "open_cells": 2, "cells_budget": 64, "epoch": 3, "fingerprint": "abc123def456",
            "orbits": 1, "orbit_synthesis": "steer 0.0.1, cancel nothing",
            "objective": "first light",
            "census": {"L:111111": 2},
            "cells": [
                {"addr": "0", "form": "spine", "state": "open"},
                {"addr": "0.0", "form": "diamond", "state": "open", "worker": 36, "barrier_timeout_s": 900},
                {"addr": "0.0.1", "form": "spine", "state": "done", "worker": 38,
                 "node": "andre-laptop", "body": "done", "measure_history": [8, 5, 2]}
            ]
        }], "count": 1});
        let (meta, rows) = build_mandala_rows(&p);
        assert_eq!(meta, "1 mandala(s) · 2 open cell(s)");
        // The header carries the whole reading; the orbit warning rides it.
        assert!(rows[0].0.contains("mandala 5"));
        assert!(rows[0].0.contains("epoch 3"));
        assert!(rows[0].0.contains("⚠ 1 orbit(s)"));
        assert_eq!(rows[0].3, "mandala");
        // Notes: objective, census, council synthesis (orbit-colored).
        assert!(rows[1].0.contains("first light"));
        assert!(rows[2].0.contains("L:111111×2"));
        assert!(rows[3].0.contains("council: steer 0.0.1"));
        assert_eq!(rows[3].2, "orbit");
        // Cells: depth = addr depth + 1 (the mandala header is depth 0); the
        // diamond styles as a gate and shows "holding" while open; the
        // remote-bodied cell carries node, live body and the measure tail.
        assert_eq!(rows[4].1, 1);
        assert_eq!(rows[5].1, 2);
        assert_eq!(rows[5].3, "gate");
        assert!(rows[5].0.contains("holding"));
        assert_eq!(rows[6].1, 3);
        assert!(rows[6].0.contains("@ andre-laptop (done)"));
        assert!(rows[6].0.contains("m 8→5→2"));
        assert_eq!(rows[6].2, "done");
        // An empty reading renders the empty hint upstream: no rows, no meta.
        let (m2, r2) = build_mandala_rows(&json!({"mandalas": [], "count": 0}));
        assert!(m2.is_empty() && r2.is_empty());
    }

    #[test]
    fn build_occipital_render_distill_cards_and_rows() {
        // A sweep: per-page cards (h2/summary/bullets/tag-quote), rule
        // separators, honest failure + backlog reporting, steerable rows.
        let r = build_occipital_render(&json!({
            "kind": "distill", "count": 2,
            "distilled": [
                {"url": "https://a", "title": "A", "summary": "What A says.",
                 "key_points": ["fact one", "fact two"],
                 "entities": ["Alpha"], "tags": ["alpha", "docs"],
                 "model": "llama3.2", "backend": "ollama", "from_cache": false},
                {"url": "https://b", "title": "B", "summary": "What B says.",
                 "key_points": [], "entities": [], "tags": [],
                 "model": "llama3.2", "backend": "ollama", "from_cache": true}
            ],
            "failed": [{"url": "https://c", "error": "needs JavaScript"}],
            "remaining": 4
        }));
        assert_eq!(r.mode, "distill");
        assert_eq!(r.title, "Distilled 2 pages");
        assert_eq!(r.badge, "", "a mixed sweep has no single freshness");
        assert!(r.meta.contains("2 pages distilled"), "meta: {}", r.meta);
        assert!(r.meta.contains("1 failed"), "meta: {}", r.meta);
        assert!(r.meta.contains("4 still undistilled"), "meta: {}", r.meta);
        assert!(r.blocks.iter().any(|(k, t, _)| k == "h2" && t == "A"));
        assert!(r.blocks.iter().any(|(k, t, _)| k == "p" && t == "What A says."));
        assert!(r.blocks.iter().any(|(k, t, _)| k == "bullet" && t == "fact one"));
        assert!(
            r.blocks.iter().any(|(k, t, _)| k == "quote" && t.contains("🏷 alpha, docs") && t.contains("Alpha")),
            "tag/entity line: {:?}", r.blocks
        );
        assert!(r.blocks.iter().any(|(k, _, _)| k == "rule"), "pages are separated");
        assert!(
            r.blocks.iter().any(|(k, t, _)| k == "quote" && t.contains("⚠") && t.contains("needs JavaScript")),
            "failures are visible: {:?}", r.blocks
        );
        assert_eq!(r.links.len(), 2, "each distilled page is a steerable row");
        assert_eq!(r.links[0].1, "https://a");
        assert_eq!(r.links[0].3, "ollama", "fresh distill chips its backend");
        assert_eq!(r.links[1].3, "cache", "hash-gated re-ask chips cache");

        // Single cached page → page-like identity + honest no-spend meta.
        let r = build_occipital_render(&json!({
            "kind": "distill", "count": 1,
            "distilled": [{"url": "https://a", "title": "A", "summary": "S.",
                           "key_points": [], "entities": [], "tags": ["t"],
                           "model": "llama3.2", "backend": "cache", "from_cache": true}],
            "failed": [], "remaining": 0
        }));
        assert_eq!(r.title, "A");
        assert_eq!(r.url, "https://a");
        assert_eq!(r.badge, "cache");
        assert!(r.meta.contains("no LLM spend"), "meta: {}", r.meta);
        assert_eq!(r.crumb_label, "distill: A");

        // Empty sweep → an honest empty state, not a blank pane.
        let r = build_occipital_render(&json!({
            "kind": "distill", "count": 0, "distilled": [], "failed": [], "remaining": 0
        }));
        assert!(
            r.blocks.iter().any(|(k, t, _)| k == "quote" && t.contains("already curated")),
            "empty sweep says so: {:?}", r.blocks
        );

        // The payload gate admits the kind (the two-places trap).
        assert!(occipital_payload(&json!({"kind": "distill", "distilled": []})).is_some());
    }

    #[test]
    fn build_occipital_render_related_rows() {
        let r = build_occipital_render(&json!({
            "kind": "related", "url": "https://a", "title": "A",
            "count": 1, "distilled_total": 7,
            "related": [
                {"url": "https://b", "title": "B", "summary_head": "What B says.",
                 "score": 3.0, "shared_entities": ["Alpha"], "shared_tags": ["docs"]}
            ]
        }));
        assert_eq!(r.mode, "related");
        assert_eq!(r.title, "A");
        assert_eq!(r.links.len(), 1);
        assert_eq!(r.links[0].0, "B");
        assert!(
            r.links[0].2.starts_with("🏷 Alpha, docs — What B says."),
            "shared terms lead the detail: {}", r.links[0].2
        );
        assert_eq!(r.links[0].3, "3.0", "overlap score is the chip");
        assert!(r.meta.contains("1 connected page"), "meta: {}", r.meta);
        assert!(r.meta.contains("7 distilled in store"), "meta: {}", r.meta);
        assert_eq!(r.crumb_label, "related: A");

        // Empty neighbourhood → the meta explains, the store size disambiguates.
        let r = build_occipital_render(&json!({
            "kind": "related", "url": "https://a", "title": "A",
            "count": 0, "distilled_total": 1, "related": []
        }));
        assert!(r.links.is_empty());
        assert!(r.meta.contains("0 connected pages · 1 distilled in store"), "meta: {}", r.meta);
        assert!(occipital_payload(&json!({"kind": "related", "related": []})).is_some());
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

        // page: an inline form annotation re-kinds to a "form" affordance
        // block (brackets stripped), and salvaged is honest in the meta.
        let r = build_occipital_render(&json!({
            "kind": "page", "url": "https://x", "title": "X", "from_cache": false,
            "salvaged": true,
            "markdown": "intro\n\n[form#1 → GET /search — search \"q\" · submit \"Go\"]"
        }));
        assert!(
            r.blocks.iter().any(|(k, t, _)| k == "form" && t.starts_with("form#1")),
            "form annotation should re-kind: {:?}", r.blocks
        );
        assert!(r.meta.contains("salvaged"), "meta: {}", r.meta);

        // page: JS-only wall → honest empty state, not a blank fetch
        let r = build_occipital_render(&json!({
            "kind": "page", "url": "https://spa", "title": "SPA",
            "from_cache": false, "js_required": true, "markdown": ""
        }));
        assert!(
            r.blocks.iter().any(|(k, t, _)| k == "quote" && t.contains("JavaScript")),
            "js_required should render an honest block: {:?}", r.blocks
        );

        // click → page layout + the interaction in the meta line
        let r = build_occipital_render(&json!({
            "kind": "click", "element": "link:3",
            "source_url": "https://a", "target_url": "https://b",
            "url": "https://b", "title": "B", "from_cache": false, "status": null,
            "markdown": "# B\n\nlanded"
        }));
        assert_eq!(r.mode, "click");
        assert_eq!(r.badge, "live");
        assert!(r.meta.contains("clicked link:3 → https://b"), "meta: {}", r.meta);
        assert!(r.blocks.iter().any(|(k, _, _)| k == "h1"));

        // submit → form + sent fields in the meta; freshness comes from
        // `cached` (NOT `from_cache` — a POST result is never cached)
        let r = build_occipital_render(&json!({
            "kind": "submit", "source_url": "https://a",
            "form": 1, "action": "https://a/search", "method": "get",
            "sent": [{"name": "q", "value": "rust"}], "status": 200, "cached": true,
            "url": "https://a/search?q=rust", "title": "results", "markdown": "## r"
        }));
        assert_eq!(r.mode, "submit");
        assert_eq!(r.badge, "cache");
        assert!(r.meta.contains("form#1 GET https://a/search"), "meta: {}", r.meta);
        assert!(r.meta.contains("q=rust"), "meta: {}", r.meta);
        assert!(r.meta.contains("HTTP 200"), "meta: {}", r.meta);

        // dom → ordinal-badged link rows + non-clickable form rows, no badge
        let r = build_occipital_render(&json!({
            "kind": "dom", "url": "https://x", "title": "X", "from_cache": true,
            "snapshot": true, "links": [{"idx": 1, "text": "a", "url": "https://a"}],
            "forms": [{"idx": 1, "action": "https://x/s", "method": "post",
                       "fields": [{"name": "q", "kind": "text"},
                                  {"name": "csrf", "kind": "hidden"}],
                       "submit": "Send"}]
        }));
        assert_eq!(r.mode, "dom");
        assert_eq!(r.badge, "", "dom is a registry — no freshness badge");
        assert_eq!(r.links[0].3, "#1");
        let form_row = &r.links[1];
        assert_eq!(form_row.0, "form#1 → POST https://x/s");
        assert_eq!(form_row.1, "", "form rows must not be steerable");
        assert!(form_row.2.contains("text \"q\""), "detail: {}", form_row.2);
        assert!(!form_row.2.contains("csrf"), "hidden fields stay out: {}", form_row.2);
        assert!(form_row.2.contains("submit \"Send\""), "detail: {}", form_row.2);
        assert!(r.meta.contains("1 link · 1 form"), "meta: {}", r.meta);
        assert!(r.meta.contains("snapshot held"), "meta: {}", r.meta);

        // an unknown kind that slips the gate renders honestly, never as recall
        // (this probe was "distill" until the distill card landed — it needs a
        // kind that stays unrendered)
        let r = build_occipital_render(&json!({"kind": "hologram", "url": "https://x"}));
        assert!(r.title.contains("hologram"), "title: {}", r.title);
        assert_ne!(r.mode, "recall", "the two-places trap: never silent-recall");
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

// ── Imagine (🖼) — the Imaginarium studio window (docs/imaginarium.md) ────────
// ui-slint is a thin HTTP client of the node-local Imaginarium daemon. Base URL
// and LAN token ride the SAME env vars the agentd MCP proxy uses
// (IMAGINARIUM_URL / IMAGINARIUM_TOKEN — /etc/agentd/env reaches both the
// daemon-side plugin and this UI's service unit), so one config wires both
// surfaces. The xAI key never appears here — that's the whole seam.

fn imagine_base_url() -> String {
    std::env::var("IMAGINARIUM_URL")
        .ok()
        .map(|s| s.trim().trim_end_matches('/').to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "http://127.0.0.1:8791".to_string())
}

/// The resolved (base, token) the Imagine callbacks use per call. Seeded from
/// env at boot; on a DESKTOP node (winit window in the user's session — no
/// /etc/agentd/env in sight, the file is 0600 root) the token arrives later
/// from agentd's `GET /api/imaginarium`, so callbacks must re-read every
/// invocation instead of baking a client at boot. Env wins when set.
static IMAGINE_REACH: std::sync::OnceLock<std::sync::Mutex<(String, String)>> =
    std::sync::OnceLock::new();

fn imagine_reach_cell() -> &'static std::sync::Mutex<(String, String)> {
    IMAGINE_REACH.get_or_init(|| {
        let token = std::env::var("IMAGINARIUM_TOKEN")
            .map(|t| imagine_token_clean(&t))
            .unwrap_or_default();
        std::sync::Mutex::new((imagine_base_url(), token))
    })
}

fn imagine_reach() -> (String, String) {
    imagine_reach_cell()
        .lock()
        .map(|g| g.clone())
        .unwrap_or_else(|e| e.into_inner().clone())
}

/// Heal the operator-footgun forms a hand-set token can take: surrounding
/// whitespace and one pair of matching quotes (a shell `export T="…"` keeps
/// them; systemd strips them — this makes both paths agree). Pure, tested.
fn imagine_token_clean(raw: &str) -> String {
    let t = raw.trim();
    let t = if t.len() >= 2
        && ((t.starts_with('"') && t.ends_with('"')) || (t.starts_with('\'') && t.ends_with('\'')))
    {
        &t[1..t.len() - 1]
    } else {
        t
    };
    t.trim().to_string()
}

/// A jobs-rail row in Send form: (id, mode, status, model, when, err, prompt).
type ImagineRow = (String, String, String, String, String, String, String);

/// Rows for the jobs rail from a `GET /v1/jobs` body (array of JobListItem).
/// Pure — malformed entries are skipped, a non-array yields no rows. Model is
/// shown without the shared "grok-imagine-" stem; created_at keeps its
/// date+minute prefix (RFC3339 "T" and SQLite " " both read fine).
fn imagine_job_rows(v: &Value) -> Vec<ImagineRow> {
    let Some(arr) = v.as_array() else { return Vec::new() };
    arr.iter()
        .filter_map(|j| {
            let id = j.get("job_id")?.as_str()?.to_string();
            let status = j.get("status").and_then(Value::as_str).unwrap_or("?").to_string();
            let mode = j.get("mode").and_then(Value::as_str).unwrap_or("").to_string();
            let model_full = j.get("model").and_then(Value::as_str).unwrap_or("");
            let model = model_full.strip_prefix("grok-imagine-").unwrap_or(model_full).to_string();
            let when = j.get("created_at").and_then(Value::as_str).unwrap_or("")
                .replace('T', " ")
                .chars()
                .take(16)
                .collect::<String>();
            let err = j.get("error").and_then(Value::as_str).unwrap_or("").to_string();
            let prompt = j.get("prompt").and_then(Value::as_str).unwrap_or("").to_string();
            Some((id, mode, status, model, when, err, prompt))
        })
        .collect()
}

/// One line summarizing a finished generation for the note strip: cost when the
/// JobResult carries usage, plus a multi-image reminder (only 00 previews here).
fn imagine_done_note(job: &Value, n: u32) -> String {
    let cost = job
        .get("usage")
        .and_then(|u| u.get("estimated_usd"))
        .and_then(Value::as_f64)
        .map(|c| format!(" · ~${c:.2}"))
        .unwrap_or_default();
    let extra = if n > 1 { format!(" · {n} images in the node library (first shown)") } else { String::new() };
    format!("done{cost}{extra}")
}

/// Attach the LAN token (when present) to a request — every /v1 call carries
/// auth explicitly; there is no default-header client to forget it in.
fn imagine_auth(req: reqwest::RequestBuilder, token: &str) -> reqwest::RequestBuilder {
    if token.is_empty() { req } else { req.bearer_auth(token) }
}

/// GET /v1/jobs?limit=40 → Ok(rows) | Err("auth"|"offline"). 401/403 = the
/// token is wrong; anything else unreachable/broken = offline (the view says
/// which). Never panics — an unparsable 200 is just zero rows.
async fn imagine_fetch_jobs(
    client: &reqwest::Client,
    base: &str,
    token: &str,
) -> Result<Vec<ImagineRow>, String> {
    let req = imagine_auth(client.get(format!("{base}/v1/jobs?limit=40")), token)
        .timeout(std::time::Duration::from_secs(6));
    match req.send().await {
        Ok(resp) if resp.status().as_u16() == 401 || resp.status().as_u16() == 403 => {
            Err("auth".into())
        }
        Ok(resp) if resp.status().is_success() => {
            let body = resp.json::<Value>().await.unwrap_or(Value::Null);
            Ok(imagine_job_rows(&body))
        }
        _ => Err("offline".into()),
    }
}

/// GET /v1/jobs/{id} → the full JobResult (Null on any failure).
async fn imagine_fetch_job(client: &reqwest::Client, base: &str, token: &str, id: &str) -> Value {
    let req = imagine_auth(client.get(format!("{base}/v1/jobs/{id}")), token)
        .timeout(std::time::Duration::from_secs(8));
    match req.send().await {
        Ok(resp) => resp.json::<Value>().await.unwrap_or(Value::Null),
        Err(_) => Value::Null,
    }
}

/// GET /v1/library/{job_id}/content → decoded RGBA (w, h, pixels), ready to
/// cross to the Slint thread (the slint::Image itself is not Send — the pixel
/// buffer is built inside the invoke closure, the thermal-heatmap idiom).
async fn imagine_fetch_preview(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    job_id: &str,
) -> Result<(u32, u32, Vec<u8>), String> {
    let resp = imagine_auth(client.get(format!("{base}/v1/library/{job_id}/content")), token)
        .timeout(std::time::Duration::from_secs(30))
        .send()
        .await
        .map_err(|e| format!("fetch failed: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("content HTTP {}", resp.status().as_u16()));
    }
    let bytes = resp.bytes().await.map_err(|e| format!("read failed: {e}"))?;
    let img = image::load_from_memory(&bytes).map_err(|e| format!("decode failed: {e}"))?;
    let rgba = img.to_rgba8();
    let (w, h) = (rgba.width(), rgba.height());
    Ok((w, h, rgba.into_raw()))
}

/// POST a generation body to the node and unwrap the JobResult envelope —
/// shared by image gen (synchronous upstream) and video submit (`no_wait`).
async fn imagine_post_job(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    path: &str,
    body: &Value,
) -> Result<Value, String> {
    let resp = imagine_auth(client.post(format!("{base}{path}")), token)
        .json(body)
        // Image gen is synchronous upstream — a quality 4-image batch takes a
        // while; be patient rather than strand a paid render. no_wait video
        // submits return in seconds regardless.
        .timeout(std::time::Duration::from_secs(180))
        .send()
        .await
        .map_err(|e| format!("node unreachable: {e}"))?;
    let code = resp.status().as_u16();
    let body = resp.json::<Value>().await.unwrap_or(Value::Null);
    if code == 401 || code == 403 {
        return Err("token rejected — check IMAGINARIUM_TOKEN".into());
    }
    if !(200..300).contains(&code) {
        let msg = body.get("error").and_then(Value::as_str).unwrap_or("generation failed");
        return Err(format!("HTTP {code}: {msg}"));
    }
    Ok(body)
}

/// POST /v1/images/generations. `model` is the chip slug — "auto" omits the
/// field (server default), "image"/"quality" pass through (ModelId aliases).
async fn imagine_generate_call(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    prompt: &str,
    model: &str,
    aspect: &str,
    n: u32,
) -> Result<Value, String> {
    let mut body = serde_json::json!({ "prompt": prompt, "n": n, "aspect_ratio": aspect });
    if model != "auto" {
        body["model"] = Value::String(model.to_string());
    }
    imagine_post_job(client, base, token, "/v1/images/generations", &body).await
}

/// Build the video submit request. Pure — returns (route, body). A "video"
/// source = an extension (continue the clip); an "image" source = I2V via a
/// `library:` ref (U1 — resolved on the node, never expires); no source = T2V.
/// Always `no_wait` — the rail polls, the human keeps working.
fn imagine_video_body(
    prompt: &str,
    model: &str,
    duration: i32,
    resolution: &str,
    aspect: &str,
    source_id: &str,
    source_kind: &str,
) -> (&'static str, Value) {
    if source_kind == "video" && !source_id.is_empty() {
        let mut body = serde_json::json!({
            "prompt": prompt,
            "video": format!("library:{source_id}"),
            "duration": duration.clamp(2, 10),
            "no_wait": true,
        });
        if model != "auto" {
            body["model"] = Value::String(model.to_string());
        }
        ("/v1/videos/extensions", body)
    } else {
        let mut body = serde_json::json!({
            "prompt": prompt,
            "duration": duration.clamp(1, 15),
            "no_wait": true,
        });
        if source_kind == "image" && !source_id.is_empty() {
            body["image"] = Value::String(format!("library:{source_id}"));
        }
        if model != "auto" {
            body["model"] = Value::String(model.to_string());
        }
        if !resolution.is_empty() && resolution != "default" {
            body["resolution"] = Value::String(resolution.to_string());
        }
        if !aspect.is_empty() && aspect != "default" {
            body["aspect_ratio"] = Value::String(aspect.to_string());
        }
        ("/v1/videos/generations", body)
    }
}

/// Jobs currently being driven by a watcher — clicking a pending row twice, or
/// submit + click, must not stack duplicate upstream-wait loops.
static IMAGINE_WATCHED: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

/// Drive a submitted (no_wait) job to completion. THE LOAD-BEARING ROUTE
/// CHOICE: `GET /v1/jobs/{id}` is a pure local-DB read — nothing on the node
/// polls xAI for a no_wait job, so a watcher on it spins on "pending" forever
/// (the 2026-07-29 field bug). `POST /v1/jobs/{id}/wait` is the route that
/// actually polls upstream and downloads the finished asset into the library;
/// it returns a terminal job as-is and a still-running job on its internal
/// window, so we loop it with a generous client timeout. Each round refreshes
/// the rail; on landing, the clip is auto-opened ONLY if the user hasn't
/// selected something else meanwhile (staging etiquette). Gives up after 20 min.
fn imagine_watch_job(rt: &tokio::runtime::Handle, uw: slint::Weak<AppWindow>, id: String) {
    {
        let mut watched = IMAGINE_WATCHED.lock().unwrap_or_else(|e| e.into_inner());
        if watched.contains(&id) {
            return;
        }
        watched.push(id.clone());
    }
    rt.spawn(async move {
        let started = std::time::Instant::now();
        loop {
            let (base, token) = imagine_reach();
            let client = reqwest::Client::new();
            // The server's internal wait window is shorter than this client
            // timeout — a slow render comes back as still-running, not an error.
            let job = match imagine_auth(
                client.post(format!("{base}/v1/jobs/{id}/wait")),
                &token,
            )
            .timeout(std::time::Duration::from_secs(180))
            .send()
            .await
            {
                Ok(resp) => resp.json::<Value>().await.unwrap_or(Value::Null),
                Err(_) => Value::Null,
            };
            let status = job.get("status").and_then(Value::as_str).unwrap_or("").to_string();
            let terminal = matches!(status.as_str(), "done" | "failed" | "expired" | "cancelled");
            let timed_out = started.elapsed().as_secs() > 1200;
            let uw2 = uw.clone();
            let id2 = id.clone();
            slint::invoke_from_event_loop(move || {
                if let Some(ui) = uw2.upgrade() {
                    ui.invoke_refresh_imagine();
                    if terminal && ui.get_imagine_selected_job().as_str() == id2 {
                        // Re-pick the finished job: the A1 flow fetches + arms ▶.
                        ui.invoke_imagine_pick_job(id2.into());
                    }
                }
            })
            .ok();
            if terminal || timed_out {
                let mut watched = IMAGINE_WATCHED.lock().unwrap_or_else(|e| e.into_inner());
                watched.retain(|w| w != &id);
                return;
            }
            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
        }
    });
}

/// Land a jobs-fetch outcome on the Slint thread: status + the rail rows move
/// together, so a 401/offline never shows stale jobs under a red dot. Rows
/// pick up their U3 poster from the cache when it has landed.
fn apply_imagine_rows(ui: &AppWindow, outcome: &Result<Vec<ImagineRow>, String>) {
    match outcome {
        Ok(rows) => {
            ui.set_imagine_status("ok".into());
            IMAGINE_JOBS.with(|m| {
                if let Some(model) = m.borrow().as_ref() {
                    model.set_vec(
                        rows.iter()
                            .map(|(id, mode, status, model_s, when, err, prompt)| {
                                let thumb = imagine_thumb_for(id);
                                ImagineJobItem {
                                    id: id.into(),
                                    mode: mode.into(),
                                    status: status.into(),
                                    model: model_s.into(),
                                    when: when.into(),
                                    err: err.into(),
                                    prompt: prompt.into(),
                                    has_thumb: thumb.is_some(),
                                    thumb: thumb.unwrap_or_default(),
                                }
                            })
                            .collect::<Vec<_>>(),
                    );
                }
            });
        }
        Err(e) => ui.set_imagine_status(e.as_str().into()),
    }
}

// ── The Cutting Room (A5, docs/imagine-studio.md) ────────────────────────────
// CUT mode turns the Imagine rail into a timeline editor over the node's craft
// engine (timeline contract v1, Imaginarium U2a/U2b/U3): click done jobs to add
// segments (video→clip, image→still, audio→music bed), trim with steppers,
// caption through the prompt box, style with chips, render with ?no_wait=true —
// the existing watcher tracks the craft job like any other. Rust owns the edit
// list; Slint sees only its projection.

/// Poster fetch state — `Failed` is terminal (audio jobs 404 by design; the
/// 3s watcher cadence must not become a refetch storm).
enum ThumbState {
    Loading,
    Ready(slint::Image),
    Failed,
}

fn imagine_thumb_for(id: &str) -> Option<slint::Image> {
    IMAGINE_THUMBS.with(|t| match t.borrow().get(id) {
        Some(ThumbState::Ready(img)) => Some(img.clone()),
        _ => None,
    })
}

/// Fetch missing posters for done rows (bounded per pass) and repaint the rows
/// they belong to as each lands. Runs on the Slint thread; the fetches don't.
fn imagine_thumb_backfill(rt: &tokio::runtime::Handle, uw: slint::Weak<AppWindow>) {
    let missing: Vec<String> = IMAGINE_JOBS.with(|m| {
        let Some(model) = m.borrow().as_ref().cloned() else {
            return Vec::new();
        };
        use slint::Model as _;
        let mut ids = Vec::new();
        for row in model.iter() {
            if row.status.as_str() == "done" {
                let id = row.id.to_string();
                let fresh = IMAGINE_THUMBS.with(|t| !t.borrow().contains_key(&id));
                if fresh {
                    ids.push(id);
                }
            }
            if ids.len() >= 12 {
                break;
            }
        }
        ids
    });
    if missing.is_empty() {
        return;
    }
    IMAGINE_THUMBS.with(|t| {
        let mut t = t.borrow_mut();
        for id in &missing {
            t.insert(id.clone(), ThumbState::Loading);
        }
    });
    rt.spawn(async move {
        let (base, token) = imagine_reach();
        let client = reqwest::Client::new();
        for id in missing {
            let fetched: Option<(u32, u32, Vec<u8>)> = async {
                let resp = imagine_auth(
                    client.get(format!("{base}/v1/library/{id}/thumb")),
                    &token,
                )
                .timeout(std::time::Duration::from_secs(10))
                .send()
                .await
                .ok()?;
                if !resp.status().is_success() {
                    return None;
                }
                let bytes = resp.bytes().await.ok()?;
                let img = image::load_from_memory(&bytes).ok()?.to_rgba8();
                let (w, h) = (img.width(), img.height());
                Some((w, h, img.into_raw()))
            }
            .await;
            let uw2 = uw.clone();
            let id2 = id.clone();
            slint::invoke_from_event_loop(move || {
                let state = match fetched {
                    Some((w, h, rgba)) => {
                        let buf = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(
                            &rgba, w, h,
                        );
                        ThumbState::Ready(slint::Image::from_rgba8(buf))
                    }
                    None => ThumbState::Failed,
                };
                IMAGINE_THUMBS.with(|t| {
                    t.borrow_mut().insert(id2.clone(), state);
                });
                if let Some(ui) = uw2.upgrade() {
                    // Repaint the row that just got its poster (in place), and
                    // the cut timeline in case it references this job.
                    use slint::Model as _;
                    IMAGINE_JOBS.with(|m| {
                        if let Some(model) = m.borrow().as_ref() {
                            for ix in 0..model.row_count() {
                                if let Some(mut row) = model.row_data(ix) {
                                    if row.id.as_str() == id2 {
                                        if let Some(img) = imagine_thumb_for(&id2) {
                                            row.thumb = img;
                                            row.has_thumb = true;
                                            model.set_row_data(ix, row);
                                        }
                                        break;
                                    }
                                }
                            }
                        }
                    });
                    cut_project(&ui);
                }
            })
            .ok();
        }
    });
}

/// One timeline segment, Rust-side truth. kind: 0 clip · 1 still · 2 card.
#[derive(Clone, Debug, PartialEq)]
struct CutSeg {
    kind: u8,
    job_id: String,
    label: String,
    in_s: f32,
    out_s: f32,
    gain_ix: i32,  // 0 → -12dB · 1 → -6 · 2 → 0
    speed_ix: i32, // 0 → 0.5× · 1 → 1× · 2 → 1.5× · 3 → 2×
    dur_ix: i32,   // 0 → 2s · 1 → 3s · 2 → 4s · 3 → 6s
    zoom_ix: i32,  // 0 static · 1 → 1.12 · 2 → 1.25
    color_ix: i32,
    caption: String,
}

const CUT_GAINS_DB: [f64; 3] = [-12.0, -6.0, 0.0];
const CUT_SPEEDS: [f64; 4] = [0.5, 1.0, 1.5, 2.0];
const CUT_DURS_S: [f64; 4] = [2.0, 3.0, 4.0, 6.0];
const CUT_ZOOM_TO: [f64; 3] = [1.0, 1.12, 1.25];
const CUT_CARD_COLORS: [&str; 4] = ["#000000", "#101418", "#B7410E", "#0E2A1C"];

impl CutSeg {
    fn clip(job_id: &str, label: &str) -> Self {
        Self {
            kind: 0,
            job_id: job_id.into(),
            label: label.into(),
            in_s: 0.0,
            out_s: 0.0,
            gain_ix: 2,
            speed_ix: 1,
            dur_ix: 1,
            zoom_ix: 1,
            color_ix: 1,
            caption: String::new(),
        }
    }
    fn still(job_id: &str, label: &str) -> Self {
        Self { kind: 1, ..Self::clip(job_id, label) }
    }
    fn card() -> Self {
        Self { kind: 2, ..Self::clip("", "card") }
    }
}

/// The row's summary line — trim window, speed, gain / duration + zoom / color.
fn cut_detail(seg: &CutSeg) -> String {
    match seg.kind {
        0 => {
            let win = if seg.out_s > seg.in_s {
                format!("{:.1}–{:.1}s", seg.in_s, seg.out_s)
            } else if seg.in_s > 0.0 {
                format!("{:.1}s→end", seg.in_s)
            } else {
                "full".into()
            };
            let speed = if seg.speed_ix != 1 {
                format!(" · {}×", CUT_SPEEDS[seg.speed_ix.clamp(0, 3) as usize])
            } else {
                String::new()
            };
            let gain = if seg.gain_ix != 2 {
                format!(" · {}dB", CUT_GAINS_DB[seg.gain_ix.clamp(0, 2) as usize])
            } else {
                String::new()
            };
            format!("{win}{speed}{gain}")
        }
        1 => format!(
            "{}s · {}",
            CUT_DURS_S[seg.dur_ix.clamp(0, 3) as usize],
            ["static", "push", "push+"][seg.zoom_ix.clamp(0, 2) as usize]
        ),
        _ => format!("{}s card", CUT_DURS_S[seg.dur_ix.clamp(0, 3) as usize]),
    }
}

/// Rough master-clock length — clips with an open out-point are unknown until
/// the node probes them, shown as "+?".
fn cut_total_label(segs: &[CutSeg], music: bool) -> String {
    if segs.is_empty() {
        return String::new();
    }
    let mut known = 0.0f64;
    let mut open = false;
    for s in segs {
        match s.kind {
            0 => {
                if s.out_s > s.in_s {
                    known += ((s.out_s - s.in_s) as f64)
                        / CUT_SPEEDS[s.speed_ix.clamp(0, 3) as usize];
                } else {
                    open = true;
                }
            }
            _ => known += CUT_DURS_S[s.dur_ix.clamp(0, 3) as usize],
        }
    }
    format!(
        "{} segment{} · ~{:.0}s{}{}",
        segs.len(),
        if segs.len() == 1 { "" } else { "s" },
        known,
        if open { "+?" } else { "" },
        if music { " · 🎵" } else { "" }
    )
}

/// Build the craft request (timeline contract v1) from the edit list — pure.
/// Captions ride segment-local with an over-long window (clamped by the
/// segment's real duration at render; drawtext past-dur enables never fire).
fn cut_build_timeline(
    segs: &[CutSeg],
    music: Option<&str>,
    letterbox_ix: i32,
    loudnorm: bool,
    fades_ix: i32,
) -> Value {
    let clips: Vec<Value> = segs
        .iter()
        .map(|s| {
            let caption = (!s.caption.is_empty()).then(|| {
                serde_json::json!([{
                    "text": s.caption,
                    "start_s": 0.0,
                    "end_s": 600.0,
                    "fontsize": if s.kind == 2 { 44 } else { 0 },
                }])
            });
            let mut v = match s.kind {
                0 => {
                    let mut c = serde_json::json!({
                        "job_id": s.job_id,
                        "in_s": s.in_s,
                        "out_s": s.out_s,
                        "gain_db": CUT_GAINS_DB[s.gain_ix.clamp(0, 2) as usize],
                    });
                    let speed = CUT_SPEEDS[s.speed_ix.clamp(0, 3) as usize];
                    if (speed - 1.0).abs() > 1e-6 {
                        c["speed"] = serde_json::json!(speed);
                    }
                    c
                }
                1 => {
                    let mut c = serde_json::json!({
                        "kind": "still",
                        "job_id": s.job_id,
                        "dur_s": CUT_DURS_S[s.dur_ix.clamp(0, 3) as usize],
                    });
                    let zoom = CUT_ZOOM_TO[s.zoom_ix.clamp(0, 2) as usize];
                    if zoom > 1.0 {
                        c["zoom_to"] = serde_json::json!(zoom);
                    }
                    c
                }
                _ => serde_json::json!({
                    "kind": "card",
                    "dur_s": CUT_DURS_S[s.dur_ix.clamp(0, 3) as usize],
                    "card_color": CUT_CARD_COLORS[s.color_ix.clamp(0, 3) as usize],
                }),
            };
            if let Some(c) = caption {
                v["captions"] = c;
            }
            v
        })
        .collect();

    let mut style = serde_json::Map::new();
    match letterbox_ix {
        1 => {
            style.insert("letterbox_frac".into(), serde_json::json!(0.12));
        }
        2 => {
            style.insert("letterbox_frac".into(), serde_json::json!(0.12));
            style.insert("letterbox_reveal_s".into(), serde_json::json!(1.5));
        }
        _ => {}
    }
    if loudnorm {
        style.insert("loudnorm".into(), serde_json::json!(true));
    }

    let mut body = serde_json::json!({
        "version": 1,
        "clips": clips,
        "note": format!("cutting room · {} segments", segs.len()),
    });
    if !style.is_empty() {
        body["style"] = Value::Object(style);
    }
    match fades_ix {
        1 => {
            body["video_fade_in_s"] = serde_json::json!(0.3);
            body["video_fade_out_s"] = serde_json::json!(0.3);
            body["audio_fade_in_s"] = serde_json::json!(0.3);
            body["audio_fade_out_s"] = serde_json::json!(0.5);
        }
        2 => {
            body["video_fade_in_s"] = serde_json::json!(0.75);
            body["video_fade_out_s"] = serde_json::json!(0.75);
            body["audio_fade_in_s"] = serde_json::json!(0.5);
            body["audio_fade_out_s"] = serde_json::json!(1.0);
        }
        _ => {}
    }
    if let Some(id) = music {
        body["music"] = serde_json::json!({
            "job_id": id,
            "gain_db": -8.0,
            "fade_in_s": 0.3,
            "fade_out_s": 0.8,
        });
    }
    body
}

/// Re-project the edit list into the Slint model + the summary label.
fn cut_project(ui: &AppWindow) {
    let (rows, total) = CUT_SEGS.with(|s| {
        let segs = s.borrow();
        let music = CUT_MUSIC.with(|m| m.borrow().is_some());
        let rows: Vec<CutSegItem> = segs
            .iter()
            .map(|seg| {
                let thumb = (!seg.job_id.is_empty())
                    .then(|| imagine_thumb_for(&seg.job_id))
                    .flatten();
                CutSegItem {
                    kind: seg.kind as i32,
                    id: seg.job_id.as_str().into(),
                    label: seg.label.as_str().into(),
                    detail: cut_detail(seg).into(),
                    caption: seg.caption.as_str().into(),
                    has_thumb: thumb.is_some(),
                    thumb: thumb.unwrap_or_default(),
                    in_s: seg.in_s,
                    out_s: seg.out_s,
                    gain_ix: seg.gain_ix,
                    speed_ix: seg.speed_ix,
                    dur_ix: seg.dur_ix,
                    zoom_ix: seg.zoom_ix,
                    color_ix: seg.color_ix,
                }
            })
            .collect();
        (rows, cut_total_label(&segs, music))
    });
    CUT_MODEL.with(|m| {
        if let Some(model) = m.borrow().as_ref() {
            model.set_vec(rows);
        }
    });
    ui.set_imagine_cut_total(total.into());
    ui.set_imagine_cut_music_label(
        CUT_MUSIC
            .with(|m| m.borrow().as_ref().map(|(_, l)| l.clone()))
            .unwrap_or_default()
            .into(),
    );
}

/// Apply one editor verb to the selected segment. Stepper deltas clamp sanely;
/// unknown fields are ignored (a UI/Rust drift shows as a no-op, not a panic).
fn cut_apply(seg: &mut CutSeg, field: &str, value: &str) {
    let num = value.parse::<f32>().unwrap_or(0.0);
    match field {
        "in" => seg.in_s = (seg.in_s + num).max(0.0),
        "out" => {
            // out steps from its current point; stepping down through in_s
            // reopens the clip (0 = full remaining).
            let base = if seg.out_s > 0.0 { seg.out_s } else { seg.in_s };
            let next = base + num;
            seg.out_s = if next > seg.in_s { next } else { 0.0 };
        }
        "gain" => seg.gain_ix = num as i32,
        "speed" => seg.speed_ix = num as i32,
        "dur" => seg.dur_ix = num as i32,
        "zoom" => seg.zoom_ix = num as i32,
        "color" => seg.color_ix = num as i32,
        "caption" => seg.caption = value.trim().chars().take(120).collect(),
        "caption-clear" => seg.caption.clear(),
        _ => {}
    }
}

/// Data-URL mime for a sonus track by extension — the import route derives the
/// stored extension from the filename hint, so this only needs to be sane.
fn sonus_mime_for_name(name: &str) -> &'static str {
    let ext = name.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    match ext.as_str() {
        "mp3" => "audio/mpeg",
        "flac" => "audio/flac",
        "ogg" => "audio/ogg",
        "m4a" => "audio/mp4",
        "opus" => "audio/opus",
        _ => "audio/wav",
    }
}

/// Build the image-edit request (A4): up to 3 `library:` refs, model left to
/// the server's edit default (the gen chips' aliases don't apply). Pure.
fn imagine_edit_body(prompt: &str, n: u32, sources: &[(String, String)]) -> Value {
    let images: Vec<Value> = sources
        .iter()
        .take(3)
        .map(|(id, _)| Value::String(format!("library:{id}")))
        .collect();
    serde_json::json!({ "prompt": prompt, "images": images, "n": n })
}

/// Re-project the edit-source list into its chip row model.
fn edit_sources_project(ui: &AppWindow) {
    let rows: Vec<ImageItem> = EDIT_SOURCES.with(|s| {
        s.borrow()
            .iter()
            .map(|(id, stem)| ImageItem {
                name: stem.as_str().into(),
                path: id.as_str().into(),
            })
            .collect()
    });
    let n = rows.len() as i32;
    EDIT_SOURCES_MODEL.with(|m| {
        if let Some(model) = m.borrow().as_ref() {
            model.set_vec(rows);
        }
    });
    ui.set_imagine_edit_count(n);
}

/// The brief handed to APEX when the human hits "ask APEX to compose" —
/// a plain queued user_prompt (the occipital-steer idiom: rides the WS through
/// the TurnGate, so it can never race an in-flight turn).
fn cut_compose_prompt(total: &str) -> String {
    format!(
        "(cutting room) I'm editing a video in the Imagine window ({}). \
         Compose a music bed for it with your sonus tools — something short and \
         cinematic that fits the cut. When the track has rendered into the sonus \
         library, tell me its filename; I'll score the cut with it from the \
         🎵 SCORE picker.",
        if total.is_empty() { "empty timeline so far" } else { total }
    )
}

// ── Imagine video player (A1, docs/imagine-studio.md) ─────────────────────────
// Slint has no video element; the player is a hand-rolled ffmpeg pipeline on
// three field-proven idioms: fetch-then-decode (clips are 2–20 MB; upstream has
// no Range and doesn't need it), `ffmpeg -f rawvideo` frames → a bounded
// channel → a Slint Timer painting SharedPixelBuffers (the thermal-heatmap
// idiom), and audio demuxed by a second ffmpeg piped into `aplay` (the
// client-side voice idiom). Clips are 4–15 s, so starting both pipelines
// together bounds AV drift to tens of ms — no sync engine, on purpose.
// Decode happens AT WINDOW SIZE (never native res) — that's the old-hardware
// story. ffmpeg CLI only; gstreamer/libav bindings are a locked-decision no.

/// One decoded frame (or end-of-stream) crossing decoder → Slint thread.
enum PlayerMsg {
    Frame(u32, u32, Vec<u8>),
    End,
}

/// Playback generation counter — bumping it orphans every in-flight decoder
/// loop (they check before sending), so stop/replay/new-selection can't race.
static IMAGINE_PLAY_GEN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// Live pipeline children (video ffmpeg, audio ffmpeg, aplay) — killed on stop.
static PLAYER_PROCS: std::sync::Mutex<Vec<tokio::process::Child>> = std::sync::Mutex::new(Vec::new());

thread_local! {
    /// The paint timer + frame receiver + pacing state (Slint thread only).
    static PLAYER_TIMER: RefCell<Option<slint::Timer>> = const { RefCell::new(None) };
    static PLAYER_RX: RefCell<Option<tokio::sync::mpsc::Receiver<PlayerMsg>>> = const { RefCell::new(None) };
    /// (started_at, fps, duration_s, frames_shown)
    static PLAYER_CLOCK: RefCell<Option<(std::time::Instant, f32, f32, u64)>> = const { RefCell::new(None) };
    /// The clip pick_job prepared: (cached file, duration_s, src_w, src_h).
    static IMAGINE_CLIP: RefCell<Option<(std::path::PathBuf, f32, u32, u32)>> = const { RefCell::new(None) };
}

/// Playback decode rate — constant regardless of source (`fps=` filter), so
/// pacing math is trivial and slow nodes drop frames instead of drifting.
const IMAGINE_PLAY_FPS: f32 = 24.0;

fn imagine_cache_dir() -> std::path::PathBuf {
    let base = std::env::var("XDG_CACHE_HOME")
        .ok()
        .filter(|s| !s.is_empty())
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
            std::path::Path::new(&home).join(".cache")
        });
    base.join("apexos-rs").join("imagine")
}

/// Fit (src_w, src_h) into (max_w, max_h) preserving aspect, both dims rounded
/// DOWN to even (yuv/rawvideo requirement — the cutting-room pitfall). Pure.
fn fit_dims(src_w: u32, src_h: u32, max_w: u32, max_h: u32) -> (u32, u32) {
    if src_w == 0 || src_h == 0 || max_w < 2 || max_h < 2 {
        return (2, 2);
    }
    let scale = (max_w as f64 / src_w as f64).min(max_h as f64 / src_h as f64).min(1.0);
    let w = ((src_w as f64 * scale) as u32).max(2) & !1;
    let h = ((src_h as f64 * scale) as u32).max(2) & !1;
    (w, h)
}

/// Parse `ffprobe -show_entries stream=width,height:format=duration` CSV output
/// (one value per line, in that order once streams precede format). Pure.
fn parse_ffprobe_video(out: &str) -> Option<(u32, u32, f32)> {
    let mut w = None;
    let mut h = None;
    let mut dur = None;
    for tok in out.split(['\n', ',']).map(str::trim) {
        if tok.is_empty() || tok == "N/A" {
            continue;
        }
        if let Ok(i) = tok.parse::<u32>() {
            if w.is_none() {
                w = Some(i);
            } else if h.is_none() {
                h = Some(i);
            }
        } else if let Ok(f) = tok.parse::<f32>() {
            if dur.is_none() && f > 0.0 {
                dur = Some(f);
            }
        }
    }
    Some((w?, h?, dur.unwrap_or(0.0)))
}

/// ffprobe the clip: (width, height, duration_s). None = ffprobe missing/failed.
async fn imagine_probe_video(path: &std::path::Path) -> Option<(u32, u32, f32)> {
    let out = tokio::process::Command::new("ffprobe")
        .args([
            "-v", "error",
            "-select_streams", "v:0",
            "-show_entries", "stream=width,height",
            "-show_entries", "format=duration",
            "-of", "csv=p=0",
        ])
        .arg(path)
        .output()
        .await
        .ok()?;
    parse_ffprobe_video(&String::from_utf8_lossy(&out.stdout))
}

/// Download a job's content into the imagine cache (skip when already there).
async fn imagine_download_clip(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    job_id: &str,
) -> Result<std::path::PathBuf, String> {
    let dir = imagine_cache_dir();
    let path = dir.join(format!("{job_id}.mp4"));
    if tokio::fs::metadata(&path).await.map(|m| m.len() > 0).unwrap_or(false) {
        return Ok(path);
    }
    tokio::fs::create_dir_all(&dir).await.map_err(|e| format!("cache dir: {e}"))?;
    let resp = imagine_auth(client.get(format!("{base}/v1/library/{job_id}/content")), token)
        .timeout(std::time::Duration::from_secs(60))
        .send()
        .await
        .map_err(|e| format!("fetch failed: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("content HTTP {}", resp.status().as_u16()));
    }
    let bytes = resp.bytes().await.map_err(|e| format!("read failed: {e}"))?;
    let tmp = dir.join(format!(".{job_id}.tmp"));
    tokio::fs::write(&tmp, &bytes).await.map_err(|e| format!("cache write: {e}"))?;
    tokio::fs::rename(&tmp, &path).await.map_err(|e| format!("cache rename: {e}"))?;
    Ok(path)
}

/// One decoded RGBA frame at (w, h) — the poster. Uses the same rawvideo path
/// as playback so a poster failing = playback would have failed too (honest).
async fn imagine_poster(path: &std::path::Path, w: u32, h: u32) -> Option<(u32, u32, Vec<u8>)> {
    let out = tokio::process::Command::new("ffmpeg")
        .args(["-nostdin", "-v", "error", "-i"])
        .arg(path)
        .args([
            "-frames:v", "1",
            "-f", "rawvideo",
            "-pix_fmt", "rgba",
            "-vf", &format!("scale={w}:{h}"),
            "-",
        ])
        .output()
        .await
        .ok()?;
    let need = (w * h * 4) as usize;
    if out.stdout.len() < need {
        return None;
    }
    Some((w, h, out.stdout[..need].to_vec()))
}

/// Kill every live pipeline child and orphan in-flight decoder loops.
fn imagine_player_kill() {
    IMAGINE_PLAY_GEN.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    if let Ok(mut procs) = PLAYER_PROCS.lock() {
        for child in procs.iter_mut() {
            let _ = child.start_kill();
        }
        procs.clear();
    }
}

/// Spawn the two pipelines for `path` at (w, h): rawvideo frames → the channel,
/// and demuxed audio → `aplay` (best-effort — a node without audio still plays
/// video). Runs on the tokio runtime; `gen` orphans it when playback stops.
fn imagine_spawn_pipelines(
    rt: &tokio::runtime::Handle,
    path: std::path::PathBuf,
    w: u32,
    h: u32,
    r#gen: u64,
    tx: tokio::sync::mpsc::Sender<PlayerMsg>,
) {
    rt.spawn(async move {
        use tokio::io::AsyncReadExt;

        // Video: constant-fps rawvideo stream at display size.
        let mut video = match tokio::process::Command::new("ffmpeg")
            .args(["-nostdin", "-v", "quiet", "-i"])
            .arg(&path)
            .args([
                "-f", "rawvideo",
                "-pix_fmt", "rgba",
                "-vf", &format!("scale={w}:{h},fps={IMAGINE_PLAY_FPS}"),
                "-an",
                "-",
            ])
            .stdout(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
        {
            Ok(c) => c,
            Err(e) => {
                eprintln!("[imagine] ffmpeg spawn failed: {e}");
                let _ = tx.send(PlayerMsg::End).await;
                return;
            }
        };
        let mut stdout = match video.stdout.take() {
            Some(s) => s,
            None => {
                let _ = tx.send(PlayerMsg::End).await;
                return;
            }
        };

        // Audio: second ffmpeg demuxes to WAV, piped straight into aplay.
        // Best-effort — desktop PipeWire and kiosk ALSA both reach aplay (the
        // voice-arc idiom); failure just means a silent clip.
        let audio = tokio::process::Command::new("ffmpeg")
            .args(["-nostdin", "-v", "quiet", "-i"])
            .arg(&path)
            .args(["-vn", "-f", "wav", "-"])
            .stdout(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn();
        if let Ok(mut ff_audio) = audio {
            if let Some(a_stdio) = ff_audio
                .stdout
                .take()
                .and_then(|out| out.into_owned_fd().ok())
                .map(std::process::Stdio::from)
            {
                match tokio::process::Command::new("aplay")
                    .arg("-q")
                    .stdin(a_stdio)
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .kill_on_drop(true)
                    .spawn()
                {
                    Ok(aplay) => {
                        if let Ok(mut procs) = PLAYER_PROCS.lock() {
                            procs.push(ff_audio);
                            procs.push(aplay);
                        }
                    }
                    Err(_) => {
                        let _ = ff_audio.start_kill();
                    }
                }
            }
        }

        // The video child handle joins the kill list too (stdout stays here).
        if let Ok(mut procs) = PLAYER_PROCS.lock() {
            procs.push(video);
        }

        let frame_len = (w * h * 4) as usize;
        let mut buf = vec![0u8; frame_len];
        loop {
            if IMAGINE_PLAY_GEN.load(std::sync::atomic::Ordering::SeqCst) != r#gen {
                return; // orphaned — a newer playback owns the stage
            }
            match stdout.read_exact(&mut buf).await {
                Ok(_) => {
                    if tx.send(PlayerMsg::Frame(w, h, buf.clone())).await.is_err() {
                        return;
                    }
                }
                Err(_) => {
                    let _ = tx.send(PlayerMsg::End).await;
                    return;
                }
            }
        }
    });
}

/// mm:ss for the progress line.
fn imagine_clock(t: f32) -> String {
    let s = t.max(0.0) as u64;
    format!("{}:{:02}", s / 60, s % 60)
}

/// Stop playback (Slint thread): kill pipelines, stop the paint timer, drop the
/// channel + clock. `state` is what the stage shows after ("idle" | "ended" —
/// ended keeps the last frame up and offers replay).
fn imagine_player_reset(ui: &AppWindow, state: &str) {
    imagine_player_kill();
    PLAYER_TIMER.with(|t| {
        if let Some(timer) = t.borrow_mut().take() {
            timer.stop();
        }
    });
    PLAYER_RX.with(|r| r.borrow_mut().take());
    PLAYER_CLOCK.with(|c| c.borrow_mut().take());
    ui.set_imagine_video_state(state.into());
}

/// One paint tick (Slint thread, ~40 ms): drain frames due by the wall clock —
/// slow decode drops frames and catches up instead of drifting against audio —
/// paint the newest, update progress, and close the stage on end-of-stream.
fn imagine_player_tick(ui: &AppWindow) {
    let Some((start, fps, dur, mut shown)) = PLAYER_CLOCK.with(|c| *c.borrow()) else {
        return;
    };
    let due = (start.elapsed().as_secs_f32() * fps) as u64;
    let mut latest: Option<(u32, u32, Vec<u8>)> = None;
    let mut ended = false;
    PLAYER_RX.with(|r| {
        if let Some(rx) = r.borrow_mut().as_mut() {
            while shown <= due {
                match rx.try_recv() {
                    Ok(PlayerMsg::Frame(w, h, px)) => {
                        latest = Some((w, h, px));
                        shown += 1;
                    }
                    Ok(PlayerMsg::End) => {
                        ended = true;
                        break;
                    }
                    Err(_) => break,
                }
            }
        }
    });
    if let Some((w, h, px)) = latest {
        let buf = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(&px, w, h);
        ui.set_imagine_preview(slint::Image::from_rgba8(buf));
    }
    let t = start.elapsed().as_secs_f32().min(dur.max(0.01));
    if dur > 0.0 {
        ui.set_imagine_video_progress((t / dur).min(1.0));
    }
    ui.set_imagine_video_time(format!("{} / {}", imagine_clock(t), imagine_clock(dur)).into());
    PLAYER_CLOCK.with(|c| {
        if let Some(s) = c.borrow_mut().as_mut() {
            s.3 = shown;
        }
    });
    if ended {
        imagine_player_reset(ui, "ended");
        ui.set_imagine_video_progress(1.0);
    }
}

/// Ask agentd for the node's Imaginarium reach (`GET /api/imaginarium`, gated —
/// works with the admin token AND a minted login session) and store it. This is
/// the DESKTOP path: the winit UI can't read the 0600 /etc/agentd/env, agentd
/// can — and serves the systemd-parsed values, immune to shell-quoting/dup-line
/// footguns. Called only when the env token is absent; per-var env still wins.
/// Returns true when a non-empty token landed.
async fn imagine_fetch_reach(client: &reqwest::Client, http_base: &str) -> bool {
    let v = json_get(client, format!("{http_base}/api/imaginarium")).await;
    let url = v
        .get("url")
        .and_then(Value::as_str)
        .map(|s| s.trim().trim_end_matches('/').to_string())
        .unwrap_or_default();
    let token = v
        .get("token")
        .and_then(Value::as_str)
        .map(imagine_token_clean)
        .unwrap_or_default();
    if token.is_empty() {
        return false;
    }
    let env_url_set = std::env::var("IMAGINARIUM_URL")
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false);
    if let Ok(mut g) = imagine_reach_cell().lock() {
        if !url.is_empty() && !env_url_set {
            g.0 = url;
        }
        g.1 = token;
    }
    true
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
    history_budget_label: String,
    history_usage:        String,
    sensor_profile:     String,
    voice_backend:        String,
    voice_api_available:  bool,
    backend:       String,
    oai_base_url:  String,
    oai_key_set:   bool,
}

/// Stepped Settings label for a budget (0 → "off"); non-preset values render as
/// "<n>k" so an env-seeded custom budget still displays (no button highlights —
/// that's honest).
fn history_budget_label(budget: u64) -> String {
    if budget == 0 { "off".into() } else { format!("{}k", budget / 1000) }
}

/// The tapped Settings label → tokens for POST /api/history. Unknown → None.
fn history_label_tokens(label: &str) -> Option<u64> {
    match label {
        "off"  => Some(0),
        "60k"  => Some(60_000),
        "120k" => Some(120_000),
        "200k" => Some(200_000),
        "400k" => Some(400_000),
        _      => None,
    }
}

/// The "window in use" readout from GET /api/history: the largest loaded session
/// (sessions[] arrives sorted desc) against the budget. Empty when nothing loaded.
fn history_usage_line(h: &serde_json::Value) -> String {
    let (sid, est) = match h["sessions"].as_array().and_then(|a| a.first()) {
        Some(s) => (s["session_id"].as_u64().unwrap_or(0), s["est_tokens"].as_u64().unwrap_or(0)),
        None => return String::new(),
    };
    let budget = h["budget"].as_u64().unwrap_or(0);
    if budget == 0 {
        format!("Largest window: session {sid} ≈ {}k tokens (trimming off)", est / 1000)
    } else {
        let trigger = h["trim_trigger"].as_u64().unwrap_or(budget + budget / 5);
        format!(
            "Largest window: session {sid} ≈ {}k of {}k (trims at {}k)",
            est / 1000, budget / 1000, trigger / 1000
        )
    }
}

// Fetch /api/status, /api/soul, /api/models, /api/cache, /api/history,
// /api/sensors/config, /api/voice, /api/backend in parallel.
async fn fetch_settings(client: &reqwest::Client, base_url: &str) -> SettingsData {
    let (status, soul, models_resp, cache, history, sensors, voice, backend) = tokio::join!(
        json_get(client, format!("{base_url}/api/status")),
        json_get(client, format!("{base_url}/api/soul")),
        json_get(client, format!("{base_url}/api/models")),
        json_get(client, format!("{base_url}/api/cache")),
        json_get(client, format!("{base_url}/api/history")),
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
        // Defaults ("120k", no readout) if agentd predates /api/history.
        history_budget_label: history["budget"].as_u64()
            .map(history_budget_label).unwrap_or_else(|| "120k".into()),
        history_usage:        history_usage_line(&history),
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
        workers:   Rc::new(slint::VecModel::default()),
        active:    Rc::new(slint::VecModel::default()),
        blocked:   Rc::new(slint::VecModel::default()),
        subagents: Rc::new(slint::VecModel::default()),
        recent:    Rc::new(slint::VecModel::default()),
    };
    ui.set_board_goals(slint::ModelRc::from(board.goals.clone()));
    ui.set_board_workers(slint::ModelRc::from(board.workers.clone()));
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

    // Imagine (🖼) — the studio's shared node jobs rail.
    let imagine_jobs_vec: Rc<slint::VecModel<ImagineJobItem>> = Rc::new(slint::VecModel::default());
    ui.set_imagine_jobs(slint::ModelRc::from(imagine_jobs_vec.clone()));
    IMAGINE_JOBS.with(|m| *m.borrow_mut() = Some(imagine_jobs_vec.clone()));

    // Imagine prompt-from-file picker (workspace text files via agentd).
    let imagine_prompt_files_vec: Rc<slint::VecModel<ImageItem>> = Rc::new(slint::VecModel::default());
    ui.set_imagine_prompt_files(slint::ModelRc::from(imagine_prompt_files_vec.clone()));
    IMAGINE_PROMPT_FILES.with(|m| *m.borrow_mut() = Some(imagine_prompt_files_vec.clone()));

    // The Cutting Room timeline (A5) — projection of the Rust edit list.
    let cut_segs_vec: Rc<slint::VecModel<CutSegItem>> = Rc::new(slint::VecModel::default());
    ui.set_imagine_cut_segments(slint::ModelRc::from(cut_segs_vec.clone()));
    CUT_MODEL.with(|m| *m.borrow_mut() = Some(cut_segs_vec.clone()));

    // Image-edit source chips (A4).
    let edit_sources_vec: Rc<slint::VecModel<ImageItem>> = Rc::new(slint::VecModel::default());
    ui.set_imagine_edit_sources(slint::ModelRc::from(edit_sources_vec.clone()));
    EDIT_SOURCES_MODEL.with(|m| *m.borrow_mut() = Some(edit_sources_vec.clone()));

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
                    AppKind::Imagine => ui.invoke_refresh_imagine(),
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

    // ── Imagine (🖼): the Imaginarium studio (docs/imaginarium.md) ────────────
    // The reach (base URL + LAN token) is process-global and re-read on EVERY
    // call: env seeds it, and on a desktop node — where the winit window never
    // sees /etc/agentd/env — agentd's `GET /api/imaginarium` fills it in after
    // login. No baked default-header client: a token that arrives late still
    // reaches the next request. The xAI key never appears UI-side.
    {
        let (b, t) = imagine_reach();
        ui.set_imagine_node_url(b.into());
        if t.is_empty() {
            // Desktop path: ask agentd once at boot (post-login re-exec has the
            // agentd token, so this succeeds exactly when it can). A refresh
            // click retries it, so a race here only costs one ⟳.
            let client = Arc::clone(&http_client);
            let hb = http_base.clone();
            let uw = ui.as_weak();
            rt.spawn(async move {
                if imagine_fetch_reach(&client, &hb).await {
                    slint::invoke_from_event_loop(move || {
                        if let Some(ui) = uw.upgrade() {
                            ui.set_imagine_node_url(imagine_reach().0.into());
                            ui.invoke_refresh_imagine();
                        }
                    })
                    .ok();
                }
            });
        }
    }

    {
        let rt_h = rt.handle().clone();
        let agentd_client = Arc::clone(&http_client);
        let agentd_base = http_base.clone();
        let uw = ui.as_weak();
        ui.on_refresh_imagine(move || {
            let agentd_client = Arc::clone(&agentd_client);
            let agentd_base = agentd_base.clone();
            let uw = uw.clone();
            let rt_h2 = rt_h.clone();
            rt_h.spawn(async move {
                // Token still missing (pre-login boot raced, or agentd had none
                // yet) → retry the agentd reach before giving an honest state.
                if imagine_reach().1.is_empty() {
                    imagine_fetch_reach(&agentd_client, &agentd_base).await;
                }
                let (base, token) = imagine_reach();
                let outcome = if token.is_empty() {
                    Err("unconfigured".to_string())
                } else {
                    imagine_fetch_jobs(&reqwest::Client::new(), &base, &token).await
                };
                slint::invoke_from_event_loop(move || {
                    if let Some(ui) = uw.upgrade() {
                        ui.set_imagine_node_url(base.into());
                        apply_imagine_rows(&ui, &outcome);
                        // Backfill U3 posters for rows that don't have one yet.
                        imagine_thumb_backfill(&rt_h2, ui.as_weak());
                    }
                })
                .ok();
            });
        });
    }

    {
        let rt_h = rt.handle().clone();
        let uw = ui.as_weak();
        ui.on_imagine_generate(move |prompt, model, aspect, n| {
            // Armed edit sources reroute this submit to /v1/images/edits (A4).
            // Thread-local read happens HERE — never inside the tokio task.
            let edit_sources = EDIT_SOURCES.with(|s| s.borrow().clone());
            let editing = !edit_sources.is_empty();
            // Busy state lands immediately — this callback runs on the Slint thread.
            if let Some(ui) = uw.upgrade() {
                ui.set_imagine_busy(true);
                ui.set_imagine_note(if editing { "✎ editing…" } else { "generating…" }.into());
            }
            let (prompt, model, aspect) = (prompt.to_string(), model.to_string(), aspect.to_string());
            let n = n.max(1) as u32;
            let uw = uw.clone();
            rt_h.spawn(async move {
                let (base, token) = imagine_reach();
                let client = reqwest::Client::new();
                let result = if token.is_empty() {
                    Err("no token — see docs/imaginarium.md".to_string())
                } else if editing {
                    let body = imagine_edit_body(&prompt, n, &edit_sources);
                    imagine_post_job(&client, &base, &token, "/v1/images/edits", &body).await
                } else {
                    imagine_generate_call(&client, &base, &token, &prompt, &model, &aspect, n).await
                };
                let (note, preview, job_id, failed) = match &result {
                    Ok(job) => {
                        let status = job.get("status").and_then(Value::as_str).unwrap_or("");
                        let id = job.get("job_id").and_then(Value::as_str).unwrap_or("").to_string();
                        if status == "done" && !id.is_empty() {
                            match imagine_fetch_preview(&client, &base, &token, &id).await {
                                Ok(px) => (imagine_done_note(job, n), Some(px), id, false),
                                Err(e) => (format!("generated, preview failed: {e}"), None, id, false),
                            }
                        } else {
                            let msg = job
                                .get("error")
                                .and_then(Value::as_str)
                                .unwrap_or("job did not complete");
                            (format!("{status}: {msg}"), None, id, true)
                        }
                    }
                    Err(e) => (e.clone(), None, String::new(), true),
                };
                // The daemon's jobs list is the shared truth — refresh it either way.
                let rows = if token.is_empty() {
                    Err("unconfigured".to_string())
                } else {
                    imagine_fetch_jobs(&client, &base, &token).await
                };
                slint::invoke_from_event_loop(move || {
                    if let Some(ui) = uw.upgrade() {
                        ui.set_imagine_busy(false);
                        ui.set_imagine_note(note.into());
                        if let Some((w, h, rgba)) = preview {
                            // Fresh output owns the stage — stop any playback.
                            imagine_player_reset(&ui, "idle");
                            IMAGINE_CLIP.with(|c| c.borrow_mut().take());
                            let buf = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(
                                &rgba, w, h,
                            );
                            ui.set_imagine_preview(slint::Image::from_rgba8(buf));
                            ui.set_imagine_preview_kind("image".into());
                            ui.set_imagine_prompt_input("".into());
                            if editing {
                                // The edit consumed its sources — disarm.
                                EDIT_SOURCES.with(|s| s.borrow_mut().clear());
                                edit_sources_project(&ui);
                            }
                        } else if failed {
                            ui.set_imagine_preview_kind("error".into());
                        }
                        if !job_id.is_empty() {
                            ui.set_imagine_selected_job(job_id.into());
                        }
                        apply_imagine_rows(&ui, &rows);
                    }
                })
                .ok();
            });
        });
    }

    // Video submit (A2): fire-and-poll. The POST returns in seconds (`no_wait`),
    // the watcher keeps the rail honest while xAI renders, and the finished clip
    // auto-arms the A1 player — unless the user moved on (etiquette).
    {
        let rt_h = rt.handle().clone();
        let uw = ui.as_weak();
        ui.on_imagine_generate_video(move |prompt, model, duration, res, aspect, src_id, src_kind| {
            let cinematic = src_kind.as_str() == "cinematic";
            if let Some(ui) = uw.upgrade() {
                ui.set_imagine_busy(true);
                ui.set_imagine_note(
                    if cinematic { "✨ 1/2 — crafting the quality still…" } else { "submitting…" }
                        .into(),
                );
            }
            let (prompt, model, res, aspect) =
                (prompt.to_string(), model.to_string(), res.to_string(), aspect.to_string());
            let (src_id, src_kind) = (src_id.to_string(), src_kind.to_string());
            let rt_h2 = rt_h.clone();
            let uw = uw.clone();
            rt_h.spawn(async move {
                let (base, token) = imagine_reach();
                let client = reqwest::Client::new();
                let result = if token.is_empty() {
                    Err("no token — see docs/imaginarium.md".to_string())
                } else if src_kind == "cinematic" {
                    // T2I2V (André's pipeline, 2026-07-29): video-1.5 is
                    // I2V-only upstream — so craft a quality still from the
                    // prompt first (synchronous), then hand its library ref to
                    // v1.5 at 1080p. Two visible spends, one button.
                    let mut still =
                        serde_json::json!({ "prompt": prompt, "n": 1, "model": "quality" });
                    if !aspect.is_empty() && aspect != "default" {
                        still["aspect_ratio"] = Value::String(aspect.clone());
                    }
                    match imagine_post_job(&client, &base, &token, "/v1/images/generations", &still)
                        .await
                    {
                        Ok(img) => {
                            let img_id = img
                                .get("job_id")
                                .and_then(Value::as_str)
                                .unwrap_or("")
                                .to_string();
                            if img.get("status").and_then(Value::as_str) == Some("done")
                                && !img_id.is_empty()
                            {
                                let uw3 = uw.clone();
                                slint::invoke_from_event_loop(move || {
                                    if let Some(ui) = uw3.upgrade() {
                                        ui.set_imagine_note(
                                            "✨ 2/2 — still crafted, v1.5 animating at 1080p…"
                                                .into(),
                                        );
                                        ui.invoke_refresh_imagine();
                                    }
                                })
                                .ok();
                                let (path, body) = imagine_video_body(
                                    &prompt, "1.5", duration, "1080p", &aspect, &img_id, "image",
                                );
                                imagine_post_job(&client, &base, &token, path, &body).await
                            } else {
                                let msg = img
                                    .get("error")
                                    .and_then(Value::as_str)
                                    .unwrap_or("still generation did not complete");
                                Err(format!("cinematic step 1 failed: {msg}"))
                            }
                        }
                        Err(e) => Err(format!("cinematic step 1 failed: {e}")),
                    }
                } else {
                    let (path, body) =
                        imagine_video_body(&prompt, &model, duration, &res, &aspect, &src_id, &src_kind);
                    imagine_post_job(&client, &base, &token, path, &body).await
                };
                let rows = if token.is_empty() {
                    Err("unconfigured".to_string())
                } else {
                    imagine_fetch_jobs(&client, &base, &token).await
                };
                slint::invoke_from_event_loop(move || {
                    if let Some(ui) = uw.upgrade() {
                        ui.set_imagine_busy(false);
                        match &result {
                            Ok(job) => {
                                let id = job
                                    .get("job_id")
                                    .and_then(Value::as_str)
                                    .unwrap_or("")
                                    .to_string();
                                let status =
                                    job.get("status").and_then(Value::as_str).unwrap_or("");
                                if status == "failed" {
                                    let msg = job
                                        .get("error")
                                        .and_then(Value::as_str)
                                        .unwrap_or("submit failed");
                                    ui.set_imagine_note(format!("failed: {msg}").into());
                                } else {
                                    ui.set_imagine_note(
                                        "🎞 rendering upstream (~30–90s) — the rail tracks it"
                                            .into(),
                                    );
                                    // The chain is consumed by a successful hand-off.
                                    ui.set_imagine_chain_source("".into());
                                    ui.set_imagine_chain_kind("".into());
                                    ui.set_imagine_prompt_input("".into());
                                    if !id.is_empty() {
                                        ui.set_imagine_selected_job(id.clone().into());
                                        imagine_watch_job(&rt_h2, ui.as_weak(), id);
                                    }
                                }
                            }
                            Err(e) => ui.set_imagine_note(e.as_str().into()),
                        }
                        apply_imagine_rows(&ui, &rows);
                    }
                })
                .ok();
            });
        });
    }

    {
        let rt_h = rt.handle().clone();
        let uw = ui.as_weak();
        ui.on_imagine_pick_job(move |id| {
            let id = id.to_string();
            let uw = uw.clone();
            // A new selection owns the stage — stop any running playback now
            // (we're on the Slint thread) so its frames can't paint over us.
            if let Some(ui) = uw.upgrade() {
                imagine_player_reset(&ui, "idle");
                ui.set_imagine_video_progress(0.0);
                ui.set_imagine_video_time("".into());
                if !id.is_empty() {
                    ui.set_imagine_note("fetching…".into());
                }
            }
            IMAGINE_CLIP.with(|c| c.borrow_mut().take());
            rt_h.spawn(async move {
                let (base, token) = imagine_reach();
                let client = reqwest::Client::new();
                let job = imagine_fetch_job(&client, &base, &token, &id).await;
                let mode = job.get("mode").and_then(Value::as_str).unwrap_or("").to_string();
                let status = job.get("status").and_then(Value::as_str).unwrap_or("").to_string();
                let err = job.get("error").and_then(Value::as_str).unwrap_or("").to_string();
                let prompt = job.get("prompt").and_then(Value::as_str).unwrap_or("").to_string();
                let is_image = mode.starts_with("image");
                let preview = if is_image && status == "done" {
                    imagine_fetch_preview(&client, &base, &token, &id).await.ok()
                } else {
                    None
                };
                // Video-ish (video_* AND craft_export renders): fetch the clip
                // into the cache, probe it, decode a poster frame — playback is
                // then instant and offline. A probe/poster failure degrades to
                // the old "open the browser studio" note, honestly worded.
                // (path, duration, src_w, src_h, poster (w, h, rgba))
                type ClipPrep = (std::path::PathBuf, f32, u32, u32, (u32, u32, Vec<u8>));
                let mut clip: Option<ClipPrep> = None;
                let mut clip_err = String::new();
                if !is_image && !mode.is_empty() && status == "done" {
                    match imagine_download_clip(&client, &base, &token, &id).await {
                        Ok(path) => match imagine_probe_video(&path).await {
                            Some((sw, sh, dur)) if sw > 0 => {
                                let (pw, ph) = fit_dims(sw, sh, 960, 720);
                                match imagine_poster(&path, pw, ph).await {
                                    Some(poster) => clip = Some((path, dur, sw, sh, poster)),
                                    None => clip_err = "decode failed (ffmpeg?)".into(),
                                }
                            }
                            _ => clip_err = "probe failed (ffprobe missing?)".into(),
                        },
                        Err(e) => clip_err = e,
                    }
                }
                // A pending/running video job needs a driver: nothing on the
                // node advances a no_wait job by itself (GET /jobs/{id} is a DB
                // read) — so SELECTING a stuck job adopts it into the upstream
                // wait loop. Clicking a stale row heals it.
                let rendering = !is_image
                    && !mode.is_empty()
                    && matches!(status.as_str(), "pending" | "running");
                if rendering {
                    imagine_watch_job(&tokio::runtime::Handle::current(), uw.clone(), id.clone());
                }
                slint::invoke_from_event_loop(move || {
                    if let Some(ui) = uw.upgrade() {
                        ui.set_imagine_selected_job(id.into());
                        if let Some((w, h, rgba)) = preview {
                            let buf = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(
                                &rgba, w, h,
                            );
                            ui.set_imagine_preview(slint::Image::from_rgba8(buf));
                            ui.set_imagine_preview_kind("image".into());
                            let note = if prompt.is_empty() { status } else { format!("{status} · {prompt}") };
                            ui.set_imagine_note(note.into());
                        } else if let Some((path, dur, sw, sh, (pw, ph, px))) = clip {
                            // The stage becomes a player: poster up, ▶ armed.
                            IMAGINE_CLIP.with(|c| *c.borrow_mut() = Some((path, dur, sw, sh)));
                            let buf = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(
                                &px, pw, ph,
                            );
                            ui.set_imagine_preview(slint::Image::from_rgba8(buf));
                            ui.set_imagine_preview_kind("video".into());
                            ui.set_imagine_video_state("ready".into());
                            ui.set_imagine_video_time(
                                format!("0:00 / {}", imagine_clock(dur)).into(),
                            );
                            let note = if prompt.is_empty() {
                                format!("{mode} · {status}")
                            } else {
                                format!("{mode} · {prompt}")
                            };
                            ui.set_imagine_note(note.into());
                        } else if !is_image && !mode.is_empty() {
                            ui.set_imagine_preview_kind("video".into());
                            ui.set_imagine_video_state(
                                if rendering { "rendering" } else { "idle" }.into(),
                            );
                            let tail = if !clip_err.is_empty() {
                                format!(" · {clip_err}")
                            } else if !err.is_empty() {
                                format!(" · {err}")
                            } else {
                                String::new()
                            };
                            ui.set_imagine_note(format!("{mode} · {status}{tail}").into());
                        } else {
                            ui.set_imagine_preview_kind("error".into());
                            let note = if err.is_empty() {
                                format!("{status} — no preview available")
                            } else {
                                err
                            };
                            ui.set_imagine_note(note.into());
                        }
                    }
                })
                .ok();
            });
        });
    }

    // Player transport: ▶ decodes at the CURRENT stage size (the old-hardware
    // rule — never native res), ⏹/replay reuse the cached clip instantly.
    {
        let rt_h = rt.handle().clone();
        let uw = ui.as_weak();
        ui.on_imagine_video_play(move |max_w, max_h| {
            let Some(ui) = uw.upgrade() else { return };
            let Some((path, dur, sw, sh)) = IMAGINE_CLIP.with(|c| c.borrow().clone()) else {
                return;
            };
            imagine_player_reset(&ui, "idle");
            let r#gen = IMAGINE_PLAY_GEN.load(std::sync::atomic::Ordering::SeqCst);
            let (w, h) = fit_dims(sw, sh, max_w.max(64) as u32, max_h.max(64) as u32);
            let (tx, rx) = tokio::sync::mpsc::channel::<PlayerMsg>(8);
            imagine_spawn_pipelines(&rt_h, path, w, h, r#gen, tx);
            PLAYER_RX.with(|r| *r.borrow_mut() = Some(rx));
            PLAYER_CLOCK.with(|c| {
                *c.borrow_mut() = Some((std::time::Instant::now(), IMAGINE_PLAY_FPS, dur, 0))
            });
            ui.set_imagine_video_state("playing".into());
            let uw_tick = ui.as_weak();
            let timer = slint::Timer::default();
            timer.start(
                slint::TimerMode::Repeated,
                std::time::Duration::from_millis(40),
                move || {
                    if let Some(ui) = uw_tick.upgrade() {
                        imagine_player_tick(&ui);
                    }
                },
            );
            PLAYER_TIMER.with(|t| *t.borrow_mut() = Some(timer));
        });
    }
    {
        let uw = ui.as_weak();
        ui.on_imagine_video_stop(move || {
            if let Some(ui) = uw.upgrade() {
                imagine_player_reset(&ui, "ended");
            }
        });
    }

    // Prompt from file: list the workspace's text files via agentd (the twin of
    // the image-attach picker), and load a picked one into the prompt box — so
    // anything written INTO the system (agent notes, USB imports, uploads)
    // becomes generation fuel without retyping.
    {
        let rt_h = rt.handle().clone();
        let client = Arc::clone(&http_client);
        let base = http_base.clone();
        let uw = ui.as_weak();
        ui.on_imagine_prompt_files_refresh(move || {
            let client = Arc::clone(&client);
            let base = base.clone();
            let uw = uw.clone();
            rt_h.spawn(async move {
                let body = json_get(&client, format!("{base}/api/workspace/texts")).await;
                let rows: Vec<(String, String)> = body
                    .get("texts")
                    .and_then(Value::as_array)
                    .map(|a| {
                        a.iter()
                            .filter_map(|t| {
                                Some((
                                    t.get("path")?.as_str()?.to_string(),
                                    t.get("name")?.as_str()?.to_string(),
                                ))
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                slint::invoke_from_event_loop(move || {
                    if uw.upgrade().is_some() {
                        IMAGINE_PROMPT_FILES.with(|m| {
                            if let Some(model) = m.borrow().as_ref() {
                                model.set_vec(
                                    rows.into_iter()
                                        .map(|(path, name)| ImageItem {
                                            path: path.into(),
                                            name: name.into(),
                                        })
                                        .collect::<Vec<_>>(),
                                );
                            }
                        });
                    }
                })
                .ok();
            });
        });
    }
    {
        let rt_h = rt.handle().clone();
        let client = Arc::clone(&http_client);
        let base = http_base.clone();
        let uw = ui.as_weak();
        ui.on_imagine_prompt_load(move |path| {
            let path = path.to_string();
            let client = Arc::clone(&client);
            let base = base.clone();
            let uw = uw.clone();
            rt_h.spawn(async move {
                // The confined workspace download route; reqwest encodes the
                // query. Prompts are text — cap at 4 KB so a stray novel can't
                // flood the box (xAI prompts top out far below that).
                let text = match client
                    .get(format!("{base}/api/workspace/download"))
                    .query(&[("path", path.as_str())])
                    .timeout(std::time::Duration::from_secs(8))
                    .send()
                    .await
                {
                    Ok(resp) if resp.status().is_success() => {
                        resp.text().await.unwrap_or_default()
                    }
                    _ => String::new(),
                };
                let name = path.rsplit('/').next().unwrap_or(&path).to_string();
                slint::invoke_from_event_loop(move || {
                    if let Some(ui) = uw.upgrade() {
                        if text.trim().is_empty() {
                            ui.set_imagine_note(format!("couldn't read {name}").into());
                        } else {
                            let mut t = text.trim().to_string();
                            let truncated = t.chars().count() > 4000;
                            if truncated {
                                t = t.chars().take(4000).collect();
                            }
                            ui.set_imagine_prompt_input(t.into());
                            ui.set_imagine_note(
                                format!(
                                    "📄 prompt loaded from {name}{}",
                                    if truncated { " (truncated to 4k chars)" } else { "" }
                                )
                                .into(),
                            );
                        }
                    }
                })
                .ok();
            });
        });
    }

    // ── Image-edit sources (A4) ──────────────────────────────────────────────
    // ✎ EDIT on a previewed image arms it as a source (≤3, deduped); the next
    // image-mode GENERATE routes to /v1/images/edits with library: refs.
    {
        let uw = ui.as_weak();
        ui.on_imagine_edit_add(move |id| {
            let Some(ui) = uw.upgrade() else { return };
            let id = id.to_string();
            // Label from the rail row (prompt stem, else model) — no refetch.
            let stem: String = IMAGINE_JOBS.with(|m| {
                use slint::Model as _;
                m.borrow().as_ref().and_then(|model| {
                    model
                        .iter()
                        .find(|r| r.id.as_str() == id)
                        .map(|r| if r.prompt.is_empty() { r.model } else { r.prompt })
                })
            })
            .map(|s| s.to_string().chars().take(18).collect())
            .unwrap_or_else(|| id.chars().take(8).collect());
            let outcome = EDIT_SOURCES.with(|s| {
                let mut list = s.borrow_mut();
                if list.iter().any(|(i, _)| *i == id) {
                    "already a source"
                } else if list.len() >= 3 {
                    "edit takes at most 3 sources"
                } else {
                    list.push((id.clone(), stem.clone()));
                    "armed"
                }
            });
            if outcome == "armed" {
                ui.set_imagine_gen_mode("image".into());
                ui.set_imagine_note(format!("✎ source armed: {stem}").into());
            } else {
                ui.set_imagine_note(outcome.into());
            }
            edit_sources_project(&ui);
        });
    }
    {
        let uw = ui.as_weak();
        ui.on_imagine_edit_remove(move |id| {
            if let Some(ui) = uw.upgrade() {
                EDIT_SOURCES.with(|s| s.borrow_mut().retain(|(i, _)| i.as_str() != id.as_str()));
                edit_sources_project(&ui);
            }
        });
    }

    // ── The Cutting Room callbacks (A5) ──────────────────────────────────────
    // Add: the rail row's mode can't tell an audio import from a video craft
    // (both craft_export) — fetch the job once and route by the REAL asset
    // kind: video→clip, image→still, audio→music bed.
    {
        let rt_h = rt.handle().clone();
        let uw = ui.as_weak();
        ui.on_imagine_cut_add(move |id| {
            let id = id.to_string();
            let uw = uw.clone();
            rt_h.spawn(async move {
                let (base, token) = imagine_reach();
                let client = reqwest::Client::new();
                let job = imagine_fetch_job(&client, &base, &token, &id).await;
                slint::invoke_from_event_loop(move || {
                    let Some(ui) = uw.upgrade() else { return };
                    let status = job.get("status").and_then(Value::as_str).unwrap_or("");
                    if status != "done" {
                        ui.set_imagine_note("only finished jobs can join the cut".into());
                        return;
                    }
                    let kind = job
                        .get("assets")
                        .and_then(|a| a.get(0))
                        .and_then(|a| a.get("kind"))
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    let label: String = job
                        .get("prompt")
                        .and_then(Value::as_str)
                        .filter(|p| !p.is_empty())
                        .unwrap_or("untitled")
                        .chars()
                        .take(26)
                        .collect();
                    match kind {
                        "video" => {
                            CUT_SEGS.with(|s| s.borrow_mut().push(CutSeg::clip(&id, &label)));
                            ui.set_imagine_cut_selected(
                                CUT_SEGS.with(|s| s.borrow().len() as i32 - 1),
                            );
                            ui.set_imagine_note(format!("🎞 added: {label}").into());
                        }
                        "image" => {
                            CUT_SEGS.with(|s| s.borrow_mut().push(CutSeg::still(&id, &label)));
                            ui.set_imagine_cut_selected(
                                CUT_SEGS.with(|s| s.borrow().len() as i32 - 1),
                            );
                            ui.set_imagine_note(format!("🖼 added as still: {label}").into());
                        }
                        "audio" => {
                            CUT_MUSIC.with(|m| *m.borrow_mut() = Some((id.clone(), label.clone())));
                            ui.set_imagine_note(format!("🎵 music bed set: {label}").into());
                        }
                        other => {
                            ui.set_imagine_note(
                                format!("can't cut this job (asset kind {other:?})").into(),
                            );
                            return;
                        }
                    }
                    cut_project(&ui);
                })
                .ok();
            });
        });
    }
    {
        let uw = ui.as_weak();
        ui.on_imagine_cut_remove(move |ix| {
            if let Some(ui) = uw.upgrade() {
                CUT_SEGS.with(|s| {
                    let mut segs = s.borrow_mut();
                    let ix = ix as usize;
                    if ix < segs.len() {
                        segs.remove(ix);
                    }
                });
                let len = CUT_SEGS.with(|s| s.borrow().len() as i32);
                if ui.get_imagine_cut_selected() >= len {
                    ui.set_imagine_cut_selected(len - 1);
                }
                cut_project(&ui);
            }
        });
    }
    {
        let uw = ui.as_weak();
        ui.on_imagine_cut_move(move |ix, delta| {
            if let Some(ui) = uw.upgrade() {
                let moved = CUT_SEGS.with(|s| {
                    let mut segs = s.borrow_mut();
                    let from = ix as usize;
                    let to = ix + delta;
                    if from < segs.len() && to >= 0 && (to as usize) < segs.len() {
                        segs.swap(from, to as usize);
                        true
                    } else {
                        false
                    }
                });
                if moved {
                    if ui.get_imagine_cut_selected() == ix {
                        ui.set_imagine_cut_selected(ix + delta);
                    }
                    cut_project(&ui);
                }
            }
        });
    }
    {
        let uw = ui.as_weak();
        ui.on_imagine_cut_add_card(move || {
            if let Some(ui) = uw.upgrade() {
                CUT_SEGS.with(|s| s.borrow_mut().push(CutSeg::card()));
                ui.set_imagine_cut_selected(CUT_SEGS.with(|s| s.borrow().len() as i32 - 1));
                cut_project(&ui);
            }
        });
    }
    {
        let uw = ui.as_weak();
        ui.on_imagine_cut_set(move |field, value| {
            if let Some(ui) = uw.upgrade() {
                let sel = ui.get_imagine_cut_selected();
                CUT_SEGS.with(|s| {
                    let mut segs = s.borrow_mut();
                    if sel >= 0 {
                        if let Some(seg) = segs.get_mut(sel as usize) {
                            cut_apply(seg, field.as_str(), value.as_str());
                        }
                    }
                });
                cut_project(&ui);
            }
        });
    }
    {
        let uw = ui.as_weak();
        ui.on_imagine_cut_music_clear(move || {
            if let Some(ui) = uw.upgrade() {
                CUT_MUSIC.with(|m| m.borrow_mut().take());
                cut_project(&ui);
            }
        });
    }
    // Score with Sonus (A6): stream the track from agentd, import it into the
    // imaginarium library as audio (the U2a coverage), and set it as the bed.
    // A session cache keeps re-scoring from duplicating imports.
    {
        let rt_h = rt.handle().clone();
        let agentd_client = Arc::clone(&http_client);
        let agentd_base = http_base.clone();
        let uw = ui.as_weak();
        ui.on_imagine_cut_score(move |name| {
            let name = name.to_string();
            let stem: String = name
                .rsplit('/')
                .next()
                .unwrap_or(&name)
                .trim_end_matches(".wav")
                .trim_end_matches(".mp3")
                .chars()
                .take(26)
                .collect();
            // Already imported this session → just point the bed at it
            // (thread-locals are main-thread; the check happens HERE, not in
            // the task).
            let cached = CUT_SONUS_IMPORTED.with(|c| c.borrow().get(&name).cloned());
            if let Some(id) = cached {
                if let Some(ui) = uw.upgrade() {
                    CUT_MUSIC.with(|m| *m.borrow_mut() = Some((id, stem.clone())));
                    ui.set_imagine_note(format!("🎵 scored with {stem}").into());
                    cut_project(&ui);
                }
                return;
            }
            if let Some(ui) = uw.upgrade() {
                ui.set_imagine_note(format!("🎵 bringing in {name}…").into());
            }
            let agentd_client = Arc::clone(&agentd_client);
            let agentd_base = agentd_base.clone();
            let uw = uw.clone();
            rt_h.spawn(async move {
                let outcome: Result<(String, String), String> = async {
                    let stem = stem.clone();
                    // Stream the bytes off agentd (traversal-guarded route).
                    let resp = agentd_client
                        .get(format!("{agentd_base}/api/sonus/stream"))
                        .query(&[("name", name.as_str())])
                        .timeout(std::time::Duration::from_secs(60))
                        .send()
                        .await
                        .map_err(|e| format!("sonus stream failed: {e}"))?;
                    if !resp.status().is_success() {
                        return Err(format!("sonus stream HTTP {}", resp.status().as_u16()));
                    }
                    let bytes = resp
                        .bytes()
                        .await
                        .map_err(|e| format!("sonus read failed: {e}"))?;
                    if bytes.len() > 38 * 1024 * 1024 {
                        return Err("track is over the 38 MB import ceiling — trim it in Audio first".into());
                    }
                    use base64::Engine as _;
                    let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
                    let body = serde_json::json!({
                        "data": format!("data:{};base64,{b64}", sonus_mime_for_name(&name)),
                        "filename": name,
                        "note": format!("sonus · {name}"),
                    });
                    let (base, token) = imagine_reach();
                    if token.is_empty() {
                        return Err("no imaginarium token — see docs/imaginarium.md".into());
                    }
                    let job = imagine_post_job(
                        &reqwest::Client::new(),
                        &base,
                        &token,
                        "/v1/library/import",
                        &body,
                    )
                    .await?;
                    let id = job
                        .get("job_id")
                        .and_then(Value::as_str)
                        .ok_or("import gave no job id")?
                        .to_string();
                    Ok((id, stem))
                }
                .await;
                let name2 = name.clone();
                slint::invoke_from_event_loop(move || {
                    let Some(ui) = uw.upgrade() else { return };
                    match outcome {
                        Ok((id, stem)) => {
                            CUT_SONUS_IMPORTED.with(|c| {
                                c.borrow_mut().insert(name2.clone(), id.clone());
                            });
                            CUT_MUSIC.with(|m| *m.borrow_mut() = Some((id, stem.clone())));
                            ui.set_imagine_note(format!("🎵 scored with {stem}").into());
                            cut_project(&ui);
                            ui.invoke_refresh_imagine();
                        }
                        Err(e) => ui.set_imagine_note(format!("score failed: {e}").into()),
                    }
                })
                .ok();
            });
        });
    }
    // "🎵 ask APEX to compose" — the steer idiom: a queued user_prompt through
    // the WS TurnGate. The brief lands in chat; new tracks appear in the picker.
    {
        let tx_score = tx.clone();
        let uw = ui.as_weak();
        ui.on_imagine_cut_score_compose(move || {
            let Some(ui) = uw.upgrade() else { return };
            let total = ui.get_imagine_cut_total().to_string();
            clear_face_hold();
            maybe_push_time_divider();
            push_message(MessageItem {
                role: "user".into(),
                text: "🎵 compose a bed for my cut…".into(),
                streaming: false,
                call_id: "".into(),
                tool_name: "".into(),
                tool_args: "".into(),
                tool_output: "".into(),
                tool_status: "".into(),
                awaiting_approval: false,
            });
            let frame = serde_json::json!({
                "type": "user_prompt",
                "text": cut_compose_prompt(&total),
            })
            .to_string();
            tx_score.send(frame).ok();
            ui.set_imagine_note(
                "🎵 asked APEX — watch the chat; the track appears in SCORE when it lands".into(),
            );
            bump_scroll(&ui);
        });
    }

    // Render: POST the v1 timeline with ?no_wait=true (U3 — the node renders in
    // the background), select the pending craft job, and let the standard
    // watcher drive it home (for craft jobs /wait returns the DB row each pass).
    {
        let rt_h = rt.handle().clone();
        let uw = ui.as_weak();
        ui.on_imagine_cut_render(move || {
            let Some(ui) = uw.upgrade() else { return };
            if CUT_SEGS.with(|s| s.borrow().is_empty()) {
                ui.set_imagine_note("the timeline is empty — add segments first".into());
                return;
            }
            let body = cut_build_timeline(
                &CUT_SEGS.with(|s| s.borrow().clone()),
                CUT_MUSIC
                    .with(|m| m.borrow().as_ref().map(|(id, _)| id.clone()))
                    .as_deref(),
                ui.get_imagine_cut_letterbox_ix(),
                ui.get_imagine_cut_loudnorm(),
                ui.get_imagine_cut_fades_ix(),
            );
            ui.set_imagine_busy(true);
            ui.set_imagine_note("✂ submitting the cut…".into());
            let rt_h2 = rt_h.clone();
            let uw = uw.clone();
            rt_h.spawn(async move {
                let (base, token) = imagine_reach();
                let client = reqwest::Client::new();
                let result = if token.is_empty() {
                    Err("no token — see docs/imaginarium.md".to_string())
                } else {
                    imagine_post_job(
                        &client,
                        &base,
                        &token,
                        "/v1/craft/video/render?no_wait=true",
                        &body,
                    )
                    .await
                };
                slint::invoke_from_event_loop(move || {
                    let Some(ui) = uw.upgrade() else { return };
                    ui.set_imagine_busy(false);
                    match result {
                        Ok(job) => {
                            let id = job
                                .get("job_id")
                                .and_then(Value::as_str)
                                .unwrap_or("")
                                .to_string();
                            if id.is_empty() {
                                ui.set_imagine_note("render submit gave no job id".into());
                                return;
                            }
                            ui.set_imagine_note(
                                "✂ rendering on the node — the rail tracks it".into(),
                            );
                            ui.set_imagine_selected_job(id.clone().into());
                            ui.set_imagine_preview_kind("video".into());
                            ui.set_imagine_video_state("rendering".into());
                            ui.invoke_refresh_imagine();
                            imagine_watch_job(&rt_h2, ui.as_weak(), id);
                        }
                        Err(e) => ui.set_imagine_note(format!("render failed: {e}").into()),
                    }
                })
                .ok();
            });
        });
    }

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
                    ui.set_settings_history_budget(data.history_budget_label.into());
                    ui.set_settings_history_usage(data.history_usage.into());
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

    // ── set-history-budget callback ───────────────────────────────────────────
    // Settings label → POST /api/history {budget}. Effective on the next turn
    // (the router reads the atomic per prompt); persisted to history_config.json.
    let rt_h_hist    = rt.handle().clone();
    let client_hist  = Arc::clone(&http_client);
    let base_hist    = http_base.clone();
    let ui_weak_hist = ui.as_weak();
    ui.on_set_history_budget(move |label| {
        let Some(budget) = history_label_tokens(label.as_str()) else { return };
        // Optimistic: reflect the new choice immediately.
        if let Some(ui) = ui_weak_hist.upgrade() {
            ui.set_settings_history_budget(label.clone());
        }
        let client = Arc::clone(&client_hist);
        let base   = base_hist.clone();
        rt_h_hist.spawn(async move {
            let ok = client.post(format!("{base}/api/history"))
                .json(&serde_json::json!({ "budget": budget }))
                .timeout(std::time::Duration::from_secs(8))
                .send().await
                .map(|r| r.status().is_success())
                .unwrap_or(false);
            if ok { notify(ToastKind::Info, "History window budget updated"); }
            else  { notify(ToastKind::Error, "Failed to update history budget"); }
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
            // Mandala tree window: a successful mandala_status mirrors into it
            // (same shape-sniffed idiom — a `mandalas` array is the signature).
            let mnd = if ok {
                mandala_payload(&out.content).map(|p| build_mandala_rows(&p))
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
                    if let Some((meta, rows)) = mnd {
                        apply_mandala_render(&ui, meta, rows);
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
        Event::GoalStateChanged { goal, objective, state, step, max_steps, detail, yolo, .. } => {
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

        // Work Board: a fanned-out worker changed state → upsert its card in the
        // WORKERS lane (Fabrica W1a). Typed arm ONLY — never add a string-keyed
        // "worker_state_changed" shortcut in the early dispatch, or whichever arm
        // lands second goes silently dead (the mesh_message lesson).
        Event::WorkerStateChanged { worker, batch, state, task, detail, yolo, node, .. } => {
            let (badge, c) = match state {
                WorkerState::Queued    => ("QUEUE", board_color(148, 163, 184)),
                WorkerState::Running   => ("RUN",   board_color(34, 211, 238)),
                WorkerState::Blocked   => ("BLOCK", board_color(251, 191, 36)),
                WorkerState::Parked    => ("PARK",  board_color(125, 145, 175)),
                WorkerState::Idle      => ("IDLE",  board_color(148, 163, 184)),
                WorkerState::Done      => ("DONE",  board_color(52, 211, 153)),
                WorkerState::Failed    => ("FAIL",  board_color(239, 68, 68)),
                WorkerState::Cancelled => ("STOP",  board_color(148, 163, 184)),
            };
            let wid = worker.0;
            let title: String = task.chars().take(60).collect();
            let mut base = if detail.is_empty() {
                format!("batch {batch}")
            } else {
                format!("batch {batch} · {detail}")
            };
            // W2: a remote row names its hosting node on the card.
            if let Some(n) = node {
                base = format!("@{n} · {base}");
            }
            // Batch-inherited yolo renders AUTO like goals (word carries if ⚡ tofus).
            let subtitle = if yolo { format!("⚡ AUTO · {base}") } else { base };
            // Approval digest bookkeeping: one card per batch with a count.
            let awaiting = state == WorkerState::Blocked && detail.starts_with("awaiting approval");
            slint::invoke_from_event_loop(move || {
                board_worker(wid, title, subtitle, badge, c);
                board_worker_approval(batch, wid, awaiting);
            }).ok();
        }

        // Work Board: a batch reported (all-terminal or deadline) → drop a card
        // into RECENT with the outcome mix. Typed arm only (the mesh_message
        // dead-handler lesson).
        Event::TaskBatchDone { batch, rows, .. } => {
            let (done, failed, timed_out) = rows.iter().fold((0, 0, 0), |(d, f, t), r| {
                if r.timed_out { (d, f, t + 1) }
                else if r.state == WorkerState::Done { (d + 1, f, t) }
                else { (d, f + 1, t) }
            });
            let title = format!("Batch {batch} reported");
            let subtitle = format!("{done} done · {failed} failed · {timed_out} timed out");
            slint::invoke_from_event_loop(move || {
                board_push_recent(title, subtitle, "BATCH", board_color(34, 211, 238));
            }).ok();
        }

        _ => {}
    }
}
