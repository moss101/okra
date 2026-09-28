/* okra workbench — browser surface over the daemon's v4 seam.
 *
 * Transport: GET /sse/<session> streams FULL projection snapshots
 * (rows + control); POST /command drives turns (sendText / createSession /
 * stop); POST /steer queues steering. GET /api/sessions lists the task
 * index; GET /api/sessions/<id>/rows replays a session from the durable
 * kernel log.
 *
 * Terminology (UI-SHELL-PLAN §3.3): user-facing = Task; the daemon's
 * "session" never appears in copy.
 */
'use strict';

/* ---------- tiny dom helpers ---------- */

const $ = (id) => document.getElementById(id);

function el(tag, cls, text) {
  const n = document.createElement(tag);
  if (cls) n.className = cls;
  if (text !== undefined) n.textContent = text;
  return n;
}

function escapeHtml(s) {
  return String(s).replace(/[&<>"']/g, (c) => ({
    '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;',
  }[c]));
}

/* ---------- safe markdown (escape first, tags generated only here) ---------- */

function renderMarkdown(src) {
  const fences = [];
  // pull fenced code blocks out first so nothing else touches them
  let text = String(src).replace(/```(\w*)\n([\s\S]*?)```/g, (_, lang, code) => {
    fences.push({ lang, code });
    return '\u0000FENCE' + (fences.length - 1) + '\u0000';
  });

  text = escapeHtml(text);

  const renderInline = (s) => s
    .replace(/`([^`]+)`/g, '<code>$1</code>')
    .replace(/\*\*([^*]+)\*\*/g, '<strong>$1</strong>')
    .replace(/(^|[\s(])\*([^*\n]+)\*/g, '$1<em>$2</em>')
    .replace(/\[([^\]]+)\]\((https?:\/\/[^)\s]+)\)/g,
      '<a href="$2" target="_blank" rel="noopener noreferrer">$1</a>');

  const lines = text.split('\n');
  let out = '';
  let list = null; // 'ul' | 'ol'
  let para = [];

  const flushPara = () => {
    if (para.length) {
      out += '<p>' + para.map(renderInline).join('<br>') + '</p>';
      para = [];
    }
  };
  const flushList = () => {
    if (list) { out += '</' + list + '>'; list = null; }
  };

  for (const raw of lines) {
    const line = raw.trimEnd();
    const h = /^(#{1,4})\s+(.*)$/.exec(line);
    const ul = /^[-*]\s+(.*)$/.exec(line.trim());
    const ol = /^\d+[.)]\s+(.*)$/.exec(line.trim());
    if (h) {
      flushPara(); flushList();
      out += `<h${h[1].length}>` + renderInline(h[2]) + `</h${h[1].length}>`;
    } else if (ul) {
      flushPara();
      if (list !== 'ul') { flushList(); out += '<ul>'; list = 'ul'; }
      out += '<li>' + renderInline(ul[1]) + '</li>';
    } else if (ol) {
      flushPara();
      if (list !== 'ol') { flushList(); out += '<ol>'; list = 'ol'; }
      out += '<li>' + renderInline(ol[1]) + '</li>';
    } else if (line.trim() === '') {
      flushPara(); flushList();
    } else {
      para.push(line);
    }
  }
  flushPara(); flushList();

  // restore fenced blocks
  out = out.replace(/\u0000FENCE(\d+)\u0000/g, (_, i) => {
    const f = fences[Number(i)];
    return '<pre><code>' + escapeHtml(f.code) + '</code></pre>';
  });
  return out;
}

/* ---------- formatting ---------- */

function relTime(ts) {
  if (!ts) return '';
  const s = Math.max(0, (Date.now() - ts) / 1000);
  if (s < 60) return 'just now';
  if (s < 3600) return Math.floor(s / 60) + 'm ago';
  if (s < 86400) return Math.floor(s / 3600) + 'h ago';
  return Math.floor(s / 86400) + 'd ago';
}

function fmtDuration(ms) {
  if (!ms || ms < 0) return '';
  if (ms < 1000) return Math.round(ms) + 'ms';
  if (ms < 60000) return (ms / 1000).toFixed(1) + 's';
  return Math.floor(ms / 60000) + 'm ' + Math.round((ms % 60000) / 1000) + 's';
}

const PHASE_LABEL = {
  draft: 'ready',
  running: 'working…',
  completedSuccess: 'completed',
  completedInterrupted: 'interrupted',
  error: 'error',
  replayed: 'history',
};

/* ---------- state ---------- */

const state = {
  sessions: [],            // [{id,title,status,eventCount,live}]
  activeId: null,
  rows: [],                // latest full snapshot for the active task
  control: { phase: 'draft' },
  es: null,                // EventSource for the active task
  connected: false,
  health: {},
  pendingNewTask: false,   // composer has text but the task doesn't exist yet
};

/* ---------- api ---------- */

function post(path, body) {
  return fetch(path, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify(body),
  }).then((r) => r.json());
}

async function loadHealth() {
  try {
    state.health = await fetch('/health').then((r) => r.json());
    $('brand-version').textContent = 'v' + (state.health.version || '');
    $('topbar-cwd').textContent = state.health.cwd || '';
    $('topbar-cwd').title = state.health.cwd || '';
    $('footer-sampler').textContent = state.health.sampler || '';
    // daemon reachable, but no task stream until a task is open
    if (!state.es) {
      $('conn-label').textContent = 'ready';
    }
  } catch (_) { /* offline; conn dot handles signaling */ }
}

async function loadSessions() {
  try {
    const data = await fetch('/api/sessions').then((r) => r.json());
    state.sessions = data.sessions || [];
    renderTaskList();
  } catch (_) { /* ignore */ }
}

async function replaySession(id) {
  try {
    const data = await fetch('/api/sessions/' + encodeURIComponent(id) + '/rows').then((r) => r.json());
    if (data.error) return false;
    state.rows = data.rows || [];
    state.control = data.control || { phase: 'replayed' };
    renderRows();
    return true;
  } catch (_) {
    return false;
  }
}

/* ---------- sse ---------- */

function connectSSE(id) {
  if (state.es) { state.es.close(); state.es = null; }
  if (!id) { updateConn(false); return; }
  const es = new EventSource('/sse/' + encodeURIComponent(id));
  state.es = es;
  es.onopen = () => updateConn(true);
  es.onerror = () => updateConn(false); // EventSource retries on its own
  es.onmessage = (e) => {
    updateConn(true);
    let msg;
    try { msg = JSON.parse(e.data); } catch (_) { return; }
    const p = msg && msg.params;
    if (!p || p.sessionId !== state.activeId) return;
    // frames are FULL snapshots — replace wholesale
    state.rows = p.rows || [];
    state.control = p.control || state.control;
    renderRows();
  };
}

function updateConn(on) {
  state.connected = on;
  const dot = $('conn-dot');
  dot.classList.toggle('on', on);
  dot.classList.toggle('off', !on && Boolean(state.es));
  if (on) {
    $('conn-label').textContent = 'connected';
  } else if (!state.es) {
    $('conn-label').textContent = state.health.daemon ? 'ready' : 'connecting';
  } else {
    $('conn-label').textContent = 'reconnecting…';
  }
}

/* ---------- selection ---------- */

async function selectSession(id) {
  state.activeId = id;
  state.pendingNewTask = false;
  renderTaskList();
  $('topbar-title').textContent = titleOf(id);
  // replay first (instant history), then live frames take over wholesale
  await replaySession(id);
  renderRows();
  connectSSE(id);
  refreshComposerMode();
}

function titleOf(id) {
  const s = state.sessions.find((x) => x.id === id);
  if (s && s.title) return s.title;
  return 'Task ' + id.replace(/^web-/, '');
}

function newTask() {
  state.activeId = null;
  state.pendingNewTask = true;
  state.rows = [];
  state.control = { phase: 'draft' };
  $('topbar-title').textContent = 'New task';
  renderTaskList();
  renderRows(); // shows the empty state
  if (state.es) { state.es.close(); state.es = null; }
  $('composer-input').focus();
  refreshComposerMode();
}

/* ---------- sending / steering / stop ---------- */

async function send() {
  const input = $('composer-input');
  const text = input.value.trim();
  if (!text) return;

  let id = state.activeId;
  const isNew = !id;
  if (isNew) {
    id = 'web-' + Math.random().toString(36).slice(2, 8);
    state.activeId = id;
    state.pendingNewTask = false;
    state.rows = [];
    $('topbar-title').textContent = text.slice(0, 80);
    connectSSE(id);
  }
  input.value = '';
  autosize();
  refreshComposerMode();

  try {
    await post('/command', {
      commandId: 'web-' + Date.now(),
      type: isNew ? 'createSession' : 'sendText',
      sessionId: id,
      payload: { text },
    });
  } catch (e) {
    toast('error', 'Could not send', String(e));
  }
  loadSessions();
}

async function stop() {
  if (!state.activeId) return;
  try {
    await post('/command', {
      commandId: 'web-stop-' + Date.now(),
      type: 'stop',
      sessionId: state.activeId,
    });
  } catch (_) { /* ignore */ }
}

/* ---------- rendering: task list ---------- */

function renderTaskList() {
  const host = $('task-list');
  host.textContent = '';
  if (!state.sessions.length) {
    host.appendChild(el('div', 'task-list-empty', 'No tasks yet — start one below.'));
    return;
  }
  for (const s of state.sessions) {
    const item = el('button', 'task-item' + (s.id === state.activeId ? ' active' : ''));
    item.type = 'button';
    item.appendChild(el('span', 'task-item-title', s.title || 'Task ' + s.id));
    const meta = el('span', 'task-item-meta');
    const dot = el('span', 'status-dot ' + (s.status || ''));
    meta.appendChild(dot);
    meta.appendChild(el('span', null, (s.live ? 'live · ' : '') + (s.eventCount != null ? s.eventCount + ' events' : '')));
    item.appendChild(meta);
    item.addEventListener('click', () => selectSession(s.id));
    host.appendChild(item);
  }
}

/* ---------- rendering: transcript rows ---------- */

const TOOL_GLYPHS = {
  read_file: 'M4 2h5l3 3v9H4V2Zm5 0v3h3',
  list_dir: 'M2 4h4l1.5 2H14v7H2V4Z',
  write_file: 'M3 13h10M8 2l4 4-6 6H3v-3l5-5Z',
  edit_file: 'M2 12l8-8 3 3-8 8H2v-3ZM9 5l3 3',
};

function toolGlyphSvg(name) {
  const svg = document.createElementNS('http://www.w3.org/2000/svg', 'svg');
  svg.setAttribute('viewBox', '0 0 16 16');
  svg.setAttribute('width', '13');
  svg.setAttribute('height', '13');
  const path = document.createElementNS('http://www.w3.org/2000/svg', 'path');
  path.setAttribute('d', TOOL_GLYPHS[name] || 'M8 2a6 6 0 1 0 0 12A6 6 0 0 0 8 2Z');
  path.setAttribute('fill', 'none');
  path.setAttribute('stroke', 'currentColor');
  path.setAttribute('stroke-width', '1.4');
  path.setAttribute('stroke-linejoin', 'round');
  svg.appendChild(path);
  return svg;
}

function renderRows() {
  const host = $('rows');
  const transcript = $('transcript');
  const pinned = transcript.scrollHeight - transcript.scrollTop - transcript.clientHeight < 90;

  // empty state vs rows
  const empty = $('empty-state');
  const showEmpty = !state.rows.length && !state.pendingNewTask && state.control.phase !== 'running';
  empty.style.display = showEmpty ? '' : 'none';
  if (showEmpty) { host.textContent = ''; refreshComposerMode(); return; }

  host.textContent = '';
  for (const r of state.rows) {
    const div = el('div', 'row row-' + r.kind);
    switch (r.kind) {
      case 'turnHeader': {
        div.classList.add('row-turn');
        div.appendChild(el('span', 't-state-' + (r.state || ''), 'turn · ' + (r.state || '').replace('completed', '')));
        break;
      }
      case 'userInput': {
        div.classList.add('row-user');
        const bubble = el('div', 'user-bubble');
        let text = r.text || '';
        if (text.startsWith('[steered] ')) {
          const chip = el('span', 'steered-chip', 'steered');
          bubble.appendChild(chip);
          text = text.slice('[steered] '.length);
        }
        bubble.appendChild(document.createTextNode(text));
        div.appendChild(bubble);
        break;
      }
      case 'assistantText': {
        div.classList.add('row-assistant', 'state-' + (r.state || 'complete'));
        const md = el('div', 'md');
        md.innerHTML = renderMarkdown(r.text || '');
        div.appendChild(md);
        break;
      }
      case 'toolCall': {
        div.appendChild(renderToolCard(r));
        break;
      }
      default:
        div.textContent = JSON.stringify(r);
    }
    host.appendChild(div);
  }

  if (pinned) transcript.scrollTop = transcript.scrollHeight;
  refreshComposerMode();
}

function renderToolCard(r) {
  const running = r.status === 'running';
  const isError = r.status === 'error';
  const card = el('div', 'tool-card' + (isError ? ' tool-error open' : ''));

  const head = el('button', 'tool-head');
  head.type = 'button';

  const glyph = el('span', 'tool-glyph');
  glyph.appendChild(toolGlyphSvg(r.toolName || ''));
  head.appendChild(glyph);

  head.appendChild(el('span', 'tool-name', r.toolName || 'tool'));
  head.appendChild(el('span', 'tool-status ' + (r.status || ''), running ? 'running…' : (r.status || '')));

  const dur = running ? null : fmtDuration((r.endedAt || 0) - (r.startedAt || 0));
  if (dur) head.appendChild(el('span', 'tool-duration', dur));

  const chev = el('span', 'tool-chevron');
  chev.innerHTML = '<svg viewBox="0 0 12 12" width="11" height="11"><path d="M4 2l4 4-4 4" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round"/></svg>';
  head.appendChild(chev);

  const body = el('div', 'tool-body');
  const out = r.output && r.output.text ? String(r.output.text) : '';
  if (out) {
    body.textContent = out;
  } else if (isError && r.error && r.error.message) {
    body.textContent = r.error.message;
  } else if (!running) {
    body.innerHTML = '<span class="tool-nooutput">no output captured for replayed events</span>';
  } else {
    body.textContent = '…';
  }

  head.addEventListener('click', () => card.classList.toggle('open'));
  if (isError) card.classList.add('open');
  card.appendChild(head);
  card.appendChild(body);
  return card;
}

/* ---------- composer state ---------- */

function phase() {
  return state.control.phase || 'draft';
}

function refreshComposerMode() {
  const running = phase() === 'running';
  const btn = $('send-btn');
  const input = $('composer-input');
  btn.classList.toggle('stop', running);
  btn.title = running ? 'Stop (Esc)' : 'Send (Enter)';
  $('steer-note').hidden = !running;
  $('phase-chip').hidden = state.pendingNewTask || !state.activeId;
  $('phase-chip').dataset.phase = phase();
  $('phase-label').textContent = PHASE_LABEL[phase()] || phase();
  updateSendDisabled();
}

function updateSendDisabled() {
  const hasText = $('composer-input').value.trim().length > 0;
  const running = phase() === 'running';
  // while running the button is STOP (enabled when a task is active);
  // otherwise it sends (enabled when there is text)
  $('send-btn').disabled = running ? !state.activeId : !hasText;
}

function autosize() {
  const t = $('composer-input');
  t.style.height = 'auto';
  t.style.height = Math.min(t.scrollHeight, 180) + 'px';
}

/* ---------- toasts (in-app notification classes) ---------- */

function toast(kind, title, body, onClick) {
  const t = el('div', 'toast' + (kind === 'error' ? ' error' : ''));
  t.appendChild(el('span', 'toast-dot'));
  const text = el('div');
  text.appendChild(el('div', 'toast-title', title));
  if (body) text.appendChild(el('div', 'toast-body', body));
  t.appendChild(text);
  t.addEventListener('click', () => { t.remove(); if (onClick) onClick(); });
  $('toasts').appendChild(t);
  setTimeout(() => t.remove(), 5000);
}

/* ---------- theme ---------- */

function applyTheme(theme) {
  document.documentElement.dataset.theme = theme;
  localStorage.setItem('okra-theme', theme);
}

function initTheme() {
  const saved = localStorage.getItem('okra-theme');
  if (saved) {
    applyTheme(saved);
  } else {
    applyTheme(window.matchMedia('(prefers-color-scheme: light)').matches ? 'light' : 'dark');
  }
  $('theme-toggle').addEventListener('click', () => {
    applyTheme(document.documentElement.dataset.theme === 'dark' ? 'light' : 'dark');
  });
}

/* ---------- starter scenes ---------- */

function loadScenes() {
  fetch('/scenes').then((r) => r.json()).then((c) => {
    const host = $('scene-chips');
    host.textContent = '';
    const data = (c && c.data) || [];
    for (const s of data) {
      Object.keys(s.options || {}).forEach((k) => {
        (s.options[k].items || []).forEach((it) => {
          const names = it.contents || {};
          const label = names.en || Object.keys(names).map((t) => names[t])[0] || it.id;
          const chip = el('button', 'scene-chip', label);
          chip.type = 'button';
          chip.addEventListener('click', () => {
            const input = $('composer-input');
            input.value = 'Explain this repo — ' + label.toLowerCase();
            autosize();
            input.focus();
            updateSendDisabled();
          });
          host.appendChild(chip);
        });
      });
    }
  }).catch(() => { /* scenes are decorative */ });
}

/* ---------- wire-up ---------- */

function init() {
  initTheme();
  loadHealth();
  loadSessions();
  loadScenes();
  renderTaskList();
  renderRows();

  $('new-task').addEventListener('click', newTask);

  const input = $('composer-input');
  input.addEventListener('input', () => { autosize(); updateSendDisabled(); });
  input.addEventListener('keydown', (e) => {
    if (e.key === 'Enter' && !e.shiftKey) { e.preventDefault(); if (phase() !== 'running') send(); }
  });

  $('send-btn').addEventListener('click', () => {
    if (phase() === 'running') stop(); else send();
  });

  document.addEventListener('keydown', (e) => {
    if (e.key === 'Escape' && phase() === 'running') stop();
    if ((e.metaKey || e.ctrlKey) && e.key.toLowerCase() === 'n') { e.preventDefault(); newTask(); }
  });

  // keep the task list fresh (turns can finish in other surfaces)
  setInterval(loadSessions, 15000);
  document.addEventListener('visibilitychange', () => { if (!document.hidden) loadSessions(); });
}

init();
