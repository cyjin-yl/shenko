import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { mkdtemp, readFile, rm, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { createInterface } from 'node:readline';
import { test } from 'node:test';
import { fileURLToPath } from 'node:url';

test('Unknown emits no terminal MCP response when stdio close rejects before CLI retirement', { timeout: 12_000 }, async t => {
  const dir = await mkdtemp(join(tmpdir(), 'shenko-dsh-close-'));
  t.after(() => rm(dir, { recursive: true, force: true }));
  const witness = join(dir, 'witness.txt');
  const config = join(dir, 'bridge.json');
  await writeFile(config, JSON.stringify({
    schemaVersion: 1,
    callTimeoutMs: 1000,
    shutdownGraceMs: 1000,
    plugins: [{
      id: 'close-failure',
      entry: fileURLToPath(new URL('./fixtures/close-failure.mjs', import.meta.url)),
      config: { witness },
    }],
  }));
  const child = spawn(process.execPath, [fileURLToPath(new URL('../src/cli.mjs', import.meta.url)), 'serve', config],
    { stdio: ['pipe', 'pipe', 'pipe'] });
  let killedByGuard = false;
  const guard = setTimeout(() => { killedByGuard = true; child.kill('SIGKILL'); }, 10_000);
  t.after(() => {
    clearTimeout(guard);
    if (child.exitCode === null && child.signalCode === null) child.kill('SIGKILL');
  });
  const messages = [];
  const invalidLines = [];
  const waiters = new Map();
  const lines = createInterface({ input: child.stdout });
  lines.on('line', line => {
    let message;
    try { message = JSON.parse(line); } catch { invalidLines.push(line); return; }
    messages.push(message);
    if (waiters.has(message.id)) {
      waiters.get(message.id).resolve(message);
      waiters.delete(message.id);
    }
  });
  let diagnostics = '';
  child.stderr.setEncoding('utf8');
  child.stderr.on('data', chunk => { diagnostics += chunk; });
  const exit = new Promise((resolve, reject) => {
    child.once('error', reject);
    child.once('close', (code, signal) => {
      clearTimeout(guard);
      for (const waiter of waiters.values()) waiter.reject(new Error(`CLI exited before response: ${code}/${signal}`));
      waiters.clear();
      resolve({ code, signal });
    });
  });
  function send(message) { child.stdin.write(`${JSON.stringify({ jsonrpc: '2.0', ...message })}\n`); }
  function request(id, method, params) {
    const response = new Promise((resolve, reject) => { waiters.set(id, { resolve, reject }); });
    send({ id, method, params });
    return response;
  }
  const initialized = await request(1, 'initialize', {
    protocolVersion: '2025-11-25', capabilities: {}, clientInfo: { name: 'close-failure-test', version: '1.0.0' },
  });
  assert.ok(initialized.result);
  send({ method: 'notifications/initialized' });
  const catalog = await request(2, 'tools/list', {});
  assert.deepEqual(catalog.result.tools.map(tool => tool.name).sort(), ['ordinary_error', 'uncertain_close']);
  const ordinary = await request(3, 'tools/call', { name: 'ordinary_error', arguments: {} });
  assert.equal(ordinary.result.isError, true);
  send({ id: 4, method: 'tools/call', params: { name: 'uncertain_close', arguments: {} } });
  assert.deepEqual(await exit, { code: 1, signal: null });
  assert.equal(killedByGuard, false);
  assert.deepEqual(invalidLines, []);
  assert.equal(messages.some(message => message.id === 4), false);
  assert.match(diagnostics, /fixture transport close rejected/);
  const primary = JSON.parse(diagnostics.trim().split('\n').at(-1));
  assert.equal(primary.status, 'error');
  assert.match(primary.message, /exceeded .*external effects require reconciliation/);
  const facts = (await readFile(witness, 'utf8')).trim().split('\n');
  assert.equal(facts.filter(line => line === 'effect-before-timeout').length, 1);
  assert.ok(facts.includes('close-rejected'));
  assert.equal(facts.filter(line => line === 'disposed').length, 1);
});
