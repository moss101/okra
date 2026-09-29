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
  awaitingQuestion: 'waiting for your answer',
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

const tools = {
  active: false,
  data: null,       // {skills: {dir, skills[]}, mcp: {servers[]}}
};

function switchTab(tab) {
  files.active = tab === 'files';
  gitState.active = tab === 'changes';
  tools.active = tab === 'tools';
  for (const [id, on] of [
    ['tab-tasks', 'tasks'], ['tab-files', 'files'],
    ['tab-changes', 'changes'], ['tab-tools', 'tools'],
  ]) {
    $(id).classList.toggle('active', tab === on);
    $(id).setAttribute('aria-selected', String(tab === on));
  }
  $('task-list').hidden = files.active || gitState.active || tools.active;
  $('file-tree').hidden = !files.active;
  $('changes-list').hidden = !gitState.active;
  $('tools-list').hidden = !tools.active;
  if (files.active && !files.dirs['']) loadFiles('');
  if (gitState.active) loadGit();
  if (tools.active) loadTools();
}

async function loadTools() {
  try {
    const [skills, mcp] = await Promise.all([
      fetch('/api/skills').then((r) => r.json()),
      fetch('/api/mcp').then((r) => r.json()),
    ]);
    tools.data = { skills, mcp };
    renderTools();
  } catch (_) { /* transient */ }
}

function renderTools() {
  const host = $('tools-list');
  host.textContent = '';
  if (!tools.data) { host.appendChild(el('div', 'tree-empty', 'loading…')); return; }

  const skills = (tools.data.skills && tools.data.skills.skills) || [];
  host.appendChild(el('div', 'git-section', 'Skills · ' + (tools.data.skills.dir || '')));
  if (!skills.length) {
    host.appendChild(el('div', 'tree-empty',
      'None installed — add *.md files with name/description/match frontmatter.'));
  }
  for (const sk of skills) {
    const row = el('div', 'tool-entry');
    const head = el('div', 'tool-entry-head');
    head.appendChild(el('span', 'tool-entry-name', sk.name));
    row.appendChild(head);
    row.appendChild(el('div', 'tool-entry-desc', sk.description || ''));
    if ((sk.patterns || []).length) {
      const chips = el('div', 'pattern-chips');
      for (const p of sk.patterns) chips.appendChild(el('span', 'pattern-chip', p));
      row.appendChild(chips);
    }
    host.appendChild(row);
  }

  const servers = (tools.data.mcp && tools.data.mcp.servers) || [];
  const mcpHead = el('div', 'git-section');
  mcpHead.appendChild(el('span', null, 'MCP servers'));
  const probeBtn = el('button', 'stage-btn', 'probe');
  probeBtn.type = 'button';
  probeBtn.title = 'Connect + list tools (bounded)';
  probeBtn.style.marginLeft = 'auto';
  probeBtn.style.textTransform = 'none';
  probeBtn.style.letterSpacing = 'normal';
  probeBtn.addEventListener('click', async () => {
    probeBtn.disabled = true;
    try {
      await post('/api/mcp/probe', {});
      await loadTools();
    } catch (_) { /* transient */ }
  });
  mcpHead.appendChild(probeBtn);
  host.appendChild(mcpHead);
  if (!servers.length) {
    host.appendChild(el('div', 'tree-empty',
      'None configured — add servers to .okra/mcp.json.'));
  }
  for (const sv of servers) {
    const row = el('div', 'tool-entry');
    const head = el('div', 'tool-entry-head');
    head.appendChild(el('span', 'mcp-dot' + (sv.enabled ? ' on' : ' off')));
    head.appendChild(el('span', 'tool-entry-name', sv.name));
    head.appendChild(el('span', 'mcp-scope', (sv.scope || '') + ' · ' + (sv.source || '')));
    head.appendChild(el('span', 'tool-entry-name', sv.name));
    if (sv.status && sv.status.status) {
      head.appendChild(el('span', 'mcp-status st-' + sv.status.status,
        sv.status.status === 'connected'
          ? 'connected · ' + (sv.status.toolCount || 0) + ' tools'
          : sv.status.status));
    }
    head.appendChild(el('span', 'mcp-scope', (sv.scope || '') + ' · ' + (sv.source || '')));
    row.appendChild(head);
    if (sv.summary) row.appendChild(el('div', 'tool-entry-desc mono', sv.summary));
    if (sv.status && (sv.status.tools || []).length) {
      const chips = el('div', 'pattern-chips');
      for (const tn of sv.status.tools) chips.appendChild(el('span', 'pattern-chip', tn));
      row.appendChild(chips);
    }
    if (!sv.enabled) row.appendChild(el('div', 'tool-entry-desc', 'disabled'));
    host.appendChild(row);
  }
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
    $('commit-box').hidden = true;
    return;
  }

  // porcelain XY: X = staged, Y = unstaged; '??' = untracked
  const staged = [];
  const unstaged = [];
  for (const c of ov.changes || []) {
    const code = c.code || '';
    if (code === '??') { unstaged.push(c); continue; }
    const x = code[0] || ' ';
    const y = code[1] || ' ';
    if (x !== ' ') staged.push(c);
    if (y !== ' ') unstaged.push({ ...c, code: y });
  }

  const headRow = el('div', 'git-head');
  headRow.appendChild(el('span', 'git-branch', ov.branch || 'HEAD'));
  headRow.appendChild(el('span', 'git-dirty',
    (ov.changes || []).length ? (ov.changes.length + ' changed') : 'clean'));
  host.appendChild(headRow);

  if (!(ov.changes || []).length) {
    host.appendChild(el('div', 'tree-empty', 'No working-tree changes.'));
  }

  const section = (label) => {
    const h = el('div', 'git-section');
    h.textContent = label;
    host.appendChild(h);
  };

  if (unstaged.length) {
    section('Changes');
    for (const c of unstaged) {
      host.appendChild(changeRow(c, false));
    }
  }
  if (staged.length) {
    section('Staged');
    for (const c of staged) {
      host.appendChild(changeRow(c, true));
    }
  }

  // commit box: needs staged changes + a message
  $('commit-box').hidden = !staged.length;
  updateCommitButton();
}

function changeRow(c, isStaged) {
  const row = el('div', 'tree-row');
  const code = el('span', 'git-code ' + statusClass(c.code), c.code || 'M');
  row.appendChild(code);
  row.appendChild(el('span', 'f-name', c.path));
  const toggle = el('button', 'stage-btn', isStaged ? '−' : '+');
  toggle.type = 'button';
  toggle.title = isStaged ? 'Unstage' : 'Stage';
  toggle.addEventListener('click', async (e) => {
    e.stopPropagation();
    try {
      await post(isStaged ? '/api/git/unstage' : '/api/git/stage', { paths: [c.path] });
      loadGit();
    } catch (_) { /* transient */ }
  });
  row.appendChild(toggle);
  row.addEventListener('click', () => openDiff(c.path));
  return row;
}

async function commitStaged() {
  const message = $('commit-message').value.trim();
  if (!message) return;
  try {
    const r = await post('/api/git/commit', { message });
    if (r.error) { toast('error', 'Commit failed', r.error); return; }
    $('commit-message').value = '';
    toast('ok', 'Committed', r.hash ? r.hash.slice(0, 10) + ' on ' + (r.branch || '') : '');
    loadGit();
  } catch (e) {
    toast('error', 'Commit failed', String(e));
  }
}

function updateCommitButton() {
  const stagedCount = (gitState.overview && gitState.overview.changes || [])
    .filter((c) => (c.code || ' ')[0] !== ' ' && c.code !== '??').length;
  $('commit-btn').disabled = stagedCount === 0
    || $('commit-message').value.trim().length === 0;
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
  unread: {},              // sessionId -> true (tray unread model)
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
    if (msg && msg.method === 'v4/notification') {
      handleNotification(msg.params || {});
      return;
    }
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
  delete state.unread[id];
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
  if (!text && !state.attachments.length) return;

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
  const attachments = state.attachments.splice(0);
  renderAttachChips();
  autosize();
  refreshComposerMode();

  try {
    await post('/command', {
      commandId: 'web-' + Date.now(),
      type: isNew ? 'createSession' : 'sendText',
      sessionId: id,
      payload: { text, attachments },
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
    const unread = state.unread[s.id];
    const item = el('button', 'task-item' + (s.id === state.activeId ? ' active' : '')
      + (unread ? ' unread' : ''));
    item.type = 'button';
    item.appendChild(el('span', 'task-item-title', s.title || 'Task ' + s.id));
    const meta = el('span', 'task-item-meta');
    const dot = el('span', 'status-dot ' + (s.status || ''));
    meta.appendChild(dot);
    meta.appendChild(el('span', null, (s.live ? 'live · ' : '') + (s.eventCount != null ? s.eventCount + ' events' : '')));
    item.appendChild(meta);
    item.addEventListener('click', () => { delete state.unread[s.id]; selectSession(s.id); });
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

/* ---------- transcript virtualizer ----------
 *
 * The daemon streams FULL projection snapshots (often several per second
 * during a turn). Rebuilding every row per frame — re-running markdown on
 * every assistant row — is O(transcript) per frame and dies on long tasks
 * (UI-SHELL-PLAN U1: "a 2k-message streaming task stays at 60 fps").
 *
 * Two mechanisms, per the ChatGPT2 docs/07 design:
 * 1. KEYED RECONCILIATION — rows are cached by stable identity (rowId)
 *    with a cheap per-kind mutation signature; unchanged rows reuse their
 *    DOM node (markdown is rendered exactly once per row, tool-card
 *    expansion survives re-renders).
 * 2. WINDOWED RENDERING WITH LAYOUT CHECKPOINTS — only the rows near the
 *    viewport exist in the DOM; positions come from the layout-checkpoint
 *    map (real measured heights, remembered; unrendered rows use per-kind
 *    estimates) driving top/bottom spacers. When checkpoints change above
 *    the viewport, scroll position is compensated so content never jumps.
 */

const virtualizer = {
  cache: new Map(),     // key -> {sig, el}   (keyed DOM reuse)
  heights: new Map(),   // key -> measured px (layout checkpoints)
  toolOpen: new Set(),  // callId -> card expanded (survives rebuilds)
  spacers: null,        // [topDiv, bottomDiv] once created
};

const ROW_ESTIMATE = {
  turnHeader: 34,
  userInput: 48,
  assistantText: 56,
  toolCall: 52,
  approval: 128,
  _default: 48,
};

function rowKey(r) { return 'r' + r.rowId; }

function rowSig(r) {
  switch (r.kind) {
    case 'turnHeader': return String(r.state || '');
    case 'userInput': return 't' + (r.text || '');
    case 'assistantText': return (r.state || '') + '|' + (r.text || '').length;
    case 'toolCall': return [r.status, r.output && r.output.text ? r.output.text.length : 0,
      r.input && r.input.path ? r.input.path : '', r.startedAt, r.endedAt].join('|');
    case 'approval': return String(r.state || '');
    case 'question': return (r.questionId || '') + '|' + (r.question || '');
    default: return JSON.stringify(r).slice(0, 80);
  }
}

function estimateHeight(key, r) {
  const measured = virtualizer.heights.get(key);
  if (measured !== undefined) return measured;
  if (r && r.kind === 'assistantText') {
    // rough flow estimate: ~70 chars/line at the transcript width
    const lines = Math.ceil((r.text || '').length / 70) + ((r.text || '').match(/\n/g) || []).length;
    return 26 + lines * 24;
  }
  return ROW_ESTIMATE[(r && r.kind) || '_default'] || ROW_ESTIMATE._default;
}

function buildRowNode(r) {
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
      for (const p of r.attachments || []) {
        const chip = el('span', 'attach-chip static');
        chip.appendChild(el('span', 'attach-name', p));
        chip.addEventListener('click', () => openPreview(p));
        bubble.appendChild(el('div'));
        bubble.appendChild(chip);
      }
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
    case 'question': {
      div.appendChild(renderQuestionCard(r));
      break;
    }
    default:
      div.textContent = JSON.stringify(r);
  }
  return div;
}

function buildQuestionNode(q) {
  const div = el('div', 'row row-question');
  div.appendChild(renderQuestionCard(q));
  return div;
}

function buildPendingApprovalNode(a) {
  const div = el('div', 'row row-approval');
  div.appendChild(renderApprovalCard({
    approvalId: a.approvalId,
    toolName: a.toolName,
    args: a.args,
    state: 'pending',
  }));
  return div;
}

/// The checkpoint pass: remember real rendered heights (rAF-batched).
function measurePass(host, keys) {
  requestAnimationFrame(() => {
    for (const key of keys) {
      const entry = virtualizer.cache.get(key);
      if (!entry || !entry.el.isConnected) continue;
      const h = entry.el.offsetHeight;
      if (h > 0 && virtualizer.heights.get(key) !== h) {
        virtualizer.heights.set(key, h);
        virtualizer.relayout = true;
      }
    }
    if (virtualizer.relayout) {
      virtualizer.relayout = false;
      layoutWindow(true); // compensate anchors with the refined heights
    }
  });
}

/// Compute [start, end) of the visible window from checkpoints.
function windowBounds(count) {
  const transcript = $('transcript');
  const OVERSCAN = 900;
  const top = transcript.scrollTop - OVERSCAN;
  const bottom = transcript.scrollTop + transcript.clientHeight + OVERSCAN;
  let y = 0;
  let start = 0;
  let startTop = 0;
  let started = false;
  for (let i = 0; i < count; i++) {
    const h = estimateHeight(virtualizer.viewKeys[i], virtualizer.viewMeta[i]);
    if (!started && y + h > top) {
      start = i;
      startTop = y;
      started = true;
    }
    if (started && y >= bottom) {
      return { start, end: i, startTop };
    }
    y += h;
  }
  if (!started && count > 0) {
    start = Math.max(0, count - 1);
    startTop = y;
  }
  return { start, end: count, startTop };
}

/// Sum checkpoint/estimate heights for rows [from, to).
function spanHeight(from, to, keys, meta) {
  let total = 0;
  for (let i = from; i < to && i < keys.length; i++) {
    total += estimateHeight(keys[i], meta[i]);
  }
  return total;
}

/// (Re)render only the window; spacers stand in for the rest.
function layoutWindow(anchorCompensate) {
  const host = $('rows');
  const transcript = $('transcript');
  const keys = virtualizer.viewKeys || [];
  const meta = virtualizer.viewMeta || [];
  const count = keys.length;
  if (!count) { host.textContent = ''; return; }

  const before = transcript.scrollTop;
  const pinned = transcript.scrollHeight - before - transcript.clientHeight < 90;

  const { start, end, startTop } = windowBounds(count);
  const topPad = spanHeight(0, start, keys, meta);
  const bottomPad = spanHeight(end, count, keys, meta);

  if (!virtualizer.spacers) {
    virtualizer.spacers = [el('div', 'vspacer'), el('div', 'vspacer')];
  }

  const winSig = start + ':' + end + ':' + count;
  if (virtualizer.winSig !== winSig) {
    // window identity changed: rebuild the window from the keyed cache
    virtualizer.winSig = winSig;
    host.textContent = '';
    host.appendChild(virtualizer.spacers[0]);
    const measured = [];
    for (let i = start; i < end; i++) {
      const key = keys[i];
      const r = meta[i];
      let entry = virtualizer.cache.get(key);
      if (!entry || entry.sig !== rowSig(r)) {
        const node = r.__pending
            ? (r.kind === 'question' ? buildQuestionNode(r) : buildPendingApprovalNode(r))
            : buildRowNode(r);
        entry = { sig: rowSig(r), el: node };
        virtualizer.cache.set(key, entry);
      }
      host.appendChild(entry.el);
      measured.push(key);
    }
    host.appendChild(virtualizer.spacers[1]);
    measurePass(host, measured);
  } else {
    // same window: refresh only rows whose mutation signature changed —
    // markdown of unchanged rows is never re-rendered
    const measured = [];
    for (let i = start; i < end; i++) {
      const key = keys[i];
      const r = meta[i];
      let entry = virtualizer.cache.get(key);
      if (!entry || entry.sig !== rowSig(r)) {
        const node = r.__pending
            ? (r.kind === 'question' ? buildQuestionNode(r) : buildPendingApprovalNode(r))
            : buildRowNode(r);
        if (entry && entry.el.isConnected) {
          entry.el.replaceWith(node);
        }
        entry = { sig: rowSig(r), el: node };
        virtualizer.cache.set(key, entry);
        measured.push(key);
      }
    }
    if (measured.length) measurePass(host, measured);
  }

  // spacers position the window in the virtual coordinate space
  virtualizer.spacers[0].style.height = topPad + 'px';
  virtualizer.spacers[1].style.height = bottomPad + 'px';

  if (pinned) {
    transcript.scrollTop = transcript.scrollHeight;
  } else if (anchorCompensate && topPad !== virtualizer.lastTopPad) {
    // a checkpoint above the viewport got refined: keep the content glued
    transcript.scrollTop = before + (topPad - (virtualizer.lastTopPad || 0));
  }
  virtualizer.lastTopPad = topPad;
}

function renderRows() {
  const host = $('rows');
  const transcript = $('transcript');
  const pinned = transcript.scrollHeight - transcript.scrollTop - transcript.clientHeight < 90;

  // empty state vs rows
  const empty = $('empty-state');
  const showEmpty = !state.rows.length && !state.pendingNewTask && state.control.phase !== 'running';
  empty.style.display = showEmpty ? '' : 'none';
  if (showEmpty) {
    host.textContent = '';
    virtualizer.viewKeys = [];
    virtualizer.viewMeta = [];
    virtualizer.winSig = null;
    refreshComposerMode();
    return;
  }

  // assemble the flat view: durable rows + LIVE pending approvals (the
  // kernel row only exists post-decision, so asks awaiting an answer
  // render straight from control)
  const approvalRowIds = new Set(
    state.rows.filter((r) => r.kind === 'approval').map((r) => r.approvalId));
  const meta = [];
  for (const r of state.rows) {
    meta.push(r);
  }
  for (const a of state.control.awaitingApproval || []) {
    if (approvalRowIds.has(a.approvalId)) continue;
    meta.push({ __pending: true, rowId: 'pending-' + a.approvalId, approvalId: a.approvalId, toolName: a.toolName, args: a.args, kind: 'approval' });
  }
  // LIVE question (N0020): ask_user blocks until answered — the card
  // renders straight from control.awaitingQuestion
  const liveQ = state.control.awaitingQuestion;
  if (liveQ && liveQ.questionId) {
    meta.push({ __pending: true, rowId: 'question-' + liveQ.questionId,
      questionId: liveQ.questionId, question: liveQ.question, kind: 'question' });
  }

  virtualizer.viewMeta = meta;
  virtualizer.viewKeys = meta.map((r) => rowKey(r));

  // bound the cache: drop entries far outside any plausible window
  if (virtualizer.cache.size > 1200) {
    const keep = new Set(virtualizer.viewKeys);
    for (const key of [...virtualizer.cache.keys()]) {
      if (!keep.has(key) && virtualizer.cache.size > 900) {
        virtualizer.cache.delete(key);
        virtualizer.heights.delete(key);
      }
    }
  }

  layoutWindow(false);
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

  const openState = virtualizer.toolOpen.has(r.toolCallId);
  head.addEventListener('click', () => {
    const nowOpen = !card.classList.contains('open');
    card.classList.toggle('open', nowOpen);
    if (r.toolCallId) {
      if (nowOpen) virtualizer.toolOpen.add(r.toolCallId);
      else virtualizer.toolOpen.delete(r.toolCallId);
    }
  });
  if (isError || openState) card.classList.add('open');
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

/* ---------- question cards (ask_user flow) ---------- */

function renderQuestionCard(q) {
  const card = el('div', 'question-card');
  const head = el('div', 'approval-head');
  const glyph = el('span', 'question-glyph');
  glyph.innerHTML = '<svg viewBox="0 0 16 16" width="13" height="13"><path d="M6 4.2c.4-1.6 1.9-2.4 3.4-2 1.3.3 2.2 1.5 2.1 2.8-.1 1.4-1.2 2-2.2 2.6-.8.5-1.3 1-1.3 2v.4M8 12.6v.4" fill="none" stroke="currentColor" stroke-width="1.4" stroke-linecap="round"/></svg>';
  head.appendChild(glyph);
  head.appendChild(el('span', 'approval-title', 'okra has a question'));
  card.appendChild(head);
  card.appendChild(el('div', 'question-text', q.question || ''));
  const actions = el('div', 'question-actions');
  const input = el('input', 'question-input');
  input.type = 'text';
  input.placeholder = 'Your answer…';
  input.setAttribute('aria-label', 'Your answer');
  const submit = el('button', 'approval-btn allow', 'Answer');
  submit.type = 'button';
  submit.addEventListener('click', () => {
    const answer = input.value.trim();
    if (!answer) { input.focus(); return; }
    answerQuestion(q.questionId, answer);
  });
  input.addEventListener('keydown', (e) => {
    if (e.key === 'Enter') {
      e.preventDefault();
      const answer = input.value.trim();
      if (answer) answerQuestion(q.questionId, answer);
    }
  });
  actions.appendChild(input);
  actions.appendChild(submit);
  card.appendChild(actions);
  return card;
}

async function answerQuestion(questionId, answer) {
  try {
    await post('/command', {
      commandId: 'web-q-' + Date.now(),
      type: 'answerQuestion',
      sessionId: state.activeId,
      payload: { questionId, answer },
    });
  } catch (e) {
    toast('error', 'Could not send the answer', String(e));
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
  return phase() === 'running' || phase() === 'awaitingApproval'
    || phase() === 'awaitingQuestion';
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
  const hasPayload = $('composer-input').value.trim().length > 0
    || state.attachments.length > 0;
  const running = turnActive();
  // while a turn is live (running or awaiting approval) the button is STOP
  // (enabled when a task is active); otherwise it sends (enabled on text)
  $('send-btn').disabled = running ? !state.activeId : !hasPayload;
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

/* ---------- notifications (3-class boundary, ChatGPT2 docs/02) ----------
 *
 * The daemon classifies and REDACTS (labels are metadata — tool output,
 * file contents and prompts never leave the process). The surface applies
 * the focus policy: focused -> in-app toast only; unfocused -> native
 * Notification (when granted) + toast, and background tasks accrue an
 * unread dot that clears on selection (the tray unread model).
 */

const NOTIF_CLASS_LABEL = {
  turn_complete: 'Task finished',
  permission_request: 'Approval needed',
  question: 'Question',
};

function handleNotification(n) {
  const classLabel = NOTIF_CLASS_LABEL[n.class] || 'Update';
  const isPermission = n.class === 'permission_request';
  const unfocused = document.hidden || !document.hasFocus();

  // unread dot for background tasks (cleared on selection)
  if (n.sessionId !== state.activeId) {
    state.unread[n.sessionId] = true;
    renderTaskList();
  }

  // the local task title is app-internal; the NATIVE body stays the
  // daemon-redacted label — never richer
  const local = n.sessionId === state.activeId
    ? $('topbar-title').textContent
    : (state.sessions.find((x) => x.id === n.sessionId) || {}).title;
  const title = local || classLabel;

  toast(isPermission ? 'error' : 'ok', classLabel, title || n.label, () => {
    if (n.sessionId && state.sessions.some((x) => x.id === n.sessionId)) {
      selectSession(n.sessionId);
    }
  });

  if (unfocused && 'Notification' in window && Notification.permission === 'granted') {
    try {
      const notif = new Notification('okra — ' + classLabel, {
        body: n.label,          // the redacted label, nothing more
        tag: n.sessionId,       // one notification per task
        silent: isPermission,   // permissions already glow in-app
      });
      notif.onclick = () => { window.focus(); if (n.sessionId) selectSession(n.sessionId); };
    } catch (_) { /* best-effort */ }
  }
}

async function requestNotificationPermission() {
  if (!('Notification' in window)) return;
  if (Notification.permission === 'default') {
    try { await Notification.requestPermission(); } catch (_) { /* denied */ }
  }
}

/* ---------- composer attachments + @-mentions (ChatGPT2 docs/07) ----------
 *
 * Chips hold workspace paths; the daemon folds their CONTENT into the
 * logged, model-visible user message (bounded). @-tokens in the composer
 * query GET /api/files/search and complete to workspace paths.
 */

state.attachments = [];

function renderAttachChips() {
  const host = $('attach-chips');
  host.textContent = '';
  host.hidden = !state.attachments.length;
  for (const p of state.attachments) {
    const chip = el('span', 'attach-chip');
    chip.appendChild(el('span', 'attach-name', p));
    const x = el('button', 'attach-x', '\u00d7');
    x.type = 'button';
    x.title = 'Remove attachment';
    x.addEventListener('click', () => {
      state.attachments = state.attachments.filter((q) => q !== p);
      renderAttachChips();
    });
    chip.appendChild(x);
    host.appendChild(chip);
  }
  updateSendDisabled();
}

function addAttachment(p) {
  if (!p || state.attachments.includes(p)) return;
  state.attachments.push(p);
  renderAttachChips();
}

/* --- mentions --- */

const mentions = { open: false, items: [], start: -1, token: '' };

function updateMentions() {
  const t = $('composer-input');
  const upToCaret = t.value.slice(0, t.selectionStart);
  const m = /(^|\s)@([^\s@]*)$/.exec(upToCaret);
  if (!m) { closeMentions(); return; }
  mentions.start = t.selectionStart - m[2].length - 1;
  mentions.token = m[2];
  clearTimeout(mentions.debounce);
  mentions.debounce = setTimeout(async () => {
    try {
      const data = await fetch('/api/files/search?q=' + encodeURIComponent(mentions.token))
        .then((r) => r.json());
      mentions.items = (data.matches || []).filter((p) => !state.attachments.includes(p));
      mentions.open = mentions.items.length > 0;
      renderMentions();
    } catch (_) { /* transient */ }
  }, 120);
}

function renderMentions() {
  const pop = $('mention-pop');
  pop.textContent = '';
  pop.hidden = !mentions.open;
  if (!mentions.open) return;
  mentions.items.slice(0, 8).forEach((p, i) => {
    const item = el('button', 'mention-item' + (i === 0 ? ' sel' : ''), p);
    item.type = 'button';
    item.addEventListener('click', () => applyMention(p));
    pop.appendChild(item);
  });
}

function applyMention(p) {
  const t = $('composer-input');
  if (mentions.start >= 0) {
    t.value = t.value.slice(0, mentions.start) + p + ' ' + t.value.slice(t.selectionStart);
    mentions.open = false;
    $('mention-pop').hidden = true;
    t.focus();
    const caret = mentions.start + p.length + 1;
    t.setSelectionRange(caret, caret);
  }
  // a mentioned file doubles as an attachment: its content rides the turn
  addAttachment(p);
  autosize();
  updateSendDisabled();
}

function closeMentions() {
  mentions.open = false;
  $('mention-pop').hidden = true;
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

/* ---------- terminal pane (plain PTY emulator) ----------
 *
 * Streams the daemon's PTY output over SSE and renders it with a small
 * stateful processor: ANSI escape sequences are stripped, \r rewrites the
 * line from its start (prompts/progress), \b steps back, \n feeds.
 * Full emulator semantics (curses apps, alt-screen) are out of scope —
 * this is the dogfood loop: ls, git status, cat, echo, ctrl-C.
 */

const term = {
  open: false,          // pane expanded
  id: null,             // active terminal id
  es: null,             // EventSource
  lines: [''],          // processed display lines
  cursor: 0,            // column in the current line
  esc: null,            // partial escape-sequence state
  decoder: new TextDecoder(),
  attached: false,
};

function termToggle() {
  term.open = !term.open;
  $('term-pane').classList.toggle('collapsed', !term.open);
  if (term.open) {
    termEnsure();
    $('term-screen').focus();
  }
}

async function termEnsure() {
  if (term.attached && term.id) return;
  // reattach to an existing session if the page reloaded
  try {
    const list = await fetch('/api/term').then((r) => r.json());
    const id = (list.ids && list.ids[0]) || await termOpenNew();
    termAttach(id);
  } catch (_) { /* daemon offline */ }
}

async function termOpenNew() {
  const r = await post('/api/term/open', {});
  return r.id;
}

async function termNew() {
  try {
    const id = await termOpenNew();
    // switch the pane to the new session (simple: reset the view)
    if (term.es) term.es.close();
    term.attached = false;
    term.lines = [''];
    term.cursor = 0;
    term.esc = null;
    termAttach(id);
  } catch (e) { toast('error', 'Terminal', String(e)); }
}

function termAttach(id) {
  term.id = id;
  term.attached = true;
  $('term-id').textContent = id;
  if (term.es) term.es.close();
  const es = new EventSource('/api/term/' + encodeURIComponent(id) + '/sse');
  term.es = es;
  es.onmessage = (e) => {
    let msg;
    try { msg = JSON.parse(e.data); } catch (_) { return; }
    if (msg.type === 'out') {
      const bytes = Uint8Array.from(atob(msg.b64), (c) => c.charCodeAt(0));
      termFeed(term.decoder.decode(bytes, { stream: true }));
    } else if (msg.type === 'reset') {
      term.lines = [''];
      term.cursor = 0;
      term.esc = null;
    } else if (msg.type === 'exit') {
      termWrite('\n[process exited]\n');
    }
    termRender();
  };
}

/* feed decoded characters through the tiny processor */
function termFeed(text) {
  for (const ch of text) {
    if (term.esc !== null) {
      // ESC [ -> params (0x30-0x3F, incl. '?' for bracketed paste) ->
      // final byte 0x40-0x7E; any other char after ESC is a 2-char escape
      if (term.esc === 'intro') {
        term.esc = ch === '[' ? 'csi' : null;
      } else if (ch >= '@' && ch <= '~') {
        term.esc = null;
      }
      continue;
    }
    if (ch === '\x1b') { term.esc = 'intro'; continue; }
    if (ch === '\r') { term.cursor = 0; continue; }
    if (ch === '\n') { term.lines.push(''); term.cursor = 0; continue; }
    if (ch === '\b') { term.cursor = Math.max(0, term.cursor - 1); continue; }
    if (ch === '\t') {
      const line = term.lines[term.lines.length - 1];
      const pad = 8 - (term.cursor % 8);
      term.lines[term.lines.length - 1] = line + ' '.repeat(pad);
      term.cursor += pad;
      continue;
    }
    const line = term.lines[term.lines.length - 1];
    // overwrite at the cursor (\r rewrites), pad if the cursor is past the end
    const base = line.slice(0, term.cursor);
    const rest = line.slice(term.cursor + ch.length);
    term.lines[term.lines.length - 1] = base + ch + rest;
    term.cursor += ch.length;
    // keep the display bounded
    if (term.lines.length > 3000) {
      term.lines.splice(0, 1000);
    }
  }
}

function termWrite(extra) {
  term.lines.push(...extra.split('\n'));
}

function termRender() {
  const screen = $('term-screen');
  if (!term.open) return;
  const text = term.lines.join('\n');
  const pinned = screen.scrollHeight - screen.scrollTop - screen.clientHeight < 40;
  screen.textContent = text;
  if (pinned) screen.scrollTop = screen.scrollHeight;
}

function termSend(data) {
  if (!term.id) return;
  // keystroke ORDER matters: chain one in-flight POST at a time (parallel
  // fetches race across connections and scramble the input)
  term.pending = (term.pending || Promise.resolve())
    .then(() => post('/api/term/' + encodeURIComponent(term.id) + '/keys', { data }))
    .catch(() => { /* transient */ });
}

/* keyboard mapping: printable chars, Enter, Backspace, Tab, ^C/^D/^L, arrows */
function termKeyHandler(e) {
  if (!term.open || !term.id) return;
  let data = null;
  if (e.key === 'Enter') data = '\r';
  else if (e.key === 'Backspace') data = '\x7f';
  else if (e.key === 'Tab') data = '\t';
  else if (e.key === 'ArrowUp') data = '\x1b[A';
  else if (e.key === 'ArrowDown') data = '\x1b[B';
  else if (e.key === 'ArrowRight') data = '\x1b[C';
  else if (e.key === 'ArrowLeft') data = '\x1b[D';
  else if (e.ctrlKey && e.key === 'c') data = '\x03';
  else if (e.ctrlKey && e.key === 'd') data = '\x04';
  else if (e.ctrlKey && e.key === 'l') data = '\x0c';
  else if (e.key.length === 1 && !e.metaKey && !e.ctrlKey) data = e.key;
  if (data !== null) {
    e.preventDefault();
    termSend(data);
  }
}

/* ---------- wire-up ---------- */

function init() {
  initTheme();
  loadHealth();
  requestNotificationPermission();
  loadSessions();
  loadScenes();
  renderTaskList();
  renderRows();

  $('new-task').addEventListener('click', newTask);

  const input = $('composer-input');
  input.addEventListener('input', () => { autosize(); updateSendDisabled(); updateMentions(); });
  input.addEventListener('keydown', (e) => {
    if (mentions.open) {
      const items = [...document.querySelectorAll('.mention-item')];
      const sel = items.findIndex((i) => i.classList.contains('sel'));
      if (e.key === 'ArrowDown' || e.key === 'ArrowUp') {
        e.preventDefault();
        const next = e.key === 'ArrowDown'
          ? Math.min(items.length - 1, sel + 1) : Math.max(0, sel - 1);
        items.forEach((it, i) => it.classList.toggle('sel', i === next));
        return;
      }
      if (e.key === 'Enter' && items.length) {
        e.preventDefault();
        applyMention(mentions.items[Math.max(0, sel)]);
        return;
      }
      if (e.key === 'Escape') { e.preventDefault(); closeMentions(); return; }
    }
    if (e.key === 'Enter' && !e.shiftKey) { e.preventDefault(); if (!turnActive()) send(); }
  });

  $('send-btn').addEventListener('click', () => {
    if (turnActive()) stop(); else send();
  });

  $('term-toggle').addEventListener('click', termToggle);
  $('term-new').addEventListener('click', termNew);
  $('term-clear').addEventListener('click', () => {
    term.lines = [''];
    term.cursor = 0;
    termRender();
  });
  const screen = $('term-screen');
  screen.addEventListener('keydown', termKeyHandler);
  screen.addEventListener('paste', (e) => {
    const text = e.clipboardData && e.clipboardData.getData('text');
    if (text) { e.preventDefault(); termSend(text); }
  });

  $('tab-tasks').addEventListener('click', () => switchTab('tasks'));
  $('tab-files').addEventListener('click', () => switchTab('files'));
  $('tab-changes').addEventListener('click', () => switchTab('changes'));
  $('tab-tools').addEventListener('click', () => switchTab('tools'));
  $('commit-btn').addEventListener('click', commitStaged);
  $('commit-message').addEventListener('input', updateCommitButton);
  $('commit-message').addEventListener('keydown', (e) => {
    if (e.key === 'Enter' && !$('commit-btn').disabled) commitStaged();
  });
  $('preview-close').addEventListener('click', closePreview);
  $('preview-backdrop').addEventListener('click', closePreview);

  // scrolling re-windows the transcript (rAF-throttled)
  let scrollQueued = false;
  $('transcript').addEventListener('scroll', () => {
    if (scrollQueued) return;
    scrollQueued = true;
    requestAnimationFrame(() => {
      scrollQueued = false;
      if (virtualizer.viewKeys && virtualizer.viewKeys.length) layoutWindow(false);
    });
  });

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
