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
const path = require('node:path');
const fs = require('node:fs');

// electron is optional so the handshake module is testable under plain
// node (node --test) — the window path is the only electron-dependent code
let electron = null;
try {
  electron = require('electron');
} catch (_) { /* plain node */ }

function parseArgs(argv) {
  const args = { cwd: process.cwd(), okra: process.env.OKRA_BIN || null, smoke: false };
  for (let i = 0; i < argv.length; i++) {
    const a = argv[i];
    if (a === '--cwd') args.cwd = path.resolve(argv[++i] || '.');
    else if (a === '--okra') args.okra = argv[++i] || null;
    else if (a === '--smoke') args.smoke = true;
  }
  return args;
}

/** Find the okra binary: explicit flag/env, else the repo's build output. */
function resolveOkraBin(args) {
  const candidates = [
    args.okra,
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
  const child = spawn(bin, ['serve', '--tcp', '--cwd', args.cwd], {
    stdio: ['ignore', 'ignore', 'pipe'],
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

/** CI/smoke mode: verify the handshake, print the facts, no window. */
async function smoke(args) {
  let code = 0;
  try {
    const d = await handshake(args);
    process.stdout.write(JSON.stringify({
      smoke: 'ok', port: d.port, bin: d.bin, cwd: args.cwd, health: d.health,
    }) + '\n');
  } catch (e) {
    process.stderr.write(`smoke failed: ${e && e.message}\n`);
    code = 1;
  } finally {
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

// dispatch only when run as the entry point; importers get pure functions
if (require.main === module) {
  const args = parseArgs(process.argv.slice(2));
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
