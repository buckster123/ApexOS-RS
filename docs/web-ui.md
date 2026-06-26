# Web UI — the browser + mobile PWA (`web/`)

The human-access surface for **headless** nodes (DGX Spark, a ROCm laptop, any
server) and a second screen for kiosk/desktop nodes. A fresh, lean, installable
PWA that agentd serves at `http://<node>:8787/`. **-RS owns this** — it is *not* the
legacy `../ApexOS/ui` web app (that lived only in the do-not-modify reference repo
and was never deployed by -RS install.sh; this replaces it for the RS line).

> Zero build step, zero dependencies — plain HTML/CSS/vanilla-JS, the "apex" theme
> (terminal-green on `#0d0f18`, straight from `palette.slint`) so it matches the
> native Slint UI.

## Files (`web/`)

| File | Role |
|------|------|
| `index.html` | App shell — login overlay + chat view |
| `style.css` | The apex theme; mobile-first, scales to desktop |
| `app.js` | All logic — 3e login, WS, chat/tools/approvals rendering |
| `sw.js` | Service worker — installable + offline app-shell (network-first) |
| `manifest.json` | PWA manifest (standalone, theme colors, icon) |
| `icon.svg` | App icon (apex peak mark) |

## Deploy

install.sh copies `web/*` → `AGENTD_UI` (`/var/lib/agentd/ui`) on **every** node
(headless included — it's their only human interface), **copy-always** (these are
-RS-owned static assets, like a binary, not seed-if-absent config), so an
`apexos-update` refreshes the web client too. The gateway's `static_handler` serves
a filename **whitelist** — when you add a web asset, add its name + content-type
there (`sw.js` and `icon.svg` were added for the PWA; `index.html`/`app.js`/
`style.css`/`manifest.json` were already allowed).

## Auth — the 3e login flow (no node secret)

A human client never holds the machine `AGENTD_TOKEN`; it logs in for a short-lived
**session token** (agent-identity.md slice 3e). The PWA implements exactly that:

1. `GET /api/auth/profiles` (UNgated) → `{users:[{id,name,has_pin}], default_user}`.
2. **Default-skip:** an open `default_user` auto-logs-in (zero-tap); a PIN default
   jumps straight to the keypad.
3. Tile tap → open profile one-taps; a PIN profile opens the keypad (lockout-aware —
   `{locked, retry_after_secs}`).
4. `POST /api/auth/login {user_id, pin}` → `{ok, token, agent_id, expires_in}`.
5. Token → `localStorage` (`apexos_token`), used as `Authorization: Bearer` on gated
   REST and **`?token=`** on the WS. `GET /api/auth/me` re-validates a stored token
   on load (agentd clears in-memory tokens on restart → falls back to the login
   screen). `POST /api/auth/logout` revokes.

## Chat — the core agent loop over the WS

Connect `ws(s)://<host>/ws?token=<token>`. The gateway pushes
`session_init{session_id, history}` on connect (history is replayed). The client
sends `{type:user_prompt|user_approval|user_cancel}` and `{type:hello,new:true}`
(new chat). Inbound events handled (the typed `Event` enum, snake_case, ids as bare
numbers — see CLAUDE.md "agentd WebSocket protocol"):

| Event | UI |
|-------|-----|
| `agent_text{delta}` | append to the streaming agent bubble (markdown-lite, XSS-safe) |
| `agent_thinking{delta}` | dim "💭 …" line |
| `tool_requested{call}` | collapsible tool card (running) |
| `tool_result{call,output}` | update the card by `call` id (done/error) |
| `approval_pending{call}` | card with Approve/Reject → `user_approval{action,granted}` |
| `turn_complete` | clear busy, close the bubble |
| `error{message}` | system error line |

Global status events (sensors/mesh/council/vast) are received but not surfaced in
this minimal client — the native UI owns them.

## Files — the phone-handoff browser (📁)

The closing leg of the USB exo-workspace loop (`docs/usb-workspace.md`): a phone reaches
the workspace's files — **including a mounted `media/<label>` stick**, since sticks own-
mount *under* the workspace. A 📁 button in the topbar opens a full-screen Files panel:

- **Browse** — `GET /api/workspace/list?path=` (the same endpoint the native Explorer
  uses): tap a folder to descend, ↑ to go up, ⟳ to refresh. Built with `createElement` +
  `textContent` (XSS-safe), like the chat.
- **Download** — a per-file `⤓` is a plain `<a download href="/api/workspace/download?path=…&token=…">`.
  `require_token` accepts `?token=`, so a direct link works on mobile (native save sheet);
  `workspace_download_handler` is workspace-confined, content-typed by extension, ≤256 MB.
- **Upload** — a file input → `POST /api/workspace/upload?path=<dir>/<name>` with the
  **raw File as the body** (no multipart dep); `workspace_upload_handler` confines via
  `resolve_workspace_write_path` (rejects `..`, parent must exist), the route raises the
  body limit to 256 MB. So you push a phone photo straight onto the stick, then eject it
  from the native UI / agent and carry it to another node.

Both endpoints are gated + share the workspace confinement of the file verbs (#196).

## Deferred (web grows toward native parity)

Voice (mic → `/api/record`, TTS → `/api/speak`), sensors/home dashboard, full
session management (list/switch/archive/export), settings, image upload + camera,
council/mesh views. Each is a follow-on slice; the native Slint UI already covers
them. PNG icons (iOS add-to-homescreen polish) are a nice-to-have over the SVG.
