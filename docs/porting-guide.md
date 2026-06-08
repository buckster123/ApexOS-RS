# UI Porting Guide — ApexOS → ApexOS-RS

Mapping every window and feature from the current HTML/JS frontend to Slint equivalents.

## Feature Status

| Feature | Current tech | RustySlint path | Difficulty | Step |
|---------|-------------|----------------|-----------|------|
| Agent chat (streaming) | JS + WS delta events | `Text` + `ScrollView` + `VecModel` | Easy | 2 |
| Tool call blocks (collapsible) | JS DOM | `Repeater` + custom component | Medium | 3 |
| Approval buttons | JS inline | Callback in tool block component | Medium | 3 |
| Home dashboard | JS + `fetch /api/run` | Rust timer + `reqwest` polls | Easy | 4 |
| Thermal heatmap | HTML5 canvas | `slint::Image` from RGBA pixels | Medium | 5 |
| IAQ badge | JS color logic | Slint property bindings | Easy | 5 |
| Session picker | JS modal | `Popup` + `VecModel` | Easy | 6 |
| Session history replay | WS session_init with ID | Same WS protocol, same Rust | Easy | 6 |
| Mic button (server-side) | `fetch /api/record/start` | `reqwest` (http-api feature) | Easy | 7 |
| Speaker toggle + TTS | `fetch /api/speak` | `reqwest` | Easy | 7 |
| Soul.md editor | `<textarea>` | `TextEdit` component | Easy | 8 |
| Plugin list | JS Alpine.js | `Repeater` + `StandardListView` | Easy | 8 |
| Policy mode selector | HTML `<select>` | `ComboBox` | Easy | 8/9 |
| Model selector | HTML `<select>` | `ComboBox` | Easy | 9 |
| Power modal (reboot/shutdown) | JS countdown modal | `Dialog` + timer | Easy | 9 |
| Wake triggered indicator | WS event → JS | `wake_triggered` event → Slint | Easy | 7 |
| Sub-agent windows | WinBox popup per child session | `Popup` per child session | Medium | post-v1 |
| PTY terminal | xterm.js + `/terminal-ws` | `alacritty_terminal` crate | Hard | post-v1 |
| Sketchpad | HTML5 canvas | Slint custom painter | Hard | post-v1 |
| Monaco IDE | JS bundle | Drop or embedded webview | Hard | post-v1 |
| Browser iframe | `<iframe>` | Not possible — drop | N/A | drop |
| Cerebro web UI | `<iframe>` to :8767 | Link opens in browser externally | N/A | drop |
| SensorHead iframe | `<iframe>` to :8080 | Link opens in browser externally | N/A | drop |

---

## Dropped Features (v1) and Mitigations

### Browser window & iframes
Slint has no iframe concept. Cerebro and SensorHead dashboards can be accessed via
the PTY terminal (`curl http://localhost:8767/...`) or a separate SSH browser session.
Long term: embed a minimal webkit2gtk webview for these two windows only.

### Monaco IDE
No Rust equivalent with the same power. Options for v1:
1. Accept: editing `soul.md` / scripts is done over SSH in vim/nano
2. Basic: Slint `TextEdit` for soul.md only (no syntax highlighting)
3. Post-v1: `alacritty_terminal` PTY window lets you run any editor in-place

### Sketchpad
Slint has `Path` and custom rendering hooks but no equivalent of `CanvasRenderingContext2D`.
Defer to post-v1; it's not on the core agent workflow path.

---

## Slint Component Map

### Chat message bubble
```slint
component MessageBubble {
    in property <string> text;
    in property <bool> is-agent;
    Rectangle {
        background: is-agent ? #1e293b : #0f4c75;
        border-radius: 8px;
        Text { text: text; color: #e2e8f0; wrap: word-wrap; }
    }
}
```

### Tool call block (collapsible)
```slint
component ToolBlock {
    in property <string> tool-name;
    in property <string> status;   // "pending" | "running" | "done" | "error"
    in property <string> output;
    in-out property <bool> expanded: false;
    callback approve(); callback reject();
    // ... toggle on click, show approve/reject if status == "pending"
}
```

### IAQ badge
```slint
component IaqBadge {
    in property <int> iaq;
    property <color> badge-color: iaq < 50 ? #22c55e : iaq < 100 ? #84cc16 :
                                   iaq < 150 ? #eab308 : iaq < 200 ? #f97316 : #ef4444;
    Rectangle {
        background: badge-color;
        border-radius: 4px;
        Text { text: iaq < 50 ? "Excellent" : iaq < 100 ? "Good" :
                     iaq < 150 ? "Moderate" : iaq < 200 ? "Poor" : "Hazardous"; }
    }
}
```

---

## Event → UI mapping (Rust dispatch table)

```rust
match ev_type {
    "hello"           => set session_id, update status label
    "turn_started"    => clear agent text, set agent_busy = true
    "agent_text"      => append delta to agent text buffer
    "turn_complete"   => set agent_busy = false, speak if speaker_on
    "tool_requested"  => push ToolBlock to tool list (status=running)
    "tool_result"     => update ToolBlock by call_id (status=done, set output)
    "approval_pending"=> update ToolBlock (status=pending, show buttons)
    "sensor_reading"  => update IAQ / thermal frame state
    "wake_triggered"  => flash wake indicator, enable mic
    "sub_agent_started"=> open child session Popup
    _                 => ignore
}
```

---

## Memory Budget (target)

| Component | Expected RAM |
|-----------|-------------|
| Slint runtime + window | ~3 MB |
| WS + tokio runtime | ~2 MB |
| Fonts (embedded) | ~1 MB |
| App state (messages, sessions) | ~1-5 MB |
| **Total** | **~7-11 MB** |

Compare: Chromium kiosk on Pi = 200-400 MB.
On Pi Zero 2W (512 MB total): leaves ~500 MB for agentd + CerebroCortex.
On Pi 4 2GB: plenty of headroom.
