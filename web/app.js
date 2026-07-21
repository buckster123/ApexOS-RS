/* ApexOS web/PWA client — login (3e session-token auth) + streaming chat + tool
   cards + inline approvals over the agentd WS/REST contract. Vanilla JS, no build.

   Wire contract (see docs/agentd-protocol.md):
   - Auth: GET /api/auth/profiles (ungated) → POST /api/auth/login {user_id,pin}
     → {token}. The token is the Bearer for gated REST + `?token=` on the WS.
   - WS: on connect the gateway pushes {type:session_init, session_id, history}.
     Inbound events are the typed Event enum, snake_case, ids as bare numbers.
     We send {type:user_prompt|user_approval|user_cancel} and {type:hello,new:true}.
*/
'use strict';

const $ = (id) => document.getElementById(id);
const TOKEN_KEY = 'apexos_token';
const getToken = () => localStorage.getItem(TOKEN_KEY) || '';
const setToken = (t) => localStorage.setItem(TOKEN_KEY, t);
const clearToken = () => localStorage.removeItem(TOKEN_KEY);

// ── REST helper ──────────────────────────────────────────────────────────────
async function api(path, opts = {}) {
  const headers = Object.assign({ 'Content-Type': 'application/json' }, opts.headers || {});
  const t = getToken();
  if (t) headers['Authorization'] = 'Bearer ' + t;
  return fetch(path, Object.assign({}, opts, { headers }));
}

// ── App state ─────────────────────────────────────────────────────────────────
const S = {
  ws: null,
  sessionId: null,
  agentId: 'APEX',
  busy: false,
  openBubble: null,          // the assistant text node currently streaming, or null
  tools: new Map(),          // call_id → tool card element
  reconnectMs: 1000,
  closing: false,            // true while logging out — suppresses reconnect
  pinUser: null,             // {id,name} mid-PIN-entry
  pinBuf: '',
  tts: false,                // speak agent replies (client-side playback)
  gotAgentText: false,       // did this turn produce agent text (→ worth speaking)?
  recording: false,          // mic capture in progress
  rec: null,                 // MediaRecorder
};

// ════════════════════════════════════════════════════════════════════════════
//  Boot
// ════════════════════════════════════════════════════════════════════════════
async function boot() {
  if ('serviceWorker' in navigator) {
    navigator.serviceWorker.register('/sw.js').catch(() => {});
  }
  wireUI();

  // A stored token may be stale (agentd clears session tokens on restart). Verify
  // it cheaply before trusting it; fall back to the login screen on any failure.
  if (getToken()) {
    try {
      const r = await api('/api/auth/me');
      if (r.ok) {
        const me = await r.json();
        if (me && me.user_id) {
          S.agentId = me.agent_id || 'APEX';
          return enterApp();
        }
      }
    } catch (_) { /* fall through to login */ }
    clearToken();
  }
  showLogin();
}

// ════════════════════════════════════════════════════════════════════════════
//  Login (3e)
// ════════════════════════════════════════════════════════════════════════════
async function showLogin() {
  S.closing = true;
  if (S.ws) { try { S.ws.close(); } catch (_) {} S.ws = null; }
  $('app').classList.add('hidden');
  $('login').classList.remove('hidden');
  $('login-err').textContent = '';
  showProfiles();
  let data;
  try {
    const r = await fetch('/api/auth/profiles'); // UNgated — no token yet
    data = await r.json();
  } catch (_) {
    $('login-err').textContent = 'Cannot reach this node. Check the connection.';
    return;
  }
  const users = (data && data.users) || [];
  renderProfiles(users);

  // Default-profile auto-skip (slice 3e): an open default logs in zero-tap; a PIN
  // default jumps straight to the keypad.
  const def = data && data.default_user;
  if (def) {
    const u = users.find((x) => x.id === def);
    if (u && !u.has_pin) return login(u.id, '');
    if (u && u.has_pin) return startPin(u);
  }
}

function showProfiles() {
  $('profiles').classList.remove('hidden');
  $('pinpad').classList.add('hidden');
  $('login-sub').textContent = 'Choose your profile';
}

function renderProfiles(users) {
  const box = $('profiles');
  box.innerHTML = '';
  if (!users.length) {
    box.innerHTML = '<div class="login-sub">No profiles on this node yet.</div>';
    return;
  }
  for (const u of users) {
    const tile = document.createElement('button');
    tile.className = 'profile-tile';
    const initial = (u.name || u.id || '?').trim().charAt(0).toUpperCase();
    const av = document.createElement('div'); av.className = 'profile-avatar'; av.textContent = initial;
    const meta = document.createElement('div'); meta.className = 'profile-meta';
    const nm = document.createElement('div'); nm.className = 'profile-name'; nm.textContent = u.name || u.id;
    const hint = document.createElement('div'); hint.className = 'profile-hint';
    hint.textContent = u.has_pin ? 'PIN required' : 'One-tap sign in';
    meta.appendChild(nm); meta.appendChild(hint);
    const lock = document.createElement('div'); lock.className = 'profile-lock'; lock.textContent = u.has_pin ? '🔒' : '→';
    tile.appendChild(av); tile.appendChild(meta); tile.appendChild(lock);
    tile.onclick = () => (u.has_pin ? startPin(u) : login(u.id, ''));
    box.appendChild(tile);
  }
}

function startPin(u) {
  S.pinUser = u; S.pinBuf = '';
  $('profiles').classList.add('hidden');
  $('pinpad').classList.remove('hidden');
  $('login-sub').textContent = 'Enter your PIN';
  $('pin-name').textContent = u.name || u.id;
  $('login-err').textContent = '';
  renderPinDots(false);
}

function renderPinDots(bad) {
  const dots = $('pin-dots'); dots.innerHTML = '';
  const n = Math.max(4, S.pinBuf.length);
  for (let i = 0; i < n; i++) {
    const d = document.createElement('div');
    d.className = 'pin-dot' + (i < S.pinBuf.length ? ' on' : '') + (bad ? ' bad' : '');
    dots.appendChild(d);
  }
}

function pinKey(k) {
  if (k === 'back') { S.pinBuf = S.pinBuf.slice(0, -1); renderPinDots(false); return; }
  if (k === 'ok') { if (S.pinBuf.length) login(S.pinUser.id, S.pinBuf); return; }
  if (S.pinBuf.length >= 12) return;
  S.pinBuf += k; renderPinDots(false);
  $('login-err').textContent = '';
}

async function login(userId, pin) {
  $('login-err').textContent = '';
  let res;
  try {
    const r = await fetch('/api/auth/login', {
      method: 'POST', headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ user_id: userId, pin: pin || '' }),
    });
    res = await r.json();
  } catch (_) {
    $('login-err').textContent = 'Login failed — node unreachable.';
    return;
  }
  if (res && res.ok && res.token) {
    setToken(res.token);
    S.agentId = res.agent_id || 'APEX';
    return enterApp();
  }
  // Failure: locked-out vs wrong PIN.
  if (res && res.locked) {
    const secs = res.retry_after_secs || 60;
    $('login-err').textContent = `Too many attempts — locked for ${secs}s.`;
  } else {
    $('login-err').textContent = pin ? 'Wrong PIN.' : 'Sign-in failed.';
  }
  S.pinBuf = '';
  if (!$('pinpad').classList.contains('hidden')) renderPinDots(true);
}

// ════════════════════════════════════════════════════════════════════════════
//  App
// ════════════════════════════════════════════════════════════════════════════
function enterApp() {
  S.closing = false;
  $('login').classList.add('hidden');
  $('app').classList.remove('hidden');
  $('agent-name').textContent = S.agentId || 'APEX';
  clearMessages();
  connect();
}

function connect() {
  const proto = location.protocol === 'https:' ? 'wss' : 'ws';
  const url = `${proto}://${location.host}/ws?token=${encodeURIComponent(getToken())}`;
  let opened = false;
  setConn('off');
  const ws = new WebSocket(url);
  S.ws = ws;

  ws.onopen = () => { opened = true; S.reconnectMs = 1000; setConn('live'); };

  ws.onmessage = (ev) => {
    let m; try { m = JSON.parse(ev.data); } catch (_) { return; }
    onEvent(m);
  };

  ws.onclose = async () => {
    setConn('off');
    if (S.closing) return;
    // Distinguish an expired/cleared token (agentd restart drops in-memory tokens)
    // from a transient drop: a quick auth check decides login vs reconnect.
    if (!opened) {
      try {
        const r = await api('/api/auth/me');
        const me = r.ok ? await r.json() : null;
        if (!me || !me.user_id) { clearToken(); return showLogin(); }
      } catch (_) { /* network blip — keep retrying */ }
    }
    setTimeout(connect, S.reconnectMs);
    S.reconnectMs = Math.min(S.reconnectMs * 1.7, 15000);
  };

  ws.onerror = () => { try { ws.close(); } catch (_) {} };
}

function send(obj) {
  if (S.ws && S.ws.readyState === WebSocket.OPEN) S.ws.send(JSON.stringify(obj));
}

// ── Inbound event dispatch ────────────────────────────────────────────────────
function onEvent(m) {
  switch (m.type) {
    case 'session_init':
      S.sessionId = m.session_id;
      clearMessages();
      replayHistory(m.history || []);
      setBusy(false);
      break;
    case 'agent_text':
      appendAgentText(m.delta || '');
      S.gotAgentText = true;
      setBusy(true);
      break;
    case 'agent_thinking':
      appendThinking(m.delta || '');
      setBusy(true);
      break;
    case 'tool_requested':
      addToolCard(m.call, 'running');
      setBusy(true);
      break;
    case 'tool_result':
      updateToolCard(m.call, m.output);
      break;
    case 'approval_pending':
      addApprovalCard(m.call);
      setBusy(true);
      break;
    case 'turn_complete':
      S.openBubble = null;
      setBusy(false);
      if (S.tts && S.gotAgentText) speakWeb(lastAgentText());
      S.gotAgentText = false;
      break;
    case 'error':
      if (m.message) sysLine(m.message, true);
      setBusy(false);
      break;
    // sensors / mesh / council / vast are global status events — ignored in this
    // minimal client (the native UI surfaces them). Add handling in a later slice.
    default: break;
  }
}

// ════════════════════════════════════════════════════════════════════════════
//  Rendering
// ════════════════════════════════════════════════════════════════════════════
const messagesEl = () => $('messages');
function clearMessages() { messagesEl().innerHTML = ''; S.openBubble = null; S.tools.clear(); }
function atBottom() { const m = messagesEl(); return m.scrollHeight - m.scrollTop - m.clientHeight < 80; }
function scroll(force) { const m = messagesEl(); if (force || atBottom()) m.scrollTop = m.scrollHeight; }

function userMsg(text) {
  const d = document.createElement('div');
  d.className = 'msg user'; d.textContent = text;
  messagesEl().appendChild(d); scroll(true);
}

function appendAgentText(delta) {
  if (!S.openBubble) {
    const d = document.createElement('div');
    d.className = 'msg agent streaming';
    d._raw = '';
    messagesEl().appendChild(d);
    S.openBubble = d;
  }
  S.openBubble._raw += delta;
  renderMarkdownInto(S.openBubble, S.openBubble._raw);
  scroll(false);
}

function appendThinking(delta) {
  let el = messagesEl().lastElementChild;
  if (!el || !el.classList.contains('thinking') || S.openBubble) {
    el = document.createElement('div'); el.className = 'thinking'; el._raw = '';
    messagesEl().appendChild(el);
    S.openBubble = null; // thinking interrupts a text bubble
  }
  el._raw += delta; el.textContent = '💭 ' + el._raw;
  scroll(false);
}

function toolIcon(name) {
  if (/read|list|cat|grep|search|recall|get/i.test(name)) return '📄';
  if (/write|edit|create|save|append/i.test(name)) return '✏️';
  if (/run|exec|command|shell|bash/i.test(name)) return '⚙️';
  if (/git/i.test(name)) return '🔱';
  if (/http|fetch|web|curl/i.test(name)) return '🌐';
  if (/face|sketch|display/i.test(name)) return '🎨';
  if (/mesh|agent|spawn|peer/i.test(name)) return '🛰️';
  return '🔧';
}

function buildToolCard(call) {
  const card = document.createElement('div');
  card.className = 'tool';
  const head = document.createElement('div'); head.className = 'tool-head';
  const icn = document.createElement('span'); icn.className = 'tool-icn'; icn.textContent = toolIcon(call.tool);
  const name = document.createElement('span'); name.className = 'tool-name'; name.textContent = call.tool;
  const status = document.createElement('span'); status.className = 'tool-status running'; status.textContent = '···';
  head.appendChild(icn); head.appendChild(name); head.appendChild(status);
  head.onclick = () => card.classList.toggle('open');
  const body = document.createElement('div'); body.className = 'tool-body';
  const argLbl = document.createElement('div'); argLbl.className = 'tool-label'; argLbl.textContent = 'arguments';
  const argPre = document.createElement('pre'); argPre.textContent = pretty(call.args);
  body.appendChild(argLbl); body.appendChild(argPre);
  card.appendChild(head); card.appendChild(body);
  card._status = status; card._body = body;
  return card;
}

function addToolCard(call, _state) {
  S.openBubble = null; // any tool ends the current text bubble
  const card = buildToolCard(call);
  messagesEl().appendChild(card);
  S.tools.set(String(call.id), card);
  scroll(false);
}

function updateToolCard(callId, output) {
  const card = S.tools.get(String(callId));
  if (!card) return;
  const ok = output && output.ok;
  card._status.className = 'tool-status ' + (ok ? 'ok' : 'err');
  card._status.textContent = ok ? 'done' : 'error';
  const lbl = document.createElement('div'); lbl.className = 'tool-label'; lbl.textContent = 'result';
  const pre = document.createElement('pre'); pre.textContent = pretty(output ? output.content : null);
  card._body.appendChild(lbl); card._body.appendChild(pre);
  if (!ok) card.classList.add('open');
  scroll(false);
}

function addApprovalCard(call) {
  S.openBubble = null;
  const card = buildToolCard(call);
  card.classList.add('open');
  card._status.className = 'tool-status wait'; card._status.textContent = 'approve?';
  const row = document.createElement('div'); row.className = 'approve-row';
  const yes = document.createElement('button'); yes.className = 'btn-approve'; yes.textContent = '✓ Approve';
  const no = document.createElement('button'); no.className = 'btn-reject'; no.textContent = '✕ Reject';
  const decide = (granted) => {
    send({ type: 'user_approval', action: call.id, granted });
    row.remove();
    card._status.className = 'tool-status ' + (granted ? 'running' : 'err');
    card._status.textContent = granted ? '···' : 'rejected';
  };
  yes.onclick = () => decide(true);
  no.onclick = () => decide(false);
  row.appendChild(yes); row.appendChild(no);
  card.appendChild(row);
  messagesEl().appendChild(card);
  S.tools.set(String(call.id), card);
  scroll(true);
}

function sysLine(text, isErr) {
  const d = document.createElement('div');
  d.className = 'sys-line' + (isErr ? ' err' : ''); d.textContent = text;
  messagesEl().appendChild(d); scroll(false);
}

// History replay from session_init (Vec<Message>; ContentBlock tagged by `type`).
function replayHistory(history) {
  for (const msg of history) {
    const blocks = msg.content || [];
    if (msg.role === 'user') {
      const text = blocks.filter((b) => b.type === 'text').map((b) => b.text).join('\n').trim();
      if (text) userMsg(text);
      for (const b of blocks) if (b.type === 'image') renderImage(b);
    } else if (msg.role === 'assistant') {
      for (const b of blocks) {
        if (b.type === 'text' && b.text.trim()) {
          const d = document.createElement('div'); d.className = 'msg agent';
          renderMarkdownInto(d, b.text); messagesEl().appendChild(d);
        } else if (b.type === 'tool_use') {
          addToolCard({ id: b.id, tool: b.name, args: b.input }, 'done');
          const c = S.tools.get(String(b.id)); if (c) { c._status.className = 'tool-status ok'; c._status.textContent = 'done'; }
        }
      }
    }
  }
  S.openBubble = null;
  scroll(true);
}

function renderImage(b) {
  const img = document.createElement('img');
  img.src = `data:${b.media_type};base64,${b.data}`;
  img.style.cssText = 'max-width:88%;border-radius:10px;margin:0 0 12px;display:block;';
  messagesEl().appendChild(img);
}

// ── XSS-safe markdown-lite: escapes everything, then renders ``` fences and
//    `inline code`. Anything else stays literal pre-wrapped text. ───────────────
function renderMarkdownInto(el, raw) {
  el.innerHTML = '';
  const parts = raw.split(/```/);
  parts.forEach((part, i) => {
    if (i % 2 === 1) {
      const pre = document.createElement('pre'); const code = document.createElement('code');
      code.textContent = part.replace(/^[^\n]*\n/, (m) => (/^\w+\n$/.test(m) ? '' : m)); // strip a lone lang line
      pre.appendChild(code); el.appendChild(pre);
    } else {
      const segs = part.split(/`/);
      segs.forEach((seg, j) => {
        if (j % 2 === 1) { const c = document.createElement('code'); c.textContent = seg; el.appendChild(c); }
        else if (seg) el.appendChild(document.createTextNode(seg));
      });
    }
  });
}

function pretty(v) {
  if (v == null) return '';
  if (typeof v === 'string') return v;
  try { return JSON.stringify(v, null, 2); } catch (_) { return String(v); }
}

// ════════════════════════════════════════════════════════════════════════════
//  Actions + UI wiring
// ════════════════════════════════════════════════════════════════════════════
function setBusy(b) {
  S.busy = b;
  $('cancel-btn').classList.toggle('hidden', !b);
  $('send-btn').disabled = b;
  if (S.busy && S.openBubble) S.openBubble.classList.add('streaming');
  if (!S.busy && S.openBubble) S.openBubble.classList.remove('streaming');
}
function setConn(cls) { const d = $('conn-dot'); d.className = 'dot ' + (cls === 'live' ? 'live' : 'off'); }

function sendPrompt() {
  const ta = $('input'); const text = ta.value.trim();
  if (!text || S.busy) return;
  userMsg(text);
  send({ type: 'user_prompt', text });
  ta.value = ''; ta.style.height = 'auto';
  S.gotAgentText = false;
  setBusy(true);
}

// ── Voice (client-side, like the native UI) ───────────────────────────────────
// TTS plays in the browser (works over plain HTTP). STT needs getUserMedia, which
// browsers gate to a secure context (HTTPS or localhost) — so the mic button is
// hidden over http://<LAN-IP> and the node would need TLS for phone mic capture.

function lastAgentText() {
  const bubbles = messagesEl().querySelectorAll('.msg.agent');
  const last = bubbles[bubbles.length - 1];
  return last ? (last._raw || last.textContent || '').trim() : '';
}

// Fetch synthesized audio from /api/tts and play it in the browser.
async function speakWeb(text) {
  if (!text) return;
  try {
    const r = await api('/api/tts', { method: 'POST', body: JSON.stringify({ text }) });
    if (!r.ok) return;
    const blob = await r.blob();
    if (!blob.size) return;
    const audio = new Audio(URL.createObjectURL(blob));
    audio.onended = () => URL.revokeObjectURL(audio.src);
    await audio.play();
  } catch (_) { /* autoplay can be blocked until a user gesture — ignore */ }
}

function toggleTts() {
  S.tts = !S.tts;
  updateVoiceUI();
}

const micAvailable = () =>
  window.isSecureContext && navigator.mediaDevices && navigator.mediaDevices.getUserMedia;

// Record the mic (MediaRecorder → webm) and POST to /api/transcribe → prompt.
async function micToggle() {
  if (S.recording) { if (S.rec) S.rec.stop(); return; }
  let stream;
  try {
    stream = await navigator.mediaDevices.getUserMedia({ audio: true });
  } catch (_) {
    sysLine('Microphone unavailable (needs HTTPS or localhost)', true);
    return;
  }
  const chunks = [];
  const rec = new MediaRecorder(stream);
  S.rec = rec;
  rec.ondataavailable = (e) => { if (e.data && e.data.size) chunks.push(e.data); };
  rec.onstop = async () => {
    stream.getTracks().forEach((t) => t.stop());
    S.recording = false; S.rec = null; updateVoiceUI();
    const blob = new Blob(chunks, { type: rec.mimeType || 'audio/webm' });
    if (!blob.size) return;
    try {
      const r = await api('/api/transcribe', {
        method: 'POST', body: blob, headers: { 'Content-Type': blob.type || 'audio/webm' },
      });
      if (!r.ok) return;
      const j = await r.json();
      const text = (j.text || '').trim();
      if (text) { $('input').value = text; sendPrompt(); }
    } catch (_) { /* network error — silent */ }
  };
  rec.start();
  S.recording = true; updateVoiceUI();
}

function updateVoiceUI() {
  const tts = $('tts-btn');
  if (tts) tts.classList.toggle('voice-on', S.tts);
  const mic = $('mic-btn');
  if (mic) mic.classList.toggle('voice-rec', S.recording);
}

function newChat() {
  if (S.busy) send({ type: 'user_cancel' });
  send({ type: 'hello', new: true });
  clearMessages();
  setBusy(false);
}

function cancelTurn() {
  send({ type: 'user_cancel' });
  if (S.openBubble) S.openBubble.classList.remove('streaming');
  S.openBubble = null;
  setBusy(false);
  sysLine('— turn cancelled —');
}

async function logout() {
  S.closing = true;
  try { await api('/api/auth/logout', { method: 'POST', body: JSON.stringify({ token: getToken() }) }); } catch (_) {}
  clearToken();
  showLogin();
}

// ── Files (phone-handoff: browse / upload / download the workspace) ───────
let filesPath = '';   // current dir, "" = workspace root

function openFiles() { $('files').classList.remove('hidden'); loadDir(''); }
function closeFiles() { $('files').classList.add('hidden'); }

function filesMsg(t) {
  const el = $('files-msg');
  if (!t) { el.classList.add('hidden'); el.textContent = ''; return; }
  el.classList.remove('hidden'); el.textContent = t;
}

function humanSize(n) {
  if (!n) return '';
  const u = ['B', 'KB', 'MB', 'GB']; let i = 0; let v = n;
  while (v >= 1024 && i < u.length - 1) { v /= 1024; i++; }
  return i === 0 ? n + ' B' : v.toFixed(1) + ' ' + u[i];
}

function fileIcon(ext) {
  ext = (ext || '').toLowerCase();
  if (['png', 'jpg', 'jpeg', 'gif', 'webp', 'bmp', 'svg', 'heic'].includes(ext)) return '🖼';
  if (['mp4', 'webm', 'mov', 'mkv'].includes(ext)) return '🎬';
  if (['mp3', 'wav', 'ogg', 'flac', 'm4a'].includes(ext)) return '🎵';
  if (ext === 'pdf') return '📕';
  if (['zip', 'tar', 'gz'].includes(ext)) return '🗜';
  return '📄';
}

async function loadDir(path) {
  filesPath = path;
  $('files-path').textContent = path === '' ? 'workspace' : path;
  filesMsg('');
  const list = $('files-list');
  list.textContent = '';
  let entries = [];
  try {
    const r = await api('/api/workspace/list?path=' + encodeURIComponent(path));
    const data = await r.json();
    entries = (data && data.entries) || [];
  } catch (_) { filesMsg('Could not load this folder.'); return; }
  if (!entries.length) { filesMsg('Empty folder.'); return; }
  for (const e of entries) {
    const isDir = e.kind === 'dir';
    const row = document.createElement('div');
    row.className = 'file-row' + (isDir ? ' is-dir' : '');
    const icn = document.createElement('span'); icn.className = 'file-icon'; icn.textContent = isDir ? '📁' : fileIcon(e.ext);
    const nm = document.createElement('span'); nm.className = 'file-name'; nm.textContent = e.name;
    const sz = document.createElement('span'); sz.className = 'file-size'; sz.textContent = isDir ? '' : humanSize(e.size);
    row.appendChild(icn); row.appendChild(nm); row.appendChild(sz);
    if (isDir) {
      row.onclick = () => loadDir(e.path);
    } else {
      // Direct link — require_token accepts ?token=, so a plain <a download> works on mobile.
      const dl = document.createElement('a');
      dl.className = 'file-dl'; dl.textContent = '⤓'; dl.title = 'Download';
      dl.href = '/api/workspace/download?path=' + encodeURIComponent(e.path) + '&token=' + encodeURIComponent(getToken());
      dl.setAttribute('download', e.name);
      row.appendChild(dl);
    }
    list.appendChild(row);
  }
}

function filesUp() {
  if (filesPath === '') return;
  const i = filesPath.lastIndexOf('/');
  loadDir(i >= 0 ? filesPath.slice(0, i) : '');
}

async function uploadFile(file) {
  if (!file) return;
  const target = (filesPath === '' ? '' : filesPath + '/') + file.name;
  filesMsg('Uploading ' + file.name + '…');
  try {
    const r = await api('/api/workspace/upload?path=' + encodeURIComponent(target), { method: 'POST', body: file });
    const data = await r.json();
    if (data && data.ok) { filesMsg(''); loadDir(filesPath); }
    else { filesMsg('Upload failed: ' + ((data && data.error) || 'unknown')); }
  } catch (_) { filesMsg('Upload failed — node unreachable.'); }
}

function wireUI() {
  // PIN keypad
  document.querySelectorAll('#pinpad .keys button').forEach((b) => {
    b.onclick = () => pinKey(b.dataset.k);
  });
  $('pin-cancel').onclick = () => { S.pinUser = null; S.pinBuf = ''; showProfiles(); $('login-err').textContent = ''; };

  // Composer
  const ta = $('input');
  ta.addEventListener('input', () => { ta.style.height = 'auto'; ta.style.height = Math.min(ta.scrollHeight, 140) + 'px'; });
  ta.addEventListener('keydown', (e) => {
    if (e.key === 'Enter' && !e.shiftKey) { e.preventDefault(); sendPrompt(); }
  });
  $('send-btn').onclick = sendPrompt;
  $('cancel-btn').onclick = cancelTurn;
  $('new-btn').onclick = newChat;
  $('logout-btn').onclick = logout;

  // Voice: TTS always (audio playback needs no secure context); mic only where
  // getUserMedia is allowed (HTTPS / localhost), otherwise hide the button.
  $('tts-btn').onclick = toggleTts;
  if (micAvailable()) $('mic-btn').onclick = micToggle;
  else $('mic-btn').classList.add('hidden');

  // Files (phone-handoff browser)
  $('files-btn').onclick = openFiles;
  $('files-close').onclick = closeFiles;
  $('files-up').onclick = filesUp;
  $('files-refresh').onclick = () => loadDir(filesPath);
  $('files-upload').onchange = (e) => { const f = e.target.files[0]; uploadFile(f); e.target.value = ''; };

  // Hardware keyboard PIN entry on desktop
  document.addEventListener('keydown', (e) => {
    if ($('pinpad').classList.contains('hidden') || $('login').classList.contains('hidden')) return;
    if (/^[0-9]$/.test(e.key)) pinKey(e.key);
    else if (e.key === 'Backspace') pinKey('back');
    else if (e.key === 'Enter') pinKey('ok');
  });
}

boot();
