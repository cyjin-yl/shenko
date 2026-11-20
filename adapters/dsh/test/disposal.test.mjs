import assert from 'node:assert/strict';
import { execFile } from 'node:child_process';
import { mkdtemp, readFile, rm, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { test } from 'node:test';
import { fileURLToPath } from 'node:url';

// Cordis emits no exporter messages for an empty AggregateError. This rejected
// disposer exercises the failure observation that a normal logged Error misses.
// Successful disposal and bounded non-quiescence have their own lifecycle checks.
test('check withholds a ready receipt when a real Cordis disposer rejects with an empty aggregate', async t => {
  const dir = await mkdtemp(join(tmpdir(), 'shenko-dsh-disposal-'));
  t.after(() => rm(dir, { recursive: true, force: true }));
  const witness = join(dir, 'witness.txt');
  const config = join(dir, 'bridge.json');
  await writeFile(config, JSON.stringify({
    schemaVersion: 1,
    callTimeoutMs: 3000,
    shutdownGraceMs: 1000,
    plugins: [{
      id: 'disposal',
      entry: fileURLToPath(new URL('./fixtures/disposal.mjs', import.meta.url)),
      config: { witness },
    }],
  }));
  const result = await new Promise(resolve => {
    execFile(process.execPath, [fileURLToPath(new URL('../src/cli.mjs', import.meta.url)), 'check', config],
      { timeout: 10_000 }, (error, stdout, stderr) => resolve({ error, stdout, stderr }));
  });
  assert.equal(result.error?.code, 1);
  assert.equal(result.error.killed, false);
  assert.equal(result.stdout, '');
  const receipt = JSON.parse(result.stderr.trim().split('\n').at(-1));
  assert.equal(receipt.status, 'error');
  assert.match(receipt.message, /cleanup reported an error/);
  assert.equal(await readFile(witness, 'utf8'), 'dispose-started\n');
});
