'use strict';
/* Desktop-shell handshake acceptance (no Electron needed): the thin main's
 * only nontrivial logic — spawn the real okra binary, read the bound port
 * from stderr, poll /health — verified against the compiled workspace.
 */

const test = require('node:test');
const assert = require('node:assert');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');

const { parseArgs, resolveOkraBin, handshake } = require('../main.js');

const OKRA_BIN = process.env.OKRA_BIN
  || path.join(__dirname, '..', '..', '..', 'target', 'debug', 'okra');

test('parseArgs reads cwd/okra/smoke', () => {
  const a = parseArgs(['--cwd', '/tmp/w', '--okra', '/bin/okra', '--smoke']);
  assert.equal(a.cwd, path.resolve('/tmp/w'));
  assert.equal(a.okra, '/bin/okra');
  assert.equal(a.smoke, true);
});

test('resolveOkraBin: explicit flag wins, repo build is the fallback', () => {
  assert.equal(resolveOkraBin({ okra: OKRA_BIN }), OKRA_BIN);
  // on a built workspace the fallback finds target/{debug,release}/okra
  const fallback = resolveOkraBin({ okra: null });
  assert.ok(fs.existsSync(fallback), `fallback not found: ${fallback}`);
});

test('handshake: daemon binds, /health answers, facts are right', async () => {
  const ws = fs.mkdtempSync(path.join(os.tmpdir(), 'okra-desktop-'));
  fs.writeFileSync(path.join(ws, 'notes.md'), '# desktop handshake\n');
  const d = await handshake({ cwd: ws, okra: OKRA_BIN });
  try {
    assert.ok(d.port > 0, 'bound port');
    assert.equal(d.health.ok, true);
    assert.equal(d.health.daemon, 'okra');
    assert.equal(d.health.cwd, ws);
  } finally {
    d.child.kill();
  }
});

test('handshake: an early-exiting daemon is an honest error', async () => {
  // a path that is not a directory makes `okra serve` exit(2) immediately
  await assert.rejects(
    () => handshake({ cwd: '/nope/not-a-dir-2', okra: OKRA_BIN }),
    /exited early|never bound/,
  );
});
