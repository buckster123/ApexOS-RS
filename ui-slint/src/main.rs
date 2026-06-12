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

use slint::Model; // row_count / row_data / set_row_data on VecModel
use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
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
}

// ── Feedback subsystem (toasts) ───────────────────────────────────────────────
static TOAST_SEQ: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(1);

/// Push a toast. Must run on the Slint thread (touches the TOASTS thread-local).
fn toast(kind: ToastKind, text: &str) {
    let timeout_ms = match kind {
        ToastKind::Error => 7000,
        ToastKind::Warn  => 6000,
        _                => 4000,
    };
    let id = TOAST_SEQ.fetch_add(1, Ordering::SeqCst);
    let item = ToastItem { id, kind, text: text.into(), timeout_ms };
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

// ── Window manager (G2) ───────────────────────────────────────────────────────
// All helpers run on the Slint thread (called from UI callbacks). The WINDOWS
// VecModel's order IS the z-order: the last row paints on top.

fn kind_ordinal(k: AppKind) -> i32 {
    match k {
        AppKind::Chat => 0,
        AppKind::System => 1,
        AppKind::Sensor => 2,
        AppKind::Sessions => 3,
        AppKind::Settings => 4,
        AppKind::Terminal => 5,
        AppKind::Council => 6,
    }
}

fn kind_from_ordinal(o: i32) -> AppKind {
    match o {
        1 => AppKind::System,
        2 => AppKind::Sensor,
        3 => AppKind::Sessions,
        4 => AppKind::Settings,
        5 => AppKind::Terminal,
        6 => AppKind::Council,
        _ => AppKind::Chat,
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
fn apply_persona(ui: &AppWindow, p: Persona, persist: bool) {
    ui.global::<Personas>().set_current(p);
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
    };
    let step = (n % 6) as f32 * 30.0;
    (72.0 + step, 32.0 + step, w, h)
}

fn wm_index_by_id(model: &Rc<slint::VecModel<WindowDesc>>, id: i32) -> Option<usize> {
    (0..model.row_count()).find(|&i| model.row_data(i).map(|d| d.id) == Some(id))
}

fn wm_index_by_kind(model: &Rc<slint::VecModel<WindowDesc>>, kind: AppKind) -> Option<usize> {
    (0..model.row_count()).find(|&i| model.row_data(i).map(|d| d.kind) == Some(kind))
}

/// Move a window to the top of the z-order (end of the model) and mark it
/// focused. Returns the focused window's kind ordinal (or -1 if not found).
fn wm_focus(ui: &AppWindow, model: &Rc<slint::VecModel<WindowDesc>>, id: i32) {
    if let Some(i) = wm_index_by_id(model, id) {
        let d = model.remove(i);
        let kind = d.kind;
        model.push(d);
        ui.set_focused_id(id);
        ui.set_focused_kind(kind_ordinal(kind));
    }
}

/// Recompute focus to the top-most non-minimised window (after a close/minimise).
fn wm_refocus_top(ui: &AppWindow, model: &Rc<slint::VecModel<WindowDesc>>) {
    for i in (0..model.row_count()).rev() {
        if let Some(d) = model.row_data(i) {
            if !d.minimized {
                ui.set_focused_id(d.id);
                ui.set_focused_kind(kind_ordinal(d.kind));
                return;
            }
        }
    }
    ui.set_focused_id(0);
    ui.set_focused_kind(-1);
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
    let (x, y, w, h) = default_geom(kind, model.row_count() as i32);
    model.push(WindowDesc {
        id,
        kind,
        title: kind_title(kind).into(),
        x,
        y,
        w,
        h,
        minimized: false,
        maximized: false,
    });
    wm_focus(ui, model, id);
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
        eprintln!("[ui-slint] terminal connecting to {url}");
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
                        write.send(Message::Binary(l.into_bytes().into())).await.ok();
                    }
                }
            }
        }
    }
}

/// Spawn the terminal WS task on first Terminal-window launch (once).
fn start_terminal(rt: &tokio::runtime::Handle, url: &str, ui_weak: slint::Weak<AppWindow>) {
    if TERM_STARTED.with(|c| { let v = c.get(); c.set(true); v }) {
        return;
    }
    if let Some(rx) = TERM_RX.with(|r| r.borrow_mut().take()) {
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
                    last.streaming = false;
                    model.set_row_data(len - 1, last);
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
    static LAST_STAMP: std::cell::Cell<i64> = std::cell::Cell::new(0);
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

fn find_tool_row(call_id: &str) -> Option<usize> {
    MESSAGES.with(|m| {
        if let Some(model) = m.borrow().as_ref() {
            for i in 0..model.row_count() {
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

#[cfg(test)]
mod tests {
    use super::ws_to_http;

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
    }).collect()
}

// POST /api/record/stop → run whisper → return transcribed text (or empty on error).
async fn stop_and_transcribe(client: &reqwest::Client, base_url: &str) -> String {
    match client
        .post(format!("{base_url}/api/record/stop"))
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
}

// Fetch /api/status, /api/soul, and /api/models in parallel.
async fn fetch_settings(client: &reqwest::Client, base_url: &str) -> SettingsData {
    let (status, soul, models_resp) = tokio::join!(
        json_get(client, format!("{base_url}/api/status")),
        json_get(client, format!("{base_url}/api/soul")),
        json_get(client, format!("{base_url}/api/models")),
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

// ── App state ─────────────────────────────────────────────────────────────────
#[derive(Default)]
struct AppState {
    session_id: Option<u64>,
    // Child sessions spawned via agent.spawn and not yet turn-complete; drives
    // the taskbar "N sub-agents running" badge.
    subagents: std::collections::HashSet<u64>,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
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

    // Initial sys stats (all zeros, offline)
    ui.set_sys_stats(empty_sys_stats());

    // ── Window manager (G2): model + seed the Chat window ─────────────────────
    let windows: Rc<slint::VecModel<WindowDesc>> = Rc::new(slint::VecModel::default());
    ui.set_windows(slint::ModelRc::from(windows.clone()));
    WINDOWS.with(|w| *w.borrow_mut() = Some(windows.clone()));
    wm_launch(&ui, &windows, AppKind::Chat);

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
                wm_launch(&ui, &w, kind);
                // Fire the per-app refresh the legacy tab strip used to trigger on
                // open-view — without it Settings/Sessions windows launch empty.
                match kind {
                    AppKind::Settings => ui.invoke_refresh_settings(),
                    AppKind::Sessions => ui.invoke_refresh_sessions(),
                    AppKind::Terminal => start_terminal(&rt_h_term, &term_url, ui.as_weak()),
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
                if let Some(i) = wm_index_by_id(&w, id) { w.remove(i); }
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
        });
    }
    {
        let w = windows.clone();
        ui.on_resize_window(move |id, ww, hh| {
            wm_update_row(&w, id, |d| { d.w = ww; d.h = hh; });
        });
    }

    let state = Arc::new(Mutex::new(AppState::default()));

    // Voice state
    let tts_enabled = Arc::new(AtomicBool::new(false));

    // Outbound WS channel
    let (tx, mut rx) = mpsc::unbounded_channel::<String>();

    let ws_url = {
        let base = std::env::var("AGENTD_WS")
            .unwrap_or_else(|_| "ws://localhost:8787/ws".to_string());
        match std::env::var("AGENTD_TOKEN") {
            Ok(t) if !t.is_empty() => format!("{base}?token={t}"),
            _ => base,
        }
    };
    let http_base = ws_to_http(&ws_url);

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
            eprintln!("[ui-slint] connecting to {ws_url}");

            let (ws, _) = match connect_async(&ws_url).await {
                Ok(pair) => pair,
                Err(e) => {
                    eprintln!("[ui-slint] WS connect failed: {e}");
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
            write.send(Message::Text(init.to_string().into())).await.ok();

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
                            write.send(Message::Text(text.into())).await.ok();
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

    // ── send-message callback ─────────────────────────────────────────────────
    let tx_send = tx.clone();
    let messages_send = messages.clone();
    ui.on_send_message(move |text| {
        if text.is_empty() {
            return;
        }
        maybe_push_time_divider();
        messages_send.push(MessageItem {
            role: "user".into(),
            text: text.clone(),
            streaming: false,
            call_id: "".into(),
            tool_name: "".into(),
            tool_args: "".into(),
            tool_output: "".into(),
            tool_status: "".into(),
            awaiting_approval: false,
        });
        let payload = serde_json::json!({"type": "user_prompt", "text": text.as_str()}).to_string();
        tx_send.send(payload).ok();
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
        if let Some(ui) = stop_weak.upgrade() {
            ui.set_agent_busy(false);
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
        }
        // Ask agentd to replay the session (Rust agentd: hello + resume_session field)
        let payload = serde_json::json!({
            "type": "hello",
            "resume_session": session_id as u64
        })
        .to_string();
        tx_restore.send(payload).ok();
    });

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
                let ok = client
                    .post(format!("{base}/api/record/start"))
                    .timeout(std::time::Duration::from_secs(8))
                    .send().await
                    .map(|r| r.status().is_success())
                    .unwrap_or(false);
                slint::invoke_from_event_loop(move || {
                    if let Some(ui) = ui_w.upgrade() {
                        if ok { ui.set_recording(true); }
                        else  { toast(ToastKind::Error, "Microphone unavailable"); }
                    }
                }).ok();
            });
        } else {
            rt_h.spawn(async move {
                let text = stop_and_transcribe(&client, &base).await;
                slint::invoke_from_event_loop(move || {
                    if let Some(ui) = ui_w.upgrade() {
                        ui.set_recording(false);
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
            slint::invoke_from_event_loop(move || {
                if let Some(ui) = ui_w.upgrade() {
                    ui.set_soul_text(data.soul_text.into());
                    ui.set_settings_policy(data.policy_mode.into());
                    ui.set_settings_model(data.current_model.into());
                    ui.set_settings_api_key_set(data.api_key_set);
                    replace_models(data.models);
                }
            }).ok();
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
                }
            },
        );
    }

    ui.run()?;
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
        }

        "turn_started" => {
            let w = ui_weak.clone();
            slint::invoke_from_event_loop(move || {
                if let Some(ui) = w.upgrade() {
                    ui.set_agent_busy(true);
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
        }

        "agent_text" => {
            let delta = ev["delta"].as_str().unwrap_or("").to_string();
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
                    }
                    update_last_agent_message(&delta);
                    bump_scroll(&ui);
                }
            })
            .ok();
        }

        "turn_complete" => {
            let tts    = ctx.tts_enabled.load(Ordering::SeqCst);
            let rt_h   = ctx.rt_handle.clone();
            let client = Arc::clone(&ctx.http_client);
            let base   = ctx.http_base.clone();
            let sess   = ev["session"].as_u64();
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
                    }
                    finish_last_agent_message();
                    ui.set_agent_busy(false);
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
                                client.post(format!("{base}/api/speak"))
                                    .json(&serde_json::json!({"text": text}))
                                    .timeout(std::time::Duration::from_secs(5))
                                    .send().await.ok();
                            });
                        }
                    }
                }
            })
            .ok();
        }

        "wake_triggered" => {
            // Wake word detected — switch to chat and auto-start recording
            let rt_h   = ctx.rt_handle.clone();
            let client = Arc::clone(&ctx.http_client);
            let base   = ctx.http_base.clone();
            let ui_w1  = ui_weak.clone();
            let ui_w2  = ui_weak.clone();
            slint::invoke_from_event_loop(move || {
                if let Some(ui) = ui_w1.upgrade() {
                    if !ui.get_recording() {
                        ui.set_current_view(0);
                        rt_h.spawn(async move {
                            let ok = client
                                .post(format!("{base}/api/record/start"))
                                .timeout(std::time::Duration::from_secs(8))
                                .send().await
                                .map(|r| r.status().is_success())
                                .unwrap_or(false);
                            slint::invoke_from_event_loop(move || {
                                if let Some(ui) = ui_w2.upgrade() {
                                    if ok { ui.set_recording(true); }
                                }
                            }).ok();
                        });
                    }
                }
            }).ok();
        }

        "tool_requested" => {
            // Event::ToolRequested { session, call: ToolCall } — fields nest under `call`.
            // ToolCall.id is ActionId(u64) → a bare number; stringify for the row key.
            let call      = &ev["call"];
            let call_id   = call["id"].as_u64().map(|n| n.to_string()).unwrap_or_default();
            let tool_name = call["tool"].as_str().unwrap_or("").to_string();
            let tool_args = if call["args"].is_null() {
                String::new()
            } else {
                serde_json::to_string_pretty(&call["args"]).unwrap_or_default()
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
                    bump_scroll(&ui);
                }
            })
            .ok();
        }

        "tool_result" => {
            // Event::ToolResult { session, call: ActionId, output: ToolOutput }.
            // `call` is the bare action-id number; output nests { ok, content }.
            let call_id = ev["call"].as_u64().map(|n| n.to_string()).unwrap_or_default();
            let out     = &ev["output"];
            let ok      = out["ok"].as_bool().unwrap_or(true);
            let output  = match &out["content"] {
                serde_json::Value::String(s) => s.clone(),
                serde_json::Value::Null      => String::new(),
                other => serde_json::to_string_pretty(other).unwrap_or_default(),
            };
            let status = if ok { "done" } else { "error" };
            slint::invoke_from_event_loop(move || {
                if let Some(row) = find_tool_row(&call_id) {
                    update_tool_row(row, |item| {
                        item.tool_output = output.into();
                        item.tool_status = status.into();
                        item.awaiting_approval = false;
                    });
                }
            })
            .ok();
        }

        "approval_pending" => {
            // Event::ApprovalPending { session, call: ToolCall } — same nesting as tool_requested.
            // Normally a tool_requested arrives first (card exists); the else-branch is a fallback.
            let call      = &ev["call"];
            let call_id   = call["id"].as_u64().map(|n| n.to_string()).unwrap_or_default();
            let tool_name = call["tool"].as_str().unwrap_or("").to_string();
            let tool_args = if call["args"].is_null() {
                String::new()
            } else {
                serde_json::to_string_pretty(&call["args"]).unwrap_or_default()
            };
            let w = ui_weak.clone();
            slint::invoke_from_event_loop(move || {
                if let Some(row) = find_tool_row(&call_id) {
                    update_tool_row(row, |item| item.awaiting_approval = true);
                } else if let Some(ui) = w.upgrade() {
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
                    bump_scroll(&ui);
                }
            })
            .ok();
        }

        // Sensor bridge events: BME688 (air_quality) + MLX90640 (thermal_frame)
        "sensor_reading" => {
            let reading = ev["reading"].clone();
            match reading["kind"].as_str() {
                Some("air_quality") => {
                    let iaq   = reading["iaq"].as_f64().unwrap_or(0.0) as f32;
                    let temp  = reading["temperature_c"].as_f64().unwrap_or(0.0) as f32;
                    // sensor bridge may use "humidity" or "humidity_pct"
                    let humid = reading["humidity_pct"]
                        .as_f64()
                        .or_else(|| reading["humidity"].as_f64())
                        .unwrap_or(0.0) as f32;
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
                Some("thermal_frame") => {
                    let min_c  = reading["min_c"].as_f64().unwrap_or(0.0) as f32;
                    let max_c  = reading["max_c"].as_f64().unwrap_or(0.0) as f32;
                    let mean_c = reading["mean_c"].as_f64().unwrap_or(0.0) as f32;
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
        "council_started" => {
            let topic = ev["topic"].as_str().unwrap_or("").to_string();
            let agents: Vec<CouncilAgent> = ev["agents"].as_array()
                .map(|arr| arr.iter().enumerate().map(|(i, a)| {
                    let id = a["id"].as_str().unwrap_or("");
                    let persona = a["persona"].as_str().unwrap_or("");
                    CouncilAgent {
                        id: id.into(),
                        persona: if persona.is_empty() { id.into() } else { persona.into() },
                        accent: council_accent(a["color"].as_str(), i),
                        text: "".into(),
                        done: false,
                    }
                }).collect())
                .unwrap_or_default();
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

        "council_round_start" => {
            let round = ev["round"].as_u64().unwrap_or(0) as i32;
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

        "council_agent_delta" => {
            let agent_id = ev["agent_id"].as_str().unwrap_or("").to_string();
            let delta    = ev["delta"].as_str().unwrap_or("").to_string();
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

        "council_agent_done" => {
            let agent_id  = ev["agent_id"].as_str().unwrap_or("").to_string();
            let full_text = ev["full_text"].as_str().unwrap_or("").to_string();
            slint::invoke_from_event_loop(move || {
                council_update(&agent_id, |a| {
                    if !full_text.is_empty() { a.text = full_text.into(); }
                    a.done = true;
                });
            }).ok();
        }

        "council_round_done" => {
            let conv = ev["convergence"].as_f64().unwrap_or(0.0) as f32;
            let w = ui_weak.clone();
            slint::invoke_from_event_loop(move || {
                if let Some(ui) = w.upgrade() { ui.set_council_convergence(conv); }
            }).ok();
        }

        "council_complete" => {
            let reason    = ev["reason"].as_str().unwrap_or("").to_string();
            let synthesis = ev["synthesis"].as_str().unwrap_or("").to_string();
            let rounds    = ev["rounds"].as_u64().unwrap_or(0) as i32;
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

        "council_butt_in" => {
            let msg = ev["message"].as_str().unwrap_or("").to_string();
            if !msg.is_empty() { notify(ToastKind::Info, format!("Council: {msg}")); }
        }

        "sub_agent_started" => {
            let child = ev["child"].as_u64();
            let st = state.clone();
            let w = ui_weak.clone();
            slint::invoke_from_event_loop(move || {
                if let Some(ui) = w.upgrade() {
                    if let Some(c) = child {
                        let n = {
                            let mut g = st.lock().unwrap_or_else(|e| e.into_inner());
                            g.subagents.insert(c);
                            g.subagents.len() as i32
                        };
                        ui.set_subagent_count(n);
                    }
                }
            }).ok();
            notify(ToastKind::Info, "Sub-agent started");
        }

        _ => {}
    }
}
