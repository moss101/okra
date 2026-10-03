// load probe (packaged-app diagnosis): proves whether main.js executes
try {
  require('node:fs').appendFileSync(
    require('node:path').join(require('node:os').tmpdir(), 'okra-main-probe.txt'),
    JSON.stringify({ at: Date.now(), requireMain: String(require.main === module),
      argv: process.argv.slice(0, 3) }) + '\n');
} catch (_) { /* probe must never break the app */ }

'use strict';
/* okra desktop shell — MASTER-PLAN block #50: a thin Electron main.
 *
 * Window + process lifecycle ONLY. Every product behavior lives in the
 * daemon and its web workbench; this process spawns the daemon, waits for
 * the handshake (bind → /health), and points a window at it. No business
 * logic, no IPC surface — the web UI already speaks loopback HTTP/SSE.
 *
 * Launch:  npm start [-- --cwd DIR] [--okra PATH]
 * Smoke:   npm run smoke   (handshake only, no window; CI-verifiable)
 */

const { spawn } = require('node:child_process');
const http = require('node:http');
const os = require('node:os');
const path = require('node:path');
const fs = require('node:fs');

// electron is optional so the handshake module is testable under plain
// node (node --test) — the window path is the only electron-dependent code
let electron = null;
try {
  electron = require('electron');
} catch (_) { /* plain node */ }

function parseArgs(argv) {
  const isPackaged = Boolean(electron && electron.app && electron.app.isPackaged);
  // packaged Electron eats unknown switches (Chromium flags) — smoke mode
  // is triggerable by env as well as the dev-mode arg
  const args = {
    cwd: process.cwd(),
    okra: process.env.OKRA_BIN || null,
    smoke: process.env.OKRA_SMOKE === '1',
  };
  for (let i = 0; i < argv.length; i++) {
    const a = argv[i];
    if (a === '--cwd') args.cwd = path.resolve(argv[++i] || '.');
    else if (a === '--okra') args.okra = argv[++i] || null;
    else if (a === '--smoke') args.smoke = true;
  }
  // double-clicked from Finder: cwd is '/', which is nobody's workspace —
  // default to a per-user workspace dir and create it
  if (isPackaged && (args.cwd === '/' || args.cwd === path.resolve('/'))) {
    args.cwd = path.join(os.homedir(), 'Documents', 'okra workspace');
    fs.mkdirSync(args.cwd, { recursive: true });
  }
  return args;
}

/** The real-model config: ~/.okra/desktop-provider.json (optional).
 *
 * { "provider": "openai", "model": "<name>", "apiKey": "<key>",
 *   "baseUrl": "<openai-compatible endpoint>", "extraHeaders": "K: V; K: V" }
 *
 * When present, the daemon launches with --provider/--model and the
 * key/base-url are injected into its environment — this is what turns the
 * demo sampler into the real thing in a packaged app (Finder launches
 * have no shell env).
 */
function readProviderConfig() {
  try {
    const p = path.join(os.homedir(), '.okra', 'desktop-provider.json');
    const cfg = JSON.parse(fs.readFileSync(p, 'utf8'));
    return cfg && cfg.provider && cfg.apiKey ? cfg : null;
  } catch (_) { return null; }
}

/** Find the okra binary: explicit flag/env, else the repo's build output. */
function resolveOkraBin(args) {
  const candidates = [
    args.okra,
    // packaged: electron-packager's extraResource lands it in Contents/Resources
    process.resourcesPath ? path.join(process.resourcesPath, 'okra') : null,
    path.join(__dirname, '..', '..', 'target', 'release', 'okra'),
    path.join(__dirname, '..', '..', 'target', 'debug', 'okra'),
  ].filter(Boolean);
  for (const c of candidates) {
    try {
      fs.accessSync(c, fs.constants.X_OK);
      return c;
    } catch (_) { /* try next */ }
  }
  throw new Error('okra binary not found (pass --okra PATH or build the workspace)');
}

/** Spawn the loopback daemon; resolve with its bound port from stderr. */
function startDaemon(args) {
  const bin = resolveOkraBin(args);
  const cfg = readProviderConfig();
  const serveArgs = ['serve', '--tcp', '--cwd', args.cwd];
  const env = { ...process.env };
  if (cfg) {
    serveArgs.push('--provider', cfg.provider);
    if (cfg.model) serveArgs.push('--model', cfg.model);
    env.OKRA_API_KEY = cfg.apiKey;
    if (cfg.baseUrl) env.OKRA_BASE_URL = cfg.baseUrl;
    if (cfg.extraHeaders) env.OKRA_EXTRA_HEADERS = cfg.extraHeaders;
  }
  const child = spawn(bin, serveArgs, {
    stdio: ['ignore', 'ignore', 'pipe'],
    env,
  });
  return new Promise((resolve, reject) => {
    const timer = setTimeout(() => reject(new Error('daemon never bound a port')), 15000);
    let stderr = '';
    const onLine = (chunk) => {
      stderr += chunk;
      for (const line of stderr.split('\n')) {
        const m = line.match(/multi-surface daemon on (127\.0\.0\.1:\d+)/);
        if (m) {
          clearTimeout(timer);
          resolve({ child, bin, port: Number(m[1].split(':')[1]) });
          return;
        }
      }
    };
    child.stderr.on('data', onLine);
    child.on('exit', (code) => {
      clearTimeout(timer);
      reject(new Error(`daemon exited early (code ${code}): ${stderr.slice(0, 400)}`));
    });
  });
}

/** GET /health once; resolves the parsed body or rejects. */
function healthOnce(port) {
  return new Promise((resolve, reject) => {
    const req = http.get({ host: '127.0.0.1', port, path: '/health', timeout: 2000 }, (res) => {
      let body = '';
      res.on('data', (c) => { body += c; });
      res.on('end', () => {
        try { resolve(JSON.parse(body)); } catch (e) { reject(e); }
      });
    });
    req.on('error', reject);
    req.on('timeout', () => { req.destroy(new Error('health timeout')); });
  });
}

/** The handshake: daemon up + /health answering. Polls the bound port. */
async function handshake(args) {
  const daemon = await startDaemon(args);
  const deadline = Date.now() + 15000;
  for (;;) {
    try {
      const health = await healthOnce(daemon.port);
      if (health && health.ok) {
        return { ...daemon, health };
      }
    } catch (_) { /* not accepting yet */ }
    if (Date.now() > deadline) {
      daemon.child.kill();
      throw new Error('daemon bound but /health never answered');
    }
    await new Promise((r) => setTimeout(r, 200));
  }
}

/** CI/smoke mode: verify the handshake, report, no window.
 *
 * PACKAGED: the .app binary has no usable stdout — the result is written
 * to `$OKRA_SMOKE_OUT` (or a temp file) instead, and the exit code carries
 * the verdict.
 */
async function smoke(args) {
  let code = 0;
  let report;
  try {
    const d = await handshake(args);
    report = { smoke: 'ok', port: d.port, bin: d.bin, cwd: args.cwd, health: d.health };
    process.stdout.write(JSON.stringify(report) + '\n');
  } catch (e) {
    report = { smoke: 'failed', error: String(e && e.message || e) };
    process.stderr.write(`smoke failed: ${report.error}\n`);
    code = 1;
  } finally {
    // packaged GUI processes have no attached stdout — mirror the result
    // to a file so `--smoke` remains verifiable in the .app
    try {
      const out = process.env.OKRA_SMOKE_OUT
        || require('node:path').join(require('node:os').tmpdir(), 'okra-smoke-result.json');
      require('node:fs').writeFileSync(out, JSON.stringify(report, null, 2) + '\n');
    } catch (_) { /* best effort */ }
    // kill the whole process group so the daemon never outlives the check
    process.exit(code);
  }
}

function runWindow(args) {
  if (!electron) {
    process.stderr.write('electron is required for the window path (npm install)\n');
    process.exit(1);
  }
  const { app, BrowserWindow, dialog } = electron;

  app.whenReady().then(async () => {
    const d = await handshake(args);
    const win = new BrowserWindow({
      width: 1440,
      height: 900,
      title: 'okra',
      autoHideMenuBar: true,
      backgroundColor: '#131316',
    });
    win.loadURL(`http://127.0.0.1:${d.port}/`);
    win.on('closed', () => d.child.kill());
    app.on('window-all-closed', () => {
      d.child.kill();
      app.quit();
    });
  }).catch((e) => {
    dialog.showErrorBox('okra failed to start', String(e && e.message || e));
    app.quit();
  });
}

// dispatch only when run as the entry point; importers get pure functions.
// PACKAGED ELECTRON: require.main can be undefined in the main process —
// dispatch there too (a plain-node require has a defined require.main, so
// importers still get the pure functions)
if (require.main === module || require.main === undefined) {
  // packaged: argv = [binary, ...userArgs]; dev: argv = [electron, appDir, ...userArgs]
  const isPackaged = Boolean(electron && electron.app && electron.app.isPackaged);
  const args = parseArgs(process.argv.slice(isPackaged ? 1 : 2));
  if (args.smoke) {
    smoke(args);
  } else if (electron) {
    runWindow(args);
  } else {
    process.stderr.write('usage: electron . [--cwd DIR] [--okra PATH] | --smoke\n');
    process.exit(1);
  }
}

module.exports = { parseArgs, resolveOkraBin, handshake, healthOnce, smoke };
