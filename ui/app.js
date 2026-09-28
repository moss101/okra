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
  awaitingApproval: 'needs approval',
  completedSuccess: 'completed',
  completedInterrupted: 'interrupted',
  error: 'error',
  replayed: 'history',
};

/* ---------- files tab + preview drawer ---------- */

const files = {
  active: false,
  dirs: {},     // relPath -> entries|null (null = not loaded)
  open: {},     // relPath -> bool (expanded)
  preview: null,
};

const gitState = {
  active: false,
  overview: null,   // {repository, branch, hash, changes[]}
};

function switchTab(tab) {
  files.active = tab === 'files';
  gitState.active = tab === 'changes';
  $('tab-tasks').classList.toggle('active', tab === 'tasks');
  $('tab-files').classList.toggle('active', files.active);
  $('tab-changes').classList.toggle('active', gitState.active);
  for (const [id, on] of [
    ['tab-tasks', 'tasks'], ['tab-files', 'files'], ['tab-changes', 'changes'],
  ]) {
    $(id).setAttribute('aria-selected', String(tab === on));
  }
  $('task-list').hidden = files.active || gitState.active;
  $('file-tree').hidden = !files.active;
  $('changes-list').hidden = !gitState.active;
  if (files.active && !files.dirs['']) loadFiles('');
  if (gitState.active) loadGit();
}

async function loadGit() {
  try {
    const data = await fetch('/api/git').then((r) => r.json());
    gitState.overview = data;
    renderChanges();
  } catch (_) { /* transient */ }
}

function renderChanges() {
  const host = $('changes-list');
  host.textContent = '';
  const ov = gitState.overview;
  if (!ov) { host.appendChild(el('div', 'tree-empty', 'loading…')); return; }
  if (!ov.repository) {
    host.appendChild(el('div', 'tree-empty', 'This workspace is not a git repository.'));
    return;
  }
  const headRow = el('div', 'git-head');
  headRow.appendChild(el('span', 'git-branch', ov.branch || 'HEAD'));
  const dirty = (ov.changes || []).length;
  headRow.appendChild(el('span', 'git-dirty', dirty ? dirty + ' changed' : 'clean'));
  host.appendChild(headRow);
  if (!dirty) {
    host.appendChild(el('div', 'tree-empty', 'No working-tree changes.'));
    return;
  }
  for (const c of ov.changes || []) {
    const row = el('button', 'tree-row');
    row.type = 'button';
    row.appendChild(el('span', 'git-code ' + statusClass(c.code), c.code || 'M'));
    row.appendChild(el('span', 'f-name', c.path));
    row.addEventListener('click', () => openDiff(c.path));
    host.appendChild(row);
  }
}

function statusClass(code) {
  if (!code) return 'st-mod';
  if (code.includes('?')) return 'st-un';
  if (code.includes('D')) return 'st-del';
  if (code.includes('R')) return 'st-ren';
  return 'st-mod';
}

async function openDiff(path) {
  try {
    const data = await fetch('/api/git/diff?path=' + encodeURIComponent(path)).then((r) => r.json());
    if (data.error) { toast('error', 'Diff', data.error); return; }
    $('preview-path').textContent = path;
    $('preview-meta').textContent = 'working-tree diff vs HEAD';
    const body = $('preview-body');
    body.textContent = '';
    if (!data.diff) {
      const none = el('span', 'tool-nooutput', 'no diff (untracked file? see the Changes list code)');
      body.appendChild(none);
    } else {
      for (const line of String(data.diff).split('\n')) {
        const div = el('div', 'dl ' + diffLineClass(line));
        div.textContent = line || ' ';
        body.appendChild(div);
      }
    }
    document.getElementById('preview-drawer').hidden = false;
    document.getElementById('preview-backdrop').hidden = false;
  } catch (_) { /* transient */ }
}

function diffLineClass(line) {
  if (line.startsWith('+')) return 'dl-add';
  if (line.startsWith('-')) return 'dl-del';
  if (line.startsWith('@@')) return 'dl-hunk';
  if (line.startsWith('diff ') || line.startsWith('index ')) return 'dl-meta';
  return '';
}

async function loadFiles(rel) {
  try {
    const data = await fetch('/api/files?path=' + encodeURIComponent(rel)).then((r) => r.json());
    if (data.error) { toast('error', 'Files', data.error); return; }
    files.dirs[rel] = data.entries || [];
    renderFileTree();
  } catch (_) { /* transient */ }
}

function fileIconSvg(isDir, name) {
  const svg = document.createElementNS('http://www.w3.org/2000/svg', 'svg');
  svg.setAttribute('viewBox', '0 0 16 16');
  svg.setAttribute('width', '13');
  svg.setAttribute('height', '13');
  svg.setAttribute('class', 'f-icon');
  const path = document.createElementNS('http://www.w3.org/2000/svg', 'path');
  path.setAttribute('fill', 'none');
  path.setAttribute('stroke', 'currentColor');
  path.setAttribute('stroke-width', '1.4');
  path.setAttribute('stroke-linejoin', 'round');
  path.setAttribute('d', isDir ? 'M2 4h4l1.5 2H14v7H2V4Z'
    : (name.endsWith('.md') ? 'M4 2h5l3 3v9H4V2Zm5 0v3h3M6 9h4M6 11h4'
      : 'M4 2h5l3 3v9H4V2Zm5 0v3h3'));
  svg.appendChild(path);
  return svg;
}

function renderFileTree() {
  const host = $('file-tree');
  host.textContent = '';
  const renderLevel = (rel, container) => {
    const entries = files.dirs[rel];
    if (!entries) {
      container.appendChild(el('div', 'tree-empty', 'loading…'));
      return;
    }
    if (!entries.length && rel === '') {
      container.appendChild(el('div', 'tree-empty', 'No files in this workspace.'));
      return;
    }
    for (const e of entries) {
      const childRel = rel ? rel + '/' + e.name : e.name;
      const row = el('button', 'tree-row' + (e.link ? ' link' : ''));
      row.type = 'button';
      if (e.dir) {
        const tw = el('span', 'twisty');
        tw.innerHTML = '<svg viewBox="0 0 12 12" width="11" height="11"><path d="M4 2l4 4-4 4" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round"/></svg>';
        row.appendChild(tw);
        row.classList.toggle('open', Boolean(files.open[childRel]));
      } else {
        row.appendChild(el('span', 'twisty'));
      }
      row.appendChild(fileIconSvg(e.dir, e.name));
      row.appendChild(el('span', 'f-name', e.name));
      row.addEventListener('click', () => {
        if (!e.dir) { openPreview(childRel); return; }
        files.open[childRel] = !files.open[childRel];
        if (files.open[childRel] && !files.dirs[childRel]) loadFiles(childRel);
        else renderFileTree();
      });
      container.appendChild(row);
      if (e.dir && files.open[childRel]) {
        const kids = el('div', 'tree-children');
        container.appendChild(kids);
        if (files.dirs[childRel]) renderLevel(childRel, kids);
        else loadFiles(childRel);
      }
    }
  };
  renderLevel('', host);
}

async function openPreview(rel) {
  try {
    const data = await fetch('/api/file?path=' + encodeURIComponent(rel)).then((r) => r.json());
    if (data.error) { toast('error', 'Preview', data.error); return; }
    files.preview = data;
    $('preview-path').textContent = data.path;
    $('preview-meta').textContent =
      data.size + ' bytes' + (data.truncated ? ' · truncated to the first 256 KB' : '')
      + (data.binary ? ' · binary content shown lossy' : '');
    $('preview-body').textContent = data.binary
      ? '(binary file — content not shown)'
      : data.content;
    $('preview-drawer').hidden = false;
    $('preview-backdrop').hidden = false;
  } catch (_) { /* transient */ }
}

function closePreview() {
  files.preview = null;
  $('preview-drawer').hidden = true;
  $('preview-backdrop').hidden = true;
}

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
  const approvalRowIds = new Set(
    state.rows.filter((r) => r.kind === 'approval').map((r) => r.approvalId));
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
      case 'approval': {
        div.appendChild(renderApprovalCard(r));
        break;
      }
      default:
        div.textContent = JSON.stringify(r);
    }
    host.appendChild(div);
  }

  // LIVE pending approvals: the kernel row only exists post-decision, so
  // asks still awaiting an answer render straight from control
  for (const a of state.control.awaitingApproval || []) {
    if (approvalRowIds.has(a.approvalId)) continue;
    const div = el('div', 'row row-approval');
    div.appendChild(renderApprovalCard({
      approvalId: a.approvalId,
      toolName: a.toolName,
      args: a.args,
      state: 'pending',
    }));
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

  // file tools carry their target — click opens the preview drawer
  const targetPath = r.input && typeof r.input.path === 'string' ? r.input.path : null;
  if (targetPath) {
    const chip = el('span', 'tool-path', targetPath);
    chip.title = 'Preview ' + targetPath;
    chip.addEventListener('click', (e) => {
      e.stopPropagation();
      openPreview(targetPath);
    });
    head.appendChild(chip);
  }

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

/* ---------- approval cards ---------- */

function pendingApprovalIds() {
  return new Set((state.control.awaitingApproval || []).map((a) => a.approvalId));
}

function renderApprovalCard(r) {
  const pending = pendingApprovalIds().has(r.approvalId) || r.state === 'pending';
  const card = el('div', 'approval-card' + (pending ? ' pending' : ''));

  const head = el('div', 'approval-head');
  const glyph = el('span', 'approval-glyph');
  glyph.innerHTML = '<svg viewBox="0 0 16 16" width="13" height="13"><path d="M8 5.5v3.4M8 11.2v.4M8 2.2 14.5 13H1.5L8 2.2Z" fill="none" stroke="currentColor" stroke-width="1.4" stroke-linecap="round" stroke-linejoin="round"/></svg>';
  head.appendChild(glyph);
  head.appendChild(el('span', 'approval-title',
    'okra wants to use ' + (r.toolName || 'a tool')));
  card.appendChild(head);

  // the approved bytes, inline (the proposed action)
  const args = el('div', 'approval-args');
  args.textContent = prettyArgs(r.args || r.argsJson || '');
  card.appendChild(args);

  const actions = el('div', 'approval-actions');
  if (pending) {
    const allow = el('button', 'approval-btn allow', 'Allow once');
    allow.type = 'button';
    allow.addEventListener('click', () => resolveApproval(r.approvalId, true));
    const deny = el('button', 'approval-btn deny', 'Deny');
    deny.type = 'button';
    deny.addEventListener('click', () => resolveApproval(r.approvalId, false));
    actions.appendChild(allow);
    actions.appendChild(deny);
  } else {
    const st = r.state === 'allowed' ? 'allowed'
      : r.state === 'cancelled' ? 'cancelled' : 'denied';
    const label = st === 'allowed' ? 'Allowed'
      : st === 'cancelled' ? 'Cancelled (stopped)' : 'Denied';
    actions.appendChild(el('span', 'approval-state ' + st, label));
  }
  card.appendChild(actions);
  return card;
}

function prettyArgs(argsJson) {
  try {
    const v = typeof argsJson === 'string' ? JSON.parse(argsJson) : argsJson;
    if (v && typeof v === 'object') {
      return Object.entries(v)
        .map(([k, val]) => k + ': ' + (typeof val === 'string' ? val : JSON.stringify(val)))
        .join('\n');
    }
    return String(argsJson);
  } catch (_) {
    return String(argsJson);
  }
}

async function resolveApproval(approvalId, allow) {
  try {
    await post('/command', {
      commandId: 'web-apr-' + Date.now(),
      type: 'resolveApproval',
      sessionId: state.activeId,
      payload: { approvalId, decision: allow ? 'allow' : 'deny' },
    });
  } catch (e) {
    toast('error', 'Could not resolve approval', String(e));
  }
}

/* ---------- composer state ---------- */

function phase() {
  return state.control.phase || 'draft';
}

function turnActive() {
  return phase() === 'running' || phase() === 'awaitingApproval';
}

function refreshComposerMode() {
  const running = turnActive();
  const btn = $('send-btn');
  btn.classList.toggle('stop', running);
  btn.title = phase() === 'awaitingApproval' ? 'Stop (cancels the pending approval)'
    : running ? 'Stop (Esc)' : 'Send (Enter)';
  $('steer-note').hidden = !running;
  $('phase-chip').hidden = state.pendingNewTask || !state.activeId;
  $('phase-chip').dataset.phase = phase();
  $('phase-label').textContent = PHASE_LABEL[phase()] || phase();
  updateSendDisabled();
}

function updateSendDisabled() {
  const hasText = $('composer-input').value.trim().length > 0;
  const running = turnActive();
  // while a turn is live (running or awaiting approval) the button is STOP
  // (enabled when a task is active); otherwise it sends (enabled on text)
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
    if (e.key === 'Enter' && !e.shiftKey) { e.preventDefault(); if (!turnActive()) send(); }
  });

  $('send-btn').addEventListener('click', () => {
    if (turnActive()) stop(); else send();
  });

  $('tab-tasks').addEventListener('click', () => switchTab('tasks'));
  $('tab-files').addEventListener('click', () => switchTab('files'));
  $('tab-changes').addEventListener('click', () => switchTab('changes'));
  $('preview-close').addEventListener('click', closePreview);
  $('preview-backdrop').addEventListener('click', closePreview);

  document.addEventListener('keydown', (e) => {
    if (e.key === 'Escape') {
      if (files.preview) { closePreview(); return; }
      if (turnActive()) stop();
    }
    if ((e.metaKey || e.ctrlKey) && e.key.toLowerCase() === 'n') { e.preventDefault(); newTask(); }
  });

  // keep the task list fresh (turns can finish in other surfaces);
  // the Changes tab rides the same cadence (writes land as diffs)
  setInterval(() => { loadSessions(); if (gitState.active) loadGit(); }, 15000);
  document.addEventListener('visibilitychange', () => { if (!document.hidden) loadSessions(); });
}

init();
