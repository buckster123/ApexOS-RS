'use strict';
/* ============================================================
   Lucida (U1) — the Atlas lens over a real cerebro.db.
   Data: /graph/export (visual channels) + /graph/layout (cached
   PCA of the embeddings) + /stats + /memory/health.
   Interaction rules (charter, field feedback 2026-08-07):
   hover = highlight + tooltip ONLY; click = pinned select.
   ============================================================ */

const REDUCED = matchMedia('(prefers-reduced-motion: reduce)').matches;

/* ---------- auth: ?token=… once, then sessionStorage ---------- */
const urlParams = new URLSearchParams(location.search);
if (urlParams.get('token')) {
  sessionStorage.setItem('lucida-token', urlParams.get('token'));
  urlParams.delete('token');
  const qs = urlParams.toString();
  history.replaceState(null, '', location.pathname + (qs ? '?' + qs : ''));
}
const TOKEN = sessionStorage.getItem('lucida-token') || '';
const AGENT = urlParams.get('agent') || '';

async function api(path, opts = {}) {
  const headers = Object.assign(
    { 'Content-Type': 'application/json' },
    TOKEN ? { 'Authorization': 'Bearer ' + TOKEN } : {},
    opts.headers || {},
  );
  const resp = await fetch(path, Object.assign({}, opts, { headers }));
  if (resp.status === 401) {
    notice('Unauthorized. Reload with <code>?token=&lt;your token&gt;</code> in the URL.');
    throw new Error('unauthorized');
  }
  if (!resp.ok) throw new Error(path + ' → ' + resp.status);
  return resp.json();
}

function notice(html) {
  const el = document.getElementById('notice');
  el.innerHTML = html;
  el.hidden = false;
}

/* ---------- palette ---------- */
const TYPE_COLOR = {
  episodic:    '#c07f28',
  affective:   '#2fa8a0',
  semantic:    '#3a7de0',
  prospective: '#3aa76c',
  procedural:  '#8f6ee0',
  schematic:   '#cf5d96',
};
const RISK = '#e05252';
const FALLBACK_COLOR = '#8fa0b8';

/* ---------- state ---------- */
let nodes = [];               // {id,type,layer,salience,tags,head,chars,glow,retr,x,y,rim,…}
let edges = [];               // {a,b (indices), weight,eff,cold}
let edgeRank = [];            // edge indices sorted by effective weight, desc
let byId = new Map();
let adj = [];                 // node index → [edge index]
let health = null;
let islandSet = new Set();    // U6: members of minor components (amber rings)

const view = { x: 0, y: 0, k: 0.55 };
let lens = 'atlas';
let hoverIdx = -1, selectedIdx = -1;
let searchSet = null;         // Set of node indices matched by the last recall
let dirty = true;
let layoutCoords = {};        // id → [x, y] (kept for Dream-lens embers)
const dream = { reports: [], selected: -1, embers: [] };   // U4 state

/* ---------- settings (U1b): localStorage-persisted, applied live ---------- */
const S_DEFAULTS = {
  twinkle: true, hopMs: 950, density: 1, topK: 12,
  defaultLens: 'atlas', tickerDepth: 40, pauseStream: false,
};
const S = Object.assign({}, S_DEFAULTS,
  JSON.parse(localStorage.getItem('lucida.settings') || '{}'));
function saveSettings() {
  localStorage.setItem('lucida.settings', JSON.stringify(S));
  dirty = true;
}

/* Thought lens (U2): one traced recall, animated. Everything here is the
   REAL spread — seeds, per-hop edge walks, final activations — recorded by
   the engine, not simulated. */
const hopMs = () => (REDUCED ? 1 : S.hopMs);
const thought = {
  trace: null,      // {seedIdx:Map(idx→sim), events:[{hop,a,b,amount}], boost:Map(idx→act), maxHop}
  t0: 0, done: false,
};
function thoughtHopNow(now) {
  return thought.trace ? (now - thought.t0) / hopMs() : -1;
}

/* deterministic small hash for rim placement + jitter */
function idHash(s) {
  let h = 2166136261;
  for (let i = 0; i < s.length; i++) { h ^= s.charCodeAt(i); h = Math.imul(h, 16777619); }
  return (h >>> 0) / 4294967296;
}

/* ---------- boot ---------- */
async function boot() {
  const scopeQ = AGENT ? '?agent_id=' + encodeURIComponent(AGENT) : '';
  document.getElementById('agent-chip').textContent = AGENT ? '⚒ ' + AGENT : '◎ ALL';

  let exp, layout, stats, healthResp;
  try {
    [exp, layout, stats, healthResp] = await Promise.all([
      api('/graph/export' + scopeQ),
      api('/graph/layout'),
      api('/stats'),
      api('/memory/health' + scopeQ),
    ]);
  } catch (e) {
    if (e.message !== 'unauthorized')
      notice('Could not reach cerebro-api: <code>' + e.message + '</code>');
    return;
  }
  health = healthResp;

  const WORLD = 850;
  nodes = exp.nodes.map((n, i) => {
    const c = layout.coords[n.id];
    const h = idHash(n.id);
    let x, y, rim = false;
    if (c) {
      // Cached PCA position + a tiny deterministic jitter so exact-duplicate
      // embeddings don't stack into one star.
      x = c[0] * WORLD + (h - 0.5) * 30;
      y = c[1] * WORLD + (idHash(n.id + '·') - 0.5) * 30;
    } else {
      // No embedding (FTS5-only store, or pre-backfill row): the outer rim,
      // honestly outside the semantic map rather than faked into it.
      rim = true;
      const ang = h * Math.PI * 2;
      const r = WORLD * 1.35 + idHash(n.id + 'r') * 120;
      x = Math.cos(ang) * r;
      y = Math.sin(ang) * r * 0.72;
    }
    byId.set(n.id, i);
    const tags = n.tags || [];
    return {
      id: n.id, type: n.memory_type, layer: n.layer, salience: n.salience,
      tags, head: n.content_head, chars: n.content_chars,
      glow: n.activation, retr: n.retrievability, access: n.access_count,
      created: n.created_at, agent: n.agent_id,
      x, y, rim, twinkle: h * Math.PI * 2, degree: 0,
      /* U6: rim-label honesty + the at-risk gutter's filter */
      embedded: !!n.embedded, reviewed: !!n.reviewed,
      /* exo-evolution markers (U4 Dream lens overlay) */
      champion:  tags.includes('skill_champion'),
      mutant:    tags.includes('dream_mutated') || tags.includes('dream_merged'),
      dreamBorn: tags.includes('dream_formed') || tags.includes('dream_distilled')
              || tags.includes('dream_extracted'),
      pruneCand: tags.includes('prune_candidate'),
    };
  });
  layoutCoords = layout.coords;   /* kept for ember placement (Dream lens) */

  edges = [];
  adj = nodes.map(() => []);
  for (const e of exp.edges) {
    const a = byId.get(e.source), b = byId.get(e.target);
    if (a === undefined || b === undefined) continue;
    const idx = edges.length;
    edges.push({ a, b, weight: e.weight, eff: e.effective_weight, cold: !e.last_traversed });
    adj[a].push(idx); adj[b].push(idx);
    nodes[a].degree++; nodes[b].degree++;
  }
  // Density LOD: a dense brain (auto-link + Hebbian encoding) can carry 30+
  // links per node — at overview zoom only the strongest strands may draw,
  // or the field becomes the hairball the charter forbids. Rank once.
  edgeRank = edges.map((_, i) => i).sort((i, j) => edges[j].eff - edges[i].eff);

  /* telemetry + health panel */
  const g = (health && health.graph) || {};
  setText('t-mem',   stats.total_memories);
  setText('t-links', stats.total_links);
  setText('t-cold',  fmtPct(g.never_traversed_links_pct));
  setText('t-comp',  g.components ?? '—');
  setText('h-comp',  g.components ?? '—');
  setText('h-iso',   (g.isolated_memories ?? '—') + ' (' + fmtPct(g.isolated_pct) + ')');
  setText('h-cold',  fmtPct(g.never_traversed_links_pct));
  setText('h-largest', g.largest_component ?? '—');

  /* U6: minor-component members ring amber in the health lens */
  islandSet = new Set();
  for (const isl of (g.islands || []))
    for (const m of (isl.members || [])) {
      const idx = byId.get(m.id);
      if (idx !== undefined) islandSet.add(idx);
    }
  /* at-risk = the engine's activation_at_risk filter, in-field: reviewed
     rows whose FSRS retrievability fell under the tool's 0.7 default */
  setText('h-risk', nodes.filter(n => n.reviewed && n.retr < 0.7).length);

  /* U6 time-lapse baseline: the live channels, restored when the slider
     comes home (a projection never overwrites truth, only the view) */
  projBase = {
    glow: nodes.map(n => n.glow),
    retr: nodes.map(n => n.retr),
    eff:  edges.map(e => e.eff),
  };
  resetProjection();

  if (exp.truncated)
    notice('Field truncated to ' + nodes.length + ' memories — raise <code>?cap=</code> on /graph/export when the LOD work lands.');

  dirty = true;
}
function setText(id, v) { document.getElementById(id).textContent = String(v); }
function fmtPct(v) { return v == null ? '—' : v + '%'; }

/* ---------- time-lapse (U6): project the sky forward ----------
   The server recomputes every decay channel (ACT-R glow, FSRS retrievability,
   link half-life) at now+N days — same math, later clock. The slider only
   changes the view; projBase restores live truth at 0. */
let projBase = null, projDays = 0, projTimer = null, projSeq = 0;
const projSlider = document.getElementById('h-proj');
function projLabel() {
  setText('h-proj-label', projDays === 0 ? 'now' : '+' + projDays + 'd');
  document.body.classList.toggle('projected', projDays > 0);
}
function resetProjection() {
  projDays = 0;
  projSeq++;                      /* orphan any in-flight projection fetch */
  if (projSlider) projSlider.value = 0;
  if (projBase) {
    nodes.forEach((n, i) => { n.glow = projBase.glow[i]; n.retr = projBase.retr[i]; });
    edges.forEach((e, i) => { e.eff = projBase.eff[i]; });
    setText('h-risk', nodes.filter(n => n.reviewed && n.retr < 0.7).length);
  }
  projLabel();
  dirty = true;
}
async function applyProjection(days) {
  projDays = days;
  projLabel();
  if (days === 0) { resetProjection(); return; }
  const seq = ++projSeq;
  const scopeQ = AGENT ? '&agent_id=' + encodeURIComponent(AGENT) : '';
  const at = new Date(Date.now() + days * 86400e3).toISOString();
  try {
    const exp = await api('/graph/export?at=' + encodeURIComponent(at) + scopeQ);
    if (seq !== projSeq) return;   /* a newer drag position superseded us */
    const chan = new Map(exp.nodes.map(n => [n.id, n]));
    for (const n of nodes) {
      const c = chan.get(n.id);
      if (c) { n.glow = c.activation; n.retr = c.retrievability; }
    }
    const eff = new Map(exp.edges.map(e => [e.source + '|' + e.target, e.effective_weight]));
    for (const e of edges) {
      const v = eff.get(nodes[e.a].id + '|' + nodes[e.b].id);
      if (v !== undefined) e.eff = v;
    }
    setText('h-risk', nodes.filter(n => n.reviewed && n.retr < 0.7).length);
    dirty = true;
  } catch (e) { /* keep the current view; the status quo is honest */ }
}
if (projSlider) projSlider.addEventListener('input', () => {
  const d = parseInt(projSlider.value, 10) || 0;
  projDays = d; projLabel();               /* label tracks the thumb live */
  clearTimeout(projTimer);
  projTimer = setTimeout(() => applyProjection(d), 350);
});

/* ---------- canvas ---------- */
const canvas = document.getElementById('field');
const ctx = canvas.getContext('2d');
let W = 0, H = 0, DPR = 1;
function resize() {
  DPR = Math.min(devicePixelRatio || 1, 2);
  W = innerWidth; H = innerHeight;
  canvas.width = W * DPR; canvas.height = H * DPR;
  canvas.style.width = W + 'px'; canvas.style.height = H + 'px';
  dirty = true;
}
addEventListener('resize', resize);
resize();

/* additive glow sprites, one per hue */
const SPRITE = 64;
const sprites = {};
function makeSprite(color) {
  const c = document.createElement('canvas');
  c.width = c.height = SPRITE;
  const g = c.getContext('2d');
  const grad = g.createRadialGradient(SPRITE/2, SPRITE/2, 0, SPRITE/2, SPRITE/2, SPRITE/2);
  grad.addColorStop(0, color);
  grad.addColorStop(0.25, color + 'b0');
  grad.addColorStop(0.6, color + '30');
  grad.addColorStop(1, color + '00');
  g.fillStyle = grad;
  g.fillRect(0, 0, SPRITE, SPRITE);
  return c;
}
for (const [t, c] of Object.entries(TYPE_COLOR)) sprites[t] = makeSprite(c);
sprites.risk = makeSprite(RISK);
sprites.fallback = makeSprite(FALLBACK_COLOR);

const sx = x => (x - view.x) * view.k + W / 2;
const sy = y => (y - view.y) * view.k + H / 2;

/* ---------- render ---------- */
let lastFrame = -1e9;
function render(now) {
  requestAnimationFrame(render);
  while (pulses.length && now - pulses[0].t0 > 8000) pulses.shift();
  const animating = !REDUCED || (thought.trace && !thought.done) || pulses.length > 0;
  if (!dirty && !animating) return;
  if (now - lastFrame < 30) return;
  lastFrame = now; dirty = false;

  ctx.setTransform(DPR, 0, 0, DPR, 0, 0);
  ctx.fillStyle = '#070b12';
  ctx.fillRect(0, 0, W, H);

  const focusSet = selectedIdx >= 0 ? new Set(adj[selectedIdx]) : null;

  /* Thought ripple clock */
  const hopNow = thoughtHopNow(now);
  if (thought.trace && !thought.done && hopNow > thought.trace.maxHop + 0.6) {
    thought.done = true;
    document.getElementById('results').classList.add('open');
  }

  /* links — density LOD: at overview zoom only the strongest strands draw;
     zooming in earns the full web. Focus and health override per-edge. */
  const zoomShare = (view.k < 0.8 ? 0.08 : view.k < 1.4 ? 0.35 : 1.0) * S.density;
  const drawCount = Math.min(edges.length,
    Math.max(400, Math.floor(edges.length * zoomShare)));
  const densityDim = Math.min(1, 900 / Math.max(1, drawCount));
  ctx.lineWidth = 1;
  for (let r = 0; r < edges.length; r++) {
    const i = edgeRank[r];
    const e = edges[i];
    const inLod = r < drawCount;
    const focused = focusSet && focusSet.has(i);
    if (!inLod && !focused && lens !== 'health') continue;

    const A = nodes[e.a], B = nodes[e.b];
    if (A.trashed || B.trashed) continue;
    const x1 = sx(A.x), y1 = sy(A.y), x2 = sx(B.x), y2 = sy(B.y);
    if (Math.max(x1, x2) < 0 || Math.min(x1, x2) > W ||
        Math.max(y1, y2) < 0 || Math.min(y1, y2) > H) continue;

    let alpha, color = '122,152,199';
    if (lens === 'health') {
      if (!inLod && !e.cold) continue;           /* cold strands are the point */
      alpha = (e.cold ? 0.20 : 0.07) * Math.max(densityDim, 0.35);
      if (e.cold) color = '90,140,235';
    } else if (focused) {
      alpha = 0.55;
    } else if (selectedIdx >= 0 || searchSet || thought.trace) {
      alpha = 0.02 * densityDim;
    } else {
      alpha = (0.04 + e.eff * 0.14) * densityDim;
    }
    ctx.strokeStyle = `rgba(${color},${alpha})`;
    ctx.beginPath(); ctx.moveTo(x1, y1); ctx.lineTo(x2, y2); ctx.stroke();
  }

  /* Thought ripple: the real spread walks, hop by hop, as light */
  if (thought.trace) {
    ctx.save();
    ctx.globalCompositeOperation = 'lighter';
    ctx.lineWidth = 1.4;
    for (const e of thought.trace.events) {
      const p = REDUCED ? 1 : Math.min(1, Math.max(0, hopNow - (e.hop - 1)));
      if (p <= 0) continue;
      const A = nodes[e.a], B = nodes[e.b];
      const x1 = sx(A.x), y1 = sy(A.y);
      const x2 = x1 + (sx(B.x) - x1) * p, y2 = y1 + (sy(B.y) - y1) * p;
      const fade = thought.done
        ? 0.10 + 0.20 * e.norm
        : Math.max(0.15, 1 - Math.max(0, hopNow - e.hop) * 0.5) * (0.25 + 0.55 * e.norm);
      ctx.strokeStyle = `rgba(140,190,255,${fade})`;
      ctx.beginPath(); ctx.moveTo(x1, y1); ctx.lineTo(x2, y2); ctx.stroke();
    }
    ctx.restore();
  }

  /* stars */
  ctx.save();
  ctx.globalCompositeOperation = 'lighter';
  const t = now / 1000;
  for (let i = 0; i < nodes.length; i++) {
    const n = nodes[i];
    if (n.trashed) continue;
    const x = sx(n.x), y = sy(n.y);
    if (x < -40 || x > W + 40 || y < -40 || y > H + 40) continue;

    /* brightness: retrievability is the long decay, activation the recent heat */
    let act = Math.max(0.06, Math.min(1, 0.2 * n.glow + 0.8 * n.retr));
    if (thought.trace) {
      const arrive = thought.trace.arrival.get(i);
      if (arrive !== undefined && (REDUCED || hopNow >= arrive)) {
        const boost = arrive === 0
          ? (thought.trace.seedIdx.get(i) || 0.5)
          : (thought.trace.boost.get(i) || 0);
        act = Math.min(1, act + boost * (thought.done ? 0.75 : 1));
      } else if (thought.done) {
        act *= 0.3;   /* the unrecalled sky recedes once the ripple lands */
      }
    }
    if (focusSet) {
      const isNeighbor = adj[i].some(li => focusSet.has(li));
      if (i !== selectedIdx && !isNeighbor) act *= 0.22;
    }
    if (searchSet && !searchSet.has(i) && i !== selectedIdx) act *= 0.15;
    for (const p of pulses) {
      if (p.idx === i) act = Math.min(1, act + Math.max(0, 1 - (now - p.t0) / 8000) * 0.6);
    }
    if (lens === 'dream' && n.pruneCand && !REDUCED) {
      act *= 0.55 + 0.45 * Math.abs(Math.sin(t * 3.1 + n.twinkle));   /* guttering */
    }
    /* U6: activation_at_risk made visible — reviewed stars under the
       engine's 0.7 retrievability bar gutter at the edge of visibility */
    if (lens === 'health' && n.reviewed && n.retr < 0.7) {
      act *= REDUCED ? 0.5 : 0.4 + 0.45 * Math.abs(Math.sin(t * 2.6 + n.twinkle));
    }
    if (!REDUCED && S.twinkle) act *= 0.93 + 0.07 * Math.sin(t * 1.7 + n.twinkle);

    const layerBoost = n.layer === 'working' ? 1.25 : 1;
    const size = (3.2 + n.salience * 9) * view.k * layerBoost;
    const sprite = (lens === 'health' && n.degree === 0) ? sprites.risk
                 : (sprites[n.type] || sprites.fallback);
    ctx.globalAlpha = Math.min(1, 0.12 + act * 0.88);
    ctx.drawImage(sprite, x - size, y - size, size * 2, size * 2);

    ctx.globalAlpha = Math.min(1, 0.35 + act * 0.6);
    ctx.fillStyle = '#dfe9f8';
    ctx.fillRect(x - 0.7, y - 0.7, 1.4, 1.4);
  }
  ctx.restore();

  /* live pulses: an expanding ring + lingering warmth per audit event */
  if (pulses.length) {
    ctx.save();
    for (const p of pulses) {
      const age = (now - p.t0) / 1000;
      if (age > 1.4 || REDUCED) continue;
      const n = nodes[p.idx];
      const x = sx(n.x), y = sy(n.y);
      if (x < -60 || x > W + 60 || y < -60 || y > H + 60) continue;
      const ease = 1 - Math.pow(1 - age / 1.4, 2);
      ctx.strokeStyle = p.color + Math.round(200 * (1 - ease)).toString(16).padStart(2, '0');
      ctx.lineWidth = 1.6;
      ctx.beginPath();
      ctx.arc(x, y, 6 + ease * 46 * Math.max(view.k, 0.5), 0, 7);
      ctx.stroke();
    }
    ctx.restore();
  }

  /* isolated rings (health lens) */
  if (lens === 'health') {
    ctx.strokeStyle = 'rgba(224,82,82,0.5)';
    ctx.lineWidth = 1;
    for (let i = 0; i < nodes.length; i++) {
      if (nodes[i].degree !== 0) continue;
      const x = sx(nodes[i].x), y = sy(nodes[i].y);
      if (x < -20 || x > W + 20 || y < -20 || y > H + 20) continue;
      ctx.beginPath(); ctx.arc(x, y, 9 * view.k + 4, 0, 7); ctx.stroke();
    }
    /* U6: island rings — linked, but marooned off the main component */
    ctx.strokeStyle = 'rgba(224,168,82,0.55)';
    for (const i of islandSet) {
      const x = sx(nodes[i].x), y = sy(nodes[i].y);
      if (x < -20 || x > W + 20 || y < -20 || y > H + 20) continue;
      ctx.beginPath(); ctx.arc(x, y, 9 * view.k + 4, 0, 7); ctx.stroke();
    }
  }

  /* Dream lens overlay (U4): the exo-evolution loop, watchable */
  if (lens === 'dream') {
    /* embers: trashed memories at their old places on the map */
    ctx.fillStyle = 'rgba(224,82,82,0.35)';
    for (const em of dream.embers) {
      const x = sx(em.x), y = sy(em.y);
      if (x < -10 || x > W + 10 || y < -10 || y > H + 10) continue;
      ctx.beginPath(); ctx.arc(x, y, 2.2, 0, 7); ctx.fill();
    }
    /* marker rings on living stars */
    for (let i = 0; i < nodes.length; i++) {
      const n = nodes[i];
      if (n.trashed || !(n.champion || n.mutant || n.dreamBorn)) continue;
      const x = sx(n.x), y = sy(n.y);
      if (x < -30 || x > W + 30 || y < -30 || y > H + 30) continue;
      const r = (3.2 + n.salience * 9) * view.k + 5;
      if (n.champion) {          /* the crowned: a golden halo */
        ctx.strokeStyle = 'rgba(232,193,90,0.85)';
        ctx.lineWidth = 1.6;
        ctx.beginPath(); ctx.arc(x, y, r + 2, 0, 7); ctx.stroke();
      }
      if (n.mutant) {            /* variation offspring: violet dashes */
        ctx.strokeStyle = 'rgba(143,110,224,0.8)';
        ctx.lineWidth = 1.2;
        ctx.setLineDash([3, 3]);
        ctx.beginPath(); ctx.arc(x, y, r, 0, 7); ctx.stroke();
        ctx.setLineDash([]);
      }
      if (n.dreamBorn) {         /* born in a dream: a rose ring */
        ctx.strokeStyle = 'rgba(207,93,150,0.7)';
        ctx.lineWidth = 1.1;
        ctx.beginPath(); ctx.arc(x, y, r - 2, 0, 7); ctx.stroke();
      }
    }
  }

  /* hover + selection rings */
  for (const [idx, alpha] of [[hoverIdx, 0.85], [selectedIdx, 1.0]]) {
    if (idx < 0) continue;
    const n = nodes[idx];
    ctx.strokeStyle = `rgba(216,225,239,${alpha})`;
    ctx.lineWidth = idx === selectedIdx ? 1.6 : 1.2;
    ctx.beginPath();
    ctx.arc(sx(n.x), sy(n.y), (3.2 + n.salience * 9) * view.k + 5, 0, 7);
    ctx.stroke();
  }
}
requestAnimationFrame(render);

/* ---------- memory card (pinned on select) ---------- */
const card = document.getElementById('card');
async function openCard(i) {
  const n = nodes[i];
  const color = TYPE_COLOR[n.type] || FALLBACK_COLOR;
  document.getElementById('card-chip').style.background = color;
  document.getElementById('card-type').textContent =
    n.type + (n.rim
      ? (n.embedded ? ' · rim (awaiting layout)' : ' · rim (no embedding)')
      : '');
  document.getElementById('card-id').textContent = n.id.slice(0, 18);
  document.getElementById('card-tags').innerHTML =
    n.tags.map(t => `<span class="tag"></span>`).join('');
  document.querySelectorAll('#card .tag').forEach((el, k) => { el.textContent = n.tags[k]; });
  setText('card-sal', n.salience.toFixed(2));
  setText('card-act', n.glow.toFixed(2));
  setText('card-ret', n.retr.toFixed(2));
  setText('card-acc', n.access);
  setText('card-layer', n.layer);
  setText('card-born', (n.created || '').slice(0, 10));
  /* index first (the head), then the full body */
  document.getElementById('card-content').textContent =
    n.head + (n.chars > n.head.length ? '…' : '');
  card.classList.add('open');
  if (n.chars > n.head.length) {
    try {
      const full = await api('/memory/' + encodeURIComponent(n.id) +
        (AGENT ? '?agent_id=' + encodeURIComponent(AGENT) : ''));
      if (selectedIdx === i) {
        document.getElementById('card-content').textContent = full.content;
      }
    } catch { /* head stays — honest partial */ }
  }
}
function deselect() {
  selectedIdx = -1;
  card.classList.remove('open');
  dirty = true;
}
document.getElementById('card-close').addEventListener('click', deselect);

function selectNode(i) {
  selectedIdx = i;
  openCard(i);
  const n = nodes[i];
  view.x += (n.x - view.x) * 0.35;
  view.y += (n.y - view.y) * 0.35;
  dirty = true;
}

/* ---------- interaction: pan / zoom / hover / click ---------- */
const tooltip = document.getElementById('tooltip');
let dragging = false, moved = false, px = 0, py = 0;
canvas.addEventListener('pointerdown', e => {
  dragging = true; moved = false; px = e.clientX; py = e.clientY;
  tooltip.style.display = 'none';
  canvas.classList.add('dragging'); canvas.setPointerCapture(e.pointerId);
});
canvas.addEventListener('pointermove', e => {
  if (dragging) {
    const dx = e.clientX - px, dy = e.clientY - py;
    if (Math.abs(dx) + Math.abs(dy) > 3) moved = true;
    view.x -= dx / view.k; view.y -= dy / view.k;
    px = e.clientX; py = e.clientY; dirty = true;
  } else {
    let best = -1, bestD = 196;
    for (let i = 0; i < nodes.length; i++) {
      if (nodes[i].trashed) continue;
      const dx = sx(nodes[i].x) - e.clientX, dy = sy(nodes[i].y) - e.clientY;
      const d = dx * dx + dy * dy;
      if (d < bestD) { bestD = d; best = i; }
    }
    if (best !== hoverIdx) {
      hoverIdx = best; dirty = true;
      canvas.style.cursor = best >= 0 ? 'pointer' : 'grab';
      if (best >= 0) {
        const n = nodes[best];
        tooltip.textContent = n.type + ' · ' + n.head.slice(0, 46) + (n.chars > 46 ? '…' : '');
        tooltip.style.display = 'block';
      } else {
        tooltip.style.display = 'none';
      }
    }
    if (best >= 0) {
      tooltip.style.left = (e.clientX + 14) + 'px';
      tooltip.style.top  = (e.clientY + 12) + 'px';
    }
  }
});
canvas.addEventListener('pointerup', () => {
  dragging = false; canvas.classList.remove('dragging');
  if (moved) return;
  /* link-drawing (U1b): an armed card completes on the next star click */
  if (linkArm !== null && hoverIdx >= 0 && hoverIdx !== linkArm) {
    completeLink(hoverIdx);
    return;
  }
  if (hoverIdx >= 0) selectNode(hoverIdx);
  else { deselect(); disarmLink(); }
});
canvas.addEventListener('wheel', e => {
  e.preventDefault();
  const k0 = view.k;
  view.k = Math.min(3.2, Math.max(0.18, view.k * (e.deltaY < 0 ? 1.12 : 0.89)));
  const mx = e.clientX - W / 2, my = e.clientY - H / 2;
  view.x += mx / k0 - mx / view.k;
  view.y += my / k0 - my / view.k;
  dirty = true;
}, { passive: false });
addEventListener('keydown', e => {
  if (e.key === 'Escape') { deselect(); clearSearch(); disarmLink(); }
});

/* ---------- recall: Atlas = highlight, Thought = animated real spread ---------- */
function fillResultsList(rows, getText, getType, getId) {
  const list = document.getElementById('results-list');
  list.innerHTML = '';
  for (const r of rows) {
    const idx = byId.get(getId(r));
    const li = document.createElement('li');
    const color = TYPE_COLOR[getType(r)] || FALLBACK_COLOR;
    li.innerHTML = `<span class="chip" style="background:${color}"></span>` +
      `<span class="head"></span><span class="score">${(r.score ?? 0).toFixed(2)}</span>`;
    li.querySelector('.head').textContent = getText(r);
    if (idx !== undefined) li.addEventListener('click', () => selectNode(idx));
    list.appendChild(li);
  }
}

async function runRecall() {
  const query = document.getElementById('query').value.trim();
  if (!query) return;
  if (lens === 'thought') return runThoughtRecall(query);

  let results;
  try {
    results = await api('/recall', {
      method: 'POST',
      body: JSON.stringify(Object.assign(
        { query, top_k: S.topK }, AGENT ? { agent_id: AGENT } : {},
      )),
    });
  } catch { return; }

  searchSet = new Set();
  for (const r of results) {
    const idx = byId.get(r.memory.id);
    if (idx !== undefined) searchSet.add(idx);
  }
  fillResultsList(results, r => r.memory.content, r => r.memory.memory_type, r => r.memory.id);
  document.getElementById('results-title').textContent = 'RECALLED';
  document.getElementById('results-replay').hidden = true;
  document.getElementById('results-meta').textContent =
    results.length + ' recalled · real pipeline (spread + rank)';
  deselect();                                       /* hides the card only */
  document.getElementById('results').classList.add('open');
  dirty = true;
}

async function runThoughtRecall(query) {
  let resp;
  try {
    resp = await api('/recall/trace', {
      method: 'POST',
      body: JSON.stringify(Object.assign(
        { query, top_k: S.topK }, AGENT ? { agent_id: AGENT } : {},
      )),
    });
  } catch { return; }

  const tr = resp.trace;
  /* index-space trace; events whose endpoints fell outside the export cap
     are dropped (they still happened — the meta line stays honest) */
  const seedIdx = new Map();
  for (const [id, sim] of tr.seeds) {
    const idx = byId.get(id);
    if (idx !== undefined) seedIdx.set(idx, sim);
  }
  let maxAmount = 1e-9, maxHop = 0;
  const events = [];
  for (const e of tr.events) {
    const a = byId.get(e.source), b = byId.get(e.target);
    if (a === undefined || b === undefined) continue;
    events.push({ hop: e.hop, a, b, amount: e.amount });
    maxAmount = Math.max(maxAmount, e.amount);
    maxHop = Math.max(maxHop, e.hop);
  }
  for (const e of events) e.norm = e.amount / maxAmount;
  const boost = new Map();
  for (const [id, act] of tr.activated) {
    const idx = byId.get(id);
    if (idx !== undefined) boost.set(idx, act);
  }
  /* arrival hop per node: seeds at 0, else the first event that reached it */
  const arrival = new Map([...seedIdx.keys()].map(i => [i, 0]));
  for (const e of events)
    if (!arrival.has(e.b) || e.hop < arrival.get(e.b)) arrival.set(e.b, e.hop);

  thought.trace = { seedIdx, events, boost, arrival, maxHop };
  thought.t0 = performance.now();
  thought.done = false;

  fillResultsList(resp.results, r => r.content_head, r => r.memory_type, r => r.id);
  document.getElementById('results-title').textContent = 'THOUGHT';
  document.getElementById('results-replay').hidden = false;
  document.getElementById('results-meta').textContent =
    tr.seeds.length + ' seeds · ' + tr.events.length + ' walks · ' +
    tr.activated.length + ' activated · reinforced';
  deselect();
  document.getElementById('results').classList.remove('open'); /* opens when the ripple lands */
  dirty = true;
}

function clearSearch() {
  searchSet = null;
  thought.trace = null; thought.done = false;
  document.getElementById('results').classList.remove('open');
  dirty = true;
}
document.getElementById('recall-btn').addEventListener('click', runRecall);
document.getElementById('query').addEventListener('keydown', e => {
  if (e.key === 'Enter') runRecall();
});
document.getElementById('results-clear').addEventListener('click', clearSearch);
document.getElementById('results-replay').addEventListener('click', () => {
  if (!thought.trace) return;
  thought.t0 = performance.now();
  thought.done = false;
  document.getElementById('results').classList.remove('open');
  dirty = true;
});

/* ---------- lenses ---------- */
const PLACARDS = {};   /* every lens is real now */

/* ---------- Live (U3): the audit-log EEG ---------- */
/* The stream connects at boot and stays up in every lens — a mutation
   ripples on the field wherever you are; the ticker panel is Live-only.
   Mutating MCP tool calls audit; reads deliberately do not. */
const pulses = [];                  // {idx, t0, color}
const born = { count: 0 };
let esLastEvent = null;

function liveDot(state) {
  const d = document.getElementById('live-dot');
  d.className = state;
  document.getElementById('live-meta').textContent =
    state === 'on'
      ? (esLastEvent ? 'streaming · last event ' + esLastEvent : 'streaming · waiting for events')
      : state === 'err' ? 'reconnecting…' : 'connecting…';
}

function addTickerRow(ev) {
  const list = document.getElementById('live-list');
  const li = document.createElement('li');
  const idx = ev.memory_id ? byId.get(ev.memory_id) : undefined;
  const what = idx !== undefined ? nodes[idx].head
    : ev.memory_id ? ev.memory_id.slice(0, 10) + '…'
    : (ev.details || '—');
  li.innerHTML = `<span class="ts"></span><span class="agent"></span>` +
    `<span class="action"></span><span class="what"></span>`;
  li.querySelector('.ts').textContent = (ev.timestamp || '').slice(11, 19);
  li.querySelector('.agent').textContent = ev.agent_id || '·';
  li.querySelector('.action').textContent = ev.action;
  li.querySelector('.what').textContent = what;
  if (idx !== undefined) {
    li.style.cursor = 'pointer';
    li.addEventListener('click', () => selectNode(idx));
  }
  list.prepend(li);
  while (list.children.length > S.tickerDepth) list.removeChild(list.lastChild);
}

const BIRTH_ACTIONS = new Set([
  'remember', 'memory_store', 'session_save', 'store_procedure',
  'store_intention', 'create_schema', 'send_message', 'ingest_file',
  'describe_image', 'episode_start',
]);

function onAuditRows(rows) {
  for (const ev of rows) {
    if (ev.id <= lastAuditId) continue;   /* replay/stream overlap guard */
    lastAuditId = ev.id;
    esLastEvent = (ev.timestamp || '').slice(11, 19);
    if (S.pauseStream) continue;   /* paused: events drop, honestly */
    if (AGENT && ev.agent_id && ev.agent_id !== AGENT) continue;
    addTickerRow(ev);
    const idx = ev.memory_id ? byId.get(ev.memory_id) : undefined;
    if (idx !== undefined) {
      const color = ev.action.startsWith('purge') || ev.action.startsWith('delete')
        ? RISK : (TYPE_COLOR[nodes[idx].type] || FALLBACK_COLOR);
      pulses.push({ idx, t0: performance.now(), color });
      if (REDUCED) setTimeout(() => { dirty = true; }, 2200);
    } else if (BIRTH_ACTIONS.has(ev.action) && ev.memory_id) {
      born.count++;
      const el = document.getElementById('live-new');
      el.hidden = false;
      el.innerHTML = '';
      el.append(born.count + ' memor' + (born.count === 1 ? 'y' : 'ies') +
        ' born since load — ');
      const btn = document.createElement('button');
      btn.textContent = 'RELOAD FIELD';
      btn.addEventListener('click', () => location.reload());
      el.append(btn);
    }
  }
  liveDot('on');
  dirty = true;
}

let lastAuditId = 0;   /* replay/stream dedupe cursor */

async function connectEvents() {
  /* ?since=<audit rowid>: replay history from that cursor via plain JSON
     first ("what happened while I was away"), THEN go live. ?es=off skips
     the stream entirely (captures, demos — a pause control is U1b material). */
  const since = urlParams.get('since');
  if (since) {
    try {
      const replay = await api('/audit/since/' + encodeURIComponent(since));
      onAuditRows(replay.rows);
    } catch { /* replay is best-effort */ }
  }
  if (urlParams.get('es') === 'off') { liveDot(''); return; }

  const es = new EventSource('/events' + (TOKEN ? '?token=' + encodeURIComponent(TOKEN) : ''));
  es.addEventListener('audit', e => {
    try { onAuditRows(JSON.parse(e.data)); } catch { /* one bad frame ≠ a dead EEG */ }
  });
  es.addEventListener('open', () => liveDot('on'));
  es.addEventListener('error', () => liveDot('err'));  /* EventSource auto-reconnects */
}
const placard = document.getElementById('placard');
function setLens(name) {
  if (lens === 'health' && name !== 'health') resetProjection();   /* U6 */
  lens = name;
  document.body.dataset.lens = name;
  document.querySelectorAll('#lenses button').forEach(b =>
    b.setAttribute('aria-pressed', String(b.dataset.lens === name)));
  document.getElementById('query').placeholder =
    name === 'thought' ? 'think…  (a traced recall — you will watch it spread)' : 'recall…';
  if (name !== 'thought' && thought.trace) clearSearch();
  if (name === 'dream') loadDreams();
  const pl = PLACARDS[name];
  if (pl) {
    document.getElementById('placard-phase').textContent = 'CHARTER · ' + pl[0];
    document.getElementById('placard-title').textContent = pl[1];
    document.getElementById('placard-body').textContent = pl[2];
    placard.classList.add('open');
  } else {
    placard.classList.remove('open');
  }
  dirty = true;
}
document.querySelectorAll('#lenses button').forEach(b =>
  b.addEventListener('click', () => setLens(b.dataset.lens)));
placard.addEventListener('click', () => setLens('atlas'));

/* ============================================================
   U1b — the instruments: settings drawer, compose, edit/trash,
   link-drawing, trash browser. All over existing routes.
   ============================================================ */

/* ---------- settings drawer ---------- */
const settingsEl = document.getElementById('settings');
function bindSettings() {
  const el = id => document.getElementById(id);
  const showVals = () => {
    el('s-hop-val').textContent = S.hopMs + 'ms';
    el('s-topk-val').textContent = S.topK;
    el('s-ticker-val').textContent = S.tickerDepth;
  };
  el('s-twinkle').checked = S.twinkle;
  el('s-hop').value = S.hopMs;
  el('s-density').value = String(S.density);
  el('s-topk').value = S.topK;
  el('s-lens').value = S.defaultLens;
  el('s-ticker').value = S.tickerDepth;
  el('s-pause').checked = S.pauseStream;
  showVals();
  el('s-twinkle').addEventListener('change', e => { S.twinkle = e.target.checked; saveSettings(); });
  el('s-hop').addEventListener('input', e => { S.hopMs = +e.target.value; showVals(); saveSettings(); });
  el('s-density').addEventListener('change', e => { S.density = +e.target.value; saveSettings(); });
  el('s-topk').addEventListener('input', e => { S.topK = +e.target.value; showVals(); saveSettings(); });
  el('s-lens').addEventListener('change', e => { S.defaultLens = e.target.value; saveSettings(); });
  el('s-ticker').addEventListener('input', e => { S.tickerDepth = +e.target.value; showVals(); saveSettings(); });
  el('s-pause').addEventListener('change', e => {
    S.pauseStream = e.target.checked; saveSettings();
    liveDot(S.pauseStream ? '' : 'on');
  });
  el('s-scope').textContent = AGENT || 'ALL';
  el('s-relayout').addEventListener('click', async () => {
    el('s-relayout').textContent = 'RECOMPUTING…';
    try { await api('/graph/layout', { method: 'POST' }); } catch { /* shown on reload */ }
    location.reload();
  });
  el('s-reload').addEventListener('click', () => location.reload());
  el('s-trash-refresh').addEventListener('click', refreshTrash);
}
document.getElementById('settings-btn').addEventListener('click', () => {
  const opening = !settingsEl.classList.contains('open');
  settingsEl.classList.toggle('open');
  if (opening) refreshTrash();
});

async function refreshTrash() {
  const list = document.getElementById('s-trash-list');
  let rows;
  try { rows = await api('/trash?limit=30'); } catch { return; }
  list.innerHTML = '';
  if (!rows.length) {
    list.innerHTML = '<div class="trow"><span class="head">trash is empty</span></div>';
    return;
  }
  for (const m of rows) {
    const div = document.createElement('div');
    div.className = 'trow';
    div.innerHTML = '<span class="head"></span><button>RESTORE</button><button class="danger">PURGE</button>';
    div.querySelector('.head').textContent = (m.content || '').slice(0, 48);
    const [restoreBtn, purgeBtn] = div.querySelectorAll('button');
    restoreBtn.addEventListener('click', async () => {
      try { await api('/trash/' + encodeURIComponent(m.id) + '/restore', { method: 'POST' }); } catch { return; }
      refreshTrash();
    });
    purgeBtn.addEventListener('click', async () => {
      if (!confirm('Purge permanently? This is irreversible.')) return;
      try { await api('/trash/' + encodeURIComponent(m.id), { method: 'DELETE' }); } catch { return; }
      refreshTrash();
    });
    list.appendChild(div);
  }
}

/* ---------- compose: a new memory through the real pipeline ---------- */
const composeEl = document.getElementById('compose');
document.getElementById('compose-btn').addEventListener('click', () =>
  composeEl.classList.toggle('open'));
document.getElementById('compose-close').addEventListener('click', () =>
  composeEl.classList.remove('open'));
document.getElementById('c-store').addEventListener('click', async () => {
  const status = document.getElementById('c-status');
  const content = document.getElementById('c-content').value.trim();
  const type = document.getElementById('c-type').value;
  const tags = document.getElementById('c-tags').value
    .split(',').map(s => s.trim()).filter(Boolean);
  const sal = document.getElementById('c-salience').value;
  const body = Object.assign(
    { content, visibility: document.getElementById('c-vis').value },
    type ? { memory_type: type } : {},
    tags.length ? { tags } : {},
    sal !== '' ? { salience: +sal } : {},
    AGENT ? { agent_id: AGENT } : {},
  );
  status.className = ''; status.textContent = 'storing…';
  try {
    const node = await api('/remember', { method: 'POST', body: JSON.stringify(body) });
    status.className = 'ok';
    status.textContent = 'stored ' + node.id.slice(0, 8) + ' — reload to place it';
    document.getElementById('c-content').value = '';
  } catch (e) {
    /* the thalamus gate speaks honestly (too short / over cap / dup policy) */
    status.className = 'err';
    status.textContent = e.message.includes('→ 500') ? 'rejected — see server reason (likely thalamus gate)' : e.message;
  }
});

/* ---------- card actions: edit / trash / link-draw ---------- */
let linkArm = null;   /* node index armed as link source, or null */
const editForm = document.getElementById('card-editform');

function disarmLink() {
  linkArm = null;
  document.getElementById('card-link').setAttribute('aria-pressed', 'false');
}

document.getElementById('card-edit').addEventListener('click', () => {
  if (selectedIdx < 0) return;
  const n = nodes[selectedIdx];
  editForm.hidden = !editForm.hidden;
  if (!editForm.hidden) {
    document.getElementById('e-content').value =
      document.getElementById('card-content').textContent;
    document.getElementById('e-tags').value = n.tags.join(', ');
    document.getElementById('e-salience').value = n.salience.toFixed(2);
    document.getElementById('e-status').textContent = '';
  }
});
document.getElementById('e-cancel').addEventListener('click', () => { editForm.hidden = true; });
document.getElementById('e-save').addEventListener('click', async () => {
  if (selectedIdx < 0) return;
  const n = nodes[selectedIdx];
  const status = document.getElementById('e-status');
  const body = {
    content:  document.getElementById('e-content').value,
    tags:     document.getElementById('e-tags').value.split(',').map(s => s.trim()).filter(Boolean),
    salience: +document.getElementById('e-salience').value,
    visibility: document.getElementById('e-vis').value,
  };
  status.className = ''; status.textContent = 'saving…';
  try {
    const updated = await api('/memory/' + encodeURIComponent(n.id) +
      (AGENT ? '?agent_id=' + encodeURIComponent(AGENT) : ''),
      { method: 'PUT', body: JSON.stringify(body) });
    n.tags = updated.tags || body.tags;
    n.salience = body.salience;
    n.head = body.content.slice(0, 200);
    n.chars = body.content.length;
    status.className = 'ok'; status.textContent = 'saved (version snapshotted)';
    editForm.hidden = true;
    openCard(selectedIdx);
    dirty = true;
  } catch (e) {
    status.className = 'err'; status.textContent = e.message;
  }
});

document.getElementById('card-trash').addEventListener('click', async () => {
  if (selectedIdx < 0) return;
  const n = nodes[selectedIdx];
  try {
    await api('/memory/' + encodeURIComponent(n.id), { method: 'DELETE' });
  } catch { return; }
  n.trashed = true;             /* restorable from the settings drawer */
  deselect(); disarmLink();
  dirty = true;
});

document.getElementById('card-link').addEventListener('click', () => {
  if (selectedIdx < 0) return;
  if (linkArm !== null) { disarmLink(); return; }
  linkArm = selectedIdx;
  document.getElementById('card-link').setAttribute('aria-pressed', 'true');
});

async function completeLink(targetIdx) {
  const srcIdx = linkArm;
  disarmLink();
  const linkType = document.getElementById('card-linktype').value;
  try {
    await api('/associate', {
      method: 'POST',
      body: JSON.stringify({
        source_id: nodes[srcIdx].id,
        target_id: nodes[targetIdx].id,
        link_type: linkType,
        weight: 0.5,
      }),
    });
  } catch { return; }
  /* mirror locally: the new strand appears without a reload */
  const idx = edges.length;
  edges.push({ a: srcIdx, b: targetIdx, weight: 0.5, eff: 0.5, cold: true });
  adj[srcIdx].push(idx); adj[targetIdx].push(idx);
  nodes[srcIdx].degree++; nodes[targetIdx].degree++;
  edgeRank = edges.map((_, i) => i).sort((i, j) => edges[j].eff - edges[i].eff);
  pulses.push({ idx: targetIdx, t0: performance.now(),
    color: TYPE_COLOR[nodes[targetIdx].type] || FALLBACK_COLOR });
  selectNode(targetIdx);
  dirty = true;
}

/* ---------- Dream observatory (U4) ---------- */
async function loadDreams() {
  let resp, trash;
  try {
    [resp, trash] = await Promise.all([
      api('/dream/reports?limit=30'),
      api('/trash?limit=100'),
    ]);
  } catch { return; }
  dream.reports = resp.reports || [];
  /* embers: trashed memories that still have a place on the semantic map */
  dream.embers = (trash || [])
    .filter(m => layoutCoords[m.id])
    .map(m => ({
      x: layoutCoords[m.id][0] * 850, y: layoutCoords[m.id][1] * 850,
      head: (m.content || '').slice(0, 40),
    }));
  renderDreamTimeline();
  if (dream.reports.length && dream.selected < 0) selectCycle(0);
  document.getElementById('dream-meta').textContent = dream.reports.length
    ? dream.reports.length + ' cycle(s) · ' + dream.embers.length + ' ember(s) in the trash'
    : 'no cycles recorded yet — DREAM NOW runs one';
  dirty = true;
}

function renderDreamTimeline() {
  const tl = document.getElementById('dream-timeline');
  tl.innerHTML = '';
  dream.reports.forEach((r, i) => {
    const div = document.createElement('div');
    div.className = 'cycle';
    div.setAttribute('aria-selected', String(i === dream.selected));
    const phases = Array.isArray(r.phases) ? r.phases : [];
    const sums = phases.reduce((a, p) => ({
      pruned:  a.pruned  + (p.memories_pruned || 0),
      schemas: a.schemas + (p.schemas_extracted || 0) + (p.skills_distilled || 0),
      links:   a.links   + (p.links_created || 0) + (p.links_strengthened || 0),
    }), { pruned: 0, schemas: 0, links: 0 });
    div.innerHTML = '<span class="when"></span><span class="who"></span><span class="sum"></span>';
    div.querySelector('.when').textContent =
      (r.started_at || '').slice(5, 16).replace('T', ' ');
    div.querySelector('.who').textContent = r.agent_id || '·';
    div.querySelector('.sum').textContent =
      `${phases.length} phases · ${sums.links} links · ${sums.schemas} schemas · ${sums.pruned} pruned`;
    div.addEventListener('click', () => selectCycle(i));
    tl.appendChild(div);
  });
}

/* Compact honest effect line per phase; zero-effect LLM-skip phases dim. */
function phaseEffects(p) {
  const parts = [];
  const add = (n, label) => { if (n) parts.push(n + ' ' + label); };
  add(p.memories_processed, 'processed');
  add(p.links_created, 'links+');
  add(p.links_strengthened, 'strengthened');
  add(p.schemas_extracted, 'schemas');
  add(p.skills_distilled, 'skills');
  add(p.procedures_extracted, 'procedures');
  add(p.procedures_rediscovered, 'rediscovered');
  add(p.procedures_mutated, 'mutated');
  add(p.procedures_merged, 'merged');
  add(p.niches_contested, 'niches');
  add(p.champions_marked, 'champions');
  add(p.procedures_demoted, 'demoted');
  add(p.memories_pruned, 'pruned');
  add(p.episodes_consolidated, 'episodes');
  add(p.llm_calls, 'LLM calls');
  if (!parts.length) return p.success ? 'no effect (likely LLM-skipped)' : 'failed';
  return parts.join(' · ');
}

function selectCycle(i) {
  dream.selected = i;
  renderDreamTimeline();
  const anatomy = document.getElementById('dream-anatomy');
  anatomy.innerHTML = '';
  const r = dream.reports[i];
  if (!r) return;
  for (const p of (Array.isArray(r.phases) ? r.phases : [])) {
    const div = document.createElement('div');
    const fx = phaseEffects(p);
    div.className = 'ph' + (fx.startsWith('no effect') ? ' skipped' : '');
    div.innerHTML = '<span class="pname"></span><span class="pfx"></span>';
    div.querySelector('.pname').textContent = p.phase;
    div.querySelector('.pfx').textContent = fx;
    anatomy.appendChild(div);
  }
}

document.getElementById('dream-now').addEventListener('click', async () => {
  if (!confirm('Run a real consolidation cycle? The brain will change — links strengthen, schemas may form, prune candidates may retire.')) return;
  const status = document.getElementById('dream-status');
  status.className = ''; status.textContent = 'dreaming…';
  try {
    const report = await api('/dream/run' +
      (AGENT ? '?agent_id=' + encodeURIComponent(AGENT) : ''), { method: 'POST' });
    status.className = 'ok';
    status.textContent = 'cycle complete (' + (report.phases || []).length +
      ' phases) — reload the field to see the new sky';
  } catch (e) {
    status.className = 'err'; status.textContent = e.message;
  }
  dream.selected = -1;
  loadDreams();
});

/* shareable boot params: ?lens=health opens a lens, ?q=… runs a recall */
boot().then(() => {
  bindSettings();
  api('/meta').then(m => {
    const db = document.getElementById('s-db');
    db.textContent = (m.db_path || 'unknown').split('/').slice(-3).join('/');
    db.title = m.db_path || '';
    document.getElementById('s-version').textContent = m.version || '';
    document.getElementById('wordmark').title = 'brain: ' + (m.db_path || 'unknown');
  }).catch(() => {});
  connectEvents();               /* the EEG runs in every lens */
  const lensParam = urlParams.get('lens');
  if (lensParam && document.querySelector(`#lenses [data-lens="${lensParam}"]`))
    setLens(lensParam);
  else if (S.defaultLens !== 'atlas')
    setLens(S.defaultLens);
  const q = urlParams.get('q');
  if (q) {
    document.getElementById('query').value = q;
    runRecall();
  }
  /* ?open=settings|compose — deep-link a drawer (docs, demos, captures) */
  const open = urlParams.get('open');
  if (open === 'settings') { settingsEl.classList.add('open'); refreshTrash(); }
  if (open === 'compose') composeEl.classList.add('open');
  /* ?proj=<days> — deep-link a time-lapse projection (U6; docs, captures) */
  const proj = parseInt(urlParams.get('proj') || '0', 10);
  if (proj > 0) {
    setLens('health');
    if (projSlider) projSlider.value = Math.min(proj, 365);
    applyProjection(Math.min(proj, 365));
  }
});
